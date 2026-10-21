//! The subagents a root session's programs start and direct.
//!
//! Reference `mistralai_vibe_local_harness/vibe/_subagents/` (`_controller.py`
//! for the operations and their failures, `_operations.py` for the shapes a
//! program reads, `_notifications.py` for what reaches the parent's model)
//! and `harness/core/src/core/features/subagents/` for the contract. A child
//! is a session of its own, one level deep: it runs the parent's tools
//! without the subagent group, under the parent's prompt without the
//! subagent sections. Every operation answers the program with a value; a
//! refusal is `{"type": "error", "error": "<code>: <message>"}`, never a
//! thrown error.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::watch;

use crate::engine::{
    ConversationMessage, Publication, SessionCore, TurnEnd, TurnEnvironment, TurnRequest, run_turn,
};
use crate::state::{HarnessState, ToolResult};

/// Reference `_models.py` limits: one level deep, eight live children,
/// four running turns, 128 names over the parent's lifetime.
const MAX_CHILDREN: usize = 8;
const MAX_CONCURRENT_TURNS: usize = 4;
const MAX_LIFETIME_NAMES: usize = 128;

/// Why the parent interrupts a child's turn.
const INTERRUPT_REASON: &str = "interrupted by parent";

/// What the reference reports of a turn its parent interrupted: its watcher
/// reads the child's turn once the interrupt is admitted, before the turn
/// has settled, and records that read failing (`_host.py`
/// `_child_turn_outcome`, `_controller.py` `_watch_generation`).
const WATCH_FAILURE: (&str, &str) = ("subagent_child_watch_failed", "child Turn is still running");

/// The result meta key that links a spawn's effect entry to its child.
pub const LINK_META: &str = "mistral.vibe.subagent";

/// A preconfigured subagent type, by name, with the instructions that
/// replace the parent's when it has its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentType {
    pub name: String,
    pub instructions: Option<String>,
    /// Reference `DeclaredAgentTypeProfile.tool_ceiling`: per builtin tool,
    /// `allow`, `ask` or `deny`. A declared type's child also runs without
    /// the parent's tool groups (`_declared_binding`).
    pub ceiling: BTreeMap<String, String>,
}

/// The tools a declared type's child keeps: the runtime's own, without the
/// groups Vibe adds.
const DECLARED_TOOLS: [&str; 6] = [
    "search_tool_functions",
    "run_typescript",
    "read_file",
    "write_file",
    "edit",
    "bash",
];

/// What a root session hands its children: the prompt they run under and
/// the types they can be started as.
#[derive(Debug, Clone, Default)]
pub struct SubagentSetup {
    /// The parent's prompt context with the subagent sections off.
    pub prompt: crate::prompt::PromptContext,
    pub agent_types: Vec<AgentType>,
}

/// Where a child's every published state goes: its ID, then the state.
pub type Listener = Arc<dyn Fn(&str, Publication) + Send + Sync>;

#[derive(Debug, Clone, PartialEq)]
enum Status {
    Running,
    Idle,
    /// The failure, and whether the child takes another turn after it.
    TurnFailed(Failure),
    Stopped,
}

#[derive(Debug, Clone, PartialEq)]
struct Failure {
    code: String,
    message: String,
    reusable: bool,
}

impl Failure {
    fn output(&self) -> Value {
        failure(&self.code, &self.message)
    }
}

/// How one child turn ended.
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Completed(String),
    Failed(Failure),
    Interrupted(String),
}

struct Child {
    name: String,
    agent_type: Option<String>,
    id: String,
    core: Arc<Mutex<SessionCore>>,
    generation: u64,
    status: Status,
    interrupt: Option<watch::Sender<Option<String>>>,
    /// Whether the parent interrupted the running turn.
    interrupted: bool,
    /// Where the child is saved, beside its parent, when the parent is.
    store: Option<crate::store::SessionStore>,
    /// The last turn that ended, by generation.
    outcomes: watch::Sender<Option<(u64, Outcome)>>,
}

/// The children of one session.
#[derive(Default)]
pub struct SubagentManager {
    children: Mutex<Vec<Child>>,
    notifications: Mutex<Vec<String>>,
    listener: Mutex<Option<Listener>>,
}

impl std::fmt::Debug for SubagentManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubagentManager")
            .finish_non_exhaustive()
    }
}

