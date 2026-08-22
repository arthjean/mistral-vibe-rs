//! Differential oracle for the managed worktree contract.
//!
//! `scripts/parity/worktree.py` drove the pinned reference's own worktree
//! functions over eight scripted repositories and committed what they answered
//! to `crates/vibe-core/tests/worktree/corpus.json`. This module rebuilds those
//! same repositories in Rust, drives [`super`] over them, projects both sides
//! the same way, and diffs them as JSON pointers.
//!
//! The replay is unconditional. The corpus carries names, pointers, counts and
//! digests and no reference prose, so CI reports a conformance count instead of
//! skipping for want of a checkout. The one test that does need the checkout is
//! the live probe at the bottom, which recaptures and asserts the committed
//! corpus is still what the pinned reference answers.
//!
//! The surviving divergence is held in [`LEDGER`], one entry per known gap with
//! the story that closes it. A divergence outside the ledger fails the suite,
//! and so does a ledger entry whose divergence has been fixed: a ledger that
//! cannot rot is the only kind worth keeping. EP-086 is instrument-first, so
//! every entry here is expected to be deleted by a later epic rather than to
//! live on.
//!
//! # What "hermetic" means on this side
//!
//! The capture script cuts this machine's git configuration off with
//! `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM` and a fixed identity. This module
//! cannot: `std::env::set_var` is `unsafe` under edition 2024, `unsafe_code` is
//! forbidden workspace-wide, and the environment is process-wide anyway while
//! these tests run beside every other test in the crate. The isolation is done
//! one level down instead, in each scripted repository's own `--local` config,
//! which outranks the global file for every key that could change a verdict:
//! the identity, `commit.gpgsign`, `core.hooksPath` so a developer's hooks
//! cannot run on `worktree add`, and `core.excludesFile` so a global ignore
//! file cannot hide the untracked fixture a cleanup case depends on. The
//! initial branch is passed to `git init` rather than read from
//! `init.defaultBranch`. What stays out of reach is a caller who exported
//! `GIT_DIR` or `GIT_WORK_TREE` into `cargo test` itself, which nothing here
//! can defend against and nothing in CI does.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};

use super::{
    inspect_worktree_for_cleanup, managed_worktree_root, prepare_worktree, target_cwd,
    validate_branch_name, validate_worktree_name,
};

const CORPUS_RELATIVE: &str = "crates/vibe-core/tests/worktree/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/worktree.py";

/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 1;

/// The case floor EP-086 commits to, so a regeneration that captured almost
/// nothing fails instead of reporting a clean but empty run.
const MINIMUM_CASES: usize = 60;
/// The per-family floor, so a corpus cannot reach the total above by driving
/// one family sixty times.
const MINIMUM_CASES_PER_FAMILY: usize = 4;

/// Every family the corpus names, so a family the replay has no builder for is
/// a named failure rather than a smaller green run.
const FAMILIES: &[&str] = &[
    "cleanup",
    "list",
    "managedRoot",
    "name",
    "prepare",
    "targetCwd",
];

/// Every scripted repository the capture builds, mirroring `SETUPS` in the
/// capture script. The corpus commits its own copy and the replay asserts the
/// two agree, so a setup added on one side cannot go unbuilt on the other.
const SETUPS: &[&str] = &[
    "plain",
    "untracked-subdirectory",
    "attached-branch",
    "occupied-target",
    "detached-target",
    "foreign-target",
    "linked-worktrees",
    "separate-git-dir",
    "target-tree",
];

/// The placeholders the corpus stores in place of what varies per run, mirrored
/// from the capture script.
const REPO_DIRECTORY: &str = "{repoDir}";
const HEAD_COMMIT: &str = "{headCommit}";

/// The fixed layout of a scripted case, mirroring the capture script's
/// constants so both sides name the same directories.
const CHECKOUT: &str = "repo";
const VIBE_HOME: &str = "home";
const LINKED: &str = "linked";
const OUTSIDE: &str = "outside";
const TREE: &str = "tree";
const STATE: &str = "state";
const DEFAULT_BRANCH: &str = "main";
const FIXED_AUTHOR: &str = "Parity Oracle";
const FIXED_EMAIL: &str = "oracle@example.invalid";

/// What a divergence names when no story can close it, because closing it would
/// mean shipping reference prose.
const LICENSING: &str = "NOTICE";

/// Why a gap stands, shared by every case that carries the same one: the reason
/// is a property of the gap, not of the case that happens to reveal it. Each is
/// stated once so a story landing removes one sentence rather than dozens.
const NO_BRANCH_PARAMETER: &str = "the reference takes the branch as a parameter and defaults it \
     to the worktree name; this port has no branch parameter, so a case that asks for a branch \
     other than the name cannot be driven here at all. US-282 adds the parameter, which is what \
     `localWorkspaceSelection` needs";
const COMMIT_PHRASING: &str = "the commit-count reason is this port's own sentence and the \
     reference's is shorter; the two booleans beside it match digest for digest. US-286 restates \
     the phrase from the reference's own form";
