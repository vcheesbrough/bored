//! Pasted images (card #67): upload, serve, cache, and what the routes refuse.
//!
//! Expected values are written out by hand from the card (header values, MIME
//! types, status codes) and the fixtures are hand-built byte strings, never
//! produced by the detector under test.

use super::*;
use axum::body::Bytes;
use axum::http::Method;

/// A real 1×1 transparent PNG (68 bytes, generated once with Python's
/// `zlib` and checked to decode), so the same bytes also render in a browser.
/// The first eight bytes are the PNG signature.
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x60, 0x00, 0x02, 0x00,
    0x00, 0x05, 0x00, 0x01, 0x7a, 0x5e, 0xab, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];
const JPEG: &[u8] = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01";
const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;";
const WEBP: &[u8] = b"RIFF\x1a\x00\x00\x00WEBPVP8L\x0d\x00\x00\x00";
const SVG: &[u8] = b"<?xml version=\"1.0\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\" \
width=\"1\" height=\"1\"><script>document.title='pwned'</script></svg>";

/// The caching policy, as the card states it.
const CACHE_CONTROL: &str = "private, max-age=31536000, immutable";
const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; sandbox";

/// Upload `bytes` with a declared content type and return the response body.
async fn upload(server: &TestServer, bytes: &[u8], declared: &str) -> shared::UploadedImage {
    let response = server
        .post("/api/images")
        .content_type(declared)
        .bytes(Bytes::copy_from_slice(bytes))
        .await;
    response.assert_status(StatusCode::CREATED);
    response.json()
}

/// The text of a response header, panicking with its name if it is missing.
fn header(response: &axum_test::TestResponse, name: &str) -> String {
    response
        .maybe_header(name)
        .unwrap_or_else(|| panic!("no {name} header"))
        .to_str()
        .expect("ascii header")
        .to_string()
}

#[tokio::test]
async fn every_format_round_trips_byte_for_byte_with_its_detected_type() {
    let server = test_app().await;
    let cases: [(&[u8], &str); 5] = [
        (PNG_1X1, "image/png"),
        (JPEG, "image/jpeg"),
        (GIF, "image/gif"),
        (WEBP, "image/webp"),
        (SVG, "image/svg+xml"),
    ];
    for (bytes, mime) in cases {
        // The declared type is deliberately wrong: the stored one must come
        // from the bytes.
        let uploaded = upload(&server, bytes, "application/octet-stream").await;
        assert_eq!(uploaded.url, format!("/api/images/{}", uploaded.id));

        let response = server.get(&uploaded.url).await;
        response.assert_status_ok();
        assert_eq!(response.as_bytes().as_ref(), bytes, "{mime}");
        assert_eq!(header(&response, "content-type"), mime);
        assert_eq!(header(&response, "x-content-type-options"), "nosniff");
        assert_eq!(header(&response, "content-security-policy"), CSP);
        assert_eq!(header(&response, "content-disposition"), "inline");
        assert_eq!(header(&response, "cache-control"), CACHE_CONTROL);
        assert_eq!(header(&response, "etag"), format!("\"{}\"", uploaded.id));
        // An HTTP-date: `Sat, 28 Nov 2026 10:20:30 GMT`.
        let last_modified = header(&response, "last-modified");
        assert!(
            httpdate::parse_http_date(&last_modified).is_ok(),
            "{last_modified:?}"
        );
    }
}

#[tokio::test]
async fn ids_are_fresh_version_4_uuids_even_for_identical_bytes() {
    let server = test_app().await;
    let first = upload(&server, PNG_1X1, "image/png").await;
    let second = upload(&server, PNG_1X1, "image/png").await;
    assert_ne!(first.id, second.id, "no de-duplication");
    for uploaded in [&first, &second] {
        let id = uuid::Uuid::parse_str(&uploaded.id).expect("a uuid");
        assert_eq!(id.get_version_num(), 4);
        // Lowercase and hyphenated, as the URL and ETag spell it.
        assert_eq!(uploaded.id, id.to_string());
        server.get(&uploaded.url).await.assert_status_ok();
    }
}

