//! What a session's system prompt is composed from.
//!
//! Reference `AgentLoop._render_system_prompt` hands
//! `get_universal_system_prompt` the configuration with the active agent's
//! overrides applied, the session's skill and agent managers, its scratchpad,
//! its headless flag, its working directory and its harness files. This module
//! gathers the same inputs from the layered configuration and the extension
//! catalog; [`vibe_core::system_prompt::compose`] turns them into the message.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use thiserror::Error;
use toml::Value as TomlValue;
use vibe_core::config::{ConfigError, LayeredConfig};
use vibe_core::extensions::{AgentKind, AgentProfile, DiscoveryRoots, discover_extensions};
use vibe_core::prompt::library::PromptFileError;
use vibe_core::provider::config::{ModelConfig, ModelRouting};
use vibe_core::system_prompt::{
    DEFAULT_SYSTEM_PROMPT, ProjectContextSettings, ProjectInputs, PromptSkill, PromptSubagent,
    SYSTEM_PROMPT_SETTING, ShellEnvironment, SystemPromptInputs, load_system_prompt,
};

use super::WorkspaceService;

/// The session a prompt is composed for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionPromptScope {
    pub working_directory: PathBuf,
    pub trusted: bool,
    pub add_directories: Vec<String>,
    pub project_file_trust: Option<bool>,
    /// The agent profile the session runs, the configured default when `None`.
    pub agent: Option<String>,
    /// The model the session runs instead of the configured one, named by
    /// alias or by model name.
    pub model: Option<String>,
    pub headless: bool,
    /// The session's scratchpad, which a subagent's session does not have.
    pub scratchpad: Option<PathBuf>,
    /// The tools the session publishes, which decide the Windows shell rules.
    pub tool_names: Vec<String>,
    /// A unified session's plugin skills, which the prompt lists in place of
    /// the legacy builtins.
    pub skill_seed: Option<Arc<BTreeMap<String, vibe_core::extensions::SkillDefinition>>>,
}

/// The host facts the prompt states, which a test fixes rather than reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptHost {
    /// The operator's home, which the dangerous directory table hangs off.
    /// Reference `Path.home()`.
    pub home: Option<PathBuf>,
    pub current_date: String,
    pub platform: String,
    pub shell: ShellEnvironment,
}

impl PromptHost {
    /// This process's host, for a session publishing `tool_names`.
    #[must_use]
    pub fn detect(tool_names: &[String]) -> Self {
        Self {
            home: std::env::home_dir(),
            current_date: vibe_core::system_prompt::current_date(),
            platform: vibe_core::system_prompt::platform_display_name().to_owned(),
            shell: ShellEnvironment::detect(tool_names),
        }
    }
}

#[derive(Debug, Error)]
pub enum SystemPromptError {
    #[error("configuration failed: {0}")]
    Config(String),
    #[error(transparent)]
    Prompt(#[from] PromptFileError),
}

impl From<ConfigError> for SystemPromptError {
    fn from(error: ConfigError) -> Self {
        match error {
            ConfigError::SystemPrompt(error) => Self::Prompt(error),
            other => Self::Config(other.to_string()),
        }
    }
}

impl WorkspaceService {
    /// The system message `scope` opens every request with, on this host.
    ///
    /// # Errors
    ///
    /// A configuration that does not load, and a `system_prompt_id` that names
    /// no prompt.
    pub fn session_system_prompt(
        &self,
        scope: &SessionPromptScope,
    ) -> Result<String, SystemPromptError> {
        let host = PromptHost::detect(&scope.tool_names);
        let inputs = self.system_prompt_inputs(scope, &host)?;
        Ok(vibe_core::system_prompt::compose(&inputs).text())
    }

    /// Everything the prompt of `scope` is composed from.
    ///
    /// # Errors
    ///
    /// As [`Self::session_system_prompt`].
    pub fn system_prompt_inputs(
        &self,
        scope: &SessionPromptScope,
        host: &PromptHost,
    ) -> Result<SystemPromptInputs, SystemPromptError> {
        // A prompt identifier that names nothing refuses the configuration
        // itself, so it is reported before anything else reads it.
        self.config.load().map_err(SystemPromptError::from)?;
        let agents = self
            .prompt_agents(scope)
            .map_err(|error| SystemPromptError::Config(error.to_string()))?;
        let config = self.session_prompt_config(scope)?;
        let snapshot = config.load().map_err(SystemPromptError::from)?;
        let effective = &snapshot.effective;
        let flag = |key: &str| {
            effective
                .get(key)
                .and_then(TomlValue::as_bool)
                .unwrap_or(true)
        };
        let prompt_id = effective
            .get(SYSTEM_PROMPT_SETTING)
            .and_then(TomlValue::as_str)
            .unwrap_or(DEFAULT_SYSTEM_PROMPT);
        // Reference `load_prompt` searches the process's harness files, not
        // the session's.
        let base = load_system_prompt(prompt_id, &self.config.harness_files().prompts_dirs())?;
        let harness = config.harness_files();
        let trust = vibe_core::trust::TrustStore::for_vibe_home(&self.paths.vibe_home);
        Ok(SystemPromptInputs {
            prompt_id: prompt_id.to_owned(),
            base,
            current_date: host.current_date.clone(),
            headless: scope.headless,
            include_commit_signature: flag("include_commit_signature"),
            include_model_info: flag("include_model_info"),
            include_prompt_detail: flag("include_prompt_detail"),
            include_project_context: flag("include_project_context"),
            model_alias: model_alias(&snapshot.config_view(), scope.model.as_deref()),
            platform: host.platform.clone(),
            shell: host.shell.clone(),
            skills: self.prompt_skills(scope),
            subagents: agents
                .iter()
                .filter(|profile| profile.kind == AgentKind::Subagent)
                .map(|profile| PromptSubagent {
                    name: profile.name.clone(),
                    description: profile.description.clone(),
                })
                .collect(),
            scratchpad: scope.scratchpad.clone(),
            project: ProjectInputs {
                cwd: scope.working_directory.clone(),
                home: host.home.clone(),
                settings: ProjectContextSettings::from_table(
                    effective
                        .get("project_context")
                        .and_then(TomlValue::as_table),
                ),
                project_roots: harness.resolved_project_roots(),
                user_instructions: (harness.user_doc_path(), harness.load_user_doc()),
                project_instructions: harness.load_project_docs(&trust),
            },
        })
    }

