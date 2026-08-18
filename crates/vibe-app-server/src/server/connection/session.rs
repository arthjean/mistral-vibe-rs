//! The session methods a connection answers: opening one, reading it, writing
//! its settings, compacting it and closing it.
//!
//! Opening is the largest of them, and it runs in three phases, each of which
//! can refuse: what the client asked for and the transcript it attaches to, the
//! policy and tools the session runs under, and the registration that makes it
//! addressable.

use super::*;

impl ServerConnection {
    /// Reports what the session's tool surface could not honor: a filter entry
    /// that does not compile, and a registered tool whose runtime prerequisite
    /// does not hold.
    ///
    /// Both are published on `diagnostics/list` rather than failing the session:
    /// the reference drops an uncompilable pattern and withholds an unavailable
    /// tool, and neither is a reason to refuse to start.
    pub(super) fn record_tool_surface_diagnostics(
        &self,
        intent: &SessionIntent,
        tools: &ToolRegistry,
    ) {
        let mut issues = Vec::new();
        for entry in NameFilter::new(&intent.enabled_tools).invalid() {
            issues.push(format!(
                "enabled_tools entry `{entry}` is not a valid regular expression and is ignored"
            ));
        }
        for entry in NameFilter::new(&intent.disabled_tools).invalid() {
            issues.push(format!(
                "disabled_tools entry `{entry}` is not a valid regular expression and is ignored"
            ));
        }
        let withheld = tools.withheld().unwrap_or_default();
        for name in withheld {
            issues.push(format!(
                "tool `{name}` is withheld: its runtime prerequisite is missing"
            ));
        }
        if issues.is_empty() {
            return;
        }
        if let Ok(mut resources) = self.server.resources.lock() {
            for issue in issues {
                resources.record_diagnostic(CONFIG_FILE_LABEL, &issue);
            }
        }
    }

    /// Reports the skill files discovery could not load.
    ///
    /// Reference `project_diagnostics` reads `skill_manager.config_issues` into
    /// the `diagnostics/list` response, so a typo in frontmatter is a message
    /// naming the file rather than a skill that silently disappeared.
    pub(super) fn record_skill_diagnostics(&self) {
        let issues = self.server.release3.skill_issues();
        if issues.is_empty() {
            return;
        }
        if let Ok(mut resources) = self.server.resources.lock() {
            for (file, message) in issues {
                resources.record_diagnostic_once(&file, &format!("Failed to load: {message}"));
            }
        }
    }

    pub(super) fn session_start(&mut self, request: ServerRequest) -> DispatchBatch {
        let id = request.id.clone();
        answered(id, self.start_session(request))
    }

