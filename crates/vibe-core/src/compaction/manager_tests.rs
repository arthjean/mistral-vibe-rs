//! What the summarizer calls, in what order, and what it makes of the answers.
//!
//! The scripted provider these tests drive is the port's counterpart of the
//! `CompletionFn` protocol the reference injects, and it is what lets the
//! compaction corpus replay the reference's own decision tree with no network
//! and no key. It lives here so the parity replay and these unit tests observe
//! the manager through exactly one lens.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::engine::{CompletionProvider, ProviderFuture};
use crate::events::{ModelMessage, ModelToolCall};
use crate::provider::{
    AssistantMessage, ProviderError, ProviderInput, ToolChoice, ToolDefinition, Usage,
};

use super::context::extract_summary;
use super::manager::{
    CompactionPlan, CompactionPromptResolution, CompactionPrompts, PLACEHOLDER_SUMMARY,
    SummarizedCompaction, compact, render_transcript,
};
use super::{CompactionFailure, CompactionFailureReason};

/// One scripted model answer: text, a number of tool calls, or an overflow.
#[derive(Debug, Clone, Default)]
pub(crate) struct ScriptedAnswer {
    pub text: String,
    pub tool_calls: usize,
    pub overflow: bool,
    pub usage: Usage,
}

impl ScriptedAnswer {
    pub(crate) fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }

    pub(crate) fn tool_call() -> Self {
        Self {
            tool_calls: 1,
            ..Self::default()
        }
    }

    pub(crate) fn overflow() -> Self {
        Self {
            overflow: true,
            ..Self::default()
        }
    }

    pub(crate) fn costing(mut self, input_tokens: u64, output_tokens: u64) -> Self {
        self.usage = Usage {
            input_tokens,
            output_tokens,
        };
        self
    }
}

/// What one call carried, recorded in the shape the compaction corpus records
/// it so a replay compares like with like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallRecord {
    pub model: Option<String>,
    pub thinking: bool,
    pub tools: usize,
    pub tool_choice: Option<ToolChoice>,
    pub messages: usize,
    pub system_messages: usize,
    pub overflow: bool,
}

/// A provider that answers from a script and remembers what it was asked.
pub(crate) struct ScriptedProvider {
    answers: Mutex<VecDeque<ScriptedAnswer>>,
    calls: Mutex<Vec<CallRecord>>,
    inputs: Mutex<Vec<Vec<ModelMessage>>>,
}

