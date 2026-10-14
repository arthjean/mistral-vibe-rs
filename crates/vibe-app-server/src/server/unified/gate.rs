//! The approval gate of a unified session.
//!
//! Reference `_rust_tool_modes` (`vibe/app_server/_runtime.py`) and
//! `UnifiedPermissionResolver` (`vibe/app_server/_unified_permissions.py`):
//! a session that bypasses approvals runs every call; otherwise each builtin
//! call is decided by the per-call rules of the session's own tools, the same
//! rules and session grants the legacy backend reads, and a call those rules
//! leave asking becomes an approval callback.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use vibe_core::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionMode, PolicyError,
};
use vibe_core::tools::{ToolApproval, ToolApprovalSource, ToolInvocation, ToolRegistry};
use vibe_harness::engine::SessionCore;
use vibe_harness::gate::{
    Approver, CallbackAnswer, GateFuture, GateVerdict, ToolGate, approval_record,
};
use vibe_harness::smart::{self, ClassificationRequest, Classifier, SmartState, Verdict};
use vibe_harness::state::ToolAction;

use super::provided::ProvidedRoutes;

const DENIED_BY_POLICY: &str = "Tool execution denied by approval policy";
const DENIED_BY_CALLBACK: &str = "Tool execution denied by approval callback";

/// Reference `RUST_BUILTIN_TOOL_SOURCES`: the session tools a builtin stands
/// for, whose rules decide its calls.
fn sources(builtin: &str) -> &'static [&'static str] {
    match builtin {
        "file_system.read_file" => &["read_file"],
        "file_system.write_file" => &["write_file"],
        "file_system.search_replace" => &["edit"],
        "file_system.bash" | "process.start" | "process.write" => &["bash"],
        "skill.read" => &["skill"],
        _ => &[],
    }
}

/// Reference `_ARGUMENT_TRANSLATORS`: the arguments a session tool reads for
/// the call a builtin makes, only what its permission rules need.
fn translated_arguments(builtin: &str, arguments: &Value) -> Value {
    let text = |key: &str| arguments.get(key).cloned().unwrap_or(Value::Null);
    match builtin {
        "file_system.read_file" => json!({"file_path": text("path")}),
        "file_system.write_file" => json!({
            "file_path": text("path"),
            "content": arguments.get("content").cloned().unwrap_or_else(|| json!("")),
        }),
        "file_system.search_replace" => {
            let block = arguments
                .get("content")
                .and_then(Value::as_array)
                .and_then(|blocks| blocks.first())
                .cloned()
                .unwrap_or_else(|| json!({}));
            json!({
                "file_path": text("file_path"),
                "old_string": block.get("old_str").cloned().unwrap_or_else(|| json!("")),
                "new_string": block.get("new_str").cloned().unwrap_or_else(|| json!("")),
                "replace_all": block.get("replace_all").cloned().unwrap_or(json!(false)),
            })
        }
        "file_system.bash" | "process.start" => json!({
            "command": arguments.get("command").cloned().unwrap_or_else(|| json!("")),
        }),
        "skill.read" => {
            json!({"name": arguments.get("name").cloned().unwrap_or_else(|| json!(""))})
        }
        _ => json!({}),
    }
}

/// Decides a unified session's calls.
pub(crate) struct UnifiedGate {
    /// Whether the session skips approvals altogether.
    pub(crate) bypass: bool,
    /// Whether the session's agent approves file edits on its own.
    pub(crate) edits: bool,
    /// The session's own tools, whose rules decide a builtin call.
    pub(crate) tools: Option<ToolRegistry>,
    /// The tools the session's profile and lists publish.
    pub(crate) available: Vec<String>,
    /// The classifier a session under smart approve consults for each call
    /// its rules leave asking.
    pub(crate) smart: Option<SmartGate>,
    /// The routes of the groups the session's servers and connectors provide.
    pub(crate) provided: Arc<ProvidedRoutes>,
}

/// Reference `_classify_gate`: what smart approve reads and remembers.
pub(crate) struct SmartGate {
    pub(crate) state: Arc<Mutex<SmartState>>,
    pub(crate) core: Arc<Mutex<SessionCore>>,
    pub(crate) classifier: Classifier,
    /// The directories the session writes to, which the classifier trusts.
    pub(crate) roots: Vec<String>,
}

/// What the session's rules make of a call before anyone is asked.
enum Resolved {
    Allow,
    Refuse(GateVerdict),
    /// The rules ask: through the tool's rules with these requirements, or,
    /// when no rule reads the call, through its configured permission.
    Ask {
        ruled: bool,
        required: Vec<Value>,
    },
}