    /// Opens a session and answers with the state a client reads it through.
    ///
    /// The work runs in three phases, each of which can refuse: what the client
    /// asked for and the transcript it attaches to, the policy and tools the
    /// session runs under, and the registration that makes it addressable.
    fn start_session(&mut self, request: ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let params = from_params::<SessionStartParams>(&request.params)?;
        let opening = self.open_session(&params)?;
        let mcp_configs = self.server.release3.mcp_servers_for_session(
            Path::new(&opening.working_directory),
            params.trusted,
            &params.mcp_servers,
        )?;
        let SessionOpening {
            session_id,
            working_directory,
            created_at,
            updated_at,
            aliases,
            snapshot,
            mut persisted,
            agent_profile,
            should_persist_agent,
            intent,
        } = opening;
        let permission_store = PermissionStore::default()
            .with_tool_config(self.server.release3.tool_config())
            .with_allowlist_persistence(self.server.release3.allowlist_persistence());
        let tools = ToolRegistry::default();
        permission_store
            .try_replace_rules_with_rationale_prefix(
                "agent-profile:",
                intent.agent_permission_rules.clone(),
            )
            .map_err(|error| ProtocolFault::internal(error.to_string()))?;
        if intent.trusted && Workspace::open(&working_directory).is_ok() {
            permission_store
                .try_set_trust(
                    &working_directory,
                    TrustDecision::SessionTrusted,
                    TrustRootKind::Workspace,
                )
                .map_err(|error| ProtocolFault::invalid_params(error.to_string()))?;
        }
        let review = self.server.register_workspace_tools(
            &session_id,
            &working_directory,
            &permission_store,
            &tools,
            &intent,
            None,
        )?;
        self.server
            .session_tool_factory
            .register(&session_id, &tools)
            .map_err(ProtocolFault::internal)?;
        self.record_tool_surface_diagnostics(&intent, &tools);
        self.record_skill_diagnostics();
        if persisted.is_none() && self.server.release3.persists_runtime_sessions() {
            persisted = Some(self.server.release3.create_runtime_session(
                &session_id,
                &working_directory,
                created_at,
            )?);
        }
        if should_persist_agent
            && let Some(hydrated) = self
                .server
                .release3
                .update_runtime_agent(&session_id, &agent_profile.name)?
        {
            persisted = Some(hydrated);
        }
        let mut sessions = self.server.lock_sessions()?;
        if sessions.contains(&session_id) {
            return Err(ProtocolFault::new(
                ProtocolErrorCode::Conflict,
                "Session already exists",
            ));
        }
        let project_trusted = intent.trusted;
        let mut session = SessionRuntime::new(
            session_id.clone(),
            working_directory.clone(),
            intent,
            permission_store.clone(),
            tools.clone(),
            review,
            created_at,
        );
        session.updated_at = updated_at;
        session.snapshot = snapshot;
        session.aliases = aliases;
        session.persisted = persisted;
        session.agent_summary = Some(crate::release3::agent_summary(&agent_profile));
        session.context_window = self.server.release3.context_window();
        session.compaction = self.server.release3.compaction_settings();
        sessions.insert(session);
        self.server.open_session_resources(
            &mut sessions,
            ResourceSession {
                session_id: session_id.clone(),
                generation: 1,
                working_directory,
                project_trusted,
                policy: permission_store,
                tools,
            },
        )?;
        self.attached_sessions.insert(session_id.clone());
        let state = sessions
            .get(&session_id)
            .map(public_session_state)
            .unwrap_or(Value::Null);
        drop(sessions);
        let mut batch = success_batch(request.id, result_map([("state", state)]));
        batch.outbound.extend(self.attachment_frames(&session_id));
        if !mcp_configs.is_empty() {
            batch.deferred.push(DeferredWork::ConfigureMcp {
                session_id,
                configs: mcp_configs,
            });
        }
        Ok(batch)
    }

