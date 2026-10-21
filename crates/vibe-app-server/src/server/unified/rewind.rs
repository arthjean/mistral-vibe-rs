//! `session/rewind` and `session/rewind/read` on a unified root.
//!
//! Reference `_rewind`, `rewind_fork` and the `session/rewind/read` arm of
//! `_dispatch_session_extension` (`vibe/app_server/_unified_harness_backend_adapter.py`):
//! a rewind is anchored on a user message of the public history, the harness
//! truncates its conversation before it (in place) or forks the conversation
//! that precedes it into a new root, and the host's file snapshots answer
//! what a restore puts back.

use super::*;
use crate::server::connection::rewind::{RewindParams, rewind_params};

/// Reference `_rewind` pages: the history and turns an in-place rewind
/// answers with.
const INPLACE_LIMIT: usize = 500;
/// Reference `rewind_fork`'s `history_limit`.
const FORK_LIMIT: usize = 200;

/// The user message a rewind is anchored on.
struct Anchor {
    /// Reference `PublicMessageEntry.text`.
    text: String,
    /// The user entries before it, oldest first.
    earlier: Vec<String>,
}

impl ServerConnection {
    /// Reference `session/rewind/read`: which files a rewind to the entry
    /// would restore.
    pub(in crate::server) fn unified_rewind_read(
        &self,
        request: &ServerRequest,
        unified: &Arc<UnifiedSession>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let params = rewind_params(&request.params, false)?;
        require_root(unified, &params)?;
        anchor(unified, &params.entry_id)?;
        let paths = unified.host().rewind.restorable_paths(&params.entry_id);
        Ok(success_batch(
            request.id.clone(),
            result_map([
                ("hasFileChanges", json!(!paths.is_empty())),
                ("paths", json!(paths)),
            ]),
        ))
    }

    /// Reference `_rewind` and `rewind_fork`.
    pub(in crate::server) fn unified_rewind(
        &mut self,
        request: &ServerRequest,
        unified: Arc<UnifiedSession>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let params = rewind_params(&request.params, true)?;
        require_root(&unified, &params)?;
        require_settled(&unified)?;
        let anchor = anchor(&unified, &params.entry_id)?;
        if params.inplace {
            self.rewind_in_place(request, &unified, &params, anchor)
        } else {
            self.rewind_into_fork(request, &unified, &params, anchor)
        }
    }

    fn rewind_in_place(
        &self,
        request: &ServerRequest,
        unified: &Arc<UnifiedSession>,
        params: &RewindParams,
        anchor: Anchor,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let publication = {
            let mut core = unified.core();
            let advance = core
                .rewind(&params.entry_id, now_millis())
                .map_err(|message| {
                    ProtocolFault::plain(ProtocolErrorCode::InvalidParams, message)
                })?;
            core.publication(advance, None)
        };
        let mut outbound = unified.publish(&publication);
        unified.settle();
        let restored = {
            let mut host = unified.host();
            let restored = if params.restore_files {
                host.rewind.restore(&params.entry_id)
            } else {
                vibe_harness::rewind::Restored::default()
            };
            host.rewind.drop_from(&params.entry_id);
            restored
        };
        let state = unified.peek_state(Some(INPLACE_LIMIT));
        outbound.extend(
            success_batch(
                request.id.clone(),
                result_map([
                    ("message", json!(anchor.text)),
                    ("restoreErrors", json!(restored.errors)),
                    ("restoredPaths", json!(restored.paths)),
                    ("state", state),
                    ("sessionLog", self.server.session_log_summary(&unified.id)),
                ]),
            )
            .outbound,
        );
        Ok(DispatchBatch {
            outbound,
            deferred: Vec::new(),
            close_after_flush: false,
        })
    }

