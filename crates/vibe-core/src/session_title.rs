//! Background session titles.
//!
//! Reference `vibe/core/session/title_model.py`, `title_policy.py` and
//! `vibe/core/agent_loop/_title_cadence.py`. After a model step a session may
//! ask the utility model to name it: the first title once the opening turn
//! answers (or after a few steps of a tool-heavy one), then a refresh every
//! few steps and after a compaction while the fast model serves it. A title
//! the model does not produce is `None`, and a failure never reaches the turn.

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde_json::{Map, Value};

use crate::events::ModelMessage;
use crate::llm::BackendContext;
use crate::llm::utility::{self, UtilityRequest, UtilitySelection};
use crate::observability::{self, LogLevel};
use crate::prompt::library::UtilityPrompt;

/// Reference `TitlePolicy.refresh_every_steps`.
pub const REFRESH_EVERY_STEPS: u64 = 6;
/// Reference `TitlePolicy.capped_max_generations`.
pub const CAPPED_MAX_GENERATIONS: u64 = 2;
/// Reference `TitlePolicy.initial_max_steps`.
pub const INITIAL_MAX_STEPS: u64 = 3;
/// Reference `TitlePolicy.max_transcript_chars`.
const MAX_TRANSCRIPT_CHARS: usize = 6_000;
/// Reference `TitlePolicy.head_transcript_chars`.
const HEAD_TRANSCRIPT_CHARS: usize = 1_500;
/// Reference `TitlePolicy.max_message_chars`.
const MAX_MESSAGE_CHARS: usize = 2_000;
/// Reference `TitlePolicy.request_timeout_seconds`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);
/// Reference `TitlePolicy.retry_budget_seconds`.
const RETRY_BUDGET: Duration = Duration::from_secs(10);
/// Reference `TitlePolicy.total_timeout_seconds`.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(20);
/// Reference `TitlePolicy.max_tokens`.
const MAX_TOKENS: u64 = 96;
/// Reference `TitlePolicy.max_title_chars`.
const MAX_TITLE_CHARS: usize = 72;
/// Reference `TitlePolicy.generic_titles`: answers that name nothing.
const GENERIC_TITLES: [&str; 3] = ["new session", "untitled session", "untitled"];
/// What stands between the head and the tail of a clipped transcript.
const ELISION: &str = "\n\n[…]\n\n";
/// Quotes a model may wrap its answer in.
const WRAPPING_QUOTES: &[char] = &['"', '\'', '`', '“', '”', '‘', '’'];

/// Reference `_DISABLE_AUTO_TITLE_ENV_VAR`: set to `1`, no title is generated.
pub const DISABLE_ENVIRONMENT: &str = "VIBE_TEST_DISABLE_AUTO_TITLE";

static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    Regex::new(r"\s+").expect("the whitespace pattern compiles")
});

/// Reference `TitleCadence`: when a background title refresh is due.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TitleCadence {
    step_index: u64,
    last_generation_index: u64,
    compacted_since_generation: bool,
    generations: u64,
}

/// A refresh [`TitleCadence::begin_if_due`] started, which
/// [`TitleCadence::restore`] takes back when no title lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TitleTicket {
    previous_index: u64,
    due_to_compaction: bool,
}

impl TitleCadence {
    /// A compaction makes the next step due.
    pub fn mark_compaction(&mut self) {
        self.compacted_since_generation = true;
    }

    /// Advances one model step, and answers a ticket when a refresh is due.
    /// `periodic` is whether the fast model serves titles; off it, only the
    /// opening title and post-compaction ones are made, a bounded number of
    /// times.
    pub fn begin_if_due(&mut self, periodic: bool, turn_completing: bool) -> Option<TitleTicket> {
        self.step_index += 1;
        if !periodic && self.generations >= CAPPED_MAX_GENERATIONS {
            return None;
        }
        let initial_pending = self.last_generation_index == 0;
        let initial_due =
            initial_pending && (turn_completing || self.step_index >= INITIAL_MAX_STEPS);
        let due = initial_due
            || self.compacted_since_generation
            || (periodic
                && !initial_pending
                && self.step_index - self.last_generation_index >= REFRESH_EVERY_STEPS);
        if !due {
            return None;
        }
        let ticket = TitleTicket {
            previous_index: self.last_generation_index,
            due_to_compaction: self.compacted_since_generation,
        };
        self.last_generation_index = self.step_index;
        self.compacted_since_generation = false;
        self.generations += 1;
        Some(ticket)
    }

