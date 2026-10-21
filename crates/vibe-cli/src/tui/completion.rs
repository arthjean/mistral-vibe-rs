use std::collections::BTreeMap;
#[cfg(test)]
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};

use serde::{Deserialize, Serialize};

use super::commands::{CommandContext, command_aliases_in, command_description};
use super::input::{InputError, PromptEditor, grapheme_byte, grapheme_count};
use super::job_slot::LatestJobSlot;

mod fuzzy;
mod inline_skill;
mod path;

pub(super) use fuzzy::fuzzy_match_score;
use inline_skill::InlineSkillGhost;
pub(crate) use path::PathIndex;

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
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResult {
    pub generation: u64,
    pub candidates: Vec<CompletionCandidate>,
}

#[derive(Debug, Default)]
pub struct CompletionEngine {
    generation: u64,
    active: Option<ActiveCompletion>,
    worker: Option<CompletionWorker>,
    user_skills: Vec<CompletionCandidate>,
    command_context: CommandContext,
    /// One index per process, shared with the worker so a workspace root is
    /// walked once however a mention query is answered.
    path_index: PathIndex,
    /// The list and highlight a re-query replaced, so an answer with the same
    /// list keeps the highlight.
    carried: Option<(Vec<CompletionCandidate>, usize)>,
    /// The generation of a mention query handed to the worker and not yet
    /// answered: reference `PathCompletionController._pending_future`.
    pending: Option<u64>,
    /// The mid-prompt skill ghost on display, reference
    /// `InlineSkillCompletionController`.
    inline: Option<InlineSkillGhost>,
}

#[derive(Debug, Clone)]
struct ActiveCompletion {
    token_range: Range<usize>,
    result: CompletionResult,
    selected: usize,
}

#[derive(Debug)]
struct CompletionWorker {
    slot: Arc<LatestJobSlot<CompletionJob>>,
    results: Receiver<CompletionResolution>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionRequest {
    pub generation: u64,
    pub range: [usize; 2],
    pub query: String,
}

impl CompletionRequest {
    #[must_use]
    pub fn new(generation: u64, token_range: Range<usize>, query: String) -> Self {
        Self {
            generation,
            range: [token_range.start, token_range.end],
            query,
        }
    }

    fn token_range(&self) -> Range<usize> {
        self.range[0]..self.range[1]
    }
}

#[derive(Debug)]
struct CompletionJob {
    request: CompletionRequest,
    workspace: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CompletionResolution {
    Results {
        request: CompletionRequest,
        candidates: Vec<CompletionCandidate>,
    },
    Failed {
        request: CompletionRequest,
        reason: String,
    },
}

impl CompletionResolution {
    #[must_use]
    pub fn failed(request: CompletionRequest, error: &InputError) -> Self {
        Self::Failed {
            request,
            reason: error.to_string(),
        }
    }

