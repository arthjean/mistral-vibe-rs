//! Running a chain of hooks and reading what each one decided.
//!
//! Reference `vibe/core/hooks/manager.py` runs every hook of the invocation's
//! type that matches it, in declaration order, and hands each result to the
//! type's handler (`_pre_tool.py`, `_post_tool.py`, `_post_agent.py`): a
//! non-zero exit, a timeout or an answer that is not the response object is a
//! failure, which warns unless the hook is strict; an empty answer passes; and
//! a response is read by the handler, which may rewrite the invocation the
//! next hook receives or end the chain.
//!
//! The chain is a stream the caller consumes as it goes: a pre-tool caller
//! validates each rewrite as it arrives and stops consuming at the first
//! denial, which is why a denied chain reports no run end.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{Map, Value};

use super::executor::run_hook;
use super::models::{
    HookConfig, HookEvent, HookExecutionResult, HookInvocation, HookSeverity, HookType, HookYield,
};
use crate::matching::NameFilter;
use crate::mcp::render::python_float;

/// How many times one post-agent hook may send the model back per user turn.
/// Reference `_MAX_RETRIES`.
pub const MAX_RETRIES: u32 = 3;

/// The hooks one session runs. Reference `HooksManager`.
#[derive(Debug)]
pub struct HooksManager {
    hooks: Vec<HookConfig>,
    cwd: PathBuf,
    /// Reference `HookRetryState`: the retries each post-agent hook asked for
    /// this user turn.
    retries: Mutex<HashMap<String, u32>>,
}

/// What one hook's result makes the chain do. Reference `_HookAction`.
struct HookAction {
    yields: Vec<HookYield>,
    /// The invocation the next hook receives, when this one changed it.
    next: Option<HookInvocation>,
    stop: bool,
}

impl HookAction {
    fn completed(hook: &HookConfig, status: HookSeverity, content: Option<String>) -> Self {
        Self {
            yields: vec![completed(hook, status, content)],
            next: None,
            stop: false,
        }
    }
}

/// A completion event, routed by the caller once the chain knows its scope.
fn completed(hook: &HookConfig, status: HookSeverity, content: Option<String>) -> HookYield {
    HookYield::Event(HookEvent::Completed {
        hook_name: hook.name.clone(),
        status,
        content,
        scope: hook.hook_type,
        tool_call_id: None,
    })
}

/// The response a hook answers with on stdout. Reference
/// `HookStructuredResponse`.
#[derive(Debug, Default)]
struct StructuredResponse {
    deny: bool,
    reason: Option<String>,
    system_message: Option<String>,
    tool_input: Option<Map<String, Value>>,
    additional_context: Option<String>,
}

impl HooksManager {
    #[must_use]
    pub fn new(hooks: Vec<HookConfig>, cwd: PathBuf) -> Self {
        Self {
            hooks,
            cwd,
            retries: Mutex::new(HashMap::new()),
        }
    }

    /// Reference `has_hooks`.
    #[must_use]
    pub fn has_hooks(&self, hook_type: HookType) -> bool {
        self.hooks.iter().any(|hook| hook.hook_type == hook_type)
    }

    /// Forgets every retry, which a new user turn does. Reference
    /// `reset_retry_count`.
    pub fn reset_retry_count(&self) {
        if let Ok(mut retries) = self.retries.lock() {
            retries.clear();
        }
    }

