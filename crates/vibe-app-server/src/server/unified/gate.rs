//! The approval gate of a unified session.
//!
//! Reference `_rust_tool_modes` (`vibe/app_server/_runtime.py`) and
//! `UnifiedPermissionResolver` (`vibe/app_server/_unified_permissions.py`):
//! a session that bypasses approvals runs every call; otherwise each builtin
//! call is decided by the per-call rules of every session tool it stands for,
//! the same rules and session grants the legacy backend reads, and a call
//! those rules leave asking becomes one approval callback whose answer every
//! one of those tools records.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use vibe_core::policy::{
    ApprovalDecision, GrantLifetime, PathGrantScope, PermissionContext, PermissionMode,
    PermissionOutcome, PermissionRequirement, PermissionScope, approval_path_scope, wildcard_match,
};
use vibe_core::tools::{ToolInvocation, ToolRegistry};
use vibe_harness::engine::SessionCore;
use vibe_harness::gate::{
    Approver, CallbackAnswer, GateFuture, GateVerdict, ToolGate, approval_record,
};
use vibe_harness::smart::{self, ClassificationRequest, Classifier, SmartState, Verdict};
use vibe_harness::state::ToolAction;

use super::lock;
use super::provided::ProvidedRoutes;

const DENIED_BY_POLICY: &str = "Tool execution denied by approval policy";
const DENIED_BY_CALLBACK: &str = "Tool execution denied by approval callback";

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
    /// The rules ask about these requirements, none when nothing scopes the
    /// call and an approval that outlasts it grants the tools themselves.
    Ask(Vec<PermissionRequirement>),
}

