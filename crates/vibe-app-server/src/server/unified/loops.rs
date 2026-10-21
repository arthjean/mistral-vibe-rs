//! The prompts a unified session schedules for itself, and the task that
//! fires them.
//!
//! Reference `_dispatch_scheduled_loops` and `_run_scheduled_loops`
//! (`vibe/app_server/_unified_harness_backend_adapter.py`): `loops/*` edits
//! the session's own schedule, and a due loop starts a turn once the session
//! is idle.

use vibe_harness::loops::ScheduledPrompt;

use super::params::{LoopCreateParams, LoopDeleteParams};
use super::*;

impl ServerConnection {
    /// Reference `_dispatch_scheduled_loops`: `loops/*` on the session's own
    /// schedule, a refusal answered as invalid parameters.
    pub(in crate::server) fn unified_loops(
        &self,
        request: &ServerRequest,
        unified: &UnifiedSession,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let loops = Arc::clone(&unified.core().loops);
        let outcome = match request.method.as_str() {
            "loops/list" => Ok(result_map([(
                "loops",
                json!(
                    loops
                        .list()
                        .iter()
                        .map(ScheduledPrompt::public)
                        .collect::<Vec<_>>()
                ),
            )])),
            "loops/create" => {
                let params = from_params::<LoopCreateParams>(&request.params)?;
                loops
                    .create(&params.interval, &params.prompt)
                    .map(|created| result_map([("loop", created.public())]))
            }
            "loops/delete" => {
                let params = from_params::<LoopDeleteParams>(&request.params)?;
                loops
                    .delete(&params.loop_id)
                    .map(|deleted| result_map([("loop", deleted.public())]))
            }
            "loops/clear" => loops
                .clear()
                .map(|count| result_map([("count", json!(count))])),
            other => {
                return Err(ProtocolFault::plain(
                    ProtocolErrorCode::MethodNotFound,
                    format!("Method not found: {other}"),
                ));
            }
        };
        let result = outcome
            .map_err(|error| ProtocolFault::plain(ProtocolErrorCode::InvalidParams, error.0))?;
        Ok(success_batch(request.id.clone(), result))
    }
}

impl AppServer {
    /// The connection now holding `session_id` takes the frames the session
    /// raises on its own, and the session's scheduled loops start running.
    pub(crate) fn attach_unified(&self, session_id: &str, frames: FrameSink) {
        let Some(session) = self.unified_session(session_id) else {
            return;
        };
        *lock(&session.outbox) = Some(frames);
        if session
            .runner
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let server = self.clone();
        let weak = Arc::downgrade(&session);
        drop(session);
        tokio::spawn(async move { run_scheduled_loops(server, weak).await });
    }
}

/// Reference `_run_scheduled_loops`: while the session is live, a due loop
/// starts a turn once the session is idle, and is then rescheduled whether
/// or not the turn could start.
async fn run_scheduled_loops(server: AppServer, weak: std::sync::Weak<UnifiedSession>) {
    let Some(wake) = weak.upgrade().map(|session| Arc::clone(&session.wake)) else {
        return;
    };
    loop {
        let delay = {
            let Some(session) = weak.upgrade() else {
                return;
            };
            session
                .core()
                .loops
                .next_due_in(vibe_core::clock::now_seconds_f64())
                .unwrap_or(1.0)
        };
        tokio::select! {
            () = tokio::time::sleep(std::time::Duration::from_secs_f64(delay.clamp(0.05, 1.0))) => {}
            () = wake.notified() => {}
        }
        let Some(session) = weak.upgrade() else {
            return;
        };
        let live = server
            .unified_session(&session.id)
            .is_some_and(|live| Arc::ptr_eq(&live, &session));
        if !live {
            return;
        }
        if session.host().running.is_some() {
            continue;
        }
        if let Some(begun) = server.drain_unified_queue(&session) {
            run_unattended(&session, begun);
            continue;
        }
        let loops = Arc::clone(&session.core().loops);
        let Some(due) = loops.due(vibe_core::clock::now_seconds_f64()) else {
            continue;
        };
        let fired_at = now_millis();
        let display = json!({
            "version": "1",
            "host": "vibe",
            "content": [{"type": "vibe.scheduled_loop", "loopId": due.id, "firedAt": fired_at}],
        });
        let prepared = server.prepare_unified_input(
            &session,
            vec![json!({"type": "text", "text": due.prompt})],
            Some(display),
            true,
        );
        let Ok((content, display)) = prepared else {
            loops.mark_fired(&due.id, vibe_core::clock::now_seconds_f64());
            continue;
        };
        let request = TurnRequest {
            injected: false,
            content,
            client_message_id: None,
            user_display_content: display,
        };
        let origin = TurnOrigin::Scheduled {
            loop_id: due.id.clone(),
            fired_at,
        };
        if let Ok(begun) = server.begin_unified_turn(Arc::clone(&session), request, origin) {
            run_unattended(&session, begun);
        }
        loops.mark_fired(&due.id, vibe_core::clock::now_seconds_f64());
    }
}

/// Publishes a turn no request started and drives it on the connection that
/// holds the session, if one does.
fn run_unattended(session: &UnifiedSession, begun: BegunTurn) {
    session.send(begun.frames);
    let outbox = lock(&session.outbox).clone();
    if let Some(outbox) = outbox {
        tokio::spawn(drive_unified_turn(begun.work, outbox));
    }
}

/// Reference `_scheduled_loop_notice`: the notice a fired loop leaves in
/// its turn, stamped when the loop fired.
pub(super) fn scheduled_loop_notice(
    session_id: &str,
    turn_id: &str,
    loop_id: &str,
    fired_at: u64,
) -> Value {
    json!({
        "type": "notice",
        "id": format!("scheduled-loop-{turn_id}"),
        "sessionId": session_id,
        "turnId": turn_id,
        "createdAt": fired_at,
        "updatedAt": fired_at,
        "generationStatus": "completed",
        "level": "info",
        "relatedEntryId": null,
        "inputEntryId": null,
        "message": format!("Loop `{loop_id}` fired"),
        "details": {"kind": "scheduled_loop_fired", "loopId": loop_id},
    })
}
