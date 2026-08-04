//! Deterministic streaming-latency harness.
//!
//! This is measurement scaffolding, not product code: it is compiled only for
//! tests and for consumers that opt in through the `test-fixtures` feature, so
//! it never ships inside the production binary.

use std::collections::{BTreeMap, VecDeque};
use std::hint::black_box;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::Serialize;
use tokio::sync::mpsc;

use vibe_core::engine::{
    CancellationToken, CompletionProvider, ConversationEngine, EventObserver, ProviderFuture,
    ProviderStreamFuture,
};
use vibe_core::events::{EngineEvent, EventEnvelope, ModelMessage};
use vibe_core::provider::{
    ProviderChunk, ProviderError, ProviderInput, ProviderStream, RequestLimits,
};

use crate::live_projection::{
    AppServerUpdate, app_server_notification, app_server_update_channel_for_turn,
};
use crate::server::AppServer;

const STREAMING_P95_TARGET_MICROS: u64 = 20_000;
const STREAMING_P99_TARGET_MICROS: u64 = 50_000;
const RELEASE_STREAMING_SAMPLE_SIZE: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamingLatencyBenchmark {
    pub chunk_count: usize,
    pub client_visible_event_count: usize,
    pub p50_micros: u64,
    pub p95_micros: u64,
    pub p99_micros: u64,
    pub max_micros: u64,
    pub total_elapsed_millis: u64,
    pub chunks_per_second: u64,
    pub p95_target_micros: u64,
    pub p99_target_micros: u64,
    pub release_sample_size: usize,
    pub release_gate_passed: bool,
}

struct BenchmarkProvider {
    chunk_count: usize,
    receipts: Arc<Mutex<VecDeque<Instant>>>,
}

impl CompletionProvider for BenchmarkProvider {
    fn complete<'a>(&'a self, _input: &'a ProviderInput) -> ProviderFuture<'a> {
        Box::pin(async {
            Err(ProviderError::MalformedStream(
                "streaming benchmark requires the streaming path".to_owned(),
            ))
        })
    }

    fn stream<'a>(&'a self, _input: &'a ProviderInput) -> ProviderStreamFuture<'a> {
        let chunk_count = self.chunk_count;
        let receipts = Arc::clone(&self.receipts);
        Box::pin(async move {
            let text = futures_util::stream::iter(0..chunk_count).map(move |_| {
                if let Ok(mut receipts) = receipts.lock() {
                    receipts.push_back(Instant::now());
                }
                Ok(ProviderChunk::Text {
                    text: "x".to_owned(),
                })
            });
            let terminal = futures_util::stream::iter([
                Ok(ProviderChunk::Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                }),
                Ok(ProviderChunk::Stop {
                    reason: "stop".to_owned(),
                }),
            ]);
            Ok(ProviderStream {
                correlation_id: Some("fake-streaming-benchmark".to_owned()),
                chunks: Box::pin(text.chain(terminal)),
            })
        })
    }
}

struct BenchmarkObserver {
    projection: Arc<dyn EventObserver>,
    updates: Mutex<mpsc::UnboundedReceiver<AppServerUpdate>>,
    server: AppServer,
    receipts: Arc<Mutex<VecDeque<Instant>>>,
    latencies: Arc<Mutex<Vec<Duration>>>,
}

impl EventObserver for BenchmarkObserver {
    fn observe(&self, event: &EventEnvelope) -> Result<(), String> {
        let receipt = if matches!(&event.event, EngineEvent::ModelText { .. }) {
            Some(
                self.receipts
                    .lock()
                    .map_err(|_| "benchmark receipt lock is poisoned".to_owned())?
                    .pop_front()
                    .ok_or_else(|| "benchmark model event has no receipt timestamp".to_owned())?,
            )
        } else {
            None
        };
        self.projection.observe(event)?;
        let mut updates = self
            .updates
            .lock()
            .map_err(|_| "benchmark update lock is poisoned".to_owned())?;
        let mut visible_updates = 0_usize;
        while let Ok(update) = updates.try_recv() {
            let bytes =
                app_server_notification(&self.server, update).map_err(|error| error.to_string())?;
            black_box(bytes);
            visible_updates = visible_updates.saturating_add(1);
        }
        if let Some(receipt) = receipt {
            if visible_updates == 0 {
                return Err("benchmark model chunk produced no client-visible event".to_owned());
            }
            self.latencies
                .lock()
                .map_err(|_| "benchmark latency lock is poisoned".to_owned())?
                .push(receipt.elapsed());
        }
        Ok(())
    }
}