/// How the person answered an approval.
enum Answer {
    /// The call runs, and `remembered` when the approval outlasts it.
    Approved {
        remembered: bool,
    },
    Declined,
    /// The answer ends the turn instead.
    Ended(GateVerdict),
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
            let Some(sources) = gated_sources(builtin) else {
                // A provided tool: `ui.ask_user_question` is never gated, and
                // Vibe's own tools run under their configured permission. So
                // does a process call the start gate already settled.
                return GateVerdict::Run {
                    approval: approval_record("execute", "always", "config"),
                };
            };
            let names = vibe_tool_names(sources, &self.available);
            if names.is_empty() {
                return refused_by_policy(None);
            }
            let invocation = ToolInvocation {
                call_id: String::new(),
                arguments: translated_arguments(builtin, &action.arguments),
            };
            self.decide(&names, invocation, action, approver).await
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
        let invocation = ToolInvocation {
            call_id: String::new(),
            arguments: action.arguments.clone(),
        };
        self.decide(&[name], invocation, action, approver).await
    }

    /// The session tools `names` decide the call together: smart approve
    /// classifies what their rules leave asking, and otherwise the person is
    /// asked once about it.
    async fn decide(
        &self,
        names: &[&str],
        invocation: ToolInvocation,
        action: &ToolAction,
        approver: &dyn Approver,
    ) -> GateVerdict {
        let Some(tools) = &self.tools else {
            return refused_by_policy(None);
        };
        let resolved = self.resolve(tools, names, invocation, action).await;
        if let Some(smart) = &self.smart {
            return smart
                .classify_gate(tools, names, resolved, action, approver)
                .await;
        }
        let requirements = match resolved {
            Resolved::Allow => {
                return GateVerdict::Run {
                    approval: approval_record("execute", "always", "config"),
                };
            }
            Resolved::Refuse(verdict) => return verdict,
            Resolved::Ask(requirements) => requirements,
        };
        match ask(tools, names, requirements, None, action, approver).await {
            Answer::Approved { .. } => GateVerdict::Run {
                approval: approval_record("execute", "ask", "user"),
            },
            Answer::Declined => refused_by_callback(),
            Answer::Ended(verdict) => verdict,
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

    /// Reference `UnifiedPermissionResolver.resolve`: what the rules of every
    /// tool in `names` make of the call, and no one asked.
    ///
    /// A refusal from any of them refuses it. Otherwise whatever any of them
    /// leaves uncovered is asked about at once, so a scope one tool reads
    /// survives a sibling that reads nothing. A tool with nothing to say about
    /// the call leaves it to its siblings, and when none of them clears or
    /// scopes it either, it is asked about under its fallback scope, which
    /// keeps an approval from granting the tools themselves.
    async fn resolve(
        &self,
        tools: &ToolRegistry,
        names: &[&str],
        invocation: ToolInvocation,
        action: &ToolAction,
    ) -> Resolved {
        if hides_environment(tools, action) {
            return self.resolve_hidden(tools, names, invocation).await;
        }
        let mut uncovered = Vec::new();
        let (mut cleared, mut scoped, mut unscoped) = (false, false, false);
        for name in names {
            match tools.resolve_permission(name, invocation.clone()).await {
                // The rules let the call run outright, or a stored grant
                // covers every requirement they raised.
                Some(Ok(PermissionOutcome::Cleared { .. })) => cleared = true,
                Some(Ok(PermissionOutcome::Refused(reason))) => {
                    return Resolved::Refuse(refused_by_policy(Some(reason)));
                }
                Some(Ok(PermissionOutcome::Unscoped { .. })) => unscoped = true,
                Some(Ok(PermissionOutcome::Needs { requirements, .. })) => {
                    scoped = true;
                    uncovered.extend(requirements);
                }
                Some(Err(error)) => {
                    return Resolved::Refuse(refused_by_policy(Some(error.to_string())));
                }
                // No rule reads the call, so its configured permission does.
                None => match self.permission(name) {
                    PermissionMode::Always => cleared = true,
                    PermissionMode::Never => return Resolved::Refuse(refused_by_policy(None)),
                    PermissionMode::Ask => unscoped = true,
                },
            }
        }
        if unscoped && !scoped && !cleared {
            let Some(fallback) = fallback_scope(&action.tool_name, &action.arguments) else {
                return Resolved::Ask(Vec::new());
            };
            for name in names {
                if !granted(tools, name, &fallback).await {
                    uncovered.push(fallback);
                    break;
                }
            }
        }
        if uncovered.is_empty() {
            Resolved::Allow
        } else {
            Resolved::Ask(uncovered)
        }
    }

    /// Reference `_resolve_start_with_hidden_environment`'s verdict on a
    /// start that picks parts of its run no command rule reads: a shell that
    /// refuses the command still refuses it, and otherwise the call is asked
    /// about unscoped, so the answer releases this call alone.
    async fn resolve_hidden(
        &self,
        tools: &ToolRegistry,
        names: &[&str],
        invocation: ToolInvocation,
    ) -> Resolved {
        for name in names {
            match tools.resolve_permission(name, invocation.clone()).await {
                Some(Ok(PermissionOutcome::Refused(reason))) => {
                    return Resolved::Refuse(refused_by_policy(Some(reason)));
                }
                None if self.permission(name) == PermissionMode::Never => {
                    return Resolved::Refuse(refused_by_policy(None));
                }
                _ => {}
            }
        }
        Resolved::Ask(Vec::new())
    }
}

/// Reference `_resolve_start_with_hidden_environment`: whether a
/// `process.start` also picks its environment, or a working directory other
/// than the one its shells run in, both of which reach the process unseen by
/// any command rule.
///
/// The shells run in the session's working directory, the first root its
/// store authorized; a store that authorized none has no directory a start
/// could match.
fn hides_environment(tools: &ToolRegistry, action: &ToolAction) -> bool {
    if action.tool_name != "process.start" {
        return false;
    }
    let arguments = &action.arguments;
    if arguments
        .get("env")
        .is_some_and(vibe_core::llm::adapter::truthy)
    {
        return true;
    }
    let Some(cwd) = arguments.get("cwd") else {
        return false;
    };
    let Some(cwd) = cwd.as_str() else {
        return vibe_core::llm::adapter::truthy(cwd);
    };
    let root = tools
        .guard()
        .and_then(|guard| guard.policy.workspace_roots().into_iter().next());
    let Some(root) = root else {
        return true;
    };
    let cwd = vibe_core::platform::expand_home(Path::new(cwd));
    vibe_core::platform::resolve_lenient(&root.join(cwd)) != root
}

impl SmartGate {
    /// Reference `_classify_gate`: the session's rules first, then the
    /// classifier for what they leave asking.
    async fn classify_gate(
        &self,
        tools: &ToolRegistry,
        names: &[&str],
        resolved: Resolved,
        action: &ToolAction,
        approver: &dyn Approver,
    ) -> GateVerdict {
        let requirements = match resolved {
            Resolved::Refuse(verdict) => return verdict,
            Resolved::Allow => {
                lock(&self.state).proceed();
                return smart_run(smart::RULES_NOTE.to_owned().into());
            }
            Resolved::Ask(requirements) => requirements,
        };
        let (request, message_id) = {
            let core = lock(&self.core);
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
                    &self.roots,
                ),
                message_id,
            )
        };
        let key = request.key();
        let remembered = {
            let mut state = lock(&self.state);
            state.enter_turn(&action.turn_id);
            state.approved(&key)
        };
        if remembered {
            lock(&self.state).proceed();
            return smart_run(smart::SESSION_NOTE.to_owned().into());
        }
        let mut classification = self
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
            lock(&self.state).proceed();
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
            || lock(&self.state).deny(
                key.clone(),
                protected.is_some(),
                classification.user_authorized,
            );
        if !escalate {
            return GateVerdict::Refuse {
                approval: approval_record("skip", "never", "smart"),
                message: smart::deny_message(&reason),
            };
        }
        // Reference `_smart_escalate`: the person settles it, under the scope
        // the rules would have asked with.
        match ask(
            tools,
            names,
            requirements,
            Some(reason.clone()),
            action,
            approver,
        )
        .await
        {
            Answer::Approved { remembered } => {
                let mut state = lock(&self.state);
                if remembered {
                    state.remember(key);
                }
                state.proceed();
            }
            Answer::Declined => {
                return GateVerdict::Refuse {
                    approval: approval_record("skip", "ask", "user"),
                    message: reason,
                };
            }
            Answer::Ended(verdict) => return verdict,
        }
        GateVerdict::Run {
            approval: smart::noted(
                approval_record("execute", "ask", "user"),
                Some(&format!("Approved: {reason}")),
            ),
        }
    }
}

