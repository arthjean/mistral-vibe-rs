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
//! default layer, because the model rules only run after the merge. The layer
//! stack, write, environment, agent profile, dotenv, sign-in rewrite and
//! encoding families of the same corpus are replayed by `layers_parity_tests`.
//!
//! One comparison is deliberately scoped: a key the reference schema does not
//! declare is dropped by the reference merge and kept by this one, which FR-04
//! requires. The corpus records those keys so the divergence is proved rather
//! than assumed.
//!
//! Every other difference is collected pointer by pointer and must be named by
//! a ledger below, scoped to its case and pointer: a difference no entry names
//! fails the replay, and so does an entry whose difference stopped reproducing.

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
const CORPUS_SCHEMA_VERSION: u32 = 5;
/// The scenario floor this epic commits to.
const MINIMUM_SCENARIOS: usize = 24;
/// What the capture writes where the vibe home is machine-dependent.
const VIBE_HOME_PLACEHOLDER: &str = "{vibe_home}";

/// Reference fields this port does not declare, each with the reason.
///
/// Empty since `utility_models` (`vibe/core/config/vibe_schema.py:376` at
/// `376f6a3`), the one field the v2.26.0 re-pin added, was declared. The
/// replay fails on any field the reference adds, and an entry whose field
/// becomes declared fails it as stale.
const UNDECLARED_FIELDS: &[(&str, &str)] = &[];

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

/// Registry fields the reference no longer declares, each with the reason.
///
/// The census fails on a registry field with no corpus entry; an entry here
/// names one the reference withdrew and this port still declares. It fails as
/// stale when the registry drops the field or the reference declares it again.
const WITHDRAWN_FIELDS: &[(&str, &str)] = &[];

/// Fields both sides declare with different merge strategies, as
/// `(field, reference strategy, port strategy, reason)`.
///
/// An entry fails as stale when the two strategies agree again, and fails when
/// either side moves to a third strategy.
const STRATEGY_DIVERGENCES: &[(&str, &str, &str, &str)] = &[];

/// Strategies a reference field adopts that this port does not implement, each
/// with the reason. An entry fails as stale when no reference field uses the
/// strategy any more or the port implements it.
const UNIMPLEMENTED_STRATEGIES: &[(&str, &str)] = &[];

/// v2.26.0 made "no threshold set" a value a model entry carries.
const UNSET_THRESHOLD_DROPPED: &str = "v2.26.0 defaults a model's `auto_compact_threshold` to the unset sentinel -1 \
     (models.py:492; _defaults.py:30) and drops the sentinel when it writes a model entry out \
     (models.py:563); the port's default models ship 200000";

/// v2.26.0 lets the global threshold reach the shipped default models.
const GLOBAL_THRESHOLD: &str = "v2.26.0 materializes the unset sentinel for a model that sets no threshold \
     (models.py:492), so a layer's global `auto_compact_threshold` now reaches the shipped \
     default models as well (vibe_schema.py:949); the port's default models carry their own \
     200000, which counts as set and keeps the global value out";

/// v2.26.0 resolves the threshold sentinel for `models` entries alone.
const UNSET_THRESHOLD: &str = "v2.26.0 resolves the unset threshold sentinel only for entries of `models` \
     (vibe_schema.py:949), so a `compaction_model`, `vision_model` or routed extra entry that \
     sets no threshold validates to -1 (models.py:492); the port completes it to 200000";

/// v2.26.0 matches the model allowlist against names.
const ALLOWLIST_BY_NAME: &str = "v2.26.0 matches `allowed_models` against model names rather than aliases \
     (vibe_schema.py:690, :1004), so a pattern written for the `local` alias matches no name, \
     warns, and admits every model; the port still matches aliases";

/// v2.26.0 warns when the allowlist excludes the pinned model.
const DISALLOWED_ACTIVE_MODEL: &str = "v2.26.0 records a warning when `allowed_models` excludes the pinned \
     `active_model` (vibe_schema.py:1016); the port falls back to the default alias without \
     one";

