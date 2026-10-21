//! `meta.json`: what a session records about itself, and the field patches
//! that rewrite one part of it without losing the rest.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::{
    MESSAGES_FILE, METADATA_FILE, SessionStore, StorageError, registry, validate_session_id,
};
use crate::atomic_file::{FileMode, write_atomically};
use crate::child::output_with_deadline;
use crate::platform::BackgroundCommand as _;
use crate::time::format_iso_millis;
use crate::time::parse_iso_millis;

/// Reference `SessionMetadata` (`vibe/core/types.py`), in its field order, plus
/// the keys a full save appends and every key neither side models.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetadata {
    #[serde(rename = "session_id")]
    pub id: String,
    #[serde(default)]
    pub parent_session_id: Option<String>,
    pub start_time: String,
    #[serde(default)]
    pub end_time: Option<String>,
    #[serde(default)]
    pub git_commit: Option<String>,
    #[serde(default)]
    pub git_branch: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub environment: BTreeMap<String, Option<String>>,
    /// Where the session began. `environment.working_directory` follows the
    /// session when it moves; this stays, so the listing still offers it from
    /// where the user started.
    #[serde(default)]
    pub origin_directory: Option<String>,
    #[serde(default = "unknown_username")]
    pub username: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub child_sessions: Vec<Value>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub loops: Vec<Value>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default = "default_title_source")]
    pub title_source: String,
    /// The latest accepted user interaction, as a UTC instant.
    #[serde(default)]
    pub bumped_at: Option<String>,
    /// When the session was pinned, or `None` while it is not.
    #[serde(default)]
    pub pinned_at: Option<String>,
    /// When the session was archived, or `None` while it is visible. Only the
    /// reference's Unified Harness sets it; this port keeps what it reads.
    #[serde(default)]
    pub archived_at: Option<String>,
    /// When the session last finished something its user has not looked at.
    /// Both marks stay `None` on a session written before unseen tracking.
    #[serde(default)]
    pub unseen_at: Option<String>,
    /// When the user last looked at the session.
    #[serde(default)]
    pub seen_at: Option<String>,
    #[serde(default, rename = "experiments")]
    pub experiment_state: Value,
    #[serde(default, deserialize_with = "null_as_default")]
    pub config: BTreeMap<String, Value>,
    #[serde(default)]
    pub import_provenance: Option<Value>,
    #[serde(default)]
    pub created_worktree: Option<Value>,
    #[serde(default, rename = "stats", deserialize_with = "null_as_default")]
    pub statistics: BTreeMap<String, Value>,
    #[serde(
        default,
        rename = "total_messages",
        deserialize_with = "null_as_default"
    )]
    pub message_count: u64,
    #[serde(default)]
    pub last_message_fingerprint: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub tools_available: Vec<Value>,
    #[serde(default)]
    pub agent_profile: Option<Value>,
    #[serde(default)]
    pub system_prompt: Option<Value>,
    /// Keys neither implementation's model names, kept so a rewrite here never
    /// drops what a newer writer added.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
    #[serde(skip)]
    pub directory: String,
    #[serde(skip)]
    pub created_at_ms: u64,
    #[serde(skip)]
    pub updated_at_ms: u64,
    #[serde(skip)]
    pub working_directory: String,
}

impl SessionMetadata {
    /// Whether this session has been written to disk.
    #[must_use]
    pub fn is_persisted(&self, store: &SessionStore) -> bool {
        store.session_path(self).join(METADATA_FILE).is_file()
    }
}

impl SessionStore {
    /// Records `link` among `parent_session_id`'s child sessions, once per tool
    /// call. Reference `record_child_session`.
    pub fn record_child_session(
        &self,
        parent_session_id: &str,
        link: Value,
    ) -> Result<(), StorageError> {
        let mut metadata = self.open(parent_session_id)?.metadata;
        let tool_call_id = link.get("tool_call_id").cloned();
        if metadata
            .child_sessions
            .iter()
            .any(|existing| existing.get("tool_call_id").cloned() == tool_call_id)
        {
            return Ok(());
        }
        metadata.child_sessions.push(link);
        self.write_metadata(&metadata)
    }

    /// Reference `SessionLogger._initialize_session_metadata`: what a new
    /// session records before anything happens in it.
    pub(super) fn compose_session(
        &self,
        id: &str,
        working_directory: &str,
        parent_session_id: Option<String>,
        now_ms: u64,
    ) -> SessionMetadata {
        let mut environment = BTreeMap::new();
        environment.insert(
            "working_directory".to_owned(),
            Some(working_directory.to_owned()),
        );
        let (git_commit, git_branch) = git_metadata(Path::new(working_directory));
        SessionMetadata {
            id: id.to_owned(),
            parent_session_id,
            start_time: format_iso_millis(now_ms),
            end_time: None,
            git_commit,
            git_branch,
            environment,
            origin_directory: Some(working_directory.to_owned()),
            username: current_username(),
            child_sessions: Vec::new(),
            loops: Vec::new(),
            title: None,
            title_source: default_title_source(),
            bumped_at: None,
            pinned_at: None,
            archived_at: None,
            unseen_at: None,
            seen_at: None,
            experiment_state: Value::Null,
            config: BTreeMap::new(),
            import_provenance: None,
            created_worktree: None,
            statistics: BTreeMap::new(),
            message_count: 0,
            last_message_fingerprint: None,
            tools_available: Vec::new(),
            agent_profile: None,
            system_prompt: None,
            extra: serde_json::Map::new(),
            directory: self.free_directory_name(now_ms, id),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            working_directory: working_directory.to_owned(),
        }
    }

