//! The attestation gate: what it accepts, what it refuses and how it says so,
//! and that its Rust half reads an attestation exactly as the Python
//! materializer writes it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use super::live::{
    ATTESTATION, CORPUS_PLATFORM, Class, Refusal, decide, environment_root, platform_refusal,
    probe_commit, verify,
};
use super::manifest::{Kind, Manifest, ManifestEntry};
use crate::parity::REFERENCE_COMMIT;
use crate::parity::replay::repository;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// Writes an attestation for every file under `root`'s two trees through the
/// Python materializer, the writer the gate has to agree with.
fn attest_with_python(root: &Path, commit: &str) {
    let script = format!(
        "import json, sys; sys.path.insert(0, {scripts:?}); import materialize; \
         root = __import__('pathlib').Path({root:?}); \
         files = materialize.snapshot(root); \
         (root / materialize.ATTESTATION).write_text(json.dumps({{'schemaVersion': \
         materialize.ATTESTATION_SCHEMA, 'commit': {commit:?}, 'files': files}}))",
        scripts = repository().join("scripts/parity").display().to_string(),
        root = root.display().to_string(),
    );
    let output = Command::new("python3")
        .args(["-c", &script])
        .output()
        .expect("python3 runs");
    assert!(
        output.status.success(),
        "the materializer could not attest the fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A small environment: a source tree, a virtual environment with a package
/// and a bytecode cache, attested by the Python materializer.
fn environment(commit: &str) -> (tempfile::TempDir, PathBuf) {
    let cache = tempfile::tempdir().expect("temporary cache");
    let root = environment_root(cache.path(), commit);
    for (path, text) in [
        ("source/pyproject.toml", "version = \"2.26.0\"\n"),
        ("source/vibe/__init__.py", "VERSION = 1\n"),
        (".venv/pyvenv.cfg", "relocatable = true\n"),
        (".venv/lib/site-packages/vibe/__init__.py", "VERSION = 1\n"),
        (
            ".venv/lib/site-packages/vibe/__pycache__/__init__.pyc",
            "cache",
        ),
    ] {
        let path = root.join(path);
        fs::create_dir_all(path.parent().expect("a parent")).expect("directories");
        fs::write(path, text).expect("fixture file");
    }
    attest_with_python(&root, commit);
    (cache, root)
}

fn refused(root: &Path, commit: &str) -> Refusal {
    verify(root, commit).expect_err("the environment must be refused")
}

/// Moves a file's modification time without touching its content.
fn retime(path: &Path) {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open to retime");
    file.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1))
        .expect("set the modification time");
}

#[test]
fn an_environment_the_materializer_attested_passes_the_rust_gate() {
    let (_cache, root) = environment(COMMIT);
    verify(&root, COMMIT).expect("a fresh attestation verifies");
}

#[test]
fn no_environment_skips_naming_the_commit_and_the_materialize_command() {
    let cache = tempfile::tempdir().expect("temporary cache");
    let refusal = refused(&environment_root(cache.path(), COMMIT), COMMIT);
    assert_eq!(refusal.class, Class::Missing);
    let message = refusal.message("example");
    assert!(
        message.starts_with(
            "skipping the live example probe: no attested environment for 0123456789ab"
        ),
        "{message}"
    );
    assert!(
        message.contains(&format!("repin.py materialize {COMMIT}")),
        "{message}"
    );
}

#[test]
fn a_directory_without_an_attestation_is_a_partial_environment() {
    let (_cache, root) = environment(COMMIT);
    fs::remove_file(root.join(ATTESTATION)).expect("drop the attestation");
    let refusal = refused(&root, COMMIT);
    assert_eq!(refusal.class, Class::Missing);
    assert!(
        refusal.detail.contains("partial environment"),
        "{}",
        refusal.detail
    );
}

#[test]
fn an_unreadable_attestation_is_refused_instead_of_passing() {
    let (_cache, root) = environment(COMMIT);
    fs::write(root.join(ATTESTATION), [0xff, 0xfe, 0x00]).expect("non-UTF-8 attestation");
    let refusal = refused(&root, COMMIT);
    assert_eq!(refusal.class, Class::Drift);
    assert!(refusal.detail.contains("not UTF-8"), "{}", refusal.detail);
    fs::write(root.join(ATTESTATION), "{").expect("truncated attestation");
    assert_eq!(refused(&root, COMMIT).class, Class::Drift);
}

#[test]
fn an_environment_attested_for_another_commit_is_drift() {
    let (_cache, root) = environment(COMMIT);
    let other = "fedcba9876543210fedcba9876543210fedcba98";
    let refusal = refused(&root, other);
    assert_eq!(refusal.class, Class::Drift);
    assert!(refusal.detail.contains(COMMIT), "{}", refusal.detail);
}

