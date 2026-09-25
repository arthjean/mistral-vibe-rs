//! The managed shell session corpus, replayed against this port's own tools.
//!
//! `scripts/parity/shell_session.py` drove the pinned reference's five shell
//! handlers over scripted scenarios and committed what each case answered. This
//! file replays the same scenarios through [`ToolRegistry::invoke`], which is
//! the path a model call takes, and diffs the two documents field by field. A
//! difference no [`LEDGER`] entry names fails the suite, so a shell divergence
//! stops being a sentence in a scorecard and becomes a red test.
//!
//! The replay is unconditional: it reads the committed corpus and needs no
//! reference checkout. Only [`the_committed_corpus_still_matches_the_pinned_reference`]
//! asks for one, and it skips with a named reason when the checkout is absent
//! or sitting off the pin.
//!
//! Two things make the comparison possible across implementations that mint
//! their own identifiers. A session identifier is replaced by a marker naming
//! its shape and the position of the session in its scenario, so `#s0` on one
//! side means the same session as `#s0` on the other while a wrong *shape*
//! still reads as a divergence. And a case's arguments are stored in the
//! authored `{s0}`, `{log0}`, `{root}` form, so each side resolves them against
//! the sessions it started itself.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Map;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use super::session::{ManagedSession, SessionStatus, kill_managed_session};
use super::*;
use crate::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};
use crate::platform::Platform;
use crate::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionStore,
    TrustDecision, TrustRootKind,
};

const CORPUS_RELATIVE: &str = "crates/vibe-core/tests/shell-session/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/shell_session.py";
/// The layout `SCHEMA_VERSION` in the capture script writes.
const CORPUS_SCHEMA_VERSION: u32 = 1;

/// The floors NFR-1 commits to: a corpus that shrank below any of them is not
/// the measurement row 6 quotes.
const MINIMUM_CASES: usize = 70;
const MINIMUM_TOOLS: usize = 5;
const MINIMUM_CASES_PER_TOOL: usize = 5;
/// What the replay is allowed to add to `cargo test --workspace --all-features`,
/// process spawns included.
const REPLAY_BUDGET: Duration = Duration::from_secs(20);

const SCORECARD_RELATIVE: &str = "docs/parity.md";
/// The scorecard row this ledger lowers, named as its cell spells it.
const SHELL_ROW: &str = "Managed shell and terminals";

/// A gap the licensing boundary keeps open, and one a divergence table of the
/// scorecard holds. Every entry names one of the two: nothing in this ledger is
/// tolerated without a row a reader can look up.
const LICENSING: &str = "NOTICE";
const RECORDED: &str = "docs/parity.md";
/// The two divergence sections [`RECORDS`] resolves its rows against.
const OPEN_SECTION: &str = "## Open divergences";
const ACCEPTED_SECTION: &str = "## Accepted divergences";
/// The token a divergence row carries when this ledger is what holds it, so a
/// row the ledger stopped reaching can be told from every other row of the two
/// tables.
const LEDGER_FILE: &str = "session_parity_ledger.rs";
/// The test a divergence row names as the one that fails when its difference
/// stops reproducing.
const STALENESS_TEST: &str = "a_ledger_entry_whose_divergence_is_fixed_fails_the_suite";

/// `CAPTURE_SHELL` in the capture script: the `shell` configuration key both
/// sides are built with.
const CAPTURE_SHELL: &str = "/bin/bash";
const ROOT_PLACEHOLDER: &str = "{root}";
const HOME_PLACEHOLDER: &str = "{home}";
const CAPTURE_PLACEHOLDER: &str = "{capture}";
/// The shape a reference session identifier collapses to, spelled exactly as
/// `SESSION_MARKER` in the capture script spells it.
const SESSION_MARKER: &str = "bash_<stamp:%Y%m%d_%H%M%S>_<hex:8>";
/// What an identifier that does not carry the reference shape collapses to, so
/// a failure message names the divergence rather than a value that changes on
/// every run.
const UNRECOGNIZED_MARKER: &str = "bash_<unrecognized-shape>";
const TIMESTAMP_MARKER: &str = "<timestamp:%Y-%m-%dT%H:%M:%S[.%f]+00:00>";
/// What an epoch-milliseconds stamp collapses to. The reference stamps in
/// ISO-8601 and this port in milliseconds, so the two markers differ and the
/// difference is the divergence US-296 closes.
const MILLIS_MARKER: &str = "<epoch-millis>";

/// The text this repository's own commands emit, mirroring `AUTHORED_TEXT` in
/// the capture script. Both sides must admit the same strings verbatim, or a
/// value one side digests and the other keeps would read as a divergence.
const AUTHORED_TEXT: &[&str] = &[
    "hello\n",
    "oops\n",
    "late\n",
    "timed\n",
    "clamped\n",
    "shell-override\n",
    "parity-token\n",
    "a\nb\nc\n",
    "héllo ✓\n",
    "log line\n",
    "notes fixture\n",
    "inner fixture\n",
    "annotated\n",
    "appended\n",
    "annotated\nappended\n",
    "/bin/bash",
    "/bin/sh",
    "posix",
];

/// The fixtures every scenario's working directory holds, mirroring `FIXTURES`
/// in the capture script.
const FIXTURES: &[(&str, &str)] = &[
    ("notes.txt", "notes fixture\n"),
    ("nested/inner.txt", "inner fixture\n"),
];

// --------------------------------------------------------------------------
// The ledger
// --------------------------------------------------------------------------

/// One tolerated gap between this port and the reference.
#[derive(Debug, Clone, Copy)]
struct Divergence {
    tool: &'static str,
    /// The one case this entry answers for. A wildcard is refused by the audit
    /// test: a gap that spans a tool spans it one case at a time.
    case: &'static str,
    /// Matched by prefix against the reported JSON pointer.
    pointer: &'static str,
    /// [`LICENSING`] when the boundary keeps this gap open, or [`RECORDED`]
    /// when a divergence table of the scorecard holds it.
    closed_by: &'static str,
    /// The row of `docs/parity.md` this gap belongs to, spelled as that row's
    /// cell spells it.
    row: &'static str,
    /// Why the gap stands, asserted non-empty so an entry cannot be added
    /// without a stated reason.
    why: &'static str,
}

