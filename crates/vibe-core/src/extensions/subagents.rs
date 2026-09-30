//! Delegation: a turn that runs another turn.
//!
//! A subagent is a child session with its own identifier and its own
//! transcript, saved beneath its parent's (reference `create_child` roots the
//! child's logger at `<parent session>/agents`) and linked into the parent's
//! record. A depth ceiling keeps delegation from recursing, and the finalizer
//! is what makes a dropped parent still settle its child, so a cancelled turn
//! never leaves a delegation recorded as running. The reference bounds neither
//! how long a child runs nor how long its answer is, so neither is bounded
//! here.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

use serde_json::Value;

use super::agents::{AgentKind, AgentProfile};
use super::{ExtensionError, MAX_DELEGATION_DEPTH};
use crate::engine::CancellationToken;
use crate::storage::{CHILD_SESSIONS_DIRECTORY, SessionStore, StorageError};

/// What a delegated run produced.
///
/// Reference `TaskResult` (`vibe/core/subagents.py:26`) declares `response`,
/// `turns_used` and `completed`, and the runner is the only place that can
/// count a turn or tell a natural end from a stopped one, so the outcome
/// carries all three rather than the response alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentRun {
    pub response: String,
    pub turns_used: u32,
    pub completed: bool,
}

impl SubagentRun {
    /// A run that ended without producing any of the counters, which is what a
    /// cancellation, a timeout and a runner failure all report.
    #[must_use]
    pub fn unfinished(response: String) -> Self {
        Self {
            response,
            turns_used: 0,
            completed: false,
        }
    }
}

pub type SubagentFuture<'a> =
    Pin<Box<dyn Future<Output = Result<SubagentRun, String>> + Send + 'a>>;

pub trait SubagentRunner: Send + Sync {
    fn run<'a>(
        &'a self,
        context: ChildContext,
        cancellation: CancellationToken,
    ) -> SubagentFuture<'a>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildLoggingPolicy {
    Full,
    SummaryOnly,
    Disabled,
}

/// What a running delegation reports to the call that started it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegationUpdate {
    /// The child session exists and its parent's record links it.
    Linked(String),
    /// One line of progress: a tool the child ran, and what it answered.
    /// Reference `SubagentRunAccumulator.observe`.
    Progress(String),
}

/// Where a delegation's updates go.
pub type DelegationSignal = Arc<dyn Fn(DelegationUpdate) + Send + Sync>;

#[derive(Clone)]
pub struct DelegationRequest {
    pub parent_session_id: String,
    /// The call that asked for the delegation, which the parent's record
    /// links the child under.
    pub tool_call_id: String,
    pub agent: AgentProfile,
    pub prompt: String,
    pub logging: ChildLoggingPolicy,
    pub signal: Option<DelegationSignal>,
}

impl std::fmt::Debug for DelegationRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DelegationRequest")
            .field("parent_session_id", &self.parent_session_id)
            .field("tool_call_id", &self.tool_call_id)
            .field("agent", &self.agent.name)
            .field("prompt", &self.prompt)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct ChildContext {
    pub parent_session_id: String,
    pub child_session_id: String,
    /// The store the child's session is saved in, beneath its parent's.
    pub store: SessionStore,
    pub depth: u8,
    pub agent: AgentProfile,
    pub prompt: String,
    pub config: BTreeMap<String, Value>,
    pub logging: ChildLoggingPolicy,
    pub working_directory: String,
    pub signal: Option<DelegationSignal>,
}

impl ChildContext {
    /// Reports `update` to the call that started the delegation.
    pub fn report(&self, update: DelegationUpdate) {
        if let Some(signal) = &self.signal {
            signal(update);
        }
    }
}

