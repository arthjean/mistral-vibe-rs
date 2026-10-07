//! The agent, skill and prompt methods.
//!
//! An agent profile decides what a session may run and under which prompt, and
//! a skill is what a prompt can invoke. Discovery is `vibe_core::extensions` and
//! `vibe_core::skills`; what is here is the catalog the boundary publishes, the
//! installation state it writes.

use super::sessions::runtime_attachment;
use super::*;

/// Why a profile a request names is not one the session may run.
enum Unavailable {
    /// No search path declares it.
    Unknown,
    /// It is declared, and the configuration or its installation withholds it;
    /// the text names the key to change.
    Excluded(String),
}

impl Unavailable {
    fn message(self, name: &str) -> String {
        match self {
            Self::Unknown => format!("Agent '{name}' not found"),
            Self::Excluded(message) => message,
        }
    }
}

/// Whether `patterns` holds an entry matching `name`, under the rules every
/// agent list follows (globs, and `re:` regular expressions).
fn listed(patterns: &[String], name: &str) -> bool {
    vibe_core::matching::NameFilter::new(patterns).matches(name)
}

/// The strings a list field of the effective document holds.
fn string_list(document: &Table, key: &str) -> Vec<String> {
    document
        .get(key)
        .and_then(TomlValue::as_array)
        .into_iter()
        .flatten()
        .filter_map(TomlValue::as_str)
        .map(ToOwned::to_owned)
        .collect()
}

fn flag(document: &Table, key: &str) -> bool {
    document
        .get(key)
        .and_then(TomlValue::as_bool)
        .unwrap_or(false)
}

/// Whether the builtin profile only runs once installed. Reference
/// `AgentProfile.install_required`, which only the shipped `lean` sets: a
/// custom file of the same name replaces the profile and the requirement.
fn install_required(profile: &AgentProfile) -> bool {
    profile.source == ExtensionSource::Builtin && profile.name == "lean"
}

/// Reference `VibeConfigSchema.smart_approve_offered`.
fn smart_approve_offered(document: &Table) -> bool {
    flag(document, "smart_approve_available") || flag(document, "smart_approve_default")
}

/// Reference `VibeConfigSchema.resolve_default_agent`: the smart-approve
/// default outranks `default_agent`.
fn resolved_default_agent(document: &Table) -> String {
    if flag(document, "smart_approve_default") {
        return builtin_agents::SMART_APPROVE.to_owned();
    }
    document
        .get("default_agent")
        .and_then(TomlValue::as_str)
        .unwrap_or("accept-edits")
        .to_owned()
}

/// Reference `AgentManager._is_agent_available`.
fn agent_available(profile: &AgentProfile, document: &Table, forced: &BTreeSet<String>) -> bool {
    if forced.contains(&profile.name) {
        return true;
    }
    if profile.name == builtin_agents::SMART_APPROVE && !smart_approve_offered(document) {
        return false;
    }
    if install_required(profile)
        && !string_list(document, "installed_agents").contains(&profile.name)
    {
        return false;
    }
    let enabled = string_list(document, "enabled_agents");
    if !enabled.is_empty() {
        return listed(&enabled, &profile.name);
    }
    !listed(&string_list(document, "disabled_agents"), &profile.name)
}

/// Why the configuration withholds a declared profile, naming the key that
/// does. Reference `excluded_agent_message` states the same facts: the
/// installation list for a profile that needs installing, else the allowlist
/// or the denylist that excluded it, and `default_agent` when the name is
/// the configured default.
fn exclusion_message(profile: &AgentProfile, document: &Table) -> String {
    let name = &profile.name;
    if install_required(profile) && !string_list(document, "installed_agents").contains(name) {
        return format!(
            "The profile '{name}' is not installed yet: start once with --agent '{name}', or \
             append it to 'installed_agents'."
        );
    }
    let default = document.get("default_agent").and_then(TomlValue::as_str) == Some(name.as_str());
    let (subject, remedy) = if default {
        (
            format!("The configured default_agent '{name}'"),
            "point 'default_agent' at a profile that is offered",
        )
    } else {
        (
            format!("The profile '{name}'"),
            "choose a profile that is offered",
        )
    };
    let enabled = string_list(document, "enabled_agents");
    if !enabled.is_empty() {
        if !listed(&enabled, name) {
            return format!(
                "{subject} is missing from 'enabled_agents' {enabled:?}; list '{name}' there, or \
                 {remedy}."
            );
        }
    } else {
        let disabled = string_list(document, "disabled_agents");
        if listed(&disabled, name) {
            return format!(
                "{subject} is withheld by 'disabled_agents' {disabled:?}; drop '{name}' from it, \
                 or {remedy}."
            );
        }
    }
    format!("The profile '{name}' is not offered by this configuration.")
}

