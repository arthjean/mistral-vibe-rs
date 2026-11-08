//! Differential oracle for the shell policy.
//!
//! The Python reference is the authority on which commands a shell call
//! extracts, which of them have their path operands inspected, which paths a
//! call reaches outside the workspace, which of them the shell's allowlist
//! already grants, and what permission the whole thing resolves to.
//! `scripts/parity/shell_policy.py` asks it every one of those questions and
//! records the answers; this module replays them against [`extract_commands`],
//! [`inspects_paths`], [`path_grant_pattern_matches`] and [`analyze_shell`].
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

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::operands::inspects_paths;
use super::*;
use crate::parity::figures::{Figures, assert_figures};
use crate::parity::pinned_interpreter;
use crate::parity::replay::{
    Class, Entry as _, PatternDivergence, assert_ledger, differences, entries, pinned_reference,
    repository, scenario_matches,
};
use crate::platform::parse_policy_path;
use crate::policy::{PermissionScope, path_grant_pattern_matches};
use crate::tools::config::{ToolConfigResolver, shell_read_only_commands};

const CAPTURE_SCRIPT: &str = "scripts/parity/shell_policy.py";
const CORPUS_RELATIVE: &str = "tests/shell-policy/policy.json";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 5;

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

/// Every requirement field the requirement ledger admits, as
/// `(case, pointer)`.
fn requirement_ledger() -> BTreeSet<(String, String)> {
    REQUIREMENT_DIVERGENCES
        .iter()
        .map(|(command, pointer, _)| ((*command).to_owned(), (*pointer).to_owned()))
        .collect()
}

/// Requirement fields that differ on a case whose permission conforms and
/// that this port still owes, by case key and pointer into the case's
/// recorded resolution. Row 6 is what the corpus measures: these are gaps the
/// v3.1.0 re-pin revealed. Each entry counts its differences exactly, in the
/// family whose cases it names.
const REQUIREMENT_GAPS: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "git diff --no-index /etc/passwd /dev/null",
        pointer: "/requirements/0/*",
        count: 3,
        row: "6",
        class: Class::Gap,
        reason: STREAM_DEVICES_ARE_NOT_FILES,
    },
    PatternDivergence {
        scenario: "git diff --no-index /etc/passwd /dev/null",
        pointer: "/requirements/1",
        count: 1,
        row: "6",
        class: Class::Gap,
        reason: STREAM_DEVICES_ARE_NOT_FILES,
    },
];

/// Why the reference raises one requirement fewer for an operand naming a
/// stream device.
const STREAM_DEVICES_ARE_NOT_FILES: &str = "since 3.0 the reference reads the null device and \
     the standard streams (`/dev/null`, `/dev/stdin`, `/dev/stdout`, `/dev/stderr`, \
     `/dev/fd/<n>`) as streams rather than files (`is_stream_device_path` in \
     vibe/core/tools/utils.py), so `/dev/null` raises no outside-directory requirement and \
     only `/etc/passwd` does; this port still asks for both, so its first requirement names \
     `/dev/null` and it raises a second";

/// The key a repository case is replayed and ledgered under.
fn repository_key(case: &RepositoryCase) -> String {
    format!("{}: {}", case.fixture, case.command)
}

/// The key an allowlist grant case is replayed and ledgered under.
fn grant_key(case: &GrantCase) -> String {
    format!(
        "{} {:?}: {} (cwd {:?})",
        case.resolver, case.allowlist, case.command, case.cwd
    )
}

