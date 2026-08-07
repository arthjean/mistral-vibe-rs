//! Differential oracle for the app-server wire surface.
//!
//! The Python reference is the authority on what crosses the JSON-RPC boundary.
//! `scripts/parity/app_server_surface.py` imports its `protocol` and
//! `_connection_protocol` modules and records the method inventory, the
//! client-tool and notification vocabularies, the error codes, the enum value
//! sets, the discriminated unions and a per-model field census. This module
//! replays that corpus against what this build declares, routes and answers.
//!
//! The corpus is committed, like the configuration one: it carries names,
//! aliases, required flags, type kinds and enum values, and no reference-authored
//! prose, which is what `NOTICE` forbids shipping. Replay therefore runs
//! unconditionally; only the live probe that recaptures from the pinned checkout
//! skips when it is absent or off-pin.
//!
//! Four families are replayed. `methods` compares the corpus inventory against
//! `SERVER_METHODS` and against what this build routes. `notifications` compares
//! the reference vocabulary against `EMITTED_NOTIFICATIONS`. `models` drives a
//! real connection and validates the response bodies it produces against the
//! field census. `enums` compares the vocabularies this port already declares.
//!
//! Each family carries a ledger of what is still divergent, with the story that
//! closes it. A ledger entry is not a waiver: the replay fails when a divergence
//! appears outside it, and fails again when an entry becomes stale because the
//! divergence is gone. That is what makes the backlog shrink rather than rot.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;
use serde_json::{Value, json};

use vibe_core::process::ClientToolRequest;
use vibe_protocol::{
    Envelope, LOCAL_EXTENSION_METHODS, SERVER_METHODS, TransportKind, decode_frame,
    is_server_method,
};

use crate::server::{
    AppServer, DeferredWork, DispatchBatch, EMITTED_NOTIFICATIONS, ServerConnection, routed_methods,
};

use vibe_core::parity::{REFERENCE_COMMIT, off_pin_reason, pinned_interpreter, reference_root};

const CAPTURE_SCRIPT: &str = "scripts/parity/app_server_surface.py";
const CORPUS_RELATIVE: &str = "tests/app-server-surface/corpus.json";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 1;
/// The session the probe opens; every session-scoped method is asked about it.
const PROBE_SESSION: &str = "session-1";

/// Reference methods this build does not route yet, each with the story that
/// adds it. A method routed while listed here fails the replay as a stale entry.
///
/// US-098 routed the last of them, so the list is empty and a method that stops
/// being routed has to earn an entry here before the replay accepts it.
const UNROUTED_METHODS: &[(&str, &str)] = &[];

/// Reference notifications this build does not emit yet.
///
/// US-085 emitted the last of them, so the list is empty and a notification
/// that stops being emitted has to earn an entry here before the replay
/// accepts it.
const UNEMITTED_NOTIFICATIONS: &[(&str, &str)] = &[];

/// Notification names this build emits that the reference does not declare.
///
/// US-086 retired the four this port had invented, so the list is empty and any
/// new name has to earn an entry here before the replay accepts it.
const LOCAL_NOTIFICATIONS: &[(&str, &str)] = &[];

/// Enum vocabularies the reference declares that this port does not model yet.
const UNMODELED_ENUMS: &[(&str, &str)] = &[("TerminalEmulator", "US-081")];

/// Methods whose probed response does not validate against the census yet, each
/// with the story that fixes it. A method that starts validating while listed
/// here fails the replay as a stale entry.
/// US-093 closed the last one, so a response that stops validating has to earn
/// an entry here before the replay accepts it.
const DIVERGENT_RESPONSES: &[(&str, &str)] = &[];

/// Read-only methods the probe calls, with the parameters they take.
///
/// Only reads: the probe drives a real server against the operator's own vibe
/// home, so a mutating method would have side effects a test must not have.
fn probe_requests() -> Vec<(&'static str, Value)> {
    let session = json!({"sessionId": PROBE_SESSION});
    vec![
        ("account/read", session.clone()),
        ("agents/list", session.clone()),
        ("config/read", session.clone()),
        ("config/schema", session.clone()),
        ("connectors/auth/read", session.clone()),
        ("connectors/read", session.clone()),
        ("diagnostics/list", session.clone()),
        ("history/list", session.clone()),
        ("mcp/read", session.clone()),
        // The session-less surface takes no session, and a path that resolves
        // to no repository is answered rather than refused, which is what makes
        // three of its methods probeable from a bare server.
        ("projectLinks/list", json!({})),
        (
            "projectLinks/resolveRoot",
            json!({"rootPath": "/workspace"}),
        ),
        (
            "projectLinks/inspectRoot",
            json!({"rootPath": "/workspace"}),
        ),
        ("review/state", session.clone()),
        ("runtime/read", session.clone()),
        ("session/list", json!({})),
        ("session/read", session.clone()),
        ("session/ready/read", session.clone()),
        ("skills/list", session.clone()),
        ("stats/read", session.clone()),
        (
            "telemetry/record",
            json!({"sessionId": PROBE_SESSION, "name": "probe", "properties": {}}),
        ),
        ("tools/list", session.clone()),
        (
            "workspace/trust/status",
            json!({"sessionId": PROBE_SESSION, "cwd": "/workspace"}),
        ),
    ]
}

/// Probed methods this build cannot answer from a bare session, each with why.
///
/// A method that starts answering while listed here fails the replay as a stale
/// entry, and one that stops answering has to earn a line here rather than
/// leaving the conforming count quietly counting fewer methods than it names.
const UNREACHABLE_PROBES: &[(&str, &str)] = &[
    (
        "connectors/auth/read",
        "no connector is configured in a bare probe session, so there is no name to authorize",
    ),
    (
        "history/list",
        "the probe session is never written to the store, and the transcript is read from it",
    ),
];

// --------------------------------------------------------------------------
// Corpus
// --------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference: Reference,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    methods: Vec<MethodEntry>,
    client_tool_methods: Vec<MethodEntry>,
    server_requests: Vec<MethodEntry>,
    notifications: Vec<NotificationEntry>,
    error_codes: Vec<String>,
    enums: Vec<EnumEntry>,
    /// Vocabularies the PRD names that the pinned reference does not declare.
    absent_enums: Vec<String>,
    unions: Vec<UnionEntry>,
    models: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MethodEntry {
    name: String,
    params: String,
    response: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotificationEntry {
    name: String,
    params: String,
    sequenced: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnumEntry {
    name: String,
    values: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnionEntry {
    name: String,
    discriminator: Option<String>,
    variants: Vec<UnionVariant>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnionVariant {
    model: String,
    value: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelEntry {
    name: String,
    fields: Vec<FieldEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldEntry {
    #[expect(
        dead_code,
        reason = "the wire compares aliases; the snake name documents them"
    )]
    name: String,
    alias: String,
    required: bool,
    kind: String,
    optional: bool,
    models: Vec<String>,
    #[expect(dead_code, reason = "the enum family compares vocabularies by name")]
    enums: Vec<String>,
    /// The values a literal field may take, which is how a variant whose
    /// discriminator is a literal set is resolved.
    literals: Vec<Value>,
}

fn corpus_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(CORPUS_RELATIVE)
}

fn corpus() -> Corpus {
    let path = corpus_path();
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|error| unreachable!("{CORPUS_RELATIVE} reads: {error}"));
    let corpus: Corpus = serde_json::from_str(&raw)
        .unwrap_or_else(|error| unreachable!("{CORPUS_RELATIVE} parses: {error}"));
    assert_eq!(
        corpus.schema_version, CORPUS_SCHEMA_VERSION,
        "the corpus was captured by another version of {CAPTURE_SCRIPT}"
    );
    assert_eq!(
        corpus.reference.commit, REFERENCE_COMMIT,
        "the corpus was captured from an unpinned reference"
    );
    corpus
}

fn ledger(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(name, story)| ((*name).to_owned(), (*story).to_owned()))
        .collect()
}

// --------------------------------------------------------------------------
// The method inventory
// --------------------------------------------------------------------------

#[test]
fn the_inventory_is_exactly_the_reference_inventory() {
    let corpus = corpus();
    let declared = SERVER_METHODS.iter().copied().collect::<BTreeSet<_>>();
    let reference = corpus
        .methods
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        reference.difference(&declared).copied().collect::<Vec<_>>(),
        Vec::<&str>::new(),
        "the reference declares these methods and SERVER_METHODS does not"
    );
    assert_eq!(
        declared.difference(&reference).copied().collect::<Vec<_>>(),
        Vec::<&str>::new(),
        "SERVER_METHODS invents these methods"
    );
    eprintln!(
        "app-server surface: methods {}/{} declared",
        declared.len(),
        reference.len()
    );
}

