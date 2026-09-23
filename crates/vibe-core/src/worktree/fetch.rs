//! Fetching a remote of a repository nobody vouched for.
//!
//! A new worktree branch starts from the remote's default branch, refreshed
//! first, and the refresh runs inside a checkout whose configuration is the
//! repository's to write. Git treats several configuration values as commands,
//! so a plain `git fetch` there would run whatever the repository names. The
//! reference closes that with `prepare_secure_fetch`
//! (`vibe/core/git/fetch.py:41-106`), and this is the same policy:
//!
//! - the remote URL is read once, validated as explicit HTTPS or SSH (or a local
//!   path when the caller opts in), and handed to git directly, so no rewrite
//!   rule or remote helper the repository configures can reinterpret it;
//! - configuration is read once with its scopes, repository-scoped HTTP settings
//!   and URL rewrites that would apply are refused, and only credential, HTTP,
//!   `safe.directory` and rewrite keys from the system and global scopes are
//!   replayed;
//! - every executable setting is overridden at command scope, the global and
//!   system files are cut off, and `PATH`, the SSH client and git's own helper
//!   directory are pinned to locations outside the checkout.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use regex::Regex;

use super::WorktreeError;
use super::git::{GitRepo, NO_HOOKS_PATH, search_trusted_path};

/// How long a refresh may take before it is abandoned, which the reference
/// bounds the same way (`vibe/core/git/repo.py:29,440-456`).
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the one-shot configuration reads may take
/// (`vibe/core/git/fetch.py:271-280,383-393`).
const CONFIG_TIMEOUT: Duration = Duration::from_secs(5);

const TRUSTED_FETCH_SCOPES: [&str; 2] = ["system", "global"];

#[expect(
    clippy::expect_used,
    reason = "the patterns are compile-time constants that compile"
)]
static SCP_SSH_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:(?P<user>[^/@:\s]+)@)?(?P<host>[^/:\s]+):(?P<path>[^:].*)$")
        .expect("the scp pattern compiles")
});

#[expect(
    clippy::expect_used,
    reason = "the patterns are compile-time constants that compile"
)]
static URL_REWRITE_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^url\..+\.insteadof$").expect("the rewrite pattern compiles")
});

#[expect(
    clippy::expect_used,
    reason = "the patterns are compile-time constants that compile"
)]
static INVALID_PERCENT_ESCAPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"%([^0-9A-Fa-f]|[0-9A-Fa-f][^0-9A-Fa-f]|[0-9A-Fa-f]?$)")
        .expect("the escape pattern compiles")
});

/// The helper directory each trusted git reports, asked once per executable
/// (`vibe/core/git/fetch.py:377-398`).
static EXEC_PATHS: LazyLock<Mutex<BTreeMap<PathBuf, String>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Why a fetch was refused before git ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnsafeFetch(pub(crate) String);

impl std::fmt::Display for UnsafeFetch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

fn unsafe_fetch(message: &str) -> UnsafeFetch {
    UnsafeFetch(message.to_owned())
}

/// A validated URL and the environment git fetches it under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SecureFetch {
    pub(crate) url: String,
    pub(crate) env: Vec<(String, String)>,
}

