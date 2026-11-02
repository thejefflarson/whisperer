use anyhow::{Result, anyhow};
use axum::{
    Router,
    extract::Request,
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use std::time::Duration;
use tokio::net::TcpListener;

use crate::utils::shutdown_signal;

async fn ruok() -> &'static str {
    "imok"
}

/// Bound each health-check request to 10 s. Without a timeout a slow or stalled
/// client on the pod network holds the Tokio task indefinitely; accumulating
/// enough such connections exhausts the server's capacity (connection-exhaustion
/// DoS). 10 s is generous for the trivial "imok" handler and well within any
/// reasonable Kubernetes probe `timeoutSeconds`.
async fn request_timeout(req: Request, next: Next) -> Response {
    tokio::time::timeout(Duration::from_secs(10), next.run(req))
        .await
        .unwrap_or_else(|_| StatusCode::REQUEST_TIMEOUT.into_response())
}

pub async fn serve(port: u16) -> Result<()> {
    let app = Router::new()
        .route("/ruok", get(ruok))
        .layer(middleware::from_fn(request_timeout));
    let listener = TcpListener::bind(&format!("0.0.0.0:{port}")).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| anyhow!(e))
}
