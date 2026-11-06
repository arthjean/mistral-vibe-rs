//! The manifest meta-test: every committed corpus is listed, tied to a driver,
//! a test, a scorecard row and an accepted commit, and unchanged since the
//! capture tool wrote it.
//!
//! The checks read the manifest and the corpora directly, so they hold on a
//! machine with no Python and no reference checkout. The negative tests below
//! feed the same checks a corrupted entry, to pin the message each failure
//! names: the corpus, and the command that resolves it.

use std::collections::BTreeSet;
use std::process::Command;

use serde_json::json;

use super::REFERENCE_COMMIT;
use super::manifest::{
    Kind, Manifest, ManifestEntry, REPIN, content_problems, families, is_corpus_location, load,
    minimum_floor, names_reference, recorded_commit,
};
use super::replay::{repository, scorecard_rows};

/// Every JSON file git tracks under a crate's `tests/` directory, where a
/// corpus has to be listed.
fn tracked_json(tracked: &BTreeSet<String>) -> Vec<String> {
    tracked
        .iter()
        .filter(|path| is_corpus_location(path))
        .cloned()
        .collect()
}

/// Every file git tracks under `crates/`: a listed corpus may sit outside
/// `tests/` when the product embeds it.
fn tracked_files() -> BTreeSet<String> {
    let listing = Command::new("git")
        .arg("-C")
        .arg(repository())
        .args(["ls-files", "-z", "--", "crates"])
        .output()
        .expect("git lists the tracked files");
    assert!(
        listing.status.success(),
        "git ls-files failed, so the manifest cannot be checked: {}",
        String::from_utf8_lossy(&listing.stderr)
    );
    String::from_utf8(listing.stdout)
        .expect("tracked paths are UTF-8")
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The tracked corpora `manifest` does not list, with the command that adds
/// each one.
fn unlisted(
    manifest: &Manifest,
    tracked: &[String],
    read: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let listed: BTreeSet<&str> = manifest
        .entries
        .iter()
        .map(|entry| entry.corpus.as_str())
        .collect();
    tracked
        .iter()
        .filter(|path| !listed.contains(path.as_str()))
        .filter(|path| {
            read(path)
                .and_then(|text| serde_json::from_str(&text).ok())
                .is_some_and(|corpus| names_reference(&corpus))
        })
        .map(|path| {
            format!(
                "{path} names the reference it came from but is not in the manifest; add it with \
                 `{REPIN} add {path} --driver \"<script> <args>\" --test <file>::<function> \
                 --row <n>`"
            )
        })
        .collect()
}

/// Whether `source` declares `function` under a `#[test]` attribute (or an
/// async runtime's `#[...::test]`), any other attributes in between.
fn resolves_to_test(source: &str, function: &str) -> bool {
    let needle = format!("fn {function}");
    source.match_indices(&needle).any(|(start, _)| {
        let after = &source[start + needle.len()..];
        if !after.trim_start().starts_with(['(', '<']) {
            return false;
        }
        let mut before = source[..start].trim_end();
        for keyword in ["async", "pub"] {
            before = before.strip_suffix(keyword).map_or(before, str::trim_end);
        }
        let mut attributes = Vec::new();
        while before.ends_with(']') {
            let mut depth = 0usize;
            let Some(open) = before.char_indices().rev().find_map(|(index, character)| {
                match character {
                    ']' => depth += 1,
                    '[' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(index);
                        }
                    }
                    _ => {}
                }
                None
            }) else {
                break;
            };
            if open == 0 || !before[..open].ends_with('#') {
                break;
            }
            attributes.push(&before[open - 1..]);
            before = before[..open - 1].trim_end();
        }
        attributes.iter().any(|attribute| {
            let inner = attribute.trim_start_matches("#[");
            inner.starts_with("test")
                && !inner[4..].starts_with(|c: char| c.is_alphanumeric() || c == '_')
                || inner.split_once("::test").is_some_and(|(runtime, rest)| {
                    runtime.chars().all(|c| c.is_alphanumeric() || c == '_')
                        && !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_')
                })
        })
    })
}