    /// Reference `rewind_fork`: the conversation before the anchor becomes a
    /// new root whose parent is the source, which keeps every turn.
    fn rewind_into_fork(
        &mut self,
        request: &ServerRequest,
        unified: &Arc<UnifiedSession>,
        params: &RewindParams,
        anchor: Anchor,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let root = self.server.unified_session_root().ok_or_else(|| {
            ProtocolFault::internal("sessions are not saved, so none can be forked")
        })?;
        let session_id = vibe_core::session_id::generate_session_id(None);
        let now = now_millis();
        let scratchpad = vibe_harness::tools::vibe::scratchpad_dir(&root, &session_id);
        let core = {
            let source = unified.core();
            vibe_harness::engine::fork_core_before(
                &source,
                &session_id,
                now,
                scratchpad,
                &params.entry_id,
            )
            .map_err(|message| {
                ProtocolFault::with_data(
                    ProtocolErrorCode::InvalidParams,
                    message,
                    json!({
                        "details": {"entry_id": params.entry_id},
                        "harnessCode": "fork_entry_not_found",
                    }),
                )
            })?
        };
        let meta = store::new_meta(
            &session_id,
            Some(&unified.id),
            &iso_millis(now),
            &unified.cwd,
            &store::username(),
        );
        SessionStore::create(store::unified_root(&root).join(&session_id), &meta, &core).map_err(
            |error| ProtocolFault::internal(format!("the fork cannot be saved: {error}")),
        )?;
        let (restored, source_snapshots) = {
            let host = unified.host();
            let restored = if params.restore_files {
                host.rewind.restore(&params.entry_id)
            } else {
                vibe_harness::rewind::Restored::default()
            };
            (restored, host.rewind.clone())
        };
        self.retire_root();
        let mut reopened = self.reopen_unified(&session_id, FORK_LIMIT, &Map::new())?;
        let forked_users = user_entry_ids(&reopened.state["history"]);
        let renamed: Vec<(String, String)> = anchor
            .earlier
            .iter()
            .rev()
            .zip(forked_users.iter().rev())
            .map(|(source, fork)| (source.clone(), fork.clone()))
            .collect();
        if let Some(fork) = self.server.unified_session(&session_id) {
            fork.host().rewind = source_snapshots.forked(&params.entry_id, &renamed);
        }
        let checkpoint = json!({
            "type": "checkpoint",
            "id": format!("checkpoint:rewind:{}", vibe_core::session_id::uuid_v4()),
            "sessionId": session_id,
            "turnId": null,
            "createdAt": now_millis(),
            "updatedAt": now_millis(),
            "generationStatus": "completed",
            "relatedEntryId": null,
            "inputEntryId": null,
            "kind": "rewind",
            "message": "Conversation rewound",
            "details": {
                "entryId": params.entry_id,
                "restoreFiles": params.restore_files,
                "inplace": params.inplace,
            },
        });
        if let Some(history) = reopened.state["history"].as_array_mut() {
            history.push(checkpoint);
        }
        let mut batch = success_batch(
            request.id.clone(),
            result_map([
                ("message", json!(anchor.text)),
                ("restoreErrors", json!(restored.errors)),
                ("restoredPaths", json!(restored.paths)),
                ("state", reopened.state),
                ("sessionLog", self.server.session_log_summary(&session_id)),
            ]),
        );
        batch.outbound.extend(reopened.frames);
        batch.deferred.extend(reopened.deferred);
        Ok(batch)
    }
}

/// Reference `_require_session`: a rewind names the session it runs on.
fn require_root(unified: &UnifiedSession, params: &RewindParams) -> Result<(), ProtocolFault> {
    if unified.id == params.session_id {
        return Ok(());
    }
    Err(unknown_session(&params.session_id))
}

/// Reference `_require_settled`.
fn require_settled(unified: &UnifiedSession) -> Result<(), ProtocolFault> {
    match unified.host().running.as_deref() {
        None => Ok(()),
        Some("") => Err(ProtocolFault::plain(
            ProtocolErrorCode::Conflict,
            "Context compaction is already running",
        )),
        Some(turn_id) => Err(ProtocolFault::with_data(
            ProtocolErrorCode::Conflict,
            format!("A turn is already running: {turn_id}"),
            json!({"activeTurnId": turn_id}),
        )),
    }
}

/// Reference `_rewound_entry`: the public user message `entry_id` names.
fn anchor(unified: &UnifiedSession, entry_id: &str) -> Result<Anchor, ProtocolFault> {
    let state = unified.peek_state(None);
    let history = state["history"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let position = history.iter().position(|entry| {
        entry["id"] == entry_id && entry["type"] == "message" && entry["role"] == "user"
    });
    let Some(position) = position else {
        return Err(ProtocolFault::plain(
            ProtocolErrorCode::NotFound,
            format!("Rewindable history entry not found: {entry_id}"),
        ));
    };
    let text = history[position]["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default();
    Ok(Anchor {
        text,
        earlier: user_entry_ids(&Value::Array(history[..position].to_vec())),
    })
}

/// Reference `_user_entry_ids`.
fn user_entry_ids(history: &Value) -> Vec<String> {
    history
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter(|entry| entry["type"] == "message" && entry["role"] == "user")
                .filter_map(|entry| entry["id"].as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default()
}
