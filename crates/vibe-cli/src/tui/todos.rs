//! The todo list a Unified Harness session pins under the composer.
//!
//! Reference `TodoTracker` (`vibe/cli/textual_ui/todo_tracker.py`) and
//! `TodoStatusRow` (`vibe/cli/textual_ui/widgets/todo_status.py`): only the
//! unified core publishes todo effects a client can follow, so the tracker
//! exists only on that backend. It follows each settled `todo` result as it
//! arrives, the one-line row under the composer summarizes the latest list,
//! and every todo result's header names what changed since the previous one
//! instead of the body restating the list.

use std::collections::BTreeMap;

use serde_json::Value;
use vibe_core::events::{PublicEffectState, PublicHistoryEntry, ToolEffectKind};

use super::state::TranscriptEntry;

/// Reference `TodoEffectStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl TodoStatus {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// Reference `status_icon`.
    pub(super) const fn icon(self) -> &'static str {
        match self {
            Self::Pending => "☐",
            Self::InProgress => "▶",
            Self::Completed => "☑",
            Self::Cancelled => "☒",
        }
    }

    const fn transition(self) -> &'static str {
        match self {
            Self::InProgress => "started",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Pending => "reopened",
        }
    }
}

/// The order transitions are counted in, and the order the overlay groups by.
const TRANSITION_ORDER: [TodoStatus; 4] = [
    TodoStatus::InProgress,
    TodoStatus::Completed,
    TodoStatus::Cancelled,
    TodoStatus::Pending,
];

/// Reference `TodoOverlayScreen._STATUS_ORDER`.
pub(super) const OVERLAY_ORDER: [TodoStatus; 4] = [
    TodoStatus::InProgress,
    TodoStatus::Pending,
    TodoStatus::Completed,
    TodoStatus::Cancelled,
];

/// Reference `TodoEffectItem`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TodoItem {
    pub(super) id: String,
    pub(super) content: String,
    pub(super) status: TodoStatus,
}

/// Reference `TodoTracker`: the list the last settled todo result held.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct TodoTracker {
    todos: Vec<TodoItem>,
}

impl TodoTracker {
    pub(super) fn todos(&self) -> &[TodoItem] {
        &self.todos
    }

    /// The text the pinned row shows, or `None` to hide it.
    pub(super) fn summary(&self) -> Option<String> {
        summary_line(&self.todos)
    }

    /// Adopts `todos`, answering how they differ from the list they replace.
    pub(super) fn record(&mut self, todos: Vec<TodoItem>) -> String {
        let delta = summarize_change(&self.todos, &todos);
        self.todos = todos;
        delta
    }

    /// Follows `entry` when it is a settled todo result.
    pub(super) fn follow(&mut self, entry: &TranscriptEntry) {
        if let Some(todos) = entry_todos(entry) {
            self.record(todos);
        }
    }
}

/// Reference `progress_label`: a cancelled item leaves the denominator.
pub(super) fn progress_label(todos: &[TodoItem]) -> String {
    let countable = todos
        .iter()
        .filter(|todo| todo.status != TodoStatus::Cancelled);
    let total = countable.clone().count();
    let done = countable
        .filter(|todo| todo.status == TodoStatus::Completed)
        .count();
    format!("{done}/{total}")
}

/// Reference `summary_line`.
fn summary_line(todos: &[TodoItem]) -> Option<String> {
    if todos.is_empty() {
        return None;
    }
    let progress = progress_label(todos);
    let focus = todos
        .iter()
        .find(|todo| todo.status == TodoStatus::InProgress)
        .or_else(|| todos.iter().find(|todo| todo.status == TodoStatus::Pending));
    Some(match focus {
        Some(todo) => format!("{} {progress} · {}", todo.status.icon(), todo.content),
        None if todos
            .iter()
            .all(|todo| todo.status == TodoStatus::Cancelled) =>
        {
            format!(
                "{} {progress} · All todos cancelled",
                TodoStatus::Cancelled.icon()
            )
        }
        None => format!(
            "{} {progress} · All todos complete",
            TodoStatus::Completed.icon()
        ),
    })
}

