//! The agent, skill and prompt methods.
//!
//! An agent profile decides what a session may run and under which prompt, and
//! a skill is what a prompt can invoke. Discovery is `vibe_core::extensions` and
//! `vibe_core::skills`; what is here is the catalog the boundary publishes, the
//! installation state it writes, and the prompt it composes for a workspace.

use super::sessions::runtime_attachment;
use super::*;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PromptParams {
    #[serde(default)]
    base: String,
    #[serde(default)]
    prompt_id: Option<String>,
    #[serde(default)]
    headless: bool,
    #[serde(default)]
    commit_policy: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    os_tool_guidance: Option<String>,
    #[serde(default)]
    scratchpad: Option<PathBuf>,
    #[serde(default)]
    project_context: Option<String>,
    #[serde(default)]
    project_context_stale: bool,
    #[serde(default)]
    add_directories: Vec<PathBuf>,
    #[serde(default)]
    resources: Vec<UserResource>,
    #[serde(default)]
    supports_images: bool,
}

impl Release3Service {
    pub(crate) fn agent_profile(&self, name: &str) -> Result<AgentProfile, Release3Error> {
        if name == "lean" && !self.installed_agent_names()?.contains(name) {
            return Err(Release3Error::InvalidParams(
                "agent `lean` must be installed with /leanstall first".to_owned(),
            ));
        }
        let profile = self.catalog().agents.remove(name).map_or_else(
            || {
                if self.persist_runtime_sessions {
                    Err(Release3Error::Extension(format!(
                        "agent `{name}` was not found"
                    )))
                } else {
                    Ok(AgentProfile {
                        name: name.to_owned(),
                        display_name: name.to_owned(),
                        description: "Externally supplied agent profile".to_owned(),
                        kind: AgentKind::Agent,
                        safety: "neutral".to_owned(),
                        overrides: Table::new(),
                        source: ExtensionSource::Configured,
                        path: None,
                    })
                }
            },
            Ok,
        )?;
        if profile.kind != AgentKind::Agent {
            return Err(Release3Error::Extension(format!(
                "subagent `{name}` cannot be selected as the primary agent"
            )));
        }
        if let Some(prompt_id) = profile.runtime_settings().system_prompt_id
            && builtin_agents::system_prompt(&prompt_id).is_none()
        {
            return Err(Release3Error::InvalidParams(format!(
                "agent `{name}` references unsupported system prompt `{prompt_id}`"
            )));
        }
        Ok(profile)
    }

    pub(crate) fn default_agent_name(&self) -> Result<String, Release3Error> {
        Ok(self
            .config
            .load()
            .map_err(config_error)?
            .effective
            .get("default_agent")
            .and_then(TomlValue::as_str)
            .unwrap_or("default")
            .to_owned())
    }

    /// The agent catalog as `AgentsListResponse` declares it.
    ///
    /// `active` is the agent a fresh session would run, which the server
    /// replaces with the one the addressed session actually runs. It is always
    /// published: the field is required, and a client that cannot resolve it
    /// has no agent to render as selected.
    pub(super) fn agents_list(&self) -> Result<Release3Dispatch, Release3Error> {
        let profiles = self.available_agents()?;
        let active = self
            .default_agent_name()
            .ok()
            .and_then(|name| {
                profiles
                    .iter()
                    .find(|profile| profile.name == name)
                    .cloned()
            })
            .or_else(|| profiles.first().cloned())
            .unwrap_or_else(builtin_agents::default_profile);
        Ok(Release3Dispatch::result([
            ("active", agent_summary(&active)),
            (
                "agents",
                Value::Array(profiles.iter().map(agent_summary).collect()),
            ),
        ]))
    }

    /// Every agent profile a session may run, with the uninstalled builtins
    /// filtered out.
    pub(super) fn available_agents(&self) -> Result<Vec<AgentProfile>, Release3Error> {
        let installed = self.installed_agent_names()?;
        Ok(self
            .catalog()
            .agents
            .into_values()
            .filter(|profile| profile.name != "lean" || installed.contains("lean"))
            .collect())
    }

