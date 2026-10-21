//! What the server does around a session besides answering its methods: the
//! resources it opens, the tools it registers, and the runtime it publishes.
//!
//! A session is more than its transcript. It has a workspace with tools bound to
//! a policy, a set of integrations the resource service owns, and a projection
//! the client reads. Attaching, refreshing and closing all three is here; the
//! session's own lifecycle is not.

use super::*;

mod deferred;
mod runtime_view;

pub(super) fn apply_persisted_session_settings(
    intent: &mut SessionIntent,
    hydrated: &HydratedSession,
) {
    let config = &hydrated.metadata.config;
    if let Some(value) = config.get("active_model").and_then(Value::as_str) {
        intent.model = Some(value.to_owned());
    }
    if let Some(value) = config.get("maxTurns").and_then(Value::as_i64) {
        intent.max_turns = Some(value);
    }
    if let Some(value) = config.get("maxTokens").and_then(Value::as_i64) {
        intent.max_tokens = Some(value);
    }
    if let Some(value) = config.get("mode").and_then(Value::as_str)
        && matches!(value, "code" | "plan")
    {
        intent.mode = Some(value.to_owned());
    }
    if let Some(value) = config.get("thinking").and_then(Value::as_bool) {
        intent.thinking = value;
    }
    if let Some(value) = config.get("reasoningEffort").and_then(Value::as_str)
        && matches!(value, "low" | "medium" | "high" | "max")
    {
        intent.reasoning_effort = Some(value.to_owned());
    }
    if let Some(value) = config.get("autoApprove").and_then(Value::as_bool) {
        intent.auto_approve = value;
    }
}

/// Whether a session publishes a tool by its name, under the session's
/// enabled and disabled lists.
pub(super) fn publishes(enabled: &[String], disabled: &[String]) -> impl Fn(&str) -> bool {
    let enabled = (!enabled.is_empty()).then(|| NameFilter::new(enabled));
    let disabled = NameFilter::new(disabled);
    move |name| {
        enabled.as_ref().is_none_or(|enabled| enabled.matches(name)) && !disabled.matches(name)
    }
}

/// Narrows a `runtime/read` tool list to what the session publishes.
///
/// Reference `project_tools` reads `ToolManager.available_tools`, which the
/// agent profile and the session's lists already filtered, and which holds
/// `task` from the start. Here `task` joins the registry only once a turn
/// builds the live client, so it is placed by name before the first turn.
pub(super) fn publish_session_tools(
    tools: &mut Vec<Value>,
    enabled: &[String],
    disabled: &[String],
) {
    publish_named(
        tools,
        |tool| tool.get("name").and_then(Value::as_str),
        || json!({"name": "task", "isCustom": false}),
        &publishes(enabled, disabled),
    );
}

/// [`publish_session_tools`] over bare tool names.
pub(super) fn published_tool_names(
    mut names: Vec<String>,
    enabled: &[String],
    disabled: &[String],
) -> Vec<String> {
    publish_named(
        &mut names,
        |name| Some(name.as_str()),
        || "task".to_owned(),
        &publishes(enabled, disabled),
    );
    names
}

fn publish_named<T>(
    tools: &mut Vec<T>,
    name_of: impl Fn(&T) -> Option<&str>,
    task: impl FnOnce() -> T,
    published: &impl Fn(&str) -> bool,
) {
    if !tools.iter().any(|tool| name_of(tool) == Some("task")) {
        let at = tools
            .iter()
            .position(|tool| name_of(tool).is_some_and(|name| name > "task"))
            .unwrap_or(tools.len());
        tools.insert(at, task());
    }
    tools.retain(|tool| name_of(tool).is_none_or(published));
}

