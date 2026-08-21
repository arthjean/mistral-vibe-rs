//! Replays the description-resolution corpus captured from the pinned
//! reference.
//!
//! `scripts/parity/tool_descriptions.py` builds a scratch tree of tool
//! directories, drives the reference's `ToolManager` over seven configurations,
//! and records three things per case: the search path order
//! `_compute_search_paths` produced, which `prompts/<name>.md` file won each
//! stem, and what `available_tool_specs` then published. This module rebuilds
//! the same tree, resolves it through [`search_paths`] and [`read_overrides`],
//! and compares.
//!
//! Every description file in the corpus is this repository's own prose, and
//! every path is a label relative to the scratch tree, so nothing the reference
//! authored is committed here. The single exception is a count: how many tools
//! the reference described out of its own package directory, which is the one
//! position this port has no counterpart for and which
//! [`the_reference_reads_its_builtin_descriptions_from_disk`] pins as a
//! deliberate divergence rather than leaving unmeasured.
//!
//! The replay runs unconditionally. Only
//! [`the_committed_corpus_still_matches_the_pinned_reference`] needs the
//! checkout, and it skips when the checkout is absent or off-pin.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::Value;

use crate::config::{ConfigPaths, ConfigSource, HarnessFiles};
use crate::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};

use super::{SearchInputs, read_overrides, search_paths};

const CORPUS_RELATIVE: &str = "crates/vibe-core/tests/tool-descriptions/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/tool_descriptions.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 1;
/// The case floor this epic commits to, so a corpus regenerated against a
/// broken capture cannot pass as a green run by recording almost nothing.
const MINIMUM_CASES: usize = 7;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Corpus {
    schema_version: u32,
    reference: Reference,
    /// What stands in for the reference's builtin tool directory, which is a
    /// path inside the pinned tree and therefore machine-dependent.
    builtin_label: String,
    /// Where the scratch vibe home sits, and therefore where the user tool
    /// directory hangs off.
    vibe_home: String,
    tree: Tree,
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
struct Tree {
    /// Every description file the capture authored, keyed by its tree-relative
    /// label.
    files: BTreeMap<String, String>,
    /// Paths where a directory stands in for a file, which is the one spelling
    /// of "cannot be read" every host and every privilege level agrees on.
    obstructions: Vec<String>,
    /// Directories the tree needs beyond the ones its files imply.
    directories: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    name: String,
    sources: Vec<String>,
    trusted: bool,
    tool_paths: Vec<String>,
    /// The resolved search path order, as tree-relative labels, starting with
    /// the builtin placeholder.
    search_paths: Vec<String>,
    /// Stem to the label of the file that described it.
    overrides: BTreeMap<String, String>,
    /// A stem the tree carries which still described nothing: blank,
    /// unreadable, or under a directory this case does not walk.
    withheld_stems: Vec<String>,
    /// A stem that described a tool the reference does not publish. Tool
    /// availability is not this epic's contract, so the replay reads this to
    /// know which published names to expect rather than asserting on it.
    unmatched_stems: Vec<String>,
    /// What the reference published, restricted to the names this tree
    /// redescribes.
    published_descriptions: BTreeMap<String, String>,
    /// How many tools the reference described out of its own package
    /// directory.
    builtin_described_count: u32,
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
        corpus.reference.commit, REFERENCE_COMMIT,
        "the corpus was captured from another commit than this build asserts"
    );
    corpus
}

/// The scratch tree, rebuilt from the corpus under a temporary root.
///
/// The root is canonicalized before anything is written, because the port keeps
/// an entry it cannot canonicalize as written and a label comparison against a
/// symlinked temporary directory would then diff the host rather than the port.
struct Scratch {
    _temporary: tempfile::TempDir,
    root: PathBuf,
}

