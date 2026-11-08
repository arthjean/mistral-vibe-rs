//! Guards that keep the pin a single edit.
//!
//! US-099 consolidated eleven copies of the reference commit into two: the Rust
//! constant and its Python mirror. These tests fail when a third appears, or
//! when the two that remain disagree, which is the drift the constants used to
//! accumulate silently. A rolling re-pin adds the commit it leaves,
//! `PREVIOUS_COMMIT`, to both sources under the same guard: four values, two
//! by two equal, and no third copy of either commit.

use std::fs;
use std::path::{Path, PathBuf};

use super::{PREVIOUS_COMMIT, REFERENCE_COMMIT, REFERENCE_VERSION, RESTORE_COMMAND, pin_commits};

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

/// `path` relative to `root`, separated by `/` on every platform, which is how
/// the expectations below spell repository paths.
fn slash_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
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

    // Every commit of the pin: the target, and the previous commit during a
    // transition.
    for commit in pin_commits() {
        let mut copies = Vec::new();
        for path in scanned_sources(&root) {
            let Ok(contents) = fs::read_to_string(&path) else {
                continue;
            };
            if !contents.contains(commit) || allowed.contains(&path) {
                continue;
            }
            copies.push(slash_relative(&root, &path));
        }
        copies.sort();

        assert!(
            copies.is_empty(),
            "the pin's commit {commit} is spelled out outside {PIN_SOURCES:?}: {}. Cite \
             `vibe_core::parity::REFERENCE_COMMIT` or `PREVIOUS_COMMIT` (in Python, \
             `scripts/parity/pin.EXPECTED_COMMIT` or `PREVIOUS_COMMIT`) instead, so a re-pin \
             stays one edit per language",
            copies.join(", ")
        );

        for source in &allowed {
            let contents = fs::read_to_string(source).expect("a pin source is readable");
            assert!(
                contents.contains(commit),
                "{} no longer carries the pin's commit {commit}",
                source.display()
            );
        }
    }
}

#[test]
fn the_previous_commit_is_empty_or_a_full_commit_other_than_the_target() {
    assert!(
        PREVIOUS_COMMIT.is_empty()
            || (PREVIOUS_COMMIT.len() == 40
                && PREVIOUS_COMMIT.bytes().all(|byte| byte.is_ascii_hexdigit())
                && PREVIOUS_COMMIT != REFERENCE_COMMIT),
        "`PREVIOUS_COMMIT` is `{PREVIOUS_COMMIT}`: empty outside a transition, otherwise the full \
         commit the re-pin left"
    );
}

#[test]
fn no_rust_module_redeclares_its_own_reference_commit() {
    let root = repo_root();
    // Composed rather than written whole so this test does not match its own
    // source. The declaration form is what matters: a module that merely
    // *names* the constant, through an import or a message, is not a copy.
    for name in ["REFERENCE_COMMIT", "PREVIOUS_COMMIT"] {
        let declaration = format!("const {name}: &str");

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
            declarations.push(slash_relative(&root, &path));
        }
        declarations.sort();

        assert_eq!(
            declarations,
            vec!["crates/vibe-core/src/parity.rs".to_owned()],
            "`grep -rn '{name}: &str' crates` must return the single definition every other \
             oracle cites"
        );
    }
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
    assert_eq!(
        quoted("PREVIOUS_COMMIT ="),
        PREVIOUS_COMMIT,
        "the capture scripts and the replays would accept a different previous commit"
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
        let declares = contents.lines().any(|line| {
            (line.starts_with("EXPECTED_COMMIT =") || line.starts_with("PREVIOUS_COMMIT ="))
                && line.contains('"')
        });
        if !declares {
            continue;
        }
        declarations.push(slash_relative(&root, &path));
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
