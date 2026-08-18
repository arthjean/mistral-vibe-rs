//! The git working tree a project link and a teleport are decided from.
//!
//! [`GitProbe`] is the whole contract: what a directory's repository looks like,
//! and what pushing it accomplishes. [`CommandGitProbe`] satisfies it by running
//! the system `git` under its own index file and its own timeouts, so a
//! repository the user is also working in is never mutated by an inspection.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use command_group::CommandGroup;
use serde::Serialize;
use url::Url;

use super::cloud::{CloudError, Project, TeleportRepository, TeleportRepositoryDiff};

pub(super) const MAX_GIT_OUTPUT_BYTES: usize = 1024 * 1024;
pub(super) const MAX_TELEPORT_DIFF_ENCODED_BYTES: usize = 1_000_000;
pub(super) const DEFAULT_GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
pub(super) const DEFAULT_GIT_NETWORK_TIMEOUT: Duration = Duration::from_secs(60);
pub(super) static NEXT_GIT_INDEX_FILE: AtomicU64 = AtomicU64::new(1);
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSnapshot {
    pub repository: String,
    pub dirty: bool,
    pub unpushed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitPushStatus {
    pub unpushed_count: u64,
    pub branch_not_pushed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectGitSnapshot {
    pub snapshot: GitSnapshot,
    pub repo_root: String,
    pub remote_name: String,
    pub branch: Option<String>,
}

/// One repository root as the session-less project-link surface publishes it.
///
/// `repo_url` is the sanitized remote the link is keyed against; the two branch
/// names are absent rather than guessed when the probe cannot observe them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectLinkRoot {
    pub repo_local_path: String,
    pub repo_name: String,
    pub repo_url: String,
    pub current_branch: Option<String>,
    pub default_branch: Option<String>,
}

/// Why a path is not an eligible project root, in the reference vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectRootRejection {
    /// The path is not inside a Git repository.
    NotGit,
    /// The repository has no remote the project surface can link against.
    UnsupportedRemote,
    /// The repository is there but its root could not be resolved.
    NestedUnresolvable,
    /// The repository has no commit yet, so there is nothing to link.
    NoCommits,
}

pub trait GitProbe: Send + Sync {
    fn inspect(&self, working_directory: &Path) -> Result<GitSnapshot, CloudError>;

    /// Resolves a path to the repository root the project-link surface keys on.
    ///
    /// The default is built on [`GitProbe::inspect_project`], which every probe
    /// answers: a path that inspects is a repository with a usable remote, and
    /// the branch names the default cannot observe are reported absent rather
    /// than guessed. [`CommandGitProbe`] overrides it with the classification
    /// Git itself reports, which is what turns a failure into one of the four
    /// reject reasons instead of a single opaque one.
    fn resolve_project_root(
        &self,
        working_directory: &Path,
    ) -> Result<ProjectLinkRoot, ProjectRootRejection> {
        let project = self
            .inspect_project(working_directory)
            .map_err(|_| ProjectRootRejection::NotGit)?;
        Ok(ProjectLinkRoot {
            repo_name: repository_name(&project.snapshot.repository),
            repo_local_path: project.repo_root,
            repo_url: project.snapshot.repository,
            current_branch: project.branch,
            default_branch: None,
        })
    }

    fn inspect_project(&self, working_directory: &Path) -> Result<ProjectGitSnapshot, CloudError> {
        let snapshot = self.inspect(working_directory)?;
        let repo_root = canonical_repository_root(working_directory)?;
        Ok(ProjectGitSnapshot {
            snapshot,
            repo_root,
            remote_name: String::new(),
            branch: None,
        })
    }

    fn inspect_for_teleport(
        &self,
        working_directory: &Path,
    ) -> Result<(GitSnapshot, TeleportRepository, GitPushStatus), CloudError> {
        let snapshot = self.inspect(working_directory)?;
        let repository = TeleportRepository {
            repo_url: snapshot.repository.clone(),
            branch: None,
            commit_sha: None,
            diff: None,
        };
        let push_status = GitPushStatus {
            unpushed_count: u64::from(snapshot.unpushed),
            branch_not_pushed: snapshot.unpushed,
        };
        Ok((snapshot, repository, push_status))
    }

