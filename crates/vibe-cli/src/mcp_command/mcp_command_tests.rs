//! `vibe mcp remove`, driven through [`run`] rather than through its parts.
//!
//! Every case here starts at the argument vector the intercept hands over, so
//! what the tests observe is what the binary does: the exit code, the two
//! streams, the user configuration file on disk and the credential store.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use tempfile::TempDir;
use vibe_core::auth::{KeyringBackend, KeyringFailure};

use super::{McpEnvironment, run};

/// A credential store that records what it was asked to delete, and can be
/// told to refuse.
#[derive(Default)]
struct RecordingKeyring {
    deletions: Mutex<Vec<(String, String)>>,
    refusal: Option<KeyringFailure>,
}

impl RecordingKeyring {
    fn refusing(failure: KeyringFailure) -> Self {
        Self {
            refusal: Some(failure),
            ..Self::default()
        }
    }
}

/// The handle the tests hold, which is also the backend the command is given.
#[derive(Clone, Default)]
struct SharedKeyring(std::sync::Arc<RecordingKeyring>);

impl SharedKeyring {
    fn refusing(failure: KeyringFailure) -> Self {
        Self(std::sync::Arc::new(RecordingKeyring::refusing(failure)))
    }

    fn deletions(&self) -> Vec<(String, String)> {
        self.0
            .deletions
            .lock()
            .map(|deletions| deletions.clone())
            .unwrap_or_default()
    }
}

impl KeyringBackend for SharedKeyring {
    fn get(&self, _service: &str, _account: &str) -> Result<Option<String>, KeyringFailure> {
        Ok(None)
    }

    fn set(&self, _service: &str, _account: &str, _secret: &str) -> Result<(), KeyringFailure> {
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), KeyringFailure> {
        if let Ok(mut deletions) = self.0.deletions.lock() {
            deletions.push((service.to_owned(), account.to_owned()));
        }
        match &self.0.refusal {
            Some(KeyringFailure::NoEntry) => Err(KeyringFailure::NoEntry),
            Some(KeyringFailure::NoBackend) => Err(KeyringFailure::NoBackend),
            Some(KeyringFailure::Backend(message)) => Err(KeyringFailure::Backend(message.clone())),
            None => Ok(()),
        }
    }
}

/// One `vibe mcp` run over a named home, with both streams captured.
struct Run {
    code: u8,
    stdout: String,
    stderr: String,
}

fn drive(
    arguments: &[&str],
    vibe_home: &Path,
    working_directory: &Path,
    keyring: &SharedKeyring,
) -> Run {
    let arguments = arguments
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect::<Vec<_>>();
    let environment =
        McpEnvironment::for_home(vibe_home, working_directory, Box::new(keyring.clone()));
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = run(&arguments, &environment, &mut stdout, &mut stderr);
    Run {
        code,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    }
}

const OAUTH_SERVER: &str = r#"
[[mcp_servers]]
name = "remote"
transport = "streamable-http"
url = "https://mcp.example.com/sse"

[mcp_servers.auth]
type = "oauth"
scopes = []
"#;

const STDIO_SERVER: &str = r#"
[[mcp_servers]]
name = "local"
transport = "stdio"
command = "echo"
"#;

/// A home holding `config.toml`, plus the workspace a run resolves against.
fn home_with(configuration: &str) -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let root = tempfile::tempdir().expect("temporary directory");
    let vibe_home = root.path().join("vibe-home");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&vibe_home).expect("home directory");
    std::fs::create_dir_all(&workspace).expect("workspace directory");
    std::fs::write(vibe_home.join("config.toml"), configuration).expect("user configuration");
    (root, vibe_home, workspace)
}

fn user_configuration(vibe_home: &Path) -> String {
    std::fs::read_to_string(vibe_home.join("config.toml")).unwrap_or_default()
}

/// The account naming is vibe-core's, so the test asks it rather than
/// restating the fingerprint.
fn expected_account(resource: &str) -> String {
    let resource = url::Url::parse(resource).expect("resource URL");
    vibe_core::auth::mcp_oauth_account(&resource).expect("account name")
}

#[test]
fn a_credential_deletion_that_fails_leaves_the_configuration_entry_in_place() {
    let (_root, vibe_home, workspace) = home_with(OAUTH_SERVER);
    let keyring = SharedKeyring::refusing(KeyringFailure::Backend(
        "the collection is locked".to_owned(),
    ));

    let outcome = drive(&["remove", "remote"], &vibe_home, &workspace, &keyring);

    assert_eq!(outcome.code, 2, "stderr was {}", outcome.stderr);
    assert!(
        outcome.stderr.contains("vibe mcp remove: error: "),
        "{}",
        outcome.stderr
    );
    assert!(outcome.stderr.contains("remote"), "{}", outcome.stderr);
    assert_eq!(
        user_configuration(&vibe_home),
        OAUTH_SERVER,
        "the configuration changed despite the credential failure"
    );
}

#[test]
fn a_successful_removal_deletes_the_credential_before_the_entry() {
    let (_root, vibe_home, workspace) = home_with(OAUTH_SERVER);
    let keyring = SharedKeyring::default();

    let outcome = drive(&["remove", "remote"], &vibe_home, &workspace, &keyring);

    assert_eq!(outcome.code, 0, "stderr was {}", outcome.stderr);
    assert_eq!(outcome.stdout, "Removed MCP server `remote`.\n");
    assert_eq!(
        keyring.deletions().as_slice(),
        &[(
            vibe_core::auth::MCP_OAUTH_KEYRING_SERVICE.to_owned(),
            expected_account("https://mcp.example.com/sse"),
        )]
    );
    assert!(
        !user_configuration(&vibe_home).contains("name = \"remote\""),
        "the entry survived the removal"
    );
}

