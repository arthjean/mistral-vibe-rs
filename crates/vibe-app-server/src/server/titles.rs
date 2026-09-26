//! Background session titles.
//!
//! Reference `AgentLoop._maybe_schedule_title_generation`,
//! `_generate_title_task` and `LegacySessionRuntime._notify_title`
//! (`vibe/core/agent_loop/_loop.py`, `vibe/app_server/_legacy_session_runtime.py`):
//! once a turn answers, a session a terminal or desktop client opened with
//! `session_logging.generate_titles` on asks the utility model to name it,
//! records the title as generated, and tells the client. A manual title is
//! never replaced, and a title that does not land makes the next turn due.

use super::*;
use vibe_core::events::{NoticeDetail, PublicEntryMetadata, PublicHistoryEntry, PublicNoticeLevel};
use vibe_core::session_title::{DISABLE_ENVIRONMENT, TitleTicket};

/// A title a settled turn made due: the conversation to name and the title it
/// may refine.
pub(crate) struct TitleJob {
    pub(crate) session_id: String,
    pub(crate) messages: Vec<ModelMessage>,
    pub(crate) previous_title: Option<String>,
    ticket: TitleTicket,
}

impl AppServer {
    /// The title the turn that just settled on `session_id` makes due, if
    /// any. `periodic` is whether the fast utility model serves titles.
    pub(crate) fn title_job(&self, session_id: &str, periodic: bool) -> Option<TitleJob> {
        if std::env::var(DISABLE_ENVIRONMENT).is_ok_and(|value| value == "1")
            || !self.workspace.session_logging().enabled
        {
            return None;
        }
        let store = self.workspace.session_store();
        let mut sessions = self.lock_sessions().ok()?;
        let session = sessions.get_mut(session_id)?;
        if !session.auto_title || session.title_in_flight {
            return None;
        }
        let hydrated = store.open(&session.id).ok()?;
        if hydrated.metadata.title_source == "manual" {
            return None;
        }
        let ticket = session.title_cadence.begin_if_due(periodic, true)?;
        session.title_in_flight = true;
        Some(TitleJob {
            session_id: session.id.clone(),
            messages: hydrated.messages,
            previous_title: hydrated.metadata.title,
            ticket,
        })
    }

    /// Records the title `job` produced and answers the notifications that
    /// announce it. A session that moved on to another identifier meanwhile
    /// keeps its own title.
    pub(crate) fn land_title(&self, job: TitleJob, title: Option<String>) -> Vec<Vec<u8>> {
        let Ok(mut sessions) = self.lock_sessions() else {
            return Vec::new();
        };
        let Some(session) = sessions.get_mut(&job.session_id) else {
            return Vec::new();
        };
        session.title_in_flight = false;
        if session.id != job.session_id {
            return Vec::new();
        }
        let Some(title) = title else {
            session.title_cadence.restore(job.ticket);
            return Vec::new();
        };
        let store = self.workspace.session_store();
        let Ok(mut hydrated) = store.open(&job.session_id) else {
            return Vec::new();
        };
        if !matches!(
            store.refresh_auto_title(&mut hydrated.metadata, &title),
            Ok(true)
        ) {
            return Vec::new();
        }
        if let Some(snapshot) = session.snapshot.as_mut() {
            snapshot.title = Some(title.clone());
        }
        if let Some(persisted) = session.persisted.as_mut() {
            persisted.metadata.title = Some(title.clone());
            "auto".clone_into(&mut persisted.metadata.title_source);
        }
        let now = now_millis();
        let entry = PublicHistoryEntry::Notice {
            metadata: PublicEntryMetadata {
                id: vibe_core::session_id::uuid_v4(),
                session_id: job.session_id.clone(),
                turn_id: None,
                created_at: now,
                updated_at: now,
                generation_status: vibe_core::events::PublicEntryGenerationStatus::Completed,
                related_entry_id: None,
            },
            level: PublicNoticeLevel::Info,
            message: "Session title changed".to_owned(),
            detail: NoticeDetail::SessionTitleUpdated {
                title: title.clone(),
            },
        };
        // Both are unsequenced, as the reference publishes them: the title is
        // session metadata rather than a step of the turn stream.
        vec![
            encode_notification(
                "session/updated",
                result_map([
                    ("eventId", json!(0)),
                    ("sessionId", json!(job.session_id)),
                    (
                        "patch",
                        json!([{"op": "replace", "path": "/title", "value": title}]),
                    ),
                    ("emittedAt", json!(now)),
                ]),
            ),
            encode_notification(
                "history/entryAdded",
                result_map([
                    ("eventId", json!(0)),
                    ("sessionId", json!(job.session_id)),
                    ("turnId", Value::Null),
                    ("entry", json!(entry)),
                    ("emittedAt", json!(now)),
                ]),
            ),
        ]
    }
}
