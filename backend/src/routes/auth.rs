// OIDC authorization-code flow + session endpoints.
//
// Three browser-facing routes implement the standard server-side OAuth2
// authorization-code exchange:
//
//   GET /auth/login    — start the flow: generate state → cookie → redirect
//   GET /auth/callback — finish the flow: verify state → exchange code → set session cookie
//   GET /auth/logout   — clear session cookie → redirect to RP-initiated logout
//
// One API route exposes the validated identity to the SPA:
//
//   GET /api/me        — return UserInfo from the request's claims (auth-gated)
//
// State (CSRF) handling: the authorize URL includes a random nonce in `state`;
// the same nonce is stored in a short-lived httpOnly cookie. On callback we
// require both to match. If they don't, the flow is aborted with 400 — the
// browser likely lost the cookie (third-party cookie blocking) or this is an
// attacker-initiated callback from another tab.
//
// Cookie attributes: `HttpOnly; Secure; SameSite=Lax`. SameSite=Lax permits
// the redirect from Authentik to attach the cookie on a top-level GET, which
// is the only flow we use here. Strict would break the callback.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    Extension, Json,
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;

use crate::auth::{Claims, AUTH_COOKIE, STATE_COOKIE};
use crate::routes::boards::AppState;

/// Cookie max-age (seconds) for the auth state nonce. Five minutes is more
/// than enough time for a user to complete the redirect to Authentik, log in,
/// and bounce back. Anything longer is just a wider attack window.
const STATE_COOKIE_MAX_AGE_SECS: i64 = 300;

/// Cookie max-age for the session token. We set this generously and let the
/// JWT's own `exp` claim be the source of truth — the middleware enforces
/// expiry, so an over-long cookie lifetime is harmless. (Tightening to match
/// the JWT exactly is a refresh-token-tier improvement.)
const AUTH_COOKIE_MAX_AGE_SECS: i64 = 60 * 60 * 24;

/// `GET /auth/login` — start the OIDC authorization-code flow.
/// Generates a random state nonce, stores it in a short-lived httpOnly
/// cookie, and redirects the browser to Authentik's authorize endpoint.
pub async fn login(State(state): State<AppState>, jar: CookieJar) -> Response {
    let Some(auth) = state.auth.as_ref() else {
        // Auth disabled — just bounce back to /. The middleware will inject
        // the synthetic anonymous claim for any subsequent API call.
        return Redirect::to("/").into_response();
    };

    // 32 random bytes → base64url, no padding. ~256 bits of entropy is
    // overkill for CSRF defence but cheap to generate.
    let mut nonce_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(nonce_bytes);

    // Build the authorize URL. We request the standard openid+profile+email
    // scopes plus the env-specific access scope so the issued token will
    // pass the middleware's scope check on subsequent requests.
    let authorize = url::Url::parse_with_params(
        auth.authorize_url(),
        &[
            ("response_type", "code"),
            ("client_id", auth.client_id.as_str()),
            ("redirect_uri", auth.redirect_uri.as_str()),
            (
                "scope",
                &format!("openid profile email {}", auth.required_scope),
            ),
            ("state", &nonce),
        ],
    )
    .expect("authorize URL must be valid");

    let state_cookie = Cookie::build((STATE_COOKIE, nonce))
        .path("/auth")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::seconds(STATE_COOKIE_MAX_AGE_SECS))
        .build();
    let jar = jar.add(state_cookie);

    (jar, Redirect::to(authorize.as_str())).into_response()
}

/// Query params delivered by Authentik on the callback redirect.
#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    /// Authorization code to exchange for tokens.
    code: String,
    /// State nonce we generated at /auth/login. Must match the cookie value.
    state: String,
    /// Authentik returns `error` instead of `code` if the user denies consent
    /// or the policy binding rejects them. Surfaced as a friendly error.
    #[serde(default)]
    error: Option<String>,
}

/// Token-endpoint response shape. Only the fields we use are deserialised;
/// other fields (refresh_token, id_token, token_type, …) are ignored. We
/// rely on the access_token alone — id_token is not required for our model
/// because all consumers use the access_token for `Authorization: Bearer`.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
}

