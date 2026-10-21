//! US-108: the directory the permission chain grants before consulting a list.

use super::*;

/// Two runtimes of one session never share a scratchpad: each opening is a
/// new directory named after the short session identifier and a random part,
/// and a dropped one is gone with what it held.
#[test]
fn every_opening_is_a_new_private_directory_removed_when_dropped() {
    let first = Scratchpad::open("0123456789abcdef").expect("the first scratchpad opens");
    let second = Scratchpad::open("0123456789abcdef").expect("the second scratchpad opens");

    assert_ne!(first.path(), second.path());
    let name = first
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .expect("a named directory");
    let random = name
        .strip_prefix(&format!("{SCRATCHPAD_PREFIX}01234567-"))
        .expect("the reference's prefix and short identifier");
    assert_eq!(random.len(), 8, "{name}");
    assert_eq!(first.path().parent(), Some(std::env::temp_dir().as_path()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(first.path())
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    let path = first.path().to_owned();
    std::fs::write(path.join("notes.txt"), "kept until the runtime ends").expect("a note");
    drop(first);
    assert!(!path.exists());
    assert!(second.path().is_dir());
}

#[test]
fn a_path_inside_the_scratchpad_is_recognized_and_one_outside_is_not() {
    let held = Scratchpad::open("scratchpad-probe-recognized").expect("the scratchpad opens");
    let scratchpad = held.path().to_owned();
    let outside = tempfile::tempdir().expect("outside");

    assert!(is_scratchpad_path(
        &scratchpad.join("notes.txt"),
        Some(&scratchpad)
    ));
    assert!(is_scratchpad_path(
        &scratchpad.join("nested/deeper.txt"),
        Some(&scratchpad)
    ));
    assert!(!is_scratchpad_path(
        &outside.path().join("notes.txt"),
        Some(&scratchpad)
    ));
    // A session without a scratchpad grants nothing.
    assert!(!is_scratchpad_path(&scratchpad.join("notes.txt"), None));

    // Removing a scratchpad already gone is not an error.
    std::fs::remove_dir(&scratchpad).expect("removed under the runtime");
    drop(held);
    assert!(!scratchpad.exists());
}
