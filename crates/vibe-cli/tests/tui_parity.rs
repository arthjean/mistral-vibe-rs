use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::path::Path;
use vibe_cli::tui::attachments::PromptDraft;
use vibe_cli::tui::chat_input::InputMode;
use vibe_cli::tui::clipboard::{SystemClipboard, SystemClipboardPort, osc52_sequence};
use vibe_cli::tui::commands::{
    CommandContext, CommandId, command_aliases, command_aliases_in, parse_command, parse_command_in,
};
use vibe_cli::tui::completion::CompletionEngine;
use vibe_cli::tui::input::PromptEditor;
use vibe_cli::tui::interaction::{
    Overlay, OverlayItem, OverlayKind, PromptQueue, QuitConfirmation,
};
use vibe_cli::tui::pickers::{config_overlay, mcp_overlay, rewind_state, sessions_overlay};
use vibe_cli::tui::render::{BannerContext, TokenState, UiContext, draw};
use vibe_cli::tui::rewind::RewindAction;
use vibe_cli::tui::setup::{DetectedTheme, Theme, resolve_theme};
use vibe_cli::tui::state::TuiState;
use vibe_cli::tui::state::{EntryStatus, TranscriptEntry, TranscriptKind};

#[test]
fn official_textual_commands_are_all_registered_with_their_aliases() {
    let aliases = command_aliases().collect::<Vec<_>>();
    let mut expected = vec![
        "/clear",
        "/compact",
        "/config",
        "/connectors",
        "/continue",
        "/copy",
        "/data-retention",
        "/debug",
        "/exit",
        "/help",
        "/leanstall",
        "/log",
        "/loop",
        "/mcp",
        "/model",
        "/new",
        "/proxy-setup",
        "/reload",
        "/rename",
        "/resume",
        "/rewind",
        "/status",
        "/theme",
        "/thinking",
        "/unleanstall",
        "/voice",
    ];
    if cfg!(target_os = "macos") {
        expected.push("/paste-image");
        expected.sort_unstable();
    }
    assert_eq!(aliases, expected);

    assert_eq!(
        parse_command("/new").map(|command| command.id),
        Some(CommandId::Clear)
    );
    assert_eq!(
        parse_command("/rename session title").map(|command| command.id),
        Some(CommandId::Rename)
    );
    assert_eq!(
        parse_command("/connectors").map(|command| command.id),
        Some(CommandId::Mcp)
    );
    assert_eq!(
        parse_command("/resume").map(|command| command.id),
        Some(CommandId::Resume)
    );
    assert_eq!(
        parse_command("/continue").map(|command| command.id),
        Some(CommandId::Resume)
    );

    for rust_only in [
        "/close",
        "/quit",
        "/title",
        "/approve",
        "/deny",
        "/fork",
        "/history",
        "/setup",
        "/settings",
        "/trust",
        "/update",
    ] {
        assert!(parse_command(rust_only).is_none(), "parsed {rust_only}");
    }
}

#[test]
fn command_availability_tracks_capabilities_exclusions_and_platform_support() {
    let context = CommandContext::new(true).with_excluded(["voice"]);
    let aliases = command_aliases_in(&context).collect::<Vec<_>>();
    assert!(aliases.contains(&"/teleport"));
    assert!(aliases.contains(&"/remote-project"));
    assert!(!aliases.contains(&"/voice"));
    assert!(parse_command_in("/voice", &context).is_none());
    assert_eq!(
        parse_command_in("/continue", &context).map(|command| command.id),
        Some(CommandId::Resume)
    );
    let macos = CommandContext::default().with_clipboard_image_supported(true);
    let non_macos = CommandContext::default().with_clipboard_image_supported(false);
    assert!(command_aliases_in(&macos).any(|alias| alias == "/paste-image"));
    assert!(!command_aliases_in(&non_macos).any(|alias| alias == "/paste-image"));
}

