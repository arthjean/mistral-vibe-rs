//! Opening a unified session as a connection's root: a new one, a stored
//! one reopened, a fork of one, and the root's history cleared.
//!
//! Reference `start`, `resume`, `continue_latest`, `import_legacy`, `fork` and
//! `clear_history` on the unified host
//! (`vibe/app_server/_unified_harness_backend_adapter.py`). The port's
//! [`SessionRuntime`] registers each one first, through the start path every
//! session takes, and the unified half is built over it.

use super::params::{ForkParams, ResumeParams};
use super::*;

/// A stored session reopened as the root: its state and what opening it
/// publishes.
pub(super) struct Reopened {
    pub(super) state: Value,
    pub(super) frames: Vec<Vec<u8>>,
    pub(super) deferred: Vec<DeferredWork>,
}

impl Reopened {
    /// The answer to the request that reopened the session, with `fields`
    /// beside its state, then what opening it publishes.
    fn into_batch<const N: usize>(
        self,
        request_id: RequestId,
        fields: [(&str, Value); N],
    ) -> DispatchBatch {
        let mut result = result_map(fields);
        result.insert("lastEventId".to_owned(), self.state["eventId"].clone());
        result.insert("state".to_owned(), self.state);
        let mut batch = success_batch(request_id, result);
        batch.outbound.extend(self.frames);
        batch.deferred.extend(self.deferred);
        batch
    }
}

/// The refusal of a continuation with nothing to continue.
fn nothing_to_continue() -> ProtocolFault {
    ProtocolFault::plain(ProtocolErrorCode::NotFound, "No session to continue")
}

/// The accounting a session opens with, priced by its model.
fn opening_stats(session: &SessionRuntime) -> AgentStatsSnapshot {
    let (input, output, cached) = session.pricing;
    AgentStatsSnapshot::priced(input, output, cached)
}

impl AppServer {
    /// Files the session `from` names under `to` for good, as a session with
    /// nothing said yet, with what this process keeps for it, so `from` names
    /// a separate saved session again (reference `clear` on the unified
    /// host). The registry moves first, and a refusal there changes nothing.
    fn rekey_cleared_session(&self, from: &str, to: &str) -> Result<(), ServerError> {
        {
            let mut sessions = self.lock_sessions();
            sessions.rekey(from, to)?;
            if let Some(session) = sessions.get_mut(to) {
                session.restart_fresh(now_millis());
            }
        }
        lock(&self.resources).rekey_session(from, to);
        let mut plugins = lock(&self.session_plugins);
        if let Some(set) = plugins.remove(from) {
            plugins.insert(to.to_owned(), set);
        }
        drop(plugins);
        let mut pluginless = lock(&self.pluginless);
        if pluginless.remove(from) {
            pluginless.insert(to.to_owned());
        }
        Ok(())
    }
}

impl ServerConnection {
    /// Reference `session/start` on the unified host.
    pub(in crate::server) fn unified_session_start(
        &mut self,
        request: ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let id = request.id.clone();
        let mut params = from_params::<SessionStartParams>(&request.params)?;
        if params.resume.is_some() || params.continue_session {
            let start = request.params.clone();
            return self.unified_reopen_start(id, &params, start);
        }
        let history_limit = usize::from(params.history_limit);
        let (session_id, _, mcp_configs) = self.start_in_worktree(&mut params)?;
        let session = self.open_unified(&session_id, None)?;
        let state = session.read_state(Some(history_limit));
        let mut batch = success_batch(
            id,
            result_map([("lastEventId", state["eventId"].clone()), ("state", state)]),
        );
        batch.deferred.push(DeferredWork::AttachUnified {
            session_id: session_id.clone(),
        });
        if !mcp_configs.is_empty() {
            batch.deferred.push(DeferredWork::ConfigureMcp {
                session_id,
                configs: mcp_configs,
            });
        }
        Ok(batch)
    }

