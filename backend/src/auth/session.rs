//! Browser sessions: the encrypted cookie set, refresh-token rotation and the
//! single-flight refresh cache that stops concurrent requests replaying a
//! rotated refresh token.

use std::{
    collections::HashMap,
    sync::{Arc, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::http::HeaderMap;
use axum_extra::extract::cookie::{Cookie, Key, PrivateCookieJar, SameSite};
use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use super::{AUTH_COOKIE, AuthConfig, Claims, ID_COOKIE, JwksCache, REFRESH_COOKIE, validate_jwt};

const ACCESS_COOKIE_DEFAULT_AGE_SECS: i64 = 15 * 60;
const SESSION_COOKIE_MAX_AGE_SECS: i64 = 30 * 24 * 60 * 60;
const REFRESH_WINDOW_SECS: u64 = 60;
const REFRESH_CACHE_TTL: Duration = Duration::from_secs(120);
const REFRESH_CACHE_MAX_ENTRIES: usize = 1024;
const REFRESH_INVALIDATION_MAX_ENTRIES: usize = REFRESH_CACHE_MAX_ENTRIES * 2;
const REFRESH_INVALIDATION_TTL: Duration = Duration::from_secs(SESSION_COOKIE_MAX_AGE_SECS as u64);

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

pub(super) fn access_needs_refresh(token: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    use axum::{http::header, response::IntoResponse};
    use base64::Engine;

    use super::{
        AUTH_COOKIE, AuthSessionManager, Claims, ID_COOKIE, REFRESH_COOKIE, TokenEndpointResponse,
        TokenSet, access_needs_refresh,
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
