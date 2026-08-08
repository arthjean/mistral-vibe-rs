//! Differential oracle for the skills surface.
//!
//! `scripts/parity/skills.py` drives the reference's own parser, schema,
//! manager, registry store and registry manifests over synthetic inputs and
//! records what each one answers. This module replays that corpus, family by
//! family, against what the port answers today.
//!
//! The corpus is committed and replayed unconditionally: it carries
//! scenario-supplied values, field names, verdicts, counts and digests, and no
//! reference-authored sentence, which is what `NOTICE` allows. Only the live
//! recapture probe skips, and it names the pin and the way back when it does.
//!
//! The oracle precedes the implementation, so the ledger below is the measured
//! defect inventory rather than a residue: a family the port cannot answer yet
//! is one `family/*` entry naming the story that implements it, and the stale
//! check retires each entry the moment its family conforms. `frontmatter`,
//! `metadata` and `projection` are compared for real since EP-047 landed the
//! parser, the schema and the whole model. `discovery` and `filtering` are
//! compared for real since EP-048 landed the five roots, the configured paths
//! and the two filter keys: the wiring reproduced here is
//! `Release3Service::skill_discovery`, which resolves the roots through
//! `search_paths` and hands them plus the filters to `discover_extensions`. No
//! builtin is seeded yet, which is the whole of what those two families still
//! diverge on.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::extensions::{DiscoveryRoots, SkillDefinition, discover_extensions};
use crate::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};
use crate::skills::parser::{SkillParseErrorKind, parse_skill_markdown};
use crate::skills::schema::SkillMetadata;
use crate::skills::{
    SearchInputs, SkillDiscovery, SkillScope, SkillSource, apply_filters, search_paths,
    skill_summary,
};

const CORPUS_RELATIVE: &str = "crates/vibe-core/tests/skills/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/skills.py";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 1;
/// The scenario floor the skills-parity PRD commits to, so a regeneration that
/// captured almost nothing fails instead of reporting a clean but empty run.
const MINIMUM_SCENARIOS: usize = 120;

/// Cases where this port answers something other than the reference, each with
/// the reason. A case that conforms while listed here fails the replay as a
/// stale entry, and a case that diverges without an entry fails naming the
/// family, the case and the observed and expected values.
///
/// A `family/*` entry covers every case of its family, and goes stale only
/// when the whole family conforms: the oracle shipped before the
/// implementation, so these entries are the measured backlog of the
/// skills-parity PRD, each naming the story that retires it.
const DIVERGENCES: &[(&str, &str)] = &[
    (
        "discovery/*",
        "PENDING US-169: EP-048 landed the five roots, the configured paths and the filter, so \
         every scenario now agrees on the roots it walks and on the disk skills it publishes; \
         the two builtins the reference seeds are still missing from every published set",
    ),
    (
        "discovery/legacy-extensions-root-unread",
        "ACCEPTED: US-165 keeps `{vibe_home}/extensions/skills` readable as a deprecated root \
         this port published before the documented ones existed, so a skill sitting there is \
         published here and unread upstream",
    ),
    (
        "filtering/*",
        "PENDING US-169: the filter itself conforms, and the cases whose kept set contains a \
         builtin diverge because no builtin is seeded yet",
    ),
    (
        "command/*",
        "PENDING US-172: slash-command parsing lives in the CLI adapter, so vibe-core has no \
         parse_skill_command to answer",
    ),
    (
        "store/*",
        "PENDING US-176: the registry version store is not ported",
    ),
    (
        "manifest/*",
        "PENDING US-177: the registry manifests are not ported",
    ),
];

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
    /// The builtin catalog by digest only, recorded for US-170's ledger check;
    /// nothing in the port publishes builtins yet.
    #[expect(dead_code, reason = "compared once US-169 seeds a builtin catalog")]
    builtins: Builtins,
    frontmatter: Vec<FrontmatterCase>,
    metadata: Vec<MetadataCase>,
    discovery: Vec<DiscoveryScenario>,
    filtering: Vec<FilteringCase>,
    command: CommandFamily,
    projection: Vec<ProjectionCase>,
    store: StoreFamily,
    manifest: Vec<ManifestCase>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[expect(dead_code, reason = "compared once US-169 seeds a builtin catalog")]
