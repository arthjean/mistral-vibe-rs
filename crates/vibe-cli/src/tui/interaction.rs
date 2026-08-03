use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use serde_json::Value;

use super::attachments::{PreparedSubmission, PromptDraft};

/// Reference `QUIT_CONFIRM_DELAY`.
pub const QUIT_CONFIRMATION_WINDOW_MS: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    Help,
    Config,
    ConfigChoice,
    ConfigTarget,
    Model,
    Thinking,
    Theme,
    Sessions,
    Mcp,
    McpDetail,
    McpAuth,
    Connectors,
    Voice,
    Debug,
    Status,
    DataRetention,
    Proxy,
    RemoteProjects,
    RemoteProjectCreate,
    TeleportApproval,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationKind {
    McpServer,
    Connector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigLayerTarget {
    User,
    Project,
}

impl ConfigLayerTarget {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }

    #[must_use]
    pub fn from_snapshot(value: &Value) -> Self {
        if value.get("selectedTarget").and_then(Value::as_str) == Some("project") {
            Self::Project
        } else {
            Self::User
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationTarget {
    pub kind: IntegrationKind,
    pub source: String,
    pub tool: Option<String>,
    pub enabled: bool,
    pub requires_auth: bool,
    pub requires_setup: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthActionKind {
    Open,
    Copy,
    Show,
    Refresh,
    Logout,
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthAction {
    pub kind: IntegrationKind,
    pub source: String,
    pub url: String,
    pub action: AuthActionKind,
    pub enable_on_complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteProjectAction {
    Select {
        project_id: String,
    },
    Create {
        name: String,
        default_branch: String,
    },
    More,
    Unlink,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteProjectDraft {
    pub name: String,
    pub default_branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeleportPushAction {
    pub operation_id: String,
    pub approved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayAction {
    Select(String),
    ConfigChoice { path: String, value: Value },
    ConfigSpecial { path: String, value: String },
    ConfigTarget(ConfigLayerTarget),
    Integration(IntegrationTarget),
    Authenticate(AuthAction),
    RemoteProject(RemoteProjectAction),
    TeleportPush(TeleportPushAction),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayItem {
    pub id: String,
    pub action: OverlayAction,
    pub label: String,
    pub description: String,
    pub disabled: bool,
}

impl OverlayItem {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        description: impl Into<String>,
        disabled: bool,
    ) -> Self {
        let id = id.into();
        Self {
            action: OverlayAction::Select(id.clone()),
            id,
            label: label.into(),
            description: description.into(),
            disabled,
        }
    }

    #[must_use]
    pub fn with_action(mut self, action: OverlayAction) -> Self {
        self.action = action;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay {
    pub kind: OverlayKind,
    pub title: String,
    pub items: Vec<OverlayItem>,
    pub query: String,
    pub notice: Option<String>,
    selected: Option<usize>,
}

impl Overlay {
    #[must_use]
    pub fn new(kind: OverlayKind, title: impl Into<String>, items: Vec<OverlayItem>) -> Self {
        let selected = items.iter().position(|item| !item.disabled);
        Self {
            kind,
            title: title.into(),
            items,
            query: String::new(),
            notice: None,
            selected,
        }
    }

    pub fn set_query(&mut self, query: impl Into<String>) {
        let selected_id = self.selected_item().map(|item| item.id.clone());
        self.query = query.into();
        let visible = self.visible_indexes();
        self.selected = selected_id
            .as_ref()
            .and_then(|id| {
                visible
                    .iter()
                    .copied()
                    .find(|index| self.items[*index].id == *id && !self.items[*index].disabled)
            })
            .or_else(|| {
                visible
                    .into_iter()
                    .find(|index| !self.items[*index].disabled)
            });
    }

    pub fn push_query(&mut self, character: char) {
        let mut query = self.query.clone();
        query.push(character);
        self.set_query(query);
    }

    pub fn pop_query(&mut self) {
        let mut query = self.query.clone();
        query.pop();
        self.set_query(query);
    }

    pub fn move_selection(&mut self, delta: isize) {
        let visible = self
            .visible_indexes()
            .into_iter()
            .filter(|index| !self.items[*index].disabled)
            .collect::<Vec<_>>();
        if visible.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .and_then(|selected| visible.iter().position(|index| *index == selected))
            .unwrap_or(0);
        let last = visible.len().saturating_sub(1);
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta.unsigned_abs()).min(last)
        };
        self.selected = visible.get(next).copied();
    }

    pub fn select_by_id(&mut self, id: &str) {
        if let Some(selected) = self
            .visible_indexes()
            .into_iter()
            .find(|index| self.items[*index].id == id && !self.items[*index].disabled)
        {
            self.selected = Some(selected);
        }
    }

    #[must_use]
    pub fn selected_item(&self) -> Option<&OverlayItem> {
        self.selected
            .and_then(|selected| self.items.get(selected))
            .filter(|item| !item.disabled && self.matches_query(item))
    }

    pub fn selected_item_mut(&mut self) -> Option<&mut OverlayItem> {
        let selected = self.selected?;
        let matches = self
            .items
            .get(selected)
            .is_some_and(|item| !item.disabled && self.matches_query(item));
        matches.then(|| &mut self.items[selected])
    }

    #[must_use]
    pub fn visible_items(&self) -> Vec<(bool, &OverlayItem)> {
        self.visible_indexes()
            .into_iter()
            .map(|index| (self.selected == Some(index), &self.items[index]))
            .collect()
    }

    fn visible_indexes(&self) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| self.matches_query(item).then_some(index))
            .collect()
    }

    fn matches_query(&self, item: &OverlayItem) -> bool {
        let query = self.query.trim().to_lowercase();
        query.is_empty()
            || item.label.to_lowercase().contains(&query)
            || item.description.to_lowercase().contains(&query)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuedIntentKind {
    Prompt,
    Shell,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedIntent {
    pub id: String,
    pub kind: QueuedIntentKind,
    pub draft: PromptDraft,
    pub prepared: Option<PreparedSubmission>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptQueue {
    items: VecDeque<QueuedIntent>,
    paused: bool,
    next_id: u64,
    scroll_offset: usize,
}

impl Default for PromptQueue {
    fn default() -> Self {
        Self {
            items: VecDeque::new(),
            paused: false,
            next_id: 1,
            scroll_offset: 0,
        }
    }
}

impl PromptQueue {
    pub fn push(&mut self, prompt: PromptDraft) {
        self.push_item(prompt, None);
    }

    pub fn push_prepared(&mut self, prompt: PromptDraft, prepared: PreparedSubmission) {
        self.push_item(prompt, Some(prepared));
    }

    fn push_item(&mut self, prompt: PromptDraft, prepared: Option<PreparedSubmission>) {
        if self.items.is_empty() {
            self.scroll_offset = 0;
        }
        let kind = if prompt.text().trim_start().starts_with('!') {
            QueuedIntentKind::Shell
        } else {
            QueuedIntentKind::Prompt
        };
        let id = format!("queued-{}", self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        self.items.push_back(QueuedIntent {
            id,
            kind,
            draft: prompt,
            prepared,
        });
    }

    pub fn take_next_batch(&mut self) -> Option<Vec<QueuedIntent>> {
        if self.paused {
            return None;
        }
        let first = self.items.pop_front()?;
        let kind = first.kind;
        let mut items = vec![first];
        if kind == QueuedIntentKind::Prompt {
            while self
                .items
                .front()
                .is_some_and(|intent| intent.kind == QueuedIntentKind::Prompt)
            {
                if let Some(intent) = self.items.pop_front() {
                    items.push(intent);
                }
            }
        }
        self.clamp_scroll();
        Some(items)
    }

    pub fn restore_batch_and_pause(&mut self, batch: Vec<QueuedIntent>) {
        for item in batch.into_iter().rev() {
            self.items.push_front(item);
        }
        self.paused = true;
        self.scroll_offset = 0;
    }

    pub fn cancel_last(&mut self) -> Option<PromptDraft> {
        let cancelled = self.items.pop_back().map(|intent| intent.draft);
        if self.items.is_empty() {
            self.paused = false;
        }
        self.clamp_scroll();
        cancelled
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.paused = false;
        self.scroll_offset = 0;
    }

    pub fn pause(&mut self) {
        self.paused = true;
    }

    pub fn resume(&mut self) {
        self.paused = false;
    }

    pub fn scroll(&mut self, delta: isize) {
        self.scroll_offset = self.scroll_offset.saturating_add_signed(delta);
        self.clamp_scroll();
    }

    #[must_use]
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    fn clamp_scroll(&mut self) {
        self.scroll_offset = self.scroll_offset.min(self.items.len().saturating_sub(1));
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    #[must_use]
    pub fn next_kind(&self) -> Option<QueuedIntentKind> {
        self.items.front().map(|item| item.kind)
    }

    #[must_use]
    pub fn presentation_lines(&self) -> Vec<String> {
        if self.items.is_empty() {
            return Vec::new();
        }
        let mut lines = vec![if self.paused {
            "Queued messages (paused)".to_owned()
        } else {
            "Queued messages".to_owned()
        }];
        lines.extend(self.items.iter().map(|intent| match intent.kind {
            QueuedIntentKind::Prompt => format!("› {}", intent.draft.text()),
            QueuedIntentKind::Shell => format!(
                    "$ {}",
                    intent
                        .draft
                        .text()
                        .trim_start()
                        .strip_prefix('!')
                        .unwrap_or(intent.draft.text())
                        .trim_start()
                ),
        }));
        lines
    }

    pub fn transient_images(&self) -> HashSet<PathBuf> {
        self.items
            .iter()
            .flat_map(|intent| intent.draft.transient_image_paths())
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuitConfirmation {
    key: Option<String>,
    requested_at_ms: Option<u64>,
}

impl QuitConfirmation {
    pub fn request(&mut self, key: &str, now_ms: u64) -> bool {
        let confirmed = self.key.as_deref() == Some(key)
            && self.requested_at_ms.is_some_and(|requested| {
                now_ms.saturating_sub(requested) < QUIT_CONFIRMATION_WINDOW_MS
            });
        if confirmed {
            self.cancel();
            return true;
        }
        self.key = Some(key.to_owned());
        self.requested_at_ms = Some(now_ms);
        false
    }

    pub fn cancel(&mut self) {
        self.key = None;
        self.requested_at_ms = None;
    }

    /// Reference `QuitManager` clears its prompt once the window closes.
    pub fn expire(&mut self, now_ms: u64) {
        if self.requested_at_ms.is_some_and(|requested| {
            now_ms.saturating_sub(requested) >= QUIT_CONFIRMATION_WINDOW_MS
        }) {
            self.cancel();
        }
    }

    #[must_use]
    pub fn pending_key(&self) -> Option<&str> {
        self.key.as_deref()
    }
}
