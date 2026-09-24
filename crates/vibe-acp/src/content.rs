//! A prompt's ACP blocks, read as the turn the app server runs.
//!
//! Reference `project_prompt` (`vibe/acp/content.py`) and
//! `extract_image_attachments` (`vibe/acp/image_blocks.py`): the text blocks
//! are joined, embedded and linked resources become user resources, images
//! become inline attachments, and the blocks a client marked automatic are
//! moved after the ones the user wrote.

use base64::Engine;
use serde_json::{Map, Value, json};

use crate::protocol::AcpError;

/// Reference `MAX_IMAGES_PER_MESSAGE`.
const MAX_IMAGES_PER_MESSAGE: usize = 8;
/// Reference `MAX_IMAGE_BYTES`.
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// What a prompt is sent as.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProjectedPrompt {
    pub(crate) text: String,
    pub(crate) images: Vec<Value>,
    pub(crate) resources: Vec<Value>,
}

pub(crate) fn project_prompt(blocks: &[Value]) -> Result<ProjectedPrompt, AcpError> {
    let automatic = |block: &Value| {
        block
            .get("_meta")
            .and_then(|meta| meta.get("automatic"))
            .is_some_and(truthy)
    };
    let ordered = blocks
        .iter()
        .filter(|block| !automatic(block))
        .chain(blocks.iter().filter(|block| automatic(block)))
        .collect::<Vec<_>>();
    let mut text = Vec::new();
    let mut resources = Vec::new();
    for block in &ordered {
        let field = |key: &str| block.get(key).cloned().unwrap_or(Value::Null);
        match block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text" => text.push(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            "resource" => {
                let resource = block.get("resource").unwrap_or(&Value::Null);
                let inner = |key: &str| resource.get(key).cloned().unwrap_or(Value::Null);
                resources.push(if resource.get("text").is_some() {
                    object([
                        ("kind", json!("text")),
                        ("uri", inner("uri")),
                        ("mediaType", inner("mimeType")),
                        ("text", inner("text")),
                    ])
                } else {
                    object([
                        ("kind", json!("blob")),
                        ("uri", inner("uri")),
                        ("mediaType", inner("mimeType")),
                        ("blob", inner("blob")),
                    ])
                });
            }
            "resource_link" => resources.push(object([
                ("kind", json!("link")),
                ("uri", field("uri")),
                ("mediaType", field("mimeType")),
                ("name", field("name")),
                ("title", field("title")),
                ("description", field("description")),
                ("size", field("size")),
            ])),
            "image" => {}
            other => {
                return Err(AcpError::InvalidParams(format!(
                    "prompts cannot carry `{other}` content"
                )));
            }
        }
    }
    Ok(ProjectedPrompt {
        text: text.join("\n\n"),
        images: image_attachments(&ordered)?,
        resources,
    })
}

/// Reference `extract_image_attachments`.
fn image_attachments(blocks: &[&Value]) -> Result<Vec<Value>, AcpError> {
    let images = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("image"))
        .collect::<Vec<_>>();
    if images.len() > MAX_IMAGES_PER_MESSAGE {
        return Err(AcpError::InvalidImage {
            detail: format!(
                "a prompt carries at most {MAX_IMAGES_PER_MESSAGE} images, and this one carries {}",
                images.len()
            ),
            reason: "too_many".to_owned(),
        });
    }
    images
        .into_iter()
        .map(|block| image_attachment(block))
        .collect()
}

fn image_attachment(block: &Value) -> Result<Value, AcpError> {
    let mime_type = block
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let extension = match mime_type {
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        _ => {
            return Err(AcpError::InvalidImage {
                detail: format!("images of type `{mime_type}` are not supported"),
                reason: "wrong_type".to_owned(),
            });
        }
    };
    let data = block
        .get("data")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|error| AcpError::InvalidImage {
            detail: format!("the image data is not valid base64: {error}"),
            reason: "invalid_base64".to_owned(),
        })?;
    if decoded.len() > MAX_IMAGE_BYTES {
        return Err(AcpError::InvalidImage {
            detail: format!(
                "an image may weigh {MAX_IMAGE_BYTES} bytes, and this one weighs {}",
                decoded.len()
            ),
            reason: "too_large".to_owned(),
        });
    }
    let alias = block
        .get("uri")
        .and_then(Value::as_str)
        .filter(|uri| !uri.is_empty())
        .and_then(|uri| std::path::Path::new(uri).file_name())
        .map_or_else(
            || format!("pasted-image{extension}"),
            |name| name.to_string_lossy().into_owned(),
        );
    Ok(json!({
        "source": {"kind": "inline", "data": data},
        "alias": alias,
        "mimeType": mime_type,
    }))
}

/// Reference `UserDisplayContent`: a version and a host that are not blank,
/// and a list of objects, with nothing else.
pub(crate) fn validate_display_content(value: &Value) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| "the display content must be an object".to_owned())?;
    for key in ["version", "host"] {
        match object.get(key) {
            Some(Value::String(text)) if !text.trim().is_empty() => {}
            Some(Value::String(_)) => return Err(format!("`{key}` must not be blank")),
            Some(_) => return Err(format!("`{key}` must be a string")),
            None => return Err(format!("`{key}` is required")),
        }
    }
    match object.get("content") {
        Some(Value::Array(items)) if items.iter().all(Value::is_object) => {}
        Some(_) => return Err("`content` must be a list of objects".to_owned()),
        None => return Err("`content` is required".to_owned()),
    }
    if let Some(extra) = object
        .keys()
        .find(|key| !["version", "host", "content"].contains(&key.as_str()))
    {
        return Err(format!("`{extra}` is not a display content field"));
    }
    Ok(())
}

/// Python truthiness for a JSON value.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
    }
}

/// An object without its `null` members.
fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .filter(|(_, value)| !value.is_null())
            .map(|(key, value)| (key.to_owned(), value))
            .collect::<Map<_, _>>(),
    )
}
