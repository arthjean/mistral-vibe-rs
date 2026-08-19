//! Guards that keep the published release describable from the manifest alone.
//!
//! The installers, the composite action and the release notes each spell the
//! version and the repository out by hand, and nothing used to compare the
//! copies against the manifest that owns them. A default-path install therefore
//! fetched from an owner the repository never had, and a bump could leave five
//! files behind without a single failing check.
//!
//! These tests apply the shape `crates/vibe-core/src/parity/parity_tests.rs`
//! already uses for the reference commit: one declaration, and a scanner that
//! fails both on a copy that disagrees and on a carrier that stops carrying it.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::tui::updates::whats_new_content;

/// The files that write `[workspace.package] version` out by hand. Each one
/// must carry it, and none may carry any other version.
const VERSION_CARRIERS: [&str; 5] = [
    "action.yml",
    ".github/workflows/action.yml",
    "scripts/install.sh",
    "scripts/install.ps1",
    "crates/vibe-cli/whats_new.md",
];

/// Release machinery that resolves the version at run time. A literal appearing
/// here is a sixth copy the bump would not reach.
const VERSION_FREE_SOURCES: [&str; 4] = [
    ".github/workflows/release.yml",
    "scripts/ci/package-release.sh",
    CI_WORKFLOW,
    INSTALLER_VERIFICATION,
];

/// The two scripts a user runs to install, both of which resolve a release
/// asset URL from a hand-written owner and repository.
const INSTALLERS: [&str; 2] = ["scripts/install.sh", "scripts/install.ps1"];

/// The workflow that publishes what the installers fetch.
const RELEASE_WORKFLOW: &str = ".github/workflows/release.yml";

/// The workflow every push and pull request runs.
const CI_WORKFLOW: &str = ".github/workflows/ci.yml";

/// The job inside it that exercises the PowerShell installer, which is the only
/// delivery path a Linux runner cannot execute.
const WINDOWS_INSTALLER_JOB: &str = "windows-installer";

/// The script that job runs. It drives `scripts/install.ps1` through the four
/// paths a Windows installation takes.
const INSTALLER_VERIFICATION: &str = "scripts/ci/verify-install-ps1.ps1";

/// The longest the Windows job may be allowed to run.
const WINDOWS_INSTALLER_BUDGET_MINUTES: u32 = 20;

/// Every target the release matrix builds. The installers resolve their archive
/// name from `uname` and from the PowerShell architecture check, so a target
/// they can ask for and the matrix does not build is a 404 by construction.
const PACKAGED_TARGETS: [&str; 5] = [
    "linux-x86_64",
    "linux-aarch64",
    "macos-x86_64",
    "macos-aarch64",
    "windows-x86_64",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{relative} is readable: {error}"))
}

/// The value of one key under `[workspace.package]` in the root manifest.
fn workspace_package(key: &str) -> String {
    let manifest: toml::Table =
        toml::from_str(&read("Cargo.toml")).expect("the root manifest is valid TOML");
    manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("package"))
        .and_then(|package| package.get(key))
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("[workspace.package] declares {key}"))
        .to_owned()
}

/// Every `MAJOR.MINOR.PATCH` token in `line`.
///
/// Two-part numbers are deliberately not versions here: `python_version: 3.12`
/// and `ubuntu-24.04` are neither release versions nor drift.
fn version_tokens(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for character in line.chars().chain(std::iter::once(' ')) {
        if character.is_ascii_digit() || character == '.' {
            current.push(character);
            continue;
        }
        let candidate = current.trim_matches('.').to_owned();
        current.clear();
        let parts: Vec<&str> = candidate.split('.').collect();
        let numeric = parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|value| value.is_ascii_digit()));
        if parts.len() == 3 && numeric {
            tokens.push(candidate);
        }
    }
    tokens
}

/// Every `owner/repository` slug named by a `https://github.com/...` URL in
/// `text`.
fn github_slugs(text: &str) -> BTreeSet<String> {
    const PREFIX: &str = "https://github.com/";
    let mut slugs = BTreeSet::new();
    let mut rest = text;
    while let Some(position) = rest.find(PREFIX) {
        rest = &rest[position + PREFIX.len()..];
        let path: String = rest
            .chars()
            .take_while(|value| {
                value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.' | '/')
            })
            .collect();
        let segments: Vec<&str> = path.split('/').collect();
        if segments.len() >= 2 && !segments[0].is_empty() && !segments[1].is_empty() {
            slugs.insert(format!("{}/{}", segments[0], segments[1]));
        }
    }
    slugs
}

