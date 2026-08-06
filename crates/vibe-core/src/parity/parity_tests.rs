//! Guards that keep the pin a single edit.
//!
//! US-099 consolidated eleven copies of the reference commit into two: the Rust
//! constant and its Python mirror. These tests fail when a third appears, or
//! when the two that remain disagree, which is the drift the constants used to
//! accumulate silently.

use std::fs;
use std::path::{Path, PathBuf};

use super::{REFERENCE_COMMIT, REFERENCE_VERSION, RESTORE_COMMAND, off_pin_reason};

/// The two files allowed to spell the commit out, relative to the repository
/// root. Every other oracle cites one of them.
const PIN_SOURCES: [&str; 2] = ["crates/vibe-core/src/parity.rs", "scripts/parity/pin.py"];

/// Where a source copy of the pin could hide. Committed corpora carry the
/// commit as captured data, so only code is scanned.
const SCANNED_ROOTS: [&str; 2] = ["crates", "scripts"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

/// Every `.rs` and `.py` file under [`SCANNED_ROOTS`], repository-relative.
fn scanned_sources(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending: Vec<PathBuf> = SCANNED_ROOTS.iter().map(|entry| root.join(entry)).collect();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // `target` holds build output and `__pycache__` compiled
                // Python; neither is authored here.
                let skipped = matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some("target" | "__pycache__" | ".git")
                );
                if !skipped {
                    pending.push(path);
                }
                continue;
            }
            let extension = path.extension().and_then(|value| value.to_str());
            if matches!(extension, Some("rs" | "py")) {
                found.push(path);
            }
        }
    }
    found
}

#[test]
fn the_reference_commit_is_written_in_exactly_two_places() {
    let root = repo_root();
    let allowed = PIN_SOURCES
        .iter()
        .map(|entry| root.join(entry))
        .collect::<Vec<_>>();

    let mut copies = Vec::new();
    for path in scanned_sources(&root) {
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        if !contents.contains(REFERENCE_COMMIT) || allowed.contains(&path) {
            continue;
        }
        copies.push(
            path.strip_prefix(&root)
                .unwrap_or(&path)
                .display()
                .to_string(),
        );
    }
    copies.sort();

    assert!(
        copies.is_empty(),
        "the reference commit is spelled out outside {PIN_SOURCES:?}: {}. Cite \
         `vibe_core::parity::REFERENCE_COMMIT` or `scripts/parity/pin.EXPECTED_COMMIT` instead, so \
         a re-pin stays one edit per language",
        copies.join(", ")
    );

    for source in allowed {
        let contents = fs::read_to_string(&source).expect("a pin source is readable");
        assert!(
            contents.contains(REFERENCE_COMMIT),
            "{} no longer carries the pin",
            source.display()
        );
    }
}

#[test]
fn no_rust_module_redeclares_its_own_reference_commit() {
    let root = repo_root();
    // Composed rather than written whole so this test does not match its own
    // source. The declaration form is what matters: a module that merely
    // *names* the constant, through an import or a message, is not a copy.
    let declaration = format!("const {}: &str", "REFERENCE_COMMIT");

    let mut declarations = Vec::new();
    for path in scanned_sources(&root) {
        if path.extension().and_then(|value| value.to_str()) != Some("rs") {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        if !contents.contains(&declaration) {
            continue;
        }
        declarations.push(
            path.strip_prefix(&root)
                .unwrap_or(&path)
                .display()
                .to_string(),
        );
    }
    declarations.sort();

    assert_eq!(
        declarations,
        vec!["crates/vibe-core/src/parity.rs".to_owned()],
        "`grep -rn 'REFERENCE_COMMIT: &str' crates` must return the single definition every other \
         oracle cites"
    );
}

#[test]
fn the_python_mirror_agrees_with_the_rust_pin() {
    let root = repo_root();
    let source = root.join("scripts/parity/pin.py");
    let contents = fs::read_to_string(&source).expect("the Python pin is readable");

    let quoted = |key: &str| -> String {
        let line = contents
            .lines()
            .find(|line| line.starts_with(key))
            .unwrap_or_else(|| panic!("{key} is declared in scripts/parity/pin.py"));
        line.split('"')
            .nth(1)
            .unwrap_or_else(|| panic!("{key} carries a quoted value"))
            .to_owned()
    };

    assert_eq!(
        quoted("EXPECTED_COMMIT ="),
        REFERENCE_COMMIT,
        "the capture scripts would measure a different commit than the replays assert"
    );
    assert_eq!(quoted("EXPECTED_VERSION ="), REFERENCE_VERSION);
}

#[test]
fn no_capture_script_redeclares_the_pin() {
    let root = repo_root();
    let mut declarations = Vec::new();
    for path in scanned_sources(&root) {
        if path.extension().and_then(|value| value.to_str()) != Some("py") {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let declares = contents
            .lines()
            .any(|line| line.starts_with("EXPECTED_COMMIT =") && line.contains('"'));
        if !declares {
            continue;
        }
        declarations.push(
            path.strip_prefix(&root)
                .unwrap_or(&path)
                .display()
                .to_string(),
        );
    }
    declarations.sort();

    assert_eq!(
        declarations,
        vec!["scripts/parity/pin.py".to_owned()],
        "a capture script declares its own pin instead of importing `scripts/parity/pin`"
    );
}

#[test]
fn the_restore_command_names_the_pinned_commit() {
    assert!(
        RESTORE_COMMAND.contains(REFERENCE_COMMIT),
        "the documented restore command must target the pin it restores"
    );
    assert!(RESTORE_COMMAND.starts_with("git -C "));
}

#[test]
fn an_off_pin_checkout_names_both_commits_and_the_way_back() {
    // The repository itself stands in for a checkout sitting at another commit,
    // which is exactly the state the message has to describe.
    let root = repo_root();
    let reason =
        off_pin_reason(&root, "example").expect("this repository is not the pinned reference");

    assert!(
        reason.contains(REFERENCE_COMMIT),
        "the skip message must name the expected commit: {reason}"
    );
    assert!(
        reason.contains(RESTORE_COMMAND),
        "the skip message must name the way back: {reason}"
    );
    assert!(
        reason.contains("example"),
        "the skip message must name the probe that went quiet: {reason}"
    );

    let head = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("this repository answers rev-parse");
    let head = String::from_utf8(head.stdout).expect("a revision is UTF-8");
    assert!(
        reason.contains(head.trim()),
        "the skip message must name the commit actually found: {reason}"
    );
}

#[test]
fn a_missing_checkout_skips_without_naming_a_commit_it_never_read() {
    let reason = off_pin_reason(Path::new("/nonexistent/reference/checkout"), "example")
        .expect("a missing checkout cannot be the oracle");
    assert!(reason.contains("no reference checkout"), "{reason}");
    assert!(
        !reason.contains(REFERENCE_COMMIT),
        "nothing was read, so no commit comparison can be reported: {reason}"
    );
}
