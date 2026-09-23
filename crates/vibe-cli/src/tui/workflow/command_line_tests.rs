//! What a submitted command line does, proved from
//! [`super::run_command`], which every command line the dispatcher routes to a
//! command reaches, and from [`crate::tui::submission::execute`], which is
//! where a refused line is answered.
//!
//! Reference `_handle_command` echoes the line and reports the registry key
//! once the command is known to run, and reference
//! `on_chat_input_container_submitted` refuses before either happens.

use std::path::Path;

use vibe_core::telemetry::TelemetryRecord;
use vibe_core::telemetry::records::TelemetryCommandKind;

use crate::Arguments;
use crate::tui::chat_input::ChatInputState;
use crate::tui::clipboard_images::ClipboardImageManager;
use crate::tui::commands::{COMMANDS, CommandContext};
use crate::tui::controls::ControlState;
use crate::tui::prompt::PromptContext;
use crate::tui::runtime::{
    InteractiveRuntime, interactive_test_runtime, interactive_test_runtime_with_server,
};
use crate::tui::state::{TranscriptKind, TuiState};
use crate::tui::submission::{Hint, Refusal, Route, execute};
use crate::tui::workflow::{FollowUp, LiveBackend, run_command};

fn arguments() -> Arguments {
    <Arguments as clap::Parser>::try_parse_from(["vibe"]).expect("interactive arguments")
}

/// Every command available, which is the context a session with Vibe Code and a
/// clipboard runs under.
fn full_context() -> CommandContext {
    CommandContext::new(true, true).with_clipboard_image_supported(true)
}

struct Dispatch {
    state: TuiState,
    composer: ChatInputState,
    follow_ups: Option<Vec<FollowUp>>,
}

impl Dispatch {
    fn echoes(&self) -> Vec<&str> {
        self.state
            .entries
            .iter()
            .filter(|entry| entry.kind == TranscriptKind::Command)
            .map(|entry| entry.text.as_str())
            .collect()
    }
}

/// One command line through the real entry point, with the runtime slot the
/// caller owns so a telemetry assertion can read what it was handed.
async fn dispatch(line: &str, runtime: &mut Option<InteractiveRuntime>) -> Dispatch {
    let arguments = arguments();
    let mut state = TuiState::new("command-line");
    let mut controls = ControlState::new("command-line");
    let mut composer = ChatInputState::new();
    composer.set_command_context(full_context());
    let mut theme = crate::tui::setup::resolve_theme(
        crate::tui::setup::Theme::Dark,
        crate::tui::setup::DetectedTheme::Dark,
        true,
    );
    let follow_ups = {
        let mut backend = LiveBackend::new(
            &arguments,
            Path::new("/workspace"),
            runtime,
            &mut state,
            &mut controls,
            &mut composer,
            &mut theme,
            false,
        );
        run_command(line, &full_context(), &mut backend).await
    };
    Dispatch {
        state,
        composer,
        follow_ups,
    }
}

/// The same line with no session behind it, which is enough for every
/// assertion that does not read telemetry.
async fn dispatch_without_runtime(line: &str) -> Dispatch {
    let mut runtime = None;
    dispatch(line, &mut runtime).await
}

/// The commands each event names, in the order they were reported.
fn builtin_commands(records: &[TelemetryRecord]) -> Vec<&str> {
    records
        .iter()
        .filter_map(|record| match record {
            TelemetryRecord::SlashCommandUsed {
                command,
                kind: TelemetryCommandKind::Builtin,
            } => Some(command.as_str()),
            _ => None,
        })
        .collect()
}

/// US-233: the line is echoed above whatever the command itself writes.
#[tokio::test]
async fn a_submitted_slash_line_is_echoed_with_its_arguments() {
    let dispatched = dispatch_without_runtime("/mcp add server").await;

    assert_eq!(dispatched.echoes(), vec!["mcp add server"]);
    assert_eq!(
        dispatched.state.entries[0].kind,
        TranscriptKind::Command,
        "the echo precedes the handler's own output"
    );
}

/// US-233: a bare alias is echoed as the key it resolved to, the way reference
/// `_handle_command` displays `parse_command`'s answer rather than the input.
#[tokio::test]
async fn a_bare_alias_is_echoed_as_its_registry_key() {
    let dispatched = dispatch_without_runtime(":q").await;

    assert_eq!(dispatched.echoes(), vec!["exit"]);
    assert_eq!(dispatched.follow_ups, Some(vec![FollowUp::Exit]));
}

/// US-233: exactly one leading slash is removed, and nothing is recased.
#[tokio::test]
async fn a_slash_line_keeps_the_case_the_operator_typed() {
    let dispatched = dispatch_without_runtime("/HELP").await;

    assert_eq!(dispatched.echoes(), vec!["HELP"]);
    assert!(
        dispatched
            .state
            .entries
            .iter()
            .any(|entry| entry.kind == TranscriptKind::Document),
        "the uppercase alias still resolves and mounts the help document"
    );
}

