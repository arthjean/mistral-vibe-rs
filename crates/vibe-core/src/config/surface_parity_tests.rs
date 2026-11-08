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
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Map, Value as JsonValue};
use toml::Value;

use super::registry::{FIELDS, MergeStrategy};
use super::*;
use crate::provider::config::{ModelRouting, UtilityFeature};

use crate::parity::figures::{Figures, assert_figures};
use crate::parity::is_pin_commit;
use crate::parity::replay::{
    Capture, Class, Entry as _, PatternDivergence, assert_ledger, entries, pinned_reference,
};
use crate::redaction::is_sensitive_key;

const CAPTURE_SCRIPT: &str = "scripts/parity/config_surface.py";
const CORPUS_RELATIVE: &str = "tests/config-surface/corpus.json";
/// The corpus layout this runner reads, matching `SCHEMA_VERSION` in the
/// capture script.
const CORPUS_SCHEMA_VERSION: u32 = 7;
/// The corpus as the manifest lists it, which every typed ledger here is
/// checked against.
pub(super) const LISTED: &str = "crates/vibe-core/tests/config-surface/corpus.json";
/// The row of `docs/parity.md` this corpus measures.
pub(super) const ROW: &str = "30";
/// The scenario floor this epic commits to.
const MINIMUM_SCENARIOS: usize = 24;
/// What the capture writes where the vibe home is machine-dependent.
const VIBE_HOME_PLACEHOLDER: &str = "{vibe_home}";

/// Reference fields this port does not declare, each with the reason. Each
/// counts as an unmeasured field in the census figures: an open gap of row 30.
///
/// The replay fails on any field the reference adds without an entry here,
/// and an entry whose field becomes declared fails it as stale.
const UNDECLARED_FIELDS: &[(&str, &str)] = &[
    ("agent_max_output_tokens", ADDED_AT_V3),
    ("api_stream_idle_timeout", ADDED_AT_V3),
    ("catalog_model_aliases", ADDED_AT_V3),
    ("catalog_recommended_model", ADDED_AT_V3),
    ("compaction_max_output_tokens", ADDED_AT_V3),
    ("compaction_reasoning_limit_tokens", ADDED_AT_V3),
    ("cursor_blink", ADDED_AT_V3),
    ("enable_auto_approve", ADDED_AT_V3),
    ("enable_background_processes", ADDED_AT_V3),
    ("enable_file_autocomplete", ADDED_AT_V3),
    ("enable_mcp", ADDED_AT_V3),
    ("enable_mouse_capture", ADDED_AT_V3),
    ("enable_subagents", ADDED_AT_V3),
    (
        "experimental_enable_document_library_connector",
        ADDED_AT_V3,
    ),
    ("experimental_enable_local_projects", ADDED_AT_V3),
    ("experimental_enable_mistral_mcp_connector", ADDED_AT_V3),
    ("experimental_enable_model_catalog", ADDED_AT_V3),
    ("experimental_enable_pr_discovery", ADDED_AT_V3),
    ("experimental_enable_pr_monitoring", ADDED_AT_V3),
    ("experimental_local_runtime_session_api", ADDED_AT_V3),
    ("max_concurrent_subagents", ADDED_AT_V3),
    ("max_subagents", ADDED_AT_V3),
    ("output_limit_retries", ADDED_AT_V3),
    ("outside_workdir", ADDED_AT_V3),
    ("strict_config", ADDED_AT_V3),
    ("title_prompt_id", ADDED_AT_V3),
];

/// Why the fields the v3 re-pin found are undeclared.
const ADDED_AT_V3: &str = "declared by the reference since v3.0.0 (`vibe/core/config/vibe_schema.py` \
                           at `c96a6db`); this port has no counterpart yet";