pub async fn benchmark_fake_provider_chunk_latency(
    chunk_count: usize,
) -> Result<StreamingLatencyBenchmark, String> {
    if chunk_count == 0 {
        return Err("streaming benchmark requires at least one chunk".to_owned());
    }

    let server = AppServer::default();
    let mut connection = server.connect(vibe_protocol::TransportKind::InProcess);
    for frame in [
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "streaming-benchmark",
                    "version": env!("CARGO_PKG_VERSION"),
                    "entrypoint": "programmatic",
                    "terminalEmulator": "unknown"
                },
                "capabilities": {}
            }
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/start",
            "params": {
                "sessionId": "streaming-benchmark-session",
                "workingDirectory": "/streaming-benchmark"
            }
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "turn/start",
            "params": {
                "sessionId": "streaming-benchmark-session",
                "input": [{"type": "text", "text": "benchmark"}]
            }
        }),
    ] {
        let batch = connection.dispatch(
            &serde_json::to_vec(&frame)
                .map_err(|error| format!("benchmark request serialization failed: {error}"))?,
        );
        if batch.close_after_flush {
            return Err("benchmark app-server connection closed during setup".to_owned());
        }
    }
    let session = server
        .session("streaming-benchmark-session")
        .map_err(|error| error.to_string())?;
    let turn_id = session
        .active_turn
        .ok_or_else(|| "benchmark turn was not reserved".to_owned())?;
    black_box(
        server
            .turn_started("streaming-benchmark-session", &turn_id)
            .map_err(|error| error.to_string())?,
    );

    let receipts = Arc::new(Mutex::new(VecDeque::with_capacity(chunk_count)));
    let latencies = Arc::new(Mutex::new(Vec::with_capacity(chunk_count)));
    let (projection, updates) =
        app_server_update_channel_for_turn("streaming-benchmark-session", turn_id.clone());
    let observer: Arc<dyn EventObserver> = Arc::new(BenchmarkObserver {
        projection,
        updates: Mutex::new(updates),
        server,
        receipts: Arc::clone(&receipts),
        latencies: Arc::clone(&latencies),
    });
    let provider = BenchmarkProvider {
        chunk_count,
        receipts,
    };
    let input = ProviderInput {
        turn_id: Some(turn_id),
        model_override: None,
        messages: vec![ModelMessage::System {
            content: "deterministic streaming benchmark".to_owned(),
        }],
        stream: true,
        images: Vec::new(),
        tools: Vec::new(),
        tool_choice: None,
        thinking: false,
        reasoning_effort: None,
        headers: BTreeMap::new(),
        limits: RequestLimits {
            max_tokens: u32::MAX,
            temperature_millis: None,
            max_response_bytes: 2 * 1024 * 1024,
        },
        metadata: BTreeMap::new(),
    };
    let total_started = Instant::now();
    ConversationEngine::new(provider)
        .with_observer(observer)
        .run_turn(
            "streaming-benchmark-session",
            input,
            "benchmark",
            CancellationToken::default(),
        )
        .await
        .map_err(|error| error.to_string())?;
    let total_elapsed = total_started.elapsed();

    let mut latency_micros = latencies
        .lock()
        .map_err(|_| "benchmark latency lock is poisoned".to_owned())?
        .iter()
        .copied()
        .map(duration_micros)
        .collect::<Vec<_>>();
    if latency_micros.len() != chunk_count {
        return Err(format!(
            "benchmark observed {} client-visible chunk events, expected {chunk_count}",
            latency_micros.len()
        ));
    }
    latency_micros.sort_unstable();
    let p50_micros = percentile(&latency_micros, 50);
    let p95_micros = percentile(&latency_micros, 95);
    let p99_micros = percentile(&latency_micros, 99);
    let max_micros = latency_micros.last().copied().unwrap_or_default();
    let total_elapsed_millis = u64::try_from(total_elapsed.as_millis()).unwrap_or(u64::MAX);
    let total_nanos = total_elapsed.as_nanos().max(1);
    let chunks_per_second = u64::try_from(
        (chunk_count as u128)
            .saturating_mul(1_000_000_000)
            .saturating_div(total_nanos),
    )
    .unwrap_or(u64::MAX);
    let release_gate_passed = chunk_count >= RELEASE_STREAMING_SAMPLE_SIZE
        && p95_micros <= STREAMING_P95_TARGET_MICROS
        && p99_micros <= STREAMING_P99_TARGET_MICROS;
    Ok(StreamingLatencyBenchmark {
        chunk_count,
        client_visible_event_count: latency_micros.len(),
        p50_micros,
        p95_micros,
        p99_micros,
        max_micros,
        total_elapsed_millis,
        chunks_per_second,
        p95_target_micros: STREAMING_P95_TARGET_MICROS,
        p99_target_micros: STREAMING_P99_TARGET_MICROS,
        release_sample_size: RELEASE_STREAMING_SAMPLE_SIZE,
        release_gate_passed,
    })
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn percentile(sorted: &[u64], percentage: usize) -> u64 {
    let rank = sorted
        .len()
        .saturating_mul(percentage)
        .saturating_add(99)
        .saturating_div(100)
        .saturating_sub(1);
    sorted.get(rank).copied().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn streaming_latency_benchmark_observes_every_fake_provider_chunk() {
        let report = benchmark_fake_provider_chunk_latency(128)
            .await
            .expect("streaming benchmark runs");
        assert_eq!(report.chunk_count, 128);
        assert_eq!(report.client_visible_event_count, 128);
        assert!(!report.release_gate_passed);
    }
}
