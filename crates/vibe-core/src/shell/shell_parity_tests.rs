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
//! Divergences are enumerated, never tolerated wholesale. A resolution where
//! this port asks and the reference grants belongs in
//! [`STRICTER_THAN_THE_REFERENCE`], which only costs an approval prompt, so the
//! guard still fails toward asking. Nothing that lets this port grant without a
//! prompt where the reference asks has a ledger: a resolution in that direction
//! fails, and so does a `find` primary the reference gates and this port does
//! not, since `find` is allowlisted and the call would be granted outright.
//!
//! A requirement field that differs on a case whose permission conforms belongs
//! in [`REQUIREMENT_DIVERGENCES`]. Both sides ask there, so the operator is
//! always prompted, but some entries are permissive in what a session grant
//! covers: where the reference records the literal command text, this port
//! records an arity pattern such as `git reset *` or `python3 *`, so one
//! approval here releases more later calls than it does upstream. Every ledger
//! is checked in both directions: an entry that stops diverging fails the suite
//! rather than rotting in the list.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::*;
use crate::parity::{off_pin_reason, pinned_interpreter, reference_root};
use crate::policy::PermissionScope;
use crate::tools::config::{ToolConfigResolver, shell_read_only_commands};

const CAPTURE_SCRIPT: &str = "scripts/parity/shell_policy.py";
const CORPUS_RELATIVE: &str = "tests/shell-policy/policy.json";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 3;

/// The commands the reference resolves to `always` and this port asks about.
///
/// Empty at 2.25.7: the three entries it held at 2.24.0 (`cat $(which ls)`,
/// `cat file.txt > out.txt` and `git diff --no-index /etc/passwd /dev/null`)
/// stopped diverging, because the reference now asks about all three too
/// (`vibe/core/tools/builtins/bash.py:666-715`).
const STRICTER_THAN_THE_REFERENCE: [&str; 0] = [];

/// Syntax that stops the extracted parts from describing what runs (a
/// heredoc, a redirect to a file, a parse error) invalidates the scope on both
/// sides, and both raise the same literal whole-command requirement after
/// whatever the guardrail raised
/// (`vibe/core/tools/builtins/_shell_permission_analysis.py:293-303,317-325`;
/// `vibe/core/tools/builtins/bash.py:372-390,700-709`). Only the label
/// differs: the reference prints its own approval wording, which the licensing
/// boundary keeps out of this repository, and this port prints original
/// wording naming the same causes.
const UNSCOPED_LABEL: &str = "the whole-command requirement for scope-invalidating syntax \
     carries this port's own wording where the reference prints its approval label \
     (_shell_permission_analysis.py:181-183, bash.py:700-709)";

/// Requirement fields that differ on a case whose permission conforms, as
/// `(command, pointer, reason)`, the pointer reaching into the case's recorded
/// resolution. The fields of the requirements both sides raise are compared
/// pairwise; a bare `/requirements/<index>` pointer names a requirement that
/// only one side raises.
const REQUIREMENT_DIVERGENCES: &[(&str, &str, &str)] = &[
    (
        "python3 <<'EOF'\nprint(1)\nEOF",
        "/requirements/0/label",
        UNSCOPED_LABEL,
    ),
    (
        "cat file.txt > out.txt",
        "/requirements/0/label",
        UNSCOPED_LABEL,
    ),
    ("cat 'unterminated", "/requirements/0/label", UNSCOPED_LABEL),
    (
        "find . -exec rm {} ; && find . -exec rm {} ;",
        "/requirements/1/label",
        UNSCOPED_LABEL,
    ),
    ("git $SUB", "/requirements/0/label", UNSCOPED_LABEL),
    ("sudo $CMD", "/requirements/0/label", UNSCOPED_LABEL),
    ("git log $REF", "/requirements/0/label", UNSCOPED_LABEL),
    (
        "HOME=/x git status",
        "/requirements/0/label",
        UNSCOPED_LABEL,
    ),
    ("ls >&file", "/requirements/0/label", UNSCOPED_LABEL),
    ("cat <<< text", "/requirements/0/label", UNSCOPED_LABEL),
    ("=ls", "/requirements/0/label", UNSCOPED_LABEL),
    ("f() { ls; }", "/requirements/0/label", UNSCOPED_LABEL),
    ("ls \\\n -la", "/requirements/0/label", UNSCOPED_LABEL),
    ("[[ -n $FOO ]]", "/requirements/0/label", UNSCOPED_LABEL),
    ("npm run $TASK", "/requirements/0/label", UNSCOPED_LABEL),
    ("cargo $CMD", "/requirements/0/label", UNSCOPED_LABEL),
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
    repository_resolutions: Vec<RepositoryCase>,
    managed_resolutions: Vec<ManagedCase>,
    windows_grammar: WindowsGrammar,
    stdin_permissions: Vec<StdinCase>,
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
    repository_cases: usize,
    managed_cases: usize,
    windows_grammar_cases: usize,
    stdin_permission_cases: usize,
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

/// A git reader resolved in a workdir holding one repository fixture.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RepositoryCase {
    fixture: String,
    command: String,
    permission: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "the reason text itself is reference prose")]
    has_reason: bool,
    #[serde(default)]
    requirements: Vec<Requirement>,
}

