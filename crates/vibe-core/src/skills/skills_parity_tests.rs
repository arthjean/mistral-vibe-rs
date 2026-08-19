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
//! The oracle preceded the implementation, and the ledger below has burned
//! down to its terminal state: every entry is a decided divergence, and the
//! stale check retires one the moment its case conforms. `frontmatter`,
//! `metadata` and `projection` are compared for real since EP-047 landed the
//! parser, the schema and the whole model. `discovery` and `filtering` are
//! compared for real since EP-048 landed the five roots, the configured paths
//! and the two filter keys: the wiring reproduced here is
//! `WorkspaceService::skill_discovery`, which resolves the roots through
//! `search_paths` and hands them plus the filters to `discover_extensions`.
//! EP-049 seeded the builtin catalog, so both families now conform whole and
//! the `builtins` block is compared for real: structure and vocabulary must
//! match, and the two prose digests must never match, which is the `NOTICE`
//! boundary enforced mechanically. EP-051 ported the registry store and the
//! manifests, so `store` and `manifest` are compared for real too, over a
//! scratch store root per case; the store's fallback description is this
//! port's own prose, masked in the trees the way the capture masks the
//! reference's and held permanently unequal by digest.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::extensions::{DiscoveryRoots, SkillDefinition, discover_extensions};
use crate::parity::{REFERENCE_COMMIT, RESTORE_COMMAND, off_pin_reason, reference_root};
use crate::skills::builtins::builtin_skills;
use crate::skills::parser::{SkillParseErrorKind, parse_skill_markdown};
use crate::skills::registry::manifest::{ManifestEntry, ManifestVersion, SkillManifest};
use crate::skills::registry::models::RegistrySkillItem;
use crate::skills::schema::SkillMetadata;
use crate::skills::{
    SearchInputs, SkillDiscovery, SkillScope, SkillSource, apply_filters, search_paths,
    skill_summary,
};
use crate::text::hex_encode;

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
/// A `family/*` entry covers every case of its family and goes stale only
/// when the whole family conforms; none remains, because every entry left is
/// a decided divergence: the deprecated legacy root and the prose digests
/// `NOTICE` holds permanently unequal.
const DIVERGENCES: &[(&str, &str)] = &[
    (
        "discovery/legacy-extensions-root-unread",
        "ACCEPTED: US-165 keeps `{vibe_home}/extensions/skills` readable as a deprecated root \
         this port published before the documented ones existed, so a skill sitting there is \
         published here and unread upstream",
    ),
    (
        "builtins/builtinProse-vibe",
        "ACCEPTED: `NOTICE` forbids shipping the reference's builtin prose, so the `vibe` body \
         is written originally in `crates/vibe-core/src/skills/assets/vibe.md` against the same \
         directive coverage; this entry keeps the divergence permanent, and the replay fails \
         the moment the body conforms to the reference digest, so it can never be closed by \
         copying",
    ),
    (
        "builtins/builtinProse-vibe-description",
        "ACCEPTED: the `vibe` description is reference-authored prose, rewritten originally in \
         `vibe_core::skills::builtins` for the same routing intent",
    ),
    (
        "builtins/builtinProse-skill-creator",
        "ACCEPTED: `NOTICE` forbids shipping the reference's builtin prose, so the \
         `skill-creator` body is written originally in \
         `crates/vibe-core/src/skills/assets/skill_creator.md` against the same directive \
         coverage; conforming to the reference digest fails the replay",
    ),
    (
        "builtins/builtinProse-skill-creator-description",
        "ACCEPTED: the `skill-creator` description is reference-authored prose, rewritten \
         originally in `vibe_core::skills::builtins` for the same routing intent",
    ),
    (
        "command/builtin-skill-creator",
        "ACCEPTED: the case invokes the `skill-creator` builtin, whose body is this port's own \
         prose (`NOTICE`), so the delivered content digest can never match the reference's",
    ),
    (
        "store/fallbackDescription",
        "ACCEPTED: the store's fallback description is reference-authored prose, so \
         `vibe_core::skills::registry::store::fallback_description` is this port's own sentence \
         for the same purpose; the recorded trees mask it as `{fallbackDescription}` on both \
         sides, and this entry holds the two digests permanently unequal, failing the replay \
         the moment the sentence conforms",
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
    /// The builtin catalog by digest only: the structure is compared for
    /// equality and the prose digests for permanent inequality.
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
struct Builtins {
    count: usize,
    skills: Vec<BuiltinSkill>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
    skills: Vec<CommandFixture>,
    cases: Vec<CommandCase>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
    prompt: String,
    result: Option<CommandResult>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommandResult {
    name: String,
    extra_instructions: Option<String>,
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
    fallback_description: Digested,
    cases: Vec<StoreCase>,
}

/// One store scenario. The record is heterogeneous by `op`, so every
/// field beyond the two discriminants is optional.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoreCase {
    case: String,
    op: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    version: Option<i64>,
    #[serde(default)]
    item: Option<Value>,
    #[serde(default)]
    name: Option<String>,
    /// Materializations replayed before the operation, as `{item, name}`
    /// records.
    #[serde(default)]
    prior: Option<Vec<Value>>,
    #[serde(default)]
    fail_write: Option<bool>,
    #[serde(default)]
    active: Option<Vec<(String, i64)>>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    rel_path: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    tree: Option<Value>,
    #[serde(default)]
    leftover_entries: Option<Vec<String>>,
    #[serde(default)]
    latest: Option<i64>,
    #[serde(default)]
    surviving: Option<Vec<String>>,
}

/// One manifest scenario, heterogeneous the same way and dispatched on its
/// case name, because each one drives a different manifest operation.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestCase {
    case: String,
    #[serde(default)]
    version: Option<Value>,
    #[serde(default)]
    alias: Option<String>,
    #[serde(default)]
    default_version_applied: Option<Value>,
    #[serde(default)]
    skills: Option<Value>,
    #[serde(default)]
    removed_existing: Option<bool>,
    #[serde(default)]
    removed_missing: Option<bool>,
    #[serde(default)]
    toml: Option<String>,
    #[serde(default)]
    lossless: Option<bool>,
    #[serde(default)]
    created: Option<bool>,
    #[serde(default)]
    rel_path: Option<String>,
    #[serde(default)]
    #[expect(dead_code, reason = "the input labels are fixed by the scenario")]
    roots: Option<Vec<String>>,
    #[serde(default)]
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

    // The production wiring, verbatim: `WorkspaceService::skill_discovery`
    // resolves the roots through `search_paths` over the configured entries and
    // the project directories a trusted workspace contributes, then
    // `discover_extensions` seeds the builtin catalog ahead of the walk and
    // filters the result with the two keys. The scenario's `home` stands in for
    // the operator's home and `home/.vibe` for the Vibe home.
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
        builtin_skills(),
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
            // The corpus masks builtin descriptions to null because they are
            // reference-authored prose (`NOTICE`); the port's own text is
            // masked the same way so the comparison stays structural.
            let description = if skill.source == SkillSource::Builtin {
                None
            } else {
                Some(skill.description.clone())
            };
            (
                skill.name.clone(),
                vocabulary(skill.source),
                vocabulary(skill.scope),
                root,
                relative,
                skill.user_invocable,
                description,
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
    Some((
        observed_paths,
        published,
        issues,
        custom_skills_count(&catalog.skills),
    ))
}

// --------------------------------------------------------------------------
// The parser, schema and projection adapters
// --------------------------------------------------------------------------

/// How many of a published catalog the operator added: everything the port did
/// not seed, which is what the reference's `custom_skills_count` answers and
/// what the banner reports through the wire's `source` field.
fn custom_skills_count(skills: &BTreeMap<String, SkillDefinition>) -> usize {
    skills
        .values()
        .filter(|skill| skill.source != SkillSource::Builtin)
        .count()
}

/// The corpus form of a prose value: character length plus the SHA-256 of the
/// UTF-8 bytes, matching the capture script's `digested`.
fn digest_of(value: &str) -> Digested {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    Digested {
        length: value.chars().count(),
        digest: hex_encode(&hasher.finalize()),
    }
}

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
// The store and manifest adapters
// --------------------------------------------------------------------------

/// A [`RegistrySkillItem`] materialized from a store case's compact item
/// record, exactly as the capture script's `_item` builds one for the
/// reference: the payload defaults stand in for every field the record does
/// not spell.
fn store_item(spec: &Value) -> RegistrySkillItem {
    let text = |key: &str, default: &str| {
        spec.get(key)
            .and_then(Value::as_str)
            .unwrap_or(default)
            .to_owned()
    };
    let mut assets = serde_json::Map::new();
    for asset in spec
        .get("assets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let path = asset
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut content = serde_json::Map::new();
        if let Some(body) = asset.get("text").and_then(Value::as_str) {
            content.insert("textContent".to_owned(), json!(body));
        } else if let Some(raw) = asset.get("base64").and_then(Value::as_str) {
            content.insert("rawContent".to_owned(), json!(raw));
        }
        content.insert(
            "isExecutable".to_owned(),
            json!(
                asset
                    .get("executable")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            ),
        );
        assets.insert(path.to_owned(), Value::Object(content));
    }
    let mut payload = json!({
        "skillId": text("skillId", "id-1"),
        "version": spec.get("version").and_then(Value::as_i64).unwrap_or(1),
        "skill": {
            "skillName": text("skillName", ""),
            "skillDescription": text("description", ""),
            "skillBody": text("body", "Registry body."),
            "skillAssets": assets,
        },
    });
    if let Some(name) = spec.get("metadataName").and_then(Value::as_str) {
        payload["metadata"] = json!({"name": name});
    }
    serde_json::from_value(payload).expect("the store item record parses")
}

/// The recorded form of one directory tree: every file with its relative
/// path, its content and its execute bits, sorted the way the capture sorts
/// them. `mask` replaces this port's fallback description the way the capture
/// replaces the reference's, so the comparison stays about structure.
fn tree_of(root: &Path, mask: Option<(&str, &str)>) -> Value {
    let mut files = Vec::new();
    collect_files(root, root, &mut files);
    files.sort_by(|a, b| a.0.split('/').cmp(b.0.split('/')));
    Value::Array(
        files
            .into_iter()
            .map(|(path, content, exec)| {
                let content = match mask {
                    Some((needle, marker)) => content.replace(needle, marker),
                    None => content,
                };
                json!({
                    "path": path,
                    "content": content,
                    "exec": {"user": exec.0, "group": exec.1, "other": exec.2},
                })
            })
            .collect(),
    )
}

fn collect_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(String, String, (bool, bool, bool))>,
) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, files);
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .expect("collected files sit under their root")
            .to_string_lossy()
            .replace('\\', "/");
        let content = fs::read_to_string(&path).expect("the collected file is readable text");
        files.push((relative, content, exec_bits(&path)));
    }
}

