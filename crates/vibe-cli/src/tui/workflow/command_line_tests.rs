//! What a submitted command line does, proved from
//! [`super::dispatch_command`], which is the entry point every submitted line
//! reaches, and from [`crate::tui::submission::execute`], which is where a
//! teleport line refuses.
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
use crate::tui::submission::{Availability, execute};
use crate::tui::workflow::{CommandAction, dispatch_command};

fn arguments() -> Arguments {
    <Arguments as clap::Parser>::try_parse_from(["vibe"]).expect("interactive arguments")
}

/// Every command available, which is the context a session with Vibe Code and a
/// clipboard runs under.
fn full_context() -> CommandContext {
    CommandContext::new(true).with_clipboard_image_supported(true)
}

struct Dispatch {
    state: TuiState,
    composer: ChatInputState,
    action: CommandAction,
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

    fn diagnostics(&self) -> Vec<&str> {
        self.state.diagnostics().collect()
    }
}

/// One submission through the real entry point, with the runtime slot the
/// caller owns so a telemetry assertion can read what it was handed.
async fn dispatch(
    line: &str,
    availability: Availability,
    runtime: &mut Option<InteractiveRuntime>,
) -> Dispatch {
    let arguments = arguments();
    let mut state = TuiState::new("command-line");
    let mut composer = ChatInputState::new();
    composer.set_command_context(full_context());
    let action = dispatch_command(
        line,
        &arguments,
        Path::new("/workspace"),
        runtime,
        &mut state,
        &mut composer,
        availability,
    )
    .await;
    Dispatch {
        state,
        composer,
        action,
    }
}

/// The same submission with no session behind it, which is enough for every
/// assertion that does not read telemetry.
async fn dispatch_without_runtime(line: &str, availability: Availability) -> Dispatch {
    let mut runtime = None;
    dispatch(line, availability, &mut runtime).await
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
    let dispatched = dispatch_without_runtime("/mcp add server", Availability::Idle).await;

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
    let dispatched = dispatch_without_runtime(":q", Availability::Idle).await;

    assert_eq!(dispatched.echoes(), vec!["exit"]);
    assert!(matches!(dispatched.action, CommandAction::Exit));
}

