//! Composing the wire request, one shape per API style.
//!
//! The three styles disagree about more than field names. Chat carries the
//! system prompt as a message and tool results as their own role; Responses
//! flattens the conversation into typed input items; Anthropic hoists the
//! system prompt out of the messages entirely and pairs every tool result with
//! the call it answers. Reasoning, images and tool choice each land in a
//! different place again.
//!
//! Keeping the three builders together is what makes those disagreements
//! readable as one table rather than discovered one field at a time.

use serde_json::{Value, json};

use super::{
    ImageInput, ProviderError, ProviderInput, ProviderStyle, anthropic_tool_choice,
    chat_tool_choice, responses_tool_choice, sanitize_tool_id,
};
use crate::events::ModelMessage;

pub(super) fn build_chat_request(
    style: ProviderStyle,
    model: &str,
    input: &ProviderInput,
) -> Result<Value, ProviderError> {
    let mut messages = input
        .messages
        .iter()
        .map(|message| {
            if style == ProviderStyle::Reasoning {
                reasoning_message(message)
            } else {
                chat_message(message)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    attach_images(&mut messages, &input.images)?;
    let tools = input
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }
            })
        })
        .collect::<Vec<_>>();
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": input.stream,
        "max_tokens": input.limits.max_tokens,
    });
    if input.stream {
        body["stream_options"] =
            if matches!(style, ProviderStyle::Mistral | ProviderStyle::Reasoning) {
                json!({"include_usage": true, "stream_tool_calls": true})
            } else {
                json!({"include_usage": true})
            };
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(choice) = &input.tool_choice {
        body["tool_choice"] = chat_tool_choice(choice);
    }
    if let Some(temperature) = input.limits.temperature_millis {
        body["temperature"] = json!(f64::from(temperature) / 1000.0);
    }
    if style == ProviderStyle::Mistral && input.thinking {
        body["temperature"] = json!(1.0);
    }
    if style == ProviderStyle::Reasoning
        && let Some(effort) = input
            .reasoning_effort
            .as_deref()
            .filter(|effort| !matches!(*effort, "off" | "none"))
            .or(input.thinking.then_some("medium"))
    {
        body["reasoning_effort"] = json!(effort);
    }
    if !input.metadata.is_empty() {
        body["metadata"] = serde_json::to_value(&input.metadata)
            .map_err(|error| ProviderError::InvalidRequest(error.to_string()))?;
    }
    Ok(body)
}

fn reasoning_message(message: &ModelMessage) -> Result<Value, ProviderError> {
    match message {
        ModelMessage::Assistant {
            content,
            reasoning: Some(reasoning),
            tool_calls,
            ..
        } => {
            let mut value = json!({
                "role": "assistant",
                "content": [
                    {
                        "type": "thinking",
                        "thinking": [{"type": "text", "text": reasoning}],
                    },
                    {"type": "text", "text": content},
                ],
            });
            if !tool_calls.is_empty() {
                value["tool_calls"] = Value::Array(
                    tool_calls
                        .iter()
                        .map(|call| {
                            json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": call.arguments,
                                }
                            })
                        })
                        .collect(),
                );
            }
            Ok(value)
        }
        _ => chat_message(message),
    }
}

fn chat_message(message: &ModelMessage) -> Result<Value, ProviderError> {
    match message {
        ModelMessage::System { content } => Ok(json!({"role": "system", "content": content})),
        ModelMessage::User { content, .. } => Ok(json!({"role": "user", "content": content})),
        ModelMessage::Assistant {
            content,
            reasoning,
            reasoning_signature,
            reasoning_state: _,
            tool_calls,
        } => {
            let mut value = json!({"role": "assistant", "content": content});
            if let Some(reasoning) = reasoning {
                value["reasoning_content"] = json!(reasoning);
            }
            if let Some(signature) = reasoning_signature {
                value["reasoning_signature"] = json!(signature);
            }
            if !tool_calls.is_empty() {
                value["tool_calls"] = Value::Array(
                    tool_calls
                        .iter()
                        .map(|call| {
                            json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": call.arguments,
                                }
                            })
                        })
                        .collect(),
                );
            }
            Ok(value)
        }
        ModelMessage::Tool {
            call_id,
            content,
            is_error,
        } => Ok(json!({
            "role": "tool",
            "tool_call_id": call_id,
            "content": content,
            "is_error": is_error,
        })),
    }
}

fn attach_images(messages: &mut [Value], images: &[ImageInput]) -> Result<(), ProviderError> {
    if images.is_empty() {
        return Ok(());
    }
    let message = messages
        .iter_mut()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .ok_or_else(|| ProviderError::InvalidRequest("images require a user message".to_owned()))?;
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let mut content = vec![json!({"type": "text", "text": text})];
    content.extend(images.iter().map(|image| {
        json!({
            "type": "image_url",
            "image_url": {
                "url": format!("data:{};base64,{}", image.media_type, image.data)
            }
        })
    }));
    message["content"] = Value::Array(content);
    Ok(())
}

