//! The repository snapshot the system prompt carries.
//!
//! Reference `ProjectContextProvider` (`vibe/core/system_prompt.py:37-154`)
//! asks git three metadata questions in parallel (the current branch, the
//! remote branches, the recent commits) and never runs `git status`: status
//! can push working-tree contents through clean and process filters the
//! repository configures, and this runs before any trust prompt, outside the
//! shell permission boundary. Every call carries `-c core.fsmonitor=` and
//! `--no-optional-locks` for the same reason, and the executable is the
//! trusted one worktree operations resolve, never one the project ships.
//!
//! The answer is cached per resolved root for the life of the process, as the
//! reference's module-level cache is: a session reads the repository as it was
//! when the first prompt of that root was built, errors included.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// The ceiling reference `_fetch_git_status` puts on every call, whatever
/// `project_context.timeout_seconds` says.
const MAX_TIMEOUT_SECONDS: f64 = 10.0;

/// How often a running git call is polled for completion.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// The two `project_context` keys. Reference `ProjectContextConfig`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProjectContextSettings {
    pub default_commit_count: i64,
    pub timeout_seconds: f64,
}

impl Default for ProjectContextSettings {
    fn default() -> Self {
        Self {
            default_commit_count: 5,
            timeout_seconds: 2.0,
        }
    }
}

impl ProjectContextSettings {
    /// Reads the `project_context` table of a merged document, keeping the
    /// default for a key that is absent or of the wrong type.
    #[must_use]
    pub fn from_table(table: Option<&toml::Table>) -> Self {
        let defaults = Self::default();
        let Some(table) = table else {
            return defaults;
        };
        Self {
            default_commit_count: table
                .get("default_commit_count")
                .and_then(toml::Value::as_integer)
                .unwrap_or(defaults.default_commit_count),
            timeout_seconds: table
                .get("timeout_seconds")
                .and_then(|value| {
                    value
                        .as_float()
                        .or_else(|| value.as_integer().map(|integer| integer as f64))
                })
                .unwrap_or(defaults.timeout_seconds),
        }
    }
}

/// What git answered about a repository, or why it answered nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitContext {
    /// The three questions were answered.
    Repository {
        /// `git branch --show-current`, empty on a detached head.
        current_branch: String,
        /// `master` when a remote-tracking `origin/master` exists, `main`
        /// otherwise.
        main_branch: String,
        /// `git log --oneline --decorate`, one line per commit, with a
        /// trailing parenthetical dropped from each subject.
        recent_commits: Vec<String>,
    },
    /// A call ran past its timeout.
    TimedOut,
    /// Git refused: not a repository, no commit yet, or a git that failed.
    Unavailable,
    /// Git could not be run at all, typically because no trusted executable
    /// exists. The message is this port's own.
    Failed(String),
}

impl GitContext {
    /// The text the project context template receives as `$git_status`.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Repository {
                current_branch,
                main_branch,
                recent_commits,
            } => {
                let mut lines = vec![
                    format!("Checked-out branch: {current_branch}"),
                    format!("Default branch (pull requests usually target it): {main_branch}"),
                ];
                if !recent_commits.is_empty() {
                    lines.push("Latest commits:".to_owned());
                    lines.extend(recent_commits.iter().cloned());
                }
                lines.join("\n")
            }
            Self::TimedOut => "Git took too long to answer (a large repository?)".to_owned(),
            Self::Unavailable => "Not inside a Git repository, or Git is unavailable".to_owned(),
            Self::Failed(reason) => format!("Could not read the Git state: {reason}"),
        }
    }
}

/// The repository snapshot for `root`, computed once per resolved root.
#[must_use]
pub fn git_context(root: &Path, settings: ProjectContextSettings) -> GitContext {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, GitContext>>> = OnceLock::new();
    let root = crate::config::harness::resolve_lenient(root);
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(known) = cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&root)
    {
        return known.clone();
    }
    let answer = fetch_git_context(&root, settings);
    cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(root)
        .or_insert(answer)
        .clone()
}

/// How one git call ended.
enum CallOutcome {
    Success(String),
    /// Git exited with a failure status.
    Refused,
    TimedOut,
    /// Git could not be started or read.
    Broken(String),
}

