//! The documents the shell family publishes, and the text a model reads on them.
//!
//! Reference `_loop` renders every tool result the same way: the result model
//! is dumped to JSON and joined into one `key: value` line per field, in the
//! order the model declares its fields, with nothing appended. Nothing is
//! appended because `get_result_extra` on the reference tool base answers
//! `None` for every tool in the tree, so no shell tool contributes a second
//! block. The values are rendered with Python's own `str`, which is why `None`,
//! `True` and `False` reach the model rather than `null`, `true` and `false`,
//! and why a nested model is rendered as a Python mapping literal.
//!
//! This workspace's `serde_json` map orders its keys rather than keeping the
//! order they were written in, so a document cannot be a `Value` and still know
//! its own field order. It is an ordered list of fields instead, and both the
//! typed result and the rendered text are built from it. That is what makes
//! [`Document::into_output`] the single way this family publishes a result: a
//! shell tool that forgets to render its own text cannot exist.

use serde_json::{Map, Value};

use crate::pycompat;
use crate::tools::ToolExecutionOutput;

/// One published result: the fields the reference declares, in its order.
pub(super) struct Document {
    fields: Vec<(&'static str, Node)>,
}

/// What one field holds. A nested result model keeps its own field order, which
/// the rendered text depends on, so it stays a [`Document`] rather than
/// collapsing to a `Value` on the way in.
enum Node {
    Leaf(Value),
    Object(Document),
    List(Vec<Document>),
}

impl Document {
    pub(super) fn new() -> Self {
        Self { fields: Vec::new() }
    }

    #[must_use]
    pub(super) fn field(mut self, name: &'static str, value: impl Into<Value>) -> Self {
        self.fields.push((name, Node::Leaf(value.into())));
        self
    }

    /// A nested result model, or `null` where the reference declares the field
    /// optional and this action leaves it unset.
    #[must_use]
    pub(super) fn nested(mut self, name: &'static str, value: Option<Self>) -> Self {
        let node = value.map_or(Node::Leaf(Value::Null), Node::Object);
        self.fields.push((name, node));
        self
    }

    #[must_use]
    pub(super) fn nested_list(mut self, name: &'static str, values: Vec<Self>) -> Self {
        self.fields.push((name, Node::List(values)));
        self
    }

    /// One field's value, for a caller that decides on the result it just
    /// built rather than on the arguments it built it from.
    pub(super) fn get(&self, name: &str) -> Option<&Value> {
        self.fields
            .iter()
            .find(|(key, _)| *key == name)
            .and_then(|(_, node)| match node {
                Node::Leaf(value) => Some(value),
                Node::Object(_) | Node::List(_) => None,
            })
    }

    pub(super) fn typed(&self) -> Value {
        let mut map = Map::new();
        for (name, node) in &self.fields {
            map.insert((*name).to_owned(), node.value());
        }
        Value::Object(map)
    }

    pub(super) fn model_text(&self) -> String {
        let mut text = String::new();
        for (index, (name, node)) in self.fields.iter().enumerate() {
            if index > 0 {
                text.push('\n');
            }
            text.push_str(name);
            text.push_str(": ");
            node.display(&mut text);
        }
        text
    }

    pub(super) fn into_output(self, display: Value) -> ToolExecutionOutput {
        ToolExecutionOutput::new(self.model_text())
            .displayed_as(display)
            .typed(self.typed())
    }

    fn repr(&self, out: &mut String) {
        out.push('{');
        for (index, (name, node)) in self.fields.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&pycompat::repr_str(name));
            out.push_str(": ");
            node.repr(out);
        }
        out.push('}');
    }
}

impl Node {
    fn value(&self) -> Value {
        match self {
            Self::Leaf(value) => value.clone(),
            Self::Object(document) => document.typed(),
            Self::List(documents) => Value::Array(documents.iter().map(Document::typed).collect()),
        }
    }

    /// Python `str`, which the top level of a result is rendered with: a string
    /// is itself, and everything else is its `repr`.
    fn display(&self, out: &mut String) {
        match self {
            Self::Leaf(Value::String(text)) => out.push_str(text),
            other => other.repr(out),
        }
    }

    fn repr(&self, out: &mut String) {
        match self {
            Self::Leaf(value) => out.push_str(&pycompat::repr_json(value)),
            Self::Object(document) => document.repr(out),
            Self::List(documents) => {
                out.push('[');
                for (index, document) in documents.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    document.repr(out);
                }
                out.push(']');
            }
        }
    }
}

#[cfg(test)]
mod document_tests;
