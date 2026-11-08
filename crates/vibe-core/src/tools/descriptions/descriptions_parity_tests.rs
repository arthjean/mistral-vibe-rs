//! Replays the tool-directory corpus captured from the pinned reference.
//!
//! Up to the v2 line the reference read `<tools-dir>/prompts/<name>.md`
//! overrides from the `tool_paths` entries and the project and user tool
//! directories, and the corpus recorded which file won. The v3 line deleted
//! custom tools, `tool_paths` and those overrides with them, so
//! `scripts/parity/tool_descriptions.py` now records the successor over the
//! same kind of scratch tree: the tool directories the session lists, the
//! files the reference reports there as having no effect (by the branch that
//! reported each, never its sentence), and whether every builtin tool the tree
//! is named after still publishes its packaged description.
//!
//! This module rebuilds the tree, answers the same three observations from
//! the port, and replays them through a [`Tally`]. The port still resolves
//! `tool_paths` and the override files and reports no ignored file, so those
//! two observations differ in every scenario; the ledger holds them as gaps
//! of row 4 until the port follows the reference.
//!
//! Every file in the corpus is this repository's own text, and every path is a
//! label relative to the scratch tree, so nothing the reference authored is
//! committed here. The replay runs unconditionally. Only
//! [`the_committed_corpus_still_matches_the_pinned_reference`] needs the
//! checkout, and it skips when the checkout is absent or off-pin.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::{ConfigPaths, ConfigSource, HarnessFiles};
use crate::parity::is_pin_commit;
use crate::parity::replay::{Class, PatternDivergence, Tally, differences, entries};

use super::{SearchInputs, read_overrides, search_paths};

const CORPUS_RELATIVE: &str = "crates/vibe-core/tests/tool-descriptions/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/tool_descriptions.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 2;
/// The scenario floor, so a corpus regenerated against a broken capture cannot
/// pass as a green run by recording almost nothing.
const MINIMUM_SCENARIOS: usize = 7;

/// Why the port departs from the reference here: it still reads what v3
/// stopped reading.
const OVERRIDES_KEPT: &str = "the port still honors `tool_paths` and the \
     `<tools-dir>/prompts/<name>.md` description overrides that v3 removed together with custom \
     tools (scripts/parity/tool_descriptions.py), so a tree file still redescribes a builtin here";
const IGNORED_UNREPORTED: &str = "the port still honors `tool_paths` and the tool-directory \
     overrides that v3 removed, so it reports none of the custom tool modules or the former \
     `task` description override that v3 lists as having no effect";