const UNASKABLE_BRANCH_GATE: &str = "the branch gate US-277 wrote refuses this branch, but the \
     case asks for a branch other than the worktree name and this port has no parameter to carry \
     one, so the refusal cannot be driven here. US-282 adds the parameter and the gate answers \
     through it";
const NO_ENUMERATION: &str = "the reference publishes `list_linked_worktrees`; this port has no \
     enumeration at all, so no case of this family can be driven here. US-280 writes it";

/// One tolerated gap between this port and the reference.
#[derive(Debug, Clone, Copy)]
struct Divergence {
    family: &'static str,
    /// The one case this entry answers for. Wildcards are refused by the audit
    /// test: a gap that spans a family spans it one case at a time, and saying
    /// so is what keeps the ledger from outliving the divergence.
    case: &'static str,
    /// Matched by prefix against the reported JSON pointer.
    pointer: &'static str,
    /// The story that closes this gap, or [`LICENSING`] when none can.
    closed_by: &'static str,
    /// Why the gap stands, asserted non-empty so an entry cannot be added
    /// without a stated reason.
    why: &'static str,
}

impl Divergence {
    fn covers(&self, family: &str, case: &str, pointer: &str) -> bool {
        self.family == family && self.case == case && pointer.starts_with(self.pointer)
    }
}

/// The divergences this port still carries, each with what closes it.
///
/// A pointer is matched by prefix, so `/prepared` covers every field under it.
/// Keep the list ordered by family then by case.
const LEDGER: &[Divergence] = &[
    // Preparation.
    //
    // `prepare/missing-base` carries no entry, and now for the right reason.
    // That case names a base inside an untracked subdirectory, which exists in
    // the checkout and not in the new worktree, so both sides create the
    // worktree, fail to resolve the session directory, and roll the worktree
    // and the branch back: the residue both record is empty. Until US-279 this
    // port agreed by accident, refusing before it created anything because it
    // could not resolve the common git directory from a subdirectory; it now
    // agrees by taking the same path.
    Divergence {
        family: "prepare",
        case: "distinct-branch",
        pointer: "/outcome",
        closed_by: "US-282",
        why: NO_BRANCH_PARAMETER,
    },
    Divergence {
        family: "prepare",
        case: "invalid-branch",
        pointer: "/outcome",
        closed_by: "US-282",
        why: UNASKABLE_BRANCH_GATE,
    },
    // Cleanup.
    Divergence {
        family: "cleanup",
        case: "one-commit",
        pointer: "/reasons/0",
        closed_by: "US-286",
        why: COMMIT_PHRASING,
    },
    Divergence {
        family: "cleanup",
        case: "two-commits",
        pointer: "/reasons/0",
        closed_by: "US-286",
        why: COMMIT_PHRASING,
    },
    Divergence {
        family: "cleanup",
        case: "detached-commit",
        pointer: "/reasons/0",
        closed_by: "US-286",
        why: COMMIT_PHRASING,
    },
    // The enumeration that does not exist here.
    Divergence {
        family: "list",
        case: "none",
        pointer: "/outcome",
        closed_by: "US-280",
        why: NO_ENUMERATION,
    },
    Divergence {
        family: "list",
        case: "not-a-repository",
        pointer: "/outcome",
        closed_by: "US-280",
        why: NO_ENUMERATION,
    },
    Divergence {
        family: "list",
        case: "several",
        pointer: "/outcome",
        closed_by: "US-280",
        why: NO_ENUMERATION,
    },
    Divergence {
        family: "list",
        case: "subdirectory-base",
        pointer: "/outcome",
        closed_by: "US-280",
        why: NO_ENUMERATION,
    },
];

// --------------------------------------------------------------------------
// The corpus
// --------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference_commit: String,
    platform: String,
    /// The scripted repositories the capture built, asserted against [`SETUPS`]
    /// so neither side can add one the other never learns about.
    setups: Vec<String>,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Case {
    family: String,
    case: String,
    /// The scripted repository this case ran over. Absent for the two families
    /// that need no filesystem.
    #[serde(default)]
    setup: Option<String>,
    input: Value,
    observed: Value,
}

impl Case {
    fn id(&self) -> String {
        format!("{}/{}", self.family, self.case)
    }

