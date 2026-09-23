//! This build's answers for the families that run code rather than read the
//! registry: `traits`, `dispatch`, `handlers`, `logLevelApply` and
//! `logLevelPicker`.
//!
//! The handlers run against [`FixtureBackend`], a [`CommandBackend`] that
//! answers from the same fixture the capture handed the reference's fakes
//! (`scripts/parity/commands.py`, `_FakeAppServer` and its siblings) and records
//! what it is asked to do the way those fakes do. Every text an effect carries
//! is reduced to the capture's length and SHA-256, so what is compared is what
//! an operator observes and never reference prose.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use vibe_core::observability::{LogLevel, LogLevelChain};
use vibe_core::telemetry::TelemetryRecord;

use super::super::command_handlers::log_level::{self, Applied, Badge, LogLevelBackend, Picker};
use super::super::command_handlers::{
    self, CommandBackend, Effect, Identity, McpAddArguments, McpLogin, McpSourceKind, McpState,
    NotifySeverity, Panel, Reloaded, ScheduledLoop, SessionLog, Stats,
};
use super::super::commands::{COMMANDS, CommandContext, parse_command_in};
use super::super::runtime::{InteractiveRuntime, RuntimeSkill, interactive_test_runtime};
use super::super::submission::{
    EMPTY_SHELL_ERROR, Hint, Occupancy, Refusal, Route, classify, route,
};

/// The clock the capture pins the loop table to (`LOOP_CLOCK`).
const LOOP_CLOCK: f64 = 1_800_000_000.0;
const DEFAULT_SESSION_ID: &str = "0123456789abcdef-session";
const DEFAULT_FORK_ID: &str = "fedcba9876543210-copy";
const DEFAULT_LOG_PATH: &str = "/home/operator/.vibe/logs/session/0123456789abcdef-session";

/// A text as the capture records it: its length in code points and the hex
/// SHA-256 of its UTF-8 bytes.
pub(super) fn digest(text: &str) -> Value {
    let hash = Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    json!({"length": text.chars().count(), "digest": hash})
}

fn effect_json(effect: Effect) -> Value {
    match effect {
        Effect::Echo(text) => json!({"type": "echo", "text": digest(&text)}),
        Effect::Message(text) => json!({"type": "message", "text": digest(&text)}),
        Effect::Error(text) => json!({"type": "error", "text": digest(&text)}),
        Effect::Warning(text) => json!({"type": "warning", "text": digest(&text)}),
        Effect::Status { text, ok } => json!({"type": "status", "text": digest(&text), "ok": ok}),
        Effect::Notify { severity, text } => json!({
            "type": "notify",
            "severity": match severity {
                NotifySeverity::Information => "information",
                NotifySeverity::Warning => "warning",
            },
            "text": digest(&text),
        }),
        // The two panels the capture builds from a factory record the source
        // they open on, an empty string when none.
        Effect::Panel { panel, initial } => match panel {
            Panel::Mcp | Panel::ConnectorAuth => json!({
                "type": "panel",
                "name": panel.name(),
                "initial": initial.unwrap_or_default(),
            }),
            _ => json!({"type": "panel", "name": panel.name()}),
        },
        Effect::ClosePanel => json!({"type": "closePanel"}),
        Effect::RemoveEcho => json!({"type": "removeEcho"}),
        Effect::ResetTranscript => json!({"type": "resetTranscript"}),
        Effect::Submit { text, injected } => {
            json!({"type": "submit", "text": digest(&text), "injected": injected})
        }
        Effect::ClipboardImage => json!({"type": "clipboardImage", "notifyWhenEmpty": true}),
        Effect::Exit => json!({"type": "exit"}),
        Effect::Teleport { target, echo } => {
            json!({"type": "teleport", "target": target, "echo": echo})
        }
        Effect::SessionsLoaded(count) => json!({"type": "sessionsLoaded", "count": count}),
        Effect::Title(text) => json!({"type": "title", "text": digest(&text)}),
    }
}

