//! `vibe mcp add` and `vibe mcp remove`, driven through [`run`] rather than
//! through their parts.
//!
//! Every case here starts at the argument vector the intercept hands over, so
//! what the tests observe is what the binary does: the exit code, the two
//! streams, the user configuration file on disk and the credential store.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use tempfile::TempDir;
use vibe_core::auth::{KeyringBackend, KeyringFailure};

use super::{LoginFuture, McpEnvironment, McpOAuthLogin, run};
use vibe_core::auth::AuthUrlSink;
use vibe_core::mcp::McpAuthenticationService;

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
        // A host with no store fails every read, which is how the reference
        // learns there is nothing to delete.
        match &self.0.refusal {
            Some(KeyringFailure::NoBackend) => Err(KeyringFailure::NoBackend),
            _ => Ok(None),
        }
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

/// What a login was asked to do, and what it answers.
///
/// A test that drives the OAuth path reads the log to check the ordering the
/// reference fixes: the entry is written, the URL is narrated, the browser is
/// offered it, and only then does the wait begin.
struct ScriptedLogin {
    /// The URL `begin` answers with, or the reason it refuses.
    begin: Result<String, String>,
    /// The reason the host cannot open a browser, when it cannot.
    open: Option<String>,
    /// The reason the exchange failed, when it did.
    finish: Option<String>,
    calls: Mutex<Vec<String>>,
}

impl Default for ScriptedLogin {
    fn default() -> Self {
        Self {
            begin: Ok("https://auth.example.com/authorize?state=parity".to_owned()),
            open: None,
            finish: None,
            calls: Mutex::new(Vec::new()),
        }
    }
}

#[derive(Clone, Default)]
struct SharedLogin(std::sync::Arc<ScriptedLogin>);

impl SharedLogin {
    fn with(script: ScriptedLogin) -> Self {
        Self(std::sync::Arc::new(script))
    }

    fn calls(&self) -> Vec<String> {
        self.0
            .calls
            .lock()
            .map(|calls| calls.clone())
            .unwrap_or_default()
    }

    fn record(&self, call: &str) {
        if let Ok(mut calls) = self.0.calls.lock() {
            calls.push(call.to_owned());
        }
    }
}

impl McpOAuthLogin for SharedLogin {
    fn login<'a>(
        &'a self,
        _authentication: &'a McpAuthenticationService,
        name: &'a str,
        on_url: AuthUrlSink,
    ) -> LoginFuture<'a, ()> {
        Box::pin(async move {
            self.record(&format!("login {name}"));
            on_url(self.0.begin.clone()?).await;
            self.record(&format!("finish {name}"));
            self.0.finish.clone().map_or(Ok(()), Err)
        })
    }

    fn open(&self, url: &str) -> Result<(), String> {
        self.record(&format!("open {url}"));
        self.0.open.clone().map_or(Ok(()), Err)
    }
}

/// A login no case here reaches, so reaching it is the failure.
struct UnusedLogin;

impl McpOAuthLogin for UnusedLogin {
    fn login<'a>(
        &'a self,
        _authentication: &'a McpAuthenticationService,
        _name: &'a str,
        _on_url: AuthUrlSink,
    ) -> LoginFuture<'a, ()> {
        Box::pin(async { Err("this case must not start a login".to_owned()) })
    }

    fn open(&self, _url: &str) -> Result<(), String> {
        Err("this case must not open a browser".to_owned())
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
    drive_with(
        arguments,
        vibe_home,
        working_directory,
        keyring,
        UnusedLogin,
    )
}

