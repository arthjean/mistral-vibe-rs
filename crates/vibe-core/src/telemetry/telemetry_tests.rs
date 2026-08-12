//! What the telemetry contract holds beyond what the corpus replays.
//!
//! The differential answers live in `telemetry_parity_tests`; these cover the
//! boundaries the reference has no case for: the label invariant this port
//! keeps on the events it authors, the credential that never leaves the
//! process, and the gate's failure mode.

use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;

use super::records::{
    ProjectPicker, ProjectSelectionSource, TelemetryToolStatus, TeleportFailureStage,
    TeleportProgress, TeleportTracker, ToolCallFinished,
};
use super::*;
use crate::compaction::{CompactionFailureReason, CompactionStatus};
use crate::events::LifecycleState;

#[derive(Default)]
struct RecordingTransport {
    calls: AtomicUsize,
    sent: Mutex<Vec<(String, String, TelemetryEnvelope)>>,
    fail: bool,
}

impl TelemetryTransport for RecordingTransport {
    fn send<'a>(
        &'a self,
        endpoint: &'a Url,
        user_agent: &'a str,
        _credential: &'a SecretString,
        envelope: &'a TelemetryEnvelope,
    ) -> TelemetryFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut sent) = self.sent.lock() {
            sent.push((
                endpoint.to_string(),
                user_agent.to_owned(),
                envelope.clone(),
            ));
        }
        let fail = self.fail;
        Box::pin(async move {
            if fail {
                return Err(TelemetryError::Delivery);
            }
            Ok(())
        })
    }
}

fn document(toml: &str) -> Table {
    toml.parse::<Table>().expect("the document parses")
}

/// The shipped shape: one Mistral provider serving the active model.
fn mistral_document(enable_telemetry: bool) -> Table {
    document(&format!(
        r#"
enable_telemetry = {enable_telemetry}
active_model = "oracle"

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
"#
    ))
}

fn credentials(name: &str) -> Option<String> {
    match name {
        "ORACLE_MISTRAL_KEY" => Some("oracle-mistral-sentinel".to_owned()),
        "ORACLE_THIRD_PARTY_KEY" => Some("oracle-third-party-sentinel".to_owned()),
        _ => None,
    }
}

fn getter(document: Table) -> TelemetryConfigGetter {
    Arc::new(move || TelemetryConfig::resolve(&document, &credentials))
}

fn context() -> TelemetryContext {
    TelemetryContext {
        launch: Some(LaunchContext {
            agent_entrypoint: "cli".to_owned(),
            agent_version: "oracle-agent-version".to_owned(),
            client_name: "oracle-client".to_owned(),
            client_version: "oracle-client-version".to_owned(),
            terminal_emulator: Some("ghostty".to_owned()),
        }),
        parent_session_id: Some("oracle-parent-session".to_owned()),
        experiments: BTreeMap::new(),
        user_plan: Some("oracle-plan".to_owned()),
    }
}

// -- US-005: the reference envelope ---------------------------------------

/// The body is `{"event", "properties"}` plus `"correlation_id"` only when one
/// is present: no `schemaVersion`, no `product`, no nesting.
#[test]
fn the_envelope_is_the_reference_open_properties_body() {
    let mut properties = Map::new();
    properties.insert("init_duration_ms".to_owned(), json!(1234));
    let without = TelemetryEnvelope::new("vibe.ready", properties.clone(), None);
    assert_eq!(
        serde_json::to_value(&without).expect("JSON"),
        json!({"event": "vibe.ready", "properties": {"init_duration_ms": 1234}})
    );
    let with = TelemetryEnvelope::new(
        "vibe.ready",
        properties.clone(),
        Some("oracle-correlation".to_owned()),
    );
    assert_eq!(
        serde_json::to_value(&with).expect("JSON")["correlation_id"],
        json!("oracle-correlation")
    );
    // Reference `if correlation_id:` drops an empty one rather than sending it.
    assert!(
        TelemetryEnvelope::new("vibe.ready", properties, Some(String::new()))
            .correlation_id
            .is_none()
    );
}

/// The endpoint is the server derived from the provider's `api_base`, and the
/// default base answers for a base carrying no version segment.
#[test]
fn the_endpoint_is_derived_from_the_provider_base() {
    for (api_base, expected) in [
        (
            "https://api.mistral.ai/v1",
            Some("https://api.mistral.ai/v1/datalake/events"),
        ),
        (
            "https://proxy.example.test/gateway/v1",
            Some("https://proxy.example.test/v1/datalake/events"),
        ),
        (
            "https://bare.example.test",
            Some("https://api.mistral.ai/v1/datalake/events"),
        ),
        ("", Some("https://api.mistral.ai/v1/datalake/events")),
        // A bearer token never travels in the clear, and never past a
        // credential of someone else's.
        ("http://plain.example.test/v1", None),
        ("https://user:password@proxy.example.test/v1", None),
    ] {
        assert_eq!(
            telemetry_endpoint(api_base).map(|url| url.to_string()),
            expected.map(ToOwned::to_owned),
            "{api_base}"
        );
    }
}

