// OIDC authentication module.
//
// This module is the security perimeter of the backend. It validates incoming
// JWTs against an external OIDC provider (Authentik in production, a mock
// server in E2E tests) and injects the resulting `Claims` into request
// extensions for downstream handlers to consume.
//
// Two token sources are accepted:
//   1. The `auth` httpOnly cookie — set by the browser-facing auth flow
//      (`/auth/login` → `/auth/callback`).
//   2. The `Authorization: Bearer <token>` header — used by the MCP service,
//      which obtains its own token via the OAuth2 client_credentials grant.
//
// JWT verification flow:
//   * Decode the unverified header to read the `kid` (key id).
//   * Look up the matching JWKS public key in the in-memory cache; on miss,
//     re-fetch the provider's `/.well-known/jwks.json` once and retry. This
//     handles routine key rotation without an explicit refresh trigger.
//   * Verify the signature, `aud`, `iss`, and `exp`/`nbf`.
//   * Confirm the token's `scope` claim (space-separated list) contains the
//     environment-specific `REQUIRED_SCOPE` (e.g. `bored:dev:access`).
//
// The middleware returns `401 Unauthorized` for any failure — invalid token,
// missing scope, expired, wrong issuer/audience. There is no `403` path here:
// scope failures are treated as 401 because from the protocol's point of view
// the presented token is simply not valid for this resource.

use std::{
    collections::HashMap,
    sync::{Arc, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{FromRequestParts, Request, State},
    http::{request::Parts, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use axum_extra::extract::cookie::{Cookie, Key, PrivateCookieJar, SameSite};
use base64::Engine;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};

/// Static cookie name for the persistent session token.
/// Kept in one place so the login/callback/logout routes and the middleware
/// extractor stay in sync.
pub const AUTH_COOKIE: &str = "auth";

/// Encrypted rotating refresh token for browser sessions.
pub const REFRESH_COOKIE: &str = "auth_refresh";

/// Encrypted OIDC ID token retained solely as an RP-initiated logout hint.
pub const ID_COOKIE: &str = "auth_id";

/// Static cookie name for the short-lived state nonce used during the
/// authorization-code exchange to defeat CSRF on the callback.
pub const STATE_COOKIE: &str = "auth_state";

const ACCESS_COOKIE_DEFAULT_AGE_SECS: i64 = 15 * 60;
const SESSION_COOKIE_MAX_AGE_SECS: i64 = 30 * 24 * 60 * 60;
const REFRESH_WINDOW_SECS: u64 = 60;
const REFRESH_CACHE_TTL: Duration = Duration::from_secs(120);
const REFRESH_CACHE_MAX_ENTRIES: usize = 1024;
const REFRESH_INVALIDATION_MAX_ENTRIES: usize = REFRESH_CACHE_MAX_ENTRIES * 2;
const REFRESH_INVALIDATION_TTL: Duration = Duration::from_secs(SESSION_COOKIE_MAX_AGE_SECS as u64);

/// Configuration sourced from environment variables at startup.
///
/// Cloned cheaply via `Arc<AuthConfig>` and shared into every request via
/// `AppState`. `client_secret` is sensitive and must never be logged — see
/// the manual `Debug` impl below which redacts it.
#[derive(Clone)]
pub struct AuthConfig {
    /// The OIDC issuer URL — e.g. `https://auth.desync.link/application/o/bored-dev/`.
    /// Used to construct the JWKS URL, validated against the `iss` claim, and
    /// used as the prefix for the authorize / token / end-session endpoints.
    pub issuer_url: String,
    /// Client identifier registered with Authentik. Must equal the `aud` claim.
    pub client_id: String,
    /// Confidential client secret. Sent in the token-exchange POST body.
    pub client_secret: String,
    /// Absolute URL Authentik redirects to after authentication. Must exactly
    /// match one of the redirect URIs configured on the provider.
    pub redirect_uri: String,
    /// Scope the token must contain to access this environment — `bored:dev:access`
    /// or `bored:prod:access`. Per-environment to prevent dev tokens being
    /// accepted at prod and vice versa.
    pub required_scope: String,
    /// End-session URL. Optional because some providers omit RP-initiated
    /// logout; if absent, `/auth/logout` just clears the cookie and returns to /.
    pub end_session_url: Option<String>,
    /// Authorization endpoint resolved via OIDC discovery. Authentik places this
    /// at `{base}/application/o/authorize/` (no per-app slug) while
    /// mock-oauth2-server uses `{issuer}/authorize` — discovery reconciles both.
    pub authorize_endpoint: String,
    /// Token endpoint resolved via OIDC discovery.
    pub token_endpoint: String,
    /// JWKS endpoint resolved via OIDC discovery.
    pub jwks_uri: String,
    /// Token revocation endpoint resolved via discovery. Optional because test
    /// providers and older OIDC implementations may omit it.
    pub revocation_endpoint: Option<String>,
    /// Optional issuer URL for the MCP service-account provider (a separate
    /// Authentik application that issues `client_credentials` tokens). When set,
    /// the JWT validator accepts tokens from either this issuer or `issuer_url`.
    /// Both providers share the same signing key so a single JWKS cache suffices.
    pub mcp_issuer_url: Option<String>,
    /// OAuth2 `client_id` for the MCP service-account provider. Tokens from
    /// that provider carry this value in their `aud` claim instead of `client_id`.
    pub mcp_client_id: Option<String>,
}

/// Manual `Debug` so accidental `{:?}` formatting (panic messages, tracing
/// events, error chains) cannot leak `client_secret`. Auto-derive would print
/// the secret verbatim, contradicting the safety comment on the struct.
impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("issuer_url", &self.issuer_url)
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .field("redirect_uri", &self.redirect_uri)
            .field("required_scope", &self.required_scope)
            .field("end_session_url", &self.end_session_url)
            .field("authorize_endpoint", &self.authorize_endpoint)
            .field("token_endpoint", &self.token_endpoint)
            .field("jwks_uri", &self.jwks_uri)
            .field("revocation_endpoint", &self.revocation_endpoint)
            .field("mcp_issuer_url", &self.mcp_issuer_url)
            .field("mcp_client_id", &self.mcp_client_id)
            .finish()
    }
}

