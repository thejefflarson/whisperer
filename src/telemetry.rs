use anyhow::Result;
use opentelemetry::{global, metrics::MeterProvider, trace::TracerProvider as _};
use opentelemetry_otlp::{MetricExporter, Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::{
    Resource,
    metrics::SdkMeterProvider,
    trace::{RandomIdGenerator, SdkTracerProvider},
};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, prelude::*};

use crate::metrics::MetricState;

/// Default service name attached to every span and metric when the operator
/// hasn't supplied one. Without a `service.name` resource attribute, collectors
/// label our telemetry `unknown_service`. An operator can override this with the
/// standard `OTEL_SERVICE_NAME` (or `OTEL_RESOURCE_ATTRIBUTES=service.name=...`)
/// environment variable.
const DEFAULT_SERVICE_NAME: &str = "whisperer";

/// True if the operator has already specified a service name via the standard
/// OpenTelemetry environment variables, in which case we must not override it.
fn service_name_set_in_env() -> bool {
    std::env::var_os("OTEL_SERVICE_NAME").is_some()
        || std::env::var("OTEL_RESOURCE_ATTRIBUTES")
            .map(|v| v.contains("service.name"))
            .unwrap_or(false)
}

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
    // Identify this process to the collector so spans and metrics are attributed
    // to a real service rather than the default "unknown_service". Honor an
    // operator-supplied OTEL_SERVICE_NAME / OTEL_RESOURCE_ATTRIBUTES (already
    // read by Resource::builder()'s env detector); only fall back to our default
    // when neither is set.
    let builder = Resource::builder();
    let resource = if service_name_set_in_env() {
        builder.build()
    } else {
        builder.with_service_name(DEFAULT_SERVICE_NAME).build()
    };

    let span_exporter = SpanExporter::builder().with_http().build()?;
    let tracer_provider = SdkTracerProvider::builder()
        .with_id_generator(RandomIdGenerator::default())
        .with_resource(resource.clone())
        .with_batch_exporter(span_exporter)
        .build();
    // Register globally so spans created via the OpenTelemetry global API (not
    // just our tracing layer) also carry service.name; without this they fall
    // back to a no-op provider and lose the resource.
    global::set_tracer_provider(tracer_provider.clone());
    let otel =
        tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer(DEFAULT_SERVICE_NAME));
    let filter = EnvFilter::from_default_env();
    tracing_subscriber::registry()
        .with(otel)
        .with(fmt::layer().with_filter(filter))
        .init();

    // Warn when OTLP goes over plaintext HTTP to a non-loopback address — metric
    // attributes include Kubernetes secret names and namespace values.
    let otlp_endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:4318".to_string());
    if !otlp_endpoint.starts_with("https://")
        && !otlp_endpoint.contains("localhost")
        && !otlp_endpoint.contains("127.0.0.1")
    {
        tracing::warn!(
            endpoint = %otlp_endpoint,
            "OTLP endpoint uses plaintext HTTP; metric attributes contain \
             Kubernetes secret names and namespaces — set \
             OTEL_EXPORTER_OTLP_ENDPOINT to an https:// URL in production"
        );
    }

    let metric_exporter = MetricExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .build()?;
    let meter_provider = SdkMeterProvider::builder()
        .with_resource(resource)
        .with_periodic_exporter(metric_exporter)
        .build();
    // Same as the tracer: make this the global meter provider so any metrics
    // recorded through the global API are attributed to service.name too.
    global::set_meter_provider(meter_provider.clone());
    let metrics = MetricState::new(meter_provider.meter(DEFAULT_SERVICE_NAME));

    Ok(Telemetry {
        tracer_provider,
        meter_provider,
        metrics,
    })
}
