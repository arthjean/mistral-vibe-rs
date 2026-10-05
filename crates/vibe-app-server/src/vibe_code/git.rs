//! The checkout a Teleport run ships: which GitHub repository it points at,
//! the commit and branch it sits on, what it has not pushed, and its
//! uncommitted changes.
//!
//! Reference `GitRepository` (`vibe/core/teleport/git.py`) with remote
//! discovery from `vibe/core/git/remote.py`. Every command runs through the
//! trusted git executable, in the checkout, under the inherited environment, as
//! GitPython runs them; the fetch is the exception and goes through the hardened
//! policy in [`vibe_core::worktree::fetch`].

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use tokio::process::Command;

/// How long one git command may run. The reference runs its slow commands on
/// an executor bounded the same way (`vibe/core/teleport/git.py:67`).
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// The two hosts the reference's URL parser recognizes as GitHub.
const GITHUB_DOMAINS: [&str; 2] = ["github.com", "gist.github.com"];

/// Reference `ServiceTeleportError` and its one subclass, which telemetry
/// reports by class name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureClass {
    Service,
    NotSupported,
}

impl FailureClass {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Service => "ServiceTeleportError",
            Self::NotSupported => "ServiceTeleportNotSupportedError",
        }
    }
}

/// Why the checkout cannot be read or acted on, as a sentence a client shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitFailure {
    pub(crate) message: String,
    pub(crate) class: FailureClass,
}

impl GitFailure {
    fn service(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            class: FailureClass::Service,
        }
    }

    fn not_supported(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            class: FailureClass::NotSupported,
        }
    }
}

/// Reference `GitRepoInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitRepoInfo {
    pub(crate) remote_name: String,
    pub(crate) remote_url: String,
    pub(crate) repo: String,
    pub(crate) branch: Option<String>,
    pub(crate) commit: String,
    pub(crate) diff: Vec<u8>,
    pub(crate) default_branch: Option<String>,
    pub(crate) repo_root: Option<PathBuf>,
}

/// One checkout, opened from a directory inside it.
pub(crate) struct GitRepository {
    executable: PathBuf,
    /// Where commands run: the working tree, or the git directory of a bare
    /// repository.
    directory: PathBuf,
    working_tree: Option<PathBuf>,
}

impl GitRepository {
    /// Reference `_repo_or_raise`: the repository `workdir` sits in, searched
    /// upward, with the trusted executable resolved first.
    pub(crate) async fn open(workdir: &Path) -> Result<Self, GitFailure> {
        let executable = vibe_core::worktree::trusted_git_executable(workdir).ok_or_else(|| {
            GitFailure::service(
                "No trusted git executable was found. Install git, or set \
                 GIT_PYTHON_GIT_EXECUTABLE to an absolute path.",
            )
        })?;
        let not_a_repository = || {
            GitFailure::not_supported(
                "Teleport works from a git checkout. Run it from inside a repository.",
            )
        };
        if !workdir.is_dir() {
            return Err(not_a_repository());
        }
        let probe = Self {
            executable,
            directory: workdir.to_path_buf(),
            working_tree: None,
        };
        let output = probe
            .run(&["rev-parse", "--is-bare-repository", "--absolute-git-dir"])
            .await
            .ok()
            .filter(|output| output.status.success())
            .ok_or_else(not_a_repository)?;
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        let mut lines = text.lines();
        let bare = lines.next() == Some("true");
        let git_dir = lines
            .next()
            .map(PathBuf::from)
            .ok_or_else(not_a_repository)?;
        if bare {
            return Ok(Self {
                executable: probe.executable,
                directory: git_dir,
                working_tree: None,
            });
        }
        let top = probe
            .stdout(&["rev-parse", "--show-toplevel"])
            .await
            .ok_or_else(not_a_repository)?;
        let working_tree = std::fs::canonicalize(&top).unwrap_or_else(|_| PathBuf::from(top));
        Ok(Self {
            executable: probe.executable,
            directory: working_tree.clone(),
            working_tree: Some(working_tree),
        })
    }

    /// Reference `get_info`: the GitHub remote, the commit, the branch, the
    /// remote's default branch and the working-tree diff, failing in that
    /// order.
    pub(crate) async fn info(&self) -> Result<GitRepoInfo, GitFailure> {
        self.build_info(true).await
    }

    /// Reference `get_metadata`: what [`Self::info`] reads but the
    /// working-tree diff, which a project link never needs.
    pub(crate) async fn metadata(&self) -> Result<GitRepoInfo, GitFailure> {
        self.build_info(false).await
    }