/// Subset of the OIDC discovery document we care about. Fields not listed
/// (e.g. `userinfo_endpoint`, `response_types_supported`) are ignored.
#[derive(Deserialize)]
struct DiscoveryDoc {
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    #[serde(default)]
    revocation_endpoint: Option<String>,
}

impl AuthConfig {
    /// Build the auth configuration from an already-loaded `OidcConfig` and
    /// resolve provider endpoints via OIDC discovery
    /// (`/.well-known/openid-configuration`).
    ///
    /// The auth-disabled-mode decision (no `oidc.issuer-url` configured ⇒ run
    /// without auth) is made by the caller via
    /// `config::load_optional_oidc` — by the time this is called, `oidc` is
    /// `Some` and every leaf it requires is already known non-blank
    /// (`OidcConfig::validate` guarantees it). A reachable discovery document
    /// is still a hard startup error.
    pub async fn from_config(oidc: &crate::config::OidcConfig) -> Self {
        let discovery = Self::discover(&oidc.issuer_url)
            .await
            .expect("OIDC discovery failed for oidc.issuer-url");

        Self {
            issuer_url: oidc.issuer_url.clone(),
            client_id: oidc.client_id.clone(),
            client_secret: oidc.client_secret.clone(),
            redirect_uri: oidc.redirect_uri.clone(),
            required_scope: oidc.required_scope.clone(),
            end_session_url: oidc.end_session_url.clone(),
            authorize_endpoint: discovery.authorization_endpoint,
            token_endpoint: discovery.token_endpoint,
            jwks_uri: discovery.jwks_uri,
            revocation_endpoint: discovery.revocation_endpoint,
            mcp_issuer_url: oidc.mcp.issuer_url.clone(),
            mcp_client_id: oidc.mcp.client_id.clone(),
        }
    }

    /// Fetch the issuer's `/.well-known/openid-configuration` document and
    /// extract the endpoints we need. Per RFC 8414 the well-known URL is the
    /// issuer plus that suffix; we trim a trailing slash so the join is clean
    /// regardless of whether the issuer URL was stored with one.
    ///
    /// Retries with a short backoff: in containerised setups (e2e, dev) the
    /// IdP may not yet be listening when bored boots, so a single attempt is
    /// flaky. Total wait is bounded so genuine misconfiguration still fails
    /// the process quickly rather than hanging.
    async fn discover(issuer_url: &str) -> Result<DiscoveryDoc, String> {
        let base = issuer_url.trim_end_matches('/');
        let url = format!("{base}/.well-known/openid-configuration");
        let mut last_err = String::new();
        for attempt in 1..=10 {
            match reqwest::get(&url).await {
                Ok(resp) => match resp.error_for_status() {
                    Ok(resp) => match resp.json::<DiscoveryDoc>().await {
                        Ok(doc) => return Ok(doc),
                        Err(e) => last_err = format!("parsing JSON: {e}"),
                    },
                    Err(e) => last_err = format!("non-success status: {e}"),
                },
                Err(e) => last_err = format!("fetch failed: {e}"),
            }
            tracing::warn!(url = %url, attempt, error = %last_err, "OIDC discovery attempt failed; retrying");
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        Err(format!(
            "OIDC discovery {url} failed after retries: {last_err}"
        ))
    }

    pub fn jwks_url(&self) -> &str {
        &self.jwks_uri
    }

    pub fn authorize_url(&self) -> &str {
        &self.authorize_endpoint
    }

    pub fn token_url(&self) -> &str {
        &self.token_endpoint
    }

    pub fn revoke_url(&self) -> Option<&str> {
        self.revocation_endpoint.as_deref()
    }
}

/// Complete browser-side OIDC session. Tokens deliberately implement a
/// redacting `Debug` so error paths can never print bearer credentials.
#[derive(Clone)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: Option<String>,
    pub access_expires_in: i64,
}

impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("id_token", &"[REDACTED]")
            .field("access_expires_in", &self.access_expires_in)
            .finish()
    }
}

/// Token endpoint response shared by authorization-code and refresh grants.
/// Authentik returns a fresh refresh token on every refresh; `id_token` may be
/// omitted on refresh, in which case the original remains a valid logout hint.
#[derive(Deserialize)]
struct TokenEndpointResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

impl TokenEndpointResponse {
    fn into_initial(self) -> Result<TokenSet, &'static str> {
        Ok(TokenSet {
            access_token: self.access_token,
            refresh_token: self
                .refresh_token
                .ok_or("token response missing refresh_token")?,
            id_token: Some(self.id_token.ok_or("token response missing id_token")?),
            access_expires_in: self
                .expires_in
                .unwrap_or(ACCESS_COOKIE_DEFAULT_AGE_SECS)
                .max(1),
        })
    }

    fn into_refreshed(self, previous_id_token: Option<String>) -> Result<TokenSet, &'static str> {
        Ok(TokenSet {
            access_token: self.access_token,
            refresh_token: self
                .refresh_token
                .ok_or("refresh response missing rotated refresh_token")?,
            id_token: self.id_token.or(previous_id_token),
            access_expires_in: self
                .expires_in
                .unwrap_or(ACCESS_COOKIE_DEFAULT_AGE_SECS)
                .max(1),
        })
    }
}

#[derive(Clone)]
struct CachedRefresh {
    session: TokenSet,
    claims: Claims,
    expires_at: Instant,
}

