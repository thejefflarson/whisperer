use std::env;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::SpanExporter;
use opentelemetry_sdk::runtime;
use opentelemetry_sdk::trace::{RandomIdGenerator, TracerProvider};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use whisperer::controller::run;
use whisperer::metrics::serve as metrics;
use whisperer::server::serve as server;

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
    let exporter = SpanExporter::builder().with_tonic().build().unwrap();
    let tracer = TracerProvider::builder()
        .with_id_generator(RandomIdGenerator::default())
        .with_batch_exporter(exporter, runtime::Tokio)
        .build();
    let otel = tracing_opentelemetry::layer().with_tracer(tracer.tracer("whisperer"));
    let filter = EnvFilter::from_default_env();
    tracing_subscriber::registry()
        .with(otel)
        .with(fmt::layer().with_filter(filter))
        .init();

    let controller = run();
    let server = server(server_port);
    let metrics = metrics(metrics_port);
    tokio::join!(controller, metrics, server).1?;
    Ok(())
}
