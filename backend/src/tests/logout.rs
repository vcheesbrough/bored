//! `/auth/logout` (card #535): GET only shows a confirm form, POST ends the
//! session, and a POST from another origin is refused before it touches a
//! cookie.
//!
//! The expected values here are read straight off the raw `Set-Cookie` and
//! `Location` headers, not through the session manager the handler uses, so
//! a bug in how the handler builds its cookies cannot also hide in the check.

use super::*;
use axum::http::header;
use serial_test::serial;

/// Where the fake provider's RP-initiated logout lives. The handler appends
/// `id_token_hint` to it.
const END_SESSION_URL: &str = "https://idp.invalid/application/o/bored-test/end-session/";

/// The ID token the minted session carries; logout must forward it as the
/// `id_token_hint`.
const ID_TOKEN: &str = "test-id-token";

/// An auth-enabled server plus a `Cookie` header value that holds a complete,
/// validly encrypted browser session (`auth`, `auth_refresh`, `auth_id`).
///
/// The cookies are minted with the server's own `AuthSessionManager`: the
/// tests need a session the handler can *decrypt* (to read the ID token
/// hint and the refresh token), and only that manager's key can produce one.
async fn server_with_session() -> (TestServer, String) {
    let state = auth_enabled_state(Some(END_SESSION_URL.to_string())).await;
    // `Option::as_ref` + `expect` borrows the `Arc` inside the state rather
    // than moving it out; `Arc::clone` then gives this function its own
    // handle while the state keeps the original.
    let sessions = Arc::clone(state.auth_sessions.as_ref().expect("auth is on"));

    let tokens = crate::auth::TokenSet {
        access_token: "test-access-token".to_string(),
        refresh_token: "test-refresh-token".to_string(),
        id_token: Some(ID_TOKEN.to_string()),
        access_expires_in: 900,
    };
    // `write_session` returns a `PrivateCookieJar` holding the encrypted
    // cookies. Turning the jar into a response is the simplest way to see
    // them as a browser would: as `Set-Cookie` header lines.
    let jar = sessions.write_session(sessions.cookie_jar(&axum::http::HeaderMap::new()), &tokens);
    let minted = axum::response::IntoResponse::into_response(jar);
    let set_cookies = set_cookie_lines(minted.headers());

    // The card's SameSite guard: every session cookie is `SameSite=Lax`, so
    // a cross-site POST arrives without them. (Lax, not Strict: Strict would
    // drop the cookies on the redirect back from the identity provider.)
    assert_eq!(set_cookies.len(), 3, "auth, auth_refresh and auth_id");
    for line in &set_cookies {
        assert!(line.contains("SameSite=Lax"), "not Lax: {line}");
    }

    // A browser sends back only `name=value`, the part before the first `;`.
    let cookie_header = set_cookies
        .iter()
        .map(|line| line.split(';').next().expect("split yields one part"))
        .collect::<Vec<_>>()
        .join("; ");

    let server = TestServer::new(app(state, "./dist", DeploymentInfo::new("test", None)).await)
        .expect("test server starts");
    (server, cookie_header)
}

/// Every `Set-Cookie` line on a response, as plain strings. `get_all` is
/// needed because a response can carry the header many times.
fn set_cookie_lines(headers: &axum::http::HeaderMap) -> Vec<String> {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().expect("ASCII cookie").to_string())
        .collect()
}

/// The names of the cookies a response *expires* (`Max-Age=0`), sorted so
/// the comparison does not depend on header order.
fn expired_cookie_names(headers: &axum::http::HeaderMap) -> Vec<String> {
    let mut names: Vec<String> = set_cookie_lines(headers)
        .into_iter()
        .filter(|line| line.contains("Max-Age=0"))
        .map(|line| line.split('=').next().unwrap_or_default().to_string())
        .collect();
    names.sort();
    names
}

