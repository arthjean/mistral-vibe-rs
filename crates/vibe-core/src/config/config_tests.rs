//! The layered store end to end: precedence, transactional writes, recovery,
//! integration persistence and the inputs it must refuse without leaking.

use std::fs;

use serde_json::json;
use toml::Value;
use url::Url;

use super::integrations::{IntegrationCollection, config_array};
use super::transaction::{ConfigJournal, JournalState};
use super::*;
use crate::mcp::{McpServerConfig, McpTransportConfig};

fn table(input: &str) -> Table {
    input.parse().expect("fixture TOML")
}

fn config(root: &Path) -> LayeredConfig {
    let home = root.join("home/.vibe");
    let project = root.join("project");
    fs::create_dir_all(project.join(".vibe")).expect("project config directory");
    fs::create_dir_all(&home).expect("user config directory");
    LayeredConfig::new(
        ConfigPaths {
            vibe_home: home,
            working_directory: project,
        },
        table(
            r#"
active_model = "default"
thinking = "off"
[nested]
default_only = true
winner = "defaults"
"#,
        ),
    )
}

#[test]
fn precedence_stacks_both_tomls_and_preserves_unknown_fields() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let mut config = config(temporary.path()).with_project_trusted(true);
    fs::write(
        config.paths.user_config(),
        "active_model = \"user\"\n[user_unknown]\nfuture = 1\n",
    )
    .expect("user fixture");
    fs::write(
        config.paths.project_config(),
        "active_model = \"project\"\n[future]\nunknown = \"kept\"\n",
    )
    .expect("project fixture");
    config.set_experiments(table("thinking = \"low\""));
    config.environment = BTreeMap::from([
        ("VIBE_ACTIVE_MODEL".to_owned(), "\"environment\"".to_owned()),
        (
            "VIBE_NESTED__WINNER".to_owned(),
            "\"environment\"".to_owned(),
        ),
    ]);
    config.runtime = table("active_model = \"runtime\"");
    config.agent = table("active_model = \"agent\"");

    let snapshot = config.load().expect("configuration composes");
    assert_eq!(snapshot.selected_target, ConfigTarget::User);
    assert_eq!(snapshot.effective["active_model"].as_str(), Some("agent"));
    assert_eq!(snapshot.effective["thinking"].as_str(), Some("low"));
    // An undeclared name is ignored by the environment layer, as
    // pydantic-settings ignores it, so the defaults keep the key.
    assert_eq!(
        snapshot.effective["nested"]["winner"].as_str(),
        Some("defaults")
    );
    assert_eq!(
        snapshot.effective["future"]["unknown"].as_str(),
        Some("kept")
    );
    // The trusted project composes above the user file, so the user's own
    // keys survive beneath it.
    assert_eq!(
        snapshot.effective["user_unknown"]["future"].as_integer(),
        Some(1)
    );
}

#[test]
fn batch_write_is_conflict_checked_atomic_and_unknown_field_preserving() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let config = config(temporary.path()).with_project_trusted(true);
    fs::write(
        config.paths.user_config(),
        "known = \"old\"\n[future]\nunknown = \"kept\"\n",
    )
    .expect("user fixture");
    fs::write(config.paths.project_config(), "project = \"old\"\n").expect("project fixture");
    let before = config.load().expect("baseline loads");

    let snapshot = config
        .batch_write(&[
            ConfigWrite {
                target: ConfigTarget::User,
                expected_fingerprint: before.fingerprints[&ConfigTarget::User].clone(),
                mutations: vec![ConfigMutation::set(
                    ["known"],
                    Value::String("new".to_owned()),
                )],
            },
            ConfigWrite {
                target: ConfigTarget::Project,
                expected_fingerprint: before.fingerprints[&ConfigTarget::Project].clone(),
                mutations: vec![ConfigMutation::set(
                    ["project"],
                    Value::String("new".to_owned()),
                )],
            },
        ])
        .expect("batch commits");
    assert_eq!(snapshot.effective["project"].as_str(), Some("new"));
    let user = read_table_optional(&config.paths.user_config()).expect("user reloads");
    assert_eq!(user["known"].as_str(), Some("new"));
    assert_eq!(user["future"]["unknown"].as_str(), Some("kept"));

    fs::write(config.paths.user_config(), "known = \"external\"\n").expect("concurrent edit");
    assert!(matches!(
        config.batch_write(&[ConfigWrite {
            target: ConfigTarget::User,
            expected_fingerprint: before.fingerprints[&ConfigTarget::User].clone(),
            mutations: vec![],
        }]),
        Err(ConfigError::ConcurrentEdit {
            target: ConfigTarget::User
        })
    ));
}

