use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use vibe_acp::{
    ACP_PROTOCOL_VERSION, AcpAgent, AcpClientCapabilities, AcpClientFuture, AcpClientPort,
    AcpFilesystemCapabilities, AcpForkSession, AcpInitializeRequest, AcpLoadSession, AcpNewSession,
};
use vibe_app_server::client::{
    DriverFuture, EchoTurnDriver, PublicContentBlock, TurnDriver, TurnReservation,
};
use vibe_app_server::release4::{
    CloudError, GitProbe, GitSnapshot, Project, ProjectCloud, ProjectPage, ProjectRepository,
    Release4Error, Release4Service, TeleportCloud, TeleportStartRequest,
};
use vibe_cli::tui::controls::{
    ApprovalScope, CallbackChoice, CallbackKind, ControlState, PendingCallback, SessionCommand,
};
use vibe_cli::tui::input::{
    ClipboardPort, CompletionCandidate, CompletionEngine, CompletionKind, ExternalEditorPort,
    InputError, PromptEditor, prepare_submission,
};
use vibe_cli::tui::render::{RenderLimits, draw, sanitize_terminal};
use vibe_cli::tui::setup::{
    CredentialError, CredentialStore, DetectedTheme, SetupError, SetupState, Theme, UpdateState,
    VoiceState, detect_terminal_theme, resolve_theme,
};
use vibe_cli::tui::state::{
    EntryStatus, EventMailbox, ServerEvent, TranscriptEntry, TranscriptKind, TuiState,
};
use vibe_cli::tui::terminal::{TerminalGuard, TerminalOps, TerminalStep};

pub(crate) fn tui_terminal_stack_contract() -> Result<Value, String> {
    let (ops, transcript) = recording_terminal_ops(None, None);
    let mut guard = TerminalGuard::enter(ops).map_err(|error| error.to_string())?;
    let entered = terminal_transcript(&transcript)?;
    guard.restore().map_err(|error| error.to_string())?;
    let restored = guard.is_restored();
    guard.resume().map_err(|error| error.to_string())?;
    let resumed = !guard.is_restored();
    drop(guard);
    let complete = terminal_transcript(&transcript)?;

    let (partial_ops, partial_transcript) =
        recording_terminal_ops(Some(TerminalStep::MouseCapture), None);
    let partial_error = TerminalGuard::enter(partial_ops)
        .err()
        .ok_or_else(|| "terminal partial-enter fixture unexpectedly succeeded".to_owned())?;
    let partial = terminal_transcript(&partial_transcript)?;

    let (restore_ops, restore_transcript) =
        recording_terminal_ops(None, Some(TerminalStep::MouseCapture));
    let mut restore_guard = TerminalGuard::enter(restore_ops).map_err(|error| error.to_string())?;
    let restore_error = restore_guard
        .restore()
        .err()
        .ok_or_else(|| "terminal restore fixture unexpectedly succeeded".to_owned())?;
    let restored_after_failure = restore_guard.is_restored();
    let sticky_restore_error = restore_guard.restore().is_err();
    drop(restore_guard);
    let expected_enter = [
        "enter:raw_mode",
        "enter:alternate_screen",
        "enter:mouse_capture",
        "enter:bracketed_paste",
        "enter:cursor_hidden",
    ];
    let expected_leave = [
        "leave:cursor_hidden",
        "leave:bracketed_paste",
        "leave:mouse_capture",
        "leave:alternate_screen",
        "leave:raw_mode",
    ];
    let expected_lifecycle = expected_enter
        .iter()
        .chain(expected_leave.iter())
        .chain(expected_enter.iter())
        .chain(expected_leave.iter())
        .copied()
        .collect::<Vec<_>>();
    let terminal = Terminal::new(TestBackend::new(72, 18)).map_err(|error| error.to_string())?;
    let size = terminal.size().map_err(|error| error.to_string())?;
    let mut editor = PromptEditor::default();
    editor.insert("β");
    let partial_rollback = partial
        == [
            "enter:raw_mode",
            "enter:alternate_screen",
            "enter:mouse_capture",
            "leave:alternate_screen",
            "leave:raw_mode",
        ];
    let clean_shutdown = entered == expected_enter
        && complete == expected_lifecycle
        && partial_error.cleanup.is_empty()
        && partial_error.step == TerminalStep::MouseCapture
        && partial_rollback
        && terminal_transcript(&restore_transcript)?.len() == 10
        && restore_error.failures.len() == 1
        && restored_after_failure
        && sticky_restore_error;

    Ok(json!({
        "headless": {
            "mounted": entered == expected_enter,
            "mouse": entered.iter().any(|step| step == "enter:mouse_capture"),
            "resize": [size.width, size.height],
            "unicode": editor.text() == "β",
        },
        "lifecycle": {
            "cleanShutdown": clean_shutdown,
            "restartable": restored && resumed,
        },
    }))
}