/// Reference fields that replaced an older name, as `(field, former name,
/// reason)`. Each must be proved by a `migrations` case in which the
/// reference migration moves a value from the former name to the field, and
/// counts in the census like an undeclared field until this port declares
/// it. It fails as stale when the corpus stops recording the move.
const RENAMED_FIELDS: &[(&str, &str, &str)] = &[(
    "worktree_copy_on_write",
    "experimental_worktree_copy_on_write",
    "v3.1.0 renamed the copy-on-write worktree switch and turned it on by default; \
     `_migrate_renamed_worktree_copy_on_write` (`vibe/core/config/_migration.py`) moves an \
     older value across. This port declares neither name",
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

/// Registry fields the reference no longer declares, each with the reason:
/// removed upstream, and still declared here.
///
/// The census fails on a registry field with no corpus entry; an entry here
/// names one the reference withdrew and this port still declares. It fails as
/// stale when the registry drops the field or the reference declares it again.
const WITHDRAWN_FIELDS: &[(&str, &str)] = &[
    (
        "tool_paths",
        "v3.0.0 stopped loading custom Python tools and withdrew the path list that named \
         them; `_migrate_removed_tool_paths` (`vibe/core/config/_migration.py`) drops it from \
         an older file, as the `withdrawn-path-list-is-dropped` migration case records",
    ),
    (
        "smart_approve_default",
        "v3.0.0 withdrew the switch that made smart-approve the default agent, in favor of \
         `default_agent` (`CHANGELOG.md` at `c96a6db`); no migration rewrites it, so the merge \
         ignores it, as the `withdrawn-default-agent-switch-is-left-to-the-merge` case records",
    ),
];

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

/// Pointers at which the document this port ships diverges from the reference
/// default document, under the scenario `defaults`. A key the port ships and
/// the reference does not is named at its own pointer.
/// Why a v3 model difference stands, shared by the entries it explains.
const LOCAL_WITHDRAWN: &str = "v3.0.0 withdrew the `local` default model (`_migrate_removed_local`, \
                               `vibe/core/config/_migration.py`); this port still ships it, so it \
                               lists, selects and resolves an alias the reference no longer has";
const LOCAL_SPARSE: &str = "with no shipped `local` entry left, the reference keeps an operator's \
                            `local` entry as written; this port completes it from the one it ships";
const LOCAL_WARNING: &str = "the reference warns about a layer that names the withdrawn `local` model \
                             or an alias it no longer resolves; this port, which still ships `local`, \
                             has nothing to warn about";
const DEFAULT_WINDOW: &str = "v3 declares the default model's context window, so the model entry and \
                              the `max_context_length` settings row carry it; this port declares none";
const DEFAULT_LEVELS: &str = "v3 narrows the default model's thinking levels to two; this port still \
                              offers five";
const UNSET_THRESHOLD: &str = "v3 leaves an unset compaction threshold unset on the model and in its \
                               settings row, where this port resolves the global default onto it";
const UNSET_SENTINEL: &str = "v3 leaves an unset compaction threshold unset, where this port writes the \
                              `-1` sentinel older clients wrote";
const WINDOW_RATIO: &str = "v3 no longer derives a model's compaction threshold from its context \
                            window; this port still takes a share of the window";
const UNSET_IMAGES: &str = "v3 leaves image support unset on a model that does not declare it, where \
                            this port records `false`";
const STRICT_THRESHOLD: &str = "v3 refuses a compaction threshold of zero or below, which the stack \
                                declares; this port accepts it";
const EMPTY_ALLOWLIST: &str = "the admin allowlist names only the withdrawn `local` model's name, so \
                               the v3 reference refuses a stack it leaves without a model; this port, \
                               which still ships `local`, accepts it";

const DEFAULT_DIVERGENCES: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "defaults",
        pointer: "/models/local",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: LOCAL_WITHDRAWN,
    },
    PatternDivergence {
        scenario: "defaults",
        pointer: "/models/mistral-medium-3.5/max_context_length",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: DEFAULT_WINDOW,
    },
    PatternDivergence {
        scenario: "defaults",
        pointer: "/models/mistral-medium-3.5/thinking_levels",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: DEFAULT_LEVELS,
    },
    PatternDivergence {
        scenario: "defaults",
        pointer: "/providers/*/preserve_api_base",
        count: 2,
        row: ROW,
        class: Class::Gap,
        reason: "v3 adds a provider switch that keeps a custom API base as written \
                 (`ProviderConfig`, `vibe/core/config/models.py`); this port has no such key",
    },
    PatternDivergence {
        scenario: "defaults",
        pointer: "/show_thinking_nodes",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: "v3 shows thinking nodes by default; this port still hides them",
    },
];

/// Pointers at which a merge scenario diverges. A field this port composes and
/// the reference does not is named at its own pointer, and a key the two sides
/// drop differently under `/droppedKeys`.
const SCENARIO_DIVERGENCES: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "concat-every-path-list",
        pointer: "/tool_paths",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: "`tool_paths` is withdrawn upstream (see `WITHDRAWN_FIELDS`), so the reference merge \
                 drops it; this port still declares and composes it",
    },
    PatternDivergence {
        scenario: "concat-every-path-list",
        pointer: "/droppedKeys/tool_paths",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: "as above: the reference drops the key the port composes",
    },
];

