//! What a test installs to read the spans this build produces.
//!
//! The tracer provider is process-global, exactly as it is in the reference, so
//! every test that installs one takes [`exclusive`] first: two tests exporting
//! into each other's collector would read spans they never opened.
//!
//! The collector below is what the reference's capture installs on its side, an
//! in-memory exporter in place of the OTLP one, so a span is compared as the
//! exporter receives it rather than as the tracer holds it.

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::ThreadId;
use std::time::Duration;

use opentelemetry::trace::noop::NoopTracerProvider;
use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};
use opentelemetry::{KeyValue, global};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SdkTracerProvider, SimpleSpanProcessor, SpanData, SpanExporter};

use super::redaction::{RedactingSpanExporter, RedactionPolicy};

/// Whether a test currently owns the global provider, and the signal that says
/// it stopped owning it.
///
/// A [`MutexGuard`](std::sync::MutexGuard) would be the obvious way to say this
/// and the wrong one: these tests hold the claim across await points, which is
/// exactly what an async-unaware guard must never be used for. The claim is a
/// flag instead, released by [`Exclusive`]'s drop.
static OWNED: Mutex<bool> = Mutex::new(false);
static RELEASED: Condvar = Condvar::new();

/// The claim a test takes before touching the global provider. A poisoned lock
/// still hands it over: a panicking test must not turn into every other tracing
/// test failing for a reason that has nothing to do with them.
pub(crate) fn exclusive() -> Exclusive {
    let mut owned = OWNED.lock().unwrap_or_else(PoisonError::into_inner);
    while *owned {
        owned = RELEASED.wait(owned).unwrap_or_else(PoisonError::into_inner);
    }
    *owned = true;
    Exclusive
}

/// Ownership of the global provider, released when dropped.
pub(crate) struct Exclusive;

impl Drop for Exclusive {
    fn drop(&mut self) {
        let mut owned = OWNED.lock().unwrap_or_else(PoisonError::into_inner);
        *owned = false;
        drop(owned);
        RELEASED.notify_one();
    }
}

/// Every span an exporter received, each with the thread that ended it.
///
/// The provider is global and the test binary is not: a test running beside
/// this one exports into this collector too, and its spans are named exactly
/// like the ones under test. What separates them is the thread, because a span
/// ends where its future was polled and no two tests share a runtime.
#[derive(Debug, Clone, Default)]
pub(crate) struct Collector {
    spans: Arc<Mutex<Vec<(ThreadId, SpanData)>>>,
}

impl Collector {
    /// The spans this thread exported, oldest first.
    pub(crate) fn spans(&self) -> Vec<SpanData> {
        let current = std::thread::current().id();
        self.spans
            .lock()
            .map(|spans| {
                spans
                    .iter()
                    .filter(|(thread, _)| *thread == current)
                    .map(|(_, span)| span.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Forgets what this thread exported, so the next case reads its own spans.
    pub(crate) fn clear(&self) {
        let current = std::thread::current().id();
        if let Ok(mut spans) = self.spans.lock() {
            spans.retain(|(thread, _)| *thread != current);
        }
    }

    fn exporter(&self) -> CollectingSpanExporter {
        CollectingSpanExporter {
            spans: Arc::clone(&self.spans),
        }
    }
}

#[derive(Debug)]
struct CollectingSpanExporter {
    spans: Arc<Mutex<Vec<(ThreadId, SpanData)>>>,
}

impl SpanExporter for CollectingSpanExporter {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let current = std::thread::current().id();
        if let Ok(mut spans) = self.spans.lock() {
            spans.extend(batch.into_iter().map(|span| (current, span)));
        }
        Ok(())
    }
}

/// An installed provider exporting into a collector, uninstalled on drop.
pub(crate) struct Harness {
    provider: SdkTracerProvider,
    collector: Collector,
}

impl Harness {
    /// A provider exporting every span as it ends, with no redaction.
    pub(crate) fn install() -> Self {
        let collector = Collector::default();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(collector.exporter()))
            .build();
        global::set_tracer_provider(provider.clone());
        Self {
            provider,
            collector,
        }
    }

    /// The same, with the exporter wrapped the way `otel_redaction` wraps it.
    pub(crate) fn install_redacting(policy: RedactionPolicy) -> Self {
        let collector = Collector::default();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(RedactingSpanExporter::new(
                collector.exporter(),
                policy,
            )))
            .build();
        global::set_tracer_provider(provider.clone());
        Self {
            provider,
            collector,
        }
    }

    /// One span carrying exactly the attributes a case declares, opened through
    /// the tracer rather than through a family, which is how a redaction case
    /// measures the exporter and nothing else.
    pub(crate) fn record(&self, name: &'static str, attributes: Vec<KeyValue>) {
        let tracer = self.provider.tracer(super::TRACER_NAME);
        let mut span = tracer
            .span_builder(name)
            .with_attributes(attributes)
            .start(&tracer);
        span.end();
    }

    /// The spans exported since the last drain, and an empty collector.
    pub(crate) fn drain(&self) -> Vec<SpanData> {
        let _ = self.provider.force_flush();
        let spans = self.collector.spans();
        self.collector.clear();
        spans
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.provider.shutdown_with_timeout(Duration::from_secs(1));
        uninstall();
    }
}

/// Returns the process to the state a build with tracing off runs in, where
/// every span is non-recording.
pub(crate) fn uninstall() {
    global::set_tracer_provider(NoopTracerProvider::new());
}