/// US-233: exactly one leading slash is removed, and nothing is recased.
#[tokio::test]
async fn a_slash_line_keeps_the_case_the_operator_typed() {
    let dispatched = dispatch_without_runtime("/HELP", Availability::Idle).await;

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

/// US-233 and US-235: a refused command writes nothing to the transcript and
/// answers the action `submit` restores the draft on. The restoration itself
/// belongs to that caller, which routes `Rejected` into
/// [`crate::tui::submission::restore_draft`]; the teleport case below proves
/// what that function does with the line it is handed.
#[tokio::test]
async fn a_refused_command_echoes_nothing_and_is_answered_as_rejected() {
    for availability in [Availability::Busy, Availability::QueuePaused] {
        let dispatched = dispatch_without_runtime("/clear", availability).await;

        assert!(matches!(dispatched.action, CommandAction::Rejected));
        assert!(
            dispatched.state.entries.is_empty(),
            "{availability:?} appended a transcript entry"
        );
        assert!(
            dispatched.composer.editor().text().is_empty(),
            "{availability:?} wrote to the composer the caller owns"
        );
    }
}

/// US-235: the two refusals tell the operator two different things.
#[tokio::test]
async fn the_two_refusals_state_their_two_reasons() {
    let busy = dispatch_without_runtime("/clear", Availability::Busy).await;
    let paused = dispatch_without_runtime("/clear", Availability::QueuePaused).await;

    let busy_message = busy.diagnostics().join(" ");
    let paused_message = paused.diagnostics().join(" ");
    assert!(
        busy_message.contains("finish"),
        "{busy_message} does not say the current job has to finish"
    );
    assert!(
        paused_message.contains("clear") && paused_message.contains("remove"),
        "{paused_message} does not say the queue has to be cleared or the input removed"
    );
    assert_ne!(
        busy_message, paused_message,
        "both refusals gave the same reason"
    );
}

/// US-233 and US-234: a line no command claims is left to the prompt path
/// untouched.
#[tokio::test]
async fn a_line_that_parses_to_no_command_echoes_and_reports_nothing() {
    let mut runtime = Some(interactive_test_runtime("no-command"));
    let dispatched = dispatch("write the tests", Availability::Idle, &mut runtime).await;

    assert!(matches!(dispatched.action, CommandAction::Unhandled));
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
        dispatch(line, Availability::Idle, &mut runtime).await;
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

/// US-234: `/exit` no longer bypasses dispatch, so it is reported like every
/// other command and reported once.
#[tokio::test]
async fn the_literal_exit_alias_reports_exactly_one_event() {
    let mut runtime = Some(interactive_test_runtime("exit-event"));
    let dispatched = dispatch("/exit", Availability::Idle, &mut runtime).await;

    assert!(matches!(dispatched.action, CommandAction::Exit));
    let reported = runtime
        .as_ref()
        .map(InteractiveRuntime::take_reported)
        .unwrap_or_default();
    assert_eq!(builtin_commands(&reported), vec!["exit"]);
}

/// US-234: a refused command reports nothing, because the refusal happens
/// before the command is resolved to a run.
#[tokio::test]
async fn a_refused_command_reports_nothing() {
    let mut runtime = Some(interactive_test_runtime("refused-event"));
    for availability in [Availability::Busy, Availability::QueuePaused] {
        dispatch("/clear", availability, &mut runtime).await;
        let reported = runtime
            .as_ref()
            .map(InteractiveRuntime::take_reported)
            .unwrap_or_default();
        assert!(
            builtin_commands(&reported).is_empty(),
            "{availability:?} reported an event for a command that never ran"
        );
    }
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
            dispatch(alias, Availability::Idle, &mut runtime).await;
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
    assert_eq!(
        COMMANDS.len(),
        28,
        "the registry the sweep walks is the one the reference publishes"
    );
    assert!(covered > COMMANDS.len(), "every alias was dispatched");
}

/// US-235: a teleport line refuses through the same two reasons, and gives the
/// draft back rather than queueing it.
#[tokio::test]
async fn a_teleport_line_carries_the_same_two_reasons() {
    let mut messages = Vec::new();
    for availability in [Availability::Busy, Availability::QueuePaused] {
        let mut runtime = Some(interactive_test_runtime("teleport-refusal"));
        let mut active = None;
        let mut state = TuiState::new("teleport-refusal");
        let mut controls = ControlState::new("teleport-refusal");
        let mut images = ClipboardImageManager::default();
        let mut input = ChatInputState::new();
        execute(
            "&ship it".to_owned(),
            availability,
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

        assert_eq!(
            input.editor().text(),
            "&ship it",
            "{availability:?} did not restore the submitted line"
        );
        assert_eq!(
            state.prompt_queue.len(),
            0,
            "{availability:?} queued a refused teleport"
        );
        messages.push(state.diagnostics().collect::<Vec<_>>().join(" "));
    }
    assert!(messages[0].contains("finish"), "{}", messages[0]);
    assert!(
        messages[1].contains("clear") && messages[1].contains("remove"),
        "{}",
        messages[1]
    );
    assert_ne!(messages[0], messages[1]);
}

/// US-233: `/clear` wipes the transcript it was echoed into, so reference
/// `_clear_history` re-mounts the line afterward, under the registry key. The
/// alias submitted is gone with everything else the reset took.
#[tokio::test(flavor = "multi_thread")]
async fn clearing_the_history_re_mounts_the_command_line_it_erased() {
    // A clear rewinds the stored session, so this one needs a session root it
    // can actually write, which the default fixture has no home for.
    let temporary = tempfile::tempdir().expect("a temporary vibe home");
    let vibe_home = temporary.path().join("vibe-home");
    std::fs::create_dir_all(&vibe_home).expect("the vibe home is created");
    let session_root = vibe_home.join("sessions");
    // The clear rewinds a stored session, so one has to exist under the id the
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
    let dispatched = dispatch("/new", Availability::Idle, &mut runtime).await;
    assert_eq!(dispatched.echoes(), vec!["new"]);
    assert!(matches!(
        dispatched.action,
        CommandAction::Runtime(crate::tui::workflow::RuntimeCommand::Clear)
    ));

    let mut state = dispatched.state;
    let mut controls = ControlState::new("clear-echo");
    let mut composer = dispatched.composer;
    let mut theme = crate::tui::setup::resolve_theme(
        crate::tui::setup::Theme::Dark,
        crate::tui::setup::DetectedTheme::Dark,
        true,
    );
    crate::tui::workflow::handle_runtime_command(
        &crate::tui::workflow::RuntimeCommand::Clear,
        Path::new("/workspace"),
        &mut runtime,
        &mut state,
        &mut controls,
        &mut composer,
        &mut theme,
        false,
    )
    .await;

    let echoes = state
        .entries
        .iter()
        .filter(|entry| entry.kind == TranscriptKind::Command)
        .map(|entry| entry.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        echoes,
        vec!["clear"],
        "the cleared transcript holds the re-mounted key alone"
    );
}