    /// The recorded verdict as one comparable document.
    ///
    /// The message digest is dropped rather than compared. The corpus records
    /// it so a re-pin that reworded a refusal is visible in its diff, but it is
    /// not a conformance target: reaching a reference digest would mean writing
    /// the reference's own sentence into this repository, which `NOTICE`
    /// forbids and the PRD lists as a non-goal. The class name beside it is
    /// compared, which is what says the two sides refuse for the same reason.
    fn document(&self) -> Value {
        let mut observed = self.observed.clone();
        if let Some(fields) = observed.as_object_mut() {
            fields.remove("message");
        }
        observed
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

fn corpus() -> Corpus {
    let raw =
        fs::read_to_string(repo_root().join(CORPUS_RELATIVE)).expect("the corpus is committed");
    let corpus: Corpus = serde_json::from_str(&raw).expect("the corpus parses");
    assert_eq!(
        corpus.schema_version, CORPUS_SCHEMA_VERSION,
        "the corpus layout moved; regenerate with `{CAPTURE_SCRIPT} --corpus`"
    );
    assert_eq!(
        corpus.reference_commit, REFERENCE_COMMIT,
        "the corpus was captured from another commit than this build asserts"
    );
    corpus
}

/// Why this corpus cannot answer for this host, or [`None`] when it can.
///
/// A corpus records what the reference did on one platform, and the scripted
/// repositories carry symbolic links and a case-sensitive name list, so
/// replaying a Linux capture on a Windows workstation would diff the host
/// rather than the port. That skips with a named reason rather than failing.
fn skip_reason(corpus: &Corpus) -> Option<String> {
    (corpus.platform != std::env::consts::OS).then(|| {
        format!(
            "skipping the worktree replay: the corpus records the {} behavior and this host is \
             {}; recapture with `{CAPTURE_SCRIPT} --corpus`",
            corpus.platform,
            std::env::consts::OS
        )
    })
}

/// Fails when the corpus no longer covers what EP-086 commits to, naming the
/// count so a shrunken corpus cannot pass as a green one.
fn assert_corpus_floor(corpus: &Corpus) {
    assert!(
        corpus.cases.len() >= MINIMUM_CASES,
        "the corpus shrank to {} cases, below the floor of {MINIMUM_CASES}",
        corpus.cases.len()
    );
    let mut per_family: BTreeMap<&str, usize> = BTreeMap::new();
    for case in &corpus.cases {
        *per_family.entry(case.family.as_str()).or_default() += 1;
    }
    assert_eq!(
        per_family.keys().copied().collect::<Vec<_>>(),
        FAMILIES,
        "the corpus drives another set of families than the replay builds for"
    );
    let thin = per_family
        .iter()
        .filter(|(_, count)| **count < MINIMUM_CASES_PER_FAMILY)
        .map(|(family, count)| format!("{family} has {count}"))
        .collect::<Vec<_>>();
    assert!(
        thin.is_empty(),
        "every family carries at least {MINIMUM_CASES_PER_FAMILY} cases, but {}",
        thin.join(", ")
    );
    assert_eq!(
        corpus.setups, SETUPS,
        "the capture scripts another set of repositories than the replay rebuilds"
    );
}

// --------------------------------------------------------------------------
// The scripted repositories, rebuilt in Rust
// --------------------------------------------------------------------------

fn git(directory: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .expect("git is on PATH");
    assert!(
        output.status.success(),
        "git {arguments:?} failed in {}: {}",
        directory.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn git_succeeds(directory: &Path, arguments: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .expect("git is on PATH")
        .status
        .success()
}

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the parent directory is writable");
    }
    fs::write(path, content).expect("the fixture is writable");
}

fn text(path: &Path) -> String {
    path.to_str().expect("a scripted path is UTF-8").to_owned()
}

/// Cuts this machine's git configuration out of one scripted repository.
///
/// See the module header for why this is `--local` rather than the environment
/// the capture script sets.
fn harden(checkout: &Path) {
    // Asked of git rather than assumed to be `.git` beside the checkout: a
    // repository created with `--separate-git-dir` keeps a file there.
    let git_directory = common_git_directory(checkout);
    let hooks = git_directory.join("parity-hooks");
    fs::create_dir_all(&hooks).expect("the hook directory is writable");
    let excludes = git_directory.join("parity-excludes");
    write_file(&excludes, "");
    for (key, value) in [
        ("user.name", FIXED_AUTHOR.to_owned()),
        ("user.email", FIXED_EMAIL.to_owned()),
        ("commit.gpgsign", "false".to_owned()),
        ("core.hooksPath", text(&hooks)),
        ("core.excludesFile", text(&excludes)),
        ("core.fsmonitor", "false".to_owned()),
        ("gc.auto", "0".to_owned()),
    ] {
        git(checkout, &["config", "--local", key, value.as_str()]);
    }
}

fn commit_all(checkout: &Path, message: &str) {
    git(checkout, &["add", "--all"]);
    git(
        checkout,
        &["commit", "--no-gpg-sign", "--quiet", "-m", message],
    );
}

fn initialize_checkout(checkout: &Path) {
    initialize_checkout_with(checkout, None);
}

/// `initialize_checkout`, optionally keeping the git data outside the checkout.
fn initialize_checkout_with(checkout: &Path, separate_git_dir: Option<&Path>) {
    fs::create_dir_all(checkout).expect("the checkout directory is writable");
    let mut arguments = vec!["init", "--quiet", "--initial-branch", DEFAULT_BRANCH];
    let separate;
    if let Some(directory) = separate_git_dir {
        separate = text(directory);
        arguments.extend(["--separate-git-dir", separate.as_str()]);
    }
    git(checkout, &arguments);
    harden(checkout);
    write_file(&checkout.join("README.md"), "fixture\n");
    write_file(&checkout.join("docs").join("guide.md"), "guide\n");
    commit_all(checkout, "fixture");
}

fn head_commit(checkout: &Path) -> String {
    git(checkout, &["rev-parse", "HEAD"])
}

fn branch_exists(checkout: &Path, branch: &str) -> bool {
    git_succeeds(
        checkout,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("the fixture symlink is created");
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_dir(target, link).expect("the fixture symlink is created");
}

/// The managed root a scripted checkout's worktrees live under, from this
/// port's own naming rule.
///
/// The capture calls the reference's `_worktree_root` for the same purpose, and
/// the `managedRoot` family is what proves the two rules agree. Using this
/// port's rule here is deliberate: a setup that pre-occupies the target has to
/// occupy the directory this port will actually reach for, and if the two rules
/// ever disagreed the `managedRoot` family would say so first.
fn managed_directory(root: &Path) -> PathBuf {
    let checkout = root.join(CHECKOUT);
    let common_git_dir = common_git_directory(&checkout);
    managed_worktree_root(&root.join(VIBE_HOME), &checkout, &common_git_dir)
        .expect("a scripted managed root stays under its vibe home")
}

/// Where the repository `checkout` belongs to keeps its shared git data.
fn common_git_directory(checkout: &Path) -> PathBuf {
    let reported = PathBuf::from(git(checkout, &["rev-parse", "--git-common-dir"]));
    let resolved = if reported.is_absolute() {
        reported
    } else {
        checkout.join(reported)
    };
    resolved
        .canonicalize()
        .expect("the common git directory resolves")
}

/// One case's own filesystem, resolved so every recorded path is relative to a
/// root with no symbolic links in it.
fn case_root(scratch: &Path, index: usize) -> PathBuf {
    let root = scratch.join(format!("case-{index}"));
    fs::create_dir_all(&root).expect("the case root is writable");
    root.canonicalize().expect("the case root resolves")
}

/// Materializes one scripted case under `root`, mirroring `build_setup` in the
/// capture script step for step.
fn build_setup(setup: &str, root: &Path) {
    let checkout = root.join(CHECKOUT);
    fs::create_dir_all(root.join(VIBE_HOME)).expect("the vibe home is writable");

    if setup == "separate-git-dir" {
        let state = root.join(STATE);
        fs::create_dir_all(&state).expect("the state directory is writable");
        initialize_checkout_with(&checkout, Some(&state.join("repo.git")));
        let linked = root.join(LINKED);
        fs::create_dir_all(&linked).expect("the linked directory is writable");
        git(
            &checkout,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "alpha",
                &text(&linked.join("alpha")),
            ],
        );
        return;
    }

    if setup == "target-tree" {
        let tree = root.join(TREE);
        fs::create_dir_all(tree.join("sub")).expect("writable");
        fs::create_dir_all(tree.join("deep").join("inner")).expect("writable");
        fs::create_dir_all(root.join(OUTSIDE)).expect("writable");
        write_file(&tree.join("file.txt"), "file\n");
        write_file(&tree.join("nested").join(".git"), "gitdir: /nowhere\n");
        symlink(&root.join(OUTSIDE), &tree.join("escape"));
        symlink(&tree.join("sub"), &tree.join("aliased"));
        return;
    }

    initialize_checkout(&checkout);

    match setup {
        "plain" => {}
        "untracked-subdirectory" => {
            write_file(&checkout.join("scratch").join("note.txt"), "scratch\n");
        }
        "attached-branch" => {
            git(&checkout, &["branch", "review"]);
        }
        "occupied-target" => {
            let target = managed_directory(root).join("review");
            write_file(&target.join("note.txt"), "occupied\n");
        }
        "detached-target" => {
            let target = managed_directory(root).join("review");
            fs::create_dir_all(target.parent().expect("the target has a parent"))
                .expect("the managed root is writable");
            git(
                &checkout,
                &["worktree", "add", "--quiet", "-b", "review", &text(&target)],
            );
            git(&target, &["checkout", "--quiet", "--detach"]);
        }
        "foreign-target" => {
            let target = managed_directory(root).join("review");
            initialize_checkout(&target);
            git(&target, &["branch", "review"]);
            git(&target, &["checkout", "--quiet", "review"]);
        }
        "linked-worktrees" => {
            let linked = root.join(LINKED);
            fs::create_dir_all(&linked).expect("the linked directory is writable");
            for branch in ["alpha", "beta"] {
                git(
                    &checkout,
                    &[
                        "worktree",
                        "add",
                        "--quiet",
                        "-b",
                        branch,
                        &text(&linked.join(branch)),
                    ],
                );
            }
            git(
                &checkout,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "--detach",
                    &text(&linked.join("gamma")),
                ],
            );
            git(
                &checkout,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    "delta",
                    &text(&linked.join("delta")),
                ],
            );
            // A record git still holds and a directory that is gone: the
            // prunable case the reference's filter excludes.
            fs::remove_dir_all(linked.join("delta")).expect("the prunable worktree is removable");
        }
        other => assert!(
            SETUPS.contains(&other),
            "the replay has no builder for the scripted repository `{other}`"
        ),
    }
}