pub(super) fn build_responses_request(
    model: &str,
    input: &ProviderInput,
) -> Result<Value, ProviderError> {
    let mut messages = Vec::new();
    for message in &input.messages {
        match message {
            ModelMessage::System { content } => {
                messages.push(json!({"role": "system", "content": content}));
            }
            ModelMessage::User { content, .. } => {
                messages.push(json!({"role": "user", "content": content}));
            }
            ModelMessage::Assistant {
                content,
                reasoning_state,
                tool_calls,
                ..
            } => {
                messages.extend(
                    reasoning_state
                        .iter()
                        .map(|state| json!({"type": "reasoning", "encrypted_content": state})),
                );
                messages.push(json!({
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": content}],
                }));
                messages.extend(tool_calls.iter().map(|call| {
                    json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments,
                    })
                }));
            }
            ModelMessage::Tool {
                call_id, content, ..
            } => messages.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": content,
            })),
        }
    }
    if let Some(last_user) = messages
        .iter_mut()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
    {
        if !input.images.is_empty() {
            let text = last_user
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let mut content = Vec::new();
            if !text.is_empty() {
                content.push(json!({"type": "input_text", "text": text}));
            }
            content.extend(input.images.iter().map(|image| {
                json!({
                    "type": "input_image",
                    "image_url": format!("data:{};base64,{}", image.media_type, image.data),
                })
            }));
            last_user["content"] = Value::Array(content);
        }
    } else if !input.images.is_empty() {
        return Err(ProviderError::InvalidRequest(
            "images require a user message".to_owned(),
        ));
    }
    let tools = input
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            })
        })
        .collect::<Vec<_>>();
    let effort = match input.reasoning_effort.as_deref() {
        Some("max") => "xhigh",
        Some("off") | Some("none") => "none",
        Some(effort) => effort,
        None if input.thinking => "medium",
        None => "none",
    };
    let mut body = json!({
        "model": model,
        "input": messages,
        "store": false,
        "max_output_tokens": input.limits.max_tokens,
        "reasoning": {"effort": effort},
    });
    if input.stream {
        body["stream"] = json!(true);
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(choice) = &input.tool_choice {
        body["tool_choice"] = responses_tool_choice(choice);
    }
    if (model.starts_with("gpt-4") || model.starts_with("gpt-3.5"))
        && let Some(temperature) = input.limits.temperature_millis
    {
        body["temperature"] = json!(f64::from(temperature) / 1000.0);
    }
    Ok(body)
}

pub(super) fn build_anthropic_request(
    style: ProviderStyle,
    model: &str,
    input: &ProviderInput,
) -> Result<Value, ProviderError> {
    let mut system = Vec::new();
    let mut messages = Vec::new();
    for message in &input.messages {
        match message {
            ModelMessage::System { content } => system.push(content.clone()),
            ModelMessage::User { content, .. } => messages.push(json!({
                "role": "user",
                "content": [{"type": "text", "text": content}],
            })),
            ModelMessage::Assistant {
                content,
                reasoning,
                reasoning_signature,
                reasoning_state: _,
                tool_calls,
            } => {
                let mut blocks = Vec::new();
                if let (Some(reasoning), Some(signature)) = (reasoning, reasoning_signature) {
                    blocks.push(json!({
                        "type": "thinking",
                        "thinking": reasoning,
                        "signature": signature,
                    }));
                }
                if !content.is_empty() {
                    blocks.push(json!({"type": "text", "text": content}));
                }
                blocks.extend(tool_calls.iter().map(|call| {
                    json!({
                        "type": "tool_use",
                        "id": sanitize_tool_id(&call.id),
                        "name": call.name,
                        "input": serde_json::from_str::<Value>(&call.arguments)
                            .unwrap_or_else(|_| json!({})),
                    })
                }));
                messages.push(json!({"role": "assistant", "content": blocks}));
            }
            ModelMessage::Tool {
                call_id,
                content,
                is_error: _,
            } => messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": sanitize_tool_id(call_id),
                    "content": content,
                }],
            })),
        }
    }
    if let Some(content) = messages
        .iter_mut()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|message| message.get_mut("content"))
        .and_then(Value::as_array_mut)
    {
        content.extend(input.images.iter().map(|image| {
            json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": image.media_type,
                    "data": image.data,
                }
            })
        }));
    } else if !input.images.is_empty() {
        return Err(ProviderError::InvalidRequest(
            "images require a user message".to_owned(),
        ));
    }
    let tools = input
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })
        })
        .collect::<Vec<_>>();
    let mut body = json!({
        "messages": messages,
        "max_tokens": input.limits.max_tokens,
    });
    if style == ProviderStyle::Anthropic {
        body["model"] = json!(model);
    } else {
        body["anthropic_version"] = json!("vertex-2023-10-16");
    }
    if input.stream {
        body["stream"] = json!(true);
    }
    if !system.is_empty() {
        body["system"] = json!(system.join("\n\n"));
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(choice) = &input.tool_choice {
        body["tool_choice"] = anthropic_tool_choice(choice);
    }
    if input.thinking {
        body["thinking"] = json!({
            "type": "enabled",
            "budget_tokens": u64::from(input.limits.max_tokens).min(2048),
        });
    }
    Ok(body)
}
