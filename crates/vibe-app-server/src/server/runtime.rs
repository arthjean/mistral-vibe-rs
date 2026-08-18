//! A session as the server holds it, and the frames that publish it.
//!
//! [`SessionRuntime`] is the whole of what the server knows about one session:
//! its identity and aliases, the intent it runs under, the projection a client
//! reads, the callback it may be blocked on, and its accounting. Everything a
//! client sees of a session is composed from this.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Running,
    WaitingCallback,
    Cancelled,
    Failed,
    Closed,
}

#[derive(Clone)]
pub(crate) struct SessionRuntime {
    pub(crate) id: String,
    pub(crate) working_directory: String,
    pub(crate) intent: SessionIntent,
    pub(crate) status: SessionStatus,
    pub(crate) active_turn: Option<String>,
    pub(crate) active_turn_started_at: Option<u64>,
    pub(crate) active_scheduled_loop: Option<String>,
    pub(crate) compaction_pending: bool,
    pub(crate) pending_callback: Option<PendingCallback>,
    pub(crate) resolved_callbacks: BTreeMap<String, ResolvedCallback>,
    pub(crate) context: Vec<String>,
    pub(crate) steering: Vec<String>,
    pub(crate) snapshot: Option<ProjectionSnapshot>,
    pub(crate) attachments: u32,
    pub(crate) resource_generation: u64,
    pub(crate) aliases: BTreeSet<String>,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) latest_turn: Option<PublicTurn>,
    /// The agent profile this session runs, projected as `AgentSummary`.
    ///
    /// The intent carries the name; a client renders the profile, so the
    /// summary is composed where the profile is resolved rather than looked up
    /// again at every projection.
    pub(crate) agent_summary: Option<Value>,
    pub(crate) event_watermark: u64,
    pub(crate) stats: SessionStats,
    /// The active model's compaction threshold, read once when the session
    /// opens. Zero means no model declares one.
    pub(crate) context_window: u64,
    /// The five compaction keys, read once when the session opens, beside the
    /// threshold the client renders. A turn carries them to the engine, where
    /// the policy layer and the reactive recovery both read them.
    pub(crate) compaction: CompactionSettings,
    pub(crate) policy: PermissionStore,
    pub(crate) tools: ToolRegistry,
    pub(crate) persisted: Option<HydratedSession>,
    pub(crate) review: Option<Arc<ReviewManager>>,
}

impl SessionRuntime {
    /// A freshly attached, idle session holding one attachment.
    ///
    /// Callers set the fields that vary by entry point: `persisted`,
    /// `snapshot`, `aliases` and `updated_at`.
    pub(crate) fn new(
        id: String,
        working_directory: String,
        intent: SessionIntent,
        policy: PermissionStore,
        tools: ToolRegistry,
        review: Option<Arc<ReviewManager>>,
        created_at: u64,
    ) -> Self {
        Self {
            id,
            working_directory,
            intent,
            status: SessionStatus::Idle,
            active_turn: None,
            active_turn_started_at: None,
            active_scheduled_loop: None,
            compaction_pending: false,
            pending_callback: None,
            resolved_callbacks: BTreeMap::new(),
            context: Vec::new(),
            steering: Vec::new(),
            snapshot: None,
            agent_summary: None,
            attachments: 1,
            resource_generation: 1,
            aliases: BTreeSet::new(),
            created_at,
            updated_at: created_at,
            latest_turn: None,
            event_watermark: 0,
            stats: SessionStats::default(),
            context_window: 0,
            compaction: CompactionSettings::default(),
            policy,
            tools,
            persisted: None,
            review,
        }
    }
}

/// The token and tool accounting one session publishes.
///
/// The reference keeps this on the agent loop and projects it into
/// `AgentStatsSnapshot`; here it lives on the session because the loop runs in
/// a driver the server does not own. The tool counters are derived from the
/// projected history rather than counted twice, so a replayed snapshot and a
/// live turn report the same numbers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SessionStats {
    pub(crate) session_prompt_tokens: u64,
    pub(crate) session_completion_tokens: u64,
    pub(crate) session_cached_tokens: u64,
    pub(crate) context_tokens: u64,
    pub(crate) last_turn_prompt_tokens: u64,
    pub(crate) last_turn_completion_tokens: u64,
    pub(crate) last_turn_cached_tokens: u64,
    pub(crate) last_turn_duration_ms: u64,
    /// Where the running turn started from, so its own usage is the difference.
    pub(crate) turn_baseline_prompt_tokens: u64,
    pub(crate) turn_baseline_completion_tokens: u64,
    pub(crate) turn_baseline_cached_tokens: u64,
}

impl SessionStats {
    /// Records the usage one provider round trip reported.
    pub(crate) fn observe(&mut self, context_tokens: u64, input_tokens: u64, output_tokens: u64) {
        self.context_tokens = context_tokens;
        self.session_prompt_tokens = input_tokens;
        self.session_completion_tokens = output_tokens;
        self.last_turn_prompt_tokens =
            input_tokens.saturating_sub(self.turn_baseline_prompt_tokens);
        self.last_turn_completion_tokens =
            output_tokens.saturating_sub(self.turn_baseline_completion_tokens);
        self.last_turn_cached_tokens = self
            .session_cached_tokens
            .saturating_sub(self.turn_baseline_cached_tokens);
    }

    /// Opens a turn: what follows counts against it rather than the session.
    pub(crate) fn begin_turn(&mut self) {
        self.turn_baseline_prompt_tokens = self.session_prompt_tokens;
        self.turn_baseline_completion_tokens = self.session_completion_tokens;
        self.turn_baseline_cached_tokens = self.session_cached_tokens;
        self.last_turn_prompt_tokens = 0;
        self.last_turn_completion_tokens = 0;
        self.last_turn_cached_tokens = 0;
        self.last_turn_duration_ms = 0;
    }
}