    fn push(&self, working_directory: &Path) -> Result<(), CloudError>;
}

pub(super) struct UnavailableGitProbe;

impl GitProbe for UnavailableGitProbe {
    fn inspect(&self, _working_directory: &Path) -> Result<GitSnapshot, CloudError> {
        Err(CloudError::Git(
            "the working directory is not an inspectable Git repository".to_owned(),
        ))
    }

    fn push(&self, _working_directory: &Path) -> Result<(), CloudError> {
        Err(CloudError::Git(
            "no Git push implementation is configured".to_owned(),
        ))
    }
}

pub struct CommandGitProbe {
    git_program: PathBuf,
    command_timeout: Duration,
    network_timeout: Duration,
    runner: Arc<dyn GitCommandRunner>,
}

impl Default for CommandGitProbe {
    fn default() -> Self {
        Self {
            git_program: PathBuf::from("git"),
            command_timeout: DEFAULT_GIT_COMMAND_TIMEOUT,
            network_timeout: DEFAULT_GIT_NETWORK_TIMEOUT,
            runner: Arc::new(SystemGitCommandRunner),
        }
    }
}

impl CommandGitProbe {
    #[must_use]
    pub fn with_timeouts(mut self, command_timeout: Duration, network_timeout: Duration) -> Self {
        self.command_timeout = command_timeout.max(Duration::from_millis(1));
        self.network_timeout = network_timeout.max(Duration::from_millis(1));
        self
    }

