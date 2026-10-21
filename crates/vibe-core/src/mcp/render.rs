//! What an MCP call hands back: reference `MCPToolResult`, and the text the
//! model reads for it.
//!
//! Reference `_parse_call_result` (`vibe/core/tools/mcp/tools.py`) keeps the
//! structured content when the server sent any and otherwise joins the text
//! blocks, and ignores the result's error flag: a result the server marked as
//! failed still reaches the model as `ok: True`. The agent loop renders the
//! model one `field: value` line per field, which spells the structured
//! content as Python's `repr` of a dictionary, keys in the order the server
//! sent them.

use serde_json::Value;

use crate::tools::document::Document;

/// Reference `MCPToolResult` for one answered call.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct McpToolResult {
    pub(crate) server: String,
    pub(crate) tool: String,
    pub(crate) text: Option<String>,
    pub(crate) structured: Option<Value>,
}

impl McpToolResult {
    /// Reference `_parse_call_result` over a validated `CallToolResult`.
    pub(crate) fn parse(server: String, tool: String, result: &Value) -> Self {
        let structured = result
            .get("structuredContent")
            .filter(|structured| structured.is_object())
            .cloned();
        if structured.is_some() {
            return Self {
                server,
                tool,
                text: None,
                structured,
            };
        }
        let parts = result
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Self {
            server,
            tool,
            text: (!parts.is_empty()).then(|| parts.join("\n")),
            structured: None,
        }
    }

    /// The published result, `MCPToolResult` in its declared field order.
    pub(crate) fn document(self) -> Document {
        Document::new()
            .field("ok", true)
            .field("server", self.server)
            .field("tool", self.tool)
            .field("text", self.text)
            .field("structured", self.structured)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn structured_content_keeps_the_server_key_order() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[],"structuredContent":{"z":1,"a":[true,null,"x'y",2.5]}}}"#;
        let message: Value = serde_json::from_str(raw).expect("fixture");
        let result = McpToolResult::parse(
            "stdio:server".to_owned(),
            "tool".to_owned(),
            &message["result"],
        );
        assert_eq!(
            result.document().model_text(),
            "ok: True\nserver: stdio:server\ntool: tool\ntext: None\nstructured: {'z': 1, 'a': [True, None, \"x'y\", 2.5]}"
        );
    }

    #[test]
    fn text_blocks_join_and_an_error_flag_is_ignored() {
        let result = json!({
            "content": [
                {"type": "text", "text": "one"},
                {"type": "image", "data": "aGk=", "mimeType": "image/png"},
                {"type": "text", "text": "two"},
            ],
            "isError": true,
        });
        let parsed = McpToolResult::parse("s".to_owned(), "t".to_owned(), &result);
        assert_eq!(parsed.text.as_deref(), Some("one\ntwo"));
        let image_only = json!({"content": [{"type": "image", "data": "", "mimeType": "x"}]});
        let parsed = McpToolResult::parse("s".to_owned(), "t".to_owned(), &image_only);
        assert_eq!(parsed.text, None);
    }
}