#[test]
fn every_routed_method_is_in_the_reference_inventory_or_a_local_extension() {
    let corpus = corpus();
    let reference = corpus
        .methods
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<BTreeSet<_>>();
    let extensions = LOCAL_EXTENSION_METHODS
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let invented = routed_methods()
        .into_iter()
        .filter(|method| !reference.contains(method) && !extensions.contains(method))
        .collect::<Vec<_>>();
    assert_eq!(
        invented,
        Vec::<&str>::new(),
        "routed but absent from both the corpus and LOCAL_EXTENSION_METHODS"
    );
    for method in &extensions {
        assert!(
            !reference.contains(method),
            "{method} is declared by the reference and is not a local extension"
        );
    }
}

#[test]
fn the_unrouted_reference_methods_are_exactly_the_recorded_backlog() {
    let corpus = corpus();
    let routed = routed_methods();
    let backlog = ledger(UNROUTED_METHODS);
    let unrouted = corpus
        .methods
        .iter()
        .map(|entry| entry.name.clone())
        .filter(|method| !routed.contains(method.as_str()))
        .collect::<BTreeSet<_>>();
    let missing = unrouted
        .iter()
        .filter(|method| !backlog.contains_key(*method))
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "these reference methods are unrouted and unrecorded: {missing:?}"
    );
    let stale = backlog
        .keys()
        .filter(|method| !unrouted.contains(*method))
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "these methods are routed now and their backlog entry is stale: {stale:?}"
    );
    eprintln!(
        "app-server surface: methods {}/{} routed, {} awaiting a story",
        corpus.methods.len() - unrouted.len(),
        corpus.methods.len(),
        unrouted.len()
    );
}

/// The handshake is not a routed method, so the response replay above never
/// reaches it. It is the first frame a reference client sends and the one this
/// port used to reject, so its answer is validated here against the same census.
#[test]
fn the_handshake_answer_validates_against_the_census() {
    let corpus = corpus();
    let census = Census::new(&corpus);
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    let response = handshake_response(&mut connection);

    let mut issues = Vec::new();
    census.validate("", "InitializeResponse", &response, &mut issues);
    assert!(
        issues.is_empty(),
        "the handshake answer diverges from the census: {issues:?}"
    );

    // `validate` walks the nested models the response carries, so reaching
    // `ServerCapabilities` here is what proves the census entry was exercised
    // rather than merely present.
    assert!(
        response["capabilities"]["methods"].is_array(),
        "the handshake carries no advertised method list: {response}"
    );
    // `ClientCapabilities` and `ClientInfo` travel the other way, so they are
    // validated against what a conforming client sends.
    for (model, sent) in [
        (
            "ClientInfo",
            json!({"name": "editor", "version": "1", "title": "Editor", "entrypoint": "ide", "terminalEmulator": "unknown"}),
        ),
        (
            "ClientCapabilities",
            json!({"callbackKinds": ["approval"], "clientTools": ["filesystem/read"], "disabledNotifications": ["warning"]}),
        ),
    ] {
        let mut issues = Vec::new();
        census.validate("", model, &sent, &mut issues);
        assert!(
            issues.is_empty(),
            "a conforming client's {model} diverges from the census: {issues:?}"
        );
    }
}

#[test]
fn the_advertised_surface_carries_no_local_extension() {
    let (server, mut connection) = probe_connection();
    let response = probe(
        &server,
        &mut connection,
        "runtime/read",
        &json!({"sessionId": PROBE_SESSION}),
    );
    assert!(response.is_some(), "the probe session answers requests");

    let advertised = advertised_methods_from_handshake();
    for method in LOCAL_EXTENSION_METHODS {
        assert!(
            !advertised.contains(&method.to_owned()),
            "{method} is advertised to a reference client"
        );
    }
    assert!(
        advertised.iter().all(|method| is_server_method(method)),
        "the handshake advertises a name outside the reference inventory"
    );
}

// --------------------------------------------------------------------------
// Notifications, error codes and enums
// --------------------------------------------------------------------------

#[test]
fn the_emitted_notifications_are_reference_names_or_recorded_extensions() {
    let corpus = corpus();
    let reference = corpus
        .notifications
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<BTreeSet<_>>();
    let local = ledger(LOCAL_NOTIFICATIONS);
    let unrecorded = EMITTED_NOTIFICATIONS
        .iter()
        .filter(|name| !reference.contains(*name) && !local.contains_key(**name))
        .collect::<Vec<_>>();
    assert!(
        unrecorded.is_empty(),
        "these notification names are neither reference names nor recorded extensions: {unrecorded:?}"
    );
    let stale = local
        .keys()
        .filter(|name| !EMITTED_NOTIFICATIONS.contains(&name.as_str()))
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "these local notification entries are stale: {stale:?}"
    );
}

#[test]
fn the_unemitted_reference_notifications_are_exactly_the_recorded_backlog() {
    let corpus = corpus();
    let backlog = ledger(UNEMITTED_NOTIFICATIONS);
    let unemitted = corpus
        .notifications
        .iter()
        .map(|entry| entry.name.clone())
        .filter(|name| !EMITTED_NOTIFICATIONS.contains(&name.as_str()))
        .collect::<BTreeSet<_>>();
    let missing = unemitted
        .iter()
        .filter(|name| !backlog.contains_key(*name))
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "these reference notifications are not emitted and not recorded: {missing:?}"
    );
    let stale = backlog
        .keys()
        .filter(|name| !unemitted.contains(*name))
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "these notifications are emitted now and their backlog entry is stale: {stale:?}"
    );
    eprintln!(
        "app-server surface: notifications {}/{} emitted, {} awaiting a story",
        corpus.notifications.len() - unemitted.len(),
        corpus.notifications.len(),
        unemitted.len()
    );

    // Every sequenced notification carries the watermark the client counts on,
    // which is what the mute list must never break.
    let sequenced = corpus
        .notifications
        .iter()
        .filter(|entry| entry.sequenced)
        .count();
    assert_eq!(sequenced, 9, "the reference sequences nine notifications");
}