#[test]
fn prompts_submitted_during_a_turn_are_queued_fifo_and_can_be_cancelled_lifo() {
    let mut queue = PromptQueue::default();
    queue.push(prompt_draft("first"));
    queue.push(prompt_draft("second"));
    queue.push(prompt_draft("third"));

    assert_eq!(
        queue.cancel_last().map(PromptDraft::into_text).as_deref(),
        Some("third")
    );
    let batch = queue.take_next_batch().expect("prompt batch");
    assert_eq!(batch[0].draft.text(), "first");
    assert_eq!(batch[1].draft.text(), "second");
    assert!(queue.is_empty());
}

#[test]
fn failed_queue_batch_can_be_restored_at_the_front_without_reordering() {
    let mut queue = PromptQueue::default();
    queue.push(prompt_draft("first"));
    queue.push(prompt_draft("second"));
    let batch = queue.take_next_batch().expect("prompt batch");
    queue.restore_batch_and_pause(batch);

    assert_eq!(queue.len(), 2);
    assert!(queue.take_next_batch().is_none());
    queue.resume();
    let restored = queue.take_next_batch().expect("restored batch");
    assert_eq!(restored[0].draft.text(), "first");
    assert_eq!(restored[1].draft.text(), "second");
}

#[test]
fn cancelling_the_last_item_resumes_an_empty_queue_and_shell_rows_drop_the_sigil() {
    let mut queue = PromptQueue::default();
    queue.push(prompt_draft("!pwd"));
    queue.pause();
    assert_eq!(
        queue.presentation_lines(),
        vec!["Queued messages (paused)", "$ pwd"]
    );

    assert_eq!(
        queue.cancel_last().map(PromptDraft::into_text).as_deref(),
        Some("!pwd")
    );
    assert!(!queue.is_paused());
}

fn prompt_draft(text: &str) -> PromptDraft {
    PromptDraft::text_only(text)
}

#[test]
fn idle_quit_requires_a_matching_second_key_within_the_confirmation_window() {
    let mut confirmation = QuitConfirmation::default();

    assert!(!confirmation.request("ctrl+c", 100));
    assert!(!confirmation.request("ctrl+d", 150));
    assert!(!confirmation.request("ctrl+c", 200));
    assert!(confirmation.request("ctrl+c", 250));
    assert!(!confirmation.request("ctrl+c", 1_251));
}

#[test]
fn picker_overlays_filter_without_losing_selection_or_disabled_rows() {
    let mut overlay = Overlay::new(
        OverlayKind::Model,
        "Select model",
        vec![
            OverlayItem::new("small", "Small", "Fast", false),
            OverlayItem::new("medium", "Medium", "Balanced", false),
            OverlayItem::new("large", "Large", "Unavailable", true),
        ],
    );

    overlay.move_selection(1);
    assert_eq!(
        overlay.selected_item().map(|item| item.id.as_str()),
        Some("medium")
    );
    overlay.set_query("lar");
    assert!(overlay.selected_item().is_none());
    overlay.set_query("med");
    assert_eq!(
        overlay.selected_item().map(|item| item.id.as_str()),
        Some("medium")
    );
}

#[test]
fn picker_overlay_is_rendered_above_the_transcript_with_keyboard_help() {
    let mut state = TuiState::new("session");
    state.overlay = Some(Overlay::new(
        OverlayKind::Theme,
        "Select theme",
        vec![
            OverlayItem::new("dark", "Textual dark", "current", false),
            OverlayItem::new("light", "Textual light", "", false),
        ],
    ));
    let editor = PromptEditor::default();
    let completion = CompletionEngine::default();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");

    terminal
        .draw(|frame| {
            draw(
                frame,
                &mut state,
                &editor,
                &completion,
                InputMode::Prompt,
                resolve_theme(Theme::Dark, DetectedTheme::Dark, true),
                UiContext {
                    cwd: Path::new("/workspace"),
                    agent_name: " default ",
                    secret_input: false,
                    safety: vibe_cli::tui::chat_input::Safety::Neutral,
                    switching: false,
                    feedback_active: false,
                    voice_phase: vibe_cli::tui::chat_input::VoicePhase::Disabled,
                    voice_indicator: 0,
                    banner: BannerContext {
                        version: env!("CARGO_PKG_VERSION"),
                        model: "model",
                        thinking: "off",
                        models_count: 1,
                        skills_count: 0,
                        mcp_servers_enabled: 0,
                        mcp_servers_total: 0,
                        connectors_connected: 0,
                        connectors_total: 0,
                        hooks_count: 0,
                        plan: None,
                    },
                    tokens: TokenState {
                        max_tokens: 1,
                        current_tokens: 0,
                    },
                },
            );
        })
        .expect("draw");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(text.contains("Select theme"));
    assert!(text.contains("Textual dark"));
    assert!(text.contains("↑↓/jk"));
    assert!(text.contains("Enter"));
    assert!(text.contains("Esc"));
}

