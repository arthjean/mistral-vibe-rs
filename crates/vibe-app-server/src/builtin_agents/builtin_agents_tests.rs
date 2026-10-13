use std::fs;

use vibe_core::config::{ConfigPaths, LayeredConfig, registry};

use super::profiles;

/// `allowed_models` matches model names, so the profile admits its model by the
/// name it is served under (`vibe/core/agents/models.py:141`): composed over
/// the shipped defaults, it runs on that model and pins nothing the list
/// excludes.
#[test]
fn the_lean_profile_admits_its_own_model_by_name() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    fs::create_dir_all(&home).expect("home directory");
    let lean = profiles(&home)
        .into_iter()
        .find(|profile| profile.name == "lean")
        .expect("the lean profile is builtin");
    let snapshot = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: temporary.path().join("project"),
        },
        registry::default_document(),
    )
    .with_agent_overlay(lean.overrides)
    .load()
    .expect("the lean profile loads");

    assert!(
        snapshot.validation_warnings.is_empty(),
        "{:?}",
        snapshot.validation_warnings
    );
    assert_eq!(snapshot.active_model_alias(), Some("leanstral"));
    assert_eq!(
        vibe_core::config::available_model_aliases(&snapshot.effective),
        vec!["leanstral"]
    );
    assert_eq!(
        snapshot
            .compaction_settings()
            .compaction_model_alias
            .as_deref(),
        Some("devstral-compact")
    );
}
