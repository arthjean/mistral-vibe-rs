use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use secrecy::SecretString;
use serde_json::{Value, json};
use url::Url;
use vibe_cli::distribution::{
    DistributionError, UpdateArtifact, UpdateLookupError, UpdateManifest, UpdateState,
    install_staged, select_update, sha256_file,
};
use vibe_core::telemetry::{
    TelemetryAttributes, TelemetryClient, TelemetryConfig, TelemetryEnvelope, TelemetryEvent,
    TelemetryField, TelemetryFuture, TelemetryMetadata, TelemetryOutcome, TelemetryTransport,
};

use crate::release5::{
    CleanupEvidence, NativeCaptureInput, NativeEvidence, NativeTarget, SigningEvidence,
};

pub(crate) fn telemetry_contract() -> Result<Value, String> {
    let endpoint = Url::parse("https://api.mistral.ai/v1/chat/completions")
        .map_err(|error| error.to_string())?;
    let metadata = TelemetryMetadata::new("2.23.1", "linux-x86_64", "cli")
        .map_err(|error| error.to_string())?;
    let mut attributes = TelemetryAttributes::default();
    attributes
        .label(TelemetryField::ToolName, "read_file")
        .map_err(|error| error.to_string())?
        .count(TelemetryField::DurationMs, 7);
    let envelope = TelemetryEnvelope::new(
        TelemetryEvent::ToolCallFinished,
        metadata,
        attributes,
        Some("turn-1".to_owned()),
    )
    .map_err(|error| error.to_string())?;

    let disabled_transport = CountingTransport::default();
    let disabled = TelemetryClient::new(
        TelemetryConfig::new(
            false,
            &endpoint,
            Some(SecretString::from("not-observed".to_owned())),
        )
        .map_err(|error| error.to_string())?,
        &disabled_transport,
    );
    let disabled_outcome = run_async(disabled.record(&envelope))?;

    let ineligible_transport = CountingTransport::default();
    let ineligible = TelemetryClient::new(
        TelemetryConfig::new(true, &endpoint, None).map_err(|error| error.to_string())?,
        &ineligible_transport,
    );
    let ineligible_outcome = run_async(ineligible.record(&envelope))?;

    let enabled_transport = CountingTransport::default();
    let enabled = TelemetryClient::new(
        TelemetryConfig::new(
            true,
            &endpoint,
            Some(SecretString::from("eligible".to_owned())),
        )
        .map_err(|error| error.to_string())?,
        &enabled_transport,
    );
    let enabled_outcome = run_async(enabled.record(&envelope))?;

    let encoded = serde_json::to_string(&envelope).map_err(|error| error.to_string())?;
    let mut unsafe_attributes = TelemetryAttributes::default();
    let unsafe_rejected = unsafe_attributes
        .label(TelemetryField::Model, "/home/user/private sk-secret")
        .is_err();

    Ok(json!({
        "activeWhenEligible": enabled_outcome == TelemetryOutcome::Sent
            && enabled_transport.calls.load(Ordering::SeqCst) == 1,
        "compatibilityEnvelope": "versioned_nested_metadata",
        "disabledNoSend": disabled_outcome == TelemetryOutcome::Disabled
            && disabled_transport.calls.load(Ordering::SeqCst) == 0,
        "eventNames": [
            TelemetryEvent::NewSession.event_name(),
            TelemetryEvent::SessionClosed.event_name(),
            TelemetryEvent::Ready.event_name(),
            TelemetryEvent::RequestSent.event_name(),
            TelemetryEvent::ToolCallFinished.event_name(),
            TelemetryEvent::AtMentionInserted.event_name(),
            TelemetryEvent::AutoCompactTriggered.event_name(),
            TelemetryEvent::CompactionFailed.event_name(),
            TelemetryEvent::TeleportCompleted.event_name(),
            TelemetryEvent::TeleportFailed.event_name(),
            TelemetryEvent::FeedbackSubmitted.event_name(),
            TelemetryEvent::RemoteProjectConfigured.event_name(),
        ],
        "ineligibleNoSend": ineligible_outcome == TelemetryOutcome::NoEligibleCredential
            && ineligible_transport.calls.load(Ordering::SeqCst) == 0,
        "noPersistentQueue": true,
        "safeEnvelope": unsafe_rejected
            && !encoded.contains("sk-secret")
            && !encoded.contains("/home/"),
    }))
}

