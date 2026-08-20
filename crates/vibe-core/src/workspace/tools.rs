//! The four file tools, and the permission chain they are published behind.
//!
//! Each tool is one handler plus one specification, and every one of them runs
//! the same three-step shape: read the call's arguments, answer from the
//! connected client when it hosts the filesystem, and otherwise reach the
//! workspace. Naming each handler rather than declaring it inside the
//! registration is what keeps that shape visible: a registration that inlines
//! four of them reads as one function with four unrelated halves.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

use super::review::{self, ReviewManager};
use super::{
    BoundedRead, EditOperation, EditResult, INSTRUCTION_FILE, MutationResult, ReadFileResult,
    SearchOptions, WARNING_TAG, Workspace, WriteFileResult, path_display, unified_diff,
};
use crate::policy::{
    PolicyGuardedTool, RequirementResolver, ToolGuard, resolve_file_tool_permission,
};
use crate::process::ClientToolIo;
use crate::schema::{ObjectSchema, Property};
use crate::tools::config::SharedToolConfig;
use crate::tools::config::{
    GrepConfig, ReadFileConfig, ToolConfigResolver, WriteFileConfig, declared_document,
};
use crate::tools::{
    OwnedToolHandlerFuture, RegistrationOutcome, ToolAvailability, ToolError, ToolExecutionOutput,
    ToolHandler, ToolInvocation, ToolOutputSink, ToolPresentationKind, ToolRegistry, ToolSource,
    ToolSpec, reference_text,
};

pub struct WorkspaceTools {
    workspace: Arc<Workspace>,
    review: Arc<ReviewManager>,
    client_io: Option<ClientToolIo>,
}

impl WorkspaceTools {
    #[must_use]
    pub fn new(workspace: Arc<Workspace>, review: Arc<ReviewManager>) -> Self {
        Self {
            workspace,
            review,
            client_io: None,
        }
    }

    /// Routes file access through the connected client when it declared the
    /// matching capability.
    ///
    /// A client that hosts the editor holds buffers the workspace cannot see, so
    /// the file the agent reads is the one the user is looking at rather than
    /// the one last saved to disk. A client that declared nothing leaves every
    /// tool on the workspace's own filesystem access.
    #[must_use]
    pub fn with_client_io(mut self, client_io: Option<ClientToolIo>) -> Self {
        self.client_io = client_io;
        self
    }

    /// Publishes `read_file`, `grep`, `edit` and `write_file`, each behind the
    /// shared permission chain.
    pub fn register(
        &self,
        registry: &ToolRegistry,
        guard: &ToolGuard,
    ) -> Result<Vec<RegistrationOutcome>, ToolError> {
        let root = self.workspace.root().to_path_buf();
        // Reference `ReadFileTool`, `GrepTool`, `EditTool` and `WriteFileTool`
        // all resolve their permission through one chain over their own path
        // argument, which is what this table declares: the published name, the
        // argument that names a path, and the handler behind it. `grep` is the
        // one whose argument defaults to the working directory, which is what
        // `GrepArgs.path` declares.
        let published: [(&'static str, &'static str, ToolSpec, Arc<dyn ToolHandler>); 4] = [
            (
                "read_file",
                "file_path",
                read_file_spec(),
                read_handler(
                    self.workspace.clone(),
                    self.client_io.clone(),
                    guard.config.clone(),
                ),
            ),
            (
                "grep",
                "path",
                grep_spec(),
                grep_handler(self.workspace.clone(), guard.config.clone()),
            ),
            (
                "edit",
                "file_path",
                edit_spec(),
                edit_handler(
                    self.workspace.clone(),
                    self.review.clone(),
                    self.client_io.clone(),
                ),
            ),
            (
                "write_file",
                "file_path",
                write_file_spec(),
                write_handler(
                    self.workspace.clone(),
                    self.review.clone(),
                    self.client_io.clone(),
                    guard.config.clone(),
                ),
            ),
        ];
        published
            .into_iter()
            .map(|(name, argument, spec, handler)| {
                registry.register(
                    spec,
                    Arc::new(PolicyGuardedTool::new(
                        name,
                        guard.policy.clone(),
                        guard.approval.clone(),
                        file_tool_permission(name, argument, &root, guard),
                        handler,
                    )),
                )
            })
            .collect()
    }
}

