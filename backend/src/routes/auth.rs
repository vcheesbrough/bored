// OIDC authorization-code flow + session endpoints.
//
// Three browser-facing routes implement the standard server-side OAuth2
// authorization-code exchange:
//
//   GET /auth/login    — start the flow: generate state → cookie → redirect
//   GET /auth/callback — finish the flow: verify state → exchange code → set session cookie
//   POST /auth/logout  — clear session cookie → redirect to RP-initiated logout
//   GET /auth/logout   — a confirm page whose only control POSTs the above
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
//
// Logout is a POST (card #535). As a GET, any third-party page could sign a
// bored user out with `<img src="https://bored…/auth/logout">`. SameSite=Lax
// alone does not close that hole for a POST either: a cross-site form POST
// carries no session cookies, but it is a top-level navigation, and browsers
// *do* apply `Set-Cookie` from the response to one — so the handler's
// "expire every session cookie" headers would still sign the user out
// locally. Hence the cross-origin check in `logout` (see
// `is_cross_origin_request`), which refuses such a POST before it touches a
// cookie.

use axum::{
    Extension, Json,
    extract::Host,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, Uri, header},
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;

use crate::auth::{Claims, STATE_COOKIE};
use crate::redact;
use crate::routes::boards::AppState;
use tracing::Instrument as _;

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
#[tracing::instrument(skip_all)]
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
    //
    // `telemetry:write` too (card #416, decision D1): the SPA presents this
    // session's access token to the telemetry ingest, which refuses a token
    // without that scope. Requested unconditionally — a provider without the
    // mapping simply does not grant it, and `/api/telemetry/token` then
    // declines to hand the token out.
    let authorize = match url::Url::parse_with_params(
        auth.authorize_url(),
        &[
            ("response_type", "code"),
            ("client_id", auth.client_id.as_str()),
            ("redirect_uri", auth.redirect_uri.as_str()),
            (
                "scope",
                &format!(
                    "openid profile email offline_access {} {}",
                    auth.required_scope,
                    crate::auth::TELEMETRY_SCOPE
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
#[tracing::instrument(skip_all)]
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
        // `err` comes from a query string anyone can type, so only a value
        // shaped like an RFC 6749 error code is logged (card #366).
        tracing::warn!(
            error = redact::oauth_error_code(err),
            "auth callback received error"
        );
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
    // A state change worth a line of its own (card #415 §4). No identity on
    // it: the user's `sub` would be an identifier in a log field, which the
    // span already scopes, and #366's policy keeps tokens out entirely.
    tracing::info!("browser session created");
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

/// The page `GET /auth/logout` answers with: a one-button form that POSTs to
/// the real logout route (card #535).
///
/// Why keep a GET at all, instead of letting axum answer 405? A tab that was
/// open across the deploy still runs the old SPA, whose "sign out" is a plain
/// link to `GET /auth/logout`. Without this page that click would land on a
/// bare "405 Method Not Allowed" and the user would stay signed in. With it,
/// they get one extra click. Typing the URL by hand works the same way.
///
/// The page changes no state, so a third party embedding it gains nothing —
/// except by framing it and tricking the user into the click (clickjacking).
/// `frame-ancestors 'none'` (and the older `X-Frame-Options: DENY`) forbid
/// framing; see `logout_confirm`.
///
/// `&'static str`: the markup is a string literal baked into the binary, so
/// it lives for the whole program and needs no allocation per request.
const LOGOUT_CONFIRM_PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Sign out of bored</title>
<style>
  body { font-family: system-ui, sans-serif; display: grid; place-items: center; min-height: 90vh; }
  form { text-align: center; }
  button { font: inherit; padding: 0.4em 1.2em; cursor: pointer; }
</style>
</head>
<body>
<form method="post" action="/auth/logout">
  <p>Sign out of bored?</p>
  <button type="submit">Sign out</button>
  <p><a href="/">Cancel</a></p>
</form>
</body>
</html>
"#;

/// `GET /auth/logout` — show the confirm form; never clears a cookie.
///
/// Returning a tuple of `(headers, body)` is axum's shorthand for "these
/// extra headers on top of whatever the body sets": `Html` contributes
/// `Content-Type: text/html; charset=utf-8`, the array adds the rest. Each
/// array entry is a `(HeaderName, &str)` pair, which axum knows how to turn
/// into response headers.
#[tracing::instrument(skip_all)]
pub async fn logout_confirm() -> impl IntoResponse {
    (
        [
            // Never cache: the page is tiny and a cached copy has no value.
            (header::CACHE_CONTROL, "no-store"),
            // Anti-clickjacking, old and new spelling. `frame-ancestors`
            // lives in CSP; `default-src 'none'` + `style-src 'unsafe-inline'`
            // lets only the inline <style> above load, and `form-action
            // 'self'` pins where the one form may submit.
            (header::X_FRAME_OPTIONS, "DENY"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; \
                 form-action 'self'; frame-ancestors 'none'",
            ),
        ],
        Html(LOGOUT_CONFIRM_PAGE),
    )
}

/// Does this request come from a page on another origin? Used to refuse a
/// cross-site logout POST (card #535; see the module comment for why
/// SameSite=Lax alone is not enough).
///
/// The algorithm is the one Go's standard library ships as
/// `net/http.CrossOriginProtection`, chosen because it is small, has been
/// reviewed, and needs no configuration (no list of trusted origins):
///
/// 1. **`Sec-Fetch-Site`** — every current browser sends it, and the page
///    cannot forge it. `same-origin` is the SPA's own form; `none` is a
///    user-initiated navigation (typed URL, bookmark). Anything else —
///    `cross-site`, `same-site` (a sibling subdomain) — is refused.
/// 2. Otherwise **`Origin`** — older browsers send it on every POST. It must
///    name this server's own host (`host` is what the request was addressed
///    to). `Origin: null` (sandboxed frames, privacy modes) is refused too.
/// 3. **Neither header** — not a browser, or one too old to send either. A
///    CSRF needs a victim's browser, and a non-browser client could simply
///    *omit* the headers anyway, so it is let through. The e2e suite's bare
///    HTTP requests rely on this.
///
/// `host` is an `Option<&str>`: `None` when the request named no host at all
/// (no `Host` header and no authority in the URI), which only a hand-made
/// request does. With an `Origin` to compare against, that counts as a
/// mismatch.
pub fn is_cross_origin_request(headers: &HeaderMap, host: Option<&str>) -> bool {
    // `HeaderMap::get` returns `Option<&HeaderValue>`; `if let Some(..)`
    // runs the block only when the header is present.
    if let Some(site) = headers.get("sec-fetch-site") {
        // `as_bytes()` compares the raw value without first converting it to
        // a `&str` (which can fail on non-ASCII bytes). `matches!` is a
        // boolean `match`: true when the value is one of the listed
        // patterns. The leading `!` turns "is same-origin" into "is cross".
        return !matches!(site.as_bytes(), b"same-origin" | b"none");
    }
    // `let … else` is the early-return form of `if let`: bind `origin` when
    // the header is there, otherwise run the `else` block, which must leave
    // the function (here: "no Origin either → not a browser CSRF").
    let Some(origin) = headers.get(header::ORIGIN) else {
        return false;
    };
    // An `Origin` value is `scheme://host[:port]`. `to_str()` fails on
    // non-ASCII bytes, `split_once("://")` fails on `null` or garbage; the
    // `?`-free chain of `Option`/`Result` combinators means any of those
    // failures turns into `None` here — and `None` is refused below.
    let origin_host = origin
        .to_str()
        .ok()
        .and_then(|value| value.split_once("://"))
        .map(|(_scheme, authority)| authority);
    // Both sides must be present and equal. Host names are case-insensitive
    // (RFC 3986 §3.2.2), so compare ignoring ASCII case. `match` on a tuple
    // of the two `Option`s reads as a small truth table.
    match (origin_host, host) {
        (Some(origin_host), Some(host)) => !origin_host.eq_ignore_ascii_case(host),
        _ => true,
    }
}

/// `POST /auth/logout` — clear the session cookie and (optionally) bounce to
/// Authentik's RP-initiated logout endpoint to terminate the upstream session.
///
/// A POST from another origin is refused with 403 before any cookie is
/// touched (card #535): `is_cross_origin_request` explains the check.
///
/// `Option<Host>`: axum's `Host` extractor reads the host the request was
/// sent to (the `Host` header, or the URI's authority under HTTP/2). Wrapping
/// it in `Option` means "give me `None` if there is none" instead of
/// rejecting the request outright.
#[tracing::instrument(skip_all)]
pub async fn logout(
    State(state): State<AppState>,
    host: Option<Host>,
    headers: HeaderMap,
    jar: CookieJar,
) -> Response {
    // `host.as_ref()` borrows the `Option<Host>` as `Option<&Host>`, and
    // `map(|Host(h)| h.as_str())` destructures the tuple struct to borrow
    // its inner `String` as `&str` — no copy of the host is made.
    if is_cross_origin_request(&headers, host.as_ref().map(|Host(h)| h.as_str())) {
        // No log line: the server span already records the 403 on this
        // route, and that is the one record a refused request gets.
        return (StatusCode::FORBIDDEN, "cross-origin logout refused").into_response();
    }

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
    let private_jar = sessions.clear_session(private_jar);
    tracing::info!("browser session ended by logout");

    if let Some(refresh_token) = refresh_token.as_deref() {
        let refresh_chain = sessions.invalidate_refresh_chain(refresh_token).await;
        let sessions = std::sync::Arc::clone(sessions);
        let auth = std::sync::Arc::clone(auth);
        // The revocation outlives the logout request, so it is detached work:
        // its own trace (`parent: None`), *linked* to the request that queued
        // it rather than a child of it — a child would hold the request's
        // trace open for as long as the identity provider takes to answer.
        // `Instrument` carries the span into the spawned task; without it the
        // task would run outside any span and its revocation calls would each
        // start an orphaned trace (skill §5, rust.md "Spawned tasks").
        let revocation_span = tracing::info_span!(
            parent: None,
            "logout revocation",
            // As `i64`: the span bridge exports a `usize`/`u64` field as a string.
            bored.revocations = i64::try_from(refresh_chain.len()).unwrap_or(i64::MAX),
        );
        crate::observability::link_to(&revocation_span, &tracing::Span::current());
        let _revocation_task = tokio::spawn(
            async move {
                for refresh_token in refresh_chain {
                    if let Err(error) = sessions.revoke_refresh_token(&auth, &refresh_token).await
                    {
                        // Local logout must not be held hostage by provider availability.
                        tracing::warn!(error = %error, "refresh-token revocation failed during logout");
                    }
                }
            }
            .instrument(revocation_span),
        );
    }

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
#[tracing::instrument(skip_all)]
pub async fn me(claims: Extension<Claims>) -> Json<shared::UserInfo> {
    Json(claims.to_user_info())
}

#[cfg(test)]
mod tests {
    use super::{is_cross_origin_request, safe_return_to, state_return_to};
    use axum::http::HeaderMap;
    use base64::Engine;

    /// Build a `HeaderMap` from `(name, value)` pairs. `HeaderMap::insert`
    /// wants typed names and values; `parse()` converts each `&str` (and
    /// `unwrap` is fine in a test: the literals are all valid).
    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    }

    #[test]
    fn fetch_metadata_decides_when_present() {
        let host = Some("bored.example");
        // Allowed: the app's own page, and a user-initiated navigation.
        for site in ["same-origin", "none"] {
            assert!(
                !is_cross_origin_request(&headers(&[("sec-fetch-site", site)]), host),
                "{site}"
            );
        }
        // Refused: any other site, including a sibling subdomain.
        for site in ["cross-site", "same-site", "garbage"] {
            assert!(
                is_cross_origin_request(&headers(&[("sec-fetch-site", site)]), host),
                "{site}"
            );
        }
        // Fetch metadata wins over Origin: a matching Origin cannot rescue a
        // cross-site request, and a mismatching one cannot sink a same-origin
        // one (it is the browser's verdict, which the page cannot forge).
        assert!(is_cross_origin_request(
            &headers(&[
                ("sec-fetch-site", "cross-site"),
                ("origin", "https://bored.example")
            ]),
            host
        ));
        assert!(!is_cross_origin_request(
            &headers(&[
                ("sec-fetch-site", "same-origin"),
                ("origin", "https://elsewhere.example")
            ]),
            host
        ));
    }

    #[test]
    fn origin_must_name_this_host_when_fetch_metadata_is_absent() {
        let host = Some("bored.example:8443");
        let check =
            |origin: &'static str| is_cross_origin_request(&headers(&[("origin", origin)]), host);
        // Same host and port, any scheme spelling, any letter case.
        assert!(!check("https://bored.example:8443"));
        assert!(!check("https://BORED.example:8443"));
        // A different host, a different port, an opaque or malformed origin.
        assert!(check("https://evil.example"));
        assert!(check("https://bored.example"));
        assert!(check("https://bored.example:8443.evil.example"));
        assert!(check("null"));
        assert!(check("bored.example:8443"));
        // An Origin with nothing to compare it to is refused.
        assert!(is_cross_origin_request(
            &headers(&[("origin", "https://bored.example:8443")]),
            None
        ));
    }

    #[test]
    fn no_browser_headers_is_not_cross_origin() {
        // A non-browser client; with or without a host.
        assert!(!is_cross_origin_request(
            &HeaderMap::new(),
            Some("bored.example")
        ));
        assert!(!is_cross_origin_request(&HeaderMap::new(), None));
    }

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
