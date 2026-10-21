//! What a unified turn runs with: the model, the prompt, the tools and the
//! gate over them, the session's skills and the request metadata.
//!
//! Reference `_build_unified_runtime` (`vibe/app_server/_runtime.py`) and
//! `_build_unified_system_instructions`
//! (`vibe/app_server/_unified_harness_backend_adapter.py`), composed afresh
//! for every turn so a configuration written meanwhile is read.

use crate::server::attachment::{published_tool_names, publishes};

use super::*;

/// Each subagent type the prompt offers, with the profile overrides it runs
/// under.
type AgentProfiles = Vec<(String, Map<String, Value>)>;

impl AppServer {
    /// What the session runs on, as a turn pins it: the model and its level.
    pub(crate) fn unified_pins(&self, session_id: &str) -> SessionPins {
        let model = self.workspace.active_model_alias();
        let thinking = self
            .lock_sessions()
            .ok()
            .and_then(|sessions| {
                let session = sessions.get(session_id)?;
                self.workspace
                    .session_model(&crate::server::turns::prompt_scope(session, None))
                    .ok()
                    .flatten()
            })
            .map(|model| model.thinking.as_str().to_owned());
        SessionPins {
            model,
            reasoning_effort: thinking,
            agent: None,
        }
    }

    pub(crate) fn unified_environment(
        &self,
        unified: &UnifiedSession,
        launch: Option<&vibe_core::telemetry::LaunchContext>,
    ) -> Result<TurnEnvironment, ProtocolFault> {
        let sessions = self.lock_sessions()?;
        let session = sessions
            .get(&unified.id)
            .ok_or_else(|| ProtocolFault::internal("the session is gone"))?;
        let model = self
            .workspace
            .session_model(&crate::server::turns::prompt_scope(session, None))
            .ok()
            .flatten();
        let auto_approve = session.intent.auto_approve
            || session
                .agent_summary
                .as_ref()
                .is_some_and(|agent| agent["name"] == "auto-approve");
        let smart_approve = !auto_approve
            && session
                .agent_summary
                .as_ref()
                .is_some_and(|agent| agent["name"] == "smart-approve");
        let edits = session.intent.approval == vibe_core::extensions::AgentApproval::Edits;
        let enabled = session.intent.enabled_tools.clone();
        let disabled = session.intent.disabled_tools.clone();
        let scope = crate::server::turns::prompt_scope(session, None);
        let compact_threshold = session.context_window;
        // Reference `_build_unified_runtime`: `max_turns or 1_000`.
        let max_iterations = session
            .intent
            .max_turns
            .and_then(|turns| u32::try_from(turns).ok())
            .filter(|turns| *turns > 0)
            .unwrap_or(1_000);
        drop(sessions);
        let tools = self
            .resources
            .lock()
            .ok()
            .and_then(|resources| resources.session_tool_registry(&unified.id));
        let available: Vec<String> = tools
            .as_ref()
            .and_then(|tools| tools.list().ok())
            .map(|specs| {
                published_tool_names(
                    specs.into_iter().map(|spec| spec.name).collect(),
                    &enabled,
                    &disabled,
                )
            })
            .unwrap_or_default();
        let mut metadata = BTreeMap::new();
        if let Some(launch) = launch {
            metadata.insert(
                "agent_entrypoint".to_owned(),
                launch.agent_entrypoint.clone(),
            );
            metadata.insert("agent_version".to_owned(), launch.agent_version.clone());
            metadata.insert("client_name".to_owned(), launch.client_name.clone());
            metadata.insert("client_version".to_owned(), launch.client_version.clone());
            if let Some(terminal) = &launch.terminal_emulator {
                metadata.insert("terminal_emulator".to_owned(), terminal.clone());
            }
        }
        metadata.insert("os".to_owned(), vibe_core::telemetry::platform_id());
        metadata.insert("arch".to_owned(), vibe_core::telemetry::platform_arch());
        if let Some(version) = vibe_core::telemetry::platform_version() {
            metadata.insert("os_version".to_owned(), version);
        }
        metadata.insert("version".to_owned(), env!("CARGO_PKG_VERSION").to_owned());
        metadata.insert("session_id".to_owned(), unified.id.clone());
        let parent = unified
            .core()
            .projector
            .state
            .session
            .parent_session_id
            .clone();
        if let Some(parent) = parent {
            metadata.insert("parent_session_id".to_owned(), parent);
        }
        metadata.insert("call_source".to_owned(), "vibe_code".to_owned());
        metadata.insert("host_kind".to_owned(), "local".to_owned());
        let provider = self
            .secondary_provider
            .clone()
            .ok_or_else(|| ProtocolFault::internal("no model provider is configured"))?;
        let cwd = PathBuf::from(&unified.cwd);
        // Reference `derive_config` under `ToolGate.CLASSIFIER`: the
        // classifier runs on the smart-approve utility model, learned at open.
        let smart = smart_approve
            .then(|| {
                self.workspace.session_utility(
                    &scope,
                    vibe_core::llm::utility::UtilityFeature::SmartApprove,
                    true,
                )
            })
            .flatten()
            .map(|selection| gate::SmartGate {
                state: Arc::clone(&unified.smart),
                core: Arc::clone(&unified.core),
                classifier: vibe_harness::smart::Classifier {
                    provider: Arc::clone(&provider),
                    model: selection.model,
                    metadata: metadata.clone(),
                    session_id: unified.id.clone(),
                },
                roots: vec![cwd.to_string_lossy().into_owned()],
            });
        let provided = provided::provided(
            tools.as_ref(),
            &available,
            self.session_plugins(&unified.id).as_deref(),
        );
        let mut callables = vibe_harness::programmatic::inventory();
        callables.extend(provided.callables);
        let (context, agent_types) =
            self.unified_prompt_context(&unified.id, scope.clone(), &callables);
        let system_prompt = vibe_harness::prompt::system_prompt(&context);
        let subagents = vibe_harness::subagents::SubagentSetup {
            prompt: vibe_harness::prompt::PromptContext {
                subagents: false,
                agent_types: Vec::new(),
                ..context
            },
            agent_types: agent_types
                .iter()
                .map(|(name, overrides)| {
                    gate::declared_agent_type(name, overrides, &available, tools.as_ref())
                })
                .collect(),
        };
        let title_route = (self.session_titles_itself(&unified.id))
            .then(|| {
                self.workspace
                    .session_utility_override(
                        &scope,
                        vibe_core::llm::utility::UtilityFeature::Title,
                    )
                    .or_else(|| model.clone())
            })
            .flatten()
            .map(|title_model| {
                Arc::new(vibe_harness::title::TitleRoute {
                    provider: Arc::clone(&provider),
                    model: title_model,
                    metadata: metadata.clone(),
                })
            });
        lock(&unified.titles).route = title_route;
        let skills = self.unified_skills(&unified.id);
        // Reference `_build_unified_runtime` (`vibe/app_server/_runtime.py`)
        // and `vibe_tool_groups` (`vibe/app_server/_provided_tools.py`): the
        // question and the todo list follow the session's tool roster, the
        // scratchpad and `cron` read its filters themselves, and
        // `skills::direct_tool` offers `skill` only when a skill is.
        let published = publishes(&enabled, &disabled);
        // Reference `_build_unified_runtime`: one command environment serves
        // the shell tool and `process.*`, configured through the `shell` key of
        // the tool it names.
        let host = vibe_core::tools::shell::HostShells::detect();
        let command_environment = host.command_environment();
        let configured_shell = self
            .workspace
            .tool_config()
            .resolve(command_environment.config_tool())
            .text("shell");
        let configured_shell = (!configured_shell.is_empty()).then_some(configured_shell);
        let model_tools = vibe_harness::catalog::model_tools(command_environment)
            .into_iter()
            .filter(|tool| match tool.name {
                "skill" => !skills.is_empty(),
                "ask_user_question" | "todo" => available.iter().any(|name| name == tool.name),
                "unified_harness_scratchpad" | "cron" => published(tool.name),
                _ => true,
            })
            .collect();
        Ok(TurnEnvironment {
            provider,
            model,
            system_prompt,
            tools: model_tools,
            metadata,
            workspace: vibe_harness::tools::files::Workspace {
                cwd: cwd.clone(),
                roots: vec![cwd.clone()],
                bounded: !auto_approve,
            },
            shell: vibe_harness::tools::shell::ShellContext {
                cwd,
                host,
                shell: configured_shell,
                env: BTreeMap::new(),
            },
            gate: gate::session_gate(
                auto_approve,
                edits,
                tools,
                available,
                smart,
                Arc::clone(&provided.routes),
            ),
            skills,
            functions: Arc::new(vibe_harness::programmatic::documents(&callables)),
            callables: Arc::new(callables),
            provided: Some(provided.routes),
            session_dir: unified
                .host()
                .store
                .as_ref()
                .map(|store| store.dir().to_path_buf()),
            subagents: Some(Arc::new(subagents)),
            message_id: None,
            compact_threshold,
            max_iterations: Some(max_iterations),
        })
    }