/// Asks the person once about `requirements`, with the reason they are asked
/// when there is one, and has every tool in `names` record an approval that
/// outlasts the call.
async fn ask(
    tools: &ToolRegistry,
    names: &[&str],
    requirements: Vec<PermissionRequirement>,
    reason: Option<String>,
    action: &ToolAction,
    approver: &dyn Approver,
) -> Answer {
    let required = requirements
        .iter()
        .filter_map(|requirement| serde_json::to_value(requirement).ok())
        .collect();
    let decision = match approver.ask(required, reason).await {
        CallbackAnswer::Output(output) => {
            match crate::client::interactive::approval_decision_from_output(&output) {
                Ok(decision) => decision,
                Err(error) => {
                    return Answer::Ended(GateVerdict::FailTurn {
                        message: error.to_string(),
                    });
                }
            }
        }
        CallbackAnswer::Closed(_) => return Answer::Declined,
    };
    let (selected, permanent) = match decision {
        ApprovalDecision::ApproveOnce => return Answer::Approved { remembered: false },
        ApprovalDecision::ApproveForSession(selected) => (selected, false),
        ApprovalDecision::ApprovePermanently(selected) => (selected, true),
        ApprovalDecision::Deny | ApprovalDecision::Reject(_) => return Answer::Declined,
        ApprovalDecision::CancelTurn => return Answer::Ended(GateVerdict::CancelTurn),
        ApprovalDecision::Fail(message) => {
            return Answer::Ended(GateVerdict::FailTurn { message });
        }
    };
    // Reference `approval_grant_permissions`: the scope the person chose has
    // to be one the approval offered, and a choice it did not offer fails the
    // turn, as the store fails it for an approval of its own.
    match approval_path_scope(&requirements, selected) {
        Ok(scope) => {
            grant(
                tools,
                names,
                &action.tool_name,
                &requirements,
                scope,
                permanent,
            )
            .await;
            Answer::Approved { remembered: true }
        }
        Err(refusal) => Answer::Ended(GateVerdict::FailTurn {
            message: refusal.to_string(),
        }),
    }
}

/// Reference `UnifiedPermissionResolver.grant`: every tool in `names` records
/// the approval of `requirements` under `scope`, through the session's store,
/// as an approval of its own would be recorded.
async fn grant(
    tools: &ToolRegistry,
    names: &[&str],
    route: &str,
    requirements: &[PermissionRequirement],
    scope: PathGrantScope,
    permanent: bool,
) {
    // A builtin that scopes every call it can read has no honest grant of
    // the tool itself: an approval carrying nothing releases that call alone.
    if requirements.is_empty() && fallback_argument(route).is_some() {
        return;
    }
    let Some(guard) = tools.guard() else {
        return;
    };
    for name in names {
        // Reference `_PROCESS_SESSION_GRANT_BUILTINS`: a process dies with
        // the session, so nothing about it is kept.
        let lifetime = if permanent && route != "process.write" {
            GrantLifetime::PermanentWithin(allowlist_scopes(name))
        } else {
            GrantLifetime::Session
        };
        guard
            .policy
            .grant(name, requirements, scope, lifetime)
            .await;
    }
}

