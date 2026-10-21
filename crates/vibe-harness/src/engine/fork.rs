//! New sessions started from a conversation: a fork of another session's,
//! or a legacy transcript imported.

use serde_json::{Value, json};

use super::SessionCore;
use super::conversation::{ConversationMessage, client_message_position, untagged};
use crate::state::HarnessState;

/// Reference `fork` on the harness host: a new session whose conversation
/// is `source`'s, imported rather than recorded, so its public history
/// numbers each message by position (`_public_state`) and holds no turns.
#[must_use]
pub fn fork_core(
    source: &SessionCore,
    session_id: &str,
    now: u64,
    scratchpad: std::path::PathBuf,
) -> SessionCore {
    fork_core_until(
        source,
        session_id,
        now,
        scratchpad,
        source.conversation.len(),
    )
}

/// Reference `fork` with an anchor left out: the conversation before the
/// user message a client named `entry_id`.
///
/// # Errors
///
/// No user message carries that identifier.
pub fn fork_core_before(
    source: &SessionCore,
    session_id: &str,
    now: u64,
    scratchpad: std::path::PathBuf,
    entry_id: &str,
) -> Result<SessionCore, String> {
    let end = client_message_position(&source.conversation, entry_id)
        .ok_or_else(|| format!("Cannot find user entry: {entry_id}"))?;
    Ok(fork_core_until(source, session_id, now, scratchpad, end))
}

/// Reference `fork` with the anchor kept (`include_entry=True`): the
/// conversation through the turn the user message `entry_id` opened, up to
/// the next user message.
///
/// # Errors
///
/// No user message carries that identifier.
pub fn fork_core_through(
    source: &SessionCore,
    session_id: &str,
    now: u64,
    scratchpad: std::path::PathBuf,
    entry_id: &str,
) -> Result<SessionCore, String> {
    let anchor = client_message_position(&source.conversation, entry_id)
        .ok_or_else(|| format!("Cannot find user entry: {entry_id}"))?;
    let end = source.conversation[anchor + 1..]
        .iter()
        .position(|message| matches!(message, ConversationMessage::User { .. }))
        .map_or(source.conversation.len(), |offset| anchor + 1 + offset);
    Ok(fork_core_until(source, session_id, now, scratchpad, end))
}

fn fork_core_until(
    source: &SessionCore,
    session_id: &str,
    now: u64,
    scratchpad: std::path::PathBuf,
    end: usize,
) -> SessionCore {
    let conversation = &source.conversation[..end];
    let source_state = &source.projector.state.session;
    let mut state = HarnessState::new(
        session_id,
        Some(source_state.id.as_str()),
        &source_state.root_session_id,
        now,
    );
    state.entries = imported_entries(conversation, session_id, now);
    state.session.preview = crate::state::first_user_message_preview(&state.entries);
    let mut core = SessionCore::new(state, scratchpad);
    core.conversation = conversation.to_vec();
    core.pins = source.pins.clone();
    core.without_plugins = source.without_plugins;
    // Reference `copy_scheduled_loops`: a fork keeps its source's schedule.
    let _ = core.loops.replace(source.loops.list());
    core
}

/// Reference `import_legacy` on the harness host: a new root session whose
/// conversation is a legacy transcript, imported as a fork's is.
#[must_use]
pub fn import_core(
    session_id: &str,
    now: u64,
    scratchpad: std::path::PathBuf,
    conversation: Vec<ConversationMessage>,
    model: Option<String>,
) -> SessionCore {
    let mut state = HarnessState::new(session_id, None, session_id, now);
    state.entries = imported_entries(&conversation, session_id, now);
    state.session.preview = crate::state::first_user_message_preview(&state.entries);
    let mut core = SessionCore::new(state, scratchpad);
    core.conversation = conversation;
    if let Some(model) = model {
        core.pins.insert("model".to_owned(), model);
    }
    core.without_plugins = true;
    core
}

/// The public entries of an imported conversation: one per user or
/// assistant message with content, tool messages and calls left out.
fn imported_entries(
    conversation: &[ConversationMessage],
    session_id: &str,
    now: u64,
) -> Vec<Value> {
    conversation
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            let (role, content) = match message {
                ConversationMessage::User { content, .. } => ("user", untagged(content)),
                ConversationMessage::Assistant { text, .. } if !text.is_empty() => {
                    ("assistant", vec![json!({"type": "text", "text": text})])
                }
                _ => return None,
            };
            if content.is_empty() {
                return None;
            }
            let mut entry = json!({
                "type": "message",
                "id": format!("imported-{}", index + 1),
                "sessionId": session_id,
                "turnId": null,
                "createdAt": now,
                "updatedAt": now,
                "generationStatus": "completed",
                "relatedEntryId": null,
                "inputEntryId": null,
                "role": role,
                "content": content,
                "source": "harness",
            });
            if role == "assistant" {
                entry["outcome"] = json!({"type": "committed"});
            }
            Some(entry)
        })
        .collect()
}
