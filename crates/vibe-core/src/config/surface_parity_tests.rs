//! Differential oracle for configuration layer composition.
//!
//! The Python reference is the authority on how two layer documents combine.
//! `scripts/parity/config_surface.py` drives its `ConfigBuilder` merge over a
//! set of synthetic layer stacks and records, per scenario, the merged document
//! it produces plus the census of every field it declares. This module replays
//! that corpus against [`LayeredConfig::load`].
//!
//! The corpus is committed, unlike the tool-surface one: it carries field names,
//! merge strategies, merge keys, editor kinds and values authored for the
//! capture, and no reference-authored description text, which is what `NOTICE`
//! forbids shipping. Replay therefore runs unconditionally; only the live probe
//! that recaptures from the pinned checkout skips when it is absent.
//!
//! Three families are replayed. `scenarios` records what `ConfigBuilder`
//! *merges*, and is compared against a stack composed without the shipped
//! defaults. `defaults` records the document `create_default_config` ships, and
//! is compared against a load with no configuration file at all.
//! `modelScenarios` records what the reference *validates* on top of its own
//! default layer, because the model rules only run after the merge.
//!
//! One comparison is deliberately scoped: a key the reference schema does not
//! declare is dropped by the reference merge and kept by this one, which FR-04
//! requires. The corpus records those keys so the divergence is proved rather
//! than assumed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;
use serde_json::{Map, Value as JsonValue};

use super::registry::{FIELDS, MergeStrategy};
use super::*;

use crate::parity::{REFERENCE_COMMIT, off_pin_reason, pinned_interpreter, reference_root};

const CAPTURE_SCRIPT: &str = "scripts/parity/config_surface.py";
const CORPUS_RELATIVE: &str = "tests/config-surface/corpus.json";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 4;
/// The scenario floor this epic commits to.
const MINIMUM_SCENARIOS: usize = 24;
/// What the capture writes where the vibe home is machine-dependent.
const VIBE_HOME_PLACEHOLDER: &str = "{vibe_home}";

/// Reference fields this port does not declare, each with the reason.
///
/// US-004 and US-005 shortened this list to one: the three fields the
/// GrowthBook layer writes are declared here now, each with the resolution that
/// gives it an effect, so the only entry left is the greeting toggle, whose
/// consumer belongs to app-server parity and which no experiment targets. The
/// gap stays visible and the replay still fails on any *other* undeclared
/// field.
///
/// An entry whose field becomes declared fails the replay as stale.
const UNDECLARED_FIELDS: &[(&str, &str)] = &[(
    "show_greeting",
    "US-142: v2.24.0 greeting toggle, unported; narrowed to the greeting by US-005",
)];

/// The sentinel v2.24.0 ships for `active_model`, meaning "not pinned": both
/// implementations now carry it in the document they ship and resolve it when
/// the alias is read, so this constant describes the shipped document rather
/// than a divergence from it.
const UNPINNED_ACTIVE_MODEL: &str = "";

/// What the sentinel resolves to when no routed default is configured, which is
/// the alias this port used to pin outright. Unpinning `active_model` must not
/// move the model an installation actually runs on, and this is the constant
/// that holds it still.
const PINNED_ACTIVE_MODEL: &str = "mistral-medium-3.5";