fn apply_mutation(worktree: &Path, step: &str) {
    match step {
        "modify" => {
            let readme = worktree.join("README.md");
            let existing = fs::read_to_string(&readme).expect("the fixture is readable");
            write_file(&readme, &format!("{existing}edit\n"));
        }
        "add" => write_file(&worktree.join("note.txt"), "note\n"),
        "commit" => commit_all(worktree, "session"),
        "detach" => {
            git(worktree, &["checkout", "--quiet", "--detach"]);
        }
        other => assert!(
            matches!(other, "modify" | "add" | "commit" | "detach"),
            "the replay has no builder for the mutation `{other}`"
        ),
    }
}

// --------------------------------------------------------------------------
// Projection, mirroring the capture script
// --------------------------------------------------------------------------

fn digest(value: &str) -> String {
    let hash = Sha256::digest(value.as_bytes());
    let hex = hash.iter().fold(String::new(), |mut accumulator, byte| {
        use std::fmt::Write as _;
        let _ = write!(accumulator, "{byte:02x}");
        accumulator
    });
    format!("sha256:{}", &hex[..32])
}

/// A published sentence, reduced to something `NOTICE` allows shipping.
///
/// The length is counted in characters rather than bytes, because the capture
/// script counts what `len()` counts on a Python string.
fn describe(value: &str) -> Value {
    json!({ "described": digest(value), "length": value.chars().count() })
}