#[test]
fn osc52_clipboard_fallback_is_terminal_and_tmux_safe() {
    assert_eq!(
        osc52_sequence("Mistral", false),
        "\u{1b}]52;c;TWlzdHJhbA==\u{7}"
    );
    assert_eq!(
        osc52_sequence("Mistral", true),
        "\u{1b}Ptmux;\u{1b}\u{1b}]52;c;TWlzdHJhbA==\u{7}\u{1b}\\"
    );
}

#[test]
fn system_clipboard_reports_image_support_by_platform() {
    let clipboard = SystemClipboard;
    assert_eq!(clipboard.supports_images(), cfg!(target_os = "macos"));
}

#[test]
fn assistant_markdown_is_rendered_semantically_instead_of_literally() {
    let mut state = TuiState::new("session");
    state.entries.push(TranscriptEntry {
        id: "assistant".to_owned(),
        revision: 1,
        kind: TranscriptKind::AssistantMessage,
        text: "# Result\n\n- first\n- second\n\n`cargo test`".to_owned(),
        status: EntryStatus::Completed,
        details: serde_json::Value::Null,
    });
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");

    terminal
        .draw(|frame| {
            draw(
                frame,
                &mut state,
                &PromptEditor::default(),
                &CompletionEngine::default(),
                InputMode::Prompt,
                resolve_theme(Theme::Dark, DetectedTheme::Dark, true),
                UiContext {
                    cwd: Path::new("/workspace"),
                    agent_name: " default ",
                    secret_input: false,
                    safety: vibe_cli::tui::chat_input::Safety::Neutral,
                    switching: false,
                    feedback_active: false,
                    voice_phase: vibe_cli::tui::chat_input::VoicePhase::Disabled,
                    voice_indicator: 0,
                    banner: BannerContext {
                        version: env!("CARGO_PKG_VERSION"),
                        model: "model",
                        thinking: "off",
                        models_count: 1,
                        skills_count: 0,
                        mcp_servers_enabled: 0,
                        mcp_servers_total: 0,
                        connectors_connected: 0,
                        connectors_total: 0,
                        hooks_count: 0,
                        plan: None,
                    },
                    tokens: TokenState {
                        max_tokens: 1,
                        current_tokens: 0,
                    },
                },
            );
        })
        .expect("draw");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(text.contains("Result"));
    assert!(!text.contains("# Result"));
    assert!(text.contains("• first"));
    assert!(text.contains("cargo test"));
    assert!(!text.contains("`cargo test`"));
}

