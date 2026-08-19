use tempfile::tempdir;
use vibe_core::events::ModelMessage;
use vibe_protocol::{Envelope, TransportKind, decode_frame};

use super::*;
use crate::server::AppServer;

fn service() -> (tempfile::TempDir, WorkspaceService) {
    let temporary = tempdir().expect("tempdir");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let service = WorkspaceService::new(
        WorkspacePaths {
            vibe_home: temporary.path().join("home"),
            working_directory: workspace,
            session_root: temporary.path().join("sessions"),
        },
        true,
    )
    .expect("service");
    (temporary, service)
}

/// A parameter of the wrong type is named rather than replaced by a
/// default.
///
/// These readers used to run through a local `optional_string` that dropped
/// the rejection, so `cwd: 42` silently continued the wrong session and
/// `newSessionId: 42` silently forked under a generated identifier. Every
/// optional reader now reports what the parameter object holds.
#[test]
fn a_malformed_optional_parameter_is_refused_rather_than_defaulted() {
    let (_temporary, service) = service();
    for (method, params) in [
        ("session/continue", json!({"cwd": 42})),
        (
            "session/fork",
            json!({"sessionId": "source", "newSessionId": 42}),
        ),
        (
            "session/resume",
            json!({"sessionId": "source", "systemPrompt": 42}),
        ),
        ("session/list", json!({"cwd": 42})),
    ] {
        let params: BTreeMap<String, Value> =
            serde_json::from_value(params).expect("parameter object");
        let error = service
            .dispatch(method, &params)
            .expect_err("a malformed parameter is refused");
        assert!(
            matches!(error, WorkspaceServiceError::InvalidParams(ref message) if message.contains("must be a string")),
            "{method}: {error}"
        );
    }
}

/// A page outside what the store accepts is a parameter problem, so it is
/// answered as one instead of surfacing as a storage conflict.
#[test]
fn a_page_outside_the_store_bounds_is_refused_as_a_parameter() {
    let (_temporary, service) = service();
    for method in ["session/list", "history/list"] {
        let params: BTreeMap<String, Value> =
            serde_json::from_value(json!({"sessionId": "source", "limit": 100_000}))
                .expect("parameter object");
        let error = service
            .dispatch(method, &params)
            .expect_err("an out-of-range page is refused");
        assert!(
            matches!(error, WorkspaceServiceError::InvalidParams(ref message) if message.contains("limit")),
            "{method}: {error}"
        );
    }
}

/// The Discovered layer reaches a client through the method it reads the
/// configuration by, carrying the settings every declared tool publishes,
/// and a file the operator owns still wins over them.
#[test]
fn config_read_publishes_the_discovered_tool_settings_a_file_can_override() {
    let (temporary, service) = service();
    let home = temporary.path().join("home");
    fs::create_dir_all(&home).expect("home");
    fs::write(
        home.join("config.toml"),
        "[tools.web_fetch]\nmax_timeout = 7\n",
    )
    .expect("user fixture");

    let snapshot = service.config_document().expect("config reads");

    let discovered = snapshot["layerValues"]
        .as_array()
        .expect("layer values")
        .iter()
        .find(|layer| layer["layer"] == "discovered")
        .expect("the discovered layer is published under its own name");
    assert_eq!(
        snapshot["layers"][1], "discovered",
        "the layer composes between the defaults and the selected file"
    );
    assert!(
        discovered["values"]["tools"]["web_fetch"]["max_content_bytes"].is_number(),
        "{discovered}"
    );
    assert_eq!(
        discovered["values"]["tools"]["web_fetch"]["max_timeout"],
        120
    );
    // The file overrides the one option it names; the rest of the
    // discovered entry survives the deep merge.
    assert_eq!(snapshot["config"]["tools"]["web_fetch"]["max_timeout"], 7);
    assert!(
        snapshot["config"]["tools"]["web_fetch"]["max_content_bytes"].is_number(),
        "{snapshot}"
    );
    assert_eq!(snapshot["validationWarnings"], json!([]));

    // The resolver reads the same document, so the handler that runs next
    // waits the seven seconds the file asked for.
    let settings: vibe_core::tools::config::WebFetchConfig =
        service.tool_config().view("web_fetch");
    assert_eq!(settings.max_timeout, 7);
}

/// US-107: a permanent approval writes the patterns it granted into
/// `tools.<name>.allowlist`, which is the half of an approval that outlives
/// the session because the tool reads that list back on the next one.
#[tokio::test]
async fn a_permanent_approval_reaches_the_configured_allowlist() {
    let (_temporary, service) = service();
    let store = vibe_core::policy::PermissionStore::default()
        .with_tool_config(service.tool_config())
        .with_allowlist_persistence(service.allowlist_persistence());

    store
        .authorize(
            "bash",
            json!({"command": "cargo test"}),
            vibe_core::policy::PermissionContext::asking(vec![
                vibe_core::policy::PermissionRequirement::command("cargo test"),
            ]),
            &AlwaysPermanently,
        )
        .await
        .expect("the operator approves permanently");

    // The merge is against the list the tool actually reads, which already
    // carries the reference defaults the Discovered layer publishes, so the
    // approval adds one entry rather than replacing the list.
    let allowlist = |snapshot: &Value| {
        snapshot["config"]["tools"]["bash"]["allowlist"]
            .as_array()
            .expect("the allowlist is an array")
            .iter()
            .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
            .collect::<Vec<_>>()
    };
    let snapshot = service.config_document().expect("config reads");
    let after_first = allowlist(&snapshot);
    assert_eq!(
        after_first
            .iter()
            .filter(|entry| *entry == "cargo test *")
            .count(),
        1,
        "{after_first:?}"
    );
    assert!(after_first.contains(&"git status".to_owned()));
    assert!(store.diagnostics().is_empty(), "{:?}", store.diagnostics());

    // A second approval extends the same list rather than replacing it, and
    // the merge stays a sorted union.
    store
        .authorize(
            "bash",
            json!({"command": "cargo build --release"}),
            vibe_core::policy::PermissionContext::asking(vec![
                vibe_core::policy::PermissionRequirement::command("cargo build"),
            ]),
            &AlwaysPermanently,
        )
        .await
        .expect("the operator approves permanently again");
    let snapshot = service.config_document().expect("config reads");
    let after_second = allowlist(&snapshot);
    assert!(after_second.contains(&"cargo test *".to_owned()));
    assert!(after_second.contains(&"cargo build *".to_owned()));
    assert_eq!(
        after_second.len(),
        after_first.len() + 1,
        "{after_second:?}"
    );
    let mut sorted = after_second.clone();
    sorted.sort();
    assert_eq!(after_second, sorted, "the merge writes a sorted union");
}

