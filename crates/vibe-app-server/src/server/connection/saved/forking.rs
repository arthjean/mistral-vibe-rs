//! Forking the root: its conversation through a chosen turn is copied onto a
//! new session, which replaces the root or is left saved.
//!
//! Reference `_session_fork` (`vibe/app_server/_handler.py`).

use super::*;

impl ServerConnection {
    /// Reference `_session_fork`: the root is copied onto a new session,
    /// through the turn `entryId` names, and the copy either replaces the root
    /// or is left saved for later.
    pub(super) fn fork_session(
        &mut self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let ForkParams {
            source_session_id: source,
            entry_id,
            history_limit,
            attach,
        } = wire_validation::validate_params(&request.method, &request.params).map_err(rejected)?;
        let history_limit = usize::from(history_limit);
        if self.root_id().as_deref() != Some(source.as_str()) {
            return Err(not_found(&source));
        }
        let (busy, logging_enabled) = {
            let sessions = self.server.lock_sessions();
            let session = sessions.get(&source).ok_or_else(|| not_found(&source))?;
            (
                session.active_turn.clone(),
                self.server.workspace.session_logging().enabled,
            )
        };
        if let Some(turn_id) = busy {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                format!("Session is busy running turn {turn_id}"),
            ));
        }
        if !attach && !logging_enabled {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                "Detached forks require session logging to be enabled",
            ));
        }
        let workspace = &self.server.workspace;
        let source_session = match workspace.publish_draft(&source)? {
            Some(published) => published,
            None => workspace.load_session(&source)?,
        };
        let keep = match &entry_id {
            Some(entry_id) => Some(fork_keep(&source_session.messages, entry_id).ok_or_else(
                || {
                    ProtocolFault::plain(
                        ProtocolErrorCode::NotFound,
                        format!("Forkable history entry not found: {entry_id}"),
                    )
                },
            )?),
            None => None,
        };
        let new_id = vibe_core::session_id::rotate_session_id(&source);
        let lease = self.server.acquire_lease(&new_id)?;
        let forked = workspace.fork_saved_session(&source, &new_id, keep)?;
        if !attach {
            drop(lease);
            let state = detached_fork_state(&self.server, &forked, history_limit);
            return Ok(success_batch(
                request.id.clone(),
                result_map([
                    ("sourceSessionId", json!(source)),
                    ("state", state),
                    ("lastEventId", json!(0)),
                ]),
            ));
        }
        let attachment = crate::workspace::runtime_attachment(&forked);
        self.server.attach_workspace_runtime(&attachment, None)?;
        lease.keep();
        if let Some(session) = self.server.lock_sessions().get_mut(&new_id) {
            session.snapshot = Some(persisted_projection(
                &forked,
                u16::try_from(history_limit).unwrap_or(u16::MAX),
                &session.working_directory,
            ));
        }
        self.retire_root();
        self.attached_sessions.insert(new_id.clone());
        self.root = Some(new_id.clone());
        let state = self.live_state(&new_id)?.unwrap_or(Value::Null);
        Ok(success_batch(
            request.id.clone(),
            result_map([
                ("sourceSessionId", json!(source)),
                ("state", state),
                ("lastEventId", json!(0)),
            ]),
        ))
    }

    /// Lets the root go: the connection detaches from it and its lease is
    /// released, which is what adopting a replacement does upstream.
    pub(in crate::server) fn retire_root(&mut self) {
        let Some(root) = self.root.take() else {
            return;
        };
        let current = self.root_id_of(&root);
        let mut scratchpad = None;
        if let Some(session) = self.server.lock_sessions().get_mut(&root)
            && self.attached_sessions.remove(&root)
        {
            session.attachments = session.attachments.saturating_sub(1);
            // The runtime nobody holds any longer ends here, its scratchpad
            // with it, as the reference's rebind removes the previous one.
            if session.attachments == 0 {
                scratchpad = session.scratchpad.take();
            }
        }
        drop(scratchpad);
        self.server
            .release_lease(current.as_deref().unwrap_or(&root));
    }

    fn root_id_of(&self, key: &str) -> Option<String> {
        let sessions = self.server.lock_sessions();
        sessions.get(key).map(|session| session.id.clone())
    }
}

/// How many stored messages a fork anchored at the user message `entry_id`
/// keeps: that message and the turn it opened (reference
/// `_messages_for_fork`). Only a user message the operator wrote anchors one.
fn fork_keep(messages: &[ModelMessage], entry_id: &str) -> Option<usize> {
    let anchor = crate::workspace::rewind_entry_index(messages, entry_id)?;
    Some(
        messages
            .iter()
            .enumerate()
            .skip(anchor + 1)
            .find(|(_, message)| {
                matches!(
                    message,
                    ModelMessage::User {
                        injected: false,
                        ..
                    }
                )
            })
            .map_or(messages.len(), |(index, _)| index),
    )
}

/// Reference `build_public_state` over a fork nobody attached: the saved
/// transcript, under the runtime the fork was built with.
fn detached_fork_state(
    server: &AppServer,
    forked: &HydratedSession,
    history_limit: usize,
) -> Value {
    let mut state = stored_state(&forked.metadata.id, forked, history_limit);
    let agent = forked
        .metadata
        .agent_profile
        .as_ref()
        .and_then(|profile| serde_json::from_value::<AgentProfile>(profile.clone()).ok())
        .map(|profile| crate::workspace::agent_summary(&profile));
    let session = &mut state["session"];
    session["agent"] = json!(agent);
    session["model"] = json!(server.workspace.active_model_alias());
    session["workspaceRoots"] = json!([forked.metadata.working_directory]);
    session["tokenUsage"] = json!({"inputTokens": 0, "outputTokens": 0, "totalTokens": 0});
    state
}

/// Reference `SessionForkParams`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForkParams {
    source_session_id: String,
    entry_id: Option<String>,
    #[serde(default = "default_history_limit")]
    history_limit: u16,
    #[serde(default = "default_true")]
    attach: bool,
}
