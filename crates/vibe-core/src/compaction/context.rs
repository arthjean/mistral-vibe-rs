//! The compaction envelope, its parser, and the messages it preserves.
//!
//! Everything here is a total function over strings and message lists: no
//! provider, no filesystem, no clock. That is what lets the calculation be
//! measured against the reference's own answers over scripted inputs, with no
//! backend and no key, before any of it is wired into a turn.
//!
//! The envelope is the part that has to be exact rather than merely equivalent,
//! because it is **persisted**. A conversation compacted through a wrong
//! envelope writes a transcript whose preserved user turns are gone for good,
//! and the envelope is also its own parser: a second compaction reads the first
//! one's block back so the turns it preserved are merged with the newer ones
//! instead of dropped.
//!
//! Reference: `vibe/core/compaction/context.py` at the pinned commit.

use crate::events::ModelMessage;

use super::tokens::{approx_token_count, truncate_middle_to_tokens};

/// How many tokens of the operator's own words survive a compaction.
pub const COMPACT_USER_MESSAGE_MAX_TOKENS: i64 = 20_000;

const PREVIOUS_USER_MESSAGES_OPEN: &str = "<previous_user_messages>";
const PREVIOUS_USER_MESSAGES_CLOSE: &str = "</previous_user_messages>";
const COMPACTION_SUMMARY_OPEN: &str = "<compaction_summary>";
const COMPACTION_SUMMARY_CLOSE: &str = "</compaction_summary>";
const PREVIOUS_USER_MESSAGE_OPEN: &str = "<previous_user_message>";
const PREVIOUS_USER_MESSAGE_CLOSE: &str = "</previous_user_message>";

/// The four tags a preserved message may not carry unescaped, because each one
/// would close or reopen the block that holds it.
const RESERVED_PREVIOUS_USER_MESSAGE_TAGS: [&str; 4] = [
    PREVIOUS_USER_MESSAGES_OPEN,
    PREVIOUS_USER_MESSAGES_CLOSE,
    PREVIOUS_USER_MESSAGE_OPEN,
    PREVIOUS_USER_MESSAGE_CLOSE,
];

const SUMMARY_OPEN: &str = "<summary>";
const SUMMARY_CLOSE: &str = "</summary>";

/// The summary a model wrote, or [`None`] when it wrote none.
///
/// A missing element and an element holding only whitespace are the same
/// answer, which is the empty-summary failure the manager classifies.
#[must_use]
pub fn extract_summary(text: &str) -> Option<String> {
    let open = text.find(SUMMARY_OPEN)?;
    let body = open + SUMMARY_OPEN.len();
    let close = text[body..].find(SUMMARY_CLOSE)? + body;
    let summary = text[body..close].trim();
    if summary.is_empty() {
        None
    } else {
        Some(summary.to_owned())
    }
}

/// `messages` without the oldest round after the system prompt, or [`None`]
/// when only the system prompt and the most recent round remain.
///
/// A round is a run of user messages, the real turn plus anything injected
/// alongside it, together with everything they trigger: assistant replies and
/// tool results, up to the next user message. Dropping the whole round reclaims
/// the tool payloads too and leaves a head that still begins with a user turn.
/// [`None`] is the retry ladder's exhaustion signal, not an error.
#[must_use]
pub fn drop_oldest_round(messages: &[ModelMessage]) -> Option<Vec<ModelMessage>> {
    // Index 0 is the system prompt, which every round keeps.
    let mut index = 1;
    while index < messages.len() && messages[index].is_user() {
        index += 1;
    }
    while index < messages.len() && !messages[index].is_user() {
        index += 1;
    }
    if index >= messages.len() {
        return None;
    }
    let mut trimmed = Vec::with_capacity(messages.len() - index + 1);
    trimmed.push(messages[0].clone());
    trimmed.extend_from_slice(&messages[index..]);
    Some(trimmed)
}

/// The envelope a compaction leaves in place of the conversation it replaced.
///
/// The preamble is this port's own prose rather than the reference's, which
/// `NOTICE` forbids shipping. It carries the same three directives: the
/// trajectory continues after a compaction, the preserved block holds prior
/// context and not new requests, and the summary follows. Everything the
/// envelope's readers depend on is reproduced exactly: the tag names, the line
/// layout, one element per preserved message, the escaping and the summary
/// block. The divergence is recorded as `envelopePreambles/preamble` in the
/// compaction oracle's ledger and in `docs/parity.md`.
#[must_use]
pub fn render_compaction_context(previous_user_messages: &[ModelMessage], summary: &str) -> String {
    let mut lines = vec![
        "The conversation up to this point has been compacted; what follows is what was kept."
            .to_owned(),
        String::new(),
        "These are the operator's most recent messages, kept word for word where the budget \
         allowed. Read them as background already stated, not as fresh instructions."
            .to_owned(),
        String::new(),
        PREVIOUS_USER_MESSAGES_OPEN.to_owned(),
    ];
    for message in previous_user_messages {
        let content = escape_reserved_previous_user_message_tags(message.content());
        lines.push(format!(
            "{PREVIOUS_USER_MESSAGE_OPEN}\n{content}\n{PREVIOUS_USER_MESSAGE_CLOSE}"
        ));
    }
    lines.extend([
        PREVIOUS_USER_MESSAGES_CLOSE.to_owned(),
        String::new(),
        "What happened before this point is summarized below:".to_owned(),
        String::new(),
        COMPACTION_SUMMARY_OPEN.to_owned(),
        summary.to_owned(),
        COMPACTION_SUMMARY_CLOSE.to_owned(),
    ]);
    lines.join("\n")
}

