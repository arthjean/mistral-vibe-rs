//! Per-strategy composition, exercised through `LayeredConfig::load` so the
//! merge is proved from the entry point that composes a real layer stack.

use std::collections::BTreeMap;

use super::*;

/// Composes `documents` as successive layers of a real load, lowest first.
fn compose(documents: &[&str]) -> Result<Table, ConfigError> {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    let project = temporary.path().join("project");
    fs::create_dir_all(project.join(".vibe")).expect("project directory");
    fs::create_dir_all(&home).expect("home directory");

    let mut layers = documents.iter();
    let defaults = layers
        .next()
        .map(|document| document.parse::<Table>().expect("fixture TOML"))
        .unwrap_or_default();
    if let Some(document) = layers.next() {
        fs::write(home.join("config.toml"), document).expect("user fixture");
    }
    let mut config = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: project,
        },
        defaults,
    );
    if let Some(document) = layers.next() {
        config.experiments = document.parse().expect("fixture TOML");
    }
    if let Some(document) = layers.next() {
        config.runtime = document.parse().expect("fixture TOML");
    }
    assert!(layers.next().is_none(), "compose accepts four layers");
    config.load().map(|snapshot| snapshot.effective)
}

fn strings(table: &Table, key: &str) -> Vec<String> {
    table[key]
        .as_array()
        .expect("array field")
        .iter()
        .map(|value| value.as_str().expect("string entry").to_owned())
        .collect()
}

#[test]
fn concat_appends_in_layer_order_and_keeps_duplicates() {
    let effective = compose(&[
        "disabled_tools = [\"dangerous\"]",
        "disabled_tools = [\"bash\"]",
        "disabled_tools = [\"bash\", \"edit\"]",
    ])
    .expect("layers compose");
    assert_eq!(
        strings(&effective, "disabled_tools"),
        ["dangerous", "bash", "bash", "edit"]
    );
}

/// The operator-facing shape of the concat fix. One TOML file is the selected
/// layer, as upstream selects one too, so the denylist a lower layer carries is
/// the one a trusted project file has to extend rather than replace.
#[test]
fn a_trusted_project_file_extends_the_lower_layer_denylist() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    let project = temporary.path().join("project");
    fs::create_dir_all(project.join(".vibe")).expect("project directory");
    fs::create_dir_all(&home).expect("home directory");
    fs::write(
        project.join(".vibe/config.toml"),
        "disabled_tools = [\"edit\"]\n",
    )
    .expect("project fixture");

    let effective = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: project,
        },
        "disabled_tools = [\"bash\"]".parse().expect("fixture TOML"),
    )
    .with_project_trusted(true)
    .load()
    .expect("layers compose")
    .effective;
    assert_eq!(strings(&effective, "disabled_tools"), ["bash", "edit"]);
}

#[test]
fn enabled_tools_replaces_because_the_reference_declares_it_replace() {
    let effective = compose(&[
        "enabled_tools = [\"bash\"]",
        "enabled_tools = [\"edit\", \"read_file\"]",
    ])
    .expect("layers compose");
    assert_eq!(strings(&effective, "enabled_tools"), ["edit", "read_file"]);
}

