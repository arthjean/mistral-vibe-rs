//! The last-session pointer: `.last_session/<tty>` names the session a
//! terminal last used, which is what a `continue` there reopens.

use std::fs;
use std::path::{Path, PathBuf};

use super::{
    HANDOFF_JOURNAL_PREFIX, HANDOFF_LOCK_PREFIX, LAST_SESSION_DIRECTORY, SessionStore,
    StorageError, ensure_private_directory, sync_directory,
};
use crate::atomic_file::{FileMode, write_atomically};

impl SessionStore {
    pub fn select_for_continue(&self, selector: &str) -> Result<(), StorageError> {
        let metadata = self.resolve(selector)?;
        self.write_pointer(&metadata.id)
    }

    /// Records `session_id` as the one this terminal last used, whether or not
    /// it has been written yet (reference `last_session_pointer.record`).
    pub fn record_pointer(&self, session_id: &str) -> Result<(), StorageError> {
        self.write_pointer(session_id)
    }

    pub(super) fn write_pointer(&self, session_id: &str) -> Result<(), StorageError> {
        let Some(pointer_key) = &self.pointer_key else {
            return Ok(());
        };
        let pointer_directory = self.root.join(LAST_SESSION_DIRECTORY);
        ensure_private_directory(&pointer_directory)?;
        let path = pointer_directory.join(pointer_key);
        write_atomically(
            &path,
            "pointer",
            format!("{session_id}\n").as_bytes(),
            FileMode::Private,
        )
        .map_err(StorageError::from)
    }

    pub(super) fn read_pointer(&self) -> Result<Option<String>, StorageError> {
        let Some(pointer_key) = &self.pointer_key else {
            return Ok(None);
        };
        let path = self.root.join(LAST_SESSION_DIRECTORY).join(pointer_key);
        match fs::read_to_string(&path) {
            Ok(pointer) => {
                let pointer = pointer.trim();
                if pointer.is_empty()
                    || pointer.contains('/')
                    || pointer.contains('\\')
                    || pointer == "."
                    || pointer == ".."
                {
                    Ok(None)
                } else {
                    Ok(Some(pointer.to_owned()))
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(StorageError::Io { path, source }),
        }
    }

    /// The session the terminal's pointer names, if any.
    pub fn pointer(&self) -> Option<String> {
        self.read_pointer().ok().flatten()
    }

    /// Reference `last_session_pointer.clear_matching`: every terminal's
    /// pointer that names the session is removed, not only this one's.
    pub(super) fn clear_pointer_if_matches(&self, session_id: &str) -> Result<(), StorageError> {
        let pointer_directory = self.root.join(LAST_SESSION_DIRECTORY);
        let Ok(entries) = fs::read_dir(&pointer_directory) else {
            return Ok(());
        };
        let mut removed = false;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if !path.is_file()
                || name.starts_with(HANDOFF_JOURNAL_PREFIX)
                || name.starts_with(HANDOFF_LOCK_PREFIX)
            {
                continue;
            }
            if fs::read_to_string(&path).is_ok_and(|pointer| pointer.trim() == session_id)
                && fs::remove_file(&path).is_ok()
            {
                removed = true;
            }
        }
        if removed {
            sync_directory(&pointer_directory)?;
        }
        Ok(())
    }
}

pub(super) fn current_tty_key() -> Option<String> {
    #[cfg(unix)]
    {
        for descriptor in ["0", "1", "2"] {
            let path = PathBuf::from("/proc/self/fd").join(descriptor);
            if let Ok(target) = fs::read_link(path)
                && target.starts_with("/dev/")
                && target != Path::new("/dev/null")
                && let Some(name) = target.file_name().and_then(|name| name.to_str())
            {
                return Some(sanitize_pointer_key(name));
            }
        }
        None
    }
    #[cfg(not(unix))]
    {
        // Reference `_windows_tty_key` without the console window handle,
        // which needs a Win32 call this crate does not make: the Windows
        // Terminal session, and otherwise the parent process.
        Some(match std::env::var("WT_SESSION") {
            Ok(value) => sanitize_pointer_key(&format!("wt-{value}")),
            Err(_) => sanitize_pointer_key(&format!("ppid-{}", parent_process_id())),
        })
    }
}

#[cfg(not(unix))]
fn parent_process_id() -> u32 {
    std::process::id()
}

pub(super) fn sanitize_pointer_key(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches('_');
    if sanitized.is_empty() {
        "unknown".to_owned()
    } else {
        sanitized.to_owned()
    }
}