    /// The configuration, catalogs and diagnostics `RuntimeSnapshot` carries.
    ///
    /// The server owns the rest of the snapshot: the tool surface, the
    /// integrations and the session's own accounting. `active_agent` names the
    /// profile the session runs, which this service cannot know on its own.
    pub fn runtime_projection(&self, active_agent: Option<&str>) -> RuntimeProjection {
        let snapshot = self.config.load().ok();
        let view = snapshot
            .as_ref()
            .map_or_else(|| Value::Object(Map::new()), ConfigSnapshot::config_view);
        let catalog = self.catalog();
        let installed = self.installed_agent_names().unwrap_or_default();
        let profiles = catalog
            .agents
            .values()
            .filter(|profile| profile.name != "lean" || installed.contains("lean"))
            .cloned()
            .collect::<Vec<_>>();
        let active = active_agent
            .and_then(|name| {
                profiles
                    .iter()
                    .find(|profile| profile.name == name)
                    .cloned()
            })
            .or_else(|| {
                let default = self.default_agent_name().ok()?;
                profiles
                    .iter()
                    .find(|profile| profile.name == default)
                    .cloned()
            })
            .unwrap_or_else(builtin_agents::default_profile);
        RuntimeProjection {
            // No agent overlay is applied to the published configuration here,
            // so the two views are the same document, as the reference host
            // path also answers with.
            base_config: view.clone(),
            config: view,
            active_agent: agent_summary(&active),
            agents: profiles.iter().map(agent_summary).collect(),
            skills: catalog.skills.values().map(skill_summary).collect(),
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
            hooks_count: catalog.hooks.len(),
        }
    }

    pub(super) fn agent_install(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        if let Some(name) = params.get("agentName").and_then(Value::as_str) {
            self.set_builtin_agent_installed(name, true)?;
            return self.agents_list();
        }
        let source = self.authorized_existing_path(Path::new(required_string(params, "path")?))?;
        self.agents
            .lock()
            .map_err(|_| Release3Error::StatePoisoned)?
            .install(&source)
            .map_err(|error| Release3Error::Extension(error.to_string()))?;
        // Both forms answer with the catalog the change produced, which is what
        // `AgentsListResponse` declares and what a client re-renders from.
        self.agents_list()
    }

    pub(super) fn agent_uninstall(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        if let Some(name) = params.get("agentName").and_then(Value::as_str) {
            self.set_builtin_agent_installed(name, false)?;
            let mut dispatch = self.agents_list()?;
            if let Some(session_id) = params.get("sessionId").and_then(Value::as_str)
                && self
                    .store
                    .load(session_id)
                    .map_err(storage_error)?
                    .metadata
                    .agent_profile
                    .as_ref()
                    .and_then(|profile| profile.get("name"))
                    .and_then(Value::as_str)
                    == Some(name)
            {
                // The session was running the agent that just went away, so it
                // falls back to the default and the answer names it as active.
                let (profile, hydrated) = self.set_session_agent(session_id, "default")?;
                dispatch
                    .result
                    .insert("active".to_owned(), agent_summary(&profile));
                dispatch.attachment = Some(runtime_attachment(&hydrated));
            }
            return Ok(dispatch);
        }
        self.agents
            .lock()
            .map_err(|_| Release3Error::StatePoisoned)?
            .uninstall(required_string(params, "name")?)
            .map_err(|error| Release3Error::Extension(error.to_string()))?;
        self.agents_list()
    }

