use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use vibe_core::process::{ProcessSpec, TerminalManager};

use crate::differential::report_is_release_ready;
use crate::model::CompatibilityReport;

pub const RELEASE_EVIDENCE_SCHEMA_VERSION: u32 = 1;
pub const RELEASE_NUMBER: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NativeTarget {
    LinuxX86_64,
    LinuxAarch64,
    MacosX86_64,
    MacosAarch64,
    WindowsX86_64,
}

impl NativeTarget {
    pub const ALL: [Self; 5] = [
        Self::LinuxX86_64,
        Self::LinuxAarch64,
        Self::MacosX86_64,
        Self::MacosAarch64,
        Self::WindowsX86_64,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "linux-x86_64",
            Self::LinuxAarch64 => "linux-aarch64",
            Self::MacosX86_64 => "macos-x86_64",
            Self::MacosAarch64 => "macos-aarch64",
            Self::WindowsX86_64 => "windows-x86_64",
        }
    }

    pub(crate) const fn expected_host(self) -> (&'static str, &'static str) {
        match self {
            Self::LinuxX86_64 => ("linux", "x86_64"),
            Self::LinuxAarch64 => ("linux", "aarch64"),
            Self::MacosX86_64 => ("macos", "x86_64"),
            Self::MacosAarch64 => ("macos", "aarch64"),
            Self::WindowsX86_64 => ("windows", "x86_64"),
        }
    }

    pub(crate) fn required_suites(self) -> &'static [&'static str] {
        match self {
            Self::LinuxX86_64 | Self::LinuxAarch64 => &[
                "acp",
                "cli",
                "filesystem",
                "keyring_failure",
                "managed_terminal",
                "package_uninstall",
                "persistence",
                "posix_shell",
                "proxy_tls",
                "signals",
                "tui",
            ],
            Self::MacosX86_64 | Self::MacosAarch64 => &[
                "acp",
                "cli",
                "filesystem",
                "gatekeeper",
                "homebrew_paths",
                "keychain",
                "login_shells",
                "notifications",
                "persistence",
                "proxy_tls",
                "pty",
                "shell",
                "terminal_encodings",
                "tui",
            ],
            Self::WindowsX86_64 => &[
                "acp",
                "cli",
                "cmd",
                "console_resize",
                "conpty",
                "credential_store",
                "filesystem",
                "git_bash",
                "locked_files",
                "path_matrix",
                "persistence",
                "powershell",
                "process_tree",
                "proxy_tls",
                "signals",
                "tui",
            ],
        }
    }
}

impl fmt::Display for NativeTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for NativeTarget {
    type Err = ReleaseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "linux-x86_64" => Ok(Self::LinuxX86_64),
            "linux-aarch64" => Ok(Self::LinuxAarch64),
            "macos-x86_64" => Ok(Self::MacosX86_64),
            "macos-aarch64" => Ok(Self::MacosAarch64),
            "windows-x86_64" => Ok(Self::WindowsX86_64),
            _ => Err(ReleaseError::UnknownTarget(value.to_owned())),
        }
    }
}