    /// Makes the next step due again; the spent attempt still counts against
    /// the cap.
    pub fn restore(&mut self, ticket: TitleTicket) {
        self.last_generation_index = ticket.previous_index;
        if ticket.due_to_compaction {
            self.compacted_since_generation = true;
        }
    }
}

/// Reference `build_title_transcript`: every message but the system prompt,
/// each clamped, keeping the opening and the latest exchange when the whole is
/// too long.
#[must_use]
pub fn build_title_transcript(messages: &[ModelMessage]) -> String {
    let blocks: Vec<String> = messages
        .iter()
        .filter_map(|message| {
            let (role, content) = match message {
                ModelMessage::System { .. } => return None,
                ModelMessage::User { content, .. } => ("user", content),
                ModelMessage::Assistant { content, .. } => ("assistant", content),
                ModelMessage::Tool { content, .. } => ("tool", content),
            };
            let text = content.trim();
            if text.is_empty() {
                return None;
            }
            let text: String = text.chars().take(MAX_MESSAGE_CHARS).collect();
            Some(format!("{role}: {text}"))
        })
        .collect();
    let transcript = blocks.join("\n\n").trim().to_owned();
    let length = transcript.chars().count();
    if length <= MAX_TRANSCRIPT_CHARS {
        return transcript;
    }
    let head: String = transcript.chars().take(HEAD_TRANSCRIPT_CHARS).collect();
    let tail: String = transcript
        .chars()
        .skip(length - (MAX_TRANSCRIPT_CHARS - HEAD_TRANSCRIPT_CHARS))
        .collect();
    format!("{}{ELISION}{}", head.trim_end(), tail.trim_start())
}

/// The user message: the transcript, behind the title it may refine.
fn user_prompt(transcript: &str, previous_title: Option<&str>) -> String {
    match previous_title.filter(|title| !title.is_empty()) {
        Some(title) => format!("Previous title: {title}\n\nConversation:\n{transcript}"),
        None => transcript.to_owned(),
    }
}

/// Reference `_clean_title`: the first line, without control characters,
/// runs of whitespace or wrapping quotes, capped in length; a generic answer
/// is no answer.
#[must_use]
pub fn clean_title(content: Option<&str>) -> Option<String> {
    let first_line = content?.trim().lines().next().unwrap_or_default();
    let printable: String = first_line
        .chars()
        .filter(|character| !matches!(u32::from(*character), 0x00..=0x1f | 0x7f..=0x9f))
        .collect();
    let collapsed = WHITESPACE.replace_all(&printable, " ");
    let collapsed = collapsed.trim().trim_matches(WRAPPING_QUOTES).trim();
    if collapsed.is_empty() || GENERIC_TITLES.contains(&collapsed.to_lowercase().as_str()) {
        return None;
    }
    if collapsed.chars().count() > MAX_TITLE_CHARS {
        let head: String = collapsed.chars().take(MAX_TITLE_CHARS).collect();
        return Some(format!("{}…", head.trim_end()));
    }
    Some(collapsed.to_owned())
}

/// Reference `generate_session_title`: the utility model's title for
/// `messages`, refining `previous_title` when there is one. Answers `None`
/// for an empty transcript, an unusable answer, a failure or a timeout; the
/// failure is logged.
pub async fn generate_session_title(
    messages: &[ModelMessage],
    previous_title: Option<&str>,
    selection: &UtilitySelection,
    context: &BackendContext,
) -> Option<String> {
    let transcript = build_title_transcript(messages);
    if transcript.is_empty() {
        return None;
    }
    let user_content = user_prompt(&transcript, previous_title);
    let metadata = Map::from_iter([("call_type".to_owned(), Value::from("secondary_call"))]);
    let request = UtilityRequest {
        system_prompt: UtilityPrompt::SessionTitle.text(),
        user_content: &user_content,
        max_tokens: MAX_TOKENS,
        request_timeout: REQUEST_TIMEOUT,
        retry_budget: RETRY_BUDGET,
        skip_if_no_key: false,
        metadata: &metadata,
    };
    match tokio::time::timeout(
        TOTAL_TIMEOUT,
        utility::complete(selection, context, &request),
    )
    .await
    {
        Ok(Ok(content)) => clean_title(content.as_deref()),
        Ok(Err(error)) => {
            observability::log(
                LogLevel::Warning,
                &format!("Session title generation failed: {error}"),
            );
            None
        }
        Err(_) => {
            observability::log(LogLevel::Warning, "Session title generation timed out");
            None
        }
    }
}

#[cfg(test)]
mod session_title_tests;
