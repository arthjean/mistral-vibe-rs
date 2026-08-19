//! Opening an interactive session: the preferences a launch resolves, the
//! services it wires together, and the banner counts the first frame reports.
//!
//! [`super::run_interactive`] owns the event loop; everything it needs to exist
//! before the first frame is built here.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use vibe_app_server::client::{HeadlessService, LiveTurnDriver};
use vibe_app_server::experiments::SessionExperiments;
use vibe_app_server::workspace::WorkspaceService;

use super::clipboard_images::ImageModels;
use super::cloud_workflow::CloudWorkflowState;
use super::runtime::{BannerMetrics, InteractiveRuntime, RuntimeSkill, UiOperationCompletion};
use super::voice::{SpeechManager, VoiceManager};
use super::{
    Arguments, CliError, DEFAULT_CONTEXT_WINDOW, DEFAULT_MODEL, active_agent_safety, bootstrap,
    startup, telemetry_observer,
};

pub(super) fn start_runtime(
    arguments: &Arguments,
    working_directory: &Path,
    workspace: WorkspaceService,
    credential: String,
    ui_operation_sender: tokio::sync::mpsc::UnboundedSender<UiOperationCompletion>,
) -> Result<InteractiveRuntime, CliError> {
    let voice_credential = credential.clone();
    let banner = banner_metrics_from_workspace(&workspace, arguments, working_directory);
    let skills = runtime_skills(&workspace);
    let preferences = startup_preferences(arguments, &workspace)?;
    let telemetry = telemetry_observer(arguments, &workspace)?;
    let mut driver = LiveTurnDriver::from_credential(
        bootstrap::live_driver_config(
            arguments,
            &preferences.model,
            workspace.compaction_prompts(),
        )?,
        credential.clone(),
    )?;
    driver = driver.with_event_observer(telemetry.clone());
    let configuration = workspace.clone();
    let server = bootstrap::resource_server(
        arguments,
        workspace,
        credential.clone(),
        Some(driver.sampling_handler(&preferences.model)),
    )?
    .using_projects_service(bootstrap::cloud_service(credential)?)
    .using_client_telemetry(telemetry.clone());
    let mut service =
        HeadlessService::new_interactive_shared_with_server(Arc::new(driver), server)?;
    let session_start = std::time::Instant::now();
    let session_id = service.start_session(&bootstrap::session_options(
        arguments,
        working_directory,
        preferences.model.clone(),
        Some(preferences.mode.clone()),
        preferences.reasoning_effort.clone(),
    ))?;
    let session_init_duration_ms =
        u64::try_from(session_start.elapsed().as_millis()).unwrap_or(u64::MAX);
    // Reference `start_initialize_experiments`: the lookup is detached the
    // moment the session exists, so nothing between here and the first frame
    // waits on a rollout service. A resumed or forked session hydrates from
    // what its metadata already carries and issues no request at all.
    let experiments = Arc::new(SessionExperiments::new(
        &configuration,
        crate::cli_credentials(arguments),
        Some(crate::cli_launch_context()),
        telemetry.exposures(),
    ));
    experiments.start(&session_id);
    // The audio surface is resolved from the configuration this session
    // publishes, not from the LLM endpoint: the transcription model, its wire
    // values, the provider's endpoint and the variable its credential is read
    // from all come from the same view a settings screen renders.
    let published_config =
        super::runtime::published_config_view(&mut service, &session_id).unwrap_or(Value::Null);
    let voice_enabled = published_config
        .get("voiceModeEnabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let vibe_home = startup::vibe_home_directory(arguments, working_directory);
    let voice = VoiceManager::production(
        &published_config,
        &voice_credential,
        &vibe_home,
        voice_enabled,
    );
    // Reference `_make_tts_client`: the read-aloud client comes from the same
    // view, and a configuration it cannot be built from leaves the narrator
    // silent rather than failing the session.
    let speech = SpeechManager::production(&published_config, &voice_credential, &vibe_home);
    let session = service.session(&session_id)?;
    let agent_name = session
        .intent
        .agent
        .clone()
        .unwrap_or_else(|| "default".to_owned());
    let safety = active_agent_safety(&mut service, &session_id, &agent_name);
    Ok(InteractiveRuntime {
        service,
        experiments: Some(experiments),
        workspace: configuration,
        session_id,
        model: session.intent.model.unwrap_or(preferences.model),
        image_models: preferences.image_models,
        thinking: session
            .intent
            .reasoning_effort
            .unwrap_or_else(|| "off".to_owned()),
        mode: session
            .intent
            .mode
            .unwrap_or_else(|| preferences.mode.clone()),
        agent_name,
        safety,
        banner,
        context_tokens: 0,
        context_window: arguments.max_tokens.unwrap_or(DEFAULT_CONTEXT_WINDOW),
        auto_approve: session.intent.auto_approve,
        vibe_code_enabled: preferences.vibe_code_enabled,
        config_target: None,
        remote_project_overlay: None,
        remote_project_draft: None,
        ui_operation_sender,
        ui_operation_generation: 0,
        active_ui_operation: None,
        skills,
        shell: None,
        cloud: CloudWorkflowState::default(),
        pending_switch: None,
        telemetry: Some(telemetry),
        project_picker: None,
        teleport_telemetry: None,
        session_init_duration_ms: Some(session_init_duration_ms),
        voice,
        speech,
    })
}

/// Reference `emit_new_session_telemetry` and `emit_ready_telemetry`, which the
/// agent loop raises together once initialization settles: the session census
/// first, then how long reaching it took.
struct StartupPreferences {
    model: String,
    image_models: ImageModels,
    mode: String,
    reasoning_effort: Option<String>,
    vibe_code_enabled: bool,
}

fn startup_preferences(
    arguments: &Arguments,
    workspace: &WorkspaceService,
) -> Result<StartupPreferences, CliError> {
    // One load answers both: the document the rest of this reads, and the alias
    // the sentinel resolves to, which is never the raw `active_model` value.
    let snapshot = workspace
        .layered_config()
        .load()
        .map_err(|error| CliError::Terminal(error.to_string()))?;
    let configured_model = snapshot.active_model_alias().map(ToOwned::to_owned);
    let document = snapshot.public_view();
    let config = document.get("config");
    let model = if arguments.model == DEFAULT_MODEL {
        configured_model.unwrap_or_else(|| arguments.model.clone())
    } else {
        arguments.model.clone()
    };
    let mut image_models = ImageModels::default();
    image_models.insert(DEFAULT_MODEL, true);
    // Models are keyed by alias in the effective configuration; the entry still
    // carries its own name, which a provider request is sent under.
    if let Some(models) = config
        .and_then(|config| config.get("models"))
        .and_then(Value::as_object)
    {
        for (alias, configured) in models {
            let supports_images = configured
                .get("supports_images")
                .or_else(|| configured.get("supportsImages"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            image_models.insert(alias, supports_images);
            if let Some(name) = configured.get("name").and_then(Value::as_str) {
                image_models.insert(name, supports_images);
            }
        }
    }
    let reasoning_effort = config
        .and_then(|config| config.get("thinking"))
        .and_then(Value::as_str)
        .filter(|value| *value != "off")
        .map(ToOwned::to_owned);
    let mode = if arguments.agent.as_deref() == Some("plan") {
        "plan".to_owned()
    } else {
        config
            .and_then(|config| config.get("mode"))
            .and_then(Value::as_str)
            .filter(|mode| matches!(*mode, "code" | "plan"))
            .unwrap_or("code")
            .to_owned()
    };
    Ok(StartupPreferences {
        model,
        image_models,
        mode,
        reasoning_effort,
        vibe_code_enabled: config
            .and_then(|config| {
                config
                    .get("vibe_code_enabled")
                    .or_else(|| config.get("vibeCodeEnabled"))
            })
            .and_then(Value::as_bool)
            .unwrap_or(true),
    })
}

/// Reference `action_suspend_with_message`: restore the terminal, print the
/// resume hint, stop, and repaint on return. Unsupported platforms do nothing.
pub(super) fn runtime_skills(workspace: &WorkspaceService) -> BTreeMap<String, RuntimeSkill> {
    workspace
        .dispatch("skills/list", &BTreeMap::new())
        .ok()
        .and_then(|dispatch| dispatch.result.get("skills").cloned())
        .map_or_else(BTreeMap::new, |skills| parse_runtime_skills(Some(&skills)))
}

pub(super) fn parse_runtime_skills(skills: Option<&Value>) -> BTreeMap<String, RuntimeSkill> {
    skills
        .and_then(Value::as_array)
        .cloned()
        .into_iter()
        .flatten()
        .filter(|skill| {
            skill
                .get("userInvocable")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .filter_map(|skill| {
            let name = skill.get("name").and_then(Value::as_str)?.to_owned();
            Some((
                name.clone(),
                RuntimeSkill {
                    name,
                    description: skill
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                },
            ))
        })
        .collect()
}

pub(super) fn banner_metrics_from_workspace(
    workspace: &WorkspaceService,
    arguments: &Arguments,
    working_directory: &Path,
) -> BannerMetrics {
    let mut banner = BannerMetrics::default();
    if let Ok(dispatch) = workspace.dispatch("skills/list", &BTreeMap::new()) {
        // The banner reports the skills the operator added, so the two seeded
        // builtins are excluded the way the reference's `custom_skills_count`
        // excludes them.
        banner.skills_count = dispatch
            .result
            .get("skills")
            .and_then(Value::as_array)
            .map_or(0, |skills| {
                skills
                    .iter()
                    .filter(|skill| skill.get("source").and_then(Value::as_str) != Some("builtin"))
                    .count()
            });
    }
    if let Ok(servers) = workspace.mcp_servers_for_session(working_directory, arguments.trust, &[])
    {
        banner.mcp_servers_total = servers.len();
        banner.mcp_servers_enabled = servers.iter().filter(|server| server.enabled).count();
    }
    banner
}

pub(super) async fn refresh_server_banner_metrics(
    service: &mut HeadlessService<LiveTurnDriver>,
    session_id: &str,
    banner: &mut BannerMetrics,
) {
    if let Ok(dispatch) = service
        .public_call_async("connectors/read", json!({"sessionId": session_id}))
        .await
        && let Some(counts) = dispatch.result.get("counts")
    {
        banner.connectors_connected = json_usize(counts.get("connected"));
        banner.connectors_total = json_usize(counts.get("total"));
    }
    if let Ok(dispatch) = service
        .public_call_async("mcp/read", json!({"sessionId": session_id}))
        .await
        && let Some(sources) = dispatch
            .result
            .get("mcp")
            .and_then(|mcp| mcp.get("sources"))
            .and_then(Value::as_array)
    {
        // The published list carries the connectors too, and the banner counts
        // them on their own line.
        let servers = sources
            .iter()
            .filter(|source| source.get("kind").and_then(Value::as_str) != Some("connector"))
            .collect::<Vec<_>>();
        banner.mcp_servers_total = servers.len();
        banner.mcp_servers_enabled = servers
            .iter()
            .filter(|source| {
                source
                    .get("status")
                    .and_then(Value::as_str)
                    .is_none_or(|status| status != "disabled")
            })
            .count();
    }
    if let Ok(result) = service.public_call("diagnostics/list", json!({"sessionId": session_id})) {
        banner.hooks_count = json_usize(result.get("hooksCount"));
    }
    if let Ok(result) = service.public_call("account/read", json!({"sessionId": session_id})) {
        banner.plan = result
            .get("account")
            .and_then(|account| account.get("plan"))
            .and_then(|plan| plan.get("title"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }
}

fn json_usize(value: Option<&Value>) -> usize {
    value
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or_default()
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped defaults reach the terminal client: with no configuration
    /// file at all, the session opens on the default model, knows that model
    /// takes images, and reads the hosted-surface flag from the same document.
    #[test]
    fn startup_preferences_read_the_shipped_default_configuration() {
        let temporary = tempfile::tempdir().expect("temporary home");
        let workspace_service = WorkspaceService::for_runtime_session_root(
            temporary.path().join(".vibe/sessions"),
            temporary.path().join("workspace"),
        );
        let arguments = <Arguments as clap::Parser>::try_parse_from(["vibe"])
            .expect("interactive arguments parse");

        let preferences = startup_preferences(&arguments, &workspace_service)
            .expect("preferences read from defaults");

        assert_eq!(preferences.model, DEFAULT_MODEL);
        assert!(preferences.image_models.get(DEFAULT_MODEL).supports_images);
        assert!(
            preferences
                .image_models
                .get("mistral-vibe-cli-latest")
                .supports_images,
            "the model's own name resolves alongside its alias"
        );
        assert!(
            !preferences.image_models.get("local").supports_images,
            "a model that takes no image is published as such"
        );
        assert!(preferences.vibe_code_enabled);
        assert_eq!(preferences.reasoning_effort, None);
    }

    /// US-169: the composer's skill map is built from `skills/list` and keeps
    /// only user-invocable entries, so `/vibe` resolves no skill and stays an
    /// ordinary prompt while `/skill-creator` is invocable.
    #[test]
    fn a_model_only_builtin_is_not_invocable_from_the_composer() {
        let temporary = tempfile::tempdir().expect("temporary home");
        let workspace_service = WorkspaceService::for_runtime_session_root(
            temporary.path().join(".vibe/sessions"),
            temporary.path().join("workspace"),
        );

        let skills = runtime_skills(&workspace_service);

        assert!(
            !skills.contains_key("vibe"),
            "`vibe` is not user invocable, so the composer cannot invoke it"
        );
        assert!(
            skills.contains_key("skill-creator"),
            "`skill-creator` is user invocable and reachable as a slash word"
        );
    }

    /// US-171: the banner counts the skills the operator added, read through
    /// the same `banner_metrics_from_workspace` the startup path calls. The two
    /// seeded builtins never count, the user's own skills do, and one withheld
    /// by `disabled_skills` drops out because the count reads the filtered
    /// catalog rather than the walked one.
    #[test]
    fn the_banner_counts_custom_skills_only() {
        let temporary = tempfile::tempdir().expect("temporary home");
        let workspace = temporary.path().join("workspace");
        let vibe_home = temporary.path().join(".vibe");
        std::fs::create_dir_all(&vibe_home).expect("vibe home");
        let workspace_service = WorkspaceService::for_runtime_session_root(
            vibe_home.join("sessions"),
            workspace.clone(),
        );
        let arguments = <Arguments as clap::Parser>::try_parse_from(["vibe"])
            .expect("interactive arguments parse");
        let counted = || {
            banner_metrics_from_workspace(&workspace_service, &arguments, &workspace).skills_count
        };

        assert_eq!(
            counted(),
            0,
            "only the two builtins are published, and neither is the operator's"
        );

        for name in ["alpha", "beta", "gamma"] {
            let directory = vibe_home.join("skills").join(name);
            std::fs::create_dir_all(&directory).expect("skill directory");
            std::fs::write(
                directory.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: A user skill.\n---\n\nBody.\n"),
            )
            .expect("skill file");
        }
        assert_eq!(
            counted(),
            3,
            "the three user skills count and the builtins beside them do not"
        );

        std::fs::write(
            vibe_home.join("config.toml"),
            "disabled_skills = [\"beta\"]\n",
        )
        .expect("user configuration");
        assert_eq!(
            counted(),
            2,
            "the withheld skill is never published, so the count reads the filtered catalog"
        );
    }
}