#[test]
fn an_edited_installed_file_is_environment_drift_never_a_corpus_to_regenerate() {
    let (_cache, root) = environment(COMMIT);
    let installed = root.join(".venv/lib/site-packages/vibe/__init__.py");
    fs::write(&installed, "VERSION = 2\n").expect("edit the installed module");
    retime(&installed);
    let refusal = refused(&root, COMMIT);
    assert_eq!(refusal.class, Class::Drift);
    let message = refusal.message("example");
    assert!(
        message.starts_with("refusing the live example probe: environment for 0123456789ab no longer matches its attestation"),
        "{message}"
    );
    assert!(
        message.contains(".venv/lib/site-packages/vibe/__init__.py changed"),
        "{message}"
    );
    assert!(
        message.contains(&format!("repin.py materialize {COMMIT}")),
        "{message}"
    );
    assert!(!message.contains("regenerate"), "{message}");
}

#[test]
fn an_edited_source_file_a_removed_file_and_an_added_file_are_drift() {
    let (_cache, root) = environment(COMMIT);
    fs::write(root.join("source/vibe/__init__.py"), "VERSION = 22\n").expect("resize");
    assert!(refused(&root, COMMIT).detail.contains("changed size"));

    let (_cache, root) = environment(COMMIT);
    fs::remove_file(root.join("source/pyproject.toml")).expect("remove");
    assert!(
        refused(&root, COMMIT)
            .detail
            .contains("source/pyproject.toml is missing")
    );

    let (_cache, root) = environment(COMMIT);
    fs::write(root.join(".venv/lib/site-packages/sitecustomize.py"), "").expect("plant");
    assert!(
        refused(&root, COMMIT)
            .detail
            .contains(".venv/lib/site-packages/sitecustomize.py is not in the attestation")
    );
}

#[test]
fn a_retimed_but_unchanged_file_and_a_new_bytecode_cache_are_not_drift() {
    let (_cache, root) = environment(COMMIT);
    retime(&root.join("source/vibe/__init__.py"));
    let cache = root.join("source/vibe/__pycache__");
    fs::create_dir_all(&cache).expect("bytecode cache");
    fs::write(cache.join("__init__.cpython-312.pyc"), "cache").expect("bytecode");
    verify(&root, COMMIT).expect("metadata alone is not drift, and caches are not attested");
}

fn lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("a record line is JSON"))
        .collect()
}

#[test]
fn a_skip_is_recorded_with_its_class_and_commit() {
    let records = tempfile::tempdir().expect("records");
    let cache = tempfile::tempdir().expect("cache");
    let verdict = verify(&environment_root(cache.path(), COMMIT), COMMIT).map(|()| PathBuf::new());
    assert!(decide("example", verdict, false, records.path()).is_none());
    let skips = lines(&records.path().join("skips.jsonl"));
    assert_eq!(skips.len(), 1, "{skips:?}");
    assert_eq!(skips[0]["probe"], json!("example"));
    assert_eq!(skips[0]["class"], json!("missing"));
    assert_eq!(skips[0]["commit"], json!(REFERENCE_COMMIT));
    assert!(
        skips[0]["test"]
            .as_str()
            .is_some_and(|test| test.ends_with("a_skip_is_recorded_with_its_class_and_commit")),
        "{skips:?}"
    );
}

#[test]
fn a_host_other_than_the_corpus_platform_skips_as_a_platform_refusal() {
    assert_eq!(platform_refusal(CORPUS_PLATFORM), None);
    let refusal = platform_refusal("windows").expect("a Windows host cannot answer a Linux corpus");
    assert_eq!(refusal.class, Class::Platform);
    let message = refusal.message("example");
    assert!(
        message.starts_with("skipping the live example probe: ")
            && message.contains("captured on linux")
            && message.contains("differently on windows"),
        "{message}"
    );
    assert!(!message.contains("regenerate"), "{message}");

    // Recorded with its class, and fatal under the require-live mode.
    let records = tempfile::tempdir().expect("records");
    assert!(decide("example", Err(refusal.clone()), false, records.path()).is_none());
    let skips = lines(&records.path().join("skips.jsonl"));
    assert_eq!(skips[0]["class"], json!("platform"), "{skips:?}");
    let failure =
        std::panic::catch_unwind(|| decide("example", Err(refusal.clone()), true, records.path()));
    assert!(
        failure.is_err(),
        "a required live probe cannot skip on another platform"
    );
}

