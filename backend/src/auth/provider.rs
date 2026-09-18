//! `AuthConfig` — the OIDC provider's endpoints, resolved once at startup via
//! the discovery document.

use serde::Deserialize;

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
