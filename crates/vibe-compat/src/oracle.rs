use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::{NamedTempFile, TempDir};
use thiserror::Error;

use crate::SCENARIOS_TOML;
use crate::canonical::{CanonicalizationError, canonicalize, redact, volatility_evidence};
use crate::model::{
    FileDelta, OracleOutcome, RecordedFixture, Scenario, ScenarioKind, ScenarioSet,
};

#[derive(Debug, Error)]
pub enum OracleError {
    #[error("scenario inventory is invalid: {0}")]
    Scenarios(#[from] toml::de::Error),
    #[error("oracle I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("canonicalization failed: {0}")]
    Canonical(#[from] CanonicalizationError),
    #[error("scenario `{scenario}` driver failed: {detail}")]
    Driver { scenario: String, detail: String },
    #[error("scenario `{scenario}` attempted undeclared dependency: {dependency}")]
    HermeticDependency {
        scenario: String,
        dependency: String,
    },
    #[error("scenario `{scenario}` remained volatile outside declared canonical fields")]
    Unstable { scenario: String },
    #[error("scenario inventory rule failed: {0}")]
    Inventory(String),
    #[error("fixture is invalid JSON: {0}")]
    FixtureJson(#[from] serde_json::Error),
    #[error("fixture `{0}` contains a sensitive value")]
    SensitiveFixture(PathBuf),
    #[error("oracle sandbox is unavailable: {0}")]
    SandboxUnavailable(String),
    #[error("oracle output path escapes the workspace: {0}")]
    UnsafeOutput(PathBuf),
    #[error("pinned checkout changed while scenario `{scenario}` ran")]
    CheckoutModified { scenario: String },
    #[error("fresh oracle environment creation failed: {0}")]
    Environment(String),
}

pub fn load_scenarios() -> Result<ScenarioSet, OracleError> {
    Ok(toml::from_str(SCENARIOS_TOML)?)
}

pub fn validate_scenarios(matrix_rows: &BTreeSet<&str>) -> Result<ScenarioSet, OracleError> {
    let scenarios = load_scenarios()?;
    if scenarios.schema_version != 1 {
        return Err(OracleError::Inventory(
            "unsupported scenario schema version".to_owned(),
        ));
    }
    if scenarios.scenarios.len() < 20 {
        return Err(OracleError::Inventory(
            "at least 20 scenarios are required".to_owned(),
        ));
    }
    let ids = scenarios
        .scenarios
        .iter()
        .map(|scenario| scenario.id.as_str())
        .collect::<BTreeSet<_>>();
    if ids.len() != scenarios.scenarios.len() {
        return Err(OracleError::Inventory(
            "scenario IDs must be unique".to_owned(),
        ));
    }
    let kinds = scenarios
        .scenarios
        .iter()
        .map(|scenario| scenario.kind)
        .collect::<BTreeSet<_>>();
    for required in [
        ScenarioKind::Process,
        ScenarioKind::Protocol,
        ScenarioKind::Persistence,
        ScenarioKind::Pty,
    ] {
        if !kinds.contains(&required) {
            return Err(OracleError::Inventory(format!(
                "missing representative {required:?} scenario"
            )));
        }
    }
    for scenario in &scenarios.scenarios {
        if !matrix_rows.contains(scenario.matrix_row.as_str()) {
            return Err(OracleError::Inventory(format!(
                "scenario `{}` references unknown matrix row `{}`",
                scenario.id, scenario.matrix_row
            )));
        }
    }
    Ok(scenarios)
}

pub fn record_all(
    root: &Path,
    checkout: &Path,
    output: &Path,
    baseline_version: &str,
    python_version: &str,
    fixture_schema_version: u32,
    scenarios: &[Scenario],
) -> Result<Vec<PathBuf>, OracleError> {
    let root = fs::canonicalize(root)?;
    let checkout = fs::canonicalize(checkout)?;
    let output = safe_output_path(&root, output)?;
    let run_parent = root.join("target/compat/runs");
    fs::create_dir_all(&run_parent)?;
    let environment_parent = root.join("target/compat/environments");
    fs::create_dir_all(&environment_parent)?;
    let environment_root = tempfile::tempdir_in(&environment_parent)?;
    let environment = environment_root.path().join("venv");
    prepare_environment(&checkout, &environment, python_version)?;
    let staging = tempfile::tempdir_in(&run_parent)?;
    let mut recorded = Vec::with_capacity(scenarios.len());
    for scenario in scenarios {
        let first_root = tempfile::tempdir_in(&run_parent)?;
        let first = run_once(&root, &checkout, &environment, scenario, &first_root)?;
        let second_root = tempfile::tempdir_in(&run_parent)?;
        let second = run_once(&root, &checkout, &environment, scenario, &second_root)?;
        let evidence = volatility_evidence(&first, &second, &scenario.volatile)?;
        let mut canonical_first = canonicalize(&first, &scenario.volatile)?;
        let canonical_second = canonicalize(&second, &scenario.volatile)?;
        if canonical_first != canonical_second {
            return Err(OracleError::Unstable {
                scenario: scenario.id.clone(),
            });
        }
        redact(
            &mut canonical_first,
            &[
                (checkout.as_path(), "<UPSTREAM_CHECKOUT>"),
                (first_root.path(), "<RUN_ROOT>"),
                (second_root.path(), "<RUN_ROOT>"),
                (Path::new("/upstream"), "<UPSTREAM_CHECKOUT>"),
                (Path::new("/work"), "<RUN_ROOT>"),
            ],
        )?;
        let fixture = RecordedFixture {
            fixture_schema_version,
            scenario_id: scenario.id.clone(),
            matrix_row: scenario.matrix_row.clone(),
            upstream_baseline: baseline_version.to_owned(),
            comparison: scenario.comparison,
            stability_runs: 2,
            volatility: evidence,
            outcome: canonical_first,
        };
        let staged = staging.path().join(format!("{}.json", scenario.id));
        write_fixture(&staged, &fixture)?;
        recorded.push(output.join(format!("{}.json", scenario.id)));
    }
    fs::create_dir_all(&output)?;
    for path in &recorded {
        let filename = path.file_name().ok_or_else(|| {
            OracleError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "fixture path has no filename",
            ))
        })?;
        fs::rename(staging.path().join(filename), path)?;
    }
    Ok(recorded)
}

pub fn validate_corpus(
    directory: &Path,
    scenarios: &[Scenario],
    baseline_version: &str,
    fixture_schema_version: u32,
) -> Result<(), OracleError> {
    let expected_names = scenarios
        .iter()
        .map(|scenario| format!("{}.json", scenario.id))
        .collect::<BTreeSet<_>>();
    let actual_names = fs::read_dir(directory)?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|value| value == "json")
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<BTreeSet<_>>();
    if actual_names != expected_names {
        return Err(OracleError::Inventory(
            "corpus files do not exactly match the scenario inventory".to_owned(),
        ));
    }
    for scenario in scenarios {
        let path = directory.join(format!("{}.json", scenario.id));
        let bytes = fs::read(&path)?;
        let fixture: RecordedFixture = serde_json::from_slice(&bytes)?;
        if fixture.scenario_id != scenario.id
            || fixture.matrix_row != scenario.matrix_row
            || fixture.upstream_baseline != baseline_version
            || fixture.fixture_schema_version != fixture_schema_version
            || fixture.stability_runs != 2
        {
            return Err(OracleError::Inventory(format!(
                "fixture metadata mismatch: {}",
                path.display()
            )));
        }
        let mut redaction_probe = fixture.outcome.clone();
        redact(&mut redaction_probe, &[])?;
        if redaction_probe != fixture.outcome {
            return Err(OracleError::SensitiveFixture(path));
        }
    }
    Ok(())
}

