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

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    Extension, Json,
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;

use crate::auth::{Claims, STATE_COOKIE};
use crate::routes::boards::AppState;

/// Cookie max-age (seconds) for the auth state nonce. Five minutes is more
/// than enough time for a user to complete the redirect to Authentik, log in,
/// and bounce back. Anything longer is just a wider attack window.
const STATE_COOKIE_MAX_AGE_SECS: i64 = 300;

#[derive(Debug, Default, Deserialize)]
pub struct LoginQuery {
    return_to: Option<String>,
}

fn safe_return_to(candidate: Option<&str>) -> String {
    let Some(candidate) = candidate else {
        return "/".to_string();
    };
    if !candidate.starts_with('/')
        || candidate.starts_with("//")
        || candidate.contains('\\')
        || candidate.chars().any(char::is_control)
    {
        return "/".to_string();
    }
    match candidate.parse::<Uri>() {
        Ok(uri) if uri.scheme().is_none() && uri.authority().is_none() => candidate.to_string(),
        _ => "/".to_string(),
    }
}

fn state_return_to(state: &str) -> String {
    let decoded = state
        .split_once('.')
        .and_then(|(_, encoded)| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded)
                .ok()
        })
        .and_then(|bytes| String::from_utf8(bytes).ok());
    safe_return_to(decoded.as_deref())
}

/// `GET /auth/login` — start the OIDC authorization-code flow.
/// Generates a random state nonce, stores it in a short-lived httpOnly
/// cookie, and redirects the browser to Authentik's authorize endpoint.
pub async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(params): Query<LoginQuery>,
) -> Response {
    let return_to = safe_return_to(params.return_to.as_deref());
    let Some(auth) = state.auth.as_ref() else {
        // Auth disabled — bounce back directly. The middleware will inject
        // the synthetic anonymous claim for any subsequent API call.
        return Redirect::to(&return_to).into_response();
    };

    // 32 random bytes → base64url, no padding. ~256 bits of entropy is
    // overkill for CSRF defence but cheap to generate.
    let mut nonce_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(nonce_bytes);
    let encoded_return_to =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(return_to.as_bytes());
    // The cookie and IdP both receive the complete value, so the existing
    // equality check protects the return path along with the random nonce.
    let oauth_state = format!("{nonce}.{encoded_return_to}");

    // Build the authorize URL. We request the standard openid+profile+email
    // scopes plus the env-specific access scope so the issued token will
    // pass the middleware's scope check on subsequent requests.
    let authorize = match url::Url::parse_with_params(
        auth.authorize_url(),
        &[
            ("response_type", "code"),
            ("client_id", auth.client_id.as_str()),
            ("redirect_uri", auth.redirect_uri.as_str()),
            (
                "scope",
                &format!(
                    "openid profile email offline_access {}",
                    auth.required_scope
                ),
            ),
            ("state", &oauth_state),
        ],
    ) {
        Ok(u) => u,
        Err(e) => {
            // The base URL came from the IdP's discovery document at startup,
            // so this should be unreachable in practice. Return 500 instead
            // of panicking — handler panics are caught by Axum but pollute
            // logs and obscure the real cause.
            tracing::error!(error = %e, "IdP authorization_endpoint URL is invalid");
            return (StatusCode::INTERNAL_SERVER_ERROR, "invalid IdP URL").into_response();
        }
    };

    let state_cookie = Cookie::build((STATE_COOKIE, oauth_state))
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
    /// Authorization code to exchange for tokens. Optional because per
    /// RFC 6749 §4.1.2.1 the provider omits `code` and instead returns
    /// `error` on the failure path; making it required here would cause
    /// Axum's `Query` extractor to 422 the request before our `error`
    /// guard below ever runs.
    code: Option<String>,
    /// State nonce we generated at /auth/login. Must match the cookie value.
    state: String,
    /// Authentik returns `error` instead of `code` if the user denies consent
    /// or the policy binding rejects them. Surfaced as a friendly error.
    #[serde(default)]
    error: Option<String>,
}