impl WorkspaceService {
    /// This service as a session sees its agents: the session's directory,
    /// trust and roots decide which profile files are read and which
    /// configuration filters them, and `forced` names the profiles its start
    /// selected past the rollout gate (reference `AgentManager._forced_agents`).
    #[must_use]
    pub(crate) fn scoped_to_agents(
        &self,
        working_directory: PathBuf,
        trusted: bool,
        add_directories: &[String],
        project_file_trust: Option<bool>,
        forced: &[String],
    ) -> Self {
        let mut scoped = self.clone();
        scoped.config = self
            .config
            .scoped_to_working_directory(working_directory, trusted)
            .with_project_file_trust(project_file_trust)
            .with_additional_roots(add_directories.iter().map(PathBuf::from).collect());
        scoped.forced_agents = forced.iter().cloned().collect();
        scoped
    }

    fn effective_document(&self) -> Result<Table, WorkspaceServiceError> {
        Ok(self.config.load().map_err(config_error)?.effective)
    }

    /// Reference `AgentManager.get_agent`: the profile a switch selects, which
    /// has to be one the session is offered.
    pub(crate) fn agent_profile(&self, name: &str) -> Result<AgentProfile, WorkspaceServiceError> {
        let document = self.effective_document()?;
        let mut discovered = self.catalog().agents;
        let profile = match discovered.remove(name) {
            Some(profile) if agent_available(&profile, &document, &self.forced_agents) => profile,
            Some(profile) => {
                return Err(WorkspaceServiceError::Refused(
                    vibe_protocol::ProtocolErrorCode::InternalError,
                    Unavailable::Excluded(exclusion_message(&profile, &document)).message(name),
                ));
            }
            None if self.persists_runtime_sessions() => {
                return Err(WorkspaceServiceError::Refused(
                    vibe_protocol::ProtocolErrorCode::InternalError,
                    Unavailable::Unknown.message(name),
                ));
            }
            None => AgentProfile {
                name: name.to_owned(),
                display_name: name.to_owned(),
                description: "Externally supplied agent profile".to_owned(),
                kind: AgentKind::Agent,
                safety: "neutral".to_owned(),
                overrides: Table::new(),
                source: ExtensionSource::Configured,
                path: None,
            },
        };
        self.check_prompt(&profile)?;
        Ok(profile)
    }

    /// Reference `AgentManager.__init__`: the profile a session starts under,
    /// and whether the start forced it past the rollout gate. A profile the
    /// configuration withholds, one no search path declares and a subagent are
    /// all refused as parameters the start cannot honor.
    pub(crate) fn initial_agent(
        &self,
        name: &str,
    ) -> Result<(AgentProfile, bool), WorkspaceServiceError> {
        let refuse = |message: String| {
            WorkspaceServiceError::Refused(vibe_protocol::ProtocolErrorCode::InvalidParams, message)
        };
        let document = self.effective_document()?;
        let mut discovered = self.catalog().agents;
        let (profile, forced) = match discovered.remove(name) {
            Some(profile) if agent_available(&profile, &document, &self.forced_agents) => {
                (profile, false)
            }
            // An explicit smart-approve start is an opt-in past the gate that
            // keeps the profile offered for the rest of the session.
            Some(profile) if name == builtin_agents::SMART_APPROVE => (profile, true),
            Some(profile) => return Err(refuse(exclusion_message(&profile, &document))),
            None if self.persists_runtime_sessions() => {
                return Err(refuse(format!("Agent '{name}' not found.")));
            }
            None => return self.agent_profile(name).map(|profile| (profile, false)),
        };
        if profile.kind != AgentKind::Agent {
            return Err(refuse(format!(
                "'{name}' is a subagent profile, which only a delegation runs; start the \
                 session under a primary profile instead."
            )));
        }
        self.check_prompt(&profile)?;
        Ok((profile, forced))
    }

