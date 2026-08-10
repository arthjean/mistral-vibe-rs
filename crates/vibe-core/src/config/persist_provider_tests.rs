//! The provider upsert the onboarding flow persists: keyed by name, carrying
//! only non-default fields, writing nothing for an unchanged provider, and
//! leaving fields this port does not model where they stand.

use super::registry::default_document;
use super::*;

/// A layered configuration over the shipped defaults with `user` as the
/// selected file, plus the paths the assertions read back.
fn store(user: &str) -> (LayeredConfig, PathBuf, tempfile::TempDir) {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    fs::create_dir_all(&home).expect("home directory");
    if !user.is_empty() {
        fs::write(home.join(CONFIG_FILE), user).expect("user fixture");
    }
    let config = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home.clone(),
            working_directory: temporary.path().join("project"),
        },
        default_document(),
    );
    (config, home.join(CONFIG_FILE), temporary)
}

fn effective_mistral(config: &LayeredConfig) -> Table {
    let snapshot = config.load().expect("the configuration loads");
    provider_entry(snapshot.effective.get("providers"), "mistral")
        .expect("the shipped mistral provider resolves")
        .clone()
}

fn written_providers(path: &PathBuf) -> Vec<Table> {
    let contents = fs::read_to_string(path).expect("the config file exists");
    let document: Table = toml::from_str(&contents).expect("the config file parses");
    document
        .get("providers")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_table)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn an_unchanged_provider_writes_nothing() {
    let (config, path, _root) = store("");
    let provider = effective_mistral(&config);
    let written = config
        .persist_provider(&provider)
        .expect("the comparison succeeds");
    assert!(written.is_none(), "an identical provider is not persisted");
    assert!(!path.exists(), "no configuration file appears");
}

#[test]
fn a_modified_provider_is_upserted_with_only_its_non_default_fields() {
    let (config, path, _root) = store("");
    let mut provider = effective_mistral(&config);
    provider.insert(
        "browser_auth_base_url".to_owned(),
        Value::String("https://console.internal.example".to_owned()),
    );
    config
        .persist_provider(&provider)
        .expect("the upsert succeeds")
        .expect("a write happened");
    let providers = written_providers(&path);
    assert_eq!(providers.len(), 1);
    let entry = &providers[0];
    assert_eq!(entry.get("name").and_then(Value::as_str), Some("mistral"));
    assert_eq!(
        entry.get("browser_auth_base_url").and_then(Value::as_str),
        Some("https://console.internal.example")
    );
    // Reference `exclude_defaults`: fields at the model's defaults stay out of
    // the file so today's defaults are not pinned.
    for absent in ["api_style", "reasoning_field_name", "project_id", "region"] {
        assert!(
            !entry.contains_key(absent),
            "{absent} sits at its default and must not be pinned"
        );
    }
}

#[test]
fn unmodeled_fields_survive_the_upsert_and_a_custom_url_is_preserved() {
    let (config, path, _root) = store(concat!(
        "[[providers]]\n",
        "name = \"mistral\"\n",
        "api_base = \"https://api.mistral.ai/v1\"\n",
        "browser_auth_base_url = \"https://console.internal.example\"\n",
        "future_field = \"kept\"\n",
    ));
    let mut provider = effective_mistral(&config);
    provider.insert("region".to_owned(), Value::String("eu-west".to_owned()));
    config
        .persist_provider(&provider)
        .expect("the upsert succeeds")
        .expect("a write happened");
    let providers = written_providers(&path);
    assert_eq!(providers.len(), 1);
    let entry = &providers[0];
    assert_eq!(
        entry.get("future_field").and_then(Value::as_str),
        Some("kept"),
        "a field this port does not model survives the write"
    );
    assert_eq!(
        entry.get("browser_auth_base_url").and_then(Value::as_str),
        Some("https://console.internal.example"),
        "the configured console URL is not replaced by the shipped default"
    );
    assert_eq!(entry.get("region").and_then(Value::as_str), Some("eu-west"));
}

#[test]
fn a_provider_without_a_name_is_refused() {
    let (config, path, _root) = store("");
    let error = config
        .persist_provider(&Table::new())
        .expect_err("a nameless entry cannot be keyed");
    assert!(matches!(error, ConfigError::InvalidProvider(_)));
    assert!(!path.exists());
}

#[test]
fn an_upsert_keyed_by_name_replaces_the_entry_in_place() {
    let (config, path, _root) = store(concat!(
        "[[providers]]\n",
        "name = \"other\"\n",
        "api_base = \"https://other.example/v1\"\n",
        "[[providers]]\n",
        "name = \"mistral\"\n",
        "api_base = \"https://old.example/v1\"\n",
    ));
    let mut provider = effective_mistral(&config);
    provider.insert(
        "api_base".to_owned(),
        Value::String("https://new.example/v1".to_owned()),
    );
    config
        .persist_provider(&provider)
        .expect("the upsert succeeds")
        .expect("a write happened");
    let providers = written_providers(&path);
    assert_eq!(providers.len(), 2, "the unrelated entry survives");
    assert_eq!(
        providers[0].get("name").and_then(Value::as_str),
        Some("other")
    );
    assert_eq!(
        providers[1].get("api_base").and_then(Value::as_str),
        Some("https://new.example/v1"),
        "the matching entry is replaced where it stands"
    );
}
