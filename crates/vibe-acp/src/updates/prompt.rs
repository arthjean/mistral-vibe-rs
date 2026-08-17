//! Validating the untrusted prompt an editor sends, and projecting it onto the
//! canonical turn request.
//!
//! This is the one place in the adapter that reads input it did not produce,
//! so every bound the transport does not enforce is enforced here.

use base64::Engine;
use serde_json::Value;
use vibe_app_server::client::{PublicContentBlock, TurnRequest};

use crate::protocol::AcpError;

const MAX_EMBEDDED_CONTENT_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_PROMPT_BLOCKS: usize = 256;

/// Validates untrusted ACP prompt blocks and projects them onto the canonical
/// turn request.
pub(crate) fn turn_request(content: Vec<Value>) -> Result<TurnRequest, AcpError> {
    if content.is_empty() {
        return Err(AcpError::InvalidParams(
            "prompt must contain at least one content block".to_owned(),
        ));
    }
    if content.len() > MAX_PROMPT_BLOCKS {
        return Err(AcpError::InvalidParams(format!(
            "prompt cannot contain more than {MAX_PROMPT_BLOCKS} content blocks"
        )));
    }
    let mut public = Vec::with_capacity(content.len());
    let mut text = Vec::new();
    let mut content_bytes = 0_usize;
    for block in &content {
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| AcpError::InvalidParams("content block type is required".to_owned()))?;
        content_bytes = content_bytes.saturating_add(serde_json::to_vec(block)?.len());
        if content_bytes > MAX_EMBEDDED_CONTENT_BYTES {
            return Err(AcpError::InvalidParams(format!(
                "prompt content exceeds {MAX_EMBEDDED_CONTENT_BYTES} bytes"
            )));
        }
        public.push(prompt_block(block, kind, &mut text)?);
    }
    if text.is_empty() {
        let fallback = "[Attached content]".to_owned();
        public.insert(
            0,
            PublicContentBlock::Text {
                text: fallback.clone(),
            },
        );
        text.push(fallback);
    }
    Ok(TurnRequest {
        prompt: text.join("\n\n"),
        input: public,
        injected: false,
        client_user_message_id: None,
        auto_title: None,
        user_display_content: Some(Value::Array(content)),
        mention_stats: None,
    })
}

fn prompt_block(
    block: &Value,
    kind: &str,
    text: &mut Vec<String>,
) -> Result<PublicContentBlock, AcpError> {
    match kind {
        "text" => {
            let value = block
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| AcpError::InvalidParams("text block requires text".to_owned()))?
                .to_owned();
            text.push(value.clone());
            Ok(PublicContentBlock::Text { text: value })
        }
        "image" => {
            let mime = block
                .get("mimeType")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    AcpError::InvalidParams("image block requires mimeType".to_owned())
                })?;
            if !matches!(
                mime,
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            ) {
                return Err(AcpError::InvalidParams(format!(
                    "unsupported image MIME type `{mime}`"
                )));
            }
            let data = block.get("data").and_then(Value::as_str).ok_or_else(|| {
                AcpError::InvalidParams("image block requires base64 data".to_owned())
            })?;
            if data.is_empty()
                || base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .is_err()
            {
                return Err(AcpError::InvalidParams(
                    "image data is not canonical base64".to_owned(),
                ));
            }
            Ok(PublicContentBlock::Image {
                attachment: block.clone(),
            })
        }
        "resource" | "resource_link" => {
            let has_uri = block
                .get("uri")
                .or_else(|| block.pointer("/resource/uri"))
                .and_then(Value::as_str)
                .is_some_and(|uri| !uri.trim().is_empty());
            if !has_uri {
                return Err(AcpError::InvalidParams(
                    "resource block requires a URI".to_owned(),
                ));
            }
            Ok(PublicContentBlock::Resource {
                resource: block.clone(),
            })
        }
        unsupported => Err(AcpError::InvalidParams(format!(
            "unsupported content block `{unsupported}`"
        ))),
    }
}