    fn check_prompt(&self, profile: &AgentProfile) -> Result<(), WorkspaceServiceError> {
        if let Some(prompt_id) = profile.runtime_settings().system_prompt_id
            && vibe_core::system_prompt::load_system_prompt(
                &prompt_id,
                &self.config.harness_files().prompts_dirs(),
            )
            .is_err()
        {
            return Err(WorkspaceServiceError::InvalidParams(format!(
                "agent `{}` references unsupported system prompt `{prompt_id}`",
                profile.name
            )));
        }
        Ok(())
    }

    /// The agent a session starts under when it names none. Reference
    /// `resolve_default_agent`.
    pub(crate) fn default_agent_name(&self) -> Result<String, WorkspaceServiceError> {
        Ok(resolved_default_agent(&self.effective_document()?))
    }

    /// The agent catalog as `AgentsListResponse` declares it.
    ///
    /// `active` is the agent a fresh session would run, which the server
    /// replaces with the one the addressed session actually runs. A listing
    /// that names no session is the host's, which hides `plan` from the picker
    /// as reference `project_unified_agent_summaries` does.
    pub(super) fn agents_list(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let host = !params.contains_key("sessionId");
        let (active, listing) = if host {
            let (profile, forced) = self.initial_agent(&self.default_agent_name()?)?;
            let listing = if forced {
                self.forced_agents_with(&profile.name)
            } else {
                self.clone()
            };
            (profile, listing)
        } else {
            let name = self.default_agent_name()?;
            let profile = self
                .available_agents()?
                .into_iter()
                .find(|profile| profile.name == name)
                .unwrap_or_else(builtin_agents::default_profile);
            (profile, self.clone())
        };
        let profiles = listing
            .available_agents()?
            .into_iter()
            .filter(|profile| !(host && profile.name == "plan"))
            .collect::<Vec<_>>();
        Ok(WorkspaceDispatch::result([
            ("active", agent_summary(&active)),
            (
                "agents",
                Value::Array(profiles.iter().map(agent_summary).collect()),
            ),
        ]))
    }

    fn forced_agents_with(&self, name: &str) -> Self {
        let mut scoped = self.clone();
        scoped.forced_agents.insert(name.to_owned());
        scoped
    }

    /// Every agent profile a session may run. Reference
    /// `AgentManager.available_agents`, in the order its dictionary keeps: the
    /// builtins in declaration order, then the custom profiles.
    pub(super) fn available_agents(&self) -> Result<Vec<AgentProfile>, WorkspaceServiceError> {
        let document = self.effective_document()?;
        let mut profiles = self
            .catalog()
            .agents
            .into_values()
            .filter(|profile| agent_available(profile, &document, &self.forced_agents))
            .collect::<Vec<_>>();
        profiles.sort_by_key(|profile| builtin_agents::declaration_rank(&profile.name));
        Ok(profiles)
    }

