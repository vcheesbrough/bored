//! Pasted images (card #67): `POST /api/images` stores one, and
//! `GET /api/images/{id}` serves it back.
//!
//! # The model
//!
//! An image is a row in `images` (Postgres `bytea`). A card body refers to it
//! only by URL, `![image](/api/images/{id})`, so no card owns an image: cut
//! and paste can carry that markdown anywhere. Images are **permanent and
//! immutable**. There is no delete or update route, because history restore
//! can bring back any old body and every image it names must still load. The
//! bytes behind an id never change, which is what makes year-long browser
//! caching safe.
//!
//! # Memory
//!
//! Both directions hold the whole image in memory: the upload is buffered
//! (`Bytes`) and the download is one `bytea` value. Postgres sends a column
//! value whole, so it cannot be streamed without a different storage design.
//! The 10 MiB cap ([`MAX_IMAGE_BYTES`]) is the only guard, by decision on the
//! card: a worst-case transfer holds roughly 20–30 MB, well inside the
//! container's 256 MB.
//!
//! # Serving untrusted bytes safely
//!
//! Anything a user uploads may be hostile. An SVG can carry `<script>`, and a
//! polyglot JPEG can also be valid HTML. What makes serving them safe on
//! bored's own origin is the response, not the format check:
//!
//! - `Content-Type` is the type detected from the bytes ([`detect`]), never
//!   the uploader's header.
//! - `X-Content-Type-Options: nosniff` stops the browser second-guessing that
//!   type, so an image is never rendered as HTML or run as a `<script src>`.
//! - `Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline';
//!   sandbox` applies when an image is opened as a document ("Open image in
//!   new tab"). The document gets an opaque origin with scripts disabled, so
//!   an SVG's script cannot run, let alone call `/api` with the viewer's
//!   session. Inline styles stay allowed so an SVG still draws as intended.
//!   Inside an `<img>`, browsers never run an SVG's scripts anyway.
//!
//! Deeper hardening (re-encoding, sanitising SVG, a separate cookieless
//! origin) is card #639.

pub(crate) mod detect;

use axum::{
    Extension, Json,
    body::Bytes,
    extract::{Path, State, rejection::BytesRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::auth::Claims;
use crate::db::{DbOperation, DbQuery, Traced as _};
use crate::error::ApiError;
use crate::routes::boards::{AppState, editor_sub};
use detect::ImageType;

/// The largest image the upload route accepts: 10 MiB, from `shared` so the
/// SPA checks the same number before it sends anything.
///
/// Applied as the route's `DefaultBodyLimit` (app.rs). axum's `Bytes`
/// extractor stops reading at this many bytes and rejects the request, which
/// [`upload_image`] turns into a 413.
pub(crate) const MAX_IMAGE_BYTES: usize = shared::MAX_IMAGE_BYTES;

/// Every successful image response's caching policy.
///
/// - `private`: the response sits behind auth, so only the viewer's own
///   browser may keep it, never a shared cache.
/// - `max-age=31536000` (one year) with `immutable`: the browser reuses its
///   copy across board loads, card opens, history previews and reloads
///   without asking the server again. Safe because an id's bytes never change.
const IMAGE_CACHE_CONTROL: &str = "private, max-age=31536000, immutable";

/// What the browser may do with an image opened as a document (see the module
/// docs): nothing may load, inline styles may apply, and the document is
/// sandboxed (opaque origin, no scripts).
const IMAGE_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; sandbox";

/// For responses that must never be stored: the upload's answer, and errors.
const NO_STORE: &str = "no-store";

/// `POST /api/images`: store the request body as a new image.
///
/// The body is the raw image bytes (not multipart): the SPA sends the pasted
/// `Blob` as it is. Answers `201 {id, url}`, `413` over [`MAX_IMAGE_BYTES`],
/// or `415` when the bytes are none of the five accepted formats.
///
/// Returns a bare `Response` rather than `Result<_, ApiError>` because one
/// failure is neither ours nor an `ApiError`: a body that could not be read at
/// all (the client hung up mid-upload) answers with axum's own rejection, as
/// a malformed JSON body does on the other routes.
pub async fn upload_image(
    State(state): State<AppState>,
    claims: Extension<Claims>,
    // `Result<Bytes, BytesRejection>` instead of `Bytes`: with plain `Bytes`,
    // axum would answer an oversized body itself, bypassing `ApiError`, and
    // the request's span and metric would lose their `error.type`. The body
    // extractor must be the handler's last argument.
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(body) => body,
        // The limit layer reports "too large" as a 413 rejection.
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return ApiError::PayloadTooLarge.into_response();
        }
        Err(rejection) => return rejection.into_response(),
    };
    match store(&state, &claims, &body).await {
        Ok(uploaded) => (
            StatusCode::CREATED,
            [(header::CACHE_CONTROL, NO_STORE)],
            Json(uploaded),
        )
            .into_response(),
        Err(error) => error.into_response(),
    }
}