/// The three execute bits the capture records. Windows has no execute bit, so
/// the recorded expectation is reproduced there rather than measured, keeping
/// the comparison about the fields the platform can answer.
#[cfg(unix)]
fn exec_bits(path: &Path) -> (bool, bool, bool) {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = fs::metadata(path)
        .expect("the collected file has metadata")
        .permissions()
        .mode();
    (mode & 0o100 != 0, mode & 0o010 != 0, mode & 0o001 != 0)
}

#[cfg(not(unix))]
fn exec_bits(_path: &Path) -> (bool, bool, bool) {
    (false, false, false)
}

/// Strips the `exec` fields from a recorded answer on platforms that cannot
/// measure them, so the comparison stays about the fields the platform can
/// answer.
fn comparable_tree_fields(answer: &Value) -> Value {
    if cfg!(unix) {
        return answer.clone();
    }
    let mut answer = answer.clone();
    strip_exec(&mut answer);
    answer
}

fn strip_exec(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.remove("exec");
            for nested in fields.values_mut() {
                strip_exec(nested);
            }
        }
        Value::Array(items) => {
            for nested in items {
                strip_exec(nested);
            }
        }
        _ => {}
    }
}

/// The port's answer for one store case, in the shape the corpus records it.
fn store_answer(case: &StoreCase, fallback: &mut Option<String>) -> Value {
    use crate::skills::registry::store;

    let scratch = tempfile::tempdir().expect("a scratch store is available");
    let root = scratch
        .path()
        .canonicalize()
        .expect("the scratch store resolves")
        .join("store");
    fs::create_dir_all(&root).expect("the store root is writable");
    for prior in case.prior.iter().flatten() {
        let item = store_item(prior.get("item").expect("a prior record carries an item"));
        let name = prior
            .get("name")
            .and_then(Value::as_str)
            .expect("a prior record carries a name");
        store::materialize(&root, &item, name).expect("the prior materialization succeeds");
    }

    match case.op.as_str() {
        "skillDir" => {
            let id = case.id.as_deref().unwrap_or_default();
            let version = case.version.unwrap_or_default();
            match store::skill_dir(&root, id, version) {
                Err(store::StoreError::UnsafeId(_)) => json!({"error": "unsafeId"}),
                Err(error) => json!({"error": error.to_string()}),
                Ok(path) => {
                    let resolved = root.canonicalize().unwrap_or_else(|_| root.clone());
                    json!({
                        "relPath": path
                            .strip_prefix(&resolved)
                            .expect("a safe id resolves under the store root")
                            .to_string_lossy()
                            .replace('\\', "/"),
                    })
                }
            }
        }
        "materialize" => {
            let item = store_item(
                case.item
                    .as_ref()
                    .expect("a materialize case carries an item"),
            );
            let name = case.name.as_deref().unwrap_or_default();
            let result = if case.fail_write == Some(true) {
                store::materialize_with(&root, &item, name, &|_, _| {
                    Err(std::io::Error::other("scripted write failure"))
                })
            } else {
                store::materialize(&root, &item, name)
            };
            let outcome = match &result {
                Err(_) => "raised",
                Ok(Some(_)) => "stored",
                Ok(None) => "skipped",
            };
            let version_dir = store::skill_dir(&root, &item.skill_id, item.version)
                .expect("the recorded ids are safe");
            let mask = (case.case == "materialize-fallback-description").then(|| {
                let sentence = store::fallback_description(name);
                *fallback = Some(sentence.clone());
                sentence
            });
            let tree = if version_dir.is_dir() {
                tree_of(
                    &version_dir,
                    mask.as_deref()
                        .map(|needle| (needle, "{fallbackDescription}")),
                )
            } else {
                Value::Null
            };
            let parent = version_dir
                .parent()
                .expect("a version directory has a parent");
            let mut leftovers: Vec<String> = if parent.is_dir() {
                fs::read_dir(parent)
                    .expect("the id directory is readable")
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|entry| {
                        Some(entry.as_str())
                            != version_dir.file_name().and_then(|name| name.to_str())
                    })
                    .collect()
            } else {
                Vec::new()
            };
            leftovers.sort();
            json!({"outcome": outcome, "tree": tree, "leftoverEntries": leftovers})
        }
        "latestMaterialized" => {
            let id = case.id.as_deref().unwrap_or_default();
            json!({
                "latest": store::latest_materialized(&root, id).expect("the recorded ids are safe"),
            })
        }
        "exportLocal" => {
            let target = scratch.path().join("exported");
            store::export_local(
                &root,
                case.id.as_deref().unwrap_or_default(),
                case.version.unwrap_or_default(),
                &target,
            )
            .expect("the recorded export succeeds");
            json!({"tree": tree_of(&target, None)})
        }
        "prune" => {
            let active: BTreeSet<(String, i64)> = case
                .active
                .clone()
                .unwrap_or_default()
                .into_iter()
                .collect();
            store::prune(&root, &active).expect("the recorded prune succeeds");
            let mut surviving = Vec::new();
            collect_files(&root, &root, &mut surviving);
            let mut surviving: Vec<String> =
                surviving.into_iter().map(|(path, _, _)| path).collect();
            surviving.sort_by(|a, b| a.split('/').cmp(b.split('/')));
            json!({"surviving": surviving})
        }
        other => json!({"error": format!("the port has no comparator for op {other:?}")}),
    }
}

