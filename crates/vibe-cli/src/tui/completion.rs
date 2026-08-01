use std::collections::BTreeMap;
#[cfg(test)]
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};

use serde::{Deserialize, Serialize};

use super::commands::{CommandContext, command_aliases_in, command_description};
use super::input::{InputError, PromptEditor, grapheme_byte, grapheme_count};

mod fuzzy;
mod path;

use fuzzy::fuzzy_match_score;
use path::{WorkspaceIndex, path_candidates};

pub const MAX_VISIBLE_COMPLETIONS: usize = 10;

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
}

#[derive(Debug, Clone)]
struct ActiveCompletion {
    token_range: Range<usize>,
    result: CompletionResult,
    selected: usize,
}

#[derive(Debug)]
struct CompletionWorker {
    state: Arc<(Mutex<CompletionWorkerState>, Condvar)>,
    results: Receiver<CompletionResolution>,
}

#[derive(Debug, Default)]
struct CompletionWorkerState {
    pending: Option<CompletionJob>,
    shutdown: bool,
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
    fn spawn() -> Result<Self, InputError> {
        let state = Arc::new((Mutex::new(CompletionWorkerState::default()), Condvar::new()));
        let thread_state = Arc::clone(&state);
        let (results_tx, results) = mpsc::channel();
        std::thread::Builder::new()
            .name("vibe-path-completion".to_owned())
            .spawn(move || completion_worker_loop(&thread_state, &results_tx))
            .map_err(|error| InputError::CompletionWorker(error.to_string()))?;
        Ok(Self { state, results })
    }

    fn submit(&self, request: CompletionJob) -> Result<(), InputError> {
        let (state, ready) = self.state.as_ref();
        let mut state = state
            .lock()
            .map_err(|_| InputError::CompletionWorker("worker lock is poisoned".to_owned()))?;
        state.pending = Some(request);
        ready.notify_one();
        Ok(())
    }

    fn cancel_pending(&self) {
        let (state, _) = self.state.as_ref();
        if let Ok(mut state) = state.lock() {
            state.pending = None;
        }
    }
}

impl Drop for CompletionWorker {
    fn drop(&mut self) {
        let (state, ready) = self.state.as_ref();
        if let Ok(mut state) = state.lock() {
            state.shutdown = true;
            state.pending = None;
            ready.notify_one();
        }
    }
}