/// Rewrites one case's absolute paths into the pointers the corpus stores.
struct Projection {
    root: PathBuf,
    repo_directory: Option<String>,
    commit: Option<String>,
}

impl Projection {
    fn new(root: &Path, repo_directory: Option<String>, commit: Option<String>) -> Self {
        Self {
            root: root.to_path_buf(),
            repo_directory,
            commit,
        }
    }

    /// The path relative to the case root, with `/` separators and the managed
    /// directory's hashed segment replaced.
    ///
    /// The components are rebuilt rather than sliced, so a `.` this port joined
    /// on compares equal to the plain directory the reference recorded. A path
    /// outside the case root is returned marked rather than panicking, so it
    /// reads as a divergence at that field instead of taking the suite down.
    fn path(&self, value: &Path) -> String {
        let Ok(relative) = value.strip_prefix(&self.root) else {
            return format!("{{outside}}{}", value.display());
        };
        let mut rendered = relative
            .components()
            .filter(|component| !matches!(component, Component::CurDir))
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        if rendered.is_empty() {
            rendered = ".".to_owned();
        }
        match &self.repo_directory {
            Some(directory) => rendered.replace(directory.as_str(), REPO_DIRECTORY),
            None => rendered,
        }
    }

    fn commit_value(&self, value: &str) -> String {
        match &self.commit {
            Some(commit) if commit == value => HEAD_COMMIT.to_owned(),
            _ => value.to_owned(),
        }
    }
}

/// The exception class the reference would have raised for this refusal.
///
/// The reference publishes three: `WorktreeError` and two subclasses for the
/// cases a caller discriminates. This port publishes one enum with the same two
/// distinguished variants, so the mapping is the whole of the comparison and
/// the sentence never enters it.
fn error_class(error: &super::WorktreeError) -> &'static str {
    match error {
        super::WorktreeError::RepositoryRequired => "WorktreeNotFoundError",
        super::WorktreeError::GitUnavailable(_) => "GitUnavailableError",
        // A note the reference attaches with `add_note` leaves the class of the
        // failure it is attached to untouched, so the class is read through it.
        super::WorktreeError::Noted { source, .. } => error_class(source),
        _ => "WorktreeError",
    }
}

fn error_record(error: &super::WorktreeError) -> Value {
    json!({ "outcome": "error", "errorClass": error_class(error) })
}

// --------------------------------------------------------------------------
// Driving this port
// --------------------------------------------------------------------------

fn observed_document(case: &Case, scratch: &Path, index: usize) -> Value {
    match case.family.as_str() {
        "name" => observed_name(case, scratch),
        "managedRoot" => observed_managed_root(case),
        family => {
            let root = case_root(scratch, index);
            let setup = case
                .setup
                .as_deref()
                .expect("a filesystem family names its scripted repository");
            build_setup(setup, &root);
            match family {
                "prepare" => observed_prepare(case, &root),
                "cleanup" => observed_cleanup(case, &root),
                "list" => observed_list(),
                "targetCwd" => observed_target_cwd(case, &root),
                // Unreachable: `assert_corpus_floor` already refused any family
                // outside `FAMILIES`, and every one of them is answered above.
                other => json!({ "outcome": format!("no builder for {other}") }),
            }
        }
    }
}

fn observed_name(case: &Case, root: &Path) -> Value {
    let name = case.input["name"].as_str().expect("a name case names one");
    json!({
        "portable": validate_worktree_name(name).is_ok(),
        // `check-ref-format` reads no repository, so the case root only decides
        // where the process starts.
        "branchValid": validate_branch_name(root, name).is_ok(),
    })
}