struct AlwaysPermanently;

impl vibe_core::policy::ApprovalAgent for AlwaysPermanently {
    fn request<'a>(
        &'a self,
        _request: vibe_core::policy::ApprovalRequest,
    ) -> vibe_core::policy::ApprovalFuture<'a> {
        Box::pin(async move { Ok(vibe_core::policy::ApprovalDecision::ApprovePermanently) })
    }
}

#[test]
fn public_config_methods_preserve_unknown_values_and_redact_proxy_secrets() {
    let (_temporary, service) = service();
    let result = service
        .dispatch(
            "config/batchWrite",
            &BTreeMap::from([(
                "writes".to_owned(),
                json!([{
                    "target": "user",
                    "expectedFingerprint": null,
                    "mutations": [
                        {"path": ["future", "flag"], "value": true},
                        {"path": ["proxy"], "value": "https://proxy.example"}
                    ]
                }]),
            )]),
        )
        .expect("write");
    assert_eq!(
        result.result["snapshot"]["config"]["future"]["flag"],
        json!(true)
    );
    assert_eq!(
        result.result["snapshot"]["config"]["proxy"],
        json!("[redacted]")
    );
    assert_eq!(
        service
            .dispatch("config/proxy/read", &BTreeMap::new())
            .expect("proxy read")
            .result["settings"]["values"]["HTTP_PROXY"],
        Value::Null
    );
}

#[test]
fn proxy_environment_round_trips_all_supported_keys_and_preserves_other_values() {
    let (temporary, service) = service();
    fs::create_dir_all(temporary.path().join("home")).expect("proxy home");
    fs::write(
        temporary.path().join("home/.env"),
        "MISTRAL_API_KEY='preserved'\nHTTP_PROXY='old'\n",
    )
    .expect("dotenv fixture");
    service
        .dispatch(
            "config/proxy/write",
            &BTreeMap::from([(
                "changes".to_owned(),
                json!({
                    "HTTP_PROXY": "https://proxy.example",
                    "HTTPS_PROXY": "https://secure-proxy.example",
                    "ALL_PROXY": "socks5://proxy.example",
                    "NO_PROXY": "localhost,.internal",
                    "SSL_CERT_FILE": "/certs/ca.pem",
                    "SSL_CERT_DIR": "/certs",
                }),
            )]),
        )
        .expect("proxy write");
    let dispatch = service
        .dispatch("config/proxy/read", &BTreeMap::new())
        .expect("proxy read");
    assert_eq!(
        dispatch.result["settings"]["values"]["NO_PROXY"],
        "localhost,.internal"
    );
    let persisted =
        fs::read_to_string(temporary.path().join("home/.env")).expect("dotenv persisted");
    assert!(persisted.contains("MISTRAL_API_KEY='preserved'"));
    assert!(persisted.contains("SSL_CERT_DIR='/certs'"));
}

#[test]
fn proxy_environment_rejects_unknown_keys_without_mutation() {
    let (temporary, service) = service();
    fs::create_dir_all(temporary.path().join("home")).expect("proxy home");
    fs::write(temporary.path().join("home/.env"), "HTTP_PROXY='old'\n").expect("dotenv fixture");
    let error = service
        .dispatch(
            "config/proxy/write",
            &BTreeMap::from([("changes".to_owned(), json!({"BAD_PROXY": "value"}))]),
        )
        .expect_err("unknown key rejected");
    assert!(matches!(error, WorkspaceServiceError::InvalidParams(_)));
    assert_eq!(
        fs::read_to_string(temporary.path().join("home/.env")).expect("unchanged"),
        "HTTP_PROXY='old'\n"
    );
}

#[test]
fn configured_default_agent_is_resolved_from_the_live_config_snapshot() {
    let (_temporary, service) = service();
    service
        .dispatch(
            "config/batchWrite",
            &BTreeMap::from([(
                "writes".to_owned(),
                json!([{
                    "target": "user",
                    "expectedFingerprint": null,
                    "mutations": [{"path": ["default_agent"], "value": "plan"}]
                }]),
            )]),
        )
        .expect("default agent config");

    assert_eq!(
        service.default_agent_name().expect("configured agent"),
        "plan"
    );
}

#[test]
fn discovered_user_agent_remains_selectable_after_service_restart() {
    let temporary = tempdir().expect("tempdir");
    let workspace = temporary.path().join("workspace");
    let vibe_home = temporary.path().join("home");
    let agents = vibe_home.join("extensions/agents");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&agents).expect("agent directory");
    fs::write(
        agents.join("reviewer.toml"),
        "display_name = \"Reviewer\"\ndescription = \"Custom reviewer\"\nagent_type = \"agent\"\n",
    )
    .expect("custom agent");

    let service = WorkspaceService::new(
        WorkspacePaths {
            session_root: temporary.path().join("sessions"),
            vibe_home,
            working_directory: workspace,
        },
        true,
    )
    .expect("restarted service");

    assert_eq!(
        service
            .agent_profile("reviewer")
            .expect("discovered profile")
            .display_name,
        "Reviewer"
    );
    fs::write(
            agents.join("reviewer.toml"),
            "display_name = \"Reloaded Reviewer\"\ndescription = \"Custom reviewer\"\nagent_type = \"agent\"\n",
        )
        .expect("updated custom agent");
    assert_eq!(
        service
            .agent_profile("reviewer")
            .expect("reloaded profile")
            .display_name,
        "Reloaded Reviewer"
    );
    assert!(
        service
            .dispatch("agents/list", &BTreeMap::new())
            .expect("agents list")
            .result["agents"]
            .as_array()
            .is_some_and(|agents| agents.iter().any(|agent| agent["name"] == "reviewer"))
    );
}