/// The key a managed case is replayed and ledgered under.
fn managed_key(case: &ManagedCase) -> String {
    format!(
        "{} (cwd {:?}, shell {:?}, env {:?})",
        case.command, case.cwd, case.shell, case.env
    )
}

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
    outside_paths: Vec<OutsideCase>,
    resolutions: Vec<ResolutionCase>,
    repository_resolutions: Vec<RepositoryCase>,
    managed_resolutions: Vec<ManagedCase>,
    windows_grammar: WindowsGrammar,
    allowlist_grants: Vec<GrantCase>,
    path_grant_matches: Vec<GrantMatchCase>,
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
    outside_path_cases: usize,
    resolution_cases: usize,
    repository_cases: usize,
    managed_cases: usize,
    windows_grammar_cases: usize,
    allowlist_grant_cases: usize,
    path_grant_match_cases: usize,
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
    paths: Vec<String>,
}

/// A call resolved with path grants in the shell's allowlist.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GrantCase {
    /// `legacy` or `managed`, the resolver that answered.
    resolver: String,
    allowlist: Vec<String>,
    command: String,
    cwd: Option<String>,
    permission: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "the reason text itself is reference prose")]
    has_reason: bool,
    #[serde(default)]
    requirements: Vec<Requirement>,
}

/// One verdict of reference `path_grant_pattern_matches`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantMatchCase {
    path: String,
    pattern: String,
    matches: bool,
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

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
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
    path_scope_root: Option<String>,
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
    assert!(
        crate::parity::is_pin_commit(&corpus.reference.commit),
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

/// Asserts the figures of a shell policy family whose every case is compared
/// for equality: `conformant` of `total`, nothing ledgered.
/// The corpus as the manifest lists it.
const CORPUS_LISTED: &str = "crates/vibe-core/tests/shell-policy/policy.json";

fn shell_figures(family: &str, conformant: usize, total: usize) {
    assert_figures(
        &format!("shell policy parity ({family}):"),
        CORPUS_LISTED,
        Figures {
            conformant,
            total,
            ..Figures::default()
        },
        serde_json::json!({}),
    );
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
    shell_figures("extraction", conforming, corpus.counts.extraction_cases);
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
    shell_figures("path commands", ported.len(), corpus.counts.path_commands);
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
        // The corpus climbs four levels out of the workspace and expects to
        // stand on the root, which a capture under `/tmp` does. The Windows
        // temporary directory sits deeper, so the tree hangs off its drive
        // root there instead.
        let root = if cfg!(windows) {
            let temporary = std::env::temp_dir();
            let drive = temporary.ancestors().last().unwrap_or(&temporary);
            tempfile::tempdir_in(drive)
        } else {
            tempfile::tempdir()
        }
        .expect("operand root");
        let workdir = root.path().join("workspace");
        let outside = root.path().join("elsewhere");
        let scratchpad = workdir.join(".vibe").join("scratchpad");
        fs::create_dir_all(outside.join("nested")).expect("outside tree");
        fs::create_dir_all(outside.join("approved")).expect("approved directory");
        fs::create_dir_all(workdir.join("nested")).expect("nested workdir");
        fs::create_dir_all(&scratchpad).expect("scratchpad");
        fs::write(workdir.join("inside.txt"), "inside").expect("inside file");
        fs::write(outside.join("secret.txt"), "secret").expect("outside file");
        fs::write(outside.join("nested").join("secret.txt"), "secret").expect("nested file");
        fs::write(scratchpad.join("note.txt"), "note").expect("scratchpad file");
        // The two directory links the capture's tree holds.
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&outside, workdir.join("escape")).expect("escape link");
            symlink(
                outside.join("nested"),
                outside.join("approved").join("link"),
            )
            .expect("approved link");
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::symlink_dir;
            symlink_dir(&outside, workdir.join("escape")).expect("escape link");
            symlink_dir(
                outside.join("nested"),
                outside.join("approved").join("link"),
            )
            .expect("approved link");
        }
        Self {
            _root: root,
            workdir,
            outside,
            scratchpad,
        }
    }

    /// The host paths each recorded placeholder stands for, longest first so a
    /// nested directory is substituted before its parent.
    ///
    /// On Windows a path is spelled with `/`, which Git Bash reads as it reads
    /// `\` and which its POSIX word splitting, like the reference's, leaves
    /// intact where it would consume a backslash.
    fn placeholders(&self) -> Vec<(&'static str, String)> {
        let mut pairs = vec![
            ("<scratchpad>", canonical(&self.scratchpad)),
            ("<outside>", canonical(&self.outside)),
            ("<workdir>", canonical(&self.workdir)),
        ];
        if let Some(home) = crate::platform::user_home() {
            pairs.push(("<home>", canonical(&home)));
        }
        if cfg!(windows) {
            for (_, host) in &mut pairs {
                *host = host.replace('\\', "/");
            }
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
    ///
    /// On Windows the reference names a resolved path with `\` and folds the
    /// case of an encoded grant (`docs/parity.md`, "Windows observations"), so
    /// every one of those spellings is replaced, and the separators after a
    /// placeholder are folded back to the `/` the corpus was captured with.
    fn normalize(&self, text: &str) -> String {
        let mut normalized = text.to_owned();
        for (placeholder, host) in self.placeholders() {
            normalized = normalized.replace(&host, placeholder);
            if cfg!(windows) {
                let native = host.replace('/', "\\");
                for spelling in [native.to_lowercase(), native, host.to_lowercase()] {
                    normalized = normalized.replace(&spelling, placeholder);
                }
            }
        }
        if cfg!(windows) {
            normalized = fold_placeholder_separators(&normalized);
        }
        normalized
    }

    fn context(&self) -> ShellPolicyContext {
        let platform = if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Posix
        };
        ShellPolicyContext::new(
            platform,
            parse_policy_path(platform, &canonical(&self.workdir)).expect("workdir"),
        )
        .with_scratchpad(Some(self.scratchpad.clone()))
    }
}