fn success() -> Value {
    json!({"type": "success"})
}

fn failure(code: &str, message: &str) -> Value {
    json!({"type": "error", "error": format!("{code}: {message}")})
}

fn not_found(name: &str) -> Value {
    failure("subagent_not_found", &format!("Unknown subagent: {name}"))
}

fn stopped() -> Value {
    failure("subagent_stopped", "Subagent is permanently closed")
}

/// `child-` and the first 24 hex digits of the parent and the action that
/// spawned it. Reference `_operations.py` `child_session_id`.
#[must_use]
pub fn child_id(parent_session_id: &str, action_id: &str) -> String {
    let hex = vibe_core::text::sha256_hex(format!("{parent_session_id}\0{action_id}").as_bytes());
    format!("child-{}", &hex[..24])
}

/// What a program reads back from an operation.
fn answer(value: Value, meta: Option<Map<String, Value>>) -> ToolResult {
    ToolResult::Success {
        content: Vec::new(),
        structured_content: Some(value),
        meta,
    }
}

impl SubagentManager {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Sets where children's states go.
    pub fn listen(&self, listener: Listener) {
        if let Ok(mut slot) = self.listener.lock() {
            *slot = Some(listener);
        }
    }

    /// The child holding the callback `callback_id`, open or answered.
    #[must_use]
    pub fn child_with_callback(
        &self,
        callback_id: &str,
    ) -> Option<(String, Arc<Mutex<SessionCore>>)> {
        let children = self.children.lock().ok()?;
        children.iter().find_map(|child| {
            let core = child.core.lock().ok()?;
            let book = &core.callbacks;
            let holds = book.is_open(callback_id)
                || book.answered.contains_key(callback_id)
                || book.closed.iter().any(|closed| closed == callback_id);
            holds.then(|| (child.id.clone(), Arc::clone(&child.core)))
        })
    }

    /// The child `child_id`'s session.
    #[must_use]
    pub fn child_core(&self, child_id: &str) -> Option<Arc<Mutex<SessionCore>>> {
        let children = self.children.lock().ok()?;
        children
            .iter()
            .find(|child| child.id == child_id)
            .map(|child| Arc::clone(&child.core))
    }

    /// Interrupts the running turn of the child `child_id`, for `reason`.
    pub fn interrupt_child(&self, child_id: &str, reason: &str) {
        if let Ok(children) = self.children.lock()
            && let Some(child) = children.iter().find(|child| child.id == child_id)
            && let Some(interrupt) = &child.interrupt
        {
            interrupt.send_replace(Some(reason.to_owned()));
        }
    }

    /// Hands a state the child `child_id` reached to the listener.
    pub fn announce(&self, child_id: &str, publication: Publication) {
        let listener = self.listener.lock().ok().and_then(|slot| slot.clone());
        if let Some(listener) = listener {
            listener(child_id, publication);
        }
    }

    /// The runtime notifications the parent's model has not read yet, each
    /// as the text it reads.
    pub fn take_notifications(&self) -> Vec<String> {
        self.notifications
            .lock()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default()
    }

