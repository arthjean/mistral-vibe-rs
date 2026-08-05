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

use vibe_protocol::{
    Envelope, LOCAL_EXTENSION_METHODS, SERVER_METHODS, TransportKind, decode_frame,
    is_server_method,
};

use crate::server::{AppServer, EMITTED_NOTIFICATIONS, ServerConnection, routed_methods};

/// The reference commit the corpus is captured from. A checkout at any other
/// revision is not an oracle for this corpus.
const REFERENCE_COMMIT: &str = "68ff32e6a92e80a874c8153312f0aa8ae4955477";
const REFERENCE_ROOT: &str = "/home/arthur/dev/mistral-vibe";
/// Lets a workstation holding the checkout elsewhere still run the live probe,
/// under the same name the parity scripts read.
const REFERENCE_VARIABLE: &str = "VIBE_REFERENCE";
/// An interpreter that can import the reference package.
const INTERPRETER_VARIABLE: &str = "VIBE_PARITY_PYTHON";
const CAPTURE_SCRIPT: &str = "scripts/parity/app_server_surface.py";
const CORPUS_RELATIVE: &str = "tests/app-server-surface/corpus.json";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 1;
/// The session the probe opens; every session-scoped method is asked about it.
const PROBE_SESSION: &str = "session-1";

/// Reference methods this build does not route yet, each with the story that
/// adds it. A method routed while listed here fails the replay as a stale entry.
const UNROUTED_METHODS: &[(&str, &str)] = &[
    ("projectLinks/create", "US-098"),
    ("projectLinks/inspectRoot", "US-097"),
    ("projectLinks/link", "US-098"),
    ("projectLinks/list", "US-097"),
    ("projectLinks/picker/load", "US-097"),
    ("projectLinks/picker/loadMore", "US-097"),
    ("projectLinks/resolveRoot", "US-097"),
    ("projectLinks/save", "US-098"),
    ("projectLinks/unlink", "US-098"),
    ("telemetry/record", "US-096"),
];

/// Reference notifications this build does not emit yet.
const UNEMITTED_NOTIFICATIONS: &[(&str, &str)] = &[
    ("error", "US-086"),
    ("mcp/authUrl", "US-086"),
    ("runtime/updated", "US-086"),
    ("session/contextCleared", "US-085"),
    ("session/snapshot", "US-083"),
    ("session/statsUpdated", "US-084"),
    ("session/updated", "US-083"),
    ("turn/retrying", "US-085"),
    ("warning", "US-086"),
];

/// Notification names this build emits that the reference does not declare.
/// They are retired by US-086, which moves their payloads onto the reference
/// names.
const LOCAL_NOTIFICATIONS: &[(&str, &str)] = &[
    ("connectors/updated", "US-086"),
    ("mcp/updated", "US-086"),
    ("shell/updated", "US-086"),
    ("workspace/trust/updated", "US-086"),
];

/// Enum vocabularies the reference declares that this port does not model yet.
const UNMODELLED_ENUMS: &[(&str, &str)] = &[
    ("AccountActionKind", "US-090"),
    ("AccountPlanKind", "US-090"),
    ("AccountStatus", "US-090"),
    ("AgentSafety", "US-093"),
    ("AgentType", "US-093"),
    ("ApprovalDecisionType", "US-089"),
    ("ConfigFieldKind", "US-091"),
    ("HookScope", "US-088"),
    ("HookSeverity", "US-088"),
    ("MCPSourceKind", "US-092"),
    ("MCPSourceStatus", "US-092"),
    ("TerminalEmulator", "US-081"),
    ("TodoEffectPriority", "US-087"),
    ("TodoEffectStatus", "US-087"),
    ("ToolEffectKind", "US-087"),
    ("TurnErrorCode", "US-085"),
];

/// Methods whose probed response does not validate against the census yet, each
/// with the story that fixes it. A method that starts validating while listed
/// here fails the replay as a stale entry.
const DIVERGENT_RESPONSES: &[(&str, &str)] = &[
    ("agents/list", "US-093"),
    ("config/read", "US-091"),
    ("runtime/read", "US-090"),
    ("session/list", "US-093"),
    ("skills/list", "US-093"),
    ("tools/list", "US-093"),
];

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
        ("history/list", json!({})),
        ("mcp/read", session.clone()),
        ("review/state", session.clone()),
        ("runtime/read", session.clone()),
        ("session/list", json!({})),
        ("session/read", session.clone()),
        ("session/ready/read", session.clone()),
        ("skills/list", session.clone()),
        ("stats/read", session.clone()),
        ("tools/list", session.clone()),
        ("workspace/trust/status", json!({"cwd": "/workspace"})),
    ]
}

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
    #[expect(
        dead_code,
        reason = "literals are compared through the union discriminators"
    )]
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
    let mut connection = probe_connection();
    let response = probe(
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
    let backlog = ledger(UNMODELLED_ENUMS);

    // The vocabularies this port already spells. Everything else is in the
    // backlog above until the story that models it lands.
    let declared: [(&str, Vec<String>); 3] = [
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
    ];
    for (name, values) in &declared {
        let expected = reference
            .get(name)
            .unwrap_or_else(|| unreachable!("{name} is not in the corpus"));
        assert_eq!(&values, expected, "the {name} vocabulary diverged");
        assert!(
            !backlog.contains_key(*name),
            "{name} is modelled now and its backlog entry is stale"
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
                let Some(variant) = union
                    .variants
                    .iter()
                    .find(|variant| variant.value.as_ref() == Some(tag))
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

    let mut connection = probe_connection();
    let mut probed = Vec::new();
    let mut conforming = Vec::new();
    let mut divergent = BTreeMap::new();
    for (method, params) in probe_requests() {
        let Some(result) = probe(&mut connection, method, &params) else {
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
        "app-server surface: responses {}/{} probed methods validate, {} awaiting a story {:?}",
        conforming.len(),
        probed.len(),
        divergent.len(),
        divergent.keys().collect::<Vec<_>>()
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

/// A connection with one open session, ready to answer read-only requests.
fn probe_connection() -> ServerConnection {
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
    connection
}

/// The result body of one request, or `None` when this build answers with an
/// error. An error is not a divergence: a method may need a backend the probe
/// does not stand up.
fn probe(connection: &mut ServerConnection, method: &str, params: &Value) -> Option<Value> {
    let batch = connection.dispatch(&frame(99, method, params));
    let frame = batch.outbound.first()?;
    match decode_frame(frame).ok()? {
        Envelope::Success(success) => Some(Value::Object(success.result.into_iter().collect())),
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
    let root = std::env::var_os(REFERENCE_VARIABLE)
        .map_or_else(|| PathBuf::from(REFERENCE_ROOT), PathBuf::from);
    if !root.is_dir() {
        return None;
    }
    let revision = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !revision.status.success() {
        return None;
    }
    let head = String::from_utf8(revision.stdout).ok()?;
    if head.trim() != REFERENCE_COMMIT {
        eprintln!(
            "skipping the live app-server surface probe: {} is at {}, not the pinned {REFERENCE_COMMIT}",
            root.display(),
            head.trim()
        );
        return None;
    }
    let interpreter = std::env::var_os(INTERPRETER_VARIABLE)
        .map(PathBuf::from)
        .into_iter()
        .chain([
            root.join(".venv/bin/python"),
            root.join(".venv/Scripts/python.exe"),
        ])
        .find(|candidate| candidate.is_file())?;
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
