//! The detail a notice entry carries: what happened, and the hook that raised
//! it when one did.

use serde::{Deserialize, Serialize};

use super::{HookScope, HookSeverity};
use crate::hooks::HookEvent;

/// What a hook notice reports about one hook or one hook run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookNotice {
    #[serde(default)]
    pub scope: HookScope,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub hook_name: Option<String>,
    #[serde(default)]
    pub status: Option<HookSeverity>,
    #[serde(default)]
    pub content: Option<String>,
}

/// Why a notice entry was written.
///
/// A condition with no variant here is not published as a notice: the port used
/// to invent a `kind`, which a reference client rejects outright.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum NoticeDetail {
    HookRunStarted(HookNotice),
    HookRunCompleted(HookNotice),
    HookStarted(HookNotice),
    HookCompleted(HookNotice),
    AgentChanged {
        agent_name: String,
    },
    ContextCleared {
        #[serde(default)]
        plan_file_path: Option<String>,
    },
    SessionTitleUpdated {
        title: String,
    },
    PlanReviewStarted {
        file_path: String,
    },
    PlanReviewEnded,
    WaitingForInput {
        task_id: String,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        predefined_answers: Option<Vec<String>>,
    },
    ScheduledLoopFired {
        loop_id: String,
    },
}

impl NoticeDetail {
    /// The notice a hook event publishes, and the message it carries.
    /// Reference `_project_hook`.
    pub(crate) fn for_hook(event: &HookEvent) -> (String, Self) {
        match event {
            HookEvent::RunStarted {
                scope,
                tool_name,
                tool_call_id,
            } => (
                "Running hooks".to_owned(),
                NoticeDetail::HookRunStarted(HookNotice {
                    scope: (*scope).into(),
                    tool_name: tool_name.clone(),
                    tool_call_id: tool_call_id.clone(),
                    ..HookNotice::default()
                }),
            ),
            HookEvent::RunCompleted {
                scope,
                tool_call_id,
            } => (
                "Hooks completed".to_owned(),
                NoticeDetail::HookRunCompleted(HookNotice {
                    scope: (*scope).into(),
                    tool_call_id: tool_call_id.clone(),
                    ..HookNotice::default()
                }),
            ),
            HookEvent::Started {
                hook_name,
                scope,
                tool_call_id,
            } => (
                format!("Running hook {hook_name}"),
                NoticeDetail::HookStarted(HookNotice {
                    scope: (*scope).into(),
                    tool_call_id: tool_call_id.clone(),
                    hook_name: Some(hook_name.clone()),
                    ..HookNotice::default()
                }),
            ),
            HookEvent::Completed {
                hook_name,
                status,
                content,
                scope,
                tool_call_id,
            } => (
                content
                    .clone()
                    .filter(|content| !content.is_empty())
                    .unwrap_or_else(|| format!("Hook {hook_name} completed")),
                NoticeDetail::HookCompleted(HookNotice {
                    scope: (*scope).into(),
                    tool_name: None,
                    tool_call_id: tool_call_id.clone(),
                    hook_name: Some(hook_name.clone()),
                    status: Some(*status),
                    content: content.clone(),
                }),
            ),
        }
    }
}
