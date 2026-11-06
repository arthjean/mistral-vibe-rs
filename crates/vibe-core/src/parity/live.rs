//! The gate every live probe passes: an attested reference environment, or a
//! skip that is recorded and can be made fatal.
//!
//! A live probe recaptures the Python reference and compares the result with
//! the committed corpus. It used to gate on the shared checkout's `HEAD`, so a
//! verdict depended on whatever that checkout's editable install held, and a
//! skip was an `eprintln!` a passing test hid. This module gates instead on an
//! environment `scripts/parity/materialize.py` built for the pinned commit
//! under `.parity/reference-<sha12>/`, checked against the `attestation.json`
//! written beside it, so the probe runs whatever commit the shared checkout
//! sits at and nothing in its working tree can reach the verdict.
//!
//! The check compares every attested file's size and modification time and
//! hashes only a file whose time moved, the same rule `materialize.verify`
//! applies in Python, and refuses any file that joined either tree since. An
//! environment that fails it is refused as environment drift, never as a
//! corpus to regenerate.
//!
//! The committed corpora are captured on Linux, and the reference answers
//! differently on another host (line endings, shells, sandboxing; see "Live
//! probes on Windows" in `docs/parity.md`), so [`gate`] skips a probe on any
//! other host with the class [`Class::Platform`]. A probe whose oracle is the
//! host itself, such as the Windows credential vault one, gates through
//! [`gate_native`] instead.
//!
//! Every probe that runs appends a line to `target/parity/probes.jsonl` and
//! every probe that skips one to `target/parity/skips.jsonl`, so a CI summary
//! can count what measured nothing; [`REQUIRE_LIVE`] set to `1` turns a skip
//! into a failure.

#![allow(
    clippy::panic,
    reason = "the gate is test machinery: a skip under the require-live mode is the calling test \
              failing"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::REFERENCE_COMMIT;
use super::replay::repository;

/// Set to `1`, a probe that would skip fails instead.
pub const REQUIRE_LIVE: &str = "VIBE_PARITY_REQUIRE_LIVE";

/// The attestation at the root of an environment.
pub const ATTESTATION: &str = "attestation.json";

/// The attestation schema `materialize.py` writes.
pub const ATTESTATION_SCHEMA: u32 = 1;

/// The command a refusal names for building or rebuilding an environment.
pub const MATERIALIZE: &str = "python3 scripts/parity/repin.py materialize";

/// The two attested trees, relative to the environment root.
pub const TREES: [&str; 2] = [SOURCE, ".venv"];

/// The tree holding the commit as `git archive` exported it, relative to the
/// environment root.
pub const SOURCE: &str = "source";

/// The reference's source files in the environment at `root`, which
/// [`gate`] returns: a probe reads a reference file such as `action.yml`
/// here, never at `root` itself, where only the attestation and the two
/// trees sit.
#[must_use]
pub fn source_tree(root: &Path) -> PathBuf {
    root.join(SOURCE)
}

/// Directory names no attestation covers: bytecode caches the interpreter
/// writes next to what it imports.
const UNATTESTED_DIRECTORIES: [&str; 1] = ["__pycache__"];

/// Why an environment cannot serve as the oracle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Nothing is materialized for the commit, or only a partial directory.
    Missing,
    /// The environment no longer matches its attestation, or the attestation
    /// cannot be read.
    Drift,
    /// The host is not the platform the committed corpora were captured on.
    Platform,
}

impl Class {
    /// How a record and a CI summary name the class.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Drift => "environment-drift",
            Self::Platform => "platform",
        }
    }
}

/// An environment the gate refused, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub class: Class,
    pub detail: String,
}

impl Refusal {
    fn missing(detail: String) -> Self {
        Self {
            class: Class::Missing,
            detail,
        }
    }

    fn drift(commit: &str, detail: impl std::fmt::Display) -> Self {
        Self {
            class: Class::Drift,
            detail: format!(
                "environment for {} no longer matches its attestation ({detail}); rerun \
                 `{MATERIALIZE} {commit}`",
                short(commit)
            ),
        }
    }

    /// The line a probe named `probe` prints or fails with.
    #[must_use]
    pub fn message(&self, probe: &str) -> String {
        match self.class {
            Class::Missing | Class::Platform => {
                format!("skipping the live {probe} probe: {}", self.detail)
            }
            Class::Drift => format!("refusing the live {probe} probe: {}", self.detail),
        }
    }
}

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// Where environments are materialized: `.parity/` at the workspace root.
#[must_use]
pub fn cache() -> PathBuf {
    repository().join(".parity")
}