    /// What a `session/start` resolves before anything is registered: the
    /// identity the session takes, the transcript it attaches to, and the
    /// intent it runs under.
    ///
    /// Every value here is decided from the parameters and the attachment
    /// alone, which is what keeps the registration above free of the precedence
    /// rules between the two.
    fn open_session(&self, params: &SessionStartParams) -> Result<SessionOpening, ProtocolFault> {
        if !(1..=500).contains(&params.history_limit) {
            return Err(ProtocolFault::invalid_params(
                "historyLimit must be between 1 and 500",
            ));
        }
        let max_price_micros = match params.max_price {
            Some(price) => price_dollars_to_micros(price)
                .map(|value| params.max_price_micros.or(Some(value)))
                .ok_or_else(|| {
                    ProtocolFault::invalid_params("maxPrice must be a finite non-negative number")
                })?,
            None => params.max_price_micros,
        };
        let requested_working_directory = params
            .working_directory
            .clone()
            .unwrap_or_else(|| ".".to_owned());
        let attachment = self.attach_saved_session(params, &requested_working_directory)?;
        let session_id = attachment
            .as_ref()
            .map(|attachment| attachment.id.clone())
            .or_else(|| params.session_id.clone())
            .unwrap_or_else(|| {
                generated_session_id(self.server.next_session.fetch_add(1, Ordering::Relaxed))
            });
        let working_directory = attachment
            .as_ref()
            .map(|attachment| attachment.working_directory.clone())
            .unwrap_or(requested_working_directory);
        let created_at = attachment
            .as_ref()
            .map(|attachment| attachment.hydrated.metadata.created_at_ms)
            .filter(|timestamp| *timestamp != 0)
            .unwrap_or_else(now_millis);
        let aliases = [params.session_id.as_deref(), params.resume.as_deref()]
            .into_iter()
            .flatten()
            .filter(|alias| *alias != session_id)
            .map(ToOwned::to_owned)
            .collect();
        let attachment_agent = attachment
            .as_ref()
            .and_then(|attachment| attachment.agent.clone());
        let selected_agent = match params.agent.clone().or(attachment_agent) {
            Some(agent) => agent,
            None => self.server.release3.default_agent_name()?,
        };
        // A client that named an agent overrides what the transcript recorded.
        // Otherwise the profile the session was saved under wins over a fresh
        // lookup, so a resumed session keeps the profile it ran with.
        let saved_profile = params
            .agent
            .is_none()
            .then(|| {
                attachment
                    .as_ref()
                    .and_then(|attachment| attachment.agent_profile.clone())
            })
            .flatten();
        let agent_profile = match saved_profile {
            Some(profile) => profile,
            None => self.server.release3.agent_profile(&selected_agent)?,
        };
        let (config_enabled_tools, config_disabled_tools) = self
            .server
            .release3
            .tool_filters_for_session(Path::new(&working_directory), params.trusted)?;
        // Reference `_session_config_overrides`: an `enabled_tools` the client
        // sent replaces the configured allowlist, while `disabled_tools`
        // concatenates onto it.
        let mut disabled_tools = config_disabled_tools;
        disabled_tools.extend(params.disabled_tools.clone());
        disabled_tools.sort();
        disabled_tools.dedup();
        let mut intent = SessionIntent {
            add_directories: params.add_directories.clone(),
            trusted: params.trusted,
            agent: Some(selected_agent),
            tool_filters: params.tool_filters.clone(),
            enabled_tools: params.enabled_tools.clone().unwrap_or(config_enabled_tools),
            disabled_tools,
            requested_enabled_tools: Vec::new(),
            requested_disabled_tools: Vec::new(),
            agent_permission_rules: Vec::new(),
            mcp_servers: params.mcp_servers.clone(),
            model: params.model.clone(),
            max_turns: params.max_turns,
            max_tokens: params.max_tokens,
            max_price_micros,
            mode: params.mode.clone(),
            thinking: params.thinking,
            reasoning_effort: params.reasoning_effort.clone(),
            auto_approve: params.auto_approve,
            requested_auto_approve: params.auto_approve,
            approval: AgentApproval::Prompt,
            system_prompt_id: None,
            resume: attachment
                .as_ref()
                .map(|attachment| attachment.id.clone())
                .or_else(|| params.resume.clone()),
            continue_session: params.continue_session && attachment.is_none(),
        };
        intent
            .requested_enabled_tools
            .clone_from(&intent.enabled_tools);
        intent
            .requested_disabled_tools
            .clone_from(&intent.disabled_tools);
        apply_agent_profile_settings(&mut intent, &agent_profile);
        Ok(SessionOpening {
            should_persist_agent: attachment.is_none() || params.agent.is_some(),
            updated_at: attachment
                .as_ref()
                .map(|attachment| attachment.hydrated.metadata.updated_at_ms)
                .filter(|timestamp| *timestamp != 0)
                .unwrap_or(created_at),
            snapshot: attachment
                .as_ref()
                .map(|attachment| persisted_projection(&attachment.hydrated, params.history_limit)),
            persisted: attachment.map(|attachment| attachment.hydrated),
            session_id,
            working_directory,
            created_at,
            aliases,
            agent_profile,
            intent,
        })
    }

