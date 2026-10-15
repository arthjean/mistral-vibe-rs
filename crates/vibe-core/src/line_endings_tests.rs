//! Guards the bytes every production `include_str!` embeds.
//!
//! `.gitattributes` checks text out with LF on every platform, so a Windows
//! build embeds the same prompts and assets as a Linux one. This test fails
//! when an asset slips past that rule (a missing attribute, a file committed
//! with CRLF, a checkout made before the attribute existed), naming the asset
//! and the source file that embeds it.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

/// A source file or directory that only builds under `cfg(test)`: its
/// embedded corpora and fixtures may carry `\r` on purpose.
fn is_test_only(path: &Path) -> bool {
    path.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        name == "tests"
            || name.ends_with("_tests")
            || name.ends_with("_tests.rs")
            || name == "tests.rs"
    })
}

/// Every production `.rs` file under `crates/*/src`.
fn production_sources(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.join("crates")];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap_or(&path);
            if is_test_only(relative) {
                continue;
            }
            if path.is_dir() {
                if path.file_name().and_then(|name| name.to_str()) != Some("target") {
                    pending.push(path);
                }
            } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                found.push(path);
            }
        }
    }
    found
}

/// The literal arguments of every `include_str!` in `source`.
fn embedded_paths(source: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut rest = source;
    while let Some(start) = rest.find("include_str!(") {
        rest = &rest[start + "include_str!(".len()..];
        let argument = rest.trim_start();
        if let Some(literal) = argument.strip_prefix('"')
            && let Some(end) = literal.find('"')
        {
            paths.push(literal[..end].to_owned());
        }
    }
    paths
}

#[test]
fn no_production_asset_embeds_a_carriage_return() {
    let root = repo_root();
    let mut embedded = 0;
    let mut offenders = Vec::new();
    for source in production_sources(&root) {
        let Ok(contents) = fs::read_to_string(&source) else {
            continue;
        };
        let directory = source.parent().unwrap_or(&root);
        for relative in embedded_paths(&contents) {
            let asset = directory.join(&relative);
            let bytes = fs::read(&asset)
                .unwrap_or_else(|error| panic!("{} embeds {relative}: {error}", source.display()));
            embedded += 1;
            if bytes.contains(&b'\r') {
                offenders.push(format!(
                    "{} (embedded by {})",
                    asset.strip_prefix(&root).unwrap_or(&asset).display(),
                    source.strip_prefix(&root).unwrap_or(&source).display()
                ));
            }
        }
    }
    // The scan must have found the prompt library; zero assets means the walk
    // itself broke and the assertion below would pass vacuously.
    assert!(embedded >= 20, "only {embedded} embedded assets were found");
    assert!(
        offenders.is_empty(),
        "embedded assets carry `\\r`; check `.gitattributes` and re-checkout:\n{}",
        offenders.join("\n")
    );
}
