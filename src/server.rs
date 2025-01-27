use anyhow::{anyhow, Result};
use axum::{routing::get, Router};
use tokio::net::TcpListener;

async fn health() -> &'static str {
    "feel my heartbeat moving to the beat"
}

pub async fn serve(port: u16) -> Result<()> {
    let app = Router::new().route("/health", get(health));
    let listener = TcpListener::bind(&format!("0.0.0.0:{port}")).await.unwrap();
    axum::serve(listener, app).await.map_err(|e| anyhow!(e))
}