    /// The first eligible GitHub remote, preferring `origin`, with its
    /// sanitized URL. `None` when the repository publishes none.
    fn github_remote(&self, working_directory: &Path) -> Option<(String, String)> {
        let remotes = self
            .git_text(
                working_directory,
                &["remote"],
                self.command_timeout,
                "list Git remotes",
            )
            .ok()?;
        let mut candidates = remotes
            .lines()
            .map(str::trim)
            .filter(|remote| !remote.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        candidates.sort_by_key(|remote| (remote != "origin", remote.clone()));
        for remote in candidates {
            let remote_url = match self.git_text_os(
                working_directory,
                &[
                    OsString::from("remote"),
                    OsString::from("get-url"),
                    OsString::from("--"),
                    OsString::from(&remote),
                ],
                self.command_timeout,
                "read the Git remote URL",
            ) {
                Ok(remote_url) => remote_url,
                Err(_) => continue,
            };
            let Ok(repo_url) = sanitize_git_remote(&remote_url) else {
                continue;
            };
            let is_github = Url::parse(&repo_url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .is_some_and(|host| host.eq_ignore_ascii_case("github.com"));
            if is_github {
                return Some((remote, repo_url));
            }
        }
        None
    }

    fn metadata(&self, working_directory: &Path) -> Result<GitMetadata, CloudError> {
        let repo_root = self.git_text(
            working_directory,
            &["rev-parse", "--show-toplevel"],
            self.command_timeout,
            "locate the repository root",
        )?;
        let repo_root = fs::canonicalize(repo_root.trim())
            .map_err(|_| CloudError::Git("Git returned an invalid repository root".to_owned()))?;
        let (remote, repo_url) = self.github_remote(working_directory).ok_or_else(|| {
            CloudError::Git("Teleport requires a GitHub remote; configure one and retry".to_owned())
        })?;
        let branch = self.git_text(
            working_directory,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
            self.command_timeout,
            "read the current branch",
        )?;
        let branch = branch.trim().to_owned();
        if branch.is_empty() {
            return Err(CloudError::Git(
                "Teleport requires a checked-out branch; switch branches and retry".to_owned(),
            ));
        }
        let commit_sha = self.git_text(
            working_directory,
            &["rev-parse", "--verify", "HEAD"],
            self.command_timeout,
            "read the current commit",
        )?;
        let commit_sha = commit_sha.trim().to_owned();
        if !(7..=64).contains(&commit_sha.len())
            || !commit_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(CloudError::Git(
                "Git returned an invalid current commit".to_owned(),
            ));
        }
        Ok(GitMetadata {
            repo_root,
            remote,
            repo_url,
            branch,
            commit_sha,
        })
    }

    fn inspection(
        &self,
        working_directory: &Path,
    ) -> Result<(GitSnapshot, TeleportRepository, GitPushStatus), CloudError> {
        let metadata = self.metadata(working_directory)?;
        let status = self.git_text(
            working_directory,
            &["status", "--porcelain=v1", "--untracked-files=normal"],
            self.command_timeout,
            "inspect the Git working tree",
        )?;
        let dirty = !status.is_empty();
        let fetch_args = [
            OsString::from("fetch"),
            OsString::from("--quiet"),
            OsString::from("--no-tags"),
            OsString::from("--"),
            OsString::from(&metadata.remote),
        ];
        let _ = self.run_git(
            working_directory,
            &fetch_args,
            self.network_timeout,
            "refresh the Git remote",
        );
        let remote_ref = format!("refs/remotes/{}/{}", metadata.remote, metadata.branch);
        let branch_pushed = self
            .run_git(
                working_directory,
                &[
                    OsString::from("show-ref"),
                    OsString::from("--verify"),
                    OsString::from("--quiet"),
                    OsString::from(&remote_ref),
                ],
                self.command_timeout,
                "check the remote branch",
            )?
            .status
            .success();
        let revision_range = if branch_pushed {
            format!("{remote_ref}..HEAD")
        } else {
            "HEAD".to_owned()
        };
        let unpushed_count = self
            .git_text_os(
                working_directory,
                &[
                    OsString::from("rev-list"),
                    OsString::from("--count"),
                    OsString::from(revision_range),
                ],
                self.command_timeout,
                "count unpushed commits",
            )?
            .trim()
            .parse::<u64>()
            .map_err(|_| {
                CloudError::Git("Git returned an invalid unpushed commit count".to_owned())
            })?;
        let diff = self.working_tree_diff(&metadata.repo_root, dirty)?;
        let branch_not_pushed = !branch_pushed;
        let unpushed = branch_not_pushed || unpushed_count > 0;
        Ok((
            GitSnapshot {
                repository: metadata.repo_url.clone(),
                dirty,
                unpushed,
            },
            TeleportRepository {
                repo_url: metadata.repo_url,
                branch: Some(metadata.branch),
                commit_sha: Some(metadata.commit_sha),
                diff,
            },
            GitPushStatus {
                unpushed_count,
                branch_not_pushed,
            },
        ))
    }

    fn working_tree_diff(
        &self,
        working_directory: &Path,
        dirty: bool,
    ) -> Result<Option<TeleportRepositoryDiff>, CloudError> {
        if !dirty {
            return Ok(None);
        }
        let git_directory = self.git_text(
            working_directory,
            &["rev-parse", "--absolute-git-dir"],
            self.command_timeout,
            "locate Git metadata",
        )?;
        let git_directory = PathBuf::from(git_directory.trim());
        if !git_directory.is_absolute() || !git_directory.is_dir() {
            return Err(CloudError::Git(
                "Git returned an invalid metadata directory".to_owned(),
            ));
        }
        let sequence = NEXT_GIT_INDEX_FILE.fetch_add(1, Ordering::Relaxed);
        let temporary_index = git_directory.join(format!(
            ".vibe-teleport-index-{}-{sequence}",
            std::process::id()
        ));
        let environment = [(
            OsString::from("GIT_INDEX_FILE"),
            temporary_index.as_os_str().to_owned(),
        )];
        let result = (|| {
            for (args, action) in [
                (
                    vec![OsString::from("read-tree"), OsString::from("HEAD")],
                    "initialize the Teleport diff index",
                ),
                (
                    vec![
                        OsString::from("add"),
                        OsString::from("-A"),
                        OsString::from("--"),
                        OsString::from("."),
                    ],
                    "stage working-tree changes for Teleport",
                ),
            ] {
                let result = self.run_git_with_environment(
                    working_directory,
                    &args,
                    &environment,
                    self.command_timeout,
                    action,
                )?;
                if !result.status.success() {
                    return Err(CloudError::Git(format!(
                        "failed to {action}; the real Git index was not changed"
                    )));
                }
            }
            let diff = self.run_git_with_environment(
                working_directory,
                &[
                    OsString::from("diff"),
                    OsString::from("--cached"),
                    OsString::from("--binary"),
                    OsString::from("--no-ext-diff"),
                    OsString::from("HEAD"),
                    OsString::from("--"),
                ],
                &environment,
                self.command_timeout,
                "capture the working-tree diff",
            )?;
            if !diff.status.success() {
                return Err(CloudError::Git(
                    "failed to capture the working-tree diff; local state is unchanged".to_owned(),
                ));
            }
            if diff.stdout_truncated {
                return Err(CloudError::Git(
                    "working-tree diff exceeded the local Git output safety limit".to_owned(),
                ));
            }
            if diff.stdout.is_empty() {
                return Err(CloudError::Git(
                    "Git reported dirty files but produced no transferable diff".to_owned(),
                ));
            }
            encode_working_tree_diff(&diff.stdout)
        })();
        let _ = fs::remove_file(&temporary_index);
        result.map(Some)
    }

    fn git_text(
        &self,
        working_directory: &Path,
        args: &[&str],
        timeout: Duration,
        action: &str,
    ) -> Result<String, CloudError> {
        let args = args.iter().map(OsString::from).collect::<Vec<_>>();
        self.git_text_os(working_directory, &args, timeout, action)
    }

    fn git_text_os(
        &self,
        working_directory: &Path,
        args: &[OsString],
        timeout: Duration,
        action: &str,
    ) -> Result<String, CloudError> {
        let result = self.run_git(working_directory, args, timeout, action)?;
        if !result.status.success() {
            return Err(CloudError::Git(format!(
                "failed to {action}; verify the repository and Git credentials"
            )));
        }
        if result.stdout_truncated {
            return Err(CloudError::Git(format!(
                "failed to {action}: Git output exceeded the safety limit"
            )));
        }
        String::from_utf8(result.stdout)
            .map_err(|_| CloudError::Git(format!("failed to {action}: Git output was not UTF-8")))
    }

    fn run_git(
        &self,
        working_directory: &Path,
        args: &[OsString],
        timeout: Duration,
        action: &str,
    ) -> Result<GitCommandResult, CloudError> {
        self.run_git_with_environment(working_directory, args, &[], timeout, action)
    }

    fn run_git_with_environment(
        &self,
        working_directory: &Path,
        args: &[OsString],
        environment: &[(OsString, OsString)],
        timeout: Duration,
        action: &str,
    ) -> Result<GitCommandResult, CloudError> {
        self.runner
            .run(
                &self.git_program,
                working_directory,
                args,
                environment,
                timeout,
                MAX_GIT_OUTPUT_BYTES,
            )
            .map_err(|error| match error {
                GitCommandError::Timeout => CloudError::Git(format!(
                    "timed out while trying to {action}; local state is unchanged"
                )),
                GitCommandError::Io => CloudError::Git(format!(
                    "could not run Git to {action}; install Git and retry"
                )),
            })
    }
}

pub(super) fn encode_working_tree_diff(diff: &[u8]) -> Result<TeleportRepositoryDiff, CloudError> {
    let compressed = zstd::stream::encode_all(diff, 3)
        .map_err(|_| CloudError::Git("working-tree diff compression failed".to_owned()))?;
    let content = BASE64_STANDARD.encode(compressed);
    if content.len() > MAX_TELEPORT_DIFF_ENCODED_BYTES {
        return Err(CloudError::Git(format!(
            "working-tree diff exceeded the {MAX_TELEPORT_DIFF_ENCODED_BYTES} byte Teleport limit"
        )));
    }
    Ok(TeleportRepositoryDiff {
        format: "git-diff",
        encoding: "base64",
        compression: "zstd",
        content,
    })
}

impl GitProbe for CommandGitProbe {
    fn inspect(&self, working_directory: &Path) -> Result<GitSnapshot, CloudError> {
        self.inspection(working_directory)
            .map(|(snapshot, _, _)| snapshot)
    }