fn run_once(
    root: &Path,
    checkout: &Path,
    environment: &Path,
    scenario: &Scenario,
    run_root: &TempDir,
) -> Result<OracleOutcome, OracleError> {
    fs::create_dir_all(run_root.path().join("home"))?;
    let before = snapshot(run_root.path())?;
    let python = if cfg!(windows) {
        environment.join("Scripts/python.exe")
    } else {
        environment.join("bin/python")
    };
    if !python.is_file() {
        return Err(OracleError::Driver {
            scenario: scenario.id.clone(),
            detail: format!("missing pinned environment executable {}", python.display()),
        });
    }
    if !cfg!(target_os = "linux") {
        return Err(OracleError::SandboxUnavailable(
            "the pinned oracle currently requires Linux bubblewrap".to_owned(),
        ));
    }
    if !Path::new("/usr/bin/bwrap").is_file() || !Path::new("/usr/bin/timeout").is_file() {
        return Err(OracleError::SandboxUnavailable(
            "required /usr/bin/bwrap or /usr/bin/timeout is missing".to_owned(),
        ));
    }
    let mut command = Command::new("/usr/bin/timeout");
    command.args([
        "--signal=TERM",
        "--kill-after=2s",
        "20s",
        "/usr/bin/bwrap",
        "--die-with-parent",
        "--unshare-all",
        "--new-session",
        "--clearenv",
        "--ro-bind",
        "/usr",
        "/usr",
    ]);
    for system_path in ["/lib", "/lib64"] {
        if Path::new(system_path).exists() {
            command.args(["--ro-bind", system_path, system_path]);
        }
    }
    command
        .args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"])
        .args(["--dir", "/etc", "--dir", "/run", "--dir", "/harness"])
        .arg("--ro-bind")
        .arg(checkout)
        .arg("/upstream")
        .arg("--ro-bind")
        .arg(environment)
        .arg("/oracle-venv")
        .arg("--ro-bind")
        .arg(root.join("compat/oracle_driver.py"))
        .arg("/harness/oracle_driver.py")
        .arg("--ro-bind")
        .arg(root.join("compat/audit"))
        .arg("/harness/audit")
        .arg("--bind")
        .arg(run_root.path())
        .arg("/work")
        .args(["--chdir", "/work"])
        .args(["--setenv", "HOME", "/work/home"])
        .args(["--setenv", "VIBE_HOME", "/work/home/.vibe"])
        .args(["--setenv", "PATH", "/usr/bin"])
        .args(["--setenv", "PYTHONUTF8", "1"])
        .args(["--setenv", "PYTHONDONTWRITEBYTECODE", "1"])
        .args(["--setenv", "HTTP_PROXY", "http://127.0.0.1:9"])
        .args(["--setenv", "HTTPS_PROXY", "http://127.0.0.1:9"])
        .args(["--setenv", "NO_PROXY", ""])
        .args(["--setenv", "VIBE_ORACLE_ENV", "/oracle-venv"])
        .arg("/oracle-venv/bin/python")
        .arg("/harness/oracle_driver.py")
        .arg("--upstream")
        .arg("/upstream")
        .arg("--scenario")
        .arg(&scenario.id)
        .arg("--kind")
        .arg(format!("{:?}", scenario.kind).to_ascii_lowercase());
    if let Some(payload) = &scenario.payload {
        command.arg("--payload").arg(payload);
    }
    if !scenario.args.is_empty() {
        command.arg("--").args(&scenario.args);
    }
    command.current_dir(run_root.path()).env_clear();
    let output = command.output()?;
    if !output.status.success() {
        return Err(OracleError::Driver {
            scenario: scenario.id.clone(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let driver: Value = serde_json::from_slice(&output.stdout)?;
    if let Some(dependency) = driver["externalDependencies"]
        .as_array()
        .and_then(|values| values.first())
        .and_then(Value::as_str)
    {
        return Err(OracleError::HermeticDependency {
            scenario: scenario.id.clone(),
            dependency: dependency.to_owned(),
        });
    }
    let mut outcome = OracleOutcome::empty(scenario.args.clone());
    let result = driver["result"].clone();
    match scenario.kind {
        ScenarioKind::Process => {
            outcome.exit_status = result["exitStatus"].as_i64().map(|value| value as i32);
            outcome.stdout = result["stdout"].as_str().unwrap_or_default().to_owned();
            outcome.stderr = result["stderr"].as_str().unwrap_or_default().to_owned();
        }
        ScenarioKind::Protocol | ScenarioKind::Initialize | ScenarioKind::Volatile => {
            outcome.json_frames.push(result);
        }
        ScenarioKind::Persistence => outcome.persisted_state = Some(result),
        ScenarioKind::Pty => {
            outcome.exit_status = result["exitStatus"].as_i64().map(|value| value as i32);
            outcome.terminal_transcript =
                Some(result["transcript"].as_str().unwrap_or_default().to_owned());
        }
    }
    let after = snapshot(run_root.path())?;
    outcome.filesystem_delta = diff_snapshots(&before, &after);
    assert_checkout_clean(checkout, &scenario.id)?;
    Ok(outcome)
}

fn prepare_environment(
    checkout: &Path,
    environment: &Path,
    python_version: &str,
) -> Result<(), OracleError> {
    let output = Command::new("uv")
        .args([
            "sync",
            "--frozen",
            "--no-config",
            "--no-install-project",
            "--python",
            python_version,
        ])
        .arg("--project")
        .arg(checkout)
        .env("UV_PROJECT_ENVIRONMENT", environment)
        .output()?;
    if !output.status.success() {
        return Err(OracleError::Environment(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(())
}

fn assert_checkout_clean(checkout: &Path, scenario: &str) -> Result<(), OracleError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()?;
    if !output.status.success() || !output.stdout.is_empty() {
        return Err(OracleError::CheckoutModified {
            scenario: scenario.to_owned(),
        });
    }
    Ok(())
}

fn safe_output_path(root: &Path, output: &Path) -> Result<PathBuf, OracleError> {
    let candidate = if output.is_absolute() {
        output.to_owned()
    } else {
        root.join(output)
    };
    if candidate
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
        || !candidate.starts_with(root)
    {
        return Err(OracleError::UnsafeOutput(candidate));
    }
    let mut existing = candidate.as_path();
    while !existing.exists() {
        existing = existing
            .parent()
            .ok_or_else(|| OracleError::UnsafeOutput(candidate.clone()))?;
    }
    if !fs::canonicalize(existing)?.starts_with(root) {
        return Err(OracleError::UnsafeOutput(candidate));
    }
    Ok(candidate)
}

fn snapshot(root: &Path) -> Result<BTreeMap<String, String>, OracleError> {
    fn visit(
        root: &Path,
        current: &Path,
        files: &mut BTreeMap<String, String>,
    ) -> Result<(), std::io::Error> {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, files)?;
            } else if entry.file_name() != ".audit.json" {
                let relative = path.strip_prefix(root).unwrap_or(&path);
                let digest = hex::encode(Sha256::digest(fs::read(&path)?));
                files.insert(relative.to_string_lossy().replace('\\', "/"), digest);
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files)?;
    Ok(files)
}

fn diff_snapshots(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Vec<FileDelta> {
    let paths = before.keys().chain(after.keys()).collect::<BTreeSet<_>>();
    paths
        .into_iter()
        .filter_map(|path| match (before.get(path), after.get(path)) {
            (None, Some(digest)) => Some(FileDelta {
                path: path.clone(),
                operation: "created".to_owned(),
                sha256: Some(digest.clone()),
            }),
            (Some(_), None) => Some(FileDelta {
                path: path.clone(),
                operation: "deleted".to_owned(),
                sha256: None,
            }),
            (Some(left), Some(right)) if left != right => Some(FileDelta {
                path: path.clone(),
                operation: "modified".to_owned(),
                sha256: Some(right.clone()),
            }),
            _ => None,
        })
        .collect()
}

fn write_fixture(path: &Path, fixture: &RecordedFixture) -> Result<(), OracleError> {
    let parent = path.parent().ok_or_else(|| {
        OracleError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "fixture path has no parent",
        ))
    })?;
    fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec_pretty(fixture)?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| OracleError::Io(error.error))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