#[test]
fn every_hand_written_version_matches_the_workspace_manifest() {
    let declared = workspace_package("version");
    let mut offenses = Vec::new();
    for carrier in VERSION_CARRIERS {
        let contents = read(carrier);
        let mut carried = 0_usize;
        for (index, line) in contents.lines().enumerate() {
            for token in version_tokens(line) {
                carried += 1;
                if token != declared {
                    offenses.push(format!(
                        "{carrier}:{} carries {token}, but [workspace.package] version is \
                         {declared}",
                        index + 1
                    ));
                }
            }
        }
        assert!(
            carried > 0,
            "{carrier} no longer carries a version at all, so this scan would pass without \
             measuring anything; restore the literal or drop the file from VERSION_CARRIERS"
        );
    }
    assert!(
        offenses.is_empty(),
        "a version literal drifted from [workspace.package] version: {}",
        offenses.join("; ")
    );
}

#[test]
fn no_release_script_or_workflow_hides_another_version_literal() {
    let mut offenses = Vec::new();
    for source in VERSION_FREE_SOURCES {
        let contents = read(source);
        for (index, line) in contents.lines().enumerate() {
            for token in version_tokens(line) {
                offenses.push(format!("{source}:{} carries {token}", index + 1));
            }
        }
    }
    assert!(
        offenses.is_empty(),
        "release machinery must resolve the version from the manifest rather than repeat it: {}",
        offenses.join("; ")
    );
}

#[test]
fn the_binary_reports_the_version_the_manifest_declares() {
    assert_eq!(
        env!("CARGO_PKG_VERSION"),
        workspace_package("version"),
        "`vibe --version` prints CARGO_PKG_VERSION, which the crate inherits from the workspace"
    );
}

#[test]
fn the_release_notes_heading_names_the_workspace_version() {
    let content = whats_new_content().expect("release notes ship with the binary");
    let heading = content
        .lines()
        .next()
        .expect("the notes open with a heading");
    assert!(
        heading.starts_with("# What's new in v"),
        "the release notes heading changed shape: {heading}"
    );
    assert_eq!(
        heading,
        format!("# What's new in v{}", workspace_package("version")),
        "the release notes name a version the workspace does not declare"
    );
}

#[test]
fn a_reference_version_disagreement_is_stated_rather_than_failed() {
    // A re-pin moves `REFERENCE_VERSION` before this port follows it, and a bump
    // here moves this port before the pin does. Either order is legitimate, so
    // the disagreement is reported and never asserted.
    let declared = workspace_package("version");
    let reference = vibe_core::parity::REFERENCE_VERSION;
    assert!(!declared.is_empty() && !reference.is_empty());
    if declared == reference {
        println!("the workspace and the pinned reference both publish {declared}");
    } else {
        println!(
            "the workspace publishes {declared} while the pinned reference publishes {reference}"
        );
    }
}

#[test]
fn both_installers_fetch_from_the_declared_repository() {
    let repository = workspace_package("repository");
    let mut offenses = Vec::new();
    for installer in INSTALLERS {
        let contents = read(installer);
        let bases: Vec<(usize, &str)> = contents
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains("releases/download"))
            .map(|(index, line)| (index + 1, line))
            .collect();
        assert_eq!(
            bases.len(),
            1,
            "{installer} must compute exactly one release base, found {}",
            bases.len()
        );
        let (number, line) = bases[0];
        if !line.contains(&format!("{repository}/releases/download/v")) {
            offenses.push(format!(
                "{installer}:{number} resolves {}, but [workspace.package] repository is \
                 {repository}",
                line.trim()
            ));
        }
        for slug in github_slugs(&contents) {
            if !repository.ends_with(&slug) {
                offenses.push(format!(
                    "{installer} names github.com/{slug}, but [workspace.package] repository is \
                     {repository}"
                ));
            }
        }
    }
    assert!(
        offenses.is_empty(),
        "an installer fetches from a repository the manifest does not declare: {}",
        offenses.join("; ")
    );
}