    /// Runs one `subagent.*` operation for the session `parent_session_id`.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute(
        self: &Arc<Self>,
        operation: &str,
        arguments: &Value,
        action_id: &str,
        parent_session_id: &str,
        environment: &TurnEnvironment,
        message_id: Option<String>,
        scratchpad: &std::path::Path,
    ) -> ToolResult {
        let text = |key: &str| {
            arguments
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let name = text("agentName");
        match operation {
            "list" => answer(self.list(), None),
            "spawn" => {
                let agent_type = arguments
                    .get("agentType")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                let spawn = Spawn {
                    name,
                    message: text("message"),
                    agent_type,
                    id: child_id(parent_session_id, action_id),
                    parent: parent_session_id.to_owned(),
                    message_id,
                    scratchpad: scratchpad.to_path_buf(),
                };
                match self.spawn(spawn, environment) {
                    Ok(child) => {
                        let mut meta = Map::new();
                        meta.insert(LINK_META.to_owned(), json!({"childSessionId": child}));
                        answer(success(), Some(meta))
                    }
                    Err(refusal) => answer(refusal, None),
                }
            }
            "wait" => {
                let timeout = arguments
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(1);
                answer(self.wait(&name, Duration::from_millis(timeout)).await, None)
            }
            "send_message" => answer(
                self.send_message(&name, &text("message"), environment, message_id),
                None,
            ),
            "interrupt" => answer(self.interrupt(&name), None),
            "stop" => answer(self.stop(&name).await, None),
            other => {
                ToolResult::failed("tool_failed", format!("Unsupported tool: subagent.{other}"))
            }
        }
    }

    fn list(&self) -> Value {
        let Ok(children) = self.children.lock() else {
            return json!({"agents": []});
        };
        let agents: Vec<Value> = children
            .iter()
            .map(|child| {
                let (status, pending) = match &child.status {
                    Status::Running => ("running", true),
                    Status::Idle => ("idle", false),
                    Status::TurnFailed(_) => ("failed", false),
                    Status::Stopped => ("stopped", false),
                };
                let mut agent = json!({
                    "agentName": child.name,
                    "agentType": child.agent_type,
                    "pendingTurn": pending,
                    "status": status,
                });
                if let Status::TurnFailed(failure) = &child.status {
                    agent["error"] = json!(failure.message);
                }
                agent
            })
            .collect();
        json!({"agents": agents})
    }

    fn spawn(
        self: &Arc<Self>,
        spawn: Spawn,
        environment: &TurnEnvironment,
    ) -> Result<String, Value> {
        let Some(setup) = environment.subagents.as_ref() else {
            return Err(failure(
                "subagent_depth_limit",
                "Subagents cannot start subagents",
            ));
        };
        let declared = match &spawn.agent_type {
            None => None,
            Some(name) => match setup.agent_types.iter().find(|agent| &agent.name == name) {
                Some(agent) => Some(agent.clone()),
                None => return Err(failure("subagent_type_not_found", "Unknown subagent type")),
            },
        };
        let core = {
            let mut children = self
                .children
                .lock()
                .map_err(|_| failure("subagent_spawn_failed", "the session is gone"))?;
            if children.iter().any(|child| child.name == spawn.name) {
                return Err(failure(
                    "subagent_name_conflict",
                    &format!("Subagent name is already reserved: {}", spawn.name),
                ));
            }
            if children.len() >= MAX_LIFETIME_NAMES {
                return Err(failure(
                    "subagent_name_limit",
                    "Subagent lifetime name limit reached",
                ));
            }
            if children
                .iter()
                .filter(|child| child.status != Status::Stopped)
                .count()
                >= MAX_CHILDREN
            {
                return Err(failure(
                    "subagent_child_limit",
                    "Subagent child limit reached",
                ));
            }
            if running(&children) >= MAX_CONCURRENT_TURNS {
                return Err(failure(
                    "subagent_concurrency_limit",
                    "Subagent concurrent Turn limit reached",
                ));
            }
            let state = HarnessState::new(
                &spawn.id,
                Some(&spawn.parent),
                &spawn.parent,
                vibe_core::clock::now_millis(),
            );
            let core = Arc::new(Mutex::new(SessionCore::new(
                state,
                spawn.scratchpad.clone(),
            )));
            let (outcomes, _) = watch::channel(None);
            children.push(Child {
                name: spawn.name.clone(),
                agent_type: spawn.agent_type.clone(),
                id: spawn.id.clone(),
                core: Arc::clone(&core),
                generation: 0,
                status: Status::Idle,
                interrupt: None,
                interrupted: false,
                store: None,
                outcomes,
            });
            core
        };
        if let Ok(locked) = core.lock() {
            let store = environment
                .session_dir
                .as_ref()
                .and_then(|parent| parent.parent())
                .and_then(|root| {
                    crate::store::SessionStore::create_child(root.join(&spawn.id), &locked).ok()
                });
            let publication = locked.publication(crate::state::Advance::Journaled, None);
            drop(locked);
            if let Ok(mut children) = self.children.lock()
                && let Some(child) = children.iter_mut().find(|child| child.id == spawn.id)
            {
                child.store = store;
            }
            self.announce(&spawn.id, publication);
        }
        let child_environment =
            child_environment(environment, setup, declared.as_ref(), spawn.message_id);
        self.start_turn(&spawn.name, &spawn.message, child_environment);
        Ok(spawn.id)
    }

    /// Starts the next turn of `name` with `message`.
    fn start_turn(self: &Arc<Self>, name: &str, message: &str, environment: TurnEnvironment) {
        let (core, id, generation, interrupted) = {
            let Ok(mut children) = self.children.lock() else {
                return;
            };
            let Some(child) = children.iter_mut().find(|child| child.name == name) else {
                return;
            };
            child.generation += 1;
            child.status = Status::Running;
            child.interrupted = false;
            let (interrupt, interrupted) = watch::channel(None);
            child.interrupt = Some(interrupt);
            (
                Arc::clone(&child.core),
                child.id.clone(),
                child.generation,
                interrupted,
            )
        };
        let turn_id = format!("{id}:turn:{generation}");
        let request = TurnRequest {
            content: vec![json!({"type": "text", "text": message})],
            ..TurnRequest::default()
        };
        if let Ok(mut locked) = core.lock() {
            locked.begin_turn_as(&request, turn_id.clone());
            let publication = locked.publication(crate::state::Advance::Journaled, None);
            drop(locked);
            self.announce(&id, publication);
        }
        let manager = Arc::clone(self);
        let name = name.to_owned();
        tokio::spawn(async move {
            let sink_manager = Arc::clone(&manager);
            let sink_id = id.clone();
            let sink: crate::engine::Sink = Arc::new(move |publication: Publication| {
                sink_manager.announce(&sink_id, publication);
            });
            let end = run_turn(Arc::clone(&core), turn_id, environment, sink, interrupted).await;
            let outcome = match end {
                TurnEnd::Completed => Outcome::Completed(final_answer(&core)),
                TurnEnd::Interrupted => Outcome::Interrupted(interrupt_reason(&core)),
                TurnEnd::Failed(error) => Outcome::Failed(Failure {
                    code: if error.code.is_empty() {
                        "subagent_turn_failed".to_owned()
                    } else {
                        error.code
                    },
                    message: error.message,
                    reusable: true,
                }),
            };
            manager.finish(&name, generation, outcome);
        });
    }

    /// Records how turn `generation` of `name` ended, and tells the parent.
    fn finish(&self, name: &str, generation: u64, outcome: Outcome) {
        let Ok(mut children) = self.children.lock() else {
            return;
        };
        let Some(child) = children.iter_mut().find(|child| child.name == name) else {
            return;
        };
        child.interrupt = None;
        if let Some(store) = child.store.as_mut()
            && let Ok(core) = child.core.lock()
        {
            let _ = store.commit(&core, false);
        }
        let outcome = match outcome {
            Outcome::Interrupted(_) if child.interrupted => Outcome::Failed(Failure {
                code: WATCH_FAILURE.0.to_owned(),
                message: WATCH_FAILURE.1.to_owned(),
                reusable: false,
            }),
            other => other,
        };
        if child.status != Status::Stopped {
            child.status = match &outcome {
                Outcome::Failed(failure) => Status::TurnFailed(failure.clone()),
                _ => Status::Idle,
            };
        }
        let (status, level, message, content) = match &outcome {
            Outcome::Completed(text) => (
                "completed",
                "info",
                format!("Subagent {name} completed turn {generation}."),
                Some(text.clone()),
            ),
            Outcome::Failed(failure) => ("failed", "error", failure.message.clone(), None),
            Outcome::Interrupted(reason) => ("interrupted", "warning", reason.clone(), None),
        };
        let notification = json!({
            "id": format!("subagent:{}:{name}:{generation}:{status}", child.id),
            "source": {"type": "subagent", "agent_name": name, "status": status},
            "level": level,
            "message": message,
        });
        let mut text = format!(
            "Runtime notification:\n{}",
            serde_json::to_string(&notification).unwrap_or_default()
        );
        if let Some(content) = content.filter(|content| !content.is_empty()) {
            text = format!("{text}\n\n{content}");
        }
        if let Ok(mut pending) = self.notifications.lock() {
            pending.push(text);
        }
        child.outcomes.send_replace(Some((generation, outcome)));
    }

    async fn wait(&self, name: &str, timeout: Duration) -> Value {
        let (mut outcomes, generation) = {
            let Ok(children) = self.children.lock() else {
                return not_found(name);
            };
            let Some(child) = children.iter().find(|child| child.name == name) else {
                return not_found(name);
            };
            if child.status == Status::Stopped {
                return stopped();
            }
            if child.generation == 0 {
                return failure("subagent_not_ready", "Subagent has no available generation");
            }
            (child.outcomes.subscribe(), child.generation)
        };
        let settled = tokio::time::timeout(timeout, async {
            loop {
                if let Some((ended, outcome)) = outcomes.borrow_and_update().clone()
                    && ended >= generation
                {
                    return Some(outcome);
                }
                if outcomes.changed().await.is_err() {
                    return None;
                }
            }
        })
        .await;
        match settled {
            Ok(Some(Outcome::Completed(text))) => json!({"type": "success", "value": text}),
            Ok(Some(Outcome::Failed(failure))) => failure.output(),
            Ok(Some(Outcome::Interrupted(reason))) => failure("subagent_interrupted", &reason),
            Ok(None) => stopped(),
            Err(_) => failure(
                "subagent_wait_timeout",
                &format!("Timed out waiting for {name}"),
            ),
        }
    }

    fn send_message(
        self: &Arc<Self>,
        name: &str,
        message: &str,
        environment: &TurnEnvironment,
        message_id: Option<String>,
    ) -> Value {
        let steer = {
            let Ok(children) = self.children.lock() else {
                return not_found(name);
            };
            let Some(child) = children.iter().find(|child| child.name == name) else {
                return not_found(name);
            };
            match &child.status {
                Status::Stopped => return stopped(),
                Status::TurnFailed(failure) if !failure.reusable => return failure.output(),
                Status::Running => Some(Arc::clone(&child.core)),
                Status::Idle | Status::TurnFailed(_) => {
                    if running(&children) >= MAX_CONCURRENT_TURNS {
                        return failure(
                            "subagent_concurrency_limit",
                            "Subagent concurrent Turn limit reached",
                        );
                    }
                    None
                }
            }
        };
        if let Some(core) = steer {
            if let Ok(mut core) = core.lock() {
                core.conversation.push(ConversationMessage::User {
                    content: vec![json!({"type": "text", "text": message})],
                    injected: false,
                });
            }
            return success();
        }
        let Some(setup) = environment.subagents.as_ref() else {
            return stopped();
        };
        let declared = self
            .children
            .lock()
            .ok()
            .and_then(|children| {
                children
                    .iter()
                    .find(|child| child.name == name)
                    .and_then(|child| child.agent_type.clone())
            })
            .and_then(|kind| {
                setup
                    .agent_types
                    .iter()
                    .find(|agent| agent.name == kind)
                    .cloned()
            });
        self.start_turn(
            name,
            message,
            child_environment(environment, setup, declared.as_ref(), message_id),
        );
        success()
    }

    fn interrupt(&self, name: &str) -> Value {
        let Ok(mut children) = self.children.lock() else {
            return not_found(name);
        };
        let Some(child) = children.iter_mut().find(|child| child.name == name) else {
            return not_found(name);
        };
        match &child.status {
            Status::Running => {
                child.interrupted = true;
                if let Some(interrupt) = &child.interrupt {
                    interrupt.send_replace(Some(INTERRUPT_REASON.to_owned()));
                }
                success()
            }
            Status::TurnFailed(failure) if !failure.reusable => failure.output(),
            Status::Idle | Status::TurnFailed(_) => {
                failure("subagent_not_running", "Subagent has no active Turn")
            }
            Status::Stopped => stopped(),
        }
    }

    async fn stop(&self, name: &str) -> Value {
        let (mut outcomes, generation) = {
            let Ok(mut children) = self.children.lock() else {
                return not_found(name);
            };
            let Some(child) = children.iter_mut().find(|child| child.name == name) else {
                return not_found(name);
            };
            match child.status {
                // Reference `_stop` answers a closed child as it did the
                // first time.
                Status::Stopped => return success(),
                Status::Running => {
                    child.interrupted = true;
                    if let Some(interrupt) = &child.interrupt {
                        interrupt.send_replace(Some(INTERRUPT_REASON.to_owned()));
                    }
                    (Some(child.outcomes.subscribe()), child.generation)
                }
                _ => {
                    child.status = Status::Stopped;
                    discard(child);
                    (None, child.generation)
                }
            }
        };
        if let Some(outcomes) = outcomes.as_mut() {
            loop {
                let ended = outcomes
                    .borrow_and_update()
                    .as_ref()
                    .is_some_and(|(ended, _)| *ended >= generation);
                if ended || outcomes.changed().await.is_err() {
                    break;
                }
            }
            if let Ok(mut children) = self.children.lock()
                && let Some(child) = children.iter_mut().find(|child| child.name == name)
            {
                child.status = Status::Stopped;
                discard(child);
            }
        }
        success()
    }

    /// Interrupts every running child, as the session closes.
    pub fn shut_down(&self) {
        if let Ok(children) = self.children.lock() {
            for child in children.iter() {
                if let Some(interrupt) = &child.interrupt {
                    interrupt.send_replace(Some(INTERRUPT_REASON.to_owned()));
                }
            }
        }
    }
}