/// Pointers at which the document this port ships diverges from the reference
/// default document, as `(pointer, reason)`. A key the port ships and the
/// reference does not is named at its own pointer.
const DEFAULT_DIVERGENCES: &[(&str, &str)] = &[
    (
        "/auto_compact_threshold",
        "v2.26.0 defaults the global threshold to the unset sentinel -1 (vibe_schema.py:386) \
         and drops it from the document `create_default_config` ships (vibe_schema.py:1119); \
         registry.rs ships 200000",
    ),
    (
        "/models/local/auto_compact_threshold",
        UNSET_THRESHOLD_DROPPED,
    ),
    (
        "/models/mistral-medium-3.5/auto_compact_threshold",
        UNSET_THRESHOLD_DROPPED,
    ),
];

/// Pointers at which a merge scenario diverges, as `(scenario, pointer, reason)`.
const SCENARIO_DIVERGENCES: &[(&str, &str, &str)] = &[];

/// Pointers at which a model scenario diverges, as `(scenario, pointer,
/// reason)`. `/active_model` and `/validation_warnings` name the validated alias
/// and the warning count; every other pointer lies under `/models` or
/// `/compaction_model`. Every entry dates from the v2.26.0 re-pin (`376f6a3`).
const MODEL_SCENARIO_DIVERGENCES: &[(&str, &str, &str)] = &[
    (
        "models-added-entry-inherits-the-global-threshold",
        "/models/local/auto_compact_threshold",
        GLOBAL_THRESHOLD,
    ),
    (
        "models-added-entry-inherits-the-global-threshold",
        "/models/mistral-medium-3.5/auto_compact_threshold",
        GLOBAL_THRESHOLD,
    ),
    (
        "models-entry-keeps-its-own-threshold",
        "/models/local/auto_compact_threshold",
        GLOBAL_THRESHOLD,
    ),
    (
        "models-entry-keeps-its-own-threshold",
        "/models/mistral-medium-3.5/auto_compact_threshold",
        GLOBAL_THRESHOLD,
    ),
    (
        "models-compaction-model-without-an-alias-borrows-its-name",
        "/compaction_model/auto_compact_threshold",
        UNSET_THRESHOLD,
    ),
    (
        "models-allowed-models-regex-is-case-insensitive",
        "/validation_warnings",
        ALLOWLIST_BY_NAME,
    ),
    (
        "models-allowed-models-regex-is-case-insensitive",
        "/available_models",
        ALLOWLIST_BY_NAME,
    ),
    (
        "models-allowed-models-regex-is-case-insensitive",
        "/default_model_alias",
        ALLOWLIST_BY_NAME,
    ),
    (
        "models-allowed-models-regex-is-case-insensitive",
        "/active_model_alias",
        ALLOWLIST_BY_NAME,
    ),
    (
        "models-allowed-models-matching-nothing-warns-and-admits-all",
        "/validation_warnings",
        ALLOWLIST_BY_NAME,
    ),
    (
        "models-allowed-models-matching-nothing-warns-and-admits-all",
        "/available_models",
        ALLOWLIST_BY_NAME,
    ),
    (
        "models-allowed-models-matching-nothing-warns-and-admits-all",
        "/default_model_alias",
        ALLOWLIST_BY_NAME,
    ),
    (
        "models-allowed-models-matching-nothing-warns-and-admits-all",
        "/active_model_alias",
        ALLOWLIST_BY_NAME,
    ),
    (
        "models-allowed-models-excluding-the-pin-falls-back",
        "/validation_warnings",
        DISALLOWED_ACTIVE_MODEL,
    ),
    (
        "models-allowed-models-excluding-the-default",
        "/validation_warnings",
        ALLOWLIST_BY_NAME,
    ),
    (
        "models-allowed-models-excluding-the-default",
        "/available_models",
        ALLOWLIST_BY_NAME,
    ),
    (
        "models-allowed-models-excluding-the-default",
        "/default_model_alias",
        ALLOWLIST_BY_NAME,
    ),
    (
        "models-allowed-models-excluding-the-default",
        "/active_model_alias",
        ALLOWLIST_BY_NAME,
    ),
    (
        "models-routed-extra-models-from-json-text",
        "/routed_extra_models/0/auto_compact_threshold",
        UNSET_THRESHOLD,
    ),
    (
        "models-routed-extra-models-from-json-text",
        "/routed_extra_models/1/auto_compact_threshold",
        UNSET_THRESHOLD,
    ),
    (
        "models-routed-extra-models-keep-what-the-operator-wrote",
        "/models/local/auto_compact_threshold",
        GLOBAL_THRESHOLD,
    ),
    (
        "models-routed-extra-models-keep-what-the-operator-wrote",
        "/models/mistral-medium-3.5/auto_compact_threshold",
        GLOBAL_THRESHOLD,
    ),
    (
        "models-routed-extra-models-keep-what-the-operator-wrote",
        "/routed_extra_models/0/auto_compact_threshold",
        UNSET_THRESHOLD,
    ),
    (
        "models-routed-extra-models-keep-what-the-operator-wrote",
        "/routed_extra_models/1/auto_compact_threshold",
        UNSET_THRESHOLD,
    ),
    (
        "models-vision-model-is-completed-like-a-model",
        "/models/local/auto_compact_threshold",
        GLOBAL_THRESHOLD,
    ),
    (
        "models-vision-model-is-completed-like-a-model",
        "/models/mistral-medium-3.5/auto_compact_threshold",
        GLOBAL_THRESHOLD,
    ),
    (
        "models-vision-model-is-completed-like-a-model",
        "/vision_model/auto_compact_threshold",
        UNSET_THRESHOLD,
    ),
    (
        "models-compaction-model-ignores-the-global-threshold",
        "/models/local/auto_compact_threshold",
        GLOBAL_THRESHOLD,
    ),
    (
        "models-compaction-model-ignores-the-global-threshold",
        "/models/mistral-medium-3.5/auto_compact_threshold",
        GLOBAL_THRESHOLD,
    ),
    (
        "models-compaction-model-ignores-the-global-threshold",
        "/compaction_model/auto_compact_threshold",
        UNSET_THRESHOLD,
    ),
    (
        "models-user-threshold-keeps-a-model-threshold",
        "/models/local/auto_compact_threshold",
        GLOBAL_THRESHOLD,
    ),
    (
        "models-user-threshold-keeps-a-model-threshold",
        "/models/mistral-medium-3.5/auto_compact_threshold",
        GLOBAL_THRESHOLD,
    ),
    (
        "models-compaction-model-keeps-the-alias-it-declares",
        "/compaction_model/auto_compact_threshold",
        UNSET_THRESHOLD,
    ),
];

