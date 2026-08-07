//! US-108: the directory the permission chain grants before consulting a list.

use super::*;

/// Two sessions never share one scratchpad, and one session finds the same
/// directory twice, which is what lets a resumed session keep its files.
#[test]
fn a_scratchpad_is_named_after_its_session_and_is_stable() {
    let first = scratchpad_path("0123456789abcdef");
    let second = scratchpad_path("0123456789abcdef");
    let other = scratchpad_path("fedcba9876543210");

    assert_eq!(first, second);
    assert_ne!(first, other);
    assert!(
        first
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == format!("{SCRATCHPAD_PREFIX}01234567")),
        "{}",
        first.display()
    );
}

#[test]
fn a_path_inside_the_scratchpad_is_recognized_and_one_outside_is_not() {
    let session = "scratchpad-probe-recognized";
    let scratchpad = init_scratchpad(session).expect("the scratchpad opens");
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

    cleanup_scratchpad(Some(&scratchpad));
    assert!(!scratchpad.exists());
    // Removing an absent scratchpad is not an error.
    cleanup_scratchpad(Some(&scratchpad));
}