#[derive(Default)]
struct RefreshState {
    cache: HashMap<[u8; 32], CachedRefresh>,
    invalidated: HashMap<[u8; 32], Instant>,
    gates: HashMap<[u8; 32], Weak<Mutex<()>>>,
}

/// Browser-session cryptography and refresh coordination.
///
/// Authentik immediately invalidates a refresh token after use. Per-token gates
/// collapse requests carrying the same old cookie without making unrelated
/// browser sessions wait for each other's provider exchanges. The short cache
/// lets same-token waiters reuse the one successful rotated response.
pub struct AuthSessionManager {
    cookie_key: Key,
    http: reqwest::Client,
    refresh_state: Mutex<RefreshState>,
}

impl std::fmt::Debug for AuthSessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthSessionManager")
            .field("cookie_key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl AuthSessionManager {
    /// Build from an already-loaded `SessionConfig`'s cookie key. Exactly 64
    /// random bytes are used by the cookie crate as independent signing and
    /// encryption key material.
    pub fn from_config(session: &crate::config::SessionConfig) -> Result<Self, String> {
        Self::from_encoded_key(&session.cookie_key)
    }

    fn from_encoded_key(encoded: &str) -> Result<Self, String> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| "session.cookie-key must be valid standard base64".to_string())?;
        if bytes.len() != 64 {
            return Err(format!(
                "session.cookie-key must decode to exactly 64 bytes (got {})",
                bytes.len()
            ));
        }
        Ok(Self::from_key_bytes(&bytes))
    }

    fn from_key_bytes(bytes: &[u8]) -> Self {
        Self {
            cookie_key: Key::from(bytes),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(10))
                .build()
                .expect("failed to build OIDC HTTP client"),
            refresh_state: Mutex::new(RefreshState::default()),
        }
    }

    pub fn cookie_jar(&self, headers: &HeaderMap) -> PrivateCookieJar {
        PrivateCookieJar::from_headers(headers, self.cookie_key.clone())
    }

    pub fn read_access_token(&self, jar: &PrivateCookieJar) -> Option<String> {
        jar.get(AUTH_COOKIE)
            .map(|cookie| cookie.value().to_string())
    }

    pub fn read_refresh_token(&self, jar: &PrivateCookieJar) -> Option<String> {
        jar.get(REFRESH_COOKIE)
            .map(|cookie| cookie.value().to_string())
    }

    pub fn read_id_token(&self, jar: &PrivateCookieJar) -> Option<String> {
        jar.get(ID_COOKIE).map(|cookie| cookie.value().to_string())
    }

    pub fn write_session(&self, jar: PrivateCookieJar, session: &TokenSet) -> PrivateCookieJar {
        let jar = jar
            .add(session_cookie(
                AUTH_COOKIE,
                session.access_token.clone(),
                session.access_expires_in,
            ))
            .add(session_cookie(
                REFRESH_COOKIE,
                session.refresh_token.clone(),
                SESSION_COOKIE_MAX_AGE_SECS,
            ));
        match session.id_token.as_ref() {
            Some(id_token) => jar.add(session_cookie(
                ID_COOKIE,
                id_token.clone(),
                SESSION_COOKIE_MAX_AGE_SECS,
            )),
            None => jar.add(expired_session_cookie(ID_COOKIE)),
        }
    }

    pub fn clear_session(&self, jar: PrivateCookieJar) -> PrivateCookieJar {
        jar.add(expired_session_cookie(AUTH_COOKIE))
            .add(expired_session_cookie(REFRESH_COOKIE))
            .add(expired_session_cookie(ID_COOKIE))
    }

    pub async fn exchange_authorization_code(
        &self,
        auth: &AuthConfig,
        code: &str,
    ) -> Result<TokenSet, String> {
        let response = self
            .post_token(
                auth,
                &[
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    ("redirect_uri", auth.redirect_uri.as_str()),
                ],
            )
            .await?;
        response.into_initial().map_err(str::to_string)
    }

    async fn post_token(
        &self,
        auth: &AuthConfig,
        grant: &[(&str, &str)],
    ) -> Result<TokenEndpointResponse, String> {
        let mut form = grant.to_vec();
        form.push(("client_id", auth.client_id.as_str()));
        form.push(("client_secret", auth.client_secret.as_str()));
        let response = self
            .http
            .post(auth.token_url())
            .form(&form)
            .send()
            .await
            .map_err(|error| format!("token endpoint unreachable: {error}"))?;
        if !response.status().is_success() {
            return Err(format!("token endpoint returned {}", response.status()));
        }
        response
            .json()
            .await
            .map_err(|error| format!("token response parse failed: {error}"))
    }

    pub async fn refresh(
        &self,
        auth: &AuthConfig,
        jwks: &JwksCache,
        old_refresh_token: &str,
        id_token: Option<String>,
    ) -> Result<(TokenSet, Claims), String> {
        self.coordinate_refresh(old_refresh_token, || async move {
            let response = self
                .post_token(
                    auth,
                    &[
                        ("grant_type", "refresh_token"),
                        ("refresh_token", old_refresh_token),
                    ],
                )
                .await?;
            let session = response.into_refreshed(id_token).map_err(str::to_string)?;
            let claims = validate_jwt(&session.access_token, auth, jwks)
                .await
                .map_err(str::to_string)?;
            Ok((session, claims))
        })
        .await
    }

    async fn coordinate_refresh<F, Fut>(
        &self,
        old_refresh_token: &str,
        exchange: F,
    ) -> Result<(TokenSet, Claims), String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<(TokenSet, Claims), String>>,
    {
        let fingerprint = refresh_fingerprint(old_refresh_token);
        let gate = {
            let mut state = self.refresh_state.lock().await;
            prune_refresh_state(&mut state, Instant::now());
            refresh_gate(&mut state, fingerprint)
        };
        let _gate = gate.lock().await;

        // Recheck after acquiring the token-specific gate: another request may
        // have completed the exchange, or logout may have tombstoned it, while
        // this request waited.
        {
            let mut state = self.refresh_state.lock().await;
            prune_refresh_state(&mut state, Instant::now());
            if state.invalidated.contains_key(&fingerprint) {
                return Err("refresh session was invalidated by logout".to_string());
            }
            if let Some(entry) = state.cache.get(&fingerprint) {
                return Ok((entry.session.clone(), entry.claims.clone()));
            }
        }

        // Only this token's gate is held across provider I/O; unrelated
        // refresh-token fingerprints can exchange concurrently.
        let (session, claims) = exchange().await?;

        let now = Instant::now();
        let mut state = self.refresh_state.lock().await;
        prune_refresh_state(&mut state, now);
        if state.invalidated.contains_key(&fingerprint)
            || state
                .invalidated
                .contains_key(&refresh_fingerprint(&session.refresh_token))
        {
            return Err("rotated refresh session was invalidated by logout".to_string());
        }

        if state.cache.len() >= REFRESH_CACHE_MAX_ENTRIES
            && let Some(oldest) = state
                .cache
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| *key)
        {
            state.cache.remove(&oldest);
        }
        state.cache.insert(
            fingerprint,
            CachedRefresh {
                session: session.clone(),
                claims: claims.clone(),
                expires_at: now + REFRESH_CACHE_TTL,
            },
        );
        Ok((session, claims))
    }

    /// Remove and tombstone every cached rotation connected to a token being
    /// logged out. Returns the raw tokens still available in cache so the
    /// caller can best-effort revoke the active end of the rotation chain.
    pub async fn invalidate_refresh_chain(&self, refresh_token: &str) -> Vec<String> {
        let root_fingerprint = refresh_fingerprint(refresh_token);
        let gate = {
            let mut state = self.refresh_state.lock().await;
            prune_refresh_state(&mut state, Instant::now());
            refresh_gate(&mut state, root_fingerprint)
        };
        let _gate = gate.lock().await;

        let now = Instant::now();
        let expires_at = now + REFRESH_INVALIDATION_TTL;
        let mut state = self.refresh_state.lock().await;
        prune_refresh_state(&mut state, now);

        let mut fingerprints = vec![root_fingerprint];
        let mut revoke_tokens = vec![refresh_token.to_string()];

        // Follow both directions: the supplied cookie may be the old token
        // that keyed a cached rotation or the new token stored in its value.
        loop {
            let connected = fingerprints
                .iter()
                .find(|fingerprint| state.cache.contains_key(*fingerprint))
                .copied()
                .or_else(|| {
                    state
                        .cache
                        .iter()
                        .find(|(_, entry)| {
                            fingerprints
                                .contains(&refresh_fingerprint(&entry.session.refresh_token))
                        })
                        .map(|(fingerprint, _)| *fingerprint)
                });
            let Some(fingerprint) = connected else {
                break;
            };
            let entry = state
                .cache
                .remove(&fingerprint)
                .expect("connected refresh entry must still exist");
            if !fingerprints.contains(&fingerprint) {
                fingerprints.push(fingerprint);
            }
            let rotated_fingerprint = refresh_fingerprint(&entry.session.refresh_token);
            if !fingerprints.contains(&rotated_fingerprint) {
                fingerprints.push(rotated_fingerprint);
            }
            if !revoke_tokens.contains(&entry.session.refresh_token) {
                revoke_tokens.push(entry.session.refresh_token);
            }
        }

        for fingerprint in fingerprints {
            if state.invalidated.len() >= REFRESH_INVALIDATION_MAX_ENTRIES
                && let Some(oldest) = state
                    .invalidated
                    .iter()
                    .min_by_key(|(_, expiry)| *expiry)
                    .map(|(fingerprint, _)| *fingerprint)
            {
                state.invalidated.remove(&oldest);
            }
            state.invalidated.insert(fingerprint, expires_at);
        }

        revoke_tokens
    }

    pub async fn refresh_was_invalidated(&self, refresh_token: &str) -> bool {
        let now = Instant::now();
        let mut state = self.refresh_state.lock().await;
        state.invalidated.retain(|_, expiry| *expiry > now);
        state
            .invalidated
            .contains_key(&refresh_fingerprint(refresh_token))
    }

    pub async fn revoke_refresh_token(
        &self,
        auth: &AuthConfig,
        refresh_token: &str,
    ) -> Result<(), String> {
        let Some(url) = auth.revoke_url() else {
            return Ok(());
        };
        let response = self
            .http
            .post(url)
            .form(&[
                ("token", refresh_token),
                ("token_type_hint", "refresh_token"),
                ("client_id", auth.client_id.as_str()),
                ("client_secret", auth.client_secret.as_str()),
            ])
            .send()
            .await
            .map_err(|error| format!("revocation endpoint unreachable: {error}"))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!(
                "revocation endpoint returned {}",
                response.status()
            ))
        }
    }
}