    async fn build_info(&self, include_diff: bool) -> Result<GitRepoInfo, GitFailure> {
        let remote = self.github_remote().await.ok_or_else(|| {
            GitFailure::not_supported(
                "Teleport only works with GitHub repositories, and no remote here points at \
                 GitHub.",
            )
        })?;
        let commit = self
            .stdout(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
            .await
            .filter(|commit| !commit.is_empty())
            .ok_or_else(|| GitFailure::not_supported("The current commit could not be read."))?;
        let branch = self.branch().await;
        let default_branch = self
            .remote_default_branch(&remote.name)
            .await
            .map(|reference| {
                reference
                    .strip_prefix(&format!("{}/", remote.name))
                    .map_or_else(|| reference.clone(), ToOwned::to_owned)
            });
        let diff = if include_diff {
            self.diff().await
        } else {
            Vec::new()
        };
        Ok(GitRepoInfo {
            remote_url: format!("https://github.com/{}/{}.git", remote.owner, remote.repo),
            remote_name: remote.name,
            repo: remote.repo,
            branch,
            commit,
            diff,
            default_branch,
            repo_root: self.working_tree.clone(),
        })
    }

    /// Reference `fetch`: every branch of `remote`, under the hardened policy,
    /// with any refusal or failure swallowed.
    pub(crate) async fn fetch(&self, remote: &str) {
        let Some(working_tree) = self.working_tree.clone() else {
            return;
        };
        let remote = remote.to_owned();
        let _ = tokio::task::spawn_blocking(move || {
            vibe_core::worktree::fetch::fetch_remote_heads(&working_tree, &remote)
        })
        .await;
    }

    /// Reference `is_commit_pushed`: some branch of `remote` contains it.
    pub(crate) async fn is_commit_pushed(&self, commit: &str, remote: &str) -> bool {
        let Some(listing) = self.stdout(&["branch", "-r", "--contains", commit]).await else {
            return false;
        };
        let prefix = format!("{remote}/");
        listing.lines().any(|line| line.trim().starts_with(&prefix))
    }

    /// Reference `is_branch_pushed`: a detached head has nothing to push.
    pub(crate) async fn is_branch_pushed(&self, remote: &str) -> bool {
        match self.branch().await {
            None => true,
            Some(branch) => self.ref_exists(&format!("{remote}/{branch}")).await,
        }
    }

    /// Reference `get_unpushed_commit_count`: against the branch's own
    /// remote-tracking ref, or against the remote's default branch when the
    /// branch was never pushed.
    pub(crate) async fn unpushed_commit_count(&self, remote: &str) -> Result<u64, GitFailure> {
        let branch = self.branch().await.ok_or_else(|| {
            GitFailure::service("Unpushed commits cannot be counted without a current branch.")
        })?;
        self.fetch(remote).await;
        if let Some(count) = self
            .rev_list_count(&format!("{remote}/{branch}..HEAD"))
            .await
        {
            return Ok(count);
        }
        if let Some(default_branch) = self.remote_default_branch(remote).await
            && let Some(count) = self
                .rev_list_count(&format!("{default_branch}..HEAD"))
                .await
        {
            return Ok(count);
        }
        Err(GitFailure::service(format!(
            "The unpushed commits on {branch} could not be counted."
        )))
    }

    /// Reference `push_current_branch`: the current branch to `remote`, setting
    /// its upstream, under the inherited environment.
    pub(crate) async fn push_current_branch(&self, remote: &str) -> bool {
        let Some(branch) = self.branch().await else {
            return false;
        };
        let Ok(output) = self
            .run(&[
                "push",
                "--porcelain",
                "--set-upstream",
                "-v",
                "--",
                remote,
                &branch,
            ])
            .await
        else {
            return false;
        };
        output.status.success()
            && !String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.starts_with('!'))
    }

    /// The checkout's top-level directory, or `None` for a bare repository.
    pub(crate) fn working_tree(&self) -> Option<&Path> {
        self.working_tree.as_deref()
    }