/// Refreshes `remote/branch` into its remote-tracking ref under the policy.
///
/// An explicit destination refspec is used, so the caller sees the fetched tip
/// without trusting the repository's `remote.<name>.fetch`
/// (`vibe/core/git/repo.py:425-458`).
pub(crate) fn fetch_branch(
    repo: &GitRepo,
    remote: &str,
    branch: &str,
) -> Result<(), WorktreeError> {
    let secure = prepare_secure_fetch(repo, remote, true).map_err(|error| {
        WorktreeError::git(format!("failed to fetch {remote}/{branch}: {error}"))
    })?;
    let refspec = format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}");
    let mut command = Command::new(repo.executable());
    command
        .arg("-C")
        .arg(repo.working_dir())
        .args(["fetch", "--no-recurse-submodules", "--no-auto-maintenance"])
        .arg(&secure.url)
        .arg(&refspec)
        .envs(secure.env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let (status, stderr) =
        run_bounded(command, FETCH_TIMEOUT).map_err(|failure| match failure {
            Bounded::TimedOut => WorktreeError::git(format!(
                "timed out fetching {remote}/{branch} after {}s",
                FETCH_TIMEOUT.as_secs()
            )),
            Bounded::Spawn(error) => {
                WorktreeError::git(format!("failed to fetch {remote}/{branch}: {error}"))
            }
        })?;
    if status {
        Ok(())
    } else {
        Err(WorktreeError::git(format!(
            "failed to fetch {remote}/{branch}: {}",
            stderr.trim()
        )))
    }
}

enum Bounded {
    TimedOut,
    Spawn(std::io::Error),
}

/// Runs `command` to completion or kills it at `limit`, draining stderr on a
/// thread so a chatty fetch cannot block on a full pipe.
fn run_bounded(mut command: Command, limit: Duration) -> Result<(bool, String), Bounded> {
    let mut child = command.spawn().map_err(Bounded::Spawn)?;
    let reader = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        })
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(reader) = reader {
                    let _ = reader.join();
                }
                return Err(Bounded::TimedOut);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(Bounded::Spawn(error)),
        }
    };
    let stderr = reader
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    Ok((status.success(), stderr))
}

/// Resolves `remote` without letting repository configuration execute code
/// (`vibe/core/git/fetch.py:41-106`).
pub(crate) fn prepare_secure_fetch(
    repo: &GitRepo,
    remote: &str,
    allow_file: bool,
) -> Result<SecureFetch, UnsafeFetch> {
    let configured = remote_url(repo, remote)?;
    let (protocol, url) = validated_fetch_url(&configured, allow_file)?;
    let trusted = inspect_fetch_config(repo, &url, protocol != "file")?;

    let mut config: Vec<(String, String)> = vec![("credential.helper".into(), String::new())];
    config.extend(trusted);
    config.extend(
        [
            ("core.askPass", ""),
            ("core.fsmonitor", ""),
            ("core.gitProxy", ""),
            ("core.hooksPath", NO_HOOKS_PATH),
            ("core.sshCommand", ""),
            ("fetch.recurseSubmodules", "false"),
            ("submodule.recurse", "false"),
            ("protocol.allow", "never"),
            ("protocol.https.allow", "always"),
            ("protocol.ssh.allow", "always"),
        ]
        .map(|(key, value)| (key.to_owned(), value.to_owned())),
    );
    if allow_file {
        config.push(("protocol.file.allow".into(), "always".into()));
    }
    let mut env = config_environment(&config);
    let null = null_device();
    let project = project_directory(repo);
    env.extend(
        [
            ("GCM_INTERACTIVE", "never".to_owned()),
            (
                "GIT_ALLOW_PROTOCOL",
                if allow_file {
                    "https:ssh:file"
                } else {
                    "https:ssh"
                }
                .to_owned(),
            ),
            ("GIT_ASKPASS", String::new()),
            ("GIT_CONFIG_GLOBAL", null.clone()),
            ("GIT_CONFIG_PARAMETERS", String::new()),
            ("GIT_CONFIG_SYSTEM", null),
            ("GIT_EXEC_PATH", trusted_git_exec_path(repo.executable())?),
            ("GIT_PROTOCOL_FROM_USER", "0".to_owned()),
            ("GIT_PROXY_COMMAND", String::new()),
            ("GIT_SSH", String::new()),
            ("GIT_SSH_COMMAND", String::new()),
            ("GIT_SSH_VARIANT", "ssh".to_owned()),
            ("GIT_TERMINAL_PROMPT", "0".to_owned()),
            ("PATH", trusted_path(&project)),
            ("SSH_ASKPASS", String::new()),
            ("SSH_ASKPASS_REQUIRE", "never".to_owned()),
        ]
        .map(|(key, value)| (key.to_owned(), value)),
    );
    match search_trusted_path("ssh", &project) {
        // A trusted global rewrite may turn an HTTPS URL into an SSH one after
        // validation, so the safe client is prepared for both protocols.
        Some(ssh) => set(&mut env, "GIT_SSH_COMMAND", quote_ssh_command(&ssh)),
        None if protocol == "ssh" => {
            return Err(unsafe_fetch(
                "cannot securely fetch an SSH remote: no trusted ssh was found",
            ));
        }
        None => {}
    }
    Ok(SecureFetch { url, env })
}

