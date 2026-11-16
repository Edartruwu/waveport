//! How much reached the collector.
//!
//! The exporter of the SDK tells nobody when a request fails, unless a
//! logging subscriber is installed, and a library installs none. So each
//! exporter is wrapped in one that counts, and whoever set the adapter
//! up can ask what became of what was sent.

use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;
use std::sync::{Arc, Mutex, PoisonError};

use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::Temporality;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::trace::{SpanData, SpanExporter};

/// How much was sent to the collector, and how much could not be.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sent {
    /// Spans the collector took.
    pub spans: u64,
    /// Spans that could not be sent.
    pub spans_lost: u64,
    /// Times the metrics were sent and taken.
    pub metrics: u64,
    /// Times the metrics could not be sent.
    pub metrics_lost: u64,
    /// What went wrong the last time something could not be sent.
    pub error: Option<String>,
}

impl Sent {
    /// Whether anything could not be sent.
    #[must_use]
    pub const fn lost_any(&self) -> bool {
        self.spans_lost > 0 || self.metrics_lost > 0
    }
}

/// The counts, shared by the two exporters and whoever asks.
#[derive(Debug, Default)]
pub(crate) struct Tally {
    spans: AtomicU64,
    spans_lost: AtomicU64,
    metrics: AtomicU64,
    metrics_lost: AtomicU64,
    error: Mutex<Option<String>>,
}

impl Tally {
    pub(crate) fn sent(&self) -> Sent {
        let error = self.error.lock().unwrap_or_else(PoisonError::into_inner);
        Sent {
            spans: self.spans.load(Ordering::Relaxed),
            spans_lost: self.spans_lost.load(Ordering::Relaxed),
            metrics: self.metrics.load(Ordering::Relaxed),
            metrics_lost: self.metrics_lost.load(Ordering::Relaxed),
            error: error.clone(),
        }
    }

    /// Counts `many` of something as sent or lost, as `result` says.
    fn count(&self, result: &OTelSdkResult, many: u64, sent: &AtomicU64, lost: &AtomicU64) {
        match result {
            Ok(()) => {
                sent.fetch_add(many, Ordering::Relaxed);
            }
            Err(error) => {
                lost.fetch_add(many, Ordering::Relaxed);
                let mut said = self.error.lock().unwrap_or_else(PoisonError::into_inner);
                *said = Some(error.to_string());
            }
        }
    }
}

/// An exporter that counts what becomes of what it is given.
#[derive(Debug)]
pub(crate) struct Counted<E> {
    inner: E,
    tally: Arc<Tally>,
}

impl<E> Counted<E> {
    pub(crate) const fn new(inner: E, tally: Arc<Tally>) -> Self {
        Self { inner, tally }
    }
}

impl<E: SpanExporter> SpanExporter for Counted<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let many = u64::try_from(batch.len()).unwrap_or(u64::MAX);
        let result = self.inner.export(batch).await;
        let tally = &self.tally;
        tally.count(&result, many, &tally.spans, &tally.spans_lost);
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

impl<E: PushMetricExporter> PushMetricExporter for Counted<E> {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        let result = self.inner.export(metrics).await;
        let tally = &self.tally;
        tally.count(&result, 1, &tally.metrics, &tally.metrics_lost);
        result
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn temporality(&self) -> Temporality {
        self.inner.temporality()
    }
}