#[test]
fn every_error_code_the_reference_declares_is_spoken_here() {
    let corpus = corpus();
    let declared = corpus.error_codes.iter().cloned().collect::<BTreeSet<_>>();
    let spoken = [
        vibe_protocol::ProtocolErrorCode::InvalidRequest,
        vibe_protocol::ProtocolErrorCode::InvalidParams,
        vibe_protocol::ProtocolErrorCode::NotInitialized,
        vibe_protocol::ProtocolErrorCode::NotFound,
        vibe_protocol::ProtocolErrorCode::Conflict,
        vibe_protocol::ProtocolErrorCode::StaleTurn,
        vibe_protocol::ProtocolErrorCode::NotSteerable,
        vibe_protocol::ProtocolErrorCode::CompactionFailed,
        vibe_protocol::ProtocolErrorCode::Unauthorized,
        vibe_protocol::ProtocolErrorCode::Forbidden,
        vibe_protocol::ProtocolErrorCode::MethodNotFound,
        vibe_protocol::ProtocolErrorCode::InternalError,
    ]
    .into_iter()
    .map(|code| {
        serde_json::to_value(code)
            .ok()
            .and_then(|value| value.as_str().map(ToOwned::to_owned))
            .unwrap_or_default()
    })
    .collect::<BTreeSet<_>>();
    assert_eq!(declared, spoken, "the error vocabulary diverged");
    eprintln!(
        "app-server surface: error codes {}/{}",
        spoken.len(),
        declared.len()
    );
}

#[test]
fn the_enum_vocabularies_this_port_declares_match_the_reference() {
    let corpus = corpus();
    let reference = corpus
        .enums
        .iter()
        .map(|entry| (entry.name.as_str(), &entry.values))
        .collect::<BTreeMap<_, _>>();
    let backlog = ledger(UNMODELED_ENUMS);

    // The vocabularies this port already spells. Everything else is in the
    // backlog above until the story that models it lands.
    let declared: [(&str, Vec<String>); 19] = [
        (
            "AccountActionKind",
            wire_values(&crate::vocabulary::AccountActionKind::ALL),
        ),
        (
            "AccountPlanKind",
            wire_values(&crate::vocabulary::AccountPlanKind::ALL),
        ),
        (
            "AccountStatus",
            wire_values(&crate::vocabulary::AccountStatus::ALL),
        ),
        (
            "AgentSafety",
            wire_values(&crate::vocabulary::AgentSafety::ALL),
        ),
        (
            "AgentType",
            wire_values(&[
                vibe_core::extensions::AgentKind::Agent,
                vibe_core::extensions::AgentKind::Subagent,
            ]),
        ),
        (
            "ApprovalDecisionType",
            wire_values(&vibe_core::events::ApprovalDecisionType::ALL),
        ),
        (
            "ConfigFieldKind",
            // The settings surface spells this vocabulary as an editor control
            // rather than as a serialized value, so its wire spelling is the
            // one the field descriptions carry.
            vibe_core::config::registry::FieldKind::ALL
                .into_iter()
                .map(|kind| kind.as_str().to_owned())
                .collect(),
        ),
        (
            "MCPSourceKind",
            wire_values(&crate::vocabulary::McpSourceKind::ALL),
        ),
        (
            "MCPSourceStatus",
            wire_values(&crate::vocabulary::McpSourceStatus::ALL),
        ),
        (
            "HookScope",
            wire_values(&[
                vibe_core::events::HookScope::PostAgent,
                vibe_core::events::HookScope::PreTool,
                vibe_core::events::HookScope::PostTool,
            ]),
        ),
        (
            "HookSeverity",
            wire_values(&[
                vibe_core::events::HookSeverity::Ok,
                vibe_core::events::HookSeverity::Warning,
                vibe_core::events::HookSeverity::Error,
            ]),
        ),
        (
            "TodoEffectPriority",
            wire_values(&[
                vibe_core::events::TodoEffectPriority::Low,
                vibe_core::events::TodoEffectPriority::Medium,
                vibe_core::events::TodoEffectPriority::High,
            ]),
        ),
        (
            "TodoEffectStatus",
            wire_values(&[
                vibe_core::events::TodoEffectStatus::Pending,
                vibe_core::events::TodoEffectStatus::InProgress,
                vibe_core::events::TodoEffectStatus::Completed,
                vibe_core::events::TodoEffectStatus::Cancelled,
            ]),
        ),
        (
            "ToolEffectKind",
            wire_values(&vibe_core::events::ToolEffectKind::ALL),
        ),
        (
            // US-105: the four scopes an approval is granted under. The
            // comparison is what refuses a fifth value, which the Python client
            // could not read, and a missing one, which would leave a
            // requirement unnameable.
            "PermissionScope",
            wire_values(&vibe_core::policy::PermissionScope::ALL),
        ),
        (
            "PublicEntryGenerationStatus",
            wire_values(&[
                vibe_core::events::PublicEntryGenerationStatus::InProgress,
                vibe_core::events::PublicEntryGenerationStatus::Completed,
            ]),
        ),
        (
            "PublicTurnStatus",
            wire_values(&[
                vibe_core::events::PublicTurnStatus::InProgress,
                vibe_core::events::PublicTurnStatus::Completed,
                vibe_core::events::PublicTurnStatus::Failed,
                vibe_core::events::PublicTurnStatus::Interrupted,
            ]),
        ),
        (
            "PublicTurnStopReason",
            wire_values(&[vibe_core::events::PublicTurnStopReason::Limit]),
        ),
        (
            "TurnErrorCode",
            wire_values(&[
                vibe_core::events::TurnErrorCode::RateLimit,
                vibe_core::events::TurnErrorCode::ContextTooLong,
                vibe_core::events::TurnErrorCode::ResponseTooLong,
                vibe_core::events::TurnErrorCode::Refusal,
                vibe_core::events::TurnErrorCode::InvalidImageAttachment,
                vibe_core::events::TurnErrorCode::ImagesNotSupported,
                vibe_core::events::TurnErrorCode::CompactionFailed,
                vibe_core::events::TurnErrorCode::BackendError,
                vibe_core::events::TurnErrorCode::InternalError,
            ]),
        ),
    ];
    for (name, values) in &declared {
        let expected = reference
            .get(name)
            .unwrap_or_else(|| unreachable!("{name} is not in the corpus"));
        assert_eq!(&values, expected, "the {name} vocabulary diverged");
        assert!(
            !backlog.contains_key(*name),
            "{name} is modeled now and its backlog entry is stale"
        );
    }

    let unmeasured = reference
        .keys()
        .filter(|name| {
            // `ProtocolErrorCode` is the one vocabulary compared by its own
            // test above, because it is the error contract rather than a
            // projection value set.
            **name != "ProtocolErrorCode"
                && !backlog.contains_key(**name)
                && !declared.iter().any(|(known, _)| known == *name)
        })
        .collect::<Vec<_>>();
    assert!(
        unmeasured.is_empty(),
        "these reference vocabularies are neither compared nor recorded: {unmeasured:?}"
    );
    assert_eq!(
        corpus.absent_enums,
        ["PublicRetryCategory"],
        "the pinned reference declares a vocabulary this corpus recorded as absent"
    );
    eprintln!(
        "app-server surface: enums {}/{} compared, {} awaiting a story",
        declared.len(),
        reference.len(),
        backlog.len()
    );
}