impl ToolGate for UnifiedGate {
    fn check<'a>(&'a self, action: &'a ToolAction, approver: &'a dyn Approver) -> GateFuture<'a> {
        Box::pin(async move {
            if self.bypass {
                return GateVerdict::Run {
                    approval: approval_record("execute", "always", "bypass"),
                };
            }
            let builtin = action.tool_name.as_str();
            // Reference `_rust_tool_modes` over the profile's overlay: an
            // agent that sets the edit tools to `always` runs them by mode.
            if self.edits
                && matches!(
                    builtin,
                    "file_system.write_file" | "file_system.search_replace"
                )
            {
                return GateVerdict::Run {
                    approval: approval_record("execute", "always", "config"),
                };
            }
            if let Some(name) = self.provided.published(builtin) {
                return self.provided_gate(name, action, approver).await;
            }
            let names = sources(builtin);
            if names.is_empty() {
                // A provided tool: `ui.ask_user_question` is never gated, and
                // Vibe's own tools run under their configured permission.
                return GateVerdict::Run {
                    approval: approval_record("execute", "always", "config"),
                };
            }
            let Some(tools) = &self.tools else {
                return refused_by_policy(None);
            };
            let Some(name) = names
                .iter()
                .find(|name| self.available.iter().any(|known| known == *name))
            else {
                return refused_by_policy(None);
            };
            let invocation = ToolInvocation {
                call_id: String::new(),
                arguments: translated_arguments(builtin, &action.arguments),
            };
            if let Some(smart) = &self.smart {
                return self
                    .classify_gate(smart, action, name, invocation, approver)
                    .await;
            }
            let agent = CallbackAgent::new(approver, None);
            match tools.authorize(name, invocation, &agent).await {
                Some(Ok(approval)) => GateVerdict::Run {
                    approval: unified_record(approval),
                },
                Some(Err(error)) => verdict_of(error),
                None => self.configured(name, approver).await,
            }
        })
    }
}

impl UnifiedGate {
    /// Reference `_resolve_provided_tool`: a server's or connector's tool has
    /// no rules that read its call, so the permission configured under the
    /// name the session publishes it as decides, asking by default.
    async fn provided_gate(
        &self,
        name: &str,
        action: &ToolAction,
        approver: &dyn Approver,
    ) -> GateVerdict {
        if self.permission(name) == PermissionMode::Never {
            return refused_by_policy(Some(format!("Tool '{name}' is permanently disabled")));
        }
        let Some(tools) = &self.tools else {
            return refused_by_policy(None);
        };
        let invocation = ToolInvocation {
            call_id: String::new(),
            arguments: action.arguments.clone(),
        };
        if let Some(smart) = &self.smart {
            return self
                .classify_gate(smart, action, name, invocation, approver)
                .await;
        }
        let agent = CallbackAgent::new(approver, None);
        match tools.authorize(name, invocation, &agent).await {
            Some(Ok(approval)) => GateVerdict::Run {
                approval: unified_record(approval),
            },
            Some(Err(error)) => verdict_of(error),
            None => self.configured(name, approver).await,
        }
    }

    /// The tool's configured permission.
    fn permission(&self, name: &str) -> PermissionMode {
        self.tools
            .as_ref()
            .and_then(ToolRegistry::guard)
            .map_or(PermissionMode::Ask, |guard| {
                guard.policy.tool_settings(name).permission
            })
    }