/// A delivery carries the three reference headers, and the user agent varies by
/// backend the way `get_user_agent` varies it.
#[test]
fn the_headers_are_the_reference_three() {
    let headers = telemetry_headers(
        &telemetry_user_agent(Some("mistral")),
        &SecretString::from("oracle-credential"),
    )
    .expect("the header set builds");
    let mut names = headers
        .keys()
        .map(|name| name.as_str().to_owned())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(names, ["authorization", "content-type", "user-agent"]);
    assert_eq!(
        headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer oracle-credential")
    );
    assert_eq!(
        telemetry_user_agent(Some("mistral")),
        format!("mistral-client-python/Mistral-Vibe/{VERSION}")
    );
    assert_eq!(
        telemetry_user_agent(Some("generic")),
        format!("Mistral-Vibe/{VERSION}")
    );
    assert_eq!(
        telemetry_user_agent(None),
        format!("Mistral-Vibe/{VERSION}")
    );
}

/// A third-party provider serving the active model does not supply the key: the
/// Mistral provider configured beside it does, and a document with no Mistral
/// provider at all resolves nothing.
#[test]
fn only_a_mistral_provider_supplies_the_credential() {
    let both = document(
        r#"
active_model = "oracle"

[[providers]]
name = "third-party"
api_base = "https://third-party.example.test/v1"
api_key_env_var = "ORACLE_THIRD_PARTY_KEY"

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "third-party"
alias = "oracle"
"#,
    );
    let target = TelemetryTarget::resolve(&both).expect("the Mistral provider answers");
    assert_eq!(target.credential_variable, "ORACLE_MISTRAL_KEY");
    assert!(TelemetryConfig::resolve(&both, &credentials).is_active());

    let third_party_only = document(
        r#"
active_model = "oracle"

[[providers]]
name = "third-party"
api_base = "https://third-party.example.test/v1"
api_key_env_var = "ORACLE_THIRD_PARTY_KEY"

[[models]]
name = "oracle-model"
provider = "third-party"
alias = "oracle"
"#,
    );
    assert_eq!(TelemetryTarget::resolve(&third_party_only), None);
    assert!(!TelemetryConfig::resolve(&third_party_only, &credentials).is_active());
}

/// A provider naming a variable nothing answers for resolves no credential, and
/// one naming no variable at all resolves no target.
#[tokio::test]
async fn an_unresolvable_variable_sends_nothing() {
    for (variable, active) in [("ORACLE_ABSENT_KEY", false), ("ORACLE_MISTRAL_KEY", true)] {
        let mut document = mistral_document(true);
        let providers = document
            .get_mut("providers")
            .and_then(toml::Value::as_array_mut)
            .expect("providers");
        providers[0].as_table_mut().expect("provider").insert(
            "api_key_env_var".to_owned(),
            toml::Value::String(variable.to_owned()),
        );
        let client = TelemetryClient::new(getter(document), RecordingTransport::default());
        assert_eq!(client.is_active(), active);
        let outcome = client
            .record(&TelemetryEnvelope::new("vibe.ready", Map::new(), None))
            .await
            .expect("the record resolves");
        assert_eq!(
            outcome,
            if active {
                TelemetryOutcome::Sent
            } else {
                TelemetryOutcome::NoEligibleCredential
            }
        );
    }
}

/// A failed delivery is swallowed by the observer, the turn is unaffected, and
/// the flush still joins the pending send.
#[tokio::test]
async fn a_failed_delivery_never_reaches_the_caller() {
    let client = TelemetryClient::new(
        getter(mistral_document(true)),
        RecordingTransport {
            fail: true,
            ..RecordingTransport::default()
        },
    );
    let observer = TelemetryEventObserver::new(client, context());
    observer
        .observe(&request_event())
        .expect("the observer never reports a delivery failure");
    observer.flush().await;
}

/// The label validators still refuse a path, a secret-shaped token and a
/// control character in an event this port authors, and properties a client
/// recorded travel unmodified.
#[test]
fn authored_labels_are_validated_and_client_properties_are_not() {
    for sensitive in [
        "/home/arthur/private.rs",
        "sk-secret",
        "https://user:password@proxy.invalid",
        "exception message",
        "tool\noutput",
    ] {
        let mut attributes = TelemetryAttributes::default();
        assert_eq!(
            attributes.label(TelemetryField::Model, sensitive),
            Err(TelemetryError::UnsafeLabel)
        );
    }
    let mut recorded = Map::new();
    recorded.insert(
        "path".to_owned(),
        json!("/home/arthur/private.rs\nsecond line"),
    );
    recorded.insert("nested".to_owned(), json!({"free": ["form", 1]}));
    let envelope = TelemetryEnvelope::new("client.event", recorded.clone(), None);
    assert_eq!(
        envelope.properties, recorded,
        "a client's properties are carried as recorded"
    );
}

/// An observer reached from outside a runtime drops the delivery rather than
/// failing the path that produced the event.
#[test]
fn an_observer_outside_a_runtime_drops_the_delivery() {
    let observer = TelemetryEventObserver::new(
        TelemetryClient::new(
            getter(mistral_document(true)),
            RecordingTransport::default(),
        ),
        context(),
    );
    observer
        .observe(&request_event())
        .expect("the observer never reports a queueing failure");
    assert_eq!(observer.client.transport.calls.load(Ordering::SeqCst), 0);
}

