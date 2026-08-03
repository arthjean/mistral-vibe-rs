//! Session state: settings, per-session harness, and lifecycle slots.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use vibe_app_server::client::{HeadlessService, SessionOptions, TurnDriver};

use crate::protocol::{AcpError, AcpSessionInfo};

const SESSION_CURSOR_PREFIX: &str = "v1:";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Mode {
    #[default]
    Code,
    Plan,
}

impl Mode {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "code" => Some(Self::Code),
            "plan" => Some(Self::Plan),
            _ => None,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Plan => "plan",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Thinking {
    Off,
    Low,
    #[default]
    Medium,
    High,
    Max,
}

impl Thinking {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            _ => Self::effort(value),
        }
    }

    /// Reasoning-effort levels, which exclude `off` because the canonical
    /// session carries effort and enablement as two separate fields.
    fn effort(value: &str) -> Option<Self> {
        match value {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
        }
    }

    pub(crate) const fn enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    const fn effort_str(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            other => Some(other.as_str()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SessionSettings {
    pub(crate) mode: Mode,
    pub(crate) thinking: Thinking,
}

impl SessionSettings {
    /// Rebuilds settings from a persisted release-3 payload, where `thinking`
    /// may be a boolean paired with a separate effort or a level string.
    pub(crate) fn from_release3_result(result: &BTreeMap<String, Value>) -> Self {
        let config = result
            .get("metadata")
            .and_then(|metadata| metadata.get("config"))
            .and_then(Value::as_object);
        let mode = config
            .and_then(|config| config.get("mode"))
            .and_then(Value::as_str)
            .and_then(Mode::parse)
            .unwrap_or_default();
        let effort = config
            .and_then(|config| {
                config
                    .get("reasoningEffort")
                    .or_else(|| config.get("reasoning_effort"))
            })
            .and_then(Value::as_str)
            .and_then(Thinking::effort);
        let thinking = match config.and_then(|config| config.get("thinking")) {
            Some(Value::Bool(false)) => Thinking::Off,
            Some(Value::String(level)) => Thinking::parse(level).or(effort).unwrap_or_default(),
            _ => effort.unwrap_or_default(),
        };
        Self { mode, thinking }
    }

    pub(crate) fn from_intent(mode: Option<&str>, thinking: bool, effort: Option<&str>) -> Self {
        Self {
            mode: mode.and_then(Mode::parse).unwrap_or_default(),
            thinking: if thinking {
                effort.and_then(Thinking::effort).unwrap_or_default()
            } else {
                Thinking::Off
            },
        }
    }

    pub(crate) fn as_config(&self) -> Value {
        let mut config = json!({
            "mode": self.mode.as_str(),
            "thinking": self.thinking.enabled(),
        });
        if let Some(effort) = self.thinking.effort_str() {
            config["reasoningEffort"] = json!(effort);
        }
        config
    }
}

/// What a session is currently doing. Reserving and Builtin have no canonical
/// turn to interrupt yet, which is why cancellation only latches a flag there.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum ActivePhase {
    #[default]
    Idle,
    Reserving,
    Builtin,
    Running(String),
}

pub(crate) struct AcpHarness<D>
where
    D: TurnDriver,
{
    pub(crate) service: tokio::sync::Mutex<HeadlessService<D>>,
    pub(crate) session_id: String,
    pub(crate) cwd: String,
    active: Mutex<ActivePhase>,
    cancel_requested: AtomicBool,
    cancel_notify: tokio::sync::Notify,
    settings: Mutex<SessionSettings>,
    user_message_anchors: Mutex<BTreeMap<String, usize>>,
    next_id: AtomicU64,
}

impl<D> AcpHarness<D>
where
    D: TurnDriver,
{
    pub(crate) fn adopt(
        mut service: HeadlessService<D>,
        session_id: &str,
    ) -> Result<Self, AcpError> {
        let view = service.session(session_id)?;
        Ok(Self {
            service: tokio::sync::Mutex::new(service),
            session_id: session_id.to_owned(),
            cwd: view.working_directory,
            active: Mutex::new(ActivePhase::Idle),
            cancel_requested: AtomicBool::new(false),
            cancel_notify: tokio::sync::Notify::new(),
            settings: Mutex::new(SessionSettings::from_intent(
                view.intent.mode.as_deref(),
                view.intent.thinking,
                view.intent.reasoning_effort.as_deref(),
            )),
            user_message_anchors: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
        })
    }

    pub(crate) fn settings(&self) -> Result<SessionSettings, AcpError> {
        Ok(*self.settings.lock().map_err(|_| AcpError::StatePoisoned)?)
    }

    pub(crate) fn update_settings(
        &self,
        update: impl FnOnce(&mut SessionSettings),
    ) -> Result<(), AcpError> {
        let mut settings = self.settings.lock().map_err(|_| AcpError::StatePoisoned)?;
        update(&mut settings);
        Ok(())
    }

    /// Claims the session for new work, or reports the conflict.
    pub(crate) fn begin(&self, phase: ActivePhase) -> Result<(), AcpError> {
        let mut active = self.active.lock().map_err(|_| AcpError::StatePoisoned)?;
        if *active != ActivePhase::Idle {
            return Err(AcpError::SessionConflict(format!(
                "session `{}` already has active work",
                self.session_id
            )));
        }
        *active = phase;
        Ok(())
    }

    pub(crate) fn set_phase(&self, phase: ActivePhase) -> Result<(), AcpError> {
        *self.active.lock().map_err(|_| AcpError::StatePoisoned)? = phase;
        Ok(())
    }

    pub(crate) fn release(&self) -> Result<(), AcpError> {
        self.cancel_requested.store(false, Ordering::Release);
        self.set_phase(ActivePhase::Idle)
    }

    pub(crate) fn phase(&self) -> Result<ActivePhase, AcpError> {
        Ok(self
            .active
            .lock()
            .map_err(|_| AcpError::StatePoisoned)?
            .clone())
    }

    /// Turn ID of a canonically reserved turn, which is the only phase where
    /// the driver has something to interrupt.
    pub(crate) fn running_turn_id(&self) -> Result<Option<String>, AcpError> {
        Ok(match self.phase()? {
            ActivePhase::Running(turn_id) => Some(turn_id),
            ActivePhase::Idle | ActivePhase::Reserving | ActivePhase::Builtin => None,
        })
    }

    pub(crate) fn request_cancel(&self) {
        self.cancel_requested.store(true, Ordering::Release);
        self.cancel_notify.notify_one();
    }

    pub(crate) fn clear_cancel(&self) {
        self.cancel_requested.store(false, Ordering::Release);
    }

    pub(crate) fn take_cancel_request(&self) -> bool {
        self.cancel_requested.swap(false, Ordering::AcqRel)
    }

    pub(crate) async fn cancelled(&self) {
        loop {
            if self.cancel_requested.load(Ordering::Acquire) {
                return;
            }
            self.cancel_notify.notified().await;
        }
    }

    /// Monotonic identifier for locally generated messages and tool calls, so
    /// two updates emitted in the same millisecond never collide.
    pub(crate) fn next_local_id(&self, prefix: &str) -> String {
        let sequence = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{}-{sequence}", self.session_id)
    }

    pub(crate) fn next_ephemeral_user_message_id(&self) -> String {
        let sequence = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("{}:user:{sequence}", self.session_id)
    }

    pub(crate) fn record_user_message_anchor(
        &self,
        message_id: String,
        history_before: usize,
        history_after: &[Value],
        prompt: &str,
    ) -> Result<(), AcpError> {
        let anchor = history_after
            .iter()
            .enumerate()
            .skip(history_before)
            .rev()
            .find(|(_, entry)| {
                entry.get("role").and_then(Value::as_str) == Some("user")
                    && entry.get("content").and_then(Value::as_str) == Some(prompt)
            })
            .map(|(index, _)| index);
        if let Some(anchor) = anchor {
            self.user_message_anchors
                .lock()
                .map_err(|_| AcpError::StatePoisoned)?
                .insert(message_id, anchor);
        }
        Ok(())
    }

    pub(crate) fn user_message_anchor(&self, message_id: &str) -> Result<Option<usize>, AcpError> {
        Ok(self
            .user_message_anchors
            .lock()
            .map_err(|_| AcpError::StatePoisoned)?
            .get(message_id)
            .copied())
    }
}

/// Lifecycle of one session ID. A single slot replaces the parallel active,
/// loading, closing, and closed collections the agent used to reconcile.
pub(crate) enum SessionSlot<D>
where
    D: TurnDriver,
{
    Loading,
    Active(Arc<AcpHarness<D>>),
    Closing(Arc<AcpHarness<D>>),
    /// Retains the closed identity so repeated closes stay idempotent. The
    /// sequence bounds retention: the oldest tombstone is evicted first.
    Closed(u64),
}

impl<D> SessionSlot<D>
where
    D: TurnDriver,
{
    pub(crate) fn harness(&self) -> Option<&Arc<AcpHarness<D>>> {
        match self {
            Self::Active(harness) | Self::Closing(harness) => Some(harness),
            Self::Loading | Self::Closed(_) => None,
        }
    }
}

pub(crate) fn session_options(
    cwd: &str,
    session_id: Option<String>,
    additional_directories: Vec<String>,
    mcp_servers: Vec<Value>,
    resume: Option<String>,
    settings: &SessionSettings,
) -> SessionOptions {
    SessionOptions {
        working_directory: cwd.to_owned(),
        session_id,
        add_directories: additional_directories,
        trusted: !mcp_servers.is_empty(),
        agent: None,
        tool_filters: Vec::new(),
        enabled_tools: Vec::new(),
        disabled_tools: vec!["ask_user_question".to_owned(), "exit_plan_mode".to_owned()],
        mcp_servers,
        model: None,
        max_turns: None,
        max_tokens: None,
        max_price_micros: None,
        mode: Some(settings.mode.as_str().to_owned()),
        thinking: settings.thinking.enabled(),
        reasoning_effort: settings.thinking.effort_str().map(ToOwned::to_owned),
        auto_approve: false,
        resume,
        continue_session: false,
    }
}

pub(crate) fn validate_session_paths(
    cwd: &str,
    additional_directories: Option<&[String]>,
    max_additional_directories: usize,
) -> Result<(), AcpError> {
    let additional_directories = additional_directories.unwrap_or_default();
    if additional_directories.len() > max_additional_directories {
        return Err(AcpError::InvalidParams(format!(
            "additionalDirectories cannot contain more than {max_additional_directories} roots"
        )));
    }
    require_absolute_cwd(cwd)?;
    if let Some(path) = additional_directories
        .iter()
        .find(|path| !Path::new(path).is_absolute())
    {
        return Err(AcpError::InvalidParams(format!(
            "additional directory `{path}` must be an absolute path"
        )));
    }
    Ok(())
}

pub(crate) fn require_absolute_cwd(cwd: &str) -> Result<(), AcpError> {
    if Path::new(cwd).is_absolute() {
        Ok(())
    } else {
        Err(AcpError::InvalidParams(
            "cwd must be an absolute path".to_owned(),
        ))
    }
}

pub(crate) fn ensure_matching_cwd(
    requested: &str,
    persisted: &str,
    operation: &str,
) -> Result<(), AcpError> {
    if same_path(requested, persisted) {
        Ok(())
    } else {
        Err(AcpError::InvalidParams(format!(
            "cannot {operation} a session saved for `{persisted}` from cwd `{requested}`"
        )))
    }
}

pub(crate) fn same_path(left: &str, right: &str) -> bool {
    resolved_path(left) == resolved_path(right)
}

/// Resolves a path for comparison. Unresolvable paths fall back to lexical
/// normalization rather than a raw string, so `/a/../b` and `/b` still match.
fn resolved_path(value: &str) -> PathBuf {
    std::fs::canonicalize(value).unwrap_or_else(|_| lexically_normalized(Path::new(value)))
}

fn lexically_normalized(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            other => normalized.push(other),
        }
    }
    normalized
}