pub(crate) fn tui_shell_contract() -> Result<Value, String> {
    let mut state = TuiState::new("session");
    let later_batch = vec![
        transcript_entry(
            "h2",
            1,
            TranscriptKind::AssistantMessage,
            "two",
            EntryStatus::Completed,
            Value::Null,
        ),
        transcript_entry(
            "h3",
            1,
            TranscriptKind::AssistantMessage,
            "three",
            EntryStatus::Completed,
            Value::Null,
        ),
    ];
    let earlier_batch = vec![
        transcript_entry(
            "h0",
            1,
            TranscriptKind::UserMessage,
            "zero",
            EntryStatus::Completed,
            Value::Null,
        ),
        transcript_entry(
            "h1",
            1,
            TranscriptKind::AssistantMessage,
            "one",
            EntryStatus::Completed,
            Value::Null,
        ),
    ];
    let batch_sizes = [earlier_batch.len(), later_batch.len()];
    state
        .prepend_history(later_batch, Some("earlier".to_owned()))
        .map_err(|error| error.to_string())?;
    state
        .prepend_history(earlier_batch, None)
        .map_err(|error| error.to_string())?;
    let entry_ids = state
        .entries
        .iter()
        .map(|entry| entry.id.as_str())
        .collect::<Vec<_>>();
    let mut snapshot = state.entries.clone();
    snapshot.clear();
    let snapshot_isolated = state.entries.len() == 4;

    let mut mailbox = EventMailbox::bounded(3, (80, 24));
    mailbox
        .try_send(ServerEvent::Ready)
        .map_err(|error| error.to_string())?;
    mailbox
        .try_send(ServerEvent::Waiting {
            event_id: 1,
            waiting: true,
        })
        .map_err(|error| error.to_string())?;
    mailbox
        .try_send(ServerEvent::TransportLost("closed".to_owned()))
        .map_err(|error| error.to_string())?;
    let fifo = matches!(mailbox.receiver.try_recv(), Ok(ServerEvent::Ready))
        && matches!(
            mailbox.receiver.try_recv(),
            Ok(ServerEvent::Waiting {
                event_id: 1,
                waiting: true,
            })
        )
        && matches!(
            mailbox.receiver.try_recv(),
            Ok(ServerEvent::TransportLost(reason)) if reason == "closed"
        );
    let clear_resumes = mailbox.try_send(ServerEvent::Ready).is_ok();

    Ok(json!({
        "history": {
            "backfillOrdered": entry_ids == ["h0", "h1", "h2", "h3"],
            "batchSizes": batch_sizes,
            "exhausted": state.cursor_before.is_none(),
        },
        "queue": {
            "clearResumes": clear_resumes,
            "fifo": fifo,
            "snapshotIsolated": snapshot_isolated,
        },
    }))
}

pub(crate) fn tui_rendering_contract() -> Result<Value, String> {
    let mut state = TuiState::new("session");
    state.ready = true;
    state.watermark = 7;
    state.entries = vec![transcript_entry(
        "answer",
        1,
        TranscriptKind::AssistantMessage,
        "answer\n\u{1b}[31munsafe",
        EntryStatus::Completed,
        json!({
            "diff": ["-old", "+new"],
            "presentationKind": "diff",
        }),
    )];
    let mut editor = PromptEditor::default();
    editor.set_text("draft input");
    let backend = TestBackend::new(72, 18);
    let mut terminal = Terminal::new(backend).map_err(|error| error.to_string())?;
    let theme = resolve_theme(Theme::System, DetectedTheme::Dark, true);
    terminal
        .draw(|frame| draw(frame, &mut state, &editor, theme, false))
        .map_err(|error| error.to_string())?;
    let snapshot = test_backend_text(&terminal);
    let markup_literal = sanitize_terminal(
        "[/]",
        RenderLimits {
            max_chars: 64,
            max_line_chars: 64,
        },
    ) == "[/]";

    Ok(json!({
        "diff": {
            "added": snapshot.contains("+new"),
            "removed": snapshot.contains("-old"),
        },
        "hostileContent": {
            "markupLiteral": markup_literal,
            "terminalControlsSafe": !snapshot.contains('\u{1b}')
                && snapshot.contains("␛[31munsafe"),
        },
    }))
}

pub(crate) fn tui_input_contract() -> Result<Value, String> {
    let mut editor = PromptEditor::default();
    editor.insert("a");
    editor.insert("e\u{301}");
    editor.insert("\nβ");
    editor.move_left(false);
    editor.move_left(false);
    editor.delete_backward();
    editor
        .submit()
        .ok_or_else(|| "Unicode prompt unexpectedly submitted as empty".to_owned())?;
    editor.set_text("draft");
    editor.history_previous();
    let previous = editor.text().to_owned();
    editor.history_next();
    let restored_draft = editor.text().to_owned();

    let mut completion = CompletionEngine::default();
    let candidates = vec![
        CompletionCandidate {
            id: "skill".to_owned(),
            kind: CompletionKind::Skill,
            label: "review".to_owned(),
            insertion: "/review".to_owned(),
        },
        CompletionCandidate {
            id: "command".to_owned(),
            kind: CompletionKind::SlashCommand,
            label: "reload".to_owned(),
            insertion: "/reload".to_owned(),
        },
    ];
    let stale = completion.complete("re", candidates.clone());
    let current = completion.complete("rev", candidates);
    let mut completed_editor = PromptEditor::default();
    let stale_ignored = matches!(
        completion.apply(&mut completed_editor, &stale, 0),
        Err(InputError::StaleCompletion)
    ) && completed_editor.text().is_empty();
    let tab_handled = completion.apply(&mut completed_editor, &current, 0).is_ok();

    let workspace = tempfile::tempdir().map_err(|error| error.to_string())?;
    let image_path = workspace.path().join("image.png");
    fs::write(&image_path, [0_u8, 1, 2]).map_err(|error| error.to_string())?;
    let direct_image =
        prepare_submission(workspace.path(), "@image.png").map_err(|error| error.to_string())?;
    let image_path_rewritten = direct_image
        .turn
        .input
        .iter()
        .any(|block| matches!(block, PublicContentBlock::Image { .. }));

    let mut clipboard = MemoryClipboard::default();
    clipboard
        .write_text("copied")
        .map_err(|error| error.to_string())?;
    let clipboard_round_trip = clipboard.read_text()?;
    let empty_rejected = MemoryClipboard::default().write_text("").is_err();
    let mut external_editor = FixtureEditor;
    let external_editor_result = external_editor.edit("draft")?;

    Ok(json!({
        "clipboard": {
            "copyAccepted": clipboard_round_trip == "copied",
            "emptyRejected": empty_rejected,
        },
        "completion": {
            "applied": completed_editor.text(),
            "staleIgnored": stale_ignored,
            "tabHandled": tab_handled,
        },
        "externalEditor": external_editor_result,
        "history": {
            "draftRestored": restored_draft,
            "previous": previous,
            "unicodePreserved": previous == "a\nβ",
        },
        "imagePathRewritten": image_path_rewritten,
    }))
}

