//! Smart approve: a model classifier that decides, call by call, whether a
//! tool call runs without asking.
//!
//! Reference `vibe/_smart_approve.py`, `vibe/_protected_paths.py` and the
//! classify gate of `vibe/_local_actions.py` in the harness runtime. The
//! classifier answers safe or risky; a risky call the person did not clearly
//! ask for fails back to the model with the reason, and one they did, one the
//! model repeats, or one past the denial limits asks the person. A failed or
//! unreadable classification never runs a call on its own.
//!
//! The classifier's instructions and the message it reads are this port's own
//! wording.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use vibe_core::engine::CompletionProvider;
use vibe_core::provider::config::ModelConfig;
use vibe_core::provider::{ProviderChunk, ProviderInput};
use vibe_core::pycompat::{self, Dumps};

use crate::engine::ConversationMessage;
use crate::utility_call::UtilityCall;

/// Reference `_CLASSIFY_TIMEOUT_S`.
const CLASSIFY_TIMEOUT: Duration = Duration::from_secs(10);
/// Reference `_MAX_ARGS_CHARS` and `_MAX_USER_MESSAGE_CHARS`.
const MAX_ARGS_CHARS: usize = 3000;
const MAX_USER_MESSAGE_CHARS: usize = 2000;
/// Reference `_SMART_DENY_CONSECUTIVE_LIMIT` and `_SMART_DENY_TOTAL_LIMIT`.
const DENY_CONSECUTIVE_LIMIT: u32 = 3;
const DENY_TOTAL_LIMIT: u32 = 20;
/// The tool whose answers clarify what the person wants.
const CLARIFICATION_TOOL: &str = "ask_user_question";

/// Reference `_ERROR_REASON`.
pub const ERROR_REASON: &str = "Smart approve could not verify this call is safe, so it was skipped. \
Try a narrower, clearly safe alternative.";
/// The reason a risky verdict that gave none is escalated with.
pub const UNCONFIRMED_REASON: &str = "Smart approve could not confirm this call is safe.";
/// Reference `_SMART_DENY_CONTINUE_HINT`.
const DENY_CONTINUE_HINT: &str = " If a genuinely safer alternative exists, try it. If none does, repeat this \
action -- a repeat asks the user to confirm it, rather than being blocked again.";
/// The note a call the session's own rules allowed carries.
pub const RULES_NOTE: &str = "Auto-approved: allowed by your permission rules";
/// The note a call a person approved earlier in the session carries.
pub const SESSION_NOTE: &str = "Approved earlier this session";

/// What the classifier is told it is for.
const CLASSIFIER_PROMPT: &str = "\
You review one tool call an AI coding assistant wants to make and decide whether it can run \
without the user confirming it first.

Judge the call against what the user asked for. Reading, listing and searching inside the \
user's working area, building, testing and formatting their project, and edits the request \
plainly calls for are safe. Deleting or overwriting what the user did not ask to change, \
reaching outside the working area, sending data over the network, changing credentials, \
system configuration or version control history, and anything irreversible are risky.

Rate the call's risk as low, medium, high or critical, and how directly the user asked for \
this exact action as unknown, low, medium or high. Never treat a critical call as safe, and \
treat a high-risk call as safe only when the user asked for it at least at medium.

Reply with a single JSON object and nothing else:
{\"safe\": true or false, \"reason\": \"one short sentence\", \"risk_level\": \"...\", \
\"user_authorization\": \"...\"}";

/// Reference `ClassificationVerdict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Prompt,
    Error,
}

/// Reference `RiskClassificationResult`, the parts the gate reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub verdict: Verdict,
    pub reason: String,
    /// Reference `user_authorized`: the person asked for this action, so
    /// only they settle it.
    pub user_authorized: bool,
}

impl Classification {
    fn error() -> Self {
        Self {
            verdict: Verdict::Error,
            reason: ERROR_REASON.to_owned(),
            user_authorized: false,
        }
    }
}

/// Reference `RiskClassificationRequest`: only the call, the person's latest
/// message and the clarifications they gave since, and the roots the
/// session writes to.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassificationRequest {
    pub tool_name: String,
    pub args: Value,
    pub user_message: Option<String>,
    pub recent_answers: Vec<String>,
    pub writable_roots: Vec<String>,
}

