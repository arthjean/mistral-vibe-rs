//! The `Arguments`-facing half of the worktree contract.
//!
//! Preparation, ownership, inspection and removal live in `vibe_core::worktree`,
//! one layer below, so the app-server reaches them too. What is left here is
//! what only a terminal launch knows: which directory the invocation meant,
//! where the vibe home is, how `--add-dir` resolves once the effective
//! directory moved, which prompt names an unnamed worktree, and whether a
//! human at a terminal agreed to discard their work.

use std::io::{BufRead, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use vibe_core::llm::AmbientCredentials;
use vibe_core::provider::config::ModelConfig;
use vibe_core::pycompat;

use vibe_core::worktree::{
    ManagedRoot, ManagedWorktree, PreparedWorktree, WorktreeError, WorktreeRepository,
    inspect_worktree_for_cleanup, naming_model, remove_worktree,
};

use crate::Arguments;

use super::{StartupError, startup_io, vibe_home_directory};

impl From<WorktreeError> for StartupError {
    fn from(error: WorktreeError) -> Self {
        match error {
            WorktreeError::InvalidName => Self::InvalidWorktreeName,
            WorktreeError::InvalidBranch { branch } => Self::InvalidWorktreeBranch { branch },
            WorktreeError::ManagedRootEscape {
                target,
                managed_root,
            } => Self::WorktreeManagedRoot {
                target,
                managed_root,
            },
            WorktreeError::RepositoryRequired => Self::WorktreeRepositoryRequired,
            WorktreeError::GitUnavailable(message) => Self::WorktreeGitUnavailable(message),
            WorktreeError::Failed { name, message } => Self::Worktree { name, message },
            WorktreeError::ListFailed { message } => Self::WorktreeListFailed(message),
            WorktreeError::Io { path, source } => Self::Io { path, source },
            error @ (WorktreeError::Git { .. }
            | WorktreeError::Record { .. }
            | WorktreeError::Noted { .. }) => Self::WorktreeRefused(error.to_string()),
        }
    }
}

/// The holder a CLI process registers in the worktree it runs in, one per
/// process. An app server sweeping the same repository reads these markers to
/// tell a live worktree from an abandoned one (`vibe/cli/entrypoint.py:289-293`).
#[must_use]
pub fn cli_worktree_holder() -> String {
    format!("cli-{}", std::process::id())
}

/// What `--worktree` asked for, read the way the reference reads the flag:
/// absent or empty asks for nothing, bare asks Vibe to name one.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WorktreeFlag {
    Named(String),
    Auto,
}

impl WorktreeFlag {
    fn from_arguments(arguments: &Arguments) -> Option<Self> {
        match &arguments.worktree {
            None => None,
            Some(None) => Some(Self::Auto),
            // An empty value is falsy upstream, so it names no worktree at all
            // rather than an unportable one (`vibe/cli/entrypoint.py:434`).
            Some(Some(name)) if name.is_empty() => None,
            Some(Some(name)) => Some(Self::Named(name.clone())),
        }
    }
}

/// Where a launch actually runs, and the worktree it prepared to get there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchWorkspace {
    pub effective_directory: PathBuf,
    pub worktree: Option<PreparedWorktree>,
}