fn patch(ops: Value) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("ops".to_owned(), ops),
        ("reason".to_owned(), json!("config screen edit")),
        ("reloadRuntime".to_owned(), json!(false)),
    ])
}

fn digest(path: &Path) -> Option<Vec<u8>> {
    fs::read(path).ok()
}

/// Reference `_config_patch`: a client addresses a field by pointer, and the
/// server decides which file backs it.
#[test]
fn config_patch_writes_by_pointer_and_reports_the_keys_it_moved() {
    let (temporary, service) = service();
    let user = temporary.path().join("home/config.toml");

    let written = service
        .dispatch(
            "config/patch",
            &patch(json!([
                {"op": "set", "path": "/theme", "value": "nord"},
                {"op": "set", "path": "/tools/bash/allowlist", "value": ["git status"]},
            ])),
        )
        .expect("patch applies");

    assert_eq!(written.result["rejected"], json!(false));
    assert_eq!(written.result["failures"], json!([]));
    // The answer names what failed, not what moved: `ConfigPatchResponse`
    // declares no room for the changed keys, so the effect is read from the
    // configuration the patch produced.
    assert!(!written.result.contains_key("changedKeys"));
    let document = service.config_document().expect("config read");
    assert_eq!(document["config"]["theme"], json!("nord"));
    assert_eq!(
        document["config"]["tools"]["bash"]["allowlist"],
        json!(["git status"]),
        "the intermediate tables the leaf needs were created"
    );
    assert!(
        fs::read_to_string(&user)
            .expect("the user file was written")
            .contains("nord")
    );

    let removed = service
        .dispatch(
            "config/patch",
            &patch(json!([{"op": "remove", "path": "/theme"}])),
        )
        .expect("removal applies");
    assert_eq!(removed.result["rejected"], json!(false));
    assert_eq!(
        service.config_document().expect("config read")["config"]["theme"],
        json!("auto"),
        "removing the override falls back to the shipped default"
    );

    // A table that already exists is diffed down to the leaf that moved.
    let deepened = service
            .dispatch(
                "config/patch",
                &patch(json!([{"op": "set", "path": "/tools/bash/allowlist", "value": ["git status", "ls"]}])),
            )
            .expect("deep set applies");
    assert_eq!(deepened.result["failures"], json!([]));
    assert_eq!(
        service.config_document().expect("config read")["config"]["tools"]["bash"]["allowlist"],
        json!(["git status", "ls"])
    );

    // A patch that writes the value already in place is answered the same
    // way rather than being refused.
    let repeated = service
            .dispatch(
                "config/patch",
                &patch(json!([{"op": "set", "path": "/tools/bash/allowlist", "value": ["git status", "ls"]}])),
            )
            .expect("repeat applies");
    assert_eq!(repeated.result["rejected"], json!(false));
    assert_eq!(repeated.result["failures"], json!([]));
}

/// The preflight runs against the merged configuration and refuses the whole
/// request, so nothing reaches disk. Reference `ConfigPatchValidationError`.
#[test]
fn a_rejected_patch_leaves_every_configuration_file_byte_identical() {
    let (temporary, service) = service();
    let user = temporary.path().join("home/config.toml");
    service
        .dispatch(
            "config/patch",
            &patch(json!([{"op": "set", "path": "/theme", "value": "nord"}])),
        )
        .expect("seed applies");
    let before = digest(&user).expect("the user file exists");

    for ops in [
        // Leaves no configured model behind.
        json!([{"op": "set", "path": "/models", "value": {}}]),
        // Traverses a scalar the merged document already carries.
        json!([{"op": "set", "path": "/theme/nested", "value": true}]),
        // Names a list position that does not exist.
        json!([{"op": "remove", "path": "/providers/9"}]),
    ] {
        let rejected = service
            .dispatch("config/patch", &patch(ops.clone()))
            .expect("the request is answered rather than raised");
        assert_eq!(rejected.result["rejected"], json!(true), "{ops}");
        assert_eq!(rejected.result["failures"], json!([]));
        assert_eq!(
            digest(&user),
            Some(before.clone()),
            "{ops} touched the file"
        );
    }
    assert_eq!(
        service.config_document().expect("config read")["config"]["theme"],
        json!("nord")
    );
}

/// The reference applies each layer on its own once the preflight passes, so
/// one file failing is reported rather than undoing the file that worked.
#[test]
fn a_write_that_cannot_land_is_reported_per_target_beside_one_that_did() {
    let (temporary, service) = service();
    let project = temporary.path().join("workspace/.vibe");
    fs::create_dir_all(&project).expect("project directory");
    fs::write(project.join("config.toml"), "theme = \"nord\"\n").expect("project fixture");

    let outcome = service
        .dispatch(
            "config/patch",
            &patch(json!([
                {"op": "set", "path": "/theme", "value": "dracula", "targetLayer": "project"},
                // `displayed_workdir` only exists in the defaults layer, so
                // the merged preflight resolves it and the user file cannot.
                {"op": "remove", "path": "/displayed_workdir", "targetLayer": "user"},
            ])),
        )
        .expect("the patch is applied per target");

    assert_eq!(outcome.result["rejected"], json!(false));
    let failures = outcome.result["failures"]
        .as_array()
        .expect("failures are reported as a list");
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0]
            .as_str()
            .is_some_and(|failure| failure.contains("/displayed_workdir")),
        "{failures:?}"
    );
    assert_eq!(
        service.config_document().expect("config read")["config"]["theme"],
        json!("dracula"),
        "the write that succeeded stands"
    );

    // An operation naming no target goes to the file the selection resolves
    // to, which is the trusted project file now that one exists.
    service
        .dispatch(
            "config/patch",
            &patch(json!([{"op": "set", "path": "/default_agent", "value": "plan"}])),
        )
        .expect("the unrouted patch applies");
    assert!(
        fs::read_to_string(project.join("config.toml"))
            .expect("the project file survives")
            .contains("plan")
    );
    assert!(
        digest(&temporary.path().join("home/config.toml")).is_none(),
        "an unrouted operation reached the user file"
    );
}