    /// What the system prompt of a root session in `scope` is built from:
    /// the core's sections over the host's instructions and the session's
    /// catalogs, with the subagent types and the instructions each runs
    /// under. Reference `_build_unified_system_instructions` for the host
    /// half, with the variant table `build_vibe_code_system_instructions`
    /// reads (an unknown variant is the `cli` one).
    fn unified_prompt_context(
        &self,
        session_id: &str,
        scope: crate::workspace::SessionPromptScope,
        callables: &[vibe_harness::programmatic::Callable],
    ) -> (vibe_harness::prompt::PromptContext, AgentProfiles) {
        use vibe_harness::prompt::{
            AgentTypeEntry, KnowledgeEntry, PluginEntry, PromptContext, SkillEntry,
        };
        let workspace = &self.workspace;
        let host = crate::workspace::PromptHost::detect(&[]);
        let instructions = workspace
            .system_prompt_inputs(&scope, &host)
            .map(|mut inputs| {
                let variant = match inputs.prompt_id.as_str() {
                    "cli_2026-07_v2" | "cli_2026-08_v3" => inputs.prompt_id.clone(),
                    _ => vibe_core::system_prompt::DEFAULT_SYSTEM_PROMPT.to_owned(),
                };
                inputs.base =
                    vibe_core::system_prompt::load_system_prompt(&variant, &[]).unwrap_or_default();
                inputs.headless = false;
                inputs.include_commit_signature = false;
                inputs.include_model_info = false;
                inputs.include_prompt_detail = false;
                vibe_core::system_prompt::compose(&inputs).text()
            })
            .unwrap_or_default();
        let plugins = self.session_plugins(session_id);
        let mut context = PromptContext {
            instructions,
            background_processes: true,
            subagents: true,
            groups: vibe_harness::programmatic::namespaces(callables)
                .into_iter()
                .map(|(group, _)| group)
                .filter(|group| group != "subagent")
                .collect(),
            ..PromptContext::default()
        };
        let skills = self.unified_skills(session_id);
        let mut profile_of: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
        if let Some(plugins) = &plugins {
            let resolution = &plugins.materialized.resolution;
            context.skills = plugins
                .runtime_skills
                .iter()
                .filter(|(_, skill)| skill.model_invocable)
                .filter(|(alias, _)| skills.iter().any(|(name, _)| name == alias))
                .map(|(alias, skill)| SkillEntry {
                    name: alias.clone(),
                    description: skill.description.clone(),
                    path: skill
                        .path
                        .as_ref()
                        .map(|path| packaged_path(plugins, alias, path).display().to_string())
                        .unwrap_or_default(),
                })
                .collect();
            context
                .skills
                .sort_by(|left, right| left.name.cmp(&right.name));
            context.plugins = resolution
                .plugins
                .iter()
                .map(|plugin| PluginEntry {
                    name: plugin.name.clone(),
                    description: plugin.description.clone(),
                    path: plugin.root.display().to_string(),
                    namespaces: plugins
                        .materialized
                        .tool_groups
                        .iter()
                        .filter(|group| group.plugin_name == plugin.name)
                        .map(|group| group.name.clone())
                        .collect(),
                })
                .collect();
            context.knowledge = plugins
                .materialized
                .knowledge
                .iter()
                .map(|folder| KnowledgeEntry {
                    name: folder.name.clone(),
                    description: folder.description.clone(),
                    path: folder.runtime_root.display().to_string(),
                    access: "read_only".to_owned(),
                    plugin: Some(folder.plugin_name.clone()),
                })
                .collect();
            context.agent_types = resolution
                .agents
                .iter()
                .map(|agent| AgentTypeEntry {
                    name: agent.name.clone(),
                    description: agent.description.clone(),
                    path: agent.source_file.display().to_string(),
                })
                .collect();
            profile_of.extend(resolution.agents.iter().map(|agent| {
                let mut overrides = agent.overrides.clone();
                if let Some(instructions) = &agent.instructions {
                    overrides.insert("instructions".to_owned(), json!(instructions));
                }
                (agent.name.clone(), overrides)
            }));
        }
        let reserved: Vec<String> = context
            .agent_types
            .iter()
            .map(|agent| agent.name.clone())
            .collect();
        if let Ok(agents) = workspace.prompt_agents(&scope) {
            context.agent_types.extend(
                agents
                    .into_iter()
                    .filter(|agent| agent.kind == vibe_core::extensions::AgentKind::Subagent)
                    .filter(|agent| !reserved.contains(&agent.name))
                    .filter(|agent| {
                        !agent.name.trim().is_empty() && !agent.description.trim().is_empty()
                    })
                    .filter_map(|agent| {
                        let path = agent.path?;
                        if let Ok(Value::Object(overrides)) = serde_json::to_value(&agent.overrides)
                        {
                            profile_of.insert(agent.name.clone(), overrides);
                        }
                        Some(AgentTypeEntry {
                            name: agent.name,
                            description: agent.description,
                            path: path.display().to_string(),
                        })
                    }),
            );
        }
        let roots: Vec<SkillEntry> = self
            .workspace
            .unified_root_skills()
            .into_iter()
            .filter(|skill| skill.model_invocable)
            .filter(|skill| !context.skills.iter().any(|known| known.name == skill.name))
            .map(|skill| SkillEntry {
                name: skill.name,
                description: skill.description,
                path: skill
                    .path
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
            })
            .collect();
        context.skills.extend(roots);
        context
            .skills
            .sort_by(|left, right| left.name.cmp(&right.name));
        let agent_types = context
            .agent_types
            .iter()
            .map(|agent| {
                (
                    agent.name.clone(),
                    profile_of.remove(&agent.name).unwrap_or_default(),
                )
            })
            .collect();
        (context, agent_types)
    }

