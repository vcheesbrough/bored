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
//
// Layout — one file per responsibility, re-exported here so callers keep
// writing `crate::auth::Claims`, `crate::auth::auth_middleware`, etc.:
//   * `provider`   — `AuthConfig`: the OIDC provider's endpoints (discovery).
//   * `session`    — browser sessions: encrypted cookies, refresh-token rotation.
//   * `jwt`        — `JwksCache`, `Claims` and `validate_jwt`.
//   * `middleware` — the axum layer that ties the three together per request.

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

/// Static cookie name for the short-lived state nonce used during the
/// authorization-code exchange to defeat CSRF on the callback.
pub const STATE_COOKIE: &str = "auth_state";
