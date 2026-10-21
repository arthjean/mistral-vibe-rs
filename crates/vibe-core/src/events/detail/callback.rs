//! The detail a callback entry carries, and the answer it settles with: an
//! approval of a call, or the operator's answers to a question.

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{ApprovalDecisionType, EffectDetail};
use crate::events::CallbackKind;
use crate::policy::{PathGrantScope, PermissionRequirement};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionChoice {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserQuestion {
    pub question: String,
    #[serde(default)]
    pub header: String,
    pub options: Vec<QuestionChoice>,
    #[serde(default, alias = "multi_select")]
    pub multi_select: bool,
    #[serde(default, alias = "hide_other")]
    pub hide_other: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserQuestionRequest {
    pub questions: Vec<UserQuestion>,
    #[serde(default, alias = "footer_note")]
    pub footer_note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserAnswer {
    pub question: String,
    pub answer: String,
    #[serde(default, alias = "is_other")]
    pub is_other: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserQuestionResult {
    pub answers: Vec<UserAnswer>,
    #[serde(default)]
    pub cancelled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDecision {
    #[serde(rename = "type")]
    pub decision: ApprovalDecisionType,
    /// Reference `ApprovalDecision.path_scope`: how far the operator chose to
    /// grant the outside paths an approval names, null when they chose none.
    #[serde(default)]
    pub path_scope: Option<PathGrantScope>,
}

/// What a callback is asking the client for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum CallbackDetail {
    Approval {
        effect: Box<EffectDetail>,
        /// Reference `ApprovalCallbackDetail.required_permissions`: the
        /// requirement models themselves, because a client renders what an
        /// "allow always" would grant from the scope and the two patterns.
        #[serde(default)]
        required_permissions: Vec<PermissionRequirement>,
        #[serde(default)]
        choices: Vec<ApprovalDecisionType>,
        /// Reference `ApprovalCallbackDetail.path_scope_choices`: the scopes
        /// an approval of outside paths may be granted under, empty when it
        /// names none.
        #[serde(default)]
        path_scope_choices: Vec<PathGrantScope>,
        #[serde(default)]
        related_entry_id: Option<String>,
        /// Reference `ApprovalCallbackDetail.reason`: why an approval is asked
        /// beyond the tool's own permission, which the static per-tool gate
        /// never states.
        #[serde(default)]
        reason: Option<String>,
    },
    UserInput {
        request: UserQuestionRequest,
        #[serde(default)]
        related_entry_id: Option<String>,
    },
}

impl CallbackDetail {
    /// The detail an engine-raised callback publishes.
    ///
    /// The engine names a kind and a prompt; the wire union wants a typed body,
    /// so an approval borrows the generic effect shape and a question becomes a
    /// single-question request.
    pub(crate) fn for_engine(kind: CallbackKind, prompt: &str) -> Self {
        match kind {
            CallbackKind::UserInput => Self::UserInput {
                request: UserQuestionRequest {
                    questions: vec![UserQuestion {
                        question: prompt.to_owned(),
                        header: String::new(),
                        options: Vec::new(),
                        multi_select: false,
                        hide_other: false,
                    }],
                    footer_note: None,
                },
                related_entry_id: None,
            },
            // A connector-authorization callback has no reference variant, and
            // the app-server refuses to raise one. It is projected as the
            // approval it behaves like rather than as an invented kind.
            CallbackKind::Approval | CallbackKind::ConnectorAuth => Self::Approval {
                effect: Box::new(EffectDetail::for_call("callback", &json!({}))),
                required_permissions: Vec::new(),
                choices: ApprovalDecisionType::ALL.to_vec(),
                path_scope_choices: Vec::new(),
                related_entry_id: None,
                reason: None,
            },
        }
    }

    /// The entry this callback relates to, when the caller named one.
    #[must_use]
    pub fn related_entry_id(&self) -> Option<&str> {
        match self {
            Self::Approval {
                related_entry_id, ..
            }
            | Self::UserInput {
                related_entry_id, ..
            } => related_entry_id.as_deref(),
        }
    }
}

/// What a client answered a callback with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum CallbackOutput {
    Approval {
        decision: ApprovalDecision,
        #[serde(default)]
        feedback: Option<String>,
    },
    UserInput {
        result: UserQuestionResult,
    },
}

impl CallbackOutput {
    /// The answer an engine-resolved callback records, in the form `detail`
    /// declares. A mismatched pair never reaches here: the app-server rejects
    /// an output whose type is not the open callback's.
    pub(crate) fn accepted(detail: &CallbackDetail, value: Option<&str>) -> Self {
        match detail {
            CallbackDetail::Approval { .. } => Self::Approval {
                decision: ApprovalDecision {
                    decision: ApprovalDecisionType::Approve,
                    path_scope: None,
                },
                feedback: value.map(str::to_owned),
            },
            CallbackDetail::UserInput { request, .. } => Self::UserInput {
                result: UserQuestionResult {
                    answers: request
                        .questions
                        .iter()
                        .map(|question| UserAnswer {
                            question: question.question.clone(),
                            answer: value.unwrap_or_default().to_owned(),
                            is_other: false,
                        })
                        .collect(),
                    cancelled: false,
                },
            },
        }
    }

    /// Whether the operator asked for the whole turn to stop.
    #[must_use]
    pub fn requests_turn_cancel(&self) -> bool {
        matches!(
            self,
            Self::Approval {
                decision: ApprovalDecision {
                    decision: ApprovalDecisionType::CancelTurn,
                    ..
                },
                ..
            }
        )
    }
}