/// Reference `summarize_change`.
fn summarize_change(previous: &[TodoItem], current: &[TodoItem]) -> String {
    if current.is_empty() {
        return if previous.is_empty() {
            "No todos"
        } else {
            "Cleared todos"
        }
        .to_owned();
    }
    let before = previous
        .iter()
        .map(|todo| (todo.id.as_str(), todo))
        .collect::<BTreeMap<_, _>>();
    let added = current
        .iter()
        .filter(|todo| !before.contains_key(todo.id.as_str()))
        .count();
    let removed = previous
        .iter()
        .filter(|todo| current.iter().all(|kept| kept.id != todo.id))
        .count();
    let mut transitions = BTreeMap::<TodoStatus, usize>::new();
    for todo in current {
        if let Some(earlier) = before.get(todo.id.as_str())
            && earlier.status != todo.status
        {
            *transitions.entry(todo.status).or_default() += 1;
        }
    }
    let mut parts = Vec::new();
    if added > 0 {
        parts.push(format!("+{added}"));
    }
    if removed > 0 {
        parts.push(format!("\u{2212}{removed}"));
    }
    parts.extend(TRANSITION_ORDER.iter().filter_map(|status| {
        transitions
            .get(status)
            .map(|count| format!("{count} {}", status.transition()))
    }));
    if parts.is_empty() {
        parts.push("no changes".to_owned());
    }
    format!("{} · {} done", parts.join(" · "), progress_label(current))
}

/// Reference `_entry_todos`: the list a settled todo result carries, when its
/// output validates as `TodoEffectOutput`.
fn entry_todos(entry: &TranscriptEntry) -> Option<Vec<TodoItem>> {
    let Some(PublicHistoryEntry::Effect {
        detail,
        state: PublicEffectState::Completed { output, .. },
        ..
    }) = entry.source.server()
    else {
        return None;
    };
    if detail.kind != ToolEffectKind::Todo {
        return None;
    }
    parse_output(output)
}

/// `TodoEffectOutput` validation: an object holding at most `todos`, each item
/// holding an `id` and a `content`, with a known status and priority when set.
fn parse_output(output: &Value) -> Option<Vec<TodoItem>> {
    let fields = output.as_object()?;
    if fields.keys().any(|key| key != "todos") {
        return None;
    }
    let Some(todos) = fields.get("todos") else {
        return Some(Vec::new());
    };
    todos.as_array()?.iter().map(parse_item).collect()
}

fn parse_item(item: &Value) -> Option<TodoItem> {
    let fields = item.as_object()?;
    if fields
        .keys()
        .any(|key| !matches!(key.as_str(), "id" | "content" | "status" | "priority"))
    {
        return None;
    }
    if let Some(priority) = fields.get("priority")
        && !matches!(priority.as_str(), Some("low" | "medium" | "high"))
    {
        return None;
    }
    let status = match fields.get("status") {
        None => TodoStatus::Pending,
        Some(status) => TodoStatus::parse(status.as_str()?)?,
    };
    Some(TodoItem {
        id: fields.get("id")?.as_str()?.to_owned(),
        content: fields.get("content")?.as_str()?.to_owned(),
        status,
    })
}

/// Reference `todo_deltas`: each todo result's change, keyed by entry, replayed
/// over the whole history so a result reads the same live and once restored.
pub(super) fn todo_deltas(entries: &[TranscriptEntry]) -> BTreeMap<String, String> {
    let mut tracker = TodoTracker::default();
    entries
        .iter()
        .filter_map(|entry| Some((entry.id.clone(), tracker.record(entry_todos(entry)?))))
        .collect()
}

#[cfg(test)]
#[path = "todos_tests.rs"]
mod todos_tests;