/// Every rule `entry` breaks besides its content: a test that resolves, a row
/// the scorecard carries, a driver for a captured corpus.
fn entry_problems(entry: &ManifestEntry, rows: &BTreeSet<&str>) -> Vec<String> {
    let mut problems = Vec::new();
    let corpus = &entry.corpus;
    let resolved = entry.test.split_once("::").is_some_and(|(file, function)| {
        std::fs::read_to_string(repository().join(file))
            .is_ok_and(|source| resolves_to_test(&source, function))
    });
    if !resolved {
        problems.push(format!(
            "{corpus}: the manifest names the test `{}`, which is no `#[test]` function in that \
             file",
            entry.test
        ));
    }
    if !rows.contains(entry.row.as_str()) {
        problems.push(format!(
            "{corpus}: the manifest names row {}, which docs/parity.md does not carry",
            entry.row
        ));
    }
    if entry.kind != Kind::Fixture {
        let runnable = entry
            .driver
            .as_ref()
            .and_then(|driver| driver.first())
            .is_some_and(|program| program == "cargo" || repository().join(program).is_file());
        if !runnable {
            problems.push(format!(
                "{corpus}: a captured corpus needs the driver that regenerates it, `cargo` or a \
                 script that exists"
            ));
        }
        if entry.kind == Kind::Assembled && entry.output.is_some() {
            problems.push(format!(
                "{corpus}: an assembled corpus is merged by hand, so its driver has no output flag"
            ));
        }
    }
    problems
}

#[test]
fn every_committed_corpus_is_listed_at_an_accepted_commit_and_unchanged_since_its_capture() {
    let manifest = load();
    let root = repository();
    let scorecard =
        std::fs::read_to_string(root.join("docs/parity.md")).expect("the scorecard is readable");
    let rows = scorecard_rows(&scorecard);
    let tracked = tracked_files();
    let accepted = manifest.accepted();

    let mut problems = unlisted(&manifest, &tracked_json(&tracked), |path| {
        std::fs::read_to_string(root.join(path)).ok()
    });
    let names: Vec<&str> = manifest
        .entries
        .iter()
        .map(|entry| entry.corpus.as_str())
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    if names != sorted {
        problems
            .push("the manifest entries are not sorted by corpus path, or one repeats".to_owned());
    }
    for entry in &manifest.entries {
        if !tracked.contains(&entry.corpus) {
            problems.push(format!(
                "{} is in the manifest but not tracked by git",
                entry.corpus
            ));
            continue;
        }
        problems.extend(entry_problems(entry, &rows));
        let text =
            std::fs::read_to_string(root.join(&entry.corpus)).expect("a listed corpus is readable");
        problems.extend(content_problems(entry, &text, &accepted));
    }
    for commit in &manifest.transition {
        if commit == REFERENCE_COMMIT || commit.len() != 40 {
            problems.push(format!(
                "the transition list names {commit}, which is the pin or not a full commit"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "the parity manifest and the corpora disagree:\n{}",
        problems.join("\n")
    );
}

fn entry(text: &str) -> ManifestEntry {
    let corpus: serde_json::Value = serde_json::from_str(text).expect("the sample parses");
    let counts = families(&corpus);
    ManifestEntry {
        corpus: "crates/sample/tests/sample/corpus.json".to_owned(),
        kind: Kind::Corpus,
        driver: Some(vec!["scripts/parity/sample.py".to_owned()]),
        output: Some("--output".to_owned()),
        output_root: None,
        test: "crates/sample/tests/sample.rs::replays".to_owned(),
        row: "1".to_owned(),
        commit: recorded_commit(&corpus).map(str::to_owned),
        off_target: false,
        sha256: Some(crate::text::sha256_hex(text.as_bytes())),
        count: Some(counts.values().sum()),
        floors: counts
            .into_iter()
            .map(|(family, count)| (family, minimum_floor(count)))
            .collect(),
        note: None,
    }
}

fn sample(commit: &str, scenarios: usize) -> String {
    let scenarios: Vec<_> = (0..scenarios)
        .map(|index| json!({"name": format!("s{index}")}))
        .collect();
    serde_json::to_string(&json!({
        "schemaVersion": 1,
        "reference": {"commit": commit},
        "scenarios": scenarios,
    }))
    .expect("the sample serializes")
}

#[test]
fn a_corpus_edited_outside_the_capture_tool_names_the_only_sanctioned_fix() {
    let captured = sample(REFERENCE_COMMIT, 20);
    let listed = entry(&captured);
    let edited = captured.replace("s19", "s99");
    let problems = content_problems(&listed, &edited, &[REFERENCE_COMMIT]);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains(&listed.corpus)
            && problems[0].contains(&format!("{REPIN} capture {}", listed.corpus))
            && problems[0].contains("only sanctioned way"),
        "{}",
        problems[0]
    );
}

#[test]
fn a_corpus_off_every_accepted_commit_names_both_commits() {
    let other = "0123456789abcdef0123456789abcdef01234567";
    let text = sample(other, 3);
    let problems = content_problems(&entry(&text), &text, &[REFERENCE_COMMIT]);
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains(other) && problem.contains(REFERENCE_COMMIT)),
        "{problems:?}"
    );
    let overridden = sample("server-override", 3);
    let problems = content_problems(&entry(&overridden), &overridden, &[REFERENCE_COMMIT]);
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("server-override")),
        "{problems:?}"
    );
}