fn refresh_fingerprint(refresh_token: &str) -> [u8; 32] {
    Sha256::digest(refresh_token.as_bytes()).into()
}

fn prune_refresh_state(state: &mut RefreshState, now: Instant) {
    state.cache.retain(|_, entry| entry.expires_at > now);
    state.invalidated.retain(|_, expiry| *expiry > now);
    state.gates.retain(|_, gate| gate.strong_count() > 0);
}

fn refresh_gate(state: &mut RefreshState, fingerprint: [u8; 32]) -> Arc<Mutex<()>> {
    if let Some(gate) = state.gates.get(&fingerprint).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(Mutex::new(()));
    state.gates.insert(fingerprint, Arc::downgrade(&gate));
    gate
}

fn session_cookie(name: &'static str, value: String, max_age_secs: i64) -> Cookie<'static> {
    Cookie::build((name, value))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::seconds(max_age_secs))
        .build()
}

fn expired_session_cookie(name: &'static str) -> Cookie<'static> {
    session_cookie(name, String::new(), 0)
}

fn access_needs_refresh(token: &str) -> bool {
    #[derive(Deserialize)]
    struct Expiry {
        exp: u64,
    }

    let Some(payload) = token.split('.').nth(1) else {
        return true;
    };
    let Ok(decoded) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) else {
        return true;
    };
    let Ok(expiry) = serde_json::from_slice::<Expiry>(&decoded) else {
        return true;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    expiry.exp <= now.saturating_add(REFRESH_WINDOW_SECS)
}

/// In-memory cache of OIDC public keys keyed by `kid`.
///
/// Production identity providers rotate signing keys periodically; the cache
/// re-fetches the JWKS document the first time it sees an unknown `kid`. The
/// cache holds an `Arc<reqwest::Client>` so the underlying HTTPS connection
/// pool is shared across refreshes.
pub struct JwksCache {
    keys: RwLock<HashMap<String, DecodingKey>>,
    http: reqwest::Client,
    jwks_url: String,
}

impl JwksCache {
    pub fn new(jwks_url: String) -> Self {
        Self {
            keys: RwLock::new(HashMap::new()),
            // Default reqwest client — picks up system CA roots via rustls-native-roots
            // wouldn't apply here since we use rustls-tls (webpki roots). That's fine
            // for talking to public IdPs over a valid TLS cert; for a local mock OIDC
            // running on plain HTTP, reqwest handles `http://` URLs natively too.
            http: reqwest::Client::new(),
            jwks_url,
        }
    }

    /// Look up a key by `kid`, refreshing the cache once on miss.
    /// Returns `None` if the key is still missing after a refresh — the caller
    /// should treat this as an invalid token (kid not signed by this issuer).
    async fn get(&self, kid: &str) -> Option<DecodingKey> {
        // Fast path: read lock for the common case where the key is cached.
        if let Some(key) = self.keys.read().await.get(kid).cloned() {
            return Some(key);
        }
        // Slow path: refresh and try once more. We don't hold the write lock
        // across the network call to avoid blocking other readers.
        if let Err(e) = self.refresh().await {
            tracing::warn!(error = %e, "JWKS refresh failed");
            return None;
        }
        self.keys.read().await.get(kid).cloned()
    }

    /// Force-refresh the entire keyset. Replaces the cache atomically.
    /// Errors are surfaced rather than swallowed — we want the caller (the
    /// refresh-on-miss path or a startup warm) to log the underlying cause.
    async fn refresh(&self) -> Result<(), String> {
        let jwks: Jwks = self
            .http
            .get(&self.jwks_url)
            .send()
            .await
            .map_err(|e| format!("fetching JWKS: {e}"))?
            .error_for_status()
            .map_err(|e| format!("JWKS HTTP status: {e}"))?
            .json()
            .await
            .map_err(|e| format!("parsing JWKS: {e}"))?;
        let mut new_keys = HashMap::new();
        for jwk in jwks.keys {
            // Only RSA keys with a kid are supported. Other key types (EC,
            // OKP) and keys without a kid are silently skipped — Authentik
            // defaults to RS256 with kid for OIDC providers.
            let (Some(kid), Some(n), Some(e)) = (jwk.kid, jwk.n, jwk.e) else {
                continue;
            };
            match DecodingKey::from_rsa_components(&n, &e) {
                Ok(key) => {
                    new_keys.insert(kid, key);
                }
                Err(err) => {
                    tracing::warn!(error = %err, "skipping malformed JWK");
                }
            }
        }
        *self.keys.write().await = new_keys;
        Ok(())
    }
}

/// JWKS document shape — only the fields we actually use.
#[derive(Debug, Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Debug, Deserialize)]
struct Jwk {
    kid: Option<String>,
    /// RSA modulus, base64url-encoded.
    n: Option<String>,
    /// RSA exponent, base64url-encoded (typically `AQAB`).
    e: Option<String>,
}

/// Validated JWT claims used by request handlers.
///
/// Cloned into request extensions by the auth middleware; downstream handlers
/// extract via `Extension<Claims>` or the `Claims` extractor below. `aud` is
/// kept as raw JSON because the spec allows either a single string or an
/// array — `jsonwebtoken` handles the comparison itself, so we only need to
/// expose the strongly-typed fields the application cares about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Stable per-user identifier. Stored in `last_edited_by` on every
    /// mutation. For MCP, this is the auto-created service account's id.
    pub sub: String,
    /// Email — present on browser tokens, usually absent on client_credentials.
    #[serde(default)]
    pub email: Option<String>,
    /// Display name — falls back to `sub` if absent.
    #[serde(default)]
    pub preferred_username: Option<String>,
    /// Service/display identity from custom scope mappings (for MCP-style
    /// machine tokens). When present, this should win over username/email for
    /// audit attribution.
    #[serde(default)]
    pub actor_display_name: Option<String>,
    /// IdP-provided avatar URL when available. Frontend falls back to Gravatar.
    #[serde(default)]
    pub picture: Option<String>,
    /// Space-separated scope list. Per OIDC, `scope` is a string, not an array.
    #[serde(default)]
    pub scope: Option<String>,
    /// Issuer — validated by `jsonwebtoken` against `Validation.iss`.
    pub iss: String,
    /// Expiry as Unix timestamp; validated by `jsonwebtoken`.
    pub exp: u64,
}

