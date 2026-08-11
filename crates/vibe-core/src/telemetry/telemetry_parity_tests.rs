//! Differential oracle for the telemetry and observability surface.
//!
//! `scripts/parity/telemetry.py` drives the reference's own datalake client,
//! metadata builders, event senders, tracing module, redaction policy, log
//! formatter and log reader over inputs the script authors, and records fifteen
//! families into `crates/vibe-core/tests/telemetry/corpus.json`. This module
//! replays that corpus against this build unconditionally: only the recapture
//! probe at the bottom skips when the checkout is absent or off-pin.
//!
//! What this build answers is read from the surfaces a running binary uses:
//! `enable_telemetry` comes from a document loaded through [`LayeredConfig`],
//! the envelope from [`TelemetryEnvelope`], the transport contract from
//! [`telemetry_headers`] and the constants beside it, the event vocabulary from
//! [`TelemetryEvent::event_name`], and the payload keys from
//! [`TelemetryProjection::from_engine_event`], which is what the engine observer
//! actually sends. Where this build has no counterpart at all, the answer is
//! `None` and the ledger below names the story that closes it, so the row goes
//! stale the moment the surface lands.
//!
//! A key is `family/field/case`, and a trailing `*` covers every key that starts
//! with the prefix. A divergence no entry names fails the replay; an entry whose
//! divergence stopped reproducing fails as stale, which is what forces a row out
//! once the behavior conforms.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::compaction::{CompactionFailureReason, CompactionStatus};
use crate::config::registry::{FIELDS, default_document};
use crate::config::{ConfigPaths, LayeredConfig};
use crate::events::{EngineEvent, EventEnvelope, LifecycleState};
use crate::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};

use super::{
    LaunchContext, TELEMETRY_ATTACHMENT_IMAGE, TELEMETRY_AUTHORIZATION_SCHEME,
    TELEMETRY_CALL_SOURCE, TELEMETRY_DEFAULT_API_KEY_VARIABLE, TELEMETRY_DEFAULT_BASE_URL,
    TELEMETRY_MAX_CONNECTIONS, TELEMETRY_MAX_KEEPALIVE_CONNECTIONS, TELEMETRY_PATH,
    TELEMETRY_TIMEOUT_SECONDS, TelemetryCallType, TelemetryClient, TelemetryConfig,
    TelemetryContext, TelemetryEnvelope, TelemetryEvent, TelemetryFuture, TelemetryProjection,
    TelemetryTransport, attachment_counts, merge_properties, platform_id, platform_version,
    telemetry_headers, telemetry_user_agent,
};

const CORPUS_RELATIVE: &str = "crates/vibe-core/tests/telemetry/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/telemetry.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 1;
/// The comparison floor this replay commits to, so a regeneration that captured
/// almost nothing fails instead of reporting a clean but empty run.
const MINIMUM_COMPARISONS: usize = 400;
/// The file the layered store reads a user document from.
const CONFIG_FILE: &str = "config.toml";

/// Every family the corpus declares and this replay reads. A family the capture
/// adds without a reader here fails the replay by name rather than passing
/// unread, and so does a family this replay expects and the corpus dropped.
const FAMILIES: [&str; 15] = [
    "constants",
    "envelope",
    "baseMetadata",
    "attachmentCounts",
    "eventVocabulary",
    "eventPayloads",
    "exporterConfig",
    "spans",
    "providerNames",
    "redaction",
    "logFormat",
    "logEncoding",
    "logParse",
    "logPagination",
    "logConfig",
];

/// Keys the corpus carries that are not families: the pin, the layout, the
/// prose-free note and the documents the configuration families are measured
/// over.
const METADATA: [&str; 4] = ["schemaVersion", "reference", "note", "documents"];

/// Cases where this build answers something other than the reference, each with
/// the reason and the story that closes it.
///
/// Every entry here is open work rather than an accepted divergence: this epic
/// builds the instrument, and EP-002 through EP-005 are what remove the rows.
/// The staleness check is what forces a row out once its behavior conforms, so
/// the ledger cannot outlive the gap it records.
const DIVERGENCES: &[(&str, &str)] = &[
    // -- EP-002: closed. What the epic left standing ------------------------
    (
        "constants/teleport*",
        "OPEN (US-011): the teleport payload vocabularies arrive with the teleport events",
    ),
    (
        "constants/projectSelectionSources/*",
        "OPEN (US-011): as above",
    ),
    (
        "constants/remoteProjectOutcomes/*",
        "OPEN (US-011): as above",
    ),
    (
        "baseMetadata/sentryTags/*",
        "ACCEPTED: the reference ships Sentry dormant, both DSNs null at the pin, so no crash \
         reporter exists here to tag; US-020 records the dormancy",
    ),
    // -- EP-003: the event vocabulary and its payloads ----------------------
    (
        "eventVocabulary/published/vibe.startup",
        "OPEN (US-008): the startup durations are not recorded by this build",
    ),
    (
        "eventVocabulary/published/vibe.slash_command_used",
        "OPEN (US-010): the slash-command record is not sent by this build",
    ),
    (
        "eventVocabulary/published/vibe.user_copied_text",
        "OPEN (US-010): the copy record is not sent by this build",
    ),
    (
        "eventVocabulary/published/vibe.user_cancelled_action",
        "OPEN (US-010): the cancellation record is not sent by this build",
    ),
    (
        "eventVocabulary/published/vibe.voice_mode_toggled",
        "OPEN (US-010): the voice toggle is not recorded by this build",
    ),
    (
        "eventVocabulary/published/vibe.admin_config_applied",
        "OPEN (US-010): the administered-configuration record is not sent by this build",
    ),
    (
        "eventVocabulary/published/vibe.onboarding_api_key_added",
        "OPEN (US-010): the onboarding record is not sent by this build",
    ),
    (
        "eventVocabulary/published/vibe.audio.*",
        "OPEN (US-012): the four transcription events are kept in a local ring buffer by \
         `crates/vibe-cli/src/tui/mod.rs` instead of being sent",
    ),
    (
        "eventVocabulary/published/vibe.read_aloud.*",
        "OPEN (US-012): the narrator has no telemetry here at all",
    ),
    (
        "eventPayloads/propertyKeys/*",
        "OPEN (US-008 through US-012): this build projects four events from engine events and \
         carries neither their full key sets nor the other twenty-two",
    ),
    // -- EP-004: OpenTelemetry tracing --------------------------------------
    (
        "constants/tracing/*",
        "OPEN (US-013, US-014): no tracer, no span families and no attribute keys exist in this \
         build; `enable_otel`, `otel_endpoint` and `otel_redaction` are declared in \
         `crates/vibe-core/src/config/registry.rs` and read by nothing",
    ),
    (
        "exporterConfig/*",
        "OPEN (US-013): no exporter configuration is resolved by this build",
    ),
    (
        "spans/*",
        "OPEN (US-014): no span family exists in this build",
    ),
    (
        "providerNames/normalized/*",
        "OPEN (US-015): the provider-name normalization has no counterpart",
    ),
    (
        "redaction/*",
        "OPEN (US-015): no redaction policy exists in this build",
    ),
    // -- EP-005: file logging and the log reader ----------------------------
    (
        "logFormat/*",
        "OPEN (US-017): no structured file log is written by this build",
    ),
    (
        "logEncoding/*",
        "OPEN (US-017): the backslash and newline encoding has no counterpart",
    ),
    (
        "logParse/*",
        "OPEN (US-018): `diagnostics/logs/read` answers from a process-local ring buffer in \
         `crates/vibe-app-server/src/resources.rs`, so no line is ever parsed",
    ),
    ("logPagination/*", "OPEN (US-018): as above"),
    (
        "logConfig/*",
        "OPEN (US-017): `LOG_LEVEL`, `DEBUG_MODE` and `LOG_MAX_BYTES` are read by nothing here",
    ),
    (
        "constants/logging/*",
        "OPEN (US-017, US-018): the log file, its rotation ceiling, its level vocabulary and its \
         line pattern have no counterpart",
    ),
];