impl ChildContext {
    /// Why this child may not delegate any further, or [`None`] when it still
    /// may.
    ///
    /// Reference `TaskTool.run` refuses outright when the agent asking is
    /// itself a subagent, so a child never forks a grandchild. Here the ceiling
    /// [`SubagentManager::delegate`] applies reads the depth of the session the
    /// call names, and a child calls the `task` its parent published, which
    /// names the parent. So the child's own surface is where the same ceiling
    /// has to be read, and this is what it reads.
    #[must_use]
    pub fn delegation_refusal(&self) -> Option<String> {
        (self.depth >= MAX_DELEGATION_DEPTH).then(|| {
            ExtensionError::DelegationDepth {
                maximum: MAX_DELEGATION_DEPTH,
            }
            .to_string()
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationStatus {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegationEffect {
    pub parent_session_id: String,
    pub child_session_id: String,
    pub public_session_id: String,
    pub status: DelegationStatus,
    pub result: String,
    /// How many turns the child spent, carried through from the runner so the
    /// `task` tool can publish the reference's own result fields.
    pub turns_used: u32,
    pub completed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildActivity {
    pub root_session_id: String,
    pub child_session_id: String,
    pub public_session_id: String,
    pub kind: String,
}

#[derive(Clone)]
pub struct SubagentManager {
    store: SessionStore,
    runner: Arc<dyn SubagentRunner>,
    pub(super) active: Arc<tokio::sync::Mutex<BTreeMap<String, (String, CancellationToken)>>>,
}

impl SubagentManager {
    #[must_use]
    pub fn new(store: SessionStore, runner: Arc<dyn SubagentRunner>) -> Self {
        Self {
            store,
            runner,
            active: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
        }
    }

    pub async fn delegate(
        &self,
        request: DelegationRequest,
        now_ms: u64,
    ) -> Result<DelegationEffect, ExtensionError> {
        if request.agent.kind != AgentKind::Subagent {
            return Err(ExtensionError::AgentNotSubagent(request.agent.name));
        }
        let parent = self.store.open(&request.parent_session_id)?;
        let parent_depth = parent
            .metadata
            .agent_profile
            .as_ref()
            .and_then(|profile| profile.get("depth"))
            .and_then(Value::as_u64)
            .and_then(|depth| u8::try_from(depth).ok())
            .unwrap_or(0);
        let depth = parent_depth.saturating_add(1);
        if depth > MAX_DELEGATION_DEPTH {
            return Err(ExtensionError::DelegationDepth {
                maximum: MAX_DELEGATION_DEPTH,
            });
        }
        let child_store = self
            .store
            .child_store(&parent.metadata, &request.agent.name);
        let child_session_id = crate::session_id::uuid_v4();
        let mut metadata = child_store.create(
            &child_session_id,
            &parent.metadata.working_directory,
            Some(request.parent_session_id.clone()),
            now_ms,
        )?;
        metadata.config = parent.metadata.config.clone();
        metadata.agent_profile = Some(json!({
            "name": request.agent.name,
            "kind": "subagent",
            "depth": depth,
            "logging": request.logging,
        }));
        // Reference `run`: the child's session is written empty, then linked
        // into its parent's record, before its turn starts.
        child_store.persist_empty(&mut metadata, now_ms)?;
        self.store.record_child_session(
            &request.parent_session_id,
            json!({
                "session_id": child_session_id,
                "tool_call_id": request.tool_call_id,
                "agent": request.agent.name,
                "relative_path": format!("{CHILD_SESSIONS_DIRECTORY}/{}", metadata.directory),
            }),
        )?;
        let cancellation = CancellationToken::default();
        self.active.lock().await.insert(
            child_session_id.clone(),
            (request.parent_session_id.clone(), cancellation.clone()),
        );
        let finalizer = DelegationFinalizer::new(
            child_store.clone(),
            self.active.clone(),
            request.parent_session_id.clone(),
            child_session_id.clone(),
            cancellation.clone(),
            now_ms.saturating_add(1),
        );
        let context = ChildContext {
            parent_session_id: request.parent_session_id.clone(),
            child_session_id: child_session_id.clone(),
            store: child_store,
            depth,
            agent: request.agent,
            prompt: request.prompt,
            config: parent.metadata.config,
            logging: request.logging,
            working_directory: parent.metadata.working_directory,
            signal: request.signal,
        };
        context.report(DelegationUpdate::Linked(child_session_id.clone()));
        let (status, run) = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                (
                    DelegationStatus::Cancelled,
                    SubagentRun::unfinished("Subagent cancelled".to_owned()),
                )
            }
            outcome = self.runner.run(context, cancellation.clone()) => {
                match outcome {
                    Ok(run) => (DelegationStatus::Completed, run),
                    Err(error) => (DelegationStatus::Failed, SubagentRun::unfinished(error)),
                }
            }
        };
        // Closing the child session is cleanup: its failure is reported with the
        // outcome rather than discarding work the subagent already completed.
        let result = match finalizer.finish().await {
            Ok(()) => run.response,
            Err(error) => format!(
                "{}\n\n[child session cleanup failed: {error}]",
                run.response
            ),
        };
        Ok(DelegationEffect {
            parent_session_id: request.parent_session_id,
            child_session_id: child_session_id.clone(),
            public_session_id: child_session_id,
            status,
            result,
            turns_used: run.turns_used,
            completed: run.completed,
        })
    }

    pub async fn cancel_parent(&self, parent_session_id: &str) {
        for (parent, cancellation) in self.active.lock().await.values() {
            if parent == parent_session_id {
                cancellation.cancel();
            }
        }
    }

    #[must_use]
    pub fn activity(effect: &DelegationEffect, kind: &str) -> ChildActivity {
        ChildActivity {
            root_session_id: effect.parent_session_id.clone(),
            child_session_id: effect.child_session_id.clone(),
            public_session_id: effect.public_session_id.clone(),
            kind: kind.to_owned(),
        }
    }
}

struct DelegationFinalizer {
    store: SessionStore,
    active: Arc<tokio::sync::Mutex<BTreeMap<String, (String, CancellationToken)>>>,
    parent_session_id: String,
    child_session_id: String,
    cancellation: CancellationToken,
    close_at_ms: u64,
    finished: bool,
}

impl DelegationFinalizer {
    fn new(
        store: SessionStore,
        active: Arc<tokio::sync::Mutex<BTreeMap<String, (String, CancellationToken)>>>,
        parent_session_id: String,
        child_session_id: String,
        cancellation: CancellationToken,
        close_at_ms: u64,
    ) -> Self {
        Self {
            store,
            active,
            parent_session_id,
            child_session_id,
            cancellation,
            close_at_ms,
            finished: false,
        }
    }

    async fn finish(mut self) -> Result<(), StorageError> {
        self.active.lock().await.remove(&self.child_session_id);
        self.store.close(&self.child_session_id, self.close_at_ms)?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for DelegationFinalizer {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.cancellation.cancel();
        let _ = self.store.close(&self.child_session_id, self.close_at_ms);
        if let Ok(mut active) = self.active.try_lock() {
            active.remove(&self.child_session_id);
            return;
        }
        let active = self.active.clone();
        let child_session_id = self.child_session_id.clone();
        let parent_session_id = self.parent_session_id.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let mut active = active.lock().await;
                if active
                    .get(&child_session_id)
                    .is_some_and(|(parent, _)| parent == &parent_session_id)
                {
                    active.remove(&child_session_id);
                }
            });
        }
    }
}