    fn request(&self) -> &CompletionRequest {
        match self {
            Self::Results { request, .. } | Self::Failed { request, .. } => request,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionApplyOutcome {
    Applied,
    Failed,
    Stale,
}

impl CompletionWorker {
    fn spawn(index: PathIndex) -> Result<Self, InputError> {
        let slot = Arc::new(LatestJobSlot::new());
        let thread_slot = Arc::clone(&slot);
        let (results_tx, results) = mpsc::channel();
        std::thread::Builder::new()
            .name("vibe-path-completion".to_owned())
            .spawn(move || completion_worker_loop(&thread_slot, &results_tx, &index))
            .map_err(|error| InputError::CompletionWorker(error.to_string()))?;
        Ok(Self { slot, results })
    }

    fn submit(&self, request: CompletionJob) {
        self.slot.submit(request);
    }

    fn cancel_pending(&self) {
        self.slot.cancel();
    }
}

impl Drop for CompletionWorker {
    fn drop(&mut self) {
        self.slot.shutdown();
    }
}

fn completion_worker_loop(
    slot: &LatestJobSlot<CompletionJob>,
    results: &mpsc::Sender<CompletionResolution>,
    index: &PathIndex,
) {
    while let Some(job) = slot.wait() {
        let request = job.request;
        let resolution = match request.query.strip_prefix('@') {
            Some(query) => match index.completer_candidates(&job.workspace, query) {
                Ok(candidates) => CompletionResolution::Results {
                    request,
                    candidates,
                },
                Err(error) => CompletionResolution::failed(request, &error),
            },
            None => CompletionResolution::Results {
                request,
                candidates: Vec::new(),
            },
        };
        if results.send(resolution).is_err() {
            return;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionAction {
    Applied,
    Submit,
}

/// The keys an open completion popup claims before the editor sees them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKey {
    Escape,
    Up,
    Down,
    Tab,
    Enter,
}

/// What the caller must do after routing a key through the popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKeyOutcome {
    /// The popup handled the key; the editor must not see it.
    Consumed,
    /// A slash command was accepted and the caller must submit the prompt.
    Submit,
    /// A path was accepted and completion must be recomputed from the result.
    Refresh,
    /// No popup was open, or it does not claim this key.
    Ignored,
}

#[derive(Debug, Clone, Copy)]
pub struct CompletionView<'a> {
    pub candidates: &'a [CompletionCandidate],
    pub selected: usize,
}

impl CompletionEngine {
    #[must_use]
    pub fn command_context(&self) -> &CommandContext {
        &self.command_context
    }

    pub fn set_command_context(&mut self, context: CommandContext) {
        if self.command_context != context {
            self.command_context = context;
            self.cancel();
        }
    }

    /// Recomputes the mid-prompt skill ghost for the prompt as it now stands.
    /// Only reached when neither the slash popup nor a mention owns the
    /// caret, which is the reference manager's controller order.
    pub(crate) fn update_inline_skill(&mut self, editor: &PromptEditor, default_mode: bool) {
        // A skill token carries a slash, which the editor already counts.
        self.inline = if editor.has_path_syntax() && !self.user_skills.is_empty() {
            inline_skill::ghost_for(
                editor.text(),
                editor.cursor_byte(),
                default_mode,
                self.user_skills.iter().map(|skill| skill.label.as_str()),
            )
        } else {
            None
        };
    }

    /// The text drawn after the caret, when a skill ghost is on display.
    #[must_use]
    pub fn inline_skill_suffix(&self) -> Option<String> {
        self.inline.as_ref().map(InlineSkillGhost::suffix)
    }

    /// Reference `_accept`, reached by Tab and by the right arrow: writes the
    /// alias over its token when the ghost still applies, reporting whether it
    /// did. A ghost that no longer applies is dropped either way.
    pub(crate) fn accept_inline_skill(
        &mut self,
        editor: &mut PromptEditor,
        default_mode: bool,
    ) -> bool {
        let Some(ghost) = self.inline.take() else {
            return false;
        };
        let text = editor.text();
        if !inline_skill::still_applies(&ghost, text, editor.cursor_byte(), default_mode) {
            return false;
        }
        let start = grapheme_count(&text[..ghost.start]);
        let end = grapheme_count(&text[..ghost.end]);
        let separator = insertion_separator(&ghost.alias, &text[ghost.end..]);
        editor.select(start..end);
        editor.insert(&ghost.alias);
        if !separator.is_empty() {
            editor.insert(separator);
        }
        true
    }

    pub fn set_user_skills<'a>(&mut self, skills: impl IntoIterator<Item = (&'a str, &'a str)>) {
        self.user_skills = skills
            .into_iter()
            .map(|(name, description)| {
                let name = name.trim_start_matches('/');
                let command = format!("/{name}");
                CompletionCandidate {
                    id: format!("skill:{name}"),
                    kind: CompletionKind::Skill,
                    label: command.clone(),
                    insertion: command,
                    description: description.to_owned(),
                }
            })
            .collect();
        self.cancel();
    }

    pub fn complete(
        &mut self,
        query: &str,
        candidates: impl IntoIterator<Item = CompletionCandidate>,
    ) -> CompletionResult {
        self.generation = self.generation.saturating_add(1);
        self.active = None;
        ranked_completion(self.generation, query, candidates)
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
        let candidates = self.prompt_candidates(workspace, &query)?;
        let result = self.complete(&query, candidates);
        if result.candidates.is_empty() {
            return Ok(false);
        }
        editor.select(token_range);
        self.apply(editor, &result, 0)?;
        Ok(true)
    }

    pub fn refresh(&mut self, editor: &PromptEditor, workspace: &Path) -> Result<(), InputError> {
        let Some((token_range, query)) = active_token(editor) else {
            self.cancel();
            return Ok(());
        };
        self.requery();
        let request = CompletionRequest::new(self.generation, token_range, query);
        if let Some(resolution) = self.dispatch_request(request, workspace)? {
            let _ = self.apply_resolution(editor, resolution);
        }
        Ok(())
    }

    /// Dispatches lookup work requested by the deterministic input reducer.
    ///
    /// Slash candidates resolve immediately. Filesystem path lookup stays on
    /// the worker so the terminal event path never blocks on I/O.
    pub(crate) fn dispatch_request(
        &mut self,
        request: CompletionRequest,
        workspace: &Path,
    ) -> Result<Option<CompletionResolution>, InputError> {
        if request.generation != self.generation {
            return Err(InputError::StaleCompletion);
        }
        if request.query.starts_with('/') {
            return Ok(Some(self.resolve_request(request, workspace)));
        }
        let worker = match self.worker.as_ref() {
            Some(worker) => worker,
            None => {
                self.worker = Some(CompletionWorker::spawn(self.path_index.clone())?);
                self.worker.as_ref().ok_or_else(|| {
                    InputError::CompletionWorker("worker is unavailable".to_owned())
                })?
            }
        };
        let generation = request.generation;
        worker.submit(CompletionJob {
            request,
            workspace: workspace.to_path_buf(),
        });
        self.pending = Some(generation);
        Ok(None)
    }

    /// Deterministic adapter used by the parity replay and synchronous tests.
    #[must_use]
    pub fn resolve_request(
        &self,
        request: CompletionRequest,
        workspace: &Path,
    ) -> CompletionResolution {
        match self.prompt_candidates(workspace, &request.query) {
            Ok(candidates) => CompletionResolution::Results {
                request,
                candidates,
            },
            Err(error) => CompletionResolution::failed(request, &error),
        }
    }

    fn prompt_candidates(
        &self,
        workspace: &Path,
        query: &str,
    ) -> Result<Vec<CompletionCandidate>, InputError> {
        let mut candidates =
            prompt_candidates_in(&self.path_index, workspace, query, &self.command_context)?;
        if query.starts_with('/') {
            candidates.extend(self.user_skills.iter().cloned());
        }
        Ok(candidates)
    }

    /// Publishes `file_watcher_for_autocomplete` to the shared index.
    pub fn set_file_watcher_enabled(&self, enabled: bool) {
        self.path_index.set_watch_enabled(enabled);
    }

    /// Takes the index's pending diagnostic, raised at most once per session.
    pub fn take_index_notice(&self) -> Option<String> {
        self.path_index.take_notice()
    }

    pub fn poll(&self) -> Vec<CompletionResolution> {
        let mut resolutions = Vec::new();
        if let Some(worker) = &self.worker {
            while let Ok(resolution) = worker.results.try_recv() {
                resolutions.push(resolution);
            }
        }
        resolutions
    }

    pub fn apply_resolution(
        &mut self,
        editor: &PromptEditor,
        resolution: CompletionResolution,
    ) -> CompletionApplyOutcome {
        let request = resolution.request();
        if self.pending == Some(request.generation) {
            self.pending = None;
        }
        if request.generation != self.generation
            || !active_token(editor).is_some_and(|(range, query)| {
                range == request.token_range() && query == request.query
            })
        {
            return CompletionApplyOutcome::Stale;
        }
        match resolution {
            CompletionResolution::Results {
                request,
                candidates,
            } => {
                self.install(
                    request.generation,
                    request.token_range(),
                    &request.query,
                    candidates,
                );
                CompletionApplyOutcome::Applied
            }
            CompletionResolution::Failed { .. } => {
                self.cancel();
                CompletionApplyOutcome::Failed
            }
        }
    }

    /// Generation of the most recent completion request.
    ///
    /// Callers that resolve candidates outside the engine tag their request
    /// with it and hand it back, so a late answer can be recognized as stale.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Ranks candidates and presents them without touching the filesystem.
    ///
    /// Slash candidates arrive raw. Slash ranking and the empty-result rule are
    /// part of the observable contract and stay in this layer whether lookup
    /// ran on the worker thread or in a replay adapter. A mention answer keeps
    /// every candidate the completer returned, as the reference has since it
    /// dropped `MAX_SUGGESTIONS_COUNT` at v2.25.3; the popup scrolls.
    pub fn install(
        &mut self,
        generation: u64,
        token_range: Range<usize>,
        query: &str,
        candidates: Vec<CompletionCandidate>,
    ) {
        self.active = None;
        let carried = self.carried.take();
        let result = ranked_completion(generation, query, candidates);
        if !result.candidates.is_empty() {
            let selected = carried
                .filter(|(previous, _)| *previous == result.candidates)
                .map_or(0, |(_, selected)| selected);
            self.active = Some(ActiveCompletion {
                token_range,
                result,
                selected,
            });
        }
    }

    #[must_use]
    pub fn view(&self) -> Option<CompletionView<'_>> {
        self.active.as_ref().map(|active| CompletionView {
            candidates: &active.result.candidates,
            selected: active.selected,
        })
    }

    pub fn move_selection(&mut self, delta: isize) -> bool {
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        let count = active.result.candidates.len();
        if count == 0 {
            return false;
        }
        let distance = delta.unsigned_abs() % count;
        active.selected = if delta.is_negative() {
            active
                .selected
                .saturating_add(count)
                .saturating_sub(distance)
                % count
        } else {
            active.selected.saturating_add(distance) % count
        };
        true
    }

    /// Routes a key through an open popup, mirroring the reference precedence.
    ///
    /// The interactive loop and the deterministic replay boundary both enter
    /// here, so selection, acceptance and dismissal cannot drift apart.
    pub fn handle_key(
        &mut self,
        key: CompletionKey,
        editor: &mut PromptEditor,
    ) -> Result<CompletionKeyOutcome, InputError> {
        // Reference `PathCompletionController.on_key`: while a mention query is
        // in flight, Tab and Enter belong to it, so neither accepts a stale row
        // nor submits a prompt whose completion is still being computed.
        if self.pending.is_some() && matches!(key, CompletionKey::Tab | CompletionKey::Enter) {
            return Ok(CompletionKeyOutcome::Consumed);
        }
        if self.active.is_none() {
            return Ok(CompletionKeyOutcome::Ignored);
        }
        match key {
            // Textual does not route Escape through either completion
            // controller. Preserve the popup and prompt so the application
            // can handle its own Escape binding.
            CompletionKey::Escape => Ok(CompletionKeyOutcome::Consumed),
            CompletionKey::Up => {
                self.move_selection(-1);
                Ok(CompletionKeyOutcome::Consumed)
            }
            CompletionKey::Down => {
                self.move_selection(1);
                Ok(CompletionKeyOutcome::Consumed)
            }
            CompletionKey::Tab => match self.accept(editor)? {
                CompletionAction::Applied => Ok(CompletionKeyOutcome::Refresh),
                CompletionAction::Submit => Ok(CompletionKeyOutcome::Consumed),
            },
            CompletionKey::Enter => match self.accept(editor)? {
                CompletionAction::Applied => Ok(CompletionKeyOutcome::Refresh),
                CompletionAction::Submit => Ok(CompletionKeyOutcome::Submit),
            },
        }
    }

    pub fn accept(&mut self, editor: &mut PromptEditor) -> Result<CompletionAction, InputError> {
        let active = self.active.take().ok_or(InputError::MissingCompletion)?;
        if active.result.generation != self.generation {
            return Err(InputError::StaleCompletion);
        }
        let candidate = active
            .result
            .candidates
            .get(active.selected)
            .ok_or(InputError::MissingCompletion)?
            .clone();
        let separator = insertion_separator(
            &candidate.insertion,
            &editor.text()[grapheme_byte(editor.text(), active.token_range.end)..],
        );
        editor.select(active.token_range);
        editor.insert(&candidate.insertion);
        if !separator.is_empty() {
            editor.insert(separator);
        }
        self.generation = self.generation.saturating_add(1);
        Ok(
            if matches!(
                candidate.kind,
                CompletionKind::SlashCommand | CompletionKind::Skill
            ) {
                CompletionAction::Submit
            } else {
                CompletionAction::Applied
            },
        )
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
        self.carried = None;
        self.inline = None;
        self.retire();
    }

    /// Supersedes the open list with a query about to be answered. Reference
    /// `SlashCommandController.on_text_changed` and its path counterpart keep
    /// the highlighted row when the new answer is the same list, which a caret
    /// move usually produces.
    pub fn requery(&mut self) {
        self.inline = None;
        self.carried = self
            .active
            .take()
            .map(|active| (active.result.candidates, active.selected));
        self.retire();
    }

    fn retire(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.active = None;
        self.pending = None;
        if let Some(worker) = &self.worker {
            worker.cancel_pending();
        }
    }

    #[cfg(test)]
    fn wait_for_pending(&mut self, editor: &PromptEditor) -> Result<(), InputError> {
        let resolution = self
            .worker
            .as_ref()
            .ok_or_else(|| InputError::CompletionWorker("worker is unavailable".to_owned()))?
            .results
            .recv_timeout(std::time::Duration::from_secs(2))
            .map_err(|error| InputError::CompletionWorker(error.to_string()))?;
        if let CompletionResolution::Failed { reason, .. } = &resolution {
            return Err(InputError::CompletionWorker(reason.clone()));
        }
        if self.apply_resolution(editor, resolution) == CompletionApplyOutcome::Stale {
            return Err(InputError::StaleCompletion);
        }
        Ok(())
    }
}

/// The spacing the reference appends to an accepted candidate, from
/// `ChatInputContainer._format_insertion`: a directory mention closes on its own
/// slash, any other mention takes a space unless the tail already opens with
/// whitespace, and every other candidate takes one only when a tail follows it.
fn insertion_separator(insertion: &str, suffix: &str) -> &'static str {
    let tail_opens_with_space = suffix.starts_with(char::is_whitespace);
    if insertion.starts_with('@') {
        if insertion.ends_with('/') {
            return "";
        }
        return if tail_opens_with_space { "" } else { " " };
    }
    if suffix.is_empty() || tail_opens_with_space {
        ""
    } else {
        " "
    }
}

fn ranked_completion(
    generation: u64,
    query: &str,
    candidates: impl IntoIterator<Item = CompletionCandidate>,
) -> CompletionResult {
    let candidates = if query.starts_with('/') {
        ranked_slash_candidates(query, candidates)
    } else {
        deduplicate_candidates(candidates)
    };
    CompletionResult {
        generation,
        candidates,
    }
}

fn ranked_slash_candidates(
    query: &str,
    candidates: impl IntoIterator<Item = CompletionCandidate>,
) -> Vec<CompletionCandidate> {
    // `_fuzzy_filter` reads `search_str[1:]`: exactly one marker is removed, so
    // `//` searches for `/`, which is a subsequence of no alias.
    let query = query.strip_prefix('/').unwrap_or(query).to_lowercase();
    let mut candidates = candidates.into_iter().collect::<Vec<_>>();
    // Python sorts the `(alias, description)` entries before building its
    // lookup. A duplicate keeps its first position but its last description.
    candidates.sort_by(|left, right| {
        left.label
            .cmp(&right.label)
            .then_with(|| left.description.cmp(&right.description))
    });
    let mut indexes = BTreeMap::<String, usize>::new();
    let mut unique = Vec::new();
    for candidate in candidates {
        if let Some(index) = indexes.get(&candidate.label).copied() {
            unique[index] = candidate;
        } else {
            indexes.insert(candidate.label.clone(), unique.len());
            unique.push(candidate);
        }
    }
    unique.retain(|candidate| slash_completion_score(candidate, &query).is_some());
    unique.sort_by(|left, right| {
        slash_completion_score(right, &query).cmp(&slash_completion_score(left, &query))
    });
    unique
}

fn deduplicate_candidates(
    candidates: impl IntoIterator<Item = CompletionCandidate>,
) -> Vec<CompletionCandidate> {
    let mut seen = BTreeMap::<String, ()>::new();
    candidates
        .into_iter()
        .filter(|candidate| seen.insert(candidate.label.clone(), ()).is_none())
        .collect()
}

fn slash_completion_score(candidate: &CompletionCandidate, query: &str) -> Option<i64> {
    let label = candidate.label.trim_start_matches('/');
    let boost = match candidate.label.as_str() {
        "/help" => 200,
        "/config" => 100,
        _ => 0,
    };
    fuzzy_match_score(query, label).map(|score| score.saturating_add(boost))
}

/// The slash head word under `cursor_byte` and the range accepting a candidate
/// replaces, or [`None`] when the line is not a command line or the caret sits
/// outside it.
///
/// Split out of [`active_token`] because the reference guards the caret in
/// `SlashCommandController.on_text_changed` rather than in the completer, and
/// this port's editor cannot produce an offset past its own text: a guard no
/// caller can reach is a guard no test can prove.
fn slash_token(text: &str, cursor_byte: usize) -> Option<(Range<usize>, String)> {
    if !text.starts_with('/') || cursor_byte > text.len() || !text.is_char_boundary(cursor_byte) {
        return None;
    }
    let end_byte = text.find(' ').unwrap_or(text.len());
    let query_end = cursor_byte.min(end_byte);
    // `CommandCompleter._head_word` reads `head[1:...]`, so the marker is
    // dropped once and re-attached once by `get_completions`. A caret before it
    // yields the empty head word the reference answers with the full list, and
    // a second slash stays inside the query, where it matches no alias.
    let head_word = &text[1..query_end.max(1)];
    // `CommandCompleter.get_replacement_range` replaces the whole command
    // word, up to the first whitespace of any kind, wherever the caret sits.
    let word_end = text.find(char::is_whitespace).unwrap_or(text.len());
    Some((
        0..grapheme_count(&text[..word_end]),
        format!("/{head_word}"),
    ))
}

pub(crate) fn active_token(editor: &PromptEditor) -> Option<(Range<usize>, String)> {
    let text = editor.text();
    let cursor_byte = editor.cursor_byte();
    if text.starts_with('/') {
        return slash_token(text, cursor_byte);
    }
    if !editor.has_mention_syntax() {
        return None;
    }
    let before_cursor = &text[..cursor_byte];
    let start_byte = before_cursor.rfind('@')?;
    let query = &text[start_byte..cursor_byte];
    if query.contains(' ') {
        return None;
    }
    Some((
        grapheme_count(&text[..start_byte])..grapheme_count(&text[..cursor_byte]),
        query.to_owned(),
    ))
}

/// Filesystem-backed candidate lookup used by the completion adapter, answered
/// from the caller's index so consecutive queries share one walk.
pub(crate) fn prompt_candidates_in(
    index: &PathIndex,
    workspace: &Path,
    query: &str,
    command_context: &CommandContext,
) -> Result<Vec<CompletionCandidate>, InputError> {
    if query.starts_with('/') {
        return Ok(command_aliases_in(command_context)
            .map(|command| CompletionCandidate {
                id: format!("command:{command}"),
                kind: CompletionKind::SlashCommand,
                label: command.to_owned(),
                insertion: command.to_owned(),
                description: command_description(command, command_context).to_owned(),
            })
            .collect());
    }

    let Some(raw_query) = query.strip_prefix('@') else {
        return Ok(Vec::new());
    };
    index.completer_candidates(workspace, raw_query)
}

#[cfg(test)]
mod completion_tests;