/// A ledger as a lookup, so a divergence is named once and read by name.
fn ledger(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(name, reason)| ((*name).to_owned(), (*reason).to_owned()))
        .collect()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Corpus {
    schema_version: u32,
    reference: Reference,
    #[expect(dead_code, reason = "the note documents the file for its readers")]
    note: String,
    strategies: Strategies,
    fields: Vec<ReferenceField>,
    defaults: Defaults,
    scenarios: Vec<Scenario>,
    model_scenarios: Vec<ModelScenario>,
    /// Replayed by `mcp_parity_tests`, which reads the same file through its
    /// own view; named here because the corpus denies unknown fields.
    #[expect(dead_code, reason = "the MCP section is replayed by its own module")]
    mcp: JsonValue,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Defaults {
    document: Map<String, JsonValue>,
    tool_names: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelScenario {
    name: String,
    layers: Vec<ScenarioLayer>,
    active_model: String,
    models: Map<String, JsonValue>,
    /// The validated `compaction_model`, or `null` where the document declares
    /// none. It is a `ModelConfig` like every entry of `models`, so it carries
    /// the same alias rule and the same per-entry defaults.
    compaction_model: Option<JsonValue>,
    validation_warnings: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Strategies {
    declared: Vec<String>,
    used: Vec<String>,
    unused: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReferenceField {
    name: String,
    strategy: String,
    merge_key: Option<String>,
    kind: String,
    choices: Vec<String>,
    popular: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Scenario {
    name: String,
    layers: Vec<ScenarioLayer>,
    merged: Map<String, JsonValue>,
    dropped_keys: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioLayer {
    name: String,
    toml: String,
}

fn corpus_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(CORPUS_RELATIVE)
}

fn corpus() -> Corpus {
    let path = corpus_path();
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("configuration corpus at {}: {error}", path.display()));
    let corpus: Corpus = serde_json::from_str(&raw)
        .unwrap_or_else(|error| panic!("configuration corpus at {}: {error}", path.display()));
    assert_eq!(
        corpus.schema_version, CORPUS_SCHEMA_VERSION,
        "the corpus was captured by another version of {CAPTURE_SCRIPT}"
    );
    assert_eq!(
        corpus.reference.commit, REFERENCE_COMMIT,
        "the corpus was captured from an unpinned reference"
    );
    corpus
}

#[test]
fn every_reference_field_is_declared_with_the_strategy_the_reference_uses() {
    let corpus = corpus();
    let declared = FIELDS
        .iter()
        .map(|spec| (spec.name, spec))
        .collect::<BTreeMap<_, _>>();
    let recorded = ledger(UNDECLARED_FIELDS);
    let mut missing = Vec::new();
    for field in &corpus.fields {
        let Some(spec) = declared.get(field.name.as_str()) else {
            if !recorded.contains_key(&field.name) {
                missing.push(field.name.clone());
            }
            continue;
        };
        assert!(
            !recorded.contains_key(&field.name),
            "field `{}` is declared now and its recorded gap is stale",
            field.name
        );
        assert_eq!(
            spec.strategy.as_str(),
            field.strategy,
            "field `{}` merges by the wrong strategy",
            field.name
        );
        assert_eq!(
            spec.merge_key.map(str::to_owned),
            field.merge_key,
            "field `{}` declares the wrong merge key",
            field.name
        );
        assert_eq!(
            spec.popular, field.popular,
            "field `{}` disagrees with the reference popular set",
            field.name
        );
        // `theme` is the one deliberate kind divergence: the reference types it
        // as free text, this port as a choice over the theme catalog it ships,
        // so the settings screen can offer a picker rather than a text field.
        if field.name == "theme" {
            assert_eq!(field.kind, "str");
            assert_eq!(spec.kind.as_str(), "enum");
            assert_eq!(spec.choices, THEME_VALUES);
            continue;
        }
        assert_eq!(
            spec.kind.as_str(),
            field.kind,
            "field `{}` publishes another editor kind",
            field.name
        );
        assert_eq!(
            spec.choices, field.choices,
            "field `{}` publishes another choice set",
            field.name
        );
    }
    assert!(
        missing.is_empty(),
        "the registry does not declare these reference fields: {missing:?}"
    );
    let withdrawn = recorded
        .keys()
        .filter(|name| !corpus.fields.iter().any(|field| &field.name == *name))
        .collect::<Vec<_>>();
    assert!(
        withdrawn.is_empty(),
        "these recorded gaps name fields the reference no longer declares: {withdrawn:?}"
    );
    let local = FIELDS
        .iter()
        .filter(|spec| spec.local)
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    // A field added to the registry without a corpus entry is the regression
    // this gate exists for: it is either a reference field the corpus predates,
    // which means recapturing, or a local one, which means declaring it as such
    // and recording the divergence. Either way the field is named.
    let known = corpus
        .fields
        .iter()
        .map(|field| field.name.as_str())
        .collect::<BTreeSet<_>>();
    let ungated = FIELDS
        .iter()
        .filter(|spec| !spec.local)
        .map(|spec| spec.name)
        .filter(|name| !known.contains(name))
        .collect::<Vec<_>>();
    assert!(
        ungated.is_empty(),
        "these registry fields have no corpus entry: {ungated:?}. Recapture with {CAPTURE_SCRIPT}, \
         or declare each as local and record the divergence in the PRD"
    );
    assert_eq!(
        local,
        [
            "thinking",
            "notifications",
            "proxy",
            "tls_ca_path",
            "dotenv_path"
        ],
        "the locally declared key set changed; record the divergence in the PRD"
    );
    // The numerator is the reference field set minus what the ledger still
    // records as undeclared, so the count `docs/parity.md` quotes moves when the
    // ledger does instead of restating the denominator twice.
    let undeclared = corpus
        .fields
        .iter()
        .filter(|field| recorded.contains_key(&field.name))
        .count();
    println!(
        "config surface: {}/{} reference fields declared, {} local, {} recorded undeclared, at {}",
        corpus.fields.len() - undeclared,
        corpus.fields.len(),
        local.len(),
        undeclared,
        &corpus.reference.commit[..12]
    );
}

/// The shipped defaults are the reference defaults, proved through the real
/// load rather than by inspecting the declaration.
#[test]
fn a_load_with_no_configuration_file_composes_the_reference_default_document() {
    let corpus = corpus();
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    fs::create_dir_all(&home).expect("home directory");
    let snapshot = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home.clone(),
            working_directory: temporary.path().join("project"),
        },
        registry::default_document(),
    )
    .load()
    .expect("the shipped defaults load on their own");
    let actual = serde_json::to_value(&snapshot.effective).expect("effective serializes");

    let recorded = ledger(UNDECLARED_FIELDS);
    let mut unpinned_observed = false;
    for (key, expected) in &corpus.defaults.document {
        // The unpinned sentinel reaches the document unchanged on both sides,
        // and the alias it resolves to is asserted beside it: unpinning the key
        // must leave the model an installation runs on exactly where it was.
        if key == "active_model" && expected.as_str() == Some(UNPINNED_ACTIVE_MODEL) {
            assert_eq!(
                actual.get(key).and_then(JsonValue::as_str),
                Some(UNPINNED_ACTIVE_MODEL),
                "the port ships a pinned alias where the reference ships the sentinel"
            );
            assert_eq!(
                snapshot.active_model_alias(),
                Some(PINNED_ACTIVE_MODEL),
                "the shipped sentinel resolves to another model than the alias it replaced"
            );
            unpinned_observed = true;
            continue;
        }
        // A field the reference declares and this port does not cannot appear
        // in a document this port composes; the census test above is what names
        // the gap, and this loop would report it a second time.
        if recorded.contains_key(key.as_str()) {
            assert!(
                actual.get(key).is_none(),
                "`{key}` is composed now and its recorded gap is stale"
            );
            continue;
        }
        // The reference serializes models as a list on write and reads them
        // back keyed by alias; the effective document carries the read form.
        let expected = if key == "models" {
            models_by_alias(expected)
        } else {
            resolve_vibe_home(expected, &home)
        };
        let Some(found) = actual.get(key) else {
            panic!("`{key}` is missing from the shipped default document");
        };
        // A default the load absolutizes is rendered with the host's path
        // separator, where the capture ran on the reference's. The two describe
        // the same directory, so they are compared separator-insensitively.
        let expected = with_posix_separators(&expected);
        let found = with_posix_separators(found);
        if let Some((pointer, want, got)) = difference(&format!("/{key}"), &expected, &found) {
            panic!("the shipped defaults diverge at {pointer}: reference {want}, port {got}");
        }
    }
    assert!(
        unpinned_observed,
        "the reference no longer ships the unpinned `active_model` sentinel; \
         drop the recorded divergence instead of carrying it"
    );

    let extra = snapshot
        .effective
        .keys()
        .filter(|key| !corpus.defaults.document.contains_key(key.as_str()))
        .collect::<Vec<_>>();
    assert!(
        extra.is_empty(),
        "the port ships defaults the reference does not: {extra:?}"
    );
    assert!(snapshot.validation_warnings.is_empty());

    // `tools` is compared for shape only: both implementations fill it from
    // their own tool discovery, so only the reference's key set is recorded.
    assert!(
        !corpus.defaults.tool_names.is_empty(),
        "the reference default document carries no discovered tool"
    );
    assert!(
        !snapshot.effective.contains_key("tools"),
        "tool discovery owns `tools`; the registry must not ship one"
    );
}

/// Reads the persisted list form the corpus records into the alias-keyed map
/// the merge composes, as the reference does when it reads a layer back.
fn models_by_alias(models: &JsonValue) -> JsonValue {
    let Some(entries) = models.as_array() else {
        return models.clone();
    };
    JsonValue::Object(
        entries
            .iter()
            .filter_map(|entry| {
                let alias = entry
                    .get("alias")
                    .or_else(|| entry.get("name"))?
                    .as_str()?
                    .to_owned();
                Some((alias, entry.clone()))
            })
            .collect(),
    )
}

/// Rewrites backslashes as forward slashes so a path rendered by a Windows
/// host compares equal to the same path rendered by the reference. On a POSIX
/// host this changes nothing.
fn with_posix_separators(value: &JsonValue) -> JsonValue {
    match value {
        JsonValue::String(text) => JsonValue::String(text.replace('\\', "/")),
        JsonValue::Object(entries) => JsonValue::Object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), with_posix_separators(value)))
                .collect(),
        ),
        JsonValue::Array(entries) => {
            JsonValue::Array(entries.iter().map(with_posix_separators).collect())
        }
        value => value.clone(),
    }
}

