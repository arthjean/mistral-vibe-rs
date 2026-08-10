//! Saving and removing the API key with the reference's outcomes.
//!
//! Reference `vibe/setup/auth/api_key_persistence.py`. A save reaches the
//! process environment first, then the keyring, and a keyring that refuses
//! the write degrades to the global dotenv instead of discarding the key the
//! operator just entered. A removal clears every source it can and surfaces a
//! real backend failure only after the rest is clean, so a sign-out never
//! looks successful while the keyring still holds the credential.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use thiserror::Error;

use super::env_file::{remove_env_file_key, write_env_file_key};
use super::keyring::{KeyringFailure, KeyringStore};

/// What a save answered. Reference `persist_api_key` returns these as strings;
/// [`PersistOutcome::as_reference_string`] reproduces that vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistOutcome {
    /// The key is durably persisted, by the keyring or by the dotenv fallback.
    Completed,
    /// The provider's key variable is empty or unusable as an environment
    /// variable name; nothing was written.
    EnvVarError { detail: String },
    /// Both the keyring and the dotenv fallback refused the write.
    SaveError { detail: String },
}

impl PersistOutcome {
    /// The exact string the reference returns for this outcome.
    #[must_use]
    pub fn as_reference_string(&self) -> String {
        match self {
            Self::Completed => "completed".to_owned(),
            Self::EnvVarError { detail } => format!("env_var_error:{detail}"),
            Self::SaveError { detail } => format!("save_error:{detail}"),
        }
    }
}

/// The onboarding event the reference sends when a Mistral key is added. The
/// telemetry envelope is a recorded divergence, so this port keeps the event
/// local, on the same terms as `telemetry/record`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiKeyAddedEvent {
    pub custom_domain: bool,
}

/// A save's outcome and the telemetry it would have sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistReport {
    pub outcome: PersistOutcome,
    pub telemetry: Vec<ApiKeyAddedEvent>,
}

fn report(outcome: PersistOutcome) -> PersistReport {
    PersistReport {
        outcome,
        telemetry: Vec::new(),
    }
}

/// Whether `env_key` can name an environment variable at all. The reference
/// discovers this by `os.environ` raising `ValueError`.
fn usable_env_key(env_key: &str) -> bool {
    !env_key.contains('=') && !env_key.contains('\0')
}

/// Persists `api_key` for `env_key`: the caller's environment map, then the
/// keyring, then the dotenv at `env_file` when the keyring refuses.
///
/// Reference `persist_api_key`. A successful keyring write removes any stale
/// dotenv copy, tolerating a removal that fails; the fallback write reports
/// completion because the key is durably saved either way; and only a
/// fallback that itself fails answers with the save error. The telemetry
/// event is recorded for a Mistral-backend provider on every completed path.
pub fn persist_api_key(
    env_key: &str,
    backend_is_mistral: bool,
    api_key: &str,
    custom_domain: bool,
    process_env: &mut BTreeMap<String, String>,
    env_file: &Path,
    store: &KeyringStore,
) -> PersistReport {
    if env_key.is_empty() {
        return report(PersistOutcome::EnvVarError {
            detail: "<empty>".to_owned(),
        });
    }
    if !usable_env_key(env_key) {
        return report(PersistOutcome::EnvVarError {
            detail: env_key.to_owned(),
        });
    }
    process_env.insert(env_key.to_owned(), api_key.to_owned());
    match store.set_api_key(env_key, api_key) {
        Ok(()) => {
            // The key is safely stored in the keyring; a stale plaintext copy
            // that cannot be removed is not worth failing a completed save.
            let _ = remove_env_file_key(env_file, env_key);
        }
        Err(_) => {
            if let Err(error) = write_env_file_key(env_file, env_key, api_key) {
                return report(PersistOutcome::SaveError {
                    detail: error.to_string(),
                });
            }
        }
    }
    let telemetry = if backend_is_mistral {
        vec![ApiKeyAddedEvent { custom_domain }]
    } else {
        Vec::new()
    };
    PersistReport {
        outcome: PersistOutcome::Completed,
        telemetry,
    }
}

/// Why a removal failed. Reference `remove_api_key` raises `ValueError` for a
/// missing key variable and re-raises the keyring's own error after clearing
/// the other sources.
#[derive(Debug, Error)]
pub enum RemoveError {
    #[error("an API key cannot be removed without an environment variable name")]
    EmptyEnvKey,
    #[error("the OS credential store failed while removing the API key: {0}")]
    Keyring(KeyringFailure),
    #[error("the global dotenv could not be rewritten: {0}")]
    EnvFile(#[from] io::Error),
}

/// Removes the credential for `env_key` from the keyring, the dotenv at
/// `env_file`, and the caller's environment map.
///
/// Reference `remove_api_key`: a missing backend and a missing entry are both
/// no-ops for sign-out, while a deletion the backend attempted and failed
/// still clears the other copies and is surfaced afterward.
///
/// # Errors
///
/// [`RemoveError::EmptyEnvKey`] when the variable name is empty,
/// [`RemoveError::EnvFile`] when the dotenv rewrite fails, and
/// [`RemoveError::Keyring`] for the deferred backend failure.
pub fn remove_api_key(
    env_key: &str,
    process_env: &mut BTreeMap<String, String>,
    env_file: &Path,
    store: &KeyringStore,
) -> Result<(), RemoveError> {
    if env_key.is_empty() {
        return Err(RemoveError::EmptyEnvKey);
    }
    let keyring_error = match store.delete_api_key(env_key) {
        Ok(()) | Err(KeyringFailure::NoEntry | KeyringFailure::NoBackend) => None,
        Err(error) => Some(error),
    };
    remove_env_file_key(env_file, env_key)?;
    process_env.remove(env_key);
    match keyring_error {
        Some(error) => Err(RemoveError::Keyring(error)),
        None => Ok(()),
    }
}
