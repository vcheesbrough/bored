use axum::{routing::get, Router};

#[tokio::main]
async fn main() {
    let app = Router::new().route("/health", get(health));
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("bored backend listening on :3000");
    axum::serve(listener, app).await.unwrap();
}

async fn health() -> &'static str {
    "ok"
}