// --------------------------------------------------------------------------
// The corpus
// --------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference: Reference,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    documents: Vec<Document>,
    constants: Constants,
    envelope: Vec<EnvelopeCase>,
    base_metadata: Vec<MetadataCase>,
    attachment_counts: Vec<AttachmentCase>,
    event_vocabulary: Vec<VocabularyEntry>,
    event_payloads: Vec<PayloadEntry>,
    exporter_config: Vec<ExporterCase>,
    spans: Vec<SpanCase>,
    provider_names: Vec<ProviderNameCase>,
    redaction: Vec<RedactionCase>,
    log_format: Vec<LogFormatCase>,
    log_encoding: Vec<LogEncodingCase>,
    log_parse: Vec<LogParseCase>,
    log_pagination: Vec<LogPaginationCase>,
    log_config: Vec<LogConfigCase>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Document {
    id: String,
    toml: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Constants {
    endpoint: EndpointConstants,
    transport: TransportConstants,
    tracing: Value,
    vocabularies: Vocabularies,
    logging: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EndpointConstants {
    default_base_url: String,
    events_path: String,
    #[expect(
        dead_code,
        reason = "the Mistral server default is the base the exporter derives from, compared by \
                  the exporterConfig family"
    )]
    default_mistral_server_url: String,
    default_api_key_variable: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TransportConstants {
    timeout_seconds: f64,
    #[expect(
        dead_code,
        reason = "the reference sets one timeout for every phase; the total is what this port caps"
    )]
    connect_timeout_seconds: f64,
    max_connections: u64,
    max_keepalive_connections: u64,
    header_names: Vec<String>,
    content_type: String,
    authorization_scheme: String,
    user_agent_mistral: String,
    user_agent_generic: String,
    #[expect(
        dead_code,
        reason = "the unknown backend answers what the generic one answers"
    )]
    user_agent_unknown: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Vocabularies {
    agent_entrypoints: Vec<String>,
    terminal_emulators: Vec<String>,
    otel_redaction_modes: Vec<String>,
    call_types: Vec<String>,
    call_source_default: String,
    attachment_kinds: Vec<String>,
    base_metadata_fields: Vec<String>,
    request_metadata_fields: Vec<String>,
    teleport_failure_stages: Vec<String>,
    teleport_context_summary_statuses: Vec<String>,
    project_selection_sources: Vec<String>,
    remote_project_outcomes: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EnvelopeCase {
    case: String,
    configuration: String,
    #[expect(
        dead_code,
        reason = "the request is what the correlation decides, compared below"
    )]
    correlation_id_requested: bool,
    enabled: bool,
    credential_resolved: bool,
    active: bool,
    sent: bool,
    flushed: bool,
    url: Option<String>,
    header_names: Vec<String>,
    content_type: Option<String>,
    user_agent: Option<String>,
    credential_variable: Option<String>,
    body_keys: Vec<String>,
    event: Option<String>,
    correlation_id: Option<String>,
    property_keys: Vec<String>,
    property_types: BTreeMap<String, String>,
    properties: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MetadataCase {
    case: String,
    base_keys: Vec<String>,
    base: Value,
    request_keys: Vec<String>,
    request: Value,
    #[expect(
        dead_code,
        reason = "the nulls-included key set is the `exclude_none` evidence US-006 reproduces"
    )]
    request_keys_with_nulls: Vec<String>,
    launch_fields: Option<Value>,
    sentry_tags: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AttachmentCase {
    case: String,
    supports_images: bool,
    counts: BTreeMap<String, u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VocabularyEntry {
    event: String,
    #[expect(
        dead_code,
        reason = "how the capture reached the name, not what it compares"
    )]
    source: String,
    #[expect(dead_code, reason = "reference paths are cited rather than compared")]
    sites: Vec<String>,
    #[expect(dead_code, reason = "as above")]
    sender: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PayloadEntry {
    event: String,
    #[expect(
        dead_code,
        reason = "how the capture reached the payload, not what it compares"
    )]
    source: String,
    property_keys: Vec<String>,
    #[expect(
        dead_code,
        reason = "US-008 through US-012 compare the types once the keys match"
    )]
    property_types: Option<Value>,
    #[expect(dead_code, reason = "as above")]
    properties: Option<Value>,
    #[expect(dead_code, reason = "as above")]
    correlated: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExporterCase {
    case: String,
    #[expect(
        dead_code,
        reason = "the request is the input; the resolution is the answer"
    )]
    endpoint_requested: String,
    configuration: Option<String>,
    resolved: Option<bool>,
    endpoint: Option<String>,
    header_names: Vec<String>,
    #[expect(
        dead_code,
        reason = "the variable is compared once US-013 resolves one"
    )]
    credential_variable: Option<String>,
    #[expect(dead_code, reason = "as above")]
    enable_telemetry: Option<bool>,
    #[expect(dead_code, reason = "as above")]
    enable_otel: Option<bool>,
    provider_installed: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpanCase {
    case: String,
    name: Option<String>,
    attribute_keys: Vec<String>,
    #[expect(dead_code, reason = "US-014 compares the values once the keys exist")]
    attributes: Value,
    status_code: Option<String>,
    #[expect(
        dead_code,
        reason = "the description is a digest, compared once US-014 writes one"
    )]
    status_description: Option<Value>,
    recorded_exceptions: u64,
    recording: bool,
    #[expect(
        dead_code,
        reason = "the never-raise policy is what `recording` records"
    )]
    body_ran: bool,
    #[expect(
        dead_code,
        reason = "the raised class is the input of the status decision"
    )]
    raised: Option<String>,
    #[expect(dead_code, reason = "as above")]
    backend_provider: Option<String>,
    #[expect(dead_code, reason = "as above")]
    backend_status: Option<i64>,
    #[expect(dead_code, reason = "as above")]
    record_unhandled_exception: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderNameCase {
    input: Option<String>,
    normalized: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RedactionCase {
    case: String,
    #[expect(dead_code, reason = "the set is named by the case")]
    attribute_set: String,
    #[expect(dead_code, reason = "as above")]
    mode: String,
    #[expect(
        dead_code,
        reason = "the span name survives redaction, compared once US-015 exports"
    )]
    span_name: String,
    surviving_keys: Vec<String>,
    #[expect(
        dead_code,
        reason = "the replaced set is the complement of the unchanged one"
    )]
    unchanged_keys: Vec<String>,
    replaced_keys: Vec<String>,
    #[expect(
        dead_code,
        reason = "the policy adds no key at the pin; US-015 compares it"
    )]
    added_keys: Vec<String>,
    #[expect(dead_code, reason = "as above")]
    removed_keys: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LogFormatCase {
    case: String,
    #[expect(dead_code, reason = "the level is the input; the line is the answer")]
    level: String,
    #[expect(dead_code, reason = "the message is the input")]
    message: String,
    line: String,
    has_exception: bool,
    #[expect(
        dead_code,
        reason = "US-017 compares the exception tail once a line is written"
    )]
    exception_separator: Option<String>,
    #[expect(dead_code, reason = "as above")]
    exception_encoded_newlines: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LogEncodingCase {
    #[expect(
        dead_code,
        reason = "the input is what the encoded and decoded pair answers for"
    )]
    input: Option<String>,
    encoded: String,
    decoded: String,
    #[expect(
        dead_code,
        reason = "the round trip is the pair of the two directions above"
    )]
    round_trips: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LogParseCase {
    case: String,
    #[expect(dead_code, reason = "the line is the input")]
    line: String,
    parsed: bool,
    timestamp: Option<String>,
    ppid: Option<i64>,
    pid: Option<i64>,
    level: Option<String>,
    message: Option<String>,
    #[expect(
        dead_code,
        reason = "the line number is supplied by the caller, not parsed"
    )]
    line_number: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LogPaginationCase {
    case: String,
    #[expect(
        dead_code,
        reason = "the page is the input; the entries are the answer"
    )]
    limit: u64,
    #[expect(dead_code, reason = "as above")]
    offset: u64,
    messages: Vec<String>,
    #[expect(
        dead_code,
        reason = "US-018 compares the numbering once entries come from a file"
    )]
    line_numbers: Vec<i64>,
    has_more: bool,
    cursor: Option<i64>,
    #[expect(
        dead_code,
        reason = "the shrink reset is compared once a reader holds a position"
    )]
    new_lines_count_reset: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LogConfigCase {
    case: String,
    level: Option<String>,
    max_bytes: Option<u64>,
    backup_count: Option<u64>,
    #[expect(
        dead_code,
        reason = "US-017 compares the encoding once a handler exists"
    )]
    encoding: Option<String>,
    #[expect(dead_code, reason = "as above")]
    logger_level: Option<String>,
    #[expect(dead_code, reason = "as above")]
    directory_created: bool,
    #[expect(dead_code, reason = "as above")]
    handler_count: u64,
    #[expect(dead_code, reason = "as above")]
    duplicate_guarded: Option<bool>,
    #[expect(
        dead_code,
        reason = "the raising branch is compared once a resolver exists"
    )]
    raised: Option<String>,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