// --------------------------------------------------------------------------
// The model census
// --------------------------------------------------------------------------

/// The census as a lookup, with the unions resolved for discriminated fields.
struct Census<'a> {
    models: BTreeMap<&'a str, &'a ModelEntry>,
    unions: &'a [UnionEntry],
}

impl<'a> Census<'a> {
    fn new(corpus: &'a Corpus) -> Self {
        Self {
            models: corpus
                .models
                .iter()
                .map(|model| (model.name.as_str(), model))
                .collect(),
            unions: &corpus.unions,
        }
    }

    /// Reports every divergence between `value` and the model named `model`, as
    /// a JSON pointer plus what is wrong at it.
    fn validate(&self, pointer: &str, model: &str, value: &Value, issues: &mut Vec<String>) {
        let Some(entry) = self.models.get(model) else {
            issues.push(format!("{pointer}: no corpus entry for model {model}"));
            return;
        };
        let Some(object) = value.as_object() else {
            issues.push(format!("{pointer}: expected an object for {model}"));
            return;
        };
        let fields = entry
            .fields
            .iter()
            .map(|field| (field.alias.as_str(), field))
            .collect::<BTreeMap<_, _>>();
        for field in &entry.fields {
            if field.required && !object.contains_key(&field.alias) {
                issues.push(format!(
                    "{pointer}/{}: {model} requires this field and it is absent",
                    field.alias
                ));
            }
        }
        for (key, child) in object {
            let child_pointer = format!("{pointer}/{key}");
            let Some(field) = fields.get(key.as_str()) else {
                issues.push(format!(
                    "{child_pointer}: {model} does not declare this field"
                ));
                continue;
            };
            self.validate_field(&child_pointer, field, child, issues);
        }
    }

    fn validate_field(
        &self,
        pointer: &str,
        field: &FieldEntry,
        value: &Value,
        issues: &mut Vec<String>,
    ) {
        if value.is_null() {
            if !field.optional {
                issues.push(format!("{pointer}: this field is not nullable"));
            }
            return;
        }
        match field.kind.as_str() {
            "model" => self.validate_value(pointer, &field.models, value, issues),
            "list" => {
                let Some(items) = value.as_array() else {
                    issues.push(format!("{pointer}: expected an array"));
                    return;
                };
                if field.models.is_empty() {
                    return;
                }
                for (index, item) in items.iter().enumerate() {
                    self.validate_value(&format!("{pointer}/{index}"), &field.models, item, issues);
                }
            }
            _ => {}
        }
    }

    /// Validates against one model, or against the union variant the value's
    /// discriminator selects.
    fn validate_value(
        &self,
        pointer: &str,
        models: &[String],
        value: &Value,
        issues: &mut Vec<String>,
    ) {
        match models {
            [] => {}
            [single] => self.validate(pointer, single, value, issues),
            _ => {
                let members = models.iter().cloned().collect::<BTreeSet<_>>();
                let Some(union) = self.unions.iter().find(|union| {
                    union
                        .variants
                        .iter()
                        .map(|variant| variant.model.clone())
                        .collect::<BTreeSet<_>>()
                        == members
                }) else {
                    // A union the corpus does not name cannot be resolved to a
                    // variant, so the subtree stays unvalidated rather than
                    // being reported as a divergence it is not.
                    return;
                };
                let Some(discriminator) = &union.discriminator else {
                    return;
                };
                let Some(tag) = value.get(discriminator) else {
                    issues.push(format!(
                        "{pointer}/{discriminator}: {} requires its discriminator",
                        union.name
                    ));
                    return;
                };
                // A variant whose discriminator is a literal set rather than a
                // single value carries no captured value, so it is resolved
                // through the literals its own model declares.
                let Some(variant) = union
                    .variants
                    .iter()
                    .find(|variant| variant.value.as_ref() == Some(tag))
                    .or_else(|| {
                        union.variants.iter().find(|variant| {
                            self.models
                                .get(variant.model.as_str())
                                .is_some_and(|model| {
                                    model.fields.iter().any(|field| {
                                        field.alias == *discriminator
                                            && field.literals.contains(tag)
                                    })
                                })
                        })
                    })
                else {
                    issues.push(format!(
                        "{pointer}/{discriminator}: {tag} is not a {} variant",
                        union.name
                    ));
                    return;
                };
                self.validate(pointer, &variant.model, value, issues);
            }
        }
    }
}

/// Every divergence between one method's answer and the model the corpus
/// declares for it.
///
/// The probe below reaches a method with whatever a bare server can answer,
/// which leaves the shapes that need a repository, a backend or a written
/// session measured by nothing. This is how a test that stands those up
/// validates against the same census rather than against its own expectations.
pub(crate) fn census_issues(method: &str, response: &Value) -> Vec<String> {
    let corpus = corpus();
    let model = corpus
        .methods
        .iter()
        .find(|entry| entry.name == method)
        .unwrap_or_else(|| unreachable!("{method} is not a corpus method"))
        .response
        .clone();
    let mut issues = Vec::new();
    Census::new(&corpus).validate("", &model, response, &mut issues);
    issues
}

