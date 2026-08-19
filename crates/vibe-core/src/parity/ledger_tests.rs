//! Guards that keep the accepted-divergence ledger honest.
//!
//! `docs/parity.md` records every divergence this port keeps on purpose, and
//! each row names what in the repository holds it in place. A row whose
//! evidence has been renamed, moved or deleted is a decision nothing enforces
//! any more, which is exactly the state the table exists to prevent, so this
//! module reads the table and resolves every pointer it names: a repository
//! path has to exist, and a symbol, ledger key or test name has to still
//! appear in the sources.
//!
//! Reference paths are skipped: the checkout is optional here, and a missing
//! checkout must never fail `cargo test`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const SCORECARD: &str = "docs/parity.md";
const SECTION: &str = "## Accepted divergences";

/// Where a pointer's name can legitimately live. Committed corpora carry
/// ledger keys as data, so JSON counts as a source here.
const SCANNED_ROOTS: [&str; 2] = ["crates", "scripts"];
const SCANNED_EXTENSIONS: [&str; 4] = ["rs", "py", "json", "toml"];

/// Path prefixes that make a backticked token a repository path.
const PATH_PREFIXES: [&str; 3] = ["crates/", "scripts/", "docs/"];
/// Root files the table names directly.
const ROOT_FILES: [&str; 2] = ["CHANGELOG.md", "NOTICE"];

/// A segment shorter than this is too generic to resolve, so it is not
/// required to appear anywhere.
const MINIMUM_SEGMENT: usize = 4;

/// Rows whose evidence deliberately sits outside this repository, each with
/// why. An exempted row that starts naming something resolvable fails as a
/// stale entry, so the list cannot quietly absorb a row that lost its
/// artifact.
const EVIDENCE_OUTSIDE_THE_REPOSITORY: [(&str, &str); 1] = [(
    "A third-party score weighing description text",
    "the row's measurement is a byte count taken in the reference checkout, which `NOTICE` keeps \
     out of this repository",
)];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

/// Every table row of the accepted-divergences section as `(part, evidence)`.
fn ledger_rows(document: &str) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    let mut inside = false;
    for line in document.lines() {
        if line.starts_with("## ") {
            inside = line.trim() == SECTION;
            continue;
        }
        if !inside || !line.starts_with('|') {
            continue;
        }
        let cells = line
            .trim_matches('|')
            .split(" | ")
            .map(str::trim)
            .collect::<Vec<_>>();
        // The header and its separator carry no evidence.
        if cells.len() < 3 || cells[0] == "Part" || cells[0].starts_with("---") {
            continue;
        }
        // A row that cites the row above shares its artifact, which is what
        // the table means and what has to resolve for both.
        let evidence = if cells[2].starts_with("Same as above") {
            let inherited = rows
                .last()
                .map(|(_, previous): &(String, String)| previous.clone())
                .unwrap_or_default();
            format!("{inherited} {}", cells[2])
        } else {
            cells[2].to_owned()
        };
        rows.push((cells[0].to_owned(), evidence));
    }
    rows
}

/// The backticked spans of a table cell.
fn backticked(cell: &str) -> Vec<String> {
    cell.split('`')
        .skip(1)
        .step_by(2)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether a token names a file in this repository rather than a symbol.
fn repository_path(token: &str) -> Option<String> {
    let path = token.split(':').next().unwrap_or(token).trim();
    if ROOT_FILES.contains(&path) {
        return Some(path.to_owned());
    }
    PATH_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
        .then(|| path.to_owned())
}

/// The identifier-shaped segments of a token that has to resolve somewhere.
fn segments(token: &str) -> Vec<String> {
    token
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|segment| segment.len() >= MINIMUM_SEGMENT)
        .filter(|segment| {
            segment
                .chars()
                .next()
                .is_some_and(|first| first.is_alphabetic() || first == '_')
        })
        .map(str::to_owned)
        .collect()
}

/// Every `.rs`, `.py`, `.json` and `.toml` file under the scanned roots.
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
            if extension.is_some_and(|value| SCANNED_EXTENSIONS.contains(&value)) {
                found.push(path);
            }
        }
    }
    found
}

/// Every identifier-shaped word the sources contain, which is what a ledger
/// pointer has to resolve against.
fn source_vocabulary(root: &Path) -> BTreeSet<String> {
    let mut words = BTreeSet::new();
    for path in scanned_sources(root) {
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        for word in
            contents.split(|character: char| !character.is_alphanumeric() && character != '_')
        {
            if word.len() >= MINIMUM_SEGMENT {
                words.insert(word.to_owned());
            }
        }
    }
    words
}

