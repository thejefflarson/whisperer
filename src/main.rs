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
