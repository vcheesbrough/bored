//! `GET /api/telemetry/token` — the bearer the SPA presents to the telemetry
//! ingest (card #416, decision D1).
//!
//! # Why this route exists
//!
//! The ingest (`otlp-collector-oidc`) accepts exactly one credential: an
//! `Authorization: Bearer <access JWT>` carrying the `telemetry:write` scope
//! and bored's audience. The SPA, though, holds only an `HttpOnly` session
//! cookie — the access token lives inside bored's encrypted cookie jar, where
//! JavaScript cannot read it. This route is the one place that hands the
//! session's *current* access token to the page, and only for that purpose.
//!
//! It sits behind the ordinary auth middleware, so a token inside the refresh
//! window has already been refreshed through the session's single-flight
//! rotation before it gets here: "refreshed through the existing session".
//!
//! # What it refuses, and why
//!
//! - **`404`** when client telemetry is not configured (no endpoint, or auth
//!   disabled): there is nothing to send to, so no token is handed out.
//! - **`403`** for a caller that authenticated with a bearer header (the MCP
//!   service, a script): it already holds its token, and this route must never
//!   become a way to swap one credential for another.
//! - **`403`** for a session whose token lacks `telemetry:write` — one that
//!   began before the scope existed. The ingest would refuse it anyway; saying
//!   so here stops the SPA at once with a clear console line instead of after
//!   two refused exports. The next sign-in fixes it.
//!
//! # The cost, recorded
//!
//! The token handed out is the session's full bored API bearer for its
//! remaining lifetime (≤ 15 minutes). Script running in the page can already
//! call `/api` with the cookie, so what this adds is the ability to *exfiltrate*
//! a short-lived token — accepted on the card, with a narrower-audience token
//! (a separate telemetry provider) noted as the way to remove it.

use axum::{
    Extension, Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};

use crate::auth::{SessionAccessToken, TELEMETRY_SCOPE};

/// The handler. `enabled` is fixed when the router is built (the route is
/// mounted as a closure capturing it, like `/api/info`'s deployment facts);
/// `session` is present only when the middleware authenticated a browser
/// cookie session — `Option<Extension<…>>` makes its absence a value rather
/// than a rejection.
pub async fn token(enabled: bool, session: Option<Extension<SessionAccessToken>>) -> Response {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    // `Extension(inner)` destructures the wrapper to reach the value.
    decide(enabled, session.as_ref().map(|Extension(inner)| inner), now)
}

/// The decision, separated from the extractors so every branch is unit-tested
/// without a live identity provider.
fn decide(enabled: bool, session: Option<&SessionAccessToken>, now: u64) -> Response {
    if !enabled {
        return (StatusCode::NOT_FOUND, "client telemetry is not configured").into_response();
    }
    let Some(session) = session else {
        return (
            StatusCode::FORBIDDEN,
            "a telemetry token is only issued to a browser session",
        )
            .into_response();
    };
    if !session.has_scope(TELEMETRY_SCOPE) {
        return (
            StatusCode::FORBIDDEN,
            "this session's token lacks telemetry:write; sign in again",
        )
            .into_response();
    }
    let body = shared::TelemetryToken {
        access_token: session.token().to_string(),
        // Relative, so a browser with a wrong clock still refreshes on time.
        expires_in: session.exp().saturating_sub(now),
    };
    let mut response = Json(body).into_response();
    // A bearer must never be stored by the browser or anything in between.
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(scope: &str, exp: u64) -> SessionAccessToken {
        SessionAccessToken::new("the-access-token".to_string(), exp, Some(scope.to_string()))
    }

    async fn body_of(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn off_is_404_whatever_the_session() {
        let s = session("openid telemetry:write", 2_000);
        assert_eq!(
            decide(false, Some(&s), 1_000).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(decide(false, None, 1_000).status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_bearer_caller_gets_no_token() {
        // The middleware adds the extension only for a cookie session, so a
        // bearer-authenticated request arrives here without one.
        let response = decide(true, None, 1_000);
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!body_of(response).await.contains("the-access-token"));
    }

    #[tokio::test]
    async fn a_session_without_the_scope_gets_no_token() {
        let s = session("openid profile bored:dev:access", 2_000);
        let response = decide(true, Some(&s), 1_000);
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!body_of(response).await.contains("the-access-token"));
    }

    #[tokio::test]
    async fn the_scope_must_match_whole_not_as_a_prefix() {
        let s = session("openid telemetry:writer", 2_000);
        assert_eq!(
            decide(true, Some(&s), 1_000).status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn a_scoped_session_gets_its_token_with_a_relative_lifetime_and_no_store() {
        let s = session("openid bored:dev:access telemetry:write", 1_900);
        let response = decide(true, Some(&s), 1_000);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        let token: shared::TelemetryToken = serde_json::from_str(&body_of(response).await).unwrap();
        assert_eq!(token.access_token, "the-access-token");
        assert_eq!(token.expires_in, 900);
    }

    #[tokio::test]
    async fn an_already_expired_token_reports_zero_not_an_underflow() {
        let s = session("telemetry:write", 500);
        let response = decide(true, Some(&s), 1_000);
        let token: shared::TelemetryToken = serde_json::from_str(&body_of(response).await).unwrap();
        assert_eq!(token.expires_in, 0);
    }
}