impl Scratch {
    fn build(tree: &Tree) -> Self {
        let temporary = tempfile::tempdir().expect("temporary root");
        let root = fs::canonicalize(temporary.path()).expect("the temporary root canonicalizes");
        for relative in &tree.directories {
            fs::create_dir_all(root.join(relative)).expect("scratch directory");
        }
        for (label, text) in &tree.files {
            let target = root.join(label);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).expect("scratch directory");
            }
            fs::write(&target, text).expect("scratch description file");
        }
        for label in &tree.obstructions {
            fs::create_dir_all(root.join(label)).expect("scratch obstruction");
        }
        Self {
            _temporary: temporary,
            root,
        }
    }

    /// `path` as the tree-relative label the corpus spells it with.
    fn label(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or_else(|_| panic!("{} resolved outside the scratch tree", path.display()))
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/")
    }
}

fn sources_of(case: &Case) -> BTreeSet<ConfigSource> {
    case.sources
        .iter()
        .map(|source| match source.as_str() {
            "project" => ConfigSource::Project,
            "user" => ConfigSource::User,
            other => panic!("the corpus names an unknown configuration source: {other}"),
        })
        .collect()
}

/// The search paths this port resolves for `case`, over `scratch`.
fn resolved(scratch: &Scratch, corpus: &Corpus, case: &Case) -> Vec<PathBuf> {
    let harness = HarnessFiles::new(
        ConfigPaths {
            vibe_home: scratch.root.join(&corpus.vibe_home),
            working_directory: scratch.root.clone(),
        },
        sources_of(case),
        Vec::new(),
        case.trusted,
    );
    let projects = harness.project_tools_dirs();
    let user = harness.user_tools_dirs();
    search_paths(&SearchInputs {
        configured: &case.tool_paths,
        projects: &projects,
        user: &user,
        user_home: Some(&scratch.root),
        working_directory: &scratch.root,
    })
}

/// The reference's search path labels with the builtin placeholder removed,
/// which is the list this port can answer for.
fn expected_labels<'a>(corpus: &'a Corpus, case: &'a Case) -> Vec<&'a str> {
    assert_eq!(
        case.search_paths.first().map(String::as_str),
        Some(corpus.builtin_label.as_str()),
        "the reference always walks its builtin directory first, and case {} does not record it",
        case.name
    );
    case.search_paths[1..]
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
}

#[test]
fn the_corpus_covers_every_resolution_rule_the_epic_claims() {
    let corpus = corpus();
    assert!(
        corpus.cases.len() >= MINIMUM_CASES,
        "the corpus records {} cases and this epic covers {MINIMUM_CASES}; regenerate with \
         `{CAPTURE_SCRIPT} --corpus`",
        corpus.cases.len()
    );
    let names: BTreeSet<&str> = corpus.cases.iter().map(|case| case.name.as_str()).collect();
    assert_eq!(names.len(), corpus.cases.len(), "two cases share a name");
    assert!(
        corpus.cases.iter().any(|case| case.tool_paths.is_empty()),
        "no case resolves without configured tool paths"
    );
    assert!(
        corpus
            .cases
            .iter()
            .any(|case| case.tool_paths.iter().any(|entry| entry.ends_with(".py"))),
        "no case names a module file as a tool path"
    );
    assert!(
        corpus.cases.iter().any(|case| !case.trusted),
        "no case resolves an untrusted workspace"
    );
    assert!(
        corpus
            .cases
            .iter()
            .any(|case| !case.sources.iter().any(|source| source == "user")),
        "no case disables the user source"
    );
    assert!(
        !corpus.tree.obstructions.is_empty(),
        "no file in the tree is unreadable, so the fallback is unmeasured"
    );
    assert!(
        corpus
            .tree
            .files
            .values()
            .any(|text| text.trim().is_empty()),
        "no file in the tree is blank, so the fallback is unmeasured"
    );
}

#[test]
fn the_search_path_order_matches_the_reference() {
    let corpus = corpus();
    let scratch = Scratch::build(&corpus.tree);
    for case in &corpus.cases {
        let resolved: Vec<String> = resolved(&scratch, &corpus, case)
            .iter()
            .map(|path| scratch.label(path))
            .collect();
        assert_eq!(
            resolved,
            expected_labels(&corpus, case),
            "case {} resolves a different search path order than the reference",
            case.name
        );
    }
}

