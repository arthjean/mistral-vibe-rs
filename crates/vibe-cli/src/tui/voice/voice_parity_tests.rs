//! Differential oracle for what an audio session is resolved from.
//!
//! `scripts/parity/voice.py` drives the reference's own schema, projection and
//! per-direction client over configuration documents the script authors, and
//! records the resolved model, the resolved provider, the failure cause and the
//! request each direction would put on the wire. This module replays that
//! corpus against this build: every document is written to a scratch
//! configuration, loaded through the layered store this port serves
//! `config/read` from, and each answer is compared field by field.
//!
//! Failures are compared by cause, never by message. The corpus records the
//! stage the reference failed at and the class of the exception it raised, and
//! [`cause`] maps that pair onto a vocabulary this port answers in too, because
//! `NOTICE` forbids committing the reference's own message text and a Rust
//! error would not carry it anyway.
//!
//! The wire families are measured at the seam each side actually builds a
//! request from: the corpus holds the URL the reference hands its websocket
//! opener and the request it hands its HTTP sender, and this port answers with
//! [`super::realtime::VoiceConfig`] and [`super::realtime::session_update`],
//! which is what a running session opens and sends. Where this build resolves
//! from something other than the configured provider, the ledger below names
//! the story that closes the gap rather than the replay passing quietly.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::Value;
use toml::Table;

use vibe_core::config::registry::default_document;
use vibe_core::config::{ConfigPaths, ConfigSnapshot, LayeredConfig};
use vibe_core::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};

use super::realtime::{
    DEFAULT_MODEL, DEFAULT_SAMPLE_RATE, DRAIN_TIMEOUT, TARGET_STREAMING_DELAY_MS, VoiceConfig,
    session_update,
};

const CORPUS_RELATIVE: &str = "crates/vibe-cli/tests/voice/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/voice.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 1;
/// The comparison floor this replay commits to, so a regeneration that
/// captured almost nothing fails instead of reporting a clean but empty run.
const MINIMUM_SCENARIOS: usize = 200;
/// The file the layered store reads a user document from.
const CONFIG_FILE: &str = "config.toml";
/// What `--api-base` defaults to, which is where this build takes the voice
/// endpoint from today instead of from the configured provider. The corpus
/// measures the gap; US-208 closes it.
const CLI_API_BASE_DEFAULT: &str = "https://api.mistral.ai/v1/chat/completions";

/// Every family the corpus declares and this replay reads. A family the
/// capture adds without a reader here fails the replay by name rather than
/// passing unread, and so does a family this replay expects and the corpus
/// dropped.
const FAMILIES: [&str; 4] = [
    "constants",
    "transcriptionResolution",
    "speechResolution",
    "wireFrames",
];

/// Keys the corpus carries that are not families: the pin, the layout, the
/// prose-free note and the documents every family is measured over.
const METADATA: [&str; 4] = ["schemaVersion", "reference", "note", "documents"];