    /// Reference `_classify_gate`: the session's rules first, then the
    /// classifier for what they leave asking.
    async fn classify_gate(
        &self,
        smart: &SmartGate,
        action: &ToolAction,
        name: &str,
        invocation: ToolInvocation,
        approver: &dyn Approver,
    ) -> GateVerdict {
        let resolved = self.resolve(name, invocation.clone()).await;
        let (ruled, required) = match resolved {
            Resolved::Refuse(verdict) => return verdict,
            Resolved::Allow => {
                if let Ok(mut state) = smart.state.lock() {
                    state.proceed();
                }
                return smart_run(smart::RULES_NOTE.to_owned().into());
            }
            Resolved::Ask { ruled, required } => (ruled, required),
        };
        let (request, message_id) = {
            let Ok(core) = smart.core.lock() else {
                return refused_by_policy(None);
            };
            let message_id = core
                .projector
                .state
                .latest_turn
                .as_ref()
                .and_then(|turn| turn.input_entry_id.clone());
            (
                ClassificationRequest::new(
                    &action.tool_name,
                    &action.arguments,
                    &core.conversation,
                    &smart.roots,
                ),
                message_id,
            )
        };
        let key = request.key();
        let remembered = smart.state.lock().is_ok_and(|mut state| {
            state.enter_turn(&action.turn_id);
            state.approved(&key)
        });
        if remembered {
            if let Ok(mut state) = smart.state.lock() {
                state.proceed();
            }
            return smart_run(smart::SESSION_NOTE.to_owned().into());
        }
        let mut classification = smart
            .classifier
            .classify(&request, message_id.as_deref())
            .await;
        let protected = smart::protected_target(&action.tool_name, &action.arguments);
        if let Some(target) = &protected
            && classification.verdict == Verdict::Allow
        {
            classification.verdict = Verdict::Prompt;
            classification.reason = smart::protected_reason(target);
        }
        if classification.verdict == Verdict::Allow {
            if let Ok(mut state) = smart.state.lock() {
                state.proceed();
            }
            let note = (!classification.reason.is_empty())
                .then(|| format!("Auto-approved: {}", classification.reason));
            return smart_run(note);
        }
        let reason = if classification.reason.is_empty() {
            smart::UNCONFIRMED_REASON.to_owned()
        } else {
            classification.reason.clone()
        };
        let escalate = classification.verdict == Verdict::Error
            || smart.state.lock().is_ok_and(|mut state| {
                state.deny(
                    key.clone(),
                    protected.is_some(),
                    classification.user_authorized,
                )
            });
        if !escalate {
            return GateVerdict::Refuse {
                approval: approval_record("skip", "never", "smart"),
                message: smart::deny_message(&reason),
            };
        }
        // Reference `_smart_escalate`: the person settles it, under the scope
        // the rules would have asked with.
        let agent = CallbackAgent::new(approver, Some(reason.clone()));
        let answered = if ruled {
            match self.tools.as_ref() {
                Some(tools) => match tools.authorize(name, invocation, &agent).await {
                    Some(Ok(_)) => Ok(()),
                    Some(Err(PolicyError::TurnCancelled)) => return GateVerdict::CancelTurn,
                    Some(Err(PolicyError::TurnFailed(message))) => {
                        return GateVerdict::FailTurn { message };
                    }
                    _ => Err(()),
                },
                None => Err(()),
            }
        } else {
            match agent
                .request(ApprovalRequest {
                    tool: name.to_owned(),
                    input: Value::Null,
                    requirements: Vec::new(),
                    rationale: String::new(),
                    call_id: None,
                })
                .await
            {
                Ok(ApprovalDecision::CancelTurn) => return GateVerdict::CancelTurn,
                Ok(ApprovalDecision::Fail(message)) | Err(PolicyError::TurnFailed(message)) => {
                    return GateVerdict::FailTurn { message };
                }
                Ok(
                    ApprovalDecision::ApproveOnce
                    | ApprovalDecision::ApproveForSession(_)
                    | ApprovalDecision::ApprovePermanently(_),
                ) => Ok(()),
                _ => Err(()),
            }
        };
        let _ = required;
        if answered.is_err() {
            return GateVerdict::Refuse {
                approval: approval_record("skip", "ask", "user"),
                message: reason,
            };
        }
        if let Ok(mut state) = smart.state.lock() {
            if agent.remembered() {
                state.remember(key);
            }
            state.proceed();
        }
        GateVerdict::Run {
            approval: smart::noted(
                approval_record("execute", "ask", "user"),
                Some(&format!("Approved: {reason}")),
            ),
        }
    }

    /// Reference `UnifiedPermissionResolver.resolve`: the rules' decision
    /// alone, with what an ask would require, and no one asked.
    async fn resolve(&self, name: &str, invocation: ToolInvocation) -> Resolved {
        let Some(tools) = &self.tools else {
            return Resolved::Refuse(refused_by_policy(None));
        };
        let probe = ProbeAgent::default();
        match tools.authorize(name, invocation, &probe).await {
            Some(Ok(_)) => Resolved::Allow,
            Some(Err(_)) if probe.asked() => Resolved::Ask {
                ruled: true,
                required: probe.required(),
            },
            Some(Err(error)) => Resolved::Refuse(verdict_of(error)),
            None => match self.permission(name) {
                PermissionMode::Always => Resolved::Allow,
                PermissionMode::Never => Resolved::Refuse(refused_by_policy(None)),
                PermissionMode::Ask => Resolved::Ask {
                    ruled: false,
                    required: Vec::new(),
                },
            },
        }
    }