/// The preserved messages an envelope carries, in the order it wrote them.
///
/// Malformed input answers an empty list rather than an error: the envelope is
/// read back from a persisted transcript, and a transcript a newer or older
/// build wrote must never fail a compaction.
#[must_use]
pub fn parse_previous_user_messages(content: &str) -> Vec<String> {
    let Some(block_start) = content.find(PREVIOUS_USER_MESSAGES_OPEN) else {
        return Vec::new();
    };
    let block_start = block_start + PREVIOUS_USER_MESSAGES_OPEN.len();
    let Some(block_end) = content[block_start..].find(PREVIOUS_USER_MESSAGES_CLOSE) else {
        return Vec::new();
    };
    let block = &content[block_start..block_start + block_end];

    // The reference matches `<tag>\n(.*?)\n</tag>`, so both newlines are part
    // of the delimiter and the shortest body wins.
    let open = format!("{PREVIOUS_USER_MESSAGE_OPEN}\n");
    let close = format!("\n{PREVIOUS_USER_MESSAGE_CLOSE}");
    let mut found = Vec::new();
    let mut cursor = 0;
    while let Some(start) = block[cursor..].find(&open) {
        let body = cursor + start + open.len();
        let Some(end) = block[body..].find(&close) else {
            break;
        };
        found.push(block[body..body + end].to_owned());
        cursor = body + end + close.len();
    }
    found
}

/// Whether this message is an envelope a previous compaction wrote.
///
/// All four markers are required: a real user turn quoting one of them is still
/// a real user turn, and treating it as an envelope would replace the operator's
/// words with whatever the quote happened to contain.
#[must_use]
pub fn is_compaction_context_message(message: &ModelMessage) -> bool {
    let content = message.content();
    message.is_injected()
        && content.contains(PREVIOUS_USER_MESSAGES_OPEN)
        && content.contains(PREVIOUS_USER_MESSAGES_CLOSE)
        && content.contains(COMPACTION_SUMMARY_OPEN)
        && content.contains(COMPACTION_SUMMARY_CLOSE)
}

/// The user messages a compaction preserves, newest first within `max_tokens`
/// and returned in transcript order.
///
/// Injected turns are dropped, because the harness can write them again; a
/// previous envelope is parsed and its own preserved turns take its place, so
/// compacting twice merges rather than forgets; and the message that spills over
/// the budget is middle-truncated to exactly what is left and ends the walk.
///
/// `summary_prefix` is the marker an older build wrote in front of an injected
/// summary. It is read as a filter and never as an instruction.
#[must_use]
pub fn collect_prior_user_messages(
    messages: &[ModelMessage],
    summary_prefix: &str,
    max_tokens: i64,
) -> Vec<ModelMessage> {
    let mut candidates: Vec<String> = Vec::new();
    for message in messages {
        let content = message.content();
        if content.is_empty() || !message.is_user() {
            continue;
        }
        if is_compaction_context_message(message) {
            candidates.extend(parse_previous_user_messages(content));
            continue;
        }
        // The marker filter comes first in the reference and is subsumed by the
        // branch under it at this pin, since every injected turn is dropped
        // either way. It is kept as a distinct branch because it is the only
        // place the marker is read, and because an injected turn that stops
        // being dropped wholesale must still drop a stored summary.
        if message.is_injected() && content.starts_with(summary_prefix) {
            continue;
        }
        if message.is_injected() {
            continue;
        }
        candidates.push(content.to_owned());
    }

    let mut selected = Vec::new();
    let mut remaining = max_tokens;
    for content in candidates.iter().rev() {
        if remaining <= 0 {
            break;
        }
        let cost = i64::try_from(approx_token_count(content)).unwrap_or(i64::MAX);
        if cost <= remaining {
            selected.push(ModelMessage::injected_user(content.clone()));
            remaining -= cost;
        } else {
            selected.push(ModelMessage::injected_user(truncate_middle_to_tokens(
                content, remaining,
            )));
            remaining = 0;
        }
    }
    selected.reverse();
    selected
}

/// One reserved tag escaped the way the reference escapes it: `&`, `<` and `>`
/// only, leaving quotes alone.
fn escape_markup(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Every reserved tag in `content` replaced by its escaped form, so the block
/// holding it cannot be closed or reopened by its own content.
fn escape_reserved_previous_user_message_tags(content: &str) -> String {
    let mut escaped = content.to_owned();
    for tag in RESERVED_PREVIOUS_USER_MESSAGE_TAGS {
        escaped = escaped.replace(tag, &escape_markup(tag));
    }
    escaped
}
