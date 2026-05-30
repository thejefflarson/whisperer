use anyhow::Result;
use opentelemetry::{metrics::MeterProvider, trace::TracerProvider as _};
use opentelemetry_otlp::{MetricExporter, Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::{
    metrics::SdkMeterProvider,
    trace::{RandomIdGenerator, SdkTracerProvider},
};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, prelude::*};

use crate::metrics::MetricState;

// Telemetry setup follows the patterns from:
// - https://github.com/kube-rs/controller-rs/blob/main/src/telemetry.rs
// - https://github.com/open-telemetry/opentelemetry-rust/blob/main/opentelemetry-otlp/examples/basic-otlp/src/main.rs

/// Owns the OpenTelemetry trace and metric pipelines for the process lifetime.
///
/// Hold this for as long as the program runs and call [`Telemetry::shutdown`]
/// before exit so buffered spans and metrics are flushed to the collector.
pub struct Telemetry {
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
    /// Metric recorders handed to the controller.
    pub metrics: MetricState,
}

impl Telemetry {
    /// Flush and tear down both pipelines. Call once, before exit.
    pub fn shutdown(self) -> Result<()> {
        // Flush metrics first, then traces, so any spans emitted while the
        // meter shuts down are still captured.
        self.meter_provider.shutdown()?;
        self.tracer_provider.shutdown()?;
        Ok(())
    }
}

/// Initialize the tracing subscriber and OTLP exporters.
///
/// Installs a global subscriber combining an OpenTelemetry span layer with a
/// `RUST_LOG`-filtered fmt layer, and wires up a periodic metric exporter.
/// Returns a [`Telemetry`] guard that must be kept alive and shut down on exit.
pub fn init() -> Result<Telemetry> {
    let span_exporter = SpanExporter::builder().with_http().build()?;
    let tracer_provider = SdkTracerProvider::builder()
        .with_id_generator(RandomIdGenerator::default())
        .with_batch_exporter(span_exporter)
        .build();
    let otel = tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer("whisperer"));
    let filter = EnvFilter::from_default_env();
    tracing_subscriber::registry()
        .with(otel)
        .with(fmt::layer().with_filter(filter))
        .init();

    let metric_exporter = MetricExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .build()?;
    let meter_provider = SdkMeterProvider::builder()
        .with_periodic_exporter(metric_exporter)
        .build();
    let metrics = MetricState::new(meter_provider.meter("whisperer"));

    Ok(Telemetry {
        tracer_provider,
        meter_provider,
        metrics,
    })
}