/// A correlation identifier this port authors is still bounded: the observer
/// refuses one that is not.
#[test]
fn an_authored_correlation_identifier_is_bounded() {
    let observer = TelemetryEventObserver::new(
        TelemetryClient::disabled(RecordingTransport::default()),
        context(),
    );
    assert_eq!(
        observer.queue(
            &TelemetryRecord::Ready {
                init_duration_ms: 12
            },
            None,
            Some("../session".to_owned()),
        ),
        Err(TelemetryError::InvalidIdentifier)
    );
}

// -- US-006: the metadata census ------------------------------------------

fn lifecycle_event(state: LifecycleState) -> EventEnvelope {
    EventEnvelope {
        session_id: "oracle-session".to_owned(),
        turn_id: Some("oracle-turn".to_owned()),
        emitted_at: 1,
        event_id: 1,
        event: EngineEvent::Lifecycle {
            state,
            message: None,
        },
    }
}

/// The request boundary event the engine emits before every model call.
fn request_event() -> EventEnvelope {
    EventEnvelope {
        session_id: "oracle-session".to_owned(),
        turn_id: Some("oracle-turn".to_owned()),
        emitted_at: 1,
        event_id: 1,
        event: EngineEvent::RequestSent {
            model: "oracle-model".to_owned(),
            agent_profile: "oracle-profile".to_owned(),
            nb_context_chars: 2048,
            nb_context_messages: 6,
            nb_prompt_chars: 512,
            nb_images: 0,
            supports_images: true,
            message_id: Some("oracle-message".to_owned()),
        },
    }
}

/// One tool call answered, as the engine reports the pair.
fn tool_events(content: &str, is_error: bool) -> [EventEnvelope; 2] {
    [
        EventEnvelope {
            session_id: "oracle-session".to_owned(),
            turn_id: Some("oracle-turn".to_owned()),
            emitted_at: 1,
            event_id: 2,
            event: EngineEvent::ToolCall {
                call_id: "call-1".to_owned(),
                name: "write_file".to_owned(),
                arguments: json!({"file_path": "/oracle/workspace/File.RS"}).to_string(),
            },
        },
        EventEnvelope {
            session_id: "oracle-session".to_owned(),
            turn_id: Some("oracle-turn".to_owned()),
            emitted_at: 1,
            event_id: 3,
            event: EngineEvent::ToolResult {
                call_id: "call-1".to_owned(),
                content: content.to_owned(),
                typed_result: json!({}),
                display: json!({}),
                duration_ms: 9,
                is_error,
                cancelled: false,
            },
        },
    ]
}

/// The base census carries the twelve reference fields, and a field with no
/// value is dropped rather than sent as null.
#[test]
fn the_base_census_carries_the_reference_fields() {
    let properties = context().base_metadata(Some("oracle-session")).properties();
    let mut keys = properties.keys().cloned().collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "agent_entrypoint",
            "agent_version",
            "client_name",
            "client_version",
            "os",
            "os_version",
            "parent_session_id",
            "session_id",
            "terminal_emulator",
            "user_plan",
            "version",
        ],
        "every field with a value is present, and `experiments` is absent"
    );
    assert_eq!(properties["version"], json!(VERSION));
    assert_eq!(properties["os"], json!(platform_id()));

    // No launch context, no session: only what the process knows about itself.
    let bare = TelemetryContext::default().base_metadata(None).properties();
    let mut keys = bare.keys().cloned().collect::<Vec<_>>();
    keys.sort_unstable();
    let expected: Vec<&str> = if platform_version().is_some() {
        vec!["os", "os_version", "version"]
    } else {
        vec!["os", "version"]
    };
    assert_eq!(keys, expected);
}

/// The request census adds the three reference fields, and `call_source`
/// defaults to `vibe_code`.
#[test]
fn the_request_census_adds_the_three_request_fields() {
    let properties = context()
        .request_metadata(
            Some("oracle-session"),
            TelemetryCallType::MainCall,
            Some("oracle-message".to_owned()),
        )
        .properties();
    assert_eq!(properties["call_type"], json!("main_call"));
    assert_eq!(properties["call_source"], json!(TELEMETRY_CALL_SOURCE));
    assert_eq!(properties["message_id"], json!("oracle-message"));
    assert_eq!(properties["session_id"], json!("oracle-session"));

    let secondary = context()
        .request_metadata(None, TelemetryCallType::SecondaryCall, None)
        .properties();
    assert_eq!(secondary["call_type"], json!("secondary_call"));
    assert!(
        !secondary.contains_key("message_id"),
        "a request with no message identifier drops the key"
    );
}

/// The experiments map is carried and stays absent while rank 16 is unshipped;
/// a filled one is sent as the reference sends it.
#[test]
fn experiments_are_absent_rather_than_empty() {
    assert!(
        !context()
            .base_metadata(None)
            .properties()
            .contains_key("experiments")
    );
    let filled = TelemetryContext {
        experiments: BTreeMap::from([("ab".to_owned(), "on".to_owned())]),
        ..context()
    };
    assert_eq!(
        filled.base_metadata(None).properties()["experiments"],
        json!({"ab": "on"})
    );
}

/// Reference `build_attachment_counts`: the count is reported only when the
/// provider accepts images, and never as a zero.
#[test]
fn attachment_counts_follow_the_image_support_gate() {
    assert!(attachment_counts(0, true).is_empty());
    assert!(attachment_counts(2, false).is_empty());
    assert_eq!(
        attachment_counts(2, true),
        BTreeMap::from([("image".to_owned(), 2)])
    );
}

