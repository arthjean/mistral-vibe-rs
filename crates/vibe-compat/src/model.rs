use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct Baseline {
    pub schema_version: u32,
    pub product: String,
    pub version: String,
    pub commit: String,
    pub tree: String,
    pub source_url: String,
    pub checkout: String,
    pub archive_sha256: String,
    pub lockfile_sha256: String,
    pub python_version: String,
    pub platform: String,
    pub fixture_schema_version: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CapabilityMatrix {
    pub schema_version: u32,
    pub baseline_version: String,
    pub rows: Vec<CapabilityRow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CapabilityRow {
    pub id: String,
    pub owner: String,
    pub support: SupportClass,
    pub priority: String,
    pub source_paths: Vec<String>,
    pub test_paths: Vec<String>,
    pub symbols: Vec<String>,
    pub fixture_class: String,
    pub rust_status: String,
    pub divergence_status: String,
    pub dependencies: Vec<String>,
    pub required_release: u32,
    #[serde(default)]
    pub items: Vec<String>,
    pub divergence: Option<DivergenceDeclaration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SupportClass {
    RequiredNative,
    Excluded,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DivergenceDeclaration {
    pub rationale: String,
    pub scope: String,
    pub upstream_fixture: String,
    pub rust_fixture: String,
    pub documentation: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiscoveredSurfaces {
    pub schema_version: u32,
    pub known_rows: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScenarioSet {
    pub schema_version: u32,
    pub scenarios: Vec<Scenario>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Scenario {
    pub id: String,
    pub matrix_row: String,
    pub kind: ScenarioKind,
    pub comparison: ComparisonMode,
    #[serde(default)]
    pub args: Vec<String>,
    pub payload: Option<String>,
    #[serde(default)]
    pub volatile: Vec<CanonicalRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioKind {
    Process,
    Protocol,
    Initialize,
    Persistence,
    Pty,
    Volatile,
    Contract,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonMode {
    Byte,
    Schema,
    Semantic,
    Filesystem,
    Pty,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalRule {
    pub pointer: String,
    pub placeholder: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct FileDelta {
    pub path: String,
    pub operation: String,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct OracleOutcome {
    pub argv: Vec<String>,
    pub stdin: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_status: Option<i32>,
    pub json_frames: Vec<Value>,
    pub public_events: Vec<Value>,
    pub filesystem_delta: Vec<FileDelta>,
    pub persisted_state: Option<Value>,
    pub terminal_transcript: Option<String>,
    pub failure: Option<String>,
}

impl OracleOutcome {
    #[must_use]
    pub fn empty(argv: Vec<String>) -> Self {
        Self {
            argv,
            stdin: String::new(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: None,
            json_frames: Vec::new(),
            public_events: Vec::new(),
            filesystem_delta: Vec::new(),
            persisted_state: None,
            terminal_transcript: None,
            failure: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct VolatilityEvidence {
    pub pointer: String,
    pub placeholder: String,
    pub changed_between_runs: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct RecordedFixture {
    pub fixture_schema_version: u32,
    pub scenario_id: String,
    pub matrix_row: String,
    pub upstream_baseline: String,
    pub comparison: ComparisonMode,
    pub stability_runs: u8,
    pub volatility: Vec<VolatilityEvidence>,
    pub outcome: OracleOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerdictStatus {
    Pass,
    Fail,
    Blocked,
    IntentionalDivergence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Verdict {
    pub matrix_row: String,
    pub scenario_id: String,
    pub status: VerdictStatus,
    pub first_difference: Option<String>,
    pub artifacts: Vec<String>,
    pub upstream_baseline: String,
    pub rust_build: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompatibilityReport {
    pub schema_version: u32,
    pub upstream_baseline: String,
    pub rust_build: String,
    pub release: u32,
    pub summary: BTreeMap<String, usize>,
    pub native_summary: BTreeMap<String, usize>,
    pub excluded_summary: BTreeMap<String, usize>,
    pub verdicts: Vec<Verdict>,
    pub missing_evidence: Vec<String>,
    pub certification_failures: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matrix_with_support(support: Option<&str>) -> String {
        format!(
            r#"
schema_version = 2
baseline_version = "2.23.1"

[[rows]]
id = "surface.example"
owner = "US-001"
{}
priority = "P0"
source_paths = []
test_paths = []
symbols = ["Example"]
fixture_class = "contract"
rust_status = "implemented"
divergence_status = "none"
dependencies = []
required_release = 1
"#,
            support.map_or_else(String::new, |value| format!("support = \"{value}\""))
        )
    }

    #[test]
    fn support_classification_is_required_and_closed() {
        assert!(
            toml::from_str::<CapabilityMatrix>(&matrix_with_support(None)).is_err(),
            "a missing support classification must fail"
        );
        assert!(
            toml::from_str::<CapabilityMatrix>(&matrix_with_support(Some("future"))).is_err(),
            "an unknown support classification must fail"
        );
        let parsed =
            toml::from_str::<CapabilityMatrix>(&matrix_with_support(Some("required-native")))
                .expect("known support classification");
        assert_eq!(parsed.rows[0].support, SupportClass::RequiredNative);
    }
}