impl Divergence {
    /// Whether this entry answers for a reported difference. The prefix stops
    /// at a pointer boundary, so `/typedResult/output` cannot swallow
    /// `/typedResult/outputPath` and hide a second gap behind the first.
    fn covers(&self, tool: &str, case: &str, pointer: &str) -> bool {
        self.tool == tool
            && self.case == case
            && (pointer == self.pointer
                || pointer
                    .strip_prefix(self.pointer)
                    .is_some_and(|rest| rest.starts_with('/')))
    }
}

/// Which divergence table of the scorecard holds a reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Table {
    /// `## Open divergences`, for a difference this port intends to close.
    Open,
    /// `## Accepted divergences`, for a difference that is a decision.
    Accepted,
}

impl Table {
    fn section(self) -> &'static str {
        match self {
            Self::Open => OPEN_SECTION,
            Self::Accepted => ACCEPTED_SECTION,
        }
    }
}

/// One reason the ledger states, and the divergence row that holds it.
#[derive(Debug, Clone, Copy)]
struct Recorded {
    /// The reason constant every entry this row answers for carries.
    why: &'static str,
    /// The row's first cell, spelled as `docs/parity.md` spells it.
    row: &'static str,
    table: Table,
}

include!("session_parity_ledger.rs");

/// The row that holds one entry. An entry the licensing boundary keeps open is
/// held by the licensing row whatever field it lands on, because a rendered
/// document that repeats a reference-authored sentence diverges for that
/// sentence's reason rather than for the rendering's.
fn recorded(entry: &Divergence) -> Option<&'static Recorded> {
    let why = if entry.closed_by == LICENSING {
        WHY_MESSAGE
    } else {
        entry.why
    };
    RECORDS.iter().find(|record| record.why == why)
}

/// The first cell of every row of one section of the scorecard.
fn section_rows(document: &str, section: &str) -> Vec<String> {
    let mut rows = Vec::new();
    let mut inside = false;
    for line in document.lines() {
        if line.starts_with("## ") {
            inside = line.trim() == section;
            continue;
        }
        if !inside || !line.starts_with('|') {
            continue;
        }
        let mut cells = line.trim_matches('|').split(" | ").map(str::trim);
        let Some(first) = cells.next() else {
            continue;
        };
        if first == "Part" || first.starts_with("---") {
            continue;
        }
        rows.push(line.to_owned());
    }
    rows
}

/// The first cell of a table line.
fn first_cell(line: &str) -> &str {
    line.trim_matches('|')
        .split(" | ")
        .next()
        .unwrap_or("")
        .trim()
}

// --------------------------------------------------------------------------
// The corpus
// --------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference_commit: String,
    platform: String,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    /// The scenario this case belongs to. Cases of one scenario share a session
    /// home, a working directory and an ordered session list, and they are
    /// replayed in corpus order because each one measures what the ones before
    /// it left behind.
    scenario: String,
    tool: String,
    case: String,
    arguments: Value,
    /// What the capture did before this case to arrange the state it measures.
    #[serde(default)]
    before: Vec<Value>,
    outcome: String,
    #[serde(default)]
    typed_result: Option<Value>,
    #[serde(default)]
    model_text: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
    /// The sessions this case created or named, with the manifest on disk and
    /// the session info the manager reported.
    #[serde(default)]
    sessions: Option<Value>,
}

impl Case {
    fn id(&self) -> String {
        format!("{}/{}", self.tool, self.case)
    }

