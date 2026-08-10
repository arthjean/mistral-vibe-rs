//! The six-state credential provenance assessment.
//!
//! Reference `vibe/setup/auth/auth_state.py`: for a provider's key variable,
//! the assessment consults the process environment, the OS keyring and the
//! global dotenv, and answers where the active credential comes from. The
//! precedence mirrors how the reference loads credentials at startup: the
//! dotenv is injected into the process environment before the keyring is read,
//! so a dotenv entry is the active credential even when the keyring also holds
//! one, and a value that was in the environment before the dotenv load belongs
//! to the process however many other sources hold copies.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use crate::config::DotenvValues;

use super::keyring::KeyringStore;

/// The key variable of the shipped Mistral provider, the only provider whose
/// credential this product owns and may therefore sign out.
pub const DEFAULT_MISTRAL_API_ENV_KEY: &str = "MISTRAL_API_KEY";

/// Where the active credential comes from. Reference `AuthStateKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStateKind {
    SignedOut,
    AuthNotRequired,
    OsKeyring,
    VibeHomeEnvFile,
    ProcessEnv,
    UnsupportedProvider,
}

impl AuthStateKind {
    /// The reference's serialization of each state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SignedOut => "signed_out",
            Self::AuthNotRequired => "auth_not_required",
            Self::OsKeyring => "os_keyring",
            Self::VibeHomeEnvFile => "vibe_home_env_file",
            Self::ProcessEnv => "process_env",
            Self::UnsupportedProvider => "unsupported_provider",
        }
    }
}

/// The assessment: the provenance, whether the active provider is usable, and
/// whether this product owns the credential enough to revoke it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthState {
    pub kind: AuthStateKind,
    pub can_use_active_provider: bool,
    pub sign_out_available: bool,
    pub env_key: Option<String>,
}

fn state(
    kind: AuthStateKind,
    can_use_active_provider: bool,
    sign_out_available: bool,
    env_key: Option<&str>,
) -> AuthState {
    AuthState {
        kind,
        can_use_active_provider,
        sign_out_available,
        env_key: env_key.map(str::to_owned),
    }
}

/// Whether the dotenv at `env_path` declares a non-empty value for `env_key`.
///
/// Reference `_dotenv_has_value`: a path that is neither a regular file nor a
/// FIFO reads as absent, while a file that exists and cannot be read
/// propagates its error, which is what the corpus records as the raised
/// `PermissionError`.
fn dotenv_has_value(env_path: &Path, env_key: &str) -> io::Result<bool> {
    let Ok(metadata) = fs::metadata(env_path) else {
        return Ok(false);
    };
    if !metadata.is_file() && !is_fifo(&metadata) {
        return Ok(false);
    }
    let contents = fs::read_to_string(env_path)?;
    Ok(DotenvValues::parse(&contents)
        .file_variable(env_key)
        .is_some_and(|value| !value.is_empty()))
}

#[cfg(unix)]
fn is_fifo(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;
    metadata.file_type().is_fifo()
}

#[cfg(not(unix))]
fn is_fifo(_metadata: &fs::Metadata) -> bool {
    false
}

/// The credential `env_key` resolves to: the caller's environment first, then
/// the keyring under the shared service names.
///
/// Reference `resolve_api_key` in `vibe/utils/api_keys.py`, where an empty
/// environment value falls through to the keyring rather than answering.
#[must_use]
pub fn resolve_api_key(
    env_key: &str,
    environ: &BTreeMap<String, String>,
    store: &KeyringStore,
) -> Option<String> {
    if env_key.is_empty() {
        return None;
    }
    environ
        .get(env_key)
        .filter(|value| !value.is_empty())
        .cloned()
        .or_else(|| store.get_api_key(env_key))
}

/// Classifies where the credential for `env_key` comes from.
///
/// Reference `assess_auth_state`. `environ` is the caller's view of the
/// process environment after the dotenv load, and
/// `process_env_had_value_before_dotenv_load` records what the variable held
/// before that load, which is the one fact the environment can no longer
/// answer afterward.
///
/// # Errors
///
/// Propagates the dotenv read failure when the file exists and cannot be
/// read; every other source failure reads as an absent value.
pub fn assess_auth_state(
    env_key: &str,
    env_path: &Path,
    environ: &BTreeMap<String, String>,
    process_env_had_value_before_dotenv_load: bool,
    store: &KeyringStore,
) -> io::Result<AuthState> {
    if env_key.is_empty() {
        return Ok(state(AuthStateKind::AuthNotRequired, true, false, None));
    }
    let keyring_has_value = store
        .get_api_key(env_key)
        .is_some_and(|value| !value.is_empty());
    let current_process_has_value = environ.get(env_key).is_some_and(|value| !value.is_empty());
    let dotenv_has_value = dotenv_has_value(env_path, env_key)?;

    if !current_process_has_value && !keyring_has_value && !dotenv_has_value {
        return Ok(state(AuthStateKind::SignedOut, false, false, Some(env_key)));
    }
    if env_key != DEFAULT_MISTRAL_API_ENV_KEY {
        return Ok(state(
            AuthStateKind::UnsupportedProvider,
            true,
            false,
            Some(env_key),
        ));
    }
    let (kind, sign_out_available) = if process_env_had_value_before_dotenv_load {
        (AuthStateKind::ProcessEnv, false)
    } else if dotenv_has_value {
        (AuthStateKind::VibeHomeEnvFile, true)
    } else if keyring_has_value {
        (AuthStateKind::OsKeyring, true)
    } else {
        (AuthStateKind::ProcessEnv, false)
    };
    Ok(state(kind, true, sign_out_available, Some(env_key)))
}