/// The shell family a replay runs under: the one the capture ran on Linux, and
/// on Windows the Git Bash family a host with Git Bash publishes.
fn replay_flavor() -> ShellFlavor {
    if cfg!(windows) {
        ShellFlavor::GitBash
    } else {
        ShellFlavor::Posix
    }
}

/// `text` with every `\` in the path run that follows a placeholder turned
/// into `/`, the run ending where a path cannot continue.
///
/// A root without a drive (`/etc/passwd`) resolves on the current drive, as
/// `Path.resolve()` anchors it on Windows, and a climb out of the workspace
/// stops at the root of the drive the operand tree hangs off, the temporary
/// directory's. The two differ on a host whose checkout and temporary
/// directory sit on separate drives (`D:` and `C:` on windows-2022), so both
/// roots are folded back to the bare `/` the corpus records.
fn fold_placeholder_separators(text: &str) -> String {
    let drive = |path: Option<std::path::PathBuf>| {
        path.and_then(|path| {
            path.to_str()
                .and_then(|path| path.get(..3))
                .map(str::to_owned)
        })
        .unwrap_or_default()
    };
    let mut drives = vec![
        drive(std::env::current_dir().ok()),
        drive(Some(std::env::temp_dir())),
    ];
    drives.retain(|drive| !drive.is_empty());
    drives.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    let mut text = text.to_owned();
    for drive in &drives {
        text = text.replace(drive, "/").replace(&drive.to_lowercase(), "/");
    }
    let mut folded = String::with_capacity(text.len());
    let mut in_path = false;
    for character in text.chars() {
        if character == '/' {
            in_path = true;
        }
        match character {
            '>' => {
                in_path = ["<scratchpad", "<outside", "<workdir", "<home"]
                    .iter()
                    .any(|opening| folded.ends_with(opening));
            }
            ' ' | ')' | ']' | '|' | '\'' | '"' | '\n' | '<' => in_path = false,
            '\\' if in_path => {
                folded.push('/');
                continue;
            }
            _ => {}
        }
        folded.push(character);
    }
    folded
}

