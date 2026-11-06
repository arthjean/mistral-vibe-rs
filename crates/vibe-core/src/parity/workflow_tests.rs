//! The CI contract of live measurement, read from the files that carry it.
//!
//! The scheduled workflows execute upstream code or read upstream trees, so
//! what keeps them safe is written in their YAML: their triggers, their token,
//! their secrets, their action pins and their cache keys. The parity gate of
//! the ordinary CI jobs and the nextest retry policy are written the same way.
//! These tests fail when any of it drifts (US-006 to US-008 of
//! `tasks/prd-trustworthy-parity-v3.md`).

use std::fs;

use super::replay::repository;

const LIVE: &str = ".github/workflows/parity-live.yml";
const SENTINEL: &str = ".github/workflows/upstream-sentinel.yml";
const CI: &str = ".github/workflows/ci.yml";
const NEXTEST: &str = ".config/nextest.toml";

/// The nextest filter every profile exempts from retries.
const PARITY_FILTER: &str =
    "test(/parity|pinned_reference|each_other_s_secrets/) | binary(/parity/)";

/// The files outside a parity-named path that run a live probe, each with the
/// test name term of [`PARITY_FILTER`] or the override that keeps it from
/// being retried.
const PROBES_OUTSIDE_PARITY_PATHS: [(&str, &str); 4] = [
    // Every e2e trial is exempt through its own `binary(e2e)` override.
    ("crates/vibe-cli/tests/e2e/reference.rs", "binary(e2e)"),
    // A helper both `telemetry_session_parity_tests` binaries include.
    (
        "crates/vibe-cli/tests/support/telemetry_session.rs",
        "binary(/parity/)",
    ),
    (
        "crates/vibe-core/src/tools/builtins/web_fetch/markdown/markdown_tests.rs",
        "pinned_reference",
    ),
    (
        "crates/vibe-core/src/auth/keyring/win_vault/win_vault_tests.rs",
        "each_other_s_secrets",
    ),
];

fn read(path: &str) -> String {
    fs::read_to_string(repository().join(path)).unwrap_or_else(|error| panic!("{path}: {error}"))
}

/// The keys of the top-level `on:` block.
fn triggers(workflow: &str) -> Vec<String> {
    let mut inside = false;
    let mut found = Vec::new();
    for line in workflow.lines() {
        if line.starts_with("on:") {
            inside = true;
            continue;
        }
        if inside {
            if !line.starts_with(' ') && !line.trim().is_empty() && !line.starts_with('#') {
                break;
            }
            if let Some(key) = line
                .strip_prefix("  ")
                .and_then(|rest| rest.strip_suffix(':'))
                .filter(|key| !key.starts_with(' '))
            {
                found.push(key.to_owned());
            }
        }
    }
    found
}