pub(crate) fn tui_controls_contract() -> Result<Value, String> {
    let pending = |callback_id: &str| PendingCallback {
        callback_id: callback_id.to_owned(),
        session_id: "session".to_owned(),
        turn_id: "turn".to_owned(),
        kind: CallbackKind::Approval,
        prompt: "Run command?".to_owned(),
        options: Vec::new(),
        allows_free_text: false,
        multi_select: false,
        questions: Vec::new(),
    };

    let mut controls = ControlState::new("session");
    controls
        .begin_turn("turn")
        .map_err(|error| error.to_string())?;
    controls
        .present_callback(pending("approval"))
        .map_err(|error| error.to_string())?;
    let first = controls
        .answer(
            "turn",
            "approval",
            CallbackChoice::Approve {
                scope: ApprovalScope::Once,
            },
        )
        .map_err(|error| error.to_string())?;

    let mut races = ControlState::new("session");
    races
        .begin_turn("turn")
        .map_err(|error| error.to_string())?;
    races
        .present_callback(pending("first"))
        .map_err(|error| error.to_string())?;
    let overlap_queued = races.present_callback(pending("second")).is_ok();
    races.interrupt().map_err(|error| error.to_string())?;
    let clears_callbacks = races
        .answer(
            "turn",
            "first",
            CallbackChoice::Approve {
                scope: ApprovalScope::Once,
            },
        )
        .is_err()
        && races
            .answer(
                "turn",
                "second",
                CallbackChoice::Approve {
                    scope: ApprovalScope::Once,
                },
            )
            .is_err();

    let commands = [
        ("clear", SessionCommand::Clear, "session/history/clear"),
        ("compact", SessionCommand::Compact, "session/compact/start"),
        ("continue", SessionCommand::Continue, "session/continue"),
        ("rename", SessionCommand::Rename, "session/title/update"),
        ("resume", SessionCommand::Resume, "session/resume"),
        ("rewind", SessionCommand::Rewind, "session/rewind"),
    ]
    .into_iter()
    .map(|(name, command, method)| {
        (
            name.to_owned(),
            controls.session_command(command).method == method,
        )
    })
    .collect::<BTreeMap<_, _>>();
    let decision = first
        .params
        .pointer("/output/decision/type")
        .and_then(Value::as_str)
        .unwrap_or("missing");
    let kind = first
        .params
        .pointer("/output/type")
        .and_then(Value::as_str)
        .unwrap_or("missing");

    Ok(json!({
        "approval": {
            "decision": decision,
            "kind": kind,
        },
        "callbackRaces": {
            "overlapQueued": overlap_queued,
        },
        "commands": commands,
        "interrupt": {
            "clearsCallbacks": clears_callbacks,
        },
    }))
}

pub(crate) fn tui_setup_contract() -> Result<Value, String> {
    let invalid_falls_back =
        detect_terminal_theme(Some("not-a-theme"), Some("15")) == DetectedTheme::Light;

    let credentials = MemoryCredentials::default();
    let mut setup = SetupState::default();
    setup
        .authenticate(&credentials, "fixture-account", "never-project-this")
        .map_err(|error| error.to_string())?;
    setup.workspace_trusted = true;
    let resources = setup
        .complete(&credentials)
        .map_err(|error| error.to_string())?;
    let projected = serde_json::to_string(&resources).map_err(|error| error.to_string())?;
    credentials
        .delete("fixture-account")
        .map_err(|error| error.to_string())?;
    let missing_auth_rejected = matches!(
        setup.complete(&credentials),
        Err(SetupError::AuthenticationRequired)
    );

    let explicit_theme = resolve_theme(Theme::Dark, DetectedTheme::Light, false);
    let mut update = UpdateState::Checking;
    update.fail();
    let update_failure_is_recoverable = matches!(
        update,
        UpdateState::Failed { ref message }
            if message == "Update check failed; the installed version remains usable"
    );
    let devices = vec!["default".to_owned(), "microphone".to_owned()];
    let mut voice = VoiceState::Idle;
    voice
        .start(Some("microphone"), &devices)
        .map_err(|error| error.to_string())?;
    let recording_started = matches!(voice, VoiceState::Recording { .. });
    voice.cancel();
    let cancel_safe = matches!(voice, VoiceState::Cancelled);

    Ok(json!({
        "authentication": {
            "missingRejected": missing_auth_rejected,
            "processCredentialExternal": !projected.contains("never-project-this"),
            "workspaceTrustDecision": if resources.workspace_trusted {
                "trust_cwd"
            } else {
                "decline"
            },
        },
        "theme": {
            "explicitPreserved": explicit_theme.theme == Theme::Dark,
            "invalidFallsBack": invalid_falls_back,
        },
        "updateFailureRecoverable": update_failure_is_recoverable,
        "voice": {
            "cancelSafe": cancel_safe,
            "recordingStarted": recording_started,
        },
    }))
}

pub(crate) fn acp_full_contract() -> Result<Value, String> {
    run_async(acp_full_contract_async())
}