#[test]
fn orphan_transaction_sidecars_are_recovered_without_touching_unrelated_files() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let store = config(temporary.path());
    let user_parent = store
        .paths
        .user_config()
        .parent()
        .expect("user parent")
        .to_path_buf();
    let project_parent = store
        .paths
        .project_config()
        .parent()
        .expect("project parent")
        .to_path_buf();
    let orphan_tmp = user_parent.join(".config.0123456789abcdef0123456789abcdef.tmp");
    let orphan_bak = project_parent.join(".config.fedcba9876543210fedcba9876543210.bak");
    let unrelated = user_parent.join(".config.keep.tmp");
    fs::write(&orphan_tmp, "partial").expect("orphan temp fixture");
    fs::write(&orphan_bak, "partial").expect("orphan backup fixture");
    fs::write(&unrelated, "keep").expect("unrelated fixture");

    store.load().expect("orphan recovery succeeds");

    assert!(!orphan_tmp.exists());
    assert!(!orphan_bak.exists());
    assert!(unrelated.exists());
}

#[test]
fn integration_rollbacks_use_the_committed_fingerprint_without_clobbering_a_later_writer() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let store = config(temporary.path());
    fs::write(
        store.paths.user_config(),
        r#"
[[mcp_servers]]
name = "docs"
url = "https://mcp.example.test/rpc"
"#,
    )
    .expect("integration fixture");

    let mcp_commit = store
        .persist_mcp_state("docs", false, &BTreeSet::from(["search".to_owned()]))
        .expect("MCP preference commit");
    let mcp_target = mcp_commit.selected_target;
    store
        .batch_write(&[ConfigWrite {
            target: mcp_target,
            expected_fingerprint: mcp_commit.fingerprints[&mcp_target].clone(),
            mutations: vec![ConfigMutation::set(
                ["writer"],
                Value::String("after-mcp".to_owned()),
            )],
        }])
        .expect("interleaved MCP writer");
    assert!(matches!(
        store.persist_mcp_state_cas(
            "docs",
            true,
            &BTreeSet::new(),
            mcp_target,
            mcp_commit.fingerprints[&mcp_target].clone(),
        ),
        Err(ConfigError::ConcurrentEdit { target }) if target == mcp_target
    ));
    assert_eq!(
        store.load().expect("MCP conflict reloads").effective["writer"].as_str(),
        Some("after-mcp")
    );
}

