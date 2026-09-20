//! Static-file service for the Leptos SPA with an `index.html` fallback.

use tower_http::services::ServeDir;

/// Sent with the SPA document so a browser always revalidates it.
///
/// `no-cache` does not mean "do not store" — it means "store it, but ask me
/// before reusing it", so the usual 304 still saves the transfer. It matters
/// because the frontend reloads itself when the server reports a version it
/// was not built from (see `frontend/src/connection.rs`): `index.html` is the
/// one file whose URL never changes across deploys — trunk fingerprints the
/// wasm, JS and CSS it links to — so a heuristically cached copy of it would
/// hand the reload the very bundle it was trying to escape.
const SPA_DOCUMENT_CACHE_CONTROL: &str = "no-cache";

// Wraps ServeDir and replaces any 404 response with index.html so that SPA
// deep-links (e.g. /boards/123) survive a browser reload.
// tower-http 0.6's ServeDir::not_found_service does not fire for paths that
// don't exist on disk, so we intercept the 404 response after the fact.
#[derive(Clone)]
pub struct SpaSvc {
    inner: ServeDir,
    index_path: std::path::PathBuf,
}

impl SpaSvc {
    pub fn new(static_dir: &str) -> Self {
        Self {
            inner: ServeDir::new(static_dir),
            index_path: std::path::Path::new(static_dir).join("index.html"),
        }
    }
}

impl tower::Service<axum::http::Request<axum::body::Body>> for SpaSvc {
    type Response = axum::http::Response<axum::body::Body>;
    type Error = std::convert::Infallible;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        // ServeDir is always ready; delegating here would reserve readiness on
        // self.inner, but call() clones it — so the reservation would be discarded.
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: axum::http::Request<axum::body::Body>) -> Self::Future {
        use axum::http::StatusCode;
        use tower::ServiceExt;
        let inner = self.inner.clone();
        let index_path = self.index_path.clone();
        Box::pin(async move {
            // ServeDir is infallible in tower-http 0.6
            let resp = inner.oneshot(req).await.unwrap();
            if resp.status() == StatusCode::NOT_FOUND {
                match tokio::fs::read(&index_path).await {
                    Ok(bytes) => Ok(axum::http::Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/html; charset=utf-8")
                        .header("cache-control", SPA_DOCUMENT_CACHE_CONTROL)
                        .body(axum::body::Body::from(bytes))
                        .expect("static index.html response is always valid")),
                    // index.html itself is missing — pass through the 404
                    Err(_) => {
                        let (parts, body) = resp.into_parts();
                        Ok(axum::http::Response::from_parts(
                            parts,
                            axum::body::Body::new(body),
                        ))
                    }
                }
            } else {
                let (mut parts, body) = resp.into_parts();
                // `/` and any other path that really is a file on disk come
                // from ServeDir, not from the fallback above — so the
                // revalidation header has to be added here too. Keyed on the
                // content type rather than the path so it covers exactly the
                // HTML documents and never the fingerprinted assets, which are
                // safe to cache hard precisely because their URLs change.
                let is_html = parts
                    .headers
                    .get(axum::http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.starts_with("text/html"));
                if is_html {
                    parts.headers.insert(
                        axum::http::header::CACHE_CONTROL,
                        axum::http::HeaderValue::from_static(SPA_DOCUMENT_CACHE_CONTROL),
                    );
                }
                Ok(axum::http::Response::from_parts(
                    parts,
                    axum::body::Body::new(body),
                ))
            }
        })
    }
}