struct Builtins {
    count: usize,
    skills: Vec<BuiltinSkill>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[expect(dead_code, reason = "compared once US-169 seeds a builtin catalog")]
struct BuiltinSkill {
    name: String,
    user_invocable: bool,
    has_path: bool,
    source: String,
    scope: String,
    description: Digested,
    prompt: Digested,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Digested {
    length: usize,
    digest: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FrontmatterCase {
    case: String,
    content: String,
    /// `boundary`, `yaml` or `mapping` when the reference rejected the
    /// document; absent when it parsed.
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    frontmatter: Option<Value>,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MetadataCase {
    case: String,
    frontmatter: Value,
    accepted: bool,
    #[serde(default)]
    fields: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DiscoveryScenario {
    case: String,
    skill_paths: Vec<String>,
    enabled_skills: Vec<String>,
    disabled_skills: Vec<String>,
    project_trusted: bool,
    symlinks: Vec<SymlinkSpec>,
    tree: BTreeMap<String, BTreeMap<String, String>>,
    /// The resolved search roots the reference walked, as `[label, relative]`
    /// pairs.
    search_paths: Vec<Vec<String>>,
    published: Vec<PublishedSkill>,
    issues: Vec<RecordedIssue>,
    custom_skills_count: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SymlinkSpec {
    link: String,
    target: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PublishedSkill {
    name: String,
    source: String,
    /// Always `global` at this pin, even for project skills.
    scope: String,
    root: Option<String>,
    rel_path: Option<String>,
    user_invocable: bool,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecordedIssue {
    root: String,
    rel_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FilteringCase {
    case: String,
    skills: Vec<String>,
    enabled_skills: Vec<String>,
    disabled_skills: Vec<String>,
    kept: Vec<String>,
    #[expect(dead_code, reason = "US-171 compares the custom count")]
    custom_skills_count: usize,
    /// Always true upstream, and structural here: the filter runs inside
    /// `discover_extensions`, which is the same call the `skill` tool makes, so
    /// `a_filtered_skill_is_invisible_to_the_skill_tool` proves it rather than
    /// this replay.
    #[expect(
        dead_code,
        reason = "proven by a behavior test rather than by the replay"
    )]
    withheld_lookup_misses: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommandFamily {
    #[expect(dead_code, reason = "US-172's parser replays over these fixtures")]
    skills: Vec<CommandFixture>,
    cases: Vec<CommandCase>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[expect(dead_code, reason = "US-172's parser replays over these fixtures")]
struct CommandFixture {
    name: String,
    description: String,
    invocable: bool,
    body: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommandCase {
    case: String,
    #[expect(dead_code, reason = "US-172's parser replays the prompt")]
    prompt: String,
    result: Option<CommandResult>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommandResult {
    name: String,
    #[expect(dead_code, reason = "US-172 compares the trailing instructions")]
    extra_instructions: Option<String>,
    #[expect(dead_code, reason = "US-172 compares the delivered body by digest")]
    content: Digested,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProjectionCase {
    case: String,
    skill: Value,
    summary: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoreFamily {
    /// The store's fallback description, reference-authored prose recorded by
    /// digest and masked as `{fallbackDescription}` in the recorded trees.
    #[expect(
        dead_code,
        reason = "US-176 writes its own fallback and ledgers the digest"
    )]
    fallback_description: Digested,
    cases: Vec<StoreCase>,
}

/// One store scenario. The record is heterogeneous by `op`, so every
/// field beyond the two discriminants is optional; US-176 reads them back.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoreCase {
    case: String,
    op: String,
    #[serde(default)]
    #[expect(dead_code, reason = "US-176 replays the scenario inputs")]
    id: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-176 replays the scenario inputs")]
    version: Option<i64>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-176 replays the scenario inputs")]
    item: Option<Value>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-176 replays the scenario inputs")]
    name: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-176 replays the scenario inputs")]
    prior: Option<Value>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-176 replays the scenario inputs")]
    fail_write: Option<bool>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-176 replays the scenario inputs")]
    active: Option<Value>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-176 compares the resolved path")]
    rel_path: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-176 compares the written tree")]
    tree: Option<Value>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-176 asserts no staging residue survives")]
    leftover_entries: Option<Vec<String>>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-176 compares the alias resolution")]
    latest: Option<i64>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-176 compares what pruning left behind")]
    surviving: Option<Vec<String>>,
}

/// One manifest scenario, heterogeneous the same way; US-177 reads it back.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestCase {
    case: String,
    #[serde(default)]
    #[expect(dead_code, reason = "US-177 replays the scenario inputs")]
    version: Option<Value>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-177 compares the alias projection")]
    alias: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-177 compares the declared default")]
    default_version_applied: Option<Value>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-177 compares the entry lists")]
    skills: Option<Value>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-177 compares the removal verdicts")]
    removed_existing: Option<bool>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-177 compares the removal verdicts")]
    removed_missing: Option<bool>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-177 compares the saved document")]
    toml: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-177 asserts the round trip is lossless")]
    lossless: Option<bool>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-177 asserts the parent directory is created")]
    created: Option<bool>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-177 compares the global manifest location")]
    rel_path: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-177 replays the project-root inputs")]
    roots: Option<Vec<String>>,
    #[serde(default)]
    #[expect(dead_code, reason = "US-177 compares the collected project paths")]
    paths: Option<Value>,
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
    let corpus: Corpus = serde_json::from_str(&raw).expect("the skills corpus parses");
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

/// The divergence ledger as a lookup, keyed `family/case` or `family/*`.
fn ledger() -> BTreeMap<String, String> {
    DIVERGENCES
        .iter()
        .map(|(case, reason)| ((*case).to_owned(), (*reason).to_owned()))
        .collect()
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
        self.total += 1;
        if expected == actual {
            self.conformant += 1;
            return;
        }
        self.observed.push(format!("{family}/{case}"));
        self.divergences.push(format!(
            "{family}/{case}: {field} diverges: reference {expected:?}, port {actual:?}"
        ));
    }

    /// Records a case the port cannot answer yet as a divergence, so the
    /// pending family stays visible in the counts and its `family/*` ledger
    /// entry goes stale the moment a real comparator lands without one.
    fn pending(&mut self, family: &str, case: &str, expected: String, story: &str) {
        self.check(
            family,
            case,
            "answer",
            &expected,
            &format!("no port counterpart until {story}"),
        );
    }
}

/// Fails on any divergence the ledger does not name, and on any ledger entry
/// whose divergence no longer reproduces. A `family/*` entry is stale once its
/// family diverges nowhere.
fn settle(report: &Report, family: &str) -> usize {
    let recorded = ledger();
    let wildcard = format!("{family}/*");
    let unrecorded = report
        .divergences
        .iter()
        .filter(|line| {
            let key = line.split(':').next().unwrap_or_default();
            !recorded.contains_key(key) && !recorded.contains_key(&wildcard)
        })
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        unrecorded.is_empty(),
        "{family} diverges from the reference and is unrecorded:\n{}",
        unrecorded.join("\n")
    );
    let stale = recorded
        .keys()
        .filter(|key| {
            if **key == wildcard {
                report.total > 0 && report.divergences.is_empty()
            } else {
                key.starts_with(&format!("{family}/")) && !report.observed.contains(key)
            }
        })
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "these {family} entries conform now and their ledger entry is stale: {stale:?}"
    );
    println!(
        "skills: {family} {}/{} conform",
        report.conformant, report.total
    );
    report.total
}

// --------------------------------------------------------------------------
// The discovery adapter
// --------------------------------------------------------------------------

/// What one side published for one skill, on the fields both sides model:
/// name, source, scope, root label, root-relative path, user invocability and
/// description.
type CatalogAnswer = (
    Vec<(String, String)>,
    Vec<(
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        bool,
        Option<String>,
    )>,
    Vec<(String, String)>,
    usize,
);

/// The wire spelling of a serialized vocabulary word, for comparing the
/// port's enums against the strings the corpus records.
fn vocabulary<T: Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_default()
}

fn expected_catalog(scenario: &DiscoveryScenario) -> CatalogAnswer {
    let search_paths = scenario
        .search_paths
        .iter()
        .filter_map(|pair| Some((pair.first()?.clone(), pair.get(1)?.clone())))
        .collect();
    let published = scenario
        .published
        .iter()
        .map(|skill| {
            (
                skill.name.clone(),
                skill.source.clone(),
                skill.scope.clone(),
                skill.root.clone(),
                skill.rel_path.clone(),
                skill.user_invocable,
                skill.description.clone(),
            )
        })
        .collect();
    let issues = scenario
        .issues
        .iter()
        .map(|issue| (issue.root.clone(), issue.rel_path.clone()))
        .collect();
    (
        search_paths,
        published,
        issues,
        scenario.custom_skills_count,
    )
}

/// The scenario's own spelling of a root, with `${label}` standing for the
/// materialized directory the capture script anchored the entry on.
fn substitute(entry: &str, roots: &BTreeMap<String, PathBuf>) -> String {
    let mut rendered = entry.to_owned();
    for (label, root) in roots {
        rendered = rendered.replace(
            &format!("${{{label}}}"),
            &root.to_string_lossy().replace('\\', "/"),
        );
    }
    rendered
}

/// The label and root-relative path of `path`, against the scenario roots. The
/// root itself is spelled `.`, which is how the capture script records it.
fn label_path(path: &Path, roots: &BTreeMap<String, PathBuf>) -> Option<(String, String)> {
    for (label, root) in roots {
        if let Ok(relative) = path.strip_prefix(root) {
            let relative = relative.to_string_lossy().replace('\\', "/");
            let relative = if relative.is_empty() {
                ".".to_owned()
            } else {
                relative
            };
            return Some((label.clone(), relative));
        }
    }
    None
}

/// The port's answer for one discovery scenario, produced by materializing the
/// scenario tree and running `discover_extensions` over the roots the two
/// production call sites wire today. [`None`] when the platform cannot build
/// the scenario, which only symlink creation can cause.
fn discovery_answer(scenario: &DiscoveryScenario) -> Option<CatalogAnswer> {
    let scratch = tempfile::tempdir().expect("a scratch directory is available");
    // The scratch root is canonicalized so the resolved paths `parse_skill`
    // now records still strip against the scenario roots.
    let scratch_root = scratch
        .path()
        .canonicalize()
        .expect("the scratch directory resolves");
    let mut roots: BTreeMap<String, PathBuf> = BTreeMap::new();
    for label in ["home", "project", "configured", "configured2"] {
        let root = scratch_root.join(label);
        fs::create_dir_all(&root).expect("the scenario root is writable");
        roots.insert(label.to_owned(), root);
    }
    for (label, files) in &scenario.tree {
        for (relative, content) in files {
            let target = roots[label].join(relative);
            fs::create_dir_all(target.parent().expect("scenario files sit under a root"))
                .expect("the scenario tree is writable");
            fs::write(&target, content).expect("the scenario file is writable");
        }
    }
    for symlink in &scenario.symlinks {
        let link = scratch_root.join(&symlink.link);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&roots[&symlink.target], &link)
            .expect("the scenario symlink is creatable");
        #[cfg(not(unix))]
        {
            if std::os::windows::fs::symlink_dir(&roots[&symlink.target], &link).is_err() {
                eprintln!(
                    "skills: skipping discovery scenario `{}`: this platform cannot create \
                     the symlink it needs",
                    scenario.case
                );
                return None;
            }
        }
    }

    // The production wiring, verbatim: `Release3Service::skill_discovery`
    // resolves the roots through `search_paths` over the configured entries and
    // the project directories a trusted workspace contributes, then filters the
    // catalog with the two keys. The scenario's `home` stands in for the
    // operator's home and `home/.vibe` for the Vibe home, and no builtin is
    // seeded because nothing publishes one yet.
    let projects = if scenario.project_trusted {
        vec![roots["project"].clone()]
    } else {
        Vec::new()
    };
    let configured = scenario
        .skill_paths
        .iter()
        .map(|entry| substitute(entry, &roots))
        .collect::<Vec<_>>();
    let walked = search_paths(&SearchInputs {
        configured: &configured,
        projects: &projects,
        vibe_home: &roots["home"].join(".vibe"),
        user_home: Some(&roots["home"]),
        working_directory: &roots["project"],
    });
    let observed_paths = walked
        .iter()
        .filter_map(|path| label_path(path, &roots))
        .collect::<Vec<_>>();
    let catalog = discover_extensions(
        &DiscoveryRoots {
            skills: SkillDiscovery {
                roots: walked,
                enabled: scenario.enabled_skills.clone(),
                disabled: scenario.disabled_skills.clone(),
            },
            ..DiscoveryRoots::default()
        },
        BTreeMap::new(),
        BTreeMap::new(),
        BTreeMap::new(),
    );

    let mut published: Vec<_> = catalog
        .skills
        .values()
        .map(|skill| {
            let location = skill
                .path
                .as_ref()
                .and_then(|path| label_path(path, &roots));
            // The corpus records the winner of a duplicate name within one
            // root without its path, because the reference's walk order there
            // is filesystem-dependent; the port's answer is masked the same
            // way so the comparison stays about the published set.
            let unrecorded = scenario.published.iter().any(|entry| {
                entry.name == skill.name && entry.source == "local" && entry.root.is_none()
            });
            let (root, relative) = if unrecorded {
                (None, None)
            } else {
                location.map_or((None, None), |(root, relative)| {
                    (Some(root), Some(relative))
                })
            };
            (
                skill.name.clone(),
                vocabulary(skill.source),
                vocabulary(skill.scope),
                root,
                relative,
                skill.user_invocable,
                Some(skill.description.clone()),
            )
        })
        .collect();
    published.sort();
    let mut issues: Vec<_> = catalog
        .issues
        .iter()
        .filter(|issue| issue.mechanism == "skills")
        .filter_map(|issue| label_path(&issue.path, &roots))
        .collect();
    issues.sort();
    Some((observed_paths, published, issues, catalog.skills.len()))
}

// --------------------------------------------------------------------------
// The parser, schema and projection adapters
// --------------------------------------------------------------------------

/// The corpus spelling of a parse rejection class.
const fn error_label(kind: SkillParseErrorKind) -> &'static str {
    match kind {
        SkillParseErrorKind::Boundary => "boundary",
        SkillParseErrorKind::Yaml => "yaml",
        SkillParseErrorKind::Mapping => "mapping",
    }
}

/// The validated fields in the shape the corpus records them.
fn metadata_fields(metadata: &SkillMetadata) -> Value {
    json!({
        "allowed_tools": metadata.allowed_tools,
        "compatibility": metadata.compatibility,
        "description": metadata.description,
        "license": metadata.license,
        "metadata": metadata.metadata,
        "name": metadata.name,
        "user_invocable": metadata.user_invocable,
    })
}

/// A skill that carries nothing but its name, which is all the filter reads.
fn named_definition(name: &str) -> SkillDefinition {
    SkillDefinition {
        name: name.to_owned(),
        description: String::new(),
        license: None,
        compatibility: None,
        metadata: BTreeMap::new(),
        allowed_tools: Vec::new(),
        user_invocable: true,
        body: String::new(),
        source: SkillSource::Local,
        scope: SkillScope::Global,
        path: None,
    }
}

/// A [`SkillDefinition`] materialized from a projection case's model record,
/// which spells only the fields it sets and leans on the model defaults for
/// the rest.
fn projection_definition(skill: &Value) -> SkillDefinition {
    let text = |field: &str| {
        skill
            .get(field)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let optional = |field: &str| {
        skill
            .get(field)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    };
    SkillDefinition {
        name: text("name"),
        description: text("description"),
        license: optional("license"),
        compatibility: optional("compatibility"),
        metadata: skill
            .get("metadata")
            .and_then(Value::as_object)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|(key, value)| {
                        value.as_str().map(|value| (key.clone(), value.to_owned()))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        allowed_tools: skill
            .get("allowed_tools")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        user_invocable: skill
            .get("user_invocable")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        body: text("prompt"),
        source: match skill.get("source").and_then(Value::as_str) {
            Some("builtin") => SkillSource::Builtin,
            Some("registry") => SkillSource::Registry,
            _ => SkillSource::Local,
        },
        scope: SkillScope::Global,
        path: None,
    }
}

// --------------------------------------------------------------------------
// The replay
// --------------------------------------------------------------------------

#[test]
fn the_committed_corpus_replays_every_family_the_reference_answered() {
    let corpus = corpus();
    let mut scenarios = 0;

    let mut report = Report::default();
    for case in &corpus.frontmatter {
        let expected = (
            case.error.clone(),
            case.frontmatter.clone(),
            case.body.clone(),
        );
        let observed = match parse_skill_markdown(&case.content) {
            Ok((frontmatter, body)) => (None, Some(Value::Object(frontmatter)), Some(body)),
            Err(error) => (Some(error_label(error.kind).to_owned()), None, None),
        };
        report.check("frontmatter", &case.case, "parse", &expected, &observed);
    }
    scenarios += settle(&report, "frontmatter");

    let mut report = Report::default();
    for case in &corpus.metadata {
        let mapping = case
            .frontmatter
            .as_object()
            .unwrap_or_else(|| panic!("`{}` holds a frontmatter mapping", case.case));
        let observed = match SkillMetadata::validate(mapping) {
            Ok(metadata) => (true, Some(metadata_fields(&metadata))),
            Err(_) => (false, None),
        };
        report.check(
            "metadata",
            &case.case,
            "verdict",
            &(case.accepted, case.fields.clone()),
            &observed,
        );
    }
    scenarios += settle(&report, "metadata");

    let mut report = Report::default();
    for scenario in &corpus.discovery {
        let Some(observed) = discovery_answer(scenario) else {
            continue;
        };
        report.check(
            "discovery",
            &scenario.case,
            "catalog",
            &expected_catalog(scenario),
            &observed,
        );
    }
    scenarios += settle(&report, "discovery");

    let mut report = Report::default();
    for case in &corpus.filtering {
        // The candidate set is the scenario's own skills plus whatever the port
        // seeds, which is nothing until US-169; the reference seeds two
        // builtins and filters them with the same two keys.
        let mut skills = case
            .skills
            .iter()
            .map(|name| (name.clone(), named_definition(name)))
            .collect::<BTreeMap<_, _>>();
        apply_filters(
            &mut skills,
            &SkillDiscovery {
                roots: Vec::new(),
                enabled: case.enabled_skills.clone(),
                disabled: case.disabled_skills.clone(),
            },
        );
        let mut kept = case.kept.clone();
        kept.sort();
        report.check(
            "filtering",
            &case.case,
            "kept",
            &kept,
            &skills.keys().cloned().collect::<Vec<_>>(),
        );
    }
    scenarios += settle(&report, "filtering");

    let mut report = Report::default();
    for case in &corpus.command.cases {
        let expected = case.result.as_ref().map_or_else(
            || "no invocation".to_owned(),
            |result| format!("invokes `{}`", result.name),
        );
        report.pending("command", &case.case, expected, "US-172");
    }
    scenarios += settle(&report, "command");

    let mut report = Report::default();
    for case in &corpus.projection {
        report.check(
            "projection",
            &case.case,
            "summary",
            &case.summary,
            &skill_summary(&projection_definition(&case.skill)),
        );
    }
    scenarios += settle(&report, "projection");

    let mut report = Report::default();
    for case in &corpus.store.cases {
        let expected = match (&case.error, &case.outcome) {
            (Some(kind), _) => format!("{} raises {kind}", case.op),
            (None, Some(outcome)) => format!("{} ends {outcome}", case.op),
            (None, None) => format!("{} answers", case.op),
        };
        report.pending("store", &case.case, expected, "US-176");
    }
    scenarios += settle(&report, "store");

    let mut report = Report::default();
    for case in &corpus.manifest {
        report.pending("manifest", &case.case, "answers".to_owned(), "US-177");
    }
    scenarios += settle(&report, "manifest");

    println!(
        "skills: {scenarios} scenarios across 8 families replayed at {}",
        &corpus.reference.commit[..12],
    );
    assert!(
        scenarios >= MINIMUM_SCENARIOS,
        "the corpus replays {scenarios} scenarios, below the {MINIMUM_SCENARIOS} the PRD \
         commits to; regenerate it with {CAPTURE_SCRIPT}"
    );
}

/// The corpus is only an oracle for as long as it still describes the pinned
/// reference. This probe recaptures it where the checkout is present and on
/// the pin, and skips everywhere else naming the pin and the way back.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "skills") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let repository = repo_root();
    let script = repository.join(CAPTURE_SCRIPT);
    let recaptured = repository.join("target/skills-corpus.json");
    let output = Command::new("python3")
        .arg(&script)
        .args(["--reference".as_ref(), root.as_os_str()])
        .arg("--output")
        .arg(&recaptured)
        .current_dir(&repository)
        .output()
        .expect("the skills capture script runs");
    assert!(
        output.status.success(),
        "the skills capture failed: {}",
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