/// The 17-field snapshot `AgentStatsSnapshot` declares.
///
/// A session with no completed turn reports zeros rather than omitting the
/// last-turn fields, because a client renders them as numbers either way, and
/// so does a session the registry no longer holds.
pub(crate) fn public_stats(session: Option<&SessionRuntime>) -> Value {
    let history = session
        .and_then(|session| session.snapshot.as_ref())
        .map(|snapshot| snapshot.history.as_slice())
        .unwrap_or_default();
    let mut steps = 0_u64;
    let mut succeeded = 0_u64;
    let mut failed = 0_u64;
    let mut agreed = 0_u64;
    let mut rejected = 0_u64;
    for entry in history {
        match entry {
            PublicHistoryEntry::Message {
                role: PublicMessageRole::Assistant,
                ..
            } => steps = steps.saturating_add(1),
            PublicHistoryEntry::Effect { state, .. } => match state {
                PublicEffectState::Completed { .. } => succeeded = succeeded.saturating_add(1),
                PublicEffectState::Failed { .. } => failed = failed.saturating_add(1),
                _ => {}
            },
            PublicHistoryEntry::Callback { state, .. } => match state {
                PublicCallbackState::Answered { .. } => agreed = agreed.saturating_add(1),
                PublicCallbackState::Cancelled { .. } | PublicCallbackState::Expired { .. } => {
                    rejected = rejected.saturating_add(1);
                }
                PublicCallbackState::Open => {}
            },
            _ => {}
        }
    }
    let owned;
    let stats = match session {
        Some(session) => &session.stats,
        None => {
            owned = SessionStats::default();
            &owned
        }
    };
    let seconds = as_f64(stats.last_turn_duration_ms) / 1_000.0;
    let tokens_per_second = if seconds > 0.0 {
        as_f64(stats.last_turn_completion_tokens) / seconds
    } else {
        0.0
    };
    json!({
        "steps": steps,
        "sessionPromptTokens": stats.session_prompt_tokens,
        "sessionCompletionTokens": stats.session_completion_tokens,
        "sessionCachedTokens": stats.session_cached_tokens,
        "inputPricePerMillion": 0.0,
        "outputPricePerMillion": 0.0,
        "cachedInputPricePerMillion": null,
        "toolCallsAgreed": agreed,
        "toolCallsRejected": rejected,
        "toolCallsFailed": failed,
        "toolCallsSucceeded": succeeded,
        "contextTokens": stats.context_tokens,
        "lastTurnPromptTokens": stats.last_turn_prompt_tokens,
        "lastTurnCompletionTokens": stats.last_turn_completion_tokens,
        "lastTurnCachedTokens": stats.last_turn_cached_tokens,
        "lastTurnDuration": seconds,
        "tokensPerSecond": tokens_per_second,
    })
}

/// Widens a counter for the float fields the wire declares, saturating rather
/// than losing precision silently on a value no session reaches.
pub(crate) fn as_f64(value: u64) -> f64 {
    u32::try_from(value).map_or(f64::from(u32::MAX), f64::from)
}

/// Publishes a fatal server-side problem as `error`.
///
/// The reference sends one before it drops a client whose background work
/// raised, so the client learns why the stream stopped instead of only that it
/// did.
pub(crate) fn server_error_frame(message: &str) -> Vec<u8> {
    encode_notification(
        "error",
        result_map([(
            "error",
            json!({"message": redact(message), "code": null, "details": null}),
        )]),
    )
}

/// Publishes the session's accounting as a sequenced `session/statsUpdated`.
pub(crate) fn stats_updated_frame(session: &mut SessionRuntime) -> Vec<u8> {
    let stats = public_stats(Some(session));
    let context_window = session.context_window;
    let event_id = next_event_id(session);
    encode_notification(
        "session/statsUpdated",
        result_map([
            ("eventId", json!(event_id)),
            ("sessionId", json!(session.id)),
            ("stats", stats),
            ("contextWindow", json!(context_window)),
            ("emittedAt", json!(now_millis())),
        ]),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingCallback {
    pub(crate) id: String,
    pub(crate) kind: EngineCallbackKind,
    pub(crate) entry: PublicHistoryEntry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedCallback {
    pub(crate) kind: EngineCallbackKind,
    pub(crate) output: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallbackRoute {
    pub(crate) session_id: String,
    pub(crate) turn_id: String,
    pub(crate) callback_id: String,
    pub(crate) answered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub id: String,
    pub working_directory: String,
    pub intent: SessionIntent,
    pub compaction: CompactionSettings,
    pub status: SessionStatus,
    pub active_turn: Option<String>,
    pub pending_callback: Option<String>,
    pub context: Vec<String>,
    pub steering: Vec<String>,
    pub snapshot: Option<ProjectionSnapshot>,
    pub attachments: u32,
}

impl From<&SessionRuntime> for SessionView {
    fn from(session: &SessionRuntime) -> Self {
        Self {
            id: session.id.clone(),
            working_directory: session.working_directory.clone(),
            intent: session.intent.clone(),
            compaction: session.compaction.clone(),
            status: session.status,
            active_turn: session.active_turn.clone(),
            pending_callback: session
                .pending_callback
                .as_ref()
                .map(|callback| callback.id.clone()),
            context: session.context.clone(),
            steering: session.steering.clone(),
            snapshot: session.snapshot.clone(),
            attachments: session.attachments,
        }
    }
}
