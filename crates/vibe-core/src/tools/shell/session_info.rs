//! What the session tools say about a managed session, whichever life it is in.
//!
//! Reference `read_output`, `inspect_session`, `info` and `list_sessions` all
//! answer with one `SessionInfo`, built from the live session by
//! `_session_info_locked` and from the manifest by `_info_from_manifest`. Both
//! are a [`SessionInfo`] here, so every tool that describes a session reads the
//! same typed value and the documents the tools publish are built from it.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::tools::ToolError;
use crate::tools::document::Document;

use super::decode::read_file_window;
use super::session::{ManagedSession, SessionShell, SessionStatus};

/// Reference `SessionInfo`: eleven snake_case fields, the two stamps rendered
/// as ISO-8601 instants in UTC, and the three optional ones present and null
/// rather than omitted. The fields are declared in the reference order, which
/// is the order a manifest is read back in.
///
/// A manifest may lack any field but the identifier the orphan scan selects it
/// by: one is still described, with an empty text and the `orphaned` status
/// standing in for what it did not record, so a fragment a previous process
/// left never refuses the listing of the sessions beside it
/// (`the_orphan_filter_reads_the_identifier_rather_than_the_file_name`). A
/// field of the wrong type is refused, as reference `_info_from_manifest`
/// refuses it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct SessionInfo {
    pub(super) session_id: String,
    #[serde(default)]
    pub(super) command: String,
    #[serde(default)]
    pub(super) cwd: String,
    #[serde(default)]
    pub(super) shell: String,
    #[serde(default)]
    pub(super) pty_backend: Option<String>,
    #[serde(default = "unrecorded_status")]
    pub(super) status: SessionStatus,
    #[serde(default)]
    pub(super) exit_code: Option<i64>,
    #[serde(default)]
    pub(super) output_path: String,
    #[serde(default)]
    pub(super) created_at: String,
    #[serde(default)]
    pub(super) updated_at: String,
    #[serde(default)]
    pub(super) reader_error: Option<String>,
}

/// The status a manifest that records none is described with: whatever it
/// last was, its process is gone.
fn unrecorded_status() -> SessionStatus {
    SessionStatus::Orphaned
}

impl SessionInfo {
    /// Reference `_info_from_manifest`: the manifest a previous process left,
    /// validated when a tool reads it rather than when it is loaded.
    pub(super) fn from_manifest(session_id: &str, manifest: &Value) -> Result<Self, ToolError> {
        Self::deserialize(manifest).map_err(|error| {
            ToolError::Execution(format!(
                "the manifest of session `{session_id}` is not a session record: {error}"
            ))
        })
    }

    /// The session as the tools that embed one publish it, in the order
    /// reference `SessionInfo` declares its fields.
    pub(super) fn document(&self) -> Document {
        Document::new()
            .field("session_id", self.session_id.clone())
            .field("command", self.command.clone())
            .field("cwd", self.cwd.clone())
            .field("shell", self.shell.clone())
            .field("pty_backend", self.pty_backend.clone())
            .field("status", self.status.as_str())
            .field("exit_code", self.exit_code)
            .field("output_path", self.output_path.clone())
            .field("created_at", self.created_at.clone())
            .field("updated_at", self.updated_at.clone())
            .field("reader_error", self.reader_error.clone())
    }
}

/// The session `session_id` names, live or left behind.
///
/// Reference `_live_session` separates the two: a live session is driven, an
/// orphaned one is only described, and an id that is neither is unknown. The
/// three answers are distinct here for the same reason, so a tool that needs a
/// process says so rather than reporting the id as missing.
pub(super) enum SessionHandle {
    Live(Arc<ManagedSession>),
    /// An orphan reports the status its own process last recorded rather than
    /// one this side invents: only
    /// [`SessionShell::load_orphaned_manifests`] rewrites a manifest, and only
    /// when it still claims the process is running.
    Orphaned(Box<SessionInfo>),
}

impl SessionHandle {
    pub(super) fn info(&self) -> SessionInfo {
        match self {
            Self::Live(session) => session.info(),
            Self::Orphaned(info) => (**info).clone(),
        }
    }

