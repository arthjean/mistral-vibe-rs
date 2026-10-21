//! Sessions served by the Unified Harness (`vibe-harness`).
//!
//! Reference `vibe/app_server/_unified_harness_backend_adapter.py`: a unified
//! session keeps its own state, which the harness republishes after every
//! step; this host composes each into the wire state, diffs it against the
//! last one the client saw, and keeps the session's marks, pins and store
//! beside it. The port's [`SessionRuntime`] still registers the session, so
//! everything the session's configuration answers (`runtime/read`, skills,
//! plugins, MCP) stays where it was.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, watch};
use vibe_core::clock::{now_micros, now_millis};
use vibe_harness::clock::{iso_micros, iso_millis, parse_iso_millis};
use vibe_harness::engine::{
    CallbackEvent, Publication, SessionCore, TurnEnd, TurnEnvironment, TurnRequest, run_turn,
};
use vibe_harness::gate::{CallbackAnswer, CallbackBook};
use vibe_harness::reconcile::{Notification, Stats, Translator};
use vibe_harness::state::{Advance, HarnessState};
use vibe_harness::store::{self, SessionStore};
use vibe_harness::wire::{
    SessionMarks, SessionPins, WireContext, wire_entry, wire_session, wire_state,
};

use super::*;

mod gate;
mod prepare;
mod provided;
mod queue;
mod rewind;
mod titles;

/// One unified session this process holds open.
pub(crate) struct UnifiedSession {
    pub(crate) id: String,
    pub(crate) cwd: String,
    pub(crate) core: Arc<Mutex<SessionCore>>,
    pub(crate) host: Mutex<UnifiedHost>,
    /// Where the session's store lives once promoted; `None` when sessions
    /// are not saved.
    session_root: Option<PathBuf>,
    /// Where frames go between turns: the connection that drove the last one.
    outbox: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    /// What a turn the session starts on its own runs with: the launch
    /// context and the client callbacks are put to.
    driver: Mutex<Option<UnifiedDriver>>,
    /// Whether the scheduled-loop runner is started.
    runner: std::sync::atomic::AtomicBool,
    /// Wakes the runner when the session may have work to start.
    wake: Arc<tokio::sync::Notify>,
    /// The descriptions its images were given for a model that reads none.
    pub(crate) describer: vibe_core::vision::ImageDescriber,
    /// What smart approve remembers across the session's calls.
    pub(crate) smart: Arc<Mutex<vibe_harness::smart::SmartState>>,
    /// Its background titles.
    titles: Mutex<titles::TitleWork>,
}

/// What a connection lends the sessions it opens, for the turns they start
/// on their own: a scheduled loop's, and a subagent's callbacks.
#[derive(Clone)]
pub(crate) struct UnifiedDriver {
    launch: Option<vibe_core::telemetry::LaunchContext>,
    delivery: CallbackDelivery,
}

/// A stored session reopened as the root: its state and what opening it
/// publishes.
struct Reopened {
    state: Value,
    frames: Vec<Vec<u8>>,
    deferred: Vec<DeferredWork>,
}

/// What the host keeps beside the harness state.
pub(crate) struct UnifiedHost {
    store: Option<SessionStore>,
    meta: Map<String, Value>,
    marks: SessionMarks,
    pins: SessionPins,
    pub(crate) translator: Translator,
    /// Turns to the reason the running turn is interrupted for.
    interrupt: Option<watch::Sender<Option<String>>>,
    pub(crate) running: Option<String>,
    /// What the session knows of its subagents.
    children: vibe_harness::subagents::ChildDirectory,
    /// The turns queued to run next.
    queue: queue::TurnQueue,
    /// The file states a rewind restores.
    rewind: vibe_harness::rewind::RewindSnapshots,
}

/// Each subagent type the prompt offers, with the profile overrides it runs
/// under.
type AgentProfiles = Vec<(String, Map<String, Value>)>;

