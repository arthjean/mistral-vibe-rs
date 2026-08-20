//! The client's tests, grouped by the surface each one drives.

pub use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use crate::projects::{
    CloudError, GitProbe, GitSnapshot, Project, ProjectCloud, ProjectPage, ProjectRepository,
    ProjectsService, TeleportCloud, TeleportStartFailure, TeleportStartRequest,
};
use crate::server::SessionStatus;
use crate::workspace::{WorkspacePaths, WorkspaceService};
use vibe_core::compaction::CompactionFailureReason;
use vibe_core::compaction::manager::PLACEHOLDER_SUMMARY;
use vibe_core::events::ModelToolCall;
use vibe_core::provider::{AssistantMessage, ImageInput, Usage};
use vibe_core::schema::{ObjectSchema, Property};
use vibe_core::tools::{
    ToolAvailability, ToolExecutionOutput, ToolPresentationKind, ToolSource, ToolSpec,
};

use std::path::Path;

use super::headless::HeadlessService;
use super::in_process::decode_mcp_warnings;
use super::interactive::*;
use crate::server::SessionToolFactory;
use vibe_core::compaction::manager::CompactionPlan;
use vibe_core::engine::ToolExecutor;
use vibe_core::engine::{Compactor, CompletionProvider};
use vibe_core::mcp::{SamplingRequest, SamplingRole};
use vibe_core::policy::{ApprovalAgent, ApprovalDecision, ApprovalRequest};
use vibe_core::provider::ProviderInput;
use vibe_core::tools::ToolInvocation;

use super::live::{ProviderSessionCompactor, SessionToolExecutor};

mod driver_tests;
mod interactive_tests;
mod interrupt_tests;
mod plan_tests;
mod programmatic_tests;
mod sampling_tests;
mod schema_tests;
mod session_tests;
mod skill_tests;

fn options() -> SessionOptions {
    SessionOptions {
        working_directory: "/workspace".to_owned(),
        session_id: Some("session-1".to_owned()),
        add_directories: vec!["/shared".to_owned()],
        trusted: true,
        agent: Some("coder".to_owned()),
        tool_filters: vec!["read".to_owned()],
        enabled_tools: vec!["read".to_owned()],
        disabled_tools: vec!["shell".to_owned()],
        mcp_servers: Vec::new(),
        model: None,
        max_turns: Some(4),
        max_tokens: Some(1000),
        max_price_micros: Some(500),
        mode: None,
        thinking: false,
        reasoning_effort: None,
        auto_approve: true,
        resume: None,
        continue_session: false,
    }
}

struct DenyEveryApproval;

impl vibe_core::policy::ApprovalAgent for DenyEveryApproval {
    fn request<'a>(
        &'a self,
        _request: vibe_core::policy::ApprovalRequest,
    ) -> vibe_core::policy::ApprovalFuture<'a> {
        Box::pin(async { Ok(vibe_core::policy::ApprovalDecision::Deny) })
    }
}

struct ProgrammaticProjects;

impl ProjectCloud for ProgrammaticProjects {
    fn create(
        &self,
        _name: &str,
        _repo_url: &str,
        _default_branch: &str,
    ) -> Result<Project, CloudError> {
        Err(CloudError::Unavailable(
            "project creation is not used by this fixture".to_owned(),
        ))
    }

    fn list(&self, _cursor: Option<&str>) -> Result<ProjectPage, CloudError> {
        Ok(ProjectPage {
            projects: vec![Project {
                project_id: "project-public-dispatch".to_owned(),
                name: "Public dispatch".to_owned(),
                repositories: vec![ProjectRepository {
                    repo_url: "https://git.example/public-dispatch".to_owned(),
                    default_branch: Some("main".to_owned()),
                }],
                is_read_only: false,
            }],
            next_cursor: None,
        })
    }
}

struct ProgrammaticTeleport;

impl TeleportCloud for ProgrammaticTeleport {
    fn start(&self, request: &TeleportStartRequest) -> Result<String, TeleportStartFailure> {
        Ok(format!("https://cloud.example/{}", request.idempotency_key))
    }
}