/// One divergence a replay observed: the case, the pointer, and both values
/// rendered with sensitive values redacted.
#[derive(Debug)]
struct Observed {
    case: String,
    pointer: String,
    reference: String,
    port: String,
}

/// Fails on every observed divergence the ledger does not record, and on every
/// recorded one that no longer reproduces.
fn reconcile(family: &str, observed: &[Observed], ledger: &[(&str, &str, &str)]) {
    let mut seen = BTreeSet::new();
    for (case, pointer, _) in ledger {
        assert!(
            seen.insert((*case, *pointer)),
            "{family}: `{case}` records {pointer} twice"
        );
    }
    let unrecorded = observed
        .iter()
        .filter(|found| {
            !ledger
                .iter()
                .any(|(case, pointer, _)| *case == found.case && *pointer == found.pointer)
        })
        .map(|found| {
            format!(
                "{}: {}: reference {}, port {}",
                found.case, found.pointer, found.reference, found.port
            )
        })
        .collect::<Vec<_>>();
    assert!(
        unrecorded.is_empty(),
        "{family} diverges where no ledger entry records it:\n{}",
        unrecorded.join("\n")
    );
    let stale = ledger
        .iter()
        .filter(|(case, pointer, _)| {
            !observed
                .iter()
                .any(|found| found.case == *case && found.pointer == *pointer)
        })
        .map(|(case, pointer, _)| format!("{case}: {pointer}"))
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "{family}: these recorded divergences no longer reproduce: {stale:?}"
    );
}

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
    /// The layer stack, environment, agent profile, dotenv and sign-in
    /// families, replayed by `layers_parity_tests` through its own view.
    #[expect(dead_code, reason = "replayed by `layers_parity_tests`")]
    stack: JsonValue,
    #[expect(dead_code, reason = "replayed by `layers_parity_tests`")]
    environment: JsonValue,
    #[expect(dead_code, reason = "replayed by `layers_parity_tests`")]
    agent_profiles: JsonValue,
    #[expect(dead_code, reason = "replayed by `layers_parity_tests`")]
    dotenv: JsonValue,
    #[expect(dead_code, reason = "replayed by `layers_parity_tests`")]
    origin_rewrite: JsonValue,
    #[expect(dead_code, reason = "replayed by `layers_parity_tests`")]
    encoding: JsonValue,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Defaults {
    document: Map<String, JsonValue>,
    tool_names: Vec<String>,
}