/// Pointers at which a model scenario diverges. `/active_model` and
/// `/validation_warnings` name the validated alias and the warning count;
/// `/fields` names the settings rows; every other pointer lies under the model
/// field it names.
const MODEL_SCENARIO_DIVERGENCES: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "models-*",
        pointer: "/models/local",
        count: 37,
        row: ROW,
        class: Class::Gap,
        reason: LOCAL_WITHDRAWN,
    },
    PatternDivergence {
        scenario: "models-routed-*",
        pointer: "/models/local/*",
        count: 9,
        row: ROW,
        class: Class::Gap,
        reason: LOCAL_SPARSE,
    },
    PatternDivergence {
        scenario: "models-*",
        pointer: "/available_models",
        count: 33,
        row: ROW,
        class: Class::Gap,
        reason: LOCAL_WITHDRAWN,
    },
    PatternDivergence {
        scenario: "models-allowed-models-match-the-model-name",
        pointer: "/available_models/0",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: LOCAL_WITHDRAWN,
    },
    PatternDivergence {
        scenario: "models-allowed-models-match-the-model-name",
        pointer: "/default_model_alias",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: LOCAL_WITHDRAWN,
    },
    PatternDivergence {
        scenario: "models-*",
        pointer: "/active_model",
        count: 5,
        row: ROW,
        class: Class::Gap,
        reason: LOCAL_WITHDRAWN,
    },
    PatternDivergence {
        scenario: "models-*",
        pointer: "/active_model_alias",
        count: 4,
        row: ROW,
        class: Class::Gap,
        reason: LOCAL_WITHDRAWN,
    },
    PatternDivergence {
        scenario: "models-*",
        pointer: "/compaction_model_alias",
        count: 4,
        row: ROW,
        class: Class::Gap,
        reason: LOCAL_WITHDRAWN,
    },
    PatternDivergence {
        scenario: "models-*",
        pointer: "/fields/max_context_length/path",
        count: 4,
        row: ROW,
        class: Class::Gap,
        reason: LOCAL_WITHDRAWN,
    },
    PatternDivergence {
        scenario: "models-utility-models-resolve-an-alias-and-the-active-selector",
        pointer: "/utility_models/title",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: LOCAL_WITHDRAWN,
    },
    PatternDivergence {
        scenario: "models-*",
        pointer: "/validation_warnings",
        count: 10,
        row: ROW,
        class: Class::Gap,
        reason: LOCAL_WARNING,
    },
    PatternDivergence {
        scenario: "models-*",
        pointer: "/models/mistral-medium-3.5/max_context_length",
        count: 39,
        row: ROW,
        class: Class::Gap,
        reason: DEFAULT_WINDOW,
    },
    PatternDivergence {
        scenario: "models-*",
        pointer: "/fields/max_context_length/value",
        count: 32,
        row: ROW,
        class: Class::Gap,
        reason: DEFAULT_WINDOW,
    },
    PatternDivergence {
        scenario: "models-*",
        pointer: "/fields/max_context_length/layerValues/0",
        count: 32,
        row: ROW,
        class: Class::Gap,
        reason: DEFAULT_WINDOW,
    },
    PatternDivergence {
        scenario: "models-*",
        pointer: "/models/mistral-medium-3.5/thinking_levels",
        count: 39,
        row: ROW,
        class: Class::Gap,
        reason: DEFAULT_LEVELS,
    },
    PatternDivergence {
        scenario: "models-window-derives-the-threshold",
        pointer: "/models/windowed/auto_compact_threshold",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: WINDOW_RATIO,
    },
    PatternDivergence {
        scenario: "models-window-derives-the-threshold",
        pointer: "/models/odd-window/auto_compact_threshold",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: WINDOW_RATIO,
    },
    PatternDivergence {
        scenario: "models-global-cap-at-the-window-falls-back-to-the-ratio",
        pointer: "/models/windowed/auto_compact_threshold",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: WINDOW_RATIO,
    },
    PatternDivergence {
        scenario: "models-routed-definition-keeps-the-unset-threshold",
        pointer: "/models/routed/auto_compact_threshold",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: WINDOW_RATIO,
    },
    PatternDivergence {
        scenario: "models-routed-definition-keeps-the-unset-threshold",
        pointer: "/fields/auto_compact_threshold/value",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: WINDOW_RATIO,
    },
    PatternDivergence {
        scenario: "models-*",
        pointer: "/models/*/auto_compact_threshold",
        count: 33,
        row: ROW,
        class: Class::Gap,
        reason: UNSET_THRESHOLD,
    },
    PatternDivergence {
        scenario: "models-*",
        pointer: "/fields/auto_compact_threshold/value",
        count: 27,
        row: ROW,
        class: Class::Gap,
        reason: UNSET_THRESHOLD,
    },
    PatternDivergence {
        scenario: "models-*",
        pointer: "/fields/auto_compact_threshold/layerValues/0",
        count: 27,
        row: ROW,
        class: Class::Gap,
        reason: UNSET_THRESHOLD,
    },
    PatternDivergence {
        scenario: "models-*",
        pointer: "/*/auto_compact_threshold",
        count: 10,
        row: ROW,
        class: Class::Gap,
        reason: UNSET_SENTINEL,
    },
    PatternDivergence {
        scenario: "models-*",
        pointer: "/routed_extra_models/*/auto_compact_threshold",
        count: 4,
        row: ROW,
        class: Class::Gap,
        reason: UNSET_SENTINEL,
    },
    PatternDivergence {
        scenario: "models-*",
        pointer: "/models/*/supports_images",
        count: 16,
        row: ROW,
        class: Class::Gap,
        reason: UNSET_IMAGES,
    },
    PatternDivergence {
        scenario: "models-*",
        pointer: "/*/supports_images",
        count: 8,
        row: ROW,
        class: Class::Gap,
        reason: UNSET_IMAGES,
    },
    PatternDivergence {
        scenario: "models-*",
        pointer: "/routed_extra_models/*/supports_images",
        count: 3,
        row: ROW,
        class: Class::Gap,
        reason: UNSET_IMAGES,
    },
    PatternDivergence {
        scenario: "models-cap-the-window-cannot-reach-is-ignored",
        pointer: "/rejected",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: STRICT_THRESHOLD,
    },
    PatternDivergence {
        scenario: "models-a-hand-written-unset-threshold-is-unset",
        pointer: "/rejected",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: STRICT_THRESHOLD,
    },
    PatternDivergence {
        scenario: "models-global-threshold-of-zero-disables-every-model",
        pointer: "/rejected",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: STRICT_THRESHOLD,
    },
    PatternDivergence {
        scenario: "models-admin-allowlist-narrows-by-name",
        pointer: "/rejected",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: EMPTY_ALLOWLIST,
    },
];

