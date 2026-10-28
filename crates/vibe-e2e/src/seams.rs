//! The port-only seams a scenario must not pass: a scenario configures the
//! provider through `config.toml`, as a user does, unless it tests the seam
//! itself and says so with `.tests_seam("<seam>")`.

use std::fs;
use std::path::{Path, PathBuf};

/// Flags and variables the reference does not have, which bypass the
/// configuration a user writes. `VIBE_TEST_` matches as a prefix.
pub const PORT_ONLY_SEAMS: &[&str] = &[
    "--api-base",
    "--credential-environment",
    "--fake-response",
    "--provider-style",
    "--model",
    "--session-root",
    "VIBE_API_BASE",
    "VIBE_PROVIDER_STYLE",
    "VIBE_CREDENTIAL_ENV",
    "VIBE_MODEL",
    "VIBE_TEST_",
];

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

/// The seams `source` uses without declaring them, as `line: seam`.
#[must_use]
pub fn violations(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (number, line) in source.lines().enumerate() {
        let bytes = line.as_bytes();
        for seam in PORT_ONLY_SEAMS {
            if source.contains(&format!("tests_seam(\"{seam}\")")) {
                continue;
            }
            let prefix = seam.ends_with('_');
            let mut from = 0;
            while let Some(offset) = line[from..].find(seam) {
                let start = from + offset;
                let end = start + seam.len();
                let before = start == 0 || !is_word(bytes[start - 1]);
                let after = prefix || end == bytes.len() || !is_word(bytes[end]);
                if before && after {
                    found.push(format!("{}: {seam}", number + 1));
                    break;
                }
                from = end;
            }
        }
    }
    found
}

fn sources(directory: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            sources(&path, into);
        } else if path.extension().is_some_and(|e| e == "rs") {
            into.push(path);
        }
    }
}

/// Every undeclared seam in the scenario sources under
/// `<workspace>/crates/*/tests/e2e/`, as `path:line: seam`.
#[must_use]
pub fn scan(workspace: &Path) -> Vec<String> {
    let mut files = Vec::new();
    if let Ok(crates) = fs::read_dir(workspace.join("crates")) {
        for krate in crates.flatten() {
            sources(&krate.path().join("tests/e2e"), &mut files);
        }
    }
    files.sort();
    files
        .iter()
        .flat_map(|file| {
            let source = fs::read_to_string(file).unwrap_or_default();
            let shown = file
                .strip_prefix(workspace)
                .unwrap_or(file)
                .display()
                .to_string();
            violations(&source)
                .into_iter()
                .map(move |violation| format!("{shown}:{violation}"))
        })
        .collect()
}

#[cfg(test)]
mod seams_tests;
