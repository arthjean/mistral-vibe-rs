//! Owner-only remediation of the save directory.
//!
//! Reference `restrict_session_log_permissions`
//! (`vibe/core/session/session_permissions.py`): logs hold raw tool results, so
//! the save directory and everything under it lose their group and other bits.
//! Owner bits stay, symbolic links are neither followed nor changed, the root
//! is never created, and a path that cannot be changed is skipped. The sweep
//! writes no data, so it runs whether or not session logging is enabled.

use std::path::Path;

/// Tightens every path under `root` and answers how many changed.
#[must_use]
pub fn restrict_session_log_permissions(root: &Path) -> usize {
    #[cfg(unix)]
    {
        if !root.is_dir() {
            return 0;
        }
        let unified = root.join("unified");
        let mut tightened = 0_usize;
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            tightened += strip_group_other(&directory);
            // A unified session directory is created owner-only, which is what
            // gates its interior, so the walk stops at it.
            if directory != unified && directory.starts_with(&unified) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_symlink() {
                    continue;
                }
                if file_type.is_dir() {
                    pending.push(entry.path());
                } else {
                    tightened += strip_group_other(&entry.path());
                }
            }
        }
        tightened
    }
    #[cfg(not(unix))]
    {
        // Mode bits do not govern access on Windows.
        let _ = root;
        0
    }
}

/// Starts [`restrict_session_log_permissions`] on a background thread, once
/// per process, as the reference does when it builds a session's config.
pub fn start_restrict_session_log_permissions(root: &Path) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    let root = root.to_path_buf();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("restrict_session_log_permissions".to_owned())
            .spawn(move || restrict_session_log_permissions(&root));
    });
}

#[cfg(unix)]
fn strip_group_other(path: &Path) -> usize {
    use std::os::unix::fs::PermissionsExt as _;
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if metadata.file_type().is_symlink() {
        return 0;
    }
    let mode = metadata.permissions().mode() & 0o7777;
    if mode & 0o077 == 0 {
        return 0;
    }
    usize::from(
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & !0o077)).is_ok(),
    )
}
