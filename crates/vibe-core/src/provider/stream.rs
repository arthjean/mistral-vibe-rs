//! Reading a streamed completion back, one event vocabulary per API style.
//!
//! A stream arrives as deltas, and every style spells them differently: chat
//! sends partial tool-call arguments indexed by position, Anthropic sends typed
//! content blocks opened and closed by index, and Responses sends named output
//! items. [`StreamParseState`] carries what a style needs between two events so
//! each parser stays a function of one event plus what came before it.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{
    AssistantMessage, ModelToolCall, ProviderChunk, ProviderError, Usage, string_field, u64_field,
};

#[derive(Debug, Default)]
pub(super) struct StreamParseState {
    tool_ids_by_index: BTreeMap<u64, String>,
    response_call_ids: BTreeMap<String, String>,
}

pub(super) fn parse_chat_event(
    value: &Value,
    state: &mut StreamParseState,
) -> Result<Vec<ProviderChunk>, ProviderError> {
    let mut chunks = Vec::new();
    if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
        chunks.push(ProviderChunk::Usage {
            input_tokens: u64_field(usage, "prompt_tokens")?,
            output_tokens: u64_field(usage, "completion_tokens")?,
        });
    }
    let Some(choice) = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    else {
        return Ok(chunks);
    };
    let delta = choice
        .get("delta")
        .or_else(|| choice.get("message"))
        .unwrap_or(choice);
    if let Some(content) = delta.get("content").filter(|content| !content.is_null()) {
        match content {
            Value::String(text) => chunks.push(ProviderChunk::Text {
                text: text.to_owned(),
            }),
            Value::Array(blocks) => {
                for block in blocks {
                    let kind = block
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    match kind {
                        "text" => chunks.push(ProviderChunk::Text {
                            text: string_field(block, "text")?,
                        }),
                        "thinking" => {
                            let text = block
                                .get("thinking")
                                .and_then(Value::as_array)
                                .map(|parts| {
                                    parts
                                        .iter()
                                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                                        .collect::<String>()
                                })
                                .unwrap_or_default();
                            chunks.push(ProviderChunk::Reasoning {
                                text,
                                signature: None,
                            });
                        }
                        _ => {
                            return Err(ProviderError::UnsupportedContentBlock(kind.to_owned()));
                        }
                    }
                }
            }
            _ => {
                return Err(ProviderError::UnsupportedContentBlock(
                    "invalid_chat_content".to_owned(),
                ));
            }
        }
    }
    if let Some(text) = delta
        .get("reasoning_content")
        .or_else(|| delta.get("reasoning"))
        .and_then(Value::as_str)
    {
        chunks.push(ProviderChunk::Reasoning {
            text: text.to_owned(),
            signature: delta
                .get("reasoning_signature")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    if let Some(refusal) = delta.get("refusal").and_then(Value::as_str) {
        chunks.push(ProviderChunk::Refusal {
            message: refusal.to_owned(),
        });
    }
    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
            if let Some(id) = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                state.tool_ids_by_index.insert(index, id.to_owned());
            }
            let id = state
                .tool_ids_by_index
                .get(&index)
                .cloned()
                .unwrap_or_else(|| format!("index-{index}"));
            chunks.push(ProviderChunk::ToolCall {
                id,
                name: call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                arguments: call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            });
        }
    }
    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        chunks.push(ProviderChunk::Stop {
            reason: reason.to_owned(),
        });
    }
    Ok(chunks)
}

