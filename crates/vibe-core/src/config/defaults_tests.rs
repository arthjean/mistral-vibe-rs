//! The shipped default document and the rules the load applies on top of it.
//!
//! The corpus in [`surface_parity_tests`](super::surface_parity_tests) proves
//! the defaults and the merged models match the reference. This module covers
//! what a corpus scenario cannot reach: the errors a malformed document raises,
//! the path the write back takes, and the keys only this port declares.

use super::registry::default_document;
use super::*;

/// A layered configuration over the shipped defaults, with `user` written to the
/// selected file.
fn shipped(user: &str) -> Result<ConfigSnapshot, ConfigError> {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    fs::create_dir_all(&home).expect("home directory");
    if !user.is_empty() {
        fs::write(home.join(CONFIG_FILE), user).expect("user fixture");
    }
    LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: temporary.path().join("project"),
        },
        default_document(),
    )
    .load()
}

#[test]
fn a_user_override_wins_without_dropping_the_untouched_defaults() {
    let shipped_defaults = default_document();
    let effective = shipped("theme = \"nord\"\napi_timeout = 12.5\n")
        .expect("the override loads")
        .effective;

    assert_eq!(effective["theme"].as_str(), Some("nord"));
    assert_eq!(effective["api_timeout"].as_float(), Some(12.5));
    for (key, value) in &shipped_defaults {
        if matches!(key.as_str(), "theme" | "api_timeout" | "session_logging") {
            continue;
        }
        // `models` gains its alias keys and the completed entry fields; every
        // other default survives the override untouched.
        if key == "models" {
            assert!(
                effective[key].is_table(),
                "models is read back keyed by alias"
            );
            continue;
        }
        assert_eq!(
            effective.get(key),
            Some(value),
            "the override dropped the default for `{key}`"
        );
    }
    assert_eq!(
        shipped_defaults.len(),
        effective.len(),
        "the load composed a key the defaults do not declare"
    );
}

/// The session log directory is absolutized through the real load, as the
/// reference `expand_save_dir` absolutizes it.
#[test]
fn a_relative_session_log_directory_is_absolutized() {
    let effective = shipped("[session_logging]\nsave_dir = \"relative/logs\"\n")
        .expect("the override loads")
        .effective;
    let resolved = effective["session_logging"]["save_dir"]
        .as_str()
        .expect("the directory stays a string");
    assert!(Path::new(resolved).is_absolute(), "{resolved}");
    assert!(resolved.ends_with("relative/logs"), "{resolved}");
}

#[test]
fn a_home_relative_session_log_directory_is_expanded() {
    let Some(home) = user_home_directory() else {
        eprintln!("skipping the home expansion check: this process resolves no home directory");
        return;
    };
    let effective = shipped("[session_logging]\nsave_dir = \"~/vibe-session-logs\"\n")
        .expect("the override loads")
        .effective;
    assert_eq!(
        effective["session_logging"]["save_dir"].as_str(),
        home.join("vibe-session-logs").to_str()
    );
}

/// The branch a load cannot reach without an unset home directory, which
/// mutating the environment would be the only other way to produce.
#[test]
fn a_session_log_directory_needing_an_unresolvable_home_fails_naming_the_field() {
    let mut effective = default_document();
    effective.insert(
        "session_logging".to_owned(),
        Value::Table(Table::from_iter([(
            "save_dir".to_owned(),
            Value::String("~/logs".to_owned()),
        )])),
    );
    let error = super::effective::resolve_session_log_dir(
        &mut effective,
        Path::new("/home/user/.vibe"),
        None,
    )
    .expect_err("an unresolvable home fails the load");
    assert!(
        error.to_string().contains("session_logging.save_dir"),
        "{error}"
    );
}

#[test]
fn a_model_entry_without_an_alias_or_a_name_fails_naming_the_field() {
    let error = shipped("[[models]]\ntemperature = 0.5\n").expect_err("the entry has no identity");
    assert!(error.to_string().contains("models"), "{error}");
    assert!(error.to_string().contains("alias"), "{error}");
}

#[test]
fn a_model_key_that_disagrees_with_its_entry_alias_fails_naming_both() {
    let error = shipped("[models.local]\nalias = \"remote\"\n")
        .expect_err("the key and the alias disagree");
    let message = error.to_string();
    assert!(message.contains("local"), "{message}");
    assert!(message.contains("remote"), "{message}");
}

