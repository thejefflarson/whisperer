use anyhow::{anyhow, Result};
use axum::{routing::get, Router};
use tokio::net::TcpListener;

async fn metrics() {
    unimplemented!()
}

pub async fn serve(port: u16) -> Result<()> {
    let app = Router::new().route("/metrics", get(metrics));
    let listener = TcpListener::bind(&format!("0.0.0.0:{port}")).await.unwrap();
    axum::serve(listener, app).await.map_err(|e| anyhow!(e))
}