/// `GET /auth/callback` — receive Authentik's redirect with the auth code,
/// verify state, exchange code for tokens, and set the session cookie.
pub async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
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
    let return_to = state_return_to(&params.state);

    // `code` is `Option` to keep the error-path deserialise-able; on the
    // success path it must be present, so reject the callback otherwise.
    let Some(code) = params.code else {
        return (StatusCode::BAD_REQUEST, "missing code").into_response();
    };

    let sessions = state
        .auth_sessions
        .as_ref()
        .expect("auth_sessions present when auth configured");
    let token_set = match sessions.exchange_authorization_code(auth, &code).await {
        Ok(tokens) => tokens,
        Err(error) => {
            tracing::error!(error = %error, "authorization-code token exchange failed");
            return (StatusCode::BAD_GATEWAY, "token exchange failed").into_response();
        }
    };

    // Validate the freshly-issued token before trusting it as a session.
    // This catches scope/aud/iss misconfiguration immediately rather than
    // letting a bad token sit in the cookie until the next API call.
    let jwks = state
        .jwks_cache
        .as_ref()
        .expect("jwks_cache present when auth configured");
    if let Err(reason) = crate::auth::validate_jwt(&token_set.access_token, auth, jwks).await {
        tracing::warn!(reason, "issued access token failed validation");
        return (StatusCode::FORBIDDEN, "issued token failed validation").into_response();
    }

    // Store the complete token set in independently encrypted private cookies
    // and clear the one-use state nonce.
    let private_jar = sessions.write_session(sessions.cookie_jar(&headers), &token_set);
    let clear_state = Cookie::build((STATE_COOKIE, ""))
        .path("/auth")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::ZERO)
        .build();
    let jar = jar.add(clear_state);

    let response = (private_jar, Redirect::to(&return_to)).into_response();
    (jar, response).into_response()
}

/// `GET /auth/logout` — clear the session cookie and (optionally) bounce to
/// Authentik's RP-initiated logout endpoint to terminate the upstream session.
pub async fn logout(State(state): State<AppState>, headers: HeaderMap, jar: CookieJar) -> Response {
    let clear_state = Cookie::build((STATE_COOKIE, ""))
        .path("/auth")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::ZERO)
        .build();
    let jar = jar.add(clear_state);

    let Some(auth) = state.auth.as_ref() else {
        return (jar, Redirect::to("/")).into_response();
    };
    let sessions = state
        .auth_sessions
        .as_ref()
        .expect("auth_sessions present when auth configured");
    let private_jar = sessions.cookie_jar(&headers);
    let refresh_token = sessions.read_refresh_token(&private_jar);
    let id_token = sessions.read_id_token(&private_jar);

    if let Some(refresh_token) = refresh_token.as_deref() {
        if let Err(error) = sessions.revoke_refresh_token(auth, refresh_token).await {
            // Local logout must not be held hostage by provider availability.
            tracing::warn!(error = %error, "refresh-token revocation failed during logout");
        }
    }
    let private_jar = sessions.clear_session(private_jar);

    let mut target = auth
        .end_session_url
        .clone()
        .unwrap_or_else(|| "/".to_string());
    if let (Some(id_token), Ok(mut url)) = (id_token, url::Url::parse(&target)) {
        url.query_pairs_mut()
            .append_pair("id_token_hint", &id_token);
        target = url.into();
    }

    let response = (private_jar, Redirect::to(&target)).into_response();
    (jar, response).into_response()
}

/// `GET /api/me` — return the public-facing user identity to the SPA.
/// Lives under `/api` so it's gated by the same auth middleware as the other
/// data endpoints; the navbar uses it to populate username + avatar.
pub async fn me(claims: Extension<Claims>) -> Json<shared::UserInfo> {
    Json(claims.to_user_info())
}

#[cfg(test)]
mod tests {
    use super::{safe_return_to, state_return_to};
    use base64::Engine;

    #[test]
    fn accepts_same_origin_return_paths() {
        assert_eq!(
            safe_return_to(Some("/boards/old-board?card=42#details")),
            "/boards/old-board?card=42#details"
        );
    }

    #[test]
    fn rejects_unsafe_return_targets() {
        for target in [
            "https://example.com/boards/a",
            "//example.com/boards/a",
            "/\\example.com/boards/a",
            "boards/a",
            "/boards/a\r\nlocation: https://example.com",
        ] {
            assert_eq!(safe_return_to(Some(target)), "/", "target: {target:?}");
        }
    }

    #[test]
    fn restores_return_path_from_oauth_state() {
        let path = "/boards/old-board?card=42";
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(path);
        assert_eq!(state_return_to(&format!("nonce.{encoded}")), path);
        assert_eq!(state_return_to("invalid"), "/");
    }
}