/// Settings rows the two sides list differently, under the scenario
/// `settingsOrder`, each at `/<row>`: a row only one side lists. The rows both
/// list are compared for their order.
/// Why a row this port lists is absent from the reference listing.
const DEPRECATED_ROW: &str = "v3 marks the field deprecated, and `config/fields/read` skips a \
                              deprecated field (`vibe/app_server/_config_introspect.py`); this \
                              port still lists it";
const SETTINGS_ORDER_DIVERGENCES: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "settingsOrder",
        pointer: "/displayed_workdir",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: DEPRECATED_ROW,
    },
    PatternDivergence {
        scenario: "settingsOrder",
        pointer: "/context_warnings",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: DEPRECATED_ROW,
    },
    PatternDivergence {
        scenario: "settingsOrder",
        pointer: "/raise_on_compaction_failure",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: DEPRECATED_ROW,
    },
    PatternDivergence {
        scenario: "settingsOrder",
        pointer: "/include_model_info",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: DEPRECATED_ROW,
    },
    PatternDivergence {
        scenario: "settingsOrder",
        pointer: "/include_prompt_detail",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: DEPRECATED_ROW,
    },
    PatternDivergence {
        scenario: "settingsOrder",
        pointer: "/api_connect_timeout",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: DEPRECATED_ROW,
    },
    PatternDivergence {
        scenario: "settingsOrder",
        pointer: "/api_write_timeout",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: DEPRECATED_ROW,
    },
    PatternDivergence {
        scenario: "settingsOrder",
        pointer: "/api_pool_timeout",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: DEPRECATED_ROW,
    },
];

/// Pointers at which a field migration leaves another document, or another
/// verdict on whether it changed anything, than the reference migration.
/// Why a field migration differs.
const RENAME_MIGRATION: &str = "this port has no `_migrate_renamed_worktree_copy_on_write`, so a \
                                document written with the former name keeps it";
const TOOL_PATHS_MIGRATION: &str = "this port has no `_migrate_removed_tool_paths`, so an older \
                                    document keeps the withdrawn path list";
const MIGRATION_DIVERGENCES: &[PatternDivergence] = &[
    PatternDivergence {
        scenario: "renamed-field-*",
        pointer: "/changed",
        count: 2,
        row: ROW,
        class: Class::Gap,
        reason: RENAME_MIGRATION,
    },
    PatternDivergence {
        scenario: "renamed-field-moves-its-value",
        pointer: "/migrated/worktree_copy_on_write",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: RENAME_MIGRATION,
    },
    PatternDivergence {
        scenario: "renamed-field-*",
        pointer: "/migrated/experimental_worktree_copy_on_write",
        count: 2,
        row: ROW,
        class: Class::Gap,
        reason: RENAME_MIGRATION,
    },
    PatternDivergence {
        scenario: "withdrawn-path-list-is-dropped",
        pointer: "/changed",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: TOOL_PATHS_MIGRATION,
    },
    PatternDivergence {
        scenario: "withdrawn-path-list-is-dropped",
        pointer: "/migrated/tool_paths",
        count: 1,
        row: ROW,
        class: Class::Gap,
        reason: TOOL_PATHS_MIGRATION,
    },
];

