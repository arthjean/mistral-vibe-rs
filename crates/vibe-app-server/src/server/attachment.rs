//! What the server does around a session besides answering its methods: the
//! resources it opens, the tools it registers, and the runtime it publishes.
//!
//! A session is more than its transcript. It has a workspace with tools bound to
//! a policy, a set of integrations the resource service owns, and a projection
//! the client reads. Attaching, refreshing and closing all three is here; the
//! session's own lifecycle is not.

use super::*;

pub(super) fn apply_persisted_session_settings(
    intent: &mut SessionIntent,
    hydrated: &HydratedSession,
) {
    let config = &hydrated.metadata.config;
    if let Some(value) = config.get("active_model").and_then(Value::as_str) {
        intent.model = Some(value.to_owned());
    }
    if let Some(value) = config.get("maxTurns").and_then(Value::as_u64)
        && let Ok(value) = u32::try_from(value)
    {
        intent.max_turns = Some(value);
    }
    if let Some(value) = config.get("maxTokens").and_then(Value::as_u64) {
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
    pub fn live_projection_seed(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<ProjectionSnapshot, ServerError> {
        let sessions = self.lock_sessions()?;
        let session = sessions
            .get(session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        if session.active_turn.as_deref() != Some(turn_id) {
            return Err(ServerError::StaleTurn(turn_id.to_owned()));
        }
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
        let mut sessions = self.lock_sessions()?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
        if session.active_turn.as_deref() != Some(turn_id) {
            return Err(ServerError::StaleTurn(turn_id.to_owned()));
        }
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

    pub async fn invoke_tool(
        &self,
        session_id: &str,
        name: &str,
        invocation: ToolInvocation,
    ) -> Result<ToolExecutionOutput, ServerError> {
        self.tool_registry(session_id)?
            .invoke(name, invocation)
            .await
            .map_err(|error| ServerError::Tool(error.to_string()))
    }

    pub async fn execute_resource_request(
        &self,
        request_id: RequestId,
        session_id: String,
        command: ResourceBackendCommand,
    ) -> DispatchBatch {
        let Some(backend) = &self.resource_backend else {
            return error_batch(
                request_id,
                ProtocolErrorCode::Conflict,
                "Operational resource backend is not attached",
            );
        };
        let request = ResourceBackendRequest {
            session_id,
            command,
        };
        let session_id = request.session_id.clone();
        let method = request.command.method();
        resource_result_batch(
            request_id,
            self,
            &session_id,
            method,
            backend.dispatch(request).await,
        )
    }

    pub async fn execute_cloud_request(
        &self,
        request_id: RequestId,
        method: String,
        params: BTreeMap<String, Value>,
    ) -> DispatchBatch {
        match self.projects.dispatch_deferred(&method, &params).await {
            Ok(dispatch) => projects_dispatch_batch(request_id, dispatch),
            Err(error) => projects_error_batch(request_id, error),
        }
    }

    pub async fn close_resource_session(
        &self,
        session_id: &str,
        generation: u64,
    ) -> Result<(), ServerError> {
        // A managed shell session outlives the call that started it, so this is
        // the only place left that can stop one. The same holds for a terminal
        // the client opened on our behalf. A failure in either must not skip the
        // backend teardown, so all three run and the first failure is reported.
        let shell = self
            .shell_tools
            .close_session(session_id)
            .await
            .map_err(|error| ServerError::Resource(error.to_string()));
        let delegated = self
            .client_tools
            .close_session(session_id)
            .await
            .map_err(|error| ServerError::Resource(error.to_string()));
        let backend = match &self.resource_backend {
            Some(backend) => backend
                .close_session(session_id, generation)
                .await
                .map_err(|error| ServerError::Resource(error.to_string())),
            None => Ok(()),
        };
        shell.and(delegated).and(backend)
    }

    /// Configures a session's MCP sources and publishes what changed.
    ///
    /// Discovery is best-effort: a source that will not start is reported as a
    /// `warning` rather than failing the session, which is why this answers with
    /// frames instead of a result.
    pub async fn configure_mcp_servers(
        &self,
        session_id: &str,
        configs: Vec<McpServerConfig>,
    ) -> Vec<Vec<u8>> {
        let warning = |message: String| ResourceSignals {
            runtime_updated: false,
            warnings: vec![message],
            auth_url: None,
            integrations: None,
        };
        let mut signals = match &self.resource_backend {
            None => warning("MCP transport backend is not configured".to_owned()),
            Some(backend) => match backend.configure_mcp(session_id, configs).await {
                Ok(dispatch) => dispatch.signals,
                Err(error) => warning(redact(&error.to_string())),
            },
        };
        if let Some(state) = signals.integrations.take()
            && let Ok(mut resources) = self.resources.lock()
        {
            resources.record_integrations(session_id, state);
        }
        signal_frames(self, session_id, &signals)
    }

    /// The session's runtime as `runtime/read` answers and `runtime/updated`
    /// publishes it, or `None` when the resource state cannot be read.
    ///
    /// Three owners contribute: the resource service holds the tool surface and
    /// the integrations, the workspace service holds the configuration and the
    /// catalogs, and the session itself holds its accounting. Composing here is
    /// what makes the answer live rather than a fixed payload, and it is the
    /// same composition the notification publishes.
    pub(crate) fn runtime_snapshot(&self, session_id: &str) -> Option<Value> {
        let mut snapshot = self.resources.lock().ok()?.runtime(session_id).ok()?;
        let (active_agent, stats, context_window) = match self.lock_sessions() {
            Ok(sessions) => {
                let session = sessions.get(session_id);
                (
                    session.and_then(|session| session.intent.agent.clone()),
                    public_stats(session),
                    session.map_or(0, |session| session.context_window),
                )
            }
            Err(_) => (None, public_stats(None), 0),
        };
        let projection = self.workspace.runtime_projection(active_agent.as_deref());
        // Discovery issues and configuration diagnostics are the same fact to a
        // client: a file the session could not read cleanly.
        if let Some(Value::Array(issues)) = snapshot.get_mut("issues") {
            issues.extend(projection.issues);
        }
        snapshot.insert("config".to_owned(), projection.config);
        snapshot.insert("baseConfig".to_owned(), projection.base_config);
        snapshot.insert("activeAgent".to_owned(), projection.active_agent);
        snapshot.insert("agents".to_owned(), Value::Array(projection.agents));
        snapshot.insert("skills".to_owned(), Value::Array(projection.skills));
        snapshot.insert("hooksCount".to_owned(), json!(projection.hooks_count));
        snapshot.insert("stats".to_owned(), stats);
        snapshot.insert("contextWindow".to_owned(), json!(context_window));
        Some(Value::Object(snapshot))
    }

    /// How many images in the session's history the active model cannot read.
    ///
    /// A client shows this after a configuration change so the operator learns
    /// that switching model dropped what the transcript already carries. A model
    /// that reads images strips nothing, so the count is zero without walking
    /// the history.
    pub(crate) fn stripped_history_images(&self, session_id: &str) -> usize {
        if self.workspace.active_model_supports_images() {
            return 0;
        }
        let Ok(sessions) = self.lock_sessions() else {
            return 0;
        };
        sessions
            .get(session_id)
            .and_then(|session| session.snapshot.as_ref())
            .map(|snapshot| {
                snapshot
                    .history
                    .iter()
                    .filter_map(|entry| match entry {
                        PublicHistoryEntry::Message { content, .. } => Some(content),
                        _ => None,
                    })
                    .flatten()
                    .filter(|block| matches!(block, PublicContentBlock::Image { .. }))
                    .count()
            })
            .unwrap_or(0)
    }

    /// Publishes the warning a full checkpoint log raised, once.
    ///
    /// A log with no room left refuses to capture rather than dropping what it
    /// already holds, and an operator whose file history quietly stopped being
    /// tracked has no way to notice. `diagnostics/list` is where a client reads
    /// what the session could not do, so that is where this lands. A lock this
    /// cannot take loses the warning rather than failing the turn that carried
    /// it.
    pub(crate) fn publish_retention_notice(&self, review: &ReviewManager) {
        let Ok(Some(notice)) = review.take_retention_notice() else {
            return;
        };
        if let Ok(mut resources) = self.resources.lock() {
            resources.record_diagnostic(FILE_HISTORY_LABEL, &notice);
        }
    }

    /// The session's logging state as `SessionLogSummary` declares it.
    ///
    /// A session the store never persisted reports its configured switch and
    /// nothing else, which is what a client renders as "not being written".
    pub(crate) fn session_log_summary(&self, session_id: &str) -> Value {
        let enabled = self.workspace.session_logging_enabled();
        let Ok(sessions) = self.lock_sessions() else {
            return json!({
                "enabled": enabled,
                "sessionId": null,
                "persisted": false,
                "path": null,
                "title": null,
                "needsInitialAutoTitle": false,
            });
        };
        let session = sessions.get(session_id);
        let persisted = session.and_then(|session| session.persisted.as_ref());
        let title = session
            .and_then(|session| {
                session
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.title.clone())
            })
            .or_else(|| persisted.and_then(|hydrated| hydrated.metadata.title.clone()));
        json!({
            "enabled": enabled,
            "sessionId": persisted.map(|hydrated| hydrated.metadata.id.clone()),
            "persisted": persisted.is_some(),
            "path": persisted
                .map(|hydrated| hydrated.metadata.directory.clone())
                .filter(|directory| !directory.is_empty()),
            "title": title,
            // A persisted session whose title is still the one the store
            // generated is waiting for its first real one.
            "needsInitialAutoTitle": persisted.is_some_and(|hydrated| {
                hydrated.metadata.title.is_none() && hydrated.metadata.title_source == "auto"
            }),
        })
    }

    pub(crate) fn orphaned_resource_generation(
        &self,
        session_id: &str,
    ) -> Result<Option<u64>, ServerError> {
        let sessions = self.lock_sessions()?;
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
        let mut sessions = self.lock_sessions()?;
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
        let (enabled_tools, disabled_tools) = self
            .workspace
            .tool_filters_for_session(
                Path::new(&attachment.working_directory),
                matches!(
                    policy.try_trust_decision(&attachment.working_directory),
                    Ok(Some(TrustDecision::Trusted | TrustDecision::SessionTrusted))
                ),
            )
            .unwrap_or_default();
        let mut intent = SessionIntent {
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
        session.compaction = self.workspace.compaction_settings();
        sessions.insert(session);
        self.open_session_resources(
            &mut sessions,
            ResourceSession {
                session_id: attachment.id.clone(),
                generation: 1,
                working_directory: attachment.working_directory.clone(),
                project_trusted: matches!(
                    policy.try_trust_decision(&attachment.working_directory),
                    Ok(Some(TrustDecision::Trusted | TrustDecision::SessionTrusted))
                ),
                policy,
                tools,
            },
        )
        .map_err(|error| ServerError::Resource(error.to_string()))
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
        self.builtin_tools
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
        let opened = self
            .resources
            .lock()
            .map_err(|_| ResourceError::Unavailable("resource state lock is poisoned".to_owned()))
            .and_then(|mut resources| {
                resources.open_session(&session_id, session.policy.clone(), session.tools.clone())
            });
        if let Err(error) = opened {
            sessions.remove(&session_id);
            return Err(error);
        }
        if let Some(backend) = &self.resource_backend
            && let Err(error) = backend.open_session(session)
        {
            if let Ok(mut resources) = self.resources.lock() {
                resources.close_session(&session_id);
            }
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
        let mut sessions = self.lock_sessions()?;
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
            self.lock_sessions()?.insert(previous);
            if let Err(rollback) = self.refresh_session_workspace_tools(&attachment.id) {
                return Err(ServerError::Resource(format!(
                    "{error}; runtime rollback failed ({rollback})"
                )));
            }
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn refresh_session_workspace_tools(
        &self,
        session_id: &str,
    ) -> Result<(), ServerError> {
        let (working_directory, policy, tools, intent, previous_review) = {
            let sessions = self.lock_sessions()?;
            let session = sessions
                .get(session_id)
                .ok_or_else(|| ServerError::SessionNotFound(session_id.to_owned()))?;
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
        if let Some(session) = self.lock_sessions()?.get_mut(session_id) {
            session.review = review;
        }
        Ok(())
    }
}