/// Substitutes the machine-dependent vibe home the capture wrote a placeholder for.
fn resolve_vibe_home(value: &JsonValue, home: &Path) -> JsonValue {
    match value {
        JsonValue::String(text) if text.starts_with(VIBE_HOME_PLACEHOLDER) => {
            JsonValue::String(text.replacen(VIBE_HOME_PLACEHOLDER, &home.display().to_string(), 1))
        }
        JsonValue::Object(entries) => JsonValue::Object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), resolve_vibe_home(value, home)))
                .collect(),
        ),
        JsonValue::Array(entries) => JsonValue::Array(
            entries
                .iter()
                .map(|entry| resolve_vibe_home(entry, home))
                .collect(),
        ),
        value => value.clone(),
    }
}

/// The model rules the reference applies once the merged document is validated:
/// sparse entries completed from the default one, the global compaction
/// threshold reaching the entries that set none, and the `active_model`
/// fallback with its warning.
#[test]
fn every_model_scenario_validates_to_the_document_the_reference_validates() {
    let corpus = corpus();
    assert!(!corpus.model_scenarios.is_empty());
    let mut unpinned_observed = false;
    for scenario in &corpus.model_scenarios {
        let temporary = tempfile::tempdir().expect("temporary root");
        let home = temporary.path().join("home/.vibe");
        fs::create_dir_all(&home).expect("home directory");
        let mut documents = scenario.layers.iter().map(|layer| {
            layer.toml.parse::<Table>().unwrap_or_else(|error| {
                panic!("{}: layer `{}`: {error}", scenario.name, layer.name)
            })
        });
        let mut config = LayeredConfig::new(
            ConfigPaths {
                vibe_home: home.clone(),
                working_directory: temporary.path().join("project"),
            },
            registry::default_document(),
        );
        if let Some(selected) = documents.next() {
            fs::write(home.join(CONFIG_FILE), selected.to_string()).expect("selected fixture");
        }
        if let Some(runtime) = documents.next() {
            config.runtime = runtime;
        }
        assert!(
            documents.next().is_none(),
            "{}: a model scenario may stack at most two layers over the defaults",
            scenario.name
        );

        let snapshot = config
            .load()
            .unwrap_or_else(|error| panic!("{}: {error}", scenario.name));
        // The unpinned sentinel survives the merge on both sides: the reference
        // leaves the alias empty and resolves it on read, and so does this
        // port. Every scenario is therefore compared as it stands.
        if scenario.active_model == UNPINNED_ACTIVE_MODEL {
            unpinned_observed = true;
        }
        assert_eq!(
            snapshot
                .effective
                .get("active_model")
                .and_then(Value::as_str),
            Some(scenario.active_model.as_str()),
            "{}: the active model diverges",
            scenario.name
        );
        assert_eq!(
            snapshot.validation_warnings.len(),
            scenario.validation_warnings,
            "{}: the port recorded {:?}",
            scenario.name,
            snapshot.validation_warnings
        );
        let models = snapshot
            .effective
            .get("models")
            .unwrap_or_else(|| panic!("{}: the merged document carries no model", scenario.name));
        let models = serde_json::to_value(models).expect("models serialize");
        if let Some((pointer, want, got)) = difference(
            "/models",
            &JsonValue::Object(scenario.models.clone()),
            &models,
        ) {
            panic!(
                "{}: the validated models diverge at {pointer}: reference {want}, port {got}",
                scenario.name
            );
        }
        let compaction = snapshot.effective.get("compaction_model");
        match scenario.compaction_model.as_ref() {
            None | Some(JsonValue::Null) => assert!(
                compaction.is_none(),
                "{}: the port composed a compaction model the reference validated to none",
                scenario.name
            ),
            Some(expected) => {
                let compaction = compaction.unwrap_or_else(|| {
                    panic!(
                        "{}: the merged document carries no compaction model",
                        scenario.name
                    )
                });
                let compaction =
                    serde_json::to_value(compaction).expect("the compaction model serializes");
                if let Some((pointer, want, got)) =
                    difference("/compaction_model", expected, &compaction)
                {
                    panic!(
                        "{}: the validated compaction model diverges at {pointer}: reference \
                         {want}, port {got}",
                        scenario.name
                    );
                }
            }
        }
    }
    assert!(
        unpinned_observed,
        "no model scenario leaves `active_model` unpinned; drop the recorded \
         divergence instead of carrying it"
    );
    println!(
        "config surface: {}/{} model scenarios conform at {}",
        corpus.model_scenarios.len(),
        corpus.model_scenarios.len(),
        &corpus.reference.commit[..12]
    );
}