    pub(super) fn set_builtin_agent_installed(
        &self,
        name: &str,
        install: bool,
    ) -> Result<(), Release3Error> {
        if name != "lean" {
            return Err(Release3Error::InvalidParams(format!(
                "unknown installable built-in agent `{name}`"
            )));
        }
        let snapshot = self.config.load().map_err(config_error)?;
        let mut installed = snapshot
            .effective
            .get("installed_agents")
            .and_then(TomlValue::as_array)
            .into_iter()
            .flatten()
            .filter_map(TomlValue::as_str)
            .map(ToOwned::to_owned)
            .collect::<BTreeSet<_>>();
        if install {
            installed.insert(name.to_owned());
        } else {
            installed.remove(name);
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

    pub(super) fn installed_agent_names(&self) -> Result<BTreeSet<String>, Release3Error> {
        Ok(self
            .config
            .load()
            .map_err(config_error)?
            .effective
            .get("installed_agents")
            .and_then(TomlValue::as_array)
            .into_iter()
            .flatten()
            .filter_map(TomlValue::as_str)
            .map(ToOwned::to_owned)
            .collect())
    }

    pub(super) fn agent_update(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let session_id = required_string(params, "sessionId")?;
        let name = required_string(params, "name")?;
        let (profile, hydrated) = self.set_session_agent(session_id, name)?;
        Ok(Release3Dispatch {
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
    pub(super) fn skills_list(&self) -> Result<Release3Dispatch, Release3Error> {
        let catalog = self.catalog();
        Ok(Release3Dispatch::result([(
            "skills",
            Value::Array(catalog.skills.values().map(skill_summary).collect()),
        )]))
    }

    pub(super) fn prompt_prepare(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let prompt: PromptParams = serde_json::from_value(Value::Object(
            params.clone().into_iter().collect::<Map<_, _>>(),
        ))
        .map_err(|error| Release3Error::InvalidParams(error.to_string()))?;
        let additional_directories = prompt
            .add_directories
            .iter()
            .map(|path| self.authorized_existing_path(path))
            .collect::<Result<Vec<_>, _>>()?;
        let catalog = self.catalog();
        let user_home = self
            .paths
            .vibe_home
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.paths.vibe_home.clone());
        let project_roots = if self.project_trusted {
            vec![(
                self.paths.working_directory.clone(),
                self.paths.working_directory.clone(),
            )]
        } else {
            Vec::new()
        };
        let loader = InstructionLoader::new(user_home, project_roots);
        let base = match prompt.prompt_id.as_deref() {
            Some(prompt_id) => {
                PromptResolver::new(
                    vec![self.paths.working_directory.join(".vibe/prompts")],
                    vec![self.paths.vibe_home.join("extensions/prompts")],
                    BTreeMap::new(),
                    self.project_trusted,
                )
                .resolve(prompt_id)
                .map_err(|error| Release3Error::Prompt(error.to_string()))?
                .content
            }
            None => prompt.base,
        };
        let composition = PromptComposition {
            base,
            headless: prompt.headless,
            commit_policy: prompt.commit_policy,
            model_info: prompt.model,
            os_tool_guidance: prompt.os_tool_guidance,
            skills: catalog
                .skills
                .values()
                .map(|skill| SkillSummary {
                    name: skill.name.clone(),
                    description: skill.description.clone(),
                    path: skill.path.clone(),
                })
                .collect(),
            subagents: catalog
                .agents
                .values()
                .filter(|agent| agent.kind == AgentKind::Subagent)
                .map(|agent| SubagentSummary {
                    name: agent.name.clone(),
                    description: agent.description.clone(),
                })
                .collect(),
            scratchpad: prompt.scratchpad,
            project_context: prompt.project_context,
            project_context_stale: prompt.project_context_stale,
            additional_directories: additional_directories.clone(),
            user_instructions: loader
                .user_document()
                .map_err(|error| Release3Error::Prompt(error.to_string()))?,
            project_instructions: loader
                .project_documents()
                .map_err(|error| Release3Error::Prompt(error.to_string()))?,
        }
        .compose();
        let mut roots = vec![self.paths.working_directory.clone()];
        roots.extend(additional_directories);
        let prepared = prepare_user_resources(&prompt.resources, &roots, prompt.supports_images)
            .map_err(|error| Release3Error::Prompt(error.to_string()))?;
        Ok(Release3Dispatch::result([
            ("prompt", serde_json::to_value(composition)?),
            ("user", serde_json::to_value(prepared)?),
            ("issues", serde_json::to_value(catalog.issues)?),
        ]))
    }

    pub(super) fn authorized_existing_path(&self, path: &Path) -> Result<PathBuf, Release3Error> {
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.paths.working_directory.join(path)
        };
        let canonical = fs::canonicalize(&candidate).map_err(|error| {
            Release3Error::InvalidParams(format!(
                "authorized path `{}` cannot be resolved: {error}",
                candidate.display()
            ))
        })?;
        let authorized = self
            .allowed_roots
            .iter()
            .any(|root| fs::canonicalize(root).is_ok_and(|allowed| canonical.starts_with(allowed)));
        if !authorized {
            return Err(Release3Error::InvalidParams(format!(
                "path `{}` is outside server-authorized workspace roots",
                canonical.display()
            )));
        }
        Ok(canonical)
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
        roots.skills = self.skill_discovery(&self.paths.working_directory, self.project_trusted);
        let mut seeded = vibe_core::skills::builtins::builtin_skills();
        // `experimental_enable_registry_skills` gates the ported registry
        // subtree: disabled returns before any registry code runs, so no cache
        // directory is created and no transport is constructed, and enabled
        // reaches the subtree through its one door, which publishes nothing
        // until the reference publishes a load lifecycle to reproduce
        // (skills-parity PRD, open question 3). Either way the catalog is
        // unchanged, which is the reference's own behavior for a key nothing
        // upstream reads.
        if self
            .config
            .load()
            .ok()
            .is_some_and(|snapshot| snapshot.registry_skills_enabled())
        {
            seeded.extend(vibe_core::skills::registry::published_skills());
        }
        discover_extensions(&roots, builtin_agents, seeded, BTreeMap::new())
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
        SkillDiscovery {
            roots: search_paths(&SearchInputs {
                configured: &configured,
                projects: &projects,
                vibe_home: &self.paths.vibe_home,
                // The operator's home is the Vibe home's parent, which is what
                // `prompt_prepare` already reads it as. Reference `AGENTS_HOME`
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
        }
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
            .filter(|issue| issue.mechanism == "skills")
            .map(|issue| (issue.path.to_string_lossy().into_owned(), issue.message))
            .collect()
    }
}
