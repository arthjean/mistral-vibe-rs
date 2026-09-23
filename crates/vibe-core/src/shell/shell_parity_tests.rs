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

use std::collections::{BTreeMap, BTreeSet};
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
const CORPUS_SCHEMA_VERSION: u32 = 2;

/// The commands the reference resolves to `always` and this port asks about.
///
/// Empty at 2.25.7: the three entries it held at 2.24.0 (`cat $(which ls)`,
/// `cat file.txt > out.txt` and `git diff --no-index /etc/passwd /dev/null`)
/// stopped diverging, because the reference now asks about all three too
/// (`vibe/core/tools/builtins/bash.py:666-715`).
const STRICTER_THAN_THE_REFERENCE: [&str; 0] = [];

/// Since 2.25.5 (c069ffa1), syntax that stops the extracted parts from
/// describing what runs (a heredoc, a redirect to a file, a parse error)
/// invalidates the scope: the reference drops every per-part requirement and
/// raises one literal requirement for the whole command text, labeled with the
/// approval label (`vibe/core/tools/builtins/bash.py:352-369,372-390,701-709`;
/// `vibe/core/tools/builtins/_shell_permission_analysis.py:181-183,301-303,
/// 317-325`). 2.25.4 (19b5b74f) had already made these constructs approval
/// reasons, but appended the whole-command requirement beside the per-part
/// ones instead of replacing them. This port still raises the per-part
/// requirement it raised at 2.24.0, `<program> <redirect>` under
/// `<program> *`.
const SCOPE_INVALIDATED: &str = "reference raises one literal whole-command \
     requirement for scope-invalidating syntax and no per-part one (2.25.5, \
     bash.py:352-369,701-709); this port keeps the per-part `<redirect>` \
     requirement under an arity session pattern";

/// Since 2.25.5, a command with option guardrails (`git`, `find`, `sort`,
/// `tree` and eleven more) is granted under its own literal text rather than an
/// arity pattern, so `git reset *` can no longer cover an option the guardrail
/// would ask about (`vibe/core/tools/builtins/bash.py:338-349`;
/// `vibe/core/tools/builtins/_shell_command_policy.py:843-863`). This port
/// still records the arity pattern and labels the requirement with it.
const LITERAL_GUARDRAILED_SESSION: &str = "reference records an option-guardrailed \
     command under its literal text (2.25.5, bash.py:338-349); this port records the \
     arity session pattern and labels the requirement with it";

/// `{}` is a brace-expansion approval reason
/// (`vibe/core/tools/builtins/_shell_permission_analysis.py:157-160`), and
/// since 2.25.5 (c069ffa1) an approval reason makes the reference include
/// allowlisted parts (`scoped_command_parts`, `bash.py:352-369`), so the
/// literal `find` requirement is raised once by `_build_required_permissions`
/// and again by the guardrail, with no deduplication across the two lists
/// (`vibe/core/tools/builtins/bash.py:612-664,693-700`). This port raises it
/// once, and it equals the reference's first requirement, so only the second
/// one is ledgered.
const DUPLICATED_FIND_REQUIREMENT: &str = "reference raises the literal find \
     requirement twice, once per part and once from the guardrail (2.25.5, \
     bash.py:612-664,693-700); this port raises it once";

/// Since 2.25.4 (19b5b74f), `git diff --no-index` nominates its positional
/// operands as path candidates
/// (`vibe/core/tools/builtins/_shell_command_policy.py:608`), so the reference
/// asks with two outside_directory requirements, `/dev/*` then `/etc/*`, and no
/// command requirement, because `git diff` is allowlisted and the text carries
/// no approval reason (`vibe/core/tools/builtins/bash.py:612-664`, the globs at
/// 661-662). This port asks with one command requirement for the segment under
/// the session pattern `git diff *` and raises no outside_directory
/// requirement.
const NO_INDEX_OUTSIDE_DIRECTORIES: &str = "reference asks with outside_directory \
     `/dev/*` and `/etc/*` for `git diff --no-index` operands (2.25.4, \
     _shell_command_policy.py:608, bash.py:661-662); this port asks with one \
     command requirement under `git diff *` and no outside_directory one";

/// A parse error invalidates the scope on both sides, and both raise the same
/// literal whole-command requirement after whatever the guardrail raised
/// (`vibe/core/tools/builtins/_shell_permission_analysis.py:301-303`;
/// `vibe/core/tools/builtins/bash.py:372-390,700-709`). Only the label differs:
/// the reference prints its own approval wording, which the licensing boundary
/// keeps out of this repository, and this port prints original wording naming
/// the same cause.
const UNSCOPED_LABEL: &str = "the whole-command requirement for a parse error carries \
     this port's own wording where the reference prints its approval label \
     (_shell_permission_analysis.py:181-183, bash.py:700-709)";

