//! How a file tool reads its path argument.

use super::*;

#[test]
fn surrounding_whitespace_is_removed_on_every_platform() {
    assert_eq!(
        normalized("file_path", "  notes.txt \n").expect("path"),
        "notes.txt"
    );
}

#[test]
fn a_git_bash_drive_path_folds_into_its_drive_form() {
    assert_eq!(fold_msys_drive("/c/Users/me"), "C:/Users/me");
    assert_eq!(fold_msys_drive("/d"), "D:/");
    assert_eq!(fold_msys_drive("/d\\x\\y"), "D:/x/y");
    assert_eq!(fold_msys_drive("/cd/x"), "/cd/x");
    assert_eq!(fold_msys_drive("/1/x"), "/1/x");
}

#[test]
fn a_windows_path_needs_both_a_drive_and_a_root_or_neither() {
    for anchored in [
        "C:\\work",
        "C:/work",
        "src\\main.rs",
        "\\\\server\\share\\x",
    ] {
        assert!(!is_unanchored_windows_path(anchored), "{anchored}");
    }
    for unanchored in ["C:work", "\\work", "/work"] {
        assert!(is_unanchored_windows_path(unanchored), "{unanchored}");
    }
}

#[test]
fn a_missing_tail_resolves_lexically_under_its_existing_prefix() {
    let directory = tempfile::tempdir().expect("tempdir");
    let root = std::fs::canonicalize(directory.path()).expect("root");
    std::fs::create_dir(root.join("kept")).expect("directory");
    assert_eq!(
        resolved(&root.join("missing/../kept/./new.txt")),
        root.join("kept/new.txt")
    );
    assert_eq!(resolved(&root.join("kept/..")), root);
}

#[test]
fn a_leading_tilde_names_the_home_directory() {
    let Some(home) = crate::config::user_home_directory() else {
        return;
    };
    assert_eq!(expanded(Path::new("~/notes.txt")), home.join("notes.txt"));
    assert_eq!(expanded(Path::new("~")), home);
    assert_eq!(expanded(Path::new("a/~")), Path::new("a/~"));
}