    /// A call no rule could read: the tool's configured permission decides,
    /// and an `ask` asks without a scope.
    async fn configured(&self, name: &str, approver: &dyn Approver) -> GateVerdict {
        let permission = self.permission(name);
        match permission {
            PermissionMode::Always => GateVerdict::Run {
                approval: approval_record("execute", "always", "config"),
            },
            PermissionMode::Never => refused_by_policy(None),
            PermissionMode::Ask => match approver.ask(Vec::new(), None).await {
                CallbackAnswer::Output(output) => {
                    match crate::client::interactive::approval_decision_from_output(&output) {
                        Ok(ApprovalDecision::CancelTurn) => GateVerdict::CancelTurn,
                        Ok(
                            ApprovalDecision::ApproveOnce
                            | ApprovalDecision::ApproveForSession(_)
                            | ApprovalDecision::ApprovePermanently(_),
                        ) => GateVerdict::Run {
                            approval: approval_record("execute", "ask", "user"),
                        },
                        Ok(_) | Err(_) => refused_by_callback(),
                    }
                }
                CallbackAnswer::Closed(_) => refused_by_callback(),
            },
        }
    }
}

/// The record of a call the session's rules let through. A stored grant the
/// legacy backend credits to its classifier is the resolver's own allow here.
fn unified_record(approval: ToolApproval) -> Value {
    let approval = if approval.approval_source == ToolApprovalSource::Smart {
        ToolApproval {
            approval_source: ToolApprovalSource::Config,
            ..approval
        }
    } else {
        approval
    };
    serde_json::to_value(approval).unwrap_or(Value::Null)
}

fn refused_by_policy(reason: Option<String>) -> GateVerdict {
    GateVerdict::Refuse {
        approval: approval_record("skip", "never", "never"),
        message: reason
            .filter(|reason| !reason.is_empty())
            .unwrap_or_else(|| DENIED_BY_POLICY.to_owned()),
    }
}

fn refused_by_callback() -> GateVerdict {
    GateVerdict::Refuse {
        approval: approval_record("skip", "ask", "user"),
        message: DENIED_BY_CALLBACK.to_owned(),
    }
}

fn verdict_of(error: PolicyError) -> GateVerdict {
    match error {
        PolicyError::Skipped { .. } => refused_by_callback(),
        PolicyError::TurnCancelled => GateVerdict::CancelTurn,
        PolicyError::TurnFailed(message) => GateVerdict::FailTurn { message },
        PolicyError::Denied(reason) => refused_by_policy(Some(reason)),
        other => refused_by_policy(Some(other.to_string())),
    }
}

/// A smart-approve decision that lets the call run, with its note.
fn smart_run(note: Option<String>) -> GateVerdict {
    GateVerdict::Run {
        approval: smart::noted(
            approval_record("execute", "always", "smart"),
            note.as_deref(),
        ),
    }
}

/// Answers the session's rules by asking the person through a callback,
/// with the reason they are asked when there is one.
struct CallbackAgent<'a> {
    approver: &'a dyn Approver,
    reason: Option<String>,
    /// Whether the answer granted more than this one call.
    remembered: std::sync::atomic::AtomicBool,
}

impl<'a> CallbackAgent<'a> {
    fn new(approver: &'a dyn Approver, reason: Option<String>) -> Self {
        Self {
            approver,
            reason,
            remembered: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn remembered(&self) -> bool {
        self.remembered.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl ApprovalAgent for CallbackAgent<'_> {
    fn request<'a>(&'a self, request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async move {
            let required = request
                .requirements
                .iter()
                .filter_map(|requirement| serde_json::to_value(requirement).ok())
                .collect();
            match self.approver.ask(required, self.reason.clone()).await {
                CallbackAnswer::Output(output) => {
                    let decision =
                        crate::client::interactive::approval_decision_from_output(&output)
                            .map_err(|error| PolicyError::TurnFailed(error.to_string()))?;
                    if matches!(
                        decision,
                        ApprovalDecision::ApproveForSession(_)
                            | ApprovalDecision::ApprovePermanently(_)
                    ) {
                        self.remembered
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    Ok(decision)
                }
                CallbackAnswer::Closed(_) => Ok(ApprovalDecision::Deny),
            }
        })
    }
}

/// Records what the rules would ask instead of asking it.
#[derive(Default)]
struct ProbeAgent {
    asked: Mutex<Option<Vec<Value>>>,
}

impl ProbeAgent {
    fn asked(&self) -> bool {
        self.asked.lock().is_ok_and(|asked| asked.is_some())
    }

    fn required(&self) -> Vec<Value> {
        self.asked
            .lock()
            .ok()
            .and_then(|asked| asked.clone())
            .unwrap_or_default()
    }
}

impl ApprovalAgent for ProbeAgent {
    fn request<'a>(&'a self, request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async move {
            if let Ok(mut asked) = self.asked.lock() {
                *asked = Some(
                    request
                        .requirements
                        .iter()
                        .filter_map(|requirement| serde_json::to_value(requirement).ok())
                        .collect(),
                );
            }
            Ok(ApprovalDecision::Deny)
        })
    }
}

