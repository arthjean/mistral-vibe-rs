//! Differential conformance runner for the chat-input parity corpus.
//!
//! Canonical traces recorded from the pinned Python reference are replayed
//! through the Rust transition boundary. `tests/parity/expectations.json`
//! declares, per trace and per dimension, how Rust stands against the
//! reference today:
//!
//! - `parity`: Rust must match; a divergence fails.
//! - `gap`: Rust is known to diverge; matching now also fails, so closing a
//!   gap cannot go unrecorded.
//! - `deferred`: the oracle records the dimension but no Rust observation
//!   exists yet. The runner does not compare it and names the story that will.
//! - `unavailable`: the scenario could not be recorded on this host.
//!
//! Every dimension a trace carries must be declared. A recorded dimension with
//! no entry fails the corpus check instead of being silently dropped.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value, json};
use vibe_cli::tui::attachments::{PromptDraft, normalize_pasted_text, prepare_submission};
use vibe_cli::tui::chat_input::{ChatInputState, InputEffect, InputEvent};
use vibe_cli::tui::commands::CommandContext;
use vibe_cli::tui::completion::CompletionRequest;

const SCHEMA_VERSION: u32 = 1;
const MAX_EFFECT_ROUNDS: usize = 8;
/// Recorded in the corpus but not compared yet, with the story that closes it.
const DEFERRED_DIMENSIONS: &[(&str, &str)] = &[(
    "render",
    "US-018 closes non-composer viewport rendering after EP-002",
)];
/// Placeholder the oracle substitutes for the recording workspace path.
const WORKSPACE_PLACEHOLDER: &str = "__WORKSPACE__";
/// Fields the reference records inside `state` that Rust does not model yet.
///
/// They are dropped from the expected observation before comparison so a
/// single unmodelled field cannot mask every other state assertion in the
/// corpus. Each entry names the story that supplies the missing state; the
/// entry is removed when that story lands, turning the field into a real
/// assertion.
const UNMODELLED_STATE_PATHS: &[(&str, &str)] = &[];
const OBSERVABLE_EFFECTS: &[&str] = &[
    "submitRequested",
    "submit",
    "modeChanged",
    "historyPrevious",
    "historyNext",
    "historyReset",
    "completionReset",
    "clipboardImageRequested",
    "feedbackRating",
    "feedbackSnooze",
    "feedbackDismissed",
    "notify",
    "recordingStartRequested",
    "recordingStopRequested",
    "recordingCancelRequested",
];
const TRACE_KEYS: &[&str] = &[
    "id",
    "gap",
    "story",
    "title",
    "capabilities",
    "setup",
    "initial",
    "events",
    "observations",
    "schemaVersion",
    "reference",
];
const DIMENSIONS: &[&str] = &["state", "effects", "render", "history", "submission"];
const STATUSES: &[&str] = &["parity", "gap", "deferred", "unavailable"];

// ---------------------------------------------------------------------------
// Corpus model
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    reference: Reference,
    workspaces: BTreeMap<String, BTreeMap<String, String>>,
    gaps: Vec<Gap>,
    traces: Vec<ManifestTrace>,
    unavailable: Vec<UnavailableTrace>,
}

#[derive(Debug, Deserialize)]
struct Reference {
    commit: String,
    version: String,
}

#[derive(Debug, Deserialize)]
struct Gap {
    id: String,
}

#[derive(Debug, Deserialize)]
struct ManifestTrace {
    id: String,
    gap: String,
    file: String,
}

#[derive(Debug, Deserialize)]
struct UnavailableTrace {
    id: String,
    gap: String,
    #[serde(rename = "missingCapabilities")]
    missing_capabilities: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Expectations {
    version: u32,
    traces: BTreeMap<String, BTreeMap<String, String>>,
}

fn parity_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("parity")
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let contents =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    serde_json::from_str(&contents).map_err(|error| format!("{}: {error}", path.display()))
}

fn load_manifest() -> Result<Manifest, String> {
    let manifest: Manifest = read_json(&parity_directory().join("manifest.json"))?;
    if manifest.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "corpus schema version {} is not supported by this runner (expected {SCHEMA_VERSION})",
            manifest.schema_version
        ));
    }
    Ok(manifest)
}