/// What the corpus recorded for one store case, in the same shape.
fn store_expected(case: &StoreCase) -> Value {
    match case.op.as_str() {
        "skillDir" => match &case.error {
            Some(error) => json!({"error": error}),
            None => json!({"relPath": case.rel_path}),
        },
        "materialize" => json!({
            "outcome": case.outcome,
            "tree": case.tree.clone().unwrap_or(Value::Null),
            "leftoverEntries": case.leftover_entries.clone().unwrap_or_default(),
        }),
        "latestMaterialized" => json!({"latest": case.latest}),
        "exportLocal" => json!({"tree": case.tree.clone().unwrap_or(Value::Null)}),
        "prune" => json!({"surviving": case.surviving.clone().unwrap_or_default()}),
        other => json!({"error": format!("the corpus has no comparator for op {other:?}")}),
    }
}

/// The recorded form of a manifest's entries, matching the capture's
/// `model_dump` of each one.
fn manifest_entries(manifest: &SkillManifest) -> Value {
    Value::Array(
        manifest
            .skills
            .iter()
            .map(|entry| {
                json!({
                    "name": entry.name,
                    "skill_id": entry.skill_id,
                    "version": match &entry.version {
                        ManifestVersion::Frozen(version) => json!(version),
                        ManifestVersion::Alias(alias) => json!(alias),
                    },
                    "description": entry.description,
                })
            })
            .collect(),
    )
}