impl Claims {
    /// Display name with sensible fallback. Used by `/api/me` and tracing.
    pub fn display_name(&self) -> String {
        self.actor_display_name
            .clone()
            .or_else(|| self.preferred_username.clone())
            .or_else(|| self.email.clone())
            .unwrap_or_else(|| self.sub.clone())
    }

    /// Convert to the public-facing UserInfo response.
    pub fn to_user_info(&self) -> shared::UserInfo {
        shared::UserInfo {
            name: self.display_name(),
            email: self.email.clone(),
            picture: self.picture.clone(),
        }
    }
}

/// Validate a raw JWT string against the configured issuer and required scope.
/// Returns the decoded claims on success, or a static error string the caller
/// should treat as `401 Unauthorized`.
pub async fn validate_jwt(
    token: &str,
    config: &AuthConfig,
    cache: &JwksCache,
) -> Result<Claims, &'static str> {
    // Decode the header without verifying so we can find the right key.
    let header = decode_header(token).map_err(|_| "invalid JWT header")?;
    let kid = header.kid.ok_or("JWT missing kid header")?;
    let key = cache.get(&kid).await.ok_or("JWT kid not in JWKS")?;
    // Build a Validation that enforces exp, nbf, iss, aud all in one pass.
    // We default to RS256; Authentik signs with RS256 unless reconfigured.
    let mut validation = Validation::new(header.alg);
    // Force-restrict the algorithm set to RSA — accept the algorithm declared
    // in the token header only if it's an RSA family alg. This prevents an
    // attacker from forging an HMAC-signed token using the JWKS public key
    // material as the HMAC secret (the classic alg-confusion attack).
    let allowed_algs = [Algorithm::RS256, Algorithm::RS384, Algorithm::RS512];
    if !allowed_algs.contains(&header.alg) {
        return Err("unsupported JWT algorithm");
    }
    validation.algorithms = allowed_algs.to_vec();
    // Accept tokens from either the browser provider or the MCP service-account
    // provider. Both live on the same Authentik instance and share a signing key,
    // so the existing JWKS cache covers both without an extra fetch.
    let mut audiences = vec![config.client_id.as_str()];
    if let Some(mcp_cid) = config.mcp_client_id.as_deref() {
        audiences.push(mcp_cid);
    }
    validation.set_audience(&audiences);
    let mut issuers = vec![config.issuer_url.as_str()];
    if let Some(mcp_iss) = config.mcp_issuer_url.as_deref() {
        issuers.push(mcp_iss);
    }
    validation.set_issuer(&issuers);
    // Default leeway is 60s; that's fine for clock skew on the mini server.
    let data = decode::<Claims>(token, &key, &validation).map_err(|e| {
        // Don't leak internal jsonwebtoken error variants to clients.
        // We keep them in the log for diagnosis.
        tracing::debug!(error = %e, "JWT validation failed");
        "JWT validation failed"
    })?;
    // Scope check — `scope` is a space-separated string per RFC 6749 §3.3.
    let scope = data.claims.scope.as_deref().unwrap_or("");
    if !scope.split_whitespace().any(|s| s == config.required_scope) {
        return Err("missing required scope");
    }
    Ok(data.claims)
}