fn load_expectations() -> Result<Expectations, String> {
    let expectations: Expectations = read_json(&parity_directory().join("expectations.json"))?;
    if expectations.version != SCHEMA_VERSION {
        return Err(format!(
            "expectations version {} is not supported (expected {SCHEMA_VERSION})",
            expectations.version
        ));
    }
    Ok(expectations)
}

/// Dimensions a trace actually records, across all of its observations.
fn recorded_dimensions(observations: &[Value]) -> BTreeSet<String> {
    let mut recorded = BTreeSet::new();
    for observation in observations {
        let Some(map) = observation.as_object() else {
            continue;
        };
        for dimension in DIMENSIONS {
            if map.contains_key(*dimension) {
                recorded.insert((*dimension).to_owned());
            }
        }
    }
    recorded
}

fn trace_observations(trace: &Map<String, Value>) -> Vec<Value> {
    trace
        .get("observations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Corpus integrity
// ---------------------------------------------------------------------------

#[test]
fn corpus_declares_every_gap_trace_and_expectation() -> Result<(), String> {
    let manifest = load_manifest()?;
    let expectations = load_expectations()?;
    let directory = parity_directory();

    if manifest.reference.commit.len() != 40 {
        return Err(format!(
            "reference commit `{}` is not a full git revision",
            manifest.reference.commit
        ));
    }
    if manifest.reference.version.is_empty() {
        return Err("reference version is missing from the manifest".to_owned());
    }

    let declared = manifest
        .gaps
        .iter()
        .map(|gap| gap.id.clone())
        .collect::<BTreeSet<_>>();
    let covered = manifest
        .traces
        .iter()
        .map(|trace| trace.gap.clone())
        .chain(manifest.unavailable.iter().map(|trace| trace.gap.clone()))
        .collect::<BTreeSet<_>>();
    let uncovered = declared.difference(&covered).collect::<Vec<_>>();
    if !uncovered.is_empty() {
        return Err(format!("gaps without a canonical trace: {uncovered:?}"));
    }
    let unknown = covered.difference(&declared).collect::<Vec<_>>();
    if !unknown.is_empty() {
        return Err(format!("traces referencing unknown gaps: {unknown:?}"));
    }

    let mut expected_ids = expectations.traces.keys().cloned().collect::<BTreeSet<_>>();
    for trace in &manifest.traces {
        let Some(dimensions) = expectations.traces.get(&trace.id) else {
            return Err(format!(
                "trace `{}` has no entry in expectations.json; a trace is never assumed to pass",
                trace.id
            ));
        };
        expected_ids.remove(&trace.id);
        for (dimension, value) in dimensions {
            if !DIMENSIONS.contains(&dimension.as_str()) {
                return Err(format!(
                    "trace `{}` declares an unknown dimension `{dimension}`",
                    trace.id
                ));
            }
            if !STATUSES.contains(&value.as_str()) {
                return Err(format!(
                    "trace `{}` declares an unsupported expectation `{value}` for `{dimension}`",
                    trace.id
                ));
            }
        }

        // A dimension the oracle recorded must have a declared status. Without
        // this, a recorded observation could be dropped without anyone noticing.
        let raw: Value = read_json(&directory.join(&trace.file))?;
        let object = raw
            .as_object()
            .ok_or_else(|| format!("trace `{}` is not a JSON object", trace.id))?;
        for dimension in recorded_dimensions(&trace_observations(object)) {
            if !dimensions.contains_key(&dimension) {
                return Err(format!(
                    "trace `{}` records observations for `{dimension}` but expectations.json does \
                     not declare it; declare `parity`, `gap` or `deferred` instead of dropping it",
                    trace.id
                ));
            }
        }
    }
    for trace in &manifest.unavailable {
        let Some(dimensions) = expectations.traces.get(&trace.id) else {
            return Err(format!(
                "unavailable trace `{}` has no entry in expectations.json",
                trace.id
            ));
        };
        expected_ids.remove(&trace.id);
        if dimensions.get("state").map(String::as_str) != Some("unavailable") {
            return Err(format!(
                "trace `{}` is unavailable ({:?}) and must be declared as `unavailable`",
                trace.id, trace.missing_capabilities
            ));
        }
    }
    if !expected_ids.is_empty() {
        return Err(format!(
            "expectations.json declares traces missing from the manifest: {expected_ids:?}"
        ));
    }
    if !manifest.workspaces.contains_key("empty") {
        return Err("the manifest must describe the `empty` workspace fixture".to_owned());
    }
    Ok(())
}

/// Every deferred dimension names the story that will start comparing it.
#[test]
fn deferred_dimensions_name_their_story() -> Result<(), String> {
    let expectations = load_expectations()?;
    let deferrable = DEFERRED_DIMENSIONS
        .iter()
        .map(|(dimension, _)| *dimension)
        .collect::<BTreeSet<_>>();
    for (id, dimensions) in &expectations.traces {
        for (dimension, status) in dimensions {
            if status == "deferred" && !deferrable.contains(dimension.as_str()) {
                return Err(format!(
                    "trace `{id}` defers `{dimension}`, which is not a known deferred dimension; \
                     add it to DEFERRED_DIMENSIONS with the story that closes it"
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

struct Divergence {
    dimension: &'static str,
    event_index: Option<usize>,
    path: String,
    expected: Value,
    actual: Value,
}

impl Divergence {
    fn describe(&self, trace: &str, commit: &str) -> String {
        let position = match self.event_index {
            Some(index) => format!("event {index}"),
            None => "initial state".to_owned(),
        };
        format!(
            "trace `{trace}` diverges at {position} on {}{}\n  expected: {}\n  actual:   {}\n  fixture revision: {commit}",
            self.dimension, self.path, self.expected, self.actual
        )
    }
}

struct Replay {
    state: ChatInputState,
    workspace: PathBuf,
}

struct StepObservation {
    effects: Vec<Value>,
    submission: Option<Value>,
}

impl Replay {
    fn new(workspace: PathBuf, setup: &Value) -> Result<Self, String> {
        let skills = setup
            .get("skills")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|entry| {
                        Some((
                            entry.get("command")?.as_str()?,
                            entry.get("description")?.as_str()?,
                        ))
                    })
                    .map(|(command, description)| (command.to_owned(), description.to_owned()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut state = ChatInputState::new();
        if let Some(width) = setup.pointer("/viewport/width").and_then(Value::as_u64)
            && let Ok(width) = u16::try_from(width)
        {
            state.set_viewport_width(width);
        }
        let vibe_code_enabled = setup
            .pointer("/commands/vibeCodeEnabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let excluded = setup
            .pointer("/commands/excluded")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        state.set_command_context(
            CommandContext::new(vibe_code_enabled).with_excluded(excluded.iter().copied()),
        );
        state.set_user_skills(
            skills
                .iter()
                .map(|(command, description)| (command.as_str(), description.as_str())),
        );
        let history = setup_history(setup);
        if !history.is_empty() {
            let contents = history
                .iter()
                .filter_map(|entry| serde_json::to_string(entry).ok())
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(workspace.join("vibehistory"), contents)
                .map_err(|error| format!("history workspace fixture: {error}"))?;
        }
        state.replace_history(history);
        Ok(Self { state, workspace })
    }

    /// Applies one trace event and settles every effect it triggers.
    fn step(&mut self, event: InputEvent) -> Result<StepObservation, String> {
        let mut observable = Vec::new();
        let mut submitted = None;
        let mut pending = self.state.apply(event);
        let mut rounds = 0usize;
        while !pending.is_empty() {
            rounds = rounds.saturating_add(1);
            if rounds > MAX_EFFECT_ROUNDS {
                return Err(format!(
                    "effects did not settle after {MAX_EFFECT_ROUNDS} rounds; {} remain",
                    pending.len()
                ));
            }
            let mut follow_up = Vec::new();
            for effect in std::mem::take(&mut pending) {
                if let InputEffect::Submit { text } = &effect {
                    submitted = Some(text.clone());
                }
                let encoded = serde_json::to_value(&effect)
                    .map_err(|error| format!("effect does not serialize: {error}"))?;
                if encoded
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| OBSERVABLE_EFFECTS.contains(&kind))
                {
                    observable.push(encoded);
                }
                match effect {
                    InputEffect::RequestCompletion { request } => {
                        follow_up.push(self.resolve_completion(request));
                    }
                    InputEffect::NormalizePastedPath { text, snapshot } => {
                        let normalized = normalize_pasted_text(&text);
                        if normalized != text {
                            follow_up.push(InputEvent::PasteNormalized {
                                snapshot,
                                text: normalized,
                            });
                        }
                    }
                    InputEffect::NormalizeCurrentText { snapshot } => {
                        let normalized = normalize_pasted_text(&snapshot.text);
                        if normalized != snapshot.text {
                            follow_up.push(InputEvent::TextNormalized {
                                snapshot,
                                text: normalized,
                            });
                        }
                    }
                    _ => {}
                }
            }
            for event in follow_up {
                pending.extend(self.state.apply(event));
            }
        }
        let submission = submitted
            .map(|text| {
                prepare_submission(
                    &self.workspace,
                    &PromptDraft::text_only(text),
                    "oracle-model",
                    true,
                )
                .map_err(|error| format!("submission preparation failed: {error}"))
                .and_then(|prepared| {
                    serde_json::to_value(prepared.turn)
                        .map_err(|error| format!("submission does not serialize: {error}"))
                })
            })
            .transpose()?;
        Ok(StepObservation {
            effects: observable,
            submission,
        })
    }

    /// Resolves the boundary request through the same engine as the live TUI.
    fn resolve_completion(&self, request: CompletionRequest) -> InputEvent {
        InputEvent::CompletionResolved {
            resolution: self
                .state
                .completion()
                .resolve_request(request, &self.workspace),
        }
    }
}

fn setup_history(setup: &Value) -> Vec<String> {
    if let Some(entries) = setup.pointer("/history/entries").and_then(Value::as_array) {
        return entries
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
    }
    setup
        .pointer("/history/raw")
        .and_then(Value::as_str)
        .map(|raw| {
            raw.lines()
                .filter(|line| !line.is_empty())
                .map(|line| {
                    serde_json::from_str::<String>(line).unwrap_or_else(|_| line.to_owned())
                })
                .collect()
        })
        .unwrap_or_default()
}

fn decode_event(raw: &Value) -> Result<InputEvent, String> {
    let kind = raw
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("event without a type: {raw}"))?;
    match kind {
        "key" | "paste" | "resize" | "mouse" | "transcript" | "switching" | "feedback" => {
            serde_json::from_value(rename_event(raw, kind)).map_err(|error| {
                format!("event `{kind}` cannot be decoded by this runner: {error} ({raw})")
            })
        }
        "safety" => Ok(InputEvent::SafetyChanged {
            value: serde_json::from_value(
                raw.get("value")
                    .cloned()
                    .ok_or_else(|| "safety event without a value".to_owned())?,
            )
            .map_err(|error| format!("safety value is invalid: {error}"))?,
        }),
        "externalEditor" => Ok(InputEvent::ExternalEditor {
            text: raw.get("text").and_then(Value::as_str).map(str::to_owned),
        }),
        other => Err(format!(
            "trace uses event `{other}` which this runner cannot replay; the trace fails instead of being skipped"
        )),
    }
}

fn rename_event(raw: &Value, kind: &str) -> Value {
    let mut object = raw.as_object().cloned().unwrap_or_default();
    if kind == "key" {
        // `char` is absent for named keys and null-safe for the boundary.
        if object.get("char").is_some_and(Value::is_null) {
            object.remove("char");
        }
    }
    Value::Object(object)
}

/// Drops the state fields Rust does not model yet from an expected observation.
fn strip_unmodelled_state(state: &mut Value) {
    for (path, _story) in UNMODELLED_STATE_PATHS {
        let mut cursor = &mut *state;
        let mut segments = path.split('.').peekable();
        while let Some(segment) = segments.next() {
            let Some(map) = cursor.as_object_mut() else {
                break;
            };
            if segments.peek().is_none() {
                map.remove(segment);
                break;
            }
            let Some(next) = map.get_mut(segment) else {
                break;
            };
            cursor = next;
        }
    }
}

fn is_ep002_story(trace: &Map<String, Value>) -> bool {
    matches!(
        trace.get("story").and_then(Value::as_str),
        Some("US-004" | "US-005" | "US-006" | "US-007")
    )
}

fn is_ep003_story(trace: &Map<String, Value>) -> bool {
    matches!(
        trace.get("story").and_then(Value::as_str),
        Some("US-008" | "US-009" | "US-010" | "US-011")
    )
}

fn composer_render_projection(render: &Value) -> Value {
    let mut projection = Map::new();
    for field in ["cursorCell", "prompt", "visualLines", "wrapWidth"] {
        if let Some(value) = render.get(field) {
            projection.insert(field.to_owned(), value.clone());
        }
    }
    Value::Object(projection)
}

/// Rewrites the recorded workspace placeholder to this run's temporary path.
///
/// Applied to events and to expected observations alike: both sides must name
/// the same directory or a path-shaped assertion can never be satisfied.
fn substitute_workspace(value: &mut Value, workspace: &str) {
    match value {
        Value::String(text) => {
            if text.contains(WORKSPACE_PLACEHOLDER) {
                *text = text.replace(WORKSPACE_PLACEHOLDER, workspace);
            }
        }
        Value::Array(items) => {
            for item in items {
                substitute_workspace(item, workspace);
            }
        }
        Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(Value::as_str).map(str::to_owned) {
                if let Some(cursor) = map.get_mut("cursor")
                    && let Some(offset) = cursor.as_u64()
                {
                    *cursor = json!(workspace_adjusted_offset(&text, offset, workspace));
                }
                if let Some(selection) = map.get_mut("selection").and_then(Value::as_array_mut) {
                    for offset in selection {
                        if let Some(value) = offset.as_u64() {
                            *offset = json!(workspace_adjusted_offset(&text, value, workspace));
                        }
                    }
                }
            }
            for item in map.values_mut() {
                substitute_workspace(item, workspace);
            }
        }
        _ => {}
    }
}

fn workspace_adjusted_offset(text: &str, offset: u64, workspace: &str) -> u64 {
    let offset = usize::try_from(offset).unwrap_or(usize::MAX);
    if offset >= text.chars().count() {
        return u64::try_from(
            text.replace(WORKSPACE_PLACEHOLDER, workspace)
                .chars()
                .count(),
        )
        .unwrap_or(u64::MAX);
    }
    let prefix = text.chars().take(offset).collect::<String>();
    let occurrences = prefix.matches(WORKSPACE_PLACEHOLDER).count();
    let placeholder_chars = WORKSPACE_PLACEHOLDER.chars().count();
    let workspace_chars = workspace.chars().count();
    if workspace_chars >= placeholder_chars {
        u64::try_from(
            offset.saturating_add(occurrences.saturating_mul(workspace_chars - placeholder_chars)),
        )
        .unwrap_or(u64::MAX)
    } else {
        u64::try_from(
            offset.saturating_sub(occurrences.saturating_mul(placeholder_chars - workspace_chars)),
        )
        .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

fn first_difference(
    expected: &Value,
    actual: &Value,
    path: &str,
) -> Option<(String, Value, Value)> {
    match (expected, actual) {
        (Value::Object(expected_map), Value::Object(actual_map)) => {
            let keys = expected_map
                .keys()
                .chain(actual_map.keys())
                .collect::<BTreeSet<_>>();
            for key in keys {
                let expected_value = expected_map.get(key).unwrap_or(&Value::Null);
                let actual_value = actual_map.get(key).unwrap_or(&Value::Null);
                if let Some(difference) =
                    first_difference(expected_value, actual_value, &format!("{path}.{key}"))
                {
                    return Some(difference);
                }
            }
            None
        }
        (Value::Array(expected_items), Value::Array(actual_items)) => {
            if expected_items.len() != actual_items.len() {
                return Some((path.to_owned(), expected.clone(), actual.clone()));
            }
            for (index, (expected_item, actual_item)) in
                expected_items.iter().zip(actual_items).enumerate()
            {
                if let Some(difference) =
                    first_difference(expected_item, actual_item, &format!("{path}[{index}]"))
                {
                    return Some(difference);
                }
            }
            None
        }
        _ if expected == actual => None,
        _ => Some((path.to_owned(), expected.clone(), actual.clone())),
    }
}

fn compare(
    dimension: &'static str,
    event_index: Option<usize>,
    expected: &Value,
    actual: &Value,
) -> Option<Divergence> {
    first_difference(expected, actual, "").map(|(path, expected, actual)| Divergence {
        dimension,
        event_index,
        path,
        expected,
        actual,
    })
}

fn materialize_workspace(
    root: &Path,
    files: &BTreeMap<String, String>,
) -> Result<(), std::io::Error> {
    for (relative, contents) in files {
        let target = root.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, contents)?;
    }
    Ok(())
}

fn validate_trace_schema(trace: &Map<String, Value>, id: &str) -> Result<(), String> {
    for key in trace.keys() {
        if !TRACE_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "trace `{id}` carries unknown field `{key}`; the schema must be updated before it can pass"
            ));
        }
    }
    for key in ["setup", "initial", "events", "observations"] {
        if !trace.contains_key(key) {
            return Err(format!("trace `{id}` is missing required field `{key}`"));
        }
    }
    let version = trace
        .get("schemaVersion")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if version != u64::from(SCHEMA_VERSION) {
        return Err(format!(
            "trace `{id}` uses schema version {version}; this runner replays version {SCHEMA_VERSION}"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The differential test
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::unwrap_in_result)]
fn canonical_traces_replay_with_their_declared_parity() -> Result<(), String> {
    let manifest = load_manifest()?;
    let expectations = load_expectations()?;
    let directory = parity_directory();
    let temporary = tempfile::tempdir().map_err(|error| format!("workspace root: {error}"))?;

    let mut failures = Vec::new();
    let mut matched = 0usize;
    let mut diverged = 0usize;
    let mut deferred = 0usize;
    // Calibration mode: rewrite the declared expectations from what this run
    // observed. It is opt-in so a normal run can never silence a regression.
    let calibrating = std::env::var_os("VIBE_PARITY_CALIBRATE").is_some();
    let mut calibrated: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for trace in &manifest.unavailable {
        calibrated.insert(
            trace.id.clone(),
            BTreeMap::from([("state".to_owned(), "unavailable".to_owned())]),
        );
    }

    for entry in &manifest.traces {
        let mut raw: Value = read_json(&directory.join(&entry.file))?;
        let workspace = temporary.path().join(&entry.id);
        substitute_workspace(&mut raw, &workspace.to_string_lossy());
        let object = raw
            .as_object()
            .ok_or_else(|| format!("trace `{}` is not a JSON object", entry.id))?;
        validate_trace_schema(object, &entry.id)?;

        let setup = object
            .get("setup")
            .ok_or_else(|| format!("trace `{}` has no setup", entry.id))?;
        let workspace_name = setup
            .get("workspace")
            .and_then(Value::as_str)
            .unwrap_or("empty");
        let files = manifest.workspaces.get(workspace_name).ok_or_else(|| {
            format!(
                "trace `{}` needs workspace fixture `{workspace_name}` which the manifest does not describe",
                entry.id
            )
        })?;
        std::fs::create_dir_all(&workspace).map_err(|error| format!("workspace: {error}"))?;
        materialize_workspace(&workspace, files)
            .map_err(|error| format!("workspace fixture `{workspace_name}`: {error}"))?;

        let declared = expectations
            .traces
            .get(&entry.id)
            .ok_or_else(|| format!("trace `{}` has no expectation entry", entry.id))?;

        let mut replay = Replay::new(workspace, setup)?;
        let mut observed: BTreeMap<&'static str, Option<Divergence>> = BTreeMap::new();

        let events = object
            .get("events")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("trace `{}` has no events", entry.id))?;
        let observations = object
            .get("observations")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("trace `{}` has no observations", entry.id))?;
        if events.len() != observations.len() {
            return Err(format!(
                "trace `{}` records {} events for {} observations",
                entry.id,
                events.len(),
                observations.len()
            ));
        }

        for (index, (raw_event, observation)) in events.iter().zip(observations).enumerate() {
            let event = decode_event(raw_event)?;
            let step = replay.step(event)?;
            let state = serde_json::to_value(replay.state.observe())
                .map_err(|error| format!("state does not serialize: {error}"))?;

            let mut expected_state = observation.get("state").cloned().unwrap_or(Value::Null);
            strip_unmodelled_state(&mut expected_state);
            if observed.get("state").is_none_or(Option::is_none)
                && let Some(divergence) = compare("state", Some(index), &expected_state, &state)
            {
                observed.insert("state", Some(divergence));
            }

            let expected_effects = observation
                .get("effects")
                .cloned()
                .unwrap_or_else(|| json!([]));
            if observed.get("effects").is_none_or(Option::is_none)
                && let Some(divergence) = compare(
                    "effects",
                    Some(index),
                    &expected_effects,
                    &Value::Array(step.effects),
                )
            {
                observed.insert("effects", Some(divergence));
            }

            if observation.get("history").is_some()
                && observed.get("history").is_none_or(Option::is_none)
                && let Some(divergence) = compare(
                    "history",
                    Some(index),
                    observation.get("history").unwrap_or(&Value::Null),
                    &json!(replay.state.history_entries()),
                )
            {
                observed.insert("history", Some(divergence));
            }

            if let Some(expected_submission) = observation.get("submission") {
                observed.entry("submission").or_insert(None);
                if observed.get("submission").is_none_or(Option::is_none)
                    && let Some(divergence) = compare(
                        "submission",
                        Some(index),
                        expected_submission,
                        step.submission.as_ref().unwrap_or(&Value::Null),
                    )
                {
                    observed.insert("submission", Some(divergence));
                }
            }

            if is_ep002_story(object)
                && let Some(expected_render) = observation.get("render")
            {
                let expected_render = composer_render_projection(expected_render);
                let actual_render = serde_json::to_value(replay.state.observe_render())
                    .map_err(|error| format!("render observation does not serialize: {error}"))?;
                let actual_render = composer_render_projection(&actual_render);
                observed.entry("render").or_insert(None);
                if observed.get("render").is_none_or(Option::is_none)
                    && let Some(divergence) =
                        compare("render", Some(index), &expected_render, &actual_render)
                {
                    observed.insert("render", Some(divergence));
                }
            } else if is_ep003_story(object)
                && let Some(expected_render) = observation.get("render")
            {
                let actual_render = serde_json::to_value(replay.state.observe_render())
                    .map_err(|error| format!("render observation does not serialize: {error}"))?;
                observed.entry("render").or_insert(None);
                if observed.get("render").is_none_or(Option::is_none)
                    && let Some(divergence) =
                        compare("render", Some(index), expected_render, &actual_render)
                {
                    observed.insert("render", Some(divergence));
                }
            }
        }

        if calibrating {
            let mut dimensions = BTreeMap::new();
            for dimension in DIMENSIONS {
                // A deferred dimension keeps its status: this run produced no
                // observation for it, so it has nothing to calibrate against.
                if let Some(status) = declared.get(*dimension)
                    && status == "deferred"
                    && !observed.contains_key(dimension)
                {
                    dimensions.insert((*dimension).to_owned(), status.clone());
                    continue;
                }
                if !declared.contains_key(*dimension) {
                    continue;
                }
                let status = if observed.get(dimension).and_then(Option::as_ref).is_some() {
                    "gap"
                } else {
                    "parity"
                };
                dimensions.insert((*dimension).to_owned(), status.to_owned());
            }
            calibrated.insert(entry.id.clone(), dimensions);
            continue;
        }

        for dimension in DIMENSIONS {
            let Some(expectation) = declared.get(*dimension) else {
                continue;
            };
            let divergence = observed.get(dimension).and_then(Option::as_ref);
            match (expectation.as_str(), divergence) {
                ("deferred", _) => deferred = deferred.saturating_add(1),
                ("parity", Some(divergence)) => {
                    diverged = diverged.saturating_add(1);
                    failures.push(divergence.describe(&entry.id, &manifest.reference.commit));
                }
                ("parity", None) => matched = matched.saturating_add(1),
                ("gap", Some(_)) => diverged = diverged.saturating_add(1),
                ("gap", None) => {
                    matched = matched.saturating_add(1);
                    failures.push(format!(
                        "trace `{}` ({}) now matches the reference on {dimension}; update expectations.json to `parity`",
                        entry.id, entry.gap
                    ));
                }
                (other, _) => {
                    return Err(format!(
                        "trace `{}` declares expectation `{other}` for `{dimension}`",
                        entry.id
                    ));
                }
            }
        }
    }

    if calibrating {
        let document = json!({"version": SCHEMA_VERSION, "traces": calibrated});
        let encoded = serde_json::to_string_pretty(&document)
            .map_err(|error| format!("expectations do not serialize: {error}"))?;
        std::fs::write(directory.join("expectations.json"), format!("{encoded}\n"))
            .map_err(|error| format!("expectations cannot be written: {error}"))?;
        return Err(
            "expectations.json was rewritten from this run; review the diff and rerun without VIBE_PARITY_CALIBRATE"
                .to_owned(),
        );
    }

    if failures.is_empty() {
        return Ok(());
    }
    let mut report = format!(
        "{matched} parity assertions matched, {diverged} diverged, {deferred} deferred, {} unresolved\n",
        failures.len()
    );
    for failure in &failures {
        let _ = writeln!(report, "\n{failure}");
    }
    Err(report)
}

// ---------------------------------------------------------------------------
// Explicit failure modes
// ---------------------------------------------------------------------------

#[test]
fn unknown_trace_fields_fail_instead_of_being_ignored() {
    let mut trace = Map::new();
    trace.insert("setup".to_owned(), json!({}));
    trace.insert("initial".to_owned(), json!({}));
    trace.insert("events".to_owned(), json!([]));
    trace.insert("observations".to_owned(), json!([]));
    trace.insert("schemaVersion".to_owned(), json!(SCHEMA_VERSION));
    trace.insert("futureField".to_owned(), json!(true));
    let error = validate_trace_schema(&trace, "sample").expect_err("unknown fields must fail");
    assert!(error.contains("futureField"), "{error}");
}

#[test]
fn schema_version_mismatch_fails_the_trace() {
    let mut trace = Map::new();
    trace.insert("setup".to_owned(), json!({}));
    trace.insert("initial".to_owned(), json!({}));
    trace.insert("events".to_owned(), json!([]));
    trace.insert("observations".to_owned(), json!([]));
    trace.insert("schemaVersion".to_owned(), json!(SCHEMA_VERSION + 1));
    let error = validate_trace_schema(&trace, "sample").expect_err("schema drift must fail");
    assert!(error.contains("schema version"), "{error}");
}

#[test]
fn unsupported_events_fail_instead_of_being_skipped() {
    let error = decode_event(&json!({"type": "gesture", "x": 1, "y": 2}))
        .expect_err("an unreplayable event must fail");
    assert!(error.contains("cannot replay"), "{error}");
    let error = decode_event(&json!({"type": "key", "key": "f13", "mods": []}))
        .expect_err("an unknown key must fail");
    assert!(error.contains("cannot be decoded"), "{error}");
}

#[test]
fn differences_report_the_first_divergent_field() {
    let expected = json!({"completion": {"items": [{"label": "/config"}]}});
    let actual = json!({"completion": {"items": [{"label": "/clear"}]}});
    let divergence = compare("state", Some(3), &expected, &actual).expect("a divergence");
    let report = divergence.describe("sample", "abc123");
    assert!(report.contains("event 3"), "{report}");
    assert!(
        report.contains("state.completion.items[0].label"),
        "{report}"
    );
    assert!(report.contains("/config"), "{report}");
    assert!(report.contains("/clear"), "{report}");
    assert!(report.contains("abc123"), "{report}");
}

#[test]
fn the_workspace_placeholder_is_substituted_on_both_sides() {
    let mut trace = json!({
        "events": [{"type": "paste", "text": "__WORKSPACE__/image one.png"}],
        "observations": [{"state": {"text": "@'__WORKSPACE__/image one.png'"}}],
    });
    substitute_workspace(&mut trace, "/tmp/ws");
    let encoded = trace.to_string();
    assert!(!encoded.contains(WORKSPACE_PLACEHOLDER), "{encoded}");
    assert!(encoded.contains("/tmp/ws/image one.png"), "{encoded}");
}

#[test]
fn modelled_state_fields_are_not_dropped() {
    let mut state = json!({
        "text": "draft",
        "history": {"navigating": true, "loadedEntry": true, "cursorMovedSinceLoad": false},
    });
    strip_unmodelled_state(&mut state);
    assert_eq!(
        state,
        json!({
            "text": "draft",
            "history": {
                "navigating": true,
                "loadedEntry": true,
                "cursorMovedSinceLoad": false
            }
        })
    );
}

/// The composer must answer for every state field the runner still compares.
#[test]
fn every_compared_state_field_is_modelled() {
    let observation =
        serde_json::to_value(ChatInputState::new().observe()).expect("the observation serializes");
    for (path, story) in UNMODELLED_STATE_PATHS {
        let mut cursor = &observation;
        for segment in path.split('.') {
            let Some(next) = cursor.get(segment) else {
                cursor = &Value::Null;
                break;
            };
            cursor = next;
        }
        assert!(
            cursor.is_null(),
            "`{path}` is declared unmodelled ({story}) but the composer now reports it; \
             remove it from UNMODELLED_STATE_PATHS so it becomes a real assertion"
        );
    }
}

#[test]
fn a_recorded_dimension_is_detected_for_declaration() {
    let observations = vec![
        json!({"state": {}, "effects": []}),
        json!({"state": {}, "effects": [], "render": {}}),
    ];
    let recorded = recorded_dimensions(&observations);
    assert!(recorded.contains("render"), "{recorded:?}");
    assert!(!recorded.contains("history"), "{recorded:?}");
}
