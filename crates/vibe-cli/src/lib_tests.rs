use std::io;

use super::*;
use vibe_app_server::client::{DriverFuture, EchoTurnDriver, TurnReservation};

struct BrokenStdout;

impl Write for BrokenStdout {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed reader"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed reader"))
    }
}

fn arguments(mode: OutputMode) -> Arguments {
    Arguments {
        help: None,
        initial_prompt: None,
        version: None,
        prompt: Some("hello".to_owned()),
        output: mode,
        resume: None,
        continue_session: false,
        workdir: Some(PathBuf::from("/workspace")),
        add_directories: vec![PathBuf::from("/shared")],
        trust: true,
        agent: Some("coder".to_owned()),
        enabled_tools: vec!["read".to_owned()],
        disabled_tools: vec!["shell".to_owned()],
        tool_filters: vec!["read".to_owned()],
        max_turns: Some(4),
        max_tokens: Some(1000),
        max_price: Some(0.01),
        experimental_harness: false,
        legacy_harness: false,
        smart_approve: false,
        auto_approve: true,
        setup: false,
        check_upgrade: true,
        worktree: None,
        teleport: false,
        provider_style: None,
        model: Some("test".to_owned()),
        input_price: None,
        output_price: None,
        api_base: Some("https://provider.invalid".to_owned()),
        credential_environment: Some("TEST_API_KEY".to_owned()),
        session_root: None,
        fake_response: Some("world".to_owned()),
    }
}

/// US-017: the binary opens the reference path under the home the
/// invocation resolves, before anything else can fail, and a second call
/// attaches nothing new. The installation is process-wide, so this is the
/// one test that owns it.
#[test]
fn the_binary_opens_the_log_file_under_the_home_it_resolves() {
    let enclosure = tempfile::tempdir().expect("a home enclosure");
    let home = enclosure.path().join(".vibe");
    let mut invocation = arguments(OutputMode::Text);
    invocation.session_root = Some(home.join("sessions"));
    install_file_logging(&invocation);

    let installed = observability::installed_log().expect("a log file is installed");
    assert_eq!(installed.path(), home.join("logs").join("vibe.log"));
    assert!(installed.path().is_file(), "the file opens at startup");

    observability::log(observability::LogLevel::Critical, "the oracle was here");
    let page = installed.reader().get_logs(10, 0);
    assert_eq!(
        page.entries
            .first()
            .map(|entry| entry.message.clone())
            .unwrap_or_default(),
        "the oracle was here",
        "a record written through the installed logger reads back"
    );

    // A second initialization attaches nothing: the file stays the one
    // already open, which is the guard the reference keeps per path.
    install_file_logging(&invocation);
    assert_eq!(
        observability::installed_log().map(observability::FileLog::path),
        Some(home.join("logs").join("vibe.log").as_path())
    );
}

#[test]
fn incompatible_session_intents_are_rejected_by_clap() {
    let parsed = Arguments::try_parse_from([
        "vibe",
        "--prompt",
        "hello",
        "--resume",
        "session-1",
        "--continue",
    ]);
    assert!(parsed.is_err());
}

#[test]
fn interactive_mode_does_not_require_the_version_flag() {
    let parsed = Arguments::try_parse_from(["vibe"]).expect("interactive arguments");
    assert!(parsed.version.is_none());
    assert!(parsed.initial_prompt.is_none());
    assert!(parsed.prompt.is_none());
}

#[test]
fn empty_programmatic_prompt_is_rejected() {
    let mut arguments = arguments(OutputMode::Text);
    arguments.prompt = Some("  ".to_owned());
    assert!(
        validate_arguments(&arguments).is_ok(),
        "a blank prompt runs"
    );
    arguments.prompt = Some(String::new());
    assert!(matches!(
        validate_arguments(&arguments),
        Err(CliError::InvalidArguments(message))
            if message == "No prompt provided for programmatic mode"
    ));
}

/// The reference publishes no telemetry flag: the configuration key is the
/// only control, so passing one is an unknown argument and no help output
/// mentions it.
#[test]
fn the_binary_publishes_no_telemetry_flag() {
    use clap::CommandFactory;

    assert!(Arguments::try_parse_from(["vibe", "--telemetry"]).is_err());
    let help = Arguments::command().render_long_help().to_string();
    assert!(!help.contains("--telemetry"), "{help}");
}