/// Every terminal this port can name is a value the published vocabulary
/// carries, so a census field can never report a terminal a client cannot
/// parse.
#[test]
fn every_detected_terminal_is_a_published_value() {
    let markers = [
        (vec![("TERM_PROGRAM", "vscode")], "vscode"),
        (
            vec![
                ("TERM_PROGRAM", "vscode"),
                ("TERM_PROGRAM_VERSION", "1.90.0-insider"),
            ],
            "vscode_insiders",
        ),
        (
            vec![
                ("TERM_PROGRAM", "vscode"),
                ("VSCODE_IPC_HOOK_CLI", "/tmp/cursor-1.sock"),
            ],
            "cursor",
        ),
        (vec![("TERM_PROGRAM", "Apple_Terminal")], "apple_terminal"),
        (vec![("TERM_PROGRAM", "iTerm.app")], "iterm2"),
        (vec![("TERM_PROGRAM", "ghostty")], "ghostty"),
        (vec![("TERM_PROGRAM", "WezTerm")], "wezterm"),
        (vec![("TERM_PROGRAM", "Hyper")], "hyper"),
        (vec![("KITTY_WINDOW_ID", "1")], "kitty"),
        (vec![("ALACRITTY_LOG", "/tmp/alacritty.log")], "alacritty"),
        (vec![("WT_SESSION", "abc")], "windows_terminal"),
        (
            vec![("TERMINAL_EMULATOR", "JetBrains-JediTerm")],
            "jetbrains",
        ),
        (vec![], "unknown"),
    ];
    for (environment, expected) in markers {
        let detected = terminal_emulator_from(&|name| {
            environment
                .iter()
                .find(|(variable, _)| *variable == name)
                .map(|(_, value)| (*value).to_owned())
        });
        assert_eq!(detected, expected, "{environment:?}");
        let published = serde_json::from_value::<vibe_protocol::TerminalEmulator>(json!(detected))
            .ok()
            .and_then(|terminal| serde_json::to_value(terminal).ok());
        assert_eq!(
            published,
            Some(json!(detected)),
            "`{detected}` is a value the published vocabulary carries"
        );
    }
}

// -- US-007: the gate ------------------------------------------------------

/// `enable_telemetry` decides in both directions, and an absent key is the
/// reference's default of true.
#[tokio::test]
async fn enable_telemetry_decides_in_both_directions() {
    for (document, expected) in [
        (mistral_document(true), TelemetryOutcome::Sent),
        (mistral_document(false), TelemetryOutcome::Disabled),
    ] {
        let client = TelemetryClient::new(getter(document), RecordingTransport::default());
        assert_eq!(
            client
                .record(&TelemetryEnvelope::new("vibe.ready", Map::new(), None))
                .await
                .expect("the record resolves"),
            expected
        );
    }
    let mut unset = mistral_document(true);
    unset.remove("enable_telemetry");
    assert!(
        TelemetryConfig::resolve(&unset, &credentials).is_active(),
        "an unset key is the reference default of true"
    );
}

/// A configuration the caller cannot read resolves to disabled rather than
/// propagating, which is the reference's `_is_enabled` fallback.
#[tokio::test]
async fn an_unreadable_configuration_resolves_to_disabled() {
    let transport = RecordingTransport::default();
    let client = TelemetryClient::new(Arc::new(TelemetryConfig::disabled), transport);
    assert!(!client.is_active());
    assert_eq!(
        client
            .record(&TelemetryEnvelope::new("vibe.ready", Map::new(), None))
            .await
            .expect("the record resolves"),
        TelemetryOutcome::Disabled
    );
}

/// The gate is read on every send, so a document edited mid-session decides the
/// next event rather than the one the process started with.
#[tokio::test]
async fn a_reloaded_document_decides_the_next_event() {
    let enabled = Arc::new(Mutex::new(true));
    let read = Arc::clone(&enabled);
    let config: TelemetryConfigGetter = Arc::new(move || {
        let enabled = read.lock().map(|enabled| *enabled).unwrap_or_default();
        TelemetryConfig::resolve(&mistral_document(enabled), &credentials)
    });
    let client = TelemetryClient::new(config, RecordingTransport::default());
    let envelope = TelemetryEnvelope::new("vibe.ready", Map::new(), None);
    assert_eq!(
        client.record(&envelope).await.expect("first"),
        TelemetryOutcome::Sent
    );
    if let Ok(mut enabled) = enabled.lock() {
        *enabled = false;
    }
    assert_eq!(
        client.record(&envelope).await.expect("second"),
        TelemetryOutcome::Disabled
    );
}

// -- The engine projection -------------------------------------------------

/// What one engine event projects to, as the observer projects it.
fn project(event: &EventEnvelope) -> Vec<TelemetryRecord> {
    let observer = TelemetryEventObserver::new(
        TelemetryClient::disabled(RecordingTransport::default()),
        context(),
    );
    observer.project(event)
}

/// The properties one record puts on the wire.
fn properties(record: &TelemetryRecord) -> Map<String, Value> {
    record
        .attributes(None)
        .expect("the record carries no unsafe label")
        .into_properties()
}

