//! The summarizer: the calls a compaction makes, and how it reads their answers.
//!
//! The reference orchestrates compaction through one injected callable, so the
//! whole decision tree, the retry ladder, the fallback and the failure
//! classification are observable with no network and no key. This port keeps
//! that shape: the manager is a free function over a [`CompletionProvider`], and
//! the corpus drives it with a scripted one.
//!
//! Three rules hold the design together. The primary call rides the live
//! conversation's own token prefix, so it is cheap, and it works on a copy, so a
//! failure never leaves a partially trimmed transcript behind. The fallback
//! breaks that prefix on purpose, because a dedicated summarizer with no tools
//! and no thinking answers more reliably than a conversation that was about to
//! overflow. And every call retries on overflow by shedding its oldest round,
//! because the condition compaction exists to solve is exactly the one that
//! would otherwise defeat it.
//!
//! Reference: `vibe/core/compaction/manager.py` at the pinned commit.

use std::path::PathBuf;

use crate::events::{ModelMessage, ModelToolCall};
use crate::prompt::library::{UtilityPrompt, load_prompt};
use crate::provider::{
    AssistantMessage, ProviderError, ProviderInput, RequestLimits, ToolChoice, ToolDefinition,
    Usage,
};

use super::context::{
    COMPACT_USER_MESSAGE_MAX_TOKENS, collect_prior_user_messages, drop_oldest_round,
    extract_summary, render_compaction_context,
};
use super::{CompactionFailure, CompactionFailureReason};

/// How many times a summarization call sheds its oldest round and tries again
/// before the overflow is propagated. Reference `_COMPACTION_PTL_RETRIES`.
pub const COMPACTION_OVERFLOW_RETRIES: u32 = 3;

/// The summary a compaction writes when neither call produced one and strict
/// mode is off. The conversation is still compacted: an operator who did not ask
/// for a hard failure gets a shorter transcript with an honest gap in it rather
/// than a turn that fails.
///
/// The wording is this port's own, which is why the oracle records the
/// reference's by digest rather than replaying it.
pub const PLACEHOLDER_SUMMARY: &str = "(no summary was produced)";

/// The heading the extra instructions are appended under.
const EXTRA_INSTRUCTIONS_HEADING: &str = "## Additional Instructions";
/// The heading the fallback's rendered transcript is appended under.
const TRANSCRIPT_HEADING: &str = "## Conversation Transcript";

/// The three texts a compaction sends or filters on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionPrompts {
    /// The compaction request, the one text `compaction_prompt_id` selects.
    pub request: String,
    /// The system message the fallback call runs under.
    pub fallback_system: String,
    /// The marker an older build wrote in front of a stored summary, read as a
    /// filter by the selection and never sent to a model.
    pub summary_prefix: String,
}

impl CompactionPrompts {
    /// The three shipped texts, which is what an unconfigured session sends.
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            request: UtilityPrompt::Compact.text().to_owned(),
            fallback_system: UtilityPrompt::CompactSystem.text().to_owned(),
            summary_prefix: UtilityPrompt::CompactSummaryPrefix.text().to_owned(),
        }
    }
}

impl Default for CompactionPrompts {
    fn default() -> Self {
        Self::builtin()
    }
}

/// What resolving `compaction_prompt_id` produced.
///
/// The reference resolves the request lazily, at each compaction, so an
/// identifier that names nothing is an error the operator meets when a
/// compaction runs rather than when the session opens. This port resolves once
/// per session and carries the failure instead, which raises it at the same
/// moment without rereading the filesystem every cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionPromptResolution {
    Resolved(CompactionPrompts),
    /// The message naming the setting, the value, the built-ins and the
    /// directories searched.
    Failed(String),
}

impl Default for CompactionPromptResolution {
    fn default() -> Self {
        Self::Resolved(CompactionPrompts::builtin())
    }
}

impl CompactionPromptResolution {
    /// The setting whose value a failed resolution names.
    pub const SETTING: &'static str = "compaction_prompt_id";

