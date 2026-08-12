//! What the corpus cannot measure: the invariants this port owes its callers
//! rather than the reference.
//!
//! The differential comparisons live in
//! `crate::telemetry::telemetry_parity_tests`, which replays the `exporterConfig`,
//! `spans`, `providerNames` and `redaction` families against the pinned
//! reference. What is asserted here is the never-raise policy, the baggage API
//! US-013 was asked to prove exists, and the failure modes a collector never
//! sees because nothing is exported.

#![expect(
    deprecated,
    reason = "`opentelemetry-semantic-conventions` 0.32.1 deprecates the whole `gen_ai.*` family \
              for having moved to the GenAI semantic-conventions repository, without changing a \
              single key string. Reading the constants keeps the keys attributed to the crate that \
              publishes them, and the `spans` corpus family fails the moment one of them changes"
)]

use opentelemetry::trace::{Span as _, TraceContextExt, Tracer as _, TracerProvider as _};
use opentelemetry::{Context, KeyValue, global};
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use opentelemetry_semantic_conventions::attribute as semconv;
use secrecy::ExposeSecret;
use serde_json::json;
use toml::Table;

use super::harness::{Harness, exclusive, uninstall};
use super::redaction::{REDACTED_VALUE, RedactingSpanExporter, RedactionPolicy};
use super::*;

/// A failure with no backend behind it, named the way the reference's plain
/// exception is named so the status description is comparable.
#[derive(Debug)]
struct RuntimeError(&'static str);

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl TracedError for RuntimeError {
    fn error_type(&self) -> &'static str {
        "RuntimeError"
    }
}

/// A failure carrying a backend status, which is what stops a span from
/// recording the exception.
#[derive(Debug)]
struct BackendError(Option<i64>);

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the backend refused the request")
    }
}

impl TracedError for BackendError {
    fn error_type(&self) -> &'static str {
        "BackendError"
    }

    fn backend_failure(&self) -> Option<BackendFailure> {
        Some(BackendFailure {
            provider: Some("mistral".to_owned()),
            status: self.0,
        })
    }
}

fn document(body: &str) -> Table {
    body.parse::<Table>().expect("the document parses")
}

fn mistral_document() -> Table {
    document(
        r#"
active_model = "primary"

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "TEST_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "model"
provider = "mistral"
alias = "primary"
"#,
    )
}

fn credentials(present: bool) -> impl Fn(&str) -> Option<String> {
    move |variable| present.then(|| format!("credential-for-{variable}"))
}

fn attribute<'a>(span: &'a SpanData, key: &str) -> Option<&'a opentelemetry::Value> {
    span.attributes
        .iter()
        .find(|attribute| attribute.key.as_str() == key)
        .map(|attribute| &attribute.value)
}

#[test]
fn an_explicit_endpoint_carries_no_credential() {
    let resolved = build_span_exporter_config(
        "https://collector.example.test",
        &mistral_document(),
        &credentials(true),
    )
    .expect("an explicit endpoint resolves");
    assert_eq!(
        resolved.endpoint,
        "https://collector.example.test/v1/traces"
    );
    assert!(
        resolved.headers.is_empty(),
        "an endpoint the operator named authenticates itself"
    );
}

#[test]
fn a_derived_endpoint_carries_the_providers_credential() {
    let resolved = build_span_exporter_config("", &mistral_document(), &credentials(true))
        .expect("the derived endpoint resolves");
    assert_eq!(
        resolved.endpoint,
        "https://api.mistral.ai/telemetry/v1/traces"
    );
    assert_eq!(resolved.header_names(), vec!["Authorization".to_owned()]);
    assert_eq!(
        resolved
            .headers
            .get("Authorization")
            .map(|value| value.expose_secret().to_owned()),
        Some("Bearer credential-for-TEST_MISTRAL_KEY".to_owned()),
    );
}

#[test]
fn a_derived_endpoint_without_a_credential_resolves_nothing() {
    assert!(
        build_span_exporter_config("", &mistral_document(), &credentials(false)).is_none(),
        "the reference warns and declines rather than exporting unauthenticated"
    );
}