pub(crate) fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

pub(crate) fn metadata_session_id(result: &BTreeMap<String, Value>) -> Result<String, AcpError> {
    result
        .get("metadata")
        .and_then(|metadata| {
            metadata
                .get("session_id")
                .or_else(|| metadata.get("sessionId"))
        })
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| AcpError::InvalidResponse("missing resumed session ID".to_owned()))
}

pub(crate) fn acp_session_info(session: &Value) -> Result<AcpSessionInfo, AcpError> {
    let session_id = session
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| AcpError::InvalidResponse("listed session omitted id".to_owned()))?
        .to_owned();
    let cwd = session
        .get("workingDirectory")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AcpError::InvalidResponse(format!("listed session `{session_id}` omitted cwd"))
        })?
        .to_owned();
    let mut meta = BTreeMap::new();
    for key in ["parentSessionId", "messageCount"] {
        if let Some(value) = session.get(key)
            && !value.is_null()
        {
            meta.insert(key.to_owned(), value.clone());
        }
    }
    Ok(AcpSessionInfo {
        session_id,
        cwd,
        title: session
            .get("title")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        updated_at: session
            .get("endTime")
            .and_then(Value::as_str)
            .or_else(|| session.get("startTime").and_then(Value::as_str))
            .map(ToOwned::to_owned),
        additional_directories: None,
        meta,
    })
}

