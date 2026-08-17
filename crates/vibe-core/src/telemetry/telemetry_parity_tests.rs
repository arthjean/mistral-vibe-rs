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

#![expect(
    deprecated,
    reason = "`opentelemetry-semantic-conventions` 0.32.1 deprecates the whole `gen_ai.*` family \
              for having moved to the GenAI semantic-conventions repository, without changing a \
              single key string"
)]
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Map, Value, json};

use opentelemetry_sdk::trace::SpanData;
use opentelemetry_semantic_conventions::attribute as semconv;

use crate::auth::UtcTimestamp;
use crate::compaction::{CompactionFailureReason, CompactionStatus};
use crate::config::registry::{FIELDS, default_document};
use crate::config::{ConfigPaths, LayeredConfig};
use crate::observability::{
    FileLog, LOG_BACKUP_COUNT, LOG_DEFAULT_MAX_BYTES, LOG_FIELD_ORDER, LOG_FIELD_SEPARATOR,
    LOG_LINE_PATTERN, LOG_LOGGER_NAME, LOG_POLL_INTERVAL_SECONDS, LOG_READ_CHUNK_SIZE,
    LOG_RELATIVE_FILE, LogInitError, LogLevel, LogReader, LogSettings, decode_log_message,
    encode_log_message, format_log_line, log_pattern_groups, parse_log_line, process_identifiers,
};
use crate::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};
use crate::tracing::{
    AGENT_NAME, AgentSpan, BackendFailure, HOOK_NAME_KEY, HOOK_TYPE_KEY, HookSpan,
    MISTRAL_OTEL_PATH, MISTRAL_PROVIDER_VALUE, ModelCallSpan, OPERATION_CHAT,
    OPERATION_EXECUTE_TOOL, OPERATION_INVOKE_AGENT, OtelExporterConfig, OtelRedactionMode,
    PROVIDER_API_STYLE_KEY, REQUEST_CALL_TYPE_KEY, REQUEST_MESSAGE_ID_KEY, REQUEST_STREAMING_KEY,
    TRACER_NAME, TRACES_EXPORT_PATH, ToolSpan, TracedError, agent_span, build_span_exporter_config,
    harness as tracing_harness, hook_span, model_call_span, otel_credential_variable,
    provider_attribute_value, redaction, set_model_call_http_status,
    set_model_call_response_metadata, set_model_call_usage, set_tool_result, setup_tracing,
    tool_span,
};

