//! Workspace trust: which directories may contribute agent configuration.
//!
//! Reference `vibe/core/trusted_folders.py`. A directory is trusted, declined or
//! undecided, and the closest decision on the way up from a path answers for
//! it. Decisions persist in `trusted_folders.toml` under the vibe home, while a
//! session grant (`--trust`, `trustWorkspace`) lives only as long as the
//! process. Both are held by one store per trust file for the whole process,
//! as the reference's module-level manager is, so a grant one session made is
//! seen by every later question the process asks about that path.
//!
//! What the store decides reaches three places: the prompt a client shows
//! about an undecided workspace ([`build_trust_prompt`]), the project roots a
//! session opens, and the project configuration file, which is gated on the
//! trust of the `.vibe` directory holding it rather than on the working
//! directory ([`project_config_trusted`]).

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use crate::worktree::{resolve_lenient, strip_verbatim_prefix};

/// The trust file's name under the vibe home.
pub const TRUST_FILE: &str = "trusted_folders.toml";

const AGENTS_MD: &str = "AGENTS.md";
const VIBE_DIRECTORY: &str = ".vibe";
const AGENTS_DIRECTORY: &str = ".agents";

/// What a path resolves to. Reference `WorkspaceTrustStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustStatus {
    Trusted,
    Session,
    Untrusted,
}

impl TrustStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Session => "session",
            Self::Untrusted => "untrusted",
        }
    }
}

/// A choice a trust prompt offers. Reference `WorkspaceTrustDecision`.
///
/// `trust_session` exists in the core vocabulary only: no prompt at the pin
/// offers it and the wire model does not accept it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceTrustDecision {
    TrustRepository,
    TrustDirectory,
    TrustSession,
    Decline,
}

impl WorkspaceTrustDecision {
    /// The wire name of a decision.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TrustRepository => "trust_repo",
            Self::TrustDirectory => "trust_cwd",
            Self::TrustSession => "trust_session",
            Self::Decline => "decline",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        [
            Self::TrustRepository,
            Self::TrustDirectory,
            Self::TrustSession,
            Self::Decline,
        ]
        .into_iter()
        .find(|decision| decision.as_str() == value)
    }
}

/// What a client is asked about an undecided workspace. Reference
/// `WorkspaceTrustPrompt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceTrustPrompt {
    pub cwd: PathBuf,
    pub repo_root: Option<PathBuf>,
    pub detected_files: Vec<String>,
    pub repo_detected_files: Vec<String>,
    pub offer_repo_trust: bool,
    pub repo_explicitly_untrusted: bool,
}