#[test]
fn the_winning_description_file_matches_the_reference() {
    let corpus = corpus();
    let scratch = Scratch::build(&corpus.tree);
    // The reference records which file won by its text, so the replay maps back
    // the same way rather than trusting its own walk to name the winner.
    let authored: BTreeMap<&str, &str> = corpus
        .tree
        .files
        .iter()
        .filter(|(_, text)| !text.trim().is_empty())
        .map(|(label, text)| (text.as_str(), label.as_str()))
        .collect();
    for case in &corpus.cases {
        let overrides = read_overrides(&resolved(&scratch, &corpus, case));
        let winners: BTreeMap<String, String> = overrides
            .iter()
            .map(|(stem, text)| {
                let label = authored.get(text.as_str()).unwrap_or_else(|| {
                    panic!(
                        "case {} described {stem} with text no tree file holds",
                        case.name
                    )
                });
                (stem.clone(), (*label).to_owned())
            })
            .collect();
        assert_eq!(
            winners, case.overrides,
            "case {} lets a different file describe the surface than the reference does",
            case.name
        );
    }
}

#[test]
fn a_withheld_stem_describes_nothing_on_either_side() {
    let corpus = corpus();
    let scratch = Scratch::build(&corpus.tree);
    for case in &corpus.cases {
        let overrides = read_overrides(&resolved(&scratch, &corpus, case));
        for stem in &case.withheld_stems {
            assert!(
                !overrides.contains_key(stem),
                "case {} describes {stem} where the reference leaves the tool its own \
                 description",
                case.name
            );
        }
    }
}

#[test]
fn the_published_text_matches_what_the_reference_published() {
    let corpus = corpus();
    let scratch = Scratch::build(&corpus.tree);
    for case in &corpus.cases {
        let overrides = read_overrides(&resolved(&scratch, &corpus, case));
        assert!(
            !case.published_descriptions.is_empty(),
            "case {} publishes none of the descriptions its tree writes",
            case.name
        );
        for (name, description) in &case.published_descriptions {
            assert_eq!(
                overrides.get(name),
                Some(description),
                "case {} publishes a different description for {name} than the reference",
                case.name
            );
            assert!(
                !case.unmatched_stems.contains(name),
                "the corpus both published and withheld {name} in case {}",
                case.name
            );
        }
    }
}

/// Pins the one position this port answers differently, so the divergence stays
/// measured instead of aging into an unnoticed gap.
///
/// The reference's first search path is the package directory holding its
/// builtin implementations and their prompt files, and the corpus records how
/// many tools it described from there. This port compiles those descriptions
/// in, so it has no such directory: the compiled text is what a search path
/// overrides. Everything after that first entry is compared position by
/// position in [`the_search_path_order_matches_the_reference`].
#[test]
fn the_reference_reads_its_builtin_descriptions_from_disk() {
    let corpus = corpus();
    let scratch = Scratch::build(&corpus.tree);
    for case in &corpus.cases {
        assert!(
            case.builtin_described_count > 0,
            "case {} records no builtin description, so the capture read no builtin directory \
             and the divergence below is unmeasured",
            case.name
        );
        assert!(
            !resolved(&scratch, &corpus, case)
                .iter()
                .any(|path| scratch.label(path) == corpus.builtin_label),
            "this port resolved a builtin tools directory, which it does not have"
        );
    }
}

/// Recaptures against the local checkout and asserts the committed corpus is
/// still what the pinned reference answers.
///
/// This is the only test here that needs the checkout, and it skips naming the
/// pin and the way back when the checkout is absent or off-pin. Everything
/// above replays regardless, which is what keeps a missing checkout from
/// failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "tool descriptions") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let repository = repo_root();
    let recaptured = repository.join("target/tool-descriptions-corpus.json");
    let output = Command::new("python3")
        .arg(repository.join(CAPTURE_SCRIPT))
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(repository.join("target/tool-descriptions-full.json"))
        .arg("--corpus")
        .arg(&recaptured)
        .current_dir(&repository)
        .output()
        .expect("the tool-descriptions capture script runs");
    assert!(
        output.status.success(),
        "the tool-descriptions capture failed: {}",
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