fn observed_managed_root(case: &Case) -> Value {
    let repo_root = PathBuf::from(
        case.input["repoRoot"]
            .as_str()
            .expect("a managed-root case names a repository root"),
    );
    let common_git_dir = PathBuf::from(
        case.input["commonGitDir"]
            .as_str()
            .expect("a managed-root case names a common git directory"),
    );
    // Any home serves: the corpus records the directory name the rule produces,
    // relative to the managed root, so the replay recomputes the function
    // rather than comparing a temporary path.
    let home = Path::new("/synthetic/home");
    let resolved = managed_worktree_root(home, &repo_root, &common_git_dir)
        .expect("a synthetic managed root stays under its vibe home");
    let directory = resolved
        .strip_prefix(home.join("worktrees"))
        .expect("the managed root lives under the vibe home");
    json!({
        "repoRootName": repo_root
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
        "directory": directory.to_string_lossy(),
    })
}

fn observed_prepare(case: &Case, root: &Path) -> Value {
    let checkout = root.join(CHECKOUT);
    let name = case.input["name"]
        .as_str()
        .expect("a prepare case is named");
    let branch = case.input["branch"].as_str();
    let base = match case.input["base"]
        .as_str()
        .expect("a prepare case has a base")
    {
        "." => root.to_path_buf(),
        relative => root.join(relative),
    };
    let projection = Projection::new(
        root,
        managed_directory(root)
            .file_name()
            .map(|value| value.to_string_lossy().into_owned()),
        Some(head_commit(&checkout)),
    );

    let mut observed = if branch.is_some_and(|value| value != name) {
        // The branch is a parameter this port does not have, so the case is not
        // merely answered differently: it cannot be asked. Saying so is more
        // honest than driving a different call and diffing the result.
        json!({ "outcome": "unsupported" })
    } else {
        let home = root.join(VIBE_HOME);
        let mut attempt = prepare_worktree(name, &base, &home);
        if attempt.is_ok() && case.input["twice"].as_bool().unwrap_or(false) {
            attempt = prepare_worktree(name, &base, &home);
        }
        match attempt {
            Ok(prepared) => json!({
                "outcome": "prepared",
                "prepared": {
                    "name": prepared.name,
                    "branch": prepared.branch,
                    "root": projection.path(&prepared.root),
                    "path": projection.path(&prepared.path),
                    "repoRoot": projection.path(&prepared.repo_root),
                    "baseCommit": projection.commit_value(&prepared.base_commit),
                    "created": prepared.created,
                    "branchCreated": prepared.branch_created,
                },
            }),
            Err(error) => error_record(&error),
        }
    };

    let target = managed_directory(root).join(name);
    let residue = json!({
        "target": target.exists(),
        "branch": branch_exists(&checkout, branch.unwrap_or(name)),
    });
    if let Some(fields) = observed.as_object_mut() {
        fields.insert("residue".to_owned(), residue);
    }
    observed
}

fn observed_cleanup(case: &Case, root: &Path) -> Value {
    let checkout = root.join(CHECKOUT);
    let name = case.input["name"]
        .as_str()
        .expect("a cleanup case is named");
    let prepared = prepare_worktree(name, &checkout, &root.join(VIBE_HOME))
        .expect("a cleanup case prepares its worktree before mutating it");
    for step in case.input["mutations"]
        .as_array()
        .expect("a cleanup case lists its mutations")
    {
        apply_mutation(
            &prepared.root,
            step.as_str().expect("a mutation is named as a string"),
        );
    }
    match inspect_worktree_for_cleanup(&prepared) {
        Ok(state) => json!({
            "outcome": "inspected",
            "hasUncommittedChanges": state.has_uncommitted_changes,
            "hasUntrackedFiles": state.has_untracked_files,
            "newCommitCount": state.new_commit_count,
            "isClean": state.is_clean(),
            "reasons": state
                .reasons()
                .iter()
                .map(|reason| describe(reason))
                .collect::<Vec<_>>(),
        }),
        Err(error) => error_record(&error),
    }
}

fn observed_list() -> Value {
    // This port has no enumeration to drive, so every case of the family
    // reports the same thing rather than pretending to an empty list.
    json!({ "outcome": "unsupported" })
}