/// Reference `resolve_agent_types` for one agent file: the instructions its
/// child runs under (`agent_instructions`) and the ceiling its tools are held
/// to (`rust_agent_tool_ceiling`), from the profile's overrides.
pub(crate) fn declared_agent_type(
    name: &str,
    overrides: &serde_json::Map<String, Value>,
    available: &[String],
    tools: Option<&ToolRegistry>,
) -> vibe_harness::subagents::AgentType {
    let instructions = overrides
        .get("instructions")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            overrides
                .get("system_prompt_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .and_then(|id| vibe_core::system_prompt::load_system_prompt(id, &[]).ok())
        });
    let patterns = |key: &str| -> Vec<String> {
        overrides
            .get(key)
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let enabled = patterns("enabled_tools");
    let disabled = patterns("disabled_tools");
    let (enabled_filter, disabled_filter) = (
        vibe_core::matching::NameFilter::new(&enabled),
        vibe_core::matching::NameFilter::new(&disabled),
    );
    let narrowed: Vec<&String> = available
        .iter()
        .filter(|name| enabled.is_empty() || enabled_filter.matches(name))
        .filter(|name| disabled.is_empty() || !disabled_filter.matches(name))
        .collect();
    let per_tool = overrides.get("tools").and_then(Value::as_object);
    let permission = |tool: &str| {
        let entry = per_tool.and_then(|table| table.get(tool));
        let declared = entry
            .and_then(|entry| entry.get("permission"))
            .and_then(Value::as_str)
            .and_then(|permission| match permission {
                "always" => Some(PermissionMode::Always),
                "ask" => Some(PermissionMode::Ask),
                "never" => Some(PermissionMode::Never),
                _ => None,
            });
        let resolved = declared.unwrap_or_else(|| {
            tools
                .and_then(ToolRegistry::guard)
                .map_or(PermissionMode::Ask, |guard| {
                    guard.policy.tool_settings(tool).permission
                })
        });
        let listed = entry.is_some_and(|entry| {
            ["allowlist", "denylist"].iter().any(|key| {
                entry
                    .get(*key)
                    .and_then(Value::as_array)
                    .is_some_and(|list| !list.is_empty())
            })
        });
        if resolved == PermissionMode::Always && listed {
            PermissionMode::Ask
        } else {
            resolved
        }
    };
    let ceiling = BUILTINS
        .iter()
        .map(|(builtin, sources)| {
            let present: Vec<&str> = sources
                .iter()
                .copied()
                .filter(|source| narrowed.iter().any(|name| name.as_str() == *source))
                .collect();
            let modes: Vec<PermissionMode> =
                present.iter().map(|source| permission(source)).collect();
            let mode = if present.is_empty() || modes.contains(&PermissionMode::Never) {
                "deny"
            } else if modes.contains(&PermissionMode::Ask) {
                "ask"
            } else {
                "allow"
            };
            ((*builtin).to_owned(), mode.to_owned())
        })
        .collect();
    vibe_harness::subagents::AgentType {
        name: name.to_owned(),
        instructions,
        ceiling,
    }
}

/// Reference `RUST_BUILTIN_TOOL_SOURCES`, every builtin with the session
/// tools it stands for.
const BUILTINS: [(&str, &[&str]); 10] = [
    ("file_system.read_file", &["read_file"]),
    ("file_system.write_file", &["write_file"]),
    ("file_system.search_replace", &["edit"]),
    ("file_system.bash", SHELLS),
    ("skill.read", &["skill"]),
    ("process.start", SHELLS),
    ("process.output", SHELLS),
    ("process.write", SHELLS),
    ("process.list", SHELLS),
    ("process.stop", SHELLS),
];

/// Reference `_SHELL_TOOL_SOURCES`.
const SHELLS: &[&str] = &["bash", "powershell", "git_bash"];

/// The gate a session opened with these settings runs.
pub(crate) fn session_gate(
    bypass: bool,
    edits: bool,
    tools: Option<ToolRegistry>,
    available: Vec<String>,
    smart: Option<SmartGate>,
    provided: Arc<ProvidedRoutes>,
) -> Arc<dyn ToolGate> {
    Arc::new(UnifiedGate {
        bypass,
        edits,
        tools,
        available,
        smart,
        provided,
    })
}
