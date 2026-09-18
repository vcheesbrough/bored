//! Static-file service for the Leptos SPA with an `index.html` fallback.

use tower_http::services::ServeDir;

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
                let (parts, body) = resp.into_parts();
                Ok(axum::http::Response::from_parts(
                    parts,
                    axum::body::Body::new(body),
                ))
            }
        })
    }
}
