//! Persistence edges the corpus states but cannot show directly: the
//! owner-only mode of the fallback file, the unusable variable name, and the
//! removal that clears the rest before surfacing a backend failure.

use std::collections::BTreeMap;
use std::fs;

use super::keyring::{KEYRING_SERVICE, KeyringStore};
use super::persistence::{PersistOutcome, RemoveError, persist_api_key, remove_api_key};
use super::testing::{ScriptedError, scripted};

#[test]
fn a_keyring_refusal_saves_the_key_to_an_owner_only_dotenv() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let env_file = temporary.path().join(".env");
    let store = KeyringStore::new(Box::new(scripted(
        &[],
        Some(ScriptedError::NoBackend),
        None,
        &[],
    )));
    let mut process_env = BTreeMap::new();
    let report = persist_api_key(
        "MISTRAL_API_KEY",
        true,
        "typed-key",
        false,
        &mut process_env,
        &env_file,
        &store,
    );
    assert_eq!(report.outcome, PersistOutcome::Completed);
    assert_eq!(
        process_env.get("MISTRAL_API_KEY").map(String::as_str),
        Some("typed-key")
    );
    let contents = fs::read_to_string(&env_file).expect("the fallback file exists");
    assert!(contents.contains("typed-key"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&env_file)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn an_unusable_variable_name_answers_the_env_var_error_and_writes_nothing() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let env_file = temporary.path().join(".env");
    let store = KeyringStore::new(Box::new(scripted(&[], None, None, &[])));
    let mut process_env = BTreeMap::new();
    let report = persist_api_key(
        "BAD=NAME",
        true,
        "typed-key",
        false,
        &mut process_env,
        &env_file,
        &store,
    );
    assert_eq!(
        report.outcome.as_reference_string(),
        "env_var_error:BAD=NAME"
    );
    assert!(report.telemetry.is_empty());
    assert!(process_env.is_empty());
    assert!(!env_file.exists());
}

#[test]
fn a_removal_with_a_real_backend_error_clears_the_rest_then_surfaces_it() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let env_file = temporary.path().join(".env");
    fs::write(&env_file, "MISTRAL_API_KEY='dotenv-copy'\n").expect("fixture");
    let backend = scripted(
        &[("vibe", "legacy-value")],
        None,
        None,
        &[(KEYRING_SERVICE, ScriptedError::Backend)],
    );
    let store = KeyringStore::new(Box::new(backend.clone()));
    let mut process_env =
        BTreeMap::from([("MISTRAL_API_KEY".to_owned(), "process-copy".to_owned())]);
    let error = remove_api_key("MISTRAL_API_KEY", &mut process_env, &env_file, &store)
        .expect_err("the backend failure surfaces");
    assert!(matches!(error, RemoveError::Keyring(_)));
    assert!(process_env.is_empty());
    assert_eq!(
        crate::config::DotenvValues::load(&env_file).file_variable("MISTRAL_API_KEY"),
        None,
        "the dotenv copy is cleared before the failure surfaces"
    );
    assert!(!backend.stored().contains_key("vibe"));
}
