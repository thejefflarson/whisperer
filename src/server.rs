use anyhow::{Result, anyhow};
use axum::{Router, routing::get};
use tokio::net::TcpListener;

use crate::utils::shutdown_signal;

async fn ruok() -> &'static str {
    "imok"
}

pub async fn serve(port: u16) -> Result<()> {
    // The health check binds 0.0.0.0 — required for Kubernetes liveness/readiness
    // probes which originate from the kubelet on the node IP, not from localhost.
    // There is no per-connection rate limit here; to prevent a probe flood from
    // causing liveness failures, add a Kubernetes NetworkPolicy that restricts
    // access to this port to the kubelet CIDR, or deploy a sidecar proxy with
    // rate limiting in front of the health endpoint.
    let app = Router::new().route("/ruok", get(ruok));
    let listener = TcpListener::bind(&format!("0.0.0.0:{port}")).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| anyhow!(e))
}