fn set(env: &mut Vec<(String, String)>, key: &str, value: String) {
    match env.iter_mut().find(|(existing, _)| existing == key) {
        Some(entry) => entry.1 = value,
        None => env.push((key.to_owned(), value)),
    }
}

fn null_device() -> String {
    if cfg!(windows) { "nul" } else { "/dev/null" }.to_owned()
}

fn project_directory(repo: &GitRepo) -> PathBuf {
    std::fs::canonicalize(repo.working_dir()).unwrap_or_else(|_| repo.working_dir().to_path_buf())
}

fn remote_url(repo: &GitRepo, remote: &str) -> Result<String, UnsafeFetch> {
    let key = format!("remote.{remote}.url");
    let url = repo
        .stdout(["config", "--get", key.as_str()], remote)
        .ok()
        .filter(|url| !url.is_empty());
    url.ok_or_else(|| UnsafeFetch(format!("remote `{remote}` has no fetch URL")))
}

/// The protocol a URL is fetched over and the exact string git receives.
///
/// HTTPS with a host, SSH with a host and user that cannot be read as
/// options, the scp-like `user@host:path` spelling, and, when the caller opts
/// in, a local path decoded to the one git will read. Everything else,
/// including remote helpers, is refused (`vibe/core/git/fetch.py:139-156`).
pub(crate) fn validated_fetch_url(
    url: &str,
    allow_file: bool,
) -> Result<(&'static str, String), UnsafeFetch> {
    if url.contains(['\0', '\r', '\n']) {
        return Err(unsafe_fetch("the remote URL contains control characters"));
    }
    if allow_file && let Some(local) = canonical_local_file_url(url) {
        return Ok(("file", local));
    }
    let parsed = split_url(url);
    if parsed.scheme == "https"
        && parsed
            .hostname
            .as_deref()
            .is_some_and(|host| !host.is_empty())
    {
        return Ok(("https", url.to_owned()));
    }
    if parsed.scheme == "ssh"
        && valid_ssh_endpoint(parsed.username.as_deref(), parsed.hostname.as_deref())
    {
        if !parsed.port_valid {
            return Err(unsafe_fetch("the SSH remote URL has an invalid port"));
        }
        return Ok(("ssh", url.to_owned()));
    }
    if parsed.scheme.is_empty()
        && let Some(captures) = SCP_SSH_URL.captures(url)
        && valid_ssh_endpoint(
            captures.name("user").map(|value| value.as_str()),
            captures.name("host").map(|value| value.as_str()),
        )
    {
        return Ok(("ssh", url.to_owned()));
    }
    Err(unsafe_fetch(
        "only explicit SSH and HTTPS remote URLs may be fetched",
    ))
}

fn valid_ssh_endpoint(user: Option<&str>, host: Option<&str>) -> bool {
    host.is_some_and(|host| !host.is_empty() && !host.starts_with('-'))
        && user.is_none_or(|user| !user.starts_with('-'))
}

