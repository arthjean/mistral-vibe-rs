//! Reopening a saved session as the root: `session/resume` and
//! `session/continue`.

use super::*;

impl ServerConnection {
    /// Reference `session/resume` and `session/continue`: the saved session
    /// is reopened as the root, and answers with its state before its runtime
    /// is published.
    pub(super) fn reopen_session(
        &mut self,
        request: ServerRequest,
        reopen: Reopen,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let ReopenParams {
            session_id,
            agent_config,
            history_limit,
        } = wire_validation::validate_params(&request.method, &request.params).map_err(rejected)?;
        let config_cwd = ["cwd", "workdir"]
            .into_iter()
            .find_map(|key| agent_config.get(key).and_then(Value::as_str))
            .map(ToOwned::to_owned);
        let target = match reopen {
            // Resuming the root reattaches it as it stands, saved or not
            // (`_session_resume`).
            Reopen::Resume if session_id.is_some() && self.root_id() == session_id => {
                session_id.unwrap_or_default()
            }
            Reopen::Resume => {
                let selector = session_id.unwrap_or_default();
                self.load_saved(&selector)?.metadata.id
            }
            Reopen::Continue => {
                if self.root_key().is_some() {
                    return Err(ProtocolFault::plain(
                        ProtocolErrorCode::Conflict,
                        "A session is already attached",
                    ));
                }
                // Reference `legacy_open_target`: the session a listing of
                // `cwd` offers to continue, else the latest one saved there.
                let listed = self.resume_sessions(config_cwd.as_deref())?;
                match self.continue_session_id(&listed) {
                    Some(session_id) => session_id,
                    None => self.continue_target(config_cwd.as_deref())?,
                }
            }
        };
        if self.root_id().as_deref() == Some(target.as_str()) {
            let state = self.live_state(&target)?.unwrap_or(Value::Null);
            let event_id = state["eventId"].clone();
            let mut batch = success_batch(
                request.id,
                result_map([("state", state), ("lastEventId", event_id)]),
            );
            batch.outbound.extend(self.runtime_updated_frame(&target));
            return Ok(batch);
        }
        let lease = self.server.acquire_lease(&target)?;
        let mut params = reopen_start_params(&agent_config);
        params.insert("resume".to_owned(), json!(target));
        params.insert("historyLimit".to_owned(), json!(history_limit));
        if !params.contains_key("agent") {
            // Reference `_AgentLoopBlueprint.agent_name`: a reopened session
            // runs under the agent the launch names, else the configured
            // default, whatever it was saved under.
            let agent = self.server.workspace.default_agent_name()?;
            params.insert("agent".to_owned(), json!(agent));
        }
        let start = ServerRequest { params, ..request };
        let opened = self.open_reopened(start, true)?;
        lease.keep();
        Ok(opened)
    }

    /// Opens the session a reopening resolved, through the start path, and
    /// answers with the reference's reopening shape.
    ///
    /// A reopening publishes the runtime it attached; a start does not
    /// (`runtime_updated` in `_dispatch_backend_host_lifecycle_locked`).
    pub(in crate::server::connection) fn open_reopened(
        &mut self,
        request: ServerRequest,
        publish_runtime: bool,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let id = request.id.clone();
        let mut params = from_params::<SessionStartParams>(&request.params)?;
        let (session_id, state, mcp_configs) = self.start_in_worktree(&mut params)?;
        let event_id = state["eventId"].clone();
        let mut batch = success_batch(
            id,
            result_map([("state", state), ("lastEventId", event_id)]),
        );
        if publish_runtime {
            batch
                .outbound
                .extend(self.runtime_updated_frame(&session_id));
        }
        if !mcp_configs.is_empty() {
            batch.deferred.push(DeferredWork::ConfigureMcp {
                session_id,
                configs: mcp_configs,
            });
        }
        Ok(batch)
    }

    /// Reference `_find_session_to_continue`.
    fn continue_target(&self, cwd: Option<&str>) -> Result<String, ProtocolFault> {
        let logging = self.server.workspace.session_logging();
        if !logging.enabled {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::NotFound,
                "Session logging is disabled. Enable it in the configuration to continue or \
                 resume a session",
            ));
        }
        let cwd = cwd.map_or_else(
            || self.server.workspace.working_directory().to_path_buf(),
            PathBuf::from,
        );
        let cwd = vibe_core::platform::canonicalize(&cwd).unwrap_or(cwd);
        let cwd = cwd.to_string_lossy().into_owned();
        match self.store().continue_target(&cwd) {
            Ok(session_id) => Ok(session_id),
            Err(StorageError::NoSessions) => {
                let mut message = format!(
                    "No previous sessions found in {} for cwd={cwd}",
                    logging.save_dir.display()
                );
                let managed =
                    vibe_core::worktree::managed_worktrees_root(self.server.workspace.vibe_home());
                if Path::new(&cwd)
                    .starts_with(vibe_core::platform::canonicalize(&managed).unwrap_or(managed))
                {
                    message.push_str(
                        ". This worktree has no session of its own yet: start one here, or \
                         name an existing session to resume",
                    );
                }
                Err(ProtocolFault::plain(ProtocolErrorCode::NotFound, message))
            }
            Err(error) => Err(storage_fault(error)),
        }
    }
}

/// Reference `SessionResumeParams` and `SessionContinueParams`: only a
/// resume names its session.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReopenParams {
    session_id: Option<String>,
    #[serde(default)]
    agent_config: Map<String, Value>,
    #[serde(default = "default_history_limit")]
    history_limit: u16,
}
