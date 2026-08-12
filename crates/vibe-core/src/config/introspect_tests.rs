//! US-068: describing the configuration surface for a settings screen.

use super::registry::default_document;
use super::*;

/// A configuration whose defaults are the shipped document, with `user` written
/// to the selected file and one runtime override on top.
fn described(user: &str, runtime: Table) -> (tempfile::TempDir, ConfigFields) {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    fs::create_dir_all(&home).expect("home directory");
    if !user.is_empty() {
        fs::write(home.join(CONFIG_FILE), user).expect("user fixture");
    }
    let described = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: temporary.path().join("project"),
        },
        default_document(),
    )
    .with_runtime_overrides(runtime)
    .describe_fields()
    .expect("the surface is described");
    (temporary, described)
}

fn field<'a>(described: &'a ConfigFields, name: &str) -> &'a ConfigFieldView {
    described
        .fields
        .iter()
        .find(|field| field.name == name)
        .unwrap_or_else(|| panic!("`{name}` is described"))
}

#[test]
fn each_field_carries_what_a_settings_screen_renders_it_with() {
    let (_temporary, described) = described(
        "theme = \"nord\"\n",
        Table::from_iter([("theme".to_owned(), Value::String("dracula".to_owned()))]),
    );

    let theme = field(&described, "theme");
    assert_eq!(theme.kind, "enum");
    assert_eq!(theme.path, "/theme");
    assert!(theme.popular);
    assert_eq!(
        theme.description,
        registry::field("theme").map_or("", |spec| spec.description)
    );
    assert!(!theme.description.is_empty());
    assert_eq!(theme.enum_choices, THEME_VALUES.to_vec());
    assert_eq!(theme.value, JsonValue::from("dracula"));
    // Highest priority first: the runtime override, then the file, then the
    // shipped default.
    assert_eq!(
        theme
            .layer_values
            .iter()
            .map(|entry| (entry.layer, entry.value.clone()))
            .collect::<Vec<_>>(),
        vec![
            (ConfigLayerKind::Runtime, JsonValue::from("dracula")),
            (ConfigLayerKind::SelectedToml, JsonValue::from("nord")),
            (ConfigLayerKind::Defaults, JsonValue::from("auto")),
        ]
    );

    // A field no layer sets still ends on the defaults layer, so a client always
    // has the value to fall back to.
    let compaction = field(&described, "compaction_model");
    assert_eq!(compaction.kind, "complex");
    assert_eq!(compaction.value, JsonValue::Null);
    assert_eq!(
        compaction
            .layer_values
            .iter()
            .map(|entry| (entry.layer, entry.value.clone()))
            .collect::<Vec<_>>(),
        vec![(ConfigLayerKind::Defaults, JsonValue::Null)]
    );
    let thinking = field(&described, "thinking");
    assert_eq!(
        thinking.layer_values,
        vec![ConfigLayerValue {
            layer: ConfigLayerKind::Defaults,
            value: JsonValue::from("off"),
        }],
        "a key only this port declares still reports its registry default"
    );
}

#[test]
fn the_described_surface_is_the_published_registry_without_the_tool_table() {
    let (_temporary, described) = described("", Table::new());

    let described_names: Vec<&str> = described.fields.iter().map(|field| field.name).collect();
    let expected: Vec<&str> = registry::FIELDS
        .iter()
        .filter(|spec| spec.published && !introspect::HIDDEN_FIELDS.contains(&spec.name))
        .map(|spec| spec.name)
        .collect();
    assert_eq!(described_names, expected);
    for hidden in introspect::HIDDEN_FIELDS {
        assert!(
            !described_names.contains(&hidden),
            "`{hidden}` is a runtime-filled field the reference hides from the settings screen"
        );
    }

    // The popular set is the reference one, which `surface_parity_tests` holds
    // against the captured corpus.
    let popular: Vec<&str> = described
        .fields
        .iter()
        .filter(|field| field.popular)
        .map(|field| field.name)
        .collect();
    assert_eq!(
        popular,
        vec![
            "active_model",
            "auto_compact_threshold",
            "mcp_servers",
            "default_agent",
            "theme",
            "autocopy_to_clipboard",
            "ask_confirmation_on_exit",
            "voice_mode_enabled",
            "bypass_tool_permissions",
            "enable_telemetry",
            "enable_auto_update",
            "enable_notifications",
        ]
    );
}

#[test]
fn a_sensitive_field_is_redacted_in_its_value_and_in_every_layer() {
    // A credential-bearing proxy URL is refused at load, so the secret here is
    // in the path rather than in the userinfo.
    let (_temporary, described) = described(
        "proxy = \"https://proxy.example/s3cr3t\"\n",
        Table::from_iter([(
            "proxy".to_owned(),
            Value::String("https://proxy.example/s3cr3t-runtime".to_owned()),
        )]),
    );

    let proxy = field(&described, "proxy");
    assert_eq!(proxy.value, JsonValue::from("[redacted]"));
    assert!(
        proxy
            .layer_values
            .iter()
            .all(|entry| entry.value == "[redacted]" || entry.value.is_null()),
        "a layer value leaked a secret: {:?}",
        proxy.layer_values
    );
    let rendered = serde_json::to_string(&described).expect("the surface serializes");
    assert!(!rendered.contains("s3cr3t"), "the surface leaked a secret");
}

#[test]
fn the_writable_targets_lead_with_the_selection_and_drop_an_untrusted_project() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let home = temporary.path().join("home/.vibe");
    let project = temporary.path().join("project");
    fs::create_dir_all(&home).expect("home directory");
    fs::create_dir_all(project.join(PROJECT_DIRECTORY)).expect("project directory");
    fs::write(
        project.join(PROJECT_DIRECTORY).join(CONFIG_FILE),
        "theme = \"nord\"\n",
    )
    .expect("project fixture");
    let config = LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: project,
        },
        default_document(),
    );

    assert_eq!(
        config
            .clone()
            .with_project_trusted(true)
            .describe_fields()
            .expect("trusted surface")
            .targets,
        vec![ConfigTarget::Project, ConfigTarget::User]
    );
    assert_eq!(
        config.describe_fields().expect("untrusted surface").targets,
        vec![ConfigTarget::User],
        "an untrusted project file is not a place a write may land"
    );
}