#[test]
fn every_model_the_census_references_has_an_entry() {
    let corpus = corpus();
    let census = Census::new(&corpus);
    let mut missing = BTreeSet::new();
    for model in &corpus.models {
        for field in &model.fields {
            for referenced in &field.models {
                if !census.models.contains_key(referenced.as_str()) {
                    missing.insert(format!("{}/{}: {referenced}", model.name, field.alias));
                }
            }
        }
    }
    for entry in corpus
        .methods
        .iter()
        .chain(&corpus.client_tool_methods)
        .chain(&corpus.server_requests)
    {
        for referenced in [&entry.params, &entry.response] {
            if !census.models.contains_key(referenced.as_str()) {
                missing.insert(format!("{}: {referenced}", entry.name));
            }
        }
    }
    for entry in &corpus.notifications {
        if !census.models.contains_key(entry.params.as_str()) {
            missing.insert(format!("{}: {}", entry.name, entry.params));
        }
    }
    for union in &corpus.unions {
        for variant in &union.variants {
            if !census.models.contains_key(variant.model.as_str()) {
                missing.insert(format!("{}: {}", union.name, variant.model));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "the census references models it does not carry: {missing:?}"
    );
    eprintln!(
        "app-server surface: models {} in the census, {} client-tool methods, {} server requests",
        corpus.models.len(),
        corpus.client_tool_methods.len(),
        corpus.server_requests.len()
    );
}

#[test]
fn every_probed_response_validates_against_the_census() {
    let corpus = corpus();
    let census = Census::new(&corpus);
    let responses = corpus
        .methods
        .iter()
        .map(|entry| (entry.name.as_str(), entry.response.as_str()))
        .collect::<BTreeMap<_, _>>();
    let backlog = ledger(DIVERGENT_RESPONSES);

    let (server, mut connection) = probe_connection();
    let mut probed = Vec::new();
    let mut unreachable = Vec::new();
    let mut conforming = Vec::new();
    let mut divergent = BTreeMap::new();
    for (method, params) in probe_requests() {
        let Some(result) = probe(&server, &mut connection, method, &params) else {
            unreachable.push(method);
            continue;
        };
        probed.push(method);
        let model = responses
            .get(method)
            .unwrap_or_else(|| unreachable!("{method} is not a corpus method"));
        let mut issues = Vec::new();
        census.validate("", model, &result, &mut issues);
        if issues.is_empty() {
            conforming.push(method);
        } else {
            divergent.insert(method, issues);
        }
    }

    assert!(
        probed.len() >= 12,
        "the probe reached only {} methods, which is too few to measure anything: {probed:?}",
        probed.len()
    );
    // A method the probe cannot reach is measured by nothing, so it is recorded
    // rather than dropped: the conforming count above only speaks for what it
    // actually validated.
    let unreachable_backlog = ledger(UNREACHABLE_PROBES);
    let unrecorded_unreachable = unreachable
        .iter()
        .filter(|method| !unreachable_backlog.contains_key(**method))
        .collect::<Vec<_>>();
    assert!(
        unrecorded_unreachable.is_empty(),
        "these probed methods answered with no result and are unrecorded: {unrecorded_unreachable:?}"
    );
    let stale_unreachable = unreachable_backlog
        .keys()
        .filter(|method| !unreachable.contains(&method.as_str()))
        .collect::<Vec<_>>();
    assert!(
        stale_unreachable.is_empty(),
        "these methods answer now and their unreachable entry is stale: {stale_unreachable:?}"
    );
    let unrecorded = divergent
        .iter()
        .filter(|(method, _)| !backlog.contains_key(**method))
        .map(|(method, issues)| format!("{method}: {}", issues.join("; ")))
        .collect::<Vec<_>>();
    assert!(
        unrecorded.is_empty(),
        "these responses diverge from the census and are unrecorded:\n{}",
        unrecorded.join("\n")
    );
    let stale = backlog
        .keys()
        .filter(|method| !divergent.contains_key(method.as_str()))
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "these entries no longer name a probed divergence and are stale: {stale:?}"
    );
    eprintln!(
        "app-server surface: responses {}/{} probed methods validate, {} awaiting a story {:?}, \
         {} unreachable {unreachable:?}",
        conforming.len(),
        probed.len(),
        divergent.len(),
        divergent.keys().collect::<Vec<_>>(),
        unreachable.len(),
    );
}

#[test]
fn a_surplus_or_missing_alias_is_reported_with_its_pointer() {
    let corpus = corpus();
    let census = Census::new(&corpus);

    let mut issues = Vec::new();
    census.validate(
        "",
        "ConnectorsReadResponse",
        &json!({"counts": {"total": 0, "connected": 0}, "connectors": []}),
        &mut issues,
    );
    assert_eq!(
        issues,
        ["/connectors: ConnectorsReadResponse does not declare this field"]
    );

    let mut issues = Vec::new();
    census.validate(
        "",
        "AgentsListResponse",
        &json!({"agents": []}),
        &mut issues,
    );
    assert_eq!(
        issues,
        ["/active: AgentsListResponse requires this field and it is absent"]
    );

    // A nested divergence is reported at the pointer that reaches it, not at
    // the top of the response.
    let mut issues = Vec::new();
    census.validate(
        "",
        "ConfigProxyReadResponse",
        &json!({"settings": {"values": {}, "descriptions": {}, "invented": true}}),
        &mut issues,
    );
    assert_eq!(
        issues,
        ["/settings/invented: ProxySettingsView does not declare this field"]
    );
}

// --------------------------------------------------------------------------
// The projection unions
// --------------------------------------------------------------------------

/// One tool per `ToolEffectKind`, with arguments and a typed result shaped the
/// way the tool actually answers. The replay drives the projection with them,
/// so the entry it validates is the one a client receives.
const EFFECT_PROBES: &[(&str, &str, &str)] = &[
    (
        "mcp_fixture_echo",
        r#"{"text":"hello"}"#,
        r#"{"echo":"hello"}"#,
    ),
    (
        "bash",
        r#"{"command":"cargo test"}"#,
        r#"{"stdout":"ok","stderr":""}"#,
    ),
    (
        "edit",
        r#"{"file_path":"/w/a.rs","old_string":"a","new_string":"b"}"#,
        r#"{"file":"/w/a.rs","old_string":"a","new_string":"b","occurrences":[]}"#,
    ),
    (
        "grep",
        r#"{"pattern":"fn","path":"src"}"#,
        r#"{"matches":"src/a.rs:1","match_count":1,"was_truncated":false}"#,
    ),
    (
        "read_file",
        r#"{"file_path":"/w/a.rs","offset":1,"limit":10}"#,
        r#"{"file_path":"/w/a.rs","content":"x","num_lines":1,"start_line":1}"#,
    ),
    (
        "todo",
        r#"{"action":"write","todos":[{"id":"a","content":"ship","status":"in_progress","priority":"high","surplus":true}]}"#,
        r#"{"todos":[]}"#,
    ),
    (
        "write_file",
        r#"{"file_path":"/w/a.rs","content":"x"}"#,
        r#"{"file_path":"/w/a.rs","content":"x"}"#,
    ),
    (
        "ask_user_question",
        r#"{"questions":[{"question":"Ship?","header":"Release","options":[{"label":"Yes"}],"multiSelect":false,"hideOther":false}]}"#,
        r#"{"answers":[{"question":"Ship?","answer":"Yes"}],"cancelled":false}"#,
    ),
    (
        "web_search",
        r#"{"query":"rust"}"#,
        r#"{"query":"rust","answer":"a","sources":[]}"#,
    ),
    (
        "web_fetch",
        r#"{"url":"https://example.com/a"}"#,
        r#"{"url":"https://example.com/a","content":"x","content_type":"text/html"}"#,
    ),
    (
        "skill",
        r#"{"name":"probe"}"#,
        r#"{"name":"probe","content":"x"}"#,
    ),
    (
        "task",
        r#"{"task":"audit","agent":"explore"}"#,
        r#"{"parentSessionId":"session-1","childSessionId":"session-child","publicSessionId":"session-child","status":"completed","result":"done"}"#,
    ),
];

/// The history entries one turn's worth of engine events projects.
fn projected_history(events: &[vibe_core::events::EngineEvent]) -> Vec<Value> {
    use vibe_core::events::{EventEnvelope, ProjectionReducer};

    let mut reducer = ProjectionReducer::for_turn(PROBE_SESSION, "turn-1");
    for (index, event) in events.iter().enumerate() {
        let envelope = EventEnvelope {
            session_id: PROBE_SESSION.to_owned(),
            turn_id: Some("turn-1".to_owned()),
            emitted_at: 1_000 + index as u64,
            event_id: index as u64 + 1,
            event: event.clone(),
        };
        reducer
            .apply(&envelope)
            .unwrap_or_else(|error| unreachable!("the projection accepts {event:?}: {error}"));
    }
    reducer
        .into_state()
        .history
        .iter()
        .map(|entry| serde_json::to_value(entry).unwrap_or(Value::Null))
        .collect()
}

/// Every `ToolEffectKind` is published with a detail that validates, which is
/// what proves the union is served rather than merely declared.
#[test]
fn every_effect_kind_publishes_an_entry_that_validates_against_the_census() {
    use vibe_core::events::{EngineEvent, ToolEffectKind};

    let corpus = corpus();
    let census = Census::new(&corpus);
    let mut published = BTreeSet::new();
    let mut issues = Vec::new();
    for (tool, arguments, typed_result) in EFFECT_PROBES {
        let history = projected_history(&[
            EngineEvent::UserMessage {
                content: "go".to_owned(),
            },
            EngineEvent::ToolCall {
                call_id: "call-1".to_owned(),
                name: (*tool).to_owned(),
                arguments: (*arguments).to_owned(),
            },
            EngineEvent::ToolResult {
                call_id: "call-1".to_owned(),
                content: "done".to_owned(),
                typed_result: serde_json::from_str(typed_result).unwrap_or(Value::Null),
                display: Value::Null,
                duration_ms: 1,
                is_error: false,
                cancelled: false,
            },
        ]);
        let entry = history
            .iter()
            .find(|entry| entry["type"] == "effect")
            .unwrap_or_else(|| unreachable!("{tool} projects an effect entry"));
        census.validate("", "PublicEffectEntry", entry, &mut issues);
        let kind = entry["detail"]["kind"]
            .as_str()
            .unwrap_or_else(|| unreachable!("{tool} publishes a detail kind"));
        published.insert(kind.to_owned());
        assert!(
            entry["detail"].get("toolCallId").is_none()
                && entry["detail"].get("arguments").is_none(),
            "{tool} still publishes the port's own detail keys: {}",
            entry["detail"]
        );
        // The subagent variant is the only one declaring a child session, and
        // the delegation result is where the projection reads it.
        if kind == ToolEffectKind::Subagent.label() {
            assert_eq!(
                entry["detail"]["childSessionId"], "session-child",
                "{tool} published no child session: {}",
                entry["detail"]
            );
        }
    }
    assert!(
        issues.is_empty(),
        "published effect entries diverge from the census: {issues:?}"
    );
    let expected = ToolEffectKind::ALL
        .into_iter()
        .map(|kind| kind.label().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(published, expected, "not every effect kind was published");
    eprintln!(
        "app-server surface: effect details {}/{} kinds validate",
        published.len(),
        expected.len()
    );
}

/// A settled effect carries the result display its state declares, and only a
/// cancellation with no result may publish none.
#[test]
fn every_settled_effect_state_carries_the_display_its_variant_declares() {
    use vibe_core::events::{EngineEvent, ToolEffectKind};

    let corpus = corpus();
    let census = Census::new(&corpus);
    let call = EngineEvent::ToolCall {
        call_id: "call-1".to_owned(),
        name: "bash".to_owned(),
        arguments: r#"{"command":"cargo test"}"#.to_owned(),
    };
    let result = |typed_result: Value, is_error: bool, cancelled: bool| EngineEvent::ToolResult {
        call_id: "call-1".to_owned(),
        content: "done".to_owned(),
        typed_result,
        display: Value::Null,
        duration_ms: 1,
        is_error,
        cancelled,
    };
    let start = EngineEvent::UserMessage {
        content: "go".to_owned(),
    };
    let mut issues = Vec::new();
    for (label, event, expects_display) in [
        (
            "completed",
            result(json!({"stdout": "ok"}), false, false),
            true,
        ),
        ("failed", result(Value::Null, true, false), true),
        (
            "cancelled with a result",
            result(json!({"stdout": ""}), false, true),
            true,
        ),
        (
            "cancelled with none",
            result(Value::Null, false, true),
            false,
        ),
    ] {
        let history = projected_history(&[start.clone(), call.clone(), event]);
        let entry = history
            .iter()
            .find(|entry| entry["type"] == "effect")
            .unwrap_or_else(|| unreachable!("{label} projects an effect entry"));
        census.validate("", "PublicEffectEntry", entry, &mut issues);
        let display = &entry["state"]["display"];
        assert_eq!(
            !display.is_null(),
            expects_display,
            "{label} published the wrong display: {display}"
        );
        if expects_display {
            let mut display_issues = Vec::new();
            census.validate("", "EffectResultDisplay", display, &mut display_issues);
            assert!(
                display_issues.is_empty(),
                "{label} published a divergent result display: {display_issues:?}"
            );
        }
    }
    assert!(
        issues.is_empty(),
        "settled effect entries diverge from the census: {issues:?}"
    );
    // The generic kind is the one a tool with no variant lands on, so the
    // display contract above holds for every kind that shares its state union.
    assert_eq!(
        ToolEffectKind::from_tool_name("bash"),
        ToolEffectKind::Shell
    );
}

/// The eight notice variants, each in the form the projection publishes.
fn notice_probes() -> Vec<(&'static str, vibe_core::events::NoticeDetail)> {
    use vibe_core::events::{HookNotice, HookScope, HookSeverity, NoticeDetail};

    let hook = || HookNotice {
        scope: HookScope::PreTool,
        tool_name: Some("bash".to_owned()),
        tool_call_id: Some("call-1".to_owned()),
        hook_name: Some("guard".to_owned()),
        status: Some(HookSeverity::Ok),
        content: Some("checked".to_owned()),
    };
    vec![
        ("hook_run_started", NoticeDetail::HookRunStarted(hook())),
        ("hook_run_completed", NoticeDetail::HookRunCompleted(hook())),
        ("hook_started", NoticeDetail::HookStarted(hook())),
        ("hook_completed", NoticeDetail::HookCompleted(hook())),
        (
            "agent_changed",
            NoticeDetail::AgentChanged {
                agent_name: "explore".to_owned(),
            },
        ),
        (
            "context_cleared",
            NoticeDetail::ContextCleared {
                plan_file_path: Some("/w/plan.md".to_owned()),
            },
        ),
        (
            "session_title_updated",
            NoticeDetail::SessionTitleUpdated {
                title: "Audit".to_owned(),
            },
        ),
        (
            "plan_review_started",
            NoticeDetail::PlanReviewStarted {
                file_path: "/w/plan.md".to_owned(),
            },
        ),
        ("plan_review_ended", NoticeDetail::PlanReviewEnded),
        (
            "waiting_for_input",
            NoticeDetail::WaitingForInput {
                task_id: "task-1".to_owned(),
                label: Some("Waiting".to_owned()),
                predefined_answers: Some(vec!["Yes".to_owned()]),
            },
        ),
        (
            "scheduled_loop_fired",
            NoticeDetail::ScheduledLoopFired {
                loop_id: "loop-1".to_owned(),
            },
        ),
    ]
}

#[test]
fn every_notice_variant_validates_against_the_census() {
    let corpus = corpus();
    let census = Census::new(&corpus);
    let union = corpus
        .unions
        .iter()
        .find(|union| union.name == "NoticeDetail")
        .unwrap_or_else(|| unreachable!("the corpus carries the notice union"));
    let mut issues = Vec::new();
    let mut covered = BTreeSet::new();
    for (kind, detail) in notice_probes() {
        let entry = json!({
            "type": "notice",
            "id": "notice-1",
            "sessionId": PROBE_SESSION,
            "turnId": "turn-1",
            "createdAt": 1,
            "updatedAt": 1,
            "generationStatus": "completed",
            "level": "info",
            "message": "a notice",
            "detail": detail,
        });
        assert_eq!(entry["detail"]["kind"], kind);
        census.validate("", "PublicNoticeEntry", &entry, &mut issues);
        let variant = union
            .variants
            .iter()
            .find(|variant| variant.value.as_ref().and_then(Value::as_str) == Some(kind))
            // The four hook kinds share one variant, whose discriminator is the
            // literal set rather than a single value.
            .map_or("HookNoticeDetail", |variant| variant.model.as_str());
        covered.insert(variant.to_owned());
    }
    assert!(
        issues.is_empty(),
        "published notice entries diverge from the census: {issues:?}"
    );
    let expected = union
        .variants
        .iter()
        .map(|variant| variant.model.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(covered, expected, "a notice variant was never published");
    eprintln!(
        "app-server surface: notice details {}/{} variants validate",
        covered.len(),
        expected.len()
    );
}

/// The callback detail, output and state unions, in both of their forms.
#[test]
fn every_callback_union_form_validates_against_the_census() {
    use vibe_core::events::{
        ApprovalDecision, ApprovalDecisionType, CallbackDetail, CallbackOutput, EffectDetail,
        QuestionChoice, UserAnswer, UserQuestion, UserQuestionRequest, UserQuestionResult,
    };

    let corpus = corpus();
    let census = Census::new(&corpus);
    let request = UserQuestionRequest {
        questions: vec![UserQuestion {
            question: "Ship?".to_owned(),
            header: "Release".to_owned(),
            options: vec![QuestionChoice {
                label: "Yes".to_owned(),
                description: "Ship it".to_owned(),
            }],
            multi_select: false,
            hide_other: false,
        }],
        footer_note: None,
    };
    let details = [
        (
            "approval",
            CallbackDetail::Approval {
                effect: Box::new(EffectDetail::for_encoded_call(
                    "bash",
                    r#"{"command":"cargo test"}"#,
                )),
                required_permissions: vec![vibe_core::policy::PermissionRequirement::command(
                    "cargo test",
                )],
                choices: ApprovalDecisionType::ALL.to_vec(),
                related_entry_id: Some("entry-1".to_owned()),
            },
        ),
        (
            "user_input",
            CallbackDetail::UserInput {
                request: request.clone(),
                related_entry_id: None,
            },
        ),
    ];
    let states = [
        ("open", json!({"status": "open"})),
        (
            "answered with an approval",
            json!({
                "status": "answered",
                "output": CallbackOutput::Approval {
                    decision: ApprovalDecision { decision: ApprovalDecisionType::Approve },
                    feedback: None,
                },
            }),
        ),
        (
            "answered with an input",
            json!({
                "status": "answered",
                "output": CallbackOutput::UserInput {
                    result: UserQuestionResult {
                        answers: vec![UserAnswer {
                            question: "Ship?".to_owned(),
                            answer: "Yes".to_owned(),
                            is_other: false,
                        }],
                        cancelled: false,
                    },
                },
            }),
        ),
        (
            "cancelled",
            json!({"status": "cancelled", "reason": "interrupted"}),
        ),
        (
            "expired",
            json!({"status": "expired", "reason": "timed out"}),
        ),
    ];

    let mut issues = Vec::new();
    for (detail_label, detail) in &details {
        for (state_label, state) in &states {
            let entry = json!({
                "type": "callback",
                "id": "callback-entry",
                "sessionId": PROBE_SESSION,
                "turnId": "turn-1",
                "createdAt": 1,
                "updatedAt": 1,
                "generationStatus": "in_progress",
                "callbackId": "callback-1",
                "title": "Approve?",
                "detail": detail,
                "state": state,
            });
            let mut form = Vec::new();
            census.validate("", "PublicCallbackEntry", &entry, &mut form);
            if !form.is_empty() {
                issues.push(format!(
                    "{detail_label} / {state_label}: {}",
                    form.join("; ")
                ));
            }
        }
    }
    assert!(
        issues.is_empty(),
        "published callback entries diverge from the census:\n{}",
        issues.join("\n")
    );
    eprintln!(
        "app-server surface: callback unions {} details x {} states validate",
        details.len(),
        states.len()
    );
}

/// A client that did not declare a callback kind is never asked one, which is
/// what keeps the server from raising a question the client cannot close.
#[test]
fn a_callback_kind_the_client_did_not_declare_is_refused() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    let batch = connection.dispatch(&frame(
        1,
        "initialize",
        &json!({
            "clientInfo": {"name": "surface-parity", "version": "1"},
            "capabilities": {
                "callbackKinds": ["approval"],
                "clientTools": [],
                "disabledNotifications": []
            }
        }),
    ));
    assert!(matches!(
        decode_frame(&batch.outbound[0]).expect("handshake answer"),
        Envelope::Success(_)
    ));
    connection.dispatch(
        &serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }))
        .expect("initialized frame"),
    );
    connection.dispatch(&frame(
        2,
        "session/start",
        &json!({"sessionId": PROBE_SESSION, "workingDirectory": "/workspace"}),
    ));

    let refused = connection.request_callback(
        PROBE_SESSION,
        "turn-1",
        vibe_core::events::CallbackKind::UserInput,
        "Ship?",
    );
    assert!(
        matches!(
            refused,
            Err(crate::server::ServerError::UnsupportedClientCallbackKind(_))
        ),
        "a user-input callback reached a client that declared only approvals: {refused:?}"
    );
}