#[tokio::test]
async fn the_upload_answer_is_never_stored() {
    let server = test_app().await;
    let response = server
        .post("/api/images")
        .content_type("image/png")
        .bytes(Bytes::from_static(PNG_1X1))
        .await;
    response.assert_status(StatusCode::CREATED);
    assert_eq!(header(&response, "cache-control"), "no-store");
}

#[tokio::test]
async fn a_matching_if_none_match_is_a_304_with_the_validators() {
    let server = test_app().await;
    let uploaded = upload(&server, PNG_1X1, "image/png").await;
    let etag = format!("\"{}\"", uploaded.id);

    let response = server
        .get(&uploaded.url)
        .add_header("if-none-match", etag.clone())
        .await;
    response.assert_status(StatusCode::NOT_MODIFIED);
    assert!(response.as_bytes().is_empty());
    assert_eq!(header(&response, "etag"), etag);
    assert_eq!(header(&response, "cache-control"), CACHE_CONTROL);

    // A weak tag and a list both match too (the weak comparison).
    for value in [format!("W/{etag}"), format!("\"other\", {etag}")] {
        server
            .get(&uploaded.url)
            .add_header("if-none-match", value)
            .await
            .assert_status(StatusCode::NOT_MODIFIED);
    }

    // Control: a different tag gets the full image.
    let response = server
        .get(&uploaded.url)
        .add_header("if-none-match", "\"something-else\"")
        .await;
    response.assert_status_ok();
    assert_eq!(response.as_bytes().as_ref(), PNG_1X1);
}