/// Reference `_fetch_git_status`, uncached.
///
/// The calls run side by side and are all awaited before any is read, so a
/// slow remote listing costs no more than the slowest call. Their outcomes are
/// then read in the reference's order: the branch decides first, the remote
/// listing is optional, and the log decides last.
fn fetch_git_context(root: &Path, settings: ProjectContextSettings) -> GitContext {
    let Ok(git) = crate::worktree::git::git_executable(root) else {
        return GitContext::Failed("no trusted Git executable was found".to_owned());
    };
    let timeout =
        Duration::try_from_secs_f64(settings.timeout_seconds.clamp(0.0, MAX_TIMEOUT_SECONDS))
            .unwrap_or(Duration::ZERO);
    let log_count = format!("-{}", settings.default_commit_count);
    let (branch, remote, log) = std::thread::scope(|scope| {
        let branch = scope.spawn(|| run_git(&git, root, &["branch", "--show-current"], timeout));
        let remote = scope.spawn(|| run_git(&git, root, &["branch", "-r"], timeout));
        let log = scope.spawn(|| {
            run_git(
                &git,
                root,
                &["log", "--oneline", log_count.as_str(), "--decorate"],
                timeout,
            )
        });
        (
            branch
                .join()
                .unwrap_or_else(|_| CallOutcome::Broken(String::new())),
            remote
                .join()
                .unwrap_or_else(|_| CallOutcome::Broken(String::new())),
            log.join()
                .unwrap_or_else(|_| CallOutcome::Broken(String::new())),
        )
    });
    let current_branch = match branch {
        CallOutcome::Success(output) => crate::text::python_strip(&output).to_owned(),
        other => return failure(other),
    };
    let main_branch = match remote {
        CallOutcome::Success(output) if output.contains("origin/master") => "master",
        CallOutcome::Broken(reason) => return GitContext::Failed(reason),
        _ => "main",
    }
    .to_owned();
    let recent_commits = match log {
        CallOutcome::Success(output) => parse_git_log(crate::text::python_strip(&output)),
        other => return failure(other),
    };
    GitContext::Repository {
        current_branch,
        main_branch,
        recent_commits,
    }
}

fn failure(outcome: CallOutcome) -> GitContext {
    match outcome {
        CallOutcome::TimedOut => GitContext::TimedOut,
        CallOutcome::Broken(reason) => GitContext::Failed(reason),
        CallOutcome::Refused | CallOutcome::Success(_) => GitContext::Unavailable,
    }
}

/// Reference `_parse_git_log`: every non-blank line, stripped, with the
/// subject cut before its last `(` when the subject also holds a `)` and the
/// `(` is not its first character.
///
/// `--decorate` prints the ref names before the subject, so they sit at index
/// zero and survive; what the rule removes is a trailing parenthetical such as
/// a pull request number.
#[must_use]
pub fn parse_git_log(output: &str) -> Vec<String> {
    let mut commits = Vec::new();
    for line in output.split('\n') {
        let line = crate::text::python_strip(line);
        if line.is_empty() {
            continue;
        }
        match line.split_once(' ') {
            Some((hash, subject)) => {
                let subject = match subject.rfind('(') {
                    Some(index) if index > 0 && subject.contains(')') => {
                        crate::text::python_strip(&subject[..index])
                    }
                    _ => subject,
                };
                commits.push(format!("{hash} {subject}"));
            }
            None => commits.push(line.to_owned()),
        }
    }
    commits
}

/// Runs one metadata call with the two options that keep a repository from
/// running a command of its choosing, and waits for it up to `timeout`.
fn run_git(git: &Path, root: &Path, arguments: &[&str], timeout: Duration) -> CallOutcome {
    let spawned = Command::new(git)
        .args(["-c", "core.fsmonitor=", "--no-optional-locks"])
        .args(arguments)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => return CallOutcome::Broken(error.to_string()),
    };
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        return CallOutcome::Broken("git produced no output pipe".to_owned());
    };
    // Read on a thread of its own so a large log cannot fill the pipe and
    // stall the child while this one waits for it to exit.
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return CallOutcome::TimedOut;
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(error) => {
                let _ = child.kill();
                return CallOutcome::Broken(error.to_string());
            }
        }
    };
    let bytes = reader.join().unwrap_or_default();
    if status.success() {
        // Text mode in the reference: undecodable bytes are replaced and every
        // line ending is read as `\n`.
        let text = String::from_utf8_lossy(&bytes)
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        CallOutcome::Success(text)
    } else {
        CallOutcome::Refused
    }
}