/// A git-local path, or [`None`] for a non-local or ambiguous spelling
/// (`vibe/core/git/fetch.py:159-181`).
fn canonical_local_file_url(url: &str) -> Option<String> {
    // Both leading positions accept either slash on Windows, and a checkout can
    // be opened there later, so every network-path spelling is refused.
    let bytes = url.as_bytes();
    if bytes.len() >= 2 && matches!(bytes[0], b'/' | b'\\') && matches!(bytes[1], b'/' | b'\\') {
        return None;
    }
    if cfg!(windows) && is_windows_drive_path(url) {
        return Some(url.to_owned());
    }
    let parsed = split_url(url);
    if parsed.scheme == "file" {
        return canonical_file_scheme_path(&parsed);
    }
    if !parsed.scheme.is_empty() || SCP_SSH_URL.is_match(url) {
        return None;
    }
    // A leading dash could be read as an option by git plumbing.
    (!url.starts_with('-')).then(|| url.to_owned())
}

fn is_windows_drive_path(url: &str) -> bool {
    let bytes = url.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

fn canonical_file_scheme_path(parsed: &SplitUrl) -> Option<String> {
    let raw = parsed.path.as_str();
    let has_suffix = !parsed.netloc.is_empty() || parsed.has_query || parsed.has_fragment;
    let ambiguous_path = !raw.starts_with('/') || raw.starts_with("//") || raw.contains('\\');
    if has_suffix || ambiguous_path || INVALID_PERCENT_ESCAPE.is_match(raw) {
        return None;
    }
    let local = percent_decode_strict(raw)?;
    if local.contains(['\0', '\r', '\n', '\\']) || local.starts_with("//") {
        return None;
    }
    let local_bytes = local.as_bytes();
    if cfg!(windows)
        && local_bytes.len() >= 4
        && local_bytes[0] == b'/'
        && local_bytes[1].is_ascii_alphabetic()
        && local_bytes[2] == b':'
        && local_bytes[3] == b'/'
    {
        return Some(local[1..].to_owned());
    }
    Some(local)
}

fn percent_decode_strict(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let pair = raw.get(index + 1..index + 3)?;
            decoded.push(u8::from_str_radix(pair, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// The parts of `urllib.parse.urlsplit` the validation reads.
#[derive(Debug, Default)]
struct SplitUrl {
    scheme: String,
    netloc: String,
    path: String,
    has_query: bool,
    has_fragment: bool,
    username: Option<String>,
    hostname: Option<String>,
    port_valid: bool,
}

/// Splits a URL the way Python's `urlsplit` does for the shapes a remote takes.
///
/// A scheme is the text before the first colon when it starts with a letter
/// and holds only scheme characters, lowercased. A network location follows
/// only a `//`. Query and fragment are noted, not parsed.
fn split_url(url: &str) -> SplitUrl {
    let mut split = SplitUrl {
        port_valid: true,
        ..SplitUrl::default()
    };
    let mut rest = url;
    if let Some(colon) = url.find(':') {
        let candidate = &url[..colon];
        let mut characters = candidate.chars();
        let leads_with_letter = characters
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic());
        if leads_with_letter
            && candidate
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "+-.".contains(character))
        {
            split.scheme = candidate.to_ascii_lowercase();
            rest = &url[colon + 1..];
        }
    }
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        split.netloc = after[..end].to_owned();
        rest = &after[end..];
    }
    if let Some(hash) = rest.find('#') {
        split.has_fragment = true;
        rest = &rest[..hash];
    }
    if let Some(question) = rest.find('?') {
        split.has_query = true;
        rest = &rest[..question];
    }
    split.path = rest.to_owned();
    if !split.netloc.is_empty() {
        let (userinfo, hostport) = match split.netloc.rfind('@') {
            Some(at) => (Some(&split.netloc[..at]), &split.netloc[at + 1..]),
            None => (None, split.netloc.as_str()),
        };
        split.username = userinfo.map(|info| info.split(':').next().unwrap_or("").to_owned());
        let (host, port) = if let Some(bracketed) = hostport.strip_prefix('[') {
            match bracketed.find(']') {
                Some(close) => (
                    bracketed[..close].to_owned(),
                    bracketed[close + 1..].strip_prefix(':'),
                ),
                None => (bracketed.to_owned(), None),
            }
        } else {
            match hostport.split_once(':') {
                Some((host, port)) => (host.to_owned(), Some(port)),
                None => (hostport.to_owned(), None),
            }
        };
        split.hostname = (!host.is_empty()).then(|| host.to_lowercase());
        split.port_valid = port.is_none_or(|port| {
            port.is_empty()
                || (port.bytes().all(|byte| byte.is_ascii_digit())
                    && port.parse::<u32>().is_ok_and(|value| value <= 65_535))
        });
    }
    split
}

/// Reads every configuration scope once and keeps only trusted fetch settings
/// (`vibe/core/git/fetch.py:256-331`).
///
/// A read that cannot classify the configuration fails closed rather than
/// treating it as trusted.
fn inspect_fetch_config(
    repo: &GitRepo,
    url: &str,
    reject_repository_http: bool,
) -> Result<Vec<(String, String)>, UnsafeFetch> {
    let unreadable = || unsafe_fetch("cannot inspect the git configuration for a fetch");
    let project = project_directory(repo);
    let git_dir = repo
        .stdout(["rev-parse", "--absolute-git-dir"], "config")
        .map_err(|_| unreadable())?;
    let mut command = Command::new(repo.executable());
    command
        .arg(format!("--git-dir={git_dir}"))
        .arg(format!("--work-tree={}", project.display()))
        .args(["config", "--includes", "--show-scope", "--null", "--list"])
        .env_remove("GIT_CONFIG_GLOBAL")
        .env_remove("GIT_CONFIG_SYSTEM")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (key, _) in std::env::vars_os() {
        let key = key.to_string_lossy();
        if key.starts_with("GIT_CONFIG_KEY_") || key.starts_with("GIT_CONFIG_VALUE_") {
            command.env_remove(key.as_ref());
        }
    }
    ensure_global_config_paths_are_trusted(&project)?;
    let output = run_capturing(command, CONFIG_TIMEOUT).ok_or_else(unreadable)?;
    if !output.status.success() {
        return Err(unreadable());
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut records = text.split('\0').collect::<Vec<_>>();
    if records.last().is_some_and(|last| last.is_empty()) {
        records.pop();
    }
    if records.len() % 2 != 0 {
        return Err(unreadable());
    }
    let mut trusted = Vec::new();
    for pair in records.chunks(2) {
        let scope = pair[0].to_lowercase();
        let (key, value) = pair[1].split_once('\n').ok_or_else(unreadable)?;
        let normalized = key.to_lowercase();
        let trusted_scope = TRUSTED_FETCH_SCOPES.contains(&scope.as_str());
        if reject_repository_http
            && (normalized.starts_with("http.") || normalized.starts_with("https."))
            && !trusted_scope
        {
            return Err(unsafe_fetch(
                "repository HTTP configuration is not allowed for fetches",
            ));
        }
        if !trusted_scope && URL_REWRITE_KEY.is_match(key) && url.starts_with(value) {
            return Err(unsafe_fetch(
                "repository URL rewrites are not allowed for fetches",
            ));
        }
        if trusted_scope && is_trusted_fetch_config_key(key) {
            if key.contains('\u{fffd}') || value.contains('\u{fffd}') {
                return Err(unsafe_fetch(
                    "trusted git configuration contains invalid UTF-8",
                ));
            }
            trusted.push((key.to_owned(), value.to_owned()));
        }
    }
    Ok(trusted)
}

fn run_capturing(mut command: Command, limit: Duration) -> Option<std::process::Output> {
    let mut child = command.spawn().ok()?;
    let reader = child.stdout.take().map(|mut stdout| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stdout.read_to_end(&mut bytes);
            bytes
        })
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => return None,
        }
    };
    let stdout = reader
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    Some(std::process::Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}

