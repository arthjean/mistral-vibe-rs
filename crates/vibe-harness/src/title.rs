//! Background session titles.
//!
//! Reference `vibe/_title.py` and `_maybe_schedule_title_generation` in
//! `vibe/_session.py` of the harness runtime: as model steps accrue, a
//! session a terminal or desktop client opened asks a model to name it from
//! a transcript of its public history, at most twice, and keeps the title
//! unless someone renamed the session meanwhile.
//!
//! The instructions the title model reads are this port's own wording.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use vibe_core::engine::CompletionProvider;
use vibe_core::provider::config::ModelConfig;
use vibe_core::provider::{ProviderInput, RequestLimits};
use vibe_core::pycompat::{self, Dumps};
use vibe_core::session_title::{clean_title, elide_transcript};

use crate::utility_call::UtilityCall;

/// Reference `TitlePolicy`.
const CONFIRM_AFTER_STEPS: usize = 6;
const MAX_GENERATIONS: u32 = 2;
const INITIAL_MAX_STEPS: usize = 3;
const MAX_MESSAGE_CHARS: usize = 2_000;
const MAX_TOOL_CHARS: usize = 400;
const TOTAL_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_TOKENS: u32 = 96;

/// What the title model is told it is for.
const SYSTEM_PROMPT: &str = "Name a coding-agent session. You read a transcript of a user working \
with an AI coding assistant; answer with a short title for the work it is about.

- Three to eight words, sentence case, plain text, no final period, no quotes, markup or emoji.
- Name the session's overall task, not its latest step.
- Put what pins the subject down first (an issue or PR number, a ticket, a file or package name), \
then the topic, and the action last, since the end of a long title gets cut. Without such an \
identifier, open with the action and the topic.
- Drop repository names, owners and URLs; keep the identifiers.
- Lines starting with `tool(` or `tool result(` show what the assistant ran and learned: use the \
facts they reveal to name the topic, never the tool activity itself. Ignore the assistant's \
narration of what it is about to do.
- When the user's first message is only a link or a terse reference, name what it turned out to \
be about.
- Prefer concrete nouns from the code or the domain over generic wording, and the shortest title \
that still says what the work is.
- Write in English whatever language the transcript is in.
- A `Current title:` names the session's task: keep it unless the work turned out to be about \
something else, and sharpen it only with a topic the transcript reveals.
- When the transcript holds no task, answer `New session`.

Answer with the title alone, on one line.";

/// Reference `TitleCadence`: when a session's title is due.
#[derive(Debug, Default)]
pub struct TitleCadence {
    generated: Option<String>,
    last_step: Option<usize>,
    last_compaction: Option<String>,
    attempts: u32,
}

impl TitleCadence {
    /// Reference `is_manual`: a title this cadence did not produce.
    #[must_use]
    pub fn is_manual(&self, current: Option<&str>) -> bool {
        current.is_some_and(|current| self.generated.as_deref() != Some(current))
    }

    /// Reference `begin_if_due`: whether a title is due, recording the
    /// attempt when it is.
    pub fn begin_if_due(
        &mut self,
        step: usize,
        current: Option<&str>,
        compaction: Option<&str>,
        turn_completing: bool,
    ) -> bool {
        if self.is_manual(current) || self.attempts >= MAX_GENERATIONS {
            return false;
        }
        let due = match self.last_step {
            None => {
                step >= INITIAL_MAX_STEPS
                    || turn_completing
                    || compaction.is_some_and(|id| self.last_compaction.as_deref() != Some(id))
            }
            Some(last) => step.saturating_sub(last) >= CONFIRM_AFTER_STEPS && step >= last,
        };
        if !due {
            return false;
        }
        self.attempts += 1;
        self.last_step = Some(step);
        self.last_compaction = compaction.map(str::to_owned);
        true
    }

    /// Reference `record`.
    pub fn record(&mut self, title: &str) {
        self.generated = Some(title.to_owned());
    }
}

/// Reference `count_model_steps`: assistant messages and completed calls
/// that answered something.
#[must_use]
pub fn count_model_steps(entries: &[Value]) -> usize {
    entries
        .iter()
        .filter(|entry| match entry["type"].as_str() {
            Some("message") => entry["role"] == "assistant",
            Some("effect") => {
                entry["generationStatus"] == "completed"
                    && entry["state"]["outputText"]
                        .as_str()
                        .is_some_and(|output| !output.trim().is_empty())
            }
            _ => false,
        })
        .count()
}