/// An emptied model set only reaches validation when nothing else contributed
/// one, which the reference confirms: composed over the default layer, an empty
/// list deep-merges to nothing and the shipped models survive.
#[test]
fn a_stack_that_configures_no_model_fails_the_load() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    fs::create_dir_all(&home).expect("home directory");
    fs::write(home.join(CONFIG_FILE), "models = []\n").expect("user fixture");
    let error = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: temporary.path().join("project"),
        },
        Table::new(),
    )
    .load()
    .expect_err("a configuration needs one model");
    assert!(matches!(error, ConfigError::NoConfiguredModel), "{error}");
    assert!(error.to_string().contains("models"), "{error}");

    let composed = shipped("models = []\n").expect("the shipped models survive an empty list");
    assert_eq!(
        composed.effective["models"].as_table().map(Table::len),
        Some(3)
    );
}

/// A client that patched the alias-keyed read form must leave the persisted
/// list form behind, which is what the reference extensions parse.
#[test]
fn writing_a_model_map_persists_the_list_form() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    fs::create_dir_all(&home).expect("home directory");
    let config = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home.clone(),
            working_directory: temporary.path().join("project"),
        },
        default_document(),
    );

    let mut entry = Table::new();
    entry.insert("name".to_owned(), Value::String("devstral".to_owned()));
    entry.insert("provider".to_owned(), Value::String("llamacpp".to_owned()));
    let mut models = Table::new();
    models.insert("local".to_owned(), Value::Table(entry));

    let snapshot = config
        .batch_write(&[ConfigWrite {
            target: ConfigTarget::User,
            expected_fingerprint: None,
            mutations: vec![ConfigMutation::set(["models"], Value::Table(models))],
        }])
        .expect("the map is written");

    let persisted = fs::read_to_string(home.join(CONFIG_FILE)).expect("the file is written");
    assert!(persisted.contains("[[models]]"), "{persisted}");
    let parsed = persisted.parse::<Table>().expect("the file parses");
    let entries = parsed["models"].as_array().expect("a list of models");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["alias"].as_str(), Some("local"));
    assert_eq!(entries[0]["name"].as_str(), Some("devstral"));
    // Read back, the same document is keyed by alias again.
    assert_eq!(
        snapshot.effective["models"]["local"]["provider"].as_str(),
        Some("llamacpp")
    );
}

/// The reference records the fallback as a warning rather than failing, and a
/// client reads it back through the snapshot instead of only seeing it logged.
#[test]
fn an_unknown_active_model_falls_back_and_publishes_a_readable_warning() {
    let snapshot = shipped("active_model = \"not-configured\"\n").expect("the fallback loads");
    assert_eq!(
        snapshot.effective["active_model"].as_str(),
        Some("mistral-medium-3.5"),
        "the first configured model is selected"
    );
    let [warning] = snapshot.validation_warnings.as_slice() else {
        panic!(
            "one warning is recorded: {:?}",
            snapshot.validation_warnings
        );
    };
    assert!(warning.contains("not-configured"), "{warning}");
    assert!(warning.contains("mistral-medium-3.5"), "{warning}");
    assert_eq!(
        snapshot.public_view()["validationWarnings"],
        serde_json::json!([warning]),
        "the warning is readable by a client, not only logged"
    );
}

/// The five keys only this port declares stay readable beside the shipped
/// reference defaults; each is a recorded divergence rather than a mapping, so
/// nothing reinterprets a value already on disk.
#[test]
fn a_persisted_locally_declared_key_still_loads_beside_the_shipped_defaults() {
    let effective = shipped(concat!(
        "thinking = \"high\"\n",
        "notifications = \"always\"\n",
        "proxy = \"http://proxy.example.test:3128\"\n",
        "tls_ca_path = \"/etc/ssl/certs/custom.pem\"\n",
        "dotenv_path = \"/home/user/.vibe/.env\"\n",
    ))
    .expect("the locally declared keys load")
    .effective;

    assert_eq!(effective["thinking"].as_str(), Some("high"));
    assert_eq!(effective["notifications"].as_str(), Some("always"));
    assert_eq!(
        effective["proxy"].as_str(),
        Some("http://proxy.example.test:3128")
    );
    assert_eq!(
        effective["tls_ca_path"].as_str(),
        Some("/etc/ssl/certs/custom.pem")
    );
    assert_eq!(
        effective["dotenv_path"].as_str(),
        Some("/home/user/.vibe/.env")
    );
    // They are declared, so they are not reported as unregistered either.
    assert!(effective.contains_key("thinking"));
    assert!(
        !shipped("")
            .expect("the defaults load")
            .effective
            .contains_key("thinking"),
        "a locally declared key must not be shipped as a default"
    );
}