impl ScriptedProvider {
    pub(crate) fn new(answers: impl IntoIterator<Item = ScriptedAnswer>) -> Self {
        Self {
            answers: Mutex::new(answers.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
            inputs: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn calls(&self) -> Vec<CallRecord> {
        self.calls.lock().expect("the call log is readable").clone()
    }

    /// The messages each call carried, in call order.
    pub(crate) fn inputs(&self) -> Vec<Vec<ModelMessage>> {
        self.inputs
            .lock()
            .expect("the input log is readable")
            .clone()
    }
}

impl CompletionProvider for ScriptedProvider {
    fn complete<'a>(&'a self, input: &'a ProviderInput) -> ProviderFuture<'a> {
        Box::pin(async move {
            let step = self
                .answers
                .lock()
                .expect("the script is readable")
                .pop_front()
                .unwrap_or_default();
            self.calls
                .lock()
                .expect("the call log is writable")
                .push(CallRecord {
                    model: input.model_override.clone(),
                    thinking: input.thinking,
                    tools: input.tools.len(),
                    tool_choice: input.tool_choice.clone(),
                    messages: input.messages.len(),
                    system_messages: input
                        .messages
                        .iter()
                        .filter(|message| matches!(message, ModelMessage::System { .. }))
                        .count(),
                    overflow: step.overflow,
                });
            self.inputs
                .lock()
                .expect("the input log is writable")
                .push(input.messages.clone());
            if step.overflow {
                return Err(ProviderError::ContextOverflow);
            }
            Ok(AssistantMessage {
                text: step.text,
                reasoning: None,
                reasoning_signature: None,
                reasoning_state: Vec::new(),
                tool_calls: (0..step.tool_calls)
                    .map(|index| ModelToolCall {
                        id: format!("call-{index}"),
                        name: "read_file".to_owned(),
                        arguments: "{}".to_owned(),
                    })
                    .collect(),
                usage: step.usage,
                refusal: None,
                stop_reason: "stop".to_owned(),
                correlation_id: None,
            })
        })
    }
}

/// The plan every test here runs under, with the two prompts standing in for
/// the resolved ones.
pub(crate) fn plan() -> CompactionPlan {
    CompactionPlan {
        prompts: CompactionPromptResolution::Resolved(CompactionPrompts {
            request: "summarize this".to_owned(),
            fallback_system: "you summarize".to_owned(),
            summary_prefix: "Another language model".to_owned(),
        }),
        model: Some("compaction-model".to_owned()),
        thinking: true,
        tools: Vec::new(),
        tool_choice: Some(ToolChoice::Auto),
        strict: false,
        ..CompactionPlan::default()
    }
}

/// The conversation the reference's own manager cases are built on.
pub(crate) fn conversation() -> Vec<ModelMessage> {
    vec![
        ModelMessage::System {
            content: "system prompt".to_owned(),
        },
        ModelMessage::user("first"),
        ModelMessage::Assistant {
            content: "reply one".to_owned(),
            reasoning: None,
            reasoning_signature: None,
            reasoning_state: Vec::new(),
            tool_calls: Vec::new(),
        },
        ModelMessage::user("second"),
        ModelMessage::Assistant {
            content: "reply two".to_owned(),
            reasoning: None,
            reasoning_signature: None,
            reasoning_state: Vec::new(),
            tool_calls: Vec::new(),
        },
    ]
}

async fn run(
    provider: &ScriptedProvider,
    plan: &CompactionPlan,
    messages: &[ModelMessage],
) -> Result<SummarizedCompaction, CompactionFailure> {
    compact(provider, plan, messages, "").await
}

/// US-152: the primary call rides the live transcript with the request appended,
/// carries the tool choice, and leaves the conversation it read untouched.
#[tokio::test]
async fn the_primary_call_rides_the_live_transcript_and_carries_the_tool_choice() {
    let provider = ScriptedProvider::new([ScriptedAnswer::text("<summary>all good</summary>")]);
    let messages = conversation();
    let plan = CompactionPlan {
        tools: vec![ToolDefinition {
            name: "read_file".to_owned(),
            description: "reads".to_owned(),
            input_schema: serde_json::json!({}),
        }],
        ..plan()
    };

    let compacted = run(&provider, &plan, &messages)
        .await
        .expect("a usable summary compacts");

    assert_eq!(compacted.summary, "all good");
    let calls = provider.calls();
    assert_eq!(calls.len(), 1, "a usable summary makes exactly one call");
    assert_eq!(
        calls[0],
        CallRecord {
            model: Some("compaction-model".to_owned()),
            thinking: true,
            tools: 1,
            tool_choice: Some(ToolChoice::Auto),
            messages: messages.len() + 1,
            system_messages: 1,
            overflow: false,
        }
    );
    let sent = provider.inputs();
    assert_eq!(
        sent[0].last().map(ModelMessage::content),
        Some("summarize this"),
        "the request is appended as the last user message"
    );
    assert_eq!(
        messages,
        conversation(),
        "the live transcript is not mutated by a compaction"
    );
    assert_eq!(
        compacted.messages.first(),
        messages.first(),
        "the transcript keeps its first message"
    );
    assert!(
        compacted.messages[1].is_injected(),
        "the envelope is injected"
    );
    assert_eq!(compacted.messages.len(), 2);
    assert!(compacted.failure.is_none());
}

/// US-152: an identifier that named nothing fails the compaction when one runs,
/// with the message the resolution produced and before any model call.
#[tokio::test]
async fn an_unresolved_prompt_identifier_fails_the_compaction_before_any_call() {
    let provider = ScriptedProvider::new([ScriptedAnswer::text("<summary>never asked</summary>")]);
    let plan = CompactionPlan {
        prompts: CompactionPromptResolution::resolve("nowhere", &[]),
        ..plan()
    };
    let failure = run(&provider, &plan, &conversation())
        .await
        .expect_err("an unresolved request fails the compaction");
    assert!(
        failure.message.contains("compaction_prompt_id"),
        "{failure}"
    );
    assert!(failure.message.contains("'nowhere'"), "{failure}");
    assert!(
        provider.calls().is_empty(),
        "an unresolved request never reaches the provider"
    );
}

/// US-152: extra instructions arrive under their own heading, after the request.
#[tokio::test]
async fn extra_instructions_are_appended_under_their_own_heading() {
    let provider = ScriptedProvider::new([ScriptedAnswer::text("<summary>noted</summary>")]);
    compact(&provider, &plan(), &conversation(), "keep the file names")
        .await
        .expect("the compaction succeeds");
    let sent = provider.inputs();
    assert_eq!(
        sent[0].last().map(ModelMessage::content),
        Some("summarize this\n\n## Additional Instructions\nkeep the file names")
    );
}

/// US-152: a tool call is a distinct, classified failure and no summary is taken
/// from the text that came with it.
#[tokio::test]
async fn a_tool_call_answer_is_classified_and_falls_back() {
    let provider = ScriptedProvider::new([
        ScriptedAnswer {
            text: "<summary>ignored</summary>".to_owned(),
            tool_calls: 1,
            ..ScriptedAnswer::default()
        },
        ScriptedAnswer::text("<summary>from the fallback</summary>"),
    ]);
    let compacted = run(&provider, &plan(), &conversation())
        .await
        .expect("the fallback recovers");
    assert_eq!(
        compacted.summary, "from the fallback",
        "the tool-call answer's own text is never read as a summary"
    );
    assert_eq!(provider.calls().len(), 2);
    assert!(
        compacted.failure.is_none(),
        "a recovered compaction is clean"
    );
}

/// US-153: the fallback carries its own system message, thinking off, no tools
/// and no tool choice, and its user message ends with the rendered transcript.
#[tokio::test]
async fn the_fallback_is_a_dedicated_call_over_a_rendered_transcript() {
    let provider = ScriptedProvider::new([
        ScriptedAnswer::text("no summary element here"),
        ScriptedAnswer::text("<summary>recovered</summary>"),
    ]);
    let plan = CompactionPlan {
        tools: vec![ToolDefinition {
            name: "read_file".to_owned(),
            description: "reads".to_owned(),
            input_schema: serde_json::json!({}),
        }],
        ..plan()
    };
    let compacted = run(&provider, &plan, &conversation())
        .await
        .expect("the fallback recovers");
    assert_eq!(compacted.summary, "recovered");

    let calls = provider.calls();
    assert_eq!(
        calls[1],
        CallRecord {
            model: Some("compaction-model".to_owned()),
            thinking: false,
            tools: 0,
            tool_choice: None,
            messages: 2,
            system_messages: 1,
            overflow: false,
        }
    );
    let sent = provider.inputs();
    assert_eq!(
        sent[1][0],
        ModelMessage::System {
            content: "you summarize".to_owned()
        }
    );
    assert_eq!(
        sent[1][1].content(),
        format!(
            "summarize this\n\n## Conversation Transcript\n{}",
            render_transcript(&conversation())
        )
    );
}

/// US-153: strict mode has no fallback and reports the primary's reason.
#[tokio::test]
async fn strict_mode_refuses_the_fallback_and_raises_the_primary_reason() {
    let provider = ScriptedProvider::new([ScriptedAnswer::tool_call()]);
    let plan = CompactionPlan {
        strict: true,
        ..plan()
    };
    let failure = run(&provider, &plan, &conversation())
        .await
        .expect_err("strict mode fails the compaction");
    assert_eq!(failure.reason, Some(CompactionFailureReason::ToolCall));
    assert_eq!(
        provider.calls().len(),
        1,
        "strict mode makes no second call"
    );
}

/// US-153: when both calls fail outside strict mode the conversation is still
/// compacted, under the placeholder, and the reported reason is the primary's.
#[tokio::test]
async fn both_calls_failing_degrades_to_the_placeholder_with_the_primary_reason() {
    let provider = ScriptedProvider::new([
        ScriptedAnswer::tool_call(),
        ScriptedAnswer::text("nothing usable"),
    ]);
    let compacted = run(&provider, &plan(), &conversation())
        .await
        .expect("a failed summarization still compacts outside strict mode");
    assert_eq!(compacted.summary, PLACEHOLDER_SUMMARY);
    assert_eq!(
        compacted.failure,
        Some(CompactionFailureReason::ToolCall),
        "the fallback can only fail as an empty summary, so the primary's reason is reported"
    );
    assert_eq!(compacted.messages.len(), 2);
}

/// US-154: an overflow sheds the oldest round and retries, at most three times.
#[tokio::test]
async fn the_overflow_ladder_sheds_the_oldest_round_and_stops_after_three_retries() {
    let mut conversation = vec![ModelMessage::System {
        content: "system prompt".to_owned(),
    }];
    for index in 0..8 {
        conversation.push(ModelMessage::user(format!("turn {index}")));
        conversation.push(ModelMessage::Assistant {
            content: format!("reply {index}"),
            reasoning: None,
            reasoning_signature: None,
            reasoning_state: Vec::new(),
            tool_calls: Vec::new(),
        });
    }
    let provider = ScriptedProvider::new(std::iter::repeat_n(ScriptedAnswer::overflow(), 5));
    let failure = run(&provider, &plan(), &conversation)
        .await
        .expect_err("the fourth overflow propagates");
    assert_eq!(
        failure.reason, None,
        "an overflow is not a classified reason"
    );
    let calls = provider.calls();
    assert_eq!(calls.len(), 4, "one attempt plus three retries");
    let sizes: Vec<usize> = calls.iter().map(|call| call.messages).collect();
    assert_eq!(
        sizes,
        vec![18, 16, 14, 12],
        "each retry sheds exactly one round"
    );
}

/// US-154: an overflow with nothing older to shed propagates immediately.
#[tokio::test]
async fn an_overflow_with_nothing_to_drop_propagates_at_once() {
    let conversation = vec![
        ModelMessage::System {
            content: "system prompt".to_owned(),
        },
        ModelMessage::user("only round"),
        ModelMessage::Assistant {
            content: "reply".to_owned(),
            reasoning: None,
            reasoning_signature: None,
            reasoning_state: Vec::new(),
            tool_calls: Vec::new(),
        },
    ];
    let provider = ScriptedProvider::new([ScriptedAnswer::overflow()]);
    run(&provider, &plan(), &conversation)
        .await
        .expect_err("the overflow propagates");
    assert_eq!(provider.calls().len(), 1);
}

/// US-154: a successful retry hands the trimmed history to the fallback, not the
/// original one.
#[tokio::test]
async fn the_fallback_renders_the_history_the_ladder_ended_on() {
    let mut conversation = vec![ModelMessage::System {
        content: "system prompt".to_owned(),
    }];
    for index in 0..4 {
        conversation.push(ModelMessage::user(format!("turn {index}")));
        conversation.push(ModelMessage::Assistant {
            content: format!("reply {index}"),
            reasoning: None,
            reasoning_signature: None,
            reasoning_state: Vec::new(),
            tool_calls: Vec::new(),
        });
    }
    let provider = ScriptedProvider::new([
        ScriptedAnswer::overflow(),
        ScriptedAnswer::text("unusable"),
        ScriptedAnswer::text("<summary>recovered</summary>"),
    ]);
    run(&provider, &plan(), &conversation)
        .await
        .expect("the fallback recovers");
    let rendered = provider.inputs()[2][1].content().to_owned();
    assert!(
        !rendered.contains("turn 0"),
        "the round the ladder shed is not rendered again: {rendered}"
    );
    assert!(rendered.contains("turn 3"));
}

/// US-154: a provider error that is not an overflow is propagated with no retry.
#[tokio::test]
async fn a_non_overflow_error_is_propagated_without_a_retry() {
    struct FailingProvider;
    impl CompletionProvider for FailingProvider {
        fn complete<'a>(&'a self, _input: &'a ProviderInput) -> ProviderFuture<'a> {
            Box::pin(async { Err(ProviderError::InvalidRequest("no".to_owned())) })
        }
    }
    let failure = compact(&FailingProvider, &plan(), &conversation(), "")
        .await
        .expect_err("the error propagates");
    assert_eq!(failure.reason, None);
    assert!(failure.message.contains("invalid provider request"));
}

/// US-155: every call a compaction makes is credited, on success and on failure.
#[tokio::test]
async fn every_call_is_accounted_on_both_paths() {
    let provider = ScriptedProvider::new([
        ScriptedAnswer::tool_call().costing(100, 10),
        ScriptedAnswer::text("<summary>done</summary>").costing(40, 5),
    ]);
    let compacted = run(&provider, &plan(), &conversation())
        .await
        .expect("the fallback recovers");
    assert_eq!(
        compacted.usage,
        Usage {
            input_tokens: 140,
            output_tokens: 15
        }
    );

    let provider = ScriptedProvider::new([ScriptedAnswer::tool_call().costing(70, 7)]);
    let plan = CompactionPlan {
        strict: true,
        ..plan()
    };
    let failure = run(&provider, &plan, &conversation())
        .await
        .expect_err("strict mode fails");
    assert_eq!(
        failure.usage,
        Usage {
            input_tokens: 70,
            output_tokens: 7
        },
        "a failed compaction still spent what it spent"
    );
}

/// US-153: the transcript rendering keeps text and tool calls, drops reasoning
/// and system messages, and omits a message with nothing to show.
#[test]
fn the_rendered_transcript_keeps_actions_and_drops_reasoning() {
    let messages = vec![
        ModelMessage::System {
            content: "system prompt".to_owned(),
        },
        ModelMessage::user("  do the thing  "),
        ModelMessage::Assistant {
            content: String::new(),
            reasoning: Some("thinking hard".to_owned()),
            reasoning_signature: None,
            reasoning_state: Vec::new(),
            tool_calls: vec![ModelToolCall {
                id: "call-0".to_owned(),
                name: "read_file".to_owned(),
                arguments: "{\"path\":\"a.rs\"}".to_owned(),
            }],
        },
        ModelMessage::Tool {
            call_id: "call-0".to_owned(),
            content: "contents".to_owned(),
            is_error: false,
        },
        ModelMessage::Assistant {
            content: String::new(),
            reasoning: Some("more thinking".to_owned()),
            reasoning_signature: None,
            reasoning_state: Vec::new(),
            tool_calls: Vec::new(),
        },
    ];
    assert_eq!(
        render_transcript(&messages),
        "### user\ndo the thing\n\n### assistant\n[calls read_file({\"path\":\"a.rs\"})]\n\n### \
         tool\ncontents"
    );
}

/// US-152: the envelope the compaction leaves is readable by the parser the
/// next compaction uses, which is what makes a second compaction merge rather
/// than forget.
#[tokio::test]
async fn the_envelope_the_compaction_writes_carries_the_preserved_turns() {
    let provider = ScriptedProvider::new([ScriptedAnswer::text("<summary>kept</summary>")]);
    let compacted = run(&provider, &plan(), &conversation())
        .await
        .expect("the compaction succeeds");
    let envelope = compacted.messages[1].content();
    assert_eq!(
        super::context::parse_previous_user_messages(envelope),
        vec!["first".to_owned(), "second".to_owned()]
    );
    assert_eq!(
        extract_summary(&format!("<summary>{}</summary>", "kept")),
        Some("kept".to_owned())
    );
    assert!(super::context::is_compaction_context_message(
        &compacted.messages[1]
    ));
}