/// One divergence a replay observed: the case, the pointer, and both values
/// rendered with sensitive values redacted.
#[derive(Debug)]
pub(super) struct Observed {
    pub(super) case: String,
    pub(super) pointer: String,
    pub(super) reference: String,
    pub(super) port: String,
}

/// What a typed ledger made of one family's divergences.
#[derive(Debug, Default)]
pub(super) struct Settled {
    /// Differences an accepted divergence covers.
    accepted: usize,
    /// Differences an open gap covers.
    gaps: usize,
    /// The cases at least one gap covers a difference of.
    gapped_cases: BTreeSet<String>,
}

impl Settled {
    /// The family's figures over `total` cases, `conformant` of which replayed
    /// without a difference: a case that differs only where an accepted
    /// divergence says it may is accepted, one a gap covers is not.
    pub(super) fn figures(&self, conformant: usize, total: usize) -> Figures {
        let differing = total.saturating_sub(conformant);
        Figures {
            conformant,
            accepted: differing.saturating_sub(self.gapped_cases.len()),
            total,
            accepted_differences: self.accepted,
            gap_differences: self.gaps,
            prose_differences: 0,
        }
    }
}

/// Settles every observed divergence against `ledger`, the first entry that
/// covers one accounting for it. Fails on a divergence no entry covers, on an
/// entry that covers another number of divergences than it declares (a gap
/// that closed or widened), and on an entry `assert_ledger` finds unsound.
pub(super) fn reconcile(
    family: &str,
    observed: &[Observed],
    ledger: &[PatternDivergence],
) -> Settled {
    assert_ledger(LISTED, &BTreeMap::new(), entries(ledger));
    let mut counted = vec![0_usize; ledger.len()];
    let mut settled = Settled::default();
    let mut unrecorded = Vec::new();
    for found in observed {
        let Some(index) = ledger
            .iter()
            .position(|entry| entry.covers(&found.case, &found.pointer))
        else {
            unrecorded.push(format!(
                "{}: {}: reference {}, port {}",
                found.case, found.pointer, found.reference, found.port
            ));
            continue;
        };
        counted[index] += 1;
        match ledger[index].class {
            Class::Gap => {
                settled.gaps += 1;
                settled.gapped_cases.insert(found.case.clone());
            }
            Class::Accepted(_) => settled.accepted += 1,
        }
    }
    assert!(
        unrecorded.is_empty(),
        "{family} diverges where no ledger entry records it:\n{}",
        unrecorded.join("\n")
    );
    let moved = ledger
        .iter()
        .zip(&counted)
        .filter(|(entry, counted)| entry.count != **counted)
        .map(|(entry, counted)| {
            format!(
                "{}: declared {}, counted {counted}",
                entry.label(),
                entry.count
            )
        })
        .collect::<Vec<_>>();
    assert!(
        moved.is_empty(),
        "{family}: these ledger entries cover another number of differences than they \
         declare:\n{}",
        moved.join("\n")
    );
    settled
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
    /// The rows `config/fields/read` lists over the shipped defaults, in order.
    settings_order: Vec<String>,
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
    /// Documents an older client wrote, and what the reference migration
    /// leaves of each.
    migrations: Vec<MigrationCase>,
}