pub(crate) fn decode_session_cursor(cursor: Option<&str>) -> Result<usize, AcpError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    cursor
        .strip_prefix(SESSION_CURSOR_PREFIX)
        .and_then(|offset| offset.parse::<usize>().ok())
        .ok_or_else(|| AcpError::InvalidParams("invalid session/list cursor".to_owned()))
}

pub(crate) fn encode_session_cursor(offset: usize) -> String {
    format!("{SESSION_CURSOR_PREFIX}{offset}")
}

pub(crate) fn session_response(
    session_id: String,
    settings: &SessionSettings,
) -> crate::protocol::AcpSession {
    crate::protocol::AcpSession {
        session_id,
        modes: Some(json!({
            "currentModeId": settings.mode.as_str(),
            "availableModes": [
                {"id": "code", "name": "Code", "description": "Execute changes"},
                {"id": "plan", "name": "Plan", "description": "Inspect and plan"},
            ],
        })),
        config_options: Some(thinking_config_options(settings.thinking)),
    }
}

pub(crate) fn thinking_config_options(current: Thinking) -> Vec<Value> {
    vec![json!({
        "id": "thinking",
        "name": "Thinking",
        "type": "select",
        "currentValue": current.as_str(),
        "options": [
            {"value": "off", "name": "Off"},
            {"value": "low", "name": "Low"},
            {"value": "medium", "name": "Medium"},
            {"value": "high", "name": "High"},
            {"value": "max", "name": "Max"},
        ],
    })]
}