#[test]
fn the_two_key_gate_decides_whether_anything_is_constructed() {
    let _exclusive = exclusive();
    let mut off = mistral_document();
    off.insert("enable_otel".to_owned(), toml::Value::Boolean(false));
    assert!(
        matches!(
            setup_tracing(&off, &credentials(true)),
            TracingSetup::Disabled
        ),
        "`enable_otel` off constructs nothing"
    );

    let mut silenced = mistral_document();
    silenced.insert("enable_otel".to_owned(), toml::Value::Boolean(true));
    silenced.insert("enable_telemetry".to_owned(), toml::Value::Boolean(false));
    assert!(
        matches!(
            setup_tracing(&silenced, &credentials(true)),
            TracingSetup::Disabled
        ),
        "telemetry off silences tracing with it"
    );

    let mut unresolvable = mistral_document();
    unresolvable.insert("enable_otel".to_owned(), toml::Value::Boolean(true));
    match setup_tracing(&unresolvable, &credentials(false)) {
        TracingSetup::MissingCredential { variable } => {
            assert_eq!(variable, "TEST_MISTRAL_KEY");
        }
        _ => panic!("a missing credential names the variable rather than installing a provider"),
    }
    uninstall();
}

#[tokio::test]
async fn a_turn_publishes_its_conversation_id_to_every_descendant() {
    let _exclusive = exclusive();
    let harness = Harness::install();
    let outcome: Result<(), RuntimeError> = agent_span(
        AgentSpan {
            model: Some("model"),
            session_id: Some("session"),
        },
        async {
            // The baggage API this story was asked to name:
            // `opentelemetry::baggage::BaggageExt::with_baggage` writes it and
            // `Context::baggage` reads it back, with the attachment scoped to
            // the future rather than to a thread.
            assert_eq!(
                conversation_from_baggage(&Context::current()),
                Some("session".to_owned()),
                "the body runs inside the context the span attached"
            );
            tool_span(
                ToolSpan {
                    tool_name: "read_file",
                    call_id: "call",
                    arguments: "{}",
                },
                async {
                    set_tool_result("done");
                    Ok(())
                },
            )
            .await
        },
    )
    .await;
    assert!(outcome.is_ok());

    let spans = harness.drain();
    let tool = spans
        .iter()
        .find(|span| span.name == "execute_tool read_file")
        .expect("the tool span was exported");
    assert_eq!(
        attribute(tool, semconv::GEN_AI_CONVERSATION_ID).map(ToString::to_string),
        Some("session".to_owned()),
        "the descendant read the conversation id out of baggage"
    );
    assert_eq!(
        attribute(tool, semconv::GEN_AI_TOOL_CALL_RESULT).map(ToString::to_string),
        Some("done".to_owned()),
        "a result set inside the body reaches the span it ran under"
    );
    assert!(
        Context::current().baggage().is_empty(),
        "the attachment ended with the future"
    );
}

#[tokio::test]
async fn a_body_runs_and_its_setters_are_silent_without_a_provider() {
    let _exclusive = exclusive();
    uninstall();
    let mut ran = false;
    let outcome: Result<bool, RuntimeError> = model_call_span(
        ModelCallSpan {
            provider_name: "mistral",
            provider_api_style: "openai",
            model: "model",
            ..ModelCallSpan::default()
        },
        async {
            ran = true;
            set_model_call_http_status(200);
            set_model_call_usage(1, 2);
            Ok(Context::current().span().is_recording())
        },
    )
    .await;
    assert!(ran, "the body ran with no tracer installed");
    assert_eq!(
        outcome.ok(),
        Some(false),
        "the span is not recording, which is what the reference's INVALID_SPAN answers"
    );
}

#[tokio::test]
async fn a_backend_failure_reports_its_status_and_records_no_exception() {
    let _exclusive = exclusive();
    let harness = Harness::install();
    let refused: Result<(), BackendError> = model_call_span(
        ModelCallSpan {
            provider_name: "mistral",
            provider_api_style: "openai",
            model: "model",
            ..ModelCallSpan::default()
        },
        async { Err(BackendError(Some(503))) },
    )
    .await;
    assert!(refused.is_err());
    let spans = harness.drain();
    let span = spans.first().expect("the failing span was exported");
    assert_eq!(
        span.status,
        Status::error("BackendError(provider=mistral, status=503)"),
        "the status carries the provider and the status, never the message"
    );
    assert_eq!(
        span.events.events.len(),
        0,
        "a model call never records the exception"
    );

    let unhandled: Result<(), RuntimeError> = tool_span(
        ToolSpan {
            tool_name: "read_file",
            call_id: "call",
            arguments: "{}",
        },
        async { Err(RuntimeError("the tool failed")) },
    )
    .await;
    assert!(unhandled.is_err());
    let spans = harness.drain();
    let span = spans.first().expect("the failing tool span was exported");
    assert_eq!(span.status, Status::error("the tool failed"));
    assert_eq!(
        span.events.events.len(),
        1,
        "every other family records the exception"
    );
}

