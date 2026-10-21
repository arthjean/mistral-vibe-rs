//! The conversation a session's model reads, and how its content reaches a
//! model call.

use serde_json::{Value, json};
use vibe_core::events::{ModelMessage, ModelToolCall};

/// One message the model is sent back on the next call.
#[derive(Debug, Clone, PartialEq)]
pub enum ConversationMessage {
    User {
        content: Vec<Value>,
        /// Written by the harness rather than typed: injected context, a
        /// compaction's prompt and handoff. A compaction preserves only the
        /// messages a person wrote.
        injected: bool,
    },
    Assistant {
        text: String,
        reasoning: Option<String>,
        tool_calls: Vec<ModelToolCall>,
    },
    Tool {
        call_id: String,
        name: String,
        content: String,
    },
}

impl ConversationMessage {
    /// The message as a model call sends it. A model that reads no image is
    /// handed the file an image was read from in its place
    /// (`image_delivery` `resource_link`); an inline image stays, which the
    /// call then refuses.
    pub(crate) fn to_model(&self, native_images: bool) -> ModelMessage {
        match self {
            Self::User { content, .. } => {
                let delivered = delivered_content(content, native_images);
                ModelMessage::User {
                    content: content_text(&delivered),
                    injected: false,
                    message_id: None,
                    attachments: image_parts(&delivered),
                    user_display_content: None,
                    input_text: None,
                    manual_shell: None,
                    compaction_boundary: false,
                }
            }
            Self::Assistant {
                text,
                reasoning,
                tool_calls,
            } => ModelMessage::Assistant {
                message_id: None,
                reasoning_message_id: None,
                content: text.clone(),
                reasoning: reasoning.clone(),
                reasoning_payloads: Vec::new(),
                tool_calls: tool_calls.clone(),
                keeps_empty_content: false,
            },
            Self::Tool {
                call_id,
                name,
                content,
            } => ModelMessage::Tool {
                call_id: call_id.clone(),
                content: content.clone(),
                is_error: false,
                name: name.clone(),
                result: None,
            },
        }
    }

    /// Whether the message still holds an image once delivered to a model
    /// that reads them only when `native_images`.
    #[must_use]
    pub fn holds_image(&self, native_images: bool) -> bool {
        match self {
            Self::User { content, .. } => delivered_content(content, native_images)
                .iter()
                .any(|block| block["type"] == "image"),
            _ => false,
        }
    }

    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::User { content, injected } => {
                let mut message = json!({"role": "user", "content": content});
                if *injected {
                    message["injected"] = json!(true);
                }
                message
            }
            Self::Assistant {
                text,
                reasoning,
                tool_calls,
            } => json!({
                "role": "assistant",
                "text": text,
                "reasoning": reasoning,
                "toolCalls": tool_calls
                    .iter()
                    .map(|call| json!({"id": call.id, "name": call.name, "arguments": call.arguments}))
                    .collect::<Vec<_>>(),
            }),
            Self::Tool {
                call_id,
                name,
                content,
            } => json!({"role": "tool", "callId": call_id, "name": name, "content": content}),
        }
    }

    #[must_use]
    pub fn from_json(value: &Value) -> Option<Self> {
        let text = |key: &str| value[key].as_str().unwrap_or_default().to_owned();
        Some(match value["role"].as_str()? {
            "user" => Self::User {
                content: value["content"].as_array().cloned().unwrap_or_default(),
                injected: value["injected"].as_bool().unwrap_or(false),
            },
            "assistant" => Self::Assistant {
                text: text("text"),
                reasoning: value["reasoning"].as_str().map(ToOwned::to_owned),
                tool_calls: value["toolCalls"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|call| ModelToolCall {
                        id: call["id"].as_str().unwrap_or_default().to_owned(),
                        name: call["name"].as_str().unwrap_or_default().to_owned(),
                        arguments: call["arguments"].as_str().unwrap_or_default().to_owned(),
                        presentation: None,
                    })
                    .collect(),
            },
            "tool" => Self::Tool {
                call_id: text("callId"),
                name: text("name"),
                content: text("content"),
            },
            _ => return None,
        })
    }
}