    /// The corpus entry as one comparable document, so a missing field on one
    /// side is a pointer difference rather than a special case.
    fn document(&self) -> Value {
        let mut document = Map::new();
        document.insert("outcome".to_owned(), Value::String(self.outcome.clone()));
        if let Some(typed) = &self.typed_result {
            document.insert("typedResult".to_owned(), typed.clone());
        }
        if let Some(text) = &self.model_text {
            document.insert("modelText".to_owned(), text.clone());
        }
        if let Some(sessions) = &self.sessions {
            document.insert("sessions".to_owned(), sessions.clone());
        }
        if let Some(error) = &self.error {
            // Only the kind is compared. The message is recorded as a digest so
            // a re-pin that reworded a refusal shows in the corpus diff, but it
            // is not a conformance target: the PRD lists byte-identical error
            // text as a non-goal, because reaching a reference digest means
            // writing the reference's own sentence.
            document.insert(
                "error".to_owned(),
                json!({"type": error.get("type").cloned().unwrap_or(Value::Null)}),
            );
        }
        Value::Object(document)
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

/// Why this corpus layout cannot be replayed, naming both versions so a capture
/// that added a field without bumping its version says so.
fn schema_complaint(found: u32) -> Option<String> {
    (found != CORPUS_SCHEMA_VERSION).then(|| {
        format!(
            "the corpus layout moved: this replay expects schema {CORPUS_SCHEMA_VERSION} and the \
             committed file carries {found}; regenerate it with `{CAPTURE_SCRIPT}`"
        )
    })
}

fn corpus() -> Corpus {
    let raw =
        fs::read_to_string(repo_root().join(CORPUS_RELATIVE)).expect("the corpus is committed");
    let corpus: Corpus = serde_json::from_str(&raw).expect("the corpus parses");
    if let Some(complaint) = schema_complaint(corpus.schema_version) {
        panic!("{complaint}");
    }
    assert_eq!(
        corpus.reference_commit, REFERENCE_COMMIT,
        "the corpus was captured from another commit than this build asserts"
    );
    corpus
}

/// Why this corpus cannot answer for this host, or [`None`] when it can.
fn skip_reason(corpus: &Corpus) -> Option<String> {
    (corpus.platform != std::env::consts::OS).then(|| {
        format!(
            "skipping the shell session replay: the corpus records the {} behavior and this host \
             is {}; recapture with `{CAPTURE_SCRIPT}`",
            corpus.platform,
            std::env::consts::OS
        )
    })
}

/// Fails when the corpus no longer covers what NFR-1 commits to, naming the
/// count so a shrunken corpus cannot pass as a green one.
fn assert_corpus_floor(corpus: &Corpus) {
    assert!(
        corpus.cases.len() >= MINIMUM_CASES,
        "the corpus shrank to {} cases, below the floor of {MINIMUM_CASES}",
        corpus.cases.len()
    );
    let mut per_tool: BTreeMap<&str, usize> = BTreeMap::new();
    for case in &corpus.cases {
        *per_tool.entry(case.tool.as_str()).or_default() += 1;
    }
    assert!(
        per_tool.len() >= MINIMUM_TOOLS,
        "the corpus drives {} tools, below the floor of {MINIMUM_TOOLS}: {:?}",
        per_tool.len(),
        per_tool.keys().collect::<Vec<_>>()
    );
    let thin = per_tool
        .iter()
        .filter(|(_, count)| **count < MINIMUM_CASES_PER_TOOL)
        .map(|(tool, count)| format!("{tool} has {count}"))
        .collect::<Vec<_>>();
    assert!(
        thin.is_empty(),
        "every tool carries at least {MINIMUM_CASES_PER_TOOL} cases, but {}",
        thin.join(", ")
    );
}

// --------------------------------------------------------------------------
// Normalization and projection, mirroring the capture script
// --------------------------------------------------------------------------

/// The shape an identifier carries, which is what the corpus compares.
///
/// Reference `_new_session_id` renders `<prefix>_%Y%m%d_%H%M%S_<8 hex>`. An
/// identifier that does not is not normalized into agreement: it collapses to
/// its own marker, so the divergence is reported once instead of at every
/// pointer that quotes an identifier.
fn session_shape(identifier: &str) -> &'static str {
    let Some(rest) = identifier.strip_prefix("bash_") else {
        return UNRECOGNIZED_MARKER;
    };
    let parts: Vec<&str> = rest.split('_').collect();
    let [date, time, suffix] = parts.as_slice() else {
        return UNRECOGNIZED_MARKER;
    };
    let shaped = date.len() == 8
        && time.len() == 6
        && suffix.len() == 8
        && date.chars().all(|c| c.is_ascii_digit())
        && time.chars().all(|c| c.is_ascii_digit())
        && suffix
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
    if shaped {
        SESSION_MARKER
    } else {
        UNRECOGNIZED_MARKER
    }
}

/// The marker one scenario's session collapses to, mirroring `session_marker`.
fn session_marker(identifier: &str, index: usize) -> String {
    format!("{}#s{index}", session_shape(identifier))
}

/// Replaces every volatile value by a marker naming its shape.
fn normalize(value: &Value, root: &str, home: Option<&str>, sessions: &[String]) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, item)| (key.clone(), normalize(item, root, home, sessions)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| normalize(item, root, home, sessions))
                .collect(),
        ),
        Value::String(text) => {
            let mut replaced = text.clone();
            for (index, identifier) in sessions.iter().enumerate() {
                replaced = replaced.replace(identifier, &session_marker(identifier, index));
            }
            replaced = replaced.replace(root, ROOT_PLACEHOLDER);
            if let Some(home) = home {
                replaced = replaced.replace(home, HOME_PLACEHOLDER);
            }
            replaced = replace_stamps(&replaced);
            Value::String(if replaced.contains(ROOT_PLACEHOLDER) {
                replaced.replace('\\', "/")
            } else {
                replaced
            })
        }
        other => other.clone(),
    }
}

/// Collapses the two stamp shapes either implementation can carry: the
/// reference's ISO-8601 instant and this port's epoch milliseconds.
fn replace_stamps(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < bytes.len() {
        if let Some(length) = iso_stamp_length(&bytes[index..]) {
            out.push_str(TIMESTAMP_MARKER);
            index += length;
            continue;
        }
        if bytes[index].is_ascii_digit() && !starts_inside_number(&bytes, index) {
            let mut end = index;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            // Thirteen digits is where epoch milliseconds sit, and no count or
            // cursor this corpus records reaches it.
            if end - index >= 13 && (end == bytes.len() || !bytes[end].is_ascii_alphanumeric()) {
                out.push_str(MILLIS_MARKER);
                index = end;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    out
}

fn starts_inside_number(text: &[char], index: usize) -> bool {
    index > 0 && (text[index - 1].is_ascii_digit() || text[index - 1] == '.')
}

/// The length of the ISO-8601 stamp starting here, if one does.
fn iso_stamp_length(text: &[char]) -> Option<usize> {
    let digits = |from: usize, count: usize| {
        text.len() >= from + count && text[from..from + count].iter().all(char::is_ascii_digit)
    };
    let at = |index: usize, character: char| text.get(index) == Some(&character);
    if !(digits(0, 4) && at(4, '-') && digits(5, 2) && at(7, '-') && digits(8, 2) && at(10, 'T')) {
        return None;
    }
    if !(digits(11, 2) && at(13, ':') && digits(14, 2) && at(16, ':') && digits(17, 2)) {
        return None;
    }
    let mut length = 19;
    if at(19, '.') {
        let mut end = 20;
        while end < text.len() && text[end].is_ascii_digit() {
            end += 1;
        }
        if end > 20 {
            length = end;
        }
    }
    // The offset, which the marker names because a local-time stamp would be a
    // divergence rather than a formatting detail.
    if at(length, '+') && digits(length + 1, 2) && at(length + 3, ':') && digits(length + 4, 2) {
        length += 6;
    }
    Some(length)
}

fn digest(text: &str) -> String {
    let hash = Sha256::digest(text.as_bytes());
    let hex = hash.iter().fold(String::new(), |mut accumulator, byte| {
        use std::fmt::Write as _;
        let _ = write!(accumulator, "{byte:02x}");
        accumulator
    });
    format!("sha256:{}", &hex[..32])
}

/// Whether a captured string may be compared as it stands, mirroring
/// `keeps_literal` in the capture script.
fn keeps_literal(text: &str, authored: &BTreeSet<String>) -> bool {
    if text.is_empty() || authored.contains(text) {
        return true;
    }
    if !text.contains(' ') && !text.contains('\n') {
        if text.starts_with(ROOT_PLACEHOLDER)
            || text.starts_with(HOME_PLACEHOLDER)
            || text.starts_with(CAPTURE_PLACEHOLDER)
        {
            return true;
        }
        if text.contains("<stamp:") || text.contains("<timestamp:") {
            return true;
        }
    }
    if authored.iter().any(|supplied| supplied.contains(text)) {
        return true;
    }
    let mut characters = text.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && text.len() <= 32
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

fn project(value: &Value, authored: &BTreeSet<String>) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, item)| (key.clone(), project(item, authored)))
                .collect(),
        ),
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| project(item, authored)).collect())
        }
        Value::String(text) if !keeps_literal(text, authored) => json!({
            "described": digest(text),
            "length": text.chars().count(),
        }),
        other => other.clone(),
    }
}