    /// Writes `metadata` over the session's record. A session not yet written
    /// keeps the change in memory, where its first save picks it up.
    pub fn update_metadata(&self, metadata: &SessionMetadata) -> Result<(), StorageError> {
        self.write_metadata(metadata)
    }

    /// One session's metadata, without reading its messages.
    ///
    /// What a caller updating a single metadata field reads first, so a rollout
    /// persisted into `experiments` never pays for a transcript it does not
    /// look at.
    pub fn metadata(&self, selector: &str) -> Result<SessionMetadata, StorageError> {
        self.resolve(selector)
    }

    /// Reference `update_saved_session_title`: a rename is manual, and it is
    /// what a later automatic title never overrides.
    pub fn update_title(
        &self,
        session_id: &str,
        title: &str,
    ) -> Result<SessionMetadata, StorageError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(StorageError::InvalidTitle);
        }
        let mut metadata = self.resolve_exact(session_id)?;
        metadata.title = Some(title.to_owned());
        "manual".clone_into(&mut metadata.title_source);
        self.write_metadata(&metadata)?;
        Ok(metadata)
    }

    /// Reference `refresh_auto_title`: a generated title replaces an earlier
    /// generated one and never a manual rename. Answers whether it changed.
    pub fn refresh_auto_title(
        &self,
        metadata: &mut SessionMetadata,
        title: &str,
    ) -> Result<bool, StorageError> {
        let title = title.trim();
        if metadata.title_source == "manual"
            || title.is_empty()
            || metadata.title.as_deref() == Some(title)
        {
            return Ok(false);
        }
        if let Ok(current) = self.resolve_exact(&metadata.id)
            && current.title_source == "manual"
        {
            return Ok(false);
        }
        metadata.title = Some(title.to_owned());
        "auto".clone_into(&mut metadata.title_source);
        self.write_metadata(metadata)?;
        Ok(true)
    }

    /// Reference `SessionLogger.persist_active_model`: the model a session
    /// pinned when it first took a turn, under `config.active_model`.
    /// Answers whether the record changed.
    pub fn persist_active_model(
        &self,
        metadata: &mut SessionMetadata,
        alias: &str,
    ) -> Result<bool, StorageError> {
        if metadata.config.get("active_model").and_then(Value::as_str) == Some(alias) {
            return Ok(false);
        }
        metadata
            .config
            .insert("active_model".to_owned(), Value::String(alias.to_owned()));
        self.write_metadata(metadata)?;
        Ok(true)
    }

    /// Reference `persist_bumped_at`: the latest accepted user interaction,
    /// kept monotonic. Answers the instant the session now records, in
    /// milliseconds.
    pub fn persist_bumped_at(
        &self,
        metadata: &mut SessionMetadata,
        now_ms: u64,
    ) -> Result<u64, StorageError> {
        if let Some(current) = metadata.bumped_at.as_deref().and_then(parse_iso_millis)
            && current >= now_ms
        {
            return Ok(current);
        }
        metadata.bumped_at = Some(format_iso_millis(now_ms));
        self.write_metadata(metadata)?;
        Ok(now_ms)
    }

    /// Reference `relocate_saved_session`: the session now works in `cwd`, and
    /// the place it began is kept, promoted from the environment entry when the
    /// record predates the field.
    pub fn relocate(&self, session_id: &str, cwd: &str) -> Result<SessionMetadata, StorageError> {
        let mut metadata = self.resolve_exact(session_id)?;
        relocate_metadata(&mut metadata, cwd);
        self.write_metadata(&metadata)?;
        Ok(metadata)
    }

    /// Stamps a delegated session's end. One that never wrote anything has no
    /// record to stamp.
    pub fn close(&self, selector: &str, now_ms: u64) -> Result<(), StorageError> {
        let mut metadata = match self.resolve(selector) {
            Ok(metadata) => metadata,
            Err(StorageError::SessionNotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        metadata.updated_at_ms = now_ms;
        metadata.end_time = Some(format_iso_millis(now_ms));
        self.write_metadata(&metadata)
    }

    pub(super) fn read_metadata_from_directory(
        &self,
        directory: &str,
    ) -> Result<SessionMetadata, StorageError> {
        let path = self.root.join(directory).join(METADATA_FILE);
        let bytes = fs::read(&path).map_err(|source| StorageError::Io {
            path: path.clone(),
            source,
        })?;
        let mut metadata: SessionMetadata =
            serde_json::from_slice(&bytes).map_err(|source| StorageError::CorruptMetadata {
                path: path.clone(),
                source,
            })?;
        validate_session_id(&metadata.id)?;
        // This port's first layout versioned its records; the key means
        // nothing to either implementation now.
        metadata.extra.remove("format_version");
        metadata.directory = directory.to_owned();
        metadata.working_directory = metadata
            .environment
            .get("working_directory")
            .and_then(Clone::clone)
            .unwrap_or_default();
        let modified = fs::metadata(self.root.join(directory).join(MESSAGES_FILE))
            .and_then(|item| item.modified())
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|duration| u64::try_from(duration.as_millis()).ok())
            .unwrap_or_default();
        metadata.created_at_ms = parse_iso_millis(&metadata.start_time).unwrap_or(modified);
        metadata.updated_at_ms = metadata
            .end_time
            .as_deref()
            .and_then(parse_iso_millis)
            .unwrap_or(modified);
        Ok(metadata)
    }

    /// Writes `metadata` over its record: every key the record already holds
    /// that the model does not name is kept, which is what the reference's
    /// field patches do. A session not yet written stays unwritten.
    pub(super) fn write_metadata(&self, metadata: &SessionMetadata) -> Result<(), StorageError> {
        let session_path = self.session_path(metadata);
        if !session_path.join(MESSAGES_FILE).is_file() {
            registry::remember_pending(&self.root, metadata);
            return Ok(());
        }
        registry::discard_pending(&self.root, &metadata.id);
        let path = session_path.join(METADATA_FILE);
        let mut record = metadata.clone();
        if let Ok(bytes) = fs::read(&path)
            && let Ok(Value::Object(existing)) = serde_json::from_slice::<Value>(&bytes)
        {
            let known = serde_json::to_value(metadata)
                .ok()
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default();
            for (key, value) in existing {
                if key == "child_sessions" {
                    // A child links itself into its parent's record while the
                    // parent's own writer holds an older copy, so a link on
                    // disk is kept rather than overwritten.
                    for link in value.as_array().into_iter().flatten() {
                        if !record.child_sessions.contains(link) {
                            record.child_sessions.push(link.clone());
                        }
                    }
                } else if key != "format_version" && !known.contains_key(&key) {
                    record.extra.entry(key).or_insert(value);
                }
            }
        }
        let encoded = serde_json::to_vec_pretty(&record).map_err(StorageError::Json)?;
        write_atomically(&path, "meta", &encoded, FileMode::Private).map_err(StorageError::from)
    }
}