#[test]
fn a_probe_that_runs_is_recorded() {
    let records = tempfile::tempdir().expect("records");
    let (_cache, root) = environment(COMMIT);
    let verdict = verify(&root, COMMIT).map(|()| root.clone());
    assert_eq!(decide("example", verdict, true, records.path()), Some(root));
    let probes = lines(&records.path().join("probes.jsonl"));
    assert_eq!(probes.len(), 1, "{probes:?}");
    assert_eq!(probes[0]["probe"], json!("example"));
    assert!(!records.path().join("skips.jsonl").exists());
}

#[test]
fn a_skip_fails_the_test_when_live_measurement_is_required() {
    let records = tempfile::tempdir().expect("records");
    let cache = tempfile::tempdir().expect("cache");
    let verdict = verify(&environment_root(cache.path(), COMMIT), COMMIT).map(|()| PathBuf::new());
    let failure = std::panic::catch_unwind(|| decide("example", verdict, true, records.path()))
        .expect_err("a required live probe cannot skip");
    let message = failure
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_default();
    assert!(
        message.starts_with("VIBE_PARITY_REQUIRE_LIVE=1 and skipping the live example probe"),
        "{message}"
    );
    assert_eq!(lines(&records.path().join("skips.jsonl")).len(), 1);
}

/// Runs `program` under `python3` as `repin.py capture` would start a
/// capture of `COMMIT`.
fn sanctioned_python(program: &str) -> std::process::Output {
    let mut command = Command::new("python3");
    command.args(["-c", program]);
    command.env("VIBE_PARITY_REPIN_COMMIT", COMMIT);
    command.output().expect("python3 runs")
}

#[test]
fn a_capture_runs_the_attested_environment_and_never_the_checkout_it_was_handed() {
    // A checkout whose working tree carries an edited console script: what a
    // capture used to run, editable install and all.
    let checkout = tempfile::tempdir().expect("a checkout");
    let planted = checkout.path().join(".venv/bin/vibe-acp");
    fs::create_dir_all(planted.parent().expect("a parent")).expect("directories");
    fs::write(&planted, "edited in the working tree").expect("the planted script");
    let (cache, root) = environment(COMMIT);
    let script = root.join(".venv/bin/vibe-acp");
    fs::create_dir_all(script.parent().expect("a parent")).expect("directories");
    fs::write(&script, "installed").expect("the installed script");
    attest_with_python(&root, COMMIT);

    let program = format!(
        "import sys; sys.path.insert(0, {scripts:?}); from pathlib import Path; import reference; \
         commit, root = reference.environment(Path({checkout:?}), {COMMIT:?}, cache=Path({cache:?})); \
         print(root); print(reference.materialize.console_script(root, 'vibe-acp'))",
        scripts = repository().join("scripts/parity").display().to_string(),
        checkout = checkout.path().display().to_string(),
        cache = cache.path().display().to_string(),
    );
    let output = sanctioned_python(&program);
    assert!(
        output.status.success(),
        "the capture could not resolve its environment: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines().map(PathBuf::from);
    let (resolved, console) = (lines.next(), lines.next());
    let same = |left: Option<&PathBuf>, right: &Path| {
        left.and_then(|left| fs::canonicalize(left).ok()) == fs::canonicalize(right).ok()
    };
    assert!(
        same(resolved.as_ref(), &root),
        "the capture resolved {resolved:?}, not {root:?}"
    );
    assert!(
        same(console.as_ref(), &script),
        "the capture would run {console:?}, not {script:?}"
    );
    assert!(
        !stdout.contains(&checkout.path().display().to_string()),
        "the checkout reached the capture: {stdout}"
    );

    // Drift in the environment refuses the capture with the same class.
    fs::write(&script, "edited after the attestation").expect("drift");
    let output = sanctioned_python(&program);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains("no longer matches its attestation"),
        "a drifted environment must refuse the capture: {stderr}"
    );
}

/// A manifest entry for `corpus` at `commit`, replayed by `test`.
fn listed(corpus: &str, test: &str, commit: &str) -> ManifestEntry {
    ManifestEntry {
        corpus: corpus.to_owned(),
        kind: Kind::Corpus,
        driver: Some(vec!["scripts/parity/sample.py".to_owned()]),
        output: Some("--output".to_owned()),
        output_root: None,
        test: test.to_owned(),
        row: "1".to_owned(),
        commit: Some(commit.to_owned()),
        off_target: false,
        sha256: None,
        count: None,
        floors: std::collections::BTreeMap::new(),
        note: None,
        debt: None,
    }
}