fn drive_with(
    arguments: &[&str],
    vibe_home: &Path,
    working_directory: &Path,
    keyring: &SharedKeyring,
    login: impl McpOAuthLogin + 'static,
) -> Run {
    let arguments = arguments
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect::<Vec<_>>();
    let environment = McpEnvironment::for_home(
        vibe_home,
        working_directory,
        std::sync::Arc::new(keyring.clone()),
        Box::new(login),
    );
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    // The command is async because the OAuth login is; a test drives it on a
    // runtime of its own rather than making every case here async.
    let code = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for the run")
        .block_on(run(&arguments, &environment, &mut stdout, &mut stderr));
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
    // The account naming is vibe-core's, so the test asks it rather than
    // restating it.
    let current = keyring
        .deletions()
        .into_iter()
        .filter(|(service, _)| service == vibe_core::auth::KEYRING_SERVICE)
        .map(|(_, account)| account)
        .collect::<Vec<_>>();
    assert_eq!(
        current,
        ["tokens", "client_info", "fingerprint"]
            .map(|kind| vibe_core::auth::mcp_oauth_username("remote", kind))
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

// --------------------------------------------------------------------------
// `vibe mcp add`
// --------------------------------------------------------------------------

/// The user configuration a home starts empty at, for the cases that only care
/// about what `add` writes.
const NO_SERVER: &str = "";

#[test]
fn a_new_stdio_server_is_appended_and_named_in_the_message() {
    let (_root, vibe_home, workspace) = home_with(NO_SERVER);
    let keyring = SharedKeyring::default();

    let outcome = drive(
        &[
            "add",
            "local",
            "--transport",
            "stdio",
            "--command",
            "/bin/true",
            "--arg",
            "serve",
            "--arg=--verbose",
            "--env",
            "FIRST=one",
            "--env",
            "SECOND= two ",
        ],
        &vibe_home,
        &workspace,
        &keyring,
    );

    assert_eq!(outcome.code, 0, "stderr was {}", outcome.stderr);
    assert_eq!(outcome.stdout, "Added MCP server `local`.\n");
    let written = user_configuration(&vibe_home);
    assert!(written.contains("name = \"local\""), "{written}");
    assert!(written.contains("command = \"/bin/true\""), "{written}");
    // Every occurrence is kept, in the order it was given.
    assert!(written.contains("\"--verbose\""), "{written}");
    assert!(
        written.find("\"serve\"") < written.find("\"--verbose\""),
        "{written}"
    );
    assert!(written.contains("FIRST = \"one\""), "{written}");
    assert!(written.contains("SECOND = \"two\""), "{written}");
    // The reference stores no launch directory for an entry the command line
    // built, so a reader resolves it against its own workspace.
    assert!(!written.contains("cwd"), "{written}");
}

/// Reference `persist_stdio_mcp_server`: the same entry twice is a no-op that
/// says so, which is what makes the command safe in a provisioning script.
#[test]
fn re_adding_the_same_stdio_server_writes_nothing_and_says_so() {
    let (_root, vibe_home, workspace) = home_with(NO_SERVER);
    let keyring = SharedKeyring::default();
    let arguments = ["add", "local", "--transport", "stdio", "--command", "echo"];

    let first = drive(&arguments, &vibe_home, &workspace, &keyring);
    assert_eq!(first.code, 0, "stderr was {}", first.stderr);
    let after_first = user_configuration(&vibe_home);

    let second = drive(&arguments, &vibe_home, &workspace, &keyring);

    assert_eq!(second.code, 0, "stderr was {}", second.stderr);
    assert_eq!(second.stdout, "MCP server `local` is already configured.\n");
    assert_eq!(
        user_configuration(&vibe_home),
        after_first,
        "the second add rewrote the configuration"
    );
}

/// Reference `_remote_servers_equivalent`: an entry that differs only in how
/// its URL is spelled is the same server, so the options decide the outcome.
#[test]
fn an_existing_remote_entry_answers_by_what_the_options_say() {
    let keyring = SharedKeyring::default();
    let cases: [(&[&str], u8, &str); 3] = [
        (
            // Identical, down to the OAuth block the reference defaults to.
            &["add", "remote", "--url", "https://mcp.example.com/sse"],
            0,
            "MCP server `remote` is already configured.",
        ),
        (
            // The same endpoint, spelled with the default port, an upper-case
            // host and a trailing slash, carrying different options.
            &[
                "add",
                "remote",
                "--url",
                "https://MCP.Example.com:443/sse/",
                "--api-key-env",
                "TOKEN",
            ],
            2,
            "MCP server `remote` is already configured with different options",
        ),
        (
            // A different kind of entry under a name already taken.
            &["add", "remote", "--transport", "stdio", "--command", "echo"],
            2,
            "MCP server name `remote` is already configured",
        ),
    ];
    for (arguments, code, fragment) in cases {
        let (_root, vibe_home, workspace) = home_with(OAUTH_SERVER);

        let outcome = drive_with(
            arguments,
            &vibe_home,
            &workspace,
            &keyring,
            SharedLogin::default(),
        );

        assert_eq!(outcome.code, code, "for {arguments:?}");
        let stream = if code == 0 {
            &outcome.stdout
        } else {
            &outcome.stderr
        };
        assert!(stream.contains(fragment), "for {arguments:?}: {stream}");
        assert_eq!(
            user_configuration(&vibe_home),
            OAUTH_SERVER,
            "{arguments:?} rewrote the configuration"
        );
    }
}

#[test]
fn a_url_another_entry_already_addresses_is_refused_naming_it() {
    let (_root, vibe_home, workspace) = home_with(OAUTH_SERVER);
    let keyring = SharedKeyring::default();

    let outcome = drive(
        &[
            "add",
            "second",
            "--url",
            "https://mcp.example.com/sse",
            "--no-login",
        ],
        &vibe_home,
        &workspace,
        &keyring,
    );

    assert_eq!(outcome.code, 2, "stdout was {}", outcome.stdout);
    assert!(
        outcome
            .stderr
            .contains("MCP server URL is already configured as `remote`"),
        "{}",
        outcome.stderr
    );
    assert_eq!(user_configuration(&vibe_home), OAUTH_SERVER);
}

#[test]
fn a_stdio_name_carrying_another_command_is_a_name_collision() {
    let (_root, vibe_home, workspace) = home_with(STDIO_SERVER);
    let keyring = SharedKeyring::default();

    let outcome = drive(
        &[
            "add",
            "local",
            "--transport",
            "stdio",
            "--command",
            "/bin/true",
        ],
        &vibe_home,
        &workspace,
        &keyring,
    );

    assert_eq!(outcome.code, 2, "stdout was {}", outcome.stdout);
    assert!(
        outcome
            .stderr
            .contains("MCP server name `local` is already configured"),
        "{}",
        outcome.stderr
    );
    assert_eq!(user_configuration(&vibe_home), STDIO_SERVER);
}

/// The static scheme is stored where a reader looks for it: the headers on the
/// entry, the carrier and its template beside the variable that fills them.
#[test]
fn a_static_remote_server_stores_its_carrier_and_its_headers() {
    let (_root, vibe_home, workspace) = home_with(NO_SERVER);
    let keyring = SharedKeyring::default();

    let outcome = drive(
        &[
            "add",
            "notes",
            "--transport",
            "http",
            "--url",
            "https://notes.example.com/mcp",
            "--bearer-token-env-var",
            "NOTES_TOKEN",
            "--api-key-header",
            "X-Api-Key",
            "--api-key-format",
            "Token {token}",
            "--header",
            "X-Trace=abc",
        ],
        &vibe_home,
        &workspace,
        &keyring,
    );

    assert_eq!(outcome.code, 0, "stderr was {}", outcome.stderr);
    assert_eq!(outcome.stdout, "Added MCP server `notes`.\n");
    let written = user_configuration(&vibe_home);
    assert!(written.contains("transport = \"http\""), "{written}");
    // The alias sets the destination `--api-key-env` sets.
    assert!(
        written.contains("api_key_env = \"NOTES_TOKEN\""),
        "{written}"
    );
    assert!(
        written.contains("api_key_header = \"X-Api-Key\""),
        "{written}"
    );
    assert!(
        written.contains("api_key_format = \"Token {token}\""),
        "{written}"
    );
    assert!(written.contains("X-Trace = \"abc\""), "{written}");
    assert!(!written.contains("oauth"), "{written}");
}

/// Reference `_parse_add_args` and everything under it: every one of these
/// refusals happens before the store is opened, so the bytes on disk are the
/// proof that nothing was written.
#[test]
fn every_add_refusal_exits_two_and_leaves_the_configuration_alone() {
    let keyring = SharedKeyring::default();
    let cases: [(&[&str], &str); 15] = [
        (
            &[
                "add",
                "s",
                "--url",
                "https://a.example.com",
                "--command",
                "x",
                "--arg",
                "y",
            ],
            "only --transport stdio accepts --command, --arg.",
        ),
        (
            &["add", "s"],
            "--url is what an http or streamable-http server is reached at.",
        ),
        (
            &[
                "add",
                "s",
                "--transport",
                "stdio",
                "--command",
                "x",
                "--url",
                "https://a.example.com",
                "--no-login",
            ],
            "--transport stdio does not accept --url, --no-login.",
        ),
        (
            &["add", "s", "--transport", "stdio"],
            "--transport stdio needs --command.",
        ),
        (
            &[
                "add",
                "s",
                "--url",
                "https://a.example.com",
                "--header",
                "oops",
            ],
            "--header values are spelled NAME=VALUE.",
        ),
        (
            &[
                "add",
                "s",
                "--transport",
                "stdio",
                "--command",
                "x",
                "--env",
                "oops",
            ],
            "--env values are spelled NAME=VALUE.",
        ),
        (
            &[
                "add",
                "s",
                "--url",
                "https://a.example.com",
                "--header",
                "X-One=1",
                "--header",
                "x-one=2",
            ],
            "--header names `x-one` twice.",
        ),
        (
            &[
                "add",
                "s",
                "--transport",
                "stdio",
                "--command",
                "x",
                "--env",
                "A=1",
                "--env",
                "A=2",
            ],
            "--env names `A` twice.",
        ),
        (
            &[
                "add",
                "s",
                "--url",
                "https://a.example.com",
                "--api-key-header",
                "X-Key",
            ],
            "--api-key-header needs --api-key-env.",
        ),
        (
            &[
                "add",
                "s",
                "--url",
                "https://a.example.com",
                "--api-key-format",
                "{token}",
            ],
            "--api-key-format needs --api-key-env.",
        ),
        (
            &[
                "add",
                "s",
                "--url",
                "https://a.example.com",
                "--api-key-env",
                "TOKEN",
                "--header",
                "authorization=x",
            ],
            "--header already defines the API key header `Authorization`.",
        ),
        (
            &[
                "add",
                "s",
                "--url",
                "https://a.example.com",
                "--api-key-env",
                "TOKEN",
                "--no-login",
            ],
            "--no-login asks for OAuth, which the static authentication options replace.",
        ),
        (
            &["add", "s", "--url", "http://plain.example.com/mcp"],
            "MCP server URL must use https unless it points to localhost",
        ),
        (
            &["add", "s", "--url", "https://a.example.com/mcp#frag"],
            "MCP server URL must not include a fragment",
        ),
        (
            &["add", "___", "--url", "https://a.example.com/mcp"],
            "MCP server field `name` must be a non-empty string",
        ),
    ];
    for (arguments, fragment) in cases {
        let (_root, vibe_home, workspace) = home_with(STDIO_SERVER);

        let outcome = drive(arguments, &vibe_home, &workspace, &keyring);

        assert_eq!(outcome.code, 2, "for {arguments:?}: {}", outcome.stdout);
        // Every one of them leaves through the root parser, which is the
        // funnel `parser.error` reaches from the sub-command.
        assert!(
            outcome.stderr.starts_with("usage: vibe mcp "),
            "for {arguments:?}: {}",
            outcome.stderr
        );
        assert!(
            outcome
                .stderr
                .contains(&format!("vibe mcp: error: {fragment}")),
            "for {arguments:?}: {}",
            outcome.stderr
        );
        assert!(outcome.stdout.is_empty(), "for {arguments:?}");
        assert_eq!(
            user_configuration(&vibe_home),
            STDIO_SERVER,
            "{arguments:?} wrote to the user configuration"
        );
    }
}

/// A loopback host is the one address the reference reaches over plain HTTP.
#[test]
fn a_loopback_url_is_accepted_over_plain_http() {
    let (_root, vibe_home, workspace) = home_with(NO_SERVER);
    let keyring = SharedKeyring::default();

    let outcome = drive(
        &[
            "add",
            "dev",
            "--url",
            "http://localhost:3000/mcp",
            "--no-login",
        ],
        &vibe_home,
        &workspace,
        &keyring,
    );

    assert_eq!(outcome.code, 0, "stderr was {}", outcome.stderr);
    assert!(
        user_configuration(&vibe_home).contains("http://localhost:3000/mcp"),
        "{}",
        user_configuration(&vibe_home)
    );
}

/// A timeout the parser could read the shape of but not the value of is an
/// argument failure, reported by the sub-command that declared it.
#[test]
fn a_non_numeric_timeout_leaves_through_the_argument_funnel() {
    let (_root, vibe_home, workspace) = home_with(STDIO_SERVER);
    let keyring = SharedKeyring::default();
    for flag in ["--startup-timeout-sec", "--tool-timeout-sec"] {
        let outcome = drive(
            &["add", "s", "--url", "https://a.example.com", flag, "soon"],
            &vibe_home,
            &workspace,
            &keyring,
        );

        assert_eq!(outcome.code, 2, "for {flag}: {}", outcome.stdout);
        assert!(
            outcome
                .stderr
                .contains(&format!("argument {flag}: invalid float value: 'soon'")),
            "for {flag}: {}",
            outcome.stderr
        );
        assert_eq!(user_configuration(&vibe_home), STDIO_SERVER);
    }
}

#[test]
fn a_declared_timeout_reaches_the_entry_in_seconds() {
    let (_root, vibe_home, workspace) = home_with(NO_SERVER);
    let keyring = SharedKeyring::default();

    let outcome = drive(
        &[
            "add",
            "slow",
            "--transport",
            "stdio",
            "--command",
            "echo",
            "--startup-timeout-sec",
            "2.5",
            "--tool-timeout-sec",
            "90",
        ],
        &vibe_home,
        &workspace,
        &keyring,
    );

    assert_eq!(outcome.code, 0, "stderr was {}", outcome.stderr);
    let written = user_configuration(&vibe_home);
    assert!(written.contains("startup_timeout_sec = 2.5"), "{written}");
    assert!(written.contains("tool_timeout_sec = 90.0"), "{written}");
}

/// Reference `_add_mcp_server` with `--no-login`: the entry is stored and the
/// user is told where the login lives, rather than being sent to a browser.
#[test]
fn an_oauth_add_that_declines_the_login_says_where_to_authenticate() {
    let (_root, vibe_home, workspace) = home_with(NO_SERVER);
    let keyring = SharedKeyring::default();
    let login = SharedLogin::default();

    let outcome = drive_with(
        &[
            "add",
            "docs",
            "--transport",
            "http",
            "--url",
            "https://docs.example.com/mcp",
            "--no-login",
        ],
        &vibe_home,
        &workspace,
        &keyring,
        login.clone(),
    );

    assert_eq!(outcome.code, 0, "stderr was {}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        "Added MCP server `docs`.\nRun `/mcp login docs` to authenticate.\n"
    );
    assert!(login.calls().is_empty(), "{:?}", login.calls());
    assert!(user_configuration(&vibe_home).contains("type = \"oauth\""));
}

/// Reference `add_mcp_server`: the entry is written, then reported, then the
/// login runs. A login that never returns still leaves a configured server.
#[test]
fn an_oauth_add_persists_before_it_logs_in_and_narrates_the_url() {
    let (_root, vibe_home, workspace) = home_with(NO_SERVER);
    let keyring = SharedKeyring::default();
    let login = SharedLogin::default();

    let outcome = drive_with(
        &["add", "docs", "--url", "https://docs.example.com/mcp"],
        &vibe_home,
        &workspace,
        &keyring,
        login.clone(),
    );

    assert_eq!(outcome.code, 0, "stderr was {}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        concat!(
            "Added MCP server `docs`.\n",
            "Open this URL in your browser:\n",
            "\n",
            "  https://auth.example.com/authorize?state=parity\n",
            "OAuth login completed.\n",
        )
    );
    assert!(outcome.stderr.is_empty(), "{}", outcome.stderr);
    assert_eq!(
        login.calls(),
        vec![
            "login docs".to_owned(),
            "open https://auth.example.com/authorize?state=parity".to_owned(),
            "finish docs".to_owned(),
        ]
    );
    assert!(user_configuration(&vibe_home).contains("name = \"docs\""));
}

#[test]
fn a_browser_that_will_not_open_is_reported_and_the_login_continues() {
    let (_root, vibe_home, workspace) = home_with(NO_SERVER);
    let keyring = SharedKeyring::default();
    let login = SharedLogin::with(ScriptedLogin {
        begin: Ok("https://auth.example.com/authorize".to_owned()),
        open: Some("this host has no browser launcher".to_owned()),
        ..ScriptedLogin::default()
    });

    let outcome = drive_with(
        &["add", "docs", "--url", "https://docs.example.com/mcp"],
        &vibe_home,
        &workspace,
        &keyring,
        login.clone(),
    );

    assert_eq!(outcome.code, 0, "stderr was {}", outcome.stderr);
    assert_eq!(
        outcome.stderr,
        "Could not open the browser: this host has no browser launcher\n"
    );
    assert!(outcome.stdout.ends_with("OAuth login completed.\n"));
    // The URL reached stdout before the browser was offered it, which is why
    // a host with no launcher can still finish the login by hand.
    assert!(
        login.calls().contains(&"finish docs".to_owned()),
        "{:?}",
        login.calls()
    );
}

/// Reference: the login is best-effort, so its failure names the session
/// command that retries it and leaves the entry where it is.
#[test]
fn a_login_that_fails_keeps_the_entry_and_exits_one() {
    let (_root, vibe_home, workspace) = home_with(NO_SERVER);
    let keyring = SharedKeyring::default();
    let login = SharedLogin::with(ScriptedLogin {
        begin: Ok("https://auth.example.com/authorize".to_owned()),
        finish: Some("the authorization server refused the exchange".to_owned()),
        ..ScriptedLogin::default()
    });

    let outcome = drive_with(
        &["add", "docs", "--url", "https://docs.example.com/mcp"],
        &vibe_home,
        &workspace,
        &keyring,
        login,
    );

    assert_eq!(outcome.code, 1, "stdout was {}", outcome.stdout);
    assert_eq!(
        outcome.stderr,
        concat!(
            "vibe mcp add: OAuth login failed: ",
            "the authorization server refused the exchange\n",
            "Run `/mcp login docs` to authenticate.\n",
        )
    );
    assert!(
        user_configuration(&vibe_home).contains("name = \"docs\""),
        "the failed login dropped the entry"
    );
    assert!(
        keyring.deletions().is_empty(),
        "a failed login touched the credential store"
    );
}

/// A login that cannot even start is the same class of failure, and it too
/// leaves the server configured.
#[test]
fn a_login_that_never_starts_still_leaves_the_server_configured() {
    let (_root, vibe_home, workspace) = home_with(NO_SERVER);
    let keyring = SharedKeyring::default();
    let login = SharedLogin::with(ScriptedLogin {
        begin: Err("no authorization server answered".to_owned()),
        ..ScriptedLogin::default()
    });

    let outcome = drive_with(
        &["add", "docs", "--url", "https://docs.example.com/mcp"],
        &vibe_home,
        &workspace,
        &keyring,
        login.clone(),
    );

    assert_eq!(outcome.code, 1, "stdout was {}", outcome.stdout);
    assert_eq!(outcome.stdout, "Added MCP server `docs`.\n");
    assert!(
        outcome
            .stderr
            .contains("vibe mcp add: OAuth login failed: "),
        "{}",
        outcome.stderr
    );
    assert_eq!(login.calls(), vec!["login docs".to_owned()]);
    assert!(user_configuration(&vibe_home).contains("name = \"docs\""));
}