    /// Whether HEAD resolves to a commit (reference `GitRepo.has_commits`).
    pub(crate) async fn has_commits(&self) -> bool {
        self.stdout(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
            .await
            .is_some_and(|commit| !commit.is_empty())
    }

    /// The first GitHub remote as an https clone URL (reference
    /// `find_github_remote` and `to_https_url`).
    pub(crate) async fn github_remote_url(&self) -> Option<String> {
        self.github_remote()
            .await
            .map(|remote| format!("https://github.com/{}/{}.git", remote.owner, remote.repo))
    }

    /// The branch HEAD is on, or `None` when it is detached.
    pub(crate) async fn branch(&self) -> Option<String> {
        self.stdout(&["symbolic-ref", "--quiet", "--short", "HEAD"])
            .await
            .filter(|branch| !branch.is_empty())
    }

    /// Reference `_get_remote_default_branch`: `<remote>/HEAD`'s target, as
    /// `<remote>/<branch>`, when that ref exists.
    async fn remote_default_branch(&self, remote: &str) -> Option<String> {
        let target = self
            .stdout(&[
                "symbolic-ref",
                "--quiet",
                &format!("refs/remotes/{remote}/HEAD"),
            ])
            .await?;
        let name = target.strip_prefix("refs/remotes/")?.to_owned();
        self.ref_exists(&name).await.then_some(name)
    }

    async fn ref_exists(&self, reference: &str) -> bool {
        self.run(&["rev-parse", "--verify", reference])
            .await
            .is_ok_and(|output| output.status.success())
    }

    async fn rev_list_count(&self, range: &str) -> Option<u64> {
        self.stdout(&["rev-list", "--count", range])
            .await?
            .parse()
            .ok()
    }

    /// Reference `find_github_remote`: remotes in the order the repository's
    /// configuration declares them, each tried by its configured URL and then
    /// by every URL git reports for it.
    async fn github_remote(&self) -> Option<GitHubRemote> {
        let listing = self
            .stdout(&["config", "--local", "--includes", "--name-only", "--list"])
            .await
            .unwrap_or_default();
        let mut names: Vec<String> = Vec::new();
        for key in listing.lines() {
            let Some(rest) = key.strip_prefix("remote.") else {
                continue;
            };
            let Some((name, _)) = rest.rsplit_once('.') else {
                continue;
            };
            if !names.iter().any(|known| known == name) {
                names.push(name.to_owned());
            }
        }
        for name in names {
            let mut urls: Vec<String> = Vec::new();
            if let Some(configured) = self
                .stdout(&[
                    "config",
                    "--local",
                    "--includes",
                    "--get-all",
                    &format!("remote.{name}.url"),
                ])
                .await
                .and_then(|values| values.lines().last().map(ToOwned::to_owned))
            {
                urls.push(configured);
            }
            if let Some(reported) = self.stdout(&["remote", "get-url", "--all", &name]).await {
                for url in reported.lines() {
                    if !urls.iter().any(|seen| seen == url) {
                        urls.push(url.to_owned());
                    }
                }
            }
            for url in urls {
                if let Some((owner, repo)) = parse_github_url(&url) {
                    return Some(GitHubRemote { name, owner, repo });
                }
            }
        }
        None
    }

    /// Reference `_get_diff`: the working tree against HEAD, untracked files
    /// included, staged on a copy of the index so the real one is untouched,
    /// with the repository's filters and monitors disabled. Any failure is an
    /// empty diff.
    async fn diff(&self) -> Vec<u8> {
        let Ok(scratch) = tempfile_directory() else {
            return Vec::new();
        };
        let diff = self.diff_in(&scratch).await.unwrap_or_default();
        let _ = std::fs::remove_dir_all(&scratch);
        diff
    }

    async fn diff_in(&self, scratch: &Path) -> Option<Vec<u8>> {
        let index = scratch.join("index");
        let reported = self
            .stdout(&["-c", "core.fsmonitor=", "rev-parse", "--git-path", "index"])
            .await?;
        let mut source = PathBuf::from(reported);
        if !source.is_absolute() {
            source = self.directory.join(source);
        }
        if source.exists() {
            std::fs::copy(&source, &index).ok()?;
        }
        let environment = [("GIT_INDEX_FILE", index.as_os_str())];
        let staged = self
            .run_with(&["-c", "core.fsmonitor=", "add", "-N", "."], &environment)
            .await
            .ok()?;
        if !staged.status.success() {
            return None;
        }
        let output = self
            .run_with(
                &[
                    "-c",
                    "core.fsmonitor=",
                    "diff",
                    "--binary",
                    "--no-textconv",
                    "--no-ext-diff",
                    "HEAD",
                ],
                &environment,
            )
            .await
            .ok()?;
        if !output.status.success() {
            return None;
        }
        // GitPython hands back its output without the final newline.
        let mut diff = output.stdout;
        if diff.last() == Some(&b'\n') {
            diff.pop();
        }
        Some(diff)
    }

    /// Git's standard output for a command that succeeded, trimmed.
    async fn stdout(&self, arguments: &[&str]) -> Option<String> {
        let output = self.run(arguments).await.ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    async fn run(&self, arguments: &[&str]) -> std::io::Result<Output> {
        self.run_with(arguments, &[]).await
    }

    async fn run_with(
        &self,
        arguments: &[&str],
        environment: &[(&str, &std::ffi::OsStr)],
    ) -> std::io::Result<Output> {
        let mut command = Command::new(&self.executable);
        command
            .args(arguments)
            .current_dir(&self.directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in environment {
            command.env(key, value);
        }
        match tokio::time::timeout(GIT_TIMEOUT, command.output()).await {
            Ok(output) => output,
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "git took too long",
            )),
        }
    }
}

/// Reference `GitHubRemoteInfo`.
struct GitHubRemote {
    name: String,
    owner: String,
    repo: String,
}

fn tempfile_directory() -> std::io::Result<PathBuf> {
    let directory = std::env::temp_dir().join(format!(
        "vibe-teleport-index-{}",
        vibe_core::session_id::uuid_v4()
    ));
    std::fs::create_dir(&directory)?;
    Ok(directory)
}

/// The three spellings the reference's URL parser (giturlparse's GitHub
/// platform) accepts, tried in its order. Matching ignores case, but the
/// domain has to be one of [`GITHUB_DOMAINS`] exactly.
#[expect(
    clippy::expect_used,
    reason = "the patterns are compile-time constants that compile"
)]
static GITHUB_PATTERNS: LazyLock<[Regex; 3]> = LazyLock::new(|| {
    [
        r"(?i)^(?:git\+)?https://(?:[^/]+?:[^/]+?@)?(?P<domain>[^/]+?)/(?P<owner>[^/]+?)/(?P<repo>[^/]+?)(?:\.git)?/?(?:(?:/blob/|/tree/).+)?$",
        r"(?i)^(?:(?:git\+)?ssh)?(?:://)?git@(?P<domain>.+?)(?::|/)(?P<owner>[^/]+)/(?P<repo>[^/]+?)(?:\.git)?/?(?:(?:/blob/|/tree/).+)?$",
        r"(?i)^git://(?P<domain>.+?)/(?P<owner>[^/]+)/(?P<repo>[^/]+?)(?:\.git)?/?(?:(?:/blob/|/tree/).+)?$",
    ]
    .map(|pattern| Regex::new(pattern).expect("the GitHub URL pattern compiles"))
});