/// What a resolution pass found: pointers that no longer resolve, and rows
/// that name nothing resolvable at all.
#[derive(Debug, Default)]
struct Findings {
    unresolved: Vec<String>,
    unevidenced: Vec<String>,
}

/// Resolves every pointer the table's rows name against `root` and
/// `vocabulary`.
fn resolve(rows: &[(String, String)], root: &Path, vocabulary: &BTreeSet<String>) -> Findings {
    let mut findings = Findings::default();
    let Findings {
        unresolved,
        unevidenced,
    } = &mut findings;
    for (part, evidence) in rows {
        let mut checked = 0usize;
        for token in backticked(evidence) {
            // The reference checkout is optional on this machine, so a
            // reference path is cited rather than resolved.
            if token.starts_with("vibe/") || token.starts_with('/') {
                continue;
            }
            // A commit is the pin, cited rather than resolved; the pin has its
            // own guard in `parity_tests`.
            if token.len() >= 7 && token.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            if let Some(path) = repository_path(&token) {
                checked += 1;
                if !root.join(&path).exists() {
                    unresolved.push(format!("{part}: `{path}` no longer exists"));
                }
                continue;
            }
            let missing = segments(&token)
                .into_iter()
                .filter(|segment| !vocabulary.contains(segment))
                .collect::<Vec<_>>();
            if segments(&token).is_empty() {
                continue;
            }
            checked += 1;
            if !missing.is_empty() {
                unresolved.push(format!(
                    "{part}: `{token}` names {missing:?}, which no source carries any more"
                ));
            }
        }
        let exempt = EVIDENCE_OUTSIDE_THE_REPOSITORY
            .iter()
            .any(|(row, _)| row == part);
        match (checked, exempt) {
            (0, false) => unevidenced.push(part.clone()),
            (0, true) => {}
            (_, true) => unresolved.push(format!(
                "{part}: listed as evidenced outside the repository, but it now names something \
                 this repository holds, so the exemption is stale"
            )),
            (_, false) => {}
        }
    }
    findings
}

#[test]
fn every_accepted_divergence_still_names_evidence_that_exists() {
    let root = repo_root();
    let document = fs::read_to_string(root.join(SCORECARD)).expect("the scorecard is readable");
    let rows = ledger_rows(&document);
    assert!(
        rows.len() >= 30,
        "the accepted-divergences table holds {} rows, which is fewer than it ever had; the \
         parser or the section moved",
        rows.len()
    );
    let findings = resolve(&rows, &root, &source_vocabulary(&root));

    assert!(
        findings.unevidenced.is_empty(),
        "these accepted divergences name no resolvable evidence: {:?}. A row without an artifact \
         is a decision nothing enforces",
        findings.unevidenced
    );
    assert!(
        findings.unresolved.is_empty(),
        "these accepted divergences point at evidence that no longer exists:\n{}",
        findings.unresolved.join("\n")
    );
    println!(
        "parity ledger: {} accepted divergences, every pointer resolves",
        rows.len()
    );
}

/// The guard is only worth having if it fails on a row whose evidence went
/// away, so the failure itself is exercised rather than assumed.
#[test]
fn a_row_whose_evidence_disappeared_is_reported() {
    let root = repo_root();
    let vocabulary = source_vocabulary(&root);
    // Composed rather than written whole, so this test's own source does not
    // put the symbol it claims is missing into the scanned vocabulary.
    let removed = format!("Removed{}Workspace", "FromThe");
    let document = format!(
        "\
## Accepted divergences

| Part | Reason | Evidence |
|---|---|---|
| A deleted file | reason | `crates/vibe-core/src/nothing_here.rs` |
| A deleted symbol | reason | `{removed}` in `crates/vibe-core/src/parity.rs` |
| A row with no artifact | reason | nothing anyone can open |
| A living row | reason | `REFERENCE_COMMIT` in `crates/vibe-core/src/parity.rs` |
"
    );
    let rows = ledger_rows(&document);
    assert_eq!(rows.len(), 4);
    let findings = resolve(&rows, &root, &vocabulary);
    assert_eq!(
        findings
            .unresolved
            .iter()
            .map(|line| line.split(':').next().unwrap_or_default())
            .collect::<Vec<_>>(),
        ["A deleted file", "A deleted symbol"]
    );
    assert_eq!(findings.unevidenced, ["A row with no artifact".to_owned()]);
}
