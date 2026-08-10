//! The OS credential store, behind the service names both implementations read.
//!
//! Reference `vibe/utils/keyring.py`: entries live under service
//! `ai.mistral.vibe` with the provider's key variable as the account, a read
//! falls back to the legacy service `vibe` and migrates what it finds, and a
//! process-wide cache serves repeated lookups. This port adds one more legacy
//! name to the read path: `mistral-vibe-rs`, the service every previously
//! released build of this port wrote under, so an upgrade never asks the
//! operator to sign in again.

use std::collections::BTreeMap;
use std::sync::Mutex;

use thiserror::Error;

/// The service both implementations write under.
pub const KEYRING_SERVICE: &str = "ai.mistral.vibe";
/// The legacy service the reference migrates from on read.
pub const LEGACY_KEYRING_SERVICES: &[&str] = &["vibe"];
/// The service prior builds of this port wrote under. Read and migrated like a
/// legacy service, never written; the reference does not know this name.
pub const PRIOR_BUILD_KEYRING_SERVICE: &str = "mistral-vibe-rs";
/// Reference `_DISABLE_KEYRING_ENV_VAR`: a test hook that makes every read and
/// write behave as if no credential store existed.
pub const DISABLE_KEYRING_ENV_VAR: &str = "VIBE_TEST_DISABLE_KEYRING";

/// Why a keyring primitive could not answer.
///
/// The distinction between an absent backend and an absent entry is
/// load-bearing: removal treats both as a no-op while persistence falls back
/// to the dotenv on either, but a diagnostic must never report one as the
/// other.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum KeyringFailure {
    /// The store works but holds nothing for this service and account.
    /// Reference `PasswordDeleteError`.
    #[error("no credential entry exists under this service and account")]
    NoEntry,
    /// No credential store is reachable at all. `keyring` 4.1.5 surfaces
    /// `NoDefaultStore` here on a Linux host without a Secret Service, measured
    /// against the pinned crate rather than assumed. Reference
    /// `NoKeyringError`.
    #[error("no OS credential store is available on this host")]
    NoBackend,
    /// The store exists and the operation failed inside it. Reference
    /// `KeyringError`.
    #[error("the OS credential store failed: {0}")]
    Backend(String),
}

/// The three primitives a credential store answers, per service.
///
/// Reference `_get_password`, `_set_password` and `_delete_password`. `get`
/// answers `Ok(None)` for an absent entry and reserves the error for a store
/// that could not be consulted; `delete` reports an absent entry as
/// [`KeyringFailure::NoEntry`] because removal needs to distinguish it.
pub trait KeyringBackend: Send + Sync {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, KeyringFailure>;
    fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), KeyringFailure>;
    fn delete(&self, service: &str, account: &str) -> Result<(), KeyringFailure>;
}

/// The `keyring` crate as a [`KeyringBackend`].
#[derive(Default)]
pub struct NativeKeyringBackend {
    /// The platform stores are not all safe under concurrent mutation from one
    /// process, so the primitives serialize.
    serialization: Mutex<()>,
}