    /// Resolves `prompt_id` against `directories`, preferring a project
    /// override, then a user one, then the built-in.
    ///
    /// Only the request is selectable: the reference reads the fallback system
    /// prompt and the summary marker from its own package with no override
    /// chain, so both stay built-in here too.
    #[must_use]
    pub fn resolve(prompt_id: &str, directories: &[PathBuf]) -> Self {
        match load_prompt(
            prompt_id,
            Self::SETTING,
            directories,
            &[UtilityPrompt::Compact],
        ) {
            Ok(request) => Self::Resolved(CompactionPrompts {
                request,
                ..CompactionPrompts::builtin()
            }),
            Err(error) => Self::Failed(error.to_string()),
        }
    }
}

/// Everything a compaction needs that is not the transcript.
///
/// It is resolved once per session from the configuration, so the manager stays
/// a total function of what it is handed and never reads a global.
#[derive(Debug, Clone)]
pub struct CompactionPlan {
    /// The three texts, or the reason the request did not resolve.
    pub prompts: CompactionPromptResolution,
    /// The model both calls address, already resolved the way
    /// `get_compaction_model` resolves it. `None` leaves the provider to answer
    /// with the model it was configured for.
    pub model: Option<String>,
    /// Whether the primary call carries the conversation's own thinking setting.
    /// The fallback forces it off regardless.
    pub thinking: bool,
    /// The tools the primary call carries, which is what makes a tool-call
    /// answer possible and therefore classifiable.
    pub tools: Vec<ToolDefinition>,
    /// The tool choice the primary call carries.
    pub tool_choice: Option<ToolChoice>,
    /// Whether a failed summarization fails the compaction instead of degrading
    /// to the placeholder. Reference `raise_on_compaction_failure`.
    pub strict: bool,
    /// The limits both calls are made under.
    pub limits: RequestLimits,
    /// How many tokens of the operator's own words survive.
    pub max_preserved_tokens: i64,
}

impl Default for CompactionPlan {
    fn default() -> Self {
        Self {
            prompts: CompactionPromptResolution::default(),
            model: None,
            thinking: false,
            tools: Vec::new(),
            tool_choice: None,
            strict: false,
            limits: RequestLimits::default(),
            max_preserved_tokens: COMPACT_USER_MESSAGE_MAX_TOKENS,
        }
    }
}

impl CompactionPlan {
    /// The three texts, or the message a failed resolution left behind.
    ///
    /// # Errors
    ///
    /// Returns the resolution failure, which names the setting, the value, the
    /// available built-ins and the directories searched.
    pub fn prompts(&self) -> Result<&CompactionPrompts, CompactionFailure> {
        match &self.prompts {
            CompactionPromptResolution::Resolved(prompts) => Ok(prompts),
            CompactionPromptResolution::Failed(message) => {
                Err(CompactionFailure::from(message.clone()))
            }
        }
    }

    /// The request as it is sent, with `extra_instructions` appended under their
    /// own heading when the caller supplied any.
    #[must_use]
    pub fn request_with(prompts: &CompactionPrompts, extra_instructions: &str) -> String {
        if extra_instructions.is_empty() {
            return prompts.request.clone();
        }
        format!(
            "{}\n\n{EXTRA_INSTRUCTIONS_HEADING}\n{extra_instructions}",
            prompts.request
        )
    }
}

/// What a completed compaction produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummarizedCompaction {
    /// The summary the envelope carries, which is the placeholder when both
    /// calls failed outside strict mode.
    pub summary: String,
    /// The transcript that replaces the conversation: its first message,
    /// followed by the envelope.
    pub messages: Vec<ModelMessage>,
    /// Every call this compaction made, so the turn's ceilings cover them.
    pub usage: Usage,
    /// The classified failure, when the summary is the placeholder. A compaction
    /// that produced a real summary carries none.
    pub failure: Option<CompactionFailureReason>,
}

