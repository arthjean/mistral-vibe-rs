//! Differential oracle for the shell policy.
//!
//! The Python reference is the authority on which commands a shell call
//! extracts, which of them have their path operands inspected, which
//! directories a call reaches outside the workspace, and what permission the
//! whole thing resolves to. `scripts/parity/shell_policy.py` asks it all four
//! questions and records the answers; this module replays them against
//! [`extract_commands`], [`inspects_paths`] and [`analyze_shell`].
//!
//! The corpus is committed: it carries command names, node-kind names, scope
//! values, booleans and the answers to cases this repository authored, and no
//! reference-authored refusal or description text, which is what `NOTICE`
//! forbids shipping. Replay therefore runs unconditionally; only the live probe
//! that recaptures from the pinned checkout skips when it is absent.
//!
//! One divergence is expected and enumerated. This port withholds an automatic
//! grant in three places the reference grants outright, listed in
//! [`STRICTER_THAN_THE_REFERENCE`]. Each one only costs an approval prompt, so
//! the guard still fails toward asking; the ledger exists so the set cannot grow
//! silently, and a case that stops diverging fails the suite rather than rotting
//! in the list.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use super::*;
use crate::parity::{off_pin_reason, pinned_interpreter, reference_root};
use crate::policy::PermissionScope;
use crate::tools::config::{ToolConfigResolver, shell_read_only_commands};

const CAPTURE_SCRIPT: &str = "scripts/parity/shell_policy.py";
const CORPUS_RELATIVE: &str = "tests/shell-policy/policy.json";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 1;

/// The commands the reference resolves to `always` and this port asks about.
///
/// None of them is refused: each reaches the operator under its own pattern.
/// The reason is that the command text does not say what the call reads. A
/// substitution runs a program the text never names, a redirect writes to a
/// target the segment never carries, and `--no-index` reads a path the index
/// never held. The other guards this port keeps cost nothing here, because the
/// reference already asks about the commands they name.
const STRICTER_THAN_THE_REFERENCE: [&str; 3] = [
    "cat $(which ls)",
    "cat file.txt > out.txt",
    "git diff --no-index /etc/passwd /dev/null",
];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference: Reference,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    counts: Counts,
    command_sets: CommandSets,
    extraction: Vec<ExtractionCase>,
    outside_dirs: Vec<OutsideCase>,
    resolutions: Vec<ResolutionCase>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Counts {
    extraction_cases: usize,
    outside_dir_cases: usize,
    resolution_cases: usize,
    path_commands: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommandSets {
    path_commands: Vec<String>,
    mutating_path_commands: Vec<String>,
    find_execution_predicates: Vec<String>,
    read_only_commands: Vec<String>,
    allowlist: Vec<String>,
    path_commands_cover_readers: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtractionCase {
    command: String,
    segments: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutsideCase {
    command: String,
    directories: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResolutionCase {
    command: String,
    /// [`None`] when the reference resolved nothing of its own, which defers to
    /// the configured permission.
    permission: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "the reason text itself is reference prose")]
    has_reason: bool,
    #[serde(default)]
    requirements: Vec<Requirement>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Requirement {
    scope: String,
    invocation_pattern: String,
    session_pattern: String,
    label: String,
}

fn corpus_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(CORPUS_RELATIVE)
}

fn corpus() -> Corpus {
    let path = corpus_path();
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("shell policy corpus at {}: {error}", path.display()));
    let corpus: Corpus = serde_json::from_str(&raw)
        .unwrap_or_else(|error| panic!("shell policy corpus at {}: {error}", path.display()));
    assert_eq!(
        corpus.schema_version, CORPUS_SCHEMA_VERSION,
        "the corpus layout moved; update this runner with the capture script"
    );
    assert_eq!(
        corpus.reference.commit,
        crate::parity::REFERENCE_COMMIT,
        "the corpus was captured from another commit than the pin"
    );
    corpus
}

/// The lists a POSIX host resolves for `bash`, which is what the capture ran
/// the reference `BashTool` under.
fn posix_lists() -> ShellCommandLists {
    ShellCommandLists::from_config(
        &ToolConfigResolver::new()
            .with_posix_shell(true)
            .view("bash"),
    )
}

/// The value `scope` crosses the wire as.
fn wire_scope(scope: PermissionScope) -> String {
    serde_json::to_value(scope)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .expect("a scope serializes as a string")
}

/// US-109: the grammar extracts what the reference grammar extracts, segment
/// for segment.
#[test]
fn every_extraction_matches_the_reference() {
    let corpus = corpus();
    assert_eq!(corpus.extraction.len(), corpus.counts.extraction_cases);
    let mut conforming = 0_usize;
    for case in &corpus.extraction {
        assert_eq!(
            extract_commands(&case.command),
            case.segments,
            "`{}` extracts differently here",
            case.command.escape_debug()
        );
        conforming += 1;
    }
    eprintln!(
        "shell policy: extraction {conforming}/{}",
        corpus.counts.extraction_cases
    );
}

/// US-111: the inspected set is the reference set, and it covers every
/// read-only command, which is what keeps an auto-allowed reader from escaping
/// the operand walk.
#[test]
fn the_path_inspecting_set_is_the_reference_set() {
    let corpus = corpus();
    let sets = &corpus.command_sets;
    assert_eq!(sets.path_commands.len(), corpus.counts.path_commands);
    assert!(
        sets.path_commands_cover_readers,
        "the reference set stopped covering its own readers"
    );

    let missing = sets
        .path_commands
        .iter()
        .filter(|program| !inspects_paths(ShellFlavor::Posix, program))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "commands the reference inspects and this port does not: {missing:?}"
    );

    // The other direction: nothing invented. The port's set is the union of the
    // eight mutating commands and the branch's readers, so it is compared
    // against the same union.
    let reference = sets.path_commands.iter().cloned().collect::<BTreeSet<_>>();
    let ported = sets
        .mutating_path_commands
        .iter()
        .cloned()
        .chain(
            shell_read_only_commands(true)
                .iter()
                .map(|program| (*program).to_owned()),
        )
        .collect::<BTreeSet<_>>();
    assert_eq!(ported, reference, "the inspected set diverged");

    // The three lists the set is composed from, each compared on its own.
    assert_eq!(
        sets.read_only_commands,
        shell_read_only_commands(true)
            .iter()
            .map(|program| (*program).to_owned())
            .collect::<Vec<_>>(),
        "the read-only command list diverged"
    );
    assert_eq!(
        sets.allowlist,
        posix_lists().allowlist,
        "the default allowlist diverged"
    );
    assert_eq!(
        sets.find_execution_predicates,
        FIND_EXECUTION_PREDICATES
            .iter()
            .map(|predicate| (*predicate).to_owned())
            .collect::<Vec<_>>(),
        "the find execution predicates diverged"
    );
    eprintln!(
        "shell policy: path commands {}/{}, 0 missing, 0 invented",
        ported.len(),
        corpus.counts.path_commands
    );
}