#[test]
fn runtime_and_agent_integration_overrides_win_over_selected_preferences() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let store = config(temporary.path())
        .with_runtime_overrides(table(
            r#"
[[mcp_servers]]
name = "docs"
disabled = true

[[connectors]]
name = "drive"
disabled = true
"#,
        ))
        .with_agent_overlay(table(
            r#"
[[mcp_servers]]
name = "docs"
disabled_tools = ["agent-search"]

[[connectors]]
name = "drive"
disabled_tools = ["agent-search"]
"#,
        ));
    fs::write(
        store.paths.user_config(),
        r#"
[[mcp_servers]]
name = "docs"
transport = "streamable-http"
url = "https://mcp.example.test/rpc"
disabled = false
disabled_tools = ["selected-search"]

[[connectors]]
name = "drive"
disabled = false
disabled_tools = ["selected-search"]
"#,
    )
    .expect("selected integration preferences");

    let snapshot = store.load().expect("layered integrations load");
    let mcp = snapshot
        .mcp_servers(Path::new("/workspace"))
        .expect("MCP decodes")
        .into_iter()
        .find(|server| server.alias == "docs")
        .expect("MCP remains defined");
    assert!(!mcp.enabled);
    assert_eq!(
        mcp.disabled_tools,
        BTreeSet::from(["agent-search".to_owned()])
    );
    let connector = config_array(&snapshot.effective, IntegrationCollection::Connectors)
        .expect("connectors decode")
        .into_iter()
        .find(|entry| {
            entry
                .as_table()
                .and_then(|entry| entry.get("name"))
                .and_then(Value::as_str)
                == Some("drive")
        })
        .expect("connector remains defined");
    let connector = connector.as_table().expect("connector table");
    assert_eq!(
        connector.get("disabled").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        connector
            .get("disabled_tools")
            .and_then(Value::as_array)
            .and_then(|tools| tools.first())
            .and_then(Value::as_str),
        Some("agent-search")
    );
}

#[test]
fn corrupt_untrusted_and_secret_bearing_inputs_fail_closed_without_leaking() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let config = config(temporary.path());
    fs::write(
        config.paths.user_config(),
        "api_key = \"parse-secret\"\nbroken = [",
    )
    .expect("corrupt configuration fixture");
    let parse_error = config.load().expect_err("corrupt TOML fails");
    assert!(matches!(parse_error, ConfigError::InvalidToml { .. }));
    assert!(!parse_error.to_string().contains("parse-secret"));

    fs::write(
        config.paths.user_config(),
        "proxy = \"https://user:secret@example.test\"\n",
    )
    .expect("proxy fixture");
    let error = config.load().expect_err("credentialed proxy rejected");
    assert!(matches!(error, ConfigError::SensitiveUrlCredentials { .. }));
    assert!(!error.to_string().contains("secret"));

    let baseline = fingerprint_optional(&config.paths.project_config()).expect("fingerprint reads");
    assert!(matches!(
        config.batch_write(&[ConfigWrite {
            target: ConfigTarget::Project,
            expected_fingerprint: baseline,
            mutations: vec![],
        }]),
        Err(ConfigError::UntrustedProject)
    ));
}

#[test]
fn public_views_redact_secrets_and_trust_revocation_drops_the_project_layer() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let config = config(temporary.path()).with_project_trusted(true);
    fs::write(
            config.paths.project_config(),
            "api_key = \"top-secret\"\nprivateKey = \"private\"\nproxy = \"https://example.test/?access_token=query-secret\"\nactive_model = \"project\"\n",
        )
        .expect("project fixture");
    let trusted = config.load().expect("trusted project loads");
    assert_eq!(trusted.selected_target, ConfigTarget::User);
    assert_eq!(trusted.public_view()["config"]["api_key"], "[redacted]");
    assert_eq!(trusted.public_view()["config"]["privateKey"], "[redacted]");
    assert_eq!(trusted.public_view()["config"]["proxy"], "[redacted]");
    assert_eq!(
        trusted.public_view()["targetValues"]["project"]["api_key"],
        "[redacted]"
    );
    assert!(
        trusted.public_view()["layerValues"]
            .as_array()
            .is_some_and(|layers| layers.iter().all(|layer| {
                layer
                    .pointer("/values/api_key")
                    .is_none_or(|value| value == "[redacted]")
            }))
    );

    let revoked = config
        .with_project_trusted(false)
        .load()
        .expect("user fallback");
    assert_eq!(revoked.selected_target, ConfigTarget::User);
    assert_ne!(revoked.effective["active_model"].as_str(), Some("project"));
    assert_eq!(revoked.public_view()["targetValues"]["project"], json!({}));
}

