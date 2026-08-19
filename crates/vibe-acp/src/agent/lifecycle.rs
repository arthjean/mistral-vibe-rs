//! Opening, reattaching, forking, and stopping editor sessions.

use std::sync::Arc;

use serde_json::{Value, json};
use vibe_app_server::client::{HeadlessService, SessionOptions, TurnDriver};

use crate::agent::{AcpAgent, MAX_ADDITIONAL_DIRECTORIES};
use crate::history::all_history_entries;
use crate::mcp::project_acp_mcp_servers;
use crate::protocol::{AcpError, AcpForkSession, AcpLoadSession, AcpNewSession, AcpSession};
use crate::session::{
    AcpHarness, ActivePhase, SessionSettings, ensure_matching_cwd, metadata_session_id,
    session_options, validate_session_paths,
};

impl<D> AcpAgent<D>
where
    D: TurnDriver,
{
    pub fn new_session(&self, request: AcpNewSession) -> Result<AcpSession, AcpError> {
        self.require_initialized()?;
        validate_session_paths(
            &request.cwd,
            request.additional_directories.as_deref(),
            MAX_ADDITIONAL_DIRECTORIES,
        )?;
        let additional_directories = request.additional_directories.unwrap_or_default();
        let mcp_servers = project_acp_mcp_servers(&request.mcp_servers)?;
        let session_id = self.lock_state()?.mint_session_id();
        let settings = SessionSettings::default();
        let mut service = self.new_service(&request.cwd, &additional_directories)?;
        let app_session_id = service.start_session(&session_options(
            &request.cwd,
            Some(session_id),
            additional_directories,
            mcp_servers,
            None,
            &settings,
        ))?;
        let harness = Arc::new(self.adopt(service, &app_session_id)?);
        let settings = harness.settings()?;
        self.lock_state()?
            .insert_active(app_session_id.clone(), harness);
        Ok(settings.as_session(app_session_id))
    }

    pub fn load_session(&self, request: AcpLoadSession) -> Result<AcpSession, AcpError> {
        self.require_initialized()?;
        validate_session_paths(
            &request.cwd,
            Some(&request.additional_directories),
            MAX_ADDITIONAL_DIRECTORIES,
        )?;
        let mcp_servers = project_acp_mcp_servers(&request.mcp_servers)?;
        let requested_id = request.session_id.clone();
        self.lock_state()?.begin_load(&requested_id)?;
        let loaded = self.attach_saved_session(&request, mcp_servers);
        let (session_id, harness) = match loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                self.lock_state()?.abort_load(&requested_id);
                return Err(error);
            }
        };
        let settings = harness.settings()?;
        self.lock_state()?
            .finish_load(&requested_id, session_id.clone(), harness)?;
        Ok(settings.as_session(session_id))
    }

    fn attach_saved_session(
        &self,
        request: &AcpLoadSession,
        mcp_servers: Vec<Value>,
    ) -> Result<(String, Arc<AcpHarness<D>>), AcpError> {
        let (session_id, settings) =
            self.with_probe(&request.cwd, &request.additional_directories, |probe| {
                let result = probe
                    .public_call("session/resume", json!({"sessionId": request.session_id}))?;
                let session_id = metadata_session_id(&result)?;
                if session_id != request.session_id {
                    return Err(AcpError::InvalidResponse(format!(
                        "requested session `{}` resumed as `{session_id}`",
                        request.session_id
                    )));
                }
                let persisted_cwd = probe.session(&session_id)?.working_directory;
                ensure_matching_cwd(&request.cwd, &persisted_cwd, "load")?;
                Ok((session_id, SessionSettings::from_workspace_result(&result)))
            })?;

        let service = self.new_service(&request.cwd, &request.additional_directories)?;
        let harness = self.attach_session(
            service,
            &session_id,
            session_options(
                &request.cwd,
                None,
                request.additional_directories.clone(),
                mcp_servers,
                Some(session_id.clone()),
                &settings,
            ),
            "loaded",
        )?;
        Ok((session_id, harness))
    }

    pub fn fork_session(&self, request: AcpForkSession) -> Result<AcpSession, AcpError> {
        self.require_initialized()?;
        validate_session_paths(
            &request.cwd,
            Some(&request.additional_directories),
            MAX_ADDITIONAL_DIRECTORIES,
        )?;
        let mcp_servers = project_acp_mcp_servers(&request.mcp_servers)?;
        // A live session's settings win over what the store recorded, and
        // reading them needs the agent lock rather than the probe.
        let live_settings = self.live_session_settings(&request.session_id)?;
        let (session_id, settings) =
            self.with_probe(&request.cwd, &request.additional_directories, |probe| {
                let source = probe
                    .public_call("session/resume", json!({"sessionId": request.session_id}))?;
                let source_id = metadata_session_id(&source)?;
                let source_cwd = probe.session(&source_id)?.working_directory;
                ensure_matching_cwd(&request.cwd, &source_cwd, "fork")?;
                let keep_messages = self.fork_keep_messages(probe, &source_id, &request)?;
                let settings = live_settings
                    .unwrap_or_else(|| SessionSettings::from_workspace_result(&source));
                let mut fork_params = json!({
                    "sessionId": source_id,
                    "newSessionId": request.new_session_id,
                    "config": settings.as_config(),
                });
                if let Some(keep_messages) = keep_messages {
                    fork_params["keepMessages"] = json!(keep_messages);
                }
                let result = probe.public_call("session/fork", fork_params)?;
                Ok((metadata_session_id(&result)?, settings))
            })?;

        let service = self.new_service(&request.cwd, &request.additional_directories)?;
        let harness = self.attach_session(
            service,
            &session_id,
            session_options(
                &request.cwd,
                None,
                request.additional_directories,
                mcp_servers,
                Some(session_id.clone()),
                &settings,
            ),
            "forked",
        )?;
        let settings = harness.settings()?;
        self.lock_state()?
            .insert_active(session_id.clone(), harness);
        Ok(settings.as_session(session_id))
    }

    fn fork_keep_messages(
        &self,
        probe: &mut HeadlessService<D>,
        source_id: &str,
        request: &AcpForkSession,
    ) -> Result<Option<usize>, AcpError> {
        let Some(message_id) = request.message_id.as_deref() else {
            return Ok(None);
        };
        let history = all_history_entries(probe, source_id)?;
        let anchor = self.resolve_user_message_anchor(&request.session_id, message_id)?;
        crate::history::fork_source_keep_messages(&history, anchor, message_id).map(Some)
    }

    /// Starts a session on `service` and checks the canonical layer honored the
    /// requested identity.
    fn attach_session(
        &self,
        mut service: HeadlessService<D>,
        session_id: &str,
        options: SessionOptions,
        operation: &str,
    ) -> Result<Arc<AcpHarness<D>>, AcpError> {
        let attached_id = service.start_session(&options)?;
        if attached_id != session_id {
            return Err(AcpError::InvalidResponse(format!(
                "{operation} session `{session_id}` was attached as `{attached_id}`"
            )));
        }
        Ok(Arc::new(self.adopt(service, session_id)?))
    }

    fn live_session_settings(&self, session_id: &str) -> Result<Option<SessionSettings>, AcpError> {
        self.lock_state()?
            .active(session_id)
            .map(|harness| harness.settings())
            .transpose()
    }

    pub async fn cancel(&self, session_id: &str) -> Result<(), AcpError> {
        let harness = self.session_harness(session_id)?;
        // Requesting the cancel reports the phase it observed under the same
        // lock that claims work, so an idle session latches nothing and a
        // running one names the turn that still needs interrupting.
        if let ActivePhase::Running(turn_id) = harness.request_cancel()? {
            harness
                .service
                .lock()
                .await
                .interrupt(session_id, &turn_id)?;
        }
        Ok(())
    }

    pub async fn close_session(&self, session_id: &str) -> Result<(), AcpError> {
        self.require_initialized()?;
        let Some(harness) = self.lock_state()?.begin_close(session_id)? else {
            return Ok(());
        };
        match self.stop_owned_session(session_id, &harness).await {
            // A concurrent disconnect finished it first and already recorded
            // the outcome.
            Ok(None) => Ok(()),
            Ok(Some(())) => {
                self.lock_state()?.finish_close(session_id, harness, true);
                Ok(())
            }
            Err(error) => {
                self.lock_state()?.finish_close(session_id, harness, false);
                Err(error)
            }
        }
    }

    pub async fn disconnect(&self) -> Result<(), AcpError> {
        let sessions = self.lock_state()?.take_live_sessions();
        let mut first_error = None;
        for (session_id, harness) in sessions {
            let outcome = self.stop_owned_session(&session_id, &harness).await;
            if let Ok(None) = outcome {
                continue;
            }
            // The session already left the map, so it is tombstoned either
            // way: a failed stop leaves nothing that could serve it again, and
            // a repeated close still has to read as idempotent.
            self.lock_state()?.tombstone(session_id);
            if let Err(error) = outcome
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Stops a session the caller has taken ownership of. `Ok(None)` means a
    /// concurrent shutdown finished it while this call waited for the service.
    async fn stop_owned_session(
        &self,
        session_id: &str,
        harness: &Arc<AcpHarness<D>>,
    ) -> Result<Option<()>, AcpError> {
        harness.request_cancel()?;
        let mut service = harness.service.lock().await;
        if self.lock_state()?.is_closed(session_id) {
            return Ok(None);
        }
        stop_session(&mut service, harness).await.map(Some)
    }
}

/// Interrupts any running turn, closes the canonical session, and stops the
/// service. Stops at the first failure so a half-closed session is reported
/// rather than silently tombstoned.
async fn stop_session<D>(
    service: &mut HeadlessService<D>,
    harness: &AcpHarness<D>,
) -> Result<(), AcpError>
where
    D: TurnDriver,
{
    // Reference `aclose` cancels the experiments task before it closes
    // anything else, so a stopping session never waits on a lookup in flight.
    if let Some(experiments) = harness.experiments.as_ref() {
        experiments.close().await;
    }
    if let Some(turn_id) = harness.running_turn_id()? {
        service.interrupt(&harness.session_id, &turn_id)?;
    }
    service.close_session(&harness.session_id).await?;
    service.shutdown()?;
    Ok(())
}
