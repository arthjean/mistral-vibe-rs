//! Differential oracle for the `@` completion index.
//!
//! `scripts/parity/autocompletion.py` drives the reference's own ignore-rule
//! compiler, index store and path ranking over scratch trees the script
//! authors, and records verdicts, entry sets, counters and scores. This module
//! replays that corpus against this build: every fixture tree is materialized
//! again under a temporary root, the same probes and queries are asked of this
//! port's readers, and each answer is compared field by field.
//!
//! Two normalizations apply on both sides and the corpus `note` carries them.
//! The reference holds a rebuilt index in `scandir` order and a mutated one in
//! relative-path order, so entry lists are compared sorted by relative path,
//! the same normalization the `grep` row of `docs/parity.md` already records.
//! Fuzzy scores are floats upstream and integers here, so the corpus records
//! them in hundredths, which is the scale [`super::super::fuzzy`] computes in.
//!
//! The `changes` family drives this build's incremental store exactly as the
//! capture drives the reference's: the index is built over the fixture tree,
//! each step mutates the tree and hands the store the change list the capture
//! recorded, and the resulting entry set and counters are compared.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::Value;

use vibe_core::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};

use super::super::{CompletionEngine, CompletionRequest, CompletionResolution};
use super::{
    ASCII_CODEPOINT_LIMIT, ChangeKind, DEFAULT_IGNORE_PATTERNS, IgnoreRule, IgnoreRules,
    IndexedPath, MASS_CHANGE_THRESHOLD, MAX_INDEXED_ENTRIES, MAX_PATH_MATCHES, WorkspaceIndex,
    build_ascii_mask, fuzzy_match_score, path_match_rank, path_search_context, rank_indexed_paths,
};

const CORPUS_RELATIVE: &str = "crates/vibe-cli/tests/autocompletion/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/autocompletion.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 1;
/// The comparison floor this replay commits to, so a regeneration that
/// captured almost nothing fails instead of reporting a clean but empty run.
const MINIMUM_SCENARIOS: usize = 150;
/// The scale the corpus records a fuzzy score in, which is the one this port's
/// matcher already computes in. A capture that rescaled would silently move
/// every score, so the constant is compared rather than assumed.
const SCORE_SCALE: i64 = 100;

/// Every family the corpus declares and this replay reads. A family the
/// capture adds without a reader here fails the replay by name rather than
/// passing unread, and so does a family this replay expects and the corpus
/// dropped.
const FAMILIES: [&str; 5] = ["constants", "ignoreRules", "walk", "changes", "ranking"];

/// Keys the corpus carries that are not families: the pin, the layout, the
/// prose-free note and the fixture declarations every family is measured over.
const METADATA: [&str; 4] = ["schemaVersion", "reference", "note", "fixtures"];