#[tokio::test]
#[serial]
async fn get_shows_a_confirm_form_and_clears_no_cookie() {
    let (server, cookies) = server_with_session().await;

    // Capture logs too: a GET must not log the "session ended" line either.
    let (response, logs) = capture_logs_async(async {
        server
            .get("/auth/logout")
            .add_header("cookie", cookies.clone())
            .await
    })
    .await;

    response.assert_status(StatusCode::OK);
    // Not a single Set-Cookie: the old GET expired all four cookies here.
    assert_eq!(set_cookie_lines(response.headers()), Vec::<String>::new());
    assert!(!logs.contains("browser session ended by logout"), "{logs}");

    // The page is a form that POSTs to the real route.
    let content_type = response.header(header::CONTENT_TYPE);
    assert!(
        content_type.to_str().unwrap().starts_with("text/html"),
        "{content_type:?}"
    );
    let body = response.text();
    assert!(
        body.contains(r#"<form method="post" action="/auth/logout">"#),
        "{body}"
    );
    assert!(body.contains(r#"<button type="submit">"#), "{body}");

    // …which no other site may frame (clickjacking the one button).
    assert_eq!(response.header(header::X_FRAME_OPTIONS), "DENY");
    let csp = response.header(header::CONTENT_SECURITY_POLICY);
    let csp = csp.to_str().unwrap();
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    // …but must not restrict `form-action`: the browser applies it to the
    // POST's redirect too, and that redirect leaves for the provider's
    // end-session URL. (e2e proves the hop in a real browser.)
    assert!(!csp.contains("form-action"), "{csp}");
}

#[tokio::test]
#[serial]
async fn same_origin_post_clears_the_session_and_redirects_to_end_session() {
    let (server, cookies) = server_with_session().await;

    let (response, logs) = capture_logs_async(async {
        server
            .post("/auth/logout")
            .add_header("cookie", cookies.clone())
            // What a browser sends for the SPA's own form.
            .add_header("sec-fetch-site", "same-origin")
            .add_header("origin", "https://bored.test")
            .add_header("host", "bored.test")
            .await
    })
    .await;

    // 303 See Other: the browser follows it with a GET, as it must for the
    // provider's end-session page.
    response.assert_status(StatusCode::SEE_OTHER);
    let location = response.header(header::LOCATION);
    let location = url::Url::parse(location.to_str().unwrap()).expect("absolute URL");
    assert!(
        location.as_str().starts_with(END_SESSION_URL),
        "redirects to end-session: {location}"
    );
    // `query_pairs` decodes the query string into `(key, value)` pairs.
    let hint = location
        .query_pairs()
        .find(|(key, _)| key == "id_token_hint")
        .map(|(_, value)| value.into_owned());
    assert_eq!(hint.as_deref(), Some(ID_TOKEN));

    // All three session cookies and the login-state nonce are expired.
    assert_eq!(
        expired_cookie_names(response.headers()),
        ["auth", "auth_id", "auth_refresh", "auth_state"]
    );
    // The telemetry the card keeps: the one state-change line.
    assert!(logs.contains("browser session ended by logout"), "{logs}");
}

#[tokio::test]
#[serial]
async fn post_with_a_matching_origin_and_no_fetch_metadata_is_allowed() {
    // An older browser: `Origin` but no `Sec-Fetch-Site`. Same host → allowed.
    let (server, cookies) = server_with_session().await;
    let response = server
        .post("/auth/logout")
        .add_header("cookie", cookies)
        .add_header("origin", "https://Bored.Test")
        .add_header("host", "bored.test")
        .await;
    response.assert_status(StatusCode::SEE_OTHER);
    assert_eq!(expired_cookie_names(response.headers()).len(), 4);
}

#[tokio::test]
#[serial]
async fn post_without_browser_headers_is_allowed() {
    // A non-browser client sends neither header; it cannot be a CSRF victim,
    // and the e2e suite's bare HTTP logout relies on this.
    let (server, cookies) = server_with_session().await;
    let response = server
        .post("/auth/logout")
        .add_header("cookie", cookies)
        .await;
    response.assert_status(StatusCode::SEE_OTHER);
    assert_eq!(expired_cookie_names(response.headers()).len(), 4);
}

#[tokio::test]
#[serial]
async fn cross_origin_posts_are_refused_and_clear_no_cookie() {
    let (server, cookies) = server_with_session().await;

    // Each case is a set of headers a page on another origin produces.
    // `&[(&str, &str)]` is a borrowed slice of string pairs.
    let cases: [&[(&str, &str)]; 4] = [
        // A modern browser on another site.
        &[
            ("sec-fetch-site", "cross-site"),
            ("origin", "https://evil.example"),
        ],
        // A sibling subdomain: same *site*, different origin.
        &[
            ("sec-fetch-site", "same-site"),
            ("origin", "https://other.bored.test"),
        ],
        // An older browser on another site: Origin only.
        &[("origin", "https://evil.example")],
        // A sandboxed frame or privacy mode: opaque origin.
        &[("origin", "null")],
    ];
    for headers in cases {
        let mut request = server
            .post("/auth/logout")
            .add_header("cookie", cookies.clone())
            .add_header("host", "bored.test");
        // `TestRequest` methods take `self` by value and hand it back, so
        // the loop rebinds `request` each time.
        for (name, value) in headers {
            request = request.add_header(*name, *value);
        }
        // `TestRequest` is `IntoFuture`, not `Future`, so it is awaited
        // inside an async block; `move` hands the block ownership of it.
        let (response, logs) = capture_logs_async(async move { request.await }).await;

        response.assert_status(StatusCode::FORBIDDEN);
        // The point of the check: the response sets no cookie at all, so a
        // browser that follows it keeps its session.
        assert_eq!(
            set_cookie_lines(response.headers()),
            Vec::<String>::new(),
            "{headers:?}"
        );
        assert!(
            response.maybe_header(header::LOCATION).is_none(),
            "{headers:?}"
        );
        assert!(!logs.contains("browser session ended by logout"), "{logs}");
    }
}

#[tokio::test]
#[serial]
async fn auth_disabled_post_returns_home_and_get_still_only_shows_the_form() {
    // Without OIDC there is no session to end; POST just goes home, and the
    // GET form is the same page.
    let server = test_app().await;
    let response = server.post("/auth/logout").await;
    response.assert_status(StatusCode::SEE_OTHER);
    assert_eq!(response.header(header::LOCATION), "/");

    let page = server.get("/auth/logout").await;
    page.assert_status(StatusCode::OK);
    assert_eq!(set_cookie_lines(page.headers()), Vec::<String>::new());
    assert!(page.text().contains(r#"action="/auth/logout""#));
}
