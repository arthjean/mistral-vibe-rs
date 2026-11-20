//! The store's migration and the native error mapping the corpus cannot
//! reach: the prior-build service is this port's own compatibility surface,
//! and the backend-versus-entry distinction is measured against the crate's
//! error vocabulary rather than assumed.

use super::keyring::{
    KEYRING_SERVICE, KeyringBackend, KeyringFailure, KeyringStore, NativeKeyringBackend,
    PRIOR_BUILD_KEYRING_SERVICE, StorelessBackend, map_native_error,
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

/// `PYTHON_KEYRING_BACKEND` names a storeless backend of the reference's
/// `keyring` library; any other value, and every value on macOS, where the
/// reference reaches the Keychain through `security`, keeps the native store.
#[test]
fn only_the_storeless_backends_the_reference_honors_replace_the_native_store() {
    let fail = Some("keyring.backends.fail.Keyring");
    let null = Some("keyring.backends.null.Keyring");
    assert_eq!(
        StorelessBackend::named(fail, false),
        Some(StorelessBackend::Fail)
    );
    assert_eq!(
        StorelessBackend::named(null, false),
        Some(StorelessBackend::Null)
    );
    for other in [
        None,
        Some(""),
        Some("keyring.backends.Windows.WinVaultKeyring"),
        Some("keyring.backends.fail"),
        Some("oracle_keyring.FileKeyring"),
    ] {
        assert_eq!(StorelessBackend::named(other, false), None, "{other:?}");
    }
    assert_eq!(StorelessBackend::named(fail, true), None);
    assert_eq!(StorelessBackend::named(null, true), None);
}

/// The fail backend answers every primitive as a host without a credential
/// store does, before the OS store is consulted, so a Windows child under the
/// capture drivers' switch answers what a Linux capture records: no key read,
/// a refused write, and a removal that fails as the reference's
/// `NoKeyringError` does.
#[test]
fn the_fail_backend_answers_as_a_host_without_a_credential_store() {
    let backend = NativeKeyringBackend::with_storeless(Some(StorelessBackend::Fail));
    assert_eq!(
        backend.get(KEYRING_SERVICE, "MISTRAL_API_KEY"),
        Err(KeyringFailure::NoBackend)
    );
    assert_eq!(
        backend.set(KEYRING_SERVICE, "MISTRAL_API_KEY", "secret"),
        Err(KeyringFailure::NoBackend)
    );
    assert_eq!(
        backend.delete(KEYRING_SERVICE, "MISTRAL_API_KEY"),
        Err(KeyringFailure::NoBackend)
    );

    let store = KeyringStore::new(Box::new(backend));
    assert_eq!(store.get_api_key("MISTRAL_API_KEY"), None);
    assert_eq!(
        store.set_api_key("MISTRAL_API_KEY", "secret"),
        Err(KeyringFailure::NoBackend)
    );
    assert_eq!(store.get_api_key("MISTRAL_API_KEY"), None);
    assert_eq!(
        store.delete_api_key("MISTRAL_API_KEY"),
        Err(KeyringFailure::NoBackend)
    );
}

/// The null backend stores nothing and refuses nothing: a write succeeds and
/// is served from the process cache afterward, as the reference caches it.
#[test]
fn the_null_backend_stores_nothing_and_refuses_nothing() {
    let backend = NativeKeyringBackend::with_storeless(Some(StorelessBackend::Null));
    assert_eq!(backend.get(KEYRING_SERVICE, "MISTRAL_API_KEY"), Ok(None));
    assert_eq!(
        backend.set(KEYRING_SERVICE, "MISTRAL_API_KEY", "secret"),
        Ok(())
    );
    assert_eq!(backend.get(KEYRING_SERVICE, "MISTRAL_API_KEY"), Ok(None));
    assert_eq!(backend.delete(KEYRING_SERVICE, "MISTRAL_API_KEY"), Ok(()));

    let store = KeyringStore::new(Box::new(backend));
    assert_eq!(store.get_api_key("OTHER_KEY"), None);
    assert_eq!(store.set_api_key("MISTRAL_API_KEY", "secret"), Ok(()));
    assert_eq!(
        store.get_api_key("MISTRAL_API_KEY").as_deref(),
        Some("secret")
    );
    assert_eq!(store.delete_api_key("MISTRAL_API_KEY"), Ok(()));
}
