//! The credential store's own contract: which entries a removal deletes, and
//! how a snapshot puts them back. The login and refresh flows are measured
//! against the reference by `crates/vibe-app-server/tests/mcp_oauth_parity_tests.rs`.

use std::sync::Arc;

use super::{McpOAuthError, McpOAuthStore, mcp_oauth_username};
use crate::auth::keyring::{KEYRING_SERVICE, KeyringBackend, KeyringFailure, MemoryKeyringBackend};

const KINDS: [&str; 3] = ["tokens", "client_info", "fingerprint"];

fn seeded(alias: &str) -> (Arc<MemoryKeyringBackend>, McpOAuthStore) {
    let backend = Arc::new(MemoryKeyringBackend::new());
    for kind in KINDS {
        backend
            .set(KEYRING_SERVICE, &mcp_oauth_username(alias, kind), kind)
            .expect("the memory store accepts every write");
    }
    let store = McpOAuthStore::new(backend.clone(), false);
    (backend, store)
}

fn held(backend: &MemoryKeyringBackend, alias: &str) -> Vec<&'static str> {
    let entries = backend.entries();
    KINDS
        .into_iter()
        .filter(|kind| {
            entries.contains_key(&(KEYRING_SERVICE.to_owned(), mcp_oauth_username(alias, kind)))
        })
        .collect()
}

/// A store that refuses every call as the named failure.
struct Refusing(KeyringFailure);

impl KeyringBackend for Refusing {
    fn get(&self, _service: &str, _account: &str) -> Result<Option<String>, KeyringFailure> {
        match &self.0 {
            KeyringFailure::NoBackend => Err(KeyringFailure::NoBackend),
            _ => Ok(Some("held".to_owned())),
        }
    }

    fn set(&self, _service: &str, _account: &str, _secret: &str) -> Result<(), KeyringFailure> {
        Err(self.0.clone())
    }

    fn delete(&self, _service: &str, _account: &str) -> Result<(), KeyringFailure> {
        Err(self.0.clone())
    }
}

#[test]
fn a_removal_deletes_every_entry_of_the_alias_and_nothing_else() {
    let (backend, store) = seeded("docs");
    backend
        .set(
            KEYRING_SERVICE,
            &mcp_oauth_username("other", "tokens"),
            "kept",
        )
        .expect("seed");

    store
        .delete_credentials("docs")
        .expect("the removal succeeds");

    assert!(held(&backend, "docs").is_empty());
    assert_eq!(
        store.get(&mcp_oauth_username("other", "tokens")),
        Ok(Some("kept".to_owned()))
    );
}

#[test]
fn a_snapshot_puts_back_what_a_removal_deleted() {
    let (backend, store) = seeded("docs");
    let backup = store.snapshot("docs");
    store
        .delete_credentials("docs")
        .expect("the removal succeeds");

    store
        .restore("docs", &backup)
        .expect("the restore succeeds");

    assert_eq!(held(&backend, "docs"), KINDS);
    assert_eq!(
        store.get(&mcp_oauth_username("docs", "client_info")),
        Ok(Some("client_info".to_owned()))
    );
}

/// Reference `restore_oauth_credentials`: an entry that was absent when the
/// snapshot was taken is absent again afterward.
#[test]
fn a_restore_deletes_an_entry_the_snapshot_did_not_hold() {
    let backend = Arc::new(MemoryKeyringBackend::new());
    let store = McpOAuthStore::new(backend.clone(), false);
    let backup = store.snapshot("docs");
    store
        .set(&mcp_oauth_username("docs", "tokens"), "written later")
        .expect("seed");

    store
        .restore("docs", &backup)
        .expect("the restore succeeds");

    assert!(held(&backend, "docs").is_empty());
}

/// Reference `delete_oauth_credentials` on a host with no credential store:
/// there is nothing to delete, and nothing to put back.
#[test]
fn a_host_without_a_store_neither_deletes_nor_restores() {
    let store = McpOAuthStore::new(Arc::new(Refusing(KeyringFailure::NoBackend)), false);

    assert_eq!(store.delete_credentials("docs"), Ok(()));
    let backup = store.snapshot("docs");
    assert_eq!(store.restore("docs", &backup), Ok(()));
}

#[test]
fn a_store_that_refuses_the_deletion_reports_the_cleanup_failure() {
    let store = McpOAuthStore::new(
        Arc::new(Refusing(KeyringFailure::Backend("locked".to_owned()))),
        false,
    );

    assert!(matches!(
        store.delete_credentials("docs"),
        Err(McpOAuthError::CleanupFailed { alias, .. }) if alias == "docs"
    ));
}

/// Reference `VIBE_TEST_DISABLE_KEYRING`: every read is empty and a deletion
/// reaches no store.
#[test]
fn a_disabled_store_reads_nothing() {
    let (backend, _) = seeded("docs");
    let store = McpOAuthStore::new(backend.clone(), true);

    assert_eq!(store.get(&mcp_oauth_username("docs", "tokens")), Ok(None));
    store.delete_credentials("docs").expect("nothing to delete");
    assert_eq!(held(&backend, "docs"), KINDS);
}