/// Asserts what every workflow that runs or reads upstream code declares.
fn assert_isolated(path: &str) {
    let workflow = read(path);
    assert_eq!(
        triggers(&workflow),
        ["schedule", "workflow_dispatch"],
        "{path} must trigger on a schedule and by hand only"
    );
    assert!(
        workflow.contains("\npermissions:\n  contents: read\n"),
        "{path} must declare `permissions: contents: read` at the top level"
    );
    assert_eq!(
        workflow.matches("permissions:").count(),
        1,
        "{path} must not widen the token in a job"
    );
    assert!(
        !workflow.contains("secrets."),
        "{path} must reference no secret"
    );
    for line in workflow.lines().filter(|line| {
        line.trim_start().starts_with("- uses:") || line.trim_start().starts_with("uses:")
    }) {
        let action = line
            .split("uses:")
            .nth(1)
            .map(str::trim)
            .unwrap_or_default();
        let pinned = action
            .split_once('@')
            .is_some_and(|(_, sha)| sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(pinned, "{path} must pin `{action}` by commit SHA");
    }
    assert!(
        workflow.contains("persist-credentials: false"),
        "{path} must not keep the token in the checkout"
    );
}

#[test]
fn the_live_workflow_runs_on_a_schedule_with_a_read_only_token_and_pinned_actions() {
    assert_isolated(LIVE);
    let workflow = read(LIVE);
    assert!(
        workflow.contains("    timeout-minutes: 90\n"),
        "{LIVE} is bounded at 90 minutes"
    );
    assert!(
        workflow.contains("repin.py materialize --accepted"),
        "{LIVE} materializes every commit the manifest accepts"
    );
    assert!(
        workflow.contains("VIBE_PARITY_REQUIRE_LIVE=1"),
        "{LIVE} runs the suite in require-live mode"
    );
    assert!(
        workflow.contains("python3 scripts/parity/repin.py check --unprobed"),
        "{LIVE} recaptures the drivers no live probe calls, so every driver's reference side runs"
    );
    assert!(
        workflow.contains("--require-live --require-drivers")
            && workflow.contains("--materialization-failed")
            && workflow.contains("--timed-out"),
        "{LIVE} summarizes by class, every driver, materialization failures and timeouts"
    );
    let key = workflow
        .lines()
        .find_map(|line| line.trim().strip_prefix("key: "))
        .expect("the live workflow keys its cache");
    assert!(
        key.starts_with("parity-live-") && key.contains("steps.accepted.outputs.key"),
        "{LIVE} keys its caches by the accepted commits under its own prefix: {key}"
    );
    assert!(
        !workflow.contains("restore-keys"),
        "{LIVE} restores by exact key only"
    );
    // The suite's own timeout ends inside the job's, however long the
    // materialization took, so the summary still runs and names the last
    // completed probe instead of the job timeout cutting both.
    assert!(
        workflow.contains("PARITY_LIVE_DEADLINE=$(( $(date +%s) + 85 * 60 ))")
            && workflow.contains("budget=$(( PARITY_LIVE_DEADLINE - $(date +%s) ))")
            && workflow.contains("timeout --signal=INT --kill-after=2m \"${budget}s\""),
        "{LIVE} bounds the suite by a deadline inside the job timeout"
    );
}

#[test]
fn no_other_workflow_reads_or_writes_the_live_caches() {
    for path in [
        CI,
        SENTINEL,
        ".github/workflows/release.yml",
        ".github/workflows/action.yml",
    ] {
        let Ok(workflow) = fs::read_to_string(repository().join(path)) else {
            continue;
        };
        assert!(
            !workflow.contains("parity-live-"),
            "{path} touches the live workflow's cache keys"
        );
    }
}

#[test]
fn the_sentinel_runs_daily_with_a_read_only_token_and_fails_while_the_pin_is_behind() {
    assert_isolated(SENTINEL);
    let workflow = read(SENTINEL);
    assert!(
        workflow.contains("cron: \"41 5 * * *\""),
        "{SENTINEL} runs daily"
    );
    assert!(
        workflow.contains("repin.py census --check") && workflow.contains("GITHUB_STEP_SUMMARY"),
        "{SENTINEL} reports the census in its summary and fails on drift"
    );
}

#[test]
fn the_linux_and_windows_gates_run_the_agents_sequence_and_count_parity() {
    let workflow = read(CI);
    assert!(
        workflow.contains(
            "env -u FORCE_COLOR VIBE_HOME=\"${home}\" cargo +stable test --workspace --all-features --no-fail-fast"
        ),
        "{CI}: the Linux gate runs the AGENTS.md test line"
    );
    assert!(
        !workflow.contains("cargo +stable test --workspace --all-features\n"),
        "{CI}: a bare `cargo test` gate is back"
    );
    assert_eq!(
        workflow.matches("scripts/parity/live_summary.py").count(),
        2,
        "{CI}: both quality jobs summarize what the suite measured"
    );
    assert!(
        workflow.contains("--junit target/nextest/ci/junit.xml"),
        "{CI}: the Windows summary reads the nextest report"
    );
}

#[test]
fn no_nextest_profile_retries_a_parity_test_and_the_ci_report_lists_skips() {
    let config: toml::Table =
        toml::from_str(&read(NEXTEST)).expect("the nextest configuration parses");
    let profiles = config["profile"].as_table().expect("nextest profiles");
    for (name, profile) in profiles {
        let retries = profile
            .get("retries")
            .and_then(toml::Value::as_integer)
            .unwrap_or(0);
        if retries == 0 {
            continue;
        }
        let overridden = profile
            .get("overrides")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .any(|entry| {
                entry["filter"].as_str() == Some(PARITY_FILTER)
                    && entry.get("retries").and_then(toml::Value::as_integer) == Some(0)
            });
        assert!(
            overridden,
            "profile {name} retries tests and does not exempt parity: a retry hides what the \
             first run measured"
        );
    }
    let junit = &profiles["ci"]["junit"];
    assert_eq!(junit["path"].as_str(), Some("junit.xml"));
    assert_eq!(junit["report-skipped"].as_str(), Some("ignored"));
    assert_eq!(
        profiles["parity-live"]["retries"].as_integer(),
        Some(0),
        "the live profile never retries"
    );
}

/// Every Rust source file under `directory`, recursively.
fn rust_sources(directory: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

#[test]
fn every_live_probe_outside_a_parity_path_is_exempt_from_retries() {
    let root = repository();
    let mut sources = Vec::new();
    rust_sources(&root.join("crates"), &mut sources);
    let mut callers: Vec<String> = sources
        .iter()
        .filter_map(|path| {
            let relative = path
                .strip_prefix(&root)
                .ok()?
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let text = fs::read_to_string(path).ok()?;
            let probes = [
                "pinned_reference(",
                "pinned_reference_native(",
                ".probe(",
                "pinned_environment(",
            ]
            .iter()
            .any(|call| text.contains(call));
            (probes && !relative.contains("parity")).then_some(relative)
        })
        .collect();
    callers.sort();
    let mut listed: Vec<String> = PROBES_OUTSIDE_PARITY_PATHS
        .iter()
        .map(|(path, _)| (*path).to_owned())
        .collect();
    listed.sort();
    assert_eq!(
        callers, listed,
        "a live probe outside a parity-named path is retried by the `ci` profile unless \
         {NEXTEST} exempts it: add its test name to the filter `{PARITY_FILTER}` and list it here"
    );
    let filter = read(NEXTEST);
    for (path, term) in PROBES_OUTSIDE_PARITY_PATHS {
        assert!(
            PARITY_FILTER.contains(term) || filter.contains(&format!("filter = \"{term}\"")),
            "{path}: `{term}` exempts none of its probes from retries"
        );
        if !term.contains('(') {
            let text = read(path);
            assert!(
                text.lines()
                    .any(|line| line.starts_with("fn ") && line.contains(term)),
                "{path}: no test function is named after `{term}`, so the filter misses it"
            );
        }
    }
}