struct Spawn {
    name: String,
    message: String,
    agent_type: Option<String>,
    id: String,
    parent: String,
    message_id: Option<String>,
    /// The parent's scratchpad, which its children share.
    scratchpad: std::path::PathBuf,
}

/// Reference `_stop`: a closed child's store is deleted.
fn discard(child: &mut Child) {
    if let Some(store) = child.store.take() {
        let _ = std::fs::remove_dir_all(store.dir());
    }
}

fn running(children: &[Child]) -> usize {
    children
        .iter()
        .filter(|child| child.status == Status::Running)
        .count()
}

/// Why the child's last turn was interrupted, as its state records it.
fn interrupt_reason(core: &Arc<Mutex<SessionCore>>) -> String {
    core.lock()
        .ok()
        .and_then(|core| {
            core.projector
                .state
                .latest_turn
                .as_ref()
                .and_then(|turn| turn.reason.clone())
        })
        .filter(|reason| !reason.is_empty())
        .unwrap_or_else(|| INTERRUPT_REASON.to_owned())
}

/// The text of the last answer the child's model gave.
fn final_answer(core: &Arc<Mutex<SessionCore>>) -> String {
    core.lock()
        .ok()
        .and_then(|core| {
            core.conversation
                .iter()
                .rev()
                .find_map(|message| match message {
                    ConversationMessage::Assistant { text, .. } => Some(text.clone()),
                    _ => None,
                })
        })
        .unwrap_or_default()
}