/// The differences this port keeps, each a gap of row 4.
const LEDGER: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "*",
        pointer: "/descriptionsUnchanged",
        count: 7,
        row: "4",
        class: Class::Gap,
        reason: OVERRIDES_KEPT,
    },
    PatternDivergence {
        scenario: "*",
        pointer: "/ignored/*",
        count: 18,
        row: "4",
        class: Class::Gap,
        reason: IGNORED_UNREPORTED,
    },
];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference: Reference,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    /// Where the scratch vibe home sits, and therefore where the user tool
    /// directory hangs off.
    vibe_home: String,
    tree: Tree,
    /// The tree's description stems that name a builtin tool of the reference.
    redescribable: Vec<String>,
    scenarios: Vec<Scenario>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tree {
    /// Every file the capture authored, keyed by its tree-relative label.
    files: BTreeMap<String, String>,
    /// Paths where a directory stands in for a file, which is the one spelling
    /// of "cannot be read" every host and every privilege level agrees on.
    obstructions: Vec<String>,
    /// Directories the tree needs beyond the ones its files imply.
    directories: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    name: String,
    scenario: Inputs,
    /// The tool directories, the ignored files and the description verdict,
    /// compared as one observation.
    observed: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Inputs {
    sources: Vec<String>,
    trusted: bool,
    /// Read by this port only: the reference dropped the setting at v3.
    tool_paths: Vec<String>,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

fn corpus_text() -> String {
    fs::read_to_string(repo_root().join(CORPUS_RELATIVE)).expect("the corpus is committed")
}

fn corpus() -> Corpus {
    let corpus: Corpus = serde_json::from_str(&corpus_text()).expect("the corpus parses");
    assert_eq!(
        corpus.schema_version, CORPUS_SCHEMA_VERSION,
        "the corpus layout moved; regenerate with `{CAPTURE_SCRIPT} --corpus`"
    );
    assert!(
        is_pin_commit(&corpus.reference.commit),
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
        let root = crate::platform::canonicalize(temporary.path())
            .expect("the temporary root canonicalizes");
        for relative in &tree.directories {
            fs::create_dir_all(root.join(relative)).expect("scratch directory");
        }
        for (label, text) in &tree.files {
            let target = root.join(label);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).expect("scratch directory");
            }
            fs::write(&target, text).expect("scratch file");
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

fn sources_of(inputs: &Inputs) -> BTreeSet<ConfigSource> {
    inputs
        .sources
        .iter()
        .map(|source| match source.as_str() {
            "project" => ConfigSource::Project,
            "user" => ConfigSource::User,
            other => panic!("the corpus names an unknown configuration source: {other}"),
        })
        .collect()
}

/// What this port answers for one scenario, in the corpus's observation shape.
fn port_observation(scratch: &Scratch, corpus: &Corpus, inputs: &Inputs) -> Value {
    let harness = HarnessFiles::new(
        ConfigPaths {
            vibe_home: scratch.root.join(&corpus.vibe_home),
            working_directory: scratch.root.clone(),
        },
        sources_of(inputs),
        Vec::new(),
        inputs.trusted,
    );
    let projects = harness.project_tools_dirs();
    let user = harness.user_tools_dirs();
    // The project directories, then the user ones, deduplicated once resolved,
    // as the reference's session runtime lists them.
    let mut tool_dirs: Vec<PathBuf> = Vec::new();
    for directory in projects.iter().chain(&user) {
        let resolved =
            crate::platform::canonicalize(directory).unwrap_or_else(|_| directory.clone());
        if !tool_dirs.contains(&resolved) {
            tool_dirs.push(resolved);
        }
    }
    let overrides = read_overrides(&search_paths(&SearchInputs {
        configured: &inputs.tool_paths,
        projects: &projects,
        user: &user,
        user_home: Some(&scratch.root),
        working_directory: &scratch.root,
    }));
    let unchanged = !corpus
        .redescribable
        .iter()
        .any(|name| overrides.contains_key(name));
    json!({
        "toolDirs": tool_dirs.iter().map(|path| scratch.label(path)).collect::<Vec<_>>(),
        // The port lists no tool-directory file as ignored: it reads them all.
        "ignored": [],
        "descriptionsUnchanged": unchanged,
    })
}

#[test]
fn the_corpus_covers_every_rule_the_replay_claims() {
    let corpus = corpus();
    assert!(
        corpus.scenarios.len() >= MINIMUM_SCENARIOS,
        "the corpus records {} scenarios and this replay covers {MINIMUM_SCENARIOS}; regenerate \
         with `{CAPTURE_SCRIPT} --corpus`",
        corpus.scenarios.len()
    );
    let names: BTreeSet<&str> = corpus
        .scenarios
        .iter()
        .map(|scenario| scenario.name.as_str())
        .collect();
    assert_eq!(
        names.len(),
        corpus.scenarios.len(),
        "two scenarios share a name"
    );
    assert!(
        !corpus.redescribable.is_empty(),
        "no tree file is named after a builtin tool, so the description verdict is vacuous"
    );
    assert!(
        corpus
            .scenarios
            .iter()
            .any(|scenario| !scenario.scenario.trusted),
        "no scenario resolves an untrusted workspace"
    );
    assert!(
        corpus.scenarios.iter().any(|scenario| !scenario
            .scenario
            .sources
            .iter()
            .any(|source| source == "user")),
        "no scenario disables the user source"
    );
    let kinds: BTreeSet<&str> = corpus
        .scenarios
        .iter()
        .flat_map(|scenario| {
            scenario.observed["ignored"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|entry| entry["kind"].as_str())
        })
        .collect();
    assert_eq!(
        kinds,
        BTreeSet::from(["customTool", "descriptionOverride"]),
        "the corpus does not record both branches of the ignored-file listing"
    );
}

#[test]
fn the_tool_directories_and_ignored_files_match_the_reference() {
    let text = corpus_text();
    let corpus = corpus();
    let scratch = Scratch::build(&corpus.tree);
    let mut tally = Tally::new(&text, entries(LEDGER));
    for scenario in &corpus.scenarios {
        let port = port_observation(&scratch, &corpus, &scenario.scenario);
        let found = differences(&scenario.observed, &port);
        tally.record(&scenario.name, &scenario.observed, &port, found);
    }
    tally.conclude("tool descriptions parity:");
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
    let Some(root) = crate::parity::replay::pinned_reference("tool descriptions") else {
        return;
    };
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
    let committed: Value = serde_json::from_str(&corpus_text()).expect("the corpus parses");
    assert_eq!(
        fresh, committed,
        "the pinned reference no longer answers what the committed corpus records; regenerate it \
         with `{CAPTURE_SCRIPT} --corpus`"
    );
}