fn manifest_entry(name: &str, skill_id: &str, version: ManifestVersion) -> ManifestEntry {
    ManifestEntry {
        name: name.to_owned(),
        skill_id: skill_id.to_owned(),
        version,
        description: String::new(),
    }
}

/// The `(expected, observed)` answers for one manifest case. The cases are
/// bespoke operations, so each one is replayed by name; a case this port does
/// not know fails visibly rather than being skipped.
fn manifest_answer(case: &ManifestCase) -> (Value, Value) {
    use crate::skills::registry::manifest;

    let scratch = tempfile::tempdir().expect("a scratch manifest directory is available");
    let workdir = scratch
        .path()
        .canonicalize()
        .expect("the scratch manifest directory resolves");
    match case.case.as_str() {
        "alias-for-string-version" => {
            let entry = manifest_entry("a", "x", ManifestVersion::Alias("latest".to_owned()));
            let applied = match ManifestVersion::default() {
                ManifestVersion::Frozen(version) => json!(version),
                ManifestVersion::Alias(alias) => json!(alias),
            };
            (
                json!({
                    "version": case.version,
                    "alias": case.alias,
                    "defaultVersionApplied": case.default_version_applied,
                }),
                json!({
                    "version": "latest",
                    "alias": entry.alias(),
                    "defaultVersionApplied": applied,
                }),
            )
        }
        "alias-for-integer-version" => {
            let entry = manifest_entry("a", "x", ManifestVersion::Frozen(3));
            (
                json!({"version": case.version, "alias": case.alias}),
                json!({"version": 3, "alias": entry.alias()}),
            )
        }
        "upsert-replaces-by-name" => {
            let mut manifest = SkillManifest::default();
            manifest.upsert(manifest_entry("a", "x", ManifestVersion::Frozen(1)));
            manifest.upsert(manifest_entry("a", "x", ManifestVersion::Frozen(2)));
            manifest.upsert(manifest_entry(
                "b",
                "y",
                ManifestVersion::Alias("latest".to_owned()),
            ));
            (
                json!({"skills": case.skills}),
                json!({"skills": manifest_entries(&manifest)}),
            )
        }
        "remove-by-name" => {
            let mut manifest = SkillManifest::default();
            manifest.upsert(manifest_entry("a", "x", ManifestVersion::Frozen(2)));
            manifest.upsert(manifest_entry(
                "b",
                "y",
                ManifestVersion::Alias("latest".to_owned()),
            ));
            let removed_existing = manifest.remove("a");
            let removed_missing = manifest.remove("missing");
            (
                json!({
                    "removedExisting": case.removed_existing,
                    "removedMissing": case.removed_missing,
                    "skills": case.skills,
                }),
                json!({
                    "removedExisting": removed_existing,
                    "removedMissing": removed_missing,
                    "skills": manifest_entries(&manifest),
                }),
            )
        }
        "save-toml-shape" | "save-load-roundtrip" => {
            let mut pinned = manifest_entry("grill-me", "uuid-1", ManifestVersion::Frozen(3));
            pinned.description = "pinned".to_owned();
            let fresh = manifest_entry(
                "fresh",
                "uuid-2",
                ManifestVersion::Alias("latest".to_owned()),
            );
            let to_save = SkillManifest {
                skills: vec![pinned, fresh],
            };
            let saved = workdir.join("skills.toml");
            manifest::save(&saved, &to_save).expect("the manifest saves");
            if case.case == "save-toml-shape" {
                (
                    json!({"skills": case.skills, "toml": case.toml}),
                    json!({
                        "skills": manifest_entries(&to_save),
                        "toml": fs::read_to_string(&saved).expect("the saved manifest reads"),
                    }),
                )
            } else {
                let loaded = manifest::load(&saved);
                (
                    json!({"lossless": case.lossless, "skills": case.skills}),
                    json!({
                        "lossless": loaded.manifest == to_save,
                        "skills": manifest_entries(&loaded.manifest),
                    }),
                )
            }
        }
        "save-creates-parents" => {
            let nested = workdir.join("nested").join("deeper").join("skills.toml");
            manifest::save(&nested, &SkillManifest::default()).expect("the manifest saves");
            (
                json!({"created": case.created}),
                json!({"created": nested.is_file()}),
            )
        }
        "load-missing-returns-empty" => (
            json!({"skills": case.skills}),
            json!({"skills": manifest_entries(&manifest::load(&workdir.join("absent.toml")).manifest)}),
        ),
        "load-malformed-returns-empty" => {
            let malformed = workdir.join("bad.toml");
            fs::write(&malformed, "this is = = not valid toml [[[").expect("the fixture writes");
            (
                json!({"skills": case.skills}),
                json!({"skills": manifest_entries(&manifest::load(&malformed).manifest)}),
            )
        }
        "load-invalid-entry-returns-empty" | "load-ignores-unknown-keys" => {
            let document = workdir.join("document.toml");
            fs::write(
                &document,
                case.toml.as_deref().expect("the case records its document"),
            )
            .expect("the fixture writes");
            let loaded = manifest::load(&document);
            if case.case == "load-invalid-entry-returns-empty" {
                assert!(
                    loaded.warning.is_some(),
                    "a well-formed document of the wrong shape carries a warning"
                );
            }
            (
                json!({"skills": case.skills}),
                json!({"skills": manifest_entries(&loaded.manifest)}),
            )
        }
        "global-manifest-under-vibe-home" => {
            let home = workdir.join("home");
            fs::create_dir_all(&home).expect("the scenario home is writable");
            let global = manifest::global_manifest_path(&home.join(".vibe"));
            (
                json!({"relPath": case.rel_path}),
                json!({
                    "relPath": global
                        .strip_prefix(&home)
                        .expect("the global manifest sits under the home")
                        .to_string_lossy()
                        .replace('\\', "/"),
                }),
            )
        }
        "project-paths-dedup-and-drop-global" => {
            let home = workdir.join("home");
            fs::create_dir_all(&home).expect("the scenario home is writable");
            let home = home.canonicalize().expect("the scenario home resolves");
            let project_a = workdir.join("proj-a");
            let project_b = workdir.join("proj-b");
            let roots = vec![
                home.clone(),
                project_a.clone(),
                project_b.clone(),
                project_a.clone(),
            ];
            let labeled = [
                (home.clone(), "home"),
                (project_a, "projectA"),
                (project_b, "projectB"),
            ];
            let observed: Vec<Value> =
                manifest::project_manifest_paths(&home.join(".vibe"), &roots)
                    .into_iter()
                    .map(|path| {
                        let (relative, label) = labeled
                            .iter()
                            .find_map(|(root, label)| {
                                let resolved = root.canonicalize().unwrap_or_else(|_| root.clone());
                                path.strip_prefix(&resolved)
                                    .ok()
                                    .map(|relative| (relative.to_path_buf(), label))
                            })
                            .expect("a collected path sits under a scenario root");
                        json!({
                            "root": label,
                            "relPath": relative.to_string_lossy().replace('\\', "/"),
                        })
                    })
                    .collect();
            (json!({"paths": case.paths}), json!({"paths": observed}))
        }
        other => (
            json!({"case": other}),
            json!({"error": "the port has no comparator for this case"}),
        ),
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
        // The candidate set is the seeded builtins plus the scenario's own
        // skills, which is what `discover_extensions` filters: the reference
        // applies the same two keys after seeding, so a pattern can select or
        // withhold a builtin.
        let mut skills = builtin_skills();
        skills.extend(
            case.skills
                .iter()
                .map(|name| (name.clone(), named_definition(name))),
        );
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
        report.check(
            "filtering",
            &case.case,
            "customSkillsCount",
            &case.custom_skills_count,
            &custom_skills_count(&skills),
        );
    }
    scenarios += settle(&report, "filtering");

    let mut report = Report::default();
    let mut command_catalog = crate::skills::builtins::builtin_skills();
    command_catalog.extend(corpus.command.skills.iter().map(|fixture| {
        let mut definition = named_definition(&fixture.name);
        definition.description.clone_from(&fixture.description);
        definition.user_invocable = fixture.invocable;
        definition.body.clone_from(&fixture.body);
        (fixture.name.clone(), definition)
    }));
    for case in &corpus.command.cases {
        let parsed = crate::skills::parse_skill_command(&command_catalog, &case.prompt);
        report.check(
            "command",
            &case.case,
            "invokes",
            &case.result.is_some(),
            &parsed.is_some(),
        );
        if let (Some(expected), Some(actual)) = (&case.result, &parsed) {
            report.check("command", &case.case, "name", &expected.name, &actual.name);
            report.check(
                "command",
                &case.case,
                "extraInstructions",
                &expected.extra_instructions,
                &actual.extra_instructions,
            );
            report.check(
                "command",
                &case.case,
                "content",
                &expected.content,
                &digest_of(&actual.content),
            );
        }
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
    let mut fallback = None;
    for case in &corpus.store.cases {
        let observed = store_answer(case, &mut fallback);
        report.check(
            "store",
            &case.case,
            "answer",
            &comparable_tree_fields(&store_expected(case)),
            &comparable_tree_fields(&observed),
        );
    }
    // The fallback description is this port's own prose: the digests must
    // never match, which the `store/fallbackDescription` ledger entry holds in
    // place the way the builtin prose entries do.
    let fallback = fallback.expect("the fallback-description case ran");
    report.check(
        "store",
        "fallbackDescription",
        "digest",
        &corpus.store.fallback_description,
        &digest_of(&fallback),
    );
    scenarios += settle(&report, "store");

    let mut report = Report::default();
    for case in &corpus.manifest {
        let (expected, observed) = manifest_answer(case);
        report.check("manifest", &case.case, "answer", &expected, &observed);
    }
    scenarios += settle(&report, "manifest");

    // The builtin catalog: everything structural must equal the reference,
    // and the two prose digests must never equal it, which is `NOTICE`
    // enforced by the stale-entry check the ledger already runs.
    let mut report = Report::default();
    let seeded = builtin_skills();
    report.check(
        "builtins",
        "count",
        "count",
        &corpus.builtins.count,
        &seeded.len(),
    );
    for expected in &corpus.builtins.skills {
        let Some(skill) = seeded.get(&expected.name) else {
            report.check("builtins", &expected.name, "present", &true, &false);
            continue;
        };
        report.check(
            "builtins",
            &expected.name,
            "userInvocable",
            &expected.user_invocable,
            &skill.user_invocable,
        );
        report.check(
            "builtins",
            &expected.name,
            "hasPath",
            &expected.has_path,
            &skill.path.is_some(),
        );
        report.check(
            "builtins",
            &expected.name,
            "source",
            &expected.source,
            &vocabulary(skill.source),
        );
        report.check(
            "builtins",
            &expected.name,
            "scope",
            &expected.scope,
            &vocabulary(skill.scope),
        );
        report.check(
            "builtins",
            &format!("builtinProse-{}", expected.name),
            "prompt digest",
            &expected.prompt,
            &digest_of(&skill.body),
        );
        report.check(
            "builtins",
            &format!("builtinProse-{}-description", expected.name),
            "description digest",
            &expected.description,
            &digest_of(&skill.description),
        );
    }
    scenarios += settle(&report, "builtins");

    println!(
        "skills: {scenarios} scenarios across 8 families plus the builtin catalog replayed at {}",
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