/// The text blocks of public content, joined as the adapter joins them.
fn content_text(content: &[Value]) -> String {
    content
        .iter()
        .filter_map(block_text)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Reference `_block_text`: what one block reads as, when it reads as text.
fn block_text(block: &Value) -> Option<String> {
    match block["type"].as_str() {
        Some("text") => block["text"].as_str().map(ToOwned::to_owned),
        Some("resource_link") => block["uri"]
            .as_str()
            .map(|uri| format!("[resource: {uri}]")),
        Some("resource") => resource_text(&block["resource"]),
        _ => None,
    }
}

/// Reference `FILE_IMAGE_RESOURCE_LINK_META_KEY`: where an image read from
/// a file keeps the link a blind model is handed instead.
pub const FILE_IMAGE_LINK: &str = "mistralai.vibe.harness/file-image-resource-link";

/// `content` as a model call takes it: an image read from a file becomes
/// the link to that file when the model reads no image.
fn delivered_content(content: &[Value], native_images: bool) -> Vec<Value> {
    content
        .iter()
        .map(|block| {
            let link = &block["_meta"][FILE_IMAGE_LINK];
            if native_images || block["type"] != "image" || !link["uri"].is_string() {
                return block.clone();
            }
            json!({"type": "resource_link", "uri": link["uri"], "name": link["name"]})
        })
        .collect()
}

/// The blocks of `content` as the chunks of a message that carries an
/// image, in order; none when it carries none.
fn image_parts(content: &[Value]) -> Vec<vibe_core::events::PublicContentBlock> {
    use vibe_core::events::PublicContentBlock;
    if !content.iter().any(|block| block["type"] == "image") {
        return Vec::new();
    }
    content
        .iter()
        .filter_map(|block| {
            if block["type"] == "image" {
                return Some(PublicContentBlock::Image {
                    attachment: json!({"mimeType": block["mimeType"], "data": block["data"]}),
                });
            }
            block_text(block).map(|text| PublicContentBlock::Text { text })
        })
        .collect()
}

/// Reference `_resource_text` (the harness's Mistral adapter): an embedded
/// text resource as the model reads it, fenced by more backticks than it
/// holds in a row; a link names its URI.
fn resource_text(resource: &Value) -> Option<String> {
    let uri = resource["uri"].as_str()?;
    let Some(text) = resource["text"].as_str() else {
        return (resource["kind"] == "link").then(|| format!("[resource: {uri}]"));
    };
    let mut fence = "````".to_owned();
    while text.contains(&fence) {
        fence.push('`');
    }
    let escaped = uri
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;");
    Some(format!("Resource: {escaped}\n{fence}\n{text}\n{fence}"))
}

/// Reference `_rust_content_blocks`: a message a client named carries its
/// identifier on each text block, which is how the harness finds the message
/// a rewind or a fork names.
pub(super) fn tagged(content: &[Value], client_message_id: Option<&str>) -> Vec<Value> {
    let Some(id) = client_message_id else {
        return content.to_vec();
    };
    content
        .iter()
        .map(|block| {
            let mut block = block.clone();
            if block["type"] == "text"
                && let Some(object) = block.as_object_mut()
            {
                object.insert("_meta".to_owned(), json!({CLIENT_MESSAGE_KEY: id}));
            }
            block
        })
        .collect()
}

/// Where a client's message identifier rides a text block.
const CLIENT_MESSAGE_KEY: &str = "vibe_client_message_id";

/// The conversation position of the user message a client named `entry_id`.
pub(super) fn client_message_position(
    conversation: &[ConversationMessage],
    entry_id: &str,
) -> Option<usize> {
    conversation.iter().position(|message| match message {
        ConversationMessage::User { content, .. } => content
            .iter()
            .any(|block| block["_meta"][CLIENT_MESSAGE_KEY] == entry_id),
        _ => false,
    })
}

/// `content` without the identifiers [`tagged`] put on it.
pub(super) fn untagged(content: &[Value]) -> Vec<Value> {
    content
        .iter()
        .map(|block| {
            let mut block = block.clone();
            if let Some(object) = block.as_object_mut() {
                object.remove("_meta");
            }
            block
        })
        .collect()
}