    /// Runs every hook matching `invocation`, handing each yield to `sink` as
    /// it happens. The chain ends early when `sink` breaks, as a reference
    /// consumer that returns out of its `async for` ends it. Reference
    /// `HooksManager.run`.
    pub async fn run<F>(&self, invocation: HookInvocation, mut sink: F)
    where
        F: FnMut(HookYield) -> ControlFlow<()>,
    {
        let hook_type = invocation.hook_type();
        let hooks = self
            .hooks
            .iter()
            .filter(|hook| hook.hook_type == hook_type && matches(hook, &invocation))
            .collect::<Vec<_>>();
        if hooks.is_empty() {
            return;
        }
        let tool_name = invocation.tool_name().map(ToOwned::to_owned);
        let tool_call_id = invocation.tool_call_id().map(ToOwned::to_owned);
        if sink(HookYield::Event(HookEvent::RunStarted {
            scope: hook_type,
            tool_name: tool_name.clone(),
            tool_call_id: tool_call_id.clone(),
        }))
        .is_break()
        {
            return;
        }
        let mut current = invocation;
        for hook in hooks {
            if sink(HookYield::Event(HookEvent::Started {
                hook_name: hook.name.clone(),
                scope: hook_type,
                tool_call_id: tool_call_id.clone(),
            }))
            .is_break()
            {
                return;
            }
            let result = self
                .execute(
                    hook,
                    &current,
                    tool_name.as_deref(),
                    tool_call_id.as_deref(),
                )
                .await;
            let action = self.process(hook, &current, &result);
            for item in action.yields {
                let item = match item {
                    HookYield::Event(HookEvent::Completed {
                        hook_name,
                        status,
                        content,
                        ..
                    }) => HookYield::Event(HookEvent::Completed {
                        hook_name,
                        status,
                        content,
                        scope: hook_type,
                        tool_call_id: tool_call_id.clone(),
                    }),
                    other => other,
                };
                if sink(item).is_break() {
                    return;
                }
            }
            if let Some(next) = action.next {
                current = next;
            }
            if action.stop {
                break;
            }
        }
        let _ = sink(HookYield::Event(HookEvent::RunCompleted {
            scope: hook_type,
            tool_call_id,
        }));
    }

    /// One hook process under its span. Reference `_run_subprocess`.
    async fn execute(
        &self,
        hook: &HookConfig,
        invocation: &HookInvocation,
        tool_name: Option<&str>,
        tool_call_id: Option<&str>,
    ) -> HookExecutionResult {
        let stdin = invocation.to_json().unwrap_or_default();
        let traced = crate::tracing::hook_span(
            crate::tracing::HookSpan {
                hook_name: &hook.name,
                hook_type: hook.hook_type.label(),
                tool_name,
                tool_call_id,
            },
            async { Ok::<_, String>(run_hook(hook, &stdin, &self.cwd).await) },
        )
        .await;
        traced.unwrap_or_else(|message| HookExecutionResult {
            hook_name: hook.name.clone(),
            exit_code: Some(1),
            stdout: String::new(),
            stderr: message,
            timed_out: false,
        })
    }

    /// Reference `_process_hook_result`.
    fn process(
        &self,
        hook: &HookConfig,
        invocation: &HookInvocation,
        result: &HookExecutionResult,
    ) -> HookAction {
        if result.timed_out || result.exit_code != Some(0) {
            let timed_out = result.timed_out || result.exit_code.is_none();
            return self.failure(
                hook,
                invocation,
                failure_reason(result),
                timed_out.then(|| format!("Timed out after {}s", python_float(hook.timeout))),
            );
        }
        match parse_structured_response(&result.stdout) {
            Err(message) => self.failure(
                hook,
                invocation,
                format!("invalid response: {message}"),
                None,
            ),
            Ok(Some(response)) if response.deny => self.on_deny(hook, invocation, response),
            Ok(Some(response)) => self.on_allow(hook, invocation, response),
            Ok(None) => {
                self.on_passthrough(hook);
                HookAction::completed(hook, HookSeverity::Ok, None)
            }
        }
    }

    /// Reference `_handle_failure`.
    fn failure(
        &self,
        hook: &HookConfig,
        invocation: &HookInvocation,
        reason: String,
        warning: Option<String>,
    ) -> HookAction {
        if hook.strict
            && let Some(escalation) = on_strict_failure(hook, invocation, &reason)
        {
            return escalation;
        }
        self.on_passthrough(hook);
        HookAction::completed(hook, HookSeverity::Warning, Some(warning.unwrap_or(reason)))
    }

    /// Reference `on_passthrough`: only a post-agent hook remembers it.
    fn on_passthrough(&self, hook: &HookConfig) {
        if hook.hook_type == HookType::PostAgent
            && let Ok(mut retries) = self.retries.lock()
        {
            retries.remove(&hook.name);
        }
    }