/// Detect, record and insert one upload.
async fn store(
    state: &AppState,
    claims: &Extension<Claims>,
    body: &[u8],
) -> Result<shared::UploadedImage, ApiError> {
    let image_type = detect::detect(body).ok_or(ApiError::UnsupportedMediaType)?;
    // At most `MAX_IMAGE_BYTES` (10 MiB), so it always fits an `i32`, which
    // is Postgres's `integer`. `try_from` makes that a checked fact, not a
    // silent truncating cast.
    let size = i32::try_from(body.len()).map_err(|_| ApiError::PayloadTooLarge)?;
    record_on_request_span(image_type, size);

    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO images (id, content_type, bytes, size, created_by) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(image_type.mime())
    .bind(body)
    .bind(size)
    .bind(editor_sub(claims))
    .execute(&state.db)
    .traced(DbQuery::new(
        "images.upload_image",
        DbOperation::Insert,
        "images",
    ))
    .await?;

    // `Uuid`'s `Display` is the lowercase hyphenated form, which is also what
    // the GET route compares an `ETag` against.
    let id = id.to_string();
    Ok(shared::UploadedImage {
        url: format!("/api/images/{id}"),
        id,
    })
}

/// One stored image, as the GET route reads it.
///
/// `sqlx::FromRow` maps the selected columns to these fields by name.
#[derive(sqlx::FromRow)]
struct StoredImage {
    content_type: String,
    bytes: Vec<u8>,
    size: i32,
    created_at: DateTime<Utc>,
}

/// `GET /api/images/{id}` (and `HEAD`, which axum answers from the same
/// handler with the body left off).
///
/// The id is taken as a `String` and parsed here rather than extracted as a
/// `Uuid`, so a malformed id is the same 404 as an unknown one, not axum's
/// 400.
pub async fn get_image(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::NotFound)?;
    let etag = entity_tag(id);

    // RFC 9110 §13.1.2: when `If-None-Match` is present, `If-Modified-Since`
    // is ignored.
    if let Some(if_none_match) = header_str(&headers, header::IF_NONE_MATCH) {
        match none_match(if_none_match, &etag) {
            // The browser already has exactly these bytes. Images never
            // change or disappear, so it can be told so without a database
            // read. Every id gets the same answer, so nothing leaks.
            NoneMatch::Hit => return Ok(not_modified(&etag, None)),
            // `*` means "any current version": true only if the image exists.
            NoneMatch::Wildcard => {
                return match select_created_at(&state, id).await? {
                    Some(created_at) => Ok(not_modified(&etag, Some(created_at))),
                    None => Err(ApiError::NotFound),
                };
            }
            // A different version: fall through to a full response.
            NoneMatch::Miss => {}
        }
    } else if let Some(since) = header_str(&headers, header::IF_MODIFIED_SINCE)
        // An unparseable date is ignored, as RFC 9110 requires.
        .and_then(|value| httpdate::parse_http_date(value).ok())
    {
        // A metadata-only read: just `created_at`, never the bytes.
        let Some(created_at) = select_created_at(&state, id).await? else {
            return Err(ApiError::NotFound);
        };
        if http_date_of(created_at) <= since {
            return Ok(not_modified(&etag, Some(created_at)));
        }
    }

    let image: Option<StoredImage> =
        sqlx::query_as("SELECT content_type, bytes, size, created_at FROM images WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .traced(DbQuery::new(
                "images.get_image",
                DbOperation::Select,
                "images",
            ))
            .await?;
    let image = image.ok_or(ApiError::NotFound)?;

    // Only ever emit one of the five MIME types, whatever the row says.
    let image_type = ImageType::from_mime(&image.content_type)
        .ok_or_else(|| ApiError::internal("stored image has an unknown content type"))?;
    record_on_request_span(image_type, image.size);

    let mut response = image.bytes.into_response();
    let response_headers = response.headers_mut();
    response_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(image_type.mime()),
    );
    response_headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response_headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(IMAGE_CSP),
    );
    response_headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("inline"),
    );
    insert_validators(response_headers, &etag, Some(image.created_at));
    Ok(response)
}