    fn inspect_project(&self, working_directory: &Path) -> Result<ProjectGitSnapshot, CloudError> {
        let metadata = self.metadata(working_directory)?;
        Ok(ProjectGitSnapshot {
            snapshot: GitSnapshot {
                repository: metadata.repo_url,
                dirty: false,
                unpushed: false,
            },
            repo_root: metadata.repo_root.to_string_lossy().into_owned(),
            remote_name: metadata.remote,
            branch: Some(metadata.branch),
        })
    }

    /// Classifies a candidate root the way the reference does, from what Git
    /// reports rather than from a single failure message.
    ///
    /// The remote is checked before the commit, so a repository with a usable
    /// GitHub remote and no commit yet is reported as `no_commits` rather than
    /// as an unsupported remote. Neither the fetch nor the working-tree scan
    /// `inspect` runs is needed here, which is why this does not go through it.
    fn resolve_project_root(
        &self,
        working_directory: &Path,
    ) -> Result<ProjectLinkRoot, ProjectRootRejection> {
        let repo_root = self
            .git_text(
                working_directory,
                &["rev-parse", "--show-toplevel"],
                self.command_timeout,
                "locate the repository root",
            )
            .map_err(|_| ProjectRootRejection::NotGit)?;
        let repo_root = fs::canonicalize(repo_root.trim())
            .map_err(|_| ProjectRootRejection::NestedUnresolvable)?;
        let (remote, repo_url) = self
            .github_remote(working_directory)
            .ok_or(ProjectRootRejection::UnsupportedRemote)?;
        let commit = self
            .git_text(
                working_directory,
                &["rev-parse", "--verify", "HEAD"],
                self.command_timeout,
                "read the current commit",
            )
            .map_err(|_| ProjectRootRejection::NoCommits)?;
        let commit = commit.trim();
        if !(7..=64).contains(&commit.len()) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ProjectRootRejection::NoCommits);
        }
        let current_branch = self
            .git_text(
                working_directory,
                &["symbolic-ref", "--quiet", "--short", "HEAD"],
                self.command_timeout,
                "read the current branch",
            )
            .ok()
            .map(|branch| branch.trim().to_owned())
            .filter(|branch| !branch.is_empty());
        // `refs/remotes/<remote>/HEAD` is what the reference reads the default
        // branch from, and it resolves to `<remote>/<branch>`.
        let default_branch = self
            .git_text(
                working_directory,
                &[
                    "symbolic-ref",
                    "--quiet",
                    "--short",
                    &format!("refs/remotes/{remote}/HEAD"),
                ],
                self.command_timeout,
                "read the remote default branch",
            )
            .ok()
            .map(|branch| branch.trim().to_owned())
            .map(|branch| {
                branch
                    .strip_prefix(&format!("{remote}/"))
                    .unwrap_or(&branch)
                    .to_owned()
            })
            .filter(|branch| !branch.is_empty());
        Ok(ProjectLinkRoot {
            repo_local_path: repo_root.to_string_lossy().into_owned(),
            repo_name: repository_name(&repo_url),
            repo_url,
            current_branch,
            default_branch,
        })
    }

    fn inspect_for_teleport(
        &self,
        working_directory: &Path,
    ) -> Result<(GitSnapshot, TeleportRepository, GitPushStatus), CloudError> {
        self.inspection(working_directory)
    }

    fn push(&self, working_directory: &Path) -> Result<(), CloudError> {
        let metadata = self.metadata(working_directory)?;
        let result = self.run_git(
            working_directory,
            &[
                OsString::from("push"),
                OsString::from("--set-upstream"),
                OsString::from("--"),
                OsString::from(metadata.remote),
                OsString::from(metadata.branch),
            ],
            self.network_timeout,
            "push the current branch",
        )?;
        if !result.status.success() {
            return Err(CloudError::Git(
                "Git push failed; verify remote access and push the branch manually".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(super) struct GitMetadata {
    repo_root: PathBuf,
    remote: String,
    repo_url: String,
    branch: String,
    commit_sha: String,
}

#[derive(Debug)]
pub(super) struct GitCommandResult {
    status: ExitStatus,
    stdout: Vec<u8>,
    stdout_truncated: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum GitCommandError {
    Timeout,
    Io,
}

pub(super) trait GitCommandRunner: Send + Sync {
    fn run(
        &self,
        program: &Path,
        working_directory: &Path,
        args: &[OsString],
        environment: &[(OsString, OsString)],
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Result<GitCommandResult, GitCommandError>;
}

pub(super) struct SystemGitCommandRunner;

impl GitCommandRunner for SystemGitCommandRunner {
    fn run(
        &self,
        program: &Path,
        working_directory: &Path,
        args: &[OsString],
        environment: &[(OsString, OsString)],
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Result<GitCommandResult, GitCommandError> {
        let mut command = Command::new(program);
        command
            .arg("-C")
            .arg(working_directory)
            .args(args)
            .envs(environment.iter().cloned())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "Never")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.group_spawn().map_err(|_| GitCommandError::Io)?;
        let stdout = child.inner().stdout.take().ok_or(GitCommandError::Io)?;
        let stderr = child.inner().stderr.take().ok_or(GitCommandError::Io)?;
        let stdout_reader = thread::spawn(move || drain_process_output(stdout, max_output_bytes));
        let stderr_reader = thread::spawn(move || drain_process_output(stderr, max_output_bytes));
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait().map_err(|_| GitCommandError::Io)? {
                Some(status) => break status,
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return Err(GitCommandError::Timeout);
                }
                None => thread::sleep(Duration::from_millis(10)),
            }
        };
        let (stdout, stdout_truncated) = stdout_reader
            .join()
            .map_err(|_| GitCommandError::Io)?
            .map_err(|_| GitCommandError::Io)?;
        let _stderr = stderr_reader
            .join()
            .map_err(|_| GitCommandError::Io)?
            .map_err(|_| GitCommandError::Io)?;
        Ok(GitCommandResult {
            status,
            stdout,
            stdout_truncated,
        })
    }
}

pub(super) fn drain_process_output(
    mut output: impl Read,
    max_output_bytes: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut retained = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = output.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = max_output_bytes.saturating_sub(retained.len());
        let keep = remaining.min(read);
        retained.extend_from_slice(&buffer[..keep]);
        truncated |= keep < read;
    }
    Ok((retained, truncated))
}

pub(super) fn canonical_repository_root(path: &Path) -> Result<String, CloudError> {
    if let Ok(path) = fs::canonicalize(path) {
        return Ok(path.to_string_lossy().into_owned());
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| CloudError::Git("working directory could not be resolved".to_owned()))?
            .join(path)
    };
    Ok(absolute.to_string_lossy().into_owned())
}

pub(super) fn normalize_repo_url(value: &str) -> String {
    let sanitized = sanitize_git_remote(value).unwrap_or_else(|_| value.trim().to_owned());
    let mut normalized = if let Ok(url) = Url::parse(&sanitized) {
        match (url.host_str(), url.path().trim_matches('/')) {
            (Some(host), path) if !path.is_empty() => format!("{host}/{path}"),
            _ => sanitized,
        }
    } else {
        sanitized
    };
    normalized = normalized.trim().trim_end_matches('/').to_ascii_lowercase();
    normalized
        .strip_suffix(".git")
        .unwrap_or(&normalized)
        .to_owned()
}

/// The repository's own name, taken from the remote rather than from the
/// checkout directory, which is what the reference publishes as `repoName`.
///
/// [`normalize_repo_url`] is not reused here: it lowercases, and a repository
/// name is displayed as its owner spelled it.
pub(super) fn repository_name(repo_url: &str) -> String {
    let sanitized = sanitize_git_remote(repo_url).unwrap_or_else(|_| repo_url.trim().to_owned());
    let path = Url::parse(&sanitized).map_or(sanitized.clone(), |url| {
        url.path().trim_matches('/').to_owned()
    });
    let last = path.rsplit('/').next().unwrap_or_default().trim();
    last.strip_suffix(".git").unwrap_or(last).to_owned()
}

pub(super) fn is_project_linked_to_repo(project: &Project, repo_url: &str) -> bool {
    let normalized_repo_url = normalize_repo_url(repo_url);
    project
        .repositories
        .iter()
        .any(|repository| normalize_repo_url(&repository.repo_url) == normalized_repo_url)
}

pub(super) fn project_is_selectable(project: &Project, repo_url: &str) -> bool {
    !project.is_read_only && is_project_linked_to_repo(project, repo_url)
}

pub(super) fn suggested_project_name(git: &ProjectGitSnapshot) -> String {
    let root_name = Path::new(&git.repo_root)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty());
    if let Some(root_name) = root_name {
        return root_name.to_owned();
    }
    normalize_repo_url(&git.snapshot.repository)
        .rsplit('/')
        .next()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("vibe-project")
        .to_owned()
}

pub(super) fn sanitize_git_remote(raw: &str) -> Result<String, CloudError> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(CloudError::Git(
            "Git remote URL is empty; configure a fetchable remote".to_owned(),
        ));
    }
    let windows_drive = value.as_bytes().get(1) == Some(&b':')
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic);
    if windows_drive
        || value.starts_with('/')
        || value.starts_with('\\')
        || value.starts_with("./")
        || value.starts_with("../")
    {
        return Err(CloudError::Git(
            "Teleport requires a network Git remote; local paths are not allowed".to_owned(),
        ));
    }
    if let Some((authority, path)) = value.split_once(':')
        && !value.contains("://")
        && !authority.contains('/')
        && !authority.eq_ignore_ascii_case("http")
        && !authority.eq_ignore_ascii_case("https")
    {
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        if host.is_empty()
            || path.is_empty()
            || path.starts_with('/')
            || path.starts_with('\\')
            || host.chars().any(char::is_whitespace)
            || path.chars().any(char::is_control)
        {
            return Err(CloudError::Git(
                "Git remote URL is invalid; configure a fetchable remote".to_owned(),
            ));
        }
        return Ok(format!("https://{host}/{}", path.trim_start_matches('/')));
    }
    let mut url = Url::parse(value).map_err(|_| {
        CloudError::Git("Git remote URL is invalid; configure a fetchable remote".to_owned())
    })?;
    if !matches!(url.scheme(), "http" | "https" | "ssh" | "git") {
        return Err(CloudError::Git(
            "Teleport requires a network Git remote".to_owned(),
        ));
    }
    if url.host_str().is_none()
        || url.path().trim_matches('/').is_empty()
        || url.path().contains('\\')
    {
        return Err(CloudError::Git(
            "Git remote URL is invalid; configure a fetchable remote".to_owned(),
        ));
    }
    if matches!(url.scheme(), "ssh" | "git") {
        let host = url.host_str().unwrap_or_default();
        return Ok(format!("https://{host}{}", url.path()));
    }
    url.set_username("")
        .map_err(|_| CloudError::Git("Git remote URL could not be sanitized".to_owned()))?;
    url.set_password(None)
        .map_err(|_| CloudError::Git("Git remote URL could not be sanitized".to_owned()))?;
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

#[cfg(test)]
pub(super) mod git_tests;