// --------------------------------------------------------------------------
// Client tools
// --------------------------------------------------------------------------

/// One request per `ClientToolRequest` variant, which is the whole set this
/// port can issue: `method` is exhaustive over the enum, so a variant added
/// without a line here leaves the name comparison below short and a variant
/// removed stops this from compiling.
fn issued_client_tool_requests() -> Vec<ClientToolRequest> {
    let session_id = PROBE_SESSION.to_owned();
    let terminal_id = "terminal-1".to_owned();
    vec![
        ClientToolRequest::ReadTextFile {
            session_id: session_id.clone(),
            path: "src/main.rs".to_owned(),
            line: Some(12),
            limit: Some(200),
        },
        ClientToolRequest::WriteTextFile {
            session_id: session_id.clone(),
            path: "src/main.rs".to_owned(),
            content: "fn main() {}\n".to_owned(),
        },
        ClientToolRequest::TerminalCreate {
            session_id: session_id.clone(),
            command: "cargo test".to_owned(),
            args: None,
            env: None,
            cwd: "/workspace".to_owned(),
            output_byte_limit: 1_048_576,
            tool_call_id: Some("call-1".to_owned()),
        },
        ClientToolRequest::TerminalWait {
            session_id: session_id.clone(),
            terminal_id: terminal_id.clone(),
        },
        ClientToolRequest::TerminalOutput {
            session_id: session_id.clone(),
            terminal_id: terminal_id.clone(),
        },
        ClientToolRequest::TerminalKill {
            session_id: session_id.clone(),
            terminal_id: terminal_id.clone(),
        },
        ClientToolRequest::TerminalRelease {
            session_id,
            terminal_id,
        },
    ]
}

