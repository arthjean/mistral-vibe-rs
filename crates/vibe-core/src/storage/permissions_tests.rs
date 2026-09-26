//! Owner-only remediation of the save directory.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use super::permissions::restrict_session_log_permissions;

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("mode set");
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path)
        .expect("path stats")
        .permissions()
        .mode()
        & 0o777
}

fn directory(path: &Path, mode: u32) {
    fs::create_dir(path).expect("directory");
    set_mode(path, mode);
}

fn file(path: &Path, mode: u32) {
    fs::write(path, b"x").expect("file");
    set_mode(path, mode);
}

#[test]
fn group_and_other_bits_are_stripped_throughout() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let root = temporary.path();
    set_mode(root, 0o755);
    directory(&root.join("nested"), 0o775);
    file(&root.join("nested").join("messages.jsonl"), 0o664);
    file(&root.join("top.json"), 0o644);

    assert_eq!(restrict_session_log_permissions(root), 4);
    assert_eq!(mode(root), 0o700);
    assert_eq!(mode(&root.join("nested")), 0o700);
    assert_eq!(mode(&root.join("nested").join("messages.jsonl")), 0o600);
    assert_eq!(mode(&root.join("top.json")), 0o600);
    assert_eq!(restrict_session_log_permissions(root), 0);
}

#[test]
fn owner_bits_are_kept_as_they_were() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let root = temporary.path();
    set_mode(root, 0o700);
    file(&root.join("read-only.json"), 0o440);
    file(&root.join("already-private.json"), 0o400);

    assert_eq!(restrict_session_log_permissions(root), 1);
    assert_eq!(mode(&root.join("read-only.json")), 0o400);
    assert_eq!(mode(&root.join("already-private.json")), 0o400);
}

#[test]
fn symbolic_links_are_neither_followed_nor_changed() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let root = temporary.path().join("root");
    let outside = temporary.path().join("outside");
    directory(&root, 0o700);
    directory(&outside, 0o755);
    file(&outside.join("secret.json"), 0o644);
    std::os::unix::fs::symlink(outside.join("secret.json"), root.join("file-link"))
        .expect("file symlink");
    std::os::unix::fs::symlink(&outside, root.join("dir-link")).expect("directory symlink");

    assert_eq!(restrict_session_log_permissions(&root), 0);
    assert_eq!(mode(&outside), 0o755);
    assert_eq!(mode(&outside.join("secret.json")), 0o644);
}

#[test]
fn the_interior_of_a_unified_session_directory_is_not_walked() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let root = temporary.path();
    set_mode(root, 0o700);
    let unified = root.join("unified");
    directory(&unified, 0o755);
    directory(&unified.join("session-a"), 0o755);
    file(&unified.join("session-a").join("log.jsonl"), 0o644);
    directory(&unified.join("session-a").join("inner"), 0o755);
    // The same shape elsewhere is walked, for contrast.
    directory(&root.join("logs"), 0o755);
    directory(&root.join("logs").join("session-b"), 0o755);
    file(
        &root.join("logs").join("session-b").join("log.jsonl"),
        0o644,
    );

    assert_eq!(restrict_session_log_permissions(root), 5);
    assert_eq!(mode(&unified), 0o700);
    assert_eq!(mode(&unified.join("session-a")), 0o700);
    assert_eq!(mode(&unified.join("session-a").join("log.jsonl")), 0o644);
    assert_eq!(mode(&unified.join("session-a").join("inner")), 0o755);
    assert_eq!(
        mode(&root.join("logs").join("session-b").join("log.jsonl")),
        0o600
    );
}

#[test]
fn a_missing_or_non_directory_root_changes_nothing() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let missing = temporary.path().join("missing");
    assert_eq!(restrict_session_log_permissions(&missing), 0);
    assert!(!missing.exists());

    let plain = temporary.path().join("plain.json");
    file(&plain, 0o644);
    assert_eq!(restrict_session_log_permissions(&plain), 0);
    assert_eq!(mode(&plain), 0o644);
}