/// The fields a session mark answers with, or patches the session with.
type MarkFields = Vec<(&'static str, Value)>;

impl UnifiedSession {
    #[allow(clippy::too_many_arguments)]
    fn new(
        id: String,
        cwd: String,
        core: SessionCore,
        session_root: Option<PathBuf>,
        meta: Map<String, Value>,
        store: Option<SessionStore>,
        stats: Stats,
        context_window: u64,
    ) -> Self {
        let marks = marks_of(&meta);
        let core = Arc::new(Mutex::new(core));
        Self {
            id,
            cwd,
            core,
            host: Mutex::new(UnifiedHost {
                store,
                meta,
                marks,
                pins: SessionPins::default(),
                translator: Translator::new(Value::Null, 0, stats, context_window),
                interrupt: None,
                running: None,
                children: vibe_harness::subagents::ChildDirectory::default(),
                queue: queue::TurnQueue::default(),
                rewind: vibe_harness::rewind::RewindSnapshots::default(),
            }),
            session_root,
            outbox: Mutex::new(None),
            driver: Mutex::new(None),
            runner: std::sync::atomic::AtomicBool::new(false),
            wake: Arc::new(tokio::sync::Notify::new()),
            describer: vibe_core::vision::ImageDescriber::default(),
            smart: Arc::default(),
            titles: Mutex::default(),
        }
    }

    /// Reference `_session.py` republishing the process list: every start
    /// and every end is a state of its own, published at once.
    fn processes_changed(&self) {
        let publication = {
            let Ok(mut core) = self.core.lock() else {
                return;
            };
            core.refresh_processes();
            core.publication(Advance::Journaled, None)
        };
        let frames = self.publish(&publication);
        self.send(frames);
    }

    /// Reference `_translate_child_event` over a state the child
    /// published: the parent's summaries of its children are republished
    /// where they changed.
    /// A callback the child opens is put to the client under the child's
    /// session, without joining the parent's history
    /// (`_callback_events(..., include_history=False)`).
    fn child_changed(&self, child_id: &str, publication: &Publication) {
        let shown;
        let state = if publication.callbacks.is_empty() {
            &publication.state
        } else {
            shown = CallbackBook::overlay(&publication.callbacks, &publication.state);
            &shown
        };
        let session = wire_state(
            state,
            &WireContext {
                cwd: Some(self.cwd.clone()),
                ..WireContext::default()
            },
        )["session"]
            .clone();
        let mut frames = {
            let Ok(mut host) = self.host.lock() else {
                return;
            };
            host.children.sessions.insert(child_id.to_owned(), session);
            Self::child_events(&mut host)
        };
        if let Some(CallbackEvent::Requested(entry)) = &publication.event
            && let Ok(driver) = self.driver.lock()
            && let Some(frame) = driver
                .as_ref()
                .and_then(|driver| driver.delivery.request_frame(entry))
        {
            frames.push(frame);
        }
        self.send(frames);
    }

    /// The summary events the parent's latest state and its children's
    /// raise.
    fn child_events(host: &mut UnifiedHost) -> Vec<Vec<u8>> {
        let history = host.translator.previous["history"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        host.children.observe(&history);
        let summaries = host.children.summaries();
        host.translator
            .child_sessions_updated(summaries)
            .into_iter()
            .map(encode)
            .collect()
    }

    /// Sends `frames` to the connection that drove the last turn.
    fn send(&self, frames: Vec<Vec<u8>>) {
        if let Ok(outbox) = self.outbox.lock()
            && let Some(outbox) = outbox.as_ref()
        {
            for frame in frames {
                let _ = outbox.send(frame);
            }
        }
    }

    fn context(
        &self,
        host: &UnifiedHost,
        event_id: u64,
        live_pins: bool,
        history_limit: Option<usize>,
    ) -> WireContext {
        WireContext {
            cwd: Some(self.cwd.clone()),
            event_id,
            history_limit,
            marks: host.marks,
            pins: if live_pins {
                host.pins.clone()
            } else {
                SessionPins::default()
            },
            worktree: None,
            child_sessions: host.children.summaries(),
        }
    }

    /// The state a read answers, with the session's live pins, which also
    /// becomes the state the next update is diffed against.
    pub(crate) fn read_state(&self, history_limit: Option<usize>) -> Value {
        let Ok(core) = self.core.lock() else {
            return Value::Null;
        };
        let Ok(mut host) = self.host.lock() else {
            return Value::Null;
        };
        let event_id = host.translator.event_id.max(core.projector.watermark);
        host.translator.event_id = event_id;
        let state = wire_state(
            &core.projector.state,
            &self.context(&host, event_id, true, history_limit),
        );
        let full = wire_state(
            &core.projector.state,
            &self.context(&host, event_id, true, None),
        );
        host.translator.previous = full;
        state
    }

    /// The state a read answers, leaving the state the next update is
    /// diffed against alone.
    pub(crate) fn peek_state(&self, history_limit: Option<usize>) -> Value {
        let Ok(core) = self.core.lock() else {
            return Value::Null;
        };
        let Ok(host) = self.host.lock() else {
            return Value::Null;
        };
        let event_id = host.translator.event_id.max(core.projector.watermark);
        wire_state(
            &core.projector.state,
            &self.context(&host, event_id, true, history_limit),
        )
    }

    /// Reference `_session_log_summary`: where the session is saved, once it
    /// is, and whether it still waits for its first title.
    pub(crate) fn session_log(&self, enabled: bool) -> Value {
        let title = self
            .core
            .lock()
            .ok()
            .and_then(|core| core.projector.state.session.title.clone());
        let path = self.host.lock().ok().and_then(|host| {
            host.store
                .as_ref()
                .map(|store| store.dir().to_string_lossy().into_owned())
        });
        json!({
            "enabled": enabled,
            "sessionId": self.id,
            "persisted": path.is_some(),
            "path": path,
            "title": title,
            "needsInitialAutoTitle": title.is_none(),
        })
    }

    /// The frames one publication raises: the callback it opened or settled,
    /// then the state, shown blocked on whatever callbacks are open.
    fn publish(&self, publication: &Publication) -> Vec<Vec<u8>> {
        let Ok(mut host) = self.host.lock() else {
            return Vec::new();
        };
        let state = &publication.state;
        if publication.advance == Advance::Journaled
            && let Some(store) = host.store.as_mut()
        {
            let _ = store.journal(
                "state",
                json!({
                    "updatedAt": state.session.updated_at,
                    "title": state.session.title,
                    "titleSource": state.session.title_source,
                }),
            );
        }
        let mut frames = Vec::new();
        match &publication.event {
            Some(CallbackEvent::Requested(entry)) => {
                frames.push(encode(
                    host.translator.callback_requested(wire_entry(entry)),
                ));
            }
            Some(CallbackEvent::Resolved(entry)) => {
                frames.push(encode(host.translator.callback_resolved(wire_entry(entry))));
            }
            None => {}
        }
        let shown;
        let state = if publication.callbacks.is_empty() {
            state
        } else {
            shown = CallbackBook::overlay(&publication.callbacks, state);
            &shown
        };
        let event_id = host.translator.event_id;
        let current = wire_state(state, &self.context(&host, event_id, false, None));
        host.rewind.observe(&current, &self.cwd);
        let (notifications, marked_unseen) = host.translator.translate(current);
        if marked_unseen {
            host.marks.is_unseen = true;
            host.meta
                .insert("unseen_at".to_owned(), json!(iso_micros(now_micros())));
            host.meta.insert("seen_at".to_owned(), Value::Null);
            if let Some(store) = &host.store {
                let _ = store.write_meta(&host.meta);
            }
        }
        frames.extend(notifications.into_iter().map(encode));
        frames.extend(Self::child_events(&mut host));
        frames
    }

    /// Rewrites the session's marks with `change`, persists them, and
    /// publishes `patch` when it changes what a client holds.
    fn remark(
        &self,
        change: impl FnOnce(&mut Map<String, Value>, &mut SessionMarks),
        patch: impl FnOnce(&SessionMarks) -> Vec<(&'static str, Value)>,
    ) -> (SessionMarks, Option<Vec<u8>>) {
        let title = self.title_meta();
        let Ok(mut host) = self.host.lock() else {
            return (SessionMarks::default(), None);
        };
        let host = &mut *host;
        host.meta.extend(title);
        change(&mut host.meta, &mut host.marks);
        if let Some(store) = &host.store {
            let _ = store.write_meta(&host.meta);
        }
        let operations = patch(&host.marks);
        let frame = host
            .translator
            .publish_session_patch(&operations)
            .map(encode);
        (host.marks, frame)
    }

    /// Reference `_persist_accepted_user_activity_metadata`: the session is
    /// bumped, and the bump published.
    fn bump(&self) -> Option<Vec<u8>> {
        let title = self.title_meta();
        let Ok(mut host) = self.host.lock() else {
            return None;
        };
        host.meta.extend(title);
        let micros = now_micros();
        let millis = micros / 1000;
        host.marks.bumped_at = Some(millis);
        host.meta
            .insert("bumped_at".to_owned(), json!(iso_micros(micros)));
        if let Some(store) = &host.store {
            let _ = store.write_meta(&host.meta);
        }
        host.translator
            .publish_session_patch(&[("/bumpedAt", json!(millis))])
            .map(encode)
    }

    /// Writes the session to disk the first time it takes a turn
    /// (reference "promotion").
    /// Has the runner look for work to start.
    fn wake(&self) {
        self.wake.notify_one();
    }

    /// Reference `_record_shell_effect` after `inject_context`: a manual
    /// command's result joins the conversation as injected context, and its
    /// settled entry joins the stored history. The live stream already
    /// carried that entry, so the frames announcing it again are dropped.
    pub(crate) fn record_host_effect(&self, entry: Option<Value>, context: String) {
        self.promote();
        let publication = {
            let Ok(mut core) = self.core.lock() else {
                return;
            };
            core.conversation
                .push(vibe_harness::engine::ConversationMessage::User {
                    content: vec![json!({"type": "text", "text": context})],
                    injected: true,
                });
            entry.map(|entry| {
                let advance = core.projector.append_entries(vec![entry], now_millis());
                core.publication(advance, None)
            })
        };
        if let Some(publication) = publication {
            let _ = self.publish(&publication);
        }
        self.settle();
    }

    fn promote(&self) {
        let Some(root) = &self.session_root else {
            return;
        };
        let Ok(core) = self.core.lock() else {
            return;
        };
        let Ok(mut host) = self.host.lock() else {
            return;
        };
        if host.store.is_some() {
            return;
        }
        let dir = store::unified_root(root).join(&self.id);
        let meta = host.meta.clone();
        if let Ok(created) = SessionStore::create(dir, &meta, &core) {
            host.store = Some(created);
        }
    }

    /// Reference `_build_unified_session_metadata`: the title a metadata
    /// write records is the session's own.
    fn title_meta(&self) -> Vec<(String, Value)> {
        self.core
            .lock()
            .map(|core| title_fields(&core))
            .unwrap_or_default()
    }

    /// Writes a generation once the session settles.
    fn settle(&self) {
        let Ok(core) = self.core.lock() else {
            return;
        };
        let Ok(mut host) = self.host.lock() else {
            return;
        };
        let end = iso_millis(core.projector.state.session.updated_at);
        host.meta.insert("end_time".to_owned(), json!(end));
        host.meta.extend(title_fields(&core));
        let meta = host.meta.clone();
        if let Some(store) = host.store.as_mut() {
            let _ = store.commit(&core, false);
            let _ = store.write_meta(&meta);
        }
    }
}

/// Reference `_metadata_title_source`: anything but a manual title is
/// recorded as `auto`.
fn title_fields(core: &SessionCore) -> Vec<(String, Value)> {
    let session = &core.projector.state.session;
    let source = if session.title_source == "manual" {
        "manual"
    } else {
        "auto"
    };
    vec![
        ("title".to_owned(), json!(session.title)),
        ("title_source".to_owned(), json!(source)),
    ]
}

fn marks_of(meta: &Map<String, Value>) -> SessionMarks {
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

fn encode(notification: Notification) -> Vec<u8> {
    let params = match notification.params {
        Value::Object(map) => map.into_iter().collect(),
        _ => BTreeMap::new(),
    };
    encode_notification(notification.method, params)
}

/// The accounting a session opens with, priced by its model.
fn opening_stats(session: &SessionRuntime) -> Stats {
    let (input, output, cached) = session.pricing;
    Stats::priced(input, output, cached)
}

impl AppServer {
    /// Files the session `from` names under `to` for good, with what this
    /// process keeps for it, so `from` names a separate saved session again
    /// (reference `clear` on the unified host).
    pub(super) fn rekey_session(&self, from: &str, to: &str) -> Result<(), ServerError> {
        self.lock_sessions()?.rekey(from, to)?;
        if let Ok(mut resources) = self.resources.lock() {
            resources.rekey_session(from, to);
        }
        if let Ok(mut plugins) = self.session_plugins.lock()
            && let Some(set) = plugins.remove(from)
        {
            plugins.insert(to.to_owned(), set);
        }
        if let Ok(mut pluginless) = self.pluginless.lock()
            && pluginless.remove(from)
        {
            pluginless.insert(to.to_owned());
        }
        Ok(())
    }

    pub(crate) fn unified_session(&self, session_id: &str) -> Option<Arc<UnifiedSession>> {
        self.unified.lock().ok()?.get(session_id).cloned()
    }

    fn insert_unified(&self, session: Arc<UnifiedSession>) {
        let weak = Arc::downgrade(&session);
        if let Ok(core) = session.core.lock() {
            core.processes.listen(Arc::new(move || {
                if let Some(session) = weak.upgrade() {
                    session.processes_changed();
                }
            }));
            let weak = Arc::downgrade(&session);
            core.subagents
                .listen(Arc::new(move |child_id: &str, publication: Publication| {
                    if let Some(session) = weak.upgrade() {
                        session.child_changed(child_id, &publication);
                    }
                }));
        }
        if let Ok(mut sessions) = self.unified.lock() {
            sessions.insert(session.id.clone(), session);
        }
    }

    /// Where unified sessions are saved, when they are.
    fn unified_session_root(&self) -> Option<PathBuf> {
        self.workspace.logged_session_root()
    }
}

impl ServerConnection {
    /// Reference `session/start` on the unified host.
    pub(super) fn unified_session_start(
        &mut self,
        request: ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let id = request.id.clone();
        let mut params = from_params::<SessionStartParams>(&request.params)?;
        if params.resume.is_some() || params.continue_session {
            let start = request.params.clone();
            return self.unified_reopen_start(id, &params, start);
        }
        let history_limit = usize::from(params.history_limit);
        let mut resolution = self.resolve_worktree(&mut params)?;
        let opened = self.open_resolved_session(&params, &mut resolution);
        let (session_id, _, mcp_configs) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                self.undo_worktree(&resolution);
                return Err(error);
            }
        };
        let session = self.open_unified(&session_id, None)?;
        let state = session.read_state(Some(history_limit));
        let mut batch = success_batch(
            id,
            result_map([("lastEventId", state["eventId"].clone()), ("state", state)]),
        );
        batch.deferred.push(DeferredWork::AttachUnified {
            session_id: session_id.clone(),
        });
        if !mcp_configs.is_empty() {
            batch.deferred.push(DeferredWork::ConfigureMcp {
                session_id,
                configs: mcp_configs,
            });
        }
        Ok(batch)
    }

    /// Builds the unified half of a registered session, fresh or from its
    /// store.
    fn open_unified(
        &self,
        session_id: &str,
        stored: Option<(SessionStore, SessionCore, Map<String, Value>)>,
    ) -> Result<Arc<UnifiedSession>, ProtocolFault> {
        let (cwd, stats, context_window, created_at, headless) = {
            let sessions = self.server.lock_sessions()?;
            let session = sessions
                .get(session_id)
                .ok_or_else(|| ProtocolFault::internal("the opened session is gone"))?;
            let cwd = vibe_core::platform::canonicalize(&session.working_directory)
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|_| session.working_directory.clone());
            (
                cwd,
                opening_stats(session),
                session.context_window,
                session.created_at,
                session.intent.headless,
            )
        };
        let root = self.server.unified_session_root();
        let scratchpad = vibe_harness::tools::vibe::scratchpad_dir(
            root.as_deref().unwrap_or_else(|| Path::new(".")),
            session_id,
        );
        let session = match stored {
            Some((store, core, meta)) => {
                let mut stats = stats;
                let unified = UnifiedSession::new(
                    session_id.to_owned(),
                    cwd,
                    core,
                    root,
                    meta,
                    Some(store),
                    Stats::priced(0.0, 0.0, None),
                    context_window,
                );
                if let (Ok(core), Ok(mut host)) = (unified.core.lock(), unified.host.lock()) {
                    let state = wire_state(&core.projector.state, &WireContext::default());
                    stats.seed(&state);
                    host.translator.stats = stats;
                    // Reference `_state_with_child_summaries` on a reopened
                    // root: each child it spawned is summarized from its own
                    // store.
                    host.children.observe(
                        state["history"]
                            .as_array()
                            .map(Vec::as_slice)
                            .unwrap_or_default(),
                    );
                    let stores = unified.session_root.as_deref().map(store::unified_root);
                    for child in host.children.known() {
                        let Some(child_core) = stores
                            .as_ref()
                            .and_then(|stores| SessionStore::open(stores.join(&child)))
                            .and_then(|stored| stored.load(PathBuf::new()))
                        else {
                            continue;
                        };
                        let session = wire_state(
                            &child_core.projector.state,
                            &WireContext {
                                cwd: Some(unified.cwd.clone()),
                                ..WireContext::default()
                            },
                        )["session"]
                            .clone();
                        host.children.sessions.insert(child, session);
                    }
                }
                unified
            }
            None => {
                let state = HarnessState::new(session_id, None, session_id, created_at);
                let core = SessionCore::new(state, scratchpad);
                let meta = store::new_meta(
                    session_id,
                    None,
                    &iso_millis(created_at),
                    &cwd,
                    &store::username(),
                );
                UnifiedSession::new(
                    session_id.to_owned(),
                    cwd,
                    core,
                    root,
                    meta,
                    None,
                    stats,
                    context_window,
                )
            }
        };
        // Reference `scheduled_loops_enabled=not headless`.
        if headless && let Ok(core) = session.core.lock() {
            core.loops.disable();
        }
        if let Ok(mut driver) = session.driver.lock() {
            *driver = Some(UnifiedDriver {
                launch: self.launch.clone(),
                delivery: CallbackDelivery {
                    server: self.server.clone(),
                    kinds: self.capabilities.callback_kinds.clone(),
                },
            });
        }
        let session = Arc::new(session);
        self.server.insert_unified(Arc::clone(&session));
        Ok(session)
    }

    /// Reference `start_turn` on a unified session.
    pub(super) fn unified_turn_start(
        &mut self,
        request: ServerRequest,
        unified: Arc<UnifiedSession>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let params = from_params::<TurnStartParams>(&request.params)?;
        let content: Vec<Value> = params
            .input
            .iter()
            .filter_map(|block| serde_json::to_value(block).ok())
            .collect();
        let (content, warnings) = self.server.describe_unified_images(&unified, content)?;
        let (content, display) = self.server.prepare_unified_input(
            &unified,
            content,
            params.user_display_content.clone(),
            true,
        )?;
        let turn_request = TurnRequest {
            content,
            client_message_id: params.client_user_message_id.clone(),
            user_display_content: display,
            injected: params.injected,
        };
        let begun = self
            .server
            .begin_unified_turn(unified, turn_request, TurnOrigin::Client)?;
        let mut outbound = warnings;
        outbound.push(success_bytes(
            request.id,
            result_map([
                (
                    "turn",
                    json!({
                        "id": begun.turn_id,
                        "sessionId": begun.session_id,
                        "status": "in_progress",
                        "startedAt": begun.started_at,
                        "completedAt": null,
                        "error": null,
                        "stopReason": null,
                        "inputEntryId": null,
                        "queueItemId": null,
                    }),
                ),
                ("lastEventId", json!(begun.event_id)),
            ]),
        ));
        let announce = self
            .server
            .lock_sessions()?
            .get_mut(&begun.session_id)
            .is_some_and(|session| std::mem::take(&mut session.connectors.announce));
        if announce {
            outbound.extend(self.runtime_updated_frame(&begun.session_id));
        }
        outbound.extend(begun.frames);
        Ok(DispatchBatch {
            outbound,
            deferred: vec![DeferredWork::RunUnifiedTurn(begun.work)],
            close_after_flush: false,
        })
    }

    /// Reference `_dispatch_scheduled_loops`: `loops/*` on the session's own
    /// schedule, a refusal answered as invalid parameters.
    pub(super) fn unified_loops(
        &self,
        request: &ServerRequest,
        unified: &UnifiedSession,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let loops = unified
            .core
            .lock()
            .map(|core| Arc::clone(&core.loops))
            .map_err(|_| ProtocolFault::internal("the session lock is poisoned"))?;
        let text = |key: &str| {
            request
                .params
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let outcome = match request.method.as_str() {
            "loops/list" => Ok(json!({
                "loops": loops.list().iter().map(vibe_harness::loops::ScheduledPrompt::public).collect::<Vec<_>>(),
            })),
            "loops/create" => loops
                .create(&text("interval"), &text("prompt"))
                .map(|created| json!({"loop": created.public()})),
            "loops/delete" => loops
                .delete(&text("loopId"))
                .map(|deleted| json!({"loop": deleted.public()})),
            "loops/clear" => loops.clear().map(|count| json!({"count": count})),
            other => {
                return Err(ProtocolFault::plain(
                    ProtocolErrorCode::MethodNotFound,
                    format!("Method not found: {other}"),
                ));
            }
        };
        let result = outcome
            .map_err(|error| ProtocolFault::plain(ProtocolErrorCode::InvalidParams, error.0))?;
        Ok(success_batch(
            request.id.clone(),
            result
                .as_object()
                .map(|object| {
                    object
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect()
                })
                .unwrap_or_default(),
        ))
    }

    /// Reference `compact` on a unified session: the context is summarized
    /// between turns, and the reply waits for the summary.
    pub(super) fn unified_compact(
        &mut self,
        request: &ServerRequest,
        unified: Arc<UnifiedSession>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let object = crate::params::object_of(&request.params);
        let (interrupt, interrupted) = watch::channel(None);
        {
            let mut host = unified
                .host
                .lock()
                .map_err(|_| ProtocolFault::internal("the session lock is poisoned"))?;
            if host.running.is_some() {
                return Err(ProtocolFault::plain(
                    ProtocolErrorCode::Conflict,
                    "A turn is already running",
                ));
            }
            host.running = Some(String::new());
            host.interrupt = Some(interrupt);
        }
        let environment = self
            .server
            .unified_environment(&unified, self.launch.as_ref())?;
        let session_log = self.server.session_log_summary(&unified.id);
        Ok(DispatchBatch {
            outbound: Vec::new(),
            deferred: vec![DeferredWork::RunUnifiedTurn(UnifiedTurnWork {
                session: unified,
                turn_id: String::new(),
                environment: Box::new(environment),
                interrupted,
                delivery: CallbackDelivery {
                    server: self.server.clone(),
                    kinds: self.capabilities.callback_kinds.clone(),
                },
                compaction: Some(CompactionRequest {
                    request_id: request.id.clone(),
                    extra_instructions: object
                        .get("extraInstructions")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    session_log,
                }),
            })],
            close_after_flush: false,
        })
    }

    /// Reference `respond_to_callback` on a unified session: the callback
    /// settles, the session is bumped, and the call waiting on it resumes once
    /// both are on the wire.
    /// Reference `delete` on the unified host: a live session is closed,
    /// its running turn interrupted, and its store removed. `None` when no
    /// unified session holds `session_id`, which the legacy path answers.
    pub(super) fn unified_delete(&self, session_id: &str) -> Option<Result<(), ProtocolFault>> {
        let live = self
            .server
            .unified
            .lock()
            .ok()
            .and_then(|mut sessions| sessions.remove(session_id));
        if let Some(live) = &live
            && let Ok(mut host) = live.host.lock()
        {
            if let Some(interrupt) = host.interrupt.take() {
                let _ = interrupt.send(Some("session deleted".to_owned()));
            }
            host.store = None;
        }
        if let Some(live) = &live
            && let Ok(core) = live.core.lock()
        {
            core.subagents.shut_down();
            let processes = Arc::clone(&core.processes);
            std::thread::spawn(move || processes.shut_down());
        }
        let directory = self
            .server
            .unified_session_root()
            .map(|root| store::unified_root(&root).join(session_id))
            .filter(|directory| directory.is_dir());
        if live.is_none() && directory.is_none() {
            return None;
        }
        if let Some(directory) = directory
            && let Err(error) = std::fs::remove_dir_all(&directory)
        {
            return Some(Err(ProtocolFault::internal(format!(
                "the session store cannot be removed: {error}"
            ))));
        }
        Some(Ok(()))
    }

    /// Reference `_inject_context`: context the model reads before the next
    /// turn, kept out of the public history unless asked for as a message.
    /// Reference `_read_background_process_output` and
    /// `_stop_background_process`, on the named root's processes.
    pub(super) fn unified_background_process(
        &self,
        method: &str,
        request_id: RequestId,
        params: &Value,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let session_id = params["sessionId"].as_str().unwrap_or_default();
        let session = self
            .server
            .unified_session(session_id)
            .ok_or_else(|| ProtocolFault::plain(ProtocolErrorCode::NotFound, "Unknown session"))?;
        let processes = session
            .core
            .lock()
            .map(|core| Arc::clone(&core.processes))
            .map_err(|_| ProtocolFault::internal("the session is gone"))?;
        let process_id = params["processId"].as_str().unwrap_or_default();
        let answered = if method == "session/backgroundProcess/stop" {
            processes.stop_raw(process_id)
        } else {
            let wait = params["waitMs"].as_u64().unwrap_or(0);
            let max_bytes =
                usize::try_from(params["maxBytes"].as_u64().unwrap_or(1)).unwrap_or(usize::MAX);
            processes.read_raw(
                process_id,
                params["fromEnd"].as_bool().unwrap_or(false),
                params["cursor"].as_u64().unwrap_or(0),
                wait,
                max_bytes,
            )
        };
        match answered {
            Ok(Value::Object(result)) => {
                Ok(success_batch(request_id, result.into_iter().collect()))
            }
            Ok(_) => Err(ProtocolFault::internal("no result")),
            Err((code, message)) => Err(ProtocolFault::plain(
                match code.as_str() {
                    "not_found" => ProtocolErrorCode::NotFound,
                    "invalid_params" => ProtocolErrorCode::InvalidParams,
                    _ => ProtocolErrorCode::InternalError,
                },
                message,
            )),
        }
    }

    pub(super) fn unified_context_inject(
        &mut self,
        request: &ServerRequest,
        unified: &Arc<UnifiedSession>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let object = crate::params::object_of(&request.params);
        if unified
            .host
            .lock()
            .map(|host| host.running.is_some())
            .unwrap_or(true)
        {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                "A turn is already running",
            ));
        }
        let content: Vec<Value> = object
            .get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if content.is_empty() {
            return Err(ProtocolFault::invalid_params(
                "Unified Harness context injection requires content",
            ));
        }
        let as_message = object
            .get("asMessage")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        unified.promote();
        let (entries, publication) = {
            let mut core = unified
                .core
                .lock()
                .map_err(|_| ProtocolFault::internal("the session lock is poisoned"))?;
            core.conversation
                .push(vibe_harness::engine::ConversationMessage::User {
                    content: content.clone(),
                    injected: true,
                });
            if as_message {
                let now = now_millis();
                let entry = vibe_harness::state::public_message_entry(
                    &unified.id,
                    None,
                    &format!("context-{}", vibe_core::session_id::random_hex::<16>()),
                    "user",
                    &content,
                    now,
                    "harness",
                );
                let advance = core.projector.append_entries(vec![entry.clone()], now);
                (vec![entry], Some(core.publication(advance, None)))
            } else {
                (Vec::new(), None)
            }
        };
        let mut outbound = vec![success_bytes(
            request.id.clone(),
            result_map([(
                "entries",
                Value::Array(entries.iter().map(wire_entry).collect()),
            )]),
        )];
        if let Some(publication) = publication {
            outbound.extend(unified.publish(&publication));
        }
        unified.settle();
        Ok(DispatchBatch {
            outbound,
            deferred: Vec::new(),
            close_after_flush: false,
        })
    }

    pub(super) fn unified_callback_result(
        &mut self,
        request: ServerRequest,
        unified: Arc<UnifiedSession>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let result = request.params.get("result").cloned().unwrap_or(Value::Null);
        let callback_id = result
            .get("callbackId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let answer = if let Some(error) = result.get("error").filter(|error| !error.is_null()) {
            CallbackAnswer::Closed(
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|message| !message.is_empty())
                    .unwrap_or("Callback closed")
                    .to_owned(),
            )
        } else if let Some(output) = result.get("output").filter(|output| !output.is_null()) {
            CallbackAnswer::Output(
                crate::wire_validation::validate_nested_field("callback/result", "output", output)
                    .map_err(|issues| self.rejected("callback/result", &issues))?,
            )
        } else {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::InvalidParams,
                "Callback result must include output or error",
            ));
        };
        let cancels_turn = matches!(
            &answer,
            CallbackAnswer::Output(output) if output.pointer("/decision/type") == Some(&json!("cancel_turn"))
        );
        // A callback a subagent opened is answered on the subagent's session.
        let child = {
            let core = unified
                .core
                .lock()
                .map_err(|_| ProtocolFault::internal("the session lock is poisoned"))?;
            let held = core.callbacks.is_open(&callback_id)
                || core.callbacks.answered.contains_key(&callback_id)
                || core.callbacks.closed.contains(&callback_id);
            if held {
                None
            } else {
                core.subagents.child_with_callback(&callback_id)
            }
        };
        let target = child
            .as_ref()
            .map_or_else(|| Arc::clone(&unified.core), |(_, core)| Arc::clone(core));
        let publication = {
            let mut core = target
                .lock()
                .map_err(|_| ProtocolFault::internal("the session lock is poisoned"))?;
            if let Some(previous) = core.callbacks.answered.get(&callback_id) {
                if *previous != result {
                    return Err(ProtocolFault::plain(
                        ProtocolErrorCode::Conflict,
                        format!("Callback already answered differently: {callback_id}"),
                    ));
                }
                None
            } else {
                if !core.callbacks.is_open(&callback_id) {
                    let message = if core.callbacks.closed.contains(&callback_id) {
                        format!("Callback closed: {callback_id}")
                    } else {
                        format!("Callback not found: {callback_id}")
                    };
                    return Err(ProtocolFault::plain(ProtocolErrorCode::NotFound, message));
                }
                let entry = core
                    .callbacks
                    .settle(&callback_id, answer, now_millis())
                    .ok_or_else(|| ProtocolFault::internal("the callback is gone"))?;
                core.callbacks.answered.insert(callback_id.clone(), result);
                Some(core.publication(Advance::Provisional, Some(CallbackEvent::Resolved(entry))))
            }
        };
        let mut outbound = Vec::new();
        let mut deferred = Vec::new();
        if let Some(publication) = publication {
            match &child {
                Some((child_id, _)) => {
                    let manager = unified
                        .core
                        .lock()
                        .ok()
                        .map(|core| Arc::clone(&core.subagents));
                    if let Some(manager) = manager {
                        manager.announce(child_id, publication);
                    }
                }
                None => outbound.extend(unified.publish(&publication)),
            }
            outbound.extend(unified.bump());
            deferred.push(DeferredWork::ReleaseUnifiedCallback {
                session_id: child
                    .as_ref()
                    .map_or_else(|| unified.id.clone(), |(child_id, _)| child_id.clone()),
                callback_id,
                cancel_turn: cancels_turn,
            });
        }
        let last_event_id = unified
            .host
            .lock()
            .map_or(0, |host| host.translator.event_id);
        outbound.push(success_bytes(
            request.id,
            result_map([
                ("accepted", json!(true)),
                ("lastEventId", json!(last_event_id)),
            ]),
        ));
        Ok(DispatchBatch {
            outbound,
            deferred,
            close_after_flush: false,
        })
    }

    /// Reference `interrupt_turn` on a unified session.
    /// Reference `steer_turn`: the message joins the running turn's history
    /// at once, and the model reads it on its next call.
    pub(super) fn unified_turn_steer(
        &mut self,
        request: ServerRequest,
        unified: Arc<UnifiedSession>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let params = from_params::<TurnSteerParams>(&request.params)?;
        let running = unified
            .host
            .lock()
            .map_err(|_| ProtocolFault::internal("the session lock is poisoned"))?
            .running
            .clone();
        if running.as_deref() != Some(params.expected_turn_id.as_str()) {
            return Err(match running {
                None => ProtocolFault::plain(ProtocolErrorCode::Conflict, "No active turn"),
                Some(active) => ProtocolFault::with_data(
                    ProtocolErrorCode::StaleTurn,
                    "No matching active turn",
                    json!({"activeTurnId": active}),
                ),
            });
        }
        let content: Vec<Value> = params
            .input
            .iter()
            .filter_map(|block| serde_json::to_value(block).ok())
            .collect();
        let (content, warnings) = self.server.describe_unified_images(&unified, content)?;
        let (content, display) = self.server.prepare_unified_input(
            &unified,
            content,
            params.user_display_content.clone(),
            params.inject_invoked_skill,
        )?;
        if content.is_empty() {
            return Err(ProtocolFault::invalid_params(
                "Unified Harness turn/steer currently supports text input only",
            ));
        }
        let publication = {
            let mut core = unified
                .core
                .lock()
                .map_err(|_| ProtocolFault::internal("the session lock is poisoned"))?;
            let active = core
                .projector
                .state
                .latest_turn
                .as_ref()
                .is_some_and(|turn| {
                    turn.id == params.expected_turn_id
                        && turn.status == vibe_harness::state::TurnStatus::InProgress
                });
            if !active {
                return Err(ProtocolFault::plain(
                    ProtocolErrorCode::Conflict,
                    "No active turn",
                ));
            }
            let advance = core.steer(
                &params.expected_turn_id,
                &TurnRequest {
                    injected: false,
                    content,
                    client_message_id: params.client_user_message_id.clone(),
                    user_display_content: display,
                },
            );
            core.publication(advance, None)
        };
        let mut outbound = warnings;
        outbound.extend(unified.publish(&publication));
        let last_event_id = unified
            .host
            .lock()
            .map(|host| host.translator.event_id)
            .unwrap_or_default();
        outbound.push(success_bytes(
            request.id,
            result_map([
                ("accepted", json!(true)),
                ("lastEventId", json!(last_event_id)),
            ]),
        ));
        Ok(DispatchBatch {
            outbound,
            deferred: Vec::new(),
            close_after_flush: false,
        })
    }

    pub(super) fn unified_turn_interrupt(
        &mut self,
        request: ServerRequest,
        unified: Arc<UnifiedSession>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let expected = request
            .params
            .get("expectedTurnId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let host = unified
            .host
            .lock()
            .map_err(|_| ProtocolFault::internal("the session lock is poisoned"))?;
        match &host.running {
            None => {
                return Err(ProtocolFault::plain(
                    ProtocolErrorCode::Conflict,
                    "No active turn",
                ));
            }
            Some(active) if active != expected => {
                return Err(ProtocolFault::with_data(
                    ProtocolErrorCode::StaleTurn,
                    "Active turn does not match expectedTurnId",
                    json!({"activeTurnId": active}),
                ));
            }
            Some(_) => {}
        }
        if let Some(interrupt) = &host.interrupt {
            interrupt.send_replace(Some("client interrupt".to_owned()));
        }
        let last_event_id = host.translator.event_id;
        Ok(success_batch(
            request.id,
            result_map([
                ("accepted", json!(true)),
                ("lastEventId", json!(last_event_id)),
            ]),
        ))
    }
}