impl LaunchWorkspace {
    /// Resolves where the launch runs, narrating the worktree it prepares.
    ///
    /// The two lines are the reference's, on the stream it writes them on: the
    /// requested name, or none for a bare flag, before any git command runs, so
    /// an invocation that dies inside `worktree add` is still attributable, and
    /// the working path once preparation returned. The launch then registers
    /// as a holder of the worktree, turning the attachment hold a reused one
    /// came back with into its own (`vibe/cli/entrypoint.py:221-246`).
    ///
    /// An unnamed worktree is named after the prompt the command line carried,
    /// `-p` first: a prompt piped on stdin is read after this runs, as it is
    /// upstream, so it never names one.
    pub fn prepare(
        arguments: &mut Arguments,
        narration: &mut impl Write,
    ) -> Result<Self, StartupError> {
        // Reading the working directory is this port's counterpart to the
        // reference's `Path.cwd()` guard: it never chdirs, so the one place a
        // deleted directory shows up is where the launch asks for it, and
        // `--workdir` pre-empts the question exactly as it does upstream.
        let requested = match &arguments.workdir {
            Some(path) => path.clone(),
            None => std::env::current_dir().map_err(|_| StartupError::WorkingDirectoryGone)?,
        };
        let expanded = vibe_core::platform::expand_home(&requested);
        let base_directory = match canonical_directory(&expanded) {
            Some(directory) => directory,
            None if arguments.workdir.is_some() => {
                // The reference reports the resolved path here, and resolving
                // is what `Path.resolve()` does whether or not the path is
                // there, so a missing one is still printed absolute.
                return Err(StartupError::WorkdirNotADirectory(absolute_path(&expanded)));
            }
            None => return Err(StartupError::WorkingDirectoryGone),
        };

        let flag = WorktreeFlag::from_arguments(arguments);
        let Some(flag) = flag.filter(|_| !arguments.setup && !arguments.check_upgrade) else {
            resolve_additional_directories(arguments, &base_directory)?;
            arguments.workdir = Some(base_directory.clone());
            return Ok(Self {
                effective_directory: base_directory,
                worktree: None,
            });
        };

        let managed = ManagedRoot::for_vibe_home(&vibe_home_directory(arguments, &base_directory));
        let requested_name = match &flag {
            WorktreeFlag::Named(name) => format!(" {}", pycompat::repr_str(name)),
            WorktreeFlag::Auto => String::new(),
        };
        writeln!(narration, "Preparing worktree{requested_name}...")
            .map_err(|source| startup_io(Path::new("stderr"), source))?;
        let repository = WorktreeRepository::open(&base_directory, &managed)?;
        let worktree = match &flag {
            WorktreeFlag::Named(name) => repository.prepare(name, None)?,
            WorktreeFlag::Auto => {
                let prompt = naming_prompt(arguments);
                let suggested = suggest_worktree_name(arguments, prompt.as_deref());
                repository.prepare_auto(prompt.as_deref(), suggested.as_deref())?
            }
        };
        writeln!(narration, "Using worktree: {}", worktree.path.display())
            .map_err(|source| startup_io(Path::new("stderr"), source))?;
        if let Some(held) = ManagedWorktree::at(&managed, &worktree.root) {
            held.hold(&cli_worktree_holder(), worktree.pending_hold.as_ref())?;
        }
        resolve_additional_directories(arguments, &worktree.path)?;
        arguments.workdir = Some(worktree.path.clone());
        arguments.trust = true;
        Ok(Self {
            effective_directory: worktree.path.clone(),
            worktree: Some(worktree),
        })
    }

    /// Drops this process's holder, for every worktree the launch held and not
    /// only the ones cleanup was offered for: a marker left behind reads as a
    /// live session to every later release (`vibe/cli/entrypoint.py:488-497`).
    pub fn release_holder(&self) {
        if let Some(worktree) = &self.worktree
            && let Some(managed) = ManagedRoot::containing(&worktree.root)
            && let Some(held) = ManagedWorktree::at(&managed, &worktree.root)
        {
            let _ = held.release_holder(&cli_worktree_holder());
        }
    }
}

/// The prompt an unnamed worktree is named after: `-p` when it carries text,
/// else the positional prompt (`vibe/cli/entrypoint.py:232-236`).
fn naming_prompt(arguments: &Arguments) -> Option<String> {
    arguments
        .prompt
        .clone()
        .filter(|prompt| !prompt.is_empty())
        .or_else(|| arguments.initial_prompt.clone())
        .filter(|prompt| !prompt.is_empty())
}

/// The naming model's suggestion, or [`None`] whenever there is none to be had
/// in time. The key the utility model needs resolves from the environment or
/// the global dotenv file, as every credential of this launch does. The
/// request reports the CLI launch (`vibe/cli/entrypoint.py:377-379`).
fn suggest_worktree_name(arguments: &Arguments, prompt: Option<&str>) -> Option<String> {
    prompt.filter(|prompt| !prompt.is_empty())?;
    let provider = utility_provider(arguments)?;
    naming_model::suggest_worktree_name_blocking(
        prompt,
        Some(provider.as_ref()),
        Some(&crate::cli_launch_context()),
    )
}