/// The engine projection never copies event content into a property.
#[test]
fn engine_projection_never_copies_event_content() {
    let [call, result] = tool_events("sk-secret /home/arthur/private", true);
    let observer = TelemetryEventObserver::new(
        TelemetryClient::disabled(RecordingTransport::default()),
        context(),
    );
    observer.project(&call);
    let projected = observer.project(&result);
    let [record] = projected.as_slice() else {
        panic!("an answered tool call reports one record");
    };
    let envelope = TelemetryEnvelope::new(
        record.event().event_name(),
        merge_properties(
            context().base_metadata(Some("session")).properties(),
            properties(record),
        ),
        None,
    );
    let encoded = serde_json::to_string(&envelope).expect("JSON");
    assert!(!encoded.contains("sk-secret"));
    assert!(!encoded.contains("/home/arthur"));
    assert!(!encoded.contains("private"));
}

/// A turn completing is not a session closing.
#[test]
fn turn_completion_is_not_misreported_as_session_close() {
    assert!(project(&lifecycle_event(LifecycleState::Completed)).is_empty());
    assert!(
        project(&lifecycle_event(LifecycleState::Running)).is_empty(),
        "a running turn is a status, not a request: the request event reports one"
    );
}

/// US-009: the request event carries the eight reference fields, and the
/// envelope reports it on the session it was observed on.
#[tokio::test]
async fn a_request_event_reports_the_reference_payload() {
    let transport = RecordingTransport::default();
    let client = TelemetryClient::new(getter(mistral_document(true)), transport);
    let observer = TelemetryEventObserver::new(client, context());
    observer.observe(&request_event()).expect("observe");
    observer.flush().await;
    let sent = observer
        .client
        .transport
        .sent
        .lock()
        .map(|sent| sent.clone())
        .unwrap_or_default();
    let [(url, user_agent, envelope)] = sent.as_slice() else {
        panic!("one request event is delivered");
    };
    assert_eq!(url, "https://api.mistral.ai/v1/datalake/events");
    assert_eq!(
        user_agent,
        &format!("mistral-client-python/Mistral-Vibe/{VERSION}")
    );
    assert_eq!(envelope.event, "vibe.request_sent");
    assert_eq!(envelope.properties["model"], json!("oracle-model"));
    assert_eq!(envelope.properties["nb_context_chars"], json!(2048));
    assert_eq!(envelope.properties["nb_context_messages"], json!(6));
    assert_eq!(envelope.properties["nb_prompt_chars"], json!(512));
    assert_eq!(envelope.properties["call_type"], json!("main_call"));
    assert_eq!(envelope.properties["call_source"], json!("vibe_code"));
    assert_eq!(envelope.properties["message_id"], json!("oracle-message"));
    assert_eq!(envelope.properties["attachment_counts"], json!({}));
    assert_eq!(envelope.properties["session_id"], json!("oracle-session"));
    assert_eq!(
        envelope.correlation_id, None,
        "only a rating correlates with the request it rates"
    );
}

/// US-009: an answered tool call reports the eleven reference fields, with the
/// model, the profile and the message identifier read off the request it
/// belongs to.
#[test]
fn a_tool_call_reports_the_reference_payload() {
    let observer = TelemetryEventObserver::new(
        TelemetryClient::disabled(RecordingTransport::default()),
        context(),
    );
    observer.project(&request_event());
    let [call, result] = tool_events("written", false);
    observer.project(&call);
    let projected = observer.project(&result);
    let [record] = projected.as_slice() else {
        panic!("an answered tool call reports one record");
    };
    let properties = properties(record);
    let mut keys = properties.keys().cloned().collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "agent_profile_name",
            "approval_type",
            "decision",
            "file_extension",
            "message_id",
            "model",
            "nb_files_created",
            "nb_files_modified",
            "status",
            "tool_name",
        ],
        "the eleventh field, `bash_background`, is omitted where no mode was named"
    );
    assert_eq!(properties["tool_name"], json!("write_file"));
    assert_eq!(properties["status"], json!("success"));
    assert_eq!(properties["model"], json!("oracle-model"));
    assert_eq!(properties["agent_profile_name"], json!("oracle-profile"));
    assert_eq!(properties["message_id"], json!("oracle-message"));
    assert_eq!(properties["nb_files_created"], json!(1));
    assert_eq!(properties["nb_files_modified"], json!(0));
    assert_eq!(
        properties["file_extension"],
        json!(".rs"),
        "the suffix is lowercased, as `_extract_file_extension` lowercases it"
    );
    assert_eq!(
        (&properties["decision"], &properties["approval_type"]),
        (&Value::Null, &Value::Null),
        "no decision was recorded, so both travel as null rather than absent"
    );
}

/// US-009: a call the operator declined is `skipped` with the reference's
/// verdict, and a call that failed on its own is a `failure` with none.
#[test]
fn a_declined_call_is_skipped_and_a_failed_one_is_a_failure() {
    for (content, status, decision) in [
        (
            format!("{}approval denied", crate::policy::DENIAL_PREFIX),
            json!("skipped"),
            json!("skip"),
        ),
        ("the tool crashed".to_owned(), json!("failure"), Value::Null),
    ] {
        let observer = TelemetryEventObserver::new(
            TelemetryClient::disabled(RecordingTransport::default()),
            context(),
        );
        let [call, result] = tool_events(&content, true);
        observer.project(&call);
        let projected = observer.project(&result);
        let [record] = projected.as_slice() else {
            panic!("an answered tool call reports one record");
        };
        let properties = properties(record);
        assert_eq!(properties["status"], status);
        assert_eq!(properties["decision"], decision);
        assert_eq!(
            properties["nb_files_created"],
            json!(0),
            "a call that produced no result reports no file metrics"
        );
        assert_eq!(properties["file_extension"], Value::Null);
    }
}

