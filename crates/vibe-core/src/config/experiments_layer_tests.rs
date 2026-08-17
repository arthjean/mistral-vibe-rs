//! The variant-to-field mapping, and the seat the mapped document composes at.
//!
//! The `configMapping` and `layerPrecedence` families of
//! [`experiments_parity_tests`](crate::experiments_parity_tests) hold both
//! against the reference's own answers. This module covers what a corpus case
//! cannot: the refusal a write through the layer meets, and the provenance a
//! snapshot reports for an assigned value.

use std::collections::BTreeMap;

use super::registry::default_document;
use super::*;

/// A prompt resolution that accepts the identifiers this fixture declares and
/// refuses everything else, standing where a session hands its own resolver.
fn resolves(known: &'static [&'static str]) -> impl Fn(&str) -> bool {
    move |prompt_id: &str| known.contains(&prompt_id)
}

fn variants(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

/// The layer one variant set composes, with `tests` and `lean` resolvable.
fn layer(entries: &[(&str, &str)]) -> ExperimentsLayer {
    let known = resolves(&["cli", "tests", "lean"]);
    ExperimentsLayer::from_variants(&variants(entries), &known)
}

const SYSTEM_PROMPT: &str = "vibe_cli_system_prompt";
const MANAGED_SHELL: &str = "vibe_cli_managed_shell_tools";
const MODEL_ROUTING: &str = "vibe_cli_default_routing_model";

// --------------------------------------------------------------------------
// US-009: the mapping
// --------------------------------------------------------------------------

#[test]
fn a_resolvable_prompt_variant_is_written_and_an_unresolvable_one_is_not() {
    let written = layer(&[(SYSTEM_PROMPT, "tests")]);
    assert_eq!(written.values()["system_prompt_id"].as_str(), Some("tests"));
    assert!(written.fingerprint().is_some());

    for refused in ["removed_after_graduation", "", "../escape"] {
        let dropped = layer(&[(SYSTEM_PROMPT, refused)]);
        assert!(
            dropped.values().is_empty(),
            "`{refused}` reached the document"
        );
        assert!(dropped.fingerprint().is_none());
    }
}

#[test]
fn a_routing_variant_writes_the_alias_and_re_encodes_the_definition() {
    let mapped = layer(&[(
        MODEL_ROUTING,
        r#"{"active_model": "routed", "model_config": {"name": "vibe-routed", "provider": "mistral", "alias": "routed"}}"#,
    )]);

    assert_eq!(
        mapped.values()["routed_default_model"].as_str(),
        Some("routed")
    );
    // The definition is re-encoded the way the proxy wrote it: keys in their
    // original order and Python's own separators, so the text the layer writes
    // is the text the reference writes.
    assert_eq!(
        mapped.values()["routed_model_config"].as_str(),
        Some(r#"{"name": "vibe-routed", "provider": "mistral", "alias": "routed"}"#)
    );
}

#[test]
fn a_routing_variant_that_carries_no_usable_alias_writes_nothing() {
    for refused in [
        "not json at all",
        "[1, 2]",
        "\"a string\"",
        "{}",
        r#"{"a": 1}"#,
        r#"{"active_model": ""}"#,
        r#"{"active_model": 7}"#,
    ] {
        let dropped = layer(&[(MODEL_ROUTING, refused)]);
        assert!(dropped.values().is_empty(), "`{refused}` wrote a field");
        assert!(dropped.fingerprint().is_none());
    }
}

/// A definition that is not an object is dropped on its own: the alias beside
/// it is still usable and is still written.
#[test]
fn a_routing_definition_that_is_not_an_object_leaves_the_alias_standing() {
    let mapped = layer(&[(
        MODEL_ROUTING,
        r#"{"active_model": "routed", "model_config": []}"#,
    )]);

    assert_eq!(
        mapped.values()["routed_default_model"].as_str(),
        Some("routed")
    );
    assert!(!mapped.values().contains_key("routed_model_config"));
}

#[test]
fn only_the_managed_arm_enables_the_managed_shell_family() {
    assert_eq!(
        layer(&[(MANAGED_SHELL, "managed")]).values()["managed_shell_tools_enabled"].as_bool(),
        Some(true)
    );
    for refused in ["legacy", "something-else", ""] {
        assert!(
            layer(&[(MANAGED_SHELL, refused)]).values().is_empty(),
            "`{refused}` wrote the toggle"
        );
    }
}

#[test]
fn a_variant_set_that_maps_to_nothing_composes_an_empty_layer() {
    for entries in [
        Vec::new(),
        vec![("vibe_cli_unknown_rollout", "on")],
        vec![
            (SYSTEM_PROMPT, "removed_after_graduation"),
            (MANAGED_SHELL, "legacy"),
        ],
    ] {
        let empty = layer(&entries);
        assert!(empty.values().is_empty(), "{entries:?} wrote a field");
        assert!(
            empty.fingerprint().is_none(),
            "{entries:?} carried a fingerprint"
        );
    }
}

/// The fingerprint stands for the contents: two layers carrying the same
/// document carry the same token, and a different document a different one.
#[test]
fn the_fingerprint_follows_the_document_it_stands_for() {
    let first = layer(&[(SYSTEM_PROMPT, "tests")]);
    let same = layer(&[(SYSTEM_PROMPT, "tests")]);
    let other = layer(&[(SYSTEM_PROMPT, "lean")]);

    assert_eq!(first.fingerprint(), same.fingerprint());
    assert_ne!(first.fingerprint(), other.fingerprint());
}

/// US-009: a write is never routed to this layer.
///
/// The reference refuses one by raising from `GrowthbookLayer._save_to_store`.
/// Here the refusal is structural: every write is addressed to a
/// [`ConfigTarget`], and none of the three names the experiments layer, so the
/// attempt has no method to reach. The match is exhaustive, so a target added
/// for this layer stops compiling rather than silently opening a write path.
#[test]
fn no_write_target_addresses_the_experiments_layer() {
    for target in [
        ConfigTarget::User,
        ConfigTarget::Project,
        ConfigTarget::Ephemeral,
    ] {
        let addresses_the_layer = match target {
            ConfigTarget::User | ConfigTarget::Project | ConfigTarget::Ephemeral => false,
        };
        assert!(
            !addresses_the_layer,
            "{target:?} would route a write into the experiments layer"
        );
    }
}

// --------------------------------------------------------------------------
// US-010: the seat
// --------------------------------------------------------------------------

/// A load over the shipped defaults with a user file, an optional trusted
/// project file, environment variables, runtime overrides and an assignment.
fn composed(
    user: &str,
    project: Option<&str>,
    environment: &[(&str, &str)],
    overrides: &str,
    assigned: &[(&str, &str)],
) -> ConfigSnapshot {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    let working = temporary.path().join("project");
    fs::create_dir_all(&home).expect("home directory");
    fs::create_dir_all(working.join(PROJECT_DIRECTORY)).expect("project directory");
    if !user.is_empty() {
        fs::write(home.join(CONFIG_FILE), user).expect("user fixture");
    }
    if let Some(document) = project {
        fs::write(working.join(PROJECT_DIRECTORY).join(CONFIG_FILE), document)
            .expect("project fixture");
    }
    let known = resolves(&["cli", "tests", "lean", "minimal"]);
    LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: working,
        },
        default_document(),
    )
    .with_project_trusted(true)
    .with_environment(
        environment
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
    )
    .with_runtime_overrides(overrides.parse::<Table>().expect("override fixture"))
    .with_experiment_variants(&variants(assigned), &known)
    .load()
    .expect("the assignment composes")
}