fn canonical(path: &Path) -> String {
    crate::platform::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// US-111: the paths a call reaches outside the workspace are the
/// reference's, each named by where it resolves.
#[test]
fn every_escaping_operand_matches_the_reference() {
    let corpus = corpus();
    assert_eq!(corpus.outside_paths.len(), corpus.counts.outside_path_cases);
    let workspace = OperandWorkspace::build();
    let context = workspace.context();
    let lists = posix_lists();

    for case in &corpus.outside_paths {
        let command = workspace.expand(&case.command);
        let analysis = analyze_shell(replay_flavor(), &command, &context, &lists);
        let reached = analysis
            .requirements
            .iter()
            .filter(|requirement| requirement.scope == PermissionScope::OutsideDirectory)
            .map(|requirement| workspace.normalize(&requirement.invocation_pattern))
            .collect::<Vec<_>>();
        assert_eq!(
            reached, case.paths,
            "`{}` reaches different paths here",
            case.command
        );
    }
    eprintln!(
        "shell policy: escaping operands {}/{}",
        corpus.outside_paths.len(),
        corpus.counts.outside_path_cases
    );
    shell_figures(
        "escaping operands",
        corpus.outside_paths.len(),
        corpus.counts.outside_path_cases,
    );
}

/// What replaying one family of resolutions found.
#[derive(Default)]
struct Replay {
    /// Every case key compared, which is what scopes a ledger entry's stale
    /// direction to the family that replays it.
    compared: BTreeSet<String>,
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
        self.compared.insert(key.to_owned());
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
                    path_scope_root: requirement
                        .path_scope_root
                        .as_deref()
                        .map(|root| workspace.normalize(root)),
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
                (
                    "pathScopeRoot",
                    here.path_scope_root == upstream.path_scope_root,
                ),
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
        let requirement_ledger = requirement_ledger();
        // The typed gaps that name a case this family compared: the others
        // belong to another family's replay.
        let gaps = REQUIREMENT_GAPS
            .iter()
            .filter(|entry| {
                self.compared
                    .iter()
                    .any(|key| scenario_matches(entry.scenario, key))
            })
            .collect::<Vec<_>>();
        let mut gap_covered = vec![0_usize; gaps.len()];
        let mut gap_cases = BTreeSet::new();
        let mut unlisted = Vec::new();
        for (key, pointer) in self.requirement_diffs.difference(&requirement_ledger) {
            match gaps.iter().position(|entry| entry.covers(key, pointer)) {
                Some(index) => {
                    gap_covered[index] += 1;
                    gap_cases.insert(key.clone());
                }
                None => unlisted.push((key.clone(), pointer.clone())),
            }
        }
        assert!(
            unlisted.is_empty(),
            "{family}: requirements diverging from the reference without a ledger entry: {unlisted:?}"
        );
        let miscounted = gaps
            .iter()
            .zip(&gap_covered)
            .filter(|(entry, found)| entry.count != **found)
            .map(|(entry, found)| {
                format!(
                    "{} declares {} differences and covered {found}",
                    entry.label(),
                    entry.count
                )
            })
            .collect::<Vec<_>>();
        assert!(
            miscounted.is_empty(),
            "{family}: requirement gaps that cover another number of differences than they \
             declare: {miscounted:?}"
        );
        let null = serde_json::Value::Null;
        let recorded: BTreeMap<String, &serde_json::Value> = self
            .compared
            .iter()
            .map(|key| (key.clone(), &null))
            .collect();
        assert_ledger(
            CORPUS_LISTED,
            &recorded,
            gaps.iter()
                .map(|entry| *entry as &dyn crate::parity::replay::Entry),
        );
        let stale = requirement_ledger
            .iter()
            .filter(|(key, _)| self.compared.contains(key))
            .filter(|entry| !self.requirement_diffs.contains(*entry))
            .collect::<Vec<_>>();
        assert!(
            stale.is_empty(),
            "{family}: requirement ledger entries that no longer diverge; remove them: {stale:?}"
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

        // A case differs when it resolves stricter or raises a requirement
        // field the reference does not. The two untyped ledgers hold decided
        // divergences; a case with a field under a typed gap is not at parity.
        let differing = self
            .stricter
            .iter()
            .cloned()
            .chain(self.requirement_diffs.iter().map(|(key, _)| key.clone()))
            .collect::<BTreeSet<String>>();
        let gap_differences = gap_covered.iter().sum::<usize>();
        assert_figures(
            &format!("shell policy parity ({family}):"),
            CORPUS_LISTED,
            Figures {
                conformant: total - differing.len(),
                accepted: differing.len() - gap_cases.len(),
                total,
                accepted_differences: self.stricter.len() + self.requirement_diffs.len()
                    - gap_differences,
                gap_differences,
                prose_differences: 0,
            },
            serde_json::json!({}),
        );
    }
}

/// Every requirement ledger entry names a case one family replays, so the
/// per-family stale check in [`Replay::finish`] reads every entry somewhere.
#[test]
fn every_requirement_ledger_entry_names_a_corpus_case() {
    let corpus = corpus();
    let keys = corpus
        .resolutions
        .iter()
        .map(|case| case.command.clone())
        .chain(corpus.repository_resolutions.iter().map(repository_key))
        .chain(corpus.managed_resolutions.iter().map(managed_key))
        .chain(corpus.allowlist_grants.iter().map(grant_key))
        .collect::<BTreeSet<_>>();
    let orphans = requirement_ledger()
        .into_iter()
        .filter(|(key, _)| !keys.contains(key))
        .collect::<Vec<_>>();
    assert!(
        orphans.is_empty(),
        "requirement ledger entries naming no corpus case: {orphans:?}"
    );
    eprintln!(
        "shell policy: {} requirement fields ledgered",
        requirement_ledger().len()
    );
}

/// US-110: the permission a command resolves to is the reference's, and every
/// requirement it raises carries the reference scope, patterns and label.
///
/// The ledgers are the one tolerated difference, and they are checked in both
/// directions: a listed case that stopped diverging fails, so the list cannot
/// rot.
#[test]
fn every_resolution_matches_the_reference() {
    crate::parity::manifest::assert_corpus("crates/vibe-core/tests/shell-policy/policy.json");
    let corpus = corpus();
    assert_eq!(corpus.resolutions.len(), corpus.counts.resolution_cases);
    let workspace = OperandWorkspace::build();
    let context = workspace.context();
    let lists = posix_lists();
    let mut replay = Replay::default();
    for case in &corpus.resolutions {
        let command = workspace.expand(&case.command);
        let analysis = analyze_shell(replay_flavor(), &command, &context, &lists);
        replay.compare(
            &case.command,
            &analysis,
            case.permission.as_deref(),
            &case.requirements,
            &lists,
            &workspace,
        );
    }
    // Every stricter entry is a resolution case, so its stale direction is read
    // against this family alone; the requirement ledgers are read per family in
    // `Replay::finish`.
    let stale = STRICTER_THAN_THE_REFERENCE
        .iter()
        .filter(|command| !replay.stricter.contains(**command))
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "ledger entries that no longer diverge; remove them: {stale:?}"
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
            let analysis = analyze_shell(replay_flavor(), &command, &context, &lists);
            replay.compare(
                &repository_key(case),
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
            replay_flavor(),
            cwd.as_deref(),
            override_requirements(case.shell.as_deref(), &case.env),
        );
        let analysis = analyze_shell(replay_flavor(), &case.command, &context, &lists);
        replay.compare(
            &managed_key(case),
            &analysis,
            case.permission.as_deref(),
            &case.requirements,
            &lists,
            &workspace,
        );
    }
    replay.finish("managed resolutions", corpus.counts.managed_cases);
}