#[test]
fn union_keys_entries_and_the_higher_layer_wins_per_key() {
    let effective = compose(&[
        r#"
[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "MISTRAL_API_KEY"

[[providers]]
name = "llamacpp"
api_base = "http://127.0.0.1:8080/v1"
"#,
        r#"
[[providers]]
name = "local"
api_base = "http://127.0.0.1:9000/v1"

[[providers]]
name = "mistral"
api_base = "https://proxy.example.test/v1"
"#,
    ])
    .expect("layers compose");
    let providers = effective["providers"].as_array().expect("providers array");
    let names = providers
        .iter()
        .map(|entry| entry["name"].as_str().expect("provider name"))
        .collect::<Vec<_>>();
    // First-seen order survives even though the higher layer redeclares the
    // first entry last.
    assert_eq!(names, ["mistral", "llamacpp", "local"]);
    assert_eq!(
        providers[0]["api_base"].as_str(),
        Some("https://proxy.example.test/v1")
    );
    // The reference replaces the whole entry, so the lower layer's extra key is
    // gone rather than merged.
    assert!(providers[0].as_table().expect("entry")["name"].is_str());
    assert_eq!(providers[0].as_table().expect("entry").len(), 2);
}

#[test]
fn deep_merge_recurses_and_preserves_keys_the_higher_layer_omits() {
    let effective = compose(&[
        "[tools.bash]\ntimeout = 30\n",
        "[tools.bash]\nallowlist = [\"git status\"]\n",
        "[tools.bash]\ntimeout = 60\n\n[tools.edit]\nconfirm = true\n",
    ])
    .expect("layers compose");
    assert_eq!(effective["tools"]["bash"]["timeout"].as_integer(), Some(60));
    assert_eq!(
        effective["tools"]["bash"]["allowlist"][0].as_str(),
        Some("git status")
    );
    assert_eq!(effective["tools"]["edit"]["confirm"].as_bool(), Some(true));
}

#[test]
fn replace_lets_the_higher_layer_win_outright() {
    let effective = compose(&[
        "[project_context]\nenabled = true\nmax_files = 40\n",
        "[project_context]\nenabled = false\n",
    ])
    .expect("layers compose");
    let context = effective["project_context"].as_table().expect("table");
    assert_eq!(context.get("enabled").and_then(Value::as_bool), Some(false));
    assert!(context.get("max_files").is_none());
}

#[test]
fn an_empty_table_where_a_list_belongs_is_treated_as_absent() {
    let concat = compose(&["installed_agents = [\"plan\"]", "[installed_agents]"])
        .expect("concat layers compose");
    assert_eq!(strings(&concat, "installed_agents"), ["plan"]);

    let concat_reversed = compose(&["[installed_agents]", "installed_agents = [\"plan\"]"])
        .expect("concat layers compose");
    assert_eq!(strings(&concat_reversed, "installed_agents"), ["plan"]);

    let union = compose(&[
        "[[tts_providers]]\nname = \"mistral\"\napi_base = \"https://api.mistral.ai\"\n",
        "[tts_providers]",
    ])
    .expect("union layers compose");
    assert_eq!(union["tts_providers"][0]["name"].as_str(), Some("mistral"));
}

#[test]
fn a_union_entry_without_its_merge_key_fails_the_load_without_echoing_the_entry() {
    let error = compose(&[
        "[[providers]]\nname = \"mistral\"\napi_base = \"https://api.mistral.ai/v1\"\n",
        "[[providers]]\napi_base = \"https://leak.example.test/v1?token=super-secret\"\n",
    ])
    .expect_err("a nameless provider cannot be keyed");
    assert!(matches!(
        &error,
        ConfigError::MergeKeyMissing { field, merge_key } if field == "providers" && merge_key == "name"
    ));
    let message = error.to_string();
    assert!(message.contains("providers"), "{message}");
    assert!(message.contains("name"), "{message}");
    assert!(!message.contains("super-secret"), "{message}");
    assert!(!message.contains("leak.example.test"), "{message}");
}

#[test]
fn a_scalar_where_a_list_belongs_fails_the_strategy_by_name() {
    let error = compose(&["disabled_tools = [\"bash\"]", "disabled_tools = \"edit\""])
        .expect_err("concat needs list operands");
    assert!(matches!(
        &error,
        ConfigError::MergeType { field, strategy } if field == "disabled_tools" && *strategy == "concat"
    ));

    let error = compose(&["[tools.bash]\ntimeout = 30\n", "tools = \"none\""])
        .expect_err("deep merge needs table operands");
    assert!(matches!(
        &error,
        ConfigError::MergeType { field, strategy } if field == "tools" && *strategy == "deep_merge"
    ));
}

/// A recorded divergence, not a defect: this port merges a matching
/// `mcp_servers` or `connectors` entry field by field where the reference
/// replaces it whole, so an enablement preference can live in the writable file
/// without copying a lower layer's command, URL or credentials into it.
/// Reconciling it belongs to EP-022.
#[test]
fn mcp_and_connector_entries_merge_field_by_field_unlike_the_reference() {
    let effective = compose(&[
        r#"
[[mcp_servers]]
name = "docs"
transport = "stdio"
command = "/usr/bin/docs-mcp"
env = { API_TOKEN = "must-not-be-copied" }

[[connectors]]
name = "drive"
disabled_tools = ["legacy"]
"#,
        r#"
[[mcp_servers]]
name = "docs"
disabled = true

[[connectors]]
name = "drive"
disabled = true
"#,
    ])
    .expect("layers compose");
    let server = effective["mcp_servers"][0].as_table().expect("entry");
    assert_eq!(server["disabled"].as_bool(), Some(true));
    assert_eq!(server["command"].as_str(), Some("/usr/bin/docs-mcp"));
    let connector = effective["connectors"][0].as_table().expect("entry");
    assert_eq!(connector["disabled"].as_bool(), Some(true));
    assert_eq!(connector["disabled_tools"][0].as_str(), Some("legacy"));
}

#[test]
fn an_unregistered_key_survives_the_merge_and_is_reported_as_unregistered() {
    let effective =
        compose(&["future_key = \"defaults\"", "future_key = \"user\""]).expect("layers compose");
    assert_eq!(effective["future_key"].as_str(), Some("user"));
    let unregistered = effective
        .keys()
        .filter(|key| registry::field(key).is_none())
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(unregistered, ["future_key"]);
}