#[test]
fn a_value_the_user_wrote_beats_an_assignment() {
    let snapshot = composed(
        "system_prompt_id = \"lean\"\n",
        None,
        &[],
        "",
        &[(SYSTEM_PROMPT, "tests")],
    );

    assert_eq!(
        snapshot.effective["system_prompt_id"].as_str(),
        Some("lean")
    );
}

#[test]
fn a_project_file_disabling_the_managed_shell_beats_an_assignment_enabling_it() {
    let snapshot = composed(
        "",
        Some("managed_shell_tools_enabled = false\n"),
        &[],
        "",
        &[(MANAGED_SHELL, "managed")],
    );

    assert_eq!(
        snapshot.effective["managed_shell_tools_enabled"].as_bool(),
        Some(false)
    );
}

#[test]
fn an_environment_variable_and_a_runtime_override_both_beat_an_assignment() {
    let environment = composed(
        "",
        None,
        &[("VIBE_SYSTEM_PROMPT_ID", "minimal")],
        "",
        &[(SYSTEM_PROMPT, "tests")],
    );
    assert_eq!(
        environment.effective["system_prompt_id"].as_str(),
        Some("minimal")
    );

    let runtime = composed(
        "",
        None,
        &[],
        "system_prompt_id = \"lean\"\n",
        &[(SYSTEM_PROMPT, "tests")],
    );
    assert_eq!(runtime.effective["system_prompt_id"].as_str(), Some("lean"));
}