    /// The merged configuration `scope` composes its prompt under, which a
    /// session compares to decide whether the prompt it holds is still the
    /// one it would compose: a configuration write or an experiment
    /// assignment recomposes it, as reference `refresh_config` does.
    ///
    /// # Errors
    ///
    /// A configuration that does not load.
    pub fn system_prompt_settings(
        &self,
        scope: &SessionPromptScope,
    ) -> Result<toml::Table, SystemPromptError> {
        self.session_prompt_config(scope)?
            .load()
            .map(|snapshot| snapshot.effective)
            .map_err(SystemPromptError::from)
    }

    /// The agents a session in `scope` sees, which name its subagents.
    fn scope_agents(&self, scope: &SessionPromptScope) -> WorkspaceService {
        self.scoped_to_agents(
            scope.working_directory.clone(),
            scope.trusted,
            &scope.add_directories,
            scope.project_file_trust,
            &[],
        )
    }

    pub(crate) fn prompt_agents(
        &self,
        scope: &SessionPromptScope,
    ) -> Result<Vec<AgentProfile>, super::WorkspaceServiceError> {
        self.scope_agents(scope).available_agents()
    }

    /// The model a session in `scope` runs, from its configuration with its
    /// agent's overrides applied: the one `scope.model` names by alias or by
    /// name, else the active one. Reference `get_active_model` over the
    /// orchestrator the agent's profile layer was installed on. `None` when
    /// the configuration declares neither.
    ///
    /// # Errors
    ///
    /// A configuration that does not load.
    pub fn session_model(
        &self,
        scope: &SessionPromptScope,
    ) -> Result<Option<ModelConfig>, SystemPromptError> {
        let snapshot = self
            .session_prompt_config(scope)?
            .load()
            .map_err(SystemPromptError::from)?;
        let routing =
            ModelRouting::from_effective(&snapshot.effective, snapshot.active_model_alias());
        let find = |wanted: &str| {
            routing
                .models
                .iter()
                .find(|model| model.alias == wanted)
                .or_else(|| routing.models.iter().find(|model| model.name == wanted))
                .cloned()
        };
        Ok(scope
            .model
            .as_deref()
            .and_then(find)
            .or_else(|| routing.active_alias.as_deref().and_then(find)))
    }

    /// The layered configuration of `scope`: its directory, trust and roots,
    /// with its agent's overrides applied.
    fn session_prompt_config(
        &self,
        scope: &SessionPromptScope,
    ) -> Result<LayeredConfig, SystemPromptError> {
        let agent_name = match &scope.agent {
            Some(agent) => agent.clone(),
            None => self
                .default_agent_name()
                .map_err(|error| SystemPromptError::Config(error.to_string()))?,
        };
        // The profile the session runs applies whether or not the catalog
        // still offers it, as the reference's installed profile layer does.
        let overrides = self
            .scope_agents(scope)
            .catalog()
            .agents
            .remove(&agent_name)
            .map(|profile| profile.overrides)
            .unwrap_or_default();
        // The directory the server runs in is an authorized root, not an
        // opened project: only its trust makes it one, as `session_hooks`
        // reads it.
        let mut roots = self
            .allowed_roots
            .iter()
            .filter(|root| **root != self.paths.working_directory)
            .cloned()
            .collect::<Vec<_>>();
        roots.extend(scope.add_directories.iter().map(PathBuf::from));
        Ok(self
            .config
            .scoped_to_working_directory(scope.working_directory.clone(), scope.trusted)
            .with_project_file_trust(scope.project_file_trust)
            .with_additional_roots(roots)
            .with_agent_overlay(overrides))
    }

    /// The skills a session in `working_directory` may load, builtins
    /// included. Reference `SkillManager.available_skills`.
    fn prompt_skills(&self, scope: &SessionPromptScope) -> Vec<PromptSkill> {
        let seeded = scope
            .skill_seed
            .as_deref()
            .cloned()
            .unwrap_or_else(vibe_core::skills::builtins::builtin_skills);
        let roots = DiscoveryRoots {
            skills: self.skill_discovery(&scope.working_directory, scope.trusted),
            ..DiscoveryRoots::default()
        };
        discover_extensions(&roots, BTreeMap::new(), seeded, BTreeMap::new())
            .skills
            .into_values()
            .map(|skill| PromptSkill {
                name: skill.name,
                description: skill.description,
                path: skill.path,
                model_invocable: skill.model_invocable,
                user_invocable: skill.user_invocable,
            })
            .collect()
    }
}

/// The alias of the model a session runs: the one it overrides the
/// configuration with, found by alias or by model name, else the active one.
fn model_alias(view: &Value, model: Option<&str>) -> String {
    let overridden = model.and_then(|model| {
        view["models"].as_array()?.iter().find_map(|entry| {
            (entry["alias"] == model || entry["name"] == model)
                .then(|| entry["alias"].as_str())
                .flatten()
        })
    });
    overridden
        .or_else(|| view["activeModel"]["alias"].as_str())
        .unwrap_or_default()
        .to_owned()
}