fn authored_values(value: &Value, into: &mut BTreeSet<String>) {
    match value {
        Value::Object(fields) => fields.values().for_each(|item| authored_values(item, into)),
        Value::Array(items) => items.iter().for_each(|item| authored_values(item, into)),
        Value::String(text) => {
            into.insert(text.clone());
        }
        _ => {}
    }
}

/// Every string this corpus supplies, whichever case reads it back, mirroring
/// `authored_vocabulary` in the capture script.
fn authored_vocabulary(corpus: &Corpus) -> BTreeSet<String> {
    let mut authored: BTreeSet<String> = AUTHORED_TEXT
        .iter()
        .map(|text| (*text).to_owned())
        .collect();
    authored.insert(ROOT_PLACEHOLDER.to_owned());
    authored.insert(HOME_PLACEHOLDER.to_owned());
    authored.insert(CAPTURE_PLACEHOLDER.to_owned());
    for case in &corpus.cases {
        authored_values(&case.arguments, &mut authored);
    }
    authored
}

// --------------------------------------------------------------------------
// Diffing
// --------------------------------------------------------------------------

#[derive(Debug)]
struct Difference {
    pointer: String,
    expected: Value,
    actual: Value,
}

/// Every difference between the two documents, as a JSON pointer and both
/// values.
///
/// When the two sides disagree on the outcome itself, only that is reported:
/// every field below it is a consequence of returning where the reference
/// raised.
fn compare(expected: &Value, actual: &Value) -> Vec<Difference> {
    let outcome = |document: &Value| document.get("outcome").cloned().unwrap_or(Value::Null);
    if outcome(expected) != outcome(actual) {
        return vec![Difference {
            pointer: "/outcome".to_owned(),
            expected: outcome(expected),
            actual: outcome(actual),
        }];
    }
    let mut found = Vec::new();
    differences("", expected, actual, &mut found);
    found
}

fn differences(pointer: &str, expected: &Value, actual: &Value, into: &mut Vec<Difference>) {
    match (expected, actual) {
        (Value::Object(left), Value::Object(right)) => {
            for (key, value) in left {
                let nested = format!("{pointer}/{key}");
                match right.get(key) {
                    Some(other) => differences(&nested, value, other, into),
                    None => into.push(Difference {
                        pointer: nested,
                        expected: value.clone(),
                        actual: Value::String("absent".to_owned()),
                    }),
                }
            }
            for key in right.keys().filter(|key| !left.contains_key(*key)) {
                into.push(Difference {
                    pointer: format!("{pointer}/{key}"),
                    expected: Value::String("absent".to_owned()),
                    actual: right[key].clone(),
                });
            }
        }
        (Value::Array(left), Value::Array(right)) if left.len() == right.len() => {
            for (index, (a, b)) in left.iter().zip(right).enumerate() {
                differences(&format!("{pointer}/{index}"), a, b, into);
            }
        }
        _ if expected == actual => {}
        _ => into.push(Difference {
            pointer: pointer.to_owned(),
            expected: expected.clone(),
            actual: actual.clone(),
        }),
    }
}

// --------------------------------------------------------------------------
// The replay harness
// --------------------------------------------------------------------------

/// Grants every approval, because what this corpus measures is what a tool
/// answers once it is allowed to run.
struct GrantApproval;

impl ApprovalAgent for GrantApproval {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::ApproveForSession) })
    }
}

/// One scenario's world: the session home the corpus normalizes against, the
/// working directory its commands run in, and the sessions it has started.
struct Scenario {
    root: PathBuf,
    registry: ToolRegistry,
    shell: Arc<SessionShell>,
    sessions: Vec<String>,
}