/// Axum middleware that extracts a token from the request, validates it, and
/// inserts the resulting `Claims` into request extensions.
///
/// Token sources tried in order:
///   1. `Authorization: Bearer <token>` — stateless MCP and curl/scripts.
///   2. The encrypted browser cookie set — refreshable SPA session.
///
/// Returns `401 Unauthorized` for any failure. Successful requests pass
/// through with the request body intact; the caller can extract claims with
/// `Extension<Claims>` or the `Claims` extractor.
pub async fn auth_middleware(
    State(state): State<crate::routes::boards::AppState>,
    headers: HeaderMap,
    mut req: Request,
    next: Next,
) -> Response {
    // If the server was started without OIDC config (local dev), short-circuit
    // by injecting a synthetic anonymous claim. This keeps the existing dev
    // workflow working without a live IdP. Production always has auth set.
    let Some(auth) = state.auth.as_ref() else {
        let claims = Claims {
            sub: "anonymous".to_string(),
            email: None,
            preferred_username: Some("anonymous".to_string()),
            actor_display_name: None,
            picture: None,
            scope: None,
            iss: "auth-disabled".to_string(),
            exp: u64::MAX,
        };
        req.extensions_mut().insert(claims);
        return next.run(req).await;
    };

    let jwks = state
        .jwks_cache
        .as_ref()
        .expect("jwks_cache must be present when auth is configured");

    // Bearer authentication is deliberately stateless. In particular, MCP
    // service tokens must never consume or rewrite browser refresh cookies.
    if let Some(token) = extract_bearer(&headers) {
        return match validate_jwt(&token, auth, jwks).await {
            Ok(claims) => {
                req.extensions_mut().insert(claims);
                next.run(req).await
            }
            Err(reason) => {
                tracing::warn!(reason, "bearer auth rejected request");
                (StatusCode::UNAUTHORIZED, reason).into_response()
            }
        };
    }

    let sessions = state
        .auth_sessions
        .as_ref()
        .expect("auth_sessions must be present when auth is configured");
    let jar = sessions.cookie_jar(&headers);
    let access_token = sessions.read_access_token(&jar);
    let refresh_token = sessions.read_refresh_token(&jar);

    // A response that was already in flight when logout ran may arrive after
    // the browser processed the clear-cookie response. Refuse the tombstoned
    // refresh chain even if that late response restored a still-valid access
    // cookie, and clear it again immediately.
    if let Some(refresh_token) = refresh_token.as_deref()
        && sessions.refresh_was_invalidated(refresh_token).await
    {
        let jar = sessions.clear_session(jar);
        return (
            jar,
            (
                StatusCode::UNAUTHORIZED,
                "session was invalidated by logout",
            ),
        )
            .into_response();
    }

    // A healthy access token outside the refresh window remains on the fast
    // path: validate locally and serve without emitting Set-Cookie.
    if let Some(token) = access_token.as_deref()
        && !access_needs_refresh(token)
    {
        return match validate_jwt(token, auth, jwks).await {
            Ok(claims) => {
                req.extensions_mut().insert(claims);
                next.run(req).await
            }
            Err(reason) => {
                tracing::warn!(reason, "browser access token rejected");
                let jar = sessions.clear_session(jar);
                (jar, (StatusCode::UNAUTHORIZED, reason)).into_response()
            }
        };
    }

    // Access token is absent, expired, or close to expiry. A complete refresh
    // session can recover even after a long-idle browser has dropped `auth`.
    let Some(refresh_token) = refresh_token else {
        let jar = sessions.clear_session(jar);
        return (jar, (StatusCode::UNAUTHORIZED, "missing refresh token")).into_response();
    };
    let id_token = sessions.read_id_token(&jar);

    match sessions.refresh(auth, jwks, &refresh_token, id_token).await {
        Ok((session, claims)) => {
            req.extensions_mut().insert(claims);
            let response = next.run(req).await;
            let jar = if sessions
                .refresh_was_invalidated(&session.refresh_token)
                .await
            {
                sessions.clear_session(jar)
            } else {
                sessions.write_session(jar, &session)
            };
            (jar, response).into_response()
        }
        Err(error) => {
            tracing::warn!(error = %error, "browser token refresh failed");
            let jar = sessions.clear_session(jar);
            (jar, (StatusCode::UNAUTHORIZED, "session refresh failed")).into_response()
        }
    }
}

