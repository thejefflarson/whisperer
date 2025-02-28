use anyhow::{anyhow, Result};
use axum::{routing::get, Router};
use tokio::net::TcpListener;

use crate::utils::shutdown_signal;

async fn health() -> &'static str {
    "feel my heartbeat moving to the beat"
}

pub async fn serve(port: u16) -> Result<()> {
    let app = Router::new().route("/health", get(health));
    let listener = TcpListener::bind(&format!("0.0.0.0:{port}")).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| anyhow!(e))
}