    /// The log the session wrote, which outlives the process that wrote it.
    pub(super) fn log_path(&self) -> PathBuf {
        match self {
            Self::Live(session) => session.log_path.clone(),
            Self::Orphaned(info) => PathBuf::from(&info.output_path),
        }
    }

    /// Whether more bytes are still coming, which is what decides a cut
    /// character is held back. An orphan's terminal is gone, and
    /// [`SessionShell::load_orphaned_manifests`] already rewrote any manifest
    /// that still said `running`, so it never answers `true`.
    pub(super) fn is_running(&self) -> bool {
        matches!(self, Self::Live(session) if session.is_running())
    }
}

pub(super) async fn session_handle(
    shell: &SessionShell,
    session_id: &str,
) -> Result<SessionHandle, ToolError> {
    let sessions = shell.managed.lock().await;
    if let Some(session) = sessions.get(session_id) {
        return Ok(SessionHandle::Live(session.clone()));
    }
    drop(sessions);
    if let Some(manifest) = shell.orphan(session_id) {
        return SessionInfo::from_manifest(session_id, &manifest)
            .map(|info| SessionHandle::Orphaned(Box::new(info)));
    }
    let active = shell
        .managed
        .lock()
        .await
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let listed = if active.is_empty() {
        "none".to_owned()
    } else {
        active.join(", ")
    };
    Err(ToolError::Execution(format!(
        "unknown session `{session_id}`; active sessions: {listed}"
    )))
}

pub(super) async fn managed_session(
    shell: &SessionShell,
    session_id: &str,
) -> Result<Arc<ManagedSession>, ToolError> {
    match session_handle(shell, session_id).await? {
        SessionHandle::Live(session) => Ok(session),
        SessionHandle::Orphaned(_) => Err(ToolError::Execution(format!(
            "session `{session_id}` was left running by a previous process and has no live \
             terminal; read its log instead"
        ))),
    }
}

/// One read of a session's log, and the session it was read from.
pub(super) struct SessionWindow {
    pub(super) info: SessionInfo,
    log_path: PathBuf,
    output: String,
    next_cursor: u64,
    truncated: bool,
}

impl SessionWindow {
    pub(super) fn read(
        handle: &SessionHandle,
        cursor: u64,
        limit: usize,
    ) -> Result<Self, ToolError> {
        let log_path = handle.log_path();
        let (output, next_cursor, truncated) =
            read_file_window(&log_path, cursor, limit, handle.is_running())?;
        Ok(Self {
            info: handle.info(),
            log_path,
            output,
            next_cursor,
            truncated,
        })
    }

    pub(super) fn display(&self) -> Value {
        json!({"kind": "shell", "command": self.info.command})
    }

    /// Reference `_result_from_session`'s computed `returncode`, which falls
    /// back to zero while `exit_code` stays null for a session that has not
    /// exited.
    pub(super) fn returncode(&self) -> i64 {
        self.info.exit_code.unwrap_or(0)
    }

    /// What the family's command tool answers once a session has been started.
    ///
    /// Reference `_result_from_session` reads the session once and fills the
    /// whole result from that one read: `output` is what the log held, both
    /// streams interleaved by the terminal, so there is no separate `stdout` or
    /// `stderr`.
    pub(super) fn command_document(&self, background: bool) -> Document {
        Document::new()
            .field("command", self.info.command.clone())
            .field("session_id", self.info.session_id.clone())
            .field("status", self.info.status.as_str())
            .field("exit_code", self.info.exit_code)
            .field("shell", self.info.shell.clone())
            .field("background", background)
            .field("output", self.output.clone())
            .field("next_cursor", self.next_cursor)
            .field("truncated", self.truncated)
            .field("output_path", self.log_path.to_string_lossy().into_owned())
            .field("returncode", self.returncode())
    }

    /// What the family's output tool answers, which is the session's state and
    /// the window it just read rather than anything about the call that
    /// started it.
    pub(super) fn poll_document(&self) -> Document {
        Document::new()
            .field("session_id", self.info.session_id.clone())
            .field("status", self.info.status.as_str())
            .field("exit_code", self.info.exit_code)
            .field("output", self.output.clone())
            .field("next_cursor", self.next_cursor)
            .field("truncated", self.truncated)
            .field("output_path", self.log_path.to_string_lossy().into_owned())
    }
}