/// A path grant in the shell's allowlist clears the outside paths it covers,
/// and nothing else in the allowlist does, under both resolvers.
#[test]
fn every_allowlist_grant_resolution_matches_the_reference() {
    let corpus = corpus();
    assert_eq!(
        corpus.allowlist_grants.len(),
        corpus.counts.allowlist_grant_cases
    );
    let workspace = OperandWorkspace::build();
    let mut replay = Replay::default();
    for case in &corpus.allowlist_grants {
        let mut lists = posix_lists();
        lists.allowlist = case
            .allowlist
            .iter()
            .map(|entry| workspace.expand(entry))
            .collect();
        let command = workspace.expand(&case.command);
        let context = match case.resolver.as_str() {
            "legacy" => workspace.context(),
            "managed" => {
                let cwd = case.cwd.as_deref().map(|cwd| workspace.expand(cwd));
                workspace
                    .context()
                    .managed(replay_flavor(), cwd.as_deref(), Vec::new())
            }
            other => panic!("unknown resolver {other}"),
        };
        let analysis = analyze_shell(replay_flavor(), &command, &context, &lists);
        replay.compare(
            &grant_key(case),
            &analysis,
            case.permission.as_deref(),
            &case.requirements,
            &lists,
            &workspace,
        );
    }
    replay.finish("allowlist grants", corpus.counts.allowlist_grant_cases);
}