fn is_trusted_fetch_config_key(key: &str) -> bool {
    let normalized = key.to_lowercase();
    normalized.starts_with("credential.")
        || normalized.starts_with("http.")
        || normalized.starts_with("https.")
        || normalized == "safe.directory"
        || URL_REWRITE_KEY.is_match(key)
}

/// Refuses a user configuration file that resolves inside the checkout, which
/// would let the repository author the "trusted" scopes
/// (`vibe/core/git/fetch.py:348-374`).
fn ensure_global_config_paths_are_trusted(project: &Path) -> Result<(), UnsafeFetch> {
    let mut homes = Vec::new();
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        homes.push(PathBuf::from(home));
    } else if cfg!(windows) {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            homes.push(PathBuf::from(profile));
        }
        if let (Some(drive), Some(path)) =
            (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH"))
        {
            let mut joined = drive;
            joined.push(path);
            homes.push(PathBuf::from(joined));
        }
    }
    let mut paths = homes
        .iter()
        .map(|home| home.join(".gitconfig"))
        .collect::<Vec<_>>();
    match std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        Some(xdg) => paths.push(PathBuf::from(xdg).join("git").join("config")),
        None => paths.extend(
            homes
                .iter()
                .map(|home| home.join(".config").join("git").join("config")),
        ),
    }
    for path in paths {
        let resolved = super::resolve_lenient(&path);
        if resolved.starts_with(project) {
            return Err(unsafe_fetch(
                "a global git configuration inside the repository is not trusted",
            ));
        }
    }
    Ok(())
}