/// The tree the capture resolved its operands against, rebuilt so the recorded
/// placeholders name real directories here too.
struct OperandWorkspace {
    _root: tempfile::TempDir,
    workdir: PathBuf,
    outside: PathBuf,
    scratchpad: PathBuf,
}

impl OperandWorkspace {
    fn build() -> Self {
        let root = tempfile::tempdir().expect("operand root");
        let workdir = root.path().join("workspace");
        let outside = root.path().join("elsewhere");
        let scratchpad = workdir.join(".vibe").join("scratchpad");
        fs::create_dir_all(outside.join("nested")).expect("outside tree");
        fs::create_dir_all(&scratchpad).expect("scratchpad");
        fs::write(workdir.join("inside.txt"), "inside").expect("inside file");
        fs::write(outside.join("secret.txt"), "secret").expect("outside file");
        fs::write(outside.join("nested").join("secret.txt"), "secret").expect("nested file");
        fs::write(scratchpad.join("note.txt"), "note").expect("scratchpad file");
        Self {
            _root: root,
            workdir,
            outside,
            scratchpad,
        }
    }

    /// The host paths each recorded placeholder stands for, longest first so a
    /// nested directory is substituted before its parent.
    fn placeholders(&self) -> Vec<(&'static str, String)> {
        let mut pairs = vec![
            ("<scratchpad>", canonical(&self.scratchpad)),
            ("<outside>", canonical(&self.outside)),
            ("<workdir>", canonical(&self.workdir)),
        ];
        if let Some(home) = crate::config::user_home_directory() {
            pairs.push(("<home>", canonical(&home)));
        }
        pairs.sort_by_key(|(_, host)| std::cmp::Reverse(host.len()));
        pairs
    }

    fn expand(&self, text: &str) -> String {
        let mut expanded = text.to_owned();
        for (placeholder, host) in self.placeholders() {
            expanded = expanded.replace(placeholder, &host);
        }
        expanded
    }

    fn context(&self) -> ShellPolicyContext {
        ShellPolicyContext::new(
            Platform::Posix,
            parse_policy_path(Platform::Posix, &canonical(&self.workdir)).expect("workdir"),
        )
        .with_scratchpad(Some(self.scratchpad.clone()))
    }
}