    /// Builds the unified half of a registered session, fresh or from its
    /// store.
    fn open_unified(
        &self,
        session_id: &str,
        stored: Option<(SessionStore, SessionCore, Map<String, Value>)>,
    ) -> Result<Arc<UnifiedSession>, ProtocolFault> {
        let (cwd, stats, context_window, created_at, headless) = {
            let sessions = self.server.lock_sessions();
            let session = sessions
                .get(session_id)
                .ok_or_else(|| ProtocolFault::internal("the opened session is gone"))?;
            let cwd = vibe_core::platform::canonicalize(&session.working_directory)
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|_| session.working_directory.clone());
            (
                cwd,
                opening_stats(session),
                session.context_window,
                session.created_at,
                session.intent.headless,
            )
        };
        let root = self.server.unified_session_root();
        let scratchpad = vibe_harness::tools::vibe::scratchpad_dir(
            root.as_deref().unwrap_or_else(|| Path::new(".")),
            session_id,
        );
        let session = match stored {
            Some((store, core, meta)) => {
                let mut stats = stats;
                let unified = UnifiedSession::new(
                    session_id.to_owned(),
                    cwd,
                    core,
                    root,
                    meta,
                    Some(store),
                    AgentStatsSnapshot::priced(0.0, 0.0, None),
                    context_window,
                );
                {
                    let core = unified.core();
                    let mut host = unified.host();
                    let state = wire_state(&core.projector.state, &WireContext::default());
                    stats.seed(&state);
                    host.translator.stats = stats;
                    // Reference `_state_with_child_summaries` on a reopened
                    // root: each child it spawned is summarized from its own
                    // store.
                    host.children.observe(
                        state["history"]
                            .as_array()
                            .map(Vec::as_slice)
                            .unwrap_or_default(),
                    );
                    let stores = unified.session_root.as_deref().map(store::unified_root);
                    for child in host.children.known() {
                        let Some(child_core) = stores
                            .as_ref()
                            .and_then(|stores| SessionStore::open(stores.join(&child)))
                            .and_then(|stored| stored.load(PathBuf::new()))
                        else {
                            continue;
                        };
                        let session = wire_state(
                            &child_core.projector.state,
                            &WireContext {
                                cwd: Some(unified.cwd.clone()),
                                ..WireContext::default()
                            },
                        )["session"]
                            .clone();
                        host.children.sessions.insert(child, session);
                    }
                }
                unified
            }
            None => {
                let state = HarnessState::new(session_id, None, session_id, created_at);
                let core = SessionCore::new(state, scratchpad);
                let meta = store::new_meta(
                    session_id,
                    None,
                    &format_iso_millis(created_at),
                    &cwd,
                    &vibe_core::storage::current_username(),
                );
                UnifiedSession::new(
                    session_id.to_owned(),
                    cwd,
                    core,
                    root,
                    meta,
                    None,
                    stats,
                    context_window,
                )
            }
        };
        // Reference `scheduled_loops_enabled=not headless`.
        if headless {
            session.core().loops.disable();
        }
        *lock(&session.driver) = Some(UnifiedDriver {
            launch: self.launch.clone(),
            delivery: self.callback_delivery(),
        });
        let session = Arc::new(session);
        self.server.insert_unified(Arc::clone(&session));
        Ok(session)
    }

    /// Reference `_require_session_logging`: nothing is reopened from a store
    /// that is not kept.
    fn require_session_logging(&self) -> Result<(), ProtocolFault> {
        if self.server.workspace.session_logging().enabled {
            return Ok(());
        }
        Err(ProtocolFault::plain(
            ProtocolErrorCode::NotFound,
            "Session logging is disabled. Enable it in config to use --continue or --resume",
        ))
    }

    /// Reference `continue_latest` on the unified host: the latest session,
    /// resumed by its identifier.
    pub(in crate::server) fn unified_continue(
        &mut self,
        mut request: ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        self.require_session_logging()?;
        let target = self
            .unified_continue_target()
            .ok_or_else(nothing_to_continue)?;
        request.params.insert("sessionId".to_owned(), json!(target));
        self.unified_resume(request)
    }

    /// A port `session/start` that names a saved session: `resume` reopens it
    /// as reference `resume` does, `continue` the latest one.
    fn unified_reopen_start(
        &mut self,
        id: vibe_protocol::RequestId,
        params: &SessionStartParams,
        mut start: BTreeMap<String, Value>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        self.require_session_logging()?;
        let session_id = match params.resume.as_deref() {
            Some(selector) => selector.to_owned(),
            None => self
                .unified_continue_target()
                .ok_or_else(nothing_to_continue)?,
        };
        let target = self.stored_or_imported(session_id)?;
        for key in ["resume", "continue", "cwd", "workingDirectory", "sessionId"] {
            start.remove(key);
        }
        let reopened = self.reopen_with(&target, usize::from(params.history_limit), start)?;
        Ok(reopened.into_batch(id, []))
    }

    /// Reference `resume` on the unified host.
    pub(in crate::server) fn unified_resume(
        &mut self,
        request: ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let ResumeParams {
            session_id,
            agent_config,
            history_limit,
        } = from_params(&request.params)?;
        if let Some(live) = self.server.unified_session(&session_id)
            && self.root_id().as_deref() == Some(session_id.as_str())
        {
            // Reference resume reattaches a fresh runtime, whose accounting
            // starts over from what the session stored.
            let opening = self
                .server
                .lock_sessions()
                .get(&session_id)
                .map(opening_stats);
            if let Some(mut stats) = opening {
                let core = live.core();
                stats.seed(&wire_state(&core.projector.state, &WireContext::default()));
                live.host().translator.stats = stats;
            }
            let state = live.read_state(Some(history_limit));
            return Ok(success_batch(
                request.id,
                result_map([("lastEventId", state["eventId"].clone()), ("state", state)]),
            ));
        }
        let target = self.stored_or_imported(session_id)?;
        let reopened = self.reopen_unified(&target, history_limit, &agent_config)?;
        Ok(reopened.into_batch(request.id, []))
    }

    /// The stored unified session `session_id` names, or, when only a legacy
    /// transcript does, the session it is imported into.
    fn stored_or_imported(&self, session_id: String) -> Result<String, ProtocolFault> {
        if self.stored_unified(&session_id).is_none()
            && let Some(legacy) = self.legacy_session(&session_id)
        {
            return self.import_legacy(&legacy);
        }
        Ok(session_id)
    }

    /// Reference `import_legacy`: a legacy transcript copied into a new
    /// unified session, which the legacy files outlive untouched. Answers the
    /// new session's identifier.
    fn import_legacy(&self, legacy: &HydratedSession) -> Result<String, ProtocolFault> {
        let root = self.server.unified_session_root().ok_or_else(|| {
            ProtocolFault::internal("sessions are not saved, so none can be imported")
        })?;
        let session_id = vibe_core::session_id::generate_session_id(None);
        let now = now_millis();
        let conversation = legacy
            .messages
            .iter()
            .filter_map(|message| match message {
                vibe_core::events::ModelMessage::User {
                    content, injected, ..
                } => Some(vibe_harness::engine::ConversationMessage::User {
                    content: vec![json!({"type": "text", "text": content})],
                    injected: *injected,
                }),
                vibe_core::events::ModelMessage::Assistant {
                    content,
                    reasoning,
                    tool_calls,
                    ..
                } => Some(vibe_harness::engine::ConversationMessage::Assistant {
                    text: content.clone(),
                    reasoning: reasoning.clone(),
                    tool_calls: tool_calls.clone(),
                }),
                vibe_core::events::ModelMessage::Tool {
                    call_id,
                    name,
                    content,
                    ..
                } => Some(vibe_harness::engine::ConversationMessage::Tool {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    content: content.clone(),
                }),
                vibe_core::events::ModelMessage::System { .. } => None,
            })
            .collect();
        // Reference `_active_model_from_metadata`.
        let model = legacy
            .metadata
            .config
            .get("active_model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .map(ToOwned::to_owned);
        let scratchpad = vibe_harness::tools::vibe::scratchpad_dir(&root, &session_id);
        let core =
            vibe_harness::engine::import_core(&session_id, now, scratchpad, conversation, model);
        let cwd = legacy
            .metadata
            .environment
            .get("working_directory")
            .cloned()
            .flatten()
            .unwrap_or_default();
        let meta = store::new_meta(
            &session_id,
            None,
            &format_iso_millis(now),
            &cwd,
            &vibe_core::storage::current_username(),
        );
        SessionStore::create_imported(store::unified_root(&root).join(&session_id), &meta, &core)
            .map_err(|error| ProtocolFault::internal(format!("the import cannot be saved: {error}")))?;
        Ok(session_id)
    }

    /// Opens the stored unified session `session_id` as this connection's
    /// root.
    pub(super) fn reopen_unified(
        &mut self,
        session_id: &str,
        history_limit: usize,
        agent_config: &Map<String, Value>,
    ) -> Result<Reopened, ProtocolFault> {
        self.reopen_with(
            session_id,
            history_limit,
            crate::server::connection::reopen_params(agent_config),
        )
    }

    /// Opens the stored unified session `session_id` as this connection's
    /// root, with the port-shaped start parameters `params`.
    fn reopen_with(
        &mut self,
        session_id: &str,
        history_limit: usize,
        mut params: BTreeMap<String, Value>,
    ) -> Result<Reopened, ProtocolFault> {
        let stored = self.require_stored(session_id)?;
        let meta = stored.read_meta().unwrap_or_default();
        let root = self.server.unified_session_root();
        let scratchpad = vibe_harness::tools::vibe::scratchpad_dir(
            root.as_deref().unwrap_or_else(|| Path::new(".")),
            session_id,
        );
        let core = stored
            .load(scratchpad)
            .ok_or_else(|| ProtocolFault::internal("the session store cannot be read"))?;
        if let Some(cwd) = meta_cwd(&meta) {
            params.entry("cwd".to_owned()).or_insert(json!(cwd));
        }
        params.insert("sessionId".to_owned(), json!(session_id));
        if core.without_plugins {
            self.server.bind_no_plugins(session_id);
        }
        let mut start = from_params::<SessionStartParams>(&params)?;
        let (opened_id, _, mcp_configs) = self.start_in_worktree(&mut start)?;
        let pins = stored_pins(&core);
        let session = self.open_unified(&opened_id, Some((stored, core, meta)))?;
        session.host().pins = pins;
        let state = session.read_state(Some(history_limit));
        let frames = self.runtime_updated_frame(&opened_id).into_iter().collect();
        let mut deferred = vec![DeferredWork::AttachUnified {
            session_id: opened_id.clone(),
        }];
        if !mcp_configs.is_empty() {
            deferred.push(DeferredWork::ConfigureMcp {
                session_id: opened_id,
                configs: mcp_configs,
            });
        }
        Ok(Reopened {
            state,
            frames,
            deferred,
        })
    }

    /// Reference `fork` on the unified host: the source's conversation is
    /// imported into a new stored session, which becomes the root unless the
    /// client asked for a detached copy.
    pub(in crate::server) fn unified_fork(
        &mut self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let ForkParams {
            source_session_id: source_id,
            entry_id,
            history_limit,
            attach,
        } = from_params(&request.params)?;
        let root = self.server.unified_session_root().ok_or_else(|| {
            ProtocolFault::internal("sessions are not saved, so none can be forked")
        })?;
        let session_id = vibe_core::session_id::generate_session_id(None);
        let now = now_millis();
        let scratchpad = vibe_harness::tools::vibe::scratchpad_dir(&root, &session_id);
        let fork = |source: &vibe_harness::engine::SessionCore| match &entry_id {
            None => Ok(vibe_harness::engine::fork_core(
                source,
                &session_id,
                now,
                scratchpad.clone(),
            )),
            Some(entry_id) => vibe_harness::engine::fork_core_through(
                source,
                &session_id,
                now,
                scratchpad.clone(),
                entry_id,
            )
            .map_err(|message| {
                ProtocolFault::with_data(
                    ProtocolErrorCode::InvalidParams,
                    message,
                    json!({
                        "details": {"entry_id": entry_id},
                        "harnessCode": "fork_entry_not_found",
                    }),
                )
            }),
        };
        let (core, cwd) = if let Some(live) = self.server.unified_session(&source_id) {
            (fork(&live.core())?, live.cwd.clone())
        } else {
            let stored = self.require_stored(&source_id)?;
            let source = stored
                .load(PathBuf::new())
                .ok_or_else(|| ProtocolFault::internal("the session store cannot be read"))?;
            let cwd = stored
                .read_meta()
                .as_ref()
                .and_then(meta_cwd)
                .unwrap_or_default();
            (fork(&source)?, cwd)
        };
        let meta = store::new_meta(
            &session_id,
            Some(&source_id),
            &format_iso_millis(now),
            &cwd,
            &vibe_core::storage::current_username(),
        );
        SessionStore::create(store::unified_root(&root).join(&session_id), &meta, &core).map_err(
            |error| ProtocolFault::internal(format!("the fork cannot be saved: {error}")),
        )?;
        let reopened = if attach {
            self.reopen_unified(&session_id, history_limit, &Map::new())?
        } else {
            Reopened {
                state: self.unified_state(&session_id, Some(history_limit))?,
                frames: Vec::new(),
                deferred: Vec::new(),
            }
        };
        Ok(reopened.into_batch(request.id.clone(), [("sourceSessionId", json!(source_id))]))
    }

    /// Reference `clear_history` on the unified host: the root continues as a
    /// new, unsaved session, and the reply carries the history it leaves
    /// behind closed by a `clear` checkpoint.
    pub(in crate::server) fn unified_clear(
        &mut self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let session_id = from_params::<SessionParams>(&request.params)?.session_id;
        let Some(key) = self.root_key() else {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                crate::server::connection::NO_ROOT,
            ));
        };
        if self.root_id().as_deref() != Some(session_id.as_str()) {
            return Err(unknown_session(&session_id));
        }
        let source = self
            .server
            .unified_session(&session_id)
            .ok_or_else(|| unknown_session(&session_id))?;
        if source.host().running.is_some() {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                "Cannot clear history while a turn is active",
            ));
        }
        let limit = crate::server::projection::PUBLIC_HISTORY_LIMIT;
        let carried = source.read_state(Some(limit))["history"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let new_id = vibe_core::session_id::generate_session_id(None);
        // The new identifier is leased, registered and opened before anything
        // lets go of the old one, so a refusal leaves the root as it stood.
        let lease = self.server.acquire_lease(&new_id)?;
        self.server.rekey_cleared_session(&key, &new_id)?;
        let session = self.open_unified(&new_id, None)?;
        lease.keep();
        self.server.release_lease(&session_id);
        self.server.remove_unified(&session_id);
        self.root = Some(new_id.clone());
        if self.attached_sessions.remove(&key) {
            self.attached_sessions.insert(new_id.clone());
        }
        let mut state = session.read_state(Some(limit));
        let mut history: Vec<Value> = carried
            .into_iter()
            .map(|mut entry| {
                entry["sessionId"] = json!(new_id);
                entry
            })
            .collect();
        history.push(
            serde_json::to_value(crate::server::projection::checkpoint_entry(
                &new_id,
                "clear",
                "New conversation started",
                Value::Null,
            ))
            .unwrap_or(Value::Null),
        );
        let excess = history.len().saturating_sub(limit);
        history.drain(..excess);
        state["history"] = Value::Array(history);
        state["historyBeforeCursor"] = Value::Null;
        Ok(success_batch(
            request.id.clone(),
            result_map([
                ("state", state),
                ("sessionLog", self.server.session_log_summary(&new_id)),
            ]),
        ))
    }
}
