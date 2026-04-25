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

use std::collections::HashMap;

use axum::{
    extract::{FromRequestParts, Request, State},
    http::{request::Parts, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use axum_extra::extract::cookie::CookieJar;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Static cookie name for the persistent session token.
/// Kept in one place so the login/callback/logout routes and the middleware
/// extractor stay in sync.
pub const AUTH_COOKIE: &str = "auth";

/// Static cookie name for the short-lived state nonce used during the
/// authorization-code exchange to defeat CSRF on the callback.
pub const STATE_COOKIE: &str = "auth_state";

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
}

impl AuthConfig {
    /// Read the auth configuration from environment variables and resolve
    /// provider endpoints via OIDC discovery (`/.well-known/openid-configuration`).
    /// Returns `None` if `OIDC_ISSUER_URL` is unset *or empty*, allowing the
    /// server to run in "auth-disabled" mode for local development without
    /// IdP setup. The empty-string case matters because `deploy/docker-compose.yml`
    /// uses `${OIDC_ISSUER_URL:-}` to forward the host env, which sets the
    /// var to "" rather than leaving it unset when the host has no OIDC config.
    /// All other required vars — and a reachable discovery document — become
    /// hard errors when issuer is set.
    pub async fn load() -> Option<Self> {
        let issuer_url = std::env::var("OIDC_ISSUER_URL")
            .ok()
            .filter(|s| !s.is_empty())?;
        let client_id = std::env::var("OIDC_CLIENT_ID")
            .expect("OIDC_CLIENT_ID required when OIDC_ISSUER_URL is set");
        let client_secret = std::env::var("OIDC_CLIENT_SECRET")
            .expect("OIDC_CLIENT_SECRET required when OIDC_ISSUER_URL is set");
        let redirect_uri = std::env::var("OIDC_REDIRECT_URI")
            .expect("OIDC_REDIRECT_URI required when OIDC_ISSUER_URL is set");
        let required_scope = std::env::var("REQUIRED_SCOPE")
            .expect("REQUIRED_SCOPE required when OIDC_ISSUER_URL is set");
        let end_session_url = std::env::var("OIDC_END_SESSION_URL")
            .ok()
            .filter(|s| !s.is_empty());

        let mcp_issuer_url = std::env::var("OIDC_MCP_ISSUER_URL")
            .ok()
            .filter(|s| !s.is_empty());
        let mcp_client_id = std::env::var("OIDC_MCP_CLIENT_ID")
            .ok()
            .filter(|s| !s.is_empty());

        let discovery = Self::discover(&issuer_url)
            .await
            .expect("OIDC discovery failed for OIDC_ISSUER_URL");

        Some(Self {
            issuer_url,
            client_id,
            client_secret,
            redirect_uri,
            required_scope,
            end_session_url,
            authorize_endpoint: discovery.authorization_endpoint,
            token_endpoint: discovery.token_endpoint,
            jwks_uri: discovery.jwks_uri,
            mcp_issuer_url,
            mcp_client_id,
        })
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
        self.preferred_username
            .clone()
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
///   1. `Authorization: Bearer <token>` — used by MCP and curl/scripts.
///   2. The `auth` cookie — used by the browser SPA.
///
/// Returns `401 Unauthorized` for any failure. Successful requests pass
/// through with the request body intact; the caller can extract claims with
/// `Extension<Claims>` or the `Claims` extractor.
pub async fn auth_middleware(
    State(state): State<crate::routes::boards::AppState>,
    headers: HeaderMap,
    cookies: CookieJar,
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
            picture: None,
            scope: None,
            iss: "auth-disabled".to_string(),
            exp: u64::MAX,
        };
        req.extensions_mut().insert(claims);
        return next.run(req).await;
    };

    let token = extract_bearer(&headers)
        .or_else(|| cookies.get(AUTH_COOKIE).map(|c| c.value().to_string()));
    let Some(token) = token else {
        return (StatusCode::UNAUTHORIZED, "missing token").into_response();
    };
    let jwks = state
        .jwks_cache
        .as_ref()
        .expect("jwks_cache must be present when auth is configured");
    match validate_jwt(&token, auth, jwks).await {
        Ok(claims) => {
            req.extensions_mut().insert(claims);
            next.run(req).await
        }
        Err(reason) => {
            tracing::warn!(reason, "auth middleware rejected request");
            (StatusCode::UNAUTHORIZED, reason).into_response()
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