pub(super) fn apply_agent_profile_settings(intent: &mut SessionIntent, profile: &AgentProfile) {
    let settings = profile.runtime_settings();
    intent.enabled_tools = if settings.enabled_tools.is_empty() {
        intent.requested_enabled_tools.clone()
    } else {
        settings.enabled_tools
    };
    intent
        .disabled_tools
        .clone_from(&intent.requested_disabled_tools);
    intent.disabled_tools.extend(settings.disabled_tools);
    intent.disabled_tools.sort();
    intent.disabled_tools.dedup();
    intent.agent_permission_rules = settings.permission_rules;
    intent.approval = settings.approval;
    intent.auto_approve = intent.requested_auto_approve || settings.approval == AgentApproval::All;
    intent.system_prompt_id = settings.system_prompt_id;
    if let Some(model) = settings.model {
        intent.model = Some(model);
    }
    if let Some(thinking) = settings.thinking {
        intent.thinking = thinking;
    }
    if settings.reasoning_effort.is_some() {
        intent.reasoning_effort = settings.reasoning_effort;
    }
    if settings.mode.is_some() {
        intent.mode = settings.mode;
    }
}

pub(super) fn recompose_agent_profile_settings(
    intent: &mut SessionIntent,
    hydrated: &HydratedSession,
    profile: Option<&AgentProfile>,
) {
    intent
        .enabled_tools
        .clone_from(&intent.requested_enabled_tools);
    intent
        .disabled_tools
        .clone_from(&intent.requested_disabled_tools);
    intent.model = None;
    intent.mode = None;
    intent.thinking = false;
    intent.reasoning_effort = None;
    intent.auto_approve = intent.requested_auto_approve;
    intent.approval = AgentApproval::Prompt;
    intent.agent_permission_rules.clear();
    intent.system_prompt_id = None;
    apply_persisted_session_settings(intent, hydrated);
    if let Some(profile) = profile {
        apply_agent_profile_settings(intent, profile);
    }
}

impl AppServer {
    /// What a session opened in `working_directory` trusts: the directory,
    /// after a `trustWorkspace` grant is recorded for the rest of the process,
    /// and the project configuration file as of now, which the session keeps.
    ///
    /// Reference `_build_session_config`: the grant goes into the process-wide
    /// trust store, so it is also what a later `workspace/trust/status` and a
    /// later session in the same directory read.
    pub(crate) fn session_trust(
        &self,
        working_directory: &Path,
        grant: bool,
    ) -> (bool, Option<bool>) {
        let vibe_home = self.workspace.vibe_home();
        let store = vibe_core::trust::TrustStore::for_vibe_home(vibe_home);
        if grant {
            store.trust_for_session(working_directory);
        }
        let trusted = store.is_trusted(working_directory) == Some(true);
        let project_file_trust = vibe_core::config::harness::discover_project_config(
            working_directory,
            vibe_home,
        )
        .map(|file| {
            vibe_core::trust::project_config_trusted(&store, &file, working_directory, trusted)
        });
        (trusted, project_file_trust)
    }