#[test]
fn typed_mcp_stdio_entries_preserve_argv_limits_filters_and_working_directory() {
    let working_directory = PathBuf::from("/workspace");
    let snapshot = ConfigSnapshot {
        effective: table(
            r#"
[[mcp_servers]]
name = "fixture"
transport = "stdio"
command = "/usr/bin/fixture"
args = ["--stdio"]
env = { TOKEN = "secret" }
cwd = "tools"
startup_timeout_sec = 1.5
tool_timeout_sec = 2
disabled_tools = ["admin"]
"#,
        ),
        selected_target: ConfigTarget::User,
        selected_path: PathBuf::from("/home/user/.vibe/config.toml"),
        fingerprints: BTreeMap::new(),
        target_values: BTreeMap::new(),
        layer_values: Vec::new(),
        validation_warnings: Vec::new(),
        awaiting_experiment_model: false,
    };
    let servers = snapshot
        .mcp_servers(&working_directory)
        .expect("typed MCP configuration");
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].alias, "fixture");
    assert_eq!(servers[0].startup_timeout_ms, 1_500);
    assert_eq!(servers[0].tool_timeout_ms, 2_000);
    assert_eq!(
        servers[0].disabled_tools,
        BTreeSet::from(["admin".to_owned()])
    );
    assert!(matches!(
        &servers[0].transport,
        McpTransportConfig::Stdio {
            command,
            arguments,
            environment,
            working_directory: Some(cwd),
        } if command == "/usr/bin/fixture"
            && arguments == &["--stdio"]
            && environment.get("TOKEN").is_some_and(|value| value == "secret")
            && cwd == &working_directory.join("tools")
    ));
}

#[test]
fn integration_mutations_persist_and_mcp_collisions_fail_before_overwrite() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let store = config(temporary.path());
    let server = McpServerConfig {
        alias: "docs".to_owned(),
        transport: McpTransportConfig::StreamableHttp {
            url: Url::parse("https://mcp.example.test/rpc").expect("fixture URL"),
            headers: BTreeMap::new(),
        },
        enabled: true,
        disabled_tools: BTreeSet::new(),
        startup_timeout_ms: 1_500,
        tool_timeout_ms: 2_000,
        auth: Default::default(),
        prompt: None,
        sampling_enabled: true,
        declared: None,
    };

    store.persist_mcp_add(&server).expect("MCP persists");
    store
        .persist_mcp_state("docs", false, &BTreeSet::from(["search".to_owned()]))
        .expect("MCP state persists");
    let configured = store
        .load()
        .expect("configuration reloads")
        .mcp_servers(Path::new("/workspace"))
        .expect("MCP configuration decodes");
    assert_eq!(configured.len(), 1);
    assert!(!configured[0].enabled);
    assert_eq!(
        configured[0].disabled_tools,
        BTreeSet::from(["search".to_owned()])
    );

    assert!(store.preflight_mcp_add(&server).is_err());
    let same_url = McpServerConfig {
        alias: "other".to_owned(),
        ..server.clone()
    };
    assert!(store.preflight_mcp_add(&same_url).is_err());
}