#[test]
fn the_release_workflow_publishes_every_target_an_installer_can_ask_for() {
    let workflow = read(RELEASE_WORKFLOW);
    let mut missing = Vec::new();
    for target in PACKAGED_TARGETS {
        if !workflow.contains(&format!("- target: {target}")) {
            missing.push(target);
        }
    }
    assert!(
        missing.is_empty(),
        "{RELEASE_WORKFLOW} does not build {missing:?}; an installer that resolves one of those \
         archive names would fetch a 404"
    );

    // The installers derive their archive name from the running platform, so the
    // names they can produce are the release's actual contract.
    let installer = read("scripts/install.sh");
    for target in PACKAGED_TARGETS
        .iter()
        .filter(|name| !name.starts_with("windows"))
    {
        assert!(
            installer.contains(&format!("echo \"{target}\"")),
            "scripts/install.sh cannot request {target}, which the release matrix builds"
        );
    }
    assert!(
        read("scripts/install.ps1").contains("windows-x86_64"),
        "scripts/install.ps1 must request the windows-x86_64 archive the matrix builds"
    );
}

#[test]
fn the_release_workflow_collects_one_aggregate_manifest() {
    let workflow = read(RELEASE_WORKFLOW);
    let required = [
        // A tag is what publishes.
        ("tags: [\"v*\"]", "the workflow must trigger on a v* tag"),
        // `upload-artifact` answers a duplicate name with a 409, so each leg
        // claims its own and the collection job reassembles them.
        (
            "name: artifacts-${{ matrix.target }}",
            "each matrix leg must upload under a name unique to its target",
        ),
        (
            "pattern: artifacts-*",
            "the collection job must download every leg with a wildcard",
        ),
        (
            "merge-multiple: true",
            "the collection job must merge every leg into one directory",
        ),
        // Run from inside the directory so each line records a bare filename.
        (
            "cd dist/release-assets",
            "the checksum tool must run from inside the archive directory",
        ),
        (
            "sha256sum mistral-vibe-rs-* > SHA256SUMS",
            "one aggregate manifest must cover every archive",
        ),
        (
            "sha256sum -c SHA256SUMS --ignore-missing",
            "the aggregate manifest must be verified where it is produced",
        ),
        (
            "gh release create",
            "the collection job must publish the release",
        ),
        (
            "does not match [workspace.package] version",
            "a tag disagreeing with the manifest must stop the run naming both values",
        ),
    ];
    for (needle, why) in required {
        assert!(
            workflow.contains(needle),
            "{RELEASE_WORKFLOW} is missing `{needle}`: {why}"
        );
    }

    // Publication depends on the whole matrix, so a leg that did not build
    // leaves the release unpublished rather than half-populated.
    assert!(
        workflow.contains("needs: [version, build]"),
        "{RELEASE_WORKFLOW} must gate publication on every matrix leg"
    );
    assert!(
        workflow.contains("needs: version"),
        "{RELEASE_WORKFLOW} must gate the matrix on the tag-and-version check"
    );
}

#[test]
fn the_upgrade_commands_rerun_an_installer_this_repository_publishes() {
    // Reference `UPDATE_COMMANDS` reruns the package managers it publishes
    // under. No package manager publishes this binary, so the upgrade reruns
    // the installer that produced it, and that script must exist.
    let repository = workspace_package("repository");
    let commands = crate::distribution::upgrade_commands();
    assert_eq!(
        commands.len(),
        1,
        "one installer is published per platform, found {commands:?}"
    );
    let command = &commands[0];
    let script = if cfg!(windows) {
        "install.ps1"
    } else {
        "install.sh"
    };
    let slug = repository
        .strip_prefix("https://github.com/")
        .expect("the declared repository is a GitHub URL");
    assert!(
        command.contains(&format!(
            "https://raw.githubusercontent.com/{slug}/main/scripts/{script}"
        )),
        "the upgrade command does not fetch the declared repository's {script}: {command}"
    );
    assert!(
        repo_root().join("scripts").join(script).is_file(),
        "the upgrade command fetches scripts/{script}, which this repository does not publish"
    );
}

#[test]
fn packaged_output_is_not_tracked() {
    let ignored = read(".gitignore");
    assert!(
        ignored.lines().any(|line| line.trim() == "dist/"),
        "a local `package-release.sh` run writes dist/, which must not appear as untracked work"
    );
}

