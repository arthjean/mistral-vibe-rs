//! `NOTICE` guard for the labels the authentication methods advertise.
//!
//! The reference names and describes each editor-protocol authentication
//! method in authored prose this repository may not ship, so
//! `scripts/parity/setup_auth.py` records each run as a length plus a SHA-256
//! in the setup-auth corpus and nothing else. This module reads that corpus
//! and holds every label [`super::browser_method`] and
//! [`super::terminal_method`] publish permanently unequal to it: a label whose
//! digest ever conforms is a label that was copied, and the test fails.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use vibe_core::parity::REFERENCE_COMMIT;

use super::{browser_method, terminal_method};

/// The corpus lives with the crate that captures it; this crate reads the same
/// committed file rather than capturing a second copy of five digests.
const CORPUS_RELATIVE: &str = "crates/vibe-core/tests/setup-auth/corpus.json";
const CAPTURE_SCRIPT: &str = "scripts/parity/setup_auth.py";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Corpus {
    reference: Reference,
    acp_auth_prose: Vec<ProseRun>,
}

#[derive(Debug, Deserialize)]
struct Reference {
    commit: String,
}

#[derive(Debug, Deserialize)]
struct ProseRun {
    surface: String,
    run: Digested,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Digested {
    length: usize,
    digest: String,
}

fn digested(value: &str) -> Digested {
    Digested {
        length: value.len(),
        digest: hex::encode(Sha256::digest(value.as_bytes())),
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

fn recorded_runs() -> BTreeMap<String, Digested> {
    let path = repo_root().join(CORPUS_RELATIVE);
    let raw = fs::read_to_string(&path).expect("the setup-auth corpus is readable");
    let corpus: Corpus = serde_json::from_str(&raw).expect("the setup-auth corpus parses");
    assert_eq!(
        corpus.reference.commit, REFERENCE_COMMIT,
        "the corpus was captured from an unpinned reference"
    );
    corpus
        .acp_auth_prose
        .into_iter()
        .map(|entry| (entry.surface, entry.run))
        .collect()
}

/// What this build publishes, keyed by the surface the corpus records.
fn published_runs() -> BTreeMap<String, String> {
    let browser = browser_method("browser-auth");
    let terminal = terminal_method();
    let field = |value: &Value, name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .expect("an advertised method carries a name and a description")
            .to_owned()
    };
    let label = terminal
        .pointer("/_meta/terminal-auth/label")
        .and_then(Value::as_str)
        .expect("the terminal method carries a label")
        .to_owned();
    BTreeMap::from([
        ("browserMethod/name".to_owned(), field(&browser, "name")),
        (
            "browserMethod/description".to_owned(),
            field(&browser, "description"),
        ),
        ("terminalMethod/name".to_owned(), field(&terminal, "name")),
        (
            "terminalMethod/description".to_owned(),
            field(&terminal, "description"),
        ),
        ("terminalMethod/label".to_owned(), label),
    ])
}

#[test]
fn every_advertised_label_is_this_ports_own_prose() {
    let recorded = recorded_runs();
    let published = published_runs();
    assert!(
        !recorded.is_empty(),
        "the corpus records no ACP method label; regenerate it with {CAPTURE_SCRIPT}"
    );
    assert_eq!(
        recorded.keys().collect::<Vec<_>>(),
        published.keys().collect::<Vec<_>>(),
        "the corpus and this build disagree on which labels exist, so a run would go unchecked"
    );
    for (surface, value) in &published {
        // The key sets were just compared, so every published surface is
        // recorded.
        let reference = recorded.get(surface).expect("the corpus records it");
        assert_ne!(
            &digested(value),
            reference,
            "{surface} conforms to the reference's own label, which `NOTICE` forbids shipping"
        );
    }
}

/// The guard is only meaningful while the labels are real sentences: an empty
/// or one-word label would differ from the reference for the wrong reason.
#[test]
fn every_advertised_label_is_a_sentence_of_this_ports_own() {
    for (surface, value) in published_runs() {
        assert!(
            value.contains(' ') && value.len() >= 8,
            "{surface} is not an authored run: {value:?}"
        );
    }
}