/// `created_at` for one image, or `None` if there is no such image.
async fn select_created_at(state: &AppState, id: Uuid) -> Result<Option<DateTime<Utc>>, ApiError> {
    // `(DateTime<Utc>,)` is a one-element tuple: sqlx decodes a row into a
    // tuple when there is no named struct for it.
    let row: Option<(DateTime<Utc>,)> =
        sqlx::query_as("SELECT created_at FROM images WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .traced(DbQuery::new(
                "images.image_meta",
                DbOperation::Select,
                "images",
            ))
            .await?;
    Ok(row.map(|(created_at,)| created_at))
}

/// A `304 Not Modified`: no body, plus the validators and caching policy the
/// full response would carry (RFC 9110 §15.4.5), so the browser refreshes its
/// stored copy's metadata.
fn not_modified(etag: &str, created_at: Option<DateTime<Utc>>) -> Response {
    let mut response = StatusCode::NOT_MODIFIED.into_response();
    insert_validators(response.headers_mut(), etag, created_at);
    response
}

/// `ETag`, `Last-Modified` (when known) and `Cache-Control`.
fn insert_validators(headers: &mut HeaderMap, etag: &str, created_at: Option<DateTime<Utc>>) {
    // Both values are built from characters that are always valid in a
    // header (hex digits, hyphens, quotes; an HTTP-date), so a failure here
    // would be a bug. `if let Ok` skips the header rather than panicking.
    if let Ok(value) = HeaderValue::from_str(etag) {
        headers.insert(header::ETAG, value);
    }
    if let Some(created_at) = created_at
        && let Ok(value) = HeaderValue::from_str(&httpdate::fmt_http_date(http_date_of(created_at)))
    {
        headers.insert(header::LAST_MODIFIED, value);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(IMAGE_CACHE_CONTROL),
    );
}

/// The image's strong entity tag: its id, quoted.
///
/// An id names one immutable set of bytes, so the id *is* the version; there
/// is nothing to hash.
fn entity_tag(id: Uuid) -> String {
    format!("\"{id}\"")
}

/// `created_at` as the instant an HTTP-date can express: whole seconds.
///
/// `Last-Modified` has one-second resolution and Postgres stores
/// microseconds. Comparing the truncated value is what makes an
/// `If-Modified-Since` that echoes our own `Last-Modified` count as "not
/// modified".
fn http_date_of(created_at: DateTime<Utc>) -> SystemTime {
    // `timestamp()` is whole seconds since the Unix epoch. An image is never
    // created before 1970, so a negative value is clamped to the epoch rather
    // than wrapped.
    let seconds = u64::try_from(created_at.timestamp()).unwrap_or(0);
    UNIX_EPOCH + Duration::from_secs(seconds)
}

/// A header's value as text, or `None` when it is absent or not visible ASCII.
fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// What an `If-None-Match` header says about our entity tag.
#[derive(Debug, PartialEq, Eq)]
enum NoneMatch {
    /// One of the listed tags is ours.
    Hit,
    /// `*`: "any current representation".
    Wildcard,
    /// None of the listed tags is ours.
    Miss,
}