/// US-235: a refused line writes nothing to the transcript and goes back to
/// the composer, and the two refusals tell the operator two different things.
#[tokio::test]
async fn a_refused_line_is_restored_with_its_reason() {
    let mut messages = Vec::new();
    for (line, reason) in [
        ("/clear", Refusal::SlashCommand),
        ("&ship it", Refusal::Teleport),
    ] {
        for hint in [Hint::Busy, Hint::Paused] {
            let mut runtime = Some(interactive_test_runtime("refusal"));
            let mut active = None;
            let mut state = TuiState::new("refusal");
            let mut controls = ControlState::new("refusal");
            let mut images = ClipboardImageManager::default();
            let mut input = ChatInputState::new();
            execute(
                line.to_owned(),
                Route::Refuse { reason, hint },
                PromptContext::new(
                    Path::new("/workspace"),
                    &mut runtime,
                    &mut active,
                    &mut state,
                    &mut controls,
                    &mut images,
                ),
                &mut input,
            )
            .await
            .expect("the refusal is not an error");

            assert_eq!(input.editor().text(), line, "{hint:?} lost the line");
            assert_eq!(
                state.prompt_queue.len(),
                0,
                "{hint:?} queued a refused line"
            );
            assert!(
                state
                    .entries
                    .iter()
                    .all(|entry| entry.kind != TranscriptKind::Command),
                "{hint:?} echoed a refused line"
            );
            let reported = runtime
                .as_ref()
                .map(InteractiveRuntime::take_reported)
                .unwrap_or_default();
            assert!(builtin_commands(&reported).is_empty());
            messages.push(state.diagnostics().collect::<Vec<_>>().join(" "));
        }
    }
    for pair in messages.chunks(2) {
        assert!(pair[0].contains("finish"), "{}", pair[0]);
        assert!(
            pair[1].contains("clear") && pair[1].contains("remove"),
            "{}",
            pair[1]
        );
        assert_ne!(pair[0], pair[1]);
    }
}

/// US-233 and US-234: a line no command claims is left to the prompt path
/// untouched.
#[tokio::test]
async fn a_line_that_parses_to_no_command_echoes_and_reports_nothing() {
    let mut runtime = Some(interactive_test_runtime("no-command"));
    let dispatched = dispatch("write the tests", &mut runtime).await;

    assert_eq!(dispatched.follow_ups, None);
    assert!(dispatched.state.entries.is_empty());
    let reported = runtime
        .as_ref()
        .map(InteractiveRuntime::take_reported)
        .unwrap_or_default();
    assert!(builtin_commands(&reported).is_empty());
}

/// US-234: the event names the registry key, not the alias typed.
#[tokio::test]
async fn an_alias_reports_the_key_it_resolved_to() {
    let mut runtime = Some(interactive_test_runtime("alias-key"));
    for (line, key) in [
        ("/connectors", "mcp"),
        ("/new", "clear"),
        (":q", "exit"),
        ("/HELP", "help"),
    ] {
        dispatch(line, &mut runtime).await;
        let reported = runtime
            .as_ref()
            .map(InteractiveRuntime::take_reported)
            .unwrap_or_default();
        assert_eq!(
            builtin_commands(&reported),
            vec![key],
            "{line} did not report {key} exactly once"
        );
    }
}

/// US-234: `/exit` is reported like every other command, and reported once.
#[tokio::test]
async fn the_literal_exit_alias_reports_exactly_one_event() {
    let mut runtime = Some(interactive_test_runtime("exit-event"));
    let dispatched = dispatch("/exit", &mut runtime).await;

    assert_eq!(dispatched.follow_ups, Some(vec![FollowUp::Exit]));
    let reported = runtime
        .as_ref()
        .map(InteractiveRuntime::take_reported)
        .unwrap_or_default();
    assert_eq!(builtin_commands(&reported), vec!["exit"]);
}

/// US-234: the whole registry, through every alias it publishes, reports the
/// key that alias belongs to. A command whose alias set grows fails here until
/// it is dispatched too.
#[tokio::test]
async fn every_alias_of_every_command_reports_that_command_key() {
    let mut runtime = Some(interactive_test_runtime("alias-sweep"));
    let mut covered = 0_usize;
    for command in COMMANDS {
        for alias in command.aliases {
            dispatch(alias, &mut runtime).await;
            let reported = runtime
                .as_ref()
                .map(InteractiveRuntime::take_reported)
                .unwrap_or_default();
            assert_eq!(
                builtin_commands(&reported),
                vec![command.name],
                "{alias} did not report {}",
                command.name
            );
            covered = covered.saturating_add(1);
        }
    }
    assert!(covered > COMMANDS.len(), "every alias was dispatched");
}

/// US-233: `/clear` wipes the transcript it was echoed into, so reference
/// `_clear_history` re-mounts the line afterward, under the registry key. The
/// alias submitted is gone with everything else the reset took.
#[tokio::test(flavor = "multi_thread")]
async fn clearing_the_history_re_mounts_the_command_line_it_erased() {
    // A clear rotates the stored session, so this one needs a session root it
    // can actually write, which the default fixture has no home for.
    let temporary = tempfile::tempdir().expect("a temporary vibe home");
    let vibe_home = temporary.path().join("vibe-home");
    std::fs::create_dir_all(&vibe_home).expect("the vibe home is created");
    let session_root = vibe_home.join("sessions");
    // The clear rotates a stored session, so one has to exist under the id the
    // runtime attaches to.
    vibe_core::storage::SessionStore::new(session_root.clone())
        .create(
            "clear-echo",
            &temporary.path().join("workspace").to_string_lossy(),
            None,
            1,
        )
        .expect("the session is stored");
    let workspace = vibe_app_server::workspace::WorkspaceService::new(
        vibe_app_server::workspace::WorkspacePaths {
            session_root,
            working_directory: temporary.path().join("workspace"),
            vibe_home,
        },
        true,
    )
    .expect("the workspace service builds");
    let mut runtime = Some(interactive_test_runtime_with_server(
        "clear-echo",
        vibe_app_server::server::AppServer::with_workspace_service(workspace),
    ));
    let dispatched = dispatch("/new", &mut runtime).await;

    assert_eq!(
        dispatched.echoes(),
        vec!["clear"],
        "the cleared transcript holds the re-mounted key alone"
    );
    assert!(dispatched.composer.editor().text().is_empty());
    assert_ne!(
        runtime.as_ref().map(|runtime| runtime.session_id.as_str()),
        Some("clear-echo"),
        "the clear continues under a new session id"
    );
}