fn canonical(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// US-111: the directories a call reaches outside the workspace are the
/// reference's, named as the globs one approval covers.
#[test]
fn every_escaping_operand_matches_the_reference() {
    let corpus = corpus();
    assert_eq!(corpus.outside_dirs.len(), corpus.counts.outside_dir_cases);
    let workspace = OperandWorkspace::build();
    let context = workspace.context();
    let lists = posix_lists();

    for case in &corpus.outside_dirs {
        let command = workspace.expand(&case.command);
        let analysis = analyze_shell(ShellFlavor::Posix, &command, &context, &lists);
        let reached = analysis
            .requirements
            .iter()
            .filter(|requirement| requirement.scope == PermissionScope::OutsideDirectory)
            .map(|requirement| requirement.invocation_pattern.clone())
            .collect::<Vec<_>>();
        let expected = case
            .directories
            .iter()
            .map(|directory| format!("{}/*", workspace.expand(directory)))
            .collect::<Vec<_>>();
        assert_eq!(
            reached, expected,
            "`{}` reaches different directories here",
            case.command
        );
    }
    eprintln!(
        "shell policy: escaping operands {}/{}",
        corpus.outside_dirs.len(),
        corpus.counts.outside_dir_cases
    );
}

/// US-110: the permission a command resolves to is the reference's, and every
/// requirement it raises carries the reference scope, patterns and label.
///
/// The ledger is the one tolerated difference, and it is checked in both
/// directions: a listed case that stopped diverging fails, so the list cannot
/// rot.
#[test]
fn every_resolution_matches_the_reference() {
    let corpus = corpus();
    assert_eq!(corpus.resolutions.len(), corpus.counts.resolution_cases);
    let workspace = tempfile::tempdir().expect("workspace");
    let context = ShellPolicyContext::new(
        Platform::Posix,
        parse_policy_path(Platform::Posix, &canonical(workspace.path())).expect("workdir"),
    );
    let lists = posix_lists();
    let ledger = STRICTER_THAN_THE_REFERENCE
        .iter()
        .map(|command| (*command).to_owned())
        .collect::<BTreeSet<String>>();

    let mut diverging = BTreeSet::new();
    let mut conforming = 0_usize;
    let mut by_case = BTreeMap::new();
    for case in &corpus.resolutions {
        let analysis = analyze_shell(ShellFlavor::Posix, &case.command, &context, &lists);
        // The reference answering `None` defers to the configured permission,
        // which is what this port's analysis returns in the same place.
        let expected = case
            .permission
            .clone()
            .unwrap_or_else(|| permission_wire(lists.permission));
        let resolved = permission_wire(analysis.mode);
        by_case.insert(case.command.clone(), (expected.clone(), resolved.clone()));

        if resolved != expected {
            assert!(
                expected == "always" && resolved == "ask",
                "`{}` resolves to `{resolved}` here and to `{expected}` upstream, \
                 which is not the one tolerated direction: {:?}",
                case.command.escape_debug(),
                analysis.rationale
            );
            assert!(
                !analysis.requirements.is_empty(),
                "`{}` withholds the grant without leaving anything to approve",
                case.command.escape_debug()
            );
            diverging.insert(case.command.clone());
            continue;
        }
        conforming += 1;

        // A conforming permission still has to raise the same requirements.
        let raised = analysis
            .requirements
            .iter()
            .map(|requirement| Requirement {
                scope: wire_scope(requirement.scope),
                invocation_pattern: requirement.invocation_pattern.clone(),
                session_pattern: requirement.session_pattern.clone(),
                label: requirement.label.clone(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            raised.len(),
            case.requirements.len(),
            "`{}` raises {} requirements here and {} upstream: {raised:?}",
            case.command.escape_debug(),
            raised.len(),
            case.requirements.len()
        );
        for (here, upstream) in raised.iter().zip(&case.requirements) {
            assert_eq!(
                here.scope,
                upstream.scope,
                "`{}`",
                case.command.escape_debug()
            );
            assert_eq!(
                here.invocation_pattern,
                upstream.invocation_pattern,
                "`{}`",
                case.command.escape_debug()
            );
            assert_eq!(
                here.session_pattern,
                upstream.session_pattern,
                "`{}`",
                case.command.escape_debug()
            );
            assert_eq!(
                here.label,
                upstream.label,
                "`{}`",
                case.command.escape_debug()
            );
        }
    }

    let unlisted = diverging.difference(&ledger).cloned().collect::<Vec<_>>();
    assert!(
        unlisted.is_empty(),
        "commands diverging from the reference without a ledger entry: {unlisted:?}"
    );
    let stale = ledger
        .iter()
        .filter(|command| by_case.contains_key(*command) && !diverging.contains(*command))
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "ledger entries that no longer diverge; remove them: {stale:?}"
    );
    eprintln!(
        "shell policy: resolutions {conforming}/{} conforming, {} ledgered",
        corpus.counts.resolution_cases,
        diverging.len()
    );
}

/// The value `mode` is recorded under upstream, which is its lowercase name.
fn permission_wire(mode: PermissionMode) -> String {
    match mode {
        PermissionMode::Never => "never".to_owned(),
        PermissionMode::Ask => "ask".to_owned(),
        PermissionMode::Always => "always".to_owned(),
    }
}

/// The live probe: recaptures from the pinned checkout and compares, so a
/// reference that moves is reported rather than silently replayed.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "shell policy") {
        eprintln!("{reason}");
        return;
    }
    let Some(interpreter) = pinned_interpreter(&root) else {
        eprintln!("skipping the live shell-policy probe: no reference interpreter");
        return;
    };
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    let temporary = tempfile::tempdir().expect("temporary root");
    let captured = temporary.path().join("policy.json");
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