/// What a child's turn runs with: the parent's model, tools and workspace,
/// without the subagent group, under `prompt`, reporting the parent's
/// message to the provider.
fn child_environment(
    parent: &TurnEnvironment,
    setup: &SubagentSetup,
    declared: Option<&AgentType>,
    message_id: Option<String>,
) -> TurnEnvironment {
    let mut prompt = setup.prompt.clone();
    if let Some(instructions) = declared.and_then(|agent| agent.instructions.clone()) {
        prompt.instructions = instructions;
    }
    let callables: Vec<crate::programmatic::Callable> = parent
        .callables
        .iter()
        .filter(|callable| callable.group != "subagent")
        .filter(|callable| declared.is_none() || callable.group != "vibe")
        .cloned()
        .collect();
    let mut environment = parent.clone();
    if let Some(agent) = declared {
        environment
            .tools
            .retain(|tool| DECLARED_TOOLS.contains(&tool.name));
        environment.gate = Arc::new(CeilingGate {
            inner: Arc::clone(&parent.gate),
            ceiling: agent.ceiling.clone(),
        });
    }
    environment.system_prompt = crate::prompt::system_prompt(&prompt);
    environment.functions = Arc::new(crate::programmatic::documents(&callables));
    environment.callables = Arc::new(callables);
    environment.subagents = None;
    environment.message_id = message_id;
    environment.session_dir = None;
    environment
}

