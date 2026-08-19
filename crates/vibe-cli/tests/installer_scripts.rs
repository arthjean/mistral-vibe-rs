//! The two shipped shell surfaces, exercised as scripts rather than as text.
//!
//! `scripts/ci/package-release.sh` and `scripts/install.sh` are what a user and
//! a release actually run, and neither was reachable from `cargo test`. The
//! aggregate checksum manifest, the release-base override and the scheme
//! allowlist are therefore driven here through the real scripts, with the
//! packaging output feeding the installer exactly as a published release does.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "a script fixture that cannot be built has no assertion left to make"
)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The version the fixtures package under. Deliberately not the workspace
/// version: nothing in this file should start passing because a bump happened.
const FIXTURE_VERSION: &str = "9.9.9";

/// The targets every fixture release carries, so the installer's lookup runs
/// against a manifest holding more than its own line.
const FIXTURE_TARGETS: [&str; 3] = ["linux-x86_64", "linux-aarch64", "windows-x86_64"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

/// The archive name `scripts/install.sh` resolves on this machine, or [`None`]
/// when the platform has no published artifact.
fn host_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        ("macos", "x86_64") => Some("macos-x86_64"),
        ("macos", "aarch64") => Some("macos-aarch64"),
        _ => None,
    }
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).expect("a fixture script is writable");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("a fixture is executable");
}

/// Stand-in release binaries. `install.sh` runs both at the end of a successful
/// install, so they answer `--version` and `--help` and nothing else.
fn fixture_binaries(directory: &Path) -> PathBuf {
    let binaries = directory.join("binaries");
    fs::create_dir_all(&binaries).expect("a fixture binary directory");
    // The Windows leg reads `.exe` names, so both spellings exist and the fixture
    // covers every target the release publishes.
    for name in ["vibe", "vibe-acp", "vibe.exe", "vibe-acp.exe"] {
        write_executable(
            &binaries.join(name),
            &format!("#!/bin/sh\necho \"{name} {FIXTURE_VERSION}\"\n"),
        );
    }
    binaries
}

/// A `curl` that records its invocation and fails. `install.sh` requires curl to
/// be resolvable before it does anything, so a shim proves the difference
/// between "present" and "used": any test that succeeds with this on `PATH`
/// made no network request.
fn curl_shim(directory: &Path) -> (PathBuf, PathBuf) {
    let shim_directory = directory.join("shim");
    fs::create_dir_all(&shim_directory).expect("a shim directory");
    let marker = directory.join("curl-was-invoked");
    write_executable(
        &shim_directory.join("curl"),
        &format!(
            "#!/bin/sh\ntouch \"{}\"\necho \"the fixture forbids network access\" >&2\nexit 7\n",
            marker.display()
        ),
    );
    (shim_directory, marker)
}

fn shimmed_path(shim_directory: &Path) -> String {
    let inherited = std::env::var("PATH").unwrap_or_default();
    format!("{}:{inherited}", shim_directory.display())
}

fn package(binaries: &Path, output_directory: &Path, target: &str, epoch: &str) -> Output {
    Command::new("bash")
        .arg("scripts/ci/package-release.sh")
        .args([target, FIXTURE_VERSION])
        .arg(output_directory)
        .current_dir(repo_root())
        .env("VIBE_SKIP_BUILD", "true")
        .env("VIBE_RELEASE_BINARY_DIR", binaries)
        .env("SOURCE_DATE_EPOCH", epoch)
        .output()
        .expect("the packaging script runs")
}

fn assert_packaged(output: &Output, target: &str) {
    assert!(
        output.status.success(),
        "packaging {target} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Every `(digest, archive)` pair the aggregate manifest holds, in file order.
fn manifest_entries(output_directory: &Path) -> Vec<(String, String)> {
    let manifest =
        fs::read_to_string(output_directory.join("SHA256SUMS")).expect("an aggregate manifest");
    manifest
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut fields = line.split_whitespace();
            let digest = fields.next().unwrap_or_default().to_owned();
            let archive = fields
                .next()
                .unwrap_or_default()
                .trim_start_matches('*')
                .to_owned();
            (digest, archive)
        })
        .collect()
}