pub(crate) fn cloud_workflows_contract() -> Result<Value, String> {
    let temporary = tempfile::tempdir().map_err(|error| error.to_string())?;
    let project_link_store = temporary.path().join("project-links.json");
    fs::write(
        &project_link_store,
        serde_json::to_vec_pretty(&json!({
            "/workspace/repo": {
                "repoUrl": "https://github.com/previous/repository.git",
                "projectId": "stale-project",
                "projectName": "stale project",
            },
        }))
        .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let pushed = Arc::new(AtomicBool::new(false));
    let git = Arc::new(FixtureGit {
        snapshot: GitSnapshot {
            repository: "fixture".to_owned(),
            dirty: true,
            unpushed: true,
        },
        pushed: pushed.clone(),
        push_fails: false,
    });
    let service = Release4Service::with_backends(
        Arc::new(FixtureProjects),
        Arc::new(FixtureTeleport { fail: false }),
        git,
    )
    .with_loop_store(temporary.path().join("loops.json"))
    .and_then(|service| service.with_project_link_store(project_link_store))
    .map_err(|error| error.to_string())?;

    let pending = service
        .dispatch(
            "vibeCode/projects/open",
            &object_params(json!({
                "sessionId": "session-pending",
                "workingDirectory": "/workspace/repo",
                "purpose": "configure",
            }))?,
        )
        .map_err(|error| error.to_string())?;
    let pending_picker = string_field(&pending.result, "pickerId")?;
    let source = service
        .dispatch(
            "vibeCode/projects/open",
            &object_params(json!({
                "sessionId": "session-source",
                "workingDirectory": "/workspace/repo",
                "purpose": "configure",
            }))?,
        )
        .map_err(|error| error.to_string())?;
    let source_picker = string_field(&source.result, "pickerId")?;
    let read_only_rejected = matches!(
        service.dispatch(
            "vibeCode/projects/select",
            &object_params(json!({
                "sessionId": "session-source",
                "pickerId": source_picker,
                "projectId": "read-only-first",
            }))?,
        ),
        Err(Release4Error::InvalidParams(message)) if message.contains("read-only")
    );
    service
        .dispatch(
            "vibeCode/projects/select",
            &object_params(json!({
                "sessionId": "session-source",
                "pickerId": source_picker,
                "projectId": "page-first",
            }))?,
        )
        .map_err(|error| error.to_string())?;
    let recovered = service
        .dispatch(
            "vibeCode/projects/recover",
            &object_params(json!({
                "sessionId": "session-pending",
                "pickerId": pending_picker,
            }))?,
        )
        .map_err(|error| error.to_string())?;
    let linked = service
        .dispatch(
            "vibeCode/projects/open",
            &object_params(json!({
                "sessionId": "session-linked",
                "workingDirectory": "/workspace/repo",
                "purpose": "configure",
            }))?,
        )
        .map_err(|error| error.to_string())?;
    let linked_picker = string_field(&linked.result, "pickerId")?;

    let teleport = service
        .dispatch(
            "vibeCode/teleport/start",
            &object_params(json!({
                "sessionId": "session-linked",
                "pickerId": linked_picker,
                "operationId": "teleport-main",
                "projectId": "page-first",
                "workingDirectory": "/workspace/repo",
                "prompt": "continue",
            }))?,
        )
        .map_err(|error| error.to_string())?;
    let mut approval_events = notification_kinds(&teleport);
    let approved = service
        .dispatch(
            "vibeCode/teleport/push/respond",
            &object_params(json!({
                "sessionId": "session-linked",
                "operationId": "teleport-main",
                "approved": true,
            }))?,
        )
        .map_err(|error| error.to_string())?;
    approval_events.extend(notification_kinds(&approved));

    let cancelled_start = service
        .dispatch(
            "vibeCode/teleport/start",
            &object_params(json!({
                "sessionId": "session-linked",
                "pickerId": linked_picker,
                "operationId": "teleport-cancelled",
                "projectId": "page-first",
            }))?,
        )
        .map_err(|error| error.to_string())?;
    let cancel_operation = string_field(&cancelled_start.result, "operationId")?;
    let cancelled = service
        .dispatch(
            "vibeCode/teleport/cancel",
            &object_params(json!({
                "sessionId": "session-linked",
                "operationId": cancel_operation,
            }))?,
        )
        .map_err(|error| error.to_string())?;
    let mut cancel_events = notification_kinds(&cancelled_start);
    cancel_events.extend(notification_kinds(&cancelled));
    let cancel_succeeded = cancelled.result["cancelled"] == json!(true);

    let created_loop = service
        .dispatch(
            "loops/create",
            &object_params(json!({
                "sessionId": "session-linked",
                "prompt": "review",
                "interval": "30s",
                "nowSeconds": 100,
            }))?,
        )
        .map_err(|error| error.to_string())?;
    let loop_id = created_loop.result["loop"]["id"]
        .as_str()
        .ok_or_else(|| "loop creation omitted loop ID".to_owned())?
        .to_owned();
    let early_fire_rejected = service.fire_loop(&loop_id, 129, true).is_err();
    let fired = service
        .fire_loop(&loop_id, 130, true)
        .map_err(|error| error.to_string())?;
    service
        .finish_loop_fire(&loop_id, 131)
        .map_err(|error| error.to_string())?;
    let due_after_interval = service
        .next_due_loop_id("session-linked", 160)
        .map_err(|error| error.to_string())?
        .as_deref()
        == Some(loop_id.as_str());
    service
        .rebind_session("session-linked", "session-rebound")
        .map_err(|error| error.to_string())?;
    service
        .close_transient_session("session-rebound")
        .map_err(|error| error.to_string())?;
    let rebound = service
        .dispatch(
            "loops/list",
            &object_params(json!({"sessionId": "session-rebound"}))?,
        )
        .map_err(|error| error.to_string())?;
    let rebound_count = array_len(&rebound.result["loops"]);
    let rearmed_after_seconds = due_after_interval
        .then(|| {
            rebound.result["loops"][0]["nextFireAt"]
                .as_f64()
                .map(|value| value as i64 - 130)
        })
        .flatten();
    let mut mutated_snapshot = rebound.result.clone();
    if let Some(loops) = mutated_snapshot
        .get_mut("loops")
        .and_then(Value::as_array_mut)
    {
        loops.clear();
    }
    let fresh = service
        .dispatch(
            "loops/list",
            &object_params(json!({"sessionId": "session-rebound"}))?,
        )
        .map_err(|error| error.to_string())?;
    let list_snapshot_isolated = array_len(&mutated_snapshot["loops"]) == 0
        && array_len(&fresh.result["loops"]) == rebound_count;
    let restarted = Release4Service::with_backends(
        Arc::new(FixtureProjects),
        Arc::new(FixtureTeleport { fail: false }),
        Arc::new(FixtureGit {
            snapshot: GitSnapshot {
                repository: "fixture".to_owned(),
                dirty: false,
                unpushed: false,
            },
            pushed: Arc::new(AtomicBool::new(false)),
            push_fails: false,
        }),
    )
    .with_loop_store(temporary.path().join("loops.json"))
    .map_err(|error| error.to_string())?;
    let persisted = restarted
        .dispatch(
            "loops/list",
            &object_params(json!({"sessionId": "session-rebound"}))?,
        )
        .map_err(|error| error.to_string())?;

    let failing_service = Release4Service::with_backends(
        Arc::new(FixtureProjects),
        Arc::new(FixtureTeleport { fail: true }),
        Arc::new(FixtureGit {
            snapshot: GitSnapshot {
                repository: "fixture".to_owned(),
                dirty: false,
                unpushed: false,
            },
            pushed: Arc::new(AtomicBool::new(false)),
            push_fails: false,
        }),
    )
    .with_loop_store(temporary.path().join("failing-loops.json"))
    .map_err(|error| error.to_string())?;
    let failed_picker = failing_service
        .dispatch(
            "vibeCode/projects/open",
            &object_params(json!({
                "sessionId": "session-failure",
                "workingDirectory": "/workspace/failure",
                "purpose": "configure",
            }))?,
        )
        .map_err(|error| error.to_string())?;
    let failed_picker_id = string_field(&failed_picker.result, "pickerId")?;
    failing_service
        .dispatch(
            "vibeCode/projects/select",
            &object_params(json!({
                "sessionId": "session-failure",
                "pickerId": failed_picker_id,
                "projectId": "page-first",
            }))?,
        )
        .map_err(|error| error.to_string())?;
    let failed_teleport = failing_service
        .dispatch(
            "vibeCode/teleport/start",
            &object_params(json!({
                "sessionId": "session-failure",
                "pickerId": failed_picker_id,
                "operationId": "teleport-failed",
                "projectId": "page-first",
            }))?,
        )
        .map_err(|error| error.to_string())?;
    let recovery_after_failure = failing_service
        .dispatch(
            "vibeCode/projects/recover",
            &object_params(json!({
                "sessionId": "session-failure",
                "pickerId": failed_picker_id,
            }))?,
        )
        .map_err(|error| error.to_string())?;
    let execution_idle = failing_service
        .dispatch(
            "vibeCode/teleport/start",
            &object_params(json!({
                "sessionId": "session-failure",
                "pickerId": failed_picker_id,
                "operationId": "teleport-retry",
                "projectId": "page-first",
            }))?,
        )
        .is_ok();

    Ok(json!({
        "failureSafety": {
            "executionIdle": execution_idle,
            "projectLinkPreserved": recovery_after_failure.result["recovered"] == json!(true),
            "teleportEvents": notification_kinds(&failed_teleport),
        },
        "loops": {
            "earlyDue": !early_fire_rejected,
            "firedAtDue": fired.notice.method == "history/entryAdded",
            "listSnapshotIsolated": list_snapshot_isolated,
            "persistedCount": rebound_count,
            "rearmedAfterSeconds": rearmed_after_seconds,
            "restoredCount": array_len(&persisted.result["loops"]),
        },
        "projects": {
            "readOnlyRejected": read_only_rejected,
            "resolvedProjectId": linked.result["resolvedProjectId"],
            "staleLinkCleared": pending.result["view"]["savedProjectLinkCleared"] == json!(true)
                && recovered.result["recovered"] == json!(true),
        },
        "teleport": {
            "approvalEvents": approval_events,
            "cancelEvents": cancel_events,
            "cancelled": cancel_succeeded,
            "pushApproved": pushed.load(Ordering::Relaxed),
        },
    }))
}

async fn acp_full_contract_async() -> Result<Value, String> {
    let temporary = tempfile::tempdir().map_err(|error| error.to_string())?;
    let session_root = temporary.path().join("sessions");
    let client = Arc::new(RecordingAcpClient::default());
    let gate = GateDriver::new("answer", &session_root);
    let interrupted = gate.interrupted.clone();
    let started = gate.started.clone();
    let agent = Arc::new(
        AcpAgent::new(gate)
            .map_err(|error| error.to_string())?
            .with_session_root(session_root)
            .with_client_port(client.clone(), Duration::from_secs(1)),
    );
    let pre_initialize_rejected = agent
        .new_session(AcpNewSession {
            cwd: temporary.path().display().to_string(),
            additional_directories: None,
            mcp_servers: Vec::new(),
            meta: None,
        })
        .is_err();
    let initialized = agent
        .initialize_with(AcpInitializeRequest {
            protocol_version: ACP_PROTOCOL_VERSION,
            client_capabilities: AcpClientCapabilities {
                fs: AcpFilesystemCapabilities {
                    read_text_file: true,
                    write_text_file: true,
                },
                terminal: true,
                session: json!({}),
                meta: None,
            },
            client_info: None,
            meta: None,
        })
        .map_err(|error| error.to_string())?;
    let duplicate_initialize_rejected = agent.initialize().is_err();
    let unsupported_auth_rejected = agent.authenticate("unknown").is_err();
    agent
        .authenticate("environment")
        .map_err(|error| error.to_string())?;
    let session = agent
        .new_session(AcpNewSession {
            cwd: temporary.path().display().to_string(),
            additional_directories: Some(Vec::new()),
            mcp_servers: Vec::new(),
            meta: None,
        })
        .map_err(|error| error.to_string())?;
    agent
        .set_mode(&session.session_id, "plan")
        .await
        .map_err(|error| format!("set ACP mode: {error}"))?;
    let config_options = agent
        .set_config_option(&session.session_id, "thinking", "high")
        .await
        .map_err(|error| format!("set ACP config option: {error}"))?;
    let (sender, mut receiver) = tokio::sync::mpsc::channel(vibe_acp::MAX_ACP_UPDATE_QUEUE);
    let response = agent
        .prompt_content_streaming(
            &session.session_id,
            vec![
                json!({"type": "text", "text": "question"}),
                json!({"type": "image", "mimeType": "image/png", "data": "AA=="}),
                json!({
                    "type": "resource",
                    "uri": "file:///workspace/context.txt",
                    "text": "context",
                }),
            ],
            sender,
        )
        .await
        .map_err(|error| format!("run ACP rich-content prompt: {error}"))?;
    let mut updates = Vec::new();
    while let Ok(update) = receiver.try_recv() {
        updates.push(update.update);
    }
    let history = agent.history(&session.session_id, 0, 50).await;
    let history_supported = history.is_ok();
    let history_non_empty = history.as_ref().is_ok_and(|page| !page.entries.is_empty());
    let listed = agent
        .list_sessions(temporary.path().to_str(), None)
        .map_err(|error| format!("list ACP sessions: {error}"))?;

    let mut client_tool_methods = Vec::new();
    for method in [
        "fs/read_text_file",
        "fs/write_text_file",
        "terminal/create",
        "terminal/output",
        "terminal/wait_for_exit",
        "terminal/kill",
        "terminal/release",
    ] {
        let result = agent
            .client_tool(
                method,
                json!({"sessionId": session.session_id, "path": "src/lib.rs"}),
            )
            .await
            .map_err(|error| format!("invoke ACP client tool `{method}`: {error}"))?;
        if result["method"] == method {
            client_tool_methods.push(method);
        }
    }
    let permission = agent
        .request_permission(
            &session.session_id,
            json!({"toolCallId": "tool-1", "title": "Run"}),
            vec![json!({"optionId": "allow_once"})],
        )
        .await
        .map_err(|error| format!("request ACP permission: {error}"))?;
    let user_input_rejected = agent.deny_unsupported_user_input().is_err();

    let blocked_agent = agent.clone();
    let blocked_session_id = session.session_id.clone();
    let blocked = tokio::spawn(async move {
        blocked_agent
            .prompt(&blocked_session_id, "block")
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    });
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .map_err(|_| "ACP cancellation fixture did not start".to_owned())?;
    let cancel_dispatched = agent.cancel(&session.session_id).await.is_ok();
    let _ = tokio::time::timeout(Duration::from_secs(1), blocked)
        .await
        .map_err(|_| "ACP cancellation fixture did not finish".to_owned())?
        .map_err(|error| error.to_string())?;
    let active_cancel_interrupts = cancel_dispatched && interrupted.load(Ordering::Relaxed);
    let idle_cancel_noop = agent.cancel(&session.session_id).await.is_ok();

    let fork = agent.fork_session(AcpForkSession {
        session_id: session.session_id.clone(),
        cwd: temporary.path().display().to_string(),
        new_session_id: Some("acp-contract-fork".to_owned()),
        message_id: None,
        additional_directories: Vec::new(),
        mcp_servers: Vec::new(),
        meta: None,
    });
    let fork_supported = fork.is_ok();
    if let Ok(forked) = fork {
        agent
            .close_session(&forked.session_id)
            .await
            .map_err(|error| format!("close forked ACP session: {error}"))?;
    }
    agent
        .close_session(&session.session_id)
        .await
        .map_err(|error| format!("close original ACP session: {error}"))?;
    let loaded = agent.load_session(AcpLoadSession {
        session_id: session.session_id.clone(),
        cwd: temporary.path().display().to_string(),
        additional_directories: Vec::new(),
        mcp_servers: Vec::new(),
        meta: None,
    });
    let load_supported = loaded.is_ok();
    let (close_cancels_tasks, close_idempotent, spawn_rejected_after_close) =
        if let Ok(loaded) = loaded {
            interrupted.store(false, Ordering::Relaxed);
            let closing_agent = agent.clone();
            let closing_session_id = loaded.session_id.clone();
            let closing_prompt = tokio::spawn(async move {
                closing_agent
                    .prompt(&closing_session_id, "block")
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            });
            tokio::time::timeout(Duration::from_secs(1), started.notified())
                .await
                .map_err(|_| "ACP close fixture did not start".to_owned())?;
            let closed = agent.close_session(&loaded.session_id).await.is_ok();
            let task_finished = tokio::time::timeout(Duration::from_secs(1), closing_prompt)
                .await
                .is_ok();
            let close_cancels = closed && task_finished && interrupted.load(Ordering::Relaxed);
            let idempotent = agent.close_session(&loaded.session_id).await.is_ok();
            let rejected = agent
                .prompt(&loaded.session_id, "after close")
                .await
                .is_err();
            (close_cancels, idempotent, rejected)
        } else {
            (false, false, false)
        };
    agent
        .disconnect()
        .await
        .map_err(|error| format!("disconnect ACP agent: {error}"))?;
    let client_calls = client
        .calls
        .lock()
        .map_err(|_| "ACP client call log is poisoned".to_owned())?
        .clone();
    let expected_client_calls = [
        "fs/read_text_file",
        "fs/write_text_file",
        "terminal/create",
        "terminal/output",
        "terminal/wait_for_exit",
        "terminal/kill",
        "terminal/release",
        "session/request_permission",
    ];
    let _additional_execution_evidence = (
        pre_initialize_rejected,
        duplicate_initialize_rejected,
        unsupported_auth_rejected,
        config_options.len(),
        response.stop_reason,
        updates.len(),
        history_supported,
        history_non_empty,
        listed.sessions.len(),
        client_tool_methods.len(),
        permission.pointer("/outcome/optionId") == Some(&json!("allow_once")),
        user_input_rejected,
        fork_supported,
        load_supported,
        client_calls == expected_client_calls,
    );
    let capabilities = &initialized.agent_capabilities;

    Ok(json!({
        "initialize": {
            "closeSession": capabilities.session_capabilities.get("close").is_some(),
            "embeddedContext": capabilities.prompt_capabilities.embedded_context,
            "forkSession": capabilities.session_capabilities.get("fork").is_some(),
            "imagePrompts": capabilities.prompt_capabilities.image,
            "listSessions": capabilities.session_capabilities.get("list").is_some(),
            "loadSession": capabilities.load_session,
            "protocolVersion": initialized.protocol_version,
        },
        "lifecycle": {
            "activeCancelInterrupts": active_cancel_interrupts,
            "closeCancelsTasks": close_cancels_tasks,
            "closeIdempotent": close_idempotent,
            "idleCancelNoop": idle_cancel_noop,
            "spawnRejectedAfterClose": spawn_rejected_after_close,
        },
    }))
}

fn run_async<T>(future: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?
        .block_on(future)
}

#[derive(Clone)]
struct RecordingTerminalOps {
    transcript: TerminalTranscript,
    fail_enter: Option<TerminalStep>,
    fail_leave: Option<TerminalStep>,
}

impl TerminalOps for RecordingTerminalOps {
    fn enter(&mut self, step: TerminalStep) -> Result<(), String> {
        self.transcript
            .lock()
            .map_err(|_| "terminal transcript is poisoned".to_owned())?
            .push((true, step));
        if self.fail_enter == Some(step) {
            Err("injected enter failure".to_owned())
        } else {
            Ok(())
        }
    }

    fn leave(&mut self, step: TerminalStep) -> Result<(), String> {
        self.transcript
            .lock()
            .map_err(|_| "terminal transcript is poisoned".to_owned())?
            .push((false, step));
        if self.fail_leave == Some(step) {
            Err("injected restore failure".to_owned())
        } else {
            Ok(())
        }
    }
}

type TerminalTranscript = Arc<Mutex<Vec<(bool, TerminalStep)>>>;

fn recording_terminal_ops(
    fail_enter: Option<TerminalStep>,
    fail_leave: Option<TerminalStep>,
) -> (RecordingTerminalOps, TerminalTranscript) {
    let transcript = Arc::new(Mutex::new(Vec::new()));
    (
        RecordingTerminalOps {
            transcript: transcript.clone(),
            fail_enter,
            fail_leave,
        },
        transcript,
    )
}

fn terminal_transcript(transcript: &TerminalTranscript) -> Result<Vec<String>, String> {
    Ok(transcript
        .lock()
        .map_err(|_| "terminal transcript is poisoned".to_owned())?
        .iter()
        .map(|(enter, step)| {
            format!(
                "{}:{}",
                if *enter { "enter" } else { "leave" },
                terminal_step_name(*step)
            )
        })
        .collect())
}

const fn terminal_step_name(step: TerminalStep) -> &'static str {
    match step {
        TerminalStep::RawMode => "raw_mode",
        TerminalStep::AlternateScreen => "alternate_screen",
        TerminalStep::MouseCapture => "mouse_capture",
        TerminalStep::BracketedPaste => "bracketed_paste",
        TerminalStep::CursorHidden => "cursor_hidden",
    }
}

