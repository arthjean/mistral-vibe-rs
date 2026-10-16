//! The Windows credential layout the reference reads and writes.
//!
//! The reference's default Windows backend is `keyring`'s `WinVaultKeyring`
//! (`keyring/backends/Windows.py` in its environment): a secret is a generic
//! credential whose target is the service and whose user name is the account.
//! When another account already holds that target, a write first moves the
//! holder to the compound target `<account>@<service>` and then takes the plain
//! target; a read tries the plain target, then the compound one. `keyring` 4
//! names its target `<account>.<service>` instead, which is where every earlier
//! build of this port stored its secrets, so a read also looks there and moves
//! what it finds into the reference's layout (`docs/parity.md`, "Windows
//! observations").

use super::{KeyringBackend, KeyringFailure};

#[cfg(test)]
mod win_vault_tests;

/// One generic credential: the user name it was stored under and its secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StoredCredential {
    pub(super) user_name: String,
    pub(super) secret: String,
}

/// The credential manager addressed by target name, the only key Windows has.
pub(super) trait CredentialTargets: Send + Sync {
    /// The credential under `target`, or [`None`] when there is none.
    fn read(&self, target: &str) -> Result<Option<StoredCredential>, KeyringFailure>;
    /// Creates or replaces the credential under `target`.
    fn write(&self, target: &str, user_name: &str, secret: &str) -> Result<(), KeyringFailure>;
    /// Deletes the credential under `target`, [`KeyringFailure::NoEntry`] when
    /// there is none.
    fn remove(&self, target: &str) -> Result<(), KeyringFailure>;
}

/// The reference's layout over a set of credential targets.
pub(super) struct WinVaultLayout<T> {
    targets: T,
}

impl<T> WinVaultLayout<T> {
    pub(super) const fn new(targets: T) -> Self {
        Self { targets }
    }
}

fn compound_target(service: &str, account: &str) -> String {
    format!("{account}@{service}")
}

/// Where `keyring` 4, and so every earlier build of this port, stores a secret.
fn prior_build_target(service: &str, account: &str) -> String {
    format!("{account}.{service}")
}

impl<T: CredentialTargets> KeyringBackend for WinVaultLayout<T> {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, KeyringFailure> {
        if let Some(plain) = self.targets.read(service)?
            && plain.user_name == account
        {
            return Ok(Some(plain.secret));
        }
        // Like the reference, the compound target is trusted by its name.
        if let Some(compound) = self.targets.read(&compound_target(service, account))? {
            return Ok(Some(compound.secret));
        }
        let prior = prior_build_target(service, account);
        let Some(stored) = self.targets.read(&prior)? else {
            return Ok(None);
        };
        // The move is best effort: the secret is answered either way, and a
        // copy that could not be rewritten stays where it was.
        if self.set(service, account, &stored.secret).is_ok() {
            let _ = self.targets.remove(&prior);
        }
        Ok(Some(stored.secret))
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), KeyringFailure> {
        // The reference moves whatever holds the plain target, its own account
        // included, before it writes.
        if let Some(holder) = self.targets.read(service)? {
            self.targets.write(
                &compound_target(service, &holder.user_name),
                &holder.user_name,
                &holder.secret,
            )?;
        }
        self.targets.write(service, account, secret)?;
        // A copy an earlier build left behind is stale from here on.
        let _ = self.targets.remove(&prior_build_target(service, account));
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), KeyringFailure> {
        let mut deleted = false;
        for target in [service.to_owned(), compound_target(service, account)] {
            if let Some(stored) = self.targets.read(&target)?
                && stored.user_name == account
            {
                self.targets.remove(&target)?;
                deleted = true;
            }
        }
        match self.targets.remove(&prior_build_target(service, account)) {
            Ok(()) => deleted = true,
            Err(KeyringFailure::NoEntry) => {}
            Err(error) => return Err(error),
        }
        if deleted {
            Ok(())
        } else {
            Err(KeyringFailure::NoEntry)
        }
    }
}

/// The Windows credential manager itself, through the store `keyring` 4
/// installs.
#[cfg(windows)]
pub(super) struct NativeTargets;

#[cfg(windows)]
impl NativeTargets {
    fn entry(target: &str) -> Result<keyring_core::Entry, KeyringFailure> {
        // `keyring` installs the Windows store as the default one the first
        // time it is asked; the target modifier then names the credential
        // outright, so the service and user arguments are unused.
        if keyring::Entry::store_status().is_err() {
            return Err(KeyringFailure::NoBackend);
        }
        let modifiers = std::collections::HashMap::from([("target", target)]);
        keyring_core::Entry::new_with_modifiers("", "", &modifiers).map_err(super::map_native_error)
    }
}

#[cfg(windows)]
impl CredentialTargets for NativeTargets {
    fn read(&self, target: &str) -> Result<Option<StoredCredential>, KeyringFailure> {
        let entry = Self::entry(target)?;
        let secret = match entry.get_password() {
            Ok(secret) => secret,
            Err(keyring::Error::NoEntry) => return Ok(None),
            Err(error) => return Err(super::map_native_error(error)),
        };
        let user_name = entry
            .get_attributes()
            .map_err(super::map_native_error)?
            .remove("username")
            .unwrap_or_default();
        Ok(Some(StoredCredential { user_name, secret }))
    }

    fn write(&self, target: &str, user_name: &str, secret: &str) -> Result<(), KeyringFailure> {
        let entry = Self::entry(target)?;
        entry
            .set_password(secret)
            .map_err(super::map_native_error)?;
        // A new credential is created with an empty user name, and a
        // replaced one keeps its previous one; either way it is set here.
        entry
            .update_attributes(&std::collections::HashMap::from([("username", user_name)]))
            .map_err(super::map_native_error)
    }

    fn remove(&self, target: &str) -> Result<(), KeyringFailure> {
        Self::entry(target)?
            .delete_credential()
            .map_err(super::map_native_error)
    }
}
