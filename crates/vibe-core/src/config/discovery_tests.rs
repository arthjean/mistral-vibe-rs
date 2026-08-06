//! The layer a runtime discovery pass composes.
//!
//! Reference `DiscoveredConfigLayer`, whose position `build_default_orchestrator`
//! documents: above the schema defaults, below the selected file. What it holds
//! is a property of the running process, so the layer is proved through the real
//! load rather than through a fixture file.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::registry::default_document;
use super::*;

/// A store over the shipped defaults whose discovery pass answers with
/// `discovered`, with `user` written to the selected file.
fn stack(
    discovered: Result<&str, &str>,
    user: &str,
) -> (tempfile::TempDir, Result<ConfigSnapshot, ConfigError>) {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    fs::create_dir_all(&home).expect("home directory");
    if !user.is_empty() {
        fs::write(home.join(CONFIG_FILE), user).expect("user fixture");
    }
    let answer = match discovered {
        Ok(document) => Ok(document.parse::<Table>().expect("discovered fixture")),
        Err(reason) => Err(reason.to_owned()),
    };
    let snapshot = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: temporary.path().join("project"),
        },
        default_document(),
    )
    .with_discovery(Arc::new(move || answer.clone()))
    .load();
    (temporary, snapshot)
}

fn layer(snapshot: &ConfigSnapshot, kind: ConfigLayerKind) -> &Table {
    &snapshot
        .layer_values
        .iter()
        .find(|layer| layer.kind == kind)
        .unwrap_or_else(|| panic!("the {kind:?} layer is composed"))
        .values
}

#[test]
fn the_discovered_layer_sits_above_the_defaults_and_below_the_selected_file() {
    let kinds = |snapshot: &ConfigSnapshot| {
        snapshot
            .layer_values
            .iter()
            .map(|layer| layer.kind)
            .collect::<Vec<_>>()
    };

    let (_temporary, snapshot) = stack(Ok("theme = \"nord\"\n"), "");
    let snapshot = snapshot.expect("the discovered layer loads");
    assert_eq!(
        kinds(&snapshot),
        [
            ConfigLayerKind::Defaults,
            ConfigLayerKind::Discovered,
            ConfigLayerKind::SelectedToml,
            ConfigLayerKind::Experiments,
            ConfigLayerKind::Environment,
            ConfigLayerKind::Runtime,
            ConfigLayerKind::Agent,
        ]
    );
    // The defaults ship `auto`; discovery overrides it because it composes on
    // top of them.
    assert_eq!(default_document()["theme"].as_str(), Some("auto"));
    assert_eq!(snapshot.effective["theme"].as_str(), Some("nord"));
    assert_eq!(
        layer(&snapshot, ConfigLayerKind::Discovered)["theme"].as_str(),
        Some("nord")
    );
}

#[test]
fn a_file_the_operator_owns_wins_over_the_key_discovery_set() {
    let (_temporary, snapshot) = stack(
        Ok("theme = \"nord\"\n[tools.read_file]\nmaxBytes = 1024\nmaxLines = 40\n"),
        "theme = \"dracula\"\n[tools.read_file]\nmaxBytes = 4096\n",
    );
    let snapshot = snapshot.expect("the user override loads");

    assert_eq!(snapshot.effective["theme"].as_str(), Some("dracula"));
    // `tools` deep-merges, so overriding one option leaves the rest of the
    // discovered entry standing.
    let read_file = snapshot.effective["tools"]["read_file"]
        .as_table()
        .expect("the discovered tool entry survives");
    assert_eq!(read_file["maxBytes"].as_integer(), Some(4096));
    assert_eq!(read_file["maxLines"].as_integer(), Some(40));
}

#[test]
fn a_store_with_no_discovery_composes_an_empty_layer_and_an_unchanged_document() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    fs::create_dir_all(&home).expect("home directory");
    let store = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: temporary.path().join("project"),
        },
        default_document(),
    );

    let without = store.load().expect("the store loads with no discovery");
    let with = store
        .clone()
        .with_discovery(Arc::new(|| Ok(Table::new())))
        .load()
        .expect("the store loads with an empty discovery");

    assert!(layer(&without, ConfigLayerKind::Discovered).is_empty());
    assert!(layer(&with, ConfigLayerKind::Discovered).is_empty());
    assert_eq!(without.effective, with.effective);
    assert!(!without.effective.contains_key("tools"));
    assert!(without.validation_warnings.is_empty());
    assert!(with.validation_warnings.is_empty());
}

#[test]
fn a_failed_discovery_pass_empties_the_layer_and_is_reported_once_per_load() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    fs::create_dir_all(&home).expect("home directory");
    fs::write(home.join(CONFIG_FILE), "theme = \"nord\"\n").expect("user fixture");
    let passes = Arc::new(AtomicUsize::new(0));
    let counted = passes.clone();
    let store = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: temporary.path().join("project"),
        },
        default_document(),
    )
    .with_discovery(Arc::new(move || {
        counted.fetch_add(1, Ordering::Relaxed);
        Err("the tool catalog could not be enumerated".to_owned())
    }));

    let first = store.load().expect("a failed pass still loads");
    let second = store.load().expect("the second load still succeeds");

    assert!(layer(&first, ConfigLayerKind::Discovered).is_empty());
    assert_eq!(first.effective["theme"].as_str(), Some("nord"));
    // One entry per snapshot, and the second load does not inherit the first
    // one's, which is what a warning list carried across loads would do.
    assert_eq!(
        first.validation_warnings,
        ["Runtime discovery is unavailable: the tool catalog could not be enumerated"]
    );
    assert_eq!(first.validation_warnings, second.validation_warnings);
    assert_eq!(
        passes.load(Ordering::Relaxed),
        2,
        "the pass runs once per load, so a catalog that changes reaches the next snapshot"
    );
}

/// A client reading `config/read` renders where a value comes from, so the layer
/// has to arrive under its own name in both lists the snapshot publishes.
#[test]
fn config_read_publishes_the_discovered_layer_under_its_own_name() {
    let (_temporary, snapshot) = stack(Ok("[tools.web_fetch]\ndefault_timeout = 30\n"), "");
    let published = snapshot.expect("the discovered layer loads").public_view();

    assert_eq!(
        published["layers"][1],
        serde_json::json!("discovered"),
        "the layer list names the discovered layer in composition order"
    );
    assert_eq!(published["layerValues"][1]["layer"], "discovered");
    assert_eq!(
        published["layerValues"][1]["values"]["tools"]["web_fetch"]["default_timeout"],
        30
    );
}

/// The pass the binaries install: the settings every declared tool carries, in
/// the shape the layer composes.
#[test]
fn every_declared_tool_publishes_the_settings_the_layer_carries() {
    let settings = crate::tools::config::ToolConfigResolver::new().discovered_document();
    let tools = settings["tools"].as_table().expect("a tool table");

    assert!(tools.contains_key("todo"), "{tools:?}");
    assert_eq!(tools["web_fetch"]["default_timeout"].as_integer(), Some(30));
    // A tool declaring no limit still publishes the four keys every tool
    // carries, so an operator can move its permission without reading the
    // source.
    assert_eq!(tools["skill"]["permission"].as_str(), Some("always"));
    // Reference `discover_tool_defaults` enumerates declarations rather than the
    // surface a host publishes, so a tool that needs a credential and a family
    // this platform never runs are both here.
    assert!(tools.contains_key("web_search"), "{tools:?}");
    assert!(tools.contains_key("powershell"), "{tools:?}");
}