    /// Reference `_on_deny` of the three handlers.
    fn on_deny(
        &self,
        hook: &HookConfig,
        invocation: &HookInvocation,
        response: StructuredResponse,
    ) -> HookAction {
        let reason = response.reason.unwrap_or_default();
        match invocation {
            HookInvocation::PreTool { tool_name, .. } => HookAction {
                yields: vec![
                    completed(
                        hook,
                        HookSeverity::Error,
                        Some(format!("Denied tool '{tool_name}'")),
                    ),
                    HookYield::ToolDenial {
                        hook_name: hook.name.clone(),
                        content: reason,
                    },
                ],
                next: None,
                stop: true,
            },
            HookInvocation::PostTool { .. } => {
                let text = match response.additional_context {
                    Some(additional) => append_text(&reason, &additional),
                    None => reason,
                };
                HookAction {
                    yields: vec![
                        completed(
                            hook,
                            HookSeverity::Warning,
                            Some(response.system_message.unwrap_or_else(|| {
                                format!("Replaced tool result ({} chars)", text.chars().count())
                            })),
                        ),
                        HookYield::TextReplacement(text.clone()),
                    ],
                    next: Some(with_output_text(invocation, text)),
                    stop: false,
                }
            }
            HookInvocation::PostAgent { .. } => {
                let Ok(mut retries) = self.retries.lock() else {
                    return HookAction::completed(hook, HookSeverity::Error, None);
                };
                let used = retries.get(&hook.name).copied().unwrap_or_default();
                if used >= MAX_RETRIES {
                    return HookAction::completed(
                        hook,
                        HookSeverity::Error,
                        Some(format!(
                            "Failed, retries exhausted ({MAX_RETRIES}/{MAX_RETRIES})"
                        )),
                    );
                }
                let remaining = MAX_RETRIES - used;
                retries.insert(hook.name.clone(), used.saturating_add(1));
                HookAction {
                    yields: vec![
                        completed(
                            hook,
                            HookSeverity::Error,
                            Some(format!(
                                "Failed, retrying ({remaining} {} remaining)",
                                if remaining == 1 { "retry" } else { "retries" }
                            )),
                        ),
                        HookYield::UserMessage(reason),
                    ],
                    next: None,
                    stop: true,
                }
            }
        }
    }

    /// Reference `_on_allow` of the three handlers.
    fn on_allow(
        &self,
        hook: &HookConfig,
        invocation: &HookInvocation,
        response: StructuredResponse,
    ) -> HookAction {
        match invocation {
            HookInvocation::PreTool {
                context,
                tool_name,
                tool_call_id,
                ..
            } => {
                // Reference `_on_allow` only logs a post-tool field a pre-tool hook sent.
                let Some(rewrite) = response.tool_input else {
                    return HookAction::completed(hook, HookSeverity::Ok, response.system_message);
                };
                let rewrite = Value::Object(rewrite);
                HookAction {
                    yields: vec![
                        completed(
                            hook,
                            HookSeverity::Warning,
                            Some(response.system_message.unwrap_or_else(|| {
                                format!("Rewrote tool_input for '{tool_name}'")
                            })),
                        ),
                        HookYield::ToolInputRewrite {
                            hook_name: hook.name.clone(),
                            tool_input: rewrite.clone(),
                        },
                    ],
                    next: Some(HookInvocation::PreTool {
                        context: context.clone(),
                        tool_name: tool_name.clone(),
                        tool_call_id: tool_call_id.clone(),
                        tool_input: rewrite,
                    }),
                    stop: false,
                }
            }
            HookInvocation::PostTool {
                tool_output_text, ..
            } => {
                // Reference `_on_allow` only logs a pre-tool field a post-tool hook sent.
                let Some(additional) = response.additional_context else {
                    return HookAction::completed(hook, HookSeverity::Ok, response.system_message);
                };
                let text = append_text(tool_output_text, &additional);
                HookAction {
                    yields: vec![
                        completed(
                            hook,
                            HookSeverity::Warning,
                            Some(response.system_message.unwrap_or_else(|| {
                                format!(
                                    "Appended {} chars to tool result",
                                    additional.chars().count()
                                )
                            })),
                        ),
                        HookYield::TextReplacement(text.clone()),
                    ],
                    next: Some(with_output_text(invocation, text)),
                    stop: false,
                }
            }
            HookInvocation::PostAgent { .. } => {
                if let Ok(mut retries) = self.retries.lock() {
                    retries.remove(&hook.name);
                }
                HookAction::completed(hook, HookSeverity::Ok, response.system_message)
            }
        }
    }
}

