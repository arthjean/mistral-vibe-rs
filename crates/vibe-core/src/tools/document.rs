//! The documents the reference tools publish, and the text a model reads on
//! them.
//!
//! Reference `_loop` renders every tool result the same way: the result model
//! is dumped to JSON and joined into one `key: value` line per field, in the
//! order the model declares its fields, with nothing appended. Nothing is
//! appended because `get_result_extra` on the reference tool base answers
//! `None` for every tool in the tree, so no tool contributes a second block.
//! The values are rendered with Python's own `str`, which is why `None`,
//! `True` and `False` reach the model rather than `null`, `true` and `false`,
//! and why a nested model is rendered as a Python mapping literal.
//!
//! A document is one ordered list of fields, and both the typed result and the
//! rendered text are built from it. That is what makes
//! [`Document::into_output`] the single way a tool built on it publishes a
//! result: a tool that forgets to render its own text, or renders a field the
//! typed result does not carry, cannot exist.

use serde_json::{Map, Value};

use crate::pycompat;
use crate::tools::ToolExecutionOutput;

/// One published result: the fields the reference declares, in its order.
///
/// A nested result model is a field whose value is another document. The
/// workspace's maps keep insertion order, so it keeps its own field order once
/// it is a JSON value, and the rendered text is the Python `str` of each value.
#[derive(Default)]
pub struct Document {
    fields: Map<String, Value>,
}

impl Document {
    #[must_use]
    pub fn new() -> Self {
        Self { fields: Map::new() }
    }

    /// One field. A nested result model, an optional one left unset (`null`),
    /// and a list of them are a [`Document`], an `Option<Document>` and a
    /// `Vec<Document>`.
    #[must_use]
    pub fn field(mut self, name: &'static str, value: impl Into<Value>) -> Self {
        self.fields.insert(name.to_owned(), value.into());
        self
    }

    #[must_use]
    pub fn typed(&self) -> Value {
        Value::Object(self.fields.clone())
    }

    #[must_use]
    pub fn model_text(&self) -> String {
        self.fields
            .iter()
            .map(|(name, value)| format!("{name}: {}", pycompat::str_json(value)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[must_use]
    pub fn into_output(self, display: Value) -> ToolExecutionOutput {
        ToolExecutionOutput::new(self.model_text())
            .displayed_as(display)
            .typed(Value::Object(self.fields))
    }
}

impl From<Document> for Value {
    fn from(document: Document) -> Self {
        Self::Object(document.fields)
    }
}

#[cfg(test)]
mod document_tests;