/// Why a decision could not be applied.
#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error("`{0}` is not a decision this prompt offers")]
    Unsupported(&'static str),
    #[error("the trust file could not be written: {0}")]
    Io(#[from] io::Error),
}

/// The decisions a trust file holds, and the grants this process made.
#[derive(Debug)]
struct Folders {
    path: PathBuf,
    trusted: Vec<String>,
    untrusted: Vec<String>,
    /// Counted rather than deduplicated, so revoking one grant leaves any other
    /// in place.
    session: Vec<String>,
}

impl Folders {
    /// Reference `TrustedFoldersManager._load`: a missing or unreadable file is
    /// written back empty, and keys other than the two lists are ignored.
    fn load(path: PathBuf) -> Self {
        let mut folders = Self {
            path,
            trusted: Vec::new(),
            untrusted: Vec::new(),
            session: Vec::new(),
        };
        if !folders.path.is_file() {
            folders.save_quietly();
            return folders;
        }
        match fs::read_to_string(&folders.path)
            .ok()
            .and_then(|text| text.parse::<toml::Table>().ok())
        {
            Some(table) => {
                folders.trusted = strings(table.get("trusted"));
                folders.untrusted = strings(table.get("untrusted"));
            }
            None => folders.save_quietly(),
        }
        folders
    }

    /// A save at load time has no caller to report to.
    fn save_quietly(&self) {
        if let Err(error) = self.save() {
            crate::observability::log(
                crate::observability::LogLevel::Warning,
                &format!(
                    "Failed to write the trust file {}: {error}",
                    self.path.display()
                ),
            );
        }
    }

    /// Reference `TrustedFoldersManager._save`.
    ///
    /// The file is a record of security decisions, so it is created owner-only,
    /// while a file that already exists keeps the mode its owner gave it: the
    /// content is rewritten in place rather than replaced. Creating the
    /// directory or the file can fail the call; the write itself cannot, and
    /// the decision then holds for this process only, as upstream.
    fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !self.path.exists() {
            let mut options = OpenOptions::new();
            options.write(true).create(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;

                options.mode(0o600);
            }
            options.open(&self.path)?;
        }
        let rendered = render(&self.trusted, &self.untrusted);
        let written = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.path)
            .and_then(|mut file| file.write_all(rendered.as_bytes()));
        if let Err(error) = written {
            crate::observability::log(
                crate::observability::LogLevel::Debug,
                &format!(
                    "Failed to write the trust file {}: {error}",
                    self.path.display()
                ),
            );
        }
        Ok(())
    }

    /// Reference `_closest_decision`: `(trusted, ancestor)` for the closest
    /// decision on the way up, a grant counting as trust.
    fn closest_decision(&self, path: &Path) -> Option<(bool, PathBuf)> {
        let resolved = resolve(path);
        resolved.ancestors().find_map(|candidate| {
            self.decision_at_normalized(&candidate.to_string_lossy())
                .map(|trusted| (trusted, candidate.to_path_buf()))
        })
    }

    fn decision_at_normalized(&self, candidate: &str) -> Option<bool> {
        if self.trusted.iter().any(|path| path == candidate)
            || self.session.iter().any(|path| path == candidate)
        {
            Some(true)
        } else if self.untrusted.iter().any(|path| path == candidate) {
            Some(false)
        } else {
            None
        }
    }

    fn trust_status(&self, path: &Path) -> TrustStatus {
        let resolved = resolve(path);
        for candidate in resolved.ancestors() {
            let candidate = candidate.to_string_lossy();
            let listed = |list: &[String]| list.iter().any(|path| *path == candidate);
            if listed(&self.session) {
                return TrustStatus::Session;
            }
            if listed(&self.trusted) {
                return TrustStatus::Trusted;
            }
            if listed(&self.untrusted) {
                return TrustStatus::Untrusted;
            }
        }
        TrustStatus::Untrusted
    }

    fn add(&mut self, path: &Path, trusted: bool) -> io::Result<()> {
        let normalized = normalize(path);
        let (keep, drop) = if trusted {
            (&mut self.trusted, &mut self.untrusted)
        } else {
            (&mut self.untrusted, &mut self.trusted)
        };
        if !keep.contains(&normalized) {
            keep.push(normalized.clone());
        }
        if let Some(index) = drop.iter().position(|candidate| *candidate == normalized) {
            drop.remove(index);
        }
        self.save()
    }
}

/// The string items of a list the trust file holds.
fn strings(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(toml::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The two lists as `tomli_w` writes them: an empty list inline, any other
/// one item per line with a four-space indent and a trailing comma, every
/// path a basic string.
fn render(trusted: &[String], untrusted: &[String]) -> String {
    format!(
        "trusted = {}\nuntrusted = {}\n",
        render_array(trusted),
        render_array(untrusted)
    )
}

fn render_array(items: &[String]) -> String {
    if items.is_empty() {
        return "[]".to_owned();
    }
    let lines = items
        .iter()
        .map(|item| format!("    {}", basic_string(item)))
        .collect::<Vec<_>>();
    format!("[\n{},\n]", lines.join(",\n"))
}

/// A TOML basic string, escaped as `tomli_w` escapes one: the six compact
/// escapes, every other control character but tab as `\uXXXX`.
fn basic_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '\u{8}' => escaped.push_str("\\b"),
            '\n' => escaped.push_str("\\n"),
            '\u{c}' => escaped.push_str("\\f"),
            '\r' => escaped.push_str("\\r"),
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\t' => escaped.push('\t'),
            control if control.is_ascii_control() => {
                escaped.push_str(&format!("\\u{:04x}", u32::from(control)));
            }
            other => escaped.push(other),
        }
    }
    escaped.push('"');
    escaped
}