/// Compare an `If-None-Match` value with `etag`.
///
/// The header is `*` or a comma-separated list of entity tags. RFC 9110
/// §13.1.2 says to use the *weak* comparison here, so a `W/"…"` tag matches
/// our strong `"…"` tag with the same value.
fn none_match(header: &str, etag: &str) -> NoneMatch {
    if header.trim() == "*" {
        return NoneMatch::Wildcard;
    }
    let ours = etag.trim_start_matches("W/");
    let hit = header
        .split(',')
        .map(|tag| tag.trim().trim_start_matches("W/"))
        .any(|tag| tag == ours);
    if hit { NoneMatch::Hit } else { NoneMatch::Miss }
}

/// Put the image's size and type on the request's server span
/// (`server_span.rs` declares both fields as empty when it opens the span).
///
/// The handler runs inside that span, so `Span::current()` is it. Both values
/// are bounded: a number, and one of five MIME literals.
fn record_on_request_span(image_type: ImageType, size: i32) {
    let span = tracing::Span::current();
    span.record("bored.image.size", i64::from(size));
    span.record("bored.image.content_type", image_type.mime());
}

/// A response layer for the image routes: any answer that is neither a
/// success nor a `304` gets `Cache-Control: no-store`, unless it already says
/// something about caching.
///
/// It wraps the auth layer too (app.rs), so the auth middleware's 401 is
/// covered as well as this module's own 404s, 413s and 415s. Without it, a
/// 404 could be cached by a browser's heuristics and keep hiding an image.
pub(crate) async fn no_store_errors(mut response: Response) -> Response {
    let status = response.status();
    let cacheable = status.is_success() || status == StatusCode::NOT_MODIFIED;
    if !cacheable && !response.headers().contains_key(header::CACHE_CONTROL) {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static(NO_STORE));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    const ETAG: &str = "\"0b6f9a4e-3f51-4a8e-9b7e-2f1d6c5a4b3c\"";

    #[test]
    fn if_none_match_compares_weakly_against_any_listed_tag() {
        assert_eq!(none_match(ETAG, ETAG), NoneMatch::Hit);
        assert_eq!(
            none_match("W/\"0b6f9a4e-3f51-4a8e-9b7e-2f1d6c5a4b3c\"", ETAG),
            NoneMatch::Hit
        );
        assert_eq!(
            none_match("\"other\", \"0b6f9a4e-3f51-4a8e-9b7e-2f1d6c5a4b3c\"", ETAG),
            NoneMatch::Hit
        );
        assert_eq!(none_match("\"other\"", ETAG), NoneMatch::Miss);
        // Unquoted is not the same tag.
        assert_eq!(
            none_match("0b6f9a4e-3f51-4a8e-9b7e-2f1d6c5a4b3c", ETAG),
            NoneMatch::Miss
        );
        assert_eq!(none_match(" * ", ETAG), NoneMatch::Wildcard);
    }

    #[test]
    fn the_entity_tag_is_the_quoted_lowercase_id() {
        let id = Uuid::parse_str("0B6F9A4E-3F51-4A8E-9B7E-2F1D6C5A4B3C").expect("a uuid");
        assert_eq!(entity_tag(id), ETAG);
    }

    #[test]
    fn http_dates_drop_the_fraction_of_a_second() {
        let created_at = DateTime::parse_from_rfc3339("2026-11-28T10:20:30.987654Z")
            .expect("a date")
            .with_timezone(&Utc);
        // 2026-11-28T10:20:30Z, worked out by hand: 20 785 days after the
        // epoch × 86 400 s + 10 h 20 min 30 s.
        let expected = UNIX_EPOCH + Duration::from_secs(20_785 * 86_400 + 37_230);
        assert_eq!(http_date_of(created_at), expected);
        assert_eq!(
            httpdate::fmt_http_date(http_date_of(created_at)),
            "Sat, 28 Nov 2026 10:20:30 GMT"
        );
    }
}