use super::{
    ExperimentExposures, LaunchContext, TELEMETRY_ATTACHMENT_IMAGE, TELEMETRY_AUTHORIZATION_SCHEME,
    TELEMETRY_CALL_SOURCE, TELEMETRY_DEFAULT_API_KEY_VARIABLE, TELEMETRY_DEFAULT_BASE_URL,
    TELEMETRY_MAX_CONNECTIONS, TELEMETRY_MAX_KEEPALIVE_CONNECTIONS, TELEMETRY_PATH,
    TELEMETRY_TIMEOUT_SECONDS, TelemetryCallType, TelemetryClient, TelemetryConfig,
    TelemetryContext, TelemetryEnvelope, TelemetryEvent, TelemetryFuture, TelemetryRecord,
    TelemetryTransport, attachment_counts, merge_properties, platform_id, platform_version,
    records, telemetry_headers, telemetry_user_agent,
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

/// What the capture writes in place of a clock and a pair of process
/// identifiers, so a line records its format rather than one run of it.
const TIMESTAMP_PLACEHOLDER: &str = "{timestamp}";
const PPID_PLACEHOLDER: &str = "{ppid}";
const PID_PLACEHOLDER: &str = "{pid}";
/// The exception text the `logFormat` case appends. The reference's traceback
/// is machine-dependent, so only its separator and the absence of a raw newline
/// are compared; this stands in for one.
const EXCEPTION_TEXT: &str = "RuntimeError: oracle failure\n  at the oracle";
/// The line number the capture hands `_parse_line`, which is the caller's to
/// decide rather than the parser's to find.
const LINE_NUMBER: i64 = 7;
/// The encoding the reference's handler is built with, and the only one this
/// port writes.
const ENCODING: &str = "utf-8";

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
        "baseMetadata/sentryTags/*",
        "ACCEPTED: the reference ships Sentry dormant, both DSNs null at the pin, so no crash \
         reporter exists here to tag; US-020 records the dormancy",
    ),
    // -- EP-003: closed. What the epic left standing ------------------------
    (
        "eventVocabulary/published/vibe.admin_config_applied",
        "ACCEPTED: the event reports on the org-managed configuration layer, which this port \
         neither fetches nor composes; declaring a name nothing can raise would be worse than \
         recording its absence, and US-020 records it",
    ),
    (
        "eventPayloads/propertyKeys/vibe.admin_config_applied",
        "ACCEPTED: as above",
    ),
    (
        "eventPayloads/propertyTypes/vibe.admin_config_applied",
        "ACCEPTED: as above",
    ),
    (
        "eventPayloads/correlated/vibe.admin_config_applied",
        "ACCEPTED: as above",
    ),
    // -- EP-005: closed. What the epic left standing ------------------------
    (
        "constants/logging/patternDigest.*",
        "ACCEPTED: the digest is of the reference's own regular expression, which `NOTICE` forbids \
         reproducing verbatim; `vibe_core::observability::LOG_LINE_PATTERN` is written for this \
         port and accepts the same language, which the whole `logParse` family measures line for \
         line",
    ),
    (
        "logPagination/messages/file-shrank-between-polls",
        "ACCEPTED: the case drives the reference's polling watcher, whose `set_consumer` and \
         `start_watching` have no caller outside `vibe/core/log_reader.py` at the pin, so nothing \
         upstream ever counts a line for a shrink to reset; this port's console pulls pages \
         through `diagnostics/logs/read` and reads backward from the end, where a truncated file \
         needs no reset",
    ),
    (
        "logPagination/lineNumbers/file-shrank-between-polls",
        "ACCEPTED: as above",
    ),
    (
        "logPagination/hasMore/file-shrank-between-polls",
        "ACCEPTED: as above",
    ),
    (
        "logPagination/cursor/file-shrank-between-polls",
        "ACCEPTED: as above",
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
    sentry: Value,
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
    property_types: Option<Value>,
    #[expect(
        dead_code,
        reason = "the values are the capture's own inputs; the key set and the types are the                   contract, and a value vocabulary the capture invented is not one"
    )]
    properties: Option<Value>,
    correlated: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExporterCase {
    case: String,
    endpoint_requested: String,
    configuration: Option<String>,
    resolved: Option<bool>,
    endpoint: Option<String>,
    header_names: Vec<String>,
    credential_variable: Option<String>,
    enable_telemetry: Option<bool>,
    enable_otel: Option<bool>,
    /// Absent on a resolution case and present on a setup one, which is what
    /// tells the two apart.
    provider_installed: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpanCase {
    case: String,
    name: Option<String>,
    attribute_keys: Vec<String>,
    attributes: Value,
    status_code: Option<String>,
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
    /// The set the case carried in, which is what the replay authors again.
    attribute_set: String,
    mode: String,
    span_name: String,
    surviving_keys: Vec<String>,
    unchanged_keys: Vec<String>,
    replaced_keys: Vec<String>,
    added_keys: Vec<String>,
    removed_keys: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LogFormatCase {
    case: String,
    level: String,
    message: String,
    line: String,
    has_exception: bool,
    exception_separator: Option<String>,
    exception_encoded_newlines: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LogEncodingCase {
    input: Option<String>,
    encoded: String,
    decoded: String,
    round_trips: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LogParseCase {
    case: String,
    line: String,
    parsed: bool,
    timestamp: Option<String>,
    ppid: Option<i64>,
    pid: Option<i64>,
    level: Option<String>,
    message: Option<String>,
    line_number: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LogPaginationCase {
    case: String,
    limit: u64,
    offset: u64,
    messages: Vec<String>,
    line_numbers: Vec<i64>,
    has_more: bool,
    cursor: Option<i64>,
    #[expect(
        dead_code,
        reason = "the counter a shrink resets belongs to the reference's polling watcher, which \
                  never starts at the pin and has no counterpart here; the ledger names the two \
                  fields that answer for it"
    )]
    new_lines_count_reset: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LogConfigCase {
    case: String,
    level: Option<String>,
    max_bytes: Option<i64>,
    backup_count: Option<u64>,
    encoding: Option<String>,
    #[expect(
        dead_code,
        reason = "the reference filters twice, once on the logger and once on the handler; this \
                  port filters where the record is written, so the level compared is the sink's"
    )]
    logger_level: Option<String>,
    directory_created: bool,
    handler_count: u64,
    duplicate_guarded: Option<bool>,
    #[expect(
        dead_code,
        reason = "the exception type is Python's; what it decides is compared through the fields \
                  it leaves unanswered"
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

/// The wire name each variant is measured under, restated here rather than read
/// from [`TelemetryEvent::event_name`], so a rename on one side fails the
/// replay instead of moving both the question and the answer.
///
/// The match carries no wildcard, which is what keeps the vocabulary
/// exhaustive: a variant added to [`TelemetryEvent`] stops this module
/// compiling until it is named here. Without that guard a new event would ship
/// with no corpus entry and
/// [`every_event_this_build_publishes_has_a_corpus_entry`] would pass quietly,
/// which is the one way this replay could overstate the score.
const fn declared_name(event: TelemetryEvent) -> &'static str {
    match event {
        TelemetryEvent::NewSession => "vibe.new_session",
        TelemetryEvent::SessionClosed => "vibe.session_closed",
        TelemetryEvent::Ready => "vibe.ready",
        TelemetryEvent::Startup => "vibe.startup",
        TelemetryEvent::RequestSent => "vibe.request_sent",
        TelemetryEvent::ToolCallFinished => "vibe.tool_call_finished",
        TelemetryEvent::AtMentionInserted => "vibe.at_mention_inserted",
        TelemetryEvent::AutoCompactTriggered => "vibe.auto_compact_triggered",
        TelemetryEvent::CompactionFailed => "vibe.compaction_failed",
        TelemetryEvent::SlashCommandUsed => "vibe.slash_command_used",
        TelemetryEvent::UserCopiedText => "vibe.user_copied_text",
        TelemetryEvent::UserCancelledAction => "vibe.user_cancelled_action",
        TelemetryEvent::VoiceModeToggled => "vibe.voice_mode_toggled",
        TelemetryEvent::OnboardingApiKeyAdded => "vibe.onboarding_api_key_added",
        TelemetryEvent::TeleportCompleted => "vibe.teleport_completed",
        TelemetryEvent::TeleportFailed => "vibe.teleport_failed",
        TelemetryEvent::FeedbackSubmitted => "vibe.user_rating_feedback",
        TelemetryEvent::RemoteProjectConfigured => "vibe.remote_project_configured",
        TelemetryEvent::TranscriptionStarted => "vibe.audio.transcription.start",
        TelemetryEvent::TranscriptionCancelled => "vibe.audio.transcription.cancel_recording",
        TelemetryEvent::TranscriptionDone => "vibe.audio.transcription.done",
        TelemetryEvent::TranscriptionFailed => "vibe.audio.transcription.error",
        TelemetryEvent::ReadAloudRequested => "vibe.read_aloud.requested",
        TelemetryEvent::ReadAloudPlayStarted => "vibe.read_aloud.play_started",
        TelemetryEvent::ReadAloudEnded => "vibe.read_aloud.ended",
    }
}

/// The event names this build publishes.
fn published_events() -> Vec<TelemetryEvent> {
    TelemetryEvent::ALL.to_vec()
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
        experiments: ExperimentExposures::default(),
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

/// Payload keys whose value is a clock reading or a generated identifier, which
/// the capture records as a type marker rather than a value. Named as
/// `VOLATILE_KEYS` in the capture names them.
const VOLATILE_KEYS: [&str; 5] = [
    "recording_duration_ms",
    "transcription_duration_ms",
    "time_to_first_read_s",
    "elapsed_seconds",
    "read_aloud_session_id",
];

/// The record this build raises for one reference event name, built with the
/// inputs the capture drove the reference's own sender with.
///
/// Every name the corpus carries is answered here except
/// `vibe.admin_config_applied`, which reports on the org-managed configuration
/// layer this port does not compose; the ledger records that.
fn port_record(event: &str) -> Option<TelemetryRecord> {
    let picker = || records::ProjectPicker {
        shown: true,
        selection_source: Some(records::ProjectSelectionSource::SelectedExisting),
        candidate_count_loaded: Some(3),
        multi_repo_match_count: Some(2),
        saved_project_link_cleared: Some(false),
        repo_remote_changed: Some(false),
    };
    Some(match event {
        "vibe.new_session" => TelemetryRecord::NewSession(records::NewSession {
            has_agents_md: true,
            nb_skills: 3,
            nb_mcp_servers: 2,
            nb_models: 4,
        }),
        "vibe.session_closed" => TelemetryRecord::SessionClosed,
        "vibe.ready" => TelemetryRecord::Ready {
            init_duration_ms: 1234,
        },
        "vibe.startup" => TelemetryRecord::Startup(records::Startup {
            first_frame_duration_ms: Some(12),
            agent_ready_duration_ms: Some(34),
            session_init_duration_ms: Some(56),
        }),
        "vibe.request_sent" => TelemetryRecord::RequestSent(records::RequestSent {
            model: "oracle-model".to_owned(),
            nb_context_chars: 2048,
            nb_context_messages: 6,
            nb_prompt_chars: 512,
            call_type: TelemetryCallType::MainCall,
            message_id: Some("oracle-message".to_owned()),
            attachment_counts: BTreeMap::new(),
        }),
        "vibe.tool_call_finished" => TelemetryRecord::ToolCallFinished(
            records::ToolCallFinished::new(records::ToolCallReport {
                tool_name: "write_file",
                status: records::TelemetryToolStatus::Success,
                arguments: &json!({
                    "file_path": "/oracle/workspace/file.rs",
                    "background": false,
                }),
                result: Some(&json!({"background": false})),
                decision: Some(records::ToolDecision {
                    verdict: records::TelemetryToolVerdict::Execute,
                    approval_type: records::TelemetryApprovalType::Ask,
                }),
                agent_profile_name: "oracle-profile",
                model: "oracle-model",
                message_id: Some("oracle-message".to_owned()),
            }),
        ),
        "vibe.at_mention_inserted" => {
            TelemetryRecord::AtMentionInserted(records::AtMentionInserted {
                nb_mentions: 2,
                context_types: BTreeMap::from([("file".to_owned(), 2)]),
                file_extensions: Some(BTreeMap::from([(".rs".to_owned(), 2)])),
                message_id: Some("oracle-message".to_owned()),
            })
        }
        "vibe.auto_compact_triggered" => TelemetryRecord::AutoCompactTriggered {
            nb_context_tokens_before: 150_000,
            auto_compact_threshold: 120_000,
            status: CompactionStatus::Success.label(),
        },
        "vibe.compaction_failed" => TelemetryRecord::CompactionFailed {
            reason: CompactionFailureReason::ToolCall.label(),
        },
        "vibe.slash_command_used" => TelemetryRecord::SlashCommandUsed {
            command: "/oracle".to_owned(),
            kind: records::TelemetryCommandKind::Builtin,
        },
        "vibe.user_copied_text" => TelemetryRecord::UserCopiedText { text_length: 11 },
        "vibe.user_cancelled_action" => TelemetryRecord::UserCancelledAction {
            action: "interrupt_agent".to_owned(),
        },
        "vibe.voice_mode_toggled" => TelemetryRecord::VoiceModeToggled { enabled: true },
        "vibe.onboarding_api_key_added" => TelemetryRecord::OnboardingApiKeyAdded {
            custom_domain: true,
        },
        "vibe.user_rating_feedback" => TelemetryRecord::FeedbackSubmitted {
            rating: 5,
            model: "oracle-model".to_owned(),
        },
        "vibe.teleport_completed" => {
            let mut tracker = records::TeleportTracker::new(
                12,
                records::TeleportFailureStage::Ineligible,
                Some(picker()),
            );
            tracker.record_progress(records::TeleportProgress::CheckingGit);
            tracker.record_progress(records::TeleportProgress::Pushing);
            tracker.record_context_summary_generated(480);
            tracker.completed()
        }
        "vibe.teleport_failed" => {
            let mut tracker = records::TeleportTracker::new(
                12,
                records::TeleportFailureStage::Ineligible,
                Some(records::ProjectPicker::hidden()),
            );
            tracker.record_progress(records::TeleportProgress::Pushing);
            tracker.record_context_summary_failed();
            tracker.record_progress(records::TeleportProgress::Pushing);
            tracker.record_service_error("OracleError", Some("http".to_owned()), Some(403));
            tracker.failed()?
        }
        "vibe.remote_project_configured" => TelemetryRecord::RemoteProjectConfigured {
            outcome: records::RemoteProjectOutcome::Configured,
            picker: records::ProjectPicker {
                shown: true,
                selection_source: Some(records::ProjectSelectionSource::CreatedProject),
                candidate_count_loaded: None,
                multi_repo_match_count: None,
                saved_project_link_cleared: None,
                repo_remote_changed: None,
            },
        },
        "vibe.audio.transcription.start" => TelemetryRecord::TranscriptionStarted {
            recording_id: "oracle-recording".to_owned(),
        },
        "vibe.audio.transcription.cancel_recording" => TelemetryRecord::TranscriptionCancelled {
            recording_id: "oracle-recording".to_owned(),
            recording_duration: std::time::Duration::from_millis(1500),
        },
        "vibe.audio.transcription.done" => TelemetryRecord::TranscriptionDone {
            recording_id: "oracle-recording".to_owned(),
            transcript_length: 17,
            transcription_duration: std::time::Duration::from_millis(1500),
            recording_duration: std::time::Duration::from_millis(1500),
        },
        "vibe.audio.transcription.error" => TelemetryRecord::TranscriptionFailed {
            recording_id: "oracle-recording".to_owned(),
            message: "oracle failure".to_owned(),
            transcription_duration: std::time::Duration::from_millis(1500),
            recording_duration: Some(std::time::Duration::from_millis(1500)),
        },
        "vibe.read_aloud.requested" => TelemetryRecord::ReadAloudRequested {
            read_aloud_session_id: "oracle-read-aloud".to_owned(),
            trigger: records::ReadAloudTrigger::AutoplayNextMessage,
        },
        "vibe.read_aloud.play_started" => TelemetryRecord::ReadAloudPlayStarted {
            read_aloud_session_id: "oracle-read-aloud".to_owned(),
            time_to_first_read: std::time::Duration::from_millis(250),
        },
        "vibe.read_aloud.ended" => TelemetryRecord::ReadAloudEnded {
            read_aloud_session_id: "oracle-read-aloud".to_owned(),
            status: records::ReadAloudStatus::Completed,
            error_type: None,
            elapsed: std::time::Duration::from_millis(900),
        },
        _ => return None,
    })
}

/// The properties one event of this build puts on the wire, measured the way
/// the capture measured the reference's: the census the envelope merges is
/// removed, so what is left is what the sender itself decided.
fn port_payload(event: &str) -> Option<Map<String, Value>> {
    let record = port_record(event)?;
    let context = oracle_context();
    let census = context.base_metadata(Some("oracle-session")).properties();
    let properties = record
        .attributes(context.launch.as_ref())
        .expect("every payload this port authors passes its own validators")
        .into_properties();
    Some(
        properties
            .into_iter()
            .filter(|(key, value)| census.get(key) != Some(value))
            .map(|(key, value)| {
                let value = if VOLATILE_KEYS.contains(&key.as_str()) {
                    Value::String(format!("{{{}}}", type_name(&value)))
                } else {
                    value
                };
                (key, value)
            })
            .collect(),
    )
}

fn port_payload_keys(event: &str) -> Option<Vec<String>> {
    Some(sorted_keys(&Value::Object(port_payload(event)?)))
}

/// The value types this build reports for one event, which is the half of a
/// payload contract a key set cannot carry.
fn port_payload_types(event: &str) -> Option<BTreeMap<String, String>> {
    Some(
        port_payload(event)?
            .into_iter()
            .map(|(key, value)| (key, type_name(&value)))
            .collect(),
    )
}

/// Whether this build correlates one event with the request it belongs to.
fn port_payload_correlated(event: &str) -> Option<bool> {
    Some(port_record(event)?.correlates_last_request())
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
    run_sentry_constants(&constants.sentry, report);
    run_vocabularies(&constants.vocabularies, report);
}

/// The reference ships its crash reporter dormant: both DSNs are `None` at the
/// pin, so `sentry_sdk.init(dsn=None)` never initializes and `init_sentry`
/// always answers `False` (`vibe/observability/sentry.py:15-16,177-209`). This
/// port initializes no crash reporter at all, which conforms exactly while the
/// reference stays dormant and stops conforming the moment it sets a DSN, which
/// is what makes the divergence `docs/parity.md` records a measured one rather
/// than a reading.
fn run_sentry_constants(sentry: &Value, report: &mut Report) {
    for (field, configured) in flatten(sentry) {
        report.check("constants", "sentry", &field, &configured, &json!(false));
    }
}

/// Every tracing constant, read off the module that declares it. Flattened
/// rather than enumerated field by field, so a key the reference adds arrives
/// here with no counterpart and fails by name instead of passing unread.
fn run_tracing_constants(tracing: &Value, report: &mut Report) {
    let declared = BTreeMap::from([
        ("agentName", AGENT_NAME),
        ("hookNameKey", HOOK_NAME_KEY),
        ("hookTypeKey", HOOK_TYPE_KEY),
        ("mistralOtelPath", MISTRAL_OTEL_PATH),
        ("mistralProviderValue", MISTRAL_PROVIDER_VALUE),
        ("operationNames.chat", OPERATION_CHAT),
        ("operationNames.executeTool", OPERATION_EXECUTE_TOOL),
        ("operationNames.invokeAgent", OPERATION_INVOKE_AGENT),
        ("providerApiStyleKey", PROVIDER_API_STYLE_KEY),
        ("requestCallTypeKey", REQUEST_CALL_TYPE_KEY),
        ("requestMessageIdKey", REQUEST_MESSAGE_ID_KEY),
        ("requestStreamingKey", REQUEST_STREAMING_KEY),
        ("semanticKeys.agentName", semconv::GEN_AI_AGENT_NAME),
        (
            "semanticKeys.conversationId",
            semconv::GEN_AI_CONVERSATION_ID,
        ),
        (
            "semanticKeys.httpRequestMethod",
            semconv::HTTP_REQUEST_METHOD,
        ),
        (
            "semanticKeys.httpResponseStatusCode",
            semconv::HTTP_RESPONSE_STATUS_CODE,
        ),
        ("semanticKeys.httpUrl", semconv::HTTP_URL),
        ("semanticKeys.operationName", semconv::GEN_AI_OPERATION_NAME),
        ("semanticKeys.providerName", semconv::GEN_AI_PROVIDER_NAME),
        (
            "semanticKeys.requestMaxTokens",
            semconv::GEN_AI_REQUEST_MAX_TOKENS,
        ),
        ("semanticKeys.requestModel", semconv::GEN_AI_REQUEST_MODEL),
        (
            "semanticKeys.requestTemperature",
            semconv::GEN_AI_REQUEST_TEMPERATURE,
        ),
        (
            "semanticKeys.responseFinishReasons",
            semconv::GEN_AI_RESPONSE_FINISH_REASONS,
        ),
        ("semanticKeys.responseId", semconv::GEN_AI_RESPONSE_ID),
        ("semanticKeys.responseModel", semconv::GEN_AI_RESPONSE_MODEL),
        (
            "semanticKeys.toolCallArguments",
            semconv::GEN_AI_TOOL_CALL_ARGUMENTS,
        ),
        ("semanticKeys.toolCallId", semconv::GEN_AI_TOOL_CALL_ID),
        (
            "semanticKeys.toolCallResult",
            semconv::GEN_AI_TOOL_CALL_RESULT,
        ),
        ("semanticKeys.toolName", semconv::GEN_AI_TOOL_NAME),
        ("semanticKeys.toolType", semconv::GEN_AI_TOOL_TYPE),
        (
            "semanticKeys.usageInputTokens",
            semconv::GEN_AI_USAGE_INPUT_TOKENS,
        ),
        (
            "semanticKeys.usageOutputTokens",
            semconv::GEN_AI_USAGE_OUTPUT_TOKENS,
        ),
        ("tracerName", TRACER_NAME),
        ("tracesExportPath", TRACES_EXPORT_PATH),
    ]);
    for (field, value) in flatten(tracing) {
        report.check(
            "constants",
            "tracing",
            &field,
            &value,
            &declared
                .get(field.as_str())
                .map_or(Value::Null, |declared| json!(declared)),
        );
    }
}

fn run_logging_constants(logging: &Value, report: &mut Report) {
    let declared = json!({
        "backupCount": LOG_BACKUP_COUNT,
        "defaultLevel": LogLevel::DEFAULT.as_str(),
        "defaultMaxBytes": LOG_DEFAULT_MAX_BYTES,
        "fieldOrder": LOG_FIELD_ORDER,
        "fieldSeparator": LOG_FIELD_SEPARATOR,
        "levels": LogLevel::ALL.map(LogLevel::as_str),
        "loggerName": LOG_LOGGER_NAME,
        "patternDigest": digest(LOG_LINE_PATTERN),
        "patternGroups": log_pattern_groups(),
        "pollIntervalSeconds": LOG_POLL_INTERVAL_SECONDS,
        "readChunkSize": LOG_READ_CHUNK_SIZE,
        "relativeLogFile": LOG_RELATIVE_FILE,
    });
    let declared = flatten(&declared).into_iter().collect::<BTreeMap<_, _>>();
    for (field, value) in flatten(logging) {
        report.check(
            "constants",
            "logging",
            &field,
            &value,
            declared.get(&field).unwrap_or(&Value::Null),
        );
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
        experiments: BTreeMap::from([("ab".to_owned(), "on".to_owned())]).into(),
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
    report.check(
        "constants",
        "teleportFailureStages",
        case,
        &vocabularies.teleport_failure_stages,
        &labels(&records::TeleportFailureStage::ALL, |stage| stage.label()),
    );
    report.check(
        "constants",
        "teleportContextSummaryStatuses",
        case,
        &vocabularies.teleport_context_summary_statuses,
        &labels(&records::TeleportContextSummaryStatus::ALL, |status| {
            status.label()
        }),
    );
    report.check(
        "constants",
        "projectSelectionSources",
        case,
        &vocabularies.project_selection_sources,
        &labels(&records::ProjectSelectionSource::ALL, |source| {
            source.label()
        }),
    );
    report.check(
        "constants",
        "remoteProjectOutcomes",
        case,
        &vocabularies.remote_project_outcomes,
        &labels(&records::RemoteProjectOutcome::ALL, |outcome| {
            outcome.label()
        }),
    );
}

/// The wire values a closed vocabulary publishes, in its declaration order,
/// which is the order the capture records it in.
fn labels<T: Copy>(variants: &[T], label: impl Fn(T) -> &'static str) -> Vec<String> {
    variants
        .iter()
        .map(|variant| label(*variant).to_owned())
        .collect()
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
            .into()
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
                experiments: ExperimentExposures::default(),
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
        // The two call-site events are read from the reference's source rather
        // than driven, so the capture recorded no types for them and there is
        // nothing to compare beyond the key set.
        if let Some(types) = entry.property_types.as_ref() {
            report.check_absent(
                "eventPayloads",
                "propertyTypes",
                &entry.event,
                &declared_types(types),
                port_payload_types(&entry.event).as_ref(),
            );
            report.check_absent(
                "eventPayloads",
                "correlated",
                &entry.event,
                &entry.correlated.unwrap_or_default(),
                port_payload_correlated(&entry.event).as_ref(),
            );
        }
    }
}

/// The captured type map, as the map this build is compared against.
fn declared_types(types: &Value) -> BTreeMap<String, String> {
    types
        .as_object()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The three tracing keys a setup case writes over its document, the way the
/// capture writes them: the document's own `enable_telemetry` line is dropped
/// and the case's own three keys are written above what is left.
fn setup_document(document: &str, telemetry: bool, otel: bool, endpoint: &str) -> toml::Table {
    let body = document
        .lines()
        .filter(|line| !line.starts_with("enable_telemetry"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "enable_telemetry = {telemetry}\nenable_otel = {otel}\notel_endpoint = \"{endpoint}\"\n\
         {body}"
    )
    .parse()
    .expect("the setup document parses")
}

/// The exporter resolution and the two-key gate, read where a starting binary
/// reads them: [`build_span_exporter_config`] for the endpoint and the header
/// set, and [`setup_tracing`] for whether a provider ends up installed.
fn run_exporter_config(corpus: &Corpus, report: &mut Report) {
    let documents = corpus
        .documents
        .iter()
        .map(|document| (document.id.as_str(), document.toml.as_str()))
        .collect::<BTreeMap<_, _>>();
    for entry in &corpus.exporter_config {
        let case = entry.case.as_str();
        let document = entry
            .configuration
            .as_deref()
            .map(|configuration| {
                *documents
                    .get(configuration)
                    .unwrap_or_else(|| panic!("`{case}` names a declared document"))
            })
            .unwrap_or_default();
        if entry.provider_installed.is_some() {
            let table = setup_document(
                document,
                entry.enable_telemetry.unwrap_or(true),
                entry.enable_otel.unwrap_or_default(),
                &entry.endpoint_requested,
            );
            let setup = setup_tracing(&table, &oracle_credentials);
            let installed = setup.is_installed();
            drop(setup);
            tracing_harness::uninstall();
            report.check(
                "exporterConfig",
                "providerInstalled",
                case,
                &entry.provider_installed,
                &Some(installed),
            );
            report.check("exporterConfig", "resolved", case, &entry.resolved, &None);
            report.check("exporterConfig", "endpoint", case, &entry.endpoint, &None);
            report.check(
                "exporterConfig",
                "headerNames",
                case,
                &entry.header_names,
                &Vec::new(),
            );
            report.check(
                "exporterConfig",
                "credentialVariable",
                case,
                &entry.credential_variable,
                &None,
            );
            continue;
        }
        let table = document
            .parse::<toml::Table>()
            .unwrap_or_else(|error| panic!("`{case}` parses: {error}"));
        let resolved =
            build_span_exporter_config(&entry.endpoint_requested, &table, &oracle_credentials);
        report.check(
            "exporterConfig",
            "resolved",
            case,
            &entry.resolved,
            &Some(resolved.is_some()),
        );
        report.check(
            "exporterConfig",
            "endpoint",
            case,
            &entry.endpoint,
            &resolved.as_ref().map(|config| config.endpoint.clone()),
        );
        report.check(
            "exporterConfig",
            "headerNames",
            case,
            &entry.header_names,
            &resolved
                .as_ref()
                .map(OtelExporterConfig::header_names)
                .unwrap_or_default(),
        );
        report.check(
            "exporterConfig",
            "credentialVariable",
            case,
            &entry.credential_variable,
            &resolved
                .as_ref()
                .filter(|config| !config.headers.is_empty())
                .map(|_| otel_credential_variable(&table)),
        );
        report.check(
            "exporterConfig",
            "providerInstalled",
            case,
            &entry.provider_installed,
            &None,
        );
    }
}

/// One span as this build produced it, in the shape the capture recorded the
/// reference's.
#[derive(Debug, Default)]
struct PortSpan {
    name: Option<String>,
    attribute_keys: Vec<String>,
    attributes: Value,
    status_code: Option<String>,
    status_description: Option<Value>,
    recorded_exceptions: u64,
    recording: bool,
}

impl PortSpan {
    /// A span the collector received, which is a span that was recorded.
    fn exported(span: &SpanData) -> Self {
        let mut attributes = Map::new();
        for attribute in &span.attributes {
            attributes.insert(
                attribute.key.as_str().to_owned(),
                attribute_value(&attribute.value),
            );
        }
        let (status_code, status_description) = match &span.status {
            opentelemetry::trace::Status::Ok => (Some("OK".to_owned()), None),
            opentelemetry::trace::Status::Unset => (Some("UNSET".to_owned()), None),
            opentelemetry::trace::Status::Error { description } => {
                (Some("ERROR".to_owned()), Some(digest(description)))
            }
        };
        Self {
            name: Some(span.name.to_string()),
            attribute_keys: attributes.keys().cloned().collect(),
            attributes: Value::Object(attributes),
            status_code,
            status_description,
            recorded_exceptions: span.events.events.len() as u64,
            recording: true,
        }
    }

    /// A span no provider recorded, which is what a build with tracing off
    /// produces and the reference's `INVALID_SPAN` answers.
    fn unrecorded() -> Self {
        Self {
            attributes: Value::Object(Map::new()),
            ..Self::default()
        }
    }
}

/// An attribute value as the corpus records it. The reference records a tuple as
/// a list, so an array lands as one here too.
fn attribute_value(value: &opentelemetry::Value) -> Value {
    match value {
        opentelemetry::Value::Bool(value) => json!(value),
        opentelemetry::Value::I64(value) => json!(value),
        opentelemetry::Value::F64(value) => json!(value),
        opentelemetry::Value::String(value) => json!(value.as_str()),
        opentelemetry::Value::Array(opentelemetry::Array::Bool(items)) => json!(items),
        opentelemetry::Value::Array(opentelemetry::Array::I64(items)) => json!(items),
        opentelemetry::Value::Array(opentelemetry::Array::F64(items)) => json!(items),
        opentelemetry::Value::Array(opentelemetry::Array::String(items)) => json!(
            items
                .iter()
                .map(opentelemetry::StringValue::as_str)
                .collect::<Vec<_>>()
        ),
        _ => Value::Null,
    }
}

/// A message recorded by its length and its SHA-256, never by its content, the
/// way the capture records every reference-authored string.
fn digest(value: &str) -> Value {
    use sha2::Digest as _;

    let mut hasher = sha2::Sha256::new();
    hasher.update(value.as_bytes());
    json!({
        "length": value.chars().count(),
        "digest": crate::text::hex_encode(&hasher.finalize()),
    })
}

/// A failure with no backend behind it. The class name is what the reference's
/// model-call status quotes, so the fixture is named after the exception the
/// capture raised.
#[derive(Debug)]
struct OracleRuntimeError {
    message: &'static str,
    backend: Option<BackendFailure>,
}

impl std::fmt::Display for OracleRuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl TracedError for OracleRuntimeError {
    fn error_type(&self) -> &'static str {
        "RuntimeError"
    }

    fn backend_failure(&self) -> Option<BackendFailure> {
        self.backend.clone()
    }
}

/// The backend failure the capture raised, whose provider and status are what
/// the status description quotes.
#[derive(Debug)]
struct OracleBackendError {
    status: Option<i64>,
}

impl std::fmt::Display for OracleBackendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("oracle reason")
    }
}

impl TracedError for OracleBackendError {
    fn error_type(&self) -> &'static str {
        "BackendError"
    }

    fn backend_failure(&self) -> Option<BackendFailure> {
        Some(BackendFailure {
            provider: Some("mistral".to_owned()),
            status: self.status,
        })
    }
}

type OracleResult = Result<(), OracleRuntimeError>;

/// The scripted span run the capture drove, opened here through the same four
/// families. Every case drains the collector, so the spans one case exported are
/// compared against the entries the corpus records for it, in the order they
/// ended: a child before the parent it ran inside.
async fn port_spans() -> Vec<(String, PortSpan)> {
    let harness = tracing_harness::Harness::install();
    let mut produced: Vec<(String, PortSpan)> = Vec::new();
    let mut drain = |case: &str, harness: &tracing_harness::Harness| {
        for span in harness.drain() {
            produced.push((case.to_owned(), PortSpan::exported(&span)));
        }
    };

    let agent = AgentSpan {
        model: Some("oracle-model"),
        session_id: Some("oracle-session"),
    };
    let tool = ToolSpan {
        tool_name: "read_file",
        call_id: "oracle-call",
        arguments: r#"{"path":"a.rs"}"#,
    };
    let model_call = ModelCallSpan {
        provider_name: "mistral",
        provider_api_style: "openai",
        model: "oracle-model",
        ..ModelCallSpan::default()
    };

    let _: OracleResult = agent_span(agent, async { Ok(()) }).await;
    drain("agent-span", &harness);

    let _: OracleResult = agent_span(AgentSpan::default(), async { Ok(()) }).await;
    drain("agent-span-without-model-or-session", &harness);

    let _: OracleResult = agent_span(agent, async {
        tool_span(tool, async {
            set_tool_result("oracle result");
            Ok(())
        })
        .await
    })
    .await;
    drain("tool-span-inside-an-agent-span", &harness);

    let _: OracleResult = tool_span(tool, async { Ok(()) }).await;
    drain("tool-span-without-a-parent", &harness);

    let _: OracleResult = agent_span(agent, async {
        model_call_span(
            ModelCallSpan {
                streaming: true,
                temperature: Some(0.3),
                max_tokens: Some(512),
                call_type: Some("main_call"),
                message_id: Some("oracle-message"),
                http_url: Some("https://api.mistral.ai/v1/chat/completions"),
                ..model_call
            },
            async {
                set_model_call_http_status(200);
                set_model_call_usage(120, 34);
                set_model_call_response_metadata(&json!({
                    "id": "oracle-response",
                    "model": "oracle-response-model",
                    "choices": [{"finish_reason": "stop"}],
                }));
                Ok(())
            },
        )
        .await
    })
    .await;
    drain("model-call-span-inside-an-agent-span", &harness);

    let _: OracleResult = model_call_span(
        ModelCallSpan {
            provider_name: "third-party",
            session_id: Some("oracle-metadata-session"),
            ..model_call
        },
        async { Ok(()) },
    )
    .await;
    drain("model-call-span-from-metadata-session", &harness);

    let _: OracleResult = hook_span(
        HookSpan {
            hook_name: "oracle-hook",
            hook_type: "pre_tool_use",
            tool_name: Some("read_file"),
            tool_call_id: Some("oracle-call"),
        },
        async { Ok(()) },
    )
    .await;
    drain("hook-span", &harness);

    let _: OracleResult = agent_span(agent, async {
        hook_span(
            HookSpan {
                hook_name: "oracle-hook",
                hook_type: "stop",
                tool_name: None,
                tool_call_id: None,
            },
            async { Ok(()) },
        )
        .await
    })
    .await;
    drain("hook-span-inside-an-agent-span", &harness);

    for (case, status) in [
        ("model-call-span-raising-a-backend-error", Some(503)),
        (
            "model-call-span-raising-a-backend-error-without-a-status",
            None,
        ),
    ] {
        let _: Result<(), OracleBackendError> =
            model_call_span(model_call, async { Err(OracleBackendError { status }) }).await;
        drain(case, &harness);
    }

    let _: OracleResult = model_call_span(model_call, async {
        Err(OracleRuntimeError {
            message: "oracle wrapper",
            backend: Some(BackendFailure {
                provider: Some("mistral".to_owned()),
                status: Some(429),
            }),
        })
    })
    .await;
    drain("model-call-span-wrapping-a-backend-error", &harness);

    let _: OracleResult = model_call_span(model_call, async {
        Err(OracleRuntimeError {
            message: "oracle unhandled failure",
            backend: None,
        })
    })
    .await;
    drain("model-call-span-raising-an-unhandled-exception", &harness);

    let _: OracleResult = tool_span(
        ToolSpan {
            arguments: "{}",
            ..tool
        },
        async {
            Err(OracleRuntimeError {
                message: "oracle unhandled failure",
                backend: None,
            })
        },
    )
    .await;
    drain("tool-span-raising-an-unhandled-exception", &harness);

    drop(harness);
    // The never-raise policy, measured where no provider is installed at all:
    // the body still runs and the span it runs under is not recording.
    let recording: Result<bool, OracleRuntimeError> = agent_span(
        AgentSpan {
            model: Some("oracle-model"),
            session_id: None,
        },
        async {
            Ok(
                opentelemetry::trace::TraceContextExt::span(&opentelemetry::Context::current())
                    .is_recording(),
            )
        },
    )
    .await;
    produced.push((
        "agent-span-without-a-provider".to_owned(),
        PortSpan {
            recording: recording.unwrap_or(true),
            ..PortSpan::unrecorded()
        },
    ));
    produced
}

fn run_spans(corpus: &Corpus, report: &mut Report, produced: &[(String, PortSpan)]) {
    for (index, entry) in corpus.spans.iter().enumerate() {
        let case = entry.case.as_str();
        let port = produced.get(index).filter(|(name, _)| name == case);
        assert!(
            port.is_some(),
            "the replay produced no span for `{case}` at position {index}; the scripted run and \
             the corpus disagree on what this build opens"
        );
        let port = port.map(|(_, span)| span);
        report.check(
            "spans",
            "name",
            case,
            &entry.name,
            &port.and_then(|span| span.name.clone()),
        );
        report.check(
            "spans",
            "attributeKeys",
            case,
            &entry.attribute_keys,
            &port
                .map(|span| span.attribute_keys.clone())
                .unwrap_or_default(),
        );
        report.check(
            "spans",
            "attributes",
            case,
            &entry.attributes,
            &port
                .map(|span| span.attributes.clone())
                .unwrap_or(Value::Null),
        );
        report.check(
            "spans",
            "statusCode",
            case,
            &entry.status_code,
            &port.and_then(|span| span.status_code.clone()),
        );
        report.check(
            "spans",
            "statusDescription",
            case,
            &entry.status_description,
            &port.and_then(|span| span.status_description.clone()),
        );
        report.check(
            "spans",
            "recordedExceptions",
            case,
            &entry.recorded_exceptions,
            &port
                .map(|span| span.recorded_exceptions)
                .unwrap_or_default(),
        );
        report.check(
            "spans",
            "recording",
            case,
            &entry.recording,
            &port.is_some_and(|span| span.recording),
        );
    }
    assert_eq!(
        produced.len(),
        corpus.spans.len(),
        "this build opened {} spans and the corpus records {}",
        produced.len(),
        corpus.spans.len()
    );
}

fn run_provider_names(corpus: &Corpus, report: &mut Report) {
    for entry in &corpus.provider_names {
        let case = entry.input.as_deref().unwrap_or("<none>");
        report.check(
            "providerNames",
            "normalized",
            case,
            &entry.normalized,
            &provider_attribute_value(entry.input.as_deref()),
        );
    }
}

/// The two attribute sets the capture carried into each mode. Every value is
/// authored here, so what is compared is which key kept its value and which one
/// lost it, never a credential.
fn redaction_set(name: &str) -> Vec<opentelemetry::KeyValue> {
    match name {
        "content-attributes" => vec![
            opentelemetry::KeyValue::new("gen_ai.operation.name", "chat"),
            opentelemetry::KeyValue::new("gen_ai.provider.name", "mistral_ai"),
            opentelemetry::KeyValue::new("gen_ai.request.model", "oracle-model"),
            opentelemetry::KeyValue::new("gen_ai.conversation.id", "oracle-session"),
            opentelemetry::KeyValue::new("gen_ai.tool.name", "read_file"),
            opentelemetry::KeyValue::new(
                "gen_ai.tool.call.arguments",
                r#"{"file_path": "/oracle/workspace/file.rs"}"#,
            ),
            opentelemetry::KeyValue::new("gen_ai.tool.call.result", "oracle tool output"),
            opentelemetry::KeyValue::new("gen_ai.usage.input_tokens", 120_i64),
            opentelemetry::KeyValue::new("gen_ai.usage.output_tokens", 34_i64),
            opentelemetry::KeyValue::new(
                "gen_ai.response.finish_reasons",
                opentelemetry::Value::Array(vec![opentelemetry::StringValue::from("stop")].into()),
            ),
            opentelemetry::KeyValue::new("http.request.method", "POST"),
            opentelemetry::KeyValue::new("http.response.status_code", 200_i64),
            opentelemetry::KeyValue::new("vibe.provider.api_style", "openai"),
            opentelemetry::KeyValue::new("vibe.request.call_type", "main_call"),
        ],
        _ => vec![
            opentelemetry::KeyValue::new("gen_ai.request.model", "oracle-model"),
            opentelemetry::KeyValue::new(
                "gen_ai.tool.call.arguments",
                "sk-oracleoracleoracleoracle",
            ),
            opentelemetry::KeyValue::new(
                "vibe.provider.api_style",
                "Bearer oracleoracleoracleoracle",
            ),
            opentelemetry::KeyValue::new("http.url", "https://api.mistral.ai/v1/chat/completions"),
        ],
    }
}

/// What one redaction case exported: the keys that survived, the ones whose
/// value is unchanged, and the four sets the capture derives from them.
struct Redacted {
    span_name: String,
    surviving: Vec<String>,
    unchanged: Vec<String>,
    replaced: Vec<String>,
    added: Vec<String>,
    removed: Vec<String>,
}

fn port_redaction(mode: &str, set: &str) -> Redacted {
    let declared = redaction_set(set);
    let policy = redaction::RedactionPolicy::for_mode(OtelRedactionMode::parse(mode));
    let harness = policy.map_or_else(
        tracing_harness::Harness::install,
        tracing_harness::Harness::install_redacting,
    );
    harness.record("chat oracle-model", declared.clone());
    let exported = harness.drain();
    drop(harness);
    let span = exported
        .first()
        .expect("the redaction case exported one span");
    let mut surviving = Vec::new();
    let mut unchanged = Vec::new();
    for attribute in &span.attributes {
        let key = attribute.key.as_str().to_owned();
        if declared
            .iter()
            .any(|entry| entry.key == attribute.key && entry.value == attribute.value)
        {
            unchanged.push(key.clone());
        }
        surviving.push(key);
    }
    surviving.sort_unstable();
    unchanged.sort_unstable();
    let declared_keys = declared
        .iter()
        .map(|entry| entry.key.as_str().to_owned())
        .collect::<Vec<_>>();
    let replaced = surviving
        .iter()
        .filter(|key| !unchanged.contains(key))
        .cloned()
        .collect();
    let added = surviving
        .iter()
        .filter(|key| !declared_keys.contains(key))
        .cloned()
        .collect();
    let mut removed = declared_keys
        .iter()
        .filter(|key| !surviving.contains(key))
        .cloned()
        .collect::<Vec<_>>();
    removed.sort_unstable();
    Redacted {
        span_name: span.name.to_string(),
        surviving,
        unchanged,
        replaced,
        added,
        removed,
    }
}

fn run_redaction(corpus: &Corpus, report: &mut Report) {
    for entry in &corpus.redaction {
        let case = entry.case.as_str();
        let port = port_redaction(&entry.mode, &entry.attribute_set);
        report.check(
            "redaction",
            "spanName",
            case,
            &entry.span_name,
            &port.span_name,
        );
        report.check(
            "redaction",
            "survivingKeys",
            case,
            &entry.surviving_keys,
            &port.surviving,
        );
        report.check(
            "redaction",
            "unchangedKeys",
            case,
            &entry.unchanged_keys,
            &port.unchanged,
        );
        report.check(
            "redaction",
            "replacedKeys",
            case,
            &entry.replaced_keys,
            &port.replaced,
        );
        report.check(
            "redaction",
            "addedKeys",
            case,
            &entry.added_keys,
            &port.added,
        );
        report.check(
            "redaction",
            "removedKeys",
            case,
            &entry.removed_keys,
            &port.removed,
        );
    }
}

/// The line this build writes for one record, with the clock and the process
/// identifiers replaced the way the capture replaces them, so what is compared
/// is the format rather than one run of it.
fn port_log_line(level: LogLevel, message: &str, exception: Option<&str>) -> String {
    let (ppid, pid) = process_identifiers();
    let line = format_log_line(UtcTimestamp::now(), ppid, pid, level, message, exception);
    let mut fields = line.splitn(5, ' ');
    let timestamp = fields.next().unwrap_or_default();
    assert!(
        UtcTimestamp::parse_iso8601(timestamp).is_some(),
        "the line opens with a timestamp: {line}"
    );
    assert_eq!(
        (
            fields.next().unwrap_or_default(),
            fields.next().unwrap_or_default()
        ),
        (ppid.to_string().as_str(), pid.to_string().as_str()),
        "the line carries this process rather than a placeholder"
    );
    format!(
        "{TIMESTAMP_PLACEHOLDER} {PPID_PLACEHOLDER} {PID_PLACEHOLDER} {} {}",
        fields.next().unwrap_or_default(),
        fields.next().unwrap_or_default()
    )
}

fn run_log_format(corpus: &Corpus, report: &mut Report) {
    for entry in &corpus.log_format {
        let level = LogLevel::parse(&entry.level).unwrap_or_default();
        let exception = entry.has_exception.then_some(EXCEPTION_TEXT);
        // The traceback is machine-dependent, so the capture recorded the line
        // up to the message and the shape of what follows it. The same line
        // written without one is that head.
        let head = port_log_line(level, &entry.message, None);
        let tail = port_log_line(level, &entry.message, exception)
            .strip_prefix(head.as_str())
            .unwrap_or_default()
            .to_owned();
        report.check("logFormat", "line", &entry.case, &entry.line, &head);
        report.check(
            "logFormat",
            "hasException",
            &entry.case,
            &entry.has_exception,
            &!tail.is_empty(),
        );
        if let Some(separator) = entry.exception_separator.as_deref() {
            report.check(
                "logFormat",
                "exceptionSeparator",
                &entry.case,
                &separator.to_owned(),
                &tail.get(..1).unwrap_or_default().to_owned(),
            );
        }
        if let Some(encoded) = entry.exception_encoded_newlines {
            report.check(
                "logFormat",
                "exceptionEncodedNewlines",
                &entry.case,
                &encoded,
                &!tail.contains('\n'),
            );
        }
    }
}

fn run_log_encoding(corpus: &Corpus, report: &mut Report) {
    for (index, entry) in corpus.log_encoding.iter().enumerate() {
        let case = format!("message-{index}");
        // A record the capture encoded is compared in both directions; one it
        // only decoded carries no input, and the encoded text is the input.
        if let Some(input) = entry.input.as_deref() {
            report.check(
                "logEncoding",
                "encoded",
                &case,
                &entry.encoded,
                &encode_log_message(input),
            );
        }
        report.check(
            "logEncoding",
            "decoded",
            &case,
            &entry.decoded,
            &decode_log_message(&entry.encoded),
        );
        if let Some(round_trips) = entry.round_trips {
            report.check(
                "logEncoding",
                "roundTrips",
                &case,
                &round_trips,
                &(entry
                    .input
                    .as_deref()
                    .map(|input| decode_log_message(&encode_log_message(input)))
                    .as_deref()
                    == entry.input.as_deref()),
            );
        }
    }
}

fn run_log_parse(corpus: &Corpus, report: &mut Report) {
    for entry in &corpus.log_parse {
        let case = entry.case.as_str();
        let parsed = parse_log_line(&entry.line, LINE_NUMBER);
        report.check("logParse", "parsed", case, &entry.parsed, &parsed.is_some());
        report.check(
            "logParse",
            "level",
            case,
            &entry.level,
            &parsed.as_ref().map(|entry| entry.level.clone()),
        );
        report.check(
            "logParse",
            "message",
            case,
            &entry.message,
            &parsed.as_ref().map(|entry| entry.message.clone()),
        );
        report.check(
            "logParse",
            "timestamp",
            case,
            &entry.timestamp,
            &parsed.as_ref().map(|entry| entry.timestamp.clone()),
        );
        report.check(
            "logParse",
            "ppid",
            case,
            &entry.ppid,
            &parsed.as_ref().map(|entry| entry.ppid),
        );
        report.check(
            "logParse",
            "pid",
            case,
            &entry.pid,
            &parsed.as_ref().map(|entry| entry.pid),
        );
        report.check(
            "logParse",
            "lineNumber",
            case,
            &entry.line_number,
            &parsed.as_ref().map(|entry| entry.line_number),
        );
    }
}

/// The file every pagination case is read from, authored the way the capture
/// authors it: twelve records, a line no pattern matches before the fourth from
/// the end, and a blank one before the ninth.
fn pagination_file(enclosure: &Path) -> PathBuf {
    let mut lines = Vec::new();
    for index in 0..12 {
        match index {
            3 => lines.push("oracle wrote this by hand".to_owned()),
            8 => lines.push(String::new()),
            _ => {}
        }
        lines.push(format!(
            "2026-02-21T10:28:{index:02}.100000+00:00 12 34 WARNING oracle entry {index}"
        ));
    }
    let path = enclosure.join("vibe.log");
    fs::write(&path, format!("{}\n", lines.join("\n"))).expect("the pagination file");
    path
}

fn run_log_pagination(corpus: &Corpus, report: &mut Report) {
    let enclosure = tempfile::tempdir().expect("a pagination enclosure");
    let populated = pagination_file(enclosure.path());
    for entry in &corpus.log_pagination {
        let case = entry.case.as_str();
        // The shrink case drives the reference's polling watcher, which never
        // starts at the pin and has no counterpart here; the ledger names it.
        let page = match case {
            "file-shrank-between-polls" => None,
            "file-absent" => Some(
                LogReader::new(enclosure.path().join("absent.log")).get_logs(
                    usize::try_from(entry.limit).unwrap_or(0),
                    usize::try_from(entry.offset).unwrap_or(0),
                ),
            ),
            "file-empty" => {
                let empty = enclosure.path().join("empty.log");
                fs::write(&empty, "").expect("the empty file");
                Some(LogReader::new(empty).get_logs(
                    usize::try_from(entry.limit).unwrap_or(0),
                    usize::try_from(entry.offset).unwrap_or(0),
                ))
            }
            _ => Some(LogReader::new(&populated).get_logs(
                usize::try_from(entry.limit).unwrap_or(0),
                usize::try_from(entry.offset).unwrap_or(0),
            )),
        };
        report.check_absent(
            "logPagination",
            "messages",
            case,
            &entry.messages,
            page.as_ref()
                .map(|page| {
                    page.entries
                        .iter()
                        .map(|entry| entry.message.clone())
                        .collect::<Vec<_>>()
                })
                .as_ref(),
        );
        report.check_absent(
            "logPagination",
            "lineNumbers",
            case,
            &entry.line_numbers,
            page.as_ref()
                .map(|page| {
                    page.entries
                        .iter()
                        .map(|entry| entry.line_number)
                        .collect::<Vec<_>>()
                })
                .as_ref(),
        );
        report.check_absent(
            "logPagination",
            "hasMore",
            case,
            &entry.has_more,
            page.as_ref().map(|page| &page.has_more),
        );
        report.check_absent(
            "logPagination",
            "cursor",
            case,
            &entry.cursor,
            page.as_ref().map(|page| &page.cursor),
        );
    }
}

/// The environment each `logConfig` case resolves under, named the way the
/// capture names it.
fn log_config_environment(case: &str) -> BTreeMap<String, String> {
    let pairs: &[(&str, &str)] = match case {
        "level-debug" => &[("LOG_LEVEL", "DEBUG")],
        "level-lowercase" => &[("LOG_LEVEL", "info")],
        "level-unknown" => &[("LOG_LEVEL", "TRACE")],
        "level-empty" => &[("LOG_LEVEL", "")],
        "debug-mode-true" => &[("LOG_LEVEL", "ERROR"), ("DEBUG_MODE", "true")],
        "debug-mode-other" => &[("LOG_LEVEL", "ERROR"), ("DEBUG_MODE", "1")],
        "max-bytes" => &[("LOG_MAX_BYTES", "4096")],
        "max-bytes-zero" => &[("LOG_MAX_BYTES", "0")],
        "max-bytes-invalid" => &[("LOG_MAX_BYTES", "not-a-number")],
        _ => &[],
    };
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

fn run_log_config(corpus: &Corpus, report: &mut Report) {
    let enclosure = tempfile::tempdir().expect("a configuration enclosure");
    for (index, entry) in corpus.log_config.iter().enumerate() {
        let case = entry.case.as_str();
        let variables = log_config_environment(case);
        let read = |name: &str| variables.get(name).cloned();
        let path = enclosure
            .path()
            .join(index.to_string())
            .join("logs")
            .join("vibe.log");
        // The directory is made before the ceiling is read, so the refused case
        // still leaves it behind, which is what the capture recorded.
        let installed = install_once(&path, &read);
        let settings = installed.as_ref().ok().map(FileLog::settings);
        report.check(
            "logConfig",
            "level",
            case,
            &entry.level,
            &settings.map(|settings| settings.level.as_str().to_owned()),
        );
        report.check(
            "logConfig",
            "maxBytes",
            case,
            &entry.max_bytes,
            &settings.map(|settings| settings.max_bytes),
        );
        report.check(
            "logConfig",
            "backupCount",
            case,
            &entry.backup_count,
            &settings.map(|_| LOG_BACKUP_COUNT),
        );
        report.check(
            "logConfig",
            "handlerCount",
            case,
            &entry.handler_count,
            &u64::from(installed.is_ok()),
        );
        report.check(
            "logConfig",
            "directoryCreated",
            case,
            &entry.directory_created,
            &path.parent().is_some_and(Path::is_dir),
        );
        report.check(
            "logConfig",
            "encoding",
            case,
            &entry.encoding,
            &settings.map(|_| ENCODING.to_owned()),
        );
        // The reference attaches one handler and returns on the second call.
        // This port installs one file per process, so the second resolution
        // answers the first one's settings rather than a second sink.
        report.check(
            "logConfig",
            "duplicateGuarded",
            case,
            &entry.duplicate_guarded,
            &installed
                .as_ref()
                .ok()
                .map(|first| install_once(&path, &read).is_ok_and(|second| &second == first)),
        );
    }
}

/// What [`init_file_logging`] would install for one path, resolved without
/// touching the process-wide installation the binaries own.
///
/// The global is a `OnceLock`, so a replay driving eleven cases through it
/// would measure the first one eleven times. The steps are the same ones in the
/// same order: the directory, then the settings, then a file that opens.
fn install_once(
    path: &Path,
    read: &dyn Fn(&str) -> Option<String>,
) -> Result<FileLog, LogInitError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| LogInitError::Directory {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let settings = LogSettings::resolve(read)?;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| LogInitError::Open {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(FileLog::new(path, settings))
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
    // The tracer provider is process-global, so the three families that install
    // one hold the tracing lock for the whole replay rather than racing the
    // module's own tests for it.
    let _exclusive = tracing_harness::exclusive();
    let produced = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for the span families")
        .block_on(port_spans());
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
    run_spans(&corpus, &mut report, &produced);
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
