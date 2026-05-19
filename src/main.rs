use opentelemetry::{metrics::MeterProvider, trace::TracerProvider as _};
use opentelemetry_otlp::{MetricExporter, Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::{
    metrics::SdkMeterProvider,
    trace::{RandomIdGenerator, SdkTracerProvider},
};
use std::env;
use tracing_subscriber::{fmt, layer::SubscriberExt, prelude::*, EnvFilter};
use whisperer::{controller::run, metrics::MetricState, server::serve as server};

// Ensure that we have a valid port
fn port(var: &str) -> u16 {
    env::var(var)
        .expect(&format!("{var} not defined"))
        .parse::<u16>()
        .expect(&format!("{var} not a valid port"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let healthcheck_port = port("HEALTHCHECK_PORT");
    let otlp_endpoint = env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:4318".to_string());
    let exporter = SpanExporter::builder().with_http().build()?;
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

    // Warn when OTLP is sent over plaintext HTTP to a non-loopback address;
    // metric attributes include Kubernetes secret names and namespace values.
    if !otlp_endpoint.starts_with("https://")
        && !otlp_endpoint.contains("localhost")
        && !otlp_endpoint.contains("127.0.0.1")
    {
        tracing::warn!(
            endpoint = %otlp_endpoint,
            "OTLP endpoint uses plaintext HTTP; metric attributes contain \
             Kubernetes secret names and namespaces — set \
             OTEL_EXPORTER_OTLP_ENDPOINT to an https:// URL in production \
             to encrypt sensitive data in transit"
        );
    }
    let exporter = MetricExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .build()?;
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter)
        .build();
    let meter = provider.meter("whisperer");
    let state = MetricState::new(meter);
    let controller = run(state.clone());
    let server = server(healthcheck_port);
    tokio::join!(controller, server).1?;
    provider.shutdown()?;
    Ok(())
}