#[test]
fn a_patch_aimed_at_an_untrusted_project_changes_nothing() {
    let temporary = tempdir().expect("tempdir");
    let workspace = temporary.path().join("workspace");
    fs::create_dir_all(workspace.join(".vibe")).expect("project directory");
    let service = WorkspaceService::new(
        WorkspacePaths {
            vibe_home: temporary.path().join("home"),
            working_directory: workspace.clone(),
            session_root: temporary.path().join("sessions"),
        },
        false,
    )
    .expect("untrusted service");

    let error = service
        .dispatch(
            "config/patch",
            &patch(json!([
                {"op": "set", "path": "/theme", "value": "nord", "targetLayer": "project"},
                {"op": "set", "path": "/default_agent", "value": "plan"},
            ])),
        )
        .expect_err("an untrusted project is refused");

    assert!(
        matches!(&error, WorkspaceServiceError::Config(message) if message.contains("trust")),
        "{error}"
    );
    assert!(
        digest(&workspace.join(".vibe/config.toml")).is_none(),
        "the project file was created despite the refusal"
    );
    assert!(
        digest(&temporary.path().join("home/config.toml")).is_none(),
        "the user half of the patch was written despite the refusal"
    );
}

#[test]
fn config_patch_rejects_a_malformed_operation_before_it_reaches_the_store() {
    let (_temporary, service) = service();
    for ops in [
        json!([{"op": "toggle", "path": "/theme", "value": "nord"}]),
        json!([{"op": "set", "path": "theme", "value": "nord"}]),
        json!([{"op": "set", "value": "nord"}]),
        json!([{"op": "set", "path": "/theme", "value": "nord", "targetLayer": "global"}]),
    ] {
        let error = service
            .dispatch("config/patch", &patch(ops.clone()))
            .expect_err("the operation is refused");
        assert!(
            matches!(error, WorkspaceServiceError::InvalidParams(_)),
            "{ops}"
        );
    }
    assert!(
        service
            .dispatch("config/patch", &BTreeMap::new())
            .is_err_and(|error| matches!(error, WorkspaceServiceError::InvalidParams(_)))
    );
}