/// Only an encoded grant or an absolute path glob reads as a path grant, and
/// then as the reference matches it.
#[test]
fn every_path_grant_verdict_matches_the_reference() {
    let corpus = corpus();
    assert_eq!(
        corpus.path_grant_matches.len(),
        corpus.counts.path_grant_match_cases
    );
    let differing = corpus
        .path_grant_matches
        .iter()
        .filter(|case| path_grant_pattern_matches(&case.path, &case.pattern) != case.matches)
        .map(|case| {
            format!(
                "{} against {}: {} upstream",
                case.path, case.pattern, case.matches
            )
        })
        .collect::<Vec<_>>();
    assert!(
        differing.is_empty(),
        "path grant verdicts differ: {differing:#?}"
    );
    eprintln!(
        "shell policy: path grant verdicts {}/{}",
        corpus.path_grant_matches.len(),
        corpus.counts.path_grant_match_cases
    );
    shell_figures(
        "path grant verdicts",
        corpus.path_grant_matches.len(),
        corpus.counts.path_grant_match_cases,
    );
}

/// Where this port's PowerShell grammar still answers otherwise than the
/// reference, by unit (`commands/`, `patterns/`, `expansions/` or `tokens/`
/// and the case it replays) and by the field that differs. Row 6 is what the
/// corpus measures: these are gaps the v3.1.0 re-pin revealed, which the
/// grammar in `windows.rs` owes. Since 3.0 the reference splits commands and
/// arguments with a tokenizer modeled on PowerShell's own command mode
/// (`vibe/core/tools/builtins/_powershell_lexer.py`), where this port still
/// splits the way the 2.x helpers of `windows_shell.py` did.
const WINDOWS_GRAMMAR_LEDGER: &[PatternDivergence] = &[
    redirection("commands/cmd 2>&1 | more", 3),
    redirection("commands/Get-Content x *>&1", 3),
    redirection("commands/type > out.txt", 6),
    redirection("commands/echo a > $null", 4),
    redirection("commands/echo a > NUL", 4),
    redirection("commands/echo a 2> 'err log.txt'", 4),
    redirection("commands/echo a >> C:\\logs\\x.txt", 4),
    redirection("commands/echo a >&2", 3),
    redirection("commands/echo a >`\"q`\" x", 4),
    PatternDivergence {
        scenario: "commands/Invoke-Command { Remove-Item x; ls }",
        pointer: "/parts/0/**",
        count: 5,
        row: "6",
        class: Class::Gap,
        reason: "the reference reads a braced script block as one argument \
                 (`_Lexer._group` in _powershell_lexer.py), where this port splits it on \
                 whitespace into six",
    },
    PatternDivergence {
        scenario: "commands/(Get-Item a).Name",
        pointer: "/parts/0/**",
        count: 5,
        row: "6",
        class: Class::Gap,
        reason: "the reference reads a parenthesized expression as one argument and the \
                 member access after it as the next (`_Lexer._group`), so the command name \
                 is the whole group; this port splits inside the parentheses",
    },
    PatternDivergence {
        scenario: "commands/echo `& not a separator",
        pointer: "/parts/0/**",
        count: 3,
        row: "6",
        class: Class::Gap,
        reason: "the reference resolves a backtick escape inside an argument \
                 (`_Lexer._backtick`), so the escaped ampersand reads as `&`; this port \
                 keeps the backtick in the token and in the match forms",
    },
    PatternDivergence {
        scenario: "commands/echo a ^& b",
        pointer: "/parts/0/**",
        count: 5,
        row: "6",
        class: Class::Gap,
        reason: CARET_IS_NOT_AN_ESCAPE,
    },
    PatternDivergence {
        scenario: "commands/echo a ^& b",
        pointer: "/parts/1",
        count: 1,
        row: "6",
        class: Class::Gap,
        reason: CARET_IS_NOT_AN_ESCAPE,
    },
    PatternDivergence {
        scenario: "commands/@{a=1}; ${x}",
        pointer: "/parts/2",
        count: 1,
        row: "6",
        class: Class::Gap,
        reason: "the reference also lists the statement inside a hash literal as a part \
                 of its own (`_Lexer._record_subexpressions`, `split_powershell_commands`), \
                 so `a=1` is a third part; this port keeps it inside the literal",
    },
];