/// `read_file`: the file the operator is looking at, and the instruction files
/// the read just brought into scope.
///
/// A client hosting the editor answers first, because its buffer may be ahead
/// of what is on disk. The budget is read per call, so an operator who raises
/// it between two turns is obeyed on the second one.
fn read_handler(
    workspace: Arc<Workspace>,
    client: Option<ClientToolIo>,
    config: ToolConfigResolver,
) -> Arc<dyn ToolHandler> {
    Arc::new(
        move |invocation: &ToolInvocation, output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let workspace = workspace.clone();
            let client = client.clone();
            // The budget is read per call, so an operator who raises it
            // between two turns is obeyed on the second one.
            let settings: ReadFileConfig = config.view("read_file");
            let path = invocation.arguments["file_path"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            // `offset` is nullable and defaults to null, so an absent or
            // explicitly null offset both mean "start at line one".
            let offset = invocation.arguments["offset"]
                .as_u64()
                .and_then(|value| usize::try_from(value).ok());
            let limit = invocation.arguments["limit"]
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(DEFAULT_READ_LINE_LIMIT);
            Box::pin(async move {
                let result = match delegated_read(&workspace, client.as_ref(), &path, offset, limit)
                    .await?
                {
                    Some(result) => result,
                    None => local_read(&workspace, &path, offset, limit, &settings)?,
                };
                // Reference `read_file` raises rather than truncating when
                // the rendered content passes the budget, because a silently
                // clipped file reads as a complete one.
                if result.content.len() > settings.max_read_bytes {
                    return Err(ToolError::Execution(format!(
                        "the rendered output is {} bytes, over the {}-byte budget; narrow it \
                     with offset and limit",
                        result.content.len(),
                        settings.max_read_bytes
                    )));
                }
                let discovered = workspace
                    .undiscovered_instructions(&result.file_path)
                    .map_err(|error| ToolError::Execution(error.to_string()))?;
                let model_text = match instruction_extra(&discovered) {
                    Some(extra) => {
                        // The reference announces the discovery as it
                        // happens, so the operator sees why the turn grew
                        // rather than only reading the result.
                        output.emit(format!(
                            "discovered {}\n",
                            discovered
                                .iter()
                                .map(|(directory, _)| format!("{directory}/{INSTRUCTION_FILE}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))?;
                        format!("{}\n\n{extra}", result.model_text())
                    }
                    None => result.model_text(),
                };
                Ok(ToolExecutionOutput::new(model_text)
                    .displayed_as(json!({
                        "kind": "read",
                        "path": result.file_path,
                        "discovered": discovered
                            .iter()
                            .map(|(directory, _)| directory.clone())
                            .collect::<Vec<_>>(),
                    }))
                    .typed(
                        serde_json::to_value(&result)
                            .map_err(|error| ToolError::InvalidResult(error.to_string()))?,
                    ))
            })
        },
    )
}

/// `grep`: content search under one path, bounded by the configured cap.
fn grep_handler(workspace: Arc<Workspace>, config: ToolConfigResolver) -> Arc<dyn ToolHandler> {
    Arc::new(
        move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let workspace = workspace.clone();
            let settings: GrepConfig = config.view("grep");
            let pattern = invocation.arguments["pattern"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let path = invocation.arguments["path"]
                .as_str()
                .unwrap_or(".")
                .to_owned();
            let requested = invocation.arguments["max_matches"]
                .as_u64()
                .and_then(|limit| usize::try_from(limit).ok());
            let use_default_ignore = invocation.arguments["use_default_ignore"]
                .as_bool()
                .unwrap_or(true);
            Box::pin(async move {
                let options = SearchOptions::from_config(&settings, requested, use_default_ignore);
                let outcome = workspace
                    .search(&pattern, &path, &options)
                    .map_err(|error| ToolError::Execution(error.to_string()))?;
                let result = json!({
                    "matches": outcome.matches.clone(),
                    "match_count": outcome.match_count,
                    "pattern": pattern,
                    "was_truncated": outcome.was_truncated,
                });
                // The projection drops the pattern the model reads and adds
                // the match list a client renders. The search root is what the
                // relative paths `rg` prints resolve against, which is why the
                // projection is built here rather than derived downstream: the
                // typed result never carries it.
                let projected = json!({
                    "matches": outcome.matches,
                    "match_count": outcome.match_count,
                    "was_truncated": outcome.was_truncated,
                    "parsed_matches": parsed_matches(&outcome.matches, workspace.root()),
                });
                let model_text = reference_text::joined(&[
                    ("matches", outcome.matches.clone()),
                    ("match_count", outcome.match_count.to_string()),
                    ("pattern", pattern),
                    (
                        "was_truncated",
                        reference_text::boolean(outcome.was_truncated).to_owned(),
                    ),
                ]);
                Ok(ToolExecutionOutput::new(model_text)
                    .displayed_as(json!({"kind": "search", "matches": outcome.match_count}))
                    .typed(result)
                    .projected(projected))
            })
        },
    )
}

/// The match list the `grep` projection carries, one entry per printed line.
///
/// Reference `GrepResult.parsed_matches`: a line that carries no separator at
/// all names no match and is dropped rather than published half-parsed.
fn parsed_matches(matches: &str, base: &Path) -> Vec<Value> {
    matches
        .lines()
        .filter_map(|line| parsed_match(line, base))
        .collect()
}

/// One `path:line:text` line, split on its first two separators only.
///
/// Reference `GrepMatch.from_output_line`. A single-letter first segment
/// followed by two more is a Windows drive letter rather than a path of its
/// own, and the line number is dropped rather than guessed when it does not
/// read as one. The path is anchored on the search root, so a client resolves
/// it without knowing where the agent was launched from.
fn parsed_match(raw: &str, base: &Path) -> Option<Value> {
    let mut parts = raw.splitn(4, ':');
    let first = parts.next()?;
    let second = parts.next()?;
    let third = parts.next();
    let drive_letter =
        first.chars().count() == 1 && first.chars().all(char::is_alphabetic) && third.is_some();
    let (path, line) = if drive_letter {
        (format!("{first}:{second}"), third.unwrap_or_default())
    } else {
        (first.to_owned(), second)
    };
    let line = if line.is_empty() {
        None
    } else {
        line.parse::<i64>().ok()
    };
    Some(json!({ "path": anchored_match_path(base, &path), "line": line }))
}

/// One printed match path, anchored on the search root and normalized the way
/// `Path.resolve` normalizes: an absolute path stands on its own, and the
/// no-op components a relative one carries are dropped.
fn anchored_match_path(base: &Path, printed: &str) -> String {
    use std::path::Component;

    let joined = base.join(printed);
    let mut resolved = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved.to_string_lossy().replace('\\', "/")
}

/// `edit`: one anchored replacement, refused before it opens anything when the
/// anchor is empty, absent or identical to its replacement.
fn edit_handler(
    workspace: Arc<Workspace>,
    review: Arc<ReviewManager>,
    client: Option<ClientToolIo>,
) -> Arc<dyn ToolHandler> {
    Arc::new(
        move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let review = review.clone();
            let client = client.clone();
            let workspace = workspace.clone();
            let path = invocation.arguments["file_path"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let old_text = invocation.arguments["old_string"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let new_text = invocation.arguments["new_string"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let replace_all = invocation.arguments["replace_all"]
                .as_bool()
                .unwrap_or(false);
            Box::pin(async move {
                // The three refusals the reference raises before it opens
                // anything, each naming its own cause.
                if path.trim().is_empty() {
                    return Err(ToolError::Execution(
                        "edit file_path cannot be empty".to_owned(),
                    ));
                }
                if old_text.is_empty() {
                    return Err(ToolError::Execution(
                        "edit old_string cannot be empty; write_file creates a new file".to_owned(),
                    ));
                }
                if old_text == new_text {
                    return Err(ToolError::Execution(
                        "edit old_string and new_string are identical, so there is nothing to \
                     change"
                            .to_owned(),
                    ));
                }
                let operations = [EditOperation {
                    old_text: old_text.clone(),
                    new_text: new_text.clone(),
                    replace_all,
                }];
                let mutation =
                    match delegated_edit(&workspace, client.as_ref(), &path, &operations).await? {
                        Some(result) => result,
                        None => review
                            .edit(&path, &operations)
                            .map_err(|error| ToolError::Execution(error.to_string()))?,
                    };
                let result = EditResult {
                    file: workspace.absolute_display(Path::new(&mutation.path)),
                    message: edit_message(replace_all),
                    old_string: old_text,
                    new_string: new_text,
                };
                Ok(ToolExecutionOutput::new(result.model_text())
                    .displayed_as(json!({
                        "kind": "diff",
                        "path": result.file,
                        "diff": mutation.diff,
                    }))
                    .typed(
                        serde_json::to_value(&result)
                            .map_err(|error| ToolError::InvalidResult(error.to_string()))?,
                    ))
            })
        },
    )
}

/// `write_file`: a new file only. Every refusal runs before anything touches
/// the filesystem, so a rejected write leaves no directory behind.
fn write_handler(
    workspace: Arc<Workspace>,
    review: Arc<ReviewManager>,
    client: Option<ClientToolIo>,
    config: ToolConfigResolver,
) -> Arc<dyn ToolHandler> {
    Arc::new(
        move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let review = review.clone();
            let client = client.clone();
            let workspace = workspace.clone();
            let settings: WriteFileConfig = config.view("write_file");
            let path = invocation.arguments["file_path"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let content = invocation.arguments["content"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            Box::pin(async move {
                if path.trim().is_empty() {
                    return Err(ToolError::Execution(
                        "write_file file_path cannot be empty".to_owned(),
                    ));
                }
                // Every check runs before anything touches the filesystem,
                // so a refused write leaves no directory behind. The
                // existence check runs again inside the write itself, which
                // is what keeps a race from overwriting a file that
                // appeared in between.
                if content.len() > settings.max_write_bytes {
                    return Err(ToolError::Execution(format!(
                        "the content is {} bytes, exceeding the {}-byte write budget",
                        content.len(),
                        settings.max_write_bytes
                    )));
                }
                if workspace.exists_at(&path) {
                    return Err(ToolError::Execution(format!(
                        "`{path}` already exists; use edit to modify it"
                    )));
                }
                if !settings.create_parent_dirs
                    && let Some(missing) = missing_parent(&workspace, &path)
                {
                    return Err(ToolError::Execution(format!(
                        "the parent directory `{missing}` does not exist and \
                 create_parent_dirs is off"
                    )));
                }
                let mutation =
                    match delegated_write(&workspace, client.as_ref(), &path, &content).await? {
                        Some(result) => result,
                        None => review
                            .write(&path, content.as_bytes())
                            .map_err(|error| ToolError::Execution(error.to_string()))?,
                    };
                let result = WriteFileResult {
                    file_path: workspace.absolute_display(Path::new(&mutation.path)),
                    bytes_written: content.len(),
                    content,
                };
                Ok(ToolExecutionOutput::new(result.model_text())
                    .displayed_as(json!({
                        "kind": "write",
                        "path": result.file_path,
                        "diff": mutation.diff,
                    }))
                    .typed(
                        serde_json::to_value(&result)
                            .map_err(|error| ToolError::InvalidResult(error.to_string()))?,
                    ))
            })
        },
    )
}

/// The permission resolver a file tool is published behind.
///
/// Reference `ReadFileTool`, `GrepTool`, `EditTool` and `WriteFileTool` all
/// answer `resolve_permission` by handing their own name, configuration and
/// path argument to one shared chain. This is that chain's entry point: the
/// argument is resolved against the workspace root, the configuration is read
/// at every call so an operator's edit lands without a re-registration, and the
/// call that names no path at all falls back to the root, which is what
/// `GrepArgs.path` defaults to.
fn file_tool_permission(
    tool: &'static str,
    argument: &'static str,
    root: &Path,
    guard: &ToolGuard,
) -> Arc<RequirementResolver> {
    let root = root.to_path_buf();
    let config = guard.config.clone();
    let scratchpad = guard.scratchpad.clone();
    Arc::new(move |invocation: &ToolInvocation| {
        let requested = invocation.arguments[argument].as_str().unwrap_or_default();
        if requested.is_empty() && argument != "path" {
            return Err(ToolError::Execution(format!(
                "{tool} {argument} is missing"
            )));
        }
        let path = if requested.is_empty() {
            root.clone()
        } else {
            root.join(requested)
        };
        let settings: SharedToolConfig = config.view(tool);
        Ok(resolve_file_tool_permission(
            &path,
            tool,
            &settings,
            scratchpad.as_deref(),
        ))
    })
}

/// The parent directory `path` needs and does not have, or [`None`] when it
/// has one.
///
/// A path the workspace refuses outright is not reported here: the confinement
/// check that follows names it, and answering "the parent is missing" for a
/// path that escapes the root would name the wrong problem.
fn missing_parent(workspace: &Workspace, path: &str) -> Option<String> {
    let relative = workspace.confined(Path::new(path), false).ok()?;
    let parent = relative
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())?;
    if workspace.root().join(parent).is_dir() {
        return None;
    }
    Some(path_display(parent))
}

/// The path a delegated request carries, or the error the local path would
/// have raised for it.
///
/// Hosting the filesystem is not a way around the workspace boundary: the same
/// confinement the local tools apply runs first, so a path that escapes the
/// root is refused here rather than handed to an editor that would happily open
/// it. What travels is the confined absolute path, which is the only form a
/// client can resolve: the request carries no working directory.
fn delegated_path(
    workspace: &Workspace,
    path: &str,
    must_exist: bool,
) -> Result<(PathBuf, String), ToolError> {
    let relative = workspace
        .confined(Path::new(path), must_exist)
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let display = path_display(&relative);
    Ok((workspace.root().join(relative), display))
}

/// Reads one file through the client, or `None` when it hosts no filesystem.
///
/// The client answers with a window rather than a file, so the totals it
/// implies are what it returned: asking for one line past the budget is how the
/// answer reports that it stopped short without a second round trip, and a
/// short answer at the top of the file is the only one that can be called
/// complete.
async fn delegated_read(
    workspace: &Workspace,
    client: Option<&ClientToolIo>,
    path: &str,
    offset: Option<usize>,
    limit: usize,
) -> Result<Option<ReadFileResult>, ToolError> {
    let Some(client) = client.filter(|client| client.supports_read()) else {
        return Ok(None);
    };
    let (absolute, _) = delegated_path(workspace, path, true)?;
    let display = absolute.to_string_lossy().replace('\\', "/");
    let start = offset.unwrap_or(1).max(1);
    let line_limit = limit.min(workspace.max_lines);
    let content = client
        .read_text_file(
            &absolute.to_string_lossy(),
            u64::try_from(start).ok(),
            u64::try_from(line_limit.saturating_add(1)).ok(),
        )
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let mut cut = workspace.max_read_bytes.min(content.len());
    while cut > 0 && !content.is_char_boundary(cut) {
        cut = cut.saturating_sub(1);
    }
    let byte_truncated = cut < content.len();
    let all_lines = content
        .get(..cut)
        .unwrap_or_default()
        .lines()
        .collect::<Vec<_>>();
    let selected = all_lines
        .iter()
        .take(line_limit)
        .map(|line| (*line).to_owned())
        .collect::<Vec<_>>();
    let was_truncated = byte_truncated || all_lines.len() > line_limit;
    let total_lines = if was_truncated {
        None
    } else if selected.is_empty() && start == 1 {
        Some(0)
    } else {
        Some(start.saturating_sub(1).saturating_add(selected.len()))
    };
    Ok(Some(build_read_result(
        display,
        BoundedRead {
            lines: selected,
            total_lines,
            was_truncated,
        },
        offset,
        limit,
    )))
}

/// Writes one file through the client, or `None` when it hosts no filesystem.
///
/// Nothing on disk changes, so nothing is checkpointed: the buffer the client
/// now holds is its own to save, and a rewind that restored the untouched file
/// would claim an edit the workspace never made.
async fn delegated_write(
    workspace: &Workspace,
    client: Option<&ClientToolIo>,
    path: &str,
    content: &str,
) -> Result<Option<MutationResult>, ToolError> {
    let Some(client) = client.filter(|client| client.supports_write()) else {
        return Ok(None);
    };
    // A write may target a file that does not exist yet, on the client's side
    // as much as on ours.
    let (absolute, display) = delegated_path(workspace, path, false)?;
    client
        .write_text_file(&absolute.to_string_lossy(), content)
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    Ok(Some(MutationResult {
        path: display,
        bytes_written: content.len(),
        files_changed: 1,
        diff: unified_diff("", content),
    }))
}

/// Edits one file through the client, or `None` when it hosts no filesystem.
///
/// The read and the write are both delegated, so the text the operations run
/// against is the buffer's rather than the file's. A client that hosts reads
/// but not writes leaves the whole edit local: applying an edit to a buffer and
/// saving it to disk would write over the very content it was not read from.
async fn delegated_edit(
    workspace: &Workspace,
    client: Option<&ClientToolIo>,
    path: &str,
    operations: &[EditOperation],
) -> Result<Option<MutationResult>, ToolError> {
    let Some(client) = client.filter(|client| client.supports_read() && client.supports_write())
    else {
        return Ok(None);
    };
    let (absolute, display) = delegated_path(workspace, path, true)?;
    let absolute = absolute.to_string_lossy().into_owned();
    let original = client
        .read_text_file(&absolute, None, None)
        .await
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let updated = review::apply_edit_operations(Path::new(&display), &original, operations)
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    if updated != original {
        client
            .write_text_file(&absolute, &updated)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))?;
    }
    Ok(Some(MutationResult {
        path: display,
        bytes_written: updated.len(),
        files_changed: 1,
        diff: unified_diff(&original, &updated),
    }))
}

/// One `read_file` call answered from the workspace itself.
///
/// The three refusals the reference raises each name their own cause: an empty
/// argument, a path that does not exist, and a directory. Collapsing them into
/// one message would leave a model unable to tell a typo from a wrong kind of
/// target.
fn local_read(
    workspace: &Workspace,
    path: &str,
    offset: Option<usize>,
    limit: usize,
    settings: &ReadFileConfig,
) -> Result<ReadFileResult, ToolError> {
    if path.trim().is_empty() {
        return Err(ToolError::Execution(
            "read_file file_path cannot be empty".to_owned(),
        ));
    }
    let relative = workspace
        .confined(Path::new(path), false)
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let display = workspace.absolute_display(&relative);
    if !workspace.exists(&relative) {
        return Err(ToolError::Execution(format!(
            "no file exists at `{display}`"
        )));
    }
    if workspace.is_directory(&relative) {
        return Err(ToolError::Execution(format!(
            "`{display}` is a directory, not a file"
        )));
    }
    let start = offset.unwrap_or(1).max(1);
    let read = workspace
        .read_lines_bounded(&relative, start, limit, settings.max_read_bytes)
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    Ok(build_read_result(display, read, offset, limit))
}

/// The result one bounded read produces, warnings included.
///
/// An empty selection is not an empty success: the reference tells an empty
/// file from an offset past the last line from a read that returned nothing at
/// all, and says which in the content the model receives.
fn build_read_result(
    file_path: String,
    read: BoundedRead,
    offset: Option<usize>,
    limit: usize,
) -> ReadFileResult {
    let start = offset.unwrap_or(1).max(1);
    let content = if read.lines.is_empty() {
        match read.total_lines {
            Some(0) => warning("this file exists and holds no content".to_owned()),
            Some(total) => warning(format!(
                "this file holds {total} lines, which stops short of the requested offset {start}"
            )),
            None => warning(format!("no content was returned for offset {start}")),
        }
    } else {
        read.lines
            .iter()
            .enumerate()
            .map(|(index, line)| format!("{:>9}\u{2192}{line}", start.saturating_add(index)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    ReadFileResult {
        file_path,
        content,
        num_lines: read.lines.len(),
        start_line: start,
        requested_offset: offset,
        requested_limit: limit,
        total_lines: read.total_lines,
        was_truncated: read.was_truncated,
    }
}

/// A notice the model reads as one, under the tag the reference publishes.
fn warning(message: String) -> String {
    format!("<{WARNING_TAG}>{message}</{WARNING_TAG}>")
}

/// The block appended to a read whose directories carry their own `AGENTS.md`.
///
/// The prose is this port's own: `NOTICE` forbids reproducing the reference's,
/// and what the block has to do is name the directory and carry its
/// instructions.
fn instruction_extra(discovered: &[(String, String)]) -> Option<String> {
    if discovered.is_empty() {
        return None;
    }
    let sections = discovered
        .iter()
        .map(|(directory, content)| {
            format!(
                "Instructions from {directory}/{INSTRUCTION_FILE}, which apply to this \
                 directory:\n\n{content}"
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Some(format!("<{WARNING_TAG}>\n{sections}\n</{WARNING_TAG}>"))
}

/// What an edit reports once it has been applied.
///
/// Two messages rather than one, because the reference distinguishes the single
/// replacement from the sweep, and a model that asked for `replace_all` needs to
/// read back that every occurrence moved.
fn edit_message(replace_all: bool) -> String {
    if replace_all {
        "the file was updated and every occurrence was replaced".to_owned()
    } else {
        "the file was updated".to_owned()
    }
}

/// Reference `ReadFileArgs.limit` default.
/// Reference `ReadFileArgs.limit` default.
const DEFAULT_READ_LINE_LIMIT: usize = 2000;

fn read_file_spec() -> ToolSpec {
    ToolSpec {
        name: "read_file".to_owned(),
        description: "Read one file from an absolute path. Page through a long file with \
                      `offset` and `limit` instead of reading it whole, reach for `grep` when \
                      looking for specific content, and leave binary and model-weight files \
                      (.bin, .safetensors, .pt, .gguf) alone."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "file_path",
                Property::string().described("The absolute path of the file to read"),
            )
            .optional(
                "offset",
                Property::integer()
                    .constrained("minimum", 1)
                    .described("The 1-indexed line the read starts at")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "limit",
                Property::integer()
                    .constrained("exclusiveMinimum", 0)
                    .described("How many lines to read at most")
                    .with_default(json!(DEFAULT_READ_LINE_LIMIT)),
            )
            .build(),
        output_schema: None,
        config: declared_document("read_file"),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Read,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

fn grep_spec() -> ToolSpec {
    ToolSpec {
        name: "grep".to_owned(),
        description: "Search file contents against a regular expression, reporting the path, \
                      the line number and the matching line. Narrow the walk with `path`."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "pattern",
                Property::string().described("The regular expression matched against file contents"),
            )
            .optional(
                "path",
                Property::string()
                    .described(
                        "The file or directory the search walks. Defaults to the working directory.",
                    )
                    .with_default("."),
            )
            .optional(
                "max_matches",
                Property::integer()
                    .described("Raises or lowers the default cap on returned matches.")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "use_default_ignore",
                Property::boolean()
                    .described("Whether .gitignore and .ignore entries are honored.")
                    .with_default(true),
            )
            .build(),
        output_schema: None,
        config: declared_document("grep"),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Search,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

fn edit_spec() -> ToolSpec {
    ToolSpec {
        name: "edit".to_owned(),
        description: "Replace an exact string in a file. Call `read_file` first, and never carry \
                      any part of its line-number prefix into `old_string` or `new_string`. When \
                      `old_string` is absent or matches more than once, add surrounding context \
                      until it is unique or set `replace_all`. Re-read the file before retrying a \
                      failed edit."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "file_path",
                Property::string().described("The absolute path of the file to modify"),
            )
            .required(
                "old_string",
                Property::string().described("The text being replaced"),
            )
            .required(
                "new_string",
                Property::string()
                    .described("The replacement text, which must differ from old_string"),
            )
            .optional(
                "replace_all",
                Property::boolean()
                    .described("Replace every occurrence of old_string instead of a single one")
                    .with_default(false),
            )
            .build(),
        output_schema: None,
        config: declared_document("edit"),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Diff,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}

/// Directive coverage for `write_file`, whose reference description this port
/// must cover without reproducing (`NOTICE`).
///
/// | Reference directive | Covered by |
/// |---|---|
/// | The path is absolute | "absolute path" in the `file_path` description |
/// | An existing file is not overwritten; `edit` modifies it | "never overwrites an existing file: reach for `edit`" |
/// | Missing parent directories are created | "Missing parent directories are created" |
/// | The content replaces the whole file | "the whole file content" in the `content` description |
fn write_file_spec() -> ToolSpec {
    ToolSpec {
        name: "write_file".to_owned(),
        description: "Create a file at an absolute path and write it whole. It never overwrites \
                      an existing file: reach for `edit` to change one. Missing parent \
                      directories are created."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "file_path",
                Property::string()
                    .described("The absolute path of the file to write, which must not exist yet"),
            )
            .required(
                "content",
                Property::string().described("The whole file content to write"),
            )
            .build(),
        output_schema: None,
        config: declared_document("write_file"),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Diff,
        source: ToolSource::BuiltIn,
        selection_priority: 100,
    }
}