    pub fn live_projection_seed(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<ProjectionSnapshot, ServerError> {
        let sessions = self.lock_sessions();
        let session = sessions.active_turn(session_id, turn_id)?;
        let mut snapshot = session
            .snapshot
            .clone()
            .unwrap_or_else(|| ProjectionSnapshot {
                session_id: session.id.clone(),
                turn_id: Some(turn_id.to_owned()),
                handoff_cause: None,
                watermark: 0,
                lifecycle: LifecycleState::Idle,
                title: None,
                history: Vec::new(),
            });
        snapshot.session_id.clone_from(&session.id);
        snapshot.turn_id = Some(turn_id.to_owned());
        snapshot.watermark = 0;
        snapshot.lifecycle = LifecycleState::Idle;
        Ok(snapshot)
    }

    pub fn apply_live_projection(
        &self,
        session_id: &str,
        turn_id: &str,
        snapshot: ProjectionSnapshot,
    ) -> Result<u64, ServerError> {
        let mut sessions = self.lock_sessions();
        let session = sessions.active_turn_mut(session_id, turn_id)?;
        if snapshot.session_id != session.id || snapshot.turn_id.as_deref() != Some(turn_id) {
            return Err(ServerError::SessionConflict(snapshot.session_id));
        }
        let event_id = next_event_id(session);
        session.snapshot = Some(merge_server_callback_history(
            session.snapshot.as_ref(),
            snapshot,
        ));
        session.updated_at = now_millis();
        Ok(event_id)
    }

    /// Publishes the warning a full checkpoint log raised, once.
    ///
    /// A log with no room left refuses to capture rather than dropping what it
    /// already holds, and an operator whose file history quietly stopped being
    /// tracked has no way to notice. `diagnostics/list` is where a client reads
    /// what the session could not do, so that is where this lands.
    pub(crate) fn publish_retention_notice(&self, review: &ReviewManager) {
        let Some(notice) = review.take_retention_notice() else {
            return;
        };
        lock(&self.resources).record_diagnostic(FILE_HISTORY_LABEL, &notice);
    }

    pub(crate) fn orphaned_resource_generation(
        &self,
        session_id: &str,
    ) -> Result<Option<u64>, ServerError> {
        let sessions = self.lock_sessions();
        Ok(sessions
            .get(session_id)
            .filter(|session| session.attachments == 0)
            .map(|session| session.resource_generation))
    }

    pub(super) fn attach_workspace_runtime(
        &self,
        attachment: &RuntimeAttachment,
        review_override: Option<Arc<ReviewManager>>,
    ) -> Result<(), ServerError> {
        let mut sessions = self.lock_sessions();
        if let Some(session) = sessions.get_mut(&attachment.id) {
            session.attachments = session.attachments.saturating_add(1);
            session.persisted = Some(attachment.hydrated.clone());
            recompose_agent_profile_settings(
                &mut session.intent,
                &attachment.hydrated,
                attachment.agent_profile.as_ref(),
            );
            session.agent_summary = attachment
                .agent_profile
                .as_ref()
                .map(crate::workspace::agent_summary);
            if review_override.is_some() {
                session.review = review_override;
            }
            session.updated_at = now_millis();
            let session_id = session.id.clone();
            drop(sessions);
            return self.refresh_session_workspace_tools(&session_id);
        }
        let policy = PermissionStore::default()
            .with_tool_config(self.workspace.tool_config())
            .with_allowlist_persistence(self.workspace.allowlist_persistence());
        let tools = ToolRegistry::default();
        // A resumed session runs under the same configuration a fresh one does,
        // so its two filter lists are read again here rather than left empty.
        let (trusted, project_file_trust) =
            self.session_trust(Path::new(&attachment.working_directory), false);
        let (enabled_tools, disabled_tools) = self
            .workspace
            .tool_filters_for_session(Path::new(&attachment.working_directory), trusted)
            .unwrap_or_default();
        let mut intent = SessionIntent {
            trusted,
            project_file_trust,
            agent: attachment.agent.clone(),
            resume: Some(attachment.id.clone()),
            requested_enabled_tools: enabled_tools.clone(),
            requested_disabled_tools: disabled_tools.clone(),
            enabled_tools,
            disabled_tools,
            ..SessionIntent::default()
        };
        apply_persisted_session_settings(&mut intent, &attachment.hydrated);
        if let Some(profile) = &attachment.agent_profile {
            apply_agent_profile_settings(&mut intent, profile);
        }
        policy
            .try_replace_rules_with_rationale_prefix(
                "agent-profile:",
                intent.agent_permission_rules.clone(),
            )
            .map_err(|error| ServerError::Resource(error.to_string()))?;
        let configured_mcp = self
            .workspace
            .mcp_servers_for_session(Path::new(&attachment.working_directory), trusted, &[])
            .unwrap_or_default();
        self.open_session_plugins(
            &attachment.id,
            Path::new(&attachment.working_directory),
            trusted,
            &intent.add_directories,
            &configured_mcp,
        );
        let review = self.register_workspace_tools(
            &attachment.id,
            &attachment.working_directory,
            &policy,
            &tools,
            &intent,
            review_override,
        )?;
        self.session_tool_factory
            .register(&attachment.id, &tools)
            .map_err(ServerError::Resource)?;
        let mut session = SessionRuntime::new(
            attachment.id.clone(),
            attachment.working_directory.clone(),
            intent,
            policy.clone(),
            tools.clone(),
            review,
            now_millis(),
        );
        session.persisted = Some(attachment.hydrated.clone());
        session.agent_summary = attachment
            .agent_profile
            .as_ref()
            .map(crate::workspace::agent_summary);
        session.context_window = self.workspace.context_window();
        session.pricing = self.workspace.active_model_pricing();
        session.active_model_alias = self.workspace.active_model_alias();
        session.displayed_workdir = self.workspace.displayed_workdir();
        session.compaction = self.workspace.compaction_settings();
        session.hooks = self.workspace.session_hooks(
            Path::new(&session.working_directory),
            session.intent.trusted,
            &session.intent.add_directories,
        );
        sessions.insert(session);
        self.open_session_resources(
            &mut sessions,
            ResourceSession {
                session_id: attachment.id.clone(),
                generation: 1,
                working_directory: attachment.working_directory.clone(),
                project_trusted: trusted,
                policy,
                tools,
            },
        )
        .map_err(|error| ServerError::Resource(error.to_string()))?;
        self.bind_plugin_mcp(&attachment.id);
        drop(sessions);
        self.start_registry_sync(&attachment.id);
        Ok(())
    }

    /// Registers the builtin tool surface for a session root.
    ///
    /// The universal tools need nothing from the filesystem root and register
    /// first; the workspace family registers only when the root opens.
    ///
    /// Returns the review manager that owns the session's file checkpoints, or
    /// `None` when the root is not a usable workspace.
    pub(super) fn register_workspace_tools(
        &self,
        session_id: &str,
        working_directory: &str,
        policy: &PermissionStore,
        tools: &ToolRegistry,
        intent: &SessionIntent,
        review: Option<Arc<ReviewManager>>,
    ) -> Result<Option<Arc<ReviewManager>>, ServerError> {
        let approval =
            self.approval_factory
                .for_agent(session_id, intent.approval, intent.auto_approve);
        // The delegation is resolved once per registration and handed to both
        // families, so the file tools and the shell agree on what this client
        // hosts for the session they are being published into.
        let client_io = self.client_tools.session_io(session_id);
        // Every family is handed the resolver rather than a snapshot of what it
        // currently answers, so a `tools.<name>` change observed between two
        // turns reaches the handlers without the surface being registered
        // again. The store narrows it with this session's permission
        // overrides, which is the one per-session part of the composition.
        //
        // The scratchpad opens with the session and is the one directory the
        // file tools reach without consulting a list, which is the capability
        // reference `init_scratchpad` gives the agent-loop runtime.
        // Reference `Workspace.for_session`: the working directory is always
        // inside the write boundary, trusted or not, and every `--add-dir`
        // root joins it. Trust only decides which configuration is read.
        let listed_roots = intent
            .add_directories
            .iter()
            .map(std::path::PathBuf::from)
            .collect::<Vec<_>>();
        policy
            .try_authorize_workspace(Path::new(working_directory), &listed_roots)
            .map_err(|error| ServerError::Resource(error.to_string()))?;
        let guard =
            ToolGuard::new(policy.clone(), approval).with_scratchpad(init_scratchpad(session_id));
        // The registry carries the composition so a family published later than
        // this one reads it: `task` registers when its turn resolves a subagent
        // runner, and it must be guarded by this session's store and approval
        // agent rather than by a second composition built at turn time.
        tools.set_guard(guard.clone());
        // The descriptions an operator wrote are read at every publication
        // rather than resolved here, so the source installed now also covers
        // the MCP and connector tools that register after this registration
        // returns, and a file written mid-session reaches the next turn.
        tools.set_descriptions(Arc::new(
            self.workspace
                .tool_descriptions(Path::new(working_directory), intent.trusted),
        ));
        let seeded;
        let builtin_tools = match self.plugin_skill_seed(session_id) {
            Some(seed) => {
                seeded = self.builtin_tools.as_ref().clone().with_seeded_skills(seed);
                &seeded
            }
            None => self.builtin_tools.as_ref(),
        };
        builtin_tools
            .register(
                session_id,
                self.workspace
                    .skill_discovery(Path::new(working_directory), intent.trusted),
                tools,
                &guard,
            )
            .map_err(|error| ServerError::Resource(error.to_string()))?;
        self.shell_tools
            .register(
                session_id,
                Path::new(working_directory),
                tools,
                client_io.clone(),
                &guard,
            )
            .map_err(|error| ServerError::Resource(error.to_string()))?;
        let Ok(workspace) = Workspace::open(working_directory) else {
            return Ok(None);
        };
        let workspace = Arc::new(workspace);
        let review = review.unwrap_or_else(|| Arc::new(ReviewManager::new(workspace.clone())));
        WorkspaceTools::new(workspace, review.clone())
            .with_client_io(client_io)
            .register(tools, &guard)
            .map_err(|error| ServerError::Resource(error.to_string()))?;
        Ok(Some(review))
    }

    /// Opens the in-memory and operational resource state for a session that is
    /// already in the map, removing it again if either backend refuses.
    pub(super) fn open_session_resources(
        &self,
        sessions: &mut SessionRegistry,
        session: ResourceSession,
    ) -> Result<(), ResourceError> {
        let session_id = session.session_id.clone();
        let opened = lock(&self.resources).open_session(
            &session_id,
            session.policy.clone(),
            session.tools.clone(),
        );
        if let Err(error) = opened {
            sessions.remove(&session_id);
            return Err(error);
        }
        if let Some(backend) = &self.resource_backend
            && let Err(error) = backend.open_session(session)
        {
            lock(&self.resources).close_session(&session_id);
            sessions.remove(&session_id);
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn refresh_workspace_runtime(
        &self,
        attachment: &RuntimeAttachment,
        review_override: Option<Arc<ReviewManager>>,
    ) -> Result<(), ServerError> {
        let mut sessions = self.lock_sessions();
        let session = sessions
            .get_mut(&attachment.id)
            .ok_or_else(|| ServerError::SessionNotFound(attachment.id.clone()))?;
        let previous = session.clone();
        session.persisted = Some(attachment.hydrated.clone());
        session.intent.agent = attachment.agent.clone();
        recompose_agent_profile_settings(
            &mut session.intent,
            &attachment.hydrated,
            attachment.agent_profile.as_ref(),
        );
        session.agent_summary = attachment
            .agent_profile
            .as_ref()
            .map(crate::workspace::agent_summary);
        if review_override.is_some() {
            session.review = review_override;
        }
        session.updated_at = now_millis();
        drop(sessions);
        if let Err(error) = self.refresh_session_workspace_tools(&attachment.id) {
            self.lock_sessions().insert(previous);
            if let Err(rollback) = self.refresh_session_workspace_tools(&attachment.id) {
                return Err(ServerError::Resource(format!(
                    "{error}; runtime rollback failed ({rollback})"
                )));
            }
            return Err(error);
        }
        Ok(())
    }

    /// Moves a session to `agent_name` while its turn runs, the way
    /// `session/agent/update` does between turns: the profile is saved with the
    /// session and the runtime rebuilt around it. Answers the profile, the intent and the
    /// tools the turn's next request is to run with. Reference
    /// `AgentLoop.switch_agent`, which `exit_plan_mode` calls mid-turn.
    pub(crate) fn switch_agent_in_turn(
        &self,
        session_id: &str,
        agent_name: &str,
    ) -> Result<(AgentProfile, SessionIntent, ToolRegistry), ServerError> {
        let scoped = {
            let sessions = self.lock_sessions();
            let session = sessions
                .get(session_id)
                .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
            self.agent_workspace(session)
        };
        let params = BTreeMap::from([
            ("sessionId".to_owned(), json!(session_id)),
            ("agentName".to_owned(), json!(agent_name)),
        ]);
        let attachment = scoped
            .dispatch("session/agent/update", &params)
            .map_err(|error| ServerError::Resource(error.to_string()))?
            .attachment
            .ok_or_else(|| ServerError::Resource("the agent switch named no session".to_owned()))?;
        let profile = attachment
            .agent_profile
            .clone()
            .ok_or_else(|| ServerError::Resource("the agent switch named no profile".to_owned()))?;
        self.refresh_workspace_runtime(&attachment, None)?;
        let sessions = self.lock_sessions();
        let session = sessions
            .get(session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        Ok((profile, session.intent.clone(), session.tools.clone()))
    }

    /// Reference `AgentLoop._bind_workspace` after a relocation: the
    /// destination's checkout is trusted for the session, its project
    /// configuration resolves the tool filters again, and the tools are
    /// registered against the new working directory. The grant an earlier
    /// move took is given back.
    pub(crate) fn rebind_session_workspace(&self, session_id: &str) -> Result<(), ServerError> {
        let (working_directory, persisted, released) = {
            let sessions = self.lock_sessions();
            let session = sessions
                .get(session_id)
                .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
            (
                session.working_directory.clone(),
                session.persisted.clone(),
                session.trust_taken_by_move.clone(),
            )
        };
        let target = Path::new(&working_directory);
        let checkout = vibe_core::worktree::WorktreeRepository::open(
            target,
            &self.workspace.managed_worktrees(),
        )
        .and_then(|repository| repository.worktree_root())
        .unwrap_or_else(|_| target.to_path_buf());
        let (trusted, project_file_trust) = self.session_trust(&checkout, true);
        if let Some(released) = released {
            vibe_core::trust::TrustStore::for_vibe_home(self.workspace.vibe_home())
                .revoke_session_trust(&released);
        }
        let (enabled_tools, mut disabled_tools) = self
            .workspace
            .tool_filters_for_session(target, trusted)
            .map_err(|error| ServerError::Resource(error.to_string()))?;
        {
            let mut sessions = self.lock_sessions();
            let session = sessions
                .get_mut(session_id)
                .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
            session.trust_taken_by_move = Some(checkout);
            let intent = &mut session.intent;
            intent.trusted = trusted;
            intent.project_file_trust = project_file_trust;
            intent.requested_enabled_tools =
                intent.client_enabled_tools.clone().unwrap_or(enabled_tools);
            disabled_tools.extend(intent.client_disabled_tools.iter().cloned());
            disabled_tools.sort();
            disabled_tools.dedup();
            intent.requested_disabled_tools = disabled_tools;
            if persisted.is_none() {
                intent
                    .enabled_tools
                    .clone_from(&intent.requested_enabled_tools);
                intent
                    .disabled_tools
                    .clone_from(&intent.requested_disabled_tools);
            }
        }
        match persisted {
            Some(hydrated) => self
                .refresh_workspace_runtime(&crate::workspace::runtime_attachment(&hydrated), None),
            None => self.refresh_session_workspace_tools(session_id),
        }
    }

    /// Reads the session's hook files again. Reference
    /// `reload_with_initial_messages(reload_hooks=True)`.
    pub(crate) fn reload_session_hooks(&self, session_id: &str) -> Result<(), ServerError> {
        let (working_directory, trusted, add_directories) = {
            let sessions = self.lock_sessions();
            let session = sessions
                .get(session_id)
                .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
            (
                session.working_directory.clone(),
                session.intent.trusted,
                session.intent.add_directories.clone(),
            )
        };
        let hooks =
            self.workspace
                .session_hooks(Path::new(&working_directory), trusted, &add_directories);
        if let Some(session) = self.lock_sessions().get_mut(session_id) {
            session.hooks = hooks;
            // The same reload reads the prompt's files again.
            session.system_prompt = None;
        }
        Ok(())
    }

    pub(super) fn refresh_session_workspace_tools(
        &self,
        session_id: &str,
    ) -> Result<(), ServerError> {
        let (working_directory, policy, tools, intent, previous_review) = {
            let sessions = self.lock_sessions();
            let session = sessions
                .get(session_id)
                .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
            self.rekey_session_plugins(session);
            (
                session.working_directory.clone(),
                session.policy.clone(),
                session.tools.clone(),
                session.intent.clone(),
                session.review.clone(),
            )
        };
        policy
            .try_replace_rules_with_rationale_prefix(
                "agent-profile:",
                intent.agent_permission_rules.clone(),
            )
            .map_err(|error| ServerError::Resource(error.to_string()))?;
        let review = self
            .register_workspace_tools(
                session_id,
                &working_directory,
                &policy,
                &tools,
                &intent,
                previous_review.clone(),
            )?
            .or(previous_review);
        if let Some(session) = self.lock_sessions().get_mut(session_id) {
            session.review = review;
        }
        Ok(())
    }
}