/// US-009: the mode a bash result reports wins over the requested one, and
/// neither being a boolean omits the key.
#[test]
fn the_bash_mode_is_read_from_the_result_first() {
    for (arguments, result, expected) in [
        (
            json!({"background": false}),
            json!({"background": true}),
            Some(true),
        ),
        (json!({"background": true}), json!({}), Some(true)),
        (json!({}), json!({}), None),
    ] {
        let call = ToolCallFinished::new(records::ToolCallReport {
            tool_name: "bash",
            status: TelemetryToolStatus::Success,
            arguments: &arguments,
            result: Some(&result),
            decision: None,
            agent_profile_name: "profile",
            model: "model",
            message_id: None,
        });
        assert_eq!(call.bash_background, expected);
        let record = TelemetryRecord::ToolCallFinished(call);
        assert_eq!(
            properties(&record).get("bash_background"),
            expected.map(Value::Bool).as_ref()
        );
    }
}

/// US-008: the session census carries the four counts plus the launch context
/// the reference repeats, and no launch context reports an unknown entrypoint
/// with the client fields absent.
#[test]
fn the_new_session_payload_carries_the_launch_context() {
    let session = records::NewSession {
        has_agents_md: true,
        nb_skills: 3,
        nb_mcp_servers: 2,
        nb_models: 4,
    };
    let record = TelemetryRecord::NewSession(session);
    let launch = LaunchContext {
        agent_entrypoint: "cli".to_owned(),
        agent_version: "1.0".to_owned(),
        client_name: "oracle-client".to_owned(),
        client_version: "2.0".to_owned(),
        terminal_emulator: Some("ghostty".to_owned()),
    };
    let carried = record
        .attributes(Some(&launch))
        .expect("the payload carries no unsafe label")
        .into_properties();
    assert_eq!(carried["entrypoint"], json!("cli"));
    assert_eq!(carried["client_name"], json!("oracle-client"));
    assert_eq!(carried["terminal_emulator"], json!("ghostty"));
    assert_eq!(carried["has_agents_md"], json!(true));
    assert_eq!(carried["nb_skills"], json!(3));

    let bare = properties(&record);
    assert_eq!(bare["entrypoint"], json!("unknown"));
    assert_eq!(bare["client_name"], Value::Null);
    assert_eq!(bare["client_version"], Value::Null);
    assert_eq!(bare["terminal_emulator"], Value::Null);
}

/// US-008: the startup event reports each duration as null when its
/// measurement never happened.
#[test]
fn an_unmeasured_startup_duration_is_null() {
    let record = TelemetryRecord::Startup(records::Startup {
        first_frame_duration_ms: Some(12),
        ..records::Startup::default()
    });
    let properties = properties(&record);
    assert_eq!(properties["first_frame_duration_ms"], json!(12));
    assert_eq!(properties["agent_ready_duration_ms"], Value::Null);
    assert_eq!(properties["session_init_duration_ms"], Value::Null);
}

/// US-010: the leading slash is stripped, as the reference's `lstrip` strips
/// it.
#[test]
fn a_slash_command_reports_the_bare_name() {
    let record = TelemetryRecord::SlashCommandUsed {
        command: "/compact".to_owned(),
        kind: records::TelemetryCommandKind::Skill,
    };
    let properties = properties(&record);
    assert_eq!(properties["command"], json!("compact"));
    assert_eq!(properties["command_type"], json!("skill"));
}

/// US-011: the tracker walks the reference's stages, clears a saved link the
/// service refused, and sends no failure for a run that succeeded.
#[test]
fn the_teleport_tracker_walks_the_reference_stages() {
    let picker = ProjectPicker {
        shown: true,
        selection_source: Some(ProjectSelectionSource::SavedLink),
        candidate_count_loaded: Some(3),
        multi_repo_match_count: Some(2),
        saved_project_link_cleared: Some(false),
        repo_remote_changed: Some(false),
    };
    let mut tracker = TeleportTracker::new(12, TeleportFailureStage::Ineligible, Some(picker));
    tracker.record_progress(TeleportProgress::SummarizingContext);
    tracker.record_context_summary_generated(480);
    tracker.record_progress(TeleportProgress::CheckingGit);
    tracker.record_progress(TeleportProgress::Pushing);
    tracker.record_service_error("ServiceTeleportError", Some("http".to_owned()), Some(403));
    let failed = tracker
        .failed()
        .expect("a classified error reports a failure");
    let carried = properties(&failed);
    assert_eq!(failed.event(), TelemetryEvent::TeleportFailed);
    assert_eq!(carried["stage"], json!("push"));
    assert_eq!(carried["error_class"], json!("ServiceTeleportError"));
    assert_eq!(carried["failure_kind"], json!("http"));
    assert_eq!(carried["http_status_code"], json!(403));
    assert_eq!(carried["context_summary"], json!("generated"));
    assert_eq!(carried["context_summary_chars"], json!(480));
    assert_eq!(carried["nb_session_messages"], json!(12));
    assert_eq!(
        carried["saved_project_link_cleared"],
        json!(true),
        "a saved link the service answered 403 for is the link it refused"
    );
    assert_eq!(carried["project_multi_repo_match_count"], json!(2));

    tracker.record_progress(TeleportProgress::Complete);
    assert!(
        tracker.failed().is_none(),
        "a run that completed sends no failure, whatever it recorded on the way"
    );
    assert_eq!(
        properties(&tracker.completed())["push_required"],
        json!(false)
    );

    let mut cancelled = TeleportTracker::new(0, TeleportFailureStage::NoHistory, None);
    cancelled.record_cancelled();
    let record = cancelled.failed().expect("a cancellation is a failure");
    let carried = properties(&record);
    assert_eq!(carried["stage"], json!("cancelled"));
    assert_eq!(carried["error_class"], json!("CancelledError"));

    let silent = TeleportTracker::new(0, TeleportFailureStage::NoHistory, None);
    assert!(
        silent.failed().is_none(),
        "a run that classified no error sends nothing"
    );
}

