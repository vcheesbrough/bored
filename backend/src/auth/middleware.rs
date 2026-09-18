//! The axum auth layer: find a token (Bearer header, then session cookies),
//! validate it, refresh the browser session when needed, inject `Claims`.

use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use super::{Claims, session::access_needs_refresh, validate_jwt};

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