pub(super) fn parse_anthropic_event(
    value: &Value,
    state: &mut StreamParseState,
) -> Result<Vec<ProviderChunk>, ProviderError> {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut chunks = Vec::new();
    match kind {
        "message_start" => {
            if let Some(usage) = value.pointer("/message/usage") {
                chunks.push(ProviderChunk::Usage {
                    input_tokens: u64_field(usage, "input_tokens")?,
                    output_tokens: u64_field(usage, "output_tokens").unwrap_or(0),
                });
            }
        }
        "content_block_start" => {
            match value.pointer("/content_block/type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let id = value
                        .pointer("/content_block/id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    state.tool_ids_by_index.insert(index, id.clone());
                    chunks.push(ProviderChunk::ToolCall {
                        id,
                        name: value
                            .pointer("/content_block/name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        arguments: String::new(),
                    });
                }
                Some("text" | "thinking") => {}
                Some(other) => {
                    return Err(ProviderError::UnsupportedContentBlock(other.to_owned()));
                }
                None => {
                    return Err(ProviderError::MalformedStream(
                        "content block omitted its type".to_owned(),
                    ));
                }
            }
        }
        "content_block_delta" => {
            let delta = value.get("delta").unwrap_or(&Value::Null);
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => chunks.push(ProviderChunk::Text {
                    text: delta
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }),
                Some("thinking_delta") => chunks.push(ProviderChunk::Reasoning {
                    text: delta
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    signature: None,
                }),
                Some("signature_delta") => chunks.push(ProviderChunk::Reasoning {
                    text: String::new(),
                    signature: delta
                        .get("signature")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                }),
                Some("input_json_delta") => chunks.push(ProviderChunk::ToolCall {
                    id: value
                        .get("index")
                        .and_then(Value::as_u64)
                        .and_then(|index| state.tool_ids_by_index.get(&index).cloned())
                        .ok_or_else(|| {
                            ProviderError::MalformedStream(
                                "tool delta preceded tool start".to_owned(),
                            )
                        })?,
                    name: String::new(),
                    arguments: delta
                        .get("partial_json")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }),
                Some(other) => {
                    return Err(ProviderError::UnsupportedContentBlock(other.to_owned()));
                }
                None => {
                    return Err(ProviderError::MalformedStream(
                        "content block delta omitted its type".to_owned(),
                    ));
                }
            }
        }
        "message_delta" => {
            if let Some(usage) = value.get("usage") {
                chunks.push(ProviderChunk::Usage {
                    input_tokens: u64_field(usage, "input_tokens").unwrap_or(0),
                    output_tokens: u64_field(usage, "output_tokens")?,
                });
            }
            if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                chunks.push(ProviderChunk::Stop {
                    reason: reason.to_owned(),
                });
            }
        }
        "error" => {
            return Err(ProviderError::MalformedStream(
                value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("provider error")
                    .to_owned(),
            ));
        }
        "content_block_stop" | "message_stop" | "ping" => {}
        "" => {
            return Err(ProviderError::MalformedStream(
                "Anthropic event omitted its type".to_owned(),
            ));
        }
        other => {
            return Err(ProviderError::MalformedStream(format!(
                "unsupported Anthropic event `{other}`"
            )));
        }
    }
    Ok(chunks)
}