/// Reference `SessionLogger.relocated_to` and `relocate_saved_session_at_path`.
pub fn relocate_metadata(metadata: &mut SessionMetadata, cwd: &str) {
    if metadata.origin_directory.is_none() {
        metadata.origin_directory = metadata
            .environment
            .get("working_directory")
            .cloned()
            .flatten();
    }
    metadata
        .environment
        .insert("working_directory".to_owned(), Some(cwd.to_owned()));
    cwd.clone_into(&mut metadata.working_directory);
}

pub(super) fn default_title_source() -> String {
    "auto".to_owned()
}

fn unknown_username() -> String {
    "unknown".to_owned()
}

/// Reads JSON `null` as the type's default, which is how a record written with
/// an explicit `null` for an optional group loads.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Reference `getpass.getuser`: the first of `LOGNAME`, `USER`, `LNAME` and
/// `USERNAME` that is set.
#[must_use]
pub fn current_username() -> String {
    ["LOGNAME", "USER", "LNAME", "USERNAME"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .unwrap_or_else(unknown_username)
}

/// How long the reference waits for `git rev-parse` before it records no
/// commit and no branch.
const GIT_METADATA_TIMEOUT: Duration = Duration::from_secs(5);

/// How often that wait checks whether git has exited.
const GIT_METADATA_POLL: Duration = Duration::from_millis(10);

/// Reference `SessionLogger._fetch_git_metadata`
/// (`vibe/core/session/session_logger.py:138-160`): the commit and branch the
/// working directory is on, when it is a git checkout and a trusted git runs
/// there within the timeout.
fn git_metadata(working_directory: &Path) -> (Option<String>, Option<String>) {
    let Some(git) = crate::worktree::trusted_git_executable(working_directory) else {
        return (None, None);
    };
    let mut command = std::process::Command::new(git);
    command
        .in_background()
        .args(["rev-parse", "HEAD", "--abbrev-ref", "HEAD"])
        .current_dir(working_directory)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    // The reference detaches stdin on Windows only and lets the child inherit
    // it elsewhere.
    if cfg!(windows) {
        command.stdin(std::process::Stdio::null());
    }
    let output = output_with_deadline(&mut command, GIT_METADATA_TIMEOUT, GIT_METADATA_POLL);
    let Ok(Some(output)) = output else {
        return (None, None);
    };
    if !output.status.success() {
        return (None, None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = crate::pycompat::splitlines(crate::pycompat::strip(&text), false).into_iter();
    (
        lines.next().map(ToOwned::to_owned),
        lines.next().map(ToOwned::to_owned),
    )
}