/// Reference `matches` of the three handlers: a tool hook is limited to the
/// tool names its `match` pattern reaches, `*` when it names none.
fn matches(hook: &HookConfig, invocation: &HookInvocation) -> bool {
    match invocation.tool_name() {
        None => true,
        Some(tool_name) => {
            NameFilter::new(&[hook.matcher.as_deref().unwrap_or("*")]).matches(tool_name)
        }
    }
}

/// Reference `on_strict_failure`: a strict pre-tool hook that fails denies
/// the call, a strict post-tool hook clears the result.
fn on_strict_failure(
    hook: &HookConfig,
    invocation: &HookInvocation,
    reason: &str,
) -> Option<HookAction> {
    match invocation {
        HookInvocation::PreTool { tool_name, .. } => Some(HookAction {
            yields: vec![
                completed(
                    hook,
                    HookSeverity::Error,
                    Some(format!("Denied tool '{tool_name}' (strict)")),
                ),
                HookYield::ToolDenial {
                    hook_name: hook.name.clone(),
                    content: reason.to_owned(),
                },
            ],
            next: None,
            stop: true,
        }),
        HookInvocation::PostTool { .. } => Some(HookAction {
            yields: vec![
                completed(
                    hook,
                    HookSeverity::Error,
                    Some("Cleared tool result (strict)".to_owned()),
                ),
                HookYield::TextReplacement(String::new()),
            ],
            next: Some(with_output_text(invocation, String::new())),
            stop: true,
        }),
        HookInvocation::PostAgent { .. } => None,
    }
}

fn with_output_text(invocation: &HookInvocation, text: String) -> HookInvocation {
    let mut next = invocation.clone();
    if let HookInvocation::PostTool {
        tool_output_text, ..
    } = &mut next
    {
        *tool_output_text = text;
    }
    next
}

/// Reference `_append_text`.
fn append_text(base: &str, addition: &str) -> String {
    if base.is_empty() {
        addition.to_owned()
    } else {
        format!("{base}\n{addition}")
    }
}

/// Reference `_failure_reason`: stderr, since stdout is reserved for the
/// response, then stdout, then the exit code.
fn failure_reason(result: &HookExecutionResult) -> String {
    let Some(code) = result.exit_code.filter(|_| !result.timed_out) else {
        return "timed out".to_owned();
    };
    if !result.stderr.is_empty() {
        return result.stderr.clone();
    }
    if !result.stdout.is_empty() {
        return result.stdout.clone();
    }
    format!("exited with code {code}")
}

/// Reference `_parse_structured_response`: `None` for an empty stdout, the
/// response for an object that validates, and an error for anything else.
fn parse_structured_response(stdout: &str) -> Result<Option<StructuredResponse>, String> {
    if stdout.is_empty() {
        return Ok(None);
    }
    if let Some((message, line, column)) = super::json::decode_error(stdout) {
        return Err(format!(
            "stdout was not valid JSON: {message} at line {line} col {column}"
        ));
    }
    let parsed: Value = match serde_json::from_str(stdout) {
        Ok(parsed) => parsed,
        // Python reads the three non-finite literals `serde_json` refuses.
        Err(_) if super::json::is_non_finite_literal(stdout) => {
            return Err("stdout was a JSON float, expected an object".to_owned());
        }
        Err(_) => {
            return Err("stdout was not valid JSON: Expecting value at line 1 col 1".to_owned());
        }
    };
    let Value::Object(object) = parsed else {
        return Err(format!(
            "stdout was a JSON {}, expected an object",
            python_type_name(&parsed)
        ));
    };
    validate_response(&object).map(Some).map_err(|errors| {
        format!(
            "stdout JSON did not match the hook response schema: {}",
            render_validation_error(&errors)
        )
    })
}