/// Summarizes `messages` into an envelope, or reports why it could not.
///
/// The live transcript is never touched: the calls are made on a copy, and the
/// replacement is returned rather than applied, so a failure leaves the
/// conversation exactly as it was.
///
/// # Errors
///
/// Returns a classified [`CompactionFailure`] when strict mode refuses to
/// degrade, and an unclassified one when a call failed for a reason the
/// summarizer does not name: a transport error, a refusal, or an overflow the
/// retry ladder could not shed its way out of.
pub async fn compact(
    provider: &dyn crate::engine::CompletionProvider,
    plan: &CompactionPlan,
    messages: &[ModelMessage],
    extra_instructions: &str,
) -> Result<SummarizedCompaction, CompactionFailure> {
    let prompts = plan.prompts()?;
    let preserved =
        collect_prior_user_messages(messages, &prompts.summary_prefix, plan.max_preserved_tokens);
    let request = CompactionPlan::request_with(prompts, extra_instructions);
    let mut usage = Usage::default();

    let summarized = summarize(provider, plan, prompts, messages, &request, &mut usage).await;
    let (summary, failure) = match summarized {
        Ok(outcome) => outcome,
        Err(mut failure) => {
            failure.usage = usage;
            return Err(failure);
        }
    };

    // The reference keeps `snapshot[0]` whatever it is, which is the system
    // prompt for every transcript a session builds.
    let mut compacted: Vec<ModelMessage> = messages.first().cloned().into_iter().collect();
    compacted.push(ModelMessage::injected_user(render_compaction_context(
        &preserved, &summary,
    )));
    Ok(SummarizedCompaction {
        summary,
        messages: compacted,
        usage,
        failure,
    })
}

/// The summary, and the failure it degraded from when it is the placeholder.
async fn summarize(
    provider: &dyn crate::engine::CompletionProvider,
    plan: &CompactionPlan,
    prompts: &CompactionPrompts,
    messages: &[ModelMessage],
    request: &str,
    usage: &mut Usage,
) -> Result<(String, Option<CompactionFailureReason>), CompactionFailure> {
    let (summary, working, reason) = primary(provider, plan, messages, request, usage).await?;
    if let Some(summary) = summary {
        return Ok((summary, None));
    }
    // A missing summary always carries a reason; treating an absent one as the
    // empty-summary case keeps the function total without a panic.
    let reason = reason.unwrap_or(CompactionFailureReason::EmptySummary);

    // Strict mode is terminal and has no fallback. Outside it, the dedicated
    // call gets one attempt, and whatever happens the reported reason stays the
    // primary's: the fallback can only ever fail as an empty summary, so the
    // primary's is the informative one.
    if !plan.strict
        && let Some(recovered) = fallback(provider, plan, prompts, &working, request, usage).await?
    {
        return Ok((recovered, None));
    }
    if plan.strict {
        return Err(CompactionFailure::classified(
            reason,
            format!(
                "compaction did not produce a summary (reason={})",
                reason.label()
            ),
        ));
    }
    Ok((PLACEHOLDER_SUMMARY.to_owned(), Some(reason)))
}

/// The primary call: the live conversation plus the request, with the tools and
/// the tool choice attached.
///
/// Returns the summary when there is one, the history the ladder ended on, and
/// the classified reason when there is none.
async fn primary(
    provider: &dyn crate::engine::CompletionProvider,
    plan: &CompactionPlan,
    messages: &[ModelMessage],
    request: &str,
    usage: &mut Usage,
) -> Result<
    (
        Option<String>,
        Vec<ModelMessage>,
        Option<CompactionFailureReason>,
    ),
    CompactionFailure,
> {
    let (answer, working) = summarize_call(
        provider,
        plan,
        messages.to_vec(),
        &CallShape::Primary,
        request,
        usage,
    )
    .await?;
    if !answer.tool_calls.is_empty() {
        return Ok((None, working, Some(CompactionFailureReason::ToolCall)));
    }
    match extract_summary(&answer.text) {
        Some(summary) => Ok((Some(summary), working, None)),
        None => Ok((None, working, Some(CompactionFailureReason::EmptySummary))),
    }
}

/// The fallback call: a dedicated system prompt, thinking forced off, no tools,
/// and the conversation rendered as a transcript instead of replayed as one.
async fn fallback(
    provider: &dyn crate::engine::CompletionProvider,
    plan: &CompactionPlan,
    prompts: &CompactionPrompts,
    history: &[ModelMessage],
    request: &str,
    usage: &mut Usage,
) -> Result<Option<String>, CompactionFailure> {
    let (answer, _) = summarize_call(
        provider,
        plan,
        history.to_vec(),
        &CallShape::Fallback {
            system: &prompts.fallback_system,
        },
        request,
        usage,
    )
    .await?;
    Ok(extract_summary(&answer.text))
}

/// Which of the two calls is being built.
enum CallShape<'a> {
    Primary,
    Fallback { system: &'a str },
}