fn corpus() -> Corpus {
    let path = repo_root().join(CORPUS_RELATIVE);
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} is readable: {error}", path.display()));
    let corpus: Corpus = serde_json::from_str(&raw).expect("the telemetry corpus parses");
    assert_eq!(
        corpus.schema_version, CORPUS_SCHEMA_VERSION,
        "the corpus layout moved; regenerate it with {CAPTURE_SCRIPT}"
    );
    assert_eq!(
        corpus.reference.commit, REFERENCE_COMMIT,
        "the corpus was captured from an unpinned reference"
    );
    corpus
}

// --------------------------------------------------------------------------
// The ledger
// --------------------------------------------------------------------------

/// Whether a ledger entry covers a divergence key: an exact match, or a
/// `prefix*` entry the key starts with.
fn covers(entry: &str, key: &str) -> bool {
    entry
        .strip_suffix('*')
        .map_or(entry == key, |prefix| key.starts_with(prefix))
}

/// Records one comparison, so a family reports a count and a divergence names
/// itself instead of stopping at the first one.
#[derive(Default)]
struct Report {
    conformant: usize,
    total: usize,
    divergences: Vec<String>,
    observed: Vec<String>,
}

impl Report {
    fn check<T: PartialEq + std::fmt::Debug>(
        &mut self,
        family: &str,
        field: &str,
        case: &str,
        expected: &T,
        actual: &T,
    ) {
        self.total = self.total.saturating_add(1);
        if expected == actual {
            self.conformant = self.conformant.saturating_add(1);
            return;
        }
        self.observed.push(format!("{family}/{field}/{case}"));
        self.divergences.push(format!(
            "{family}/{field}/{case}: reference {expected:?}, port {actual:?}"
        ));
    }

    /// A comparison against a surface this build does not have yet. The port
    /// answer is [`None`], which is what makes the entry go stale the moment the
    /// surface starts answering.
    fn check_absent<T: PartialEq + std::fmt::Debug>(
        &mut self,
        family: &str,
        field: &str,
        case: &str,
        expected: &T,
        actual: Option<&T>,
    ) {
        self.check(family, field, case, &Some(expected), &actual);
    }
}

/// What the ledger has to say about one family's report: the divergences it does
/// not name, and the entries whose divergence no longer reproduces.
fn audit(report: &Report, family: &str, ledger: &[(&str, &str)]) -> (Vec<String>, Vec<String>) {
    let unrecorded = report
        .divergences
        .iter()
        .filter(|line| {
            let key = line.split(':').next().unwrap_or_default();
            !ledger.iter().any(|(entry, _)| covers(entry, key))
        })
        .cloned()
        .collect::<Vec<_>>();
    let family_prefix = format!("{family}/");
    let stale = ledger
        .iter()
        .map(|(entry, _)| (*entry).to_owned())
        .filter(|entry| entry.starts_with(&family_prefix))
        .filter(|entry| !report.observed.iter().any(|key| covers(entry, key)))
        .collect::<Vec<_>>();
    (unrecorded, stale)
}

/// Fails on any divergence the ledger does not name, and on any ledger entry
/// whose divergence no longer reproduces, then reports the family's count.
fn settle(report: &Report, family: &str) -> usize {
    let (unrecorded, stale) = audit(report, family, DIVERGENCES);
    assert!(
        unrecorded.is_empty(),
        "{family} diverges from the reference and is unrecorded:\n{}",
        unrecorded.join("\n")
    );
    assert!(
        stale.is_empty(),
        "these {family} entries conform now and their ledger entry is stale: {stale:?}"
    );
    let ledgered = report.total.saturating_sub(report.conformant);
    println!(
        "telemetry: {family} {}/{} conform ({ledgered} ledgered)",
        report.conformant, report.total
    );
    report.total
}

// --------------------------------------------------------------------------
// This build's answers
// --------------------------------------------------------------------------

/// One configuration document, loaded the way the app-server loads a user's.
struct Loaded {
    _enclosure: tempfile::TempDir,
    effective: Option<toml::Table>,
}

impl Loaded {
    fn from_document(document: &str) -> Self {
        let enclosure = tempfile::tempdir().expect("temporary configuration root");
        let home = enclosure.path().join("home/.vibe");
        fs::create_dir_all(&home).expect("configuration home");
        if !document.trim().is_empty() {
            fs::write(home.join(CONFIG_FILE), document).expect("user document");
        }
        let effective = LayeredConfig::new(
            ConfigPaths {
                vibe_home: home,
                working_directory: enclosure.path().join("project"),
            },
            default_document(),
        )
        .load()
        .ok()
        .map(|snapshot| snapshot.effective);
        Self {
            _enclosure: enclosure,
            effective,
        }
    }

    /// Whether this build sends telemetry for the document, read where a
    /// running binary would read it.
    fn enable_telemetry(&self) -> Option<bool> {
        self.effective.as_ref()?.get("enable_telemetry")?.as_bool()
    }
}

/// The declared choices of a published enum field, which is what a settings
/// surface builds its list from.
fn published_choices(field: &str) -> Option<Vec<String>> {
    Some(
        FIELDS
            .iter()
            .find(|declared| declared.name == field)?
            .choices
            .iter()
            .map(|choice| (*choice).to_owned())
            .collect(),
    )
}

/// Every [`TelemetryEvent`] variant, which is the vocabulary this build is
/// measured over. Kept exhaustive by [`declared_name`].
const ALL_EVENTS: [TelemetryEvent; 12] = [
    TelemetryEvent::NewSession,
    TelemetryEvent::SessionClosed,
    TelemetryEvent::Ready,
    TelemetryEvent::RequestSent,
    TelemetryEvent::ToolCallFinished,
    TelemetryEvent::AtMentionInserted,
    TelemetryEvent::AutoCompactTriggered,
    TelemetryEvent::CompactionFailed,
    TelemetryEvent::TeleportCompleted,
    TelemetryEvent::TeleportFailed,
    TelemetryEvent::FeedbackSubmitted,
    TelemetryEvent::RemoteProjectConfigured,
];

