use axum::{routing::get, Router};
use axum_server::tls_rustls::RustlsConfig;
use std::net::SocketAddr;

pub fn app() -> Router {
    Router::new().route("/health", get(health))
}

#[tokio::main]
async fn main() {
    let cert = std::env::var("TLS_CERT").unwrap_or_else(|_| "/app/cert.pem".to_string());
    let key = std::env::var("TLS_KEY").unwrap_or_else(|_| "/app/key.pem".to_string());

    let config = RustlsConfig::from_pem_file(&cert, &key)
        .await
        .expect("failed to load TLS config");

    let addr = SocketAddr::from(([0, 0, 0, 0], 443));
    println!("bored backend listening on :443");
    axum_server::bind_rustls(addr, config)
        .serve(app().into_make_service())
        .await
        .unwrap();
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
            .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
