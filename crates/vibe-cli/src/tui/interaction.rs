use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use super::attachments::PromptDraft;

const QUIT_CONFIRMATION_WINDOW_MS: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    Help,
    Config,
    Model,
    Thinking,
    Theme,
    Sessions,
    Mcp,
    Connectors,
    Voice,
    Debug,
    Status,
    DataRetention,
    Proxy,
    Rewind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayItem {
    pub id: String,
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
        Self {
            id: id.into(),
            label: label.into(),
            description: description.into(),
            disabled,
        }
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptQueue {
    prompts: VecDeque<PromptDraft>,
    paused: bool,
}

impl PromptQueue {
    pub fn push(&mut self, prompt: PromptDraft) {
        self.prompts.push_back(prompt);
    }

    pub fn push_front(&mut self, prompt: PromptDraft) {
        self.prompts.push_front(prompt);
    }

    pub fn pop_next(&mut self) -> Option<PromptDraft> {
        (!self.paused).then(|| self.prompts.pop_front()).flatten()
    }

    pub fn cancel_last(&mut self) -> Option<PromptDraft> {
        self.prompts.pop_back()
    }

    pub fn clear(&mut self) {
        self.prompts.clear();
        self.paused = false;
    }

    pub fn pause(&mut self) {
        self.paused = true;
    }

    pub fn resume(&mut self) {
        self.paused = false;
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.prompts.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.prompts.len()
    }

    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn transient_images(&self) -> HashSet<PathBuf> {
        self.prompts
            .iter()
            .flat_map(PromptDraft::transient_image_paths)
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

    #[must_use]
    pub fn pending_key(&self) -> Option<&str> {
        self.key.as_deref()
    }
}