/// The client-tool methods travel the other way, so no probe can answer them.
/// What is measurable is the request this port puts on the wire and the answer
/// shape it reads back, both validated against the same census as every
/// client-to-server body.
#[test]
fn every_issued_client_tool_request_validates_against_the_census() {
    let corpus = corpus();
    let census = Census::new(&corpus);
    let issued = issued_client_tool_requests();

    let declared = corpus
        .client_tool_methods
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<BTreeSet<_>>();
    let issues_names = issued
        .iter()
        .map(ClientToolRequest::method)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        issues_names, declared,
        "the issued client-tool methods are not the reference set"
    );

    let params_models = corpus
        .client_tool_methods
        .iter()
        .map(|entry| (entry.name.as_str(), entry.params.as_str()))
        .collect::<BTreeMap<_, _>>();
    let mut issues = Vec::new();
    for request in &issued {
        let method = request.method();
        let envelope = serde_json::to_value(request).expect("a request serializes");
        let params = envelope.get("params").cloned().unwrap_or_else(|| json!({}));
        let model = params_models
            .get(method)
            .unwrap_or_else(|| unreachable!("{method} is not a corpus client-tool method"));
        let mut form = Vec::new();
        census.validate("", model, &params, &mut form);
        if !form.is_empty() {
            issues.push(format!("{method}: {}", form.join("; ")));
        }
    }

    // The answers the delegation reads: a field this port looks for that the
    // reference does not declare fails here rather than at an editor.
    for (model, answer) in [
        (
            "ClientToolReadTextFileResponse",
            json!({"content": "fn main() {}\n"}),
        ),
        (
            "ClientToolTerminalCreateResponse",
            json!({"terminalId": "terminal-1"}),
        ),
        (
            "ClientToolTerminalWaitResponse",
            json!({"exitCode": 0, "signal": null}),
        ),
        (
            "ClientToolTerminalOutputResponse",
            json!({"output": "ok", "truncated": false}),
        ),
        ("EmptyResponse", json!({})),
    ] {
        let mut form = Vec::new();
        census.validate("", model, &answer, &mut form);
        if !form.is_empty() {
            issues.push(format!("{model}: {}", form.join("; ")));
        }
    }

    assert!(
        issues.is_empty(),
        "issued client-tool bodies diverge from the census:\n{}",
        issues.join("\n")
    );
    eprintln!(
        "app-server surface: client-tool methods {}/{} issued",
        issued.len(),
        corpus.client_tool_methods.len()
    );
}