/// The body of one job in `workflow`, without its header line.
///
/// A job body is indented four spaces; the next job's header and the comments
/// introducing it sit at two, which is where the block ends.
fn workflow_job(workflow: &str, name: &str) -> String {
    let header = format!("  {name}:");
    let mut body = Vec::new();
    let mut found = false;
    for line in workflow.lines() {
        if line.trim_end() == header {
            found = true;
            continue;
        }
        if found {
            if !line.trim().is_empty() && !line.starts_with("    ") {
                break;
            }
            body.push(line);
        }
    }
    assert!(found, "{CI_WORKFLOW} declares no job named {name}");
    body.join("\n")
}

#[test]
fn the_powershell_installer_is_exercised_on_a_windows_runner() {
    // Every other gate runs on Linux, where `install.ps1` is unreachable. This
    // job is the only thing that executes it, so its shape is asserted here
    // rather than trusted to whoever last edited the workflow.
    let job = workflow_job(&read(CI_WORKFLOW), WINDOWS_INSTALLER_JOB);
    let required = [
        (
            "runs-on: windows-",
            "the installer must be exercised on a Windows runner",
        ),
        (
            "scripts/ci/package-release.sh windows-x86_64",
            "the job must package the archive the installer fetches",
        ),
        (
            "VIBE_SKIP_BUILD",
            "the job must package the binaries it already built rather than rebuilding them",
        ),
        (
            INSTALLER_VERIFICATION,
            "the job must run the script that drives the installer",
        ),
        ("shell: pwsh", "the verification must run under PowerShell"),
    ];
    for (needle, why) in required {
        assert!(
            job.contains(needle),
            "{CI_WORKFLOW} job {WINDOWS_INSTALLER_JOB} is missing `{needle}`: {why}"
        );
    }

    let budget: u32 = job
        .lines()
        .find_map(|line| line.trim().strip_prefix("timeout-minutes:"))
        .map(|value| {
            value
                .trim()
                .parse()
                .unwrap_or_else(|error| panic!("the job's timeout is a number: {error}"))
        })
        .unwrap_or_else(|| {
            panic!(
                "{CI_WORKFLOW} job {WINDOWS_INSTALLER_JOB} declares no timeout, so a stalled \
                 runner would hold it for the six-hour default"
            )
        });
    assert!(
        budget <= WINDOWS_INSTALLER_BUDGET_MINUTES,
        "{CI_WORKFLOW} job {WINDOWS_INSTALLER_JOB} allows itself {budget} minutes, more than the \
         {WINDOWS_INSTALLER_BUDGET_MINUTES} it is budgeted"
    );

    assert!(
        repo_root().join(INSTALLER_VERIFICATION).is_file(),
        "{CI_WORKFLOW} runs {INSTALLER_VERIFICATION}, which this repository does not publish"
    );
}

#[test]
fn the_installer_verification_refuses_on_the_words_the_installer_throws() {
    // The verification asserts a refusal by matching the message. A reworded
    // `throw` would leave the pattern matching nothing, and the run would report
    // a failure only on a Windows runner. This binds the two locally.
    let verification = read(INSTALLER_VERIFICATION);
    let installer = read("scripts/install.ps1");

    let mut patterns = Vec::new();
    let mut rest = verification.as_str();
    const PREFIX: &str = "-Pattern \"";
    while let Some(position) = rest.find(PREFIX) {
        rest = &rest[position + PREFIX.len()..];
        let (pattern, tail) = rest
            .split_once('"')
            .unwrap_or_else(|| panic!("{INSTALLER_VERIFICATION} closes every -Pattern literal"));
        patterns.push(pattern.to_owned());
        rest = tail;
    }
    assert!(
        patterns.len() >= 2,
        "{INSTALLER_VERIFICATION} asserts {} refusals; the story names two, the interrupted \
         upgrade and the mismatched digest",
        patterns.len()
    );
    for pattern in &patterns {
        assert!(
            installer
                .lines()
                .any(|line| line.contains("throw") && line.contains(pattern.as_str())),
            "{INSTALLER_VERIFICATION} expects a refusal matching `{pattern}`, which no \
             scripts/install.ps1 throw emits"
        );
    }

    let required = [
        (
            "file://",
            "the verification must install from the local release the job just packaged",
        ),
        (
            "-Uninstall",
            "the verification must exercise removal as well as installation",
        ),
        (
            "--version",
            "the verification must run the installed binary",
        ),
    ];
    for (needle, why) in required {
        assert!(
            verification.contains(needle),
            "{INSTALLER_VERIFICATION} is missing `{needle}`: {why}"
        );
    }
}