impl NativeKeyringBackend {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn entry(service: &str, account: &str) -> Result<keyring::Entry, KeyringFailure> {
    keyring::Entry::new(service, account).map_err(map_native_error)
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub(super) fn map_native_error(error: keyring::Error) -> KeyringFailure {
    match error {
        keyring::Error::NoEntry => KeyringFailure::NoEntry,
        keyring::Error::NoDefaultStore | keyring::Error::NoStorageAccess(_) => {
            KeyringFailure::NoBackend
        }
        other => KeyringFailure::Backend(other.to_string()),
    }
}

impl KeyringBackend for NativeKeyringBackend {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, KeyringFailure> {
        let _guard = self
            .serialization
            .lock()
            .map_err(|_| KeyringFailure::Backend("keyring serialization poisoned".to_owned()))?;
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            match entry(service, account)?.get_password() {
                Ok(secret) => Ok(Some(secret)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(error) => Err(map_native_error(error)),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            let _ = (service, account);
            Err(KeyringFailure::NoBackend)
        }
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), KeyringFailure> {
        let _guard = self
            .serialization
            .lock()
            .map_err(|_| KeyringFailure::Backend("keyring serialization poisoned".to_owned()))?;
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            entry(service, account)?
                .set_password(secret)
                .map_err(map_native_error)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            let _ = (service, account, secret);
            Err(KeyringFailure::NoBackend)
        }
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), KeyringFailure> {
        let _guard = self
            .serialization
            .lock()
            .map_err(|_| KeyringFailure::Backend("keyring serialization poisoned".to_owned()))?;
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            entry(service, account)?
                .delete_credential()
                .map_err(map_native_error)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            let _ = (service, account);
            Err(KeyringFailure::NoBackend)
        }
    }
}

/// The credential store the product consults: current service first, then the
/// reference legacy name, then the prior-build name, migrating on read and
/// caching per account.
pub struct KeyringStore {
    backend: Box<dyn KeyringBackend>,
    disabled: bool,
    cache: Mutex<BTreeMap<String, Option<String>>>,
}

impl KeyringStore {
    #[must_use]
    pub fn new(backend: Box<dyn KeyringBackend>) -> Self {
        Self {
            backend,
            disabled: false,
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    /// The production store: the OS backend, disabled when the reference's
    /// test hook asks for it.
    #[must_use]
    pub fn native() -> Self {
        let mut store = Self::new(Box::new(NativeKeyringBackend::new()));
        store.disabled = std::env::var(DISABLE_KEYRING_ENV_VAR).as_deref() == Ok("1");
        store
    }

    /// A store that answers as if no backend existed, the reference's
    /// `VIBE_TEST_DISABLE_KEYRING` behavior.
    #[must_use]
    pub fn disabled(backend: Box<dyn KeyringBackend>) -> Self {
        let mut store = Self::new(backend);
        store.disabled = true;
        store
    }

    fn read_services() -> impl Iterator<Item = &'static str> {
        std::iter::once(KEYRING_SERVICE)
            .chain(LEGACY_KEYRING_SERVICES.iter().copied())
            .chain(std::iter::once(PRIOR_BUILD_KEYRING_SERVICE))
    }

    fn cache_insert(&self, account: &str, value: Option<String>) -> Option<String> {
        if account.is_empty() {
            return value;
        }
        if let Ok(mut cache) = self.cache.lock() {
            cache
                .entry(account.to_owned())
                .or_insert(value.clone())
                .clone()
        } else {
            value
        }
    }

    fn cache_forget(&self, account: &str) {
        if account.is_empty() {
            return;
        }
        if let Ok(mut cache) = self.cache.lock() {
            cache.remove(account);
        }
    }

    /// Reads the credential for `account`, consulting the current service
    /// first and migrating what a legacy service answers.
    ///
    /// Reference `get_api_key_from_keyring`: an empty account and a disabled
    /// store consult nothing, a backend error reads as absent without touching
    /// the remaining services, and the answer is cached either way it is
    /// found. A migration whose rewrite fails leaves the legacy entry where it
    /// was and still returns the key; a rewrite that succeeds deletes the
    /// legacy source, tolerating a failed delete.
    #[must_use]
    pub fn get_api_key(&self, account: &str) -> Option<String> {
        if account.is_empty() || self.disabled {
            return None;
        }
        if let Ok(cache) = self.cache.lock()
            && let Some(cached) = cache.get(account)
        {
            return cached.clone();
        }
        for service in Self::read_services() {
            match self.backend.get(service, account) {
                Err(_) => return None,
                Ok(None) => continue,
                Ok(Some(secret)) => {
                    if service != KEYRING_SERVICE {
                        self.migrate_legacy_entry(account, &secret, service);
                    }
                    return self.cache_insert(account, Some(secret));
                }
            }
        }
        self.cache_insert(account, None)
    }

    fn migrate_legacy_entry(&self, account: &str, secret: &str, source_service: &str) {
        if self.backend.set(KEYRING_SERVICE, account, secret).is_err() {
            return;
        }
        let _ = self.backend.delete(source_service, account);
    }

    /// Writes the credential under the current service and clears every legacy
    /// copy.
    ///
    /// Reference `set_api_key_in_keyring`: a failed write drops the stale
    /// cache entry and surfaces the failure, a successful one deletes the
    /// legacy services best-effort and caches the new value.
    pub fn set_api_key(&self, account: &str, secret: &str) -> Result<(), KeyringFailure> {
        if self.disabled {
            self.cache_forget(account);
            return Err(KeyringFailure::Backend(
                "the keyring is disabled for this run".to_owned(),
            ));
        }
        if let Err(error) = self.backend.set(KEYRING_SERVICE, account, secret) {
            self.cache_forget(account);
            return Err(error);
        }
        for service in LEGACY_KEYRING_SERVICES
            .iter()
            .copied()
            .chain(std::iter::once(PRIOR_BUILD_KEYRING_SERVICE))
        {
            let _ = self.backend.delete(service, account);
        }
        self.cache_forget(account);
        self.cache_insert(account, Some(secret.to_owned()));
        Ok(())
    }

    /// Deletes the credential from every service this store reads.
    ///
    /// Reference `delete_api_key_from_keyring`: every service is attempted
    /// whatever the earlier ones answered, a real backend failure wins over a
    /// missing entry, and [`KeyringFailure::NoEntry`] is reported only when no
    /// service deleted anything. The cache is dropped on every path.
    pub fn delete_api_key(&self, account: &str) -> Result<(), KeyringFailure> {
        if self.disabled {
            self.cache_forget(account);
            return Ok(());
        }
        let mut missing = false;
        let mut operation_error = None;
        let mut deleted = false;
        for service in Self::read_services() {
            match self.backend.delete(service, account) {
                Ok(()) => deleted = true,
                Err(KeyringFailure::NoEntry) => missing = true,
                Err(error) => {
                    if operation_error.is_none() {
                        operation_error = Some(error);
                    }
                }
            }
        }
        self.cache_forget(account);
        if let Some(error) = operation_error {
            return Err(error);
        }
        if !deleted && missing {
            return Err(KeyringFailure::NoEntry);
        }
        Ok(())
    }
}