/// Reference the Runtime's mode narrowing under a declared type: a builtin
/// its ceiling denies is refused without asking anyone, and every other call
/// is the parent's gate's to decide.
struct CeilingGate {
    inner: Arc<dyn crate::gate::ToolGate>,
    ceiling: BTreeMap<String, String>,
}

impl crate::gate::ToolGate for CeilingGate {
    fn check<'a>(
        &'a self,
        action: &'a crate::state::ToolAction,
        approver: &'a dyn crate::gate::Approver,
    ) -> crate::gate::GateFuture<'a> {
        if self
            .ceiling
            .get(&action.tool_name)
            .is_some_and(|mode| mode == "deny")
        {
            return Box::pin(async {
                crate::gate::GateVerdict::Refuse {
                    approval: crate::gate::approval_record("skip", "never", "never"),
                    message: "Tool execution denied by approval policy".to_owned(),
                }
            });
        }
        self.inner.check(action, approver)
    }
}

/// The children summaries a parent publishes, from what its history shows
/// of each child. Reference `_state_with_child_summaries` and
/// `_observe_child_metadata` in the adapter.
#[derive(Debug, Default, Clone)]
pub struct ChildDirectory {
    /// Each child's public session, as it last published it.
    pub sessions: BTreeMap<String, Value>,
    identities: BTreeMap<String, Identity>,
    stopped: BTreeMap<String, u64>,
    observed_stops: Vec<String>,
}

