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
const PROBES_OUTSIDE_PARITY_PATHS: [(&str, &str); 3] = [
    // Every e2e trial is exempt through its own `binary(e2e)` override.
    ("crates/vibe-cli/tests/e2e/reference.rs", "binary(e2e)"),
    // A helper both `telemetry_session_parity_tests` binaries include.
    (
        "crates/vibe-cli/tests/support/telemetry_session.rs",
        "binary(/parity/)",
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

/// The version `text` pins right after `prefix` (`ruff==`, `mypy==`).
fn pinned_version<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = text.split_once(prefix)?.1;
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    Some(&rest[..end]).filter(|version| !version.is_empty())
}

/// US-027 of `tasks/prd-trustworthy-parity-v3.md`: the Linux gate lints and
/// type-checks `scripts/parity/` at the versions its requirements file pins,
/// which are the ones AGENTS.md runs locally, and ruff's is the one the
/// committed configuration requires. The workflow itself carries no version
/// literal (`release_parity_tests` in `vibe-cli`).
#[test]
fn the_parity_scripts_are_linted_and_type_checked_at_one_version_of_each_tool() {
    let workflow = read(CI);
    let agents = read("AGENTS.md");
    let config = read("scripts/parity/pyproject.toml");
    let requirements = read("scripts/parity/lint-requirements.txt");
    let ruff = pinned_version(&requirements, "\nruff==").expect("a pinned ruff");
    let mypy = pinned_version(&requirements, "\nmypy==").expect("a pinned mypy");
    assert!(
        workflow.contains("working-directory: scripts/parity")
            && workflow.contains("--requirement lint-requirements.txt\n")
            && workflow.contains("          ruff check .\n")
            && workflow.contains("          mypy\n"),
        "{CI}: the quality job installs the pinned tools and runs ruff and mypy from scripts/parity"
    );
    assert!(
        config.contains(&format!("required-version = \"=={ruff}\"")),
        "scripts/parity/pyproject.toml requires a ruff other than the {ruff} the requirements pin"
    );
    assert!(
        agents.contains(&format!("uvx ruff@{ruff} check . && uvx mypy=={mypy}")),
        "AGENTS.md runs other versions than the ruff {ruff} and mypy {mypy} the requirements pin"
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

/// The pattern the `Count windows ignore tags` step of [`CI`] counts a tagged
/// test by, line by line: one string literal naming its cause and stories.
const COUNTED_TAG: &str = r#""windows: [^"]*\(US-[0-9]+(, US-[0-9]+)*\)""#;
/// The opening the same step counts a test ignored by design with.
const BY_DESIGN: &str = r#""windows by design: "#;

/// The string literal opening at `start` of `text`, quotes included.
fn literal_at(text: &str, start: usize) -> &str {
    let mut escaped = false;
    for (offset, character) in text[start + 1..].char_indices() {
        match character {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => return &text[start..start + 1 + offset + 1],
            _ => escaped = false,
        }
    }
    &text[start..]
}

/// Every reason a test is skipped on Windows with, under AGENTS.md: a tag
/// `"windows: <cause> (US-NNN)"` the CI summary counts, or a reason opening
/// `"windows by design: "`. A reason the step's line-by-line `grep` cannot
/// see (one spread over lines, one missing its story, a constant or no reason
/// at all on an `ignore` attribute) leaves a Windows-only failure uncounted.
#[test]
fn every_windows_ignore_reason_is_one_the_ci_summary_counts() {
    use regex::Regex;

    let ci = read(CI);
    assert!(
        ci.contains(&format!(
            "grep -rhoE '{COUNTED_TAG}' crates --include='*.rs'"
        )) && ci.contains(&format!("grep -rhoE '{BY_DESIGN}' crates --include='*.rs'")),
        "{CI} no longer counts tags with `{COUNTED_TAG}` and `{BY_DESIGN}`: update both"
    );
    let counted = Regex::new(COUNTED_TAG).expect("the counted pattern compiles");
    let attribute = Regex::new(
        r#"cfg_attr\(\s*(?:windows|not\(unix\)|target_os\s*=\s*"windows")\s*,\s*ignore\b\s*(=\s*)?"#,
    )
    .expect("the attribute pattern compiles");
    let root = repository();
    let mut sources = Vec::new();
    rust_sources(&root.join("crates"), &mut sources);
    let mut offenders = Vec::new();
    let mut tags = 0;
    for path in &sources {
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if relative == "crates/vibe-core/src/parity/workflow_tests.rs" {
            continue;
        }
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let line_of = |offset: usize| text[..offset].matches('\n').count() + 1;
        for (start, _) in text.match_indices("\"windows: ") {
            let reason = literal_at(&text, start);
            tags += 1;
            if reason.contains('\n') || !counted.is_match(reason) {
                offenders.push(format!("{relative}:{}: {reason}", line_of(start)));
            }
        }
        for found in attribute.captures_iter(&text) {
            let whole = found.get(0).expect("a match");
            let rest = &text[whole.end()..];
            // An attribute quoted inside a string literal is a fixture's text,
            // whose reason opens with an escaped quote.
            if rest.starts_with("\\\"") {
                continue;
            }
            let reasoned = found.get(1).is_some()
                && rest.starts_with('"')
                && (rest.starts_with(BY_DESIGN) || rest.starts_with("\"windows: "));
            if !reasoned {
                let shown: String = rest.chars().take(60).collect();
                offenders.push(format!(
                    "{relative}:{}: an ignore reason that is not a tag literal: {shown}",
                    line_of(whole.start())
                ));
            }
        }
    }
    assert!(
        tags > 0,
        "no `windows:` tag found, so the scan reads nothing"
    );
    assert!(
        offenders.is_empty(),
        "these Windows ignore reasons are not counted by the CI summary; write each as one \
         literal `\"windows: <cause> (US-NNN)\"` or `\"windows by design: <cause>\"` \
         (AGENTS.md, Quality gates):\n{}",
        offenders.join("\n")
    );
}