#[test]
fn an_assignment_alone_is_effective_and_reports_the_experiments_layer() {
    let snapshot = composed("", None, &[], "", &[(SYSTEM_PROMPT, "tests")]);

    assert_eq!(
        snapshot.effective["system_prompt_id"].as_str(),
        Some("tests")
    );
    let assigned = snapshot
        .layer_values
        .iter()
        .find(|layer| layer.kind == ConfigLayerKind::Experiments)
        .expect("the experiments layer composes");
    assert_eq!(assigned.values["system_prompt_id"].as_str(), Some("tests"));
    // The settings surface reports provenance highest priority first, so the
    // assignment is named there rather than being mistaken for a default.
    let field = introspect::describe_fields(&snapshot)
        .into_iter()
        .find(|field| field.name == "system_prompt_id")
        .expect("the field is published");
    assert_eq!(
        field
            .layer_values
            .iter()
            .map(|entry| entry.layer)
            .collect::<Vec<_>>(),
        [ConfigLayerKind::Experiments, ConfigLayerKind::Defaults]
    );
}

/// The routed alias an assignment carries reaches an unpinned installation, and
/// loses to a pinned one, which is the same precedence read through the model
/// resolution rather than through a field.
#[test]
fn a_routing_assignment_selects_the_model_only_for_an_unpinned_operator() {
    let definition = concat!(
        r#"{"active_model": "routed", "model_config": {"name": "vibe-routed", "#,
        r#""provider": "mistral", "alias": "routed"}}"#
    );

    let unpinned = composed("", None, &[], "", &[(MODEL_ROUTING, definition)]);
    assert_eq!(unpinned.effective["active_model"].as_str(), Some(""));
    assert_eq!(unpinned.active_model_alias(), Some("routed"));

    let pinned = composed(
        "active_model = \"mistral-medium-3.5\"\n",
        None,
        &[],
        "",
        &[(MODEL_ROUTING, definition)],
    );
    assert_eq!(pinned.active_model_alias(), Some("mistral-medium-3.5"));
    assert_eq!(
        default_model_alias(&pinned.effective),
        Some("mistral-medium-3.5"),
        "the routed alias is still resolvable, it is simply not selected"
    );
}

/// The operator's global compaction threshold reaches the model the assignment
/// injects and not the definition the schema publishes beside it.
///
/// Measured against the pinned reference, which fills the two from different
/// places: `_apply_global_auto_compact_threshold` rewrites the entries of
/// `models`, while `routed_model_config` keeps the default `ModelConfig`
/// declares.
#[test]
fn a_global_threshold_reaches_the_injected_model_and_not_the_routed_definition() {
    let snapshot = composed(
        "auto_compact_threshold = 50000\n",
        None,
        &[],
        "",
        &[(
            MODEL_ROUTING,
            concat!(
                r#"{"active_model": "routed", "model_config": {"name": "vibe-routed", "#,
                r#""provider": "mistral", "alias": "routed"}}"#
            ),
        )],
    );

    let injected = snapshot.effective["models"]["routed"]
        .as_table()
        .expect("the routed model is injected");
    assert_eq!(
        injected["auto_compact_threshold"].as_integer(),
        Some(50_000)
    );
    assert_eq!(
        snapshot.effective["routed_model_config"]["auto_compact_threshold"].as_integer(),
        Some(registry::DEFAULT_AUTO_COMPACT_THRESHOLD),
        "the published definition keeps the shipped threshold"
    );
}

/// A rollout payload that spells its own threshold keeps it in both places,
/// because the reference records the field as set and skips it.
#[test]
fn a_threshold_the_payload_spells_survives_both_completions() {
    let snapshot = composed(
        "auto_compact_threshold = 50000\n",
        None,
        &[],
        "",
        &[(
            MODEL_ROUTING,
            concat!(
                r#"{"active_model": "routed", "model_config": {"name": "vibe-routed", "#,
                r#""provider": "mistral", "alias": "routed", "auto_compact_threshold": 90000}}"#
            ),
        )],
    );

    assert_eq!(
        snapshot.effective["models"]["routed"]["auto_compact_threshold"].as_integer(),
        Some(90_000)
    );
    assert_eq!(
        snapshot.effective["routed_model_config"]["auto_compact_threshold"].as_integer(),
        Some(90_000)
    );
}
