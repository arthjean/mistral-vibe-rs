use std::cmp::Ordering;
use std::fs;
use std::fs::OpenOptions;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;
use vibe_app_server::client::{PublicContentBlock, TurnRequest};

pub const MAX_PASTE_BYTES: usize = 256 * 1024;
pub const MAX_RESOURCE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_COMPLETION_CANDIDATES: usize = 64;
const MAX_COMPLETION_SCAN_ENTRIES: usize = 4_096;
const SLASH_COMMANDS: &[&str] = &[
    "/approve",
    "/clear",
    "/close",
    "/compact",
    "/continue",
    "/deny",
    "/exit",
    "/fork",
    "/help",
    "/history",
    "/loop",
    "/quit",
    "/remote-project",
    "/resume",
    "/rewind",
    "/setup",
    "/settings",
    "/teleport",
    "/theme",
    "/title",
    "/trust",
    "/update",
    "/voice",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptEditor {
    text: String,
    cursor: usize,
    selection: Option<Range<usize>>,
    selection_anchor: Option<usize>,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: String,
}

impl PromptEditor {
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    #[must_use]
    pub fn selection(&self) -> Option<Range<usize>> {
        self.selection.clone()
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = grapheme_count(&self.text);
        self.selection = None;
        self.selection_anchor = None;
        self.history_index = None;
    }

    pub fn select(&mut self, range: Range<usize>) {
        let end = grapheme_count(&self.text);
        let start = range.start.min(end);
        let target = range.end.min(end);
        self.selection_anchor = Some(start);
        self.selection = Some(start.min(target)..start.max(target));
        self.cursor = target;
    }

    pub fn insert(&mut self, text: &str) {
        self.delete_selection();
        let byte = grapheme_byte(&self.text, self.cursor);
        self.text.insert_str(byte, text);
        self.cursor = self.cursor.saturating_add(grapheme_count(text));
    }

    pub fn paste(&mut self, text: &str) -> Result<(), InputError> {
        if text.len() > MAX_PASTE_BYTES {
            return Err(InputError::PasteTooLarge {
                bytes: text.len(),
                limit: MAX_PASTE_BYTES,
            });
        }
        self.insert(text);
        Ok(())
    }

    pub fn move_left(&mut self, extend_selection: bool) {
        self.move_cursor(self.cursor.saturating_sub(1), extend_selection);
    }

    pub fn move_right(&mut self, extend_selection: bool) {
        self.move_cursor(
            self.cursor
                .saturating_add(1)
                .min(grapheme_count(&self.text)),
            extend_selection,
        );
    }

    pub fn move_home(&mut self, extend_selection: bool) {
        self.move_cursor(0, extend_selection);
    }

    pub fn move_end(&mut self, extend_selection: bool) {
        self.move_cursor(grapheme_count(&self.text), extend_selection);
    }

    pub fn delete_backward(&mut self) {
        if self.delete_selection() || self.cursor == 0 {
            return;
        }
        let start = grapheme_byte(&self.text, self.cursor - 1);
        let end = grapheme_byte(&self.text, self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn delete_forward(&mut self) {
        if self.delete_selection() || self.cursor == grapheme_count(&self.text) {
            return;
        }
        let start = grapheme_byte(&self.text, self.cursor);
        let end = grapheme_byte(&self.text, self.cursor + 1);
        self.text.replace_range(start..end, "");
    }

    pub fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.history_index {
            Some(index) => index.saturating_sub(1),
            None => {
                self.draft.clone_from(&self.text);
                self.history.len() - 1
            }
        };
        self.history_index = Some(index);
        self.text.clone_from(&self.history[index]);
        self.cursor = grapheme_count(&self.text);
        self.selection = None;
        self.selection_anchor = None;
    }

    pub fn history_next(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.history.len() {
            self.history_index = Some(index + 1);
            self.text.clone_from(&self.history[index + 1]);
        } else {
            self.history_index = None;
            self.text.clone_from(&self.draft);
        }
        self.cursor = grapheme_count(&self.text);
        self.selection = None;
        self.selection_anchor = None;
    }

    pub fn submit(&mut self) -> Option<String> {
        let submitted = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.selection = None;
        self.selection_anchor = None;
        self.history_index = None;
        self.draft.clear();
        if submitted.trim().is_empty() {
            return None;
        }
        if self.history.last() != Some(&submitted) {
            self.history.push(submitted.clone());
        }
        Some(submitted)
    }

    pub fn take_unrecorded(&mut self) -> Option<String> {
        let submitted = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.selection = None;
        self.selection_anchor = None;
        self.history_index = None;
        self.draft.clear();
        (!submitted.trim().is_empty()).then_some(submitted)
    }

    fn move_cursor(&mut self, target: usize, extend_selection: bool) {
        if extend_selection {
            let anchor = *self.selection_anchor.get_or_insert(self.cursor);
            self.selection = Some(anchor.min(target)..anchor.max(target));
        } else {
            self.selection = None;
            self.selection_anchor = None;
        }
        self.cursor = target;
    }

    fn delete_selection(&mut self) -> bool {
        let Some(selection) = self.selection.take() else {
            return false;
        };
        self.selection_anchor = None;
        if selection.is_empty() {
            return false;
        }
        let start = grapheme_byte(&self.text, selection.start);
        let end = grapheme_byte(&self.text, selection.end);
        self.text.replace_range(start..end, "");
        self.cursor = selection.start;
        true
    }
}

fn grapheme_count(text: &str) -> usize {
    text.graphemes(true).count()
}

fn grapheme_byte(text: &str, index: usize) -> usize {
    text.grapheme_indices(true)
        .nth(index)
        .map_or(text.len(), |(byte, _)| byte)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionKind {
    SlashCommand,
    Path,
    Agent,
    Skill,
    Mention,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionCandidate {
    pub id: String,
    pub kind: CompletionKind,
    pub label: String,
    pub insertion: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResult {
    pub generation: u64,
    pub candidates: Vec<CompletionCandidate>,
}

#[derive(Debug, Clone, Default)]
pub struct CompletionEngine {
    generation: u64,
}

impl CompletionEngine {
    pub fn complete(
        &mut self,
        query: &str,
        candidates: impl IntoIterator<Item = CompletionCandidate>,
    ) -> CompletionResult {
        self.generation = self.generation.saturating_add(1);
        let normalized = query.to_lowercase();
        let mut candidates = candidates
            .into_iter()
            .filter(|candidate| candidate.label.to_lowercase().contains(&normalized))
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            completion_rank(left, &normalized)
                .cmp(&completion_rank(right, &normalized))
                .then_with(|| left.kind.cmp(&right.kind))
                .then_with(|| left.label.to_lowercase().cmp(&right.label.to_lowercase()))
                .then_with(|| left.id.cmp(&right.id))
        });
        candidates.truncate(MAX_COMPLETION_CANDIDATES);
        CompletionResult {
            generation: self.generation,
            candidates,
        }
    }

    pub fn complete_prompt(
        &mut self,
        editor: &mut PromptEditor,
        workspace: &Path,
    ) -> Result<bool, InputError> {
        let Some((token_range, query)) = active_token(editor) else {
            self.cancel();
            return Ok(false);
        };
        let candidates = prompt_candidates(workspace, &query)?;
        let result = self.complete(&query, candidates);
        if result.candidates.is_empty() {
            return Ok(false);
        }
        editor.select(token_range);
        self.apply(editor, &result, 0)?;
        Ok(true)
    }

    pub fn apply(
        &self,
        editor: &mut PromptEditor,
        result: &CompletionResult,
        selected: usize,
    ) -> Result<(), InputError> {
        if result.generation != self.generation {
            return Err(InputError::StaleCompletion);
        }
        let candidate = result
            .candidates
            .get(selected)
            .ok_or(InputError::MissingCompletion)?;
        editor.insert(&candidate.insertion);
        Ok(())
    }

    pub fn cancel(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }
}

fn completion_rank(candidate: &CompletionCandidate, query: &str) -> u8 {
    let label = candidate.label.to_lowercase();
    match label.as_str().cmp(query) {
        Ordering::Equal => 0,
        _ if label.starts_with(query) => 1,
        _ => 2,
    }
}

fn active_token(editor: &PromptEditor) -> Option<(Range<usize>, String)> {
    let text = editor.text();
    let cursor_byte = grapheme_byte(text, editor.cursor());
    let start_byte = text[..cursor_byte]
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map_or(0, |(byte, character)| byte + character.len_utf8());
    let end_byte = text[cursor_byte..]
        .char_indices()
        .find(|(_, character)| character.is_whitespace())
        .map_or(text.len(), |(byte, _)| cursor_byte + byte);
    let query = text[start_byte..cursor_byte].to_owned();
    if query.is_empty() {
        return None;
    }
    Some((
        grapheme_count(&text[..start_byte])..grapheme_count(&text[..end_byte]),
        query,
    ))
}

fn prompt_candidates(
    workspace: &Path,
    query: &str,
) -> Result<Vec<CompletionCandidate>, InputError> {
    if query.starts_with('/') {
        return Ok(SLASH_COMMANDS
            .iter()
            .map(|command| CompletionCandidate {
                id: format!("command:{command}"),
                kind: CompletionKind::SlashCommand,
                label: (*command).to_owned(),
                insertion: (*command).to_owned(),
            })
            .collect());
    }

    let (kind, marker, raw_query) = if let Some(query) = query.strip_prefix('@') {
        (CompletionKind::Mention, "@", query)
    } else {
        (CompletionKind::Path, "", query)
    };
    let (parent_display, leaf) = raw_query
        .rsplit_once('/')
        .map_or(("", raw_query), |(parent, leaf)| {
            (&raw_query[..parent.len().saturating_add(1)], leaf)
        });
    let parent = parent_display.trim_end_matches('/');
    let Ok(relative_parent) = safe_relative_path(parent) else {
        return Ok(Vec::new());
    };
    let canonical_workspace =
        fs::canonicalize(workspace).map_err(|error| InputError::Workspace(error.to_string()))?;
    let directory = canonical_workspace.join(relative_parent);
    let Ok(canonical_directory) = fs::canonicalize(directory) else {
        return Ok(Vec::new());
    };
    if !canonical_directory.starts_with(&canonical_workspace) || !canonical_directory.is_dir() {
        return Ok(Vec::new());
    }

    let entries = fs::read_dir(canonical_directory)
        .map_err(|error| InputError::Resource(error.to_string()))?;
    let mut entries = entries
        .take(MAX_COMPLETION_SCAN_ENTRIES.saturating_add(1))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| InputError::Resource(error.to_string()))?;
    if entries.len() > MAX_COMPLETION_SCAN_ENTRIES {
        return Ok(Vec::new());
    }
    entries.sort_by_key(|entry| entry.file_name());
    let mut candidates = Vec::new();
    for entry in entries {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !name.to_lowercase().contains(&leaf.to_lowercase()) {
            continue;
        }
        let Ok(canonical) = fs::canonicalize(entry.path()) else {
            continue;
        };
        if !canonical.starts_with(&canonical_workspace) {
            continue;
        }
        let directory_suffix = if canonical.is_dir() { "/" } else { "" };
        let insertion = format!("{marker}{parent_display}{name}{directory_suffix}");
        candidates.push(CompletionCandidate {
            id: format!(
                "{}:{insertion}",
                if marker.is_empty() { "path" } else { "mention" }
            ),
            kind,
            label: insertion.clone(),
            insertion,
        });
    }
    Ok(candidates)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MentionMetric {
    pub token: String,
    pub kind: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedSubmission {
    pub turn: TurnRequest,
    pub metrics: Vec<MentionMetric>,
}

pub fn prepare_submission(
    workspace: &Path,
    prompt: &str,
) -> Result<PreparedSubmission, InputError> {
    let canonical_workspace =
        fs::canonicalize(workspace).map_err(|error| InputError::Workspace(error.to_string()))?;
    let mut input = vec![PublicContentBlock::Text {
        text: prompt.to_owned(),
    }];
    let mut metrics = Vec::new();
    for token in prompt
        .split_whitespace()
        .filter(|token| token.starts_with('@'))
    {
        let raw = token
            .trim_start_matches('@')
            .trim_matches(|character: char| ",.;:!?)]}".contains(character));
        if raw.is_empty() {
            continue;
        }
        let relative = safe_relative_path(raw)?;
        let path = canonical_workspace.join(relative);
        let canonical =
            fs::canonicalize(&path).map_err(|_| InputError::MissingMention(raw.to_owned()))?;
        if !canonical.starts_with(&canonical_workspace) {
            return Err(InputError::MentionOutsideWorkspace(raw.to_owned()));
        }
        let metadata =
            fs::metadata(&canonical).map_err(|error| InputError::Resource(error.to_string()))?;
        if !metadata.is_file() {
            return Err(InputError::InvalidMention(raw.to_owned()));
        }
        if metadata.len() > MAX_RESOURCE_BYTES {
            return Err(InputError::ResourceTooLarge {
                path: raw.to_owned(),
                bytes: metadata.len(),
                limit: MAX_RESOURCE_BYTES,
            });
        }
        let bytes =
            fs::read(&canonical).map_err(|error| InputError::Resource(error.to_string()))?;
        let extension = canonical
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let uri = format!("file://{}", canonical.to_string_lossy());
        if matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") {
            let media_type = match extension.as_str() {
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                _ => "image/png",
            };
            input.push(PublicContentBlock::Image {
                attachment: json!({
                    "uri": uri,
                    "name": raw,
                    "bytes": metadata.len(),
                    "mediaType": media_type,
                    "data": BASE64_STANDARD.encode(&bytes),
                }),
            });
            metrics.push(MentionMetric {
                token: token.to_owned(),
                kind: "image".to_owned(),
                bytes: metadata.len(),
            });
        } else {
            if bytes.contains(&0) {
                return Err(InputError::BinaryMention(raw.to_owned()));
            }
            let text = String::from_utf8(bytes)
                .map_err(|_| InputError::InvalidUnicodeMention(raw.to_owned()))?;
            input.push(PublicContentBlock::Resource {
                resource: json!({
                    "uri": uri,
                    "name": raw,
                    "text": text,
                }),
            });
            metrics.push(MentionMetric {
                token: token.to_owned(),
                kind: "file".to_owned(),
                bytes: metadata.len(),
            });
        }
    }
    let turn = TurnRequest {
        prompt: prompt.to_owned(),
        input,
        client_user_message_id: None,
        auto_title: None,
        user_display_content: Some(json!({"type": "text", "text": prompt})),
        mention_stats: Some(serde_json::to_value(&metrics).map_err(InputError::Json)?),
    };
    Ok(PreparedSubmission { turn, metrics })
}

fn safe_relative_path(raw: &str) -> Result<PathBuf, InputError> {
    let path = Path::new(raw);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(InputError::MentionOutsideWorkspace(raw.to_owned()));
    }
    Ok(path.to_path_buf())
}

pub trait ClipboardPort {
    fn read_text(&mut self) -> Result<String, String>;
    fn write_text(&mut self, value: &str) -> Result<(), String>;
}

pub trait ExternalEditorPort {
    fn edit(&mut self, initial: &str) -> Result<String, String>;
}

pub struct SystemExternalEditor {
    command: String,
}

impl SystemExternalEditor {
    #[must_use]
    pub fn from_environment() -> Self {
        Self::from_sources(std::env::var("VISUAL").ok(), std::env::var("EDITOR").ok())
    }

    fn from_sources(visual: Option<String>, editor: Option<String>) -> Self {
        Self {
            command: visual
                .filter(|value| !value.trim().is_empty())
                .or_else(|| editor.filter(|value| !value.trim().is_empty()))
                .unwrap_or_else(|| "nano".to_owned()),
        }
    }

    fn command_parts(&self) -> Result<Vec<String>, String> {
        let parts = shlex::split(&self.command)
            .filter(|parts| !parts.is_empty())
            .ok_or_else(|| "External editor command is invalid".to_owned())?;
        Ok(parts)
    }
}

impl ExternalEditorPort for SystemExternalEditor {
    fn edit(&mut self, initial: &str) -> Result<String, String> {
        let command = self.command_parts()?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "mistral-vibe-rs-editor-{}-{stamp}.md",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(|error| error.to_string())?;
        std::io::Write::write_all(&mut file, initial.as_bytes())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        let result = Command::new(&command[0])
            .args(&command[1..])
            .arg(&path)
            .status();
        let edited = match result {
            Ok(status) if status.success() => fs::read_to_string(&path)
                .map(|content| content.trim_end().to_owned())
                .map_err(|error| error.to_string()),
            Ok(status) => Err(format!("External editor exited with {status}")),
            Err(error) => Err(format!("External editor could not start: {error}")),
        };
        let _ = fs::remove_file(&path);
        edited
    }
}

#[derive(Debug, Error)]
pub enum InputError {
    #[error("paste contains {bytes} bytes; limit is {limit}")]
    PasteTooLarge { bytes: usize, limit: usize },
    #[error("completion result is stale")]
    StaleCompletion,
    #[error("selected completion does not exist")]
    MissingCompletion,
    #[error("workspace is unavailable: {0}")]
    Workspace(String),
    #[error("mentioned path `{0}` does not exist")]
    MissingMention(String),
    #[error("mentioned path `{0}` is outside the workspace")]
    MentionOutsideWorkspace(String),
    #[error("mentioned path `{0}` is not a regular file")]
    InvalidMention(String),
    #[error("mentioned file `{path}` contains {bytes} bytes; limit is {limit}")]
    ResourceTooLarge {
        path: String,
        bytes: u64,
        limit: u64,
    },
    #[error("mentioned file `{0}` is binary")]
    BinaryMention(String),
    #[error("mentioned file `{0}` is not valid UTF-8")]
    InvalidUnicodeMention(String),
    #[error("resource could not be read: {0}")]
    Resource(String),
    #[error("mention metrics could not be encoded: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_uses_unicode_graphemes_and_preserves_multiline_history() {
        let mut editor = PromptEditor::default();
        editor.insert("a");
        editor.insert("e\u{301}");
        editor.insert("\nβ");
        assert_eq!(editor.cursor(), 4);
        editor.move_left(false);
        editor.move_left(false);
        editor.delete_backward();
        assert_eq!(editor.text(), "a\nβ");
        let submitted = editor.submit().expect("non-empty prompt");
        assert_eq!(submitted, "a\nβ");
        editor.set_text("draft");
        editor.history_previous();
        assert_eq!(editor.text(), "a\nβ");
        editor.history_next();
        assert_eq!(editor.text(), "draft");
    }

    #[test]
    fn external_editor_prefers_visual_parses_arguments_and_defaults_to_nano() {
        let configured = SystemExternalEditor::from_sources(
            Some("code --wait".to_owned()),
            Some("nvim".to_owned()),
        );
        assert_eq!(
            configured.command_parts(),
            Ok(vec!["code".to_owned(), "--wait".to_owned()])
        );

        let fallback = SystemExternalEditor::from_sources(None, None);
        assert_eq!(fallback.command_parts(), Ok(vec!["nano".to_owned()]));

        let invalid = SystemExternalEditor::from_sources(Some("'".to_owned()), None);
        assert_eq!(
            invalid.command_parts(),
            Err("External editor command is invalid".to_owned())
        );
    }

    #[test]
    fn huge_paste_is_rejected_without_losing_existing_input() {
        let mut editor = PromptEditor::default();
        editor.insert("keep");
        let paste = "x".repeat(MAX_PASTE_BYTES + 1);
        assert!(matches!(
            editor.paste(&paste),
            Err(InputError::PasteTooLarge { .. })
        ));
        assert_eq!(editor.text(), "keep");
    }

    #[test]
    fn secret_submission_never_enters_prompt_history() {
        let mut editor = PromptEditor::default();
        editor.set_text("secret-api-key");
        assert_eq!(editor.take_unrecorded().as_deref(), Some("secret-api-key"));
        editor.set_text("ordinary prompt");
        assert_eq!(editor.submit().as_deref(), Some("ordinary prompt"));
        assert_eq!(editor.history, vec!["ordinary prompt"]);
    }

    #[test]
    fn completions_are_stable_and_racing_results_are_rejected() {
        let mut engine = CompletionEngine::default();
        let candidates = vec![
            CompletionCandidate {
                id: "skill".to_owned(),
                kind: CompletionKind::Skill,
                label: "review".to_owned(),
                insertion: "/review".to_owned(),
            },
            CompletionCandidate {
                id: "command".to_owned(),
                kind: CompletionKind::SlashCommand,
                label: "reload".to_owned(),
                insertion: "/reload".to_owned(),
            },
        ];
        let stale = engine.complete("re", candidates.clone());
        let current = engine.complete("rev", candidates);
        let mut editor = PromptEditor::default();
        assert!(matches!(
            engine.apply(&mut editor, &stale, 0),
            Err(InputError::StaleCompletion)
        ));
        engine
            .apply(&mut editor, &current, 0)
            .expect("current completion applies");
        assert_eq!(editor.text(), "/review");
    }

    #[test]
    fn runtime_completion_applies_slash_path_and_mention_candidates() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        fs::create_dir(temporary.path().join("src")).expect("path fixture");
        fs::write(temporary.path().join("notes.txt"), "context").expect("mention fixture");
        let mut engine = CompletionEngine::default();
        let mut editor = PromptEditor::default();

        editor.set_text("/clo");
        assert!(
            engine
                .complete_prompt(&mut editor, temporary.path())
                .expect("slash completion")
        );
        assert_eq!(editor.text(), "/close");

        editor.set_text("inspect sr");
        assert!(
            engine
                .complete_prompt(&mut editor, temporary.path())
                .expect("path completion")
        );
        assert_eq!(editor.text(), "inspect src/");

        editor.set_text("inspect @no");
        assert!(
            engine
                .complete_prompt(&mut editor, temporary.path())
                .expect("mention completion")
        );
        assert_eq!(editor.text(), "inspect @notes.txt");
    }

    #[test]
    fn completion_and_selection_state_stay_bounded_and_stable() {
        let mut engine = CompletionEngine::default();
        let result = engine.complete(
            "",
            (0..MAX_COMPLETION_CANDIDATES.saturating_mul(2)).map(|index| CompletionCandidate {
                id: format!("candidate-{index}"),
                kind: CompletionKind::Path,
                label: format!("candidate-{index}"),
                insertion: format!("candidate-{index}"),
            }),
        );
        assert_eq!(result.candidates.len(), MAX_COMPLETION_CANDIDATES);
        engine.cancel();
        assert!(matches!(
            engine.apply(&mut PromptEditor::default(), &result, 0),
            Err(InputError::StaleCompletion)
        ));

        let mut editor = PromptEditor::default();
        editor.set_text("abcd");
        editor.move_left(false);
        editor.move_left(true);
        assert_eq!(editor.selection(), Some(2..3));
        editor.move_right(true);
        assert_eq!(editor.selection(), Some(3..3));
        editor.move_right(true);
        assert_eq!(editor.selection(), Some(3..4));
    }

    #[test]
    fn mentions_keep_display_text_and_model_resources_separate() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        fs::write(temporary.path().join("notes.txt"), "safe context").expect("text fixture");
        fs::write(temporary.path().join("image.png"), b"image").expect("image fixture");
        let prepared = prepare_submission(temporary.path(), "inspect @notes.txt and @image.png")
            .expect("mentions prepare");
        assert_eq!(prepared.turn.prompt, "inspect @notes.txt and @image.png");
        assert_eq!(prepared.turn.input.len(), 3);
        assert_eq!(prepared.metrics.len(), 2);
        let PublicContentBlock::Image { attachment } = &prepared.turn.input[2] else {
            return;
        };
        assert_eq!(attachment["mediaType"], "image/png");
        assert_eq!(
            BASE64_STANDARD
                .decode(attachment["data"].as_str().unwrap_or_default())
                .expect("canonical image data"),
            b"image"
        );
        assert_eq!(
            prepared
                .turn
                .user_display_content
                .as_ref()
                .and_then(|value| value.get("text"))
                .and_then(serde_json::Value::as_str),
            Some("inspect @notes.txt and @image.png")
        );
    }

    #[test]
    fn binary_and_external_mentions_fail_without_partial_submission() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        fs::write(temporary.path().join("binary"), b"a\0b").expect("binary fixture");
        assert!(matches!(
            prepare_submission(temporary.path(), "@binary"),
            Err(InputError::BinaryMention(_))
        ));
        assert!(matches!(
            prepare_submission(temporary.path(), "@../outside"),
            Err(InputError::MentionOutsideWorkspace(_))
        ));
    }
}
