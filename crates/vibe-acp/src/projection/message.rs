//! Message and reasoning entries as message and thought chunks.

use serde_json::{Value, json};

use super::{object, text_field};

/// The concatenated text of a message's text blocks, which is what a
/// revision's delta is measured against.
pub(super) fn message_text(entry: &Value) -> String {
    entry
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect()
}

pub(super) fn message_updates(entry: &Value, text: Option<&str>) -> Vec<Value> {
    if let Some(text) = text {
        return vec![message_chunk(entry, json!({"type": "text", "text": text}))];
    }
    let mut updates = Vec::new();
    for block in entry
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let content = match block.get("type").and_then(Value::as_str) {
            Some("text") => match block.get("text").and_then(Value::as_str) {
                Some(text) if !text.is_empty() => json!({"type": "text", "text": text}),
                _ => continue,
            },
            Some("image") => match image_block(block.get("attachment")) {
                Some(content) => content,
                None => continue,
            },
            Some("resource") => match resource_block(block.get("resource")) {
                Some(content) => content,
                None => continue,
            },
            _ => continue,
        };
        updates.push(message_chunk(entry, content));
    }
    updates
}

fn image_block(attachment: Option<&Value>) -> Option<Value> {
    let attachment = attachment?;
    let source = attachment.get("source")?;
    let mime_type = attachment.get("mimeType").cloned().unwrap_or(Value::Null);
    match source.get("kind").and_then(Value::as_str) {
        Some("inline") => Some(object(vec![
            ("type", json!("image")),
            ("data", source.get("data").cloned().unwrap_or(Value::Null)),
            ("mimeType", mime_type),
        ])),
        Some("file") => Some(object(vec![
            ("type", json!("resource_link")),
            (
                "name",
                attachment.get("alias").cloned().unwrap_or(Value::Null),
            ),
            ("uri", source.get("path").cloned().unwrap_or(Value::Null)),
            ("mimeType", mime_type),
        ])),
        _ => None,
    }
}

fn resource_block(resource: Option<&Value>) -> Option<Value> {
    let resource = resource?;
    let field = |key: &str| resource.get(key).cloned().unwrap_or(Value::Null);
    match resource.get("kind").and_then(Value::as_str) {
        Some("text") => Some(json!({
            "type": "resource",
            "resource": object(vec![
                ("uri", field("uri")),
                ("mimeType", field("mediaType")),
                ("text", field("text")),
            ]),
        })),
        Some("blob") => Some(json!({
            "type": "resource",
            "resource": object(vec![
                ("uri", field("uri")),
                ("mimeType", field("mediaType")),
                ("blob", field("blob")),
            ]),
        })),
        Some("link") => {
            let name = ["name", "title", "uri"]
                .into_iter()
                .find_map(|key| {
                    resource
                        .get(key)
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                })
                .map_or(Value::Null, |name| json!(name));
            Some(object(vec![
                ("type", json!("resource_link")),
                ("name", name),
                ("uri", field("uri")),
                ("title", field("title")),
                ("description", field("description")),
                ("mimeType", field("mediaType")),
                ("size", field("size")),
            ]))
        }
        _ => None,
    }
}

fn message_chunk(entry: &Value, content: Value) -> Value {
    let id = entry.get("id").cloned().unwrap_or(Value::Null);
    if entry.get("role").and_then(Value::as_str) == Some("user") {
        let meta = entry
            .get("userDisplayContent")
            .filter(|display| !display.is_null())
            .map_or(
                Value::Null,
                |display| json!({"user_display_content": display}),
            );
        return object(vec![
            ("sessionUpdate", json!("user_message_chunk")),
            ("content", content),
            ("messageId", id),
            ("_meta", meta),
        ]);
    }
    object(vec![
        ("sessionUpdate", json!("agent_message_chunk")),
        ("content", content),
        ("messageId", id),
    ])
}

pub(super) fn reasoning_updates(entry: &Value, text: Option<&str>) -> Vec<Value> {
    let value = text.unwrap_or_else(|| text_field(entry, "text"));
    if value.is_empty() {
        return Vec::new();
    }
    vec![object(vec![
        ("sessionUpdate", json!("agent_thought_chunk")),
        ("content", json!({"type": "text", "text": value})),
        ("messageId", entry.get("id").cloned().unwrap_or(Value::Null)),
    ])]
}