struct ProgrammaticGit;

impl GitProbe for ProgrammaticGit {
    fn inspect(&self, _working_directory: &std::path::Path) -> Result<GitSnapshot, CloudError> {
        Ok(GitSnapshot {
            repository: "https://git.example/public-dispatch".to_owned(),
            dirty: false,
            unpushed: false,
        })
    }

    fn push(&self, _working_directory: &std::path::Path) -> Result<(), CloudError> {
        Ok(())
    }
}

struct RecordingProvider {
    seen: Arc<Mutex<Vec<ModelMessage>>>,
}

impl CompletionProvider for RecordingProvider {
    fn complete<'a>(&'a self, input: &'a ProviderInput) -> vibe_core::engine::ProviderFuture<'a> {
        Box::pin(async move {
            *self.seen.lock().map_err(|_| {
                vibe_core::provider::ProviderError::MalformedStream("test lock poisoned".to_owned())
            })? = input.messages.clone();
            Ok(AssistantMessage {
                text: "resumed answer".to_owned(),
                reasoning: None,
                reasoning_signature: None,
                reasoning_state: Vec::new(),
                tool_calls: Vec::new(),
                usage: Usage {
                    input_tokens: 3,
                    output_tokens: 2,
                },
                refusal: None,
                stop_reason: "stop".to_owned(),
                correlation_id: None,
            })
        })
    }
}

struct ToolSelectingProvider {
    calls: AtomicUsize,
    saw_definition: Arc<AtomicBool>,
}

struct SubagentSelectingProvider {
    root_calls: AtomicUsize,
    child_calls: AtomicUsize,
    saw_task_definition: AtomicBool,
    /// What the parent turn actually publishes for `task`, captured so the
    /// reference argument shape is asserted from the live registration
    /// rather than from the spec function in isolation.
    published_task_parameters: std::sync::Mutex<Option<Value>>,
    child_hid_task_definition: AtomicBool,
    child_inherited_restrictions: AtomicBool,
}