/// Reference `_config_fields_read`: the settings screen renders from this
/// answer alone.
#[test]
fn config_fields_read_describes_the_published_surface_and_its_targets() {
    let (_temporary, service) = service();
    service
        .dispatch(
            "config/patch",
            &patch(json!([{"op": "set", "path": "/theme", "value": "nord"}])),
        )
        .expect("seed applies");

    let response = service
        .dispatch("config/fields/read", &BTreeMap::new())
        .expect("fields read");
    let fields = response.result["fields"]
        .as_array()
        .expect("the response carries a field list");
    assert_eq!(response.result["targets"], json!(["user", "project"]));
    for hidden in vibe_core::config::HIDDEN_FIELDS {
        assert!(
            fields.iter().all(|field| field["name"] != json!(hidden)),
            "`{hidden}` is filled by a runtime and has no editor on either side"
        );
    }
    assert_eq!(
        fields.len(),
        vibe_core::config::registry::FIELDS
            .iter()
            .filter(|spec| {
                spec.published && !vibe_core::config::HIDDEN_FIELDS.contains(&spec.name)
            })
            .count()
    );

    let theme = fields
        .iter()
        .find(|field| field["name"] == json!("theme"))
        .expect("theme is described");
    assert_eq!(theme["kind"], json!("enum"));
    assert_eq!(theme["path"], json!("/theme"));
    assert_eq!(theme["value"], json!("nord"));
    assert_eq!(theme["popular"], json!(true));
    assert!(
        theme["enumChoices"]
            .as_array()
            .is_some_and(|choices| choices.contains(&json!("nord")))
    );
    assert!(
        theme["description"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    );
    assert_eq!(
        theme["layerValues"],
        json!([
            {"layer": "selected_toml", "value": "nord"},
            {"layer": "defaults", "value": "auto"},
        ]),
        "layer values run from the highest priority down to the defaults"
    );
    assert_eq!(
        fields
            .iter()
            .filter(|field| field["popular"] == json!(true))
            .count(),
        12,
        "the popular set is the reference one"
    );
}

/// Reference `ConfigSchemaReadResponse`: a version token beside the schema
/// object, so a client can cache the surface it names.
#[test]
fn config_schema_publishes_every_declared_field_with_a_version_token() {
    let (_temporary, service) = service();
    let response = service
        .dispatch("config/schema", &BTreeMap::new())
        .expect("config schema");

    let version = response.result["configSchemaVersion"]
        .as_str()
        .expect("the response carries a version token");
    assert!(version.starts_with("sha256:"), "{version}");
    let properties = response.result["schema"]["properties"]
        .as_object()
        .expect("the schema declares properties");
    assert_eq!(properties.len(), vibe_core::config::registry::FIELDS.len());
    for field in vibe_core::config::registry::FIELDS {
        assert!(
            properties.contains_key(field.name),
            "`{}` is not published",
            field.name
        );
    }
    // A settings screen renders these directly, so their shape is asserted
    // rather than assumed.
    assert_eq!(
        properties["auto_compact_threshold"]["type"],
        json!("integer")
    );
    assert_eq!(
        properties["auto_compact_threshold"]["default"],
        json!(200_000)
    );
    assert_eq!(properties["api_timeout"]["type"], json!("number"));
    assert_eq!(
        properties["otel_redaction"]["enum"],
        json!(["default", "none", "strict"])
    );

    let again = service
        .dispatch("config/schema", &BTreeMap::new())
        .expect("config schema");
    assert_eq!(again.result, response.result, "the schema is not cacheable");
}

/// The Defaults layer is the shipped document at every construction site,
/// so a session opened without a configuration file still reads the
/// reference defaults.
#[test]
fn config_read_composes_the_shipped_defaults_without_a_configuration_file() {
    let (_temporary, service) = service();
    let snapshot = service.config_document().expect("config read");
    let config = &snapshot["config"];

    // The shipped document carries the reference's unpinned sentinel; the
    // alias it resolves to is read through the snapshot, never off the key.
    assert_eq!(config["active_model"], json!(""));
    assert_eq!(
        service
            .layered_config()
            .load()
            .expect("config loads")
            .active_model_alias(),
        Some("mistral-medium-3.5")
    );
    assert_eq!(config["theme"], json!("auto"));
    assert_eq!(config["auto_compact_threshold"], json!(200_000));
    assert_eq!(
        config["models"]["local"]["provider"],
        json!("llamacpp"),
        "models are read back keyed by alias"
    );
    assert_eq!(
        snapshot["validationWarnings"],
        json!([]),
        "the shipped defaults need no repair"
    );
    let layers = snapshot["layerValues"]
        .as_array()
        .expect("the snapshot lists its layers");
    let defaults = layers
        .iter()
        .find(|layer| layer["layer"] == json!("defaults"))
        .expect("the defaults layer is composed");
    assert!(
        defaults["values"]
            .as_object()
            .is_some_and(|values| values.len() > 50),
        "the defaults layer is empty"
    );
}

#[test]
fn builtin_lean_agent_installation_is_persisted_and_reflected_in_agent_listing() {
    let (_temporary, service) = service();
    let before = service
        .dispatch("agents/list", &BTreeMap::new())
        .expect("agents list");
    assert!(
        before.result["agents"]
            .as_array()
            .is_some_and(|agents| agents.iter().all(|agent| agent["name"] != "lean"))
    );

    service
        .dispatch(
            "agents/install",
            &BTreeMap::from([("agentName".to_owned(), json!("lean"))]),
        )
        .expect("lean install");
    let installed = service
        .dispatch("agents/list", &BTreeMap::new())
        .expect("installed agents list");
    assert!(
        installed.result["agents"]
            .as_array()
            .is_some_and(|agents| agents.iter().any(|agent| agent["name"] == "lean"))
    );
    let config = service.config_document().expect("config after install");
    assert_eq!(config["config"]["installed_agents"], json!(["lean"]));

    service
        .dispatch(
            "agents/uninstall",
            &BTreeMap::from([("agentName".to_owned(), json!("lean"))]),
        )
        .expect("lean uninstall");
    let after = service
        .dispatch("agents/list", &BTreeMap::new())
        .expect("agents after uninstall");
    assert!(
        after.result["agents"]
            .as_array()
            .is_some_and(|agents| agents.iter().all(|agent| agent["name"] != "lean"))
    );
}

#[test]
fn switching_away_from_auto_approve_removes_its_permission_override() {
    let (_temporary, service) = service();
    let session_id = "agent-switch";
    let working_directory = service
        .paths
        .working_directory
        .to_string_lossy()
        .into_owned();
    service
        .create_runtime_session(session_id, &working_directory, 1)
        .expect("session");
    let mut metadata = service.store.load(session_id).expect("metadata").metadata;
    metadata
        .config
        .insert("active_model".to_owned(), json!("base-model"));
    service
        .store
        .update_metadata(&metadata)
        .expect("base session config");

    for name in ["auto-approve", "default"] {
        let update = service
            .dispatch(
                "session/agent/update",
                &BTreeMap::from([
                    ("sessionId".to_owned(), json!(session_id)),
                    ("name".to_owned(), json!(name)),
                ]),
            )
            .expect("agent update");
        assert_eq!(
            update
                .attachment
                .as_ref()
                .and_then(|attachment| attachment.agent.as_deref()),
            Some(name)
        );
    }

    let metadata = service.store.load(session_id).expect("metadata").metadata;
    assert_eq!(
        metadata
            .agent_profile
            .as_ref()
            .and_then(|profile| profile.get("name")),
        Some(&json!("default"))
    );
    assert!(
        !metadata.config.contains_key("bypass_tool_permissions"),
        "the prior auto-approve override must not survive"
    );
    assert_eq!(
        metadata.config.get("active_model"),
        Some(&json!("base-model")),
        "agent overlays must not destroy the underlying session config"
    );
}

#[test]
fn rewind_resolves_an_entry_identity_and_forks_before_the_selected_message() {
    let (_temporary, service) = service();
    let session_id = "rewind-source";
    let working_directory = service
        .paths
        .working_directory
        .to_string_lossy()
        .into_owned();
    let mut hydrated = service
        .create_runtime_session(session_id, &working_directory, 1)
        .expect("session");
    for (offset, message) in [
        ModelMessage::user("first question".to_owned()),
        ModelMessage::Assistant {
            content: "first answer".to_owned(),
            reasoning: None,
            reasoning_signature: None,
            reasoning_state: Vec::new(),
            tool_calls: Vec::new(),
        },
        ModelMessage::user("edit this question".to_owned()),
        ModelMessage::Assistant {
            content: "second answer".to_owned(),
            reasoning: None,
            reasoning_signature: None,
            reasoning_state: Vec::new(),
            tool_calls: Vec::new(),
        },
    ]
    .iter()
    .enumerate()
    {
        service
            .store
            .append_message(
                &mut hydrated.metadata,
                message,
                u64::try_from(offset).unwrap_or_default().saturating_add(2),
            )
            .expect("append message");
    }

    // A rewindable point is addressed by the identity a stored user
    // message carries, and this service answers the two fields only the
    // transcript decides; the paths come from the session's engine.
    let preview = service
        .dispatch(
            "session/rewind/read",
            &BTreeMap::from([
                ("sessionId".to_owned(), json!(session_id)),
                ("entryId".to_owned(), json!("history:2:user")),
            ]),
        )
        .expect("rewind preview");
    assert_eq!(preview.result["hasFileChanges"], json!(false));
    assert_eq!(preview.result["paths"], json!([]));
    assert!(
        matches!(
            service.dispatch(
                "session/rewind/read",
                &BTreeMap::from([
                    ("sessionId".to_owned(), json!(session_id)),
                    ("entryId".to_owned(), json!("history:1:user")),
                ]),
            ),
            Err(WorkspaceServiceError::NotFound(_))
        ),
        "an assistant message is not a rewindable entry"
    );

    let rewind = service
        .dispatch(
            "session/rewind",
            &BTreeMap::from([
                ("sessionId".to_owned(), json!(session_id)),
                ("entryId".to_owned(), json!("history:2:user")),
                ("restoreFiles".to_owned(), json!(false)),
                ("statistics".to_owned(), json!({"tokens": 17})),
            ]),
        )
        .expect("rewind");
    let child = rewind.attachment.expect("child attachment");
    assert_eq!(child.parent_session_id.as_deref(), Some(session_id));
    assert_eq!(rewind.result["message"], json!("edit this question"));
    assert_eq!(child.hydrated.messages.len(), 2);
    assert_eq!(child.hydrated.metadata.statistics["tokens"], 17);
    assert_eq!(
        service
            .store
            .load(session_id)
            .expect("source remains")
            .messages
            .len(),
        4
    );
}

#[test]
fn caller_paths_cannot_expand_server_authorized_roots() {
    let (temporary, service) = service();
    let outside = temporary.path().join("outside");
    std::fs::create_dir(&outside).expect("outside directory");
    std::fs::write(
        outside.join("agent.toml"),
        "display_name = \"Outside\"\nagent_type = \"agent\"\n",
    )
    .expect("outside agent");
    let install = service.dispatch(
        "agents/install",
        &BTreeMap::from([("path".to_owned(), json!(outside.join("agent.toml")))]),
    );
    assert!(matches!(
        install,
        Err(WorkspaceServiceError::InvalidParams(_))
    ));
    let prompt = service.dispatch(
        "workspace/prompt/prepare",
        &BTreeMap::from([
            ("base".to_owned(), json!("base")),
            ("addDirectories".to_owned(), json!([outside])),
        ]),
    );
    assert!(matches!(
        prompt,
        Err(WorkspaceServiceError::InvalidParams(_))
    ));
}

#[test]
fn project_mcp_stdio_config_activates_only_after_workspace_trust() {
    let (temporary, service) = service();
    fs::create_dir_all(temporary.path().join("workspace/.vibe")).expect("project config root");
    fs::write(
        temporary.path().join("workspace/.vibe/config.toml"),
        r#"
[[mcp_servers]]
name = "fixture"
transport = "stdio"
command = "/usr/bin/fixture"
args = ["--stdio"]
env = { MODE = "test" }
cwd = "."
startup_timeout_sec = 1
tool_timeout_sec = 2
"#,
    )
    .expect("project MCP config");

    assert!(
        service
            .mcp_servers_for_session(&temporary.path().join("workspace"), false, &[])
            .expect("untrusted user fallback")
            .is_empty()
    );
    let trusted = service
        .mcp_servers_for_session(&temporary.path().join("workspace"), true, &[])
        .expect("trusted project MCP config");
    assert_eq!(trusted.len(), 1);
    assert_eq!(trusted[0].alias, "fixture");
    assert_eq!(trusted[0].startup_timeout_ms, 1_000);
    assert_eq!(trusted[0].tool_timeout_ms, 2_000);

    let runtime = json!({
        "name": "runtime",
        "transport": "stdio",
        "command": "/must-not-run"
    });
    assert!(matches!(
        service.mcp_servers_for_session(
            &temporary.path().join("workspace"),
            false,
            &[runtime]
        ),
        Err(WorkspaceServiceError::InvalidParams(message))
            if message.contains("trusted workspace")
    ));
}

#[test]
fn saved_session_methods_create_independent_runtime_attachments() {
    let (_temporary, service) = service();
    let mut metadata = service
        .store
        .create("parent", "/workspace", None, 1)
        .expect("create");
    service
        .store
        .append_message(&mut metadata, &ModelMessage::user("hello".to_owned()), 2)
        .expect("append");
    let fork = service
        .dispatch(
            "session/fork",
            &BTreeMap::from([
                ("sessionId".to_owned(), json!("parent")),
                ("newSessionId".to_owned(), json!("child")),
                ("systemPrompt".to_owned(), json!("fresh")),
                ("config".to_owned(), json!({"model": "child"})),
            ]),
        )
        .expect("fork");
    assert_eq!(
        fork.attachment.expect("attachment").parent_session_id,
        Some("parent".to_owned())
    );
    assert_eq!(fork.result["currentConfig"]["model"], "child");
    assert_eq!(
        service
            .dispatch(
                "session/list",
                &BTreeMap::from([("limit".to_owned(), json!(50))]),
            )
            .expect("list")
            .result["sessions"]
            .as_array()
            .expect("array")
            .len(),
        2
    );
}

/// US-072: the global dotenv file stands in for a `VIBE_*` variable the
/// process does not export, all the way through to the composed
/// configuration.
#[test]
fn the_global_dotenv_file_feeds_the_environment_layer() {
    let temporary = tempdir().expect("tempdir");
    let vibe_home = temporary.path().join("home");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir_all(&vibe_home).expect("vibe home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(
        vibe_home.join(".env"),
        "VIBE_THEME=nord\nMISTRAL_API_KEY=secret\n",
    )
    .expect("dotenv fixture");

    let service = WorkspaceService::new(
        WorkspacePaths {
            vibe_home: vibe_home.clone(),
            working_directory: workspace,
            session_root: temporary.path().join("sessions"),
        },
        true,
    )
    .expect("service");
    let snapshot = service.config_document().expect("configuration reads");

    assert_eq!(snapshot["config"]["theme"], json!("nord"));
    // The credential in the same file is not a configuration field and
    // never reaches the published document.
    assert!(
        !snapshot.to_string().contains("secret"),
        "no dotenv secret reaches the configuration surface"
    );
}

/// US-073: the startup step brings an older file forward; constructing the
/// service does not.
#[test]
fn the_startup_migration_rewrites_the_user_file() {
    let temporary = tempdir().expect("tempdir");
    let vibe_home = temporary.path().join("home");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir_all(&vibe_home).expect("vibe home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let user_path = vibe_home.join("config.toml");
    std::fs::write(&user_path, "disabled_tools = [\"search_replace\"]\n").expect("user fixture");

    let service = WorkspaceService::new(
        WorkspacePaths {
            vibe_home,
            working_directory: workspace,
            session_root: temporary.path().join("sessions"),
        },
        true,
    )
    .expect("service");
    assert_eq!(
        std::fs::read_to_string(&user_path).expect("user file"),
        "disabled_tools = [\"search_replace\"]\n",
        "building the service leaves the file alone"
    );

    assert!(
        service
            .migrate_configuration()
            .expect("migrations run")
            .is_empty()
    );

    assert!(
        std::fs::read_to_string(&user_path)
            .expect("user file")
            .contains("edit")
    );
    let snapshot = service.config_document().expect("configuration reads");
    assert_eq!(snapshot["config"]["disabled_tools"], json!(["edit"]));
}

/// US-071: an added directory is a project root of its own, so its
/// `.vibe/hooks.toml` and the rest of its extensions are read, ahead of the
/// user-level file.
#[test]
fn an_added_directory_contributes_its_own_extension_root() {
    let temporary = tempdir().expect("tempdir");
    let workspace = temporary.path().join("workspace");
    let added = temporary.path().join("library");
    let vibe_home = temporary.path().join("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(added.join(".vibe/commands")).expect("added extensions");
    std::fs::create_dir_all(vibe_home.join("extensions")).expect("user extensions");
    std::fs::write(
        added.join(".vibe/commands/release.md"),
        "Cut a release build.\n",
    )
    .expect("command fixture");
    let hook = "[[hooks]]\nname = \"%NAME%\"\ntype = \"pre_tool\"\nprogram = \"echo\"\n";
    std::fs::write(
        added.join(".vibe/hooks.toml"),
        hook.replace("%NAME%", "project-hook"),
    )
    .expect("project hook fixture");
    std::fs::write(
        vibe_home.join("extensions/hooks.toml"),
        hook.replace("%NAME%", "user-hook"),
    )
    .expect("user hook fixture");

    let service = WorkspaceService::new(
        WorkspacePaths {
            vibe_home,
            working_directory: workspace.clone(),
            session_root: temporary.path().join("sessions"),
        },
        true,
    )
    .expect("service")
    .with_allowed_roots(vec![added.clone(), added.clone(), workspace]);

    assert_eq!(
        service.discovery_roots.project,
        vec![
            service.paths.working_directory.join(".vibe"),
            added.join(".vibe")
        ],
        "the working directory leads, the added directory follows once"
    );
    let catalog = service.catalog();
    assert!(
        catalog.commands.contains_key("release"),
        "the added directory's commands are discovered"
    );
    assert_eq!(
        catalog
            .hooks
            .iter()
            .map(|hook| hook.name.as_str())
            .collect::<Vec<_>>(),
        vec!["project-hook", "user-hook"],
        "each open root's hook file is read, then the user-level one"
    );
}

/// The names `skills/list` publishes, which is the wire surface every
/// discovery criterion is finally measured on.
fn listed_skills(service: &WorkspaceService) -> Vec<String> {
    service
        .dispatch("skills/list", &BTreeMap::new())
        .expect("skills/list answers")
        .result["skills"]
        .as_array()
        .expect("the response carries an array")
        .iter()
        .filter_map(|skill| skill["name"].as_str().map(ToOwned::to_owned))
        .collect()
}

fn write_skill(root: &Path, name: &str) {
    let directory = root.join(name);
    std::fs::create_dir_all(&directory).expect("skill directory");
    std::fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: a {name}\n---\nbody\n"),
    )
    .expect("skill fixture");
}

/// US-166: `skill_paths` is read from the merged document per catalog build,
/// so writing the key changes what the next `skills/list` publishes, from
/// whichever file the configuration selected.
///
/// A user entry and a project entry do not both survive here, and the
/// reason is not skills: this port composes one selected TOML layer, the
/// project file while the workspace is trusted and the user file otherwise
/// (`crates/vibe-core/src/config.rs:585`), so no key concatenates across the
/// two files and a user document is discarded whole once a trusted project
/// ships its own. That is configuration layering rather than skill discovery,
/// and out of scope for this test.
#[test]
fn skill_paths_is_read_from_the_merged_document() {
    let temporary = tempdir().expect("tempdir");
    let workspace = temporary.path().join("workspace");
    let vibe_home = temporary.path().join("home");
    let user_skills = temporary.path().join("from-user");
    let project_skills = temporary.path().join("from-project");
    std::fs::create_dir_all(workspace.join(".vibe")).expect("workspace");
    std::fs::create_dir_all(&vibe_home).expect("vibe home");
    write_skill(&user_skills, "from-user");
    write_skill(&project_skills, "from-project");

    let service = WorkspaceService::new(
        WorkspacePaths {
            vibe_home: vibe_home.clone(),
            working_directory: workspace.clone(),
            session_root: temporary.path().join("sessions"),
        },
        true,
    )
    .expect("service");
    assert!(
        !listed_skills(&service).contains(&"from-user".to_owned()),
        "nothing names the directory yet"
    );

    std::fs::write(
        vibe_home.join("config.toml"),
        format!("skill_paths = [{:?}]\n", user_skills.to_string_lossy()),
    )
    .expect("user fixture");
    assert_eq!(
        listed_skills(&service),
        vec!["from-user", "skill-creator", "vibe"],
        "the key is re-read per build, so the next one publishes what it names"
    );

    std::fs::write(
        workspace.join(".vibe/config.toml"),
        format!("skill_paths = [{:?}]\n", project_skills.to_string_lossy()),
    )
    .expect("project fixture");
    assert_eq!(
        listed_skills(&service),
        vec!["from-project", "skill-creator", "vibe"],
        "the selected file moves to the trusted project's, and its entry is read"
    );
}

/// US-167: the two filter keys are read from the same document and narrow
/// what the wire publishes, with the allowlist deciding alone.
#[test]
fn the_skill_filters_narrow_what_the_wire_publishes() {
    let temporary = tempdir().expect("tempdir");
    let workspace = temporary.path().join("workspace");
    let vibe_home = temporary.path().join("home");
    std::fs::create_dir_all(&vibe_home).expect("vibe home");
    for name in ["alpha", "beta"] {
        write_skill(&workspace.join(".vibe/skills"), name);
    }
    let service = WorkspaceService::new(
        WorkspacePaths {
            vibe_home: vibe_home.clone(),
            working_directory: workspace,
            session_root: temporary.path().join("sessions"),
        },
        true,
    )
    .expect("service");
    let published = |document: &str| {
        std::fs::write(vibe_home.join("config.toml"), document).expect("user fixture");
        listed_skills(&service)
    };

    assert_eq!(
        published("disabled_skills = [\"beta\"]\n"),
        vec!["alpha", "skill-creator", "vibe"],
        "the denylist withholds its match and leaves the seeded builtins published"
    );
    assert_eq!(
        published("enabled_skills = [\"beta\"]\ndisabled_skills = [\"beta\"]\n"),
        vec!["beta"],
        "the allowlist decides alone and the denylist is not consulted"
    );
}

/// US-179: `experimental_enable_registry_skills` is read from the merged
/// document and gates a subtree that is dormant upstream. False, the
/// default, runs no registry code and creates no cache directory; true
/// with no registry configured changes nothing either, because the
/// reference publishes no load lifecycle at the pin, and it surfaces no
/// error.
#[test]
fn the_registry_experiment_key_gates_a_dormant_subtree() {
    let temporary = tempdir().expect("tempdir");
    let vibe_home = temporary.path().join("home");
    std::fs::create_dir_all(&vibe_home).expect("vibe home");
    let service = WorkspaceService::new(
        WorkspacePaths {
            vibe_home: vibe_home.clone(),
            working_directory: temporary.path().join("workspace"),
            session_root: temporary.path().join("sessions"),
        },
        true,
    )
    .expect("service");
    let cache = vibe_home.join("skills-registry-cache");

    let disabled = listed_skills(&service);
    assert!(!cache.exists(), "the default runs no registry code");

    std::fs::write(
        vibe_home.join("config.toml"),
        "experimental_enable_registry_skills = true\n",
    )
    .expect("user fixture");
    assert_eq!(
        listed_skills(&service),
        disabled,
        "enabled with no registry configured leaves the catalog unchanged"
    );
    assert!(!cache.exists(), "enabled still creates no cache directory");
}

/// US-169: with nothing on disk the wire still publishes the two seeded
/// builtins, `vibe` model-only and `skill-creator` user-invocable, both
/// under `source: "builtin"` and without a path field.
#[test]
fn the_builtins_are_published_on_the_wire() {
    let temporary = tempdir().expect("tempdir");
    let service = WorkspaceService::new(
        WorkspacePaths {
            vibe_home: temporary.path().join("home"),
            working_directory: temporary.path().join("workspace"),
            session_root: temporary.path().join("sessions"),
        },
        true,
    )
    .expect("service");

    let dispatch = service
        .dispatch("skills/list", &BTreeMap::new())
        .expect("skills/list answers");
    let skills = dispatch.result["skills"]
        .as_array()
        .expect("the response carries an array");
    let entry = |name: &str| {
        skills
            .iter()
            .find(|skill| skill["name"] == name)
            .expect("both builtins are published")
    };

    let vibe = entry("vibe");
    assert_eq!(vibe["userInvocable"], serde_json::json!(false));
    assert_eq!(vibe["source"], serde_json::json!("builtin"));
    assert!(
        vibe.get("path").is_none(),
        "a builtin has no file on disk, so no path is emitted: {vibe}"
    );
    let creator = entry("skill-creator");
    assert_eq!(creator["userInvocable"], serde_json::json!(true));
    assert_eq!(creator["source"], serde_json::json!("builtin"));
}

/// US-168: a `SKILL.md` that will not parse is published as an issue naming
/// the file, which is what `diagnostics/list` reads.
#[test]
fn an_unloadable_skill_is_published_as_an_issue() {
    let temporary = tempdir().expect("tempdir");
    let workspace = temporary.path().join("workspace");
    let broken = workspace.join(".vibe/skills/broken");
    std::fs::create_dir_all(&broken).expect("skill directory");
    std::fs::write(broken.join("SKILL.md"), "no frontmatter here\n").expect("skill fixture");
    let service = WorkspaceService::new(
        WorkspacePaths {
            vibe_home: temporary.path().join("home"),
            working_directory: workspace,
            session_root: temporary.path().join("sessions"),
        },
        true,
    )
    .expect("service");

    let issues = service.skill_issues();

    assert_eq!(issues.len(), 1, "{issues:?}");
    assert!(issues[0].0.ends_with("broken/SKILL.md"), "{issues:?}");
    assert!(!issues[0].1.is_empty(), "the issue carries a reason");
    assert_eq!(
        listed_skills(&service),
        vec!["skill-creator", "vibe"],
        "the unloadable skill is absent from the catalog and the builtins remain"
    );
}

#[test]
fn app_server_advertises_and_dispatches_workspace_resources() {
    let (_temporary, service) = service();
    service
        .store
        .create("saved", "/workspace", None, 1)
        .expect("saved session");
    let server = AppServer::with_workspace_service(service);
    let mut connection = server.connect(TransportKind::InProcess);
    let initialized = connection.dispatch(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"test","version":"1"},"capabilities":{"callbackKinds":[]}}}"#,
        );
    let response = decode_frame(&initialized.outbound[0]).expect("initialize response");
    assert!(matches!(response, Envelope::Success(_)));
    let Envelope::Success(response) = response else {
        return;
    };
    assert!(
        response.result["capabilities"]["methods"]
            .as_array()
            .expect("methods")
            .contains(&json!("config/schema"))
    );
    connection.dispatch(br#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);
    let resume = connection.dispatch(
            br#"{"jsonrpc":"2.0","id":2,"method":"session/resume","params":{"sessionId":"saved","systemPrompt":"fresh","config":{}}}"#,
        );
    assert!(matches!(
        decode_frame(&resume.outbound[0]).expect("resume response"),
        Envelope::Success(_)
    ));
    assert_eq!(
        server.session("saved").expect("attached runtime").id,
        "saved"
    );
}
