//! OIDC authentication module.
//!
//! This module is the security perimeter of the backend. It validates incoming
//! JWTs against an external OIDC provider (Authentik in production, a mock
//! server in E2E tests) and injects the resulting `Claims` into request
//! extensions for downstream handlers to consume.
//!
//! Two token sources are accepted:
//! 1. The `auth` httpOnly cookie — set by the browser-facing auth flow
//!    (`/auth/login` → `/auth/callback`).
//! 2. The `Authorization: Bearer <token>` header — used by the MCP service,
//!    which obtains its own token via the OAuth2 client_credentials grant.
//!
//! JWT verification flow:
//! * Decode the unverified header to read the `kid` (key id).
//! * Look up the matching JWKS public key in the in-memory cache; on miss,
//!   re-fetch the provider's `/.well-known/jwks.json` once and retry. This
//!   handles routine key rotation without an explicit refresh trigger.
//! * Verify the signature, `aud`, `iss`, and `exp`/`nbf`.
//! * Confirm the token's `scope` claim (space-separated list) contains the
//!   environment-specific `REQUIRED_SCOPE` (e.g. `bored:dev:access`).
//!
//! The middleware returns `401 Unauthorized` for any failure — invalid token,
//! missing scope, expired, wrong issuer/audience. There is no `403` path here:
//! scope failures are treated as 401 because from the protocol's point of view
//! the presented token is simply not valid for this resource.
//!
//! Layout — one file per responsibility, re-exported here so callers keep
//! writing `crate::auth::Claims`, `crate::auth::auth_middleware`, etc.:
//! * `provider`   — `AuthConfig`: the OIDC provider's endpoints (discovery).
//! * `session`    — browser sessions: encrypted cookies, refresh-token rotation.
//! * `jwt`        — `JwksCache`, `Claims` and `validate_jwt`.
//! * `middleware` — the axum layer that ties the three together per request.

mod jwt;
mod middleware;
mod provider;
mod session;

// `pub use` re-exports an item under this module's path. The submodules above
// stay private, so `crate::auth::…` remains the only way in.
pub use jwt::{Claims, JwksCache, validate_jwt};
pub use middleware::auth_middleware;
pub use provider::AuthConfig;
pub use session::AuthSessionManager;

/// Static cookie name for the persistent session token.
/// Kept in one place so the login/callback/logout routes and the middleware
/// extractor stay in sync.
pub const AUTH_COOKIE: &str = "auth";

/// Encrypted rotating refresh token for browser sessions.
pub const REFRESH_COOKIE: &str = "auth_refresh";

/// Encrypted OIDC ID token retained solely as an RP-initiated logout hint.
pub const ID_COOKIE: &str = "auth_id";

/// The browser session's current access token, placed in request extensions
/// by [`auth_middleware`] **only** when the request authenticated with the
/// session cookies — never for a bearer caller. Read by exactly one handler,
/// `GET /api/telemetry/token` (card #416), which hands it to the SPA for the
/// telemetry ingest.
///
/// Fields are private and `Debug` is written by hand, so the token cannot be
/// printed by accident: the only way to it is [`SessionAccessToken::token`].
#[derive(Clone)]
pub struct SessionAccessToken {
    token: String,
    exp: u64,
    scope: Option<String>,
}

impl SessionAccessToken {
    pub fn new(token: String, exp: u64, scope: Option<String>) -> Self {
        Self { token, exp, scope }
    }

    /// Built from a token the middleware has just validated, and its claims.
    pub(crate) fn from_claims(token: String, claims: &Claims) -> Self {
        Self::new(token, claims.exp, claims.scope.clone())
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// Expiry, seconds since the Unix epoch.
    pub fn exp(&self) -> u64 {
        self.exp
    }

    /// Whether the token's space-separated `scope` holds `wanted` as a whole
    /// word (so `telemetry:writer` does not count as `telemetry:write`).
    pub fn has_scope(&self, wanted: &str) -> bool {
        self.scope
            .as_deref()
            .is_some_and(|scope| scope.split_whitespace().any(|s| s == wanted))
    }
}

impl std::fmt::Debug for SessionAccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionAccessToken")
            .field("token", &"[REDACTED]")
            .field("exp", &self.exp)
            .field("scope", &self.scope)
            .finish()
    }
}

/// The scope bored's telemetry ingest requires on the bearer the SPA presents
/// (card #416). Requested at login; `/api/telemetry/token` hands out only a
/// session token that carries it.
pub const TELEMETRY_SCOPE: &str = "telemetry:write";

/// Static cookie name for the short-lived state nonce used during the
/// authorization-code exchange to defeat CSRF on the callback.
pub const STATE_COOKIE: &str = "auth_state";