fn transcript_entry(
    id: &str,
    revision: u64,
    kind: TranscriptKind,
    text: &str,
    status: EntryStatus,
    details: Value,
) -> TranscriptEntry {
    TranscriptEntry {
        id: id.to_owned(),
        revision,
        kind,
        text: text.to_owned(),
        status,
        details,
    }
}

fn test_backend_text(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[derive(Default)]
struct MemoryClipboard {
    value: String,
}

impl ClipboardPort for MemoryClipboard {
    fn read_text(&mut self) -> Result<String, String> {
        Ok(self.value.clone())
    }

    fn write_text(&mut self, value: &str) -> Result<(), String> {
        if value.is_empty() {
            return Err("clipboard text is empty".to_owned());
        }
        self.value = value.to_owned();
        Ok(())
    }
}

struct FixtureEditor;

impl ExternalEditorPort for FixtureEditor {
    fn edit(&mut self, initial: &str) -> Result<String, String> {
        Ok(format!("{initial} edited"))
    }
}

#[derive(Default)]
struct MemoryCredentials {
    values: Mutex<BTreeMap<String, String>>,
}

impl CredentialStore for MemoryCredentials {
    fn set(&self, account: &str, secret: &str) -> Result<(), CredentialError> {
        self.values
            .lock()
            .map_err(|_| CredentialError::Unavailable)?
            .insert(account.to_owned(), secret.to_owned());
        Ok(())
    }

    fn get(&self, account: &str) -> Result<Option<String>, CredentialError> {
        Ok(self
            .values
            .lock()
            .map_err(|_| CredentialError::Unavailable)?
            .get(account)
            .cloned())
    }

    fn delete(&self, account: &str) -> Result<(), CredentialError> {
        self.values
            .lock()
            .map_err(|_| CredentialError::Unavailable)?
            .remove(account);
        Ok(())
    }
}

#[derive(Default)]
struct RecordingAcpClient {
    calls: Mutex<Vec<String>>,
}

impl AcpClientPort for RecordingAcpClient {
    fn request<'a>(&'a self, method: &'a str, _params: Value) -> AcpClientFuture<'a> {
        Box::pin(async move {
            self.calls
                .lock()
                .map_err(|_| "ACP client call log is poisoned".to_owned())?
                .push(method.to_owned());
            if method == "session/request_permission" {
                Ok(json!({
                    "outcome": {
                        "outcome": "selected",
                        "optionId": "allow_once",
                    },
                }))
            } else {
                Ok(json!({"method": method}))
            }
        })
    }
}

struct GateDriver {
    inner: EchoTurnDriver,
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    interrupted: Arc<AtomicBool>,
}

impl GateDriver {
    fn new(response: &str, session_root: &Path) -> Self {
        Self {
            inner: EchoTurnDriver::new(response).with_session_root(session_root),
            started: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
            interrupted: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl TurnDriver for GateDriver {
    fn run<'a>(&'a self, reservation: &'a TurnReservation) -> DriverFuture<'a> {
        Box::pin(async move {
            if reservation.prompt == "block" {
                self.started.notify_one();
                self.release.notified().await;
            }
            self.inner.run(reservation).await
        })
    }

    fn interrupt(
        &self,
        _session_id: &str,
        _turn_id: &str,
    ) -> Result<(), vibe_app_server::client::DriverError> {
        self.interrupted.store(true, Ordering::Relaxed);
        self.release.notify_waiters();
        Ok(())
    }
}

struct FixtureProjects;

impl ProjectCloud for FixtureProjects {
    fn create(
        &self,
        name: &str,
        repo_url: &str,
        default_branch: &str,
    ) -> Result<Project, CloudError> {
        Ok(Project {
            project_id: format!("project-{name}"),
            name: name.to_owned(),
            repositories: vec![ProjectRepository {
                repo_url: repo_url.to_owned(),
                default_branch: Some(default_branch.to_owned()),
            }],
            is_read_only: false,
        })
    }

    fn list(&self, cursor: Option<&str>) -> Result<ProjectPage, CloudError> {
        Ok(ProjectPage {
            projects: vec![
                Project {
                    project_id: format!("page-{}", cursor.unwrap_or("first")),
                    name: "page".to_owned(),
                    repositories: vec![ProjectRepository {
                        repo_url: "fixture".to_owned(),
                        default_branch: Some("main".to_owned()),
                    }],
                    is_read_only: false,
                },
                Project {
                    project_id: format!("read-only-{}", cursor.unwrap_or("first")),
                    name: "read only".to_owned(),
                    repositories: Vec::new(),
                    is_read_only: true,
                },
            ],
            next_cursor: cursor.is_none().then(|| "next".to_owned()),
        })
    }
}

struct FixtureTeleport {
    fail: bool,
}

impl TeleportCloud for FixtureTeleport {
    fn start(&self, request: &TeleportStartRequest) -> Result<String, CloudError> {
        if self.fail {
            Err(CloudError::Unauthorized("sign in again".to_owned()))
        } else {
            Ok(format!(
                "https://cloud.example/teleport/{}",
                request.project_id
            ))
        }
    }
}

struct FixtureGit {
    snapshot: GitSnapshot,
    pushed: Arc<AtomicBool>,
    push_fails: bool,
}

impl GitProbe for FixtureGit {
    fn inspect(&self, _working_directory: &Path) -> Result<GitSnapshot, CloudError> {
        Ok(self.snapshot.clone())
    }

    fn push(&self, _working_directory: &Path) -> Result<(), CloudError> {
        self.pushed.store(true, Ordering::Relaxed);
        if self.push_fails {
            Err(CloudError::Git("injected push failure".to_owned()))
        } else {
            Ok(())
        }
    }
}

fn object_params(value: Value) -> Result<BTreeMap<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| "contract parameters must be an object".to_owned())
        .map(|object| {
            object
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
}

fn string_field(values: &BTreeMap<String, Value>, key: &str) -> Result<String, String> {
    values
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("contract result omitted string `{key}`"))
}

fn notification_kinds(dispatch: &vibe_app_server::release4::Release4Dispatch) -> Vec<String> {
    dispatch
        .notifications
        .iter()
        .filter_map(|notification| {
            notification
                .params
                .get("event")
                .and_then(|event| event.get("kind"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}

fn array_len(value: &Value) -> usize {
    value.as_array().map_or(0, Vec::len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_stack_contract_executes_public_restoration_api() {
        let checks = tui_terminal_stack_contract().expect("terminal stack contract");
        assert_eq!(checks["headless"]["resize"], json!([72, 18]));
        assert_eq!(checks["headless"]["unicode"], true);
        assert_eq!(checks["lifecycle"]["cleanShutdown"], true);
    }

    #[test]
    fn tui_shell_contract_executes_public_state_and_mailbox_api() {
        let checks = tui_shell_contract().expect("TUI shell contract");
        assert_eq!(checks["history"]["batchSizes"], json!([2, 2]));
        assert_eq!(checks["history"]["backfillOrdered"], true);
        assert_eq!(checks["queue"]["fifo"], true);
    }

    #[test]
    fn tui_rendering_contract_executes_public_renderer_api() {
        let checks = tui_rendering_contract().expect("TUI rendering contract");
        assert_eq!(checks["diff"]["added"], true);
        assert_eq!(checks["diff"]["removed"], true);
        assert_eq!(checks["hostileContent"]["terminalControlsSafe"], true);
    }

    #[test]
    fn tui_input_contract_executes_public_editor_and_submission_api() {
        let checks = tui_input_contract().expect("TUI input contract");
        assert_eq!(checks["completion"]["applied"], "/review");
        assert_eq!(checks["history"]["unicodePreserved"], true);
        assert_eq!(checks["clipboard"]["copyAccepted"], true);
    }

    #[test]
    fn tui_controls_contract_executes_public_callback_and_command_api() {
        let checks = tui_controls_contract().expect("TUI controls contract");
        assert_eq!(checks["approval"]["kind"], "approval");
        assert_eq!(checks["callbackRaces"]["overlapQueued"], true);
        assert_eq!(checks["interrupt"]["clearsCallbacks"], true);
    }

    #[test]
    fn tui_setup_contract_executes_public_setup_and_preferences_api() {
        let checks = tui_setup_contract().expect("TUI setup contract");
        assert_eq!(checks["authentication"]["missingRejected"], true);
        assert_eq!(checks["authentication"]["processCredentialExternal"], true);
        assert_eq!(checks["theme"]["explicitPreserved"], true);
        assert_eq!(checks["voice"]["recordingStarted"], true);
    }

    #[test]
    fn acp_contract_executes_public_lifecycle_and_client_api() {
        let checks = acp_full_contract().expect("ACP contract");
        assert_eq!(checks["initialize"]["embeddedContext"], true);
        assert_eq!(checks["lifecycle"]["activeCancelInterrupts"], true);
        assert_eq!(checks["lifecycle"]["closeCancelsTasks"], true);
    }

    #[test]
    fn cloud_contract_executes_public_project_teleport_and_loop_api() {
        let checks = cloud_workflows_contract().expect("cloud workflow contract");
        assert_eq!(checks["projects"]["staleLinkCleared"], true);
        assert_eq!(checks["teleport"]["pushApproved"], true);
        assert_eq!(checks["loops"]["persistedCount"], 1);
        assert_eq!(checks["failureSafety"]["executionIdle"], true);
    }
}