/// Reference `resolve_api_key`: the credential a delivery authenticates
/// with is read from the environment first and the OS keyring second, so a
/// key onboarding stored in the keyring alone still activates telemetry.
#[test]
fn the_telemetry_credential_reaches_the_keyring() {
    use vibe_core::auth::{KEYRING_SERVICE, KeyringBackend, KeyringFailure};

    struct StoredKey;

    impl KeyringBackend for StoredKey {
        fn get(&self, service: &str, account: &str) -> Result<Option<String>, KeyringFailure> {
            Ok((service == KEYRING_SERVICE && account == "MISTRAL_API_KEY")
                .then(|| "keyring-credential".to_owned()))
        }

        fn set(&self, _: &str, _: &str, _: &str) -> Result<(), KeyringFailure> {
            Err(KeyringFailure::NoBackend)
        }

        fn delete(&self, _: &str, _: &str) -> Result<(), KeyringFailure> {
            Err(KeyringFailure::NoEntry)
        }
    }

    let stored = || KeyringStore::new(Box::new(StoredKey));
    let credentials = telemetry_credentials(BTreeMap::new(), stored());
    assert_eq!(
        credentials("MISTRAL_API_KEY").as_deref(),
        Some("keyring-credential"),
        "a key held only by the credential store still resolves"
    );
    assert_eq!(credentials("ABSENT_KEY"), None);

    let exported = telemetry_credentials(
        BTreeMap::from([("MISTRAL_API_KEY".to_owned(), "exported".to_owned())]),
        stored(),
    );
    assert_eq!(
        exported("MISTRAL_API_KEY").as_deref(),
        Some("exported"),
        "the environment still wins over the store"
    );
}