#[tokio::test]
async fn a_hook_reports_the_tool_it_guards() {
    let _exclusive = exclusive();
    let harness = Harness::install();
    let outcome: Result<(), RuntimeError> = hook_span(
        HookSpan {
            hook_name: "guard",
            hook_type: "pre_tool_use",
            tool_name: Some("read_file"),
            tool_call_id: Some("call"),
        },
        async { Ok(()) },
    )
    .await;
    assert!(outcome.is_ok());
    let spans = harness.drain();
    let span = spans.first().expect("the hook span was exported");
    assert_eq!(span.name, "hook pre_tool_use guard");
    assert_eq!(
        attribute(span, HOOK_NAME_KEY).map(ToString::to_string),
        Some("guard".to_owned())
    );
    assert_eq!(
        attribute(span, semconv::GEN_AI_TOOL_NAME).map(ToString::to_string),
        Some("read_file".to_owned())
    );
}

#[tokio::test]
async fn a_response_fills_the_model_the_id_and_the_finish_reasons() {
    let _exclusive = exclusive();
    let harness = Harness::install();
    let outcome: Result<(), RuntimeError> = model_call_span(
        ModelCallSpan {
            provider_name: "mistral",
            provider_api_style: "openai",
            model: "model",
            ..ModelCallSpan::default()
        },
        async {
            set_model_call_response_metadata(&json!({
                "message": {"model": "answering-model", "id": "response"},
                "choices": [{"finish_reason": "stop"}],
            }));
            Ok(())
        },
    )
    .await;
    assert!(outcome.is_ok());
    let spans = harness.drain();
    let span = spans.first().expect("the span was exported");
    assert_eq!(
        attribute(span, semconv::GEN_AI_RESPONSE_MODEL).map(ToString::to_string),
        Some("answering-model".to_owned()),
        "the model is taken from the first source that supplies one"
    );
    assert_eq!(
        attribute(span, semconv::GEN_AI_RESPONSE_ID).map(ToString::to_string),
        Some("response".to_owned())
    );
    assert_eq!(
        attribute(span, semconv::GEN_AI_RESPONSE_FINISH_REASONS).map(ToString::to_string),
        Some("[\"stop\"]".to_owned())
    );
}

#[test]
fn every_pattern_compiles() {
    let policy = RedactionPolicy::Content;
    assert_eq!(
        policy.redact_attributes(vec![KeyValue::new(
            "vibe.provider.api_style",
            "Bearer oracleoracleoracleoracle",
        )])[0]
            .value
            .as_str(),
        REDACTED_VALUE,
        "a credential-shaped value is replaced wherever it sits"
    );
    assert_eq!(
        policy.redact_attributes(vec![KeyValue::new(
            "http.url",
            "https://api.example.test/v1"
        )])[0]
            .value
            .as_str(),
        "https://api.example.test/v1",
        "the content-oriented policy leaves a plain value alone"
    );
    let strict = RedactionPolicy::Key;
    let redacted = strict.redact_attributes(vec![
        KeyValue::new(semconv::GEN_AI_TOOL_CALL_ARGUMENTS, "{\"path\":\"a.rs\"}"),
        KeyValue::new(semconv::GEN_AI_TOOL_NAME, "read_file"),
        KeyValue::new(semconv::GEN_AI_USAGE_INPUT_TOKENS, 12_i64),
    ]);
    assert_eq!(redacted[0].value.as_str(), REDACTED_VALUE);
    assert_eq!(redacted[1].value.as_str(), "read_file");
    assert_eq!(redacted[2].value.as_str(), "12");
    assert_eq!(
        redacted.len(),
        3,
        "no key is added and none is dropped, whatever the mode"
    );
}

/// An inner exporter that always fails, which is what proves the wrapper never
/// turns an export failure into a caller's problem.
#[derive(Debug)]
struct FailingExporter;

impl SpanExporter for FailingExporter {
    async fn export(&self, _batch: Vec<SpanData>) -> OTelSdkResult {
        Err(OTelSdkError::InternalFailure(
            "the collector is gone".into(),
        ))
    }
}

