//! The provenance assessment's edges: the corpus replay covers the full
//! five-source matrix, so these tests hold the acceptance criteria that name
//! behavior rather than a scenario, chiefly that an unavailable keyring never
//! fails the assessment.

use std::collections::BTreeMap;
use std::fs;

use super::keyring::{KEYRING_SERVICE, KeyringStore};
use super::state::{AuthStateKind, DEFAULT_MISTRAL_API_ENV_KEY, assess_auth_state};
use super::testing::{ScriptedError, scripted};

fn environ(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

#[test]
fn an_empty_key_variable_needs_no_authentication() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let store = KeyringStore::new(Box::new(scripted(&[], None, None, &[])));
    let state = assess_auth_state(
        "",
        &temporary.path().join(".env"),
        &environ(&[]),
        false,
        &store,
    )
    .expect("the assessment completes");
    assert_eq!(state.kind, AuthStateKind::AuthNotRequired);
    assert!(state.can_use_active_provider);
    assert!(!state.sign_out_available);
    assert_eq!(state.env_key, None);
}

#[test]
fn an_unavailable_keyring_completes_the_assessment_on_the_remaining_sources() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let env_path = temporary.path().join(".env");
    fs::write(&env_path, "MISTRAL_API_KEY=dotenv-value\n").expect("dotenv fixture");
    let store = KeyringStore::new(Box::new(scripted(
        &[],
        None,
        Some(ScriptedError::NoBackend),
        &[],
    )));
    let state = assess_auth_state(
        DEFAULT_MISTRAL_API_ENV_KEY,
        &env_path,
        &environ(&[]),
        false,
        &store,
    )
    .expect("a missing backend reads as an absent source");
    assert_eq!(state.kind, AuthStateKind::VibeHomeEnvFile);
    assert!(state.sign_out_available);
}

#[test]
fn a_dotenv_entry_wins_over_the_keyring_because_it_loads_first() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let env_path = temporary.path().join(".env");
    fs::write(&env_path, "MISTRAL_API_KEY=dotenv-value\n").expect("dotenv fixture");
    let store = KeyringStore::new(Box::new(scripted(
        &[(KEYRING_SERVICE, "keyring-value")],
        None,
        None,
        &[],
    )));
    let state = assess_auth_state(
        DEFAULT_MISTRAL_API_ENV_KEY,
        &env_path,
        &environ(&[("MISTRAL_API_KEY", "injected")]),
        false,
        &store,
    )
    .expect("the assessment completes");
    assert_eq!(state.kind, AuthStateKind::VibeHomeEnvFile);
}

#[test]
fn a_credential_predating_the_dotenv_load_belongs_to_the_process() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let env_path = temporary.path().join(".env");
    fs::write(&env_path, "MISTRAL_API_KEY=dotenv-value\n").expect("dotenv fixture");
    let store = KeyringStore::new(Box::new(scripted(&[], None, None, &[])));
    let state = assess_auth_state(
        DEFAULT_MISTRAL_API_ENV_KEY,
        &env_path,
        &environ(&[("MISTRAL_API_KEY", "exported")]),
        true,
        &store,
    )
    .expect("the assessment completes");
    assert_eq!(state.kind, AuthStateKind::ProcessEnv);
    assert!(!state.sign_out_available);
}

#[test]
fn a_non_default_key_variable_is_an_unsupported_provider_without_sign_out() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let store = KeyringStore::new(Box::new(scripted(
        &[(KEYRING_SERVICE, "keyring-value")],
        None,
        None,
        &[],
    )));
    let state = assess_auth_state(
        "OTHER_PROVIDER_KEY",
        &temporary.path().join(".env"),
        &environ(&[]),
        false,
        &store,
    )
    .expect("the assessment completes");
    assert_eq!(state.kind, AuthStateKind::UnsupportedProvider);
    assert!(state.can_use_active_provider);
    assert!(!state.sign_out_available);
    assert_eq!(state.env_key.as_deref(), Some("OTHER_PROVIDER_KEY"));
}

#[test]
fn empty_strings_read_as_absent_in_every_source() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let env_path = temporary.path().join(".env");
    fs::write(&env_path, "MISTRAL_API_KEY=\n").expect("dotenv fixture");
    let store = KeyringStore::new(Box::new(scripted(
        &[(KEYRING_SERVICE, "")],
        None,
        None,
        &[],
    )));
    let state = assess_auth_state(
        DEFAULT_MISTRAL_API_ENV_KEY,
        &env_path,
        &environ(&[("MISTRAL_API_KEY", "")]),
        false,
        &store,
    )
    .expect("the assessment completes");
    assert_eq!(state.kind, AuthStateKind::SignedOut);
    assert!(!state.can_use_active_provider);
    assert_eq!(state.env_key.as_deref(), Some(DEFAULT_MISTRAL_API_ENV_KEY));
}

#[test]
fn resolution_prefers_the_environment_and_falls_through_an_empty_value() {
    use super::state::resolve_api_key;

    let store = KeyringStore::new(Box::new(scripted(
        &[(KEYRING_SERVICE, "keyring-value")],
        None,
        None,
        &[],
    )));
    assert_eq!(
        resolve_api_key(
            DEFAULT_MISTRAL_API_ENV_KEY,
            &environ(&[("MISTRAL_API_KEY", "env-value")]),
            &store,
        )
        .as_deref(),
        Some("env-value")
    );
    assert_eq!(
        resolve_api_key(
            DEFAULT_MISTRAL_API_ENV_KEY,
            &environ(&[("MISTRAL_API_KEY", "")]),
            &store,
        )
        .as_deref(),
        Some("keyring-value"),
        "an empty environment value falls through to the keyring"
    );
    assert_eq!(resolve_api_key("", &environ(&[]), &store), None);
}

#[cfg(unix)]
#[test]
fn an_unreadable_dotenv_propagates_its_error_as_the_reference_does() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempfile::tempdir().expect("temporary root");
    let env_path = temporary.path().join(".env");
    fs::write(&env_path, "MISTRAL_API_KEY=dotenv-value\n").expect("dotenv fixture");
    fs::set_permissions(&env_path, fs::Permissions::from_mode(0o000)).expect("chmod");
    let store = KeyringStore::new(Box::new(scripted(&[], None, None, &[])));
    let error = assess_auth_state(
        DEFAULT_MISTRAL_API_ENV_KEY,
        &env_path,
        &environ(&[]),
        false,
        &store,
    )
    .expect_err("an unreadable file is an error, not an absent source");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    fs::set_permissions(&env_path, fs::Permissions::from_mode(0o600)).expect("chmod back");
}