/// Reference `BaseTool.allowlist_scopes`: a shell reads command prefixes and
/// path grants back from its allowlist, and every other tool, a provided one
/// included, reads every scope.
fn allowlist_scopes(name: &str) -> &'static [PermissionScope] {
    if SHELLS.contains(&name) {
        &[
            PermissionScope::CommandPattern,
            PermissionScope::OutsideDirectory,
        ]
    } else {
        &PermissionScope::ALL
    }
}

/// Reference `_covered`: whether `name` already granted the fallback scope,
/// through a stored approval or, for a command, through the allowlist a
/// permanent one wrote. The tool's own rules never read that entry back:
/// they match a parsed command, and this call parsed to nothing.
async fn granted(tools: &ToolRegistry, name: &str, fallback: &PermissionRequirement) -> bool {
    let Some(guard) = tools.guard() else {
        return false;
    };
    let stored = matches!(
        guard
            .policy
            .outcome(name, &PermissionContext::asking(vec![fallback.clone()]))
            .await,
        Ok(PermissionOutcome::Cleared { .. })
    );
    stored
        || (!is_process_id(&fallback.invocation_pattern)
            && guard
                .policy
                .tool_settings(name)
                .allowlist
                .iter()
                .any(|pattern| wildcard_match(&fallback.invocation_pattern, pattern)))
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

/// A smart-approve decision that lets the call run, with its note.
fn smart_run(note: Option<String>) -> GateVerdict {
    GateVerdict::Run {
        approval: smart::noted(
            approval_record("execute", "always", "smart"),
            note.as_deref(),
        ),
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

/// Reference `_PROCESS_UNGATED_BUILTINS`: the reads and the stop of a process
/// the `process.start` gate already let run, which widen nothing.
const UNGATED: [&str; 3] = ["process.output", "process.list", "process.stop"];

/// The session tools whose rules decide a call to `builtin`, or `None` for a
/// call that is not theirs to decide: an ungated process call, or a tool the
/// session provides.
fn gated_sources(builtin: &str) -> Option<&'static [&'static str]> {
    if UNGATED.contains(&builtin) {
        return None;
    }
    BUILTINS
        .iter()
        .find(|(name, _)| *name == builtin)
        .map(|(_, sources)| *sources)
}

/// Reference `vibe_tool_names`: the tools of `sources` the session
/// publishes, sorted.
fn vibe_tool_names<'a>(sources: &[&'a str], available: &[String]) -> Vec<&'a str> {
    let mut names: Vec<&str> = sources
        .iter()
        .copied()
        .filter(|source| available.iter().any(|name| name == source))
        .collect();
    names.sort_unstable();
    names
}

/// Reference `_FALLBACK_SCOPES`: the argument a call to `builtin` is scoped
/// to when no session tool could read it, the command as written for a shell
/// call or a start and the process for a write.
fn fallback_argument(builtin: &str) -> Option<&'static str> {
    match builtin {
        "file_system.bash" | "process.start" => Some("command"),
        "process.write" => Some("processId"),
        _ => None,
    }
}

/// The exact scope a call no session tool could read is asked about under,
/// or `None` when its arguments carry nothing to scope it to: no command at
/// all, or a process id the harness never minted.
fn fallback_scope(builtin: &str, arguments: &Value) -> Option<PermissionRequirement> {
    let key = fallback_argument(builtin)?;
    let scope = arguments.get(key)?.as_str()?;
    if key == "processId" && !is_process_id(scope) {
        return None;
    }
    Some(PermissionRequirement::exact_command(scope))
}

/// Reference `_PROCESS_ID`: an id as the harness mints one,
/// `process-<hex>-<hex>` (`vibe_harness::processes::process_id`), which
/// matches itself and nothing else.
fn is_process_id(text: &str) -> bool {
    let hex = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    };
    text.strip_prefix("process-")
        .and_then(|rest| rest.split_once('-'))
        .is_some_and(|(session, call)| hex(session) && hex(call))
}

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

#[cfg(test)]
mod gate_tests;
