//! The `todo` tool: the plan the model keeps for itself.
//!
//! A call replaces the whole list rather than patching it, because the model
//! answers with the plan as it now stands. The transcript renders the widget
//! from the typed result, so the items travel there rather than in the display
//! metadata.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use serde_json::{Value, json};

use super::{TodoItem, declared_document};
use crate::schema::{ObjectSchema, Property};
use crate::tools::config::TodoConfig;
use crate::tools::{
    ToolAvailability, ToolError, ToolExecutionOutput, ToolPresentationKind, ToolSource, ToolSpec,
    reference_text,
};

/// Directive coverage for `todo`, whose reference description this port must
/// cover without reproducing (`NOTICE`).
///
/// | Reference directive | Covered by |
/// |---|---|
/// | Every call names an action, `read` or `write` | the `action` description |
/// | `write` replaces the whole list rather than patching it | "replaces the whole list" |
/// | Ids are stable across updates so an item can be re-stated | the `id` description |
/// | Exactly one item is `in_progress` at a time | "Keep one item in_progress" |
/// | An item is completed as soon as it is done, not in a batch at the end | "mark it completed as soon as it is done" |
pub(super) fn todo_spec() -> ToolSpec {
    let item = ObjectSchema::new()
        .define(
            "TodoStatus",
            Property::string().constrained("enum", json!(TodoItem::STATUSES)),
        )
        .define(
            "TodoPriority",
            Property::string().constrained("enum", json!(TodoItem::PRIORITIES)),
        )
        .required(
            "id",
            Property::string().described("A stable identifier for the task, reused across updates"),
        )
        .required(
            "content",
            Property::string().described("A short description of the task"),
        )
        .optional(
            "status",
            Property::reference("TodoStatus")
                .described("Where the task stands: pending, in_progress, completed, cancelled")
                .with_default("pending"),
        )
        .optional(
            "priority",
            Property::reference("TodoPriority")
                .described("How much the task matters: high, medium, low")
                .with_default("medium"),
        );
    ToolSpec {
        name: "todo".to_owned(),
        description: "Track a multi-step task. `read` returns the current list, `write` replaces \
                      the whole list with the one you send. Keep one item in_progress at a time \
                      and mark it completed as soon as it is done rather than settling the list \
                      at the end."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .define("TodoItem", item)
            .required(
                "action",
                Property::string().described(
                    "Required on every call: `read` to view the current list, `write` to replace \
                     it",
                ),
            )
            .optional(
                "todos",
                Property::array(Property::reference("TodoItem"))
                    .described(
                        "Required when action is `write`: the whole list, which replaces the \
                         previous one",
                    )
                    .with_default(Value::Null)
                    .nullable(),
            )
            .build(),
        output_schema: None,
        config: declared_document("todo"),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Generic,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

pub(super) fn run_todo(
    todos: &Mutex<BTreeMap<String, Vec<TodoItem>>>,
    session_id: &str,
    arguments: &Value,
    settings: &TodoConfig,
) -> Result<ToolExecutionOutput, ToolError> {
    let action = arguments["action"].as_str().unwrap_or_default();
    let mut stored = todos
        .lock()
        .map_err(|_| ToolError::Execution("the todo list lock is poisoned".to_owned()))?;
    let (verb, items) = match action {
        // A list that was never written reads back empty, not as an error.
        "read" => (
            "Retrieved",
            stored.get(session_id).cloned().unwrap_or_default(),
        ),
        "write" => {
            let items = parse_todo_items(&arguments["todos"])?;
            if items.len() > settings.max_todos {
                return Err(ToolError::Execution(format!(
                    "the todo list holds {} items, exceeding the {}-item limit",
                    items.len(),
                    settings.max_todos
                )));
            }
            let mut seen = BTreeSet::new();
            for item in &items {
                if !seen.insert(item.id.as_str()) {
                    return Err(ToolError::Execution(format!(
                        "todo id `{}` appears more than once",
                        item.id
                    )));
                }
            }
            stored.insert(session_id.to_owned(), items.clone());
            ("Updated", items)
        }
        other => {
            return Err(ToolError::Execution(format!(
                "unknown todo action `{other}`; use `read` or `write`"
            )));
        }
    };
    drop(stored);
    let total = items.len();
    // Reference `TodoResult.message` is a computed field, so it is rendered
    // from the verb and the count rather than stored.
    let message = format!("{verb} {total} todos");
    let rendered = items
        .iter()
        .map(TodoItem::rendered_fields)
        .collect::<Vec<_>>();
    Ok(ToolExecutionOutput::new(reference_text::joined(&[
        ("verb", verb.to_owned()),
        ("todos", reference_text::dictionary_list(&rendered)),
        ("total_count", total.to_string()),
        ("message", message.clone()),
    ]))
    .displayed_as(json!({"kind": "todo", "count": total}))
    // The transcript renders the todo widget from the typed result, so the
    // items travel there rather than in the display metadata.
    .typed(json!({
        "verb": verb,
        "todos": items,
        "total_count": total,
        "message": message,
    })))
}

pub(super) fn parse_todo_items(value: &Value) -> Result<Vec<TodoItem>, ToolError> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let entries = value.as_array().ok_or_else(|| ToolError::SchemaViolation {
        path: "/todos".to_owned(),
        message: "must be an array of todo items".to_owned(),
    })?;
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            serde_json::from_value::<TodoItem>(entry.clone()).map_err(|error| {
                ToolError::SchemaViolation {
                    path: format!("/todos/{index}"),
                    message: error.to_string(),
                }
            })
        })
        .collect()
}