/// Cases where this build answers something other than the reference, each
/// with the reason. A case that conforms while listed here fails the replay as
/// a stale entry, and a case that diverges without an entry fails naming the
/// family, the case and the observed and expected values.
///
/// One entry: the non-goal `WALK_SKIP_DIR_NAMES`, exported by the reference
/// and imported by nothing at the pinned commit.
const DIVERGENCES: &[(&str, &str)] = &[(
    "constants/walkSkipDirNames",
    "ACCEPTED: `WALK_SKIP_DIR_NAMES` is exported by the reference and imported by nothing at the \
     pinned commit, so this port ships no derived constant with no consumer; \
     docs/parity.md records it as a non-goal",
)];

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
    constants: Constants,
    fixtures: Vec<Fixture>,
    ignore_rules: Vec<IgnoreCase>,
    walk: Vec<WalkCase>,
    changes: Vec<ChangeCase>,
    ranking: Vec<RankingCase>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Constants {
    max_entries_to_process: usize,
    target_matches: usize,
    mass_change_threshold: u64,
    ascii_codepoint_limit: u32,
    score_scale: i64,
    #[expect(
        dead_code,
        reason = "the three strategy multipliers are proven through every fuzzyScore the \
                  ranking family compares, not as constants this port names separately"
    )]
    fuzzy_multipliers: BTreeMap<String, i64>,
    default_ignore_patterns: Vec<DefaultPattern>,
    compiled_default_patterns: Vec<CompiledPattern>,
    walk_skip_dir_names: Vec<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DefaultPattern {
    raw: String,
    is_exclude: bool,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompiledPattern {
    stripped: String,
    is_exclude: bool,
    dir_only: bool,
    name_only: bool,
    anchor_root: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Fixture {
    id: String,
    /// A path ending in `/` is a directory; every other path is a file whose
    /// parents are created with it.
    tree: Vec<String>,
    gitignore: Option<String>,
    file_body: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IgnoreCase {
    case: String,
    fixture: String,
    rel: String,
    name: String,
    is_dir: bool,
    ignored: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WalkCase {
    case: String,
    fixture: String,
    entries: Vec<Entry>,
    stats: Stats,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Entry {
    rel: String,
    name: String,
    is_dir: bool,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Stats {
    rebuilds: u64,
    incremental_updates: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChangeCase {
    case: String,
    fixture: String,
    steps: Vec<ChangeStep>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChangeStep {
    step: usize,
    mutations: Vec<Mutation>,
    /// The `(kind, path)` pairs the reference handed `apply_changes`, which is
    /// what this build's incremental store is handed too.
    changes: Vec<Vec<String>>,
    entries: Vec<Entry>,
    stats: Stats,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Mutation {
    op: String,
    path: String,
    #[serde(default)]
    to: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RankingCase {
    case: String,
    fixture: String,
    query: String,
    candidates: Vec<RankedCandidate>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RankedCandidate {
    label: String,
    rank: Rank,
}

/// The reference's ten ranking components, as integers. Booleans are recorded
/// as 0 and 1 because the reference sorts an integer tuple.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Rank {
    exact_directory: i64,
    immediate_child_of_exact_path: i64,
    exact_filename: i64,
    preferred_stem_match: i64,
    exact_stem: i64,
    stem_prefix: i64,
    name_prefix: i64,
    extension_match: i64,
    fuzzy_score: i64,
    shallow_path: i64,
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
    let corpus: Corpus = serde_json::from_str(&raw).expect("the autocompletion corpus parses");
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
        case: &str,
        field: &str,
        expected: &T,
        actual: &T,
    ) {
        self.total = self.total.saturating_add(1);
        if expected == actual {
            self.conformant = self.conformant.saturating_add(1);
            return;
        }
        self.observed.push(format!("{family}/{case}"));
        self.divergences.push(format!(
            "{family}/{case}: {field} diverges: reference {expected:?}, port {actual:?}"
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
        "autocompletion: {family} {}/{} conform ({ledgered} ledgered)",
        report.conformant, report.total
    );
    report.total
}

// --------------------------------------------------------------------------
// The fixtures
// --------------------------------------------------------------------------

/// One materialized fixture tree, kept alive for as long as it is queried.
struct Scratch {
    _enclosure: tempfile::TempDir,
    root: PathBuf,
}

impl Scratch {
    /// Writes `fixture` under a fresh temporary root, inside an enclosing
    /// directory so a change sequence can name a path outside the index root.
    fn materialize(fixture: &Fixture) -> Self {
        let enclosure = tempfile::tempdir().expect("temporary fixture enclosure");
        let root = enclosure.path().join("root");
        fs::create_dir(&root).expect("fixture root");
        for entry in &fixture.tree {
            if let Some(directory) = entry.strip_suffix('/') {
                fs::create_dir_all(root.join(directory)).expect("fixture directory");
                continue;
            }
            let target = root.join(entry);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).expect("fixture parent");
            }
            fs::write(&target, &fixture.file_body).expect("fixture file");
        }
        if let Some(gitignore) = &fixture.gitignore {
            fs::write(root.join(".gitignore"), gitignore).expect("fixture ignore file");
        }
        Self {
            _enclosure: enclosure,
            root,
        }
    }

    /// Applies one scripted mutation, in the same vocabulary the capture uses.
    fn mutate(&self, mutation: &Mutation, body: &str) {
        let target = self.root.join(&mutation.path);
        match mutation.op.as_str() {
            "createFile" => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).expect("mutation parent");
                }
                fs::write(&target, body).expect("mutation file");
            }
            "createDir" => fs::create_dir_all(&target).expect("mutation directory"),
            "modifyFile" => {
                fs::write(&target, format!("{body}modified\n")).expect("mutation rewrite");
            }
            "delete" => {
                if target.is_dir() {
                    fs::remove_dir_all(&target).expect("mutation directory removal");
                } else if target.exists() {
                    fs::remove_file(&target).expect("mutation file removal");
                }
            }
            "rename" => {
                let destination = self.root.join(
                    mutation
                        .to
                        .as_deref()
                        .expect("a rename mutation names its destination"),
                );
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent).expect("mutation destination parent");
                }
                fs::rename(&target, &destination).expect("mutation rename");
            }
            other => panic!("unknown fixture mutation `{other}`"),
        }
    }
}

fn fixtures(corpus: &Corpus) -> BTreeMap<&str, &Fixture> {
    corpus
        .fixtures
        .iter()
        .map(|fixture| (fixture.id.as_str(), fixture))
        .collect()
}

fn fixture<'a>(index: &BTreeMap<&str, &'a Fixture>, id: &str) -> &'a Fixture {
    index
        .get(id)
        .copied()
        .unwrap_or_else(|| panic!("the corpus declares the fixture `{id}`"))
}

/// An index built over `root` and nothing else, which is what the capture
/// drives when it records a walk or a change sequence.
fn built(root: &Path) -> WorkspaceIndex {
    let mut index = WorkspaceIndex::default();
    index.rebuild(root);
    index
}

/// One index's entry set, in the relative-path order both sides are
/// normalized to.
fn held(index: &WorkspaceIndex) -> Vec<Entry> {
    index.entries.values().map(entry_of).collect()
}

fn entry_of(entry: &IndexedPath) -> Entry {
    Entry {
        rel: entry.rel.clone(),
        name: entry.name.clone(),
        is_dir: entry.is_directory,
    }
}

fn stats_of(index: &WorkspaceIndex) -> Stats {
    let stats = index.stats();
    Stats {
        rebuilds: stats.rebuilds,
        incremental_updates: stats.incremental_updates,
    }
}

/// The change list a step hands the store, in this build's vocabulary.
fn change_list(root: &Path, step: &ChangeStep) -> Vec<(ChangeKind, PathBuf)> {
    step.changes
        .iter()
        .map(|pair| {
            let [kind, relative] = pair.as_slice() else {
                panic!("a corpus change is a `(kind, path)` pair: {pair:?}");
            };
            let kind = match kind.as_str() {
                "added" => ChangeKind::Added,
                "modified" => ChangeKind::Modified,
                "deleted" => ChangeKind::Deleted,
                other => panic!("unknown corpus change kind `{other}`"),
            };
            (kind, root.join(relative))
        })
        .collect()
}

// --------------------------------------------------------------------------
// The families
// --------------------------------------------------------------------------

fn run_constants(constants: &Constants, report: &mut Report) {
    report.check(
        "constants",
        "maxEntriesToProcess",
        "cap",
        &constants.max_entries_to_process,
        &MAX_INDEXED_ENTRIES,
    );
    report.check(
        "constants",
        "targetMatches",
        "cap",
        &constants.target_matches,
        &MAX_PATH_MATCHES,
    );
    report.check(
        "constants",
        "scoreScale",
        "scale",
        &constants.score_scale,
        &SCORE_SCALE,
    );
    report.check(
        "constants",
        "massChangeThreshold",
        "threshold",
        &constants.mass_change_threshold,
        &(MASS_CHANGE_THRESHOLD as u64),
    );
    report.check(
        "constants",
        "asciiCodepointLimit",
        "limit",
        &constants.ascii_codepoint_limit,
        &ASCII_CODEPOINT_LIMIT,
    );
    let defaults = DEFAULT_IGNORE_PATTERNS
        .iter()
        .map(|raw| DefaultPattern {
            raw: (*raw).to_owned(),
            is_exclude: true,
        })
        .collect::<Vec<_>>();
    report.check(
        "constants",
        "defaultIgnorePatterns",
        "patterns",
        &constants.default_ignore_patterns,
        &defaults,
    );
    let compiled = DEFAULT_IGNORE_PATTERNS
        .iter()
        .filter_map(|raw| IgnoreRule::parse(raw, true))
        .map(|rule| CompiledPattern {
            stripped: rule.pattern.clone(),
            is_exclude: rule.excludes,
            dir_only: rule.directory_only,
            name_only: rule.name_only,
            anchor_root: rule.anchored_at_root,
        })
        .collect::<Vec<_>>();
    report.check(
        "constants",
        "compiledDefaultPatterns",
        "compilation",
        &constants.compiled_default_patterns,
        &compiled,
    );
    // The reference exports the skip-directory set and imports it nowhere at
    // this commit, so this port derives none.
    report.check(
        "constants",
        "walkSkipDirNames",
        "names",
        &Some(constants.walk_skip_dir_names.clone()),
        &None,
    );
}

fn run_ignore_rules(corpus: &Corpus, report: &mut Report) {
    let index = fixtures(corpus);
    let mut current: Option<(String, Scratch, IgnoreRules)> = None;
    for case in &corpus.ignore_rules {
        let reuse = current
            .as_ref()
            .is_some_and(|(id, _, _)| id == &case.fixture);
        if !reuse {
            let scratch = Scratch::materialize(fixture(&index, &case.fixture));
            let rules = IgnoreRules::load(&scratch.root);
            current = Some((case.fixture.clone(), scratch, rules));
        }
        let Some((_, _, rules)) = current.as_ref() else {
            continue;
        };
        report.check(
            "ignoreRules",
            &case.case,
            "ignored",
            &case.ignored,
            &rules.should_ignore(&case.rel, &case.name, case.is_dir),
        );
    }
}

fn run_walk(corpus: &Corpus, report: &mut Report) {
    let index = fixtures(corpus);
    for case in &corpus.walk {
        let scratch = Scratch::materialize(fixture(&index, &case.fixture));
        let workspace = built(&scratch.root);
        report.check(
            "walk",
            &case.case,
            "entries",
            &case.entries,
            &held(&workspace),
        );
        report.check(
            "walk",
            &case.case,
            "stats",
            &case.stats,
            &stats_of(&workspace),
        );
    }
}

fn run_changes(corpus: &Corpus, report: &mut Report) {
    let index = fixtures(corpus);
    for case in &corpus.changes {
        let declared = fixture(&index, &case.fixture);
        let scratch = Scratch::materialize(declared);
        let mut store = built(&scratch.root);
        for step in &case.steps {
            for mutation in &step.mutations {
                scratch.mutate(mutation, &declared.file_body);
            }
            store.apply_changes(&change_list(&scratch.root, step));
            let case_id = format!("{}#{}", case.case, step.step);
            report.check("changes", &case_id, "entries", &step.entries, &held(&store));
            report.check("changes", &case_id, "stats", &step.stats, &stats_of(&store));
        }
    }
}

fn run_ranking(corpus: &Corpus, report: &mut Report) {
    let index = fixtures(corpus);
    // One engine for the whole family: the adapter that answers every `@`
    // query in the running client, holding the one index it answers from.
    let engine = CompletionEngine::default();
    let mut current: Option<(String, Scratch, WorkspaceIndex)> = None;
    for case in &corpus.ranking {
        let reuse = current
            .as_ref()
            .is_some_and(|(id, _, _)| id == &case.fixture);
        if !reuse {
            let scratch = Scratch::materialize(fixture(&index, &case.fixture));
            let entries = built(&scratch.root);
            current = Some((case.fixture.clone(), scratch, entries));
        }
        let Some((_, scratch, entries)) = current.as_ref() else {
            continue;
        };
        let query = format!("@{}", case.query);
        let resolution = engine.resolve_request(
            CompletionRequest::new(0, 0..query.chars().count(), query),
            &scratch.root,
        );
        let CompletionResolution::Results { candidates, .. } = resolution else {
            panic!("the fixture root answers a mention query: {}", case.case);
        };
        let context = path_search_context(&case.query);
        let by_label = entries
            .entries
            .values()
            .map(|entry| {
                let suffix = if entry.is_directory { "/" } else { "" };
                (format!("@{}{suffix}", entry.rel), entry)
            })
            .collect::<BTreeMap<_, _>>();
        let ranked = candidates
            .iter()
            .map(|candidate| {
                let entry = by_label
                    .get(&candidate.label)
                    .unwrap_or_else(|| panic!("`{}` names an indexed entry", candidate.label));
                let score = if context.search_pattern.is_empty() {
                    0
                } else {
                    fuzzy_match_score(context.search_pattern, &entry.rel).unwrap_or_default()
                };
                let rank = path_match_rank(entry, &context, score);
                RankedCandidate {
                    label: candidate.label.clone(),
                    rank: Rank {
                        exact_directory: i64::from(rank.exact_directory),
                        immediate_child_of_exact_path: i64::from(
                            rank.immediate_child_of_exact_path,
                        ),
                        exact_filename: i64::from(rank.exact_filename),
                        preferred_stem_match: i64::from(rank.preferred_stem_match),
                        exact_stem: i64::from(rank.exact_stem),
                        stem_prefix: i64::from(rank.stem_prefix),
                        name_prefix: i64::from(rank.name_prefix),
                        extension_match: i64::from(rank.extension_match),
                        fuzzy_score: rank.fuzzy_score,
                        shallow_path: i64::from(rank.shallow_path),
                    },
                }
            })
            .collect::<Vec<_>>();
        report.check(
            "ranking",
            &case.case,
            "candidates",
            &case.candidates,
            &ranked,
        );
    }
}

// --------------------------------------------------------------------------
// The replay
// --------------------------------------------------------------------------

/// The corpus declares exactly the families this replay reads. A family the
/// capture adds without a reader here fails by name rather than passing
/// unread, which is the way a corpus silently outgrows its runner.
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

/// A divergence no ledger entry names is what the replay exists to fail on,
/// and a ledger entry whose divergence stopped reproducing is a decision that
/// outlived its cause. Both verdicts are computed by [`audit`], so they are
/// asserted here rather than only through the corpus that currently exhibits
/// neither.
#[test]
fn the_ledger_fails_an_unrecorded_divergence_and_a_stale_entry() {
    let ledger: &[(&str, &str)] = &[("walk/plain", "recorded"), ("changes/*", "recorded")];

    let mut report = Report::default();
    report.check("walk", "plain", "entries", &1, &2);
    let (unrecorded, stale) = audit(&report, "walk", ledger);
    assert!(unrecorded.is_empty(), "a recorded divergence passes");
    assert!(stale.is_empty(), "the entry that reproduced is not stale");

    let mut report = Report::default();
    report.check("walk", "hidden", "entries", &1, &2);
    let (unrecorded, stale) = audit(&report, "walk", ledger);
    assert_eq!(
        unrecorded.len(),
        1,
        "an unnamed divergence is reported: {unrecorded:?}"
    );
    assert!(
        unrecorded[0].starts_with("walk/hidden: entries diverges:"),
        "the failure names the family, the case and the field: {unrecorded:?}"
    );
    assert_eq!(
        stale,
        vec!["walk/plain".to_owned()],
        "the entry whose divergence stopped reproducing is stale"
    );

    let mut report = Report::default();
    report.check("changes", "add-one-file#0", "entries", &1, &1);
    let (unrecorded, stale) = audit(&report, "changes", ledger);
    assert!(unrecorded.is_empty(), "a conforming family reports nothing");
    assert_eq!(
        stale,
        vec!["changes/*".to_owned()],
        "a prefix entry goes stale once its whole family conforms"
    );
}

#[test]
fn the_committed_corpus_replays_against_this_port() {
    let corpus = corpus();
    println!(
        "autocompletion: divergence ledger ({} entries)",
        DIVERGENCES.len()
    );
    for (case, reason) in DIVERGENCES {
        println!("  {case}: {reason}");
    }
    let mut scenarios = 0;
    let mut report = Report::default();
    run_constants(&corpus.constants, &mut report);
    scenarios += settle(&report, "constants");
    let mut report = Report::default();
    run_ignore_rules(&corpus, &mut report);
    scenarios += settle(&report, "ignoreRules");
    let mut report = Report::default();
    run_walk(&corpus, &mut report);
    scenarios += settle(&report, "walk");
    let mut report = Report::default();
    run_changes(&corpus, &mut report);
    scenarios += settle(&report, "changes");
    let mut report = Report::default();
    run_ranking(&corpus, &mut report);
    scenarios += settle(&report, "ranking");
    println!(
        "autocompletion: {scenarios} comparisons across {} families replayed at {}",
        FAMILIES.len(),
        &corpus.reference.commit[..12],
    );
    assert!(
        scenarios >= MINIMUM_SCENARIOS,
        "the corpus replays {scenarios} comparisons, below the {MINIMUM_SCENARIOS} floor; \
         regenerate it with {CAPTURE_SCRIPT}"
    );
}

/// The ASCII mask is a prefilter, so it may only remove entries the matcher
/// would have rejected anyway. Every fixture tree and every recorded query is
/// answered twice, once with the filter and once without, and the two candidate
/// lists must be identical: an asymmetry between how an entry's mask and a
/// query's mask fold case would show up here as a dropped candidate.
#[test]
fn the_ascii_mask_filter_never_removes_a_candidate() {
    let corpus = corpus();
    let index = fixtures(&corpus);
    let mut compared = 0;
    let mut current: Option<(String, Scratch, WorkspaceIndex)> = None;
    for case in &corpus.ranking {
        let reuse = current
            .as_ref()
            .is_some_and(|(id, _, _)| id == &case.fixture);
        if !reuse {
            let scratch = Scratch::materialize(fixture(&index, &case.fixture));
            let entries = built(&scratch.root);
            current = Some((case.fixture.clone(), scratch, entries));
        }
        let Some((_, _, entries)) = current.as_ref() else {
            continue;
        };
        let filtered = rank_indexed_paths(entries.entries.values(), &case.query, true);
        let unfiltered = rank_indexed_paths(entries.entries.values(), &case.query, false);
        assert_eq!(
            filtered, unfiltered,
            "the mask filter changed the candidates for `{}`",
            case.case
        );
        compared += 1;
    }
    println!("autocompletion: asciiMask {compared}/{compared} queries unchanged by the filter");
    assert!(
        compared > 0,
        "the corpus carries ranking queries to compare"
    );
}

/// The mask is one bit per ASCII codepoint of the lowercased relative path,
/// and a wider codepoint contributes none, which is why a query carrying one
/// disables the filter instead of narrowing it.
#[test]
fn the_entry_mask_records_one_bit_per_ascii_codepoint() {
    assert_eq!(build_ascii_mask(""), 0);
    assert_eq!(build_ascii_mask("a"), 1_u128 << u32::from('a'));
    assert_eq!(build_ascii_mask("aa"), 1_u128 << u32::from('a'));
    assert_eq!(
        build_ascii_mask("ab"),
        (1_u128 << u32::from('a')) | (1_u128 << u32::from('b'))
    );
    assert_eq!(
        build_ascii_mask("café"),
        build_ascii_mask("caf"),
        "a codepoint at or above the limit contributes no bit"
    );
    assert!(
        u32::from('\u{7f}') < ASCII_CODEPOINT_LIMIT,
        "the mask covers the whole ASCII range"
    );
}

/// The corpus is only an oracle for as long as it still describes the pinned
/// reference. This probe recaptures it where the checkout is present and on
/// the pin, and skips everywhere else naming the pin and the way back.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "autocompletion") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let repository = repo_root();
    let script = repository.join(CAPTURE_SCRIPT);
    let recaptured = repository.join("target/autocompletion-corpus.json");
    let output = Command::new("python3")
        .arg(&script)
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(repository.join("target/autocompletion-full.json"))
        .arg("--corpus")
        .arg(&recaptured)
        .current_dir(&repository)
        .output()
        .expect("the autocompletion capture script runs");
    assert!(
        output.status.success(),
        "the autocompletion capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let fresh = fs::read_to_string(&recaptured).expect("the recaptured corpus is readable");
    let committed =
        fs::read_to_string(repository.join(CORPUS_RELATIVE)).expect("the corpus is readable");
    let fresh: Value = serde_json::from_str(&fresh).expect("the recaptured corpus parses");
    let committed: Value = serde_json::from_str(&committed).expect("the corpus parses");
    assert_eq!(
        fresh, committed,
        "the pinned reference no longer answers what the committed corpus records; regenerate \
         it with `{CAPTURE_SCRIPT}`"
    );
}