    /// The configuration, catalogs and diagnostics `RuntimeSnapshot` carries.
    ///
    /// The server owns the rest of the snapshot: the tool surface, the
    /// integrations and the session's own accounting. `active_agent` names the
    /// profile the session runs, which this service cannot know on its own and
    /// which stays the active one even once the catalog stops offering it.
    pub fn runtime_projection(&self, active_agent: Option<&str>) -> RuntimeProjection {
        let catalog = self.catalog();
        let profiles = self.available_agents().unwrap_or_default();
        let active = active_agent
            .and_then(|name| catalog.agents.get(name).cloned())
            .or_else(|| {
                let default = self.default_agent_name().ok()?;
                profiles
                    .iter()
                    .find(|profile| profile.name == default)
                    .cloned()
            })
            .unwrap_or_else(builtin_agents::default_profile);
        // Reference `project_config` reads the configuration with the active
        // profile's layer installed, so the model an agent selects is the one
        // the view names.
        let snapshot = self
            .config
            .clone()
            .with_agent_overlay(active.overrides.clone())
            .load()
            .ok();
        let view = snapshot
            .as_ref()
            .map_or_else(|| Value::Object(Map::new()), ConfigSnapshot::config_view);
        let configured_bypass = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.effective.get("bypass_tool_permissions"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(false);
        let agent_bypass = active
            .overrides
            .get("bypass_tool_permissions")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false);
        RuntimeProjection {
            bypass_tool_permissions: configured_bypass || agent_bypass,
            config: view,
            active_agent: agent_summary(&active),
            agents: profiles.iter().map(agent_summary).collect(),
            skills: catalog
                .skills
                .values()
                .filter(|skill| !self.plugin_claims(skill))
                .map(skill_summary)
                .collect(),
            issues: catalog
                .issues
                .iter()
                .map(|issue| {
                    json!({
                        "file": issue.path.to_string_lossy(),
                        "message": issue.message,
                    })
                })
                .collect(),
        }
    }

    pub(super) fn agent_install(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        if let Some(name) = params.get("agentName").and_then(Value::as_str) {
            self.set_builtin_agent_installed(name, true)?;
            return self.agents_list(params);
        }
        let source = self.authorized_existing_path(Path::new(required_string(params, "path")?))?;
        self.agents
            .lock()
            .map_err(|_| WorkspaceServiceError::StatePoisoned)?
            .install(&source)
            .map_err(|error| WorkspaceServiceError::Extension(error.to_string()))?;
        // Both forms answer with the catalog the change produced, which is what
        // `AgentsListResponse` declares and what a client re-renders from.
        self.agents_list(params)
    }

    pub(super) fn agent_uninstall(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        // Reference `_agent_install(install=False)` only edits the list: a
        // session running the profile keeps running it, and the server names
        // it as the active one.
        if let Some(name) = params.get("agentName").and_then(Value::as_str) {
            self.set_builtin_agent_installed(name, false)?;
            return self.agents_list(params);
        }
        self.agents
            .lock()
            .map_err(|_| WorkspaceServiceError::StatePoisoned)?
            .uninstall(required_string(params, "name")?)
            .map_err(|error| WorkspaceServiceError::Extension(error.to_string()))?;
        self.agents_list(params)
    }

    pub(super) fn set_builtin_agent_installed(
        &self,
        name: &str,
        install: bool,
    ) -> Result<(), WorkspaceServiceError> {
        // Reference `_agent_install` edits the list whatever the name.
        let snapshot = self.config.load().map_err(config_error)?;
        let mut installed = snapshot
            .effective
            .get("installed_agents")
            .and_then(TomlValue::as_array)
            .into_iter()
            .flatten()
            .filter_map(TomlValue::as_str)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        // Appended in the order the installations happened.
        if install && !installed.iter().any(|entry| entry == name) {
            installed.push(name.to_owned());
        }
        if !install {
            installed.retain(|entry| entry != name);
        }
        let expected_fingerprint = snapshot
            .fingerprints
            .get(&snapshot.selected_target)
            .cloned()
            .flatten();
        self.config
            .batch_write(&[ConfigWrite {
                target: snapshot.selected_target,
                expected_fingerprint,
                mutations: vec![ConfigMutation::set(
                    ["installed_agents"],
                    TomlValue::Array(installed.iter().cloned().map(TomlValue::String).collect()),
                )],
            }])
            .map_err(config_error)?;
        Ok(())
    }

    /// Where this service reads agent profiles from, under its configuration
    /// as it stands now, so an `agent_paths` entry written between two reads
    /// is searched by the second.
    fn agent_search_paths(&self) -> Vec<(ExtensionSource, PathBuf)> {
        let configured = self
            .config
            .load()
            .map(|snapshot| string_list(&snapshot.effective, "agent_paths"))
            .unwrap_or_default();
        vibe_core::extensions::agent_search_paths(
            &configured,
            &self.config.harness_files(),
            self.paths.vibe_home.parent(),
            self.config.working_directory(),
        )
    }