fn telemetry_json(record: &TelemetryRecord) -> Value {
    let properties = record
        .attributes(None)
        .expect("every record a handler reports passes its own validators")
        .into_properties();
    json!({
        "type": "telemetry",
        "event": record.event().event_name(),
        "properties": properties,
    })
}

/// The failure a fixture value asks for: `raise` is the capture's own
/// exception and `error` an app-server error, and both print their message.
fn failure(value: Option<&Value>) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    match value.get("raise").or_else(|| value.get("error")) {
        Some(message) => Err(message.as_str().unwrap_or_default().to_owned()),
        None => Ok(()),
    }
}

fn text(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

fn level(value: Option<&Value>) -> Option<LogLevel> {
    value.and_then(Value::as_str).and_then(LogLevel::parse)
}

fn level_json(level: Option<LogLevel>) -> Value {
    level.map_or(Value::Null, |level| {
        Value::String(level.as_str().to_owned())
    })
}

/// The session the capture's fakes describe, answering from one fixture.
struct FixtureBackend<'a> {
    fixture: &'a Map<String, Value>,
    effects: Vec<Value>,
}

impl<'a> FixtureBackend<'a> {
    fn new(fixture: &'a Map<String, Value>) -> Self {
        Self {
            fixture,
            effects: Vec::new(),
        }
    }

    fn get(&self, key: &str) -> Option<&'a Value> {
        self.fixture.get(key)
    }

    fn flag(&self, key: &str) -> Option<bool> {
        self.get(key).and_then(Value::as_bool)
    }

    fn count(&self, key: &str) -> u64 {
        self.get(key).and_then(Value::as_u64).unwrap_or_default()
    }

    fn mcp(&self, key: &str) -> Option<&'a Value> {
        self.get("mcp").and_then(|mcp| mcp.get(key))
    }

    fn loops(&self) -> Result<&'a Value, String> {
        let loops = self.get("loops").unwrap_or(&Value::Null);
        failure(Some(loops))?;
        Ok(loops)
    }

    fn record(&mut self, effect: Value) {
        self.effects.push(effect);
    }
}

fn scheduled(value: &Value) -> ScheduledLoop {
    ScheduledLoop {
        id: text(value.get("id")).unwrap_or_default(),
        prompt: text(value.get("prompt")).unwrap_or_default(),
        interval_seconds: value
            .get("interval_seconds")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        next_fire_at: value
            .get("next_fire_at")
            .and_then(Value::as_f64)
            .unwrap_or_default(),
    }
}