impl AppServer {
    /// Hands a settled callback's answer to the call waiting on it, and
    /// interrupts the turn when the answer asked to.
    pub(crate) fn release_unified_callback(
        &self,
        session_id: &str,
        callback_id: &str,
        cancel_turn: bool,
    ) {
        let Some(unified) = self.unified_session(session_id) else {
            self.release_child_callback(session_id, callback_id, cancel_turn);
            return;
        };
        if let Ok(mut core) = unified.core.lock() {
            core.callbacks.release(callback_id);
        }
        if cancel_turn
            && let Ok(host) = unified.host.lock()
            && let Some(interrupt) = &host.interrupt
        {
            interrupt.send_replace(Some("approval cancel_turn".to_owned()));
        }
    }
}

impl AppServer {
    /// What the session runs on, as a turn pins it: the model and its level.
    pub(crate) fn unified_pins(&self, session_id: &str) -> SessionPins {
        let model = self.workspace.active_model_alias();
        let thinking = self
            .lock_sessions()
            .ok()
            .and_then(|sessions| {
                let session = sessions.get(session_id)?;
                self.workspace
                    .session_model(&crate::server::turns::prompt_scope(session, None))
                    .ok()
                    .flatten()
            })
            .map(|model| model.thinking);
        SessionPins {
            model,
            reasoning_effort: thinking.filter(|level| !level.is_empty()),
            agent: None,
        }
    }