/// One document an older client wrote, as the reference migration left it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationCase {
    name: String,
    toml: String,
    changed: bool,
    migrated: Map<String, JsonValue>,
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
    /// The validated `routed_model_config`, a field of its own beside the copy
    /// injected into `models`.
    routed_model_config: Option<JsonValue>,
    /// The alias a compaction runs on, reference `get_compaction_model`.
    compaction_model_alias: Option<String>,
    /// Per `[utility_models]` key, the alias reference `get_utility_model`
    /// resolves, or `null` for automatic selection.
    utility_models: Option<BTreeMap<String, Option<String>>>,
    /// The `config/fields/read` rows some layer above the defaults sets, and
    /// the two read off the active model, without their prose.
    fields: Option<Vec<JsonValue>>,
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
    assert!(
        is_pin_commit(&corpus.reference.commit),
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
    let renamed = RENAMED_FIELDS
        .iter()
        .map(|(field, former, _)| (*field, *former))
        .collect::<BTreeMap<_, _>>();
    let mut missing = Vec::new();
    for field in &corpus.fields {
        let Some(spec) = declared.get(field.name.as_str()) else {
            if !recorded.contains_key(&field.name) && !renamed.contains_key(field.name.as_str()) {
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
    for (field, former) in &renamed {
        assert!(
            corpus.fields.iter().any(|entry| entry.name == *field)
                && !corpus.fields.iter().any(|entry| entry.name == *former),
            "the reference no longer declares `{field}` in place of `{former}`; its recorded \
             rename is stale"
        );
        assert!(
            corpus
                .migrations
                .iter()
                .any(|case| moves_a_value(case, former, field)),
            "no migration case records the reference moving `{former}` to `{field}`"
        );
    }
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
        .filter(|field| {
            recorded.contains_key(&field.name)
                || (renamed.contains_key(field.name.as_str())
                    && !declared.contains_key(field.name.as_str()))
        })
        .count();
    println!(
        "config surface: {}/{} reference fields declared, {} local, {} recorded undeclared \
         ({} of them renamed), {} withdrawn upstream, at {}",
        corpus.fields.len() - undeclared,
        corpus.fields.len(),
        local.len(),
        undeclared,
        renamed.len(),
        retired.len(),
        &corpus.reference.commit[..12]
    );
    // A reference field this port does not declare is an open gap.
    assert_figures(
        "config surface parity (fields):",
        "crates/vibe-core/tests/config-surface/corpus.json",
        Figures {
            conformant: corpus.fields.len() - undeclared,
            accepted: 0,
            total: corpus.fields.len(),
            accepted_differences: 0,
            gap_differences: undeclared,
            prose_differences: 0,
        },
        serde_json::json!({}),
    );
}

/// Whether the reference migration, in `case`, moved the value `former` held
/// to `field` and dropped `former`.
fn moves_a_value(case: &MigrationCase, former: &str, field: &str) -> bool {
    let Ok(written) = case.toml.parse::<Table>() else {
        return false;
    };
    let Some(value) = written.get(former) else {
        return false;
    };
    !written.contains_key(field)
        && !case.migrated.contains_key(former)
        && case.migrated.get(field) == serde_json::to_value(value).ok().as_ref()
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
    let actual =
        as_written_by_create_default_config(&snapshot.effective, &registry::default_document());

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
        if recorded.contains_key(key.as_str())
            || RENAMED_FIELDS.iter().any(|(field, ..)| field == key)
        {
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
    for (key, value) in actual
        .as_object()
        .expect("a document is a table")
        .iter()
        .filter(|(key, _)| !corpus.defaults.document.contains_key(key.as_str()))
        .filter(|(key, _)| !retired.contains_key(key.as_str()))
    {
        let value = value.clone();
        observed.push(Observed {
            case: "defaults".to_owned(),
            pointer: format!("/{}", escape_pointer_token(key)),
            reference: "absent".to_owned(),
            port: render(key, &value),
        });
    }
    reconcile(
        "the shipped default document",
        &observed,
        DEFAULT_DIVERGENCES,
    );
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

/// The settings rows that carry an observation, as the capture keeps them:
/// every row some layer above the defaults sets, without its prose, and the two
/// read off the active model with their values.
fn settings_rows(snapshot: &ConfigSnapshot) -> Vec<JsonValue> {
    const MODEL_SCOPED: [&str; 2] = ["auto_compact_threshold", "max_context_length"];
    introspect::describe_fields(snapshot)
        .into_iter()
        .filter_map(|field| {
            let layers = field
                .layer_values
                .iter()
                .map(|entry| entry.layer)
                .collect::<Vec<_>>();
            let mut row = serde_json::json!({
                "name": field.name,
                "path": field.path,
                "layers": layers,
            });
            if MODEL_SCOPED.contains(&field.name) {
                row["value"] = field.value;
                row["layerValues"] = field
                    .layer_values
                    .into_iter()
                    .map(|entry| entry.value)
                    .collect();
            } else if layers == ["default"] {
                return None;
            }
            Some(row)
        })
        .collect()
}

/// Settings rows keyed by name, so a divergence names the row it lies in.
fn settings_rows_by_name(rows: Vec<JsonValue>) -> JsonValue {
    JsonValue::Object(
        rows.into_iter()
            .map(|row| {
                let name = row["name"].as_str().unwrap_or_default().to_owned();
                (name, row)
            })
            .collect(),
    )
}

/// `config/fields/read` lists the same rows in the same order: every published
/// field the settings screen offers, then the active model's context window.
#[test]
fn the_settings_rows_are_listed_in_the_reference_order() {
    let corpus = corpus();
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    fs::create_dir_all(&home).expect("home directory");
    let snapshot = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: temporary.path().join("project"),
        },
        registry::default_document(),
    )
    .load()
    .expect("the shipped defaults load on their own");
    // The four keys only this port declares are published like any other; the
    // ledger row `4 configuration keys with no upstream counterpart` keeps them.
    let rows = introspect::describe_fields(&snapshot)
        .into_iter()
        .filter(|field| registry::field(field.name).is_none_or(|spec| !spec.local))
        .map(|field| field.name.to_owned())
        .collect::<Vec<_>>();
    // A reference row this port does not declare is the census's to name, and
    // a row it withdrew while this port still declares it is too.
    let unmeasured = UNDECLARED_FIELDS
        .iter()
        .map(|(name, _)| *name)
        .chain(RENAMED_FIELDS.iter().map(|(name, ..)| *name))
        .collect::<BTreeSet<_>>();
    let retired = ledger(WITHDRAWN_FIELDS);
    let reference = corpus
        .settings_order
        .iter()
        .filter(|name| !unmeasured.contains(name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let rows = rows
        .into_iter()
        .filter(|name| !retired.contains_key(name))
        .collect::<Vec<_>>();
    let mut observed = Vec::new();
    for name in rows.iter().filter(|name| !reference.contains(name)) {
        observed.push(Observed {
            case: "settingsOrder".to_owned(),
            pointer: format!("/{}", escape_pointer_token(name)),
            reference: "absent".to_owned(),
            port: "listed".to_owned(),
        });
    }
    for name in reference.iter().filter(|name| !rows.contains(name)) {
        observed.push(Observed {
            case: "settingsOrder".to_owned(),
            pointer: format!("/{}", escape_pointer_token(name)),
            reference: "listed".to_owned(),
            port: "absent".to_owned(),
        });
    }
    reconcile("the settings rows", &observed, SETTINGS_ORDER_DIVERGENCES);
    let shared = |listed: &[String], other: &[String]| {
        listed
            .iter()
            .filter(|name| other.contains(name))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        shared(&rows, &reference),
        shared(&reference, &rows),
        "the rows both sides list come in another order"
    );
}

/// What reference `create_default_config` writes for a loaded document: it
/// dumps the schema's declaration without running the model validators, and
/// drops the unset compaction threshold on the way out (`vibe_schema.py:1119`,
/// `models.py:563`). The load runs the validators, which resolve a threshold
/// on every model; the document shows the one the Defaults layer declares, so
/// that is what the projection reads, and the resolved ones are replayed by the
/// `models-defaults-only` model scenario.
fn as_written_by_create_default_config(effective: &Table, declared: &Table) -> JsonValue {
    let mut document = serde_json::to_value(effective).expect("effective serializes");
    let unset = JsonValue::from(registry::UNSET_AUTO_COMPACT_THRESHOLD);
    if document.get("auto_compact_threshold") == Some(&unset) {
        document
            .as_object_mut()
            .expect("a document is a table")
            .remove("auto_compact_threshold");
    }
    let declared_threshold = |alias: &str| {
        declared
            .get("models")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_table)
            .find(|entry| entry.get("alias").and_then(Value::as_str) == Some(alias))
            .and_then(|entry| entry.get("auto_compact_threshold"))
            .and_then(Value::as_integer)
            .filter(|threshold| *threshold != registry::UNSET_AUTO_COMPACT_THRESHOLD)
    };
    if let Some(models) = document
        .get_mut("models")
        .and_then(JsonValue::as_object_mut)
    {
        for (alias, entry) in models.iter_mut() {
            let Some(entry) = entry.as_object_mut() else {
                continue;
            };
            match declared_threshold(alias) {
                Some(threshold) => {
                    entry.insert("auto_compact_threshold".to_owned(), threshold.into());
                }
                None => {
                    entry.remove("auto_compact_threshold");
                }
            }
        }
    }
    document
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
        compare(
            "/routed_model_config",
            expected(&scenario.routed_model_config),
            field("routed_model_config"),
        );
        // The runtime reads the compaction model off the settings a session
        // is built with, so that is what is compared.
        compare(
            "/compaction_model_alias",
            text(scenario.compaction_model_alias.as_deref()),
            text(
                snapshot
                    .compaction_settings()
                    .compaction_model_alias
                    .as_deref(),
            ),
        );
        let routing = ModelRouting::from_effective(effective, snapshot.active_model_alias());
        compare(
            "/utility_models",
            serde_json::to_value(scenario.utility_models.clone().unwrap_or_default())
                .expect("utility models serialize"),
            JsonValue::Object(
                UtilityFeature::ALL
                    .into_iter()
                    .map(|feature| {
                        let alias = routing
                            .utility_model(feature)
                            .ok()
                            .flatten()
                            .map(|model| model.alias);
                        (feature.key().to_owned(), text(alias.as_deref()))
                    })
                    .collect(),
            ),
        );
        compare(
            "/fields",
            settings_rows_by_name(scenario.fields.clone().unwrap_or_default()),
            settings_rows_by_name(settings_rows(&snapshot)),
        );
    }
    let settled = reconcile(
        "the validated model scenarios",
        &observed,
        MODEL_SCENARIO_DIVERGENCES,
    );
    assert!(
        unpinned_observed,
        "no model scenario leaves `active_model` unpinned; drop the recorded \
         divergence instead of carrying it"
    );
    let conformant = conforming(
        corpus.model_scenarios.iter().map(|scenario| &scenario.name),
        &observed,
    );
    println!(
        "config surface: {}/{} model scenarios conform at {}",
        conformant,
        corpus.model_scenarios.len(),
        &corpus.reference.commit[..12]
    );
    // Every observed divergence passed `reconcile`, so it is a recorded one.
    assert_figures(
        "config surface parity (models):",
        LISTED,
        settled.figures(conformant, corpus.model_scenarios.len()),
        serde_json::json!({}),
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
    crate::parity::manifest::assert_corpus("crates/vibe-core/tests/config-surface/corpus.json");
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
    let settled = reconcile("the merge scenarios", &observed, SCENARIO_DIVERGENCES);
    let conformant = conforming(
        corpus.scenarios.iter().map(|scenario| &scenario.name),
        &observed,
    );
    println!(
        "config surface: {}/{} merge scenarios conform at {}",
        conformant,
        corpus.scenarios.len(),
        &corpus.reference.commit[..12]
    );
    assert_figures(
        "config surface parity (merges):",
        LISTED,
        settled.figures(conformant, corpus.scenarios.len()),
        serde_json::json!({}),
    );
}

/// A document an older client wrote is brought forward as the reference
/// brings it forward: a renamed field's value moves to its new name, and a
/// withdrawn field is dropped or left for the merge to ignore.
#[test]
fn every_field_migration_leaves_the_document_the_reference_leaves() {
    let corpus = corpus();
    assert!(!corpus.migrations.is_empty());
    let mut observed = Vec::new();
    for case in &corpus.migrations {
        let mut document = case
            .toml
            .parse::<Table>()
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let changed = super::migration::migrate_document(&mut document);
        observe(
            &case.name,
            "/changed",
            &JsonValue::from(case.changed),
            &JsonValue::from(changed),
            &mut observed,
        );
        observe(
            &case.name,
            "/migrated",
            &JsonValue::Object(case.migrated.clone()),
            &serde_json::to_value(&document).expect("a migrated document serializes"),
            &mut observed,
        );
    }
    let settled = reconcile("the field migrations", &observed, MIGRATION_DIVERGENCES);
    let conformant = conforming(corpus.migrations.iter().map(|case| &case.name), &observed);
    println!(
        "config surface: {}/{} field migrations conform at {}",
        conformant,
        corpus.migrations.len(),
        &corpus.reference.commit[..12]
    );
    assert_figures(
        "config surface parity (migrations):",
        LISTED,
        settled.figures(conformant, corpus.migrations.len()),
        serde_json::json!({}),
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

    // A field this port composes and the reference does not is a divergence
    // at its own pointer.
    for key in effective
        .keys()
        .filter(|key| registry::field(key).is_some())
        .filter(|key| !scenario.merged.contains_key(key.as_str()))
    {
        observed.push(Observed {
            case: scenario.name.clone(),
            pointer: format!("/{}", escape_pointer_token(key)),
            reference: "absent".to_owned(),
            port: render(key, &actual[key.as_str()]),
        });
    }

    // The keys the reference drops are the ones this port preserves without
    // registering, which FR-04 requires; a key one side drops and the other
    // does not is a divergence under `/droppedKeys`.
    let preserved = snapshot.unregistered_keys();
    for key in scenario
        .dropped_keys
        .iter()
        .filter(|key| !preserved.contains(&key.as_str()))
    {
        observed.push(Observed {
            case: scenario.name.clone(),
            pointer: format!("/droppedKeys/{}", escape_pointer_token(key)),
            reference: "dropped".to_owned(),
            port: "composed".to_owned(),
        });
    }
    for key in preserved
        .iter()
        .filter(|key| !scenario.dropped_keys.iter().any(|dropped| dropped == *key))
    {
        observed.push(Observed {
            case: scenario.name.clone(),
            pointer: format!("/droppedKeys/{}", escape_pointer_token(key)),
            reference: "composed".to_owned(),
            port: "dropped".to_owned(),
        });
    }
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

#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let Some(root) = pinned_reference("configuration oracle") else {
        return;
    };
    let temporary = tempfile::tempdir().expect("temporary root");
    let recaptured = temporary.path().join("corpus.json");
    let capture = Capture {
        script: CAPTURE_SCRIPT,
        family: &[],
        arguments: &[],
    };
    capture.recapture(
        &root,
        &["--output".as_ref(), recaptured.as_os_str()],
        &recaptured,
        &corpus_path(),
    );
}