/// The name Python gives the type `json.loads` returns for this value.
fn python_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// One Pydantic error of the response model: where, what kind, the message,
/// and the value it rejected.
struct ResponseError {
    location: &'static str,
    kind: &'static str,
    message: &'static str,
    input: Value,
}

/// Reference `HookStructuredResponse.model_validate`: `decision` is `allow`
/// or `deny`, the texts are strings, and `hook_specific_output` is an object
/// whose `tool_input` is an object and whose `additional_context` is a string.
/// Unknown fields at every level are ignored.
fn validate_response(
    object: &Map<String, Value>,
) -> Result<StructuredResponse, Vec<ResponseError>> {
    let mut errors = Vec::new();
    let mut response = StructuredResponse::default();
    match object.get("decision") {
        None => {}
        Some(Value::String(decision)) if decision == "allow" => {}
        Some(Value::String(decision)) if decision == "deny" => response.deny = true,
        Some(other) => errors.push(ResponseError {
            location: "decision",
            kind: "literal_error",
            message: "Input should be 'allow' or 'deny'",
            input: other.clone(),
        }),
    }
    response.reason = optional_text(object, "reason", "reason", &mut errors);
    response.system_message =
        optional_text(object, "system_message", "system_message", &mut errors);
    match object.get("hook_specific_output") {
        None => {}
        Some(Value::Object(output)) => {
            match output.get("tool_input") {
                None | Some(Value::Null) => {}
                Some(Value::Object(input)) => response.tool_input = Some(input.clone()),
                Some(other) => errors.push(ResponseError {
                    location: "hook_specific_output.tool_input",
                    kind: "dict_type",
                    message: "Input should be a valid dictionary",
                    input: other.clone(),
                }),
            }
            response.additional_context = optional_text(
                output,
                "additional_context",
                "hook_specific_output.additional_context",
                &mut errors,
            );
        }
        Some(other) => errors.push(ResponseError {
            location: "hook_specific_output",
            kind: "model_type",
            message: "Input should be a valid dictionary or instance of HookSpecificOutput",
            input: other.clone(),
        }),
    }
    if errors.is_empty() {
        Ok(response)
    } else {
        Err(errors)
    }
}

fn optional_text(
    object: &Map<String, Value>,
    key: &str,
    location: &'static str,
    errors: &mut Vec<ResponseError>,
) -> Option<String> {
    match object.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(other) => {
            errors.push(ResponseError {
                location,
                kind: "string_type",
                message: "Input should be a valid string",
                input: other.clone(),
            });
            None
        }
    }
}

/// Pydantic's `str(ValidationError)` for the response model.
fn render_validation_error(errors: &[ResponseError]) -> String {
    let mut rendered = format!(
        "{} validation error{} for HookStructuredResponse",
        errors.len(),
        if errors.len() == 1 { "" } else { "s" }
    );
    for error in errors {
        let input = pydantic_input_repr(&error.input);
        rendered.push_str(&format!(
            "\n{}\n  {} [type={}, input_value={}, input_type={}]",
            error.location,
            error.message,
            error.kind,
            input,
            python_type_name(&error.input),
        ));
    }
    rendered
}

/// Pydantic's `input_value`: the Python repr, shortened past fifty characters
/// to its first and last twenty-four around an ellipsis.
fn pydantic_input_repr(value: &Value) -> String {
    let repr = crate::mcp::render::python_repr_of_json(value);
    let characters = repr.chars().collect::<Vec<_>>();
    if characters.len() <= 50 {
        return repr;
    }
    let head = characters.iter().take(25).collect::<String>();
    let tail = characters
        .iter()
        .skip(characters.len().saturating_sub(24))
        .collect::<String>();
    format!("{head}...{tail}")
}