#[test]
fn a_transition_commit_is_accepted_while_the_list_names_it() {
    let previous = "0123456789abcdef0123456789abcdef01234567";
    let text = sample(previous, 3);
    assert!(content_problems(&entry(&text), &text, &[REFERENCE_COMMIT, previous]).is_empty());
}

#[test]
fn a_shrunken_or_empty_corpus_fails_its_floor() {
    let captured = sample(REFERENCE_COMMIT, 40);
    let mut listed = entry(&captured);
    assert_eq!(listed.floors["scenarios"], 38);
    let shrunk = sample(REFERENCE_COMMIT, 30);
    listed.sha256 = Some(crate::text::sha256_hex(shrunk.as_bytes()));
    listed.count = Some(30);
    let problems = content_problems(&listed, &shrunk, &[REFERENCE_COMMIT]);
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("holds 30 scenarios, below the floor of 38")),
        "{problems:?}"
    );
    let empty = sample(REFERENCE_COMMIT, 0);
    let problems = content_problems(&entry(&empty), &empty, &[REFERENCE_COMMIT]);
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("case lists are empty")),
        "{problems:?}"
    );
}

#[test]
fn an_unlisted_corpus_names_the_command_that_adds_it() {
    let manifest = Manifest {
        schema_version: 1,
        note: None,
        transition: Vec::new(),
        entries: Vec::new(),
    };
    let path = "crates/sample/tests/sample/corpus.json".to_owned();
    let found = unlisted(&manifest, std::slice::from_ref(&path), |_| {
        Some(sample(REFERENCE_COMMIT, 1))
    });
    assert_eq!(found.len(), 1);
    assert!(
        found[0].contains(&path) && found[0].contains(&format!("{REPIN} add {path}")),
        "{}",
        found[0]
    );
    let uncommitted = unlisted(&manifest, std::slice::from_ref(&path), |_| {
        Some("{\"reference\": {\"path\": \"vibe/core\"}, \"cases\": []}".to_owned())
    });
    assert_eq!(
        uncommitted.len(),
        1,
        "a file with a `reference` key is a corpus before it records a commit"
    );
    let fixture = unlisted(&manifest, std::slice::from_ref(&path), |_| {
        Some("{\"cases\": []}".to_owned())
    });
    assert!(
        fixture.is_empty(),
        "a file that names no reference is not a corpus"
    );
}

#[test]
fn a_test_name_resolves_only_to_a_function_under_a_test_attribute() {
    let source = "\
#[test]
#[cfg_attr(windows, ignore = \"windows by design: reasons [with brackets]\")]
fn replays() {}

#[tokio::test]
async fn replays_async() {}

fn helper() {}

#[test_case]
fn not_a_test() {}
";
    assert!(resolves_to_test(source, "replays"));
    assert!(resolves_to_test(source, "replays_async"));
    assert!(!resolves_to_test(source, "helper"));
    assert!(!resolves_to_test(source, "not_a_test"));
    assert!(!resolves_to_test(source, "missing"));
    assert!(
        !resolves_to_test(source, "replay"),
        "a prefix is not the function"
    );
}

#[test]
fn a_manifest_entry_whose_test_no_longer_resolves_is_named() {
    let text = sample(REFERENCE_COMMIT, 1);
    let mut listed = entry(&text);
    listed.test = "crates/vibe-core/src/parity/manifest_tests.rs::no_such_test".to_owned();
    listed.driver = Some(vec!["scripts/parity/repin.py".to_owned()]);
    let rows = BTreeSet::from(["1"]);
    let problems = entry_problems(&listed, &rows);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains(&listed.corpus) && problems[0].contains("no_such_test"),
        "{}",
        problems[0]
    );
}

#[test]
fn the_python_verifier_agrees_with_the_meta_test() {
    let output = Command::new("python3")
        .arg(repository().join("scripts/parity/repin.py"))
        .arg("verify")
        .current_dir(repository())
        .output()
        .expect("python3 runs repin.py");
    assert!(
        output.status.success(),
        "`{REPIN} verify` disagrees with the meta-test: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