/// One summarization call, retried on overflow by shedding the oldest round.
///
/// Returns the answer and the history it was finally obtained from, which is
/// what a subsequent fallback renders: the ladder's trimming is not undone.
async fn summarize_call(
    provider: &dyn crate::engine::CompletionProvider,
    plan: &CompactionPlan,
    mut working: Vec<ModelMessage>,
    shape: &CallShape<'_>,
    request: &str,
    usage: &mut Usage,
) -> Result<(AssistantMessage, Vec<ModelMessage>), CompactionFailure> {
    let mut tries_left = COMPACTION_OVERFLOW_RETRIES;
    loop {
        let input = build_input(plan, shape, &working, request);
        match provider.complete(&input).await {
            Ok(answer) => {
                usage.input_tokens = usage.input_tokens.saturating_add(answer.usage.input_tokens);
                usage.output_tokens = usage
                    .output_tokens
                    .saturating_add(answer.usage.output_tokens);
                return Ok((answer, working));
            }
            Err(ProviderError::ContextOverflow) => match drop_oldest_round(&working) {
                Some(trimmed) if tries_left > 0 => {
                    working = trimmed;
                    tries_left -= 1;
                }
                _ => {
                    return Err(CompactionFailure::from(
                        ProviderError::ContextOverflow.to_string(),
                    ));
                }
            },
            Err(error) => return Err(CompactionFailure::from(error.to_string())),
        }
    }
}

/// The request one call sends, built from the history the ladder is holding.
fn build_input(
    plan: &CompactionPlan,
    shape: &CallShape<'_>,
    working: &[ModelMessage],
    request: &str,
) -> ProviderInput {
    let (messages, tools, tool_choice, thinking) = match shape {
        CallShape::Primary => {
            let mut messages = working.to_vec();
            messages.push(ModelMessage::user(request.to_owned()));
            (
                messages,
                plan.tools.clone(),
                plan.tool_choice.clone(),
                plan.thinking,
            )
        }
        CallShape::Fallback { system } => (
            vec![
                ModelMessage::System {
                    content: (*system).to_owned(),
                },
                ModelMessage::user(format!(
                    "{request}\n\n{TRANSCRIPT_HEADING}\n{}",
                    render_transcript(working)
                )),
            ],
            Vec::new(),
            None,
            false,
        ),
    };
    ProviderInput {
        turn_id: None,
        model_override: plan.model.clone(),
        messages,
        stream: false,
        images: Vec::new(),
        tools,
        tool_choice,
        thinking,
        reasoning_effort: None,
        headers: std::collections::BTreeMap::new(),
        limits: plan.limits.clone(),
        metadata: std::collections::BTreeMap::from([(
            "operation".to_owned(),
            "session_compaction".to_owned(),
        )]),
    }
}

/// The conversation as text the fallback can read without replaying it.
///
/// System messages are skipped, because the fallback brings its own. Tool calls
/// are kept, so the summarizer sees what invoked the results it also sees.
/// Reasoning is dropped: it is ephemeral and would inflate the very transcript
/// compaction is shrinking. A message left with nothing to show is omitted
/// rather than rendered as a bare heading.
#[must_use]
pub fn render_transcript(messages: &[ModelMessage]) -> String {
    let mut blocks: Vec<String> = Vec::new();
    for message in messages {
        if matches!(message, ModelMessage::System { .. }) {
            continue;
        }
        let mut parts: Vec<String> = Vec::new();
        let text = message.content().trim();
        if !text.is_empty() {
            parts.push(text.to_owned());
        }
        for call in tool_calls_of(message) {
            parts.push(format!("[calls {}({})]", call.name, call.arguments));
        }
        if parts.is_empty() {
            continue;
        }
        blocks.push(format!("### {}\n{}", role_label(message), parts.join("\n")));
    }
    blocks.join("\n\n")
}

fn tool_calls_of(message: &ModelMessage) -> &[ModelToolCall] {
    match message {
        ModelMessage::Assistant { tool_calls, .. } => tool_calls,
        _ => &[],
    }
}

/// The role name the reference writes in a transcript heading, which is the
/// value of its role enum.
const fn role_label(message: &ModelMessage) -> &'static str {
    match message {
        ModelMessage::System { .. } => "system",
        ModelMessage::User { .. } => "user",
        ModelMessage::Assistant { .. } => "assistant",
        ModelMessage::Tool { .. } => "tool",
    }
}