#[test]
fn integration_mutations_never_copy_higher_layer_secrets_into_the_selected_file() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let store = config(temporary.path()).with_experiments(table(
        r#"
[[mcp_servers]]
name = "inherited"
transport = "stdio"
command = "/usr/bin/inherited"
env = { API_TOKEN = "must-not-be-copied" }
"#,
    ));
    let server = McpServerConfig {
        alias: "selected".to_owned(),
        transport: McpTransportConfig::StreamableHttp {
            url: Url::parse("https://selected.example.test/mcp").expect("fixture URL"),
            headers: BTreeMap::new(),
        },
        enabled: true,
        disabled_tools: BTreeSet::new(),
        startup_timeout_ms: 1_000,
        tool_timeout_ms: 1_000,
        auth: Default::default(),
        prompt: None,
        sampling_enabled: true,
        declared: None,
    };

    store
        .persist_mcp_add(&server)
        .expect("selected MCP persists");
    let aliases = store
        .load()
        .expect("merged integrations reload")
        .mcp_servers(Path::new("/workspace"))
        .expect("merged MCP configuration")
        .into_iter()
        .map(|config| config.alias)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        aliases,
        BTreeSet::from(["inherited".to_owned(), "selected".to_owned()])
    );

    store
        .persist_mcp_state("inherited", false, &BTreeSet::from(["search".to_owned()]))
        .expect("inherited MCP preference persists");
    let inherited = store
        .load()
        .expect("preference reloads")
        .mcp_servers(Path::new("/workspace"))
        .expect("effective MCP configuration")
        .into_iter()
        .find(|config| config.alias == "inherited")
        .expect("inherited MCP remains visible");
    assert!(!inherited.enabled);
    assert_eq!(
        inherited.disabled_tools,
        BTreeSet::from(["search".to_owned()])
    );

    let selected = fs::read_to_string(store.paths.user_config()).expect("selected file reads");
    assert!(selected.contains("selected"));
    assert!(selected.contains("inherited"));
    assert!(!selected.contains("/usr/bin/inherited"));
    assert!(!selected.contains("must-not-be-copied"));
}

#[test]
fn duplicate_mcp_entries_fail_without_echoing_secret_bearing_values() {
    let snapshot = ConfigSnapshot {
        effective: table(
            r#"
[[mcp_servers]]
name = "duplicate"
transport = "stdio"
command = "/first"

[[mcp_servers]]
name = "duplicate"
transport = "stdio"
command = "top-secret-command"
"#,
        ),
        selected_target: ConfigTarget::User,
        selected_path: PathBuf::from("/home/user/.vibe/config.toml"),
        fingerprints: BTreeMap::new(),
        target_values: BTreeMap::new(),
        layer_values: Vec::new(),
        validation_warnings: Vec::new(),
        awaiting_experiment_model: false,
    };
    let error = snapshot
        .mcp_servers(Path::new("/workspace"))
        .expect_err("duplicate aliases fail closed")
        .to_string();
    assert!(error.contains("unique"));
    assert!(!error.contains("top-secret-command"));
}

#[test]
fn unsupported_mcp_transport_fails_closed() {
    let snapshot = ConfigSnapshot {
        effective: table(
            r#"
[[mcp_servers]]
name = "invalid"
transport = "python"
command = "must-not-run"
"#,
        ),
        selected_target: ConfigTarget::User,
        selected_path: PathBuf::from("/home/user/.vibe/config.toml"),
        fingerprints: BTreeMap::new(),
        target_values: BTreeMap::new(),
        layer_values: Vec::new(),
        validation_warnings: Vec::new(),
        awaiting_experiment_model: false,
    };
    let error = snapshot
        .mcp_servers(Path::new("/workspace"))
        .expect_err("unsupported transport fails closed")
        .to_string();
    assert!(error.contains("`http`, `streamable-http` or `stdio`"));
    assert!(!error.contains("must-not-run"));
}

#[test]
fn recovery_rejects_journal_paths_outside_configured_targets() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let config = config(temporary.path());
    let victim = temporary.path().join("victim.txt");
    let backup = temporary.path().join(".config.1.bak");
    let temporary_file = temporary.path().join(".config.1.tmp");
    fs::write(&victim, "keep").expect("victim fixture");
    fs::write(&backup, "replace").expect("backup fixture");
    fs::write(&temporary_file, "temporary").expect("temporary fixture");
    let journal = ConfigJournal {
        state: JournalState::Prepared,
        entries: vec![transaction::JournalEntry {
            destination: victim.clone(),
            temporary: temporary_file,
            backup,
            had_original: true,
        }],
    };
    fs::write(
        config.paths.vibe_home.join(TRANSACTION_FILE),
        serde_json::to_vec(&journal).expect("journal serializes"),
    )
    .expect("journal fixture");

    assert!(matches!(config.load(), Err(ConfigError::UnsafeJournal(_))));
    assert_eq!(fs::read_to_string(victim).expect("victim remains"), "keep");
}