fn completion_worker_loop(
    shared: &Arc<(Mutex<CompletionWorkerState>, Condvar)>,
    results: &mpsc::Sender<CompletionResolution>,
) {
    let mut index = WorkspaceIndex::default();
    loop {
        let job = {
            let (state, ready) = shared.as_ref();
            let Ok(mut state) = state.lock() else {
                return;
            };
            while state.pending.is_none() && !state.shutdown {
                let Ok(next) = ready.wait(state) else {
                    return;
                };
                state = next;
            }
            if state.shutdown {
                return;
            }
            state.pending.take()
        };
        let Some(job) = job else {
            continue;
        };
        let request = job.request;
        let resolution = match request.query.strip_prefix('@') {
            Some(query) => match index.candidates(&job.workspace, query) {
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

    pub fn set_vibe_code_enabled(&mut self, enabled: bool) {
        if self.command_context.vibe_code_enabled != enabled {
            self.command_context.vibe_code_enabled = enabled;
            self.cancel();
        }
    }

    pub fn set_command_context(&mut self, context: CommandContext) {
        if self.command_context != context {
            self.command_context = context;
            self.cancel();
        }
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
        self.cancel();
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
                self.worker = Some(CompletionWorker::spawn()?);
                self.worker.as_ref().ok_or_else(|| {
                    InputError::CompletionWorker("worker is unavailable".to_owned())
                })?
            }
        };
        worker.submit(CompletionJob {
            request,
            workspace: workspace.to_path_buf(),
        })?;
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
        let mut candidates = prompt_candidates_in(workspace, query, &self.command_context)?;
        if query.starts_with('/') {
            candidates.extend(self.user_skills.iter().cloned());
        }
        Ok(candidates)
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
    /// with it and hand it back, so a late answer can be recognised as stale.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Ranks candidates and presents them without touching the filesystem.
    ///
    /// Slash candidates arrive raw. Slash ranking, the mention cap and the
    /// empty-result rule are part of the observable contract and stay in this
    /// layer whether lookup ran on the worker thread or in a replay adapter.
    pub fn install(
        &mut self,
        generation: u64,
        token_range: Range<usize>,
        query: &str,
        candidates: Vec<CompletionCandidate>,
    ) {
        self.active = None;
        let mut result = ranked_completion(generation, query, candidates);
        if result
            .candidates
            .first()
            .is_some_and(|candidate| candidate.kind == CompletionKind::Mention)
        {
            result.candidates.truncate(MAX_VISIBLE_COMPLETIONS);
        }
        if !result.candidates.is_empty() {
            self.active = Some(ActiveCompletion {
                token_range,
                result,
                selected: 0,
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
        editor.select(active.token_range);
        editor.insert(&candidate.insertion);
        if candidate.kind == CompletionKind::Mention && !candidate.insertion.ends_with('/') {
            editor.insert(" ");
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
        self.generation = self.generation.saturating_add(1);
        self.active = None;
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
    let query = query.trim_start_matches('/').to_lowercase();
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

pub(crate) fn active_token(editor: &PromptEditor) -> Option<(Range<usize>, String)> {
    let text = editor.text();
    let cursor_byte = grapheme_byte(text, editor.cursor());
    if text.starts_with('/') {
        let end_byte = text.find(' ').unwrap_or(text.len());
        let query_end = cursor_byte.min(end_byte);
        return Some((
            0..grapheme_count(&text[..query_end]),
            text[..query_end].to_owned(),
        ));
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

/// Filesystem-backed candidate lookup used by the completion adapter.
pub fn prompt_candidates(
    workspace: &Path,
    query: &str,
) -> Result<Vec<CompletionCandidate>, InputError> {
    prompt_candidates_in(workspace, query, &CommandContext::default())
}

pub fn prompt_candidates_in(
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
                description: command_description(command).to_owned(),
            })
            .collect());
    }

    let Some(raw_query) = query.strip_prefix('@') else {
        return Ok(Vec::new());
    };
    path_candidates(workspace, raw_query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completions_are_stable_and_racing_results_are_rejected() {
        let mut engine = CompletionEngine::default();
        let candidates = vec![
            CompletionCandidate {
                id: "skill".to_owned(),
                kind: CompletionKind::Skill,
                label: "review".to_owned(),
                insertion: "/review".to_owned(),
                description: "Review changes".to_owned(),
            },
            CompletionCandidate {
                id: "command".to_owned(),
                kind: CompletionKind::SlashCommand,
                label: "reload".to_owned(),
                insertion: "/reload".to_owned(),
                description: "Reload configuration".to_owned(),
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
    fn runtime_completion_applies_slash_and_at_mention_candidates() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        fs::create_dir(temporary.path().join("src")).expect("path fixture");
        let mut engine = CompletionEngine::default();
        let mut editor = PromptEditor::default();

        editor.set_text("/hel");
        assert!(
            engine
                .complete_prompt(&mut editor, temporary.path())
                .expect("slash completion")
        );
        assert_eq!(editor.text(), "/help");

        editor.set_text("inspect sr");
        assert!(
            !engine
                .complete_prompt(&mut editor, temporary.path())
                .expect("bare paths do not activate completion")
        );
        assert_eq!(editor.text(), "inspect sr");

        editor.set_text("inspect @sr");
        assert!(
            engine
                .complete_prompt(&mut editor, temporary.path())
                .expect("mention completion")
        );
        assert_eq!(editor.text(), "inspect @src/");
    }

    #[test]
    fn live_completion_tracks_selection_and_applies_official_spacing() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        fs::write(temporary.path().join("notes.txt"), "context").expect("mention fixture");
        fs::create_dir(temporary.path().join("src")).expect("nested directory");
        fs::write(temporary.path().join("src/lib.rs"), "pub fn nested() {}")
            .expect("nested mention fixture");
        let mut engine = CompletionEngine::default();
        let mut editor = PromptEditor::default();

        editor.set_text("/cl");
        engine
            .refresh(&editor, temporary.path())
            .expect("slash suggestions");
        let view = engine.view().expect("visible slash suggestions");
        assert_eq!(view.candidates[view.selected].label, "/clear");
        let suggestion_count = view.candidates.len();
        assert!(engine.move_selection(-1));
        assert_eq!(
            engine.view().expect("wrapped selection").selected,
            suggestion_count - 1
        );
        assert!(engine.move_selection(1));
        assert_eq!(engine.view().expect("rewrapped selection").selected, 0);
        assert_eq!(
            engine.accept(&mut editor).expect("slash completion"),
            CompletionAction::Submit
        );
        assert_eq!(editor.text(), "/clear");
        assert!(engine.view().is_none());

        editor.set_text("inspect @no");
        engine
            .refresh(&editor, temporary.path())
            .expect("mention suggestions");
        engine
            .wait_for_pending(&editor)
            .expect("completed mention search");
        assert_eq!(
            engine.accept(&mut editor).expect("mention completion"),
            CompletionAction::Applied
        );
        assert_eq!(editor.text(), "inspect @notes.txt ");

        editor.set_text("inspect @lib");
        engine
            .refresh(&editor, temporary.path())
            .expect("recursive mention suggestions");
        engine
            .wait_for_pending(&editor)
            .expect("completed recursive search");
        assert_eq!(
            engine
                .view()
                .expect("recursive suggestion")
                .candidates
                .first()
                .map(|candidate| candidate.label.as_str()),
            Some("@src/lib.rs")
        );

        editor.set_text("inspect @noXYZ");
        editor.move_left(false);
        editor.move_left(false);
        editor.move_left(false);
        engine
            .refresh(&editor, temporary.path())
            .expect("partial mention suggestions");
        engine
            .wait_for_pending(&editor)
            .expect("completed partial search");
        engine.accept(&mut editor).expect("partial completion");
        assert_eq!(editor.text(), "inspect @notes.txt XYZ");
    }

    #[test]
    fn completion_candidates_are_not_truncated_at_the_old_sixty_four_item_bound() {
        let mut engine = CompletionEngine::default();
        let result = engine.complete(
            "/",
            (0..128).map(|index| CompletionCandidate {
                id: format!("candidate-{index}"),
                kind: CompletionKind::SlashCommand,
                label: format!("/candidate-{index}"),
                insertion: format!("/candidate-{index}"),
                description: String::new(),
            }),
        );
        assert_eq!(result.candidates.len(), 128);
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
        assert_eq!(editor.selection(), None);
        editor.move_right(true);
        assert_eq!(editor.selection(), Some(3..4));
    }

    #[test]
    fn duplicate_slash_aliases_keep_one_entry_and_the_last_sorted_description() {
        let mut engine = CompletionEngine::default();
        let result = engine.complete(
            "/",
            [
                CompletionCandidate {
                    id: "command:review".to_owned(),
                    kind: CompletionKind::SlashCommand,
                    label: "/review".to_owned(),
                    insertion: "/review".to_owned(),
                    description: "Built-in review".to_owned(),
                },
                CompletionCandidate {
                    id: "skill:review".to_owned(),
                    kind: CompletionKind::Skill,
                    label: "/review".to_owned(),
                    insertion: "/review".to_owned(),
                    description: "Skill review".to_owned(),
                },
            ],
        );
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].description, "Skill review");
    }

    #[test]
    fn capability_updates_preserve_command_exclusions() {
        let mut engine = CompletionEngine::default();
        engine.set_command_context(CommandContext::default().with_excluded(["voice"]));
        engine.set_vibe_code_enabled(true);
        let labels = engine
            .prompt_candidates(Path::new("/workspace"), "/")
            .expect("in-memory slash candidates")
            .into_iter()
            .map(|candidate| candidate.label)
            .collect::<Vec<_>>();
        assert!(labels.contains(&"/teleport".to_owned()));
        assert!(!labels.contains(&"/voice".to_owned()));
    }

    #[test]
    fn path_popup_keeps_at_most_ten_ranked_candidates() {
        let temporary = tempfile::tempdir().expect("temporary workspace");
        for index in 0..15 {
            fs::write(
                temporary.path().join(format!("file-{index:02}.txt")),
                "fixture",
            )
            .expect("path candidate fixture");
        }
        let candidates = prompt_candidates(temporary.path(), "@").expect("path candidates");
        assert_eq!(candidates.len(), 15);
        let mut engine = CompletionEngine::default();
        engine.install(0, 0..1, "@", candidates);
        assert_eq!(
            engine.view().expect("path popup").candidates.len(),
            MAX_VISIBLE_COMPLETIONS
        );
    }
}