#[test]
fn the_port_implements_every_strategy_the_reference_reaches() {
    let corpus = corpus();
    let implemented = BTreeSet::from([
        MergeStrategy::Replace.as_str(),
        MergeStrategy::Concat.as_str(),
        MergeStrategy::Union.as_str(),
        MergeStrategy::DeepMerge.as_str(),
    ]);
    let used = corpus
        .strategies
        .used
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        used, implemented,
        "a reference field adopted a strategy this port does not implement"
    );
    // `WithShallowMerge` and `WithConflictMerge` exist in the reference
    // vocabulary under the strategy values `merge` and `conflict`; no field
    // declares either, which is why neither is implemented here.
    assert_eq!(corpus.strategies.unused, ["conflict", "merge"]);
    assert!(
        corpus.strategies.declared.len() == used.len() + corpus.strategies.unused.len(),
        "the reference strategy vocabulary changed"
    );
}

#[test]
fn every_scenario_composes_the_document_the_reference_produces() {
    let corpus = corpus();
    assert!(
        corpus.scenarios.len() >= MINIMUM_SCENARIOS,
        "the corpus covers {} scenarios, below the {MINIMUM_SCENARIOS} this epic commits to",
        corpus.scenarios.len()
    );
    for scenario in &corpus.scenarios {
        replay(scenario);
    }
    println!(
        "config surface: {}/{} merge scenarios conform at {}",
        corpus.scenarios.len(),
        corpus.scenarios.len(),
        &corpus.reference.commit[..12]
    );
}

