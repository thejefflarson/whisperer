use std::{
    sync::{Arc, RwLock},
    time::SystemTime,
};

use opentelemetry::{
    metrics::{Counter, Histogram, Meter},
    KeyValue,
};

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
                .u64_counter("reconciliations")
                .with_description("number of reconciliations performed")
                .build(),
            failures: meter
                .u64_counter("failures")
                .with_description("number of failed reconciliations")
                .build(),
            duration: meter
                .f64_histogram("duration")
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

impl MetricState {
    pub fn new(meter: Meter) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Metrics::new(meter))),
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