/// What the reference validated one model stack to. Every field but the name,
/// the layers and the verdict is absent when validation refused the stack.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelScenario {
    name: String,
    layers: Vec<ScenarioLayer>,
    /// Whether validation refused the stack, as a vision model that cannot
    /// read images or names an unknown provider is refused.
    rejected: bool,
    active_model: Option<String>,
    models: Option<Map<String, JsonValue>>,
    /// The validated `compaction_model`, or `null` where the document declares
    /// none. It is a `ModelConfig` like every entry of `models`, so it carries
    /// the same alias rule and the same per-entry defaults.
    compaction_model: Option<JsonValue>,
    vision_model: Option<JsonValue>,
    routed_extra_models: Option<Vec<JsonValue>>,
    /// The aliases the allowlist leaves selectable, in the order the reference
    /// lists them.
    available_models: Option<Vec<String>>,
    default_model_alias: Option<String>,
    active_model_alias: Option<String>,
    validation_warnings: Option<usize>,
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
        if let Some((_, reference, port, _)) = STRATEGY_DIVERGENCES
            .iter()
            .find(|(name, ..)| *name == field.name)
        {
            assert_eq!(
                (field.strategy.as_str(), spec.strategy.as_str()),
                (*reference, *port),
                "field `{}` moved away from its recorded strategy divergence",
                field.name
            );
        } else {
            assert_eq!(
                spec.strategy.as_str(),
                field.strategy,
                "field `{}` merges by the wrong strategy",
                field.name
            );
        }
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
    let stale_strategies = STRATEGY_DIVERGENCES
        .iter()
        .map(|(name, ..)| *name)
        .filter(|name| {
            !corpus.fields.iter().any(|field| field.name == *name) || !declared.contains_key(name)
        })
        .collect::<Vec<_>>();
    assert!(
        stale_strategies.is_empty(),
        "these recorded strategy divergences name a field one side no longer declares: \
         {stale_strategies:?}"
    );
    let retired = ledger(WITHDRAWN_FIELDS);
    for name in retired.keys() {
        assert!(
            !corpus.fields.iter().any(|field| &field.name == name),
            "the reference declares `{name}` again and its recorded withdrawal is stale"
        );
        assert!(
            declared.get(name.as_str()).is_some_and(|spec| !spec.local),
            "the registry no longer declares `{name}` and its recorded withdrawal is stale"
        );
    }
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
        .filter(|name| !known.contains(name) && !retired.contains_key(*name))
        .collect::<Vec<_>>();
    assert!(
        ungated.is_empty(),
        "these registry fields have no corpus entry: {ungated:?}. Recapture with {CAPTURE_SCRIPT}, \
         or declare each as local and record the divergence in the PRD"
    );
    assert_eq!(
        local,
        ["thinking", "proxy", "tls_ca_path", "dotenv_path"],
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
    let mut observed = Vec::new();
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
        observe(
            "defaults",
            &format!("/{key}"),
            &expected,
            &found,
            &mut observed,
        );
    }
    // A key the port ships and the reference does not is a divergence at its
    // own pointer. A field the reference withdrew still ships here, and the
    // census test is what records it; this check would report it a second time.
    let retired = ledger(WITHDRAWN_FIELDS);
    for (key, value) in snapshot
        .effective
        .iter()
        .filter(|(key, _)| !corpus.defaults.document.contains_key(key.as_str()))
        .filter(|(key, _)| !retired.contains_key(key.as_str()))
    {
        let value = serde_json::to_value(value).expect("a field serializes");
        observed.push(Observed {
            case: "defaults".to_owned(),
            pointer: format!("/{}", escape_pointer_token(key)),
            reference: "absent".to_owned(),
            port: render(key, &value),
        });
    }
    let default_ledger = DEFAULT_DIVERGENCES
        .iter()
        .map(|(pointer, reason)| ("defaults", *pointer, *reason))
        .collect::<Vec<_>>();
    reconcile("the shipped default document", &observed, &default_ledger);
    assert!(
        unpinned_observed,
        "the reference no longer ships the unpinned `active_model` sentinel; \
         drop the recorded divergence instead of carrying it"
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
/// sparse entries completed from the default one, the routed and extra routed
/// entries injected, the global and admin compaction thresholds, the allowlist,
/// the vision model checks, and the `active_model` fallback with its warning.
#[test]
fn every_model_scenario_validates_to_the_document_the_reference_validates() {
    let corpus = corpus();
    assert!(!corpus.model_scenarios.is_empty());
    let mut unpinned_observed = false;
    let mut observed = Vec::new();
    for scenario in &corpus.model_scenarios {
        let temporary = tempfile::tempdir().expect("temporary root");
        let home = temporary.path().join("home/.vibe");
        let project = temporary.path().join("project");
        fs::create_dir_all(&home).expect("home directory");
        let mut config = LayeredConfig::new(
            ConfigPaths {
                vibe_home: home.clone(),
                working_directory: project.clone(),
            },
            registry::default_document(),
        )
        .with_project_trusted(true)
        .with_project_file_trust(Some(true));
        for layer in &scenario.layers {
            let document = layer.toml.parse::<Table>().unwrap_or_else(|error| {
                panic!("{}: layer `{}`: {error}", scenario.name, layer.name)
            });
            // Each layer lands where the reference stack named it.
            match layer.name.as_str() {
                "user" => {
                    fs::write(home.join(CONFIG_FILE), document.to_string()).expect("user fixture")
                }
                "project" => {
                    let directory = project.join(".vibe");
                    fs::create_dir_all(&directory).expect("project directory");
                    fs::write(directory.join(CONFIG_FILE), document.to_string())
                        .expect("project fixture");
                }
                "overrides" => config.runtime = document,
                "admin" => {
                    config.set_admin(document);
                }
                other => panic!("{}: no layer is named `{other}`", scenario.name),
            }
        }

        let loaded = config.load();
        observe(
            &scenario.name,
            "/rejected",
            &JsonValue::from(scenario.rejected),
            &JsonValue::from(loaded.is_err()),
            &mut observed,
        );
        let (Ok(snapshot), false) = (loaded, scenario.rejected) else {
            continue;
        };
        let effective = &snapshot.effective;
        // The unpinned sentinel survives the merge on both sides: the reference
        // leaves the alias empty and resolves it on read, and so does this
        // port. Every scenario is therefore compared as it stands.
        if scenario.active_model.as_deref() == Some(UNPINNED_ACTIVE_MODEL) {
            unpinned_observed = true;
        }
        let text = |value: Option<&str>| value.map_or(JsonValue::Null, JsonValue::from);
        let field = |name: &str| {
            effective.get(name).map_or(JsonValue::Null, |value| {
                serde_json::to_value(value).expect("a field serializes")
            })
        };
        let expected = |value: &Option<JsonValue>| value.clone().unwrap_or(JsonValue::Null);
        let mut compare = |pointer: &str, reference: JsonValue, port: JsonValue| {
            observe(&scenario.name, pointer, &reference, &port, &mut observed);
        };
        compare(
            "/active_model",
            text(scenario.active_model.as_deref()),
            text(effective.get("active_model").and_then(Value::as_str)),
        );
        compare(
            "/validation_warnings",
            JsonValue::from(scenario.validation_warnings),
            JsonValue::from(snapshot.validation_warnings.len()),
        );
        compare(
            "/models",
            JsonValue::Object(scenario.models.clone().unwrap_or_default()),
            field("models"),
        );
        compare(
            "/compaction_model",
            expected(&scenario.compaction_model),
            field("compaction_model"),
        );
        compare(
            "/vision_model",
            expected(&scenario.vision_model),
            field("vision_model"),
        );
        compare(
            "/routed_extra_models",
            JsonValue::from(scenario.routed_extra_models.clone().unwrap_or_default()),
            match field("routed_extra_models") {
                JsonValue::Null => JsonValue::Array(Vec::new()),
                published => published,
            },
        );
        compare(
            "/available_models",
            JsonValue::from(scenario.available_models.clone().unwrap_or_default()),
            JsonValue::from(available_model_aliases(effective)),
        );
        compare(
            "/default_model_alias",
            text(scenario.default_model_alias.as_deref()),
            text(default_model_alias(effective)),
        );
        compare(
            "/active_model_alias",
            text(scenario.active_model_alias.as_deref()),
            text(active_model_alias(effective)),
        );
    }
    reconcile(
        "the validated model scenarios",
        &observed,
        MODEL_SCENARIO_DIVERGENCES,
    );
    assert!(
        unpinned_observed,
        "no model scenario leaves `active_model` unpinned; drop the recorded \
         divergence instead of carrying it"
    );
    println!(
        "config surface: {}/{} model scenarios conform at {}",
        conforming(
            corpus.model_scenarios.iter().map(|scenario| &scenario.name),
            &observed
        ),
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
        MergeStrategy::Merge.as_str(),
        MergeStrategy::DeepMerge.as_str(),
    ]);
    let used = corpus
        .strategies
        .used
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let recorded = UNIMPLEMENTED_STRATEGIES
        .iter()
        .map(|(strategy, _)| *strategy)
        .collect::<BTreeSet<_>>();
    let stale = recorded
        .iter()
        .filter(|strategy| !used.contains(*strategy) || implemented.contains(*strategy))
        .collect::<Vec<_>>();
    assert!(
        stale.is_empty(),
        "these recorded strategy gaps no longer reproduce: {stale:?}"
    );
    assert_eq!(
        used.difference(&recorded).copied().collect::<BTreeSet<_>>(),
        implemented,
        "a reference field adopted a strategy this port does not implement"
    );
    // `WithConflictMerge` exists in the reference vocabulary under the strategy
    // value `conflict`; no field declares it, which is why it is not
    // implemented here.
    assert_eq!(corpus.strategies.unused, ["conflict"]);
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
    let mut observed = Vec::new();
    for scenario in &corpus.scenarios {
        replay(scenario, &mut observed);
    }
    reconcile("the merge scenarios", &observed, SCENARIO_DIVERGENCES);
    println!(
        "config surface: {}/{} merge scenarios conform at {}",
        conforming(
            corpus.scenarios.iter().map(|scenario| &scenario.name),
            &observed
        ),
        corpus.scenarios.len(),
        &corpus.reference.commit[..12]
    );
}