/// Pull a Bearer token out of the `Authorization` header, if present and
/// well-formed. Returns `None` for missing or malformed values; the caller
/// should fall back to the cookie source.
fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    // Case-insensitive scheme match per RFC 7235.
    let mut parts = value.splitn(2, char::is_whitespace);
    let scheme = parts.next()?;
    let token = parts.next()?.trim();
    if scheme.eq_ignore_ascii_case("Bearer") && !token.is_empty() {
        Some(token.to_string())
    } else {
        None
    }
}

/// Extractor that pulls validated `Claims` out of request extensions.
///
/// The auth middleware inserts the claims unconditionally; this extractor
/// surfaces them to handlers without requiring `Extension<Claims>` boilerplate
/// at every call site. Returns 401 if the middleware was bypassed (defensive
/// programming — should never happen for routes mounted under the middleware).
#[axum::async_trait]
impl<S> FromRequestParts<S> for Claims
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Claims>()
            .cloned()
            .ok_or((StatusCode::UNAUTHORIZED, "no claims in extensions"))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    use axum::{http::header, response::IntoResponse};
    use base64::Engine;

    use super::{
        access_needs_refresh, AuthSessionManager, Claims, TokenEndpointResponse, TokenSet,
        AUTH_COOKIE, ID_COOKIE, REFRESH_COOKIE,
    };

    fn test_manager() -> AuthSessionManager {
        AuthSessionManager::from_key_bytes(&[7_u8; 64])
    }

    fn token_set() -> TokenSet {
        TokenSet {
            access_token: "access-token".to_string(),
            refresh_token: "rotated-refresh-token".to_string(),
            id_token: Some("id-token".to_string()),
            access_expires_in: 900,
        }
    }

    fn claims() -> Claims {
        Claims {
            sub: "user".to_string(),
            email: None,
            preferred_username: Some("user".to_string()),
            actor_display_name: None,
            picture: None,
            scope: Some("bored:test:access".to_string()),
            iss: "issuer".to_string(),
            exp: u64::MAX,
        }
    }

    fn unsigned_token(exp: u64) -> String {
        let payload = serde_json::json!({ "exp": exp });
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        format!("header.{encoded}.signature")
    }

    #[test]
    fn refresh_window_includes_expired_and_near_expiry_tokens() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(!access_needs_refresh(&unsigned_token(now + 61)));
        assert!(access_needs_refresh(&unsigned_token(now + 60)));
        assert!(access_needs_refresh(&unsigned_token(now - 1)));
        assert!(access_needs_refresh("not-a-jwt"));
    }

    #[test]
    fn initial_exchange_requires_refresh_and_id_tokens() {
        let missing_refresh = TokenEndpointResponse {
            access_token: "access".to_string(),
            refresh_token: None,
            id_token: Some("id".to_string()),
            expires_in: Some(900),
        };
        assert!(missing_refresh.into_initial().is_err());

        let missing_id = TokenEndpointResponse {
            access_token: "access".to_string(),
            refresh_token: Some("refresh".to_string()),
            id_token: None,
            expires_in: Some(900),
        };
        assert!(missing_id.into_initial().is_err());
    }

    #[test]
    fn private_cookie_key_requires_valid_64_byte_base64() {
        let valid = base64::engine::general_purpose::STANDARD.encode([9_u8; 64]);
        assert!(AuthSessionManager::from_encoded_key(&valid).is_ok());
        assert!(AuthSessionManager::from_encoded_key("not base64").is_err());

        let too_short = base64::engine::general_purpose::STANDARD.encode([9_u8; 63]);
        assert!(AuthSessionManager::from_encoded_key(&too_short).is_err());
    }

    #[test]
    fn refresh_response_persists_rotation_and_preserves_missing_id_token() {
        let refreshed = TokenEndpointResponse {
            access_token: "new-access".to_string(),
            refresh_token: Some("new-refresh".to_string()),
            id_token: None,
            expires_in: Some(900),
        }
        .into_refreshed(Some("original-id".to_string()))
        .unwrap();

        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(refreshed.refresh_token, "new-refresh");
        assert_eq!(refreshed.id_token.as_deref(), Some("original-id"));

        let without_hint = TokenEndpointResponse {
            access_token: "new-access".to_string(),
            refresh_token: Some("new-refresh".to_string()),
            id_token: None,
            expires_in: Some(900),
        }
        .into_refreshed(None)
        .unwrap();
        assert!(without_hint.id_token.is_none());
    }

    #[test]
    fn private_cookie_round_trip_and_tamper_rejection() {
        let manager = test_manager();
        let jar = manager.write_session(
            manager.cookie_jar(&axum::http::HeaderMap::new()),
            &token_set(),
        );
        let response = jar.into_response();
        let set_cookies: Vec<String> = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap().to_string())
            .collect();
        assert_eq!(set_cookies.len(), 3);
        for cookie in &set_cookies {
            assert!(cookie.contains("HttpOnly"));
            assert!(cookie.contains("Secure"));
            assert!(cookie.contains("SameSite=Lax"));
            assert!(cookie.contains("Path=/"));
        }

        let cookie_header = set_cookies
            .iter()
            .map(|cookie| cookie.split(';').next().unwrap())
            .collect::<Vec<_>>()
            .join("; ");
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::COOKIE, cookie_header.parse().unwrap());
        let round_trip = manager.cookie_jar(&headers);
        assert_eq!(
            manager.read_access_token(&round_trip).as_deref(),
            Some("access-token")
        );
        assert_eq!(
            manager.read_refresh_token(&round_trip).as_deref(),
            Some("rotated-refresh-token")
        );
        assert_eq!(
            manager.read_id_token(&round_trip).as_deref(),
            Some("id-token")
        );

        let tampered = cookie_header
            .split("; ")
            .map(|cookie| {
                if let Some(value) = cookie.strip_prefix(&format!("{REFRESH_COOKIE}=")) {
                    let mut bytes = value.as_bytes().to_vec();
                    bytes[0] = if bytes[0] == b'A' { b'B' } else { b'A' };
                    format!("{REFRESH_COOKIE}={}", String::from_utf8(bytes).unwrap())
                } else {
                    cookie.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        headers.insert(header::COOKIE, tampered.parse().unwrap());
        let tampered_jar = manager.cookie_jar(&headers);
        assert!(manager.read_refresh_token(&tampered_jar).is_none());
        assert!(manager.read_access_token(&tampered_jar).is_some());
        assert!(manager.read_id_token(&tampered_jar).is_some());
        assert!(cookie_header.contains(&format!("{AUTH_COOKIE}=")));
        assert!(cookie_header.contains(&format!("{ID_COOKIE}=")));
    }

    #[tokio::test]
    async fn concurrent_refreshes_share_one_exchange() {
        let manager = Arc::new(test_manager());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let manager = manager.clone();
            let calls = calls.clone();
            tasks.push(tokio::spawn(async move {
                manager
                    .coordinate_refresh("one-use-refresh-token", || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        Ok((token_set(), claims()))
                    })
                    .await
                    .unwrap()
            }));
        }

        for task in tasks {
            let (session, returned_claims) = task.await.unwrap();
            assert_eq!(session.refresh_token, "rotated-refresh-token");
            assert_eq!(returned_claims.sub, "user");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unrelated_refresh_tokens_exchange_concurrently() {
        let manager = Arc::new(test_manager());
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut tasks = Vec::new();

        for refresh_token in ["refresh-a", "refresh-b"] {
            let manager = manager.clone();
            let barrier = barrier.clone();
            let started_tx = started_tx.clone();
            tasks.push(tokio::spawn(async move {
                manager
                    .coordinate_refresh(refresh_token, || async move {
                        started_tx.send(refresh_token).unwrap();
                        barrier.wait().await;
                        Ok((token_set(), claims()))
                    })
                    .await
                    .unwrap()
            }));
        }
        drop(started_tx);

        tokio::time::timeout(std::time::Duration::from_millis(250), async {
            assert!(started_rx.recv().await.is_some());
            assert!(started_rx.recv().await.is_some());
        })
        .await
        .expect("unrelated refresh exchanges should both start");
        barrier.wait().await;

        for task in tasks {
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn logout_invalidates_an_in_flight_refresh_chain_before_waiters_replay_it() {
        let manager = Arc::new(test_manager());
        let (exchange_started_tx, exchange_started_rx) = tokio::sync::oneshot::channel();
        let (finish_exchange_tx, finish_exchange_rx) = tokio::sync::oneshot::channel();

        let first_manager = manager.clone();
        let first = tokio::spawn(async move {
            first_manager
                .coordinate_refresh("old-refresh-token", || async move {
                    exchange_started_tx.send(()).unwrap();
                    finish_exchange_rx.await.unwrap();
                    Ok((token_set(), claims()))
                })
                .await
        });
        exchange_started_rx.await.unwrap();

        // The same-token gate is FIFO: queue logout before the waiter so it
        // removes and tombstones the just-created cache entry first.
        let logout_manager = manager.clone();
        let logout = tokio::spawn(async move {
            logout_manager
                .invalidate_refresh_chain("old-refresh-token")
                .await
        });
        tokio::task::yield_now().await;

        let replay_calls = Arc::new(AtomicUsize::new(0));
        let waiter_manager = manager.clone();
        let waiter_calls = replay_calls.clone();
        let waiter = tokio::spawn(async move {
            waiter_manager
                .coordinate_refresh("old-refresh-token", || async move {
                    waiter_calls.fetch_add(1, Ordering::SeqCst);
                    Ok((token_set(), claims()))
                })
                .await
        });

        finish_exchange_tx.send(()).unwrap();
        assert!(first.await.unwrap().is_ok());
        let revoked = logout.await.unwrap();
        assert!(revoked.contains(&"old-refresh-token".to_string()));
        assert!(revoked.contains(&"rotated-refresh-token".to_string()));
        assert!(waiter.await.unwrap().is_err());
        assert_eq!(replay_calls.load(Ordering::SeqCst), 0);
        assert!(manager.refresh_was_invalidated("old-refresh-token").await);
        assert!(
            manager
                .refresh_was_invalidated("rotated-refresh-token")
                .await
        );
    }

    #[test]
    fn token_debug_output_is_redacted() {
        let debug = format!("{:?}", token_set());
        assert!(!debug.contains("access-token"));
        assert!(!debug.contains("rotated-refresh-token"));
        assert!(!debug.contains("id-token"));
        assert!(debug.contains("[REDACTED]"));
    }
}