/// US-011: reference `count_multi_repo_matches`. A project counts when it
/// carries more than one repository and one of them is the current remote, so
/// the ordinary multi-repository project is what the count is about; a
/// single-repository project and a multi-repository project linked elsewhere
/// both count for nothing, and a workspace on no remote counts nothing at all.
#[test]
fn the_multi_repo_count_counts_linked_projects_carrying_several_repositories() {
    let remote = "git@example.test:vibe.git";
    let other = "git@example.test:other.git".to_owned();
    let projects: Vec<Vec<String>> = vec![
        vec![remote.to_owned(), other.clone()],
        vec![remote.to_owned(), remote.to_owned()],
        vec![remote.to_owned()],
        vec![other.clone(); 2],
    ];
    assert_eq!(
        records::multi_repo_match_count(projects.iter().map(Vec::as_slice), remote),
        2,
        "the two linked projects carrying a second repository are the matches"
    );
    assert_eq!(
        records::multi_repo_match_count(projects.iter().map(Vec::as_slice), ""),
        0,
        "a workspace on no remote matches nothing"
    );
}

/// US-012: playback that never started reports zero rather than a negative or
/// an absent measure, and the key set is the reference's.
#[test]
fn a_read_aloud_that_never_played_reports_zero() {
    let record = TelemetryRecord::ReadAloudEnded {
        read_aloud_session_id: "read-1".to_owned(),
        status: records::ReadAloudStatus::Canceled,
        error_type: None,
        elapsed: std::time::Duration::ZERO,
    };
    let properties = properties(&record);
    let mut keys = properties.keys().cloned().collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "elapsed_seconds",
            "error_type",
            "read_aloud_session_id",
            "speed_selection",
            "status",
        ]
    );
    assert_eq!(properties["elapsed_seconds"], json!(0.0));
    assert_eq!(properties["status"], json!("canceled"));
    assert_eq!(properties["error_type"], Value::Null);
    assert_eq!(properties["speed_selection"], Value::Null);
}

/// US-012: a transcription failure carries the endpoint's own message, which is
/// the one payload the label validators are not applied to.
#[test]
fn a_transcription_failure_carries_its_message() {
    let record = TelemetryRecord::TranscriptionFailed {
        recording_id: "req-1".to_owned(),
        message: "the endpoint refused: /tmp/audio.wav".to_owned(),
        transcription_duration: std::time::Duration::from_millis(1500),
        recording_duration: None,
    };
    let properties = properties(&record);
    assert_eq!(
        properties["error_message"],
        json!("the endpoint refused: /tmp/audio.wav")
    );
    assert_eq!(properties["transcription_duration_ms"], json!(1500.0));
    assert_eq!(
        properties["recording_duration_ms"],
        Value::Null,
        "a recording that never stopped cleanly reports no duration"
    );
}