    pub(super) fn agent_update(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let session_id = required_string(params, "sessionId")?;
        // Reference `AgentSwitchParams` names it `agentName`; this port's own
        // clients send `name`.
        let name = match params.get("agentName") {
            Some(_) => required_string(params, "agentName")?,
            None => required_string(params, "name")?,
        };
        // Reference `AgentLoop.switch_agent` returns before any lookup when the
        // session already runs the profile, so a profile the configuration
        // has since withheld is still switched to without a refusal.
        let current = self
            .store
            .open(session_id)
            .ok()
            .and_then(|hydrated| hydrated.metadata.agent_profile)
            .and_then(|profile| serde_json::from_value::<AgentProfile>(profile).ok())
            .filter(|profile| profile.name == name);
        let (profile, hydrated) = match current {
            Some(profile) => {
                let hydrated = self.store.open(session_id).map_err(storage_error)?;
                (profile, hydrated)
            }
            None => self.set_session_agent(session_id, name)?,
        };
        Ok(WorkspaceDispatch {
            result: [("agent".to_owned(), serde_json::to_value(profile)?)]
                .into_iter()
                .collect(),
            attachment: Some(runtime_attachment(&hydrated)),
        })
    }

    /// The skill catalog as `SkillsListResponse` declares it.
    ///
    /// The discovery issues the catalog also carries are published on
    /// `runtime/read` rather than here, which is where the reference reports
    /// them and the only shape this response accepts.
    pub(super) fn skills_list(&self) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
        let catalog = self.catalog();
        Ok(WorkspaceDispatch::result([(
            "skills",
            Value::Array(catalog.skills.values().map(skill_summary).collect()),
        )]))
    }

    pub(super) fn authorized_existing_path(
        &self,
        path: &Path,
    ) -> Result<PathBuf, WorkspaceServiceError> {
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.paths.working_directory.join(path)
        };
        let canonical = fs::canonicalize(&candidate).map_err(|error| {
            WorkspaceServiceError::InvalidParams(format!(
                "authorized path `{}` cannot be resolved: {error}",
                candidate.display()
            ))
        })?;
        let authorized = self
            .allowed_roots
            .iter()
            .any(|root| fs::canonicalize(root).is_ok_and(|allowed| canonical.starts_with(allowed)));
        if !authorized {
            return Err(WorkspaceServiceError::InvalidParams(format!(
                "path `{}` is outside server-authorized workspace roots",
                canonical.display()
            )));
        }
        Ok(canonical)
    }

    /// Whether a skill a root publishes sits at a `SKILL.md` a seeded plugin
    /// skill already configured, which the reference's unified catalogue
    /// skips (`vibe/app_server/_skills.py`, `project_core_skills`).
    fn plugin_claims(&self, skill: &vibe_core::extensions::SkillDefinition) -> bool {
        let (Some(seed), Some(path)) = (&self.seeded_skills, &skill.path) else {
            return false;
        };
        if skill.source == vibe_core::skills::SkillSource::Plugin {
            return false;
        }
        let resolved = path.canonicalize().unwrap_or_else(|_| path.clone());
        seed.values()
            .filter_map(|plugin| plugin.path.as_ref())
            .any(|claimed| claimed.canonicalize().unwrap_or_else(|_| claimed.clone()) == resolved)
    }

    pub(super) fn catalog(&self) -> ExtensionCatalog {
        let builtin_agents = self
            .agents
            .lock()
            .ok()
            .map(|agents| {
                agents
                    .list()
                    .into_iter()
                    .filter(|agent| agent.source == ExtensionSource::Builtin)
                    .map(|agent| (agent.name.clone(), agent.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let mut roots = self.discovery_roots.clone();
        roots.agents = self.agent_search_paths();
        roots.skills = self.skill_discovery(&self.paths.working_directory, self.project_trusted);
        discover_extensions(&roots, builtin_agents, self.skill_seed(), BTreeMap::new())
    }

    /// Where a session looks for skills and what it publishes once it has
    /// looked, read from the merged document at call time.
    ///
    /// Reference `SkillManager` holds a `config_getter` and recomputes its
    /// search paths per construction, so a `skill_paths` entry written between
    /// two sessions is read by the second. Reading the snapshot here rather
    /// than caching the roots is what reproduces that.
    #[must_use]
    pub fn skill_discovery(&self, working_directory: &Path, trusted: bool) -> SkillDiscovery {
        let snapshot = self.config.load().ok();
        let configured = snapshot
            .as_ref()
            .map(ConfigSnapshot::skill_paths)
            .unwrap_or_default();
        let mut projects = Vec::new();
        if trusted {
            projects.push(working_directory.to_path_buf());
            projects.extend(project_skill_roots(&self.config, trusted));
        }
        // Reference `_discover_registry_skills`: the pins load only while the
        // experiment is on, from the global manifest and the open roots'.
        let registry = snapshot
            .as_ref()
            .is_some_and(ConfigSnapshot::registry_skills_enabled)
            .then(|| {
                let mut project_roots: Vec<PathBuf> = Vec::new();
                for root in &projects {
                    if !project_roots.contains(root) {
                        project_roots.push(root.clone());
                    }
                }
                RegistrySources {
                    vibe_home: self.paths.vibe_home.clone(),
                    project_roots,
                }
            });
        SkillDiscovery {
            roots: search_paths(&SearchInputs {
                configured: &configured,
                projects: &projects,
                vibe_home: &self.paths.vibe_home,
                // The operator's home is the Vibe home's parent. Reference `AGENTS_HOME`
                // hangs off `Path.home()` and ignores `VIBE_HOME`; the two
                // agree on every default installation, where the Vibe home is
                // `~/.vibe`, and this spelling keeps a relocated home from
                // reaching outside itself.
                user_home: self.paths.vibe_home.parent(),
                working_directory,
            }),
            enabled: snapshot
                .as_ref()
                .map(ConfigSnapshot::enabled_skills)
                .unwrap_or_default(),
            disabled: snapshot
                .as_ref()
                .map(ConfigSnapshot::disabled_skills)
                .unwrap_or_default(),
            registry,
        }
    }

    /// Where this session reads the descriptions an operator wrote.
    ///
    /// Reference `_compute_search_paths` walks `tool_paths`, the project tool
    /// directories and the user tool directory, and `ToolManager` recomputes
    /// them per construction rather than caching them across sessions, so the
    /// snapshot is read here at call time the way [`Self::skill_discovery`]
    /// reads it.
    ///
    /// The session's own working directory contributes its `.vibe/tools` first,
    /// because a session may open a directory the service was not started in
    /// and that directory is the project the operator is working on.
    #[must_use]
    pub fn tool_descriptions(
        &self,
        working_directory: &Path,
        trusted: bool,
    ) -> DirectoryDescriptions {
        let snapshot = self.config.load().ok();
        let configured = snapshot
            .as_ref()
            .map(ConfigSnapshot::tool_paths)
            .unwrap_or_default();
        let harness = self.config.harness_files();
        let mut projects = Vec::new();
        if trusted {
            let session_tools = working_directory.join(".vibe").join("tools");
            if session_tools.is_dir() {
                projects.push(session_tools);
            }
            projects.extend(harness.project_tools_dirs());
        }
        DirectoryDescriptions::new(tool_search_paths(&ToolSearchInputs {
            configured: &configured,
            projects: &projects,
            user: &harness.user_tools_dirs(),
            // The operator's home is the Vibe home's parent, the spelling
            // `skill_discovery` already resolves `~` against.
            user_home: self.paths.vibe_home.parent(),
            working_directory,
        }))
    }

    /// The skill files discovery could not load, as `(file, message)` pairs.
    ///
    /// Reference `project_diagnostics` reads `skill_manager.config_issues` into
    /// the `diagnostics/list` response; this is the port's side of that read.
    #[must_use]
    pub fn skill_issues(&self) -> Vec<(String, String)> {
        self.catalog()
            .issues
            .into_iter()
            .filter_map(|issue| {
                issue
                    .skill_issue_message()
                    .map(|message| (issue.path.to_string_lossy().into_owned(), message))
            })
            .collect()
    }
}
