//! Reading a session's history and replaying it as ACP updates.

use vibe_app_server::client::TurnDriver;

use crate::agent::AcpAgent;
use crate::history::{HISTORY_PAGE_SIZE, history_page, parse_history_user_message_id};
use crate::protocol::{AcpError, AcpHistoryPage, AcpSessionUpdate};
use crate::updates::history_entry_updates;

impl<D> AcpAgent<D>
where
    D: TurnDriver,
{
    pub(crate) async fn history(
        &self,
        session_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<AcpHistoryPage, AcpError> {
        if !(1..=HISTORY_PAGE_SIZE).contains(&limit) {
            return Err(AcpError::InvalidParams(format!(
                "history limit must be between 1 and {HISTORY_PAGE_SIZE}"
            )));
        }
        let harness = self.session_harness(session_id)?;
        let mut service = harness.service.lock().await;
        let entries = history_page(&mut service, session_id, offset, limit)?;
        Ok(AcpHistoryPage {
            next_offset: (entries.len() == limit).then_some(offset.saturating_add(entries.len())),
            entries,
        })
    }

    /// Replays a saved session as ACP updates so a reconnecting client can
    /// rebuild the transcript.
    pub async fn replay_history(
        &self,
        session_id: &str,
        mut emit: impl FnMut(AcpSessionUpdate) -> Result<(), AcpError>,
    ) -> Result<(), AcpError> {
        let mut offset = 0;
        loop {
            let page = self.history(session_id, offset, HISTORY_PAGE_SIZE).await?;
            for (index, entry) in page.entries.into_iter().enumerate() {
                for update in history_entry_updates(&entry, offset.saturating_add(index))? {
                    emit(AcpSessionUpdate {
                        session_id: session_id.to_owned(),
                        update,
                    })?;
                }
            }
            let Some(next) = page.next_offset else {
                return Ok(());
            };
            if next <= offset {
                return Err(AcpError::InvalidResponse(
                    "history cursor did not advance".to_owned(),
                ));
            }
            offset = next;
        }
    }

    /// Resolves the history index a fork should branch from, accepting both
    /// replayed identifiers and IDs minted for live turns.
    pub(crate) fn resolve_user_message_anchor(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<usize, AcpError> {
        if let Some(anchor) = parse_history_user_message_id(message_id) {
            return Ok(anchor);
        }
        if let Some(harness) = self.lock_state()?.active(session_id)
            && let Some(anchor) = harness.user_message_anchor(message_id)?
        {
            return Ok(anchor);
        }
        Err(AcpError::InvalidParams(format!(
            "cannot fork from unknown user messageId `{message_id}`"
        )))
    }
}