#[tokio::test]
async fn if_none_match_star_is_a_304_only_for_an_image_that_exists() {
    let server = test_app().await;
    let uploaded = upload(&server, PNG_1X1, "image/png").await;
    server
        .get(&uploaded.url)
        .add_header("if-none-match", "*")
        .await
        .assert_status(StatusCode::NOT_MODIFIED);
    server
        .get(&format!("/api/images/{}", uuid::Uuid::new_v4()))
        .add_header("if-none-match", "*")
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn if_modified_since_is_a_304_from_the_upload_second_onwards() {
    let server = test_app().await;
    let uploaded = upload(&server, PNG_1X1, "image/png").await;
    let last_modified = header(&server.get(&uploaded.url).await, "last-modified");

    // Echoing our own `Last-Modified` back: not modified.
    let response = server
        .get(&uploaded.url)
        .add_header("if-modified-since", last_modified.clone())
        .await;
    response.assert_status(StatusCode::NOT_MODIFIED);
    assert!(response.as_bytes().is_empty());
    assert_eq!(header(&response, "last-modified"), last_modified);

    // Later than that: still not modified.
    server
        .get(&uploaded.url)
        .add_header("if-modified-since", "Fri, 31 Dec 9999 23:59:59 GMT")
        .await
        .assert_status(StatusCode::NOT_MODIFIED);

    // Before the upload: the full image.
    let response = server
        .get(&uploaded.url)
        .add_header("if-modified-since", "Thu, 01 Jan 1970 00:00:00 GMT")
        .await;
    response.assert_status_ok();
    assert_eq!(response.as_bytes().as_ref(), PNG_1X1);

    // RFC 9110: `If-Modified-Since` is ignored when `If-None-Match` is there.
    server
        .get(&uploaded.url)
        .add_header("if-none-match", "\"something-else\"")
        .add_header("if-modified-since", last_modified)
        .await
        .assert_status_ok();

    // An unparseable date is ignored, not an error.
    server
        .get(&uploaded.url)
        .add_header("if-modified-since", "yesterday")
        .await
        .assert_status_ok();
}

#[tokio::test]
async fn head_answers_the_headers_without_the_body() {
    let server = test_app().await;
    let uploaded = upload(&server, PNG_1X1, "image/png").await;
    let response = server.method(Method::HEAD, &uploaded.url).await;
    response.assert_status_ok();
    assert!(response.as_bytes().is_empty());
    assert_eq!(header(&response, "content-type"), "image/png");
    assert_eq!(header(&response, "cache-control"), CACHE_CONTROL);
    assert_eq!(header(&response, "etag"), format!("\"{}\"", uploaded.id));
}

#[tokio::test]
async fn a_10_mib_image_is_accepted_and_one_byte_more_is_413() {
    let server = test_app().await;
    let limit = crate::routes::images::MAX_IMAGE_BYTES;
    // Hand-checked: the card's cap is 10 MiB.
    assert_eq!(limit, 10_485_760);

    // A PNG signature padded out to exactly the limit.
    let mut at_limit = PNG_1X1.to_vec();
    at_limit.resize(limit, 0);
    let uploaded = upload(&server, &at_limit, "image/png").await;
    let response = server.get(&uploaded.url).await;
    response.assert_status_ok();
    assert_eq!(response.as_bytes().len(), limit);

    let mut over = at_limit;
    over.push(0);
    let response = server
        .post("/api/images")
        .content_type("image/png")
        .bytes(Bytes::from(over))
        .await;
    response.assert_status(StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(header(&response, "cache-control"), "no-store");
}

#[tokio::test]
async fn bytes_that_are_no_accepted_format_are_415() {
    let server = test_app().await;
    let cases: [&[u8]; 4] = [
        b"<!DOCTYPE html><html><body><script>alert(1)</script></body></html>",
        b"<?xml version=\"1.0\"?><root/>",
        b"",
        b"%PDF-1.7",
    ];
    for bytes in cases {
        // Declared as an image: the header must not get it accepted.
        let response = server
            .post("/api/images")
            .content_type("image/png")
            .bytes(Bytes::copy_from_slice(bytes))
            .await;
        response.assert_status(StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(header(&response, "cache-control"), "no-store");
    }
}

#[tokio::test]
async fn unknown_and_malformed_ids_are_404_and_not_stored() {
    let server = test_app().await;
    for path in [
        format!("/api/images/{}", uuid::Uuid::new_v4()),
        "/api/images/not-a-uuid".to_string(),
        "/api/images/01kpkmm9ytysndxgzv586svckv".to_string(),
    ] {
        let response = server.get(&path).await;
        response.assert_status(StatusCode::NOT_FOUND);
        assert_eq!(header(&response, "cache-control"), "no-store", "{path}");
    }
}

#[tokio::test]
async fn images_cannot_be_deleted() {
    let server = test_app().await;
    let uploaded = upload(&server, PNG_1X1, "image/png").await;
    // The path exists for GET, so another method is "not allowed".
    server
        .delete(&uploaded.url)
        .await
        .assert_status(StatusCode::METHOD_NOT_ALLOWED);
    server.get(&uploaded.url).await.assert_status_ok();
}

#[tokio::test]
async fn an_image_outlives_the_card_that_referenced_it() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;
    let uploaded = upload(&server, PNG_1X1, "image/png").await;
    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: format!("# With image\n\n![image]({})", uploaded.url),
            ..Default::default()
        })
        .await
        .json();
    server
        .delete(&format!("/api/cards/{}", card.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let response = server.get(&uploaded.url).await;
    response.assert_status_ok();
    assert_eq!(response.as_bytes().as_ref(), PNG_1X1);
}

#[tokio::test]
async fn without_a_session_both_routes_are_401_and_not_stored() {
    let server = TestServer::new(
        app(
            auth_enabled_state(None).await,
            "./dist",
            DeploymentInfo::new("dev", None),
        )
        .await,
    )
    .expect("test server");
    let get = server
        .get(&format!("/api/images/{}", uuid::Uuid::new_v4()))
        .await;
    get.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(header(&get, "cache-control"), "no-store");

    let post = server
        .post("/api/images")
        .content_type("image/png")
        .bytes(Bytes::from_static(PNG_1X1))
        .await;
    post.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(header(&post, "cache-control"), "no-store");
}