/// The wire name each variant is measured under, restated here rather than read
/// from [`TelemetryEvent::event_name`], so a rename on one side fails the
/// replay instead of moving both the question and the answer.
///
/// The match carries no wildcard, which is what keeps [`ALL_EVENTS`]
/// exhaustive: a variant added to [`TelemetryEvent`] stops this module
/// compiling until it is named here and listed above. Without that guard a new
/// event would ship with no corpus entry and
/// [`every_event_this_build_publishes_has_a_corpus_entry`] would pass quietly,
/// which is the one way this replay could overstate the score.
const fn declared_name(event: TelemetryEvent) -> &'static str {
    match event {
        TelemetryEvent::NewSession => "vibe.new_session",
        TelemetryEvent::SessionClosed => "vibe.session_closed",
        TelemetryEvent::Ready => "vibe.ready",
        TelemetryEvent::RequestSent => "vibe.request_sent",
        TelemetryEvent::ToolCallFinished => "vibe.tool_call_finished",
        TelemetryEvent::AtMentionInserted => "vibe.at_mention_inserted",
        TelemetryEvent::AutoCompactTriggered => "vibe.auto_compact_triggered",
        TelemetryEvent::CompactionFailed => "vibe.compaction_failed",
        TelemetryEvent::TeleportCompleted => "vibe.teleport_completed",
        TelemetryEvent::TeleportFailed => "vibe.teleport_failed",
        TelemetryEvent::FeedbackSubmitted => "vibe.user_rating_feedback",
        TelemetryEvent::RemoteProjectConfigured => "vibe.remote_project_configured",
    }
}

/// The event names this build publishes.
fn published_events() -> Vec<TelemetryEvent> {
    ALL_EVENTS.to_vec()
}

/// The launch context the capture drove the reference's builders with, so both
/// sides answer for the same inputs.
fn oracle_launch(terminal: Option<&str>) -> LaunchContext {
    LaunchContext {
        agent_entrypoint: "cli".to_owned(),
        agent_version: "oracle-agent-version".to_owned(),
        client_name: "oracle-client".to_owned(),
        client_version: "oracle-client-version".to_owned(),
        terminal_emulator: terminal.map(ToOwned::to_owned),
    }
}

/// The context every envelope case is sent under, which is the one the capture
/// built its client with.
fn oracle_context() -> TelemetryContext {
    TelemetryContext {
        launch: Some(oracle_launch(Some("ghostty"))),
        parent_session_id: Some("oracle-parent-session".to_owned()),
        experiments: BTreeMap::new(),
        user_plan: Some("oracle-plan".to_owned()),
    }
}

/// The sentinels the capture set before driving the reference, named here by
/// the same variables. A value never leaves this function, and
/// `ORACLE_ABSENT_KEY` is deliberately absent so the no-credential branch is
/// reached through a variable the corpus names.
fn oracle_credentials(variable: &str) -> Option<String> {
    match variable {
        "MISTRAL_API_KEY"
        | "ORACLE_MISTRAL_KEY"
        | "ORACLE_PROXY_KEY"
        | "ORACLE_THIRD_PARTY_KEY" => Some(format!("{variable}-sentinel")),
        _ => None,
    }
}

/// Records the request one send would have issued, one call before the
/// connection, which is where the capture intercepted the reference.
#[derive(Default)]
struct RecordingTransport {
    sent: std::sync::Mutex<Vec<(String, String, TelemetryEnvelope)>>,
}

impl TelemetryTransport for RecordingTransport {
    fn send<'a>(
        &'a self,
        endpoint: &'a url::Url,
        user_agent: &'a str,
        _credential: &'a secrecy::SecretString,
        envelope: &'a TelemetryEnvelope,
    ) -> TelemetryFuture<'a> {
        if let Ok(mut sent) = self.sent.lock() {
            sent.push((
                endpoint.to_string(),
                user_agent.to_owned(),
                envelope.clone(),
            ));
        }
        Box::pin(async { Ok(()) })
    }
}

/// What this build answers for one envelope case, driven through the client the
/// CLI observer sends with.
struct Sent {
    enabled: bool,
    credential_resolved: bool,
    active: bool,
    request: Option<(String, String, TelemetryEnvelope)>,
}

impl Sent {
    fn drive(
        document: &toml::Table,
        payload: Map<String, Value>,
        correlation: Option<&str>,
    ) -> Self {
        let owned = document.clone();
        let config = TelemetryConfig::resolve(&owned, &oracle_credentials);
        let enabled = owned
            .get("enable_telemetry")
            .and_then(toml::Value::as_bool)
            .unwrap_or(true);
        let credential_resolved = config
            .target()
            .and_then(|target| oracle_credentials(&target.credential_variable))
            .is_some();
        let active = config.is_active();
        let client = TelemetryClient::new(
            Arc::new(move || TelemetryConfig::resolve(&owned, &oracle_credentials)),
            RecordingTransport::default(),
        );
        let envelope = TelemetryEnvelope::new(
            TelemetryEvent::Ready.event_name(),
            merge_properties(
                oracle_context()
                    .base_metadata(Some("oracle-session"))
                    .properties(),
                payload,
            ),
            correlation.map(ToOwned::to_owned),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime for the replay");
        // The record future resolving is what the reference's `aclose` waits
        // for: a pending delivery is joined rather than dropped.
        runtime
            .block_on(client.record(&envelope))
            .expect("the record resolves");
        let request = client
            .transport
            .sent
            .lock()
            .ok()
            .and_then(|sent| sent.first().cloned());
        Self {
            enabled,
            credential_resolved,
            active,
            request,
        }
    }
}

/// The corpus masks three host-dependent values, so a comparison substitutes
/// what this host answers before comparing.
fn unmask(value: &Value) -> Value {
    let Some(text) = value.as_str() else {
        return value.clone();
    };
    match text {
        "{platformId}" => Value::String(platform_id()),
        "{platformVersion}" => platform_version().map_or(Value::Null, Value::String),
        _ => Value::String(text.replace("{version}", env!("CARGO_PKG_VERSION"))),
    }
}

fn unmask_map(value: &Value) -> Map<String, Value> {
    value
        .as_object()
        .map(|entries| {
            entries
                .iter()
                .map(|(key, value)| (key.clone(), unmask(value)))
                .collect()
        })
        .unwrap_or_default()
}

/// The wire type of one property, named as `type_name` in the capture names it.
fn type_name(value: &Value) -> String {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "string",
        Value::Object(_) => "object",
        Value::Array(_) => "array",
    }
    .to_owned()
}

fn sorted_keys(value: &Value) -> Vec<String> {
    value
        .as_object()
        .map(|object| object.keys().cloned().collect())
        .unwrap_or_default()
}

/// The attribute keys this build attaches to one event name, driven through the
/// projection the engine observer uses.
///
/// The four events below are the ones an engine event reaches; the rest have no
/// producer here at all, which is what the ledger records.
fn port_payload_keys(event: &str) -> Option<Vec<String>> {
    let envelopes = [
        EventEnvelope {
            session_id: "oracle-session".to_owned(),
            turn_id: Some("oracle-turn".to_owned()),
            emitted_at: 1,
            event_id: 1,
            event: EngineEvent::ToolResult {
                call_id: "oracle-call".to_owned(),
                content: "oracle result".to_owned(),
                typed_result: json!({}),
                display: json!({}),
                duration_ms: 9,
                is_error: false,
                cancelled: false,
            },
        },
        EventEnvelope {
            session_id: "oracle-session".to_owned(),
            turn_id: Some("oracle-turn".to_owned()),
            emitted_at: 1,
            event_id: 2,
            event: EngineEvent::CompactionOutcome {
                compaction_id: "oracle-compaction".to_owned(),
                status: CompactionStatus::Failure,
                context_tokens_before: 150_000,
                threshold: 120_000,
                reason: Some(CompactionFailureReason::ToolCall),
            },
        },
        EventEnvelope {
            session_id: "oracle-session".to_owned(),
            turn_id: Some("oracle-turn".to_owned()),
            emitted_at: 1,
            event_id: 3,
            event: EngineEvent::Lifecycle {
                state: LifecycleState::Running,
                message: None,
            },
        },
    ];
    for envelope in &envelopes {
        for projection in
            TelemetryProjection::from_engine_event(envelope).expect("the projection succeeds")
        {
            if projection.event.event_name() != event {
                continue;
            }
            let attributes =
                serde_json::to_value(&projection.attributes).expect("attributes serialize");
            return Some(sorted_keys(&attributes));
        }
    }
    None
}

// --------------------------------------------------------------------------
// The families
// --------------------------------------------------------------------------