impl CommandBackend for FixtureBackend<'_> {
    fn emit(&mut self, effect: Effect) {
        self.record(effect_json(effect));
    }

    fn report(&mut self, record: TelemetryRecord) {
        self.record(telemetry_json(&record));
    }

    fn session_id(&self) -> String {
        text(self.get("sessionId")).unwrap_or_else(|| DEFAULT_SESSION_ID.to_owned())
    }

    fn turn_active(&self) -> bool {
        self.flag("turnActive").unwrap_or(false)
    }

    fn session_log(&self) -> SessionLog {
        let log = self.get("sessionLog");
        let field = |key: &str| log.and_then(|log| log.get(key));
        SessionLog {
            enabled: field("enabled").and_then(Value::as_bool).unwrap_or(true),
            persisted: field("persisted").and_then(Value::as_bool).unwrap_or(true),
            path: text(field("path")).unwrap_or_else(|| DEFAULT_LOG_PATH.to_owned()),
        }
    }

    async fn reload(&mut self) -> Result<Reloaded, String> {
        let reload = self.get("reload");
        failure(reload)?;
        Ok(Reloaded {
            stripped_images: reload
                .and_then(|reload| reload.get("strippedImages"))
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            model_display_name: text(self.get("activeModelDisplayName"))
                .unwrap_or_else(|| "Model".to_owned()),
        })
    }

    async fn clear_history(&mut self) -> Result<(), String> {
        failure(self.get("clearHistory"))
    }

    fn last_assistant_message(&self) -> Option<String> {
        text(self.get("lastAssistantMessage"))
    }

    fn copy_text(&mut self, copied: &str) -> bool {
        self.record(json!({"type": "clipboard", "text": digest(copied)}));
        self.flag("clipboardVerified").unwrap_or(true)
    }

    fn history_is_empty(&self) -> bool {
        self.get("history")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
    }

    async fn compact(&mut self, _instructions: &str) -> Result<(), String> {
        failure(self.get("compact"))
    }

    fn stats(&mut self) -> Stats {
        let stats = self.get("stats");
        let counter = |key: &str| {
            stats
                .and_then(|stats| stats.get(key))
                .and_then(Value::as_u64)
                .unwrap_or_default()
        };
        Stats {
            steps: counter("steps"),
            session_prompt_tokens: counter("sessionPromptTokens"),
            session_cached_tokens: counter("sessionCachedTokens"),
            session_completion_tokens: counter("sessionCompletionTokens"),
            session_total_llm_tokens: counter("sessionTotalLlmTokens"),
            last_turn_total_tokens: counter("lastTurnTotalTokens"),
            last_turn_cached_tokens: counter("lastTurnCachedTokens"),
            session_cost: stats
                .and_then(|stats| stats.get("sessionCost"))
                .and_then(Value::as_f64)
                .unwrap_or_default(),
        }
    }

    async fn identity(&mut self) -> Result<Option<Identity>, String> {
        let identity = self.get("identity");
        failure(identity)?;
        Ok(identity
            .filter(|identity| !identity.is_null())
            .map(|identity| {
                let named = |key: &str| text(identity.get(key).and_then(|value| value.get("name")));
                Identity {
                    name: text(identity.get("name")),
                    email: text(identity.get("email")),
                    workspace: named("workspace"),
                    organization: named("organization"),
                }
            }))
    }

    async fn account_plan(&mut self) -> Result<Option<String>, String> {
        let account = self.get("account");
        failure(account)?;
        Ok(text(
            account
                .and_then(|account| account.get("plan"))
                .and_then(|plan| plan.get("title")),
        ))
    }

    async fn open_projects(&mut self) -> Result<(), String> {
        failure(self.get("openProjects"))
    }

    async fn saved_sessions(&mut self) -> usize {
        usize::try_from(self.count("savedSessions")).unwrap_or_default()
    }

    async fn rename(&mut self, title: &str) -> Result<String, String> {
        failure(self.get("rename"))?;
        Ok(title.to_owned())
    }

    async fn mcp_state(&mut self) -> McpState {
        let names = |key: &str, kind: McpSourceKind| {
            self.mcp(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(move |name| (name.to_owned(), kind))
                .collect::<Vec<_>>()
        };
        let mut sources = names("sources", McpSourceKind::Server);
        sources.extend(names("connectors", McpSourceKind::Connector));
        McpState {
            sources,
            connector_error: text(self.mcp("connectorError")),
            statuses: self
                .mcp("statuses")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter_map(|(name, status)| Some((name.clone(), status.as_str()?.to_owned())))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    async fn mcp_login(&mut self, _alias: &str) -> Result<McpLogin, String> {
        let login = self.get("mcpLogin");
        failure(login)?;
        Ok(McpLogin {
            urls: login
                .and_then(|login| login.get("urls"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            completed: true,
        })
    }

    async fn mcp_logout(&mut self, alias: &str) -> Result<(), String> {
        failure(self.get("mcpLogout"))?;
        self.record(json!({"type": "mcpLogout", "alias": alias}));
        Ok(())
    }

    async fn mcp_add(&mut self, arguments: &McpAddArguments) -> Result<(String, bool), String> {
        let added = self.get("mcpAdd");
        failure(added)?;
        self.record(json!({
            "type": "mcpAdd",
            "url": arguments.url,
            "name": arguments.name.clone().unwrap_or_default(),
            "scopes": arguments.scopes,
            "transport": arguments.transport,
            "allowInsecureHttp": arguments.allow_insecure_http,
        }));
        let field = |key: &str| added.and_then(|added| added.get(key));
        Ok((
            text(field("name")).unwrap_or_else(|| "server".to_owned()),
            field("created").and_then(Value::as_bool).unwrap_or(true),
        ))
    }

    fn open_url(&mut self, url: &str) {
        self.record(json!({"type": "openUrl", "url": url}));
    }

    fn has_todos(&self) -> bool {
        false
    }

    async fn agent_names(&mut self) -> Vec<String> {
        self.get("agents").and_then(Value::as_array).map_or_else(
            || vec!["default".to_owned()],
            |agents| {
                agents
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            },
        )
    }

    async fn set_agent_installed(&mut self, name: &str, installed: bool) -> Result<(), String> {
        self.record(json!({"type": "agentInstalled", "name": name, "installed": installed}));
        Ok(())
    }

    async fn fork(&mut self) -> Result<String, String> {
        failure(self.get("fork"))?;
        self.record(json!({"type": "fork", "attach": false}));
        Ok(text(self.get("forkedSessionId")).unwrap_or_else(|| DEFAULT_FORK_ID.to_owned()))
    }

    fn retry_offered(&self) -> bool {
        self.flag("retryOffered").unwrap_or(false)
    }

    fn now(&self) -> f64 {
        LOOP_CLOCK
    }

    async fn loops_list(&mut self) -> Result<Vec<ScheduledLoop>, String> {
        Ok(self
            .loops()?
            .get("list")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(scheduled)
            .collect())
    }

    async fn loops_create(
        &mut self,
        _interval: &str,
        prompt: &str,
    ) -> Result<ScheduledLoop, String> {
        let created = self.loops()?.get("created");
        let mut scheduled = created.map_or_else(
            || ScheduledLoop {
                id: String::new(),
                prompt: String::new(),
                interval_seconds: 0,
                next_fire_at: 0.0,
            },
            scheduled,
        );
        if created.and_then(|created| created.get("id")).is_none() {
            scheduled.id = "loop-1".to_owned();
        }
        if created
            .and_then(|created| created.get("interval_seconds"))
            .is_none()
        {
            scheduled.interval_seconds = 300;
        }
        prompt.clone_into(&mut scheduled.prompt);
        Ok(scheduled)
    }

    async fn loops_delete(&mut self, id: &str) -> Result<ScheduledLoop, String> {
        let prompt = text(self.loops()?.get("deletedPrompt")).unwrap_or_else(|| "check".to_owned());
        Ok(ScheduledLoop {
            id: id.to_owned(),
            prompt,
            interval_seconds: 0,
            next_fire_at: 0.0,
        })
    }

    async fn loops_clear(&mut self) -> Result<u64, String> {
        Ok(self
            .loops()?
            .get("cleared")
            .and_then(Value::as_u64)
            .unwrap_or_default())
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime builds")
        .block_on(future)
}

/// The registry the capture builds for a handler scenario: every gate open
/// unless the scenario's context closes one (`_stub_registry`).
fn scenario_context(context: Option<&Value>) -> CommandContext {
    let flag = |key: &str| {
        context
            .and_then(|context| context.get(key))
            .and_then(Value::as_bool)
            .unwrap_or(true)
    };
    let excluded = context
        .and_then(|context| context.get("excluded"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    CommandContext::new(flag("registrySkillsEnabled"), flag("experimentalHarness"))
        .with_clipboard_image_supported(flag("clipboardSupported"))
        .with_excluded(excluded)
}

pub(super) fn traits(case: &Map<String, Value>, field: &str) -> Option<Value> {
    let id = case.get("id")?.as_str()?;
    let command = COMMANDS.iter().find(|command| command.name == id)?;
    match field {
        "sideChannel" => Some(Value::Bool(command.side_channel)),
        "exits" => Some(Value::Bool(command.exits)),
        _ => None,
    }
}

pub(super) fn handlers(case: &Map<String, Value>) -> Option<Value> {
    let line = case.get("line")?.as_str()?;
    let empty = Map::new();
    let fixture = case
        .get("fixture")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let context = scenario_context(case.get("context"));
    let mut backend = FixtureBackend::new(fixture);
    match parse_command_in(line, &context) {
        Some(parsed) => block_on(command_handlers::run(line, &parsed, &context, &mut backend)),
        None => backend.record(json!({"type": "unhandled"})),
    }
    Some(Value::Array(backend.effects))
}

/// A runtime exposing the one skill the capture's fake runtime knows.
fn runtime_with_review_skill() -> InteractiveRuntime {
    let mut runtime = interactive_test_runtime("dispatch-replay");
    runtime.skills.insert(
        "review".to_owned(),
        RuntimeSkill {
            name: "review".to_owned(),
            description: String::new(),
        },
    );
    runtime
}

fn occupancy(state: &str) -> Option<Occupancy> {
    let (turn, shell, paused) = match state {
        "idle" => (false, false, false),
        "busy" => (true, false, false),
        "shell" => (false, true, false),
        "paused" => (false, false, true),
        "pausedBusy" => (true, false, true),
        "pausedShell" => (false, true, true),
        _ => return None,
    };
    Some(Occupancy {
        turn,
        shell,
        paused,
    })
}

/// What one submitted line does in one composer state.
///
/// Classification and routing are this build's own; the effect each route
/// stands for is what `execute` and the prompt path do with it: a skill turn
/// reports its skill before it starts, a refusal and a refused bare `!` put
/// the line back.
pub(super) fn dispatch(case: &Map<String, Value>) -> Option<Value> {
    let input = case.get("input")?.as_str()?;
    let occupancy = occupancy(case.get("state")?.as_str()?)?;
    let free = case.get("sideChannelFree")?.as_bool()?;
    let context = scenario_context(None);
    let runtime = runtime_with_review_skill();
    let submission = classify(input, &context, Some(&runtime));
    let restored = json!({"type": "restored"});
    let effects = match route(submission, occupancy, free) {
        Route::Command => {
            let parsed = parse_command_in(input, &context)?;
            let key = COMMANDS
                .iter()
                .find(|command| command.id == parsed.id)?
                .name;
            vec![json!({"type": "run", "command": key})]
        }
        Route::Teleport(target) => {
            vec![json!({"type": "teleport", "target": target, "echo": true})]
        }
        Route::Skill => {
            let name = input.strip_prefix('/')?.split_whitespace().next()?;
            vec![
                telemetry_json(&TelemetryRecord::SlashCommandUsed {
                    command: name.to_owned(),
                    kind: vibe_core::telemetry::records::TelemetryCommandKind::Skill,
                }),
                json!({"type": "submit", "text": digest(input), "injected": false}),
            ]
        }
        Route::Prompt => vec![json!({"type": "submit", "text": digest(input), "injected": false})],
        Route::Shell(command) => vec![json!({"type": "shell", "text": digest(&command)})],
        Route::EmptyShell { restore } => {
            let mut effects = vec![json!({"type": "error", "text": digest(EMPTY_SHELL_ERROR)})];
            if restore {
                effects.push(restored);
            }
            effects
        }
        Route::Queue { skill, resume } => {
            let mut effects = vec![json!({
                "type": "queued",
                "kind": if skill { "skill" } else { "prompt" },
            })];
            if resume {
                effects.push(json!({"type": "queueResumed"}));
            }
            effects
        }
        Route::Refuse { reason, hint } => vec![
            json!({
                "type": "refused",
                "reason": match reason {
                    Refusal::SlashCommand => "slashCommand",
                    Refusal::Shell => "shell",
                    Refusal::Teleport => "teleport",
                    Refusal::ShellRunning => "shellRunning",
                    Refusal::SideChannelBusy => "sideChannelBusy",
                },
                "hint": match hint {
                    Hint::Busy => "busy",
                    Hint::Paused => "paused",
                    Hint::None => "none",
                },
            }),
            restored,
        ],
    };
    Some(Value::Array(effects))
}

/// The chain a case declares, resolved the way the reference resolves it:
/// `DEBUG_MODE` wins over `LOG_LEVEL`.
fn chain(case: &Map<String, Value>) -> LogLevelChain {
    let chain = case.get("chain");
    let field = |key: &str| chain.and_then(|chain| chain.get(key));
    let env = if field("debugMode").and_then(Value::as_bool) == Some(true) {
        Some(LogLevel::Debug)
    } else {
        level(field("env"))
    };
    LogLevelChain::resolve(level(field("session")), env, level(field("config")))
}

/// The panel's levels as the capture's patched state holds them.
struct FixtureLevels {
    session: Option<LogLevel>,
    env: Option<LogLevel>,
    config: Option<LogLevel>,
    persist_failure: Option<String>,
    effects: Vec<Value>,
}

impl LogLevelBackend for FixtureLevels {
    fn chain(&self) -> LogLevelChain {
        LogLevelChain::resolve(self.session, self.env, self.config)
    }

    fn set_session_override(&mut self, level: Option<LogLevel>) {
        self.session = level;
    }

    // The capture's config write records the change and leaves the configured
    // level where it was, so the feedback measures the level before it.
    fn persist(&mut self, level: Option<LogLevel>) -> Result<(), String> {
        if let Some(failure) = &self.persist_failure {
            return Err(failure.clone());
        }
        self.effects.push(json!({
            "type": "configWrite",
            "key": "log_level",
            "value": level_json(level),
        }));
        Ok(())
    }

    fn emit(&mut self, effect: Effect) {
        self.effects.push(effect_json(effect));
    }
}

fn applied(value: Option<&Value>) -> Applied {
    let field = |key: &str| value.and_then(|value| value.get(key));
    Applied {
        session: level(field("session")),
        config: level(field("config")),
        config_cleared: field("cleared").and_then(Value::as_bool).unwrap_or(false),
    }
}

fn applied_json(applied: Applied) -> Value {
    json!({
        "session": level_json(applied.session),
        "config": level_json(applied.config),
        "cleared": applied.config_cleared,
    })
}

pub(super) fn log_level_apply(case: &Map<String, Value>) -> Option<Value> {
    let chain = chain(case);
    let mut levels = FixtureLevels {
        session: chain.session,
        env: chain.env,
        config: chain.config,
        persist_failure: text(case.get("persistFailure")),
        effects: Vec::new(),
    };
    log_level::apply(applied(case.get("applied")), &mut levels);
    let session = level_json(levels.session);
    levels
        .effects
        .push(json!({"type": "sessionOverride", "level": session}));
    Some(Value::Array(levels.effects))
}

pub(super) fn log_level_picker(case: &Map<String, Value>, field: &str) -> Option<Value> {
    let mut picker = Picker::new(chain(case));
    let initial = picker.highlighted();
    let mut subtitles = vec![digest(&picker.subtitle())];
    for action in case.get("actions")?.as_array()? {
        let action = action.as_array()?;
        match (
            action.first()?.as_str()?,
            action.get(1).and_then(Value::as_str),
        ) {
            ("highlight", Some(level)) => picker.highlight(LogLevel::parse(level)?),
            ("badge", Some("session")) => picker.focus(Badge::Session),
            ("badge", Some("config")) => picker.focus(Badge::Config),
            ("toggle", None) => {
                picker.toggle();
                subtitles.push(digest(&picker.subtitle()));
            }
            _ => return None,
        }
    }
    match field {
        "initialHighlight" => Some(Value::String(initial.as_str().to_owned())),
        "subtitles" => Some(Value::Array(subtitles)),
        "applied" => Some(applied_json(picker.applied())),
        _ => None,
    }
}