#[test]
fn a_probe_gates_on_the_commit_of_the_corpora_its_test_file_replays() {
    let manifest = Manifest {
        schema_version: 1,
        note: None,
        transition: vec![
            "crates/a/tests/left.json".to_owned(),
            "crates/a/tests/mixed-left.json".to_owned(),
            "crates/c/src/runtime/one.json".to_owned(),
        ],
        entries: vec![
            listed(
                "crates/a/tests/left.json",
                "crates/a/tests/left.rs::replays",
                COMMIT,
            ),
            listed(
                "crates/a/tests/moved.json",
                "crates/a/tests/moved.rs::replays",
                REFERENCE_COMMIT,
            ),
            listed(
                "crates/a/tests/mixed-left.json",
                "crates/a/tests/mixed.rs::one",
                COMMIT,
            ),
            listed(
                "crates/a/tests/mixed-moved.json",
                "crates/a/tests/mixed.rs::two",
                REFERENCE_COMMIT,
            ),
            listed(
                "crates/c/src/runtime/one.json",
                "crates/c/src/runtime/one.rs::replays",
                COMMIT,
            ),
        ],
    };
    assert_eq!(
        probe_commit(&manifest, "crates/a/tests/left.rs", COMMIT).as_deref(),
        Ok(COMMIT)
    );
    assert_eq!(
        probe_commit(&manifest, r"crates\a\tests\left.rs", COMMIT).as_deref(),
        Ok(COMMIT),
        "a Windows caller path names the same file"
    );
    assert_eq!(
        probe_commit(&manifest, "crates/a/tests/moved.rs", COMMIT).as_deref(),
        Ok(REFERENCE_COMMIT)
    );
    assert_eq!(
        probe_commit(&manifest, "crates/c/src/runtime.rs", COMMIT).as_deref(),
        Ok(COMMIT),
        "a file with no entry of its own answers for the test modules it declares"
    );
    assert_eq!(
        probe_commit(&manifest, "crates/z/src/helper.rs", COMMIT).as_deref(),
        Ok(REFERENCE_COMMIT),
        "a file no entry names gates on the target"
    );
    assert_eq!(
        probe_commit(&manifest, "crates/a/tests/left.rs", "").as_deref(),
        Ok(REFERENCE_COMMIT),
        "outside a transition every probe gates on the target"
    );
    let unlisted = Manifest {
        transition: Vec::new(),
        ..manifest
    };
    assert_eq!(
        probe_commit(&unlisted, "crates/a/tests/left.rs", COMMIT).as_deref(),
        Ok(REFERENCE_COMMIT),
        "a corpus the transition list does not name is never measured at the previous commit"
    );
}

#[test]
fn a_hand_owned_fixture_beside_corpora_at_the_previous_commit_names_no_commit() {
    let mut fixture = listed(
        "crates/a/tests/expectations.json",
        "crates/a/tests/left.rs::replays",
        REFERENCE_COMMIT,
    );
    fixture.kind = Kind::Fixture;
    fixture.commit = None;
    let manifest = Manifest {
        schema_version: 1,
        note: None,
        transition: vec!["crates/a/tests/left.json".to_owned()],
        entries: vec![
            listed(
                "crates/a/tests/left.json",
                "crates/a/tests/left.rs::replays",
                COMMIT,
            ),
            fixture,
        ],
    };
    assert_eq!(
        probe_commit(&manifest, "crates/a/tests/left.rs", COMMIT).as_deref(),
        Ok(COMMIT),
        "the fixture is no oracle's output, so the file's corpora alone decide"
    );
}

#[test]
fn a_probe_whose_file_replays_both_commits_is_refused_as_a_transition() {
    let manifest = Manifest {
        schema_version: 1,
        note: None,
        transition: vec!["crates/a/tests/mixed-left.json".to_owned()],
        entries: vec![
            listed(
                "crates/a/tests/mixed-left.json",
                "crates/a/tests/mixed.rs::one",
                COMMIT,
            ),
            listed(
                "crates/a/tests/mixed-moved.json",
                "crates/a/tests/mixed.rs::two",
                REFERENCE_COMMIT,
            ),
        ],
    };
    let refusal = probe_commit(&manifest, "crates/a/tests/mixed.rs", COMMIT)
        .expect_err("a file at both commits has no one oracle");
    assert_eq!(refusal.class, Class::Transition);
    assert!(
        refusal.detail.contains("crates/a/tests/mixed.rs")
            && refusal.detail.contains("repin.py capture"),
        "{}",
        refusal.detail
    );
    let records = tempfile::tempdir().expect("a records directory");
    assert!(decide("mixed", Err(refusal), false, records.path()).is_none());
    let skips =
        fs::read_to_string(records.path().join("skips.jsonl")).expect("the skip is recorded");
    assert!(skips.contains("\"class\":\"transition\""), "{skips}");
}