/// Where the environment of `commit` is materialized under `cache`.
#[must_use]
pub fn environment_root(cache: &Path, commit: &str) -> PathBuf {
    cache.join(format!("reference-{}", short(commit)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Attestation {
    schema_version: u32,
    commit: String,
    #[serde(default)]
    files: BTreeMap<String, (u64, u64, String)>,
}

/// Checks the environment at `root` against its attestation for `commit`:
/// every attested file present at its size, hashed when its modification time
/// moved, and no other file in either tree.
///
/// # Errors
///
/// A [`Refusal`] naming why the environment cannot serve: missing, partial,
/// unreadable, attested for another commit, or drifted.
pub fn verify(root: &Path, commit: &str) -> Result<(), Refusal> {
    if !root.is_dir() {
        return Err(Refusal::missing(format!(
            "no attested environment for {}; run `{MATERIALIZE} {commit}`",
            short(commit)
        )));
    }
    let bytes = match fs::read(root.join(ATTESTATION)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(Refusal::missing(format!(
                "{} holds no attestation, so it is a partial environment; run `{MATERIALIZE} \
                 {commit}`",
                root.display()
            )));
        }
        Err(error) => {
            return Err(Refusal::drift(
                commit,
                format!("its attestation is unreadable: {error}"),
            ));
        }
    };
    let text = String::from_utf8(bytes)
        .map_err(|_| Refusal::drift(commit, "its attestation is not UTF-8"))?;
    let attestation: Attestation = serde_json::from_str(&text).map_err(|error| {
        Refusal::drift(commit, format!("its attestation does not parse: {error}"))
    })?;
    if attestation.schema_version != ATTESTATION_SCHEMA {
        return Err(Refusal::drift(
            commit,
            format!("its attestation is not at schema {ATTESTATION_SCHEMA}"),
        ));
    }
    if attestation.commit != commit {
        return Err(Refusal::drift(
            commit,
            format!("its attestation names {}", attestation.commit),
        ));
    }
    if attestation.files.is_empty() {
        return Err(Refusal::drift(commit, "its attestation lists no files"));
    }
    let mut present = BTreeSet::new();
    for tree in TREES {
        walk(root, Path::new(tree), &mut present)
            .map_err(|error| Refusal::drift(commit, format!("{tree} is unreadable: {error}")))?;
    }
    for (relative, (size, modified, digest)) in &attestation.files {
        if !present.contains(relative) {
            return Err(Refusal::drift(commit, format!("{relative} is missing")));
        }
        let path = root.join(relative);
        let metadata = fs::metadata(&path)
            .map_err(|error| Refusal::drift(commit, format!("{relative}: {error}")))?;
        if metadata.len() != *size {
            return Err(Refusal::drift(commit, format!("{relative} changed size")));
        }
        if modified_ns(&metadata) != Some(*modified)
            && sha256_file(&path).ok().as_deref() != Some(digest.as_str())
        {
            return Err(Refusal::drift(commit, format!("{relative} changed")));
        }
    }
    if let Some(added) = present
        .iter()
        .find(|relative| !attestation.files.contains_key(*relative))
    {
        return Err(Refusal::drift(
            commit,
            format!("{added} is not in the attestation"),
        ));
    }
    Ok(())
}

/// Every file under `root/relative` as a `/`-separated path relative to
/// `root`, as Python's `os.walk` lists them: a symbolic link to a directory is
/// not descended, and one that resolves nowhere is not a file.
fn walk(root: &Path, relative: &Path, found: &mut BTreeSet<String>) -> io::Result<()> {
    let directory = root.join(relative);
    if !directory.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let child = relative.join(&name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if !UNATTESTED_DIRECTORIES.iter().any(|skip| name == *skip) {
                walk(root, &child, found)?;
            }
        } else if kind.is_file() || (kind.is_symlink() && entry.path().is_file()) {
            let components: Vec<String> = child
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect();
            found.insert(components.join("/"));
        }
    }
    Ok(())
}

fn modified_ns(metadata: &fs::Metadata) -> Option<u64> {
    let since = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(since.as_nanos()).ok()
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// The environment of `commit` under [`cache`], verified once per process:
/// a test binary runs many probes, and the environment does not change under
/// them.
fn verified(commit: &str) -> Result<PathBuf, Refusal> {
    static VERIFIED: OnceLock<Mutex<BTreeMap<String, Result<PathBuf, Refusal>>>> = OnceLock::new();
    let mut verified = VERIFIED
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    verified
        .entry(commit.to_owned())
        .or_insert_with(|| {
            let root = environment_root(&cache(), commit);
            verify(&root, commit).map(|()| root)
        })
        .clone()
}

/// Whether [`REQUIRE_LIVE`] asks for every probe to measure.
#[must_use]
pub fn live_required() -> bool {
    std::env::var(REQUIRE_LIVE).is_ok_and(|value| value.trim() == "1")
}

/// The attested environment of the pin, or why there is none, for a caller
/// that reports the reason itself instead of skipping a probe.
///
/// # Errors
///
/// The [`Refusal`] [`verify`] returned.
pub fn pinned_environment() -> Result<PathBuf, Refusal> {
    verified(REFERENCE_COMMIT)
}

/// The platform every committed corpus was captured on, and the only host a
/// live probe measures from through [`gate`].
pub const CORPUS_PLATFORM: &str = "linux";

/// Why a host whose operating system is `os` cannot recapture a corpus
/// captured on [`CORPUS_PLATFORM`], or [`None`] when it is that platform.
#[must_use]
pub fn platform_refusal(os: &str) -> Option<Refusal> {
    (os != CORPUS_PLATFORM).then(|| Refusal {
        class: Class::Platform,
        detail: format!(
            "the committed corpora were captured on {CORPUS_PLATFORM} and the reference \
             answers differently on {os} (`docs/parity.md`, \"Live probes on Windows\"); live \
             measurement runs on {CORPUS_PLATFORM}"
        ),
    })
}

/// The attested environment the live probe named `probe` runs, or [`None`]
/// after recording why it skips: on a host other than [`CORPUS_PLATFORM`],
/// or for want of an attested environment. Under [`REQUIRE_LIVE`] a skip
/// panics with its reason instead.
#[must_use]
pub fn gate(probe: &str) -> Option<PathBuf> {
    let verdict = platform_refusal(std::env::consts::OS).map_or_else(pinned_environment, Err);
    decide(probe, verdict, live_required(), &records())
}

/// [`gate`] for a probe whose oracle is the host's own platform, such as one
/// that reads what the reference stored in the Windows credential vault: it
/// runs on any host that holds an attested environment.
#[must_use]
pub fn gate_native(probe: &str) -> Option<PathBuf> {
    decide(probe, pinned_environment(), live_required(), &records())
}

/// What [`gate`] does with `verdict`, recording under `records`.
pub(super) fn decide(
    probe: &str,
    verdict: Result<PathBuf, Refusal>,
    required: bool,
    records: &Path,
) -> Option<PathBuf> {
    match verdict {
        Ok(root) => {
            append(
                &records.join("probes.jsonl"),
                &json!({"probe": probe, "commit": REFERENCE_COMMIT, "test": test_name()}),
            );
            Some(root)
        }
        Err(refusal) => {
            let message = refusal.message(probe);
            append(
                &records.join("skips.jsonl"),
                &json!({
                    "probe": probe,
                    "class": refusal.class.as_str(),
                    "commit": REFERENCE_COMMIT,
                    "test": test_name(),
                    "reason": message,
                }),
            );
            assert!(
                !required,
                "{REQUIRE_LIVE}=1 and {message}; the committed corpus replayed regardless"
            );
            eprintln!("{message}");
            eprintln!("the committed corpus replayed regardless");
            None
        }
    }
}

/// The test running on this thread, as libtest and nextest name its thread.
pub(super) fn test_name() -> Value {
    std::thread::current()
        .name()
        .filter(|name| *name != "main")
        .map_or(Value::Null, |name| Value::String(name.to_owned()))
}

/// Where the records go: `parity/` under the Cargo target directory.
#[must_use]
pub fn records() -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || repository().join("target"),
        |target| {
            let target = PathBuf::from(target);
            if target.is_absolute() {
                target
            } else {
                repository().join(target)
            }
        },
    );
    target.join("parity")
}

/// Appends `line` to the record `name`. A line is one write, so concurrent
/// test processes never interleave inside one; a record that cannot be
/// written loses the line rather than failing the test.
pub fn record(name: &str, line: &Value) {
    append(&records().join(name), line);
}

fn append(path: &Path, line: &Value) {
    let mut text = line.to_string();
    text.push('\n');
    let written = path
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| {
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?
                .write_all(text.as_bytes())
        });
    if let Err(error) = written {
        eprintln!("could not record to {}: {error}", path.display());
    }
}