fn config_environment(config: &[(String, String)]) -> Vec<(String, String)> {
    let mut env = vec![("GIT_CONFIG_COUNT".to_owned(), config.len().to_string())];
    for (index, (key, value)) in config.iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{index}"), key.clone()));
        env.push((format!("GIT_CONFIG_VALUE_{index}"), value.clone()));
    }
    env
}

fn trusted_git_exec_path(executable: &Path) -> Result<String, UnsafeFetch> {
    let unresolved = || unsafe_fetch("cannot resolve git's trusted helper path");
    if let Ok(cache) = EXEC_PATHS.lock()
        && let Some(path) = cache.get(executable)
    {
        return Ok(path.clone());
    }
    let mut command = Command::new(executable);
    command
        .arg("--exec-path")
        .env_remove("GIT_EXEC_PATH")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = run_capturing(command, CONFIG_TIMEOUT).ok_or_else(unresolved)?;
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || path.is_empty() {
        return Err(unresolved());
    }
    if let Ok(mut cache) = EXEC_PATHS.lock() {
        cache.insert(executable.to_path_buf(), path.clone());
    }
    Ok(path)
}

/// The inherited `PATH` without relative entries or entries inside the
/// checkout (`vibe/core/git/fetch.py:207-226`).
fn trusted_path(project: &Path) -> String {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let trusted = std::env::split_paths(&path)
        .filter(|entry| !entry.as_os_str().is_empty())
        .filter_map(|entry| {
            let text = entry.to_string_lossy();
            let entry = match text
                .strip_prefix('"')
                .and_then(|inner| inner.strip_suffix('"'))
            {
                Some(inner) if cfg!(windows) => PathBuf::from(inner),
                _ => entry.clone(),
            };
            if !entry.is_absolute() {
                return None;
            }
            let resolved = std::fs::canonicalize(&entry).ok()?;
            (!resolved.starts_with(project)).then_some(resolved)
        })
        .collect::<Vec<_>>();
    std::env::join_paths(trusted)
        .map(|joined| joined.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn quote_ssh_command(executable: &Path) -> String {
    let text = executable.to_string_lossy();
    if cfg!(windows) {
        if text.contains([' ', '\t', '"']) {
            format!("\"{}\"", text.replace('"', "\\\""))
        } else {
            text.into_owned()
        }
    } else {
        shlex::try_quote(&text)
            .map_or_else(|_| text.clone().into_owned(), |quoted| quoted.into_owned())
    }
}
