mod observability;

use axum::{routing::get, Router};
use axum_server::tls_rustls::RustlsConfig;
use std::net::SocketAddr;
use tower_http::trace::TraceLayer;

pub fn app() -> Router {
    Router::new()
        .route("/health", get(health))
        .layer(TraceLayer::new_for_http())
}

#[tokio::main]
async fn main() {
    let _obs = observability::init();

    let cert = std::env::var("TLS_CERT");
    let key = std::env::var("TLS_KEY");

    match (cert, key) {
        (Ok(cert), Ok(key)) => {
            let config = RustlsConfig::from_pem_file(&cert, &key)
                .await
                .expect("failed to load TLS config");
            let addr = SocketAddr::from(([0, 0, 0, 0], 443));
            tracing::info!(%addr, "bored backend listening (TLS)");
            axum_server::bind_rustls(addr, config)
                .serve(app().into_make_service())
                .await
                .unwrap();
        }
        _ => {
            let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
            tracing::info!(%addr, "bored backend listening (plain HTTP)");
            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            axum::serve(listener, app()).await.unwrap();
        }
    }
}

async fn health() -> &'static str {
    "ok"
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_handler_returns_ok() {
        assert_eq!(health().await, "ok");
    }

    #[tokio::test]
    async fn health_route_returns_200() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
