use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use thiserror::Error;

use crate::canonical::canonicalize;
use crate::model::{
    CapabilityMatrix, ComparisonMode, CompatibilityReport, RecordedFixture, Scenario, Verdict,
    VerdictStatus,
};

#[derive(Debug, Error)]
pub enum DifferentialError {
    #[error("differential I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("differential fixture JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("canonicalization failed: {0}")]
    Canonical(#[from] crate::canonical::CanonicalizationError),
    #[error("scenario `{0}` has no fixture")]
    MissingFixture(String),
}

pub fn compare_directories(
    expected_directory: &Path,
    actual_directory: &Path,
    scenarios: &[Scenario],
    intentional_rows: &BTreeSet<&str>,
) -> Result<Vec<Verdict>, DifferentialError> {
    let mut verdicts = Vec::with_capacity(scenarios.len());
    for scenario in scenarios {
        let filename = format!("{}.json", scenario.id);
        let expected_path = expected_directory.join(&filename);
        let actual_path = actual_directory.join(&filename);
        if !expected_path.exists() || !actual_path.exists() {
            verdicts.push(Verdict {
                matrix_row: scenario.matrix_row.clone(),
                scenario_id: scenario.id.clone(),
                status: VerdictStatus::Blocked,
                first_difference: Some("fixture is missing".to_owned()),
                artifacts: vec![
                    expected_path.display().to_string(),
                    actual_path.display().to_string(),
                ],
                upstream_baseline: "mistral-vibe@2.23.1".to_owned(),
                rust_build: rust_build(),
            });
            continue;
        }
        let expected: RecordedFixture = serde_json::from_slice(&fs::read(&expected_path)?)?;
        let actual: RecordedFixture = serde_json::from_slice(&fs::read(&actual_path)?)?;
        let metadata_difference = if expected.scenario_id != scenario.id
            || actual.scenario_id != scenario.id
            || expected.matrix_row != scenario.matrix_row
            || actual.matrix_row != scenario.matrix_row
            || expected.fixture_schema_version != actual.fixture_schema_version
        {
            Some("fixture metadata differs".to_owned())
        } else {
            None
        };
        let expected_outcome = canonicalize(&expected.outcome, &scenario.volatile)?;
        let actual_outcome = canonicalize(&actual.outcome, &scenario.volatile)?;
        let difference = metadata_difference
            .or_else(|| first_difference(&expected_outcome, &actual_outcome, scenario.comparison));
        let status = if difference.is_none() {
            VerdictStatus::Pass
        } else if intentional_rows.contains(scenario.matrix_row.as_str()) {
            VerdictStatus::IntentionalDivergence
        } else {
            VerdictStatus::Fail
        };
        verdicts.push(Verdict {
            matrix_row: scenario.matrix_row.clone(),
            scenario_id: scenario.id.clone(),
            status,
            first_difference: difference,
            artifacts: vec![
                expected_path.display().to_string(),
                actual_path.display().to_string(),
            ],
            upstream_baseline: expected.upstream_baseline,
            rust_build: rust_build(),
        });
    }
    verdicts.sort_by(|left, right| {
        (&left.matrix_row, &left.scenario_id).cmp(&(&right.matrix_row, &right.scenario_id))
    });
    Ok(verdicts)
}

pub fn build_report(
    matrix: &CapabilityMatrix,
    release: u32,
    mut verdicts: Vec<Verdict>,
) -> CompatibilityReport {
    verdicts.sort_by(|left, right| {
        (&left.matrix_row, &left.scenario_id).cmp(&(&right.matrix_row, &right.scenario_id))
    });
    let evidenced = verdicts
        .iter()
        .map(|verdict| verdict.matrix_row.as_str())
        .collect::<BTreeSet<_>>();
    let missing_evidence = matrix
        .rows
        .iter()
        .filter(|row| row.required_release <= release)
        .filter(|row| !evidenced.contains(row.id.as_str()))
        .map(|row| row.id.clone())
        .collect::<Vec<_>>();
    let mut summary = BTreeMap::from([
        ("blocked".to_owned(), 0),
        ("fail".to_owned(), 0),
        ("intentional-divergence".to_owned(), 0),
        ("pass".to_owned(), 0),
    ]);
    for verdict in &verdicts {
        let key = match verdict.status {
            VerdictStatus::Pass => "pass",
            VerdictStatus::Fail => "fail",
            VerdictStatus::Blocked => "blocked",
            VerdictStatus::IntentionalDivergence => "intentional-divergence",
        };
        summary
            .entry(key.to_owned())
            .and_modify(|count| *count += 1);
    }
    CompatibilityReport {
        schema_version: 1,
        upstream_baseline: format!("mistral-vibe@{}", matrix.baseline_version),
        rust_build: rust_build(),
        release,
        summary,
        verdicts,
        missing_evidence,
    }
}

pub fn report_is_release_ready(report: &CompatibilityReport) -> bool {
    report.missing_evidence.is_empty()
        && report.verdicts.iter().all(|verdict| {
            matches!(
                verdict.status,
                VerdictStatus::Pass | VerdictStatus::IntentionalDivergence
            )
        })
}

pub fn write_reports(
    report: &CompatibilityReport,
    json_path: &Path,
    markdown_path: &Path,
) -> Result<(), DifferentialError> {
    fs::write(json_path, serde_json::to_vec_pretty(report)?)?;
    fs::write(markdown_path, render_markdown(report))?;
    Ok(())
}

pub fn render_markdown(report: &CompatibilityReport) -> String {
    let mut output = format!(
        "# Compatibility report\n\nBaseline: `{}`  \nRust build: `{}`  \nRelease: `{}`\n\n",
        report.upstream_baseline, report.rust_build, report.release
    );
    output.push_str("| Matrix row | Scenario | Verdict | First difference |\n");
    output.push_str("|---|---|---|---|\n");
    for verdict in &report.verdicts {
        let difference = verdict.first_difference.as_deref().unwrap_or("");
        output.push_str(&format!(
            "| {} | {} | {:?} | {} |\n",
            verdict.matrix_row, verdict.scenario_id, verdict.status, difference
        ));
    }
    if !report.missing_evidence.is_empty() {
        output.push_str("\n## Missing evidence\n\n");
        for row in &report.missing_evidence {
            output.push_str(&format!("- `{row}`\n"));
        }
    }
    output
}

fn first_difference(
    expected: &crate::model::OracleOutcome,
    actual: &crate::model::OracleOutcome,
    mode: ComparisonMode,
) -> Option<String> {
    match mode {
        ComparisonMode::Byte => {
            if expected.stdout != actual.stdout {
                Some("stdout differs".to_owned())
            } else if expected.stderr != actual.stderr {
                Some("stderr differs".to_owned())
            } else if expected.exit_status != actual.exit_status {
                Some("exit status differs".to_owned())
            } else {
                None
            }
        }
        ComparisonMode::Schema => semantic_difference(
            &serde_json::to_value(&expected.json_frames).ok()?,
            &serde_json::to_value(&actual.json_frames).ok()?,
            "",
        ),
        ComparisonMode::Filesystem => {
            if expected.filesystem_delta != actual.filesystem_delta {
                Some("filesystem delta differs".to_owned())
            } else {
                semantic_difference(
                    &expected.persisted_state.clone().unwrap_or(Value::Null),
                    &actual.persisted_state.clone().unwrap_or(Value::Null),
                    "/persistedState",
                )
            }
        }
        ComparisonMode::Pty => {
            if expected.terminal_transcript != actual.terminal_transcript {
                Some("terminal transcript differs".to_owned())
            } else if expected.exit_status != actual.exit_status {
                Some("PTY exit status differs".to_owned())
            } else {
                None
            }
        }
        ComparisonMode::Semantic => {
            let expected = serde_json::to_value(expected).ok()?;
            let actual = serde_json::to_value(actual).ok()?;
            semantic_difference(&expected, &actual, "")
        }
    }
}

fn semantic_difference(expected: &Value, actual: &Value, path: &str) -> Option<String> {
    match (expected, actual) {
        (Value::Object(left), Value::Object(right)) => {
            let keys = left.keys().chain(right.keys()).collect::<BTreeSet<_>>();
            for key in keys {
                let next = format!("{path}/{key}");
                match (left.get(key), right.get(key)) {
                    (Some(left), Some(right)) => {
                        if let Some(difference) = semantic_difference(left, right, &next) {
                            return Some(difference);
                        }
                    }
                    _ => return Some(format!("first semantic difference at {next}")),
                }
            }
            None
        }
        (Value::Array(left), Value::Array(right)) => {
            if left.len() != right.len() {
                return Some(format!("first semantic difference at {path}/length"));
            }
            for (index, (left, right)) in left.iter().zip(right).enumerate() {
                if let Some(difference) =
                    semantic_difference(left, right, &format!("{path}/{index}"))
                {
                    return Some(difference);
                }
            }
            None
        }
        _ if expected == actual => None,
        _ => Some(format!(
            "first semantic difference at {}",
            if path.is_empty() { "/" } else { path }
        )),
    }
}

fn rust_build() -> String {
    format!("mistral-vibe-rs@{}", env!("CARGO_PKG_VERSION"))
}

pub fn load_fixture(path: &Path) -> Result<RecordedFixture, DifferentialError> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

pub fn fixture_path(directory: &Path, scenario_id: &str) -> PathBuf {
    directory.join(format!("{scenario_id}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CapabilityRow, CompatibilityReport, OracleOutcome};

    fn matrix() -> CapabilityMatrix {
        CapabilityMatrix {
            schema_version: 1,
            baseline_version: "2.23.1".to_owned(),
            rows: vec![CapabilityRow {
                id: "required".to_owned(),
                owner: "US-001".to_owned(),
                priority: "P0".to_owned(),
                source_paths: vec![],
                test_paths: vec![],
                symbols: vec![],
                fixture_class: "test".to_owned(),
                rust_status: "implemented".to_owned(),
                divergence_status: "none".to_owned(),
                dependencies: vec![],
                required_release: 0,
                items: vec![],
                divergence: None,
            }],
        }
    }

    #[test]
    fn release_report_fails_closed_without_evidence() {
        let report = build_report(&matrix(), 0, vec![]);
        assert_eq!(report.missing_evidence, ["required"]);
        assert!(!report_is_release_ready(&report));
    }

    #[test]
    fn semantic_comparison_preserves_event_order_and_error_codes() {
        let mut expected = OracleOutcome::empty(vec![]);
        expected.public_events = vec![
            serde_json::json!({"code": "conflict"}),
            serde_json::json!({"code": "forbidden"}),
        ];
        let mut actual = expected.clone();
        actual.public_events.reverse();
        assert_eq!(
            first_difference(&expected, &actual, ComparisonMode::Semantic),
            Some("first semantic difference at /publicEvents/0/code".to_owned())
        );
    }

    #[test]
    fn markdown_and_json_are_deterministic() {
        let report = CompatibilityReport {
            schema_version: 1,
            upstream_baseline: "mistral-vibe@2.23.1".to_owned(),
            rust_build: "mistral-vibe-rs@0.0.1".to_owned(),
            release: 0,
            summary: BTreeMap::new(),
            verdicts: vec![],
            missing_evidence: vec!["required".to_owned()],
        };
        assert_eq!(render_markdown(&report), render_markdown(&report));
        assert_eq!(
            serde_json::to_vec(&report).expect("report JSON"),
            serde_json::to_vec(&report).expect("report JSON")
        );
    }
}