impl ClassificationRequest {
    /// Reference `build_classification_request` over the conversation.
    #[must_use]
    pub fn new(
        tool_name: &str,
        args: &Value,
        conversation: &[ConversationMessage],
        roots: &[String],
    ) -> Self {
        Self {
            tool_name: tool_name.to_owned(),
            args: args.clone(),
            user_message: recent_user_message(conversation),
            recent_answers: recent_clarifications(conversation),
            writable_roots: roots.to_vec(),
        }
    }

    /// Reference `args_hash` with the tool: what a person's session grant
    /// covers, the exact call they saw.
    #[must_use]
    pub fn key(&self) -> (String, String) {
        let canonical = pycompat::dumps(
            &self.args,
            Dumps {
                ensure_ascii: false,
                sort_keys: true,
                separators: Some((",", ":")),
                ..Dumps::DEFAULT
            },
        );
        (
            self.tool_name.clone(),
            vibe_core::text::sha256_hex(canonical.as_bytes()),
        )
    }

    /// The message the classifier reads.
    fn user_message_text(&self) -> String {
        let args: String = serde_json::to_string(&self.args)
            .unwrap_or_default()
            .chars()
            .take(MAX_ARGS_CHARS)
            .collect();
        let mut parts = vec![
            format!("Tool call: {}", self.tool_name),
            format!("Arguments: {args}"),
        ];
        if let Some(message) = &self.user_message {
            let message: String = message.chars().take(MAX_USER_MESSAGE_CHARS).collect();
            parts.push(format!("What the user asked: {message}"));
        }
        if !self.recent_answers.is_empty() {
            parts.push(format!(
                "Answers the user gave since, which win over the request where they differ:\n{}",
                self.recent_answers
                    .iter()
                    .map(|answer| format!("- {answer}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if !self.writable_roots.is_empty() {
            parts.push(format!(
                "The user's working area, set by the session rather than the assistant:\n{}",
                self.writable_roots
                    .iter()
                    .map(|root| format!("- {root}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        parts.join("\n")
    }
}

/// Reference `_recent_user_message`.
fn recent_user_message(conversation: &[ConversationMessage]) -> Option<String> {
    conversation.iter().rev().find_map(|message| match message {
        ConversationMessage::User { content, .. } => {
            Some(text_blocks(content)).filter(|text| !text.is_empty())
        }
        _ => None,
    })
}

/// Reference `_recent_clarifications`: the answers to the clarification
/// tool since the latest user message.
fn recent_clarifications(conversation: &[ConversationMessage]) -> Vec<String> {
    let mut answers = Vec::new();
    for message in conversation {
        match message {
            ConversationMessage::User { .. } => answers.clear(),
            ConversationMessage::Tool { name, content, .. }
                if name == CLARIFICATION_TOOL && !content.is_empty() =>
            {
                answers.push(content.clone());
            }
            _ => {}
        }
    }
    answers
}

fn text_blocks(content: &[Value]) -> String {
    content
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Where a classification is asked for: the session's provider, the
/// classifier model, and the metadata the session's requests carry.
pub struct Classifier {
    pub provider: Arc<dyn CompletionProvider>,
    pub model: ModelConfig,
    pub metadata: BTreeMap<String, String>,
    pub session_id: String,
}

impl Classifier {
    /// Reference `classify_tool_call`: never `Allow` on a failure.
    pub async fn classify(
        &self,
        request: &ClassificationRequest,
        message_id: Option<&str>,
    ) -> Classification {
        let call = UtilityCall {
            model: &self.model,
            metadata: &self.metadata,
            session_id: &self.session_id,
            call_type: "smart_approve",
            message_id,
            system_prompt: CLASSIFIER_PROMPT,
        };
        let input = ProviderInput {
            stream: true,
            ..call.input(request.user_message_text())
        };
        let streamed = async {
            let mut stream = self.provider.stream(&input).await?;
            let mut text = String::new();
            while let Some(chunk) = stream.chunks.next().await {
                if let ProviderChunk::Text { text: piece } = chunk? {
                    text.push_str(&piece);
                }
            }
            Ok::<String, vibe_core::provider::ProviderError>(text)
        };
        match tokio::time::timeout(CLASSIFY_TIMEOUT, streamed).await {
            Ok(Ok(text)) => parse(&text).unwrap_or_else(|_| Classification::error()),
            _ => Classification::error(),
        }
    }
}

/// Reference `_parse_result`.
///
/// # Errors
///
/// An answer that is not a JSON object with a boolean `safe`.
pub fn parse(text: &str) -> Result<Classification, String> {
    let parsed: Value =
        serde_json::from_str(&strip_code_fences(text)).map_err(|error| error.to_string())?;
    let object = parsed.as_object().ok_or("the answer is not an object")?;
    let safe = match object.get("safe") {
        None => false,
        Some(Value::Bool(safe)) => *safe,
        Some(_) => return Err("`safe` is not a boolean".to_owned()),
    };
    let risk = taxonomy(
        object.get("risk_level"),
        &["low", "medium", "high", "critical"],
    );
    let authorization = taxonomy(
        object.get("user_authorization"),
        &["unknown", "low", "medium", "high"],
    );
    let authorized = matches!(authorization.as_deref(), Some("medium" | "high"));
    let verdict = if !safe
        || risk.as_deref() == Some("critical")
        || (risk.as_deref() == Some("high") && !authorized)
    {
        Verdict::Prompt
    } else {
        Verdict::Allow
    };
    // Python's `str()` of whatever the answer put there.
    let reason = object
        .get("reason")
        .map(pycompat::str_json)
        .unwrap_or_default();
    Ok(Classification {
        verdict,
        reason,
        user_authorized: authorized,
    })
}

/// Reference `_taxonomy_value`.
fn taxonomy(value: Option<&Value>, allowed: &[&str]) -> Option<String> {
    let normalized = value?.as_str()?.trim().to_lowercase();
    allowed.contains(&normalized.as_str()).then_some(normalized)
}

/// Reference `_strip_code_fences`.
fn strip_code_fences(content: &str) -> String {
    let stripped = content.trim();
    let Some(mut body) = stripped.strip_prefix("```") else {
        return stripped.to_owned();
    };
    if let Some((language, rest)) = body.split_once('\n') {
        let language = language.trim();
        if language.is_empty() || language.chars().all(char::is_alphabetic) {
            body = rest;
        }
    }
    let body = body.trim();
    body.strip_suffix("```").unwrap_or(body).trim().to_owned()
}

/// What smart approve remembers across a session's calls.
#[derive(Debug, Default)]
pub struct SmartState {
    turn_id: Option<String>,
    consecutive: u32,
    total: u32,
    denied: BTreeSet<(String, String)>,
    approved: BTreeSet<(String, String)>,
}

impl SmartState {
    /// Reference `_reset_smart_counters_on_new_turn`.
    pub fn enter_turn(&mut self, turn_id: &str) {
        if self.turn_id.as_deref() == Some(turn_id) {
            return;
        }
        self.turn_id = Some(turn_id.to_owned());
        self.consecutive = 0;
        self.total = 0;
    }

    /// Whether a person already approved this exact call for the session.
    #[must_use]
    pub fn approved(&self, key: &(String, String)) -> bool {
        self.approved.contains(key)
    }

    /// Reference `_smart_proceed`: a call ran, which ends the denial streak.
    pub fn proceed(&mut self) {
        self.consecutive = 0;
    }

    /// Remembers a person's session grant of this exact call.
    pub fn remember(&mut self, key: (String, String)) {
        self.approved.insert(key);
    }

    /// Counts a risky verdict and answers whether it goes to the person
    /// rather than back to the model.
    pub fn deny(&mut self, key: (String, String), protected: bool, authorized: bool) -> bool {
        self.consecutive += 1;
        self.total += 1;
        let repeat = !self.denied.insert(key);
        protected
            || authorized
            || repeat
            || self.consecutive >= DENY_CONSECUTIVE_LIMIT
            || self.total >= DENY_TOTAL_LIMIT
    }
}

/// The message a call refused back to the model carries.
#[must_use]
pub fn deny_message(reason: &str) -> String {
    format!("Smart approve blocked this action: {reason}.{DENY_CONTINUE_HINT}")
}

/// The reason a protected path is never auto-approved.
#[must_use]
pub fn protected_reason(target: &str) -> String {
    format!("This changes {target}, which decides what runs later.")
}

/// Reference `protected_target`: the path this call names that decides what
/// the session runs later, if any.
#[must_use]
pub fn protected_target(tool_name: &str, args: &Value) -> Option<String> {
    let text_argument = match tool_name {
        "file_system.bash" | "process.start" => Some("command"),
        "process.write" => Some("text"),
        _ => None,
    };
    if let Some(key) = text_argument {
        let text = args.get(key)?.as_str()?;
        return text
            .split_whitespace()
            .any(names_protected_path)
            .then(|| text.to_owned());
    }
    if !matches!(
        tool_name,
        "file_system.write_file" | "file_system.search_replace"
    ) {
        return None;
    }
    ["path", "file_path"]
        .iter()
        .filter_map(|key| args.get(*key).and_then(Value::as_str))
        .find(|value| names_protected_path(value))
        .map(ToOwned::to_owned)
}

/// Reference `_names_protected_path`: any spelling of `text` that reaches
/// a protected path.
fn names_protected_path(text: &str) -> bool {
    normalizations(text)
        .iter()
        .any(|form| matches_protected(form))
}

/// Reference `_normalisations`: the text, its joining noise removed, then
/// each `..` cancellation one at a time.
fn normalizations(text: &str) -> Vec<String> {
    let mut forms = vec![text.to_owned()];
    let mut joined = text.replace('\\', "/");
    while joined.contains("//") {
        joined = joined.replace("//", "/");
    }
    loop {
        let next = remove_dot_segments(&joined);
        if next == joined {
            break;
        }
        joined = next;
    }
    forms.push(joined.clone());
    while let Some(cancelled) = cancel_parent(&joined) {
        joined = cancelled;
        forms.push(joined.clone());
    }
    forms
}

/// `/./` joins what it sits between: drops each `/.` followed by `/`.
fn remove_dot_segments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'/'
            && bytes.get(index + 1) == Some(&b'.')
            && bytes.get(index + 2) == Some(&b'/')
        {
            index += 2;
            continue;
        }
        let character = text[index..].chars().next().unwrap_or_default();
        out.push(character);
        index += character.len_utf8().max(1);
    }
    out
}

/// The first `segment/..` (followed by `/` or the end) removed, if any.
fn cancel_parent(text: &str) -> Option<String> {
    let mut search = 0;
    while let Some(found) = text[search..].find("/..") {
        let at = search + found;
        let after = at + 3;
        let ends = after == text.len() || text.as_bytes()[after] == b'/';
        let start = text[..at].rfind('/').map_or(0, |slash| slash + 1);
        if ends && start < at {
            let end = if after < text.len() { after + 1 } else { after };
            return Some(format!("{}{}", &text[..start], &text[end..]));
        }
        search = at + 1;
    }
    None
}

/// Reference `_PROTECTED_PATH`, matched by hand: `.vibe` outside its
/// worktrees, `.git/hooks`, `.git/config`, and the shell startup files.
fn matches_protected(text: &str) -> bool {
    let lower = text.to_lowercase();
    let word = |character: char| character.is_alphanumeric() || character == '_';
    let open = |at: usize| {
        lower[..at]
            .chars()
            .next_back()
            .is_none_or(|before| !(word(before) || before == '.' || before == '-'))
    };
    let closed = |end: usize| {
        lower[end..]
            .chars()
            .next()
            .is_none_or(|after| !(word(after) || after == '-'))
    };
    let found = |needle: &str, extra: &dyn Fn(usize) -> bool| {
        lower
            .match_indices(needle)
            .any(|(at, _)| open(at) && closed(at + needle.len()) && extra(at + needle.len()))
    };
    let outside_worktrees = |end: usize| {
        let rest = &lower[end..];
        let Some(tail) = rest
            .strip_prefix("/worktrees")
            .or_else(|| rest.strip_prefix("\\worktrees"))
        else {
            return true;
        };
        !(tail.is_empty() || tail.starts_with('/') || tail.starts_with('\\'))
    };
    if found(".vibe", &outside_worktrees) {
        return true;
    }
    let any = |_: usize| true;
    [
        ".git/hooks",
        ".git/config",
        ".bashrc",
        ".zshrc",
        ".bash_profile",
        ".zshenv",
        ".zprofile",
        ".profile",
    ]
    .iter()
    .any(|needle| found(needle, &any))
}

/// The approval record a gate decision carries, with the note that rides
/// the call's result.
#[must_use]
pub fn noted(mut approval: Value, note: Option<&str>) -> Value {
    if let (Some(note), Some(object)) = (note, approval.as_object_mut()) {
        object.insert("note".to_owned(), json!(note));
    }
    approval
}

#[cfg(test)]
mod smart_tests;