pub(super) fn parse_responses_event(
    value: &Value,
    state: &mut StreamParseState,
) -> Result<Vec<ProviderChunk>, ProviderError> {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let chunk = match kind {
        "response.output_text.delta" => Some(ProviderChunk::Text {
            text: string_field(value, "delta")?,
        }),
        "response.reasoning_summary_text.delta" => Some(ProviderChunk::Reasoning {
            text: string_field(value, "delta")?,
            signature: None,
        }),
        "response.refusal.delta" => Some(ProviderChunk::Refusal {
            message: string_field(value, "delta")?,
        }),
        "response.output_item.added"
            if value.pointer("/item/type").and_then(Value::as_str) == Some("function_call") =>
        {
            let item_id = value
                .pointer("/item/id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let call_id = value
                .pointer("/item/call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            state.response_call_ids.insert(item_id, call_id.clone());
            Some(ProviderChunk::ToolCall {
                id: call_id,
                name: value
                    .pointer("/item/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                arguments: String::new(),
            })
        }
        "response.function_call_arguments.delta" => Some(ProviderChunk::ToolCall {
            id: value
                .get("item_id")
                .and_then(Value::as_str)
                .and_then(|item_id| state.response_call_ids.get(item_id))
                .cloned()
                .or_else(|| {
                    value
                        .get("call_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .ok_or_else(|| {
                    ProviderError::MalformedStream(
                        "function arguments preceded output item".to_owned(),
                    )
                })?,
            name: String::new(),
            arguments: string_field(value, "delta")?,
        }),
        "response.completed" => {
            let response = value.get("response").unwrap_or(value);
            let usage = response.get("usage").unwrap_or(&Value::Null);
            let mut chunks = response
                .get("output")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
                .filter_map(|item| item.get("encrypted_content").and_then(Value::as_str))
                .map(|signature| ProviderChunk::Reasoning {
                    text: String::new(),
                    signature: Some(signature.to_owned()),
                })
                .collect::<Vec<_>>();
            chunks.push(ProviderChunk::Usage {
                input_tokens: u64_field(usage, "input_tokens")?,
                output_tokens: u64_field(usage, "output_tokens")?,
            });
            chunks.push(ProviderChunk::Stop {
                reason: response
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("completed")
                    .to_owned(),
            });
            return Ok(chunks);
        }
        "response.failed" => {
            return Err(ProviderError::MalformedStream(
                value
                    .pointer("/response/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("response failed")
                    .to_owned(),
            ));
        }
        _ => None,
    };
    Ok(chunk.into_iter().collect())
}

pub(crate) fn aggregate_provider_chunks(
    chunks: Vec<ProviderChunk>,
    correlation_id: Option<String>,
) -> Result<AssistantMessage, ProviderError> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut reasoning_signature = None;
    let mut reasoning_state = Vec::new();
    let mut tool_calls = Vec::<ModelToolCall>::new();
    let mut usage: Option<Usage> = None;
    let mut refusal = None;
    let mut stop_reason = None;
    for chunk in chunks {
        match chunk {
            ProviderChunk::Text { text: delta } => text.push_str(&delta),
            ProviderChunk::Reasoning {
                text: delta,
                signature,
            } => {
                reasoning.push_str(&delta);
                if signature.is_some() {
                    reasoning_signature.clone_from(&signature);
                    if let Some(signature) = signature {
                        reasoning_state.push(signature);
                    }
                }
            }
            ProviderChunk::ToolCall {
                id,
                name,
                arguments,
            } => {
                if let Some(existing) = tool_calls.iter_mut().find(|call| call.id == id) {
                    if !name.is_empty() {
                        existing.name = name;
                    }
                    existing.arguments.push_str(&arguments);
                } else {
                    tool_calls.push(ModelToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
            }
            ProviderChunk::Usage {
                input_tokens,
                output_tokens,
            } => match &mut usage {
                Some(usage) => {
                    if input_tokens > 0 {
                        usage.input_tokens = input_tokens;
                    }
                    if output_tokens > 0 {
                        usage.output_tokens = output_tokens;
                    }
                }
                None => {
                    usage = Some(Usage {
                        input_tokens,
                        output_tokens,
                    });
                }
            },
            ProviderChunk::Refusal { message } => refusal = Some(message),
            ProviderChunk::Stop { reason } => stop_reason = Some(reason),
        }
    }
    let usage = usage.ok_or(ProviderError::MissingUsage)?;
    if let Some(message) = &refusal {
        return Err(ProviderError::Refusal(message.clone()));
    }
    let stop_reason = stop_reason.ok_or_else(|| {
        ProviderError::MalformedStream("provider response omitted stop state".to_owned())
    })?;
    Ok(AssistantMessage {
        text,
        reasoning: (!reasoning.is_empty()).then_some(reasoning),
        reasoning_signature,
        reasoning_state,
        tool_calls,
        usage,
        refusal,
        stop_reason,
        correlation_id,
    })
}