/// The launch context is the reference's: the `cli` entrypoint, this
/// build's version on both sides, and a terminal named from the published
/// vocabulary.
#[test]
fn the_launch_context_reports_the_cli_entrypoint() {
    let context = cli_telemetry_context(cli_launch_context(), ExperimentExposures::default(), true);
    let launch = context.launch.expect("the CLI declares a launch context");
    assert_eq!(launch.agent_entrypoint, "cli");
    assert_eq!(launch.client_name, "vibe_cli");
    assert_eq!(launch.agent_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(launch.client_version, env!("CARGO_PKG_VERSION"));
    assert!(launch.terminal_emulator.is_some());
}

/// The census reads the exposures through the handle rather than through a
/// value taken at construction, which is what lets a rollout resolved after
/// the client is built reach the next event. Reference builds its client
/// with a getter closed over the experiment manager for the same reason.
#[test]
fn a_rollout_resolved_after_the_client_reaches_the_next_event() {
    let exposures = ExperimentExposures::default();
    let context = cli_telemetry_context(cli_launch_context(), exposures.clone(), true);
    assert!(
        !context
            .base_metadata(None)
            .properties()
            .contains_key("experiments"),
        "an unenrolled session reports no field at all"
    );
    exposures.publish(vec![vibe_core::telemetry::ExperimentAssignment {
        experiment_id: "vibe_cli_system_prompt".to_owned(),
        experiment_name: "vibe_cli_system_prompt".to_owned(),
        variation_name: "lean".to_owned(),
        variation_id: None,
        in_experiment: None,
        hash_attribute: None,
        hash_value: None,
        feature_id: None,
    }]);
    assert_eq!(
        context.base_metadata(None).properties()["experiments"],
        serde_json::json!({"vibe_cli_system_prompt": "lean"})
    );
}

#[test]
fn an_untrusted_folder_with_project_configuration_warns_on_stderr() {
    let root = tempfile::tempdir().expect("root");
    let home = root.path().join("vibe-home");
    let project = root.path().join("project");
    std::fs::create_dir_all(project.join(".vibe")).expect("project");
    std::fs::write(project.join(".vibe/config.toml"), "").expect("config");
    std::fs::write(project.join("AGENTS.md"), "").expect("agents");
    let mut stderr = Vec::new();
    programmatic::warn_if_workspace_untrusted(&home, &project, &mut stderr).expect("warned");
    let warning = String::from_utf8(stderr).expect("UTF-8");
    assert!(warning.contains("(.vibe/, AGENTS.md)"), "{warning}");

    let store = vibe_core::trust::TrustStore::for_vibe_home(&home);
    store.trust_for_session(&project);
    let mut stderr = Vec::new();
    programmatic::warn_if_workspace_untrusted(&home, &project, &mut stderr).expect("silent");
    store.revoke_session_trust(&project);
    assert!(stderr.is_empty(), "a --trust run is not warned");
}

#[tokio::test]
async fn text_json_and_streaming_have_deterministic_channels() {
    for (mode, expected) in [
        (OutputMode::Text, "world\n"),
        (OutputMode::Json, "\"role\": \"assistant\""),
        (OutputMode::Streaming, "\"role\": \"assistant\""),
    ] {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        execute(
            arguments(mode),
            EchoTurnDriver::new("world"),
            &mut stdout,
            &mut stderr,
        )
        .await
        .expect("programmatic run");
        let stdout = String::from_utf8(stdout).expect("UTF-8 output");
        assert!(stdout.contains(expected), "{stdout}");
        assert!(stderr.is_empty());
    }
}

/// The programmatic entry point declares its session headless and
/// withholds the two tools a question would have to reach a human through.
///
/// Both are read off the reservation the server hands the driver, so what
/// is asserted is what the session actually opened with rather than what
/// the options struct was filled with. The reference sets the same pair on
/// this branch and neither on the interactive one
/// (`vibe/cli/cli.py:151-192` against `:209-272`).
#[tokio::test]
async fn a_programmatic_run_opens_a_headless_session_without_the_interactive_tools() {
    struct IntentRecordingDriver {
        inner: EchoTurnDriver,
        seen: Arc<std::sync::Mutex<Option<vibe_app_server::server::SessionIntent>>>,
    }

    impl TurnDriver for IntentRecordingDriver {
        fn run<'a>(&'a self, reservation: &'a TurnReservation) -> DriverFuture<'a> {
            *vibe_core::sync::lock(&self.seen) = Some(reservation.intent.clone());
            self.inner.run(reservation)
        }
    }

    // The allowlist names one of the two withheld tools, which is the case
    // the denylist has to stay final for.
    let mut arguments = arguments(OutputMode::Text);
    arguments.enabled_tools = vec!["ask_user_question".to_owned(), "read_file".to_owned()];
    arguments.disabled_tools = vec!["shell".to_owned(), "exit_plan_mode".to_owned()];
    let seen = Arc::new(std::sync::Mutex::new(None));
    let driver = IntentRecordingDriver {
        inner: EchoTurnDriver::new("world"),
        seen: Arc::clone(&seen),
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    execute(arguments, driver, &mut stdout, &mut stderr)
        .await
        .expect("programmatic run");

    let intent = seen
        .lock()
        .expect("recorded intent")
        .clone()
        .expect("the turn was reserved");
    assert!(intent.headless, "the programmatic session is headless");
    assert!(
        intent.disabled_tools.contains(&"shell".to_owned()),
        "the user's own list survives: {:?}",
        intent.disabled_tools
    );
    for withheld in ["ask_user_question", "exit_plan_mode"] {
        assert_eq!(
            intent
                .disabled_tools
                .iter()
                .filter(|name| *name == withheld)
                .count(),
            1,
            "{withheld} is withheld exactly once: {:?}",
            intent.disabled_tools
        );
    }
    assert!(
        intent
            .enabled_tools
            .contains(&"ask_user_question".to_owned()),
        "the allowlist is left as the user wrote it: {:?}",
        intent.enabled_tools
    );
}

#[tokio::test]
async fn broken_stdout_is_typed_after_session_cleanup_is_attempted() {
    let mut stdout = BrokenStdout;
    let mut stderr = Vec::new();
    let error = execute(
        arguments(OutputMode::Text),
        EchoTurnDriver::new("world"),
        &mut stdout,
        &mut stderr,
    )
    .await
    .expect_err("broken stdout fails");
    assert!(matches!(error, CliError::Stdout(_)));
    assert_eq!(error.exit_code(), 1);
}

/// The reference types `--max-price` as a plain float and compares it, so
/// a negative budget is a budget already spent rather than a refusal.
#[test]
fn a_negative_price_budget_is_accepted_as_it_was_given() {
    let mut arguments = arguments(OutputMode::Text);
    arguments.max_price = Some(-1.0);
    assert!(validate_arguments(&arguments).is_ok());
    let budgets = bootstrap::Budgets::of(&arguments);
    assert_eq!(budgets.max_price_micros, Some(-1_000_000));
}

/// Reference `run_cli` prints a missing key after `Error: ` and exits 1,
/// the code every other programmatic failure exits with.
#[test]
fn a_missing_key_is_an_error_line_and_exit_one() {
    let error = CliError::MissingApiKey {
        variable: "MISTRAL_API_KEY".to_owned(),
        provider: "mistral".to_owned(),
    };
    assert!(
        error
            .to_string()
            .starts_with("Error: Missing MISTRAL_API_KEY ")
    );
    assert_eq!(error.exit_code(), 1);
}