#[derive(Debug, Clone)]
struct Identity {
    name: String,
    agent_type: Value,
    spawned_at: u64,
}

impl ChildDirectory {
    /// The children the observed history spawned.
    #[must_use]
    pub fn known(&self) -> Vec<String> {
        self.identities.keys().cloned().collect()
    }

    /// Reads the links `history` (wire entries) shows: spawns that name
    /// their child, and stops that close one.
    pub fn observe(&mut self, history: &[Value]) {
        let mut preceding: Vec<String> = Vec::new();
        for entry in history {
            if entry["type"] != "effect" {
                continue;
            }
            let detail = &entry["detail"];
            if detail["kind"] == "subagent"
                && detail["toolName"] == "subagent.spawn"
                && let Some(child) = detail["childSessionId"].as_str()
                && !detail["input"].is_null()
            {
                let name = detail["display"]["message"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .unwrap_or("subagent")
                    .to_owned();
                self.identities.insert(
                    child.to_owned(),
                    Identity {
                        name,
                        agent_type: detail["input"]["agent"].clone(),
                        spawned_at: entry["createdAt"].as_u64().unwrap_or_default(),
                    },
                );
                preceding.push(child.to_owned());
                continue;
            }
            let id = entry["id"].as_str().unwrap_or_default();
            if self.observed_stops.iter().any(|seen| seen == id)
                || detail["kind"] != "tool"
                || detail["toolName"] != "subagent.stop"
                || entry["state"]["status"] != "completed"
            {
                continue;
            }
            let Some(agent_name) = detail["input"]["agentName"].as_str() else {
                continue;
            };
            let created_at = entry["createdAt"].as_u64().unwrap_or_default();
            let candidate = self
                .identities
                .iter()
                .filter(|(child, identity)| {
                    identity.name == agent_name
                        && (identity.spawned_at < created_at || preceding.contains(child))
                        && !self.stopped.contains_key(*child)
                })
                .map(|(child, identity)| (identity.spawned_at, child.clone()))
                .max();
            if let Some((_, child)) = candidate {
                self.stopped
                    .insert(child, entry["updatedAt"].as_u64().unwrap_or_default());
                self.observed_stops.push(id.to_owned());
            }
        }
    }

    /// Every child's summary, oldest first.
    #[must_use]
    pub fn summaries(&self) -> Vec<Value> {
        let mut children: Vec<Value> = self
            .sessions
            .iter()
            .map(|(id, session)| {
                let identity = self.identities.get(id);
                let stopped_at = self.stopped.get(id).copied();
                let name = identity.map_or_else(
                    || {
                        session["title"]
                            .as_str()
                            .map_or_else(|| id.chars().take(8).collect(), ToOwned::to_owned)
                    },
                    |identity| identity.name.clone(),
                );
                let updated_at = session["updatedAt"].as_u64().unwrap_or_default();
                let token_usage = if session["tokenUsage"].is_null() {
                    json!({"inputTokens": 0, "outputTokens": 0, "totalTokens": 0})
                } else {
                    session["tokenUsage"].clone()
                };
                json!({
                    "agentType": identity.map_or_else(|| json!("subagent"), |identity| identity.agent_type.clone()),
                    "contextUsage": session["contextUsage"],
                    "createdAt": session["createdAt"],
                    "id": id,
                    "name": name,
                    "status": if stopped_at.is_some() { json!({"type": "archived"}) } else { session["status"].clone() },
                    "tokenUsage": token_usage,
                    "updatedAt": updated_at.max(stopped_at.unwrap_or_default()),
                })
            })
            .collect();
        children.sort_by(|left, right| {
            (left["createdAt"].as_u64(), left["id"].as_str())
                .cmp(&(right["createdAt"].as_u64(), right["id"].as_str()))
        });
        children
    }
}

#[cfg(test)]
mod subagents_tests;