/// `GET /auth/callback` — receive Authentik's redirect with the auth code,
/// verify state, exchange code for tokens, and set the session cookie.
pub async fn callback(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(params): Query<CallbackQuery>,
) -> Response {
    let Some(auth) = state.auth.as_ref() else {
        return Redirect::to("/").into_response();
    };

    if let Some(err) = &params.error {
        tracing::warn!(error = %err, "auth callback received error");
        return (
            StatusCode::FORBIDDEN,
            format!("authentication denied: {err}"),
        )
            .into_response();
    }

    // CSRF check — the state cookie must exist and match the query string.
    let cookie_state = jar.get(STATE_COOKIE).map(|c| c.value().to_string());
    let Some(cookie_state) = cookie_state else {
        return (StatusCode::BAD_REQUEST, "missing state cookie").into_response();
    };
    if cookie_state != params.state {
        return (StatusCode::BAD_REQUEST, "state mismatch").into_response();
    }

    // Exchange the code for an access token via the token endpoint. Authentik
    // expects `application/x-www-form-urlencoded` here per RFC 6749.
    let http = reqwest::Client::new();
    let token_response: TokenResponse = match http
        .post(auth.token_url())
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &params.code),
            ("redirect_uri", &auth.redirect_uri),
            ("client_id", &auth.client_id),
            ("client_secret", &auth.client_secret),
        ])
        .send()
        .await
    {
        Ok(resp) => match resp.error_for_status() {
            Ok(ok) => match ok.json().await {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(error = %e, "failed to parse token response");
                    return (StatusCode::BAD_GATEWAY, "token parse failed").into_response();
                }
            },
            Err(e) => {
                tracing::error!(error = %e, "token endpoint returned error");
                return (StatusCode::BAD_GATEWAY, "token exchange failed").into_response();
            }
        },
        Err(e) => {
            tracing::error!(error = %e, "token endpoint unreachable");
            return (StatusCode::BAD_GATEWAY, "token endpoint unreachable").into_response();
        }
    };

    // Validate the freshly-issued token before trusting it as a session.
    // This catches scope/aud/iss misconfiguration immediately rather than
    // letting a bad token sit in the cookie until the next API call.
    let jwks = state
        .jwks_cache
        .as_ref()
        .expect("jwks_cache present when auth configured");
    if let Err(reason) = crate::auth::validate_jwt(&token_response.access_token, auth, jwks).await {
        tracing::warn!(reason, "issued access token failed validation");
        return (StatusCode::FORBIDDEN, "issued token failed validation").into_response();
    }

    // Set the session cookie and clear the state nonce.
    let session = Cookie::build((AUTH_COOKIE, token_response.access_token))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::seconds(AUTH_COOKIE_MAX_AGE_SECS))
        .build();
    let clear_state = Cookie::build((STATE_COOKIE, ""))
        .path("/auth")
        .max_age(time::Duration::ZERO)
        .build();
    let jar = jar.add(session).add(clear_state);

    (jar, Redirect::to("/")).into_response()
}

/// `GET /auth/logout` — clear the session cookie and (optionally) bounce to
/// Authentik's RP-initiated logout endpoint to terminate the upstream session.
pub async fn logout(State(state): State<AppState>, jar: CookieJar) -> Response {
    let clear = Cookie::build((AUTH_COOKIE, ""))
        .path("/")
        .max_age(time::Duration::ZERO)
        .build();
    let jar = jar.add(clear);

    let target = state
        .auth
        .as_ref()
        .and_then(|a| a.end_session_url.clone())
        .unwrap_or_else(|| "/".to_string());

    (jar, Redirect::to(&target)).into_response()
}

/// `GET /api/me` — return the public-facing user identity to the SPA.
/// Lives under `/api` so it's gated by the same auth middleware as the other
/// data endpoints; the navbar uses it to populate username + avatar.
pub async fn me(claims: Extension<Claims>) -> Json<shared::UserInfo> {
    Json(claims.to_user_info())
}

/// Helper exposed for use by tests that want to construct a deterministic
/// timestamp without pulling in the chrono dependency. Not used in prod.
#[allow(dead_code)]
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
