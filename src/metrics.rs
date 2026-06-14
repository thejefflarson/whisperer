use std::{
    sync::{Arc, RwLock},
    time::SystemTime,
};

use opentelemetry::metrics::{Counter, Histogram, Meter};

// this file is neat! I'm proud of it.

#[derive(Clone)]
pub struct Metrics {
    reconciliations: Counter<u64>,
    failures: Counter<u64>,
    duration: Histogram<f64>,
}

impl Metrics {
    pub fn new(meter: Meter) -> Self {
        Self {
            reconciliations: meter
                .u64_counter("whisperer.reconciliations")
                .with_description("number of reconciliations performed")
                .build(),
            failures: meter
                .u64_counter("whisperer.failures")
                .with_description("number of failed reconciliations")
                .build(),
            duration: meter
                .f64_histogram("whisperer.duration")
                .with_description("duration of reconciliation attempts")
                .with_unit("ms")
                .build(),
        }
    }
}

#[derive(Clone)]
pub struct MetricState {
    inner: Arc<RwLock<Metrics>>,
}

// These metrics deliberately carry NO per-secret attributes. The secret name
// and namespace are attacker-controlled, so using them as metric labels would
// let anyone who can create secrets mint unbounded time series and exhaust the
// operator's and the collector's memory. Per-object detail belongs in logs and
// traces, where cardinality is not a resource-exhaustion vector.
impl MetricState {
    pub fn new(meter: Meter) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Metrics::new(meter))),
        }
    }

    pub fn reconcile(&self) {
        self.inner.write().unwrap().reconciliations.add(1, &[])
    }

    pub fn failure(&self) {
        self.inner.write().unwrap().failures.add(1, &[])
    }

    pub fn duration(&self) -> Record {
        Record {
            start: SystemTime::now(),
            metrics: self.clone(),
        }
    }

    fn finish(&self, millis: f64) {
        self.inner.write().unwrap().duration.record(millis, &[]);
    }
}

pub struct Record {
    start: SystemTime,
    metrics: MetricState,
}

impl Drop for Record {
    fn drop(&mut self) {
        // `elapsed()` errors only if the clock went backwards; treat that as a
        // zero-length sample rather than panicking inside a Drop (a panic here,
        // during unwinding, would abort the whole process).
        let diff = self.start.elapsed().unwrap_or_default();
        self.metrics.finish(diff.as_millis() as f64)
    }
}