fn replay(scenario: &Scenario) {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    fs::create_dir_all(&home).expect("home directory");

    // Every layer is composed through the real load: the lowest becomes the
    // defaults document, the next the selected file, and the rest the
    // experiments, runtime and agent layers, in the order `load` composes them.
    let mut documents = scenario.layers.iter().map(|layer| {
        layer
            .toml
            .parse::<Table>()
            .unwrap_or_else(|error| panic!("{}: layer `{}`: {error}", scenario.name, layer.name))
    });
    let mut config = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home.clone(),
            working_directory: temporary.path().join("project"),
        },
        documents.next().unwrap_or_default(),
    );
    if let Some(selected) = documents.next() {
        fs::write(home.join(CONFIG_FILE), selected.to_string()).expect("selected fixture");
    }
    if let Some(experiments) = documents.next() {
        config.experiments = experiments;
    }
    if let Some(runtime) = documents.next() {
        config.runtime = runtime;
    }
    if let Some(agent) = documents.next() {
        config.agent = agent;
    }
    assert!(
        documents.next().is_none(),
        "{}: a scenario may stack at most five layers",
        scenario.name
    );

    let snapshot = config
        .load()
        .unwrap_or_else(|error| panic!("{}: {error}", scenario.name));
    let effective = &snapshot.effective;
    let actual = serde_json::to_value(effective).expect("effective document serializes");

    for (key, expected) in &scenario.merged {
        // TOML has no null, so a field the reference merged to `None` is absent
        // here rather than present and empty.
        if expected.is_null() {
            assert!(
                !effective.contains_key(key),
                "{}: `{key}` should be absent where the reference merged it to null",
                scenario.name
            );
            continue;
        }
        let Some(found) = actual.get(key) else {
            panic!(
                "{}: `{key}` is missing from the merged document",
                scenario.name
            );
        };
        if let Some((pointer, want, got)) = difference(&format!("/{key}"), expected, found) {
            panic!(
                "{}: merged configuration diverges at {pointer}: reference {want}, port {got}",
                scenario.name
            );
        }
    }

    let unexpected = effective
        .keys()
        .filter(|key| registry::field(key).is_some())
        .filter(|key| !scenario.merged.contains_key(key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        unexpected.is_empty(),
        "{}: the port composed fields the reference did not: {unexpected:?}",
        scenario.name
    );

    assert_eq!(
        snapshot.unregistered_keys(),
        scenario.dropped_keys,
        "{}: the keys the reference drops and this port preserves changed",
        scenario.name
    );
}

