//! Credential provenance, storage and lifecycle.
//!
//! Reference `vibe/setup/auth/` and `vibe/utils/keyring.py`. `vibe-cli` and
//! `vibe-acp` are adapters over this module; nothing here draws a screen or
//! speaks a wire protocol.

pub mod keyring;

#[cfg(test)]
mod keyring_tests;
#[cfg(test)]
pub(crate) mod testing;

pub use keyring::{
    KEYRING_SERVICE, KeyringBackend, KeyringFailure, KeyringStore, LEGACY_KEYRING_SERVICES,
    NativeKeyringBackend, PRIOR_BUILD_KEYRING_SERVICE,
};