/// Cases where this build answers something other than the reference, each with
/// the reason. A case that conforms while listed here fails the replay as a
/// stale entry, and a case that diverges without an entry fails naming the
/// family, the field, the case and the two values.
///
/// A key is `family/field/case`, and a trailing `*` covers every key that
/// starts with the prefix, which is how a divergence that reproduces across
/// every document is recorded once.
///
/// Most entries here are pending rather than accepted: this epic builds the
/// instrument, and EP-061 and EP-062 are what remove the rows. The staleness
/// check is what forces a row out once its story lands.
const DIVERGENCES: &[(&str, &str)] = &[
    (
        "constants/providerApiKeyEnvVarDefault/*",
        "ACCEPTED: the reference defaults a provider entry's `api_key_env_var` to the empty string \
         and fills it per shipped provider, so the field default and the shipped value differ; \
         this port materializes whole provider entries in its default document, and the defaults \
         view it answers with here is the shipped variable rather than the field default",
    ),
    (
        "constants/speechOutputFormats/*",
        "PENDING US-211 and US-214: this build has no speech transport and no audio model \
         choice, so it publishes no output-format vocabulary",
    ),
    (
        "constants/recordingModes/*",
        "ACCEPTED: the reference selects between a buffered and a streaming recorder, and this \
         port streams unconditionally, so it names no recording mode; \
         tasks/prd-autocompletion-voice-parity.md records the streaming-only surface",
    ),
    (
        "constants/playback/*",
        "PENDING US-212: this build has no audio player, so it names no block size, sample \
         format or sample width",
    ),
    (
        "transcriptionResolution/cause/*",
        "PENDING US-210: the reference raises on a document it cannot resolve, and this port's \
         load repairs and its view falls back to the first declared entry, so no cause is \
         reported",
    ),
    (
        "transcriptionResolution/configView/*",
        "PENDING US-210: a resolution failure takes the reference's whole `ConfigView` down, and \
         this port publishes a view for every document",
    ),
    (
        "speechResolution/cause/*",
        "PENDING US-210: as above, on the read-aloud surface",
    ),
    (
        "speechResolution/configView/*",
        "PENDING US-210: as above, on the read-aloud surface",
    ),
    (
        "wireFrames/transcription.endpointUrl/*",
        "PENDING US-208: this build takes the voice endpoint from `--api-base` and hard-codes the \
         model in its query, so a configured provider and a configured model name reach neither",
    ),
    (
        "wireFrames/transcription.model/*",
        "PENDING US-208: the model on the wire is a constant here, not the configured entry",
    ),
    (
        "wireFrames/transcription.sampleRate/*",
        "PENDING US-208: the session frame carries a constant sample rate, not the configured one",
    ),
    (
        "wireFrames/transcription.targetStreamingDelayMs/*",
        "PENDING US-208: the session frame carries a constant streaming delay, not the configured \
         one",
    ),
    (
        "wireFrames/transcription.credentialEnvVar/*",
        "PENDING US-209: this build presents the credential it was started with and never reads \
         the variable the provider names",
    ),
    (
        "wireFrames/transcription.serverUrl/*",
        "PENDING US-208: the server a session is opened against is the one `--api-base` names \
         here, so a configured provider's `api_base` reaches no frame",
    ),
    (
        "wireFrames/speech.*",
        "PENDING US-211 and US-213: this build has no speech transport, so it posts no request \
         at all",
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
    transcription_resolution: Vec<TranscriptionCase>,
    speech_resolution: Vec<SpeechCase>,
    wire_frames: Vec<FrameCase>,
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
    transcribe: SurfaceConstants,
    speech: SurfaceConstants,
    vocabularies: Vocabularies,
    transcription_drain_timeout_seconds: f64,
    playback: Playback,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SurfaceConstants {
    active_alias: String,
    model_name: String,
    provider_name: String,
    api_base: String,
    api_key_env_var: String,
    /// The per-entry defaults the reference fills a partial entry from. This
    /// port materializes them inside its shipped entry instead, which is what
    /// the comparison reads.
    model_field_defaults: Table,
    provider_field_defaults: Table,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Vocabularies {
    audio_client: Vec<String>,
    transcription_encoding: Vec<String>,
    speech_output_format: Vec<String>,
    recording_mode: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Playback {
    block_size: u32,
    dtype: String,
    sample_width: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TranscriptionCase {
    case: String,
    outcome: String,
    #[serde(default)]
    error: Option<Failure>,
    #[serde(default)]
    model: Option<TranscribeModel>,
    #[serde(default)]
    provider: Option<AudioProvider>,
    validation_warnings: usize,
    /// What projecting the whole `ConfigView` answered: `resolved`, the class
    /// of the exception it raised, or `unbuilt` when the document never became
    /// a configuration at all.
    config_view: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpeechCase {
    case: String,
    outcome: String,
    #[serde(default)]
    error: Option<Failure>,
    #[serde(default)]
    model: Option<SpeechModel>,
    #[serde(default)]
    provider: Option<AudioProvider>,
    validation_warnings: usize,
    config_view: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Failure {
    /// Where the reference gave up: building the configuration, resolving the
    /// active alias, or resolving that entry's provider.
    stage: String,
    /// The class it raised. Mapped to a cause rather than compared directly,
    /// because a Rust error carries neither the class nor the message.
    r#type: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TranscribeModel {
    name: String,
    sample_rate: i64,
    encoding: String,
    language: String,
    target_streaming_delay_ms: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpeechModel {
    name: String,
    voice: String,
    response_format: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AudioProvider {
    api_base: String,
    api_key_env_var: String,
    client: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FrameCase {
    case: String,
    #[serde(default)]
    transcription: Option<TranscriptionFrame>,
    #[serde(default)]
    speech: Option<SpeechFrame>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TranscriptionFrame {
    endpoint_url: String,
    header_names: Vec<String>,
    server_url: String,
    model: String,
    audio_format: AudioFormat,
    target_streaming_delay_ms: i64,
    credential: Credential,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AudioFormat {
    encoding: String,
    sample_rate: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpeechFrame {
    method: String,
    endpoint_url: String,
    body: Table,
    server_url: String,
    model: String,
    voice: String,
    response_format: String,
    credential: Credential,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Credential {
    env_var: String,
    /// `environment` when the variable the provider names carried the
    /// credential, `unset` when the provider names none. Never a value.
    #[expect(
        dead_code,
        reason = "the origin documents that the corpus holds no secret; the variable is what \
                  this port is compared on"
    )]
    source: String,
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
    let corpus: Corpus = serde_json::from_str(&raw).expect("the voice corpus parses");
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
///
/// A key is `family/field/case` rather than `family/case/field`, because a
/// divergence in this corpus reproduces across documents for one field, and
/// that ordering is what lets one ledger entry cover it.
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
}

/// What the ledger has to say about one family's report: the divergences it
/// does not name, and the entries whose divergence no longer reproduces. A
/// `prefix*` entry is stale once no observed divergence starts with its prefix.
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
        "voice: {family} {}/{} conform ({ledgered} ledgered)",
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
    snapshot: Option<ConfigSnapshot>,
}

impl Loaded {
    fn from_document(document: &str) -> Self {
        let enclosure = tempfile::tempdir().expect("temporary configuration root");
        let home = enclosure.path().join("home/.vibe");
        fs::create_dir_all(&home).expect("configuration home");
        if !document.trim().is_empty() {
            fs::write(home.join(CONFIG_FILE), document).expect("user document");
        }
        let snapshot = LayeredConfig::new(
            ConfigPaths {
                vibe_home: home,
                working_directory: enclosure.path().join("project"),
            },
            default_document(),
        )
        .load()
        .ok();
        Self {
            _enclosure: enclosure,
            snapshot,
        }
    }

    fn view(&self) -> Option<Value> {
        self.snapshot.as_ref().map(ConfigSnapshot::config_view)
    }
}

/// The cause vocabulary both sides answer in.
///
/// The reference names a stage and an exception class; this port either fails
/// the load or repairs the document and falls back. Mapping both onto the same
/// three words is what makes the comparison one of causes rather than one of
/// message text, which `NOTICE` forbids committing in the first place.
fn cause(failure: Option<&Failure>) -> &'static str {
    match failure.map(|failure| failure.stage.as_str()) {
        None => "resolved",
        Some("build") => "documentRejected",
        Some("model") => "activeAliasUnresolved",
        Some("provider") => "providerUnresolved",
        Some(_) => "unknownStage",
    }
}

/// The same vocabulary, read off this build. A load that fails rejected the
/// document; anything else resolved, because the view falls back rather than
/// refusing.
fn port_cause(loaded: &Loaded) -> &'static str {
    if loaded.snapshot.is_none() {
        "documentRejected"
    } else {
        "resolved"
    }
}

fn string_at(view: &Value, pointer: &str) -> String {
    view.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn integer_at(view: &Value, pointer: &str) -> i64 {
    view.pointer(pointer).and_then(Value::as_i64).unwrap_or(0)
}

/// The URL a session would open and the frame it would send, built the way a
/// running client builds them: from the endpoint this build was started with.
struct PortTranscriptionFrame {
    endpoint: String,
    model: String,
    encoding: String,
    sample_rate: i64,
    target_streaming_delay_ms: i64,
}

fn port_transcription_frame() -> PortTranscriptionFrame {
    let config = VoiceConfig::from_api_base(CLI_API_BASE_DEFAULT)
        .expect("the shipped API base builds a voice endpoint");
    let model = config
        .endpoint
        .query_pairs()
        .find(|(key, _)| key == "model")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default();
    let frame: Value = serde_json::from_str(&session_update(
        config.requested_sample_rate,
        config.target_streaming_delay_ms,
    ))
    .expect("the session frame is JSON");
    PortTranscriptionFrame {
        endpoint: config.endpoint.to_string(),
        model,
        encoding: string_at(&frame, "/session/audio_format/encoding"),
        sample_rate: integer_at(&frame, "/session/audio_format/sample_rate"),
        target_streaming_delay_ms: integer_at(&frame, "/session/target_streaming_delay_ms"),
    }
}

// --------------------------------------------------------------------------
// The families
// --------------------------------------------------------------------------

fn defaults_view() -> Value {
    Loaded::from_document("")
        .view()
        .expect("the shipped defaults load")
}

fn run_constants(constants: &Constants, report: &mut Report) {
    let view = defaults_view();
    let aliases = |key: &str| {
        view[key]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let surfaces = [
        (
            "transcribe",
            &constants.transcribe,
            "transcribeModels",
            "transcription",
        ),
        ("speech", &constants.speech, "ttsModels", "speech"),
    ];
    for (surface, declared, alias_key, view_key) in surfaces {
        report.check(
            "constants",
            "activeAlias",
            surface,
            &vec![declared.active_alias.clone()],
            &aliases(alias_key),
        );
        report.check(
            "constants",
            "modelName",
            surface,
            &declared.model_name,
            &string_at(&view, &format!("/{view_key}/model/name")),
        );
        report.check(
            "constants",
            "apiBase",
            surface,
            &declared.api_base,
            &string_at(&view, &format!("/{view_key}/provider/apiBase")),
        );
        report.check(
            "constants",
            "apiKeyEnvVar",
            surface,
            &declared.api_key_env_var,
            &string_at(&view, &format!("/{view_key}/provider/apiKeyEnvVar")),
        );
        // The reference names its shipped provider in the entry itself; this
        // port publishes the provider it resolved, so the name is compared
        // through the client the view declares.
        report.check(
            "constants",
            "providerClient",
            surface,
            &declared
                .provider_field_defaults
                .get("client")
                .and_then(toml::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            &string_at(&view, &format!("/{view_key}/provider/client")),
        );
        report.check(
            "constants",
            "providerApiBaseDefault",
            surface,
            &declared
                .provider_field_defaults
                .get("api_base")
                .and_then(toml::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            &string_at(&view, &format!("/{view_key}/provider/apiBase")),
        );
        // The reference defaults this field to the empty string and fills it
        // per shipped provider; this port has no per-field rule to answer with.
        report.check(
            "constants",
            "providerApiKeyEnvVarDefault",
            surface,
            &declared
                .provider_field_defaults
                .get("api_key_env_var")
                .and_then(toml::Value::as_str)
                .map(ToOwned::to_owned),
            &None,
        );
        assert!(
            !declared.provider_name.is_empty(),
            "the corpus names the shipped {surface} provider"
        );
    }

    let transcribe_defaults = &constants.transcribe.model_field_defaults;
    let integer_default = |key: &str| {
        transcribe_defaults
            .get(key)
            .and_then(toml::Value::as_integer)
            .unwrap_or_default()
    };
    let string_default = |table: &Table, key: &str| {
        table
            .get(key)
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    report.check(
        "constants",
        "modelSampleRateDefault",
        "transcribe",
        &integer_default("sample_rate"),
        &integer_at(&view, "/transcription/model/sampleRate"),
    );
    report.check(
        "constants",
        "modelEncodingDefault",
        "transcribe",
        &string_default(transcribe_defaults, "encoding"),
        &string_at(&view, "/transcription/model/encoding"),
    );
    report.check(
        "constants",
        "modelLanguageDefault",
        "transcribe",
        &string_default(transcribe_defaults, "language"),
        &string_at(&view, "/transcription/model/language"),
    );
    report.check(
        "constants",
        "modelStreamingDelayDefault",
        "transcribe",
        &integer_default("target_streaming_delay_ms"),
        &integer_at(&view, "/transcription/model/targetStreamingDelayMs"),
    );
    let speech_defaults = &constants.speech.model_field_defaults;
    report.check(
        "constants",
        "modelVoiceDefault",
        "speech",
        &string_default(speech_defaults, "voice"),
        &string_at(&view, "/speech/model/voice"),
    );
    report.check(
        "constants",
        "modelResponseFormatDefault",
        "speech",
        &string_default(speech_defaults, "response_format"),
        &string_at(&view, "/speech/model/responseFormat"),
    );

    // The vocabularies. Where this build serves one member of a closed set, the
    // member is what it answers with; where it serves none, it answers nothing
    // and the ledger names the story that changes that.
    report.check(
        "constants",
        "audioClient",
        "vocabulary",
        &constants.vocabularies.audio_client,
        &vec![string_at(&view, "/transcription/provider/client")],
    );
    let port_frame = port_transcription_frame();
    report.check(
        "constants",
        "transcriptionEncodings",
        "vocabulary",
        &constants.vocabularies.transcription_encoding,
        &vec![port_frame.encoding.clone()],
    );
    report.check(
        "constants",
        "speechOutputFormats",
        "vocabulary",
        &Some(constants.vocabularies.speech_output_format.clone()),
        &None,
    );
    report.check(
        "constants",
        "recordingModes",
        "vocabulary",
        &Some(constants.vocabularies.recording_mode.clone()),
        &None,
    );
    report.check(
        "constants",
        "drainTimeoutSeconds",
        "voice",
        &constants.transcription_drain_timeout_seconds,
        &DRAIN_TIMEOUT.as_secs_f64(),
    );
    report.check(
        "constants",
        "defaultModel",
        "voice",
        &constants.transcribe.model_name,
        &DEFAULT_MODEL.to_owned(),
    );
    report.check(
        "constants",
        "defaultSampleRate",
        "voice",
        &integer_default("sample_rate"),
        &i64::from(DEFAULT_SAMPLE_RATE),
    );
    report.check(
        "constants",
        "defaultStreamingDelay",
        "voice",
        &integer_default("target_streaming_delay_ms"),
        &i64::from(TARGET_STREAMING_DELAY_MS),
    );
    report.check(
        "constants",
        "playback",
        "blockSize",
        &Some(i64::from(constants.playback.block_size)),
        &None,
    );
    report.check(
        "constants",
        "playback",
        "dtype",
        &Some(constants.playback.dtype.clone()),
        &None,
    );
    report.check(
        "constants",
        "playback",
        "sampleWidth",
        &Some(i64::from(constants.playback.sample_width)),
        &None,
    );
}

fn documents(corpus: &Corpus) -> Vec<(&str, &str)> {
    corpus
        .documents
        .iter()
        .map(|document| (document.id.as_str(), document.toml.as_str()))
        .collect()
}

fn document<'a>(index: &[(&'a str, &'a str)], id: &str) -> &'a str {
    index
        .iter()
        .find(|(case, _)| *case == id)
        .map(|(_, toml)| *toml)
        .unwrap_or_else(|| panic!("the corpus declares the document `{id}`"))
}

fn run_transcription_resolution(corpus: &Corpus, report: &mut Report) {
    let index = documents(corpus);
    for case in &corpus.transcription_resolution {
        let loaded = Loaded::from_document(document(&index, &case.case));
        report.check(
            "transcriptionResolution",
            "cause",
            &case.case,
            &cause(case.error.as_ref()).to_owned(),
            &port_cause(&loaded).to_owned(),
        );
        let Some(view) = loaded.view() else {
            continue;
        };
        report.check(
            "transcriptionResolution",
            "configView",
            &case.case,
            &case.config_view,
            &"resolved".to_owned(),
        );
        report.check(
            "transcriptionResolution",
            "validationWarnings",
            &case.case,
            &case.validation_warnings,
            &view["validationWarnings"]
                .as_array()
                .map(Vec::len)
                .unwrap_or_default(),
        );
        let Some(model) = case.model.as_ref() else {
            continue;
        };
        report.check(
            "transcriptionResolution",
            "model.name",
            &case.case,
            &model.name,
            &string_at(&view, "/transcription/model/name"),
        );
        report.check(
            "transcriptionResolution",
            "model.sampleRate",
            &case.case,
            &model.sample_rate,
            &integer_at(&view, "/transcription/model/sampleRate"),
        );
        report.check(
            "transcriptionResolution",
            "model.encoding",
            &case.case,
            &model.encoding,
            &string_at(&view, "/transcription/model/encoding"),
        );
        report.check(
            "transcriptionResolution",
            "model.language",
            &case.case,
            &model.language,
            &string_at(&view, "/transcription/model/language"),
        );
        report.check(
            "transcriptionResolution",
            "model.targetStreamingDelayMs",
            &case.case,
            &model.target_streaming_delay_ms,
            &integer_at(&view, "/transcription/model/targetStreamingDelayMs"),
        );
        let Some(provider) = case.provider.as_ref() else {
            continue;
        };
        report.check(
            "transcriptionResolution",
            "provider.apiBase",
            &case.case,
            &provider.api_base,
            &string_at(&view, "/transcription/provider/apiBase"),
        );
        report.check(
            "transcriptionResolution",
            "provider.apiKeyEnvVar",
            &case.case,
            &provider.api_key_env_var,
            &string_at(&view, "/transcription/provider/apiKeyEnvVar"),
        );
        report.check(
            "transcriptionResolution",
            "provider.client",
            &case.case,
            &provider.client,
            &string_at(&view, "/transcription/provider/client"),
        );
    }
}

fn run_speech_resolution(corpus: &Corpus, report: &mut Report) {
    let index = documents(corpus);
    for case in &corpus.speech_resolution {
        let loaded = Loaded::from_document(document(&index, &case.case));
        report.check(
            "speechResolution",
            "cause",
            &case.case,
            &cause(case.error.as_ref()).to_owned(),
            &port_cause(&loaded).to_owned(),
        );
        let Some(view) = loaded.view() else {
            continue;
        };
        report.check(
            "speechResolution",
            "configView",
            &case.case,
            &case.config_view,
            &"resolved".to_owned(),
        );
        report.check(
            "speechResolution",
            "validationWarnings",
            &case.case,
            &case.validation_warnings,
            &view["validationWarnings"]
                .as_array()
                .map(Vec::len)
                .unwrap_or_default(),
        );
        let Some(model) = case.model.as_ref() else {
            continue;
        };
        report.check(
            "speechResolution",
            "model.name",
            &case.case,
            &model.name,
            &string_at(&view, "/speech/model/name"),
        );
        report.check(
            "speechResolution",
            "model.voice",
            &case.case,
            &model.voice,
            &string_at(&view, "/speech/model/voice"),
        );
        report.check(
            "speechResolution",
            "model.responseFormat",
            &case.case,
            &model.response_format,
            &string_at(&view, "/speech/model/responseFormat"),
        );
        let Some(provider) = case.provider.as_ref() else {
            continue;
        };
        report.check(
            "speechResolution",
            "provider.apiBase",
            &case.case,
            &provider.api_base,
            &string_at(&view, "/speech/provider/apiBase"),
        );
        report.check(
            "speechResolution",
            "provider.apiKeyEnvVar",
            &case.case,
            &provider.api_key_env_var,
            &string_at(&view, "/speech/provider/apiKeyEnvVar"),
        );
        report.check(
            "speechResolution",
            "provider.client",
            &case.case,
            &provider.client,
            &string_at(&view, "/speech/provider/client"),
        );
    }
}

fn run_wire_frames(corpus: &Corpus, report: &mut Report) {
    let index = documents(corpus);
    let port = port_transcription_frame();
    for case in &corpus.wire_frames {
        // A document this port cannot load leaves no session to open, so there
        // is no frame of its own to compare against the reference's.
        if Loaded::from_document(document(&index, &case.case))
            .view()
            .is_none()
        {
            continue;
        }
        if let Some(frame) = case.transcription.as_ref() {
            report.check(
                "wireFrames",
                "transcription.endpointUrl",
                &case.case,
                &frame.endpoint_url,
                &port.endpoint,
            );
            report.check(
                "wireFrames",
                "transcription.model",
                &case.case,
                &frame.model,
                &port.model,
            );
            report.check(
                "wireFrames",
                "transcription.encoding",
                &case.case,
                &frame.audio_format.encoding,
                &port.encoding,
            );
            report.check(
                "wireFrames",
                "transcription.sampleRate",
                &case.case,
                &frame.audio_format.sample_rate,
                &port.sample_rate,
            );
            report.check(
                "wireFrames",
                "transcription.targetStreamingDelayMs",
                &case.case,
                &frame.target_streaming_delay_ms,
                &port.target_streaming_delay_ms,
            );
            // The credential a session presents is the one the provider names.
            // This build presents whatever it was started with, so the compared
            // value is the variable the view publishes and nothing this build
            // reads.
            report.check(
                "wireFrames",
                "transcription.credentialEnvVar",
                &case.case,
                &frame.credential.env_var,
                &String::new(),
            );
            // The server the reference's client holds is the `api_base` of the
            // provider it resolved. This build opens its session against the
            // endpoint it was started with, so that is what answers here; the
            // provider it merely publishes in its view would report a
            // conformance no session has.
            report.check(
                "wireFrames",
                "transcription.serverUrl",
                &case.case,
                &frame.server_url,
                &CLI_API_BASE_DEFAULT.to_owned(),
            );
            assert!(
                frame
                    .header_names
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case("authorization")),
                "the reference authorizes its realtime request: {:?}",
                frame.header_names
            );
        }
        let Some(frame) = case.speech.as_ref() else {
            continue;
        };
        report.check(
            "wireFrames",
            "speech.method",
            &case.case,
            &Some(frame.method.clone()),
            &None,
        );
        report.check(
            "wireFrames",
            "speech.endpointUrl",
            &case.case,
            &Some(frame.endpoint_url.clone()),
            &None,
        );
        report.check(
            "wireFrames",
            "speech.model",
            &case.case,
            &Some(frame.model.clone()),
            &None,
        );
        report.check(
            "wireFrames",
            "speech.voice",
            &case.case,
            &Some(frame.voice.clone()),
            &None,
        );
        report.check(
            "wireFrames",
            "speech.responseFormat",
            &case.case,
            &Some(frame.response_format.clone()),
            &None,
        );
        report.check(
            "wireFrames",
            "speech.credentialEnvVar",
            &case.case,
            &Some(frame.credential.env_var.clone()),
            &None,
        );
        // The body the reference posts is what US-211 has to reproduce, so the
        // corpus is asserted to carry it rather than only its scalars.
        assert!(
            frame.body.contains_key("model")
                && frame.body.contains_key("input")
                && frame.body.contains_key("voice_id")
                && frame.body.contains_key("response_format"),
            "the speech body carries the four values a request is built from: {:?}",
            frame.body.keys().collect::<Vec<_>>()
        );
        assert!(
            !frame.server_url.is_empty(),
            "the speech frame names the server it would post to"
        );
    }
}

// --------------------------------------------------------------------------
// The replay
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

/// The corpus is a corpus of documents, and every scenario names one of them.
/// A capture that recorded a case with no document would compare this build
/// against nothing.
#[test]
fn every_scenario_names_a_declared_document() {
    let corpus = corpus();
    let index = documents(&corpus);
    let declared = index.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    assert!(
        declared.len() >= 20,
        "the corpus carries {} documents, below the 20 the story requires",
        declared.len()
    );
    for case in &corpus.transcription_resolution {
        assert!(declared.contains(&case.case.as_str()), "{}", case.case);
    }
    for case in &corpus.speech_resolution {
        assert!(declared.contains(&case.case.as_str()), "{}", case.case);
    }
    for case in &corpus.wire_frames {
        assert!(declared.contains(&case.case.as_str()), "{}", case.case);
    }
}

/// A failure is compared by cause, so the corpus has to carry causes the
/// vocabulary knows and at least one of each kind the reference can produce.
#[test]
fn every_recorded_failure_maps_to_a_known_cause() {
    let corpus = corpus();
    for (case, outcome, failed, resolved) in corpus
        .transcription_resolution
        .iter()
        .map(|case| {
            (
                &case.case,
                &case.outcome,
                case.error.is_some(),
                case.model.is_some(),
            )
        })
        .chain(corpus.speech_resolution.iter().map(|case| {
            (
                &case.case,
                &case.outcome,
                case.error.is_some(),
                case.model.is_some(),
            )
        }))
    {
        assert_eq!(
            outcome == "error",
            failed,
            "`{case}` records an outcome its failure does not match"
        );
        assert_eq!(
            outcome == "resolved",
            resolved,
            "`{case}` records an outcome its resolution does not match"
        );
    }
    let mut seen = Vec::new();
    for failure in corpus
        .transcription_resolution
        .iter()
        .filter_map(|case| case.error.as_ref())
        .chain(
            corpus
                .speech_resolution
                .iter()
                .filter_map(|case| case.error.as_ref()),
        )
    {
        let mapped = cause(Some(failure));
        assert_ne!(
            mapped, "unknownStage",
            "the corpus records the stage `{}`, which this replay cannot map",
            failure.stage
        );
        assert!(
            !failure.r#type.is_empty(),
            "a recorded failure names the class the reference raised"
        );
        if !seen.contains(&mapped) {
            seen.push(mapped);
        }
    }
    seen.sort_unstable();
    assert_eq!(
        seen,
        [
            "activeAliasUnresolved",
            "documentRejected",
            "providerUnresolved"
        ],
        "the corpus exercises every failure the reference's resolution can reach"
    );
}

/// A divergence no ledger entry names is what the replay exists to fail on, and
/// a ledger entry whose divergence stopped reproducing is a decision that
/// outlived its cause. Both verdicts are computed by [`audit`], so they are
/// asserted here rather than only through the corpus.
#[test]
fn the_ledger_fails_an_unrecorded_divergence_and_a_stale_entry() {
    let ledger: &[(&str, &str)] = &[
        ("wireFrames/speech.model/defaults", "recorded"),
        ("wireFrames/speech.voice/*", "recorded"),
    ];

    let mut report = Report::default();
    report.check("wireFrames", "speech.model", "defaults", &1, &2);
    report.check("wireFrames", "speech.voice", "defaults", &1, &2);
    let (unrecorded, stale) = audit(&report, "wireFrames", ledger);
    assert!(unrecorded.is_empty(), "recorded divergences pass");
    assert!(stale.is_empty(), "entries that reproduced are not stale");

    let mut report = Report::default();
    report.check("wireFrames", "speech.method", "defaults", &1, &2);
    report.check("wireFrames", "speech.voice", "defaults", &1, &2);
    let (unrecorded, stale) = audit(&report, "wireFrames", ledger);
    assert_eq!(unrecorded.len(), 1, "the unnamed divergence is reported");
    assert!(
        unrecorded[0].starts_with("wireFrames/speech.method/defaults: reference"),
        "the failure names the family, the field, the case and both values: {unrecorded:?}"
    );
    assert_eq!(
        stale,
        vec!["wireFrames/speech.model/defaults".to_owned()],
        "the entry whose divergence stopped reproducing is stale"
    );
}

#[test]
fn the_committed_corpus_replays_against_this_port() {
    let corpus = corpus();
    println!("voice: divergence ledger ({} entries)", DIVERGENCES.len());
    for (case, reason) in DIVERGENCES {
        println!("  {case}: {reason}");
    }
    let mut scenarios = 0;
    let mut report = Report::default();
    run_constants(&corpus.constants, &mut report);
    scenarios += settle(&report, "constants");
    let mut report = Report::default();
    run_transcription_resolution(&corpus, &mut report);
    scenarios += settle(&report, "transcriptionResolution");
    let mut report = Report::default();
    run_speech_resolution(&corpus, &mut report);
    scenarios += settle(&report, "speechResolution");
    let mut report = Report::default();
    run_wire_frames(&corpus, &mut report);
    scenarios += settle(&report, "wireFrames");
    println!(
        "voice: {scenarios} comparisons across {} families replayed at {}",
        FAMILIES.len(),
        &corpus.reference.commit[..12],
    );
    assert!(
        scenarios >= MINIMUM_SCENARIOS,
        "the corpus replays {scenarios} comparisons, below the {MINIMUM_SCENARIOS} floor; \
         regenerate it with {CAPTURE_SCRIPT}"
    );
}

/// The corpus is only an oracle for as long as it still describes the pinned
/// reference. This probe recaptures it where the checkout is present and on the
/// pin, and skips everywhere else naming the pin and the way back.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "voice") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let repository = repo_root();
    let script = repository.join(CAPTURE_SCRIPT);
    let recaptured = repository.join("target/voice-corpus.json");
    let output = Command::new("python3")
        .arg(&script)
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(repository.join("target/voice-full.json"))
        .arg("--corpus")
        .arg(&recaptured)
        .current_dir(&repository)
        .output()
        .expect("the voice capture script runs");
    assert!(
        output.status.success(),
        "the voice capture failed: {}",
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