fn run_constants(constants: &Constants, report: &mut Report) {
    let case = "declared";
    report.check(
        "constants",
        "eventsPath",
        case,
        &constants.endpoint.events_path,
        &TELEMETRY_PATH.to_owned(),
    );
    report.check(
        "constants",
        "defaultBaseUrl",
        case,
        &constants.endpoint.default_base_url,
        &TELEMETRY_DEFAULT_BASE_URL.to_owned(),
    );
    report.check(
        "constants",
        "defaultApiKeyVariable",
        case,
        &constants.endpoint.default_api_key_variable,
        &TELEMETRY_DEFAULT_API_KEY_VARIABLE.to_owned(),
    );

    report.check(
        "constants",
        "timeoutSeconds",
        case,
        &constants.transport.timeout_seconds,
        #[expect(
            clippy::cast_precision_loss,
            reason = "the reference records a float second count and this port a whole one"
        )]
        &(TELEMETRY_TIMEOUT_SECONDS as f64),
    );
    report.check(
        "constants",
        "maxKeepaliveConnections",
        case,
        &constants.transport.max_keepalive_connections,
        &(TELEMETRY_MAX_KEEPALIVE_CONNECTIONS as u64),
    );
    report.check(
        "constants",
        "maxConnections",
        case,
        &constants.transport.max_connections,
        &(TELEMETRY_MAX_CONNECTIONS as u64),
    );

    let headers = telemetry_headers(
        &telemetry_user_agent(Some("mistral")),
        &secrecy::SecretString::from("oracle-credential"),
    )
    .expect("the header set builds");
    let mut names = headers
        .keys()
        .map(|name| name.as_str().to_owned())
        .collect::<Vec<_>>();
    names.sort_unstable();
    let mut declared = constants
        .transport
        .header_names
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect::<Vec<_>>();
    declared.sort_unstable();
    report.check("constants", "headerNames", case, &declared, &names);
    report.check(
        "constants",
        "contentType",
        case,
        &constants.transport.content_type,
        &headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned(),
    );
    report.check(
        "constants",
        "authorizationScheme",
        case,
        &constants.transport.authorization_scheme,
        &TELEMETRY_AUTHORIZATION_SCHEME.to_owned(),
    );
    for (field, declared, backend) in [
        (
            "userAgentMistral",
            &constants.transport.user_agent_mistral,
            Some("mistral"),
        ),
        (
            "userAgentGeneric",
            &constants.transport.user_agent_generic,
            Some("generic"),
        ),
    ] {
        report.check(
            "constants",
            field,
            case,
            &declared.replace("{version}", env!("CARGO_PKG_VERSION")),
            &telemetry_user_agent(backend),
        );
    }

    run_tracing_constants(&constants.tracing, report);
    run_logging_constants(&constants.logging, report);
    run_vocabularies(&constants.vocabularies, report);
}

/// Every tracing constant, compared against the nothing this build declares.
/// Flattened rather than enumerated field by field so a key the reference adds
/// is compared without editing this module.
fn run_tracing_constants(tracing: &Value, report: &mut Report) {
    for (field, value) in flatten(tracing) {
        report.check_absent("constants", "tracing", &field, &value, None);
    }
}

fn run_logging_constants(logging: &Value, report: &mut Report) {
    for (field, value) in flatten(logging) {
        report.check_absent("constants", "logging", &field, &value, None);
    }
}

/// A nested constant table as `(pointer, value)` pairs, so one comparison is
/// reported per leaf rather than one per table.
fn flatten(value: &Value) -> Vec<(String, Value)> {
    let mut flattened = Vec::new();
    let mut pending = vec![(String::new(), value.clone())];
    while let Some((prefix, current)) = pending.pop() {
        match current {
            Value::Object(entries) => {
                for (key, item) in entries {
                    let path = if prefix.is_empty() {
                        key
                    } else {
                        format!("{prefix}.{key}")
                    };
                    pending.push((path, item));
                }
            }
            other => flattened.push((prefix, other)),
        }
    }
    flattened.sort_by(|left, right| left.0.cmp(&right.0));
    flattened
}

fn run_vocabularies(vocabularies: &Vocabularies, report: &mut Report) {
    let case = "declared";
    let mut entrypoints = vocabularies.agent_entrypoints.clone();
    entrypoints.sort_unstable();
    report.check(
        "constants",
        "agentEntrypoints",
        case,
        &entrypoints,
        &serde_variants::<vibe_protocol::ClientEntrypoint>(&[
            vibe_protocol::ClientEntrypoint::Unknown,
            vibe_protocol::ClientEntrypoint::Cli,
            vibe_protocol::ClientEntrypoint::Acp,
            vibe_protocol::ClientEntrypoint::Programmatic,
        ]),
    );
    let mut terminals = vocabularies.terminal_emulators.clone();
    terminals.sort_unstable();
    report.check(
        "constants",
        "terminalEmulators",
        case,
        &terminals,
        &serde_variants::<vibe_protocol::TerminalEmulator>(&[
            vibe_protocol::TerminalEmulator::Unknown,
            vibe_protocol::TerminalEmulator::Vscode,
            vibe_protocol::TerminalEmulator::VscodeInsiders,
            vibe_protocol::TerminalEmulator::Cursor,
            vibe_protocol::TerminalEmulator::Jetbrains,
            vibe_protocol::TerminalEmulator::AppleTerminal,
            vibe_protocol::TerminalEmulator::Iterm2,
            vibe_protocol::TerminalEmulator::Wezterm,
            vibe_protocol::TerminalEmulator::Ghostty,
            vibe_protocol::TerminalEmulator::Alacritty,
            vibe_protocol::TerminalEmulator::Kitty,
            vibe_protocol::TerminalEmulator::Hyper,
            vibe_protocol::TerminalEmulator::WindowsTerminal,
        ]),
    );
    let mut modes = vocabularies.otel_redaction_modes.clone();
    modes.sort_unstable();
    let mut published = published_choices("otel_redaction").unwrap_or_default();
    published.sort_unstable();
    report.check("constants", "otelRedactionModes", case, &modes, &published);

    let mut call_types = vocabularies.call_types.clone();
    call_types.sort_unstable();
    report.check(
        "constants",
        "callTypes",
        case,
        &call_types,
        &serde_variants::<TelemetryCallType>(&[
            TelemetryCallType::MainCall,
            TelemetryCallType::SecondaryCall,
        ]),
    );
    report.check(
        "constants",
        "callSourceDefault",
        case,
        &vocabularies.call_source_default,
        &TELEMETRY_CALL_SOURCE.to_owned(),
    );
    report.check(
        "constants",
        "attachmentKinds",
        case,
        &vocabularies.attachment_kinds,
        &attachment_counts(1, true)
            .into_keys()
            .collect::<Vec<String>>(),
    );

    // The census fields are read off a fully populated one, which is the only
    // way an optional field is observable at all.
    let populated = TelemetryContext {
        launch: Some(oracle_launch(Some("ghostty"))),
        parent_session_id: Some("oracle-parent-session".to_owned()),
        experiments: BTreeMap::from([("ab".to_owned(), "on".to_owned())]),
        user_plan: Some("oracle-plan".to_owned()),
    };
    let base = Value::Object(populated.base_metadata(Some("oracle-session")).properties());
    let mut declared = vocabularies.base_metadata_fields.clone();
    declared.sort_unstable();
    report.check(
        "constants",
        "baseMetadataFields",
        case,
        &declared,
        &sorted_keys(&base),
    );
    let request = Value::Object(
        populated
            .request_metadata(
                Some("oracle-session"),
                TelemetryCallType::MainCall,
                Some("oracle-message".to_owned()),
            )
            .properties(),
    );
    let mut declared = vocabularies.request_metadata_fields.clone();
    declared.sort_unstable();
    let mut added = sorted_keys(&request)
        .into_iter()
        .filter(|key| !sorted_keys(&base).contains(key))
        .collect::<Vec<_>>();
    added.sort_unstable();
    report.check(
        "constants",
        "requestMetadataFields",
        case,
        &declared,
        &added,
    );
    report.check_absent(
        "constants",
        "teleportFailureStages",
        case,
        &vocabularies.teleport_failure_stages,
        None,
    );
    report.check_absent(
        "constants",
        "teleportContextSummaryStatuses",
        case,
        &vocabularies.teleport_context_summary_statuses,
        None,
    );
    report.check_absent(
        "constants",
        "projectSelectionSources",
        case,
        &vocabularies.project_selection_sources,
        None,
    );
    report.check_absent(
        "constants",
        "remoteProjectOutcomes",
        case,
        &vocabularies.remote_project_outcomes,
        None,
    );
}