/// A call the managed resolver answered, with the overrides it carried.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedCase {
    command: String,
    cwd: Option<String>,
    shell: Option<String>,
    env: Vec<String>,
    permission: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "the reason text itself is reference prose")]
    has_reason: bool,
    #[serde(default)]
    requirements: Vec<Requirement>,
}

/// What input to a session running `command` needed, where `known` is false
/// for a session the family does not know.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StdinCase {
    family: String,
    command: String,
    known: bool,
    permission: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "the reference raises no reason for pager input")]
    has_reason: bool,
    #[serde(default)]
    requirements: Vec<Requirement>,
}

/// What the PowerShell grammar helpers answered, which are pure string
/// functions a POSIX host evaluates as a Windows host would.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowsGrammar {
    commands: Vec<WindowsCommand>,
    patterns: Vec<WindowsPattern>,
    expansions: Vec<WindowsExpansion>,
    tokens: Vec<WindowsToken>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowsCommand {
    command: String,
    parts: Vec<WindowsPart>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WindowsPart {
    part: String,
    tokens: Vec<String>,
    forms: Vec<String>,
    forms_without_basename: Vec<String>,
    command_name: Option<String>,
    file_redirections: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowsPattern {
    pattern: String,
    forms: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowsExpansion {
    token: String,
    value: String,
    unresolved: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WindowsToken {
    token: String,
    option: bool,
    path: bool,
    attached_value: Option<String>,
}

/// The environment the capture expanded its tokens against:
/// `WINDOWS_EXPANSION_ENVIRONMENT` in the capture script.
const WINDOWS_EXPANSION_ENVIRONMENT: [(&str, &str); 2] = [
    ("USERPROFILE", "C:\\Users\\me"),
    ("AppData", "C:\\Users\\me\\AppData\\Roaming"),
];

/// The repository configurations the capture wrote, by fixture name: the same
/// text `REPOSITORY_FIXTURES` in the capture script writes.
const REPOSITORY_FIXTURES: [(&str, &str); 8] = [
    ("plain", "[core]\n\tbare = false\n"),
    ("pager", "[core]\n\tpager = less\n"),
    ("pager-off", "[core]\n\tpager = off\n"),
    ("include", "[include]\n\tpath = other.config\n"),
    ("log-pager", "[pager]\n\tlog = cat\n"),
    ("fsmonitor", "[core]\n\tfsmonitor\n"),
    ("diff-driver", "[diff \"x\"]\n\ttextconv = cat\n"),
    ("gpg", "[gpg]\n\tprogram = gpg2\n"),
];

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Requirement {
    scope: String,
    invocation_pattern: String,
    session_pattern: String,
    label: Label,
}

/// A requirement label as the corpus commits it.
///
/// A label that is one of its requirement's two patterns is the case's own
/// command text and survives verbatim. Any other label is reference-authored
/// text, committed as a digest and a length, so the port's label is reduced
/// the same way before the two are compared.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
enum Label {
    Verbatim(String),
    Described(Described),
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Described {
    described: String,
    length: usize,
}

impl Label {
    /// The committed form of `label`, by the capture script's rule.
    fn committed(label: &str, invocation_pattern: &str, session_pattern: &str) -> Self {
        if label == invocation_pattern || label == session_pattern {
            return Self::Verbatim(label.to_owned());
        }
        let hash = Sha256::digest(label.as_bytes());
        let hex = hash.iter().fold(String::new(), |mut accumulator, byte| {
            use std::fmt::Write as _;
            let _ = write!(accumulator, "{byte:02x}");
            accumulator
        });
        Self::Described(Described {
            described: format!("sha256:{}", &hex[..32]),
            length: label.chars().count(),
        })
    }
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
        .filter(|program| !inspects_paths(program))
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
    // No ledger: `find` is allowlisted, so a primary the reference gates and
    // this port does not is a call granted here without a prompt.
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
        fs::create_dir_all(workdir.join("nested")).expect("nested workdir");
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

    /// `text` with every host path replaced by its placeholder.
    fn normalize(&self, text: &str) -> String {
        let mut normalized = text.to_owned();
        for (placeholder, host) in self.placeholders() {
            normalized = normalized.replace(&host, placeholder);
        }
        normalized
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

/// What replaying one family of resolutions found.
#[derive(Default)]
struct Replay {
    conforming: usize,
    /// Cases resolving to `ask` here where the reference grants.
    stricter: BTreeSet<String>,
    /// `(command, pointer)` for every requirement field that differs.
    requirement_diffs: BTreeSet<(String, String)>,
    untolerated: Vec<String>,
}

impl Replay {
    /// Compares one analysis with what the reference recorded for `key`.
    fn compare(
        &mut self,
        key: &str,
        analysis: &ShellAnalysis,
        permission: Option<&str>,
        requirements: &[Requirement],
        lists: &ShellCommandLists,
        workspace: &OperandWorkspace,
    ) {
        // The reference answering `None` defers to the configured permission,
        // which is what this port's analysis returns in the same place.
        let expected =
            permission.map_or_else(|| permission_wire(lists.permission), ToOwned::to_owned);
        let resolved = permission_wire(analysis.mode);
        if resolved != expected {
            // Collected rather than asserted in place, so the ledgers are
            // still checked; the assertion at the end is as strict.
            if expected == "always" && resolved == "ask" && !analysis.requirements.is_empty() {
                self.stricter.insert(key.to_owned());
            } else {
                self.untolerated.push(format!(
                    "`{}` resolves to `{resolved}` here and to `{expected}` upstream: {:?}",
                    key.escape_debug(),
                    analysis.rationale
                ));
            }
            return;
        }
        if permission.is_none() && !analysis.requirements.is_empty() {
            self.untolerated.push(format!(
                "`{}` defers upstream and raises requirements here: {:?}",
                key.escape_debug(),
                analysis.requirements
            ));
            return;
        }
        self.conforming += 1;

        // A conforming permission still has to raise the same requirements.
        let raised = analysis
            .requirements
            .iter()
            .map(|requirement| {
                let invocation_pattern = workspace.normalize(&requirement.invocation_pattern);
                let session_pattern = workspace.normalize(&requirement.session_pattern);
                Requirement {
                    scope: wire_scope(requirement.scope),
                    label: Label::committed(
                        &workspace.normalize(&requirement.label),
                        &invocation_pattern,
                        &session_pattern,
                    ),
                    invocation_pattern,
                    session_pattern,
                }
            })
            .collect::<Vec<_>>();
        // A requirement only one side raises is its own divergence; the ones
        // both sides raise are still compared field by field.
        let shared = raised.len().min(requirements.len());
        for index in shared..raised.len().max(requirements.len()) {
            self.requirement_diffs
                .insert((key.to_owned(), format!("/requirements/{index}")));
            eprintln!(
                "`{}` /requirements/{index}: {:?} here, {:?} upstream",
                key.escape_debug(),
                raised.get(index),
                requirements.get(index)
            );
        }
        for (index, (here, upstream)) in raised.iter().zip(requirements).enumerate() {
            let fields = [
                ("scope", here.scope == upstream.scope),
                (
                    "invocationPattern",
                    here.invocation_pattern == upstream.invocation_pattern,
                ),
                (
                    "sessionPattern",
                    here.session_pattern == upstream.session_pattern,
                ),
                ("label", here.label == upstream.label),
            ];
            for (field, equal) in fields {
                if !equal {
                    self.requirement_diffs
                        .insert((key.to_owned(), format!("/requirements/{index}/{field}")));
                    eprintln!(
                        "`{}` /requirements/{index}/{field}: {here:?} here, {upstream:?} upstream",
                        key.escape_debug()
                    );
                }
            }
        }
    }

    /// Checks the ledgers in both directions and fails on anything untolerated.
    fn finish(self, family: &str, total: usize) {
        let ledger = STRICTER_THAN_THE_REFERENCE
            .iter()
            .map(|command| (*command).to_owned())
            .collect::<BTreeSet<String>>();
        let unlisted = self
            .stricter
            .difference(&ledger)
            .cloned()
            .collect::<Vec<_>>();
        assert!(
            unlisted.is_empty(),
            "{family}: commands diverging from the reference without a ledger entry: {unlisted:?}"
        );
        let requirement_ledger = REQUIREMENT_DIVERGENCES
            .iter()
            .map(|(command, pointer, _)| ((*command).to_owned(), (*pointer).to_owned()))
            .collect::<BTreeSet<_>>();
        let unlisted = self
            .requirement_diffs
            .difference(&requirement_ledger)
            .cloned()
            .collect::<Vec<_>>();
        assert!(
            unlisted.is_empty(),
            "{family}: requirements diverging from the reference without a ledger entry: {unlisted:?}"
        );
        eprintln!(
            "shell policy: {family} {}/{total} conforming, {} ledgered, {} requirement fields ledgered",
            self.conforming,
            self.stricter.len(),
            self.requirement_diffs.len()
        );
        assert!(
            self.untolerated.is_empty(),
            "{family}: resolutions that differ in another direction than the one tolerated \
             (`always` upstream, `ask` here): {:#?}",
            self.untolerated
        );
    }
}

/// US-110: the permission a command resolves to is the reference's, and every
/// requirement it raises carries the reference scope, patterns and label.
///
/// The ledgers are the one tolerated difference, and they are checked in both
/// directions: a listed case that stopped diverging fails, so the list cannot
/// rot.
#[test]
fn every_resolution_matches_the_reference() {
    let corpus = corpus();
    assert_eq!(corpus.resolutions.len(), corpus.counts.resolution_cases);
    let workspace = OperandWorkspace::build();
    let context = workspace.context();
    let lists = posix_lists();
    let mut replay = Replay::default();
    for case in &corpus.resolutions {
        let command = workspace.expand(&case.command);
        let analysis = analyze_shell(ShellFlavor::Posix, &command, &context, &lists);
        replay.compare(
            &case.command,
            &analysis,
            case.permission.as_deref(),
            &case.requirements,
            &lists,
            &workspace,
        );
    }
    // Every ledger entry is a resolution case, so the stale direction is read
    // against this family alone.
    let stale = STRICTER_THAN_THE_REFERENCE
        .iter()
        .filter(|command| !replay.stricter.contains(**command))
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "ledger entries that no longer diverge; remove them: {stale:?}"
    );
    let stale = REQUIREMENT_DIVERGENCES
        .iter()
        .map(|(command, pointer, _)| ((*command).to_owned(), (*pointer).to_owned()))
        .filter(|entry| !replay.requirement_diffs.contains(entry))
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "requirement ledger entries that no longer diverge; remove them: {stale:?}"
    );
    replay.finish("resolutions", corpus.counts.resolution_cases);
}

/// A git reader is granted in a repository whose configuration runs nothing,
/// and keyed to the repository it reads where the configuration could run a
/// helper.
#[test]
fn every_repository_resolution_matches_the_reference() {
    let corpus = corpus();
    assert_eq!(
        corpus.repository_resolutions.len(),
        corpus.counts.repository_cases
    );
    let lists = posix_lists();
    let mut replay = Replay::default();
    for (fixture, config) in REPOSITORY_FIXTURES {
        let workspace = OperandWorkspace::build();
        fs::create_dir_all(workspace.workdir.join(".git")).expect("git directory");
        fs::write(workspace.workdir.join(".git").join("config"), config).expect("git config");
        let context = workspace.context();
        for case in corpus
            .repository_resolutions
            .iter()
            .filter(|case| case.fixture == fixture)
        {
            let command = workspace.expand(&case.command);
            let analysis = analyze_shell(ShellFlavor::Posix, &command, &context, &lists);
            replay.compare(
                &format!("{fixture}: {}", case.command),
                &analysis,
                case.permission.as_deref(),
                &case.requirements,
                &lists,
                &workspace,
            );
        }
    }
    assert!(
        corpus
            .repository_resolutions
            .iter()
            .all(|case| REPOSITORY_FIXTURES
                .iter()
                .any(|(name, _)| *name == case.fixture)),
        "the corpus names a repository fixture this runner does not write"
    );
    replay.finish("repository resolutions", corpus.counts.repository_cases);
}

/// The managed resolver: a call's `cwd`, custom shell and custom environment
/// are read with its command, and the denylist also matches a basename.
#[test]
fn every_managed_resolution_matches_the_reference() {
    let corpus = corpus();
    assert_eq!(
        corpus.managed_resolutions.len(),
        corpus.counts.managed_cases
    );
    let workspace = OperandWorkspace::build();
    let lists = posix_lists();
    let mut replay = Replay::default();
    for case in &corpus.managed_resolutions {
        let cwd = case.cwd.as_deref().map(|cwd| workspace.expand(cwd));
        let context = workspace.context().managed(
            ShellFlavor::Posix,
            cwd.as_deref(),
            override_requirements(case.shell.as_deref(), &case.env),
        );
        let analysis = analyze_shell(ShellFlavor::Posix, &case.command, &context, &lists);
        replay.compare(
            &format!(
                "{} (cwd {:?}, shell {:?}, env {:?})",
                case.command, case.cwd, case.shell, case.env
            ),
            &analysis,
            case.permission.as_deref(),
            &case.requirements,
            &lists,
            &workspace,
        );
    }
    replay.finish("managed resolutions", corpus.counts.managed_cases);
}

/// The PowerShell grammar splits, tokenizes, names, matches and expands as the
/// reference does.
#[test]
fn every_windows_grammar_answer_matches_the_reference() {
    use super::windows;
    let corpus = corpus();
    let grammar = &corpus.windows_grammar;
    assert_eq!(grammar.commands.len(), corpus.counts.windows_grammar_cases);
    for case in &grammar.commands {
        let parts = windows::split_command_parts(&case.command)
            .into_iter()
            .map(|part| {
                let tokens = windows::split_command_tokens(&part);
                WindowsPart {
                    command_name: (!tokens.is_empty()).then(|| {
                        windows::windows_command_name(&windows::invoked_command(&tokens).0)
                    }),
                    forms: windows::command_match_forms(&part, true),
                    forms_without_basename: windows::command_match_forms(&part, false),
                    file_redirections: windows::file_redirection_targets(&part),
                    tokens,
                    part,
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(parts, case.parts, "`{}`", case.command.escape_debug());
    }
    for case in &grammar.patterns {
        assert_eq!(
            windows::policy_pattern_forms(&case.pattern),
            case.forms,
            "pattern `{}`",
            case.pattern
        );
    }
    let environment = WINDOWS_EXPANSION_ENVIRONMENT
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect::<Vec<_>>();
    let home = crate::config::user_home_directory()
        .map(|home| home.display().to_string())
        .unwrap_or_default();
    for case in &grammar.expansions {
        let (value, unresolved) =
            windows::expand_powershell_path(&case.token, "C:\\work", &environment);
        let value = if home.is_empty() {
            value
        } else {
            value.replace(&home, "<home>")
        };
        assert_eq!(
            (value, unresolved),
            (case.value.clone(), case.unresolved),
            "token `{}`",
            case.token
        );
    }
    for case in &grammar.tokens {
        assert_eq!(
            (
                windows::looks_like_option(&case.token),
                windows::looks_like_path(&case.token),
                windows::attached_parameter_value(&case.token),
            ),
            (case.option, case.path, case.attached_value.clone()),
            "token `{}`",
            case.token
        );
    }
    eprintln!(
        "shell policy: windows grammar {} commands, {} patterns, {} expansions, {} tokens",
        grammar.commands.len(),
        grammar.patterns.len(),
        grammar.expansions.len(),
        grammar.tokens.len()
    );
}

/// Input to a session is asked about exactly when the reference would, under
/// the same pattern.
#[test]
fn every_stdin_permission_matches_the_reference() {
    let corpus = corpus();
    assert_eq!(
        corpus.stdin_permissions.len(),
        corpus.counts.stdin_permission_cases
    );
    for case in &corpus.stdin_permissions {
        let flavor = match case.family.as_str() {
            "posix" => ShellFlavor::Posix,
            "git_bash" => ShellFlavor::GitBash,
            "powershell" => ShellFlavor::PowerShell,
            other => panic!("unknown family {other}"),
        };
        let context = pager_input_permission(
            flavor,
            "session_1",
            case.known.then_some(case.command.as_str()),
        );
        let requirements = context
            .requirements
            .iter()
            .map(|requirement| Requirement {
                scope: wire_scope(requirement.scope),
                invocation_pattern: requirement.invocation_pattern.clone(),
                session_pattern: requirement.session_pattern.clone(),
                label: Label::committed(
                    &requirement.label,
                    &requirement.invocation_pattern,
                    &requirement.session_pattern,
                ),
            })
            .collect::<Vec<_>>();
        let label = format!("{} `{}` (known {})", case.family, case.command, case.known);
        assert_eq!(
            context.permission.map(permission_wire),
            case.permission,
            "{label}"
        );
        assert_eq!(requirements, case.requirements, "{label}");
    }
    eprintln!(
        "shell policy: {} stdin permissions",
        corpus.stdin_permissions.len()
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