#[test]
fn a_server_that_stores_no_credential_is_removed_without_reaching_the_keyring() {
    let (_root, vibe_home, workspace) = home_with(STDIO_SERVER);
    let keyring = SharedKeyring::refusing(KeyringFailure::Backend(
        "the collection is locked".to_owned(),
    ));

    let outcome = drive(&["remove", "local"], &vibe_home, &workspace, &keyring);

    assert_eq!(outcome.code, 0, "stderr was {}", outcome.stderr);
    assert_eq!(outcome.stdout, "Removed MCP server `local`.\n");
    assert!(
        keyring.deletions().is_empty(),
        "a stdio server reached the credential store"
    );
}

/// Reference `delete_oauth_credentials`: a store that holds nothing and a host
/// with no store at all both leave the removal free to proceed.
#[test]
fn an_unusable_credential_store_does_not_block_the_removal() {
    for failure in [KeyringFailure::NoEntry, KeyringFailure::NoBackend] {
        let (_root, vibe_home, workspace) = home_with(OAUTH_SERVER);
        let keyring = SharedKeyring::refusing(failure);

        let outcome = drive(&["remove", "remote"], &vibe_home, &workspace, &keyring);

        assert_eq!(outcome.code, 0, "stderr was {}", outcome.stderr);
        assert_eq!(outcome.stdout, "Removed MCP server `remote`.\n");
    }
}

#[test]
fn removing_a_server_no_entry_carries_reports_it_and_succeeds() {
    let (_root, vibe_home, workspace) = home_with(STDIO_SERVER);
    let keyring = SharedKeyring::default();

    let outcome = drive(
        &["remove", "absent-server"],
        &vibe_home,
        &workspace,
        &keyring,
    );

    assert_eq!(outcome.code, 0, "stderr was {}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        "MCP server `absent-server` is not configured in the user config.\n"
    );
    assert!(
        user_configuration(&vibe_home).contains("name = \"local\""),
        "an unrelated entry was dropped"
    );
}

/// A name the store refuses is still an argument failure, so it leaves through
/// `parser.error` rather than as a bare message.
#[test]
fn a_name_the_store_refuses_funnels_through_the_usage_error() {
    let (_root, vibe_home, workspace) = home_with(STDIO_SERVER);
    let keyring = SharedKeyring::default();

    let outcome = drive(&["remove", "  "], &vibe_home, &workspace, &keyring);

    assert_eq!(outcome.code, 2, "stdout was {}", outcome.stdout);
    assert!(
        outcome
            .stderr
            .starts_with("usage: vibe mcp remove [-h] NAME\n"),
        "{}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("vibe mcp remove: error: "),
        "{}",
        outcome.stderr
    );
    assert!(outcome.stdout.is_empty());
    assert_eq!(
        user_configuration(&vibe_home),
        STDIO_SERVER,
        "a refused name still reached the user configuration"
    );
}

/// The reference builds a user-only harness for this command, so an entry of
/// the same name in the workspace is neither read nor rewritten.
#[test]
fn a_project_configuration_of_the_same_name_is_left_alone() {
    let (_root, vibe_home, workspace) = home_with(STDIO_SERVER);
    let project = workspace.join(".vibe");
    std::fs::create_dir_all(&project).expect("project directory");
    let project_file = project.join("config.toml");
    std::fs::write(&project_file, STDIO_SERVER).expect("project configuration");
    let keyring = SharedKeyring::default();

    let outcome = drive(&["remove", "local"], &vibe_home, &workspace, &keyring);

    assert_eq!(outcome.code, 0, "stderr was {}", outcome.stderr);
    assert_eq!(
        std::fs::read_to_string(&project_file).expect("project configuration"),
        STDIO_SERVER,
        "the project configuration was rewritten"
    );
    assert!(!user_configuration(&vibe_home).contains("name = \"local\""));
}

/// The help renders and the argument failures do not touch the store at all,
/// which is what lets the parity replay drive them without a home.
#[test]
fn every_help_render_and_argument_failure_answers_the_documented_code() {
    let (_root, vibe_home, workspace) = home_with(STDIO_SERVER);
    let keyring = SharedKeyring::default();
    let expected: BTreeMap<&[&str], (u8, bool)> = BTreeMap::from([
        (&[][..], (0, true)),
        (&["-h"][..], (0, true)),
        (&["--help"][..], (0, true)),
        (&["add", "-h"][..], (0, true)),
        (&["remove", "-h"][..], (0, true)),
        (&["list"][..], (2, false)),
        (&["remove"][..], (2, false)),
        (&["remove", "first", "second"][..], (2, false)),
        (&["add"][..], (2, false)),
        (&["add", "server", "--transport", "bogus"][..], (2, false)),
        (&["add", "server", "--url"][..], (2, false)),
    ]);
    for (arguments, (code, on_stdout)) in expected {
        let outcome = drive(arguments, &vibe_home, &workspace, &keyring);
        assert_eq!(outcome.code, code, "for {arguments:?}");
        assert_eq!(
            user_configuration(&vibe_home),
            STDIO_SERVER,
            "{arguments:?} wrote to the user configuration"
        );
        assert_eq!(
            !outcome.stdout.is_empty(),
            on_stdout,
            "for {arguments:?}: stdout {:?}, stderr {:?}",
            outcome.stdout,
            outcome.stderr
        );
        assert_eq!(!outcome.stderr.is_empty(), !on_stdout, "for {arguments:?}");
    }
}