// --------------------------------------------------------------------------
// Probing a real connection
// --------------------------------------------------------------------------

fn frame(id: i64, method: &str, params: &Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
    .expect("request frame")
}

fn handshake_response(connection: &mut ServerConnection) -> Value {
    let batch = connection.dispatch(&frame(
        1,
        "initialize",
        &json!({
            "clientInfo": {"name": "surface-parity", "version": "1"},
            "capabilities": {
                "callbackKinds": ["approval", "user_input"],
                "clientTools": [],
                "disabledNotifications": []
            }
        }),
    ));
    let response = decode_frame(&batch.outbound[0]).expect("handshake answer");
    let Envelope::Success(success) = response else {
        unreachable!("the handshake was rejected: {response:?}");
    };
    Value::Object(success.result.into_iter().collect())
}

fn advertised_methods_from_handshake() -> Vec<String> {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    let response = handshake_response(&mut connection);
    response["capabilities"]["methods"]
        .as_array()
        .expect("the handshake advertises a method list")
        .iter()
        .filter_map(|method| method.as_str().map(ToOwned::to_owned))
        .collect()
}

/// A connection with one open session, ready to answer read-only requests,
/// beside the server that owns it.
///
/// The server is handed back because the integration reads answer from the
/// resource backend rather than inline: their frame is produced by the deferred
/// work the dispatch returns, and only the server can run it.
fn probe_connection() -> (AppServer, ServerConnection) {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    handshake_response(&mut connection);
    connection.dispatch(
        &serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }))
        .expect("initialized frame"),
    );
    let started = connection.dispatch(&frame(
        2,
        "session/start",
        &json!({"sessionId": PROBE_SESSION, "workingDirectory": "/workspace"}),
    ));
    assert!(
        matches!(
            decode_frame(&started.outbound[0]).expect("session answer"),
            Envelope::Success(_)
        ),
        "the probe session did not start"
    );
    (server, connection)
}

/// The result body of one request, or `None` when this build answers with an
/// error. An error is not a divergence: a method may need a backend the probe
/// does not stand up, which [`UNREACHABLE_PROBES`] records.
fn probe(
    server: &AppServer,
    connection: &mut ServerConnection,
    method: &str,
    params: &Value,
) -> Option<Value> {
    let batch = connection.dispatch(&frame(99, method, params));
    let batch = match batch.outbound.first() {
        Some(_) => batch,
        None => run_deferred(server, batch)?,
    };
    match decode_frame(batch.outbound.first()?).ok()? {
        Envelope::Success(success) => Some(Value::Object(success.result.into_iter().collect())),
        _ => None,
    }
}

/// Runs the work a dispatch deferred, so its answer is validated like an inline
/// one.
///
/// `mcp/read` and `connectors/read` are served by the asynchronous resource
/// backend, and the session-less project surface resolves a repository root off
/// the loop, so in both cases the frame exists only after the deferred work
/// runs. Without this those methods would leave the probe silently.
fn run_deferred(server: &AppServer, batch: DispatchBatch) -> Option<DispatchBatch> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    match batch.deferred.into_iter().next()? {
        DeferredWork::ResourceRequest {
            request_id,
            session_id,
            command,
        } => {
            Some(runtime.block_on(server.execute_resource_request(request_id, session_id, command)))
        }
        DeferredWork::CloudRequest {
            request_id,
            method,
            params,
        } => Some(runtime.block_on(server.execute_cloud_request(request_id, method, params))),
        _ => None,
    }
}

fn wire_values<T: serde::Serialize>(values: &[T]) -> Vec<String> {
    values
        .iter()
        .filter_map(|value| {
            serde_json::to_value(value)
                .ok()
                .and_then(|value| value.as_str().map(ToOwned::to_owned))
        })
        .collect()
}

// --------------------------------------------------------------------------
// The live probe
// --------------------------------------------------------------------------

/// The pinned checkout and an interpreter that can drive it, or `None` when the
/// live probe cannot run here. Every replay above still ran against the
/// committed corpus.
fn pinned_reference() -> Option<(PathBuf, PathBuf)> {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "app-server surface") {
        eprintln!("{reason}");
        return None;
    }
    let interpreter = pinned_interpreter(&root)?;
    Some((root, interpreter))
}

#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let Some((root, interpreter)) = pinned_reference() else {
        eprintln!("skipping the live app-server surface probe: no pinned checkout to capture from");
        return;
    };
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    let temporary = tempfile::tempdir().expect("temporary root");
    let captured = temporary.path().join("corpus.json");
    let output = Command::new(&interpreter)
        .arg(workspace.join(CAPTURE_SCRIPT))
        .arg("--reference")
        .arg(&root)
        .arg("--output")
        .arg(&captured)
        .output()
        .expect("the capture script runs");
    assert!(
        output.status.success(),
        "{CAPTURE_SCRIPT} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recaptured = fs::read_to_string(&captured).expect("captured corpus reads");
    let committed = fs::read_to_string(corpus_path()).expect("committed corpus reads");
    assert_eq!(
        recaptured.replace("\r\n", "\n"),
        committed.replace("\r\n", "\n"),
        "the committed corpus no longer matches the pinned reference; rerun {CAPTURE_SCRIPT}"
    );
}