impl CompletionProvider for SubagentSelectingProvider {
    fn complete<'a>(&'a self, input: &'a ProviderInput) -> vibe_core::engine::ProviderFuture<'a> {
        Box::pin(async move {
            if input.metadata.contains_key("parent_session_id") {
                let child_call = self.child_calls.fetch_add(1, Ordering::AcqRel);
                self.child_hid_task_definition.store(
                    !input.tools.iter().any(|tool| tool.name == "task"),
                    Ordering::Release,
                );
                self.child_inherited_restrictions.store(
                    input
                        .tools
                        .iter()
                        .map(|tool| tool.name.as_str())
                        .eq(["read"]),
                    Ordering::Release,
                );
                if child_call == 0 {
                    return Ok(AssistantMessage {
                        text: String::new(),
                        reasoning: None,
                        reasoning_signature: None,
                        reasoning_state: Vec::new(),
                        tool_calls: vec![
                            ModelToolCall {
                                id: "child-edit".to_owned(),
                                name: "edit".to_owned(),
                                arguments: "{}".to_owned(),
                            },
                            ModelToolCall {
                                id: "child-shell".to_owned(),
                                name: "shell".to_owned(),
                                arguments: "{}".to_owned(),
                            },
                        ],
                        usage: Usage::default(),
                        refusal: None,
                        stop_reason: "tool_calls".to_owned(),
                        correlation_id: None,
                    });
                }
                for call_id in ["child-edit", "child-shell"] {
                    if !input.messages.iter().any(|message| {
                        matches!(
                            message,
                            ModelMessage::Tool {
                                call_id: actual,
                                is_error: true,
                                ..
                            } if actual == call_id
                        )
                    }) {
                        return Err(vibe_core::provider::ProviderError::MalformedStream(
                            format!("restricted child tool `{call_id}` was not rejected"),
                        ));
                    }
                }
                return Ok(AssistantMessage {
                    text: "child answer".to_owned(),
                    reasoning: None,
                    reasoning_signature: None,
                    reasoning_state: Vec::new(),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    refusal: None,
                    stop_reason: "stop".to_owned(),
                    correlation_id: None,
                });
            }
            let call = self.root_calls.fetch_add(1, Ordering::AcqRel);
            if call == 0 {
                self.saw_task_definition.store(
                    input.tools.iter().any(|tool| tool.name == "task"),
                    Ordering::Release,
                );
                if let Some(task) = input.tools.iter().find(|tool| tool.name == "task")
                    && let Ok(mut published) = self.published_task_parameters.lock()
                {
                    *published = Some(task.input_schema.clone());
                }
                Ok(AssistantMessage {
                    text: String::new(),
                    reasoning: None,
                    reasoning_signature: None,
                    reasoning_state: Vec::new(),
                    tool_calls: vec![ModelToolCall {
                        id: "delegate-1".to_owned(),
                        name: "task".to_owned(),
                        // `agent` is omitted so the reference default has
                        // to reach the handler for the delegation to run.
                        arguments: r#"{"task":"inspect"}"#.to_owned(),
                    }],
                    usage: Usage::default(),
                    refusal: None,
                    stop_reason: "tool_calls".to_owned(),
                    correlation_id: None,
                })
            } else if input.messages.iter().any(|message| {
                matches!(
                    message,
                    ModelMessage::Tool {
                        call_id,
                        content,
                        is_error: false,
                    // Reference `TaskResult` reaches the parent as one field
                    // per line, so the delegation's answer is a line of the
                    // tool message rather than the whole of it. The child
                    // spends two assistant turns here, one calling tools and
                    // one answering, and the reference counts exactly those.
                    } if call_id == "delegate-1"
                        && content == "response: child answer\nturns_used: 2\ncompleted: True"
                )
            }) {
                Ok(AssistantMessage {
                    text: "root done".to_owned(),
                    reasoning: None,
                    reasoning_signature: None,
                    reasoning_state: Vec::new(),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    refusal: None,
                    stop_reason: "stop".to_owned(),
                    correlation_id: None,
                })
            } else {
                Err(vibe_core::provider::ProviderError::MalformedStream(
                    "subagent result did not return to the parent".to_owned(),
                ))
            }
        })
    }
}

impl CompletionProvider for ToolSelectingProvider {
    fn complete<'a>(&'a self, input: &'a ProviderInput) -> vibe_core::engine::ProviderFuture<'a> {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, Ordering::AcqRel);
            if call == 0 {
                self.saw_definition.store(
                    input
                        .tools
                        .iter()
                        .any(|tool| tool.name == "mcp_fixture_echo"),
                    Ordering::Release,
                );
                Ok(AssistantMessage {
                    text: String::new(),
                    reasoning: None,
                    reasoning_signature: None,
                    reasoning_state: Vec::new(),
                    tool_calls: vec![ModelToolCall {
                        id: "call-1".to_owned(),
                        name: "mcp_fixture_echo".to_owned(),
                        arguments: r#"{"message":"rust"}"#.to_owned(),
                    }],
                    usage: Usage::default(),
                    refusal: None,
                    stop_reason: "tool_calls".to_owned(),
                    correlation_id: None,
                })
            } else {
                let returned = input.messages.iter().any(|message| {
                    matches!(
                        message,
                        ModelMessage::Tool {
                            call_id,
                            content,
                            is_error: false,
                        } if call_id == "call-1" && content == "hello rust"
                    )
                });
                if !returned {
                    return Err(vibe_core::provider::ProviderError::MalformedStream(
                        "tool result did not return through the live driver".to_owned(),
                    ));
                }
                Ok(AssistantMessage {
                    text: "done".to_owned(),
                    reasoning: None,
                    reasoning_signature: None,
                    reasoning_state: Vec::new(),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    refusal: None,
                    stop_reason: "stop".to_owned(),
                    correlation_id: None,
                })
            }
        })
    }
}