/// Requirement fields that differ on a case whose permission conforms, as
/// `(command, pointer, reason)`, the pointer reaching into the case's recorded
/// resolution. The fields of the requirements both sides raise are compared
/// pairwise; a bare `/requirements/<index>` pointer names a requirement that
/// only one side raises.
const REQUIREMENT_DIVERGENCES: &[(&str, &str, &str)] = &[
    (
        "python3 <<'EOF'\nprint(1)\nEOF",
        "/requirements/0/invocationPattern",
        SCOPE_INVALIDATED,
    ),
    (
        "python3 <<'EOF'\nprint(1)\nEOF",
        "/requirements/0/sessionPattern",
        SCOPE_INVALIDATED,
    ),
    (
        "python3 <<'EOF'\nprint(1)\nEOF",
        "/requirements/0/label",
        SCOPE_INVALIDATED,
    ),
    (
        "cat file.txt > out.txt",
        "/requirements/0/invocationPattern",
        SCOPE_INVALIDATED,
    ),
    (
        "cat file.txt > out.txt",
        "/requirements/0/sessionPattern",
        SCOPE_INVALIDATED,
    ),
    (
        "cat file.txt > out.txt",
        "/requirements/0/label",
        SCOPE_INVALIDATED,
    ),
    (
        "find . -exec rm {} ;",
        "/requirements/1",
        DUPLICATED_FIND_REQUIREMENT,
    ),
    (
        "find . -execdir rm {} ;",
        "/requirements/1",
        DUPLICATED_FIND_REQUIREMENT,
    ),
    (
        "find . -ok rm {} ;",
        "/requirements/1",
        DUPLICATED_FIND_REQUIREMENT,
    ),
    (
        "find . -okdir rm {} ;",
        "/requirements/1",
        DUPLICATED_FIND_REQUIREMENT,
    ),
    ("cat 'unterminated", "/requirements/0/label", UNSCOPED_LABEL),
    (
        "find . -exec rm {} ; && find . -exec rm {} ;",
        "/requirements/1/label",
        UNSCOPED_LABEL,
    ),
    (
        "git -c core.pager=sh log",
        "/requirements/0/sessionPattern",
        LITERAL_GUARDRAILED_SESSION,
    ),
    (
        "git -c core.pager=sh log",
        "/requirements/0/label",
        LITERAL_GUARDRAILED_SESSION,
    ),
    (
        "git reset --hard",
        "/requirements/0/sessionPattern",
        LITERAL_GUARDRAILED_SESSION,
    ),
    (
        "git reset --hard",
        "/requirements/0/label",
        LITERAL_GUARDRAILED_SESSION,
    ),
    (
        "git reset --hard -- src",
        "/requirements/0/sessionPattern",
        LITERAL_GUARDRAILED_SESSION,
    ),
    (
        "git reset --hard -- src",
        "/requirements/0/label",
        LITERAL_GUARDRAILED_SESSION,
    ),
    (
        "git diff --no-index /etc/passwd /dev/null",
        "/requirements/0/scope",
        NO_INDEX_OUTSIDE_DIRECTORIES,
    ),
    (
        "git diff --no-index /etc/passwd /dev/null",
        "/requirements/0/invocationPattern",
        NO_INDEX_OUTSIDE_DIRECTORIES,
    ),
    (
        "git diff --no-index /etc/passwd /dev/null",
        "/requirements/0/sessionPattern",
        NO_INDEX_OUTSIDE_DIRECTORIES,
    ),
    (
        "git diff --no-index /etc/passwd /dev/null",
        "/requirements/0/label",
        NO_INDEX_OUTSIDE_DIRECTORIES,
    ),
    (
        "git diff --no-index /etc/passwd /dev/null",
        "/requirements/1",
        NO_INDEX_OUTSIDE_DIRECTORIES,
    ),
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
    let mut requirement_diffs = BTreeSet::new();
    let mut untolerated = Vec::new();
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
            // Collected rather than asserted in place, so the ledgers below are
            // still checked; the assertion at the end is as strict.
            if !(expected == "always" && resolved == "ask") {
                untolerated.push(format!(
                    "`{}` resolves to `{resolved}` here and to `{expected}` upstream: {:?}",
                    case.command.escape_debug(),
                    analysis.rationale
                ));
                continue;
            }
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
                label: Label::committed(
                    &requirement.label,
                    &requirement.invocation_pattern,
                    &requirement.session_pattern,
                ),
            })
            .collect::<Vec<_>>();
        // A requirement only one side raises is its own divergence; the ones
        // both sides raise are still compared field by field.
        let shared = raised.len().min(case.requirements.len());
        for index in shared..raised.len().max(case.requirements.len()) {
            requirement_diffs.insert((case.command.clone(), format!("/requirements/{index}")));
            eprintln!(
                "`{}` /requirements/{index}: {:?} here, {:?} upstream",
                case.command.escape_debug(),
                raised.get(index),
                case.requirements.get(index)
            );
        }
        for (index, (here, upstream)) in raised.iter().zip(&case.requirements).enumerate() {
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
                    requirement_diffs.insert((
                        case.command.clone(),
                        format!("/requirements/{index}/{field}"),
                    ));
                    eprintln!(
                        "`{}` /requirements/{index}/{field}: {here:?} here, {upstream:?} upstream",
                        case.command.escape_debug()
                    );
                }
            }
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

    let requirement_ledger = REQUIREMENT_DIVERGENCES
        .iter()
        .map(|(command, pointer, _)| ((*command).to_owned(), (*pointer).to_owned()))
        .collect::<BTreeSet<_>>();
    let unlisted = requirement_diffs
        .difference(&requirement_ledger)
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        unlisted.is_empty(),
        "requirements diverging from the reference without a ledger entry: {unlisted:?}"
    );
    let stale = requirement_ledger
        .difference(&requirement_diffs)
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "requirement ledger entries that no longer diverge; remove them: {stale:?}"
    );
    eprintln!(
        "shell policy: resolutions {conforming}/{} conforming, {} ledgered, \
         {} requirement fields ledgered",
        corpus.counts.resolution_cases,
        diverging.len(),
        requirement_diffs.len()
    );
    assert!(
        untolerated.is_empty(),
        "resolutions that differ in another direction than the one tolerated \
         (`always` upstream, `ask` here): {untolerated:#?}"
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
