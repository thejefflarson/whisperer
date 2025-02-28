use std::{
    sync::{Arc, RwLock},
    time::SystemTime,
};

use crate::utils::shutdown_signal;
use anyhow::{anyhow, Result};
use axum::{
    extract::State,
    http::{header, HeaderMap},
    response::IntoResponse,
    routing::get,
    Router,
};
use opentelemetry::{
    metrics::{Counter, Histogram, Meter},
    KeyValue,
};
use prometheus::{proto::MetricFamily, Encoder, Registry, TextEncoder};
use tokio::net::TcpListener;

// this file is neat! I'm proud of it.

#[derive(Clone)]
pub struct Metrics {
    registry: Registry,
    reconciliations: Counter<u64>,
    failures: Counter<u64>,
    duration: Histogram<f64>,
}

impl Metrics {
    pub fn new(registry: Registry, meter: Meter) -> Self {
        Self {
            registry,
            reconciliations: meter
                .u64_counter("reconciliations")
                .with_description("number of reconciliations performed")
                .build(),
            failures: meter
                .u64_counter("failures")
                .with_description("number of failed reconciliations")
                .build(),
            duration: meter
                .f64_histogram("duration")
                .with_description("duration of reciliation attempts")
                .with_unit("ms")
                .build(),
        }
    }
}

#[derive(Clone)]
pub struct MetricState {
    inner: Arc<RwLock<Metrics>>,
}

impl MetricState {
    pub fn new(registry: Registry, meter: Meter) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Metrics::new(registry, meter))),
        }
    }

    pub fn reconcile(&self, key: String, value: String) {
        self.inner
            .write()
            .unwrap()
            .reconciliations
            .add(1, &[KeyValue::new(key, value)])
    }

    pub fn failure(&self, key: String, value: String) {
        self.inner
            .write()
            .unwrap()
            .failures
            .add(1, &[KeyValue::new(key, value)])
    }

    pub fn duration(&self, key: String, value: String) -> Record {
        Record {
            start: SystemTime::now(),
            metrics: self.clone(),
            key,
            value,
        }
    }

    fn metrics(&self) -> Vec<MetricFamily> {
        self.inner.read().unwrap().registry.gather()
    }

    fn finish(&self, millis: f64, key: String, value: String) {
        self.inner
            .write()
            .unwrap()
            .duration
            .record(millis, &[KeyValue::new(key, value)]);
    }
}

pub struct Record {
    start: SystemTime,
    metrics: MetricState,
    key: String,
    value: String,
}

impl Drop for Record {
    fn drop(&mut self) {
        let diff = self.start.elapsed().unwrap();
        self.metrics.finish(
            diff.as_millis() as f64,
            self.key.clone(),
            self.value.clone(),
        )
    }
}

// from https://github.com/open-telemetry/opentelemetry-rust/blob/main/opentelemetry-prometheus/examples/hyper.rs
async fn metrics(State(state): State<MetricState>) -> impl IntoResponse {
    let metrics = state.metrics();
    let mut buffer = vec![];
    let encoder = TextEncoder::new();
    encoder.encode(&metrics, &mut buffer).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, encoder.format_type().parse().unwrap());
    (headers, buffer)
}

pub async fn serve(port: u16, state: MetricState) -> Result<()> {
    let app = Router::new()
        .route("/metrics", get(metrics))
        .with_state(state.clone());
    let listener = TcpListener::bind(&format!("0.0.0.0:{port}")).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| anyhow!(e))
}