/// A release directory holding every fixture target plus the host's, packaged by
/// the real script.
fn fixture_release(directory: &Path) -> (PathBuf, &'static str) {
    let host = host_target().expect("this platform has a published artifact");
    let binaries = fixture_binaries(directory);
    let assets = directory.join("release-assets");
    for target in FIXTURE_TARGETS.iter().copied().chain(std::iter::once(host)) {
        if manifest_entries_contains(&assets, target) {
            continue;
        }
        assert_packaged(&package(&binaries, &assets, target, "1700000000"), target);
    }
    (assets, host)
}

fn manifest_entries_contains(assets: &Path, target: &str) -> bool {
    assets.join("SHA256SUMS").exists()
        && manifest_entries(assets)
            .iter()
            .any(|(_, archive)| archive.contains(target))
}

fn install(assets: &Path, root: &Path, shim: &Path) -> Output {
    Command::new("sh")
        .arg("scripts/install.sh")
        .current_dir(repo_root())
        .env("PATH", shimmed_path(shim))
        .env("VIBE_VERSION", FIXTURE_VERSION)
        .env(
            "VIBE_RELEASE_BASE_URL",
            format!("file://{}", assets.display()),
        )
        .env("VIBE_INSTALL_DIR", root.join("bin"))
        .env("VIBE_COMPLETION_DIR", root.join("completions"))
        .output()
        .expect("the installer runs")
}

#[test]
fn packaging_accumulates_every_target_into_one_sorted_manifest() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let binaries = fixture_binaries(directory.path());
    let assets = directory.path().join("release-assets");

    for target in FIXTURE_TARGETS {
        assert_packaged(&package(&binaries, &assets, target, "1700000000"), target);
    }

    let entries = manifest_entries(&assets);
    assert_eq!(
        entries.len(),
        FIXTURE_TARGETS.len(),
        "the aggregate manifest must hold one line per packaged archive: {entries:?}"
    );
    let archives: Vec<&str> = entries.iter().map(|(_, name)| name.as_str()).collect();
    let mut sorted = archives.clone();
    sorted.sort_unstable();
    assert_eq!(
        archives, sorted,
        "the manifest must be sorted by archive name"
    );
    for target in FIXTURE_TARGETS {
        assert!(
            archives.iter().any(|archive| archive.contains(target)),
            "{target} is missing from the aggregate manifest: {archives:?}"
        );
    }
    for (digest, archive) in &entries {
        assert_eq!(digest.len(), 64, "{archive} carries no SHA-256 digest");
        assert!(
            !archive.contains('/'),
            "{archive} must be a bare filename so `sha256sum -c` resolves it in place"
        );
    }
}

#[test]
fn packaging_the_same_target_twice_leaves_exactly_one_line() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let binaries = fixture_binaries(directory.path());
    let assets = directory.path().join("release-assets");

    assert_packaged(
        &package(&binaries, &assets, "linux-x86_64", "1700000000"),
        "linux-x86_64",
    );
    assert_packaged(
        &package(&binaries, &assets, "linux-aarch64", "1700000000"),
        "linux-aarch64",
    );
    assert_packaged(
        &package(&binaries, &assets, "linux-x86_64", "1700000001"),
        "linux-x86_64",
    );

    let entries = manifest_entries(&assets);
    let repeats = entries
        .iter()
        .filter(|(_, archive)| archive.contains("linux-x86_64"))
        .count();
    assert_eq!(
        repeats, 1,
        "a repackaged target must replace its line: {entries:?}"
    );
    assert_eq!(
        entries.len(),
        2,
        "the other target's line must survive: {entries:?}"
    );
}

#[test]
fn a_fixed_source_date_epoch_produces_identical_archives() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let binaries = fixture_binaries(directory.path());
    let first = directory.path().join("first");
    let second = directory.path().join("second");

    assert_packaged(
        &package(&binaries, &first, "linux-x86_64", "1700000000"),
        "linux-x86_64",
    );
    assert_packaged(
        &package(&binaries, &second, "linux-x86_64", "1700000000"),
        "linux-x86_64",
    );

    let archive = format!("mistral-vibe-rs-{FIXTURE_VERSION}-linux-x86_64.tar.gz");
    assert_eq!(
        fs::read(first.join(&archive)).expect("the first archive"),
        fs::read(second.join(&archive)).expect("the second archive"),
        "two runs of the same target at the same epoch must produce identical bytes"
    );
}