/// The provider utility completions of this launch run on: the fast Mistral
/// model when a Mistral provider is usable, the launch's own model otherwise.
/// Keys resolve from the environment, the global dotenv file and the keyring,
/// as every credential of this launch does.
#[must_use]
pub(crate) fn utility_provider(
    arguments: &Arguments,
) -> Option<Arc<dyn vibe_core::engine::CompletionProvider>> {
    let workspace = vibe_app_server::workspace::WorkspaceService::default();
    let mut routing = crate::bootstrap::model_routing(&workspace).ok()?;
    let launch = crate::bootstrap::launch_provider(arguments, &routing).ok()?;
    // The session's model runs on the provider the launch names.
    let mut active = routing
        .model(Some(arguments.model()))
        .unwrap_or_else(|_| ModelConfig::new(arguments.model(), &launch.name));
    active.provider.clone_from(&launch.name);
    routing
        .providers
        .retain(|provider| provider.name != launch.name);
    routing.providers.insert(0, launch);
    routing.models.retain(|model| model.alias != active.alias);
    routing.active_alias = Some(active.alias.clone());
    routing.models.push(active);
    let credentials = Arc::new(AmbientCredentials::new(
        crate::bootstrap::dotenv_values(arguments),
        vibe_core::auth::KeyringStore::native(),
    ));
    naming_model::utility_provider(&routing, credentials)
}

fn resolve_additional_directories(
    arguments: &mut Arguments,
    effective_directory: &Path,
) -> Result<(), StartupError> {
    for directory in &mut arguments.add_directories {
        // The reference reports the argument as it was typed, so the raw
        // spelling is taken before anything expands or joins it.
        let typed = directory.display().to_string();
        let expanded = vibe_core::platform::expand_home(directory);
        let candidate = if expanded.is_absolute() {
            expanded
        } else {
            effective_directory.join(expanded)
        };
        *directory =
            canonical_directory(&candidate).ok_or(StartupError::AddDirNotADirectory(typed))?;
    }
    Ok(())
}

/// `path` made absolute without touching the filesystem.
///
/// `fs::canonicalize` is the wrong tool for a path that is being reported
/// precisely because it is not there: it fails on a missing path, while the
/// reference's `Path.resolve()` returns one. The lexical walk below is what is
/// left once the filesystem cannot be asked.
fn absolute_path(path: &Path) -> PathBuf {
    let mut resolved = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().unwrap_or_default()
    };
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupOutcome {
    NotOwned,
    Kept,
    Removed,
    /// Git refused the removal after the question was answered, so what is left
    /// on disk is whatever git left there.
    Failed,
}

/// Whether a finished run offers to discard the worktree it prepared.
///
/// Reference `_run_cli_with_worktree_cleanup`: only a worktree this run created,
/// only without `--prompt`, and only when the run ended with exit code 0 or
/// none. A startup that failed keeps the worktree, because a reused one and its
/// branch must survive a bad configuration (`vibe/cli/entrypoint.py:465-487`).
#[must_use]
pub fn cleanup_is_offered(
    worktree: Option<&PreparedWorktree>,
    prompt: Option<&str>,
    exit_code: Option<u8>,
) -> bool {
    worktree.is_some_and(|worktree| worktree.created)
        && prompt.is_none()
        && matches!(exit_code, None | Some(0))
}

/// Offers cleanup at the terminal, asking on stdin.
///
/// The reference asks through `input()`, which reads stdin and nothing else, so
/// a piped stdin at end of file declines rather than reopening the controlling
/// terminal behind the pipe (`vibe/cli/entrypoint.py:249-286`).
pub fn cleanup_worktree_terminal(
    worktree: PreparedWorktree,
) -> Result<CleanupOutcome, StartupError> {
    cleanup_worktree(
        worktree,
        &mut std::io::stdin().lock(),
        &mut std::io::stderr().lock(),
    )
}

