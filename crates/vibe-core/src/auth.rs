//! Credential provenance, storage and lifecycle.
//!
//! Reference `vibe/setup/auth/` and `vibe/utils/keyring.py`: the six-state
//! provenance assessment and the OS credential store with its service-name
//! migration. `vibe-cli` and `vibe-acp` are adapters over this module; nothing
//! here draws a screen or speaks a wire protocol.

pub mod keyring;
pub mod state;

#[cfg(test)]
mod keyring_tests;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
pub(crate) mod testing;

pub use keyring::{
    KEYRING_SERVICE, KeyringBackend, KeyringFailure, KeyringStore, LEGACY_KEYRING_SERVICES,
    NativeKeyringBackend, PRIOR_BUILD_KEYRING_SERVICE,
};
pub use state::{
    AuthState, AuthStateKind, DEFAULT_MISTRAL_API_ENV_KEY, assess_auth_state, resolve_api_key,
};
