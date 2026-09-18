//! JWT validation: the JWKS key cache, the `Claims` we read out of a token and
//! `validate_jwt`, which checks signature, issuer, audience, expiry and scope.

use std::collections::HashMap;

use axum::{
    extract::FromRequestParts,
    http::{StatusCode, request::Parts},
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use super::AuthConfig;

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
