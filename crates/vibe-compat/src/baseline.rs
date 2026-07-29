use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::BASELINE_TOML;
use crate::model::Baseline;

#[derive(Debug, Error)]
pub enum BaselineError {
    #[error("baseline manifest is invalid: {0}")]
    Manifest(#[from] toml::de::Error),
    #[error("baseline I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("baseline command `{command}` failed: {stderr}")]
    Command { command: String, stderr: String },
    #[error("baseline mismatch for {field}: expected {expected}, got {actual}")]
    Mismatch {
        field: &'static str,
        expected: String,
        actual: String,
    },
    #[error("baseline checkout exists but is not a valid clean checkout: {0}")]
    InvalidCheckout(PathBuf),
}

pub fn load() -> Result<Baseline, BaselineError> {
    Ok(toml::from_str(BASELINE_TOML)?)
}

pub fn checkout_path(root: &Path, baseline: &Baseline) -> PathBuf {
    root.join(&baseline.checkout)
}

pub fn provision(
    root: &Path,
    source: &Path,
    sync_environment: bool,
) -> Result<PathBuf, BaselineError> {
    let baseline = load()?;
    let checkout = checkout_path(root, &baseline);
    if checkout.exists() {
        validate_checkout(&checkout, &baseline)?;
    } else {
        if let Some(parent) = checkout.parent() {
            fs::create_dir_all(parent)?;
        }
        run(Command::new("git")
            .args(["clone", "--no-hardlinks", "--no-checkout"])
            .arg(source)
            .arg(&checkout))?;
        if let Err(error) = run(Command::new("git").arg("-C").arg(&checkout).args([
            "checkout",
            "--detach",
            &baseline.commit,
        ])) {
            let _cleanup = fs::remove_dir_all(&checkout);
            return Err(error);
        }
        validate_checkout(&checkout, &baseline)?;
    }
    if sync_environment {
        run(Command::new("uv")
            .arg("sync")
            .args(["--frozen", "--python", &baseline.python_version])
            .current_dir(&checkout))?;
    }
    Ok(checkout)
}

pub fn validate_checkout(checkout: &Path, baseline: &Baseline) -> Result<(), BaselineError> {
    if !checkout.join(".git").exists() {
        return Err(BaselineError::InvalidCheckout(checkout.to_owned()));
    }
    let commit = text(run(Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["rev-parse", "HEAD"]))?);
    require_equal("commit", &baseline.commit, &commit)?;
    let tree = text(run(Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["rev-parse", "HEAD^{tree}"]))?);
    require_equal("tree", &baseline.tree, &tree)?;
    let dirty = text(run(Command::new("git").arg("-C").arg(checkout).args([
        "status",
        "--porcelain",
        "--untracked-files=no",
    ]))?);
    if !dirty.is_empty() {
        return Err(BaselineError::InvalidCheckout(checkout.to_owned()));
    }
    let archive =
        run(Command::new("git")
            .arg("-C")
            .arg(checkout)
            .args(["archive", "--format=tar", "HEAD"]))?;
    let digest = hex_digest(Sha256::digest(archive.stdout));
    require_equal("archive_sha256", &baseline.archive_sha256, &digest)?;
    let lockfile = fs::read(checkout.join("uv.lock"))?;
    let lock_digest = hex_digest(Sha256::digest(lockfile));
    require_equal("lockfile_sha256", &baseline.lockfile_sha256, &lock_digest)?;
    Ok(())
}

fn run(command: &mut Command) -> Result<Output, BaselineError> {
    let rendered = format!("{command:?}");
    let output = command.output()?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(BaselineError::Command {
            command: rendered,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn text(output: Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn require_equal(field: &'static str, expected: &str, actual: &str) -> Result<(), BaselineError> {
    if expected == actual {
        Ok(())
    } else {
        Err(BaselineError::Mismatch {
            field,
            expected: expected.to_owned(),
            actual: actual.to_owned(),
        })
    }
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(bytes)
}
