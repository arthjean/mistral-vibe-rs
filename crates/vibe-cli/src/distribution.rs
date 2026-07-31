use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateState {
    Current,
    Available {
        version: String,
        artifact_url: String,
        sha256: String,
    },
    Offline,
    Unsupported,
    PartialUpgrade,
}

impl UpdateState {
    #[must_use]
    pub const fn recovery_guidance(&self) -> &'static str {
        match self {
            Self::Current => "The installed version is current.",
            Self::Available { .. } => "Download and verify the selected release artifact.",
            Self::Offline => {
                "The update service is unavailable; the installed binary is unchanged."
            }
            Self::Unsupported => {
                "This platform has no compatible artifact; keep the installed binary."
            }
            Self::PartialUpgrade => {
                "Restore the adjacent .previous binary, then retry the complete upgrade."
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateArtifact {
    pub target: String,
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateManifest {
    pub schema_version: u32,
    pub version: String,
    pub artifacts: Vec<UpdateArtifact>,
}

pub fn select_update(
    current_version: &str,
    target: &str,
    manifest: Result<&UpdateManifest, UpdateLookupError>,
) -> Result<UpdateState, DistributionError> {
    let manifest = match manifest {
        Ok(manifest) => manifest,
        Err(UpdateLookupError::Offline) => return Ok(UpdateState::Offline),
        Err(UpdateLookupError::PartialUpgrade) => return Ok(UpdateState::PartialUpgrade),
    };
    if manifest.schema_version != 1 {
        return Err(DistributionError::ManifestSchema);
    }
    validate_release_label(&manifest.version)?;
    if manifest.version == current_version {
        return Ok(UpdateState::Current);
    }
    let Some(artifact) = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.target == target)
    else {
        return Ok(UpdateState::Unsupported);
    };
    validate_sha256(&artifact.sha256)?;
    let artifact_url =
        Url::parse(&artifact.url).map_err(|_| DistributionError::InsecureArtifactUrl)?;
    if artifact_url.scheme() != "https"
        || artifact_url.host_str().is_none()
        || artifact_url.username() != ""
        || artifact_url.password().is_some()
    {
        return Err(DistributionError::InsecureArtifactUrl);
    }
    Ok(UpdateState::Available {
        version: manifest.version.clone(),
        artifact_url: artifact.url.clone(),
        sha256: artifact.sha256.clone(),
    })
}

pub fn install_staged(
    installed_binary: &Path,
    staged_binary: &Path,
    expected_sha256: &str,
) -> Result<(), DistributionError> {
    validate_sha256(expected_sha256)?;
    let actual = sha256_file(staged_binary)?;
    if actual != expected_sha256 {
        return Err(DistributionError::ChecksumMismatch);
    }
    let backup = backup_path(installed_binary)?;
    if backup.exists() {
        return Err(DistributionError::PartialUpgrade);
    }
    fs::rename(installed_binary, &backup).map_err(DistributionError::PrepareUpgrade)?;
    if let Err(error) = fs::rename(staged_binary, installed_binary) {
        let recovery = fs::rename(&backup, installed_binary);
        return match recovery {
            Ok(()) => Err(DistributionError::ActivateUpgrade(error)),
            Err(_) => Err(DistributionError::RollbackRequired),
        };
    }
    if let Err(error) = fs::remove_file(&backup) {
        return Err(DistributionError::FinalizeUpgrade(error));
    }
    Ok(())
}

pub fn rollback(installed_binary: &Path) -> Result<(), DistributionError> {
    let backup = backup_path(installed_binary)?;
    if !backup.is_file() {
        return Err(DistributionError::NoRollback);
    }
    if installed_binary.exists() {
        fs::remove_file(installed_binary).map_err(DistributionError::Rollback)?;
    }
    fs::rename(backup, installed_binary).map_err(DistributionError::Rollback)
}

pub fn sha256_file(path: &Path) -> Result<String, DistributionError> {
    let mut file = fs::File::open(path).map_err(DistributionError::ArtifactIo)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(DistributionError::ArtifactIo)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateLookupError {
    Offline,
    PartialUpgrade,
}

#[derive(Debug, Error)]
pub enum DistributionError {
    #[error("update manifest schema is unsupported")]
    ManifestSchema,
    #[error("update release label is invalid")]
    InvalidRelease,
    #[error("update artifact URL must use HTTPS")]
    InsecureArtifactUrl,
    #[error("artifact SHA-256 is invalid")]
    InvalidChecksum,
    #[error("artifact checksum did not match; the installed binary is unchanged")]
    ChecksumMismatch,
    #[error("artifact could not be read: {0}")]
    ArtifactIo(std::io::Error),
    #[error("upgrade could not preserve the installed binary: {0}")]
    PrepareUpgrade(std::io::Error),
    #[error("upgrade activation failed and the installed binary was restored: {0}")]
    ActivateUpgrade(std::io::Error),
    #[error("upgrade activated but its backup could not be removed: {0}")]
    FinalizeUpgrade(std::io::Error),
    #[error("a previous upgrade backup exists and must be resolved first")]
    PartialUpgrade,
    #[error("upgrade failed and automatic rollback also failed; restore .previous manually")]
    RollbackRequired,
    #[error("no rollback binary is available")]
    NoRollback,
    #[error("rollback failed: {0}")]
    Rollback(std::io::Error),
}

fn validate_release_label(value: &str) -> Result<(), DistributionError> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        return Err(DistributionError::InvalidRelease);
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), DistributionError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DistributionError::InvalidChecksum);
    }
    Ok(())
}