/// Why a caret splits a PowerShell command in the reference and not here.
const CARET_IS_NOT_AN_ESCAPE: &str = "a caret escapes nothing in PowerShell, so the reference \
                                      ends the part at the ampersand after it and reads `b` \
                                      as a second command (`_ARGUMENT_BREAKS` in \
                                      _powershell_lexer.py); this port treats `^&` as an \
                                      escaped character and keeps one part";

/// A command whose redirection the reference strips from its arguments and
/// match forms, with how many differences that makes in it.
const fn redirection(scenario: &'static str, count: usize) -> PatternDivergence {
    PatternDivergence {
        scenario,
        pointer: "/parts/0/**",
        count,
        row: "6",
        class: Class::Gap,
        reason: "the reference lexes a redirection operator and its target out of the \
                 arguments (`_Lexer._redirect`, `split_powershell_arguments` in \
                 _powershell_lexer.py), so neither reaches the tokens or the match forms \
                 `_windows_command_match_forms` builds from them; this port keeps both as \
                 arguments",
    }
}

/// One unit of the Windows grammar replay: its scenario name, what the
/// reference answered and what this port answers.
type GrammarUnit = (String, serde_json::Value, serde_json::Value);

/// What this port answers for every case of the Windows grammar corpus, beside
/// what the reference answered.
fn windows_grammar_units(grammar: &WindowsGrammar) -> Vec<GrammarUnit> {
    use super::windows;
    let mut units = Vec::new();
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
        units.push((
            format!("commands/{}", case.command),
            serde_json::json!({ "parts": case.parts }),
            serde_json::json!({ "parts": parts }),
        ));
    }
    for case in &grammar.patterns {
        units.push((
            format!("patterns/{}", case.pattern),
            serde_json::json!({ "forms": case.forms }),
            serde_json::json!({ "forms": windows::policy_pattern_forms(&case.pattern) }),
        ));
    }
    let environment = WINDOWS_EXPANSION_ENVIRONMENT
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect::<Vec<_>>();
    let home = crate::platform::user_home()
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
        units.push((
            format!("expansions/{}", case.token),
            serde_json::json!({ "value": case.value, "unresolved": case.unresolved }),
            serde_json::json!({ "value": value, "unresolved": unresolved }),
        ));
    }
    for case in &grammar.tokens {
        units.push((
            format!("tokens/{}", case.token),
            serde_json::json!({
                "option": case.option,
                "path": case.path,
                "attachedValue": case.attached_value,
            }),
            serde_json::json!({
                "option": windows::looks_like_option(&case.token),
                "path": windows::looks_like_path(&case.token),
                "attachedValue": windows::attached_parameter_value(&case.token),
            }),
        ));
    }
    units
}