fn observed_target_cwd(case: &Case, root: &Path) -> Value {
    let projection = Projection::new(root, None, None);
    let tree = root.join(
        case.input["root"]
            .as_str()
            .expect("a target case names its tree"),
    );
    let relative_base = case.input["relativeBase"]
        .as_str()
        .expect("a target case names its relative base");
    match target_cwd(&tree, Path::new(relative_base), "review") {
        Ok(resolved) => json!({ "outcome": "resolved", "path": projection.path(&resolved) }),
        Err(error) => error_record(&error),
    }
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
/// raised, and reporting them as well would turn one gap into four ledger
/// entries that all close together.
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

/// Every difference under `pointer`, all of them rather than the first, because
/// the ledger is matched per pointer: reporting only the earliest would leave
/// an entry for a later field permanently unexercised, and the staleness check
/// would then delete a gap that is still open.
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
            for (index, (one, other)) in left.iter().zip(right).enumerate() {
                differences(&format!("{pointer}/{index}"), one, other, into);
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
// The replay
// --------------------------------------------------------------------------

#[test]
fn the_committed_corpus_replays_against_this_port() {
    let corpus = corpus();
    assert_corpus_floor(&corpus);
    if let Some(reason) = skip_reason(&corpus) {
        eprintln!("{reason}");
        return;
    }
    let scratch = tempfile::tempdir().expect("tempdir");

    let mut conforming = 0usize;
    let mut tolerated: BTreeSet<(String, String)> = BTreeSet::new();
    let mut unlisted = Vec::new();
    for (index, case) in corpus.cases.iter().enumerate() {
        let observed = observed_document(case, scratch.path(), index);
        let found = compare(&case.document(), &observed);
        if found.is_empty() {
            conforming += 1;
        }
        for difference in found {
            match LEDGER
                .iter()
                .find(|entry| entry.covers(&case.family, &case.case, &difference.pointer))
            {
                Some(entry) => {
                    tolerated.insert((
                        format!("{}/{} at {}", entry.family, entry.case, entry.pointer),
                        entry.closed_by.to_owned(),
                    ));
                }
                None => unlisted.push(format!(
                    "{} at {}: the reference says {}, this port says {}",
                    case.id(),
                    difference.pointer,
                    difference.expected,
                    difference.actual
                )),
            }
        }
    }

    println!(
        "worktree: {conforming}/{} cases match the reference at {} across {} families and {} \
         scripted repositories, {} ledger entries exercised",
        corpus.cases.len(),
        &corpus.reference_commit[..12],
        FAMILIES.len(),
        SETUPS.len(),
        tolerated.len()
    );
    for (entry, closed_by) in &tolerated {
        println!("  tolerated {entry} until {closed_by}");
    }

    assert!(
        unlisted.is_empty(),
        "worktree divergences outside the ledger:\n  {}",
        unlisted.join("\n  ")
    );
}

#[test]
fn a_ledger_entry_whose_divergence_is_fixed_fails_the_suite() {
    let corpus = corpus();
    if let Some(reason) = skip_reason(&corpus) {
        eprintln!("{reason}");
        return;
    }
    let scratch = tempfile::tempdir().expect("tempdir");

    let mut exercised: BTreeSet<usize> = BTreeSet::new();
    for (index, case) in corpus.cases.iter().enumerate() {
        let observed = observed_document(case, scratch.path(), index);
        for difference in compare(&case.document(), &observed) {
            if let Some(position) = LEDGER
                .iter()
                .position(|entry| entry.covers(&case.family, &case.case, &difference.pointer))
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
                entry.family, entry.case, entry.pointer, entry.closed_by
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
    for entry in LEDGER {
        assert!(
            entry.closed_by.starts_with("US-") || entry.closed_by == LICENSING,
            "a tolerated divergence names the story that closes it or the licensing boundary that \
             keeps it open, not {}",
            entry.closed_by
        );
        assert!(entry.pointer.starts_with('/'), "{}", entry.pointer);
        assert_ne!(
            entry.case, "*",
            "a ledger entry answers for one case, not for every case of {}: a wildcard outlives \
             the divergence it was written for",
            entry.family
        );
        assert!(
            FAMILIES.contains(&entry.family),
            "{} is not a family the corpus drives",
            entry.family
        );
        assert!(!entry.why.is_empty(), "{}/{}", entry.family, entry.case);
    }
}

#[test]
fn the_committed_corpus_carries_no_reference_prose() {
    let corpus = corpus();
    let mut offending = Vec::new();
    for case in &corpus.cases {
        let mut authored = BTreeSet::new();
        collect_authored(&case.input, &mut authored);
        authored.insert(case.family.clone());
        authored.insert(case.case.clone());
        collect_literals(&case.observed, &authored, &mut offending);
    }
    assert!(
        offending.is_empty(),
        "the corpus carries strings that are neither an input it authored, a normalized path, nor \
         a single path segment, so they may be reference prose: {offending:?}"
    );
}

/// Every string the capture authored for one case, plus the segments of the
/// paths among them, because a record names the last segment of an input path
/// on its own.
fn collect_authored(value: &Value, into: &mut BTreeSet<String>) {
    match value {
        Value::Object(fields) => fields
            .values()
            .for_each(|item| collect_authored(item, into)),
        Value::Array(items) => items.iter().for_each(|item| collect_authored(item, into)),
        Value::String(text) => {
            into.insert(text.clone());
            for segment in text.split(['/', '\\']) {
                into.insert(segment.to_owned());
            }
        }
        _ => {}
    }
}

/// Every committed string that [`keeps_literal`] would not have admitted, which
/// is what a prose leak would look like.
fn collect_literals(value: &Value, authored: &BTreeSet<String>, into: &mut Vec<String>) {
    match value {
        Value::Object(fields) => {
            // A described sentence is recorded as exactly these two keys.
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

/// Whether a committed string may stand in cleartext.
///
/// Three shapes may: a string the capture itself authored, a normalized path
/// built from path segments and the corpus placeholders, and a bare segment
/// such as a class name or an outcome. A sentence carries a space and matches
/// none of them.
fn keeps_literal(text: &str, authored: &BTreeSet<String>) -> bool {
    if authored.contains(text) {
        return true;
    }
    if text.is_empty() || text.chars().count() > 128 {
        return false;
    }
    text.split('/').all(|segment| {
        !segment.is_empty()
            && segment.chars().all(|character| {
                character.is_alphanumeric()
                    || matches!(character, '.' | '-' | '_' | '$' | '{' | '}')
            })
    })
}

#[test]
fn the_projection_agrees_with_the_capture_script() {
    // The two sides must digest identically, or every described string would
    // read as a divergence. Anchored on a value the Python side computes with
    // `hashlib.sha256(text.encode()).hexdigest()[:32]`.
    assert_eq!(
        digest("uncommitted changes"),
        "sha256:31e3e828864d2c00e54f15dcdd17be78"
    );
    assert_eq!(
        describe("untracked files"),
        json!({ "described": "sha256:58f592fe9ad4e9071b207336c06ba70c", "length": 15 })
    );
    // A character count, not a byte count: the capture script counts what
    // Python's `len()` counts.
    assert_eq!(describe("dépôt")["length"], json!(5));

    // The managed-root rule, anchored on the same synthetic path the corpus
    // records, so a change to the digest length or the separator fails here
    // rather than across six cases.
    let home = Path::new("/synthetic/home");
    assert_eq!(
        managed_worktree_root(
            home,
            Path::new("/synthetic/repo"),
            Path::new("/synthetic/repo/.git")
        )
        .expect("the synthetic managed root stays under its vibe home"),
        home.join("worktrees").join("repo-48615fc593d4")
    );

    let authored = BTreeSet::from(["a b".to_owned()]);
    assert!(keeps_literal("a b", &authored));
    assert!(keeps_literal("home/worktrees/{repoDir}/review", &authored));
    assert!(keeps_literal("{headCommit}", &authored));
    assert!(keeps_literal("WorktreeNotFoundError", &authored));
    assert!(keeps_literal("dépôt-2672f68d7648", &authored));
    assert!(keeps_literal(".", &authored));
    assert!(!keeps_literal(
        "worktree path does not exist after checkout",
        &authored
    ));
    assert!(!keeps_literal("", &authored));
}

/// The scripted repositories rebuild the same way twice, so a case cannot pass
/// because of what the case before it left behind.
#[test]
fn the_scripted_repositories_are_deterministic() {
    let corpus = corpus();
    if let Some(reason) = skip_reason(&corpus) {
        eprintln!("{reason}");
        return;
    }
    let scratch = tempfile::tempdir().expect("tempdir");

    let listing = |root: &Path| -> BTreeSet<String> {
        // The managed directory is named after a digest of an absolute path,
        // so it differs between two case roots by construction. The projection
        // replaces it the same way the corpus does. A setup that scripts no
        // checkout at all has no managed directory to name.
        let projection = Projection::new(
            root,
            root.join(CHECKOUT)
                .is_dir()
                .then(|| managed_directory(root))
                .and_then(|value| value.file_name().map(|v| v.to_string_lossy().into_owned())),
            None,
        );
        let mut found = BTreeSet::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(&directory).expect("readable").flatten() {
                let path = entry.path();
                let relative = projection.path(&path);
                // The git directory holds a commit hash and an index; the
                // fixture layout is what has to be stable.
                if relative.contains(".git") {
                    continue;
                }
                if path.is_dir() {
                    pending.push(path);
                }
                found.insert(relative);
            }
        }
        found
    };

    for (index, setup) in SETUPS.iter().enumerate() {
        let first = case_root(scratch.path(), index * 2);
        let second = case_root(scratch.path(), index * 2 + 1);
        build_setup(setup, &first);
        build_setup(setup, &second);
        assert_eq!(
            listing(&first),
            listing(&second),
            "the scripted repository `{setup}` is not deterministic"
        );
    }
}

/// Recaptures against the local checkout and asserts the committed corpus is
/// still what the pinned reference answers.
///
/// This is the only test here that needs the checkout, and it skips naming the
/// pin and the way back when the checkout is absent or off-pin. The replay
/// above runs regardless, which is what keeps a missing checkout from failing
/// `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "worktree") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let repository = repo_root();
    let recaptured = repository.join("target/worktree-corpus.json");
    let output = Command::new("python3")
        .arg(repository.join(CAPTURE_SCRIPT))
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(repository.join("target/worktree-full.json"))
        .arg("--corpus")
        .arg(&recaptured)
        .current_dir(&repository)
        .output()
        .expect("the worktree capture script runs");
    assert!(
        output.status.success(),
        "the worktree capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let fresh: Value = serde_json::from_str(
        &fs::read_to_string(&recaptured).expect("the recaptured corpus is readable"),
    )
    .expect("the recaptured corpus parses");
    let committed: Value = serde_json::from_str(
        &fs::read_to_string(repository.join(CORPUS_RELATIVE)).expect("the corpus is readable"),
    )
    .expect("the corpus parses");
    assert_eq!(
        fresh, committed,
        "the pinned reference no longer answers what the committed corpus records; regenerate it \
         with `{CAPTURE_SCRIPT} --corpus`"
    );
}