    /// The saved session a `resume` selector or a `continue` names, if either
    /// does.
    fn attach_saved_session(
        &self,
        params: &SessionStartParams,
        requested_working_directory: &str,
    ) -> Result<Option<RuntimeAttachment>, ProtocolFault> {
        let (method, selector) = if let Some(selector) = params.resume.as_deref() {
            ("session/resume", ("sessionId", json!(selector)))
        } else if params.continue_session {
            (
                "session/continue",
                ("cwd", json!(requested_working_directory)),
            )
        } else {
            return Ok(None);
        };
        Ok(self
            .server
            .release3
            .dispatch(
                method,
                &result_map([selector, ("systemPrompt", json!("")), ("config", json!({}))]),
            )?
            .attachment)
    }

    pub(super) fn session_read(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = match from_params::<SessionParams>(&request.params) {
            Ok(params) => params,
            Err(rejection) => {
                return invalid_params_batch(request.id, rejection);
            }
        };
        if let Some(batch) = self.attachment_error(request.id.clone(), &params.session_id) {
            return batch;
        }
        match self.server.session(&params.session_id) {
            Ok(_) => match self.server.lock_sessions() {
                Ok(sessions) => match sessions.get(&params.session_id) {
                    Some(session) => success_batch(
                        request.id,
                        result_map([("state", public_session_state(session))]),
                    ),
                    None => {
                        error_batch(request.id, ProtocolErrorCode::NotFound, "Session not found")
                    }
                },
                Err(error) => internal_error_batch(request.id, &error),
            },
            Err(ServerError::SessionNotFound(_)) => {
                error_batch(request.id, ProtocolErrorCode::NotFound, "Session not found")
            }
            Err(error) => internal_error_batch(request.id, &error),
        }
    }

    pub(super) fn session_close(&mut self, request: ServerRequest) -> DispatchBatch {
        let id = request.id.clone();
        answered(id, self.close_session(request))
    }

    fn close_session(&mut self, request: ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let params = from_params::<SessionParams>(&request.params)?;
        if let Some(batch) = self.attachment_error(request.id.clone(), &params.session_id) {
            return Ok(batch);
        }
        let mut sessions = self.server.lock_sessions()?;
        let key = sessions
            .key(&params.session_id)
            .map(ToOwned::to_owned)
            .ok_or_else(|| session_missing("Session not found"))?;
        let session = sessions
            .get_mut(&key)
            .ok_or_else(|| session_missing("Session not found"))?;
        if session.compaction_pending {
            return Err(ProtocolFault::new(
                ProtocolErrorCode::Conflict,
                "Cannot close while compaction is active",
            ));
        }
        let canonical_session_id = session.id.clone();
        self.server
            .release3
            .close_saved_session(&canonical_session_id, now_millis())?;
        self.server
            .release4
            .close_transient_session(&canonical_session_id)
            .map_err(|error| ProtocolFault::from(ServerError::Release4(error.to_string())))?;
        let active_turn = session.active_turn.clone();
        session.status = SessionStatus::Closed;
        session.updated_at = now_millis();
        cancel_pending_callback(session, "Session was closed");
        let closed_status = session_updated_frame(session);
        if self.attached_sessions.remove(&key) {
            session.attachments = session.attachments.saturating_sub(1);
        }
        let session_id = canonical_session_id;
        let resource_generation = session.resource_generation;
        drop(sessions);
        if let Ok(mut resources) = self.server.resources.lock() {
            resources.close_session(&session_id);
        }
        // The scratchpad is a capability of the runtime, not of the workspace,
        // so it goes with the session that opened it. Reference
        // `cleanup_scratchpad` on the agent-loop shutdown path.
        cleanup_scratchpad(scratchpad_path(&session_id).as_path().into());
        self.state = ConnectionState::Closed;
        let mut deferred = active_turn
            .map(|turn_id| DeferredWork::InterruptTurn {
                session_id: session_id.clone(),
                turn_id,
            })
            .into_iter()
            .collect::<Vec<_>>();
        if self.server.resource_backend.is_some() {
            deferred.push(DeferredWork::CloseResources {
                session_id: session_id.clone(),
                generation: resource_generation,
            });
        }
        Ok(DispatchBatch {
            outbound: vec![success_bytes(request.id, BTreeMap::new()), closed_status],
            deferred,
            close_after_flush: true,
        })
    }