/// The first JSON pointer at which two documents disagree, with both values
/// rendered and any value under a sensitive key redacted.
fn difference(
    pointer: &str,
    expected: &JsonValue,
    actual: &JsonValue,
) -> Option<(String, String, String)> {
    match (expected, actual) {
        (JsonValue::Object(expected), JsonValue::Object(actual)) => {
            for (key, value) in expected {
                let nested = format!("{pointer}/{}", escape_pointer_token(key));
                let Some(found) = actual.get(key) else {
                    // TOML has no null, so a field the reference validated to
                    // `None` is absent here rather than present and empty.
                    if value.is_null() {
                        continue;
                    }
                    return Some((nested, render(key, value), "absent".to_owned()));
                };
                if let Some(found) = difference(&nested, value, found) {
                    return Some(found);
                }
            }
            for key in actual.keys() {
                if !expected.contains_key(key) {
                    let nested = format!("{pointer}/{}", escape_pointer_token(key));
                    return Some((nested, "absent".to_owned(), render(key, &actual[key])));
                }
            }
            None
        }
        (JsonValue::Array(expected_entries), JsonValue::Array(actual_entries)) => {
            if expected_entries.len() != actual_entries.len() {
                return Some((
                    pointer.to_owned(),
                    format!("{} entries", expected_entries.len()),
                    format!("{} entries", actual_entries.len()),
                ));
            }
            expected_entries
                .iter()
                .zip(actual_entries)
                .enumerate()
                .find_map(|(index, (expected, actual))| {
                    difference(&format!("{pointer}/{index}"), expected, actual)
                })
        }
        (expected, actual) if expected == actual => None,
        (expected, actual) => {
            let key = pointer.rsplit('/').next().unwrap_or_default();
            Some((
                pointer.to_owned(),
                render(key, expected),
                render(key, actual),
            ))
        }
    }
}

