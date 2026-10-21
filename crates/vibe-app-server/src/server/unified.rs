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
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, watch};
use vibe_core::clock::{now_micros, now_millis};
use vibe_core::time::{format_iso_micros, format_iso_millis, parse_iso_millis};
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
use marks::marks_of;

mod environment;
mod gate;
mod loops;
mod marks;
mod params;
mod prepare;
mod provided;
mod queue;
mod reopen;
mod rewind;
mod stored;
mod titles;
mod turns;

pub(crate) use turns::{BegunTurn, TurnOrigin};

/// One unified session this process holds open.
pub(crate) struct UnifiedSession {
    pub(crate) id: String,
    pub(crate) cwd: String,
    /// The harness state, which a turn locks on its own as it runs.
    core: Arc<Mutex<SessionCore>>,
    host: Mutex<UnifiedHost>,
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

impl UnifiedHost {
    /// Refuses while a turn or a compaction runs.
    fn require_idle(&self) -> Result<(), ProtocolFault> {
        match self.running {
            Some(_) => Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                "A turn is already running",
            )),
            None => Ok(()),
        }
    }

    /// Reference `_require_active_turn`: the turn running is the one the
    /// client expects, and `stale` words the refusal of another one.
    fn require_turn(&self, expected: &str, stale: &str) -> Result<(), ProtocolFault> {
        match self.running.as_deref() {
            None => Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                "No active turn",
            )),
            Some(active) if active != expected => Err(ProtocolFault::with_data(
                ProtocolErrorCode::StaleTurn,
                stale,
                json!({"activeTurnId": active}),
            )),
            Some(_) => Ok(()),
        }
    }

    /// The turn or compaction that ran is over.
    fn end_turn(&mut self) {
        self.running = None;
        self.interrupt = None;
    }
}

impl UnifiedSession {
    /// The harness state, locked under the policy of [`lock`]. When both are
    /// held, the core is taken before the host.
    pub(crate) fn core(&self) -> MutexGuard<'_, SessionCore> {
        lock(&self.core)
    }

    /// What the host keeps beside the harness state, locked under the policy
    /// of [`lock`].
    pub(crate) fn host(&self) -> MutexGuard<'_, UnifiedHost> {
        lock(&self.host)
    }

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
            let mut core = self.core();
            core.refresh_processes();
            core.publication(Advance::Journaled, None)
        };
        let frames = self.publish(&publication);
        self.send(frames);
    }

    /// Reference `_translate_child_event` over a state the child
    /// published: the parent's summaries of its children are republished
    /// where they changed.
    ///
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
            let mut host = self.host();
            host.children.sessions.insert(child_id.to_owned(), session);
            Self::child_events(&mut host)
        };
        if let Some(CallbackEvent::Requested(entry)) = &publication.event
            && let Some(frame) = lock(&self.driver)
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
        if let Some(outbox) = lock(&self.outbox).as_ref() {
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
        let core = self.core();
        let mut host = self.host();
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
        let core = self.core();
        let host = self.host();
        let event_id = host.translator.event_id.max(core.projector.watermark);
        wire_state(
            &core.projector.state,
            &self.context(&host, event_id, true, history_limit),
        )
    }

    /// Reference `_session_log_summary`: where the session is saved, once it
    /// is, and whether it still waits for its first title.
    pub(crate) fn session_log(&self, enabled: bool) -> Value {
        let title = self.core().projector.state.session.title.clone();
        let path = self
            .host()
            .store
            .as_ref()
            .map(|store| store.dir().to_string_lossy().into_owned());
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
        let mut host = self.host();
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
            host.meta.insert(
                "unseen_at".to_owned(),
                json!(format_iso_micros(now_micros())),
            );
            host.meta.insert("seen_at".to_owned(), Value::Null);
            if let Some(store) = &host.store {
                let _ = store.write_meta(&host.meta);
            }
        }
        frames.extend(notifications.into_iter().map(encode));
        frames.extend(Self::child_events(&mut host));
        frames
    }

    /// Reference `_persist_accepted_user_activity_metadata`: the session is
    /// bumped, and the bump published.
    fn bump(&self) -> Option<Vec<u8>> {
        let title = self.title_meta();
        let mut host = self.host();
        host.meta.extend(title);
        let micros = now_micros();
        let millis = micros / 1000;
        host.marks.bumped_at = Some(millis);
        host.meta
            .insert("bumped_at".to_owned(), json!(format_iso_micros(micros)));
        if let Some(store) = &host.store {
            let _ = store.write_meta(&host.meta);
        }
        host.translator
            .publish_session_patch(&[("/bumpedAt", json!(millis))])
            .map(encode)
    }

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
            let mut core = self.core();
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

    /// Writes the session to disk the first time it takes a turn
    /// (reference "promotion").
    fn promote(&self) {
        let Some(root) = &self.session_root else {
            return;
        };
        let core = self.core();
        let mut host = self.host();
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
        title_fields(&self.core())
    }

    /// Writes a generation once the session settles.
    fn settle(&self) {
        let core = self.core();
        let mut host = self.host();
        let end = format_iso_millis(core.projector.state.session.updated_at);
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

/// The directory a session's metadata records it runs in.
fn meta_cwd(meta: &Map<String, Value>) -> Option<String> {
    meta.get("environment")
        .and_then(|environment| environment.get("working_directory"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

/// What a stored session runs on, as its last turn pinned it.
fn stored_pins(core: &SessionCore) -> SessionPins {
    SessionPins {
        model: core.pins.get("model").cloned(),
        reasoning_effort: core.pins.get("reasoningEffort").cloned(),
        agent: None,
    }
}

/// The refusal of a session the unified host does not know.
fn unknown_session(session_id: &str) -> ProtocolFault {
    ProtocolFault::plain(
        ProtocolErrorCode::NotFound,
        format!("Session not found: {session_id}"),
    )
}

fn encode(notification: Notification) -> Vec<u8> {
    let params = match notification.params {
        Value::Object(map) => map.into_iter().collect(),
        _ => BTreeMap::new(),
    };
    encode_notification(notification.method, params)
}

impl AppServer {
    pub(crate) fn unified_session(&self, session_id: &str) -> Option<Arc<UnifiedSession>> {
        lock(&self.unified).get(session_id).cloned()
    }

    fn insert_unified(&self, session: Arc<UnifiedSession>) {
        {
            let core = session.core();
            let weak = Arc::downgrade(&session);
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
        lock(&self.unified).insert(session.id.clone(), session);
    }

    /// Lets go of the unified session `session_id` names, if this process
    /// holds it.
    fn remove_unified(&self, session_id: &str) -> Option<Arc<UnifiedSession>> {
        lock(&self.unified).remove(session_id)
    }

    /// Where unified sessions are saved, when they are.
    fn unified_session_root(&self) -> Option<PathBuf> {
        self.workspace.logged_session_root()
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

impl ServerConnection {
    /// Where a turn this connection drives asks its callbacks.
    fn callback_delivery(&self) -> CallbackDelivery {
        CallbackDelivery {
            server: self.server.clone(),
            kinds: self.capabilities.callback_kinds.clone(),
        }
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
    *lock(&session.outbox) = Some(frames.clone());
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
        session.host().end_turn();
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
    let paused = {
        let mut host = session.host();
        host.end_turn();
        end != TurnEnd::Completed && host.queue.pause()
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