    /// The skills a unified session reads: its plugins' model-invocable ones,
    /// by their qualified names, then the root's own.
    fn unified_skills(&self, session_id: &str) -> Vec<(String, String)> {
        let mut skills: Vec<(String, String)> = self
            .session_plugins(session_id)
            .map(|plugins| {
                plugins
                    .runtime_skills
                    .iter()
                    .filter(|(_, skill)| skill.model_invocable)
                    .map(|(alias, skill)| {
                        (
                            alias.clone(),
                            vibe_core::tools::builtins::skill_payload(skill, alias),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        for skill in self.workspace.unified_root_skills() {
            if skill.model_invocable && !skills.iter().any(|(name, _)| *name == skill.name) {
                let payload = vibe_core::tools::builtins::skill_payload(&skill, &skill.name);
                skills.push((skill.name, payload));
            }
        }
        skills.sort_by(|left, right| left.0.cmp(&right.0));
        skills
    }
}

/// Where a plugin skill's file sits inside the package the session mounted:
/// the reference lists a skill under its plugin's package root, which is
/// where the session reads it from, rather than where it was installed.
fn packaged_path(plugins: &crate::plugins::SessionPlugins, alias: &str, path: &Path) -> PathBuf {
    let namespace = alias
        .split_once(':')
        .map_or(alias, |(namespace, _)| namespace);
    plugins
        .materialized
        .resolution
        .plugins
        .iter()
        .filter(|plugin| plugin.namespace == namespace)
        .find_map(|plugin| {
            let installed = plugins.installed_roots.get(&plugin.name)?;
            let relative = path.strip_prefix(installed).ok()?;
            Some(plugin.root.join(relative))
        })
        .unwrap_or_else(|| path.to_path_buf())
}