    pub(crate) fn unified_environment(
        &self,
        unified: &UnifiedSession,
        launch: Option<&vibe_core::telemetry::LaunchContext>,
    ) -> Result<TurnEnvironment, ProtocolFault> {
        let sessions = self.lock_sessions()?;
        let session = sessions
            .get(&unified.id)
            .ok_or_else(|| ProtocolFault::internal("the session is gone"))?;
        let model = self
            .workspace
            .session_model(&crate::server::turns::prompt_scope(session, None))
            .ok()
            .flatten();
        let auto_approve = session.intent.auto_approve
            || session
                .agent_summary
                .as_ref()
                .is_some_and(|agent| agent["name"] == "auto-approve");
        let smart_approve = !auto_approve
            && session
                .agent_summary
                .as_ref()
                .is_some_and(|agent| agent["name"] == "smart-approve");
        let edits = session.intent.approval == vibe_core::extensions::AgentApproval::Edits;
        let enabled = session.intent.enabled_tools.clone();
        let disabled = session.intent.disabled_tools.clone();
        let scope = crate::server::turns::prompt_scope(session, None);
        let compact_threshold = session.context_window;
        // Reference `_build_unified_runtime`: `max_turns or 1_000`.
        let max_iterations = session
            .intent
            .max_turns
            .and_then(|turns| u32::try_from(turns).ok())
            .filter(|turns| *turns > 0)
            .unwrap_or(1_000);
        drop(sessions);
        let tools = self
            .resources
            .lock()
            .ok()
            .and_then(|resources| resources.session_tool_registry(&unified.id));
        let available: Vec<String> = tools
            .as_ref()
            .and_then(|tools| tools.list().ok())
            .map(|specs| {
                let mut listed: Vec<Value> = specs
                    .into_iter()
                    .map(|spec| json!({"name": spec.name}))
                    .collect();
                super::attachment::publish_session_tools(&mut listed, &enabled, &disabled);
                listed
                    .into_iter()
                    .filter_map(|tool| tool["name"].as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let mut metadata = BTreeMap::new();
        if let Some(launch) = launch {
            metadata.insert(
                "agent_entrypoint".to_owned(),
                launch.agent_entrypoint.clone(),
            );
            metadata.insert("agent_version".to_owned(), launch.agent_version.clone());
            metadata.insert("client_name".to_owned(), launch.client_name.clone());
            metadata.insert("client_version".to_owned(), launch.client_version.clone());
            if let Some(terminal) = &launch.terminal_emulator {
                metadata.insert("terminal_emulator".to_owned(), terminal.clone());
            }
        }
        metadata.insert("os".to_owned(), vibe_core::telemetry::platform_id());
        metadata.insert("arch".to_owned(), vibe_core::telemetry::platform_arch());
        if let Some(version) = vibe_core::telemetry::platform_version() {
            metadata.insert("os_version".to_owned(), version);
        }
        metadata.insert("version".to_owned(), env!("CARGO_PKG_VERSION").to_owned());
        metadata.insert("session_id".to_owned(), unified.id.clone());
        let parent = unified
            .core
            .lock()
            .ok()
            .and_then(|core| core.projector.state.session.parent_session_id.clone());
        if let Some(parent) = parent {
            metadata.insert("parent_session_id".to_owned(), parent);
        }
        metadata.insert("call_source".to_owned(), "vibe_code".to_owned());
        metadata.insert("host_kind".to_owned(), "local".to_owned());
        let provider = self
            .secondary_provider
            .clone()
            .ok_or_else(|| ProtocolFault::internal("no model provider is configured"))?;
        let cwd = PathBuf::from(&unified.cwd);
        // Reference `derive_config` under `ToolGate.CLASSIFIER`: the
        // classifier runs on the smart-approve utility model, learned at open.
        let smart = smart_approve
            .then(|| {
                self.workspace.session_utility(
                    &scope,
                    vibe_core::llm::utility::UtilityFeature::SmartApprove,
                    true,
                )
            })
            .flatten()
            .map(|selection| gate::SmartGate {
                state: Arc::clone(&unified.smart),
                core: Arc::clone(&unified.core),
                classifier: vibe_harness::smart::Classifier {
                    provider: Arc::clone(&provider),
                    model: selection.model,
                    metadata: metadata.clone(),
                    session_id: unified.id.clone(),
                },
                roots: vec![cwd.to_string_lossy().into_owned()],
            });
        let provided = provided::provided(
            tools.as_ref(),
            &available,
            self.session_plugins(&unified.id).as_deref(),
        );
        let mut callables = vibe_harness::programmatic::inventory();
        callables.extend(provided.callables);
        let (context, agent_types) =
            self.unified_prompt_context(&unified.id, scope.clone(), &callables);
        let system_prompt = vibe_harness::prompt::system_prompt(&context);
        let subagents = vibe_harness::subagents::SubagentSetup {
            prompt: vibe_harness::prompt::PromptContext {
                subagents: false,
                agent_types: Vec::new(),
                ..context
            },
            agent_types: agent_types
                .iter()
                .map(|(name, overrides)| {
                    gate::declared_agent_type(name, overrides, &available, tools.as_ref())
                })
                .collect(),
        };
        let title_route = (self.session_titles_itself(&unified.id))
            .then(|| {
                self.workspace
                    .session_utility_override(
                        &scope,
                        vibe_core::llm::utility::UtilityFeature::Title,
                    )
                    .or_else(|| model.clone())
            })
            .flatten()
            .map(|title_model| {
                Arc::new(vibe_harness::title::TitleRoute {
                    provider: Arc::clone(&provider),
                    model: title_model,
                    metadata: metadata.clone(),
                })
            });
        if let Ok(mut titles) = unified.titles.lock() {
            titles.route = title_route;
        }
        let skills = self.unified_skills(&unified.id);
        // Reference `_build_unified_runtime` (`vibe/app_server/_runtime.py`)
        // and `vibe_tool_groups` (`vibe/app_server/_provided_tools.py`): the
        // question and the todo list follow the session's tool roster, the
        // scratchpad and `cron` read its filters themselves, and
        // `skills::direct_tool` offers `skill` only when a skill is.
        let filtered = {
            let mut listed = vec![
                json!({"name": "unified_harness_scratchpad"}),
                json!({"name": "cron"}),
            ];
            super::attachment::publish_session_tools(&mut listed, &enabled, &disabled);
            listed
                .into_iter()
                .filter_map(|tool| tool["name"].as_str().map(ToOwned::to_owned))
                .collect::<Vec<_>>()
        };
        // Reference `_build_unified_runtime`: one command environment serves
        // the shell tool and `process.*`, configured through the `shell` key of
        // the tool it names.
        let host = vibe_core::tools::shell::HostShells::detect();
        let command_environment = host.command_environment();
        let configured_shell = self
            .workspace
            .tool_config()
            .resolve(command_environment.config_tool())
            .text("shell");
        let configured_shell = (!configured_shell.is_empty()).then_some(configured_shell);
        let model_tools = vibe_harness::catalog::model_tools(command_environment)
            .into_iter()
            .filter(|tool| match tool.name {
                "skill" => !skills.is_empty(),
                "ask_user_question" | "todo" => available.iter().any(|name| name == tool.name),
                "unified_harness_scratchpad" | "cron" => {
                    filtered.iter().any(|name| name == tool.name)
                }
                _ => true,
            })
            .collect();
        Ok(TurnEnvironment {
            provider,
            model,
            system_prompt,
            tools: model_tools,
            metadata,
            workspace: vibe_harness::tools::files::Workspace {
                cwd: cwd.clone(),
                roots: vec![cwd.clone()],
                bounded: !auto_approve,
            },
            shell: vibe_harness::tools::shell::ShellContext {
                cwd,
                host,
                shell: configured_shell,
                env: BTreeMap::new(),
            },
            gate: gate::session_gate(
                auto_approve,
                edits,
                tools,
                available,
                smart,
                Arc::clone(&provided.routes),
            ),
            skills,
            functions: Arc::new(vibe_harness::programmatic::documents(&callables)),
            callables: Arc::new(callables),
            provided: Some(provided.routes),
            session_dir: unified
                .host
                .lock()
                .ok()
                .and_then(|host| host.store.as_ref().map(|store| store.dir().to_path_buf())),
            subagents: Some(Arc::new(subagents)),
            message_id: None,
            compact_threshold,
            max_iterations: Some(max_iterations),
        })
    }

    /// The skills a unified session reads: its plugins' model-invocable ones,
    /// by their qualified names.
    /// What the system prompt of a root session in `scope` is built from:
    /// the core's sections over the host's instructions and the session's
    /// catalogs, with the subagent types and the instructions each runs
    /// under. Reference `_build_unified_system_instructions` for the host
    /// half, with the variant table `build_vibe_code_system_instructions`
    /// reads (an unknown variant is the `cli` one).
    fn unified_prompt_context(
        &self,
        session_id: &str,
        scope: crate::workspace::SessionPromptScope,
        callables: &[vibe_harness::programmatic::Callable],
    ) -> (vibe_harness::prompt::PromptContext, AgentProfiles) {
        use vibe_harness::prompt::{
            AgentTypeEntry, KnowledgeEntry, PluginEntry, PromptContext, SkillEntry,
        };
        let workspace = &self.workspace;
        let host = crate::workspace::PromptHost::detect(&[]);
        let instructions = workspace
            .system_prompt_inputs(&scope, &host)
            .map(|mut inputs| {
                let variant = match inputs.prompt_id.as_str() {
                    "cli_2026-07_v2" | "cli_2026-08_v3" => inputs.prompt_id.clone(),
                    _ => vibe_core::system_prompt::DEFAULT_SYSTEM_PROMPT.to_owned(),
                };
                inputs.base =
                    vibe_core::system_prompt::load_system_prompt(&variant, &[]).unwrap_or_default();
                inputs.headless = false;
                inputs.include_commit_signature = false;
                inputs.include_model_info = false;
                inputs.include_prompt_detail = false;
                vibe_core::system_prompt::compose(&inputs).text()
            })
            .unwrap_or_default();
        let plugins = self.session_plugins(session_id);
        let mut context = PromptContext {
            instructions,
            background_processes: true,
            subagents: true,
            groups: vibe_harness::programmatic::namespaces(callables)
                .into_iter()
                .map(|(group, _)| group)
                .filter(|group| group != "subagent")
                .collect(),
            ..PromptContext::default()
        };
        let skills = self.unified_skills(session_id);
        let mut profile_of: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
        if let Some(plugins) = &plugins {
            let resolution = &plugins.materialized.resolution;
            context.skills = plugins
                .runtime_skills
                .iter()
                .filter(|(_, skill)| skill.model_invocable)
                .filter(|(alias, _)| skills.iter().any(|(name, _)| name == alias))
                .map(|(alias, skill)| SkillEntry {
                    name: alias.clone(),
                    description: skill.description.clone(),
                    path: skill
                        .path
                        .as_ref()
                        .map(|path| packaged_path(plugins, alias, path).display().to_string())
                        .unwrap_or_default(),
                })
                .collect();
            context
                .skills
                .sort_by(|left, right| left.name.cmp(&right.name));
            context.plugins = resolution
                .plugins
                .iter()
                .map(|plugin| PluginEntry {
                    name: plugin.name.clone(),
                    description: plugin.description.clone(),
                    path: plugin.root.display().to_string(),
                    namespaces: plugins
                        .materialized
                        .tool_groups
                        .iter()
                        .filter(|group| group.plugin_name == plugin.name)
                        .map(|group| group.name.clone())
                        .collect(),
                })
                .collect();
            context.knowledge = plugins
                .materialized
                .knowledge
                .iter()
                .map(|folder| KnowledgeEntry {
                    name: folder.name.clone(),
                    description: folder.description.clone(),
                    path: folder.runtime_root.display().to_string(),
                    access: "read_only".to_owned(),
                    plugin: Some(folder.plugin_name.clone()),
                })
                .collect();
            context.agent_types = resolution
                .agents
                .iter()
                .map(|agent| AgentTypeEntry {
                    name: agent.name.clone(),
                    description: agent.description.clone(),
                    path: agent.source_file.display().to_string(),
                })
                .collect();
            profile_of.extend(resolution.agents.iter().map(|agent| {
                let mut overrides = agent.overrides.clone();
                if let Some(instructions) = &agent.instructions {
                    overrides.insert("instructions".to_owned(), json!(instructions));
                }
                (agent.name.clone(), overrides)
            }));
        }
        let reserved: Vec<String> = context
            .agent_types
            .iter()
            .map(|agent| agent.name.clone())
            .collect();
        if let Ok(agents) = workspace.prompt_agents(&scope) {
            context.agent_types.extend(
                agents
                    .into_iter()
                    .filter(|agent| agent.kind == vibe_core::extensions::AgentKind::Subagent)
                    .filter(|agent| !reserved.contains(&agent.name))
                    .filter(|agent| {
                        !agent.name.trim().is_empty() && !agent.description.trim().is_empty()
                    })
                    .filter_map(|agent| {
                        let path = agent.path?;
                        if let Ok(Value::Object(overrides)) = serde_json::to_value(&agent.overrides)
                        {
                            profile_of.insert(agent.name.clone(), overrides);
                        }
                        Some(AgentTypeEntry {
                            name: agent.name,
                            description: agent.description,
                            path: path.display().to_string(),
                        })
                    }),
            );
        }
        let roots: Vec<SkillEntry> = self
            .workspace
            .unified_root_skills()
            .into_iter()
            .filter(|skill| skill.model_invocable)
            .filter(|skill| !context.skills.iter().any(|known| known.name == skill.name))
            .map(|skill| SkillEntry {
                name: skill.name,
                description: skill.description,
                path: skill
                    .path
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
            })
            .collect();
        context.skills.extend(roots);
        context
            .skills
            .sort_by(|left, right| left.name.cmp(&right.name));
        let agent_types = context
            .agent_types
            .iter()
            .map(|agent| {
                (
                    agent.name.clone(),
                    profile_of.remove(&agent.name).unwrap_or_default(),
                )
            })
            .collect();
        (context, agent_types)
    }

    fn unified_skills(&self, session_id: &str) -> Vec<(String, String)> {
        let mut skills: Vec<(String, String)> = self
            .session_plugins(session_id)
            .map(|plugins| {
                plugins
                    .runtime_skills
                    .iter()
                    .filter(|(_, skill)| skill.model_invocable)
                    .map(|(alias, skill)| {
                        (
                            alias.clone(),
                            vibe_core::tools::builtins::skill_payload(skill, alias),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        for skill in self.workspace.unified_root_skills() {
            if skill.model_invocable && !skills.iter().any(|(name, _)| *name == skill.name) {
                let payload = vibe_core::tools::builtins::skill_payload(&skill, &skill.name);
                skills.push((skill.name, payload));
            }
        }
        skills.sort_by(|left, right| left.0.cmp(&right.0));
        skills
    }
}

/// Who starts a turn.
pub(crate) enum TurnOrigin {
    /// A client's `turn/start`, which bumps the session.
    Client,
    /// A scheduled loop that fired.
    Scheduled { loop_id: String, fired_at: u64 },
    /// The queue, with the context entries the turn's user entry follows.
    Queued {
        item_id: String,
        context: Vec<Vec<Value>>,
    },
}

/// A turn committed and published, waiting to run.
pub(crate) struct BegunTurn {
    pub(crate) turn_id: String,
    pub(crate) session_id: String,
    pub(crate) started_at: u64,
    /// The last event before the turn's own.
    pub(crate) event_id: u64,
    pub(crate) frames: Vec<Vec<u8>>,
    pub(crate) work: UnifiedTurnWork,
}

impl AppServer {
    /// Reference `start_turn`: commits `request` as the session's next turn,
    /// publishes it, and hands back the work that runs it. A client's turn
    /// bumps the session; a scheduled loop's, named by `scheduled`, leaves a
    /// notice of its firing instead.
    pub(crate) fn begin_unified_turn(
        &self,
        unified: Arc<UnifiedSession>,
        request: TurnRequest,
        origin: TurnOrigin,
    ) -> Result<BegunTurn, ProtocolFault> {
        {
            let host = unified
                .host
                .lock()
                .map_err(|_| ProtocolFault::internal("the session lock is poisoned"))?;
            if host.running.is_some() {
                return Err(ProtocolFault::plain(
                    ProtocolErrorCode::Conflict,
                    "A turn is already running",
                ));
            }
            if !matches!(origin, TurnOrigin::Queued { .. }) && !host.queue.is_empty() {
                return Err(ProtocolFault::with_data(
                    ProtocolErrorCode::Conflict,
                    "Resume queued turns before starting a direct turn",
                    json!({"harnessCode": "turn_queue_pending"}),
                ));
            }
        }
        let pins = self.unified_pins(&unified.id);
        unified.promote();
        let (turn_id, state) = {
            let mut core = unified
                .core
                .lock()
                .map_err(|_| ProtocolFault::internal("the session lock is poisoned"))?;
            core.pins.clear();
            if let Some(model) = &pins.model {
                core.pins.insert("model".to_owned(), model.clone());
            }
            if let Some(effort) = &pins.reasoning_effort {
                core.pins
                    .insert("reasoningEffort".to_owned(), effort.clone());
            }
            match &origin {
                TurnOrigin::Queued { item_id, context } => {
                    core.begin_queued_turn(&request, item_id, context)
                }
                _ => core.begin_turn(&request),
            }
        };
        let (interrupt, interrupted) = watch::channel(None);
        let event_id = {
            let mut host = unified
                .host
                .lock()
                .map_err(|_| ProtocolFault::internal("the session lock is poisoned"))?;
            host.running = Some(turn_id.clone());
            host.interrupt = Some(interrupt);
            host.pins = pins;
            host.translator.event_id
        };
        let started_at = state
            .latest_turn
            .as_ref()
            .map_or_else(now_millis, |turn| turn.started_at);
        let mut frames = Vec::new();
        if matches!(origin, TurnOrigin::Client) {
            frames.extend(unified.bump());
        }
        frames.extend(unified.publish(&Publication {
            state,
            advance: Advance::Journaled,
            callbacks: Vec::new(),
            event: None,
        }));
        if let TurnOrigin::Scheduled { loop_id, fired_at } = &origin {
            let publication = {
                let mut core = unified
                    .core
                    .lock()
                    .map_err(|_| ProtocolFault::internal("the session lock is poisoned"))?;
                let notice = scheduled_loop_notice(&unified.id, &turn_id, loop_id, *fired_at);
                let advance = core.projector.append_entries(vec![notice], now_millis());
                core.publication(advance, None)
            };
            frames.extend(unified.publish(&publication));
        }
        let driver = unified.driver.lock().ok().and_then(|driver| driver.clone());
        let environment = self.unified_environment(
            &unified,
            driver.as_ref().and_then(|driver| driver.launch.as_ref()),
        )?;
        let delivery = driver.map_or_else(
            || CallbackDelivery {
                server: self.clone(),
                kinds: Vec::new(),
            },
            |driver| driver.delivery,
        );
        Ok(BegunTurn {
            session_id: unified.id.clone(),
            turn_id: turn_id.clone(),
            started_at,
            event_id,
            frames,
            work: UnifiedTurnWork {
                session: unified,
                turn_id,
                environment: Box::new(environment),
                interrupted,
                delivery,
                compaction: None,
            },
        })
    }
}

impl AppServer {
    /// The connection now holding `session_id` takes the frames the session
    /// raises on its own, and the session's scheduled loops start running.
    pub(crate) fn attach_unified(&self, session_id: &str, frames: mpsc::UnboundedSender<Vec<u8>>) {
        let Some(session) = self.unified_session(session_id) else {
            return;
        };
        if let Ok(mut outbox) = session.outbox.lock() {
            *outbox = Some(frames);
        }
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
                .core
                .lock()
                .ok()
                .and_then(|core| core.loops.next_due_in(vibe_core::clock::now_seconds_f64()))
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
        if session
            .host
            .lock()
            .map(|host| host.running.is_some())
            .unwrap_or(true)
        {
            continue;
        }
        if let Some(begun) = server.drain_unified_queue(&session) {
            session.send(begun.frames);
            let outbox = session.outbox.lock().ok().and_then(|outbox| outbox.clone());
            if let Some(outbox) = outbox {
                tokio::spawn(drive_unified_turn(begun.work, outbox));
            }
            continue;
        }
        let Some(loops) = session.core.lock().ok().map(|core| Arc::clone(&core.loops)) else {
            continue;
        };
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
            session.send(begun.frames);
            let outbox = session.outbox.lock().ok().and_then(|outbox| outbox.clone());
            if let Some(outbox) = outbox {
                tokio::spawn(drive_unified_turn(begun.work, outbox));
            }
        }
        loops.mark_fired(&due.id, vibe_core::clock::now_seconds_f64());
    }
}

/// Reference `_scheduled_loop_notice`: the notice a fired loop leaves in
/// its turn, stamped when the loop fired.
fn scheduled_loop_notice(session_id: &str, turn_id: &str, loop_id: &str, fired_at: u64) -> Value {
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

impl AppServer {
    /// Whether `id` names a subagent of the session `root` runs.
    pub(crate) fn unified_child_of(&self, root: &str, id: &str) -> bool {
        self.unified_session(root)
            .and_then(|session| {
                session
                    .core
                    .lock()
                    .ok()
                    .map(|core| Arc::clone(&core.subagents))
            })
            .is_some_and(|manager| manager.child_core(id).is_some())
    }

    /// [`Self::release_unified_callback`] for a callback a subagent opened.
    fn release_child_callback(&self, child_id: &str, callback_id: &str, cancel_turn: bool) {
        let managers: Vec<_> = self
            .unified
            .lock()
            .map(|sessions| {
                sessions
                    .values()
                    .filter_map(|session| {
                        session
                            .core
                            .lock()
                            .ok()
                            .map(|core| Arc::clone(&core.subagents))
                    })
                    .collect()
            })
            .unwrap_or_default();
        for manager in managers {
            if let Some(core) = manager.child_core(child_id) {
                if let Ok(mut core) = core.lock() {
                    core.callbacks.release(callback_id);
                }
                if cancel_turn {
                    manager.interrupt_child(child_id, "approval cancel_turn");
                }
                return;
            }
        }
    }
}

/// Where a turn asks its callbacks: the connection's client, for the kinds
/// it declared it answers.
#[derive(Clone)]
pub(crate) struct CallbackDelivery {
    server: AppServer,
    kinds: Vec<CallbackKind>,
}

impl CallbackDelivery {
    /// Reference `CallbackRequested`: the `callback/call` request putting
    /// `entry` to the client, when it answers that kind.
    fn request_frame(&self, entry: &Value) -> Option<Vec<u8>> {
        let kind = match entry["detail"]["kind"].as_str() {
            Some("approval") => CallbackKind::Approval,
            Some("user_input") => CallbackKind::UserInput,
            _ => return None,
        };
        if !self.kinds.contains(&kind) {
            return None;
        }
        let sequence = self.server.next_callback.fetch_add(1, Ordering::Relaxed);
        let id = RequestId::Integer(i64::try_from(sequence).unwrap_or(i64::MAX));
        self.server.client_tools.expect(id.clone());
        Some(encode_frame(&Envelope::Request(ServerRequest {
            jsonrpc: JsonRpcVersion::V2,
            id,
            method: "callback/call".to_owned(),
            params: result_map([("callback", wire_entry(entry))]),
        })))
    }
}

/// A unified turn waiting for the serve loop to run it.
pub struct UnifiedTurnWork {
    pub(crate) session: Arc<UnifiedSession>,
    pub(crate) turn_id: String,
    pub(crate) environment: Box<TurnEnvironment>,
    pub(crate) interrupted: watch::Receiver<Option<String>>,
    pub(crate) delivery: CallbackDelivery,
    /// A compaction the client asked for, run in place of a turn and
    /// answered once it ends.
    pub(crate) compaction: Option<CompactionRequest>,
}

/// A `session/compact` waiting on its compaction.
#[derive(Debug, Clone)]
pub(crate) struct CompactionRequest {
    pub(crate) request_id: RequestId,
    pub(crate) extra_instructions: String,
    pub(crate) session_log: Value,
}

impl std::fmt::Debug for UnifiedTurnWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UnifiedTurnWork")
            .field("session", &self.session.id)
            .field("turn_id", &self.turn_id)
            .finish_non_exhaustive()
    }
}

impl PartialEq for UnifiedTurnWork {
    fn eq(&self, other: &Self) -> bool {
        self.session.id == other.session.id && self.turn_id == other.turn_id
    }
}

impl Eq for UnifiedTurnWork {}

impl Clone for UnifiedTurnWork {
    fn clone(&self) -> Self {
        Self {
            session: Arc::clone(&self.session),
            turn_id: self.turn_id.clone(),
            environment: self.environment.clone(),
            interrupted: self.interrupted.clone(),
            delivery: self.delivery.clone(),
            compaction: self.compaction.clone(),
        }
    }
}

/// Runs a unified turn, sending every frame it raises to `frames`.
pub async fn drive_unified_turn(work: UnifiedTurnWork, frames: mpsc::UnboundedSender<Vec<u8>>) {
    let session = Arc::clone(&work.session);
    if let Ok(mut outbox) = session.outbox.lock() {
        *outbox = Some(frames.clone());
    }
    let sink_session = Arc::clone(&session);
    let sink_frames = frames.clone();
    let delivery = work.delivery.clone();
    let sink: vibe_harness::engine::Sink = Arc::new(move |publication: Publication| {
        for frame in sink_session.publish(&publication) {
            let _ = sink_frames.send(frame);
        }
        sink_session.tick_title(&publication);
        if let Some(CallbackEvent::Requested(entry)) = &publication.event
            && let Some(frame) = delivery.request_frame(entry)
        {
            let _ = sink_frames.send(frame);
        }
    });
    if let Some(compaction) = work.compaction {
        let end = vibe_harness::engine::compact_session(
            &session.core,
            &work.environment,
            &sink,
            &compaction.extra_instructions,
        )
        .await;
        session.settle();
        if let Ok(mut host) = session.host.lock() {
            host.running = None;
            host.interrupt = None;
        }
        let frame = match end {
            vibe_harness::engine::CompactionEnd::Compacted { summary } => {
                let state = session.peek_state(Some(super::projection::PUBLIC_HISTORY_LIMIT));
                success_bytes(
                    compaction.request_id,
                    result_map([
                        ("sessionLog", compaction.session_log),
                        ("state", state),
                        ("summary", json!(summary)),
                    ]),
                )
            }
            vibe_harness::engine::CompactionEnd::Failed { message, .. } => error_batch(
                compaction.request_id,
                ProtocolErrorCode::InternalError,
                &message,
            )
            .outbound
            .concat(),
        };
        let _ = frames.send(frame);
        return;
    }
    let end = run_turn(
        Arc::clone(&session.core),
        work.turn_id.clone(),
        *work.environment,
        sink,
        work.interrupted,
    )
    .await;
    session.settle();
    let paused = match session.host.lock() {
        Ok(mut host) => {
            host.running = None;
            host.interrupt = None;
            end != TurnEnd::Completed && host.queue.pause()
        }
        Err(_) => false,
    };
    // Reference `_handle_terminal_turn`: a turn that failed or was
    // interrupted pauses the queue rather than starting the next one.
    if paused {
        for frame in queue::publish_queue(&session) {
            let _ = frames.send(frame);
        }
    }
    session.wake();
}

fn fields_map(fields: Vec<(&str, Value)>) -> BTreeMap<String, Value> {
    fields
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

/// A mark's time as the session metadata stores it.
fn mark_time(at: Option<u64>) -> Value {
    at.map_or(Value::Null, |millis| json!(iso_micros(millis * 1000)))
}

/// Applies one of the mark methods to `meta` and `marks`, answering the
/// response fields and the session patch it publishes.
fn apply_mark(
    method: &str,
    object: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    marks: &mut SessionMarks,
) -> (MarkFields, MarkFields) {
    let now = now_millis();
    match method {
        "session/pin" => {
            let pinned = object
                .get("pinned")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            marks.pinned_at = pinned.then_some(now);
            meta.insert("pinned_at".to_owned(), mark_time(marks.pinned_at));
            (
                vec![("pinnedAt", json!(marks.pinned_at))],
                vec![("/pinnedAt", json!(marks.pinned_at))],
            )
        }
        "session/archive" => {
            let archived = object
                .get("archived")
                .and_then(Value::as_bool)
                .unwrap_or(false);
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
        _ => {
            marks.is_unseen = false;
            meta.insert("seen_at".to_owned(), mark_time(Some(now)));
            (Vec::new(), vec![("/isUnseen", json!(false))])
        }
    }
}

/// The session patch a mark method publishes, read off the marks it left.
fn apply_mark_patch(method: &str, marks: &SessionMarks) -> Vec<(&'static str, Value)> {
    match method {
        "session/pin" => vec![("/pinnedAt", json!(marks.pinned_at))],
        "session/archive" => vec![
            ("/archivedAt", json!(marks.archived_at)),
            ("/pinnedAt", json!(marks.pinned_at)),
        ],
        _ => vec![("/isUnseen", json!(marks.is_unseen))],
    }
}

/// The most recently updated listed session that is not archived.
fn latest_open(items: &[Value]) -> Option<String> {
    items
        .iter()
        .filter(|item| item["archivedAt"].is_null())
        .max_by_key(|item| {
            (
                item["updatedAt"].as_u64(),
                item["id"].as_str().map(ToOwned::to_owned),
            )
        })
        .and_then(|item| item["id"].as_str().map(ToOwned::to_owned))
}

/// The public session a listing shows for a stored unified session.
///
/// Reference `_list` prefers the projection of a session this process holds
/// open (`_live_listed_sessions`), so `live` stands in for the stored one.
pub(crate) fn listed_unified_session(
    store: &SessionStore,
    live: Option<&UnifiedSession>,
) -> Option<Value> {
    let snapshot = store.snapshot()?;
    let meta = store.read_meta().unwrap_or_default();
    let live = live.and_then(|live| {
        live.core
            .lock()
            .ok()
            .map(|core| core.projector.state.session.clone())
    });
    let session = match live {
        Some(session) => session,
        None => {
            let mut session = HarnessState::from_persisted(&snapshot, Vec::new()).session;
            store.replay_journal(&mut session);
            session
        }
    };
    let cwd = meta
        .get("environment")
        .and_then(|environment| environment.get("working_directory"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    Some(wire_session(
        &session,
        &WireContext {
            cwd,
            marks: marks_of(&meta),
            ..WireContext::default()
        },
    ))
}

impl ServerConnection {
    /// The stored unified session `session_id` names, with its directory.
    fn stored_unified(&self, session_id: &str) -> Option<SessionStore> {
        let root = self.server.unified_session_root()?;
        SessionStore::open(store::unified_root(&root).join(session_id))
    }

    /// A stored unified session as a listing shows it.
    fn listed(&self, store: &SessionStore) -> Option<Value> {
        let id = store.dir().file_name()?.to_str()?;
        listed_unified_session(store, self.server.unified_session(id).as_deref())
    }

    /// Reference `list` on the unified host: the unified sessions merged with
    /// the legacy ones, newest first.
    pub(super) fn unified_list(
        &self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let object = crate::params::object_of(&request.params);
        let limit = object
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(50, |limit| usize::try_from(limit).unwrap_or(50));
        let cwd = object.get("cwd").and_then(Value::as_str).map(|cwd| {
            vibe_core::platform::canonicalize(cwd)
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|_| cwd.to_owned())
        });
        let include_archived = object
            .get("includeArchived")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let pinned = object.get("pinned").and_then(Value::as_bool);
        let mut items: Vec<Value> = self
            .server
            .unified_session_root()
            .map(|root| store::list(&root))
            .unwrap_or_default()
            .iter()
            .filter_map(|store| self.listed(store))
            .filter(|item| {
                cwd.as_ref()
                    .is_none_or(|cwd| item["cwd"].as_str() == Some(cwd))
            })
            .collect();
        let continue_session_id = latest_open(&items).map_or(Value::Null, |id| json!(id));
        // Reference `_merged_session_list`: the sessions an older Vibe saved
        // are listed beside the unified ones, and never continued.
        items.extend(self.legacy_rows(cwd.as_deref()));
        if !include_archived {
            items.retain(|item| item["archivedAt"].is_null());
        }
        if let Some(pinned) = pinned {
            items.retain(|item| item["pinnedAt"].is_null() != pinned);
        }
        items.sort_by_key(|item| {
            std::cmp::Reverse((
                item["updatedAt"].as_u64(),
                item["id"].as_str().map(ToOwned::to_owned),
            ))
        });
        items.truncate(limit);
        Ok(success_batch(
            request.id.clone(),
            result_map([
                ("items", json!(items)),
                ("nextCursor", Value::Null),
                ("previousCursor", Value::Null),
                ("continueSessionId", continue_session_id),
            ]),
        ))
    }

    /// The session `--continue` reopens: reference `_sweep_unified_sessions`
    /// lists without a directory, so it is the latest open one anywhere.
    fn unified_continue_target(&self) -> Option<String> {
        let items: Vec<Value> = self
            .server
            .unified_session_root()
            .map(|root| store::list(&root))
            .unwrap_or_default()
            .iter()
            .filter_map(|store| self.listed(store))
            .collect();
        latest_open(&items)
    }

    /// Reference `_require_session_logging`: nothing is reopened from a store
    /// that is not kept.
    fn require_session_logging(&self) -> Result<(), ProtocolFault> {
        if self.server.workspace.session_logging().enabled {
            return Ok(());
        }
        Err(ProtocolFault::plain(
            ProtocolErrorCode::NotFound,
            "Session logging is disabled. Enable it in config to use --continue or --resume",
        ))
    }

    /// Reference `continue_latest` on the unified host: the latest session,
    /// resumed by its identifier.
    pub(super) fn unified_continue(
        &mut self,
        mut request: ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        self.require_session_logging()?;
        let target = self.unified_continue_target().ok_or_else(|| {
            ProtocolFault::plain(ProtocolErrorCode::NotFound, "No session to continue")
        })?;
        request.params.insert("sessionId".to_owned(), json!(target));
        self.unified_resume(request)
    }

    /// A port `session/start` that names a saved session: `resume` reopens it
    /// as reference `resume` does, `continue` the latest one.
    fn unified_reopen_start(
        &mut self,
        id: vibe_protocol::RequestId,
        params: &SessionStartParams,
        mut start: BTreeMap<String, Value>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        self.require_session_logging()?;
        let session_id = match params.resume.as_deref() {
            Some(selector) => selector.to_owned(),
            None => self.unified_continue_target().ok_or_else(|| {
                ProtocolFault::plain(ProtocolErrorCode::NotFound, "No session to continue")
            })?,
        };
        let target = if self.stored_unified(&session_id).is_none()
            && let Some(legacy) = self.legacy_session(&session_id)
        {
            self.import_legacy(&legacy)?
        } else {
            session_id
        };
        for key in ["resume", "continue", "cwd", "workingDirectory", "sessionId"] {
            start.remove(key);
        }
        let reopened = self.reopen_with(&target, usize::from(params.history_limit), start)?;
        let mut batch = success_batch(
            id,
            result_map([
                ("lastEventId", reopened.state["eventId"].clone()),
                ("state", reopened.state),
            ]),
        );
        batch.outbound.extend(reopened.frames);
        batch.deferred.extend(reopened.deferred);
        Ok(batch)
    }

    /// Reference `resume` on the unified host.
    pub(super) fn unified_resume(
        &mut self,
        request: ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let object = crate::params::object_of(&request.params);
        let session_id = object
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let history_limit = object
            .get("historyLimit")
            .and_then(Value::as_u64)
            .map_or(200, |limit| usize::try_from(limit).unwrap_or(200));
        if let Some(live) = self.server.unified_session(&session_id)
            && self.root_id().as_deref() == Some(session_id.as_str())
        {
            // Reference resume reattaches a fresh runtime, whose accounting
            // starts over from what the session stored.
            let opening = self
                .server
                .lock_sessions()
                .ok()
                .and_then(|sessions| sessions.get(&session_id).map(opening_stats));
            if let Some(mut stats) = opening
                && let (Ok(core), Ok(mut host)) = (live.core.lock(), live.host.lock())
            {
                stats.seed(&wire_state(&core.projector.state, &WireContext::default()));
                host.translator.stats = stats;
            }
            let state = live.read_state(Some(history_limit));
            return Ok(success_batch(
                request.id,
                result_map([("lastEventId", state["eventId"].clone()), ("state", state)]),
            ));
        }
        let agent_config = object
            .get("agentConfig")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let target = if self.stored_unified(&session_id).is_none()
            && let Some(legacy) = self.legacy_session(&session_id)
        {
            self.import_legacy(&legacy)?
        } else {
            session_id
        };
        let reopened = self.reopen_unified(&target, history_limit, &agent_config)?;
        let mut batch = success_batch(
            request.id,
            result_map([
                ("lastEventId", reopened.state["eventId"].clone()),
                ("state", reopened.state),
            ]),
        );
        batch.outbound.extend(reopened.frames);
        batch.deferred.extend(reopened.deferred);
        Ok(batch)
    }

    /// Reference `import_legacy`: a legacy transcript copied into a new
    /// unified session, which the legacy files outlive untouched. Answers the
    /// new session's identifier.
    fn import_legacy(&self, legacy: &HydratedSession) -> Result<String, ProtocolFault> {
        let root = self.server.unified_session_root().ok_or_else(|| {
            ProtocolFault::internal("sessions are not saved, so none can be imported")
        })?;
        let session_id = vibe_core::session_id::generate_session_id(None);
        let now = now_millis();
        let conversation = legacy
            .messages
            .iter()
            .filter_map(|message| match message {
                vibe_core::events::ModelMessage::User {
                    content, injected, ..
                } => Some(vibe_harness::engine::ConversationMessage::User {
                    content: vec![json!({"type": "text", "text": content})],
                    injected: *injected,
                }),
                vibe_core::events::ModelMessage::Assistant {
                    content,
                    reasoning,
                    tool_calls,
                    ..
                } => Some(vibe_harness::engine::ConversationMessage::Assistant {
                    text: content.clone(),
                    reasoning: reasoning.clone(),
                    tool_calls: tool_calls.clone(),
                }),
                vibe_core::events::ModelMessage::Tool {
                    call_id,
                    name,
                    content,
                    ..
                } => Some(vibe_harness::engine::ConversationMessage::Tool {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    content: content.clone(),
                }),
                vibe_core::events::ModelMessage::System { .. } => None,
            })
            .collect();
        // Reference `_active_model_from_metadata`.
        let model = legacy
            .metadata
            .config
            .get("active_model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .map(ToOwned::to_owned);
        let scratchpad = vibe_harness::tools::vibe::scratchpad_dir(&root, &session_id);
        let core =
            vibe_harness::engine::import_core(&session_id, now, scratchpad, conversation, model);
        let cwd = legacy
            .metadata
            .environment
            .get("working_directory")
            .cloned()
            .flatten()
            .unwrap_or_default();
        let meta = store::new_meta(
            &session_id,
            None,
            &iso_millis(now),
            &cwd,
            &store::username(),
        );
        SessionStore::create_imported(store::unified_root(&root).join(&session_id), &meta, &core)
            .map_err(|error| ProtocolFault::internal(format!("the import cannot be saved: {error}")))?;
        Ok(session_id)
    }

    /// Opens the stored unified session `session_id` as this connection's
    /// root.
    fn reopen_unified(
        &mut self,
        session_id: &str,
        history_limit: usize,
        agent_config: &Map<String, Value>,
    ) -> Result<Reopened, ProtocolFault> {
        self.reopen_with(
            session_id,
            history_limit,
            super::connection::reopen_params(agent_config),
        )
    }

    /// Opens the stored unified session `session_id` as this connection's
    /// root, with the port-shaped start parameters `params`.
    fn reopen_with(
        &mut self,
        session_id: &str,
        history_limit: usize,
        mut params: BTreeMap<String, Value>,
    ) -> Result<Reopened, ProtocolFault> {
        let stored = self.stored_unified(session_id).ok_or_else(|| {
            ProtocolFault::plain(
                ProtocolErrorCode::NotFound,
                format!("Session not found: {session_id}"),
            )
        })?;
        let meta = stored.read_meta().unwrap_or_default();
        let root = self.server.unified_session_root();
        let scratchpad = vibe_harness::tools::vibe::scratchpad_dir(
            root.as_deref().unwrap_or_else(|| Path::new(".")),
            session_id,
        );
        let core = stored
            .load(scratchpad)
            .ok_or_else(|| ProtocolFault::internal("the session store cannot be read"))?;
        let cwd = meta
            .get("environment")
            .and_then(|environment| environment.get("working_directory"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if let Some(cwd) = cwd {
            params.entry("cwd".to_owned()).or_insert(json!(cwd));
        }
        params.insert("sessionId".to_owned(), json!(session_id));
        if core.without_plugins {
            self.server.bind_no_plugins(session_id);
        }
        let mut start = from_params::<SessionStartParams>(&params)?;
        let mut resolution = self.resolve_worktree(&mut start)?;
        let opened = self.open_resolved_session(&start, &mut resolution);
        let (opened_id, _, mcp_configs) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                self.undo_worktree(&resolution);
                return Err(error);
            }
        };
        let pins = core.pins.clone();
        let session = self.open_unified(&opened_id, Some((stored, core, meta)))?;
        if let Ok(mut host) = session.host.lock() {
            host.pins = SessionPins {
                model: pins.get("model").cloned(),
                reasoning_effort: pins.get("reasoningEffort").cloned(),
                agent: None,
            };
        }
        let state = session.read_state(Some(history_limit));
        let frames = self.runtime_updated_frame(&opened_id).into_iter().collect();
        let mut deferred = vec![DeferredWork::AttachUnified {
            session_id: opened_id.clone(),
        }];
        if !mcp_configs.is_empty() {
            deferred.push(DeferredWork::ConfigureMcp {
                session_id: opened_id,
                configs: mcp_configs,
            });
        }
        Ok(Reopened {
            state,
            frames,
            deferred,
        })
    }

    /// Reference `fork` on the unified host: the source's conversation is
    /// imported into a new stored session, which becomes the root unless the
    /// client asked for a detached copy.
    pub(super) fn unified_fork(
        &mut self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let object = crate::params::object_of(&request.params);
        let source_id = object
            .get("sourceSessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let history_limit = object
            .get("historyLimit")
            .and_then(Value::as_u64)
            .map_or(200, |limit| usize::try_from(limit).unwrap_or(200));
        let attach = object
            .get("attach")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let root = self.server.unified_session_root().ok_or_else(|| {
            ProtocolFault::internal("sessions are not saved, so none can be forked")
        })?;
        let session_id = vibe_core::session_id::generate_session_id(None);
        let now = now_millis();
        let scratchpad = vibe_harness::tools::vibe::scratchpad_dir(&root, &session_id);
        let entry_id = object
            .get("entryId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let fork = |source: &vibe_harness::engine::SessionCore| match &entry_id {
            None => Ok(vibe_harness::engine::fork_core(
                source,
                &session_id,
                now,
                scratchpad.clone(),
            )),
            Some(entry_id) => vibe_harness::engine::fork_core_through(
                source,
                &session_id,
                now,
                scratchpad.clone(),
                entry_id,
            )
            .map_err(|message| {
                ProtocolFault::with_data(
                    ProtocolErrorCode::InvalidParams,
                    message,
                    json!({
                        "details": {"entry_id": entry_id},
                        "harnessCode": "fork_entry_not_found",
                    }),
                )
            }),
        };
        let (core, cwd) = if let Some(live) = self.server.unified_session(&source_id) {
            let source = live
                .core
                .lock()
                .map_err(|_| ProtocolFault::internal("the session lock is poisoned"))?;
            (fork(&source)?, live.cwd.clone())
        } else {
            let stored = self.stored_unified(&source_id).ok_or_else(|| {
                ProtocolFault::plain(
                    ProtocolErrorCode::NotFound,
                    format!("Session not found: {source_id}"),
                )
            })?;
            let source = stored
                .load(PathBuf::new())
                .ok_or_else(|| ProtocolFault::internal("the session store cannot be read"))?;
            let cwd = stored
                .read_meta()
                .unwrap_or_default()
                .get("environment")
                .and_then(|environment| environment.get("working_directory"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .unwrap_or_default();
            (fork(&source)?, cwd)
        };
        let meta = store::new_meta(
            &session_id,
            Some(&source_id),
            &iso_millis(now),
            &cwd,
            &store::username(),
        );
        SessionStore::create(store::unified_root(&root).join(&session_id), &meta, &core).map_err(
            |error| ProtocolFault::internal(format!("the fork cannot be saved: {error}")),
        )?;
        let reopened = if attach {
            self.reopen_unified(&session_id, history_limit, &Map::new())?
        } else {
            Reopened {
                state: self.unified_state(&session_id, Some(history_limit))?,
                frames: Vec::new(),
                deferred: Vec::new(),
            }
        };
        let mut batch = success_batch(
            request.id.clone(),
            result_map([
                ("lastEventId", reopened.state["eventId"].clone()),
                ("sourceSessionId", json!(source_id)),
                ("state", reopened.state),
            ]),
        );
        batch.outbound.extend(reopened.frames);
        batch.deferred.extend(reopened.deferred);
        Ok(batch)
    }

    /// Reference `clear_history` on the unified host: the root continues as a
    /// new, unsaved session, and the reply carries the history it leaves
    /// behind closed by a `clear` checkpoint.
    pub(super) fn unified_clear(
        &mut self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let object = crate::params::object_of(&request.params);
        let session_id = object
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some(key) = self.root_key() else {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                super::connection::NO_ROOT,
            ));
        };
        if self.root_id().as_deref() != Some(session_id.as_str()) {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::NotFound,
                format!("Session not found: {session_id}"),
            ));
        }
        let source = self.server.unified_session(&session_id).ok_or_else(|| {
            ProtocolFault::plain(
                ProtocolErrorCode::NotFound,
                format!("Session not found: {session_id}"),
            )
        })?;
        if source
            .host
            .lock()
            .map(|host| host.running.is_some())
            .unwrap_or(true)
        {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                "Cannot clear history while a turn is active",
            ));
        }
        let carried = source.read_state(Some(super::projection::PUBLIC_HISTORY_LIMIT))["history"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let new_id = vibe_core::session_id::generate_session_id(None);
        self.server.transfer_lease(&session_id, &new_id)?;
        if let Ok(mut sessions) = self.server.unified.lock() {
            sessions.remove(&session_id);
        }
        let now = now_millis();
        self.server.rekey_session(&key, &new_id)?;
        {
            let mut sessions = self.server.lock_sessions()?;
            self.root = Some(new_id.clone());
            if self.attached_sessions.remove(&key) {
                self.attached_sessions.insert(new_id.clone());
            }
            if let Some(session) = sessions.get_mut(&new_id) {
                session.created_at = now;
                session.updated_at = now;
                session.bumped_at = None;
                session.event_watermark = 0;
                session.stats = crate::server::runtime::SessionStats::default();
                session.turns.clear();
                session.latest_turn = None;
                session.status = SessionStatus::Idle;
            }
        }
        let session = self.open_unified(&new_id, None)?;
        let mut state = session.read_state(Some(super::projection::PUBLIC_HISTORY_LIMIT));
        let mut history: Vec<Value> = carried
            .into_iter()
            .map(|mut entry| {
                entry["sessionId"] = json!(new_id);
                entry
            })
            .collect();
        history.push(
            serde_json::to_value(super::projection::checkpoint_entry(
                &new_id,
                "clear",
                "New conversation started",
                Value::Null,
            ))
            .unwrap_or(Value::Null),
        );
        let excess = history
            .len()
            .saturating_sub(super::projection::PUBLIC_HISTORY_LIMIT);
        history.drain(..excess);
        state["history"] = Value::Array(history);
        state["historyBeforeCursor"] = Value::Null;
        Ok(success_batch(
            request.id.clone(),
            result_map([
                ("state", state),
                ("sessionLog", self.server.session_log_summary(&new_id)),
            ]),
        ))
    }

    /// Reference `rename`, `pin`, `archive` and `mark_as_seen` on the
    /// unified host: an open session publishes what changed, a stored one is
    /// rewritten in place.
    pub(super) fn unified_mark(
        &mut self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let object = crate::params::object_of(&request.params);
        let session_id = object
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let method = request.method.as_str();
        if method == "session/rename" {
            return self.unified_rename(request, &session_id, &object);
        }
        if let Some(live) = self.server.unified_session(&session_id) {
            let mut fields = Vec::new();
            let (_, frame) = live.remark(
                |meta, marks| {
                    let (response, patch) = apply_mark(method, &object, meta, marks);
                    fields = response;
                    let _ = patch;
                },
                |marks| apply_mark_patch(method, marks),
            );
            let mut batch = success_batch(request.id.clone(), fields_map(fields));
            batch.outbound.splice(0..0, frame);
            return Ok(batch);
        }
        let stored = self.stored_unified(&session_id).ok_or_else(|| {
            ProtocolFault::plain(
                ProtocolErrorCode::NotFound,
                format!("Session not found: {session_id}"),
            )
        })?;
        let mut meta = stored.read_meta().unwrap_or_default();
        let mut marks = marks_of(&meta);
        let (fields, _) = apply_mark(method, &object, &mut meta, &mut marks);
        stored
            .write_meta(&meta)
            .map_err(|error| ProtocolFault::internal(error.to_string()))?;
        Ok(success_batch(request.id.clone(), fields_map(fields)))
    }

    /// Reference `rename`: the harness retitles the session, as its owner
    /// chose.
    fn unified_rename(
        &mut self,
        request: &ServerRequest,
        session_id: &str,
        object: &Map<String, Value>,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let title = object
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
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
                let mut core = live
                    .core
                    .lock()
                    .map_err(|_| ProtocolFault::internal("the session lock is poisoned"))?;
                let advance = core.projector.rename(&title, "manual", now_millis());
                (core.publication(advance, None), core.projector.watermark)
            };
            let frames = live.publish(&publication);
            if let Ok(mut host) = live.host.lock() {
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
        let mut stored = self.stored_unified(session_id).ok_or_else(|| {
            ProtocolFault::plain(
                ProtocolErrorCode::NotFound,
                format!("Session not found: {session_id}"),
            )
        })?;
        let mut core = stored
            .load(PathBuf::new())
            .ok_or_else(|| ProtocolFault::internal("the session store cannot be read"))?;
        let _ = core.projector.rename(&title, "manual", now_millis());
        stored
            .commit(&core, false)
            .map_err(|error| ProtocolFault::internal(error.to_string()))?;
        let mut meta = stored.read_meta().unwrap_or_default();
        meta.insert("title".to_owned(), json!(title));
        meta.insert("title_source".to_owned(), json!("manual"));
        let _ = stored.write_meta(&meta);
        Ok(success_batch(request.id.clone(), respond(0)))
    }

    /// Reference `read` on the unified host.
    pub(super) fn unified_read(
        &self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let object = crate::params::object_of(&request.params);
        let session_id = object
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let history_limit = object
            .get("history")
            .and_then(|history| history.get("limit"))
            .and_then(Value::as_u64)
            .map(|limit| usize::try_from(limit).unwrap_or(usize::MAX));
        let state = self.unified_state(&session_id, history_limit)?;
        Ok(success_batch(
            request.id.clone(),
            result_map([("lastEventId", state["eventId"].clone()), ("state", state)]),
        ))
    }

    /// The directory the unified session `session_id` runs in, live or
    /// stored.
    fn unified_cwd(&self, session_id: &str) -> Option<String> {
        if let Some(live) = self.server.unified_session(session_id) {
            return Some(live.cwd.clone());
        }
        self.stored_unified(session_id)?
            .read_meta()?
            .get("environment")?
            .get("working_directory")?
            .as_str()
            .map(ToOwned::to_owned)
    }

    /// The public state of a unified session, live or stored, with at most
    /// `history_limit` of its latest history entries.
    pub(super) fn unified_state(
        &self,
        session_id: &str,
        history_limit: Option<usize>,
    ) -> Result<Value, ProtocolFault> {
        if let Some(live) = self.server.unified_session(session_id) {
            return Ok(live.read_state(history_limit));
        }
        let Some(stored) = self.stored_unified(session_id) else {
            return self.legacy_state(session_id, history_limit).ok_or_else(|| {
                ProtocolFault::plain(
                    ProtocolErrorCode::NotFound,
                    format!("Session not found: {session_id}"),
                )
            });
        };
        let core = stored
            .load(PathBuf::new())
            .ok_or_else(|| ProtocolFault::internal("the session store cannot be read"))?;
        let meta = stored.read_meta().unwrap_or_default();
        let session = &core.projector.state.session;
        // A child keeps no metadata of its own: it runs in its root's
        // directory.
        let cwd = meta
            .get("environment")
            .and_then(|environment| environment.get("working_directory"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| {
                (session.root_session_id != session.id)
                    .then(|| self.unified_cwd(&session.root_session_id))
                    .flatten()
            });
        Ok(wire_state(
            &core.projector.state,
            &WireContext {
                cwd,
                event_id: core.projector.watermark,
                history_limit,
                marks: marks_of(&meta),
                pins: SessionPins {
                    model: core.pins.get("model").cloned(),
                    reasoning_effort: core.pins.get("reasoningEffort").cloned(),
                    agent: None,
                },
                worktree: None,
                child_sessions: Vec::new(),
            },
        ))
    }
}

/// Where a plugin skill's file sits inside the package the session mounted:
/// the reference lists a skill under its plugin's package root, which is
/// where the session reads it from, rather than where it was installed.
fn packaged_path(plugins: &crate::plugins::SessionPlugins, alias: &str, path: &Path) -> PathBuf {
    let namespace = alias
        .split_once(':')
        .map_or(alias, |(namespace, _)| namespace);
    plugins
        .materialized
        .resolution
        .plugins
        .iter()
        .filter(|plugin| plugin.namespace == namespace)
        .find_map(|plugin| {
            let installed = plugins.installed_roots.get(&plugin.name)?;
            let relative = path.strip_prefix(installed).ok()?;
            Some(plugin.root.join(relative))
        })
        .unwrap_or_else(|| path.to_path_buf())
}
