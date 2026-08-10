//! The store's migration and the native error mapping the corpus cannot
//! reach: the prior-build service is this port's own compatibility surface,
//! and the backend-versus-entry distinction is measured against the crate's
//! error vocabulary rather than assumed.

use super::keyring::{
    KEYRING_SERVICE, KeyringFailure, KeyringStore, PRIOR_BUILD_KEYRING_SERVICE, map_native_error,
};
use super::testing::{ScriptedError, scripted};

#[test]
fn a_prior_build_credential_resolves_and_migrates_to_the_current_service() {
    let backend = scripted(
        &[(PRIOR_BUILD_KEYRING_SERVICE, "prior-build-value")],
        None,
        None,
        &[],
    );
    let store = KeyringStore::new(Box::new(backend.clone()));
    assert_eq!(
        store.get_api_key("MISTRAL_API_KEY").as_deref(),
        Some("prior-build-value")
    );
    let stored = backend.stored();
    assert_eq!(
        stored.get(KEYRING_SERVICE).map(String::as_str),
        Some("prior-build-value")
    );
    assert!(!stored.contains_key(PRIOR_BUILD_KEYRING_SERVICE));
    assert_eq!(
        backend.calls(),
        [
            "get:ai.mistral.vibe",
            "get:vibe",
            "get:mistral-vibe-rs",
            "set:ai.mistral.vibe",
            "delete:mistral-vibe-rs",
        ]
    );
}

#[test]
fn a_prior_build_migration_whose_rewrite_fails_still_returns_the_key() {
    let backend = scripted(
        &[(PRIOR_BUILD_KEYRING_SERVICE, "prior-build-value")],
        Some(ScriptedError::Backend),
        None,
        &[],
    );
    let store = KeyringStore::new(Box::new(backend.clone()));
    assert_eq!(
        store.get_api_key("MISTRAL_API_KEY").as_deref(),
        Some("prior-build-value")
    );
    assert_eq!(
        backend
            .stored()
            .get(PRIOR_BUILD_KEYRING_SERVICE)
            .map(String::as_str),
        Some("prior-build-value")
    );
}

#[test]
fn a_write_clears_the_prior_build_copy_alongside_the_reference_legacy_one() {
    let backend = scripted(
        &[
            ("vibe", "legacy-value"),
            (PRIOR_BUILD_KEYRING_SERVICE, "prior-build-value"),
        ],
        None,
        None,
        &[],
    );
    let store = KeyringStore::new(Box::new(backend.clone()));
    store
        .set_api_key("MISTRAL_API_KEY", "fresh-value")
        .expect("the scripted write succeeds");
    let stored = backend.stored();
    assert_eq!(
        stored.get(KEYRING_SERVICE).map(String::as_str),
        Some("fresh-value")
    );
    assert!(!stored.contains_key("vibe"));
    assert!(!stored.contains_key(PRIOR_BUILD_KEYRING_SERVICE));
}

#[test]
fn the_native_mapping_never_reports_an_absent_backend_as_an_absent_entry() {
    assert_eq!(
        map_native_error(keyring::Error::NoEntry),
        KeyringFailure::NoEntry
    );
    // Measured on keyring 4.1.5: a Linux host with no reachable Secret
    // Service answers `NoDefaultStore` from `Entry::new`, on the first and
    // every later access.
    assert_eq!(
        map_native_error(keyring::Error::NoDefaultStore),
        KeyringFailure::NoBackend
    );
    assert_eq!(
        map_native_error(keyring::Error::NoStorageAccess("locked".into())),
        KeyringFailure::NoBackend
    );
    assert!(matches!(
        map_native_error(keyring::Error::PlatformFailure("dbus failure".into())),
        KeyringFailure::Backend(_)
    ));
}

#[test]
fn a_disabled_store_consults_nothing_and_refuses_writes() {
    let backend = scripted(&[(KEYRING_SERVICE, "stored-value")], None, None, &[]);
    let store = KeyringStore::disabled(Box::new(backend.clone()));
    assert_eq!(store.get_api_key("MISTRAL_API_KEY"), None);
    assert!(store.set_api_key("MISTRAL_API_KEY", "fresh").is_err());
    assert!(store.delete_api_key("MISTRAL_API_KEY").is_ok());
    assert!(backend.calls().is_empty());
}