#[test]
fn public_server_payloads_build_searchable_config_session_and_mcp_pickers() {
    let config = config_overlay(
        &serde_json::json!({
            "config": {
                "active_model": "codestral",
                "thinking": "high",
                "voice_mode_enabled": true
            },
            "selectedTarget": "project",
            "layerValues": [
                {"layer": "defaults", "values": {"thinking": "off"}},
                {"layer": "selected_toml", "values": {"active_model": "codestral", "thinking": "high"}},
                {"layer": "environment", "values": {"voice_mode_enabled": true}}
            ]
        }),
        &serde_json::json!({
            "properties": {
                "active_model": {"type": "string"},
                "thinking": {"enum": ["off", "low", "high"]}
            }
        }),
    );
    assert_eq!(config.kind, OverlayKind::Config);
    assert!(
        config
            .items
            .iter()
            .any(|item| { item.id == "active_model" && item.description.contains("codestral") })
    );
    assert!(
        config
            .items
            .iter()
            .any(|item| item.id == "thinking" && item.description.contains("project"))
    );
    assert!(
        config
            .items
            .iter()
            .any(|item| item.id == "voice_mode_enabled"
                && item.description.contains("environment"))
    );

    let sessions = sessions_overlay(
        &serde_json::json!({
            "sessions": [{
                "id": "session-123",
                "title": "Parity work",
                "endTime": "2026-07-31T10:00:00Z",
                "messageCount": 12
            }]
        }),
        "other-session",
    );
    assert_eq!(
        sessions.selected_item().map(|item| item.id.as_str()),
        Some("session-123")
    );

    let mcp = mcp_overlay(&serde_json::json!({
        "mcp": {
            "sources": [{
                "name": "github",
                "status": "connected",
                "enabled": true,
                "tools": [{"name": "search"}, {"name": "read"}]
            }]
        }
    }));
    assert!(mcp.items[0].description.contains("2 tools"));

    let fresh = config_overlay(
        &serde_json::json!({"config": {}}),
        &serde_json::json!({
            "properties": {
                "thinking": {"enum": ["off", "low"], "default": "off"},
                "voice_mode_enabled": {"type": "boolean", "default": false}
            }
        }),
    );
    assert!(fresh.items.iter().any(|item| item.id == "thinking"));
    assert!(
        fresh
            .items
            .iter()
            .any(|item| item.id == "voice_mode_enabled")
    );

    let rewind = rewind_state(&serde_json::json!({
        "messages": [
            {"messageIndex": 0, "message": "first prompt"},
            {
                "messageIndex": 3,
                "message": "prompt to edit",
                "hasFileChanges": true
            }
        ],
        "restoreSupported": true
    }))
    .expect("rewind targets");
    assert_eq!(rewind.target().message_index, 3);
    assert!(rewind.target().message.contains("prompt to edit"));
    assert_eq!(
        rewind.actions(),
        &[RewindAction::RestoreAndEdit, RewindAction::EditOnly]
    );
}

#[test]
fn overlay_rendering_survives_a_tiny_terminal_and_wide_unicode_labels() {
    let mut state = TuiState::new("session");
    state.overlay = Some(Overlay::new(
        OverlayKind::Model,
        "模型",
        vec![OverlayItem::new("wide", "模型模型", "description", false)],
    ));
    let backend = TestBackend::new(7, 3);
    let mut terminal = Terminal::new(backend).expect("terminal");

    terminal
        .draw(|frame| {
            draw(
                frame,
                &mut state,
                &PromptEditor::default(),
                &CompletionEngine::default(),
                InputMode::Prompt,
                resolve_theme(Theme::Dark, DetectedTheme::Dark, true),
                UiContext {
                    cwd: Path::new("/w"),
                    agent_name: "default",
                    secret_input: false,
                    safety: vibe_cli::tui::chat_input::Safety::Neutral,
                    switching: false,
                    feedback_active: false,
                    voice_phase: vibe_cli::tui::chat_input::VoicePhase::Disabled,
                    voice_indicator: 0,
                    banner: BannerContext {
                        version: env!("CARGO_PKG_VERSION"),
                        model: "m",
                        thinking: "off",
                        models_count: 1,
                        skills_count: 0,
                        mcp_servers_enabled: 0,
                        mcp_servers_total: 0,
                        connectors_connected: 0,
                        connectors_total: 0,
                        hooks_count: 0,
                        plan: None,
                    },
                    tokens: TokenState {
                        max_tokens: 1,
                        current_tokens: 0,
                    },
                },
            );
        })
        .expect("tiny overlay renders");
}

#[test]
fn unicode_prompt_selection_exposes_exact_clipboard_text() {
    let mut editor = PromptEditor::default();
    editor.set_text("aβe\u{301}z");
    editor.select(1..3);
    assert_eq!(editor.selected_text().as_deref(), Some("βe\u{301}"));
}

#[test]
fn user_invocable_skills_join_slash_completion() {
    let temporary = tempfile::tempdir().expect("workspace");
    let mut completion = CompletionEngine::default();
    completion.set_user_skills([("review", "Review the current change")]);
    let mut editor = PromptEditor::default();
    editor.set_text("/rev");

    assert!(
        completion
            .complete_prompt(&mut editor, temporary.path())
            .expect("skill completion")
    );
    assert_eq!(editor.text(), "/review");
}