fn backup_path(installed_binary: &Path) -> Result<PathBuf, DistributionError> {
    let filename = installed_binary
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(DistributionError::InvalidRelease)?;
    Ok(installed_binary.with_file_name(format!("{filename}.previous")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(target: &str) -> UpdateManifest {
        UpdateManifest {
            schema_version: 1,
            version: "2.23.2".to_owned(),
            artifacts: vec![UpdateArtifact {
                target: target.to_owned(),
                url: "https://releases.example/vibe.tar.gz".to_owned(),
                sha256: "ab".repeat(32),
            }],
        }
    }

    #[test]
    fn updater_states_are_explicit_and_non_destructive() {
        let release_manifest = manifest("linux-x86_64");
        assert_eq!(
            select_update("2.23.2", "linux-x86_64", Ok(&release_manifest)).expect("state"),
            UpdateState::Current
        );
        assert!(matches!(
            select_update("2.23.1", "linux-x86_64", Ok(&release_manifest)).expect("state"),
            UpdateState::Available { .. }
        ));
        assert_eq!(
            select_update("2.23.1", "windows-x86_64", Ok(&release_manifest)).expect("state"),
            UpdateState::Unsupported
        );
        assert_eq!(
            select_update("2.23.1", "linux-x86_64", Err(UpdateLookupError::Offline))
                .expect("state"),
            UpdateState::Offline
        );
        assert_eq!(
            select_update(
                "2.23.1",
                "linux-x86_64",
                Err(UpdateLookupError::PartialUpgrade)
            )
            .expect("state"),
            UpdateState::PartialUpgrade
        );
        let mut insecure = manifest("linux-x86_64");
        insecure.artifacts[0].url = "https://user:secret@releases.example/vibe".to_owned();
        assert!(matches!(
            select_update("2.23.1", "linux-x86_64", Ok(&insecure)),
            Err(DistributionError::InsecureArtifactUrl)
        ));
    }

    #[test]
    fn checksum_failure_never_replaces_a_working_binary() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let installed = directory.path().join("vibe");
        let staged = directory.path().join("vibe.new");
        fs::write(&installed, b"working").expect("installed");
        fs::write(&staged, b"tampered").expect("staged");
        let error =
            install_staged(&installed, &staged, &"00".repeat(32)).expect_err("checksum mismatch");
        assert!(matches!(error, DistributionError::ChecksumMismatch));
        assert_eq!(fs::read(&installed).expect("installed"), b"working");
        assert_eq!(fs::read(&staged).expect("staged"), b"tampered");
    }

    #[test]
    fn verified_upgrade_is_atomic_and_removes_rollback_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let installed = directory.path().join("vibe");
        let staged = directory.path().join("vibe.new");
        fs::write(&installed, b"old").expect("installed");
        fs::write(&staged, b"new").expect("staged");
        let digest = sha256_file(&staged).expect("digest");
        install_staged(&installed, &staged, &digest).expect("upgrade");
        assert_eq!(fs::read(&installed).expect("installed"), b"new");
        assert!(!directory.path().join("vibe.previous").exists());
    }
}