/// US-148: reference `_check_compaction_model_provider`. A compaction model
/// naming a provider nothing configures fails the load rather than reaching a
/// summarization that cannot be sent.
#[test]
fn a_compaction_model_on_an_unconfigured_provider_fails_naming_both() {
    let error = shipped(concat!(
        "[compaction_model]\n",
        "name = \"summarizer\"\n",
        "provider = \"absent\"\n",
        "alias = \"cheap\"\n",
    ))
    .expect_err("the provider is not configured");
    assert!(
        matches!(
            &error,
            ConfigError::CompactionModelProviderMissing { alias, provider }
                if alias == "cheap" && provider == "absent"
        ),
        "{error}"
    );
    let message = error.to_string();
    assert!(message.contains("cheap"), "{message}");
    assert!(message.contains("absent"), "{message}");
}

/// The reference also refuses a compaction model served by a different provider
/// than the active model, because the two share one client.
#[test]
fn a_compaction_model_on_another_provider_than_the_active_one_fails() {
    let error = shipped(concat!(
        "[compaction_model]\n",
        "name = \"devstral\"\n",
        "provider = \"llamacpp\"\n",
        "alias = \"local-summarizer\"\n",
    ))
    .expect_err("the two providers differ");
    assert!(
        matches!(
            &error,
            ConfigError::CompactionModelProviderMismatch { alias, provider, active_provider }
                if alias == "local-summarizer"
                    && provider == "llamacpp"
                    && active_provider == "mistral"
        ),
        "{error}"
    );

    let agreed = shipped(concat!(
        "[compaction_model]\n",
        "name = \"devstral-small-latest\"\n",
        "provider = \"mistral\"\n",
        "alias = \"cheap\"\n",
    ))
    .expect("a compaction model on the active provider loads");
    assert_eq!(
        agreed.compaction_settings().compaction_model.as_deref(),
        Some("devstral-small-latest")
    );
}

// --------------------------------------------------------------------------
// The routed model an experiment assigns
// --------------------------------------------------------------------------

/// A layered configuration over the shipped defaults with `user` in the
/// selected file and `experiments` in the layer the GrowthBook assignment
/// fills, which is where the two routed keys actually arrive.
fn routed(user: &str, experiments: &str) -> Result<ConfigSnapshot, ConfigError> {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    fs::create_dir_all(&home).expect("home directory");
    if !user.is_empty() {
        fs::write(home.join(CONFIG_FILE), user).expect("user fixture");
    }
    LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: temporary.path().join("project"),
        },
        default_document(),
    )
    .with_experiments(
        experiments
            .parse::<Table>()
            .expect("the experiment layer parses"),
    )
    .load()
}

/// The definition a routing variant carries, as the layer re-encodes it: the
/// JSON text of one model, which the load coerces before anything reads it.
///
/// The prices are quoted and the surplus key is there on purpose: this is the
/// shape the oracle records under `configMapping/routing-with-model-config` in
/// `crates/vibe-core/tests/experiments/corpus.json`, and reference
/// `ModelConfig` reads a quoted price as the number it spells and ignores a key
/// it does not declare.
const ROUTED_DEFINITION: &str = concat!(
    r#"routed_model_config = "{\"name\": \"vibe-routed-latest\", "#,
    r#"\"provider\": \"mistral\", \"alias\": \"routed\", \"supports_images\": true, "#,
    r#"\"input_price\": \"0.0\", \"output_price\": \"1.5\", \"graduated\": true}""#,
    "\n",
);

/// US-004: an unpinned installation runs on the model the rollout routed it to,
/// which is what makes `routed_default_model` more than a key nothing reads.
#[test]
fn an_unpinned_active_model_resolves_to_the_routed_default() {
    let snapshot = routed(
        "",
        concat!(
            "routed_default_model = \"local\"\n",
            "system_prompt_id = \"cli\"\n",
        ),
    )
    .expect("the assignment loads");

    assert_eq!(
        snapshot.effective["active_model"].as_str(),
        Some(""),
        "the document keeps the sentinel; only the read resolves"
    );
    assert_eq!(snapshot.active_model_alias(), Some("local"));
    // The `config/read` view is a real reader of the resolved alias, so the
    // assignment is observed where a client would see it.
    assert_eq!(
        snapshot.config_view()["activeModel"]["alias"],
        JsonValue::from("local")
    );
}

/// A rollout naming a model this installation does not configure selects the
/// shipped default instead of failing, reference `resolve_default_model_alias`.
#[test]
fn a_routed_default_naming_no_configured_model_falls_back_to_the_shipped_alias() {
    let snapshot = routed("", "routed_default_model = \"absent\"\n")
        .expect("an unresolvable routed default still loads");

    assert_eq!(snapshot.active_model_alias(), Some("mistral-medium-3.5"));
    assert!(
        snapshot.validation_warnings.is_empty(),
        "an unresolvable routed default is not a repair: {:?}",
        snapshot.validation_warnings
    );
}