impl Serialize for NativeTarget {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for NativeTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupEvidence {
    pub trials: u64,
    pub terminated_within_500_ms: u64,
    pub terminated_within_5_s: u64,
    pub orphaned_after_5_s: u64,
}

impl CleanupEvidence {
    fn validate(&self, target: NativeTarget, failures: &mut Vec<String>) {
        if self.trials < 10_000 {
            failures.push(format!(
                "{target}: cleanup evidence has fewer than 10000 trials"
            ));
        }
        if self.terminated_within_5_s != self.trials || self.orphaned_after_5_s != 0 {
            failures.push(format!(
                "{target}: cleanup evidence does not prove 100% termination within 5 seconds"
            ));
        }
        if self.terminated_within_500_ms.saturating_mul(100) < self.trials.saturating_mul(99) {
            failures.push(format!(
                "{target}: cleanup evidence does not prove 99% termination within 500 ms"
            ));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SigningEvidence {
    pub checksum_verified: bool,
    pub signed: bool,
    pub notarized: bool,
    pub clean_host_download: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeEvidence {
    pub schema_version: u32,
    pub target: NativeTarget,
    pub host_os: String,
    pub host_arch: String,
    pub executed_natively: bool,
    pub source_revision: String,
    pub artifact: PathBuf,
    pub artifact_sha256: String,
    pub host_runtime: String,
    pub minimum_runtime: String,
    pub suites: BTreeMap<String, bool>,
    pub cleanup: CleanupEvidence,
    pub signing: SigningEvidence,
}

impl NativeEvidence {
    pub fn capture(root: &Path, input: NativeCaptureInput) -> Result<Self, ReleaseError> {
        let target = input.target;
        let (expected_os, expected_arch) = target.expected_host();
        let host_os = std::env::consts::OS.to_owned();
        let host_arch = std::env::consts::ARCH.to_owned();
        if host_os != expected_os || host_arch != expected_arch {
            return Err(ReleaseError::NonNativeCapture {
                target,
                actual: format!("{host_os}-{host_arch}"),
            });
        }
        let host_runtime = detect_host_runtime(target)?;
        validate_root_relative(&input.artifact)?;
        let artifact_sha256 = sha256_file(&root.join(&input.artifact))?;
        let evidence = Self {
            schema_version: RELEASE_EVIDENCE_SCHEMA_VERSION,
            target,
            host_os,
            host_arch,
            executed_natively: true,
            source_revision: input.source_revision,
            artifact: input.artifact,
            artifact_sha256,
            host_runtime,
            minimum_runtime: input.minimum_runtime,
            suites: input.suites,
            cleanup: input.cleanup,
            signing: input.signing,
        };
        let failures = evidence.validation_failures(root);
        if !failures.is_empty() {
            return Err(ReleaseError::Certification(failures));
        }
        Ok(evidence)
    }

    #[must_use]
    pub fn validation_failures(&self, root: &Path) -> Vec<String> {
        let mut failures = Vec::new();
        let (expected_os, expected_arch) = self.target.expected_host();
        if self.schema_version != RELEASE_EVIDENCE_SCHEMA_VERSION {
            failures.push(format!("{}: unsupported evidence schema", self.target));
        }
        if !self.executed_natively || self.host_os != expected_os || self.host_arch != expected_arch
        {
            failures.push(format!(
                "{}: evidence was not recorded on the declared native host",
                self.target
            ));
        }
        if !valid_revision(&self.source_revision) {
            failures.push(format!("{}: source revision is invalid", self.target));
        }
        if validate_root_relative(&self.artifact).is_err() {
            failures.push(format!(
                "{}: artifact path is not root-relative",
                self.target
            ));
        } else {
            match sha256_file(&root.join(&self.artifact)) {
                Ok(actual) if actual == self.artifact_sha256 => {}
                Ok(_) => failures.push(format!("{}: artifact checksum mismatch", self.target)),
                Err(_) => failures.push(format!("{}: artifact is unavailable", self.target)),
            }
        }
        let required = self.target.required_suites();
        for suite in required {
            if self.suites.get(*suite) != Some(&true) {
                failures.push(format!("{}: suite `{suite}` is not passing", self.target));
            }
        }
        if self.suites.values().any(|passed| !passed) {
            failures.push(format!(
                "{}: at least one recorded suite failed",
                self.target
            ));
        }
        let minimum_runtime_valid = match self.target {
            NativeTarget::LinuxX86_64 | NativeTarget::LinuxAarch64 => {
                self.minimum_runtime.starts_with("glibc-")
            }
            NativeTarget::MacosX86_64 | NativeTarget::MacosAarch64 => {
                self.minimum_runtime.starts_with("macos-")
            }
            NativeTarget::WindowsX86_64 => self.minimum_runtime.starts_with("windows-"),
        };
        if !minimum_runtime_valid {
            failures.push(format!(
                "{}: minimum runtime metadata is absent or invalid",
                self.target
            ));
        }
        if self.host_runtime != self.minimum_runtime {
            failures.push(format!(
                "{}: declared minimum runtime was not exercised on the native host",
                self.target
            ));
        }
        if !self.signing.checksum_verified
            || !self.signing.signed
            || !self.signing.clean_host_download
        {
            failures.push(format!(
                "{}: checksum, signing, or clean-host download evidence is incomplete",
                self.target
            ));
        }
        if matches!(
            self.target,
            NativeTarget::MacosX86_64 | NativeTarget::MacosAarch64
        ) && !self.signing.notarized
        {
            failures.push(format!("{}: notarization evidence is absent", self.target));
        }
        self.cleanup.validate(self.target, &mut failures);
        failures
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeCaptureInput {
    pub target: NativeTarget,
    pub source_revision: String,
    pub artifact: PathBuf,
    pub minimum_runtime: String,
    pub suites: BTreeMap<String, bool>,
    pub cleanup: CleanupEvidence,
    pub signing: SigningEvidence,
}

pub fn validate_native_evidence_set(
    root: &Path,
    directory: &Path,
    source_revision: &str,
) -> Result<Vec<NativeEvidence>, ReleaseError> {
    let mut evidence = Vec::with_capacity(NativeTarget::ALL.len());
    let mut failures = Vec::new();
    for target in NativeTarget::ALL {
        let path = directory.join(format!("{target}.json"));
        let item: NativeEvidence = match read_json(&path) {
            Ok(item) => item,
            Err(error) => {
                failures.push(format!("{target}: {error}"));
                continue;
            }
        };
        if item.target != target {
            failures.push(format!("{target}: evidence target does not match filename"));
        }
        if item.source_revision != source_revision {
            failures.push(format!("{target}: source revision differs from candidate"));
        }
        failures.extend(item.validation_failures(root));
        evidence.push(item);
    }
    if failures.is_empty() {
        Ok(evidence)
    } else {
        Err(ReleaseError::Certification(failures))
    }
}

pub async fn benchmark_cleanup(
    trials: u64,
    concurrency: usize,
) -> Result<CleanupEvidence, ReleaseError> {
    if trials == 0 || concurrency == 0 || concurrency > 256 {
        return Err(ReleaseError::InvalidBenchmark);
    }
    let mut completed = 0_u64;
    let mut within_500_ms = 0_u64;
    let mut within_5_s = 0_u64;
    let mut orphaned = 0_u64;
    while completed < trials {
        let batch = usize::try_from((trials - completed).min(concurrency as u64))
            .map_err(|_| ReleaseError::InvalidBenchmark)?;
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..batch {
            tasks.spawn(cleanup_trial());
        }
        while let Some(result) = tasks.join_next().await {
            completed += 1;
            match result {
                Ok(Ok(duration)) => {
                    if duration <= std::time::Duration::from_millis(500) {
                        within_500_ms += 1;
                    }
                    if duration <= std::time::Duration::from_secs(5) {
                        within_5_s += 1;
                    } else {
                        orphaned += 1;
                    }
                }
                Ok(Err(_)) | Err(_) => orphaned += 1,
            }
        }
    }
    Ok(CleanupEvidence {
        trials,
        terminated_within_500_ms: within_500_ms,
        terminated_within_5_s: within_5_s,
        orphaned_after_5_s: orphaned,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactProvenance {
    pub target: NativeTarget,
    pub artifact: PathBuf,
    pub sha256: String,
    pub signature: PathBuf,
    pub attestation: PathBuf,
    pub sbom: PathBuf,
    pub license_inventory: PathBuf,
    pub build_metadata: PathBuf,
    pub source_revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReproducibilityEvidence {
    pub compared_builds: u32,
    pub byte_identical: bool,
    #[serde(default)]
    pub documented_differences: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupplyChainEvidence {
    pub schema_version: u32,
    pub source_revision: String,
    pub cargo_lock_sha256: String,
    pub notice: PathBuf,
    pub artifacts: Vec<ArtifactProvenance>,
    pub reproducibility: ReproducibilityEvidence,
    #[serde(default)]
    pub unknown_licenses: Vec<String>,
    #[serde(default)]
    pub yanked_crates: Vec<String>,
    #[serde(default)]
    pub critical_advisories: Vec<String>,
    pub signing_credentials_healthy: bool,
}

impl SupplyChainEvidence {
    #[must_use]
    pub fn validation_failures(&self, root: &Path, source_revision: &str) -> Vec<String> {
        let mut failures = Vec::new();
        if self.schema_version != RELEASE_EVIDENCE_SCHEMA_VERSION {
            failures.push("supply chain evidence schema is unsupported".to_owned());
        }
        if self.source_revision != source_revision || !valid_revision(&self.source_revision) {
            failures.push("supply chain source revision differs from candidate".to_owned());
        }
        match sha256_file(&root.join("Cargo.lock")) {
            Ok(actual) if actual == self.cargo_lock_sha256 => {}
            _ => failures.push("Cargo.lock digest is missing or stale".to_owned()),
        }
        require_file(root, &self.notice, "NOTICE", &mut failures);
        let targets = self
            .artifacts
            .iter()
            .map(|artifact| artifact.target)
            .collect::<BTreeSet<_>>();
        if targets != NativeTarget::ALL.into_iter().collect() || self.artifacts.len() != 5 {
            failures.push("supply chain manifest must contain exactly five targets".to_owned());
        }
        for artifact in &self.artifacts {
            if artifact.source_revision != source_revision {
                failures.push(format!(
                    "{}: artifact source revision differs from candidate",
                    artifact.target
                ));
            }
            match sha256_file(&root.join(&artifact.artifact)) {
                Ok(actual) if actual == artifact.sha256 => {}
                _ => failures.push(format!(
                    "{}: artifact digest is missing or stale",
                    artifact.target
                )),
            }
            for (path, label) in [
                (&artifact.signature, "signature"),
                (&artifact.attestation, "attestation"),
                (&artifact.sbom, "SBOM"),
                (&artifact.license_inventory, "license inventory"),
                (&artifact.build_metadata, "build metadata"),
            ] {
                require_file(root, path, label, &mut failures);
            }
        }
        if self.reproducibility.compared_builds < 2 {
            failures.push("reproducibility evidence compares fewer than two builds".to_owned());
        }
        if !self.reproducibility.byte_identical
            && self.reproducibility.documented_differences.is_empty()
        {
            failures.push("non-identical builds lack documented byte differences".to_owned());
        }
        if !self.unknown_licenses.is_empty()
            || !self.yanked_crates.is_empty()
            || !self.critical_advisories.is_empty()
        {
            failures.push("dependency policy has unresolved release blockers".to_owned());
        }
        if !self.signing_credentials_healthy {
            failures.push("signing credential health is not proven".to_owned());
        }
        failures
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseMetrics {
    pub startup_p95_ms: BTreeMap<NativeTarget, u64>,
    pub tui_ready_p95_ms: BTreeMap<NativeTarget, u64>,
    pub streaming_chunks: u64,
    pub streaming_p95_ms: u64,
    pub streaming_p99_ms: u64,
    pub idle_rss_mib: u64,
    pub history_rss_mib: u64,
    pub cancellation: CleanupEvidence,
    pub handoff_schedules: u64,
    pub handoff_failures: u64,
    pub persistence_crash_points: u64,
    pub persistence_failures: u64,
    pub secret_cases: u64,
    pub secret_disclosures: u64,
    pub deterministic_repetitions: u64,
    pub distinct_report_digests: u64,
}

impl ReleaseMetrics {
    #[must_use]
    pub fn validation_failures(&self) -> Vec<String> {
        let mut failures = Vec::new();
        validate_target_metric(&self.startup_p95_ms, 100, "cold --help p95", &mut failures);
        validate_target_metric(
            &self.tui_ready_p95_ms,
            300,
            "cold TUI-ready p95",
            &mut failures,
        );
        if self.streaming_chunks < 100_000
            || self.streaming_p95_ms > 20
            || self.streaming_p99_ms > 50
        {
            failures.push("streaming latency NFR is not proven".to_owned());
        }
        if self.idle_rss_mib > 80 || self.history_rss_mib > 300 {
            failures.push("memory NFR is not met".to_owned());
        }
        self.cancellation
            .validate(NativeTarget::LinuxX86_64, &mut failures);
        if self.handoff_schedules < 10_000 || self.handoff_failures != 0 {
            failures.push("handoff reliability NFR is not proven".to_owned());
        }
        if self.persistence_crash_points < 10_000 || self.persistence_failures != 0 {
            failures.push("persistence reliability NFR is not proven".to_owned());
        }
        if self.secret_cases < 10_000 || self.secret_disclosures != 0 {
            failures.push("secret-safety NFR is not proven".to_owned());
        }
        if self.deterministic_repetitions < 100 || self.distinct_report_digests != 1 {
            failures.push("deterministic report NFR is not proven".to_owned());
        }
        failures
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseCandidate {
    pub schema_version: u32,
    pub source_revision: String,
    pub compatibility_report: PathBuf,
    pub native_evidence_directory: PathBuf,
    pub supply_chain_evidence: PathBuf,
    pub security_audit: PathBuf,
    pub metrics: PathBuf,
    pub required_documentation: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecurityAuditEvidence {
    pub schema_version: u32,
    pub source_revision: String,
    pub axes: BTreeMap<String, bool>,
    pub seeded_cases: u64,
    pub secret_disclosures: u64,
    #[serde(default)]
    pub open_high_impact_findings: Vec<String>,
}

impl SecurityAuditEvidence {
    #[must_use]
    pub fn validation_failures(&self, source_revision: &str) -> Vec<String> {
        let mut failures = Vec::new();
        if self.schema_version != RELEASE_EVIDENCE_SCHEMA_VERSION
            || self.source_revision != source_revision
        {
            failures.push("security audit is not bound to the release candidate".to_owned());
        }
        for axis in [
            "extension",
            "oauth",
            "path",
            "protocol",
            "secret",
            "subprocess",
            "supply_chain",
        ] {
            if self.axes.get(axis) != Some(&true) {
                failures.push(format!("security audit axis `{axis}` is not passing"));
            }
        }
        if self.seeded_cases < 10_000 || self.secret_disclosures != 0 {
            failures.push("security audit secret-safety corpus is incomplete".to_owned());
        }
        if !self.open_high_impact_findings.is_empty() {
            failures.push("security audit has unresolved high-impact findings".to_owned());
        }
        failures
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseCertification {
    pub schema_version: u32,
    pub source_revision: String,
    pub release: u32,
    pub ready: bool,
    pub failures: Vec<String>,
}

pub fn certify_release(
    root: &Path,
    candidate: &ReleaseCandidate,
) -> Result<ReleaseCertification, ReleaseError> {
    let mut failures = Vec::new();
    if candidate.schema_version != RELEASE_EVIDENCE_SCHEMA_VERSION {
        failures.push("release candidate schema is unsupported".to_owned());
    }
    if !valid_revision(&candidate.source_revision) {
        failures.push("release candidate source revision is invalid".to_owned());
    }
    match read_root_json::<CompatibilityReport>(root, &candidate.compatibility_report) {
        Ok(compatibility)
            if compatibility.release == RELEASE_NUMBER
                && report_is_release_ready(&compatibility)
                && compatibility.source_revision == candidate.source_revision
                && !compatibility.dirty_source => {}
        Ok(_) => failures.push("release 5 compatibility report is not release-ready".to_owned()),
        Err(_) => failures.push("release 5 compatibility report is unavailable".to_owned()),
    }
    if validate_root_relative(&candidate.native_evidence_directory).is_err() {
        failures.push("native certification evidence path is unsafe".to_owned());
    } else {
        match validate_native_evidence_set(
            root,
            &root.join(&candidate.native_evidence_directory),
            &candidate.source_revision,
        ) {
            Ok(_) => {}
            Err(ReleaseError::Certification(native_failures)) => failures.extend(native_failures),
            Err(_) => failures.push("native certification evidence is unavailable".to_owned()),
        }
    }
    match read_root_json::<SupplyChainEvidence>(root, &candidate.supply_chain_evidence) {
        Ok(supply_chain) => {
            failures.extend(supply_chain.validation_failures(root, &candidate.source_revision));
        }
        Err(_) => failures.push("supply chain evidence is unavailable".to_owned()),
    }
    match read_root_json::<SecurityAuditEvidence>(root, &candidate.security_audit) {
        Ok(audit) => failures.extend(audit.validation_failures(&candidate.source_revision)),
        Err(_) => failures.push("security audit evidence is unavailable".to_owned()),
    }
    match read_root_json::<ReleaseMetrics>(root, &candidate.metrics) {
        Ok(metrics) => failures.extend(metrics.validation_failures()),
        Err(_) => failures.push("release metric evidence is unavailable".to_owned()),
    }
    for path in &candidate.required_documentation {
        require_file(root, path, "release documentation", &mut failures);
    }
    failures.sort();
    failures.dedup();
    Ok(ReleaseCertification {
        schema_version: RELEASE_EVIDENCE_SCHEMA_VERSION,
        source_revision: candidate.source_revision.clone(),
        release: RELEASE_NUMBER,
        ready: failures.is_empty(),
        failures,
    })
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<(), ReleaseError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut encoded = serde_json::to_vec_pretty(value)?;
    encoded.push(b'\n');
    fs::write(path, encoded)?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum ReleaseError {
    #[error("release evidence I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("release evidence JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unknown native target `{0}`")]
    UnknownTarget(String),
    #[error("cannot capture {target} evidence on host {actual}")]
    NonNativeCapture {
        target: NativeTarget,
        actual: String,
    },
    #[error("release evidence path must be root-relative")]
    UnsafePath,
    #[error("cleanup benchmark arguments are invalid")]
    InvalidBenchmark,
    #[error("cleanup benchmark process failed")]
    CleanupBenchmark,
    #[error("native host runtime could not be identified")]
    RuntimeEvidence,
    #[error("release certification failed: {}", .0.join("; "))]
    Certification(Vec<String>),
}

fn validate_target_metric(
    values: &BTreeMap<NativeTarget, u64>,
    maximum: u64,
    label: &str,
    failures: &mut Vec<String>,
) {
    for target in NativeTarget::ALL {
        match values.get(&target) {
            Some(value) if *value <= maximum => {}
            Some(_) => failures.push(format!("{target}: {label} exceeds {maximum} ms")),
            None => failures.push(format!("{target}: {label} is missing")),
        }
    }
}

fn detect_host_runtime(target: NativeTarget) -> Result<String, ReleaseError> {
    match target {
        NativeTarget::LinuxX86_64 | NativeTarget::LinuxAarch64 => {
            let output = Command::new("ldd")
                .arg("--version")
                .output()
                .map_err(|_| ReleaseError::RuntimeEvidence)?;
            let text = String::from_utf8_lossy(if output.stdout.is_empty() {
                &output.stderr
            } else {
                &output.stdout
            });
            let version = text
                .lines()
                .next()
                .and_then(|line| {
                    line.split_whitespace().rev().find(|value| {
                        value
                            .bytes()
                            .next()
                            .is_some_and(|byte| byte.is_ascii_digit())
                    })
                })
                .ok_or(ReleaseError::RuntimeEvidence)?;
            Ok(format!("glibc-{version}"))
        }
        NativeTarget::MacosX86_64 | NativeTarget::MacosAarch64 => {
            let output = Command::new("sw_vers")
                .arg("-productVersion")
                .output()
                .map_err(|_| ReleaseError::RuntimeEvidence)?;
            if !output.status.success() {
                return Err(ReleaseError::RuntimeEvidence);
            }
            let major = String::from_utf8_lossy(&output.stdout)
                .trim()
                .split('.')
                .next()
                .filter(|value| !value.is_empty())
                .ok_or(ReleaseError::RuntimeEvidence)?
                .to_owned();
            Ok(format!("macos-{major}"))
        }
        NativeTarget::WindowsX86_64 => {
            let output = Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    "(Get-CimInstance Win32_OperatingSystem).Caption",
                ])
                .output()
                .map_err(|_| ReleaseError::RuntimeEvidence)?;
            if !output.status.success() {
                return Err(ReleaseError::RuntimeEvidence);
            }
            let caption = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
            for (needle, label) in [
                ("windows server 2022", "windows-server-2022"),
                ("windows 11", "windows-11"),
                ("windows 10", "windows-10"),
            ] {
                if caption.contains(needle) {
                    return Ok(label.to_owned());
                }
            }
            Err(ReleaseError::RuntimeEvidence)
        }
    }
}

async fn cleanup_trial() -> Result<std::time::Duration, ReleaseError> {
    let directory = std::env::current_dir().map_err(|_| ReleaseError::CleanupBenchmark)?;
    let manager = TerminalManager::with_cleanup_grace(std::time::Duration::from_secs(2));
    #[cfg(windows)]
    let mut spec = {
        let mut spec = ProcessSpec::new("cmd.exe", &directory);
        spec.arguments = vec![
            "/D".to_owned(),
            "/S".to_owned(),
            "/C".to_owned(),
            "ping -n 31 127.0.0.1 >NUL".to_owned(),
        ];
        spec
    };
    #[cfg(not(windows))]
    let mut spec = {
        let mut spec = ProcessSpec::new("/bin/sh", &directory);
        spec.arguments = vec!["-c".to_owned(), "sleep 30".to_owned()];
        spec
    };
    spec.max_output_bytes = 1024;
    manager
        .run(spec)
        .await
        .map_err(|_| ReleaseError::CleanupBenchmark)?;
    let started = std::time::Instant::now();
    let outcomes = manager
        .cleanup_all()
        .await
        .map_err(|_| ReleaseError::CleanupBenchmark)?;
    if outcomes.len() != 1 || !manager.list().await.is_empty() {
        return Err(ReleaseError::CleanupBenchmark);
    }
    Ok(started.elapsed())
}

fn require_file(root: &Path, path: &Path, label: &str, failures: &mut Vec<String>) {
    if validate_root_relative(path).is_err() || !root.join(path).is_file() {
        failures.push(format!("{label} is missing: {}", path.display()));
    }
}

fn read_root_json<T: for<'de> Deserialize<'de>>(
    root: &Path,
    path: &Path,
) -> Result<T, ReleaseError> {
    validate_root_relative(path)?;
    read_json(&root.join(path))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, ReleaseError> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn sha256_file(path: &Path) -> Result<String, ReleaseError> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn validate_root_relative(path: &Path) -> Result<(), ReleaseError> {
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ReleaseError::UnsafePath);
    }
    Ok(())
}

fn valid_revision(value: &str) -> bool {
    value.len() == 40
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        && value.bytes().any(|byte| byte != b'0')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cleanup() -> CleanupEvidence {
        CleanupEvidence {
            trials: 10_000,
            terminated_within_500_ms: 9_900,
            terminated_within_5_s: 10_000,
            orphaned_after_5_s: 0,
        }
    }

    fn suites(target: NativeTarget) -> BTreeMap<String, bool> {
        target
            .required_suites()
            .iter()
            .map(|suite| ((*suite).to_owned(), true))
            .collect()
    }

    #[test]
    fn non_native_and_emulated_evidence_fail_closed() {
        let target = if std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64" {
            NativeTarget::WindowsX86_64
        } else {
            NativeTarget::LinuxX86_64
        };
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::write(directory.path().join("artifact"), b"binary").expect("artifact");
        let error = NativeEvidence::capture(
            directory.path(),
            NativeCaptureInput {
                target,
                source_revision: "a".repeat(40),
                artifact: PathBuf::from("artifact"),
                minimum_runtime: "supported".to_owned(),
                suites: suites(target),
                cleanup: cleanup(),
                signing: SigningEvidence {
                    checksum_verified: true,
                    signed: true,
                    notarized: true,
                    clean_host_download: true,
                },
            },
        )
        .expect_err("wrong host");
        assert!(matches!(error, ReleaseError::NonNativeCapture { .. }));
    }

    #[test]
    fn suite_cleanup_signing_and_notarization_are_mandatory() {
        let target = NativeTarget::MacosAarch64;
        let evidence = NativeEvidence {
            schema_version: 1,
            target,
            host_os: "macos".to_owned(),
            host_arch: "aarch64".to_owned(),
            executed_natively: false,
            source_revision: "a".repeat(40),
            artifact: PathBuf::from("missing"),
            artifact_sha256: "00".repeat(32),
            host_runtime: String::new(),
            minimum_runtime: String::new(),
            suites: BTreeMap::new(),
            cleanup: CleanupEvidence {
                trials: 1,
                terminated_within_500_ms: 0,
                terminated_within_5_s: 0,
                orphaned_after_5_s: 1,
            },
            signing: SigningEvidence {
                checksum_verified: false,
                signed: false,
                notarized: false,
                clean_host_download: false,
            },
        };
        let failures = evidence.validation_failures(Path::new("."));
        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("native host"))
        );
        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("notarization"))
        );
        assert!(failures.iter().any(|failure| failure.contains("10000")));
        assert!(failures.iter().any(|failure| failure.contains("suite")));
    }

    #[test]
    fn release_metrics_enforce_every_nfr_threshold() {
        let metrics = ReleaseMetrics {
            startup_p95_ms: NativeTarget::ALL
                .into_iter()
                .map(|target| (target, 101))
                .collect(),
            tui_ready_p95_ms: BTreeMap::new(),
            streaming_chunks: 99_999,
            streaming_p95_ms: 21,
            streaming_p99_ms: 51,
            idle_rss_mib: 81,
            history_rss_mib: 301,
            cancellation: CleanupEvidence {
                trials: 9_999,
                terminated_within_500_ms: 0,
                terminated_within_5_s: 0,
                orphaned_after_5_s: 1,
            },
            handoff_schedules: 9_999,
            handoff_failures: 1,
            persistence_crash_points: 9_999,
            persistence_failures: 1,
            secret_cases: 9_999,
            secret_disclosures: 1,
            deterministic_repetitions: 99,
            distinct_report_digests: 2,
        };
        let failures = metrics.validation_failures();
        assert!(failures.len() >= 15, "{failures:?}");
    }

    #[test]
    fn supply_chain_blocks_missing_artifacts_and_policy_findings() {
        let evidence = SupplyChainEvidence {
            schema_version: 1,
            source_revision: "a".repeat(40),
            cargo_lock_sha256: "00".repeat(32),
            notice: PathBuf::from("NOTICE"),
            artifacts: Vec::new(),
            reproducibility: ReproducibilityEvidence {
                compared_builds: 1,
                byte_identical: false,
                documented_differences: Vec::new(),
            },
            unknown_licenses: vec!["unknown".to_owned()],
            yanked_crates: Vec::new(),
            critical_advisories: vec!["critical".to_owned()],
            signing_credentials_healthy: false,
        };
        let failures = evidence.validation_failures(Path::new("."), &"a".repeat(40));
        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("five targets"))
        );
        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("dependency policy"))
        );
        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("signing credential"))
        );
    }

    #[test]
    fn missing_candidate_evidence_produces_a_blocked_report() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let candidate = ReleaseCandidate {
            schema_version: 1,
            source_revision: "a".repeat(40),
            compatibility_report: PathBuf::from("missing-compatibility.json"),
            native_evidence_directory: PathBuf::from("missing-native"),
            supply_chain_evidence: PathBuf::from("missing-supply-chain.json"),
            security_audit: PathBuf::from("missing-security-audit.json"),
            metrics: PathBuf::from("missing-metrics.json"),
            required_documentation: vec![PathBuf::from("missing-docs.md")],
        };
        let report = certify_release(directory.path(), &candidate).expect("blocked report");
        assert!(!report.ready);
        assert!(
            report
                .failures
                .iter()
                .any(|failure| failure.contains("compatibility report is unavailable"))
        );
        assert!(
            report
                .failures
                .iter()
                .any(|failure| failure.contains("supply chain evidence is unavailable"))
        );
        assert!(
            report
                .failures
                .iter()
                .any(|failure| failure.contains("metric evidence is unavailable"))
        );
        assert!(
            report
                .failures
                .iter()
                .any(|failure| failure.contains("security audit evidence is unavailable"))
        );
    }
}