/// The PowerShell grammar splits, tokenizes, names, matches and expands as the
/// reference does, except where [`WINDOWS_GRAMMAR_LEDGER`] records a gap.
/// Every unit is compared in full, so a gap in one field never hides a
/// difference in another.
#[test]
fn every_windows_grammar_answer_matches_the_reference() {
    let corpus = corpus();
    let grammar = &corpus.windows_grammar;
    assert_eq!(grammar.commands.len(), corpus.counts.windows_grammar_cases);
    let units = windows_grammar_units(grammar);
    let mut covered = vec![0_usize; WINDOWS_GRAMMAR_LEDGER.len()];
    let mut unexplained = Vec::new();
    let mut conformant = 0_usize;
    for (scenario, reference, port) in &units {
        let found = differences(reference, port);
        if found.is_empty() {
            conformant += 1;
            continue;
        }
        for pointer in found {
            match WINDOWS_GRAMMAR_LEDGER
                .iter()
                .position(|entry| entry.covers(scenario, &pointer))
            {
                Some(index) => covered[index] += 1,
                None => unexplained.push(format!(
                    "{} {pointer}\n  reference: {}\n  port:      {}",
                    scenario.escape_debug(),
                    reference
                        .pointer(&pointer)
                        .map_or_else(|| "absent".to_owned(), ToString::to_string),
                    port.pointer(&pointer)
                        .map_or_else(|| "absent".to_owned(), ToString::to_string),
                )),
            }
        }
    }
    assert!(
        unexplained.is_empty(),
        "{} differences in the Windows grammar fall under no ledger entry:\n{}",
        unexplained.len(),
        unexplained.join("\n")
    );
    let miscounted: Vec<String> = WINDOWS_GRAMMAR_LEDGER
        .iter()
        .zip(&covered)
        .filter(|(entry, found)| entry.count != **found)
        .map(|(entry, found)| {
            format!(
                "{} declares {} differences and covered {found}",
                entry.label(),
                entry.count
            )
        })
        .collect();
    assert!(miscounted.is_empty(), "{}", miscounted.join("\n"));
    let recorded: BTreeMap<String, &serde_json::Value> = units
        .iter()
        .map(|(scenario, reference, _)| (scenario.clone(), reference))
        .collect();
    assert_ledger(CORPUS_LISTED, &recorded, entries(WINDOWS_GRAMMAR_LEDGER));
    eprintln!(
        "shell policy: windows grammar {} commands, {} patterns, {} expansions, {} tokens",
        grammar.commands.len(),
        grammar.patterns.len(),
        grammar.expansions.len(),
        grammar.tokens.len()
    );
    assert_figures(
        "shell policy parity (windows grammar):",
        CORPUS_LISTED,
        Figures {
            conformant,
            total: units.len(),
            gap_differences: covered.iter().sum(),
            ..Figures::default()
        },
        serde_json::json!({}),
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
    // The capture script has no `--check` mode, so the probe recaptures into a
    // scratch file and compares it here rather than through `Capture::probe`.
    let Some(root) = pinned_reference("shell policy") else {
        return;
    };
    let Some(interpreter) = pinned_interpreter(&root) else {
        eprintln!("skipping the live shell-policy probe: no reference interpreter");
        return;
    };
    let workspace = repository();
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