impl Scenario {
    async fn open(scratch: &Path, name: &str) -> Self {
        let root = scratch.join(name);
        let work = root.join("work");
        for (relative, content) in FIXTURES {
            let target = work.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).expect("the fixture directory is writable");
            }
            fs::write(&target, content).expect("the fixture is writable");
        }
        let home = root.join("vibe-home");
        fs::create_dir_all(&home).expect("the session home is writable");
        let root = root.canonicalize().expect("the scenario root resolves");
        let work = work.canonicalize().expect("the working directory resolves");

        assert!(
            root.starts_with(scratch),
            "a replayed scenario writes into its own temporary session home, never the user's: \
             {} sits outside {}",
            root.display(),
            scratch.display()
        );

        let policy = PermissionStore::default();
        policy
            .set_trust(&work, TrustDecision::Trusted, TrustRootKind::Workspace)
            .await
            .expect("the working directory is trusted");
        let guard = ToolGuard::new(policy, Arc::new(GrantApproval) as Arc<dyn ApprovalAgent>);
        guard.config.set_managed_shell_tools(true);
        // Mirrors `build_tool` in the capture script, which pins the configured
        // shell so the fallback ladder does not decide by what the capturing
        // host happens to install.
        guard.config.update(toml::Table::from_iter([(
            "bash".to_owned(),
            toml::Value::Table(toml::Table::from_iter([(
                "shell".to_owned(),
                toml::Value::String(CAPTURE_SHELL.to_owned()),
            )])),
        )]));
        let registry = ToolRegistry::default();
        let tools = ShellTools::with_host(
            root.join("vibe-home"),
            HostShells {
                platform: Platform::Posix,
                git_bash: None,
                powershell: None,
            },
        );
        tools
            .register("session-1", &work, &registry, None, &guard)
            .expect("the shell family registers");
        let shell = tools
            .session_shell("session-1", ShellFamily::Bash)
            .expect("the session shell");
        Self {
            root,
            registry,
            shell,
            sessions: Vec::new(),
        }
    }

    fn sessions_directory(&self) -> PathBuf {
        self.root
            .join("vibe-home")
            .join(LOG_DIRECTORY)
            .join(SESSIONS_DIRECTORY)
    }

    /// A case's arguments with the authored placeholders resolved against the
    /// sessions this replay started, mirroring `resolve_arguments`.
    fn resolve(&self, value: &Value) -> Value {
        match value {
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(key, item)| (key.clone(), self.resolve(item)))
                    .collect(),
            ),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.resolve(item)).collect())
            }
            Value::String(text) => Value::String(self.resolve_text(text)),
            other => other.clone(),
        }
    }

    fn resolve_text(&self, text: &str) -> String {
        let mut resolved = text.replace(ROOT_PLACEHOLDER, &self.root.display().to_string());
        for (index, identifier) in self.sessions.iter().enumerate() {
            resolved = resolved.replace(&format!("{{s{index}}}"), identifier);
            resolved = resolved.replace(
                &format!("{{log{index}}}"),
                &format!("{SESSIONS_DIRECTORY}/{identifier}.log"),
            );
        }
        resolved
    }

    async fn managed(&self, identifier: &str) -> Option<Arc<ManagedSession>> {
        self.shell.managed.lock().await.get(identifier).cloned()
    }

    /// What this port records for one session: the manifest on disk and the
    /// session info the family reports, which is what the corpus holds.
    async fn session_state(&self, identifier: &str) -> Value {
        let info = match self.managed(identifier).await {
            Some(session) => session.info(),
            None => self.shell.orphan(identifier).unwrap_or(Value::Null),
        };
        let manifest =
            fs::read_to_string(self.sessions_directory().join(format!("{identifier}.json")))
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .unwrap_or(Value::Null);
        json!({
            "sessionId": identifier,
            "manifest": manifest,
            "sessionInfo": info,
        })
    }

    /// Registers every session this scenario has started but not yet seen, in
    /// the order they were created, and answers their indices.
    ///
    /// The session home is the source rather than the result document: a case
    /// that raised still started a session, and the corpus records the state it
    /// left behind.
    async fn discover(&mut self) -> Vec<usize> {
        let mut candidates: Vec<(String, String)> = Vec::new();
        for identifier in self.shell.managed.lock().await.keys() {
            if !self.sessions.contains(identifier) {
                candidates.push((String::new(), identifier.clone()));
            }
        }
        if let Ok(entries) = fs::read_dir(self.sessions_directory()) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "json")
                    && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                    && !self.sessions.contains(&stem.to_owned())
                    && !candidates.iter().any(|(_, known)| known == stem)
                {
                    candidates.push((String::new(), stem.to_owned()));
                }
            }
        }
        for candidate in &mut candidates {
            candidate.0 = self.created_at(&candidate.1).await;
        }
        candidates.sort();
        let mut found = Vec::new();
        for (_, identifier) in candidates {
            found.push(self.sessions.len());
            self.sessions.push(identifier);
        }
        found
    }

    /// When a session was created, read from whichever record holds it, so the
    /// discovery order is the creation order rather than the directory order.
    /// The record carries an ISO-8601 instant, which orders lexically the way
    /// it orders chronologically.
    async fn created_at(&self, identifier: &str) -> String {
        let record = match self.managed(identifier).await {
            Some(session) => session.info(),
            None => {
                fs::read_to_string(self.sessions_directory().join(format!("{identifier}.json")))
                    .ok()
                    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                    .unwrap_or(Value::Null)
            }
        };
        record
            .get("created_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    /// Terminates every session this scenario still holds and answers the ones
    /// that survived it, so a background command cannot outlive the run.
    async fn sweep(&self) -> Vec<String> {
        let held: Vec<Arc<ManagedSession>> =
            self.shell.managed.lock().await.values().cloned().collect();
        let mut survivors = Vec::new();
        for session in held {
            if session.is_running() {
                let _ = kill_managed_session(&self.shell, &session, SessionStatus::Killed).await;
            }
            if session.is_running() {
                survivors.push(session.info()["session_id"].to_string());
            }
        }
        survivors
    }

    /// Arranges the state the next case measures, mirroring `_perform`.
    async fn perform(&self, action: &Value) {
        let index = action
            .get("session")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .expect("a capture action names a session");
        let identifier = self
            .sessions
            .get(index)
            .expect("the action names a session this replay started")
            .clone();
        match action.get("do").and_then(Value::as_str) {
            Some("wait") => {
                let seconds = action.get("seconds").and_then(Value::as_f64).unwrap_or(0.0);
                let deadline = Instant::now() + Duration::from_secs_f64(seconds);
                while Instant::now() < deadline {
                    match self.managed(&identifier).await {
                        Some(session) if session.is_running() => {
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                        _ => break,
                    }
                }
            }
            Some("delete-log") => {
                let _ =
                    fs::remove_file(self.sessions_directory().join(format!("{identifier}.log")));
            }
            other => panic!("unsupported capture action: {other:?}"),
        }
    }
}

/// What this port answers for one case, projected the way the corpus is.
async fn observe(scenario: &mut Scenario, case: &Case, authored: &BTreeSet<String>) -> Value {
    for action in &case.before {
        scenario.perform(action).await;
    }
    let arguments = scenario.resolve(&case.arguments);
    let mut targets = referenced_sessions(&case.arguments);
    let outcome = scenario
        .registry
        .invoke(
            &case.tool,
            ToolInvocation {
                call_id: format!("{}-1", case.tool),
                arguments,
            },
        )
        .await;

    let mut document = Map::new();
    match outcome {
        Ok(output) => {
            document.insert("outcome".to_owned(), Value::String("returned".to_owned()));
            document.insert("typedResult".to_owned(), output.typed_result.clone());
            document.insert("modelText".to_owned(), Value::String(output.model_text));
        }
        Err(error) => {
            document.insert("outcome".to_owned(), Value::String("raised".to_owned()));
            document.insert("error".to_owned(), json!({"type": error_type(&error)}));
        }
    }
    // A case that raised can still have started a session, which is why the
    // discovery reads the session home rather than the result document.
    for index in scenario.discover().await {
        targets.push(index);
    }
    targets.sort_unstable();
    targets.dedup();
    let mut states = Vec::new();
    for index in targets {
        if let Some(identifier) = scenario.sessions.get(index).cloned() {
            states.push(scenario.session_state(&identifier).await);
        }
    }
    if !states.is_empty() {
        document.insert("sessions".to_owned(), Value::Array(states));
    }

    let home = std::env::var("HOME").ok();
    project(
        &normalize(
            &Value::Object(document),
            &scenario.root.display().to_string(),
            home.as_deref(),
            &scenario.sessions,
        ),
        authored,
    )
}

/// The ordered session indices a case's arguments name, mirroring
/// `referenced_sessions`.
fn referenced_sessions(value: &Value) -> Vec<usize> {
    let mut found = Vec::new();
    collect_references(value, &mut found);
    found
}

fn collect_references(value: &Value, into: &mut Vec<usize>) {
    match value {
        Value::Object(fields) => fields
            .values()
            .for_each(|item| collect_references(item, into)),
        Value::Array(items) => items.iter().for_each(|item| collect_references(item, into)),
        Value::String(text) => {
            let mut rest = text.as_str();
            while let Some(start) = rest.find('{') {
                let Some(end) = rest[start..].find('}') else {
                    break;
                };
                let token = &rest[start + 1..start + end];
                let digits = token
                    .strip_prefix('s')
                    .or_else(|| token.strip_prefix("log"))
                    .unwrap_or("");
                if !digits.is_empty()
                    && let Ok(index) = digits.parse::<usize>()
                {
                    into.push(index);
                }
                rest = &rest[start + end + 1..];
            }
        }
        _ => {}
    }
}

/// The error's kind, named the way the reference names its exception class.
fn error_type(error: &ToolError) -> &'static str {
    match error {
        ToolError::Approved { source, .. } => error_type(source),
        ToolError::SchemaViolation { .. } => "ValidationError",
        _ => "ToolError",
    }
}

/// Replays the whole corpus, returning the differences found per case.
async fn replay(corpus: &Corpus, scratch: &Path) -> Vec<(String, String, Vec<Difference>)> {
    let authored = authored_vocabulary(corpus);
    let mut found = Vec::new();
    // Every scenario is held until the end so the sweep below can answer for
    // the processes it started, which is what the security requirement asks.
    let mut opened: Vec<(String, Scenario)> = Vec::new();
    for case in &corpus.cases {
        if opened.last().is_none_or(|(name, _)| *name != case.scenario) {
            opened.push((
                case.scenario.clone(),
                Scenario::open(scratch, &case.scenario).await,
            ));
        }
        let current = &mut opened.last_mut().expect("just opened").1;
        let observed = observe(current, case, &authored).await;
        found.push((
            case.tool.clone(),
            case.case.clone(),
            compare(&case.document(), &observed),
        ));
    }
    let mut survivors = Vec::new();
    for (name, scenario) in &opened {
        for identifier in scenario.sweep().await {
            survivors.push(format!("{name}/{identifier}"));
        }
    }
    assert!(
        survivors.is_empty(),
        "these sessions outlived the replay that started them: {survivors:?}"
    );
    found
}

// --------------------------------------------------------------------------
// The tests
// --------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn shell_sessions_match_the_reference_except_for_the_recorded_gap() {
    let corpus = corpus();
    assert_corpus_floor(&corpus);
    if let Some(reason) = skip_reason(&corpus) {
        eprintln!("{reason}");
        return;
    }
    let scratch = TempDir::new().expect("tempdir");
    let started = Instant::now();
    let replayed = replay(&corpus, scratch.path()).await;
    let elapsed = started.elapsed();

    let mut conforming = 0;
    let mut tolerated: BTreeSet<(String, String)> = BTreeSet::new();
    let mut unlisted = Vec::new();
    let tools: BTreeSet<&str> = corpus.cases.iter().map(|case| case.tool.as_str()).collect();

    for (tool, case, found) in &replayed {
        if found.is_empty() {
            conforming += 1;
        }
        for difference in found {
            match LEDGER
                .iter()
                .find(|entry| entry.covers(tool, case, &difference.pointer))
            {
                Some(entry) => {
                    let record = recorded(entry).expect(
                        "every ledger reason names a divergence row, which \
                         every_recorded_reason_has_exactly_one_divergence_row holds",
                    );
                    tolerated.insert((
                        format!("{}/{} at {}", entry.tool, entry.case, entry.pointer),
                        record.row.to_owned(),
                    ));
                }
                None => unlisted.push(format!(
                    "{tool}/{case} at {}: the reference says {}, this port says {}",
                    difference.pointer, difference.expected, difference.actual
                )),
            }
        }
    }

    println!(
        "shell sessions: {conforming}/{} cases match the reference at {} over {} tools, {} ledger \
         entries exercised",
        corpus.cases.len(),
        &corpus.reference_commit[..12],
        tools.len(),
        tolerated.len()
    );
    let mut per_row: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, row) in &tolerated {
        *per_row.entry(row.as_str()).or_default() += 1;
    }
    println!(
        "  {} recorded divergences hold them, {} open and {} accepted",
        per_row.len(),
        RECORDS
            .iter()
            .filter(|record| record.table == Table::Open && per_row.contains_key(record.row))
            .count(),
        RECORDS
            .iter()
            .filter(|record| record.table == Table::Accepted && per_row.contains_key(record.row))
            .count(),
    );
    for (row, count) in &per_row {
        println!("  {count} recorded as {row:?}");
    }

    assert!(
        elapsed <= REPLAY_BUDGET,
        "the replay took {elapsed:?}, above the {REPLAY_BUDGET:?} this suite is allowed to add"
    );
    assert!(
        unlisted.is_empty(),
        "shell session divergences outside the ledger:\n  {}",
        unlisted.join("\n  ")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ledger_entry_whose_divergence_is_fixed_fails_the_suite() {
    let corpus = corpus();
    if let Some(reason) = skip_reason(&corpus) {
        eprintln!("{reason}");
        return;
    }
    let scratch = TempDir::new().expect("tempdir");
    let replayed = replay(&corpus, scratch.path()).await;

    let mut exercised: BTreeSet<usize> = BTreeSet::new();
    for (tool, case, found) in &replayed {
        for difference in found {
            if let Some(position) = LEDGER
                .iter()
                .position(|entry| entry.covers(tool, case, &difference.pointer))
            {
                exercised.insert(position);
            }
        }
    }

    let stale = LEDGER
        .iter()
        .enumerate()
        .filter(|(position, _)| !exercised.contains(position))
        .map(|(_, entry)| {
            format!(
                "{}/{} at {} ({})",
                entry.tool, entry.case, entry.pointer, entry.closed_by
            )
        })
        .collect::<Vec<_>>();

    assert!(
        stale.is_empty(),
        "these ledger entries no longer describe a divergence and must be removed, which is what \
         keeps the ledger from rotting:\n  {}",
        stale.join("\n  ")
    );
}

#[test]
fn every_ledger_entry_names_what_closes_it() {
    let scorecard = fs::read_to_string(repo_root().join(SCORECARD_RELATIVE))
        .expect("the scorecard is committed");
    let cells: Vec<&str> = scorecard
        .lines()
        .filter(|line| line.starts_with("| "))
        .flat_map(|line| line.trim_matches('|').split(" | "))
        .map(str::trim)
        .collect();
    for entry in LEDGER {
        assert!(
            entry.closed_by.starts_with("US-")
                || entry.closed_by == LICENSING
                || entry.closed_by == RECORDED,
            "a tolerated divergence names the story that closes it, the licensing boundary that \
             keeps it open, or the scorecard that records the decision, not {}",
            entry.closed_by
        );
        assert!(
            entry.pointer.starts_with('/'),
            "{}/{} names {}, which is not a JSON pointer",
            entry.tool,
            entry.case,
            entry.pointer
        );
        assert!(
            !entry.pointer.contains('*') && entry.case != "*" && entry.tool != "*",
            "a ledger entry answers for one case at one pointer, not for every case of {}: a \
             wildcard outlives the divergence it was written for",
            entry.tool
        );
        assert!(
            !entry.why.is_empty(),
            "{}/{} states no reason",
            entry.tool,
            entry.case
        );
        let named = cells.iter().filter(|cell| **cell == entry.row).count();
        assert!(
            named >= 1,
            "{}/{} names the row {:?}, which `{SCORECARD_RELATIVE}` does not carry",
            entry.tool,
            entry.case,
            entry.row
        );
    }
}

/// The binding US-306 asks for, resolved in both directions: a reason naming a
/// row `docs/parity.md` does not carry fails, and a divergence row this ledger
/// holds that no reason reaches fails too. Without the second half a row could
/// outlive the entries that justified it, which is the state the tables exist
/// to prevent.
#[test]
fn every_recorded_reason_has_exactly_one_divergence_row() {
    let scorecard = fs::read_to_string(repo_root().join(SCORECARD_RELATIVE))
        .expect("the scorecard is committed");

    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for record in RECORDS {
        assert!(
            seen.insert(record.row),
            "{:?} is recorded twice, so a difference would have two rows",
            record.row
        );
        assert!(
            !record.why.is_empty(),
            "{:?} holds a reason nobody stated",
            record.row
        );
    }
    assert_eq!(
        RECORDS
            .iter()
            .map(|record| record.why)
            .collect::<BTreeSet<_>>()
            .len(),
        RECORDS.len(),
        "two records claim the same reason, so a difference would have two rows"
    );

    // Forward: every reason a live entry states resolves to one row, in the
    // table that row is written in, and that row names the test that fails when
    // the difference stops reproducing.
    let mut reached: BTreeSet<&str> = BTreeSet::new();
    for entry in LEDGER {
        let record = recorded(entry).unwrap_or_else(|| {
            panic!(
                "{}/{} states a reason no row of `{SCORECARD_RELATIVE}` holds: {}",
                entry.tool, entry.case, entry.why
            )
        });
        reached.insert(record.row);
    }
    for record in RECORDS {
        assert!(
            reached.contains(record.row),
            "{:?} is written in `{SCORECARD_RELATIVE}` and no ledger entry reaches it any more, \
             so the row outlived its difference",
            record.row
        );
        let section = record.table.section();
        let line = section_rows(&scorecard, section)
            .into_iter()
            .find(|line| first_cell(line) == record.row)
            .unwrap_or_else(|| {
                panic!(
                    "`{SCORECARD_RELATIVE}` carries no {section:?} row named {:?}",
                    record.row
                )
            });
        assert!(
            line.contains(LEDGER_FILE),
            "{:?} does not name `{LEDGER_FILE}`, so a reader cannot reach the entries it holds",
            record.row
        );
        assert!(
            line.contains(STALENESS_TEST),
            "{:?} does not name `{STALENESS_TEST}`, which is the test that fails when its \
             difference stops reproducing",
            record.row
        );
    }

    // Reverse: a row of either table that says this ledger holds it has to be
    // one of the records above.
    for section in [OPEN_SECTION, ACCEPTED_SECTION] {
        for line in section_rows(&scorecard, section) {
            if !line.contains(LEDGER_FILE) {
                continue;
            }
            let part = first_cell(&line);
            assert!(
                RECORDS.iter().any(|record| record.row == part),
                "the {section:?} row {part:?} says `{LEDGER_FILE}` holds it, and no reason in \
                 that ledger names it"
            );
        }
    }

    println!(
        "shell divergence rows: {} recorded, {} open and {} accepted, every one reached",
        RECORDS.len(),
        RECORDS.iter().filter(|r| r.table == Table::Open).count(),
        RECORDS
            .iter()
            .filter(|r| r.table == Table::Accepted)
            .count(),
    );
}

/// What the rendered row claims: the digest of a model document diverges only
/// where the typed document under it does. A case whose only entry is
/// `/modelText` would falsify it, and the row would have to be rewritten as an
/// independent difference rather than a consequence of the rows above it.
#[test]
fn no_rendered_document_diverges_on_its_own() {
    let mut per_case: BTreeMap<(&str, &str), Vec<&str>> = BTreeMap::new();
    for entry in LEDGER {
        per_case
            .entry((entry.tool, entry.case))
            .or_default()
            .push(entry.pointer);
    }
    let alone = per_case
        .iter()
        .filter(|(_, pointers)| {
            pointers
                .iter()
                .all(|pointer| pointer.starts_with("/modelText"))
        })
        .map(|((tool, case), _)| format!("{tool}/{case}"))
        .collect::<Vec<_>>();
    assert!(
        alone.is_empty(),
        "these cases diverge on the rendered document and on nothing else, so it is not the \
         consequence its divergence row says it is:\n  {}",
        alone.join("\n  ")
    );
}

#[test]
fn a_corpus_layout_change_without_a_version_bump_names_both_versions() {
    assert!(schema_complaint(CORPUS_SCHEMA_VERSION).is_none());
    let complaint = schema_complaint(CORPUS_SCHEMA_VERSION + 1).expect("the drift is refused");
    assert!(
        complaint.contains(&CORPUS_SCHEMA_VERSION.to_string())
            && complaint.contains(&(CORPUS_SCHEMA_VERSION + 1).to_string())
            && complaint.contains(CAPTURE_SCRIPT),
        "the refusal names the expected version, the found one and the way back: {complaint}"
    );
}

#[test]
fn the_committed_corpus_carries_no_reference_prose() {
    let corpus = corpus();
    let authored = authored_vocabulary(&corpus);
    let mut offending = Vec::new();
    let mut undescribed = Vec::new();
    for case in &corpus.cases {
        collect_literals(&case.document(), &authored, &mut offending);
        if let Some(error) = &case.error {
            let described = error
                .get("message")
                .and_then(Value::as_object)
                .is_some_and(|message| message.contains_key("described"));
            if !described {
                undescribed.push(case.id());
            }
        }
    }
    assert!(
        offending.is_empty(),
        "the corpus carries strings that are neither a value it supplied, a normalized path, a \
         marker, nor an identifier, so they may be reference prose: {offending:?}"
    );
    assert!(
        undescribed.is_empty(),
        "these cases record an error message that is not a digest: {undescribed:?}"
    );
}

fn collect_literals(value: &Value, authored: &BTreeSet<String>, into: &mut Vec<String>) {
    match value {
        Value::Object(fields) => {
            if fields.len() == 2
                && fields.contains_key("described")
                && fields.contains_key("length")
            {
                return;
            }
            fields
                .values()
                .for_each(|item| collect_literals(item, authored, into));
        }
        Value::Array(items) => items
            .iter()
            .for_each(|item| collect_literals(item, authored, into)),
        Value::String(text) if !keeps_literal(text, authored) => into.push(text.clone()),
        _ => {}
    }
}

#[test]
fn the_projection_agrees_with_the_capture_script() {
    // Both sides must digest identically, or every projected string would read
    // as a divergence. Anchored on a value the Python side computes with
    // `hashlib.sha256(text.encode()).hexdigest()[:32]`.
    assert_eq!(
        digest("alpha one"),
        "sha256:447ddb49ae0e88206741f4e0d10b1371"
    );
    let authored = BTreeSet::from(["log line\n".to_owned()]);
    assert!(keeps_literal("log line\n", &authored));
    // A window over an authored string carries nothing the whole string did
    // not, which is the slice rule both sides apply.
    assert!(keeps_literal("log", &authored));
    assert!(keeps_literal("{root}/work/notes.txt", &authored));
    assert!(keeps_literal("completed", &authored));
    assert!(keeps_literal(SESSION_MARKER, &authored));
    assert!(keeps_literal(TIMESTAMP_MARKER, &authored));
    assert!(!keeps_literal("Killed session 3 of them", &authored));

    assert_eq!(
        session_shape("bash_20260822_101112_0a1b2c3d"),
        SESSION_MARKER
    );
    assert_eq!(
        session_shape("bash_1755859200000_0a1b2c3d"),
        UNRECOGNIZED_MARKER
    );
    assert_eq!(
        replace_stamps("started 2026-08-22T10:11:12.123456+00:00 at 1755859200000"),
        format!("started {TIMESTAMP_MARKER} at {MILLIS_MARKER}")
    );
    assert_eq!(
        replace_stamps("2026-08-22T10:11:12+00:00"),
        TIMESTAMP_MARKER.to_owned()
    );
}

/// Recaptures against the local checkout and asserts the committed corpus is
/// still what the pinned reference answers.
///
/// This is the only test here that needs the checkout, and it skips naming both
/// commits and the way back when the checkout is absent or off-pin. The replay
/// above runs regardless, which is what keeps a missing checkout from failing
/// `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "shell sessions") {
        eprintln!("{reason}");
        eprintln!(
            "this build asserts {REFERENCE_COMMIT}; the committed corpus replayed regardless, \
             restore with `{RESTORE_COMMAND}`"
        );
        return;
    }
    let repository = repo_root();
    let output = Command::new("python3")
        .arg(repository.join(CAPTURE_SCRIPT))
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(repository.join("target/shell-session-full.json"))
        .arg("--check")
        .current_dir(&repository)
        .output()
        .expect("the shell session capture script runs");
    assert!(
        output.status.success(),
        "the pinned reference no longer answers what the committed corpus records; regenerate it \
         with `{CAPTURE_SCRIPT}`: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