    pub(super) fn session_settings_update(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = match from_params::<SessionSettingsUpdateParams>(&request.params) {
            Ok(params) => params,
            Err(rejection) => {
                return invalid_params_batch(request.id, rejection);
            }
        };
        self.write_session_settings(request.id, params.into())
    }

    /// The local extension that writes what the reference settings method does
    /// not declare.
    pub(super) fn session_overrides_write(&mut self, request: ServerRequest) -> DispatchBatch {
        let params = match from_params::<SessionOverridesWriteParams>(&request.params) {
            Ok(params) => params,
            Err(rejection) => {
                return invalid_params_batch(request.id, rejection);
            }
        };
        self.write_session_settings(request.id, params.into())
    }

    pub(super) fn write_session_settings(
        &mut self,
        request_id: RequestId,
        params: SessionSettings,
    ) -> DispatchBatch {
        let id = request_id.clone();
        answered(id, self.apply_session_settings(request_id, params))
    }

    fn apply_session_settings(
        &mut self,
        request_id: RequestId,
        params: SessionSettings,
    ) -> Result<DispatchBatch, ProtocolFault> {
        params.validate()?;
        if let Some(batch) = self.attachment_error(request_id.clone(), &params.session_id) {
            return Ok(batch);
        }
        let canonical_session_id = {
            let sessions = self.server.lock_sessions()?;
            let session = sessions
                .get(&params.session_id)
                .ok_or_else(|| session_missing("Session was not found"))?;
            // Approval is the only setting a running turn is already acting on,
            // so it is the only one this refuses to change under one.
            if params.auto_approve.is_some() && session.active_turn.is_some() {
                return Err(ProtocolFault::new(
                    ProtocolErrorCode::Conflict,
                    "autoApprove can only change while the session is idle",
                ));
            }
            session.id.clone()
        };
        let approval_changed = params.auto_approve.is_some();
        let persisted = self
            .server
            .release3
            .update_runtime_settings(&canonical_session_id, &params.entries())?;
        let mut sessions = self.server.lock_sessions()?;
        let session = sessions
            .get_mut(&params.session_id)
            .ok_or_else(|| session_missing("Session was not found"))?;
        params.apply(&mut session.intent);
        if let Some(persisted) = persisted {
            session.persisted = Some(persisted);
        }
        session.updated_at = now_millis();
        drop(sessions);
        // The tool surface is gated on approval, so a change to it has to reach
        // the registry the running session executes through.
        if approval_changed {
            self.server
                .refresh_session_workspace_tools(&canonical_session_id)?;
        }
        Ok(success_batch(request_id, BTreeMap::new()))
    }

    pub(super) fn session_compact_start(&mut self, request: ServerRequest) -> DispatchBatch {
        let id = request.id.clone();
        answered(id, self.start_session_compaction(request))
    }

    fn start_session_compaction(
        &mut self,
        request: ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let params = from_params::<SessionCompactParams>(&request.params)?;
        if let Some(batch) = self.attachment_error(request.id.clone(), &params.session_id) {
            return Ok(batch);
        }
        let canonical_session_id = {
            let mut sessions = self.server.lock_sessions()?;
            let session = sessions
                .get_mut(&params.session_id)
                .ok_or_else(|| session_missing("Session was not found"))?;
            if session.active_turn.is_some() || session.compaction_pending {
                return Err(ProtocolFault::new(
                    ProtocolErrorCode::Conflict,
                    "Cannot compact while the session has active work",
                ));
            }
            if session.status == SessionStatus::Closed {
                return Err(ProtocolFault::new(
                    ProtocolErrorCode::Conflict,
                    "Cannot compact a closed session",
                ));
            }
            session.compaction_pending = true;
            session.updated_at = now_millis();
            session.id.clone()
        };
        Ok(DispatchBatch {
            outbound: Vec::new(),
            deferred: vec![DeferredWork::CompactSession {
                request_id: request.id,
                session_id: canonical_session_id,
                extra_instructions: params.extra_instructions,
            }],
            close_after_flush: false,
        })
    }
}