/// The serialized names of an enum's variants, sorted, which is the vocabulary a
/// wire value is drawn from.
fn serde_variants<T: serde::Serialize>(variants: &[T]) -> Vec<String> {
    let mut names = variants
        .iter()
        .map(|variant| {
            serde_json::to_value(variant)
                .ok()
                .and_then(|value| value.as_str().map(ToOwned::to_owned))
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

/// The payload and the correlation identifier the capture sent one case with.
/// The two named cases are the caller-override and the empty-identifier
/// probes; every other case sends the startup duration the reference sends.
fn envelope_inputs(case: &str) -> (Map<String, Value>, Option<&'static str>) {
    match case {
        "caller-properties-win-over-metadata" => (
            [
                ("version".to_owned(), json!("authored-by-the-caller")),
                ("extra".to_owned(), json!(1)),
            ]
            .into_iter()
            .collect(),
            None,
        ),
        "empty-correlation-id" => (
            [("init_duration_ms".to_owned(), json!(1234))]
                .into_iter()
                .collect(),
            Some(""),
        ),
        _ => (
            [("init_duration_ms".to_owned(), json!(1234))]
                .into_iter()
                .collect(),
            case.ends_with("-correlation-yes")
                .then_some("oracle-correlation"),
        ),
    }
}

fn run_envelope(corpus: &Corpus, report: &mut Report) {
    let documents = corpus
        .documents
        .iter()
        .map(|document| (document.id.as_str(), document.toml.as_str()))
        .collect::<BTreeMap<_, _>>();
    for entry in &corpus.envelope {
        let document = documents
            .get(entry.configuration.as_str())
            .unwrap_or_else(|| panic!("`{}` names a declared document", entry.case));
        let case = entry.case.as_str();
        // The capture validated the document on its own, so the provider table
        // both sides resolve over is the document's own rather than one the
        // shipped defaults filled in.
        let table = document
            .parse::<toml::Table>()
            .unwrap_or_else(|error| panic!("`{case}` parses: {error}"));
        let (payload, correlation) = envelope_inputs(case);
        let sent = Sent::drive(&table, payload, correlation);

        // The gate, read twice: through the store a running binary loads, and
        // through the resolution the client performs on every send.
        report.check(
            "envelope",
            "enabled",
            case,
            &entry.enabled,
            &Loaded::from_document(document)
                .enable_telemetry()
                .unwrap_or_default(),
        );
        report.check(
            "envelope",
            "enabledResolved",
            case,
            &entry.enabled,
            &sent.enabled,
        );
        report.check(
            "envelope",
            "credentialResolved",
            case,
            &entry.credential_resolved,
            &sent.credential_resolved,
        );
        report.check("envelope", "active", case, &entry.active, &sent.active);
        report.check(
            "envelope",
            "sent",
            case,
            &entry.sent,
            &sent.request.is_some(),
        );
        // The reference's `aclose` gathers the pending task before closing the
        // client; this replay awaits the delivery future for the same reason.
        report.check("envelope", "flushed", case, &entry.flushed, &true);
        report.check(
            "envelope",
            "url",
            case,
            &entry.url,
            &sent.request.as_ref().map(|(url, _, _)| url.clone()),
        );
        report.check(
            "envelope",
            "credentialVariable",
            case,
            &entry.credential_variable,
            &sent.request.as_ref().and_then(|_| {
                TelemetryConfig::resolve(&table, &oracle_credentials)
                    .target()
                    .map(|target| target.credential_variable.clone())
            }),
        );
        report.check(
            "envelope",
            "userAgent",
            case,
            &entry
                .user_agent
                .as_ref()
                .map(|agent| agent.replace("{version}", env!("CARGO_PKG_VERSION"))),
            &sent.request.as_ref().map(|(_, agent, _)| agent.clone()),
        );

        let headers = sent.request.as_ref().map(|(_, agent, _)| {
            telemetry_headers(agent, &secrecy::SecretString::from("oracle-credential"))
                .expect("the header set builds")
        });
        report.check(
            "envelope",
            "contentType",
            case,
            &entry.content_type,
            &headers.as_ref().and_then(|headers| {
                headers
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned)
            }),
        );
        let mut declared = entry
            .header_names
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect::<Vec<_>>();
        declared.sort_unstable();
        let mut names = headers
            .as_ref()
            .map(|headers| {
                headers
                    .keys()
                    .map(|name| name.as_str().to_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        names.sort_unstable();
        report.check("envelope", "headerNames", case, &declared, &names);

        let body = sent.request.as_ref().map(|(_, _, envelope)| {
            serde_json::to_value(envelope).expect("the envelope serializes")
        });
        report.check(
            "envelope",
            "correlationId",
            case,
            &entry.correlation_id,
            &sent
                .request
                .as_ref()
                .and_then(|(_, _, envelope)| envelope.correlation_id.clone()),
        );
        report.check(
            "envelope",
            "bodyKeys",
            case,
            &entry.body_keys,
            &body.as_ref().map(sorted_keys).unwrap_or_default(),
        );
        report.check(
            "envelope",
            "event",
            case,
            &entry.event,
            &sent
                .request
                .as_ref()
                .map(|(_, _, envelope)| envelope.event.clone()),
        );
        let properties = sent
            .request
            .as_ref()
            .map(|(_, _, envelope)| Value::Object(envelope.properties.clone()))
            .unwrap_or(Value::Object(Map::new()));
        report.check(
            "envelope",
            "propertyKeys",
            case,
            &entry.property_keys,
            &sorted_keys(&properties),
        );
        report.check(
            "envelope",
            "propertyTypes",
            case,
            &entry.property_types,
            &properties
                .as_object()
                .map(|entries| {
                    entries
                        .iter()
                        .map(|(key, value)| (key.clone(), type_name(value)))
                        .collect()
                })
                .unwrap_or_default(),
        );
        report.check(
            "envelope",
            "properties",
            case,
            &unmask_map(&entry.properties),
            &properties.as_object().cloned().unwrap_or_default(),
        );
    }
}

/// The inputs the capture built one metadata case from, named by the case.
/// A case this table does not know fails the replay rather than being measured
/// against inputs it was not captured with.
fn metadata_inputs(scenario: &str) -> Option<(TelemetryContext, Option<&'static str>)> {
    let session = Some("oracle-session");
    let parent = Some("oracle-parent-session".to_owned());
    let plan = Some("oracle-plan".to_owned());
    let experiments = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>()
    };
    Some(match scenario {
        "full-launch-context" => (
            TelemetryContext {
                launch: Some(oracle_launch(Some("ghostty"))),
                parent_session_id: parent,
                experiments: experiments(&[("ab", "on")]),
                user_plan: plan,
            },
            session,
        ),
        "no-launch-context" => (
            TelemetryContext {
                launch: None,
                parent_session_id: parent,
                experiments: BTreeMap::new(),
                user_plan: plan,
            },
            session,
        ),
        "no-session" => (
            TelemetryContext {
                launch: Some(oracle_launch(Some("ghostty"))),
                ..TelemetryContext::default()
            },
            None,
        ),
        "empty-experiments" => (
            TelemetryContext {
                launch: Some(oracle_launch(Some("ghostty"))),
                ..TelemetryContext::default()
            },
            session,
        ),
        "no-terminal-emulator" => (
            TelemetryContext {
                launch: Some(oracle_launch(None)),
                parent_session_id: parent,
                ..TelemetryContext::default()
            },
            session,
        ),
        "no-user-plan" => (
            TelemetryContext {
                launch: Some(oracle_launch(Some("ghostty"))),
                parent_session_id: parent,
                experiments: experiments(&[("ab", "off")]),
                user_plan: None,
            },
            session,
        ),
        _ => return None,
    })
}

fn run_base_metadata(corpus: &Corpus, report: &mut Report) {
    for entry in &corpus.base_metadata {
        let case = entry.case.as_str();
        let (scenario, call_type) = case
            .strip_suffix("-main_call")
            .map(|scenario| (scenario, TelemetryCallType::MainCall))
            .or_else(|| {
                case.strip_suffix("-secondary_call")
                    .map(|scenario| (scenario, TelemetryCallType::SecondaryCall))
            })
            .unwrap_or_else(|| panic!("`{case}` names a call type"));
        let (context, session) = metadata_inputs(scenario)
            .unwrap_or_else(|| panic!("`{scenario}` is a captured metadata scenario"));

        let base = Value::Object(context.base_metadata(session).properties());
        report.check(
            "baseMetadata",
            "baseKeys",
            case,
            &entry.base_keys,
            &sorted_keys(&base),
        );
        report.check(
            "baseMetadata",
            "base",
            case,
            &unmask_map(&entry.base),
            &base.as_object().cloned().unwrap_or_default(),
        );

        let message =
            matches!(call_type, TelemetryCallType::MainCall).then(|| "oracle-message".to_owned());
        let request = Value::Object(
            context
                .request_metadata(session, call_type, message)
                .properties(),
        );
        report.check(
            "baseMetadata",
            "requestKeys",
            case,
            &entry.request_keys,
            &sorted_keys(&request),
        );
        report.check(
            "baseMetadata",
            "request",
            case,
            &unmask_map(&entry.request),
            &request.as_object().cloned().unwrap_or_default(),
        );
        report.check(
            "baseMetadata",
            "launchFields",
            case,
            &entry.launch_fields,
            &context
                .launch
                .as_ref()
                .map(|launch| serde_json::to_value(launch).expect("the launch context serializes")),
        );
        report.check_absent("baseMetadata", "sentryTags", case, &entry.sentry_tags, None);
    }
}

/// The images and the image support one attachment case was captured with.
fn attachment_inputs(case: &str) -> Option<usize> {
    match case {
        "no-message" | "no-images" => Some(0),
        "images-with-support" | "images-without-support" => Some(2),
        _ => None,
    }
}

fn run_attachment_counts(corpus: &Corpus, report: &mut Report) {
    for entry in &corpus.attachment_counts {
        let images = attachment_inputs(&entry.case)
            .unwrap_or_else(|| panic!("`{}` is a captured attachment scenario", entry.case));
        report.check(
            "attachmentCounts",
            "counts",
            &entry.case,
            &entry.counts,
            &attachment_counts(images, entry.supports_images),
        );
        report.check(
            "attachmentCounts",
            "kind",
            &entry.case,
            &entry
                .counts
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .first()
                .cloned(),
            &attachment_counts(images, entry.supports_images)
                .contains_key(TELEMETRY_ATTACHMENT_IMAGE)
                .then(|| TELEMETRY_ATTACHMENT_IMAGE.to_owned()),
        );
    }
}

fn run_event_vocabulary(corpus: &Corpus, report: &mut Report) {
    let published = published_events()
        .into_iter()
        .map(TelemetryEvent::event_name)
        .collect::<Vec<_>>();
    for entry in &corpus.event_vocabulary {
        report.check(
            "eventVocabulary",
            "published",
            &entry.event,
            &true,
            &published.contains(&entry.event.as_str()),
        );
    }
}

fn run_event_payloads(corpus: &Corpus, report: &mut Report) {
    for entry in &corpus.event_payloads {
        report.check_absent(
            "eventPayloads",
            "propertyKeys",
            &entry.event,
            &entry.property_keys,
            port_payload_keys(&entry.event).as_ref(),
        );
    }
}

fn run_exporter_config(corpus: &Corpus, report: &mut Report) {
    for entry in &corpus.exporter_config {
        let case = entry.case.as_str();
        report.check_absent("exporterConfig", "resolved", case, &entry.resolved, None);
        report.check_absent("exporterConfig", "endpoint", case, &entry.endpoint, None);
        report.check_absent(
            "exporterConfig",
            "headerNames",
            case,
            &entry.header_names,
            None,
        );
        report.check_absent(
            "exporterConfig",
            "providerInstalled",
            case,
            &entry.provider_installed,
            None,
        );
    }
}

fn run_spans(corpus: &Corpus, report: &mut Report) {
    for entry in &corpus.spans {
        let case = entry.case.as_str();
        report.check_absent("spans", "name", case, &entry.name, None);
        report.check_absent("spans", "attributeKeys", case, &entry.attribute_keys, None);
        report.check_absent("spans", "statusCode", case, &entry.status_code, None);
        report.check_absent(
            "spans",
            "recordedExceptions",
            case,
            &entry.recorded_exceptions,
            None,
        );
        report.check_absent("spans", "recording", case, &entry.recording, None);
    }
}

fn run_provider_names(corpus: &Corpus, report: &mut Report) {
    for entry in &corpus.provider_names {
        let case = entry.input.as_deref().unwrap_or("<none>");
        report.check_absent("providerNames", "normalized", case, &entry.normalized, None);
    }
}

fn run_redaction(corpus: &Corpus, report: &mut Report) {
    for entry in &corpus.redaction {
        let case = entry.case.as_str();
        report.check_absent(
            "redaction",
            "survivingKeys",
            case,
            &entry.surviving_keys,
            None,
        );
        report.check_absent(
            "redaction",
            "replacedKeys",
            case,
            &entry.replaced_keys,
            None,
        );
    }
}

fn run_log_format(corpus: &Corpus, report: &mut Report) {
    for entry in &corpus.log_format {
        report.check_absent("logFormat", "line", &entry.case, &entry.line, None);
        report.check_absent(
            "logFormat",
            "hasException",
            &entry.case,
            &entry.has_exception,
            None,
        );
    }
}

fn run_log_encoding(corpus: &Corpus, report: &mut Report) {
    for (index, entry) in corpus.log_encoding.iter().enumerate() {
        let case = format!("message-{index}");
        report.check_absent("logEncoding", "encoded", &case, &entry.encoded, None);
        report.check_absent("logEncoding", "decoded", &case, &entry.decoded, None);
    }
}

fn run_log_parse(corpus: &Corpus, report: &mut Report) {
    for entry in &corpus.log_parse {
        let case = entry.case.as_str();
        report.check_absent("logParse", "parsed", case, &entry.parsed, None);
        report.check_absent("logParse", "level", case, &entry.level, None);
        report.check_absent("logParse", "message", case, &entry.message, None);
        report.check_absent("logParse", "timestamp", case, &entry.timestamp, None);
        report.check_absent("logParse", "ppid", case, &entry.ppid, None);
        report.check_absent("logParse", "pid", case, &entry.pid, None);
    }
}

fn run_log_pagination(corpus: &Corpus, report: &mut Report) {
    for entry in &corpus.log_pagination {
        let case = entry.case.as_str();
        report.check_absent("logPagination", "messages", case, &entry.messages, None);
        report.check_absent("logPagination", "hasMore", case, &entry.has_more, None);
        report.check_absent("logPagination", "cursor", case, &entry.cursor, None);
    }
}

fn run_log_config(corpus: &Corpus, report: &mut Report) {
    for entry in &corpus.log_config {
        let case = entry.case.as_str();
        report.check_absent("logConfig", "level", case, &entry.level, None);
        report.check_absent("logConfig", "maxBytes", case, &entry.max_bytes, None);
        report.check_absent("logConfig", "backupCount", case, &entry.backup_count, None);
    }
}

// --------------------------------------------------------------------------
// The tests
// --------------------------------------------------------------------------

/// The corpus declares exactly the families this replay reads. A family the
/// capture adds without a reader here fails by name rather than passing unread,
/// which is the way a corpus silently outgrows its runner.
#[test]
fn every_declared_family_has_a_reader() {
    let path = repo_root().join(CORPUS_RELATIVE);
    let raw = fs::read_to_string(&path).expect("the corpus is readable");
    let document: Value = serde_json::from_str(&raw).expect("the corpus parses");
    let object = document.as_object().expect("the corpus is an object");
    let mut declared = object
        .keys()
        .map(String::as_str)
        .filter(|key| !METADATA.contains(key))
        .collect::<Vec<_>>();
    declared.sort_unstable();
    let mut known = FAMILIES.to_vec();
    known.sort_unstable();
    assert_eq!(
        declared, known,
        "the corpus and this replay disagree on the families; regenerate with {CAPTURE_SCRIPT} \
         or give the new family a reader"
    );
}

/// The corpus is a corpus of documents, and every configuration case names one
/// of them. A case with no document would compare this build against nothing.
#[test]
fn every_configuration_case_names_a_declared_document() {
    let corpus = corpus();
    let declared = corpus
        .documents
        .iter()
        .map(|document| document.id.as_str())
        .collect::<Vec<_>>();
    assert!(
        declared.len() >= 8,
        "the corpus carries {} documents, below the 8 the envelope cases need",
        declared.len()
    );
    for entry in &corpus.envelope {
        assert!(
            declared.contains(&entry.configuration.as_str()),
            "{}",
            entry.case
        );
    }
    for entry in &corpus.exporter_config {
        if let Some(configuration) = entry.configuration.as_deref() {
            assert!(declared.contains(&configuration), "{}", entry.case);
        }
    }
}

/// The corpus is the vocabulary this build is measured against, so an event name
/// this build publishes has to appear in it. A name added here without a corpus
/// entry would ship unmeasured.
#[test]
fn every_event_this_build_publishes_has_a_corpus_entry() {
    let corpus = corpus();
    let declared = corpus
        .event_vocabulary
        .iter()
        .map(|entry| entry.event.as_str())
        .collect::<Vec<_>>();
    for event in published_events() {
        assert_eq!(
            declared_name(event),
            event.event_name(),
            "`{event:?}` is measured under one name and published under another"
        );
        assert!(
            declared.contains(&event.event_name()),
            "this build publishes `{}`, which the corpus does not record; regenerate it with \
             {CAPTURE_SCRIPT} or stop publishing the event",
            event.event_name()
        );
    }
    assert_eq!(
        corpus.event_vocabulary.len(),
        corpus.event_payloads.len(),
        "every event name carries a payload record"
    );
    assert!(
        corpus.event_vocabulary.len() >= 26,
        "the reference raises 26 events; the corpus records {}",
        corpus.event_vocabulary.len()
    );
}

/// A divergence no ledger entry names is what the replay exists to fail on, and
/// a ledger entry whose divergence stopped reproducing is a decision that
/// outlived its cause. Both verdicts are computed by [`audit`], so they are
/// asserted here rather than only through the corpus.
#[test]
fn the_ledger_fails_an_unrecorded_divergence_and_a_stale_entry() {
    let ledger: &[(&str, &str)] = &[
        ("spans/name/agent-span", "recorded"),
        ("spans/attributeKeys/*", "recorded"),
    ];

    let mut report = Report::default();
    report.check("spans", "name", "agent-span", &1, &2);
    report.check("spans", "attributeKeys", "agent-span", &1, &2);
    let (unrecorded, stale) = audit(&report, "spans", ledger);
    assert!(unrecorded.is_empty(), "recorded divergences pass");
    assert!(stale.is_empty(), "entries that reproduced are not stale");

    let mut report = Report::default();
    report.check("spans", "statusCode", "agent-span", &1, &2);
    report.check("spans", "attributeKeys", "agent-span", &1, &2);
    let (unrecorded, stale) = audit(&report, "spans", ledger);
    assert_eq!(unrecorded.len(), 1, "the unnamed divergence is reported");
    assert!(
        unrecorded[0].starts_with("spans/statusCode/agent-span: reference"),
        "the failure names the family, the field, the case and both values: {unrecorded:?}"
    );
    assert_eq!(
        stale,
        vec!["spans/name/agent-span".to_owned()],
        "the entry whose divergence stopped reproducing is stale"
    );
}

#[test]
fn the_committed_corpus_replays_against_this_port() {
    let corpus = corpus();
    println!(
        "telemetry: divergence ledger ({} entries)",
        DIVERGENCES.len()
    );
    for (case, reason) in DIVERGENCES {
        println!("  {case}: {reason}");
    }
    let mut comparisons = 0;

    let mut report = Report::default();
    run_constants(&corpus.constants, &mut report);
    comparisons += settle(&report, "constants");

    let mut report = Report::default();
    run_envelope(&corpus, &mut report);
    comparisons += settle(&report, "envelope");

    let mut report = Report::default();
    run_base_metadata(&corpus, &mut report);
    comparisons += settle(&report, "baseMetadata");

    let mut report = Report::default();
    run_attachment_counts(&corpus, &mut report);
    comparisons += settle(&report, "attachmentCounts");

    let mut report = Report::default();
    run_event_vocabulary(&corpus, &mut report);
    comparisons += settle(&report, "eventVocabulary");

    let mut report = Report::default();
    run_event_payloads(&corpus, &mut report);
    comparisons += settle(&report, "eventPayloads");

    let mut report = Report::default();
    run_exporter_config(&corpus, &mut report);
    comparisons += settle(&report, "exporterConfig");

    let mut report = Report::default();
    run_spans(&corpus, &mut report);
    comparisons += settle(&report, "spans");

    let mut report = Report::default();
    run_provider_names(&corpus, &mut report);
    comparisons += settle(&report, "providerNames");

    let mut report = Report::default();
    run_redaction(&corpus, &mut report);
    comparisons += settle(&report, "redaction");

    for (family, run) in [
        ("logFormat", run_log_format as fn(&Corpus, &mut Report)),
        ("logEncoding", run_log_encoding),
        ("logParse", run_log_parse),
        ("logPagination", run_log_pagination),
        ("logConfig", run_log_config),
    ] {
        let mut report = Report::default();
        run(&corpus, &mut report);
        comparisons += settle(&report, family);
    }

    println!(
        "telemetry: {comparisons} comparisons across {} families replayed at {}",
        FAMILIES.len(),
        &corpus.reference.commit[..12],
    );
    assert!(
        comparisons >= MINIMUM_COMPARISONS,
        "the corpus replays {comparisons} comparisons, below the {MINIMUM_COMPARISONS} floor; \
         regenerate it with {CAPTURE_SCRIPT}"
    );
}

/// The corpus is only an oracle for as long as it still describes the pinned
/// reference. This probe recaptures it where the checkout is present and on the
/// pin, and skips everywhere else naming the pin and the way back.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "telemetry") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let repository = repo_root();
    let script = repository.join(CAPTURE_SCRIPT);
    let recaptured = repository.join("target/telemetry-corpus.json");
    let output = Command::new("python3")
        .arg(&script)
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(repository.join("target/telemetry-full.json"))
        .arg("--corpus")
        .arg(&recaptured)
        .current_dir(&repository)
        .output()
        .expect("the telemetry capture script runs");
    assert!(
        output.status.success(),
        "the telemetry capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let fresh = fs::read_to_string(&recaptured).expect("the recaptured corpus is readable");
    let committed =
        fs::read_to_string(repository.join(CORPUS_RELATIVE)).expect("the corpus is readable");
    let fresh: Value = serde_json::from_str(&fresh).expect("the recaptured corpus parses");
    let committed: Value = serde_json::from_str(&committed).expect("the corpus parses");
    assert_eq!(
        fresh, committed,
        "the pinned reference no longer answers what the committed corpus records; regenerate it \
         with `{CAPTURE_SCRIPT}`"
    );
}