/// Asks whether to discard the worktree, then removes it through the core.
///
/// A run that reached this point already produced its result, so neither git
/// failure ends it: an inspection that cannot read the worktree cannot ask a
/// truthful question about it and keeps it, and a removal git refuses is
/// reported and leaves behind whatever git left. This process's holder stays up
/// throughout, and the holders are read again after the questions, because a
/// session elsewhere can join the worktree while the operator sits at them
/// (`vibe/cli/entrypoint.py:296-349`).
pub fn cleanup_worktree(
    worktree: PreparedWorktree,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<CleanupOutcome, StartupError> {
    if !worktree.created {
        return Ok(CleanupOutcome::NotOwned);
    }
    let state = match inspect_worktree_for_cleanup(&worktree) {
        Ok(state) => state,
        Err(error) => {
            writeln!(output, "Could not inspect worktree for cleanup: {error}")
                .map_err(|source| startup_io(&worktree.root, source))?;
            return Ok(CleanupOutcome::Kept);
        }
    };
    if !state.is_clean() {
        writeln!(
            output,
            "Worktree {} has {}.",
            pycompat::repr_str(&worktree.name),
            state.reasons().join(", ")
        )
        .map_err(|source| startup_io(&worktree.root, source))?;
        writeln!(
            output,
            "Remove it and delete its branch? This discards worktree changes, untracked files, and commits."
        )
        .map_err(|source| startup_io(&worktree.root, source))?;
        if !confirm(
            input,
            output,
            "Remove worktree? [y/N] ",
            &["y", "yes", "remove"],
        )? {
            writeln!(output, "Keeping worktree: {}", worktree.root.display())
                .map_err(|source| startup_io(&worktree.root, source))?;
            return Ok(CleanupOutcome::Kept);
        }
    }

    let delete_branch = if worktree.branch_created {
        true
    } else {
        writeln!(
            output,
            "Branch {} existed before this session and was attached, not created by Vibe.",
            pycompat::repr_str(&worktree.branch)
        )
        .map_err(|source| startup_io(&worktree.root, source))?;
        confirm(
            input,
            output,
            &format!(
                "Also delete branch {}? [y/N] ",
                pycompat::repr_str(&worktree.branch)
            ),
            &["y", "yes", "delete"],
        )?
    };

    let managed = ManagedRoot::containing(&worktree.root)
        .and_then(|managed| ManagedWorktree::at(&managed, &worktree.root));
    let holder = cli_worktree_holder();
    let others = managed.as_ref().map_or(0, |managed| {
        managed
            .holders()
            .iter()
            .filter(|held| **held != holder)
            .count()
    });
    if others > 0 {
        writeln!(
            output,
            "Keeping worktree {}: in use by {others} other session(s)",
            worktree.root.display()
        )
        .map_err(|source| startup_io(&worktree.root, source))?;
        return Ok(CleanupOutcome::Kept);
    }

    writeln!(output, "Removing worktree: {}", worktree.root.display())
        .map_err(|source| startup_io(&worktree.root, source))?;
    if let Err(error) = remove_worktree(&worktree, delete_branch) {
        writeln!(output, "Could not remove worktree: {error}")
            .map_err(|source| startup_io(&worktree.root, source))?;
        return Ok(CleanupOutcome::Failed);
    }
    if let Some(managed) = &managed {
        managed.forget();
    }
    writeln!(output, "Removed worktree: {}", worktree.root.display())
        .map_err(|source| startup_io(&worktree.root, source))?;
    if !delete_branch {
        writeln!(output, "Kept branch: {}", worktree.branch)
            .map_err(|source| startup_io(&worktree.root, source))?;
    }
    Ok(CleanupOutcome::Removed)
}

fn confirm(
    input: &mut impl BufRead,
    output: &mut impl Write,
    prompt: &str,
    accepted: &[&str],
) -> Result<bool, StartupError> {
    write!(output, "{prompt}").map_err(|source| startup_io(Path::new("stderr"), source))?;
    output
        .flush()
        .map_err(|source| startup_io(Path::new("stderr"), source))?;
    let mut answer = String::new();
    // The reference catches `EOFError` and `KeyboardInterrupt` around `input()`,
    // writes a bare newline so the cursor leaves the prompt line, and declines.
    // End of file reads zero bytes here and takes the same branch; an interrupt
    // at the question is a signal the process dies on before this returns,
    // which keeps the worktree as well.
    let read = input
        .read_line(&mut answer)
        .map_err(|source| startup_io(Path::new("stdin"), source))?;
    if read == 0 {
        writeln!(output).map_err(|source| startup_io(Path::new("stderr"), source))?;
        return Ok(false);
    }
    Ok(accepted.contains(&answer.trim().to_ascii_lowercase().as_str()))
}

/// The directory `path` names, or `None` when there is none there.
///
/// The operating system's own error is dropped rather than carried: every
/// caller reports the flag it came from and the path the user gave, which is
/// what the reference prints and all a reader needs to fix the invocation.
fn canonical_directory(path: &Path) -> Option<PathBuf> {
    let canonical = vibe_core::platform::canonicalize(path).ok()?;
    canonical.is_dir().then_some(canonical)
}

#[cfg(test)]
mod worktree_tests;
