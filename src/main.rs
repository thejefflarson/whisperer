use std::env;

use opentelemetry::{metrics::MeterProvider, trace::TracerProvider as _};
use opentelemetry_otlp::SpanExporter;
use opentelemetry_sdk::{
    metrics::SdkMeterProvider,
    runtime,
    trace::{RandomIdGenerator, SdkTracerProvider},
};
use prometheus::Registry;
use tracing_subscriber::{fmt, layer::SubscriberExt, prelude::*, EnvFilter};
use whisperer::{
    controller::run,
    metrics::{serve as metrics, MetricState},
    server::serve as server,
};

// NEAT! Ensure that we have a valid port
fn port(var: &str) -> u16 {
    env::var(var)
        .expect(&format!("{var} not defined"))
        .parse::<u16>()
        .expect(&format!("{var} not a valid port"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let server_port = port("SERVER_PORT");
    let metrics_port = port("METRICS_PORT");
    let exporter = SpanExporter::builder().with_http().build().unwrap();
    let tracer = SdkTracerProvider::builder()
        .with_id_generator(RandomIdGenerator::default())
        .with_batch_exporter(exporter)
        .build();
    let otel = tracing_opentelemetry::layer().with_tracer(tracer.tracer("whisperer"));
    let filter = EnvFilter::from_default_env();
    tracing_subscriber::registry()
        .with(otel)
        .with(fmt::layer().with_filter(filter))
        .init();

    // open question: how do I merge this with what's happening above?
    let registry = Registry::new();
    let exporter = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        .build()?;
    let provider = SdkMeterProvider::builder().with_reader(exporter).build();
    let meter = provider.meter("whisperer");

    let state = MetricState::new(registry, meter);
    let controller = run(state.clone());
    let server = server(server_port);
    let metrics = metrics(metrics_port, state);
    tokio::join!(controller, metrics, server).1?;
    Ok(())
}
