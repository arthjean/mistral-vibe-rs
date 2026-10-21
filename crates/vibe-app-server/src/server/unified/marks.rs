//! The marks a unified host keeps beside each session, and its title.
//!
//! Reference `rename`, `pin`, `archive` and `mark_as_seen` on the unified
//! host (`vibe/app_server/_unified_harness_backend_adapter.py`): the marks
//! live in the session's metadata, an open session publishes what changed,
//! and a stored one is rewritten in place. A mark that cannot be written is
//! refused, open session or stored one; a title is the harness's, so the
//! metadata that mirrors it is written at best (reference
//! `_persist_session_metadata_best_effort`).

use super::params::{MarkParams, RenameParams};
use super::*;

/// The fields a session mark answers with, or patches the session with.
type MarkFields = Vec<(&'static str, Value)>;

/// The change one of the mark methods asks for.
#[derive(Clone, Copy)]
enum Mark {
    Pin(bool),
    Archive(bool),
    Seen,
}

impl Mark {
    fn of(method: &str, params: &MarkParams) -> Self {
        match method {
            "session/pin" => Self::Pin(params.pinned),
            "session/archive" => Self::Archive(params.archived),
            _ => Self::Seen,
        }
    }

    /// Applies the mark to `meta` and `marks`, answering the response fields
    /// and the session patch it publishes.
    fn apply(
        self,
        meta: &mut Map<String, Value>,
        marks: &mut SessionMarks,
    ) -> (MarkFields, MarkFields) {
        let now = now_millis();
        match self {
            Self::Pin(pinned) => {
                marks.pinned_at = pinned.then_some(now);
                meta.insert("pinned_at".to_owned(), mark_time(marks.pinned_at));
                (
                    vec![("pinnedAt", json!(marks.pinned_at))],
                    vec![("/pinnedAt", json!(marks.pinned_at))],
                )
            }
            Self::Archive(archived) => {
                marks.archived_at = archived.then_some(now);
                meta.insert("archived_at".to_owned(), mark_time(marks.archived_at));
                // Archiving a session unpins it; restoring one leaves the pin.
                if archived {
                    marks.pinned_at = None;
                    meta.insert("pinned_at".to_owned(), Value::Null);
                }
                (
                    vec![("archivedAt", json!(marks.archived_at))],
                    vec![
                        ("/archivedAt", json!(marks.archived_at)),
                        ("/pinnedAt", json!(marks.pinned_at)),
                    ],
                )
            }
            Self::Seen => {
                marks.is_unseen = false;
                meta.insert("seen_at".to_owned(), mark_time(Some(now)));
                (Vec::new(), vec![("/isUnseen", json!(false))])
            }
        }
    }
}

/// The marks a session's metadata records.
pub(super) fn marks_of(meta: &Map<String, Value>) -> SessionMarks {
    let millis = |key: &str| {
        meta.get(key)
            .and_then(Value::as_str)
            .and_then(parse_iso_millis)
    };
    let unseen = millis("unseen_at");
    let seen = millis("seen_at");
    SessionMarks {
        bumped_at: millis("bumped_at"),
        pinned_at: millis("pinned_at"),
        archived_at: millis("archived_at"),
        is_unseen: match (unseen, seen) {
            (Some(unseen), Some(seen)) => unseen > seen,
            (Some(_), None) => true,
            _ => false,
        },
    }
}

/// A mark's time as the session metadata stores it.
fn mark_time(at: Option<u64>) -> Value {
    at.map_or(Value::Null, |millis| json!(iso_micros(millis * 1000)))
}

impl UnifiedSession {
    /// Rewrites the session's marks with `mark`, persists them, and publishes
    /// the patch it makes when that changes what a client holds. Answers the
    /// response fields and that frame; marks that cannot be persisted are
    /// refused and left as they stood.
    fn remark(&self, mark: Mark) -> Result<(MarkFields, Option<Vec<u8>>), ProtocolFault> {
        let title = self.title_meta();
        let mut host = self.host();
        let host = &mut *host;
        let mut meta = host.meta.clone();
        meta.extend(title);
        let mut marks = host.marks;
        let (fields, patch) = mark.apply(&mut meta, &mut marks);
        if let Some(store) = &host.store {
            store.write_meta(&meta).map_err(meta_fault)?;
        }
        host.meta = meta;
        host.marks = marks;
        let frame = host.translator.publish_session_patch(&patch).map(encode);
        Ok((fields, frame))
    }
}

impl ServerConnection {
    /// Reference `pin`, `archive` and `mark_as_seen` on the unified host.
    pub(in crate::server) fn unified_mark(
        &mut self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let params = from_params::<MarkParams>(&request.params)?;
        let mark = Mark::of(&request.method, &params);
        if let Some(live) = self.server.unified_session(&params.session_id) {
            let (fields, frame) = live.remark(mark)?;
            let mut batch = success_batch(request.id.clone(), fields_map(fields));
            batch.outbound.splice(0..0, frame);
            return Ok(batch);
        }
        let stored = self.require_stored(&params.session_id)?;
        let mut meta = stored.read_meta().unwrap_or_default();
        let mut marks = marks_of(&meta);
        let (fields, _) = mark.apply(&mut meta, &mut marks);
        stored.write_meta(&meta).map_err(meta_fault)?;
        Ok(success_batch(request.id.clone(), fields_map(fields)))
    }

    /// Reference `rename`: the harness retitles the session, as its owner
    /// chose.
    pub(in crate::server) fn unified_rename(
        &mut self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let params = from_params::<RenameParams>(&request.params)?;
        let session_id = params.session_id.as_str();
        let title = params.title.trim();
        if title.is_empty() {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::InvalidParams,
                "Session title cannot be empty.",
            ));
        }
        let respond = |watermark: u64| {
            result_map([
                ("title", json!(title)),
                ("updatedAt", Value::Null),
                ("lastEventId", json!(watermark)),
            ])
        };
        if let Some(live) = self.server.unified_session(session_id) {
            let (publication, watermark) = {
                let mut core = live.core();
                let advance = core.projector.rename(title, "manual", now_millis());
                (core.publication(advance, None), core.projector.watermark)
            };
            let frames = live.publish(&publication);
            {
                let mut host = live.host();
                host.meta.insert("title".to_owned(), json!(title));
                host.meta.insert("title_source".to_owned(), json!("manual"));
                if let Some(store) = &host.store {
                    let _ = store.write_meta(&host.meta);
                }
            }
            let mut batch = success_batch(request.id.clone(), respond(watermark));
            batch.outbound.splice(0..0, frames);
            return Ok(batch);
        }
        let mut stored = self.require_stored(session_id)?;
        let mut core = stored
            .load(PathBuf::new())
            .ok_or_else(|| ProtocolFault::internal("the session store cannot be read"))?;
        let _ = core.projector.rename(title, "manual", now_millis());
        stored
            .commit(&core, false)
            .map_err(|error| ProtocolFault::internal(error.to_string()))?;
        let mut meta = stored.read_meta().unwrap_or_default();
        meta.insert("title".to_owned(), json!(title));
        meta.insert("title_source".to_owned(), json!("manual"));
        let _ = stored.write_meta(&meta);
        Ok(success_batch(request.id.clone(), respond(0)))
    }
}

/// The refusal of a mark the session metadata could not record.
fn meta_fault(error: std::io::Error) -> ProtocolFault {
    ProtocolFault::internal(error.to_string())
}

fn fields_map(fields: MarkFields) -> BTreeMap<String, Value> {
    fields
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}