/// Reference `latest_compaction_id`.
#[must_use]
pub fn latest_compaction_id(entries: &[Value]) -> Option<String> {
    entries
        .iter()
        .rev()
        .find(|entry| {
            entry["type"] == "checkpoint"
                && entry["kind"] == "compaction"
                && entry["generationStatus"] != "in_progress"
        })
        .and_then(|entry| entry["id"].as_str().map(str::to_owned))
}

/// Reference `build_title_transcript`.
#[must_use]
pub fn transcript(entries: &[Value]) -> String {
    let blocks: Vec<String> = entries
        .iter()
        .filter_map(|entry| match entry["type"].as_str() {
            Some("message") => message_block(entry),
            Some("effect") => tool_block(entry),
            _ => None,
        })
        .collect();
    elide_transcript(blocks.join("\n\n").trim())
}

fn truncated(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn message_block(entry: &Value) -> Option<String> {
    let role = entry["role"]
        .as_str()
        .filter(|role| matches!(*role, "user" | "assistant"))?;
    let text: Vec<&str> = entry["content"]
        .as_array()?
        .iter()
        .filter_map(|part| part["text"].as_str())
        .collect();
    let text = text.join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| format!("{role}: {}", truncated(text, MAX_MESSAGE_CHARS)))
}

fn tool_block(entry: &Value) -> Option<String> {
    let detail = &entry["detail"];
    let name = detail["toolName"]
        .as_str()
        .filter(|name| !name.is_empty())
        .or_else(|| entry["title"].as_str().filter(|title| !title.is_empty()))
        .unwrap_or("tool");
    let mut lines = Vec::new();
    let call = &detail["input"];
    if !call.is_null() {
        let text = call.as_str().map_or_else(
            || {
                pycompat::dumps(
                    call,
                    Dumps {
                        ensure_ascii: false,
                        ..Dumps::DEFAULT
                    },
                )
            },
            str::to_owned,
        );
        let text = text.trim();
        if !text.is_empty() && !matches!(text, "{}" | "[]" | "\"\"") {
            lines.push(format!("tool({name}): {}", truncated(text, MAX_TOOL_CHARS)));
        }
    }
    let state = &entry["state"];
    if let Some(output) = state["outputText"]
        .as_str()
        .map(str::trim)
        .filter(|output| !output.is_empty())
    {
        lines.push(format!(
            "tool result({name}): {}",
            truncated(output, MAX_TOOL_CHARS)
        ));
    } else if state["status"] == "failed"
        && let Some(message) = state["error"]["message"]
            .as_str()
            .map(str::trim)
            .filter(|message| !message.is_empty())
    {
        lines.push(format!(
            "tool result({name}) failed: {}",
            truncated(message, MAX_TOOL_CHARS)
        ));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// Where a title is asked for.
pub struct TitleRoute {
    pub provider: Arc<dyn CompletionProvider>,
    pub model: ModelConfig,
    pub metadata: BTreeMap<String, String>,
}

impl TitleRoute {
    /// Reference `generate_session_title`: `None` on a failure, a timeout,
    /// or an answer that names nothing.
    pub async fn generate(
        &self,
        session_id: &str,
        entries: &[Value],
        previous: Option<&str>,
        message_id: Option<&str>,
    ) -> Option<String> {
        let transcript = transcript(entries);
        if transcript.is_empty() {
            return None;
        }
        let prompt = match previous.filter(|title| !title.is_empty()) {
            Some(previous) => format!("Current title: {previous}\n\nTranscript:\n{transcript}"),
            None => transcript,
        };
        let call = UtilityCall {
            model: &self.model,
            metadata: &self.metadata,
            session_id,
            call_type: "title_generation",
            message_id,
            system_prompt: SYSTEM_PROMPT,
        };
        let input = ProviderInput {
            limits: RequestLimits {
                max_tokens: Some(MAX_TOKENS),
                temperature_millis: None,
            },
            ..call.input(prompt)
        };
        let answer = tokio::time::timeout(TOTAL_TIMEOUT, self.provider.complete(&input))
            .await
            .ok()?
            .ok()?;
        clean_title(Some(&answer.text))
    }
}

#[cfg(test)]
mod title_tests;
