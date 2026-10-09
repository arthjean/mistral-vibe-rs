//! Background session titles.
//!
//! Reference `AgentLoop._maybe_schedule_title_generation`,
//! `_generate_title_task` and `LegacySessionRuntime._notify_title`
//! (`vibe/core/agent_loop/_loop.py`, `vibe/app_server/_legacy_session_runtime.py`):
//! after a model step the cadence makes due, a session a terminal or desktop
//! client opened with `session_logging.generate_titles` on asks the utility
//! model to name it beside the running turn, records the title as generated,
//! and tells the client. A manual title is never replaced, and a title that
//! does not land makes the next step due. While the title runs the session is
//! not quiescent, which a snapshot announces on either side
//! (`LegacySessionRuntime._handle_background_work`).

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
    /// Reference `_auto_titles_enabled` for a session this server opened:
    /// a terminal or desktop client with `generate_titles` on, unless the
    /// harness switch turns titles off. Such a session probes for a fast
    /// model before it is answered (`vibe/app_server/_runtime.py:1951-1959`).
    pub(crate) fn session_titles_itself(&self, session_id: &str) -> bool {
        if std::env::var(DISABLE_ENVIRONMENT).is_ok_and(|value| value == "1") {
            return false;
        }
        self.lock_sessions()
            .ok()
            .and_then(|sessions| sessions.get(session_id).map(|session| session.auto_title))
            .unwrap_or(false)
    }

    /// The title the model step that just completed on `session_id` makes
    /// due, if any, with the snapshot that announces the work. `periodic` is
    /// whether the fast utility model serves titles, and `turn_completing`
    /// whether the step ended on the model's answer.
    pub(crate) fn title_job(
        &self,
        session_id: &str,
        periodic: bool,
        turn_completing: bool,
    ) -> Option<(TitleJob, Vec<u8>)> {
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
        let ticket = session
            .title_cadence
            .begin_if_due(periodic, turn_completing)?;
        session.title_in_flight = true;
        let started = background_snapshot(session);
        Some((
            TitleJob {
                session_id: session.id.clone(),
                messages: hydrated.messages,
                previous_title: hydrated.metadata.title,
                ticket,
            },
            started,
        ))
    }

    /// Records the title `job` produced and answers the notifications that
    /// announce it, closed by the snapshot that makes the session quiescent
    /// again. A session that moved on to another identifier meanwhile keeps
    /// its own title and hears nothing.
    pub(crate) fn land_title(&self, job: TitleJob, title: Option<String>) -> Vec<Vec<u8>> {
        let Ok(mut sessions) = self.lock_sessions() else {
            return Vec::new();
        };
        let Some(session) = sessions.get_mut(&job.session_id) else {
            return Vec::new();
        };
        if session.id != job.session_id || !session.title_in_flight {
            return Vec::new();
        }
        session.title_in_flight = false;
        let mut frames = self.title_frames(session, job, title);
        frames.push(background_snapshot(session));
        frames
    }

    fn title_frames(
        &self,
        session: &mut SessionRuntime,
        job: TitleJob,
        title: Option<String>,
    ) -> Vec<Vec<u8>> {
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
            message: "Session title updated".to_owned(),
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

/// Reference `emit_snapshot(include_history=False, include_turns=False)`: the
/// session's state without its history or turns, sequenced like any snapshot.
fn background_snapshot(session: &mut SessionRuntime) -> Vec<u8> {
    let event_id = next_event_id(session);
    let mut state = public_session_state(session);
    for key in ["history", "historyBeforeCursor", "turns"] {
        state[key] = Value::Null;
    }
    encode_notification(
        "session/snapshot",
        result_map([
            ("eventId", json!(event_id)),
            ("sessionId", json!(session.id)),
            ("state", state),
            ("emittedAt", json!(now_millis())),
        ]),
    )
}