static STORES: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<Folders>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The process-wide trust store for one vibe home.
///
/// The first handle for a trust file loads it, creating it when it is missing,
/// and every later handle shares that state, which is what lets a session
/// grant or a decision a client made be seen by every other question the
/// process asks. Another process's writes are not re-read, as upstream.
#[derive(Debug, Clone)]
pub struct TrustStore {
    folders: Arc<Mutex<Folders>>,
}

impl TrustStore {
    /// The store for `vibe_home`, loaded on first use.
    #[must_use]
    pub fn for_vibe_home(vibe_home: &Path) -> Self {
        let path = vibe_home.join(TRUST_FILE);
        let mut stores = STORES.lock().unwrap_or_else(PoisonError::into_inner);
        let folders = stores
            .entry(path.clone())
            .or_insert_with(|| Arc::new(Mutex::new(Folders::load(path))))
            .clone();
        Self { folders }
    }

    fn with<T>(&self, read: impl FnOnce(&mut Folders) -> T) -> T {
        read(&mut self.folders.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Where the decisions persist.
    #[must_use]
    pub fn settings_path(&self) -> PathBuf {
        self.with(|folders| folders.path.clone())
    }

    /// The closest decision on the way up from `path`, a session grant
    /// counting as trust; `None` when nothing decides. Reference `is_trusted`.
    #[must_use]
    pub fn is_trusted(&self, path: &Path) -> Option<bool> {
        self.with(|folders| folders.closest_decision(path).map(|(trusted, _)| trusted))
    }

    /// Reference `trust_status`: a grant outranks a stored decision at the
    /// same level, and an undecided path is untrusted.
    #[must_use]
    pub fn trust_status(&self, path: &Path) -> TrustStatus {
        self.with(|folders| folders.trust_status(path))
    }

    /// Whether `path` itself is declined, without walking up. Reference
    /// `is_explicitly_untrusted`.
    #[must_use]
    pub fn is_explicitly_untrusted(&self, path: &Path) -> bool {
        let normalized = normalize(path);
        self.with(|folders| folders.untrusted.contains(&normalized))
    }

    /// The decision recorded for `path` itself, without walking up.
    #[must_use]
    pub fn decision_at(&self, path: &Path) -> Option<bool> {
        let normalized = normalize(path);
        self.with(|folders| folders.decision_at_normalized(&normalized))
    }

    /// The closest trusted ancestor, unless a closer decline blocks it.
    /// Reference `find_trust_root`.
    #[must_use]
    pub fn find_trust_root(&self, path: &Path) -> Option<PathBuf> {
        self.with(|folders| match folders.closest_decision(path) {
            Some((true, root)) => Some(root),
            _ => None,
        })
    }

    /// Records trust in `path`, replacing a decline of it.
    ///
    /// # Errors
    ///
    /// When the trust file's directory or the file itself cannot be created.
    pub fn add_trusted(&self, path: &Path) -> io::Result<()> {
        self.with(|folders| folders.add(path, true))
    }

    /// Records a decline of `path`, replacing trust in it.
    ///
    /// # Errors
    ///
    /// When the trust file's directory or the file itself cannot be created.
    pub fn add_untrusted(&self, path: &Path) -> io::Result<()> {
        self.with(|folders| folders.add(path, false))
    }

    /// Trusts `path` and every directory below it until the process exits.
    pub fn trust_for_session(&self, path: &Path) {
        let normalized = normalize(path);
        self.with(|folders| folders.session.push(normalized));
    }

    /// Undoes one [`Self::trust_for_session`] grant, leaving any other.
    pub fn revoke_session_trust(&self, path: &Path) {
        let normalized = normalize(path);
        self.with(|folders| {
            if let Some(index) = folders
                .session
                .iter()
                .position(|candidate| *candidate == normalized)
            {
                folders.session.remove(index);
            }
        });
    }
}

/// `path` as the reference's `Path.expanduser().resolve()` answers it: a
/// leading `~` is the home directory, a relative path is anchored on the
/// process working directory, symlinks are followed as far as the path exists
/// and the rest is folded lexically. Windows' verbatim prefix is dropped, so
/// what is stored matches what the reference stores.
#[must_use]
pub fn resolve(path: &Path) -> PathBuf {
    let expanded = expand_user(path);
    let absolute = std::path::absolute(&expanded).unwrap_or(expanded);
    let resolved = resolve_lenient(&absolute);
    PathBuf::from(strip_verbatim_prefix(&resolved.to_string_lossy()))
}

fn normalize(path: &Path) -> String {
    resolve(path).to_string_lossy().into_owned()
}

fn home() -> Option<PathBuf> {
    std::env::home_dir()
}

fn expand_user(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let rest = match text.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with(['/', std::path::MAIN_SEPARATOR]) => rest,
        _ => return path.to_path_buf(),
    };
    match home() {
        Some(home) => PathBuf::from(format!("{}{rest}", home.to_string_lossy())),
        None => path.to_path_buf(),
    }
}

fn is_strict_ancestor(ancestor: &Path, path: &Path) -> bool {
    path.ancestors()
        .skip(1)
        .any(|candidate| candidate == ancestor)
}

/// Reference `has_agents_md_file`.
#[must_use]
pub fn has_agents_md_file(path: &Path) -> bool {
    path.join(AGENTS_MD).is_file()
}

/// The configuration directories at `root` itself, `.vibe` then `.agents`.
///
/// Reference `find_local_config_dirs(...).config_dirs`: `.vibe` counts once it
/// holds a `tools`, `skills`, `plugins`, `agents` or `prompts` directory or a
/// `config.toml`, and `.agents` once it holds `skills`. No subdirectory of
/// `root` is searched.
#[must_use]
pub fn local_config_dirs(root: &Path) -> Vec<PathBuf> {
    let resolved = resolve(root);
    let mut found = Vec::new();
    let vibe = resolved.join(VIBE_DIRECTORY);
    if vibe.is_dir()
        && (["tools", "skills", "plugins", "agents", "prompts"]
            .iter()
            .any(|name| vibe.join(name).is_dir())
            || vibe.join("config.toml").is_file())
    {
        found.push(vibe);
    }
    let agents = resolved.join(AGENTS_DIRECTORY);
    if agents.is_dir() && agents.join("skills").is_dir() {
        found.push(agents);
    }
    found
}

/// The closest ancestor of `path`, or `path` itself, holding a real
/// `.git/HEAD`, never the home directory or the filesystem root. Reference
/// `find_git_repo_ancestor`: a `.git` file, as a linked worktree has, is not a
/// repository root.
#[must_use]
pub fn find_git_repo_ancestor(path: &Path) -> Option<PathBuf> {
    let resolved = resolve(path);
    let home = home().map(|home| resolve(&home));
    resolved
        .ancestors()
        .take_while(|candidate| Some(*candidate) != home.as_deref() && candidate.parent().is_some())
        .find(|candidate| {
            let git = candidate.join(".git");
            git.is_dir() && git.join("HEAD").is_file()
        })
        .map(Path::to_path_buf)
}

/// What under `path` would change how the agent behaves, relative and sorted.
/// Reference `find_trustable_files`.
#[must_use]
pub fn find_trustable_files(path: &Path) -> Vec<String> {
    let resolved = resolve(path);
    let mut found = Vec::new();
    if has_agents_md_file(path) {
        found.push(AGENTS_MD.to_owned());
    }
    for directory in local_config_dirs(path) {
        if let Ok(relative) = directory.strip_prefix(&resolved) {
            let label = format!("{}/", relative.to_string_lossy());
            if !found.contains(&label) {
                found.push(label);
            }
        }
    }
    found.sort();
    found
}

/// The repository files that reach `cwd` from above it: everything trustable
/// at the repository root, and every `AGENTS.md` between the two. Reference
/// `find_repo_trustable_files_for_cwd`.
#[must_use]
pub fn find_repo_trustable_files_for_cwd(cwd: &Path, repo_root: Option<&Path>) -> Vec<String> {
    let Some(repo_root) = repo_root else {
        return Vec::new();
    };
    let cwd = resolve(cwd);
    let repo_root = resolve(repo_root);
    if !is_strict_ancestor(&repo_root, &cwd) {
        return Vec::new();
    }
    let mut found = find_trustable_files(&repo_root)
        .into_iter()
        .collect::<BTreeSet<_>>();
    for directory in cwd.ancestors().skip(1) {
        if directory == repo_root {
            break;
        }
        if has_agents_md_file(directory)
            && let Ok(relative) = directory.join(AGENTS_MD).strip_prefix(&repo_root)
        {
            found.insert(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    found.into_iter().collect()
}

/// The configuration directories of a trusted `cwd` that are declined on
/// their own, sorted. Reference `find_untrusted_config_dirs`: a decline left
/// on a `.vibe` or `.agents` keeps the project file out even when the folder
/// holding it is trusted, which is what a client warns about.
#[must_use]
pub fn find_untrusted_config_dirs(cwd: &Path, store: &TrustStore) -> Vec<PathBuf> {
    let cwd = resolve(cwd);
    if store.is_trusted(&cwd) != Some(true) {
        return Vec::new();
    }
    let mut found = local_config_dirs(&cwd)
        .into_iter()
        .filter(|directory| store.is_explicitly_untrusted(directory))
        .collect::<Vec<_>>();
    found.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
    found
}

/// The prompt to show about `cwd`, or `None` when there is nothing to ask:
/// the home directory, a trusted directory, a declined one unless
/// `include_explicitly_untrusted`, or a directory nothing trustable reaches.
/// Reference `maybe_build_workspace_trust_prompt`.
#[must_use]
pub fn build_trust_prompt(
    cwd: &Path,
    include_explicitly_untrusted: bool,
    store: &TrustStore,
) -> Option<WorkspaceTrustPrompt> {
    let resolved_cwd = resolve(cwd);
    if home().map(|home| resolve(&home)).as_deref() == Some(resolved_cwd.as_path()) {
        return None;
    }
    if store.is_trusted(cwd) == Some(true) {
        return None;
    }
    if !include_explicitly_untrusted && store.is_explicitly_untrusted(cwd) {
        return None;
    }
    let repo_root = find_git_repo_ancestor(cwd);
    let detected_files = find_trustable_files(cwd);
    let repo_detected_files = find_repo_trustable_files_for_cwd(cwd, repo_root.as_deref());
    if detected_files.is_empty() && repo_detected_files.is_empty() {
        return None;
    }
    let offer_repo_trust = repo_root.as_deref().is_some_and(|root| {
        is_strict_ancestor(root, &resolved_cwd)
            && store.is_trusted(root) != Some(true)
            && (include_explicitly_untrusted || !store.is_explicitly_untrusted(root))
    });
    let repo_explicitly_untrusted = repo_root
        .as_deref()
        .is_some_and(|root| store.is_explicitly_untrusted(root));
    Some(WorkspaceTrustPrompt {
        cwd: cwd.to_path_buf(),
        repo_root,
        detected_files,
        repo_detected_files,
        offer_repo_trust,
        repo_explicitly_untrusted,
    })
}

/// What `prompt` lets the user choose, in the order a dialog lists it.
/// Reference `available_workspace_trust_decisions`.
#[must_use]
pub fn available_decisions(
    prompt: &WorkspaceTrustPrompt,
    include_session: bool,
) -> Vec<WorkspaceTrustDecision> {
    let mut decisions = Vec::with_capacity(4);
    if prompt.offer_repo_trust {
        decisions.push(WorkspaceTrustDecision::TrustRepository);
    }
    decisions.push(WorkspaceTrustDecision::TrustDirectory);
    if include_session {
        decisions.push(WorkspaceTrustDecision::TrustSession);
    }
    decisions.push(WorkspaceTrustDecision::Decline);
    decisions
}

/// Records `decision` about `prompt`. Reference
/// `apply_workspace_trust_decision`.
///
/// # Errors
///
/// [`TrustError::Unsupported`] for repository trust the prompt did not offer,
/// and [`TrustError::Io`] when the trust file cannot be created.
pub fn apply_decision(
    prompt: &WorkspaceTrustPrompt,
    decision: WorkspaceTrustDecision,
    store: &TrustStore,
) -> Result<(), TrustError> {
    match decision {
        WorkspaceTrustDecision::TrustRepository => match &prompt.repo_root {
            Some(root) if prompt.offer_repo_trust => store.add_trusted(root)?,
            _ => return Err(TrustError::Unsupported(decision.as_str())),
        },
        WorkspaceTrustDecision::TrustDirectory => store.add_trusted(&prompt.cwd)?,
        WorkspaceTrustDecision::TrustSession => store.trust_for_session(&prompt.cwd),
        WorkspaceTrustDecision::Decline => store.add_untrusted(&prompt.cwd)?,
    }
    Ok(())
}

/// Whether the project configuration file at `config_file` may be read.
///
/// Reference `ProjectConfigLayer._check_trust` asks about the `.vibe`
/// directory holding the file, and trust resolves upward from there: a
/// decline left on that `.vibe` keeps the file out even in a trusted
/// directory, and a file found above the working directory needs trust at
/// that level, which a grant on the working directory never reaches.
///
/// `working_directory_trusted` answers for the working directory when the file
/// sits there and nothing is recorded about its `.vibe`, which is where a
/// caller's own resolution of the session's trust stands in for the store's.
#[must_use]
pub fn project_config_trusted(
    store: &TrustStore,
    config_file: &Path,
    working_directory: &Path,
    working_directory_trusted: bool,
) -> bool {
    let Some(directory) = config_file.parent() else {
        return false;
    };
    if let Some(decided) = store.decision_at(directory) {
        return decided;
    }
    if directory.parent() == Some(working_directory) {
        return working_directory_trusted;
    }
    store.is_trusted(directory) == Some(true)
}

/// The `cache.toml` section recording which untrusted configuration folders a
/// client already warned about.
const WARNING_SECTION: &str = "untrusted_config_warning";

/// Reference `_show_untrusted_config_warning`'s bookkeeping: whether `dirs`
/// holds a folder no earlier launch warned about, in which case all of them
/// are recorded as warned and the caller shows the warning.
///
/// A folder left untrusted on purpose is mentioned once rather than on every
/// launch, while a new one is still surfaced. An unreadable cache reads as
/// empty and a failed write is dropped, as the reference's cache store does.
#[must_use]
pub fn untrusted_config_warning_due(vibe_home: &Path, dirs: &[String]) -> bool {
    if dirs.is_empty() {
        return false;
    }
    let path = vibe_home.join("cache.toml");
    let mut document = fs::read_to_string(&path)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .unwrap_or_default();
    let mut section = match document.remove(WARNING_SECTION) {
        Some(toml::Value::Table(section)) => section,
        _ => toml::Table::new(),
    };
    let acknowledged = strings(section.get("dirs"))
        .into_iter()
        .collect::<BTreeSet<_>>();
    if dirs
        .iter()
        .all(|directory| acknowledged.contains(directory))
    {
        return false;
    }
    let merged = acknowledged
        .into_iter()
        .chain(dirs.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(toml::Value::String)
        .collect();
    section.insert("dirs".to_owned(), toml::Value::Array(merged));
    document.insert(WARNING_SECTION.to_owned(), toml::Value::Table(section));
    if let Ok(encoded) = toml::to_string_pretty(&toml::Value::Table(document)) {
        let _ = fs::create_dir_all(vibe_home);
        let _ = crate::atomic_file::write_atomically(&path, "cache.toml", encoded.as_bytes());
    }
    true
}

#[cfg(test)]
mod trust_tests;