fn render(key: &str, value: &JsonValue) -> String {
    if is_sensitive_key(key) {
        return "[redacted]".to_owned();
    }
    redacted(value).to_string()
}

/// Redacts every nested value whose own key is sensitive.
///
/// A divergence at a key one side omits renders that side's whole subtree, so
/// checking only the key at the pointer would let a nested secret through.
fn redacted(value: &JsonValue) -> JsonValue {
    match value {
        JsonValue::Object(entries) => JsonValue::Object(
            entries
                .iter()
                .map(|(key, value)| {
                    let value = if is_sensitive_key(key) {
                        JsonValue::from("[redacted]")
                    } else {
                        redacted(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        JsonValue::Array(entries) => JsonValue::Array(entries.iter().map(redacted).collect()),
        value => value.clone(),
    }
}

fn escape_pointer_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// No corpus scenario diverges in a passing suite, so the message a divergence
/// would produce is proved here instead: it has to name the pointer and both
/// values, and it must never carry a value read from a sensitive-named key.
#[test]
fn a_divergence_names_its_pointer_and_redacts_a_sensitive_value() {
    let (pointer, expected, actual) = difference(
        "",
        &serde_json::json!({"theme": "system"}),
        &serde_json::json!({"theme": "nord"}),
    )
    .expect("the two documents disagree on the theme");
    assert_eq!(pointer, "/theme");
    assert_eq!(expected, "\"system\"");
    assert_eq!(actual, "\"nord\"");

    let (pointer, expected, actual) = difference(
        "",
        &serde_json::json!({
            "providers": [{"name": "mistral", "api_key_env_var": "REFERENCE_VALUE"}]
        }),
        &serde_json::json!({
            "providers": [{"name": "mistral", "api_key_env_var": "PORT_VALUE"}]
        }),
    )
    .expect("the two documents disagree under a sensitive key");
    assert_eq!(pointer, "/providers/0/api_key_env_var");
    assert!(is_sensitive_key("api_key_env_var"));
    assert_eq!(expected, "[redacted]");
    assert_eq!(actual, "[redacted]");

    // A key present on one side only renders that side's whole subtree, so the
    // redaction has to reach a sensitive key nested inside it.
    let (pointer, expected, actual) = difference(
        "",
        &serde_json::json!({"tools": {"bash": {"token": "reference", "timeout": 30}}}),
        &serde_json::json!({"tools": {}}),
    )
    .expect("the port dropped a nested table");
    assert_eq!(pointer, "/tools/bash");
    assert!(expected.contains("\"token\":\"[redacted]\""), "{expected}");
    assert!(!expected.contains("reference"), "{expected}");
    assert!(expected.contains("\"timeout\":30"), "{expected}");
    assert_eq!(actual, "absent");
}

/// The pinned checkout and an interpreter that can drive it, or `None` when the
/// live probe cannot run here. Every replay above still ran against the
/// committed corpus.
fn pinned_reference() -> Option<(PathBuf, PathBuf)> {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "configuration oracle") {
        eprintln!("{reason}");
        return None;
    }
    let interpreter = pinned_interpreter(&root)?;
    Some((root, interpreter))
}

#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let Some((root, interpreter)) = pinned_reference() else {
        eprintln!(
            "skipping the live configuration oracle probe: no pinned checkout to capture from"
        );
        return;
    };
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    let temporary = tempfile::tempdir().expect("temporary root");
    let captured = temporary.path().join("corpus.json");
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