/// Reference `parse_github_url`: the owner and repository a GitHub remote URL
/// names, in their written case.
pub(crate) fn parse_github_url(url: &str) -> Option<(String, String)> {
    GITHUB_PATTERNS.iter().find_map(|pattern| {
        let captures = pattern.captures(url)?;
        let domain = captures.name("domain")?.as_str();
        if !GITHUB_DOMAINS.contains(&domain) {
            return None;
        }
        let owner = captures.name("owner")?.as_str();
        let repo = captures.name("repo")?.as_str();
        (!owner.is_empty() && !repo.is_empty()).then(|| (owner.to_owned(), repo.to_owned()))
    })
}

/// Reference `normalize_repo_url` (`vibe/utils/repository.py`): one spelling
/// for comparing two repository URLs, never shown.
pub(crate) fn normalize_repo_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let mut value = if let Some(path) = trimmed.strip_prefix("git@github.com:") {
        format!("github.com/{path}")
    } else {
        match url_netloc_and_path(trimmed) {
            Some((netloc, path)) if !netloc.is_empty() && !path.is_empty() => {
                format!("{netloc}/{}", path.trim_start_matches('/'))
            }
            _ => trimmed.to_owned(),
        }
    };
    while value.ends_with('/') {
        value.pop();
    }
    match value.strip_suffix(".git") {
        Some(stripped) => stripped.to_lowercase(),
        None => value.to_lowercase(),
    }
}

/// The network location and path `urllib.parse.urlparse` splits out of `url`:
/// both empty unless a scheme is followed by `//`.
fn url_netloc_and_path(url: &str) -> Option<(String, String)> {
    let (scheme, rest) = url.split_once(':')?;
    let valid_scheme = scheme
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "+-.".contains(character));
    if !valid_scheme {
        return None;
    }
    let rest = rest.strip_prefix("//")?;
    let rest = rest
        .split_once(['?', '#'])
        .map_or(rest, |(before, _)| before);
    let (netloc, path) = rest.find('/').map_or((rest, ""), |at| rest.split_at(at));
    Some((netloc.to_owned(), path.to_owned()))
}

#[cfg(test)]
mod git_tests;
