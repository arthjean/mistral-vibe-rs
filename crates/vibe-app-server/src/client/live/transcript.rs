//! What a turn opens with: the system messages the reservation composes, and
//! the transcript the store already holds for the session.

use super::middleware::runs_plan_agent;
use super::*;

/// The persisted half of a turn: the transcript the store already holds, the
/// identity the engine runs the turn under, and the metadata the sink and the
/// baseline are built from.
///
/// A driver with no session root has none of this, which is the only difference
/// between a persisted turn and an ephemeral one.
pub(super) struct TurnTranscript {
    pub(super) store: SessionStore,
    pub(super) metadata: vibe_core::storage::SessionMetadata,
    pub(super) session_id: String,
    pub(super) messages: Vec<ModelMessage>,
}

impl LiveTurnDriver {
    /// The system messages every cycle of this session opens with.
    ///
    /// Reference `_loop.py` composes them on every cycle rather than only on
    /// the first, so a resumed session runs under the same directives a fresh
    /// one does. Plan mode also creates the file it names here, which is what
    /// makes the path writable by the time the model reads the directive.
    pub(super) async fn system_preamble(
        &self,
        reservation: &TurnReservation,
    ) -> Result<Vec<ModelMessage>, DriverError> {
        // The server composes the session's prompt, headless section included
        // (`vibe/core/system_prompt.py:380-425`). A reservation built without
        // it runs on this driver's configured prompt, which carries the
        // headless directive the same way.
        let content = match &reservation.system_prompt {
            Some(prompt) => prompt.text.clone(),
            None if reservation.intent.headless => format!(
                "{}\n\n{}",
                self.system_prompt.trim_end(),
                vibe_core::system_prompt::HEADLESS_SECTION
            ),
            None => self.system_prompt.clone(),
        };
        let messages = vec![ModelMessage::System { content }];
        if runs_plan_agent(reservation) {
            let plan_path = self
                .plan_directory()
                .map(|directory| plan_file_path(&directory, &reservation.session_id))
                .ok_or_else(|| DriverError::Tool("plan file root is unavailable".to_owned()))?;
            if let Some(parent) = plan_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|error| DriverError::Tool(error.to_string()))?;
            }
            tokio::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&plan_path)
                .await
                .map_err(|error| DriverError::Tool(error.to_string()))?;
        }
        Ok(messages)
    }

    /// Opens the transcript this turn continues, creating one when the store
    /// holds none.
    ///
    /// A `resume` selector and a `continue` both name a session the store
    /// already has. Without either, the session's own identifier is tried and a
    /// miss means this is its first turn.
    pub(super) fn open_transcript(
        &self,
        root: &Path,
        reservation: &TurnReservation,
    ) -> Result<TurnTranscript, DriverError> {
        let store = SessionStore::new(root);
        let hydrated = if let Some(selector) = &reservation.intent.resume {
            Some(
                store
                    .resume(selector, &self.system_prompt, BTreeMap::new())
                    .map_err(DriverError::Storage)?,
            )
        } else if reservation.intent.continue_session {
            Some(
                store
                    .continue_session(
                        &reservation.working_directory,
                        &self.system_prompt,
                        BTreeMap::new(),
                    )
                    .map_err(DriverError::Storage)?,
            )
        } else {
            // The session the server opened: written already, or named and
            // waiting for its first save.
            match store.open(&reservation.session_id) {
                Ok(hydrated) => Some(hydrated),
                Err(vibe_core::storage::StorageError::SessionNotFound(_)) => None,
                Err(error) => return Err(DriverError::Storage(error)),
            }
        };
        match hydrated {
            Some(hydrated) => Ok(TurnTranscript {
                store,
                session_id: hydrated.metadata.id.clone(),
                metadata: hydrated.metadata,
                messages: hydrated.messages,
            }),
            None => {
                let metadata = store
                    .create(
                        &reservation.session_id,
                        &reservation.working_directory,
                        None,
                        crate::host::now_millis(),
                    )
                    .map_err(DriverError::Storage)?;
                Ok(TurnTranscript {
                    store,
                    metadata,
                    session_id: reservation.session_id.clone(),
                    messages: Vec::new(),
                })
            }
        }
    }
}