/// Every event name this build publishes is produced by a record, so a declared
/// name can never ship without a producer.
#[test]
fn every_published_event_name_has_a_record() {
    let records = [
        TelemetryRecord::NewSession(records::NewSession {
            has_agents_md: false,
            nb_skills: 0,
            nb_mcp_servers: 0,
            nb_models: 0,
        }),
        TelemetryRecord::SessionClosed,
        TelemetryRecord::Ready {
            init_duration_ms: 1,
        },
        TelemetryRecord::Startup(records::Startup::default()),
        TelemetryRecord::RequestSent(records::RequestSent {
            model: "model".to_owned(),
            nb_context_chars: 0,
            nb_context_messages: 0,
            nb_prompt_chars: 0,
            call_type: TelemetryCallType::MainCall,
            message_id: None,
            attachment_counts: BTreeMap::new(),
        }),
        TelemetryRecord::ToolCallFinished(ToolCallFinished::new(records::ToolCallReport {
            tool_name: "bash",
            status: TelemetryToolStatus::Success,
            arguments: &json!({}),
            result: None,
            decision: None,
            agent_profile_name: "profile",
            model: "model",
            message_id: None,
        })),
        TelemetryRecord::AtMentionInserted(records::AtMentionInserted {
            nb_mentions: 1,
            context_types: BTreeMap::new(),
            file_extensions: None,
            message_id: None,
        }),
        TelemetryRecord::AutoCompactTriggered {
            nb_context_tokens_before: 0,
            auto_compact_threshold: 0,
            status: "success",
        },
        TelemetryRecord::CompactionFailed {
            reason: "tool_call",
        },
        TelemetryRecord::SlashCommandUsed {
            command: "help".to_owned(),
            kind: records::TelemetryCommandKind::Builtin,
        },
        TelemetryRecord::UserCopiedText { text_length: 1 },
        TelemetryRecord::UserCancelledAction {
            action: "interrupt_agent".to_owned(),
        },
        TelemetryRecord::VoiceModeToggled { enabled: true },
        TelemetryRecord::OnboardingApiKeyAdded {
            custom_domain: false,
        },
        TelemetryRecord::FeedbackSubmitted {
            rating: 5,
            model: "model".to_owned(),
        },
        TeleportTracker::new(0, TeleportFailureStage::NoHistory, None).completed(),
        records::teleport_early_failure(TeleportFailureStage::NoHistory, "Error", 0),
        TelemetryRecord::RemoteProjectConfigured {
            outcome: records::RemoteProjectOutcome::Configured,
            picker: ProjectPicker::hidden(),
        },
        TelemetryRecord::TranscriptionStarted {
            recording_id: "req".to_owned(),
        },
        TelemetryRecord::TranscriptionCancelled {
            recording_id: "req".to_owned(),
            recording_duration: std::time::Duration::ZERO,
        },
        TelemetryRecord::TranscriptionDone {
            recording_id: "req".to_owned(),
            transcript_length: 0,
            transcription_duration: std::time::Duration::ZERO,
            recording_duration: std::time::Duration::ZERO,
        },
        TelemetryRecord::TranscriptionFailed {
            recording_id: "req".to_owned(),
            message: String::new(),
            transcription_duration: std::time::Duration::ZERO,
            recording_duration: None,
        },
        TelemetryRecord::ReadAloudRequested {
            read_aloud_session_id: "read".to_owned(),
            trigger: records::ReadAloudTrigger::AutoplayNextMessage,
        },
        TelemetryRecord::ReadAloudPlayStarted {
            read_aloud_session_id: "read".to_owned(),
            time_to_first_read: std::time::Duration::ZERO,
        },
        TelemetryRecord::ReadAloudEnded {
            read_aloud_session_id: "read".to_owned(),
            status: records::ReadAloudStatus::Completed,
            error_type: None,
            elapsed: std::time::Duration::ZERO,
        },
    ];
    let produced = records
        .iter()
        .map(TelemetryRecord::event)
        .collect::<Vec<_>>();
    for event in TelemetryEvent::ALL {
        assert!(
            produced.contains(&event),
            "`{}` is published without a record that produces it",
            event.event_name()
        );
    }
    for record in &records {
        record
            .attributes(None)
            .expect("every payload this port authors passes its own validators");
    }
}

fn compaction_outcome(
    status: CompactionStatus,
    reason: Option<CompactionFailureReason>,
) -> EventEnvelope {
    EventEnvelope {
        session_id: "session".to_owned(),
        turn_id: Some("turn-1".to_owned()),
        emitted_at: 1,
        event_id: 1,
        event: EngineEvent::CompactionOutcome {
            compaction_id: "compaction-1".to_owned(),
            status,
            context_tokens_before: 150_000,
            threshold: 120_000,
            reason,
        },
    }
}

/// US-151: the auto-compaction record carries the tokens before, the threshold
/// and the status, which is the reference's payload.
#[test]
fn a_compaction_outcome_reports_the_reference_payload() {
    for (status, label) in [
        (CompactionStatus::Success, "success"),
        (CompactionStatus::Failure, "failure"),
        (CompactionStatus::Cancelled, "cancelled"),
    ] {
        let projections = project(&compaction_outcome(status, None));
        let [record] = projections.as_slice() else {
            panic!("an unclassified outcome reports one record");
        };
        assert_eq!(record.event(), TelemetryEvent::AutoCompactTriggered);
        let encoded = properties(record);
        assert_eq!(encoded["nb_context_tokens_before"], 150_000);
        assert_eq!(encoded["auto_compact_threshold"], 120_000);
        assert_eq!(encoded["status"], label);
    }
}

/// US-151: a classified failure reports the failure record too, carrying the
/// reason and nothing else.
#[test]
fn a_classified_failure_reports_both_records() {
    let projections = project(&compaction_outcome(
        CompactionStatus::Failure,
        Some(CompactionFailureReason::ToolCall),
    ));
    let [triggered, failed] = projections.as_slice() else {
        panic!("a classified failure reports two records");
    };
    assert_eq!(triggered.event(), TelemetryEvent::AutoCompactTriggered);
    assert_eq!(failed.event(), TelemetryEvent::CompactionFailed);
    assert_eq!(
        Value::Object(properties(failed)),
        json!({"reason": "tool_call"}),
        "the failure record carries the reason and no transcript"
    );
}

/// US-151: telemetry disabled writes nothing, whatever the compaction did.
#[tokio::test]
async fn a_disabled_client_writes_no_compaction_record() {
    let client = TelemetryClient::new(
        getter(mistral_document(false)),
        RecordingTransport::default(),
    );
    let observer = TelemetryEventObserver::new(client, context());
    observer
        .observe(&compaction_outcome(
            CompactionStatus::Failure,
            Some(CompactionFailureReason::EmptySummary),
        ))
        .expect("observe");
    observer.flush().await;
    assert_eq!(observer.client.transport.calls.load(Ordering::SeqCst), 0);
}