/// How many of the named cases replayed without a single divergence.
fn conforming<'a>(cases: impl Iterator<Item = &'a String>, observed: &[Observed]) -> usize {
    cases
        .filter(|case| !observed.iter().any(|found| &found.case == *case))
        .count()
}

fn replay(scenario: &Scenario, observed: &mut Vec<Observed>) {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    fs::create_dir_all(&home).expect("home directory");

    // Every layer is composed through the real load: the lowest becomes the
    // defaults document, the next the experiments assignment, and the rest the
    // selected file, the runtime and the agent layers, in the order `load`
    // composes them.
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
    if let Some(experiments) = documents.next() {
        config.set_experiments(experiments);
    }
    if let Some(selected) = documents.next() {
        fs::write(home.join(CONFIG_FILE), selected.to_string()).expect("selected fixture");
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
    // The reference family records what `ConfigBuilder` merges, before the
    // schema validates the result, so the layers the load assembled are merged
    // again here without the validation pass that completes model fields and
    // resolves the session log directory.
    let mut effective = Table::new();
    for layer in &snapshot.layer_values {
        merge::merge_layer(&mut effective, &layer.values)
            .unwrap_or_else(|error| panic!("{}: {error}", scenario.name));
    }
    let effective = &effective;
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
        observe(
            &scenario.name,
            &format!("/{key}"),
            expected,
            found,
            observed,
        );
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

/// Records, under `case`, every pointer at which two documents disagree.
fn observe(
    case: &str,
    pointer: &str,
    expected: &JsonValue,
    actual: &JsonValue,
    observed: &mut Vec<Observed>,
) {
    let mut found = Vec::new();
    differences(pointer, expected, actual, &mut found);
    observed.extend(
        found
            .into_iter()
            .map(|(pointer, reference, port)| Observed {
                case: case.to_owned(),
                pointer,
                reference,
                port,
            }),
    );
}

/// The first JSON pointer at which two documents disagree, with both values
/// rendered and any value under a sensitive key redacted.
fn difference(
    pointer: &str,
    expected: &JsonValue,
    actual: &JsonValue,
) -> Option<(String, String, String)> {
    let mut found = Vec::new();
    differences(pointer, expected, actual, &mut found);
    found.into_iter().next()
}

/// Every JSON pointer at which two documents disagree, in document order, with
/// both values rendered and any value under a sensitive key redacted. A key one
/// side omits and an array whose length differs are each one divergence at
/// their own pointer; nothing beneath them is walked.
fn differences(
    pointer: &str,
    expected: &JsonValue,
    actual: &JsonValue,
    found: &mut Vec<(String, String, String)>,
) {
    match (expected, actual) {
        (JsonValue::Object(expected), JsonValue::Object(actual)) => {
            for (key, value) in expected {
                let nested = format!("{pointer}/{}", escape_pointer_token(key));
                let Some(present) = actual.get(key) else {
                    // TOML has no null, so a field the reference validated to
                    // `None` is absent here rather than present and empty.
                    if !value.is_null() {
                        found.push((nested, render(key, value), "absent".to_owned()));
                    }
                    continue;
                };
                differences(&nested, value, present, found);
            }
            for key in actual.keys() {
                if !expected.contains_key(key) {
                    let nested = format!("{pointer}/{}", escape_pointer_token(key));
                    found.push((nested, "absent".to_owned(), render(key, &actual[key])));
                }
            }
        }
        (JsonValue::Array(expected_entries), JsonValue::Array(actual_entries)) => {
            if expected_entries.len() != actual_entries.len() {
                found.push((
                    pointer.to_owned(),
                    format!("{} entries", expected_entries.len()),
                    format!("{} entries", actual_entries.len()),
                ));
                return;
            }
            for (index, (expected, actual)) in
                expected_entries.iter().zip(actual_entries).enumerate()
            {
                differences(&format!("{pointer}/{index}"), expected, actual, found);
            }
        }
        (expected, actual) if expected == actual => {}
        (expected, actual) => {
            let key = pointer.rsplit('/').next().unwrap_or_default();
            found.push((
                pointer.to_owned(),
                render(key, expected),
                render(key, actual),
            ));
        }
    }
}

fn render(key: &str, value: &JsonValue) -> String {
    if is_sensitive_key(key) {
        return crate::redaction::REDACTED.to_owned();
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
                        JsonValue::from(crate::redaction::REDACTED)
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

/// A passing suite reports only the divergences its ledgers record, so the
/// message an unrecorded one would produce is proved here instead: it has to
/// name the pointer and both values, and it must never carry a value read from
/// a sensitive-named key.
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