#[test]
fn a_file_release_base_installs_without_any_network_request() {
    if host_target().is_none() {
        println!("skipping: this platform has no published artifact to install");
        return;
    }
    let directory = tempfile::tempdir().expect("a temporary directory");
    let (assets, _host) = fixture_release(directory.path());
    let (shim, marker) = curl_shim(directory.path());
    let root = directory.path().join("installed");

    let output = install(&assets, &root, &shim);
    assert!(
        output.status.success(),
        "the installer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !marker.exists(),
        "a file:// base must never reach curl, yet the shim recorded an invocation"
    );
    assert!(
        manifest_entries(&assets).len() >= 3,
        "the installer must have resolved its digest out of a multi-target manifest"
    );

    for executable in ["vibe", "vibe-acp"] {
        let installed = root.join("bin").join(executable);
        assert!(installed.is_file(), "{executable} was not installed");
        assert!(
            !root.join("bin").join(format!("{executable}.new")).exists()
                && !root
                    .join("bin")
                    .join(format!("{executable}.previous"))
                    .exists(),
            "{executable} left staging state behind"
        );
    }
    for completion in ["vibe.bash", "_vibe", "vibe.fish", "vibe.ps1"] {
        assert!(
            root.join("completions").join(completion).is_file(),
            "{completion} was not installed"
        );
    }
    let reported = String::from_utf8_lossy(&output.stdout);
    assert!(
        reported.contains(&format!("Mistral Vibe RS {FIXTURE_VERSION} installed in")),
        "the installer must report the version it staged: {reported}"
    );
}

#[test]
fn a_manifest_without_the_archive_line_fails_before_staging() {
    if host_target().is_none() {
        println!("skipping: this platform has no published artifact to install");
        return;
    }
    let directory = tempfile::tempdir().expect("a temporary directory");
    let (assets, host) = fixture_release(directory.path());
    let (shim, _marker) = curl_shim(directory.path());
    let root = directory.path().join("installed");

    let retained: Vec<String> = manifest_entries(&assets)
        .into_iter()
        .filter(|(_, archive)| !archive.contains(host))
        .map(|(digest, archive)| format!("{digest}  {archive}"))
        .collect();
    fs::write(assets.join("SHA256SUMS"), retained.join("\n") + "\n")
        .expect("the manifest is rewritable");

    let output = install(&assets, &root, &shim);
    assert!(
        !output.status.success(),
        "an unverifiable archive must not install"
    );
    let reported = String::from_utf8_lossy(&output.stderr);
    assert!(
        reported.contains(&format!("mistral-vibe-rs-{FIXTURE_VERSION}-{host}")),
        "the failure must name the archive it could not verify: {reported}"
    );
    assert!(
        !root.join("bin").join("vibe").exists(),
        "nothing may be staged before the checksum is resolved"
    );
}

#[test]
fn an_http_release_base_is_refused_before_any_fetch() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let (shim, marker) = curl_shim(directory.path());
    let root = directory.path().join("installed");

    let output = Command::new("sh")
        .arg("scripts/install.sh")
        .current_dir(repo_root())
        .env("PATH", shimmed_path(&shim))
        .env("VIBE_VERSION", FIXTURE_VERSION)
        .env("VIBE_RELEASE_BASE_URL", "http://example.invalid/releases")
        .env("VIBE_INSTALL_DIR", root.join("bin"))
        .env("VIBE_COMPLETION_DIR", root.join("completions"))
        .output()
        .expect("the installer runs");

    assert!(
        !output.status.success(),
        "a plain-HTTP release base must be refused"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("refusing non-HTTPS release source"),
        "the refusal must name its cause: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!marker.exists(), "the refusal must precede any fetch");
    assert!(!root.join("bin").join("vibe").exists());
}