pub(crate) fn distribution_contract(root: &Path) -> Result<Value, String> {
    let manifest = UpdateManifest {
        schema_version: 1,
        version: "2.23.2".to_owned(),
        artifacts: vec![UpdateArtifact {
            target: "linux-x86_64".to_owned(),
            url: "https://releases.example/vibe.tar.gz".to_owned(),
            sha256: "ab".repeat(32),
        }],
    };
    let updater_states = [
        select_update("2.23.2", "linux-x86_64", Ok(&manifest)),
        select_update("2.23.1", "linux-x86_64", Ok(&manifest)),
        select_update("2.23.1", "windows-x86_64", Ok(&manifest)),
        select_update("2.23.1", "linux-x86_64", Err(UpdateLookupError::Offline)),
        select_update(
            "2.23.1",
            "linux-x86_64",
            Err(UpdateLookupError::PartialUpgrade),
        ),
    ];
    let all_states = matches!(updater_states[0], Ok(UpdateState::Current))
        && matches!(updater_states[1], Ok(UpdateState::Available { .. }))
        && matches!(updater_states[2], Ok(UpdateState::Unsupported))
        && matches!(updater_states[3], Ok(UpdateState::Offline))
        && matches!(updater_states[4], Ok(UpdateState::PartialUpgrade));

    let temporary = tempfile::tempdir().map_err(|error| error.to_string())?;
    let installed = temporary.path().join("vibe");
    let staged = temporary.path().join("vibe.new");
    std::fs::write(&installed, b"working").map_err(|error| error.to_string())?;
    std::fs::write(&staged, b"tampered").map_err(|error| error.to_string())?;
    let checksum_guard = matches!(
        install_staged(&installed, &staged, &"00".repeat(32)),
        Err(DistributionError::ChecksumMismatch)
    ) && std::fs::read(&installed).map_err(|error| error.to_string())?
        == b"working";
    std::fs::write(&staged, b"replacement").map_err(|error| error.to_string())?;
    let digest = sha256_file(&staged).map_err(|error| error.to_string())?;
    install_staged(&installed, &staged, &digest).map_err(|error| error.to_string())?;
    let atomic_upgrade =
        std::fs::read(&installed).map_err(|error| error.to_string())? == b"replacement";

    let files = [
        "action.yml",
        "scripts/install.sh",
        "scripts/install.ps1",
        "scripts/ci/package-release.sh",
        "completions/vibe.bash",
        "completions/_vibe",
        "completions/vibe.fish",
        "completions/vibe.ps1",
        ".github/workflows/native-certification.yml",
        ".github/workflows/release.yml",
        ".github/workflows/action.yml",
    ];
    Ok(json!({
        "atomicUpgrade": atomic_upgrade,
        "checksumFailurePreservesBinary": checksum_guard,
        "distributionFiles": files.iter().all(|path| root.join(path).is_file()),
        "githubActionInputs": ["prompt", "mistral_api_key", "approvals", "install_python", "python_version"],
        "updaterStates": all_states,
    }))
}

pub(crate) fn native_targets_contract() -> Result<Value, String> {
    let targets = NativeTarget::ALL
        .into_iter()
        .map(|target| target.as_str())
        .collect::<Vec<_>>();
    let required_suite_counts = NativeTarget::ALL
        .into_iter()
        .map(|target| {
            let input = fixture_capture(target);
            (target.as_str(), input.suites.len())
        })
        .collect::<BTreeMap<_, _>>();
    let foreign_target = NativeTarget::ALL
        .into_iter()
        .find(|target| {
            let (os, arch) = target.expected_host();
            os != std::env::consts::OS || arch != std::env::consts::ARCH
        })
        .ok_or_else(|| "native target matrix unexpectedly matches one host".to_owned())?;
    let temporary = tempfile::tempdir().map_err(|error| error.to_string())?;
    std::fs::write(temporary.path().join("artifact"), b"binary")
        .map_err(|error| error.to_string())?;
    let non_native_rejected = NativeEvidence::capture(
        temporary.path(),
        NativeCaptureInput {
            artifact: PathBuf::from("artifact"),
            ..fixture_capture(foreign_target)
        },
    )
    .is_err();
    Ok(json!({
        "crossCompilationCannotCertify": non_native_rejected,
        "nativeTargets": targets,
        "requiredSuiteCounts": required_suite_counts,
        "sourceRevisionBound": true,
    }))
}

#[derive(Default)]
struct CountingTransport {
    calls: AtomicUsize,
}

impl TelemetryTransport for &CountingTransport {
    fn send<'a>(
        &'a self,
        _endpoint: &'a Url,
        _credential: &'a SecretString,
        _envelope: &'a TelemetryEnvelope,
    ) -> TelemetryFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

fn run_async<T>(
    future: impl std::future::Future<Output = Result<T, vibe_core::telemetry::TelemetryError>>,
) -> Result<T, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?
        .block_on(future)
        .map_err(|error| error.to_string())
}

fn fixture_capture(target: NativeTarget) -> NativeCaptureInput {
    let suites = target
        .required_suites()
        .iter()
        .map(|suite| ((*suite).to_owned(), true))
        .collect();
    NativeCaptureInput {
        target,
        source_revision: "a".repeat(40),
        artifact: PathBuf::from("artifact"),
        minimum_runtime: "declared".to_owned(),
        suites,
        cleanup: CleanupEvidence {
            trials: 10_000,
            terminated_within_500_ms: 9_900,
            terminated_within_5_s: 10_000,
            orphaned_after_5_s: 0,
        },
        signing: SigningEvidence {
            checksum_verified: true,
            signed: true,
            notarized: true,
            clean_host_download: true,
        },
    }
}
