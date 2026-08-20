//! Delegation: a turn that runs another turn.
//!
//! A subagent is a child session with its own identifier, its own transcript
//! and a bounded budget: a depth ceiling so delegation cannot recurse without
//! end, a duration ceiling, and a result size the parent will accept. The
//! finalizer is what makes a dropped parent still settle its child, so a
//! cancelled turn never leaves a delegation recorded as running.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::json;

use serde_json::Value;
use tokio::time::timeout;

use super::agents::{AgentKind, AgentProfile};
use super::{
    ExtensionError, MAX_CHILD_ID_ATTEMPTS, MAX_DELEGATION_DEPTH, MAX_DELEGATION_DURATION,
    MAX_DELEGATION_RESULT_BYTES,
};
use crate::engine::CancellationToken;

static CHILD_SEQUENCE: AtomicU64 = AtomicU64::new(1);
use crate::storage::{SessionStore, StorageError};
use crate::text::bounded_utf8;

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

#[derive(Debug, Clone, PartialEq)]
pub struct DelegationRequest {
    pub parent_session_id: String,
    pub agent: AgentProfile,
    pub prompt: String,
    pub logging: ChildLoggingPolicy,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChildContext {
    pub parent_session_id: String,
    pub child_session_id: String,
    pub depth: u8,
    pub agent: AgentProfile,
    pub prompt: String,
    pub config: BTreeMap<String, Value>,
    pub logging: ChildLoggingPolicy,
    pub working_directory: String,
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
        let parent = self.store.load(&request.parent_session_id)?;
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
        let (child_session_id, mut metadata) = {
            let mut created = None;
            for _ in 0..MAX_CHILD_ID_ATTEMPTS {
                let sequence = CHILD_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let candidate = format!(
                    "child-{now_ms:016x}-{:08x}-{sequence:016x}",
                    std::process::id()
                );
                match self.store.create_child(
                    &candidate,
                    &parent.metadata.working_directory,
                    request.parent_session_id.clone(),
                    now_ms,
                ) {
                    Ok(metadata) => {
                        created = Some((candidate, metadata));
                        break;
                    }
                    Err(StorageError::DuplicateSessionId(_)) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            created.ok_or(ExtensionError::ChildIdExhausted)?
        };
        metadata.config = parent.metadata.config.clone();
        metadata.agent_profile = Some(json!({
            "name": request.agent.name,
            "kind": "subagent",
            "depth": depth,
            "logging": request.logging,
        }));
        self.store.update_metadata(&metadata)?;
        let cancellation = CancellationToken::default();
        self.active.lock().await.insert(
            child_session_id.clone(),
            (request.parent_session_id.clone(), cancellation.clone()),
        );
        let finalizer = DelegationFinalizer::new(
            self.store.clone(),
            self.active.clone(),
            request.parent_session_id.clone(),
            child_session_id.clone(),
            cancellation.clone(),
            now_ms.saturating_add(1),
        );
        let context = ChildContext {
            parent_session_id: request.parent_session_id.clone(),
            child_session_id: child_session_id.clone(),
            depth,
            agent: request.agent,
            prompt: request.prompt,
            config: parent.metadata.config,
            logging: request.logging,
            working_directory: parent.metadata.working_directory,
        };
        let (status, run) = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                (
                    DelegationStatus::Cancelled,
                    SubagentRun::unfinished("Subagent cancelled".to_owned()),
                )
            }
            outcome = timeout(
                MAX_DELEGATION_DURATION,
                self.runner.run(context, cancellation.clone()),
            ) => {
                match outcome {
                    Ok(Ok(run)) => (
                        DelegationStatus::Completed,
                        SubagentRun {
                            response: bounded_utf8(
                                &run.response,
                                MAX_DELEGATION_RESULT_BYTES,
                                "…[truncated]",
                            ),
                            ..run
                        },
                    ),
                    Ok(Err(error)) => (
                        DelegationStatus::Failed,
                        SubagentRun::unfinished(bounded_utf8(
                            &error,
                            MAX_DELEGATION_RESULT_BYTES,
                            "…[truncated]",
                        )),
                    ),
                    Err(_) => {
                        cancellation.cancel();
                        (
                            DelegationStatus::Failed,
                            SubagentRun::unfinished("Subagent timed out".to_owned()),
                        )
                    }
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