#[tokio::test]
async fn an_export_failure_never_reaches_the_body() {
    let _exclusive = exclusive();
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_span_processor(opentelemetry_sdk::trace::SimpleSpanProcessor::new(
            RedactingSpanExporter::new(FailingExporter, RedactionPolicy::Key),
        ))
        .build();
    global::set_tracer_provider(provider.clone());
    let outcome: Result<u8, RuntimeError> = agent_span(
        AgentSpan {
            model: Some("model"),
            session_id: Some("session"),
        },
        async { Ok(7) },
    )
    .await;
    assert_eq!(outcome.ok(), Some(7), "the turn answered its own value");
    let _ = provider.shutdown();
    uninstall();
}

#[test]
fn a_provider_name_normalizes_the_way_the_aliases_decide() {
    assert_eq!(provider_attribute_value(Some("  MISTRAL  ")), "mistral_ai");
    assert_eq!(
        provider_attribute_value(Some("google-vertex")),
        "gcp.vertex_ai"
    );
    assert_eq!(
        provider_attribute_value(Some("unknown-provider")),
        "unknown_provider"
    );
    assert_eq!(provider_attribute_value(Some("")), UNKNOWN_PROVIDER_VALUE);
    assert_eq!(provider_attribute_value(None), UNKNOWN_PROVIDER_VALUE);
}

#[test]
fn the_redaction_mode_reads_the_published_choices() {
    assert_eq!(OtelRedactionMode::parse("none"), OtelRedactionMode::None);
    assert_eq!(
        OtelRedactionMode::parse("strict"),
        OtelRedactionMode::Strict
    );
    assert_eq!(
        OtelRedactionMode::parse("nonsense"),
        OtelRedactionMode::Default,
        "an unreadable value never widens what leaves the process"
    );
    assert_eq!(
        RedactionPolicy::for_mode(OtelRedactionMode::None),
        None,
        "one mode installs no policy at all"
    );
}

/// The one thing no corpus family can measure: whether an installed provider
/// reaches a collector at all. The exporter runs on the batch processor's own
/// thread, which blocks on the export rather than driving it on a reactor, so
/// an HTTP client that needs one exports nothing and takes its thread down with
/// it. A local collector is what tells the two apart, and this test fails the
/// moment the transport stops being able to run there.
#[tokio::test]
async fn an_installed_provider_reaches_a_local_collector() {
    let _exclusive = exclusive();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a local collector");
    let port = listener.local_addr().expect("the bound port").port();
    let collector = std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};

        let (mut stream, _) = listener.accept().expect("one export request");
        let mut request = vec![0_u8; 64 * 1024];
        let read = stream.read(&mut request).expect("the request");
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\n\r\n",
            )
            .expect("the collector answers");
        request.truncate(read);
        request
    });

    let table = document(&format!(
        "enable_telemetry = true\nenable_otel = true\notel_endpoint = \"http://127.0.0.1:{port}\"\n"
    ));
    let setup = setup_tracing(&table, &credentials(false));
    assert!(
        setup.is_installed(),
        "an explicit endpoint installs a provider with no credential of its own"
    );
    let outcome: Result<(), RuntimeError> = agent_span(
        AgentSpan {
            model: Some("model"),
            session_id: Some("session"),
        },
        async { Ok(()) },
    )
    .await;
    assert!(outcome.is_ok());
    match &setup {
        TracingSetup::Installed(guard) => guard.shutdown().expect("the pending batch is flushed"),
        _ => panic!("the provider was installed"),
    }
    drop(setup);
    uninstall();

    let request = collector.join().expect("the collector thread");
    let request = String::from_utf8_lossy(&request);
    let head = request.split("\r\n\r\n").next().unwrap_or_default();
    assert!(
        head.starts_with("POST /v1/traces "),
        "the exporter posts to the resolved traces path: {head}"
    );
    assert!(
        !head.to_lowercase().contains("authorization:"),
        "an endpoint the operator named carries no credential of ours"
    );
    assert!(
        request.contains("invoke_agent mistral-vibe"),
        "the span the turn opened reached the collector"
    );
}

/// The tracer this build asks for, which is what a collector reads the scope
/// from. Held here rather than in a family so a rename fails a test rather than
/// only a corpus comparison.
#[test]
fn the_tracer_is_named_after_the_product() {
    let _exclusive = exclusive();
    let harness = Harness::install();
    let tracer = global::tracer_provider().tracer_with_scope(scope());
    let mut span = tracer.start("probe");
    span.end();
    drop(span);
    let spans = harness.drain();
    let scope = &spans
        .first()
        .expect("the probe was exported")
        .instrumentation_scope;
    assert_eq!(scope.name(), TRACER_NAME);
    assert_eq!(scope.version(), Some(VERSION));
}
