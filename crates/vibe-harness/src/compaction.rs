//! Compacting a session's context into a handoff summary.
//!
//! Reference `features/compaction/` in `harness/core/src/core/`: the model is
//! asked, with the session's own tools offered but refused, for a summary of
//! the work so far. An accepted summary replaces the conversation with the
//! latest messages the person wrote, framed by a preamble and the summary; a
//! response with tool calls, no text or broken `<summary>` delimiters is asked
//! for once more, then fails the compaction. The prose the model reads here is
//! this port's own.

use serde_json::Value;

use crate::engine::ConversationMessage;

/// Reference `MAX_SUMMARY_RETRIES`: one more request after a rejected
/// summary.
pub const MAX_ATTEMPTS: u64 = 2;

/// Reference `USER_MESSAGE_MAX_TOKENS`: how much of what the person wrote a
/// replacement keeps.
const USER_MESSAGE_MAX_TOKENS: usize = 20_000;

/// Reference `APPROX_BYTES_PER_TOKEN`.
const BYTES_PER_TOKEN: usize = 4;

const SUMMARY_OPEN: &str = "<summary>";
const SUMMARY_CLOSE: &str = "</summary>";
const EXTRA_OPEN: &str = "<extra_compaction_instructions>";
const EXTRA_CLOSE: &str = "</extra_compaction_instructions>";

/// What the model is asked to do; this port's own wording of the
/// reference's directives.
const PROMPT: &str = "Answer in plain text and make no tool calls: any call is refused.

The conversation above is about to be replaced by a summary, and another model will pick \
the work up from that summary alone. Write the handoff it needs:
- what the user wants and every constraint or preference they stated
- what has been done, the decisions taken and why
- the files read or changed, one line each unless a snippet matters
- what is left, and the very next step

Keep it short and structured, say nothing twice, and do not add a closing answer: the \
summary is the whole handoff. Put it between the delimiters below.

<summary>
the handoff summary
</summary>";

const PRESERVED_OPEN: &str = "The conversation so far was compacted, and you are picking it up \
from here.

The latest messages the user wrote follow, verbatim where they fit. They are background, \
not new requests.

<previous_user_messages>";

const PRESERVED_CLOSE: &str = "</previous_user_messages>";

/// The message that asks for the summary, with the client's extra
/// instructions when it gave some.
#[must_use]
pub fn prompt(extra_instructions: &str) -> String {
    let extra = extra_instructions.trim();
    if extra.is_empty() {
        PROMPT.to_owned()
    } else {
        format!("{PROMPT}\n\n{EXTRA_OPEN}\n{extra}\n{EXTRA_CLOSE}")
    }
}

/// Why a summary was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    EmptyResponse,
    MalformedDelimiters,
    ToolCalls,
}

impl Rejection {
    /// Reference `invalid_summary_error`'s message.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::EmptyResponse => "compaction response was empty",
            Self::MalformedDelimiters => {
                "compaction response contained malformed <summary> delimiters"
            }
            Self::ToolCalls => "compaction response emitted tool calls",
        }
    }
}

/// Reference `extract_summary`: the text between the delimiters, or the
/// whole answer when it carries none.
///
/// # Errors
///
/// An empty answer, or delimiters that do not enclose a summary.
pub fn extract_summary(response: &str, called_tools: bool) -> Result<String, Rejection> {
    if called_tools {
        return Err(Rejection::ToolCalls);
    }
    let response = response.trim();
    if response.is_empty() {
        return Err(Rejection::EmptyResponse);
    }
    let Some(start) = response.find(SUMMARY_OPEN) else {
        return if response.contains(SUMMARY_CLOSE) {
            Err(Rejection::MalformedDelimiters)
        } else {
            Ok(response.to_owned())
        };
    };
    let content = &response[start + SUMMARY_OPEN.len()..];
    let Some(end) = content.find(SUMMARY_CLOSE) else {
        return Err(Rejection::MalformedDelimiters);
    };
    let summary = content[..end].trim();
    if summary.is_empty() {
        return Err(Rejection::MalformedDelimiters);
    }
    Ok(summary.to_owned())
}

/// Reference `fitted_replacement` without a token threshold: the preamble,
/// the latest messages the person wrote within the budget, and the summary.
#[must_use]
pub fn replacement(
    conversation: &[ConversationMessage],
    summary: &str,
) -> Vec<ConversationMessage> {
    let injected = |text: String| ConversationMessage::User {
        content: vec![serde_json::json!({"type": "text", "text": text})],
        injected: true,
    };
    let mut messages = vec![injected(PRESERVED_OPEN.to_owned())];
    messages.extend(preserved_user_messages(conversation));
    messages.push(injected(format!(
        "{PRESERVED_CLOSE}\n\nWhat happened so far, summarized:\n\n<compaction_summary>\n{summary}\n</compaction_summary>"
    )));
    messages
}

/// Reference `collect_user_messages`: the newest messages first until the
/// budget runs out, a text message cut to what is left, at least one kept.
fn preserved_user_messages(conversation: &[ConversationMessage]) -> Vec<ConversationMessage> {
    let mut remaining = USER_MESSAGE_MAX_TOKENS;
    let mut selected = Vec::new();
    for message in conversation.iter().rev() {
        let ConversationMessage::User {
            content,
            injected: false,
        } = message
        else {
            continue;
        };
        if remaining == 0 {
            if selected.is_empty() {
                selected.push(message.clone());
            }
            break;
        }
        let cost = content.iter().map(block_tokens).sum::<usize>();
        if cost <= remaining {
            selected.push(message.clone());
            remaining -= cost;
        } else if let Some(truncated) = truncated(content, remaining) {
            selected.push(truncated);
            break;
        } else {
            if selected.is_empty() {
                selected.push(message.clone());
            }
            break;
        }
    }
    selected.reverse();
    selected
}

fn block_tokens(block: &Value) -> usize {
    match block["text"].as_str() {
        Some(text) if block["type"] == "text" => text.len().div_ceil(BYTES_PER_TOKEN),
        _ => serde_json::to_string(block).map_or(0, |text| text.len().div_ceil(BYTES_PER_TOKEN)),
    }
}

/// A text-only message cut to the last `tokens` worth of its text.
fn truncated(content: &[Value], tokens: usize) -> Option<ConversationMessage> {
    if content.iter().any(|block| block["type"] != "text") {
        return None;
    }
    let text = content
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let keep = tokens * BYTES_PER_TOKEN;
    let mut start = text.len().saturating_sub(keep);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    Some(ConversationMessage::User {
        content: vec![serde_json::json!({"type": "text", "text": &text[start..]})],
        injected: false,
    })
}

#[cfg(test)]
mod compaction_tests;