/// US-004: the definition travelling with the alias is merged into the model
/// map, so a rollout can route onto a model no shipped default declares.
#[test]
fn a_routed_definition_is_merged_into_the_model_map_under_its_alias() {
    let snapshot = routed(
        "",
        &format!("routed_default_model = \"routed\"\n{ROUTED_DEFINITION}"),
    )
    .expect("the routed definition loads");

    let entry = snapshot.effective["models"]["routed"]
        .as_table()
        .expect("the routed alias reached the model map");
    assert_eq!(entry["name"].as_str(), Some("vibe-routed-latest"));
    assert_eq!(entry["provider"].as_str(), Some("mistral"));
    assert_eq!(entry["supports_images"].as_bool(), Some(true));
    // The rollout quotes its prices and the model reads them as numbers, so a
    // price reaches the map as one rather than as the text that spelled it.
    assert_eq!(entry["input_price"].as_float(), Some(0.0));
    assert_eq!(entry["output_price"].as_float(), Some(1.5));
    assert!(
        !entry.contains_key("graduated"),
        "a key the model does not declare reached the map: {entry:?}"
    );
    // The injected entry is completed like any other: the per-entry defaults
    // and the global compaction threshold both reach it.
    assert_eq!(entry["temperature"].as_float(), Some(0.2));
    assert_eq!(entry["auto_compact_threshold"].as_integer(), Some(200_000));
    assert_eq!(snapshot.active_model_alias(), Some("routed"));
    assert_eq!(
        snapshot.config_view()["activeModel"]["name"],
        JsonValue::from("vibe-routed-latest")
    );
}

/// Reference `_coerce_routed_model_config` drops text that is not a model.
/// Nothing else moves, and the load still succeeds.
///
/// A value the model cannot read makes the whole definition unusable, not just
/// the field carrying it: a temperature that spells no number and a thinking
/// level outside the five the model names are both measured against the oracle
/// as dropping the definition outright.
#[test]
fn a_routed_definition_that_is_not_a_model_is_dropped_with_a_warning() {
    for payload in [
        "routed_model_config = \"not json at all\"\n",
        "routed_model_config = \"[1, 2, 3]\"\n",
        "routed_model_config = \"{\\\"provider\\\": \\\"mistral\\\"}\"\n",
        concat!(
            r#"routed_model_config = "{\"name\": \"m\", \"provider\": \"mistral\", "#,
            r#"\"alias\": \"routed\", \"temperature\": \"hot\"}""#,
            "\n",
        ),
        concat!(
            r#"routed_model_config = "{\"name\": \"m\", \"provider\": \"mistral\", "#,
            r#"\"alias\": \"routed\", \"thinking\": \"extreme\"}""#,
            "\n",
        ),
    ] {
        let snapshot = routed("", &format!("routed_default_model = \"routed\"\n{payload}"))
            .unwrap_or_else(|error| panic!("{payload}: {error}"));
        assert!(
            !snapshot.effective.contains_key("routed_model_config"),
            "{payload}: an unusable definition reached the document"
        );
        assert!(
            !snapshot.effective["models"]
                .as_table()
                .expect("models")
                .contains_key("routed"),
            "{payload}: an unusable definition was injected anyway"
        );
        assert_eq!(
            snapshot.validation_warnings.len(),
            1,
            "{payload}: {:?}",
            snapshot.validation_warnings
        );
        assert_eq!(snapshot.active_model_alias(), Some("mistral-medium-3.5"));
    }
}

/// A definition naming another alias than the routed one is left where it is:
/// reference `_inject_routed_model` compares the two before injecting.
#[test]
fn a_routed_definition_disagreeing_with_the_routed_alias_is_not_injected() {
    let snapshot = routed(
        "",
        &format!("routed_default_model = \"elsewhere\"\n{ROUTED_DEFINITION}"),
    )
    .expect("the mismatched definition loads");

    assert!(
        !snapshot.effective["models"]
            .as_table()
            .expect("models")
            .contains_key("elsewhere"),
        "a definition carrying another alias was injected"
    );
    assert_eq!(snapshot.active_model_alias(), Some("mistral-medium-3.5"));
}

/// US-005: a pinned alias wins outright, and the routed definition is not even
/// injected, because the routed alias can never be selected for that operator.
#[test]
fn a_pinned_active_model_wins_over_the_routed_default() {
    let snapshot = routed(
        "active_model = \"local\"\n",
        &format!("routed_default_model = \"routed\"\n{ROUTED_DEFINITION}"),
    )
    .expect("the pinned alias loads");

    assert_eq!(snapshot.active_model_alias(), Some("local"));
    assert!(
        !snapshot.effective["models"]
            .as_table()
            .expect("models")
            .contains_key("routed"),
        "a pinned installation was given a routed definition it cannot select"
    );
}
