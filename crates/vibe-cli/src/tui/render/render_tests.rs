//! Frame-level rendering tests: the banner, the transcript regions, the
//! overlays, the composer and the terminal-safety guards, all painted through
//! a `TestBackend` so a regression shows up as a changed frame.
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use serde_json::json;
use vibe_app_server::client::EffectDetail;

use super::*;
use crate::tui::chat_input::{ChatInputState, InputEffect, InputEvent, KeyName};
use crate::tui::rewind::{RewindState, RewindTarget};
use crate::tui::setup::Theme;
use crate::tui::state::{EntryStatus, TranscriptEntry, TranscriptKind};

fn theme(colors_enabled: bool) -> ResolvedTheme {
    ResolvedTheme {
        theme: Theme::Dark,
        colors_enabled,
    }
}

fn test_context(secret_input: bool) -> UiContext<'static> {
    UiContext {
        cwd: Path::new("/workspace"),
        agent_name: " default ",
        secret_input,
        safety: Safety::Neutral,
        switching: false,
        feedback_active: false,
        voice_phase: VoicePhase::Disabled,
        voice_indicator: 0,
        banner: BannerContext {
            version: env!("CARGO_PKG_VERSION"),
            model: "mistral-medium-3.5",
            thinking: "high",
            models_count: 3,
            skills_count: 53,
            mcp_servers_enabled: 0,
            mcp_servers_total: 0,
            connectors_connected: 0,
            connectors_total: 17,
            hooks_count: 0,
            plan: Some("Free"),
        },
        tokens: TokenState {
            max_tokens: 200_000,
            current_tokens: 0,
        },
    }
}

fn draw_test(
    frame: &mut Frame<'_>,
    state: &mut TuiState,
    editor: &PromptEditor,
    secret_input: bool,
) {
    draw(
        frame,
        state,
        editor,
        &CompletionEngine::default(),
        InputMode::Prompt,
        theme(false),
        test_context(secret_input),
    );
}

fn draw_test_mode(
    frame: &mut Frame<'_>,
    state: &mut TuiState,
    editor: &PromptEditor,
    mode: InputMode,
) {
    draw(
        frame,
        state,
        editor,
        &CompletionEngine::default(),
        mode,
        theme(false),
        test_context(false),
    );
}

#[test]
fn hostile_content_never_reaches_the_terminal_as_control_sequences() {
    let rendered = sanitize_terminal(
        "\u{1b}]52;c;secret\u{7}\nline\u{0}",
        RenderLimits::default(),
    );
    assert!(!rendered.contains('\u{1b}'));
    assert!(!rendered.contains('\u{7}'));
    assert!(!rendered.contains('\u{0}'));
    assert!(rendered.contains('␛'));
}

#[test]
fn huge_and_narrow_content_is_bounded_without_splitting_graphemes() {
    let input = format!("{}e\u{301}", "x".repeat(MAX_RENDER_CHARS + 100));
    let rendered = sanitize_terminal(&input, RenderLimits::default());
    assert!(rendered.chars().count() < MAX_RENDER_CHARS + 100);
    assert_eq!(truncate_width("abc e\u{301}z", 5), "abc …");
}

#[test]
fn active_feedback_has_keyboard_visible_instructions() {
    let backend = TestBackend::new(80, 12);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut state = TuiState::new("session");
    state.ready = true;
    let editor = PromptEditor::default();
    let mut context = test_context(false);
    context.feedback_active = true;
    terminal
        .draw(|frame| {
            draw(
                frame,
                &mut state,
                &editor,
                &CompletionEngine::default(),
                InputMode::Prompt,
                theme(false),
                context,
            );
        })
        .expect("feedback frame");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Rate response: 1-3, 0 later, Esc dismisses"));
}

#[test]
fn rewind_overlay_preserves_target_actions_and_help_at_fixed_widths() {
    for width in [40, 80, 120] {
        let backend = TestBackend::new(width, 18);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut state = TuiState::new("session");
        state.rewind = RewindState::new(vec![RewindTarget {
            entry_id: "history:4:user".to_owned(),
            message: "edit the runtime checkpoint behavior".to_owned(),
            has_file_changes: true,
        }]);
        state
            .rewind
            .as_mut()
            .expect("rewind state")
            .set_error("Rewind failed: injected failure");
        terminal
            .draw(|frame| {
                draw_test(frame, &mut state, &PromptEditor::default(), false);
            })
            .expect("rewind frame");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Message 1 of 1"));
        assert!(rendered.contains("restore files"));
        assert!(rendered.contains("without restoring"));
        assert!(rendered.contains("injected failure"));
        assert!(rendered.contains("accept"));
    }
}

#[test]
fn every_semantic_region_renders_at_the_reference_widths() {
    let entries = vec![
        TranscriptEntry {
            id: "user".to_owned(),
            revision: 1,
            kind: TranscriptKind::UserMessage,
            text: "deploy the service".to_owned(),
            status: EntryStatus::Completed,
            details: serde_json::Value::Null,
        },
        TranscriptEntry {
            id: "reasoning".to_owned(),
            revision: 1,
            kind: TranscriptKind::Reasoning,
            text: "weighing options".to_owned(),
            status: EntryStatus::Completed,
            details: serde_json::Value::Null,
        },
        TranscriptEntry {
            id: "shell".to_owned(),
            revision: 1,
            kind: TranscriptKind::Effect,
            text: "bash".to_owned(),
            status: EntryStatus::Completed,
            details: json!({
                "type": "effect",
                "detail": EffectDetail::for_call("bash", &json!({"command": "cargo test"})),
                "state": {
                    "status": "completed",
                    "output": {"stdout": "ok", "stderr": ""},
                    "display": {"success": true, "verb": "Ran", "message": "cargo test"},
                },
            }),
        },
        TranscriptEntry {
            id: "edit".to_owned(),
            revision: 1,
            kind: TranscriptKind::Effect,
            text: "edit".to_owned(),
            status: EntryStatus::Completed,
            details: json!({
                "type": "effect",
                "detail": EffectDetail::for_call("edit", &json!({"file_path": "src/lib.rs"})),
                "state": {
                    "status": "completed",
                    "output": {"file": "src/lib.rs", "old_string": "old", "new_string": "new"},
                    "display": {"success": true, "verb": "Edited", "message": "lib.rs"},
                },
            }),
        },
        TranscriptEntry {
            id: "hook".to_owned(),
            revision: 1,
            kind: TranscriptKind::Notice,
            text: "reformatted".to_owned(),
            status: EntryStatus::Completed,
            details: json!({
                "type": "notice",
                "level": "info",
                "detail": {"kind": "hook_completed", "hookName": "format", "content": "reformatted", "status": "ok"},
            }),
        },
        TranscriptEntry {
            id: "compaction".to_owned(),
            revision: 1,
            kind: TranscriptKind::Checkpoint,
            text: "summarized 40 messages".to_owned(),
            status: EntryStatus::Completed,
            details: json!({"type": "checkpoint", "kind": "compaction"}),
        },
        TranscriptEntry {
            id: "assistant".to_owned(),
            revision: 1,
            kind: TranscriptKind::AssistantMessage,
            text: "done".to_owned(),
            status: EntryStatus::Completed,
            details: serde_json::Value::Null,
        },
    ];
    for width in [40, 80, 120] {
        let mut state = TuiState::new("session");
        state.ready = true;
        state.entries.clone_from(&entries);
        let backend = TestBackend::new(width, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let editor = PromptEditor::default();
        terminal
            .draw(|frame| draw_test(frame, &mut state, &editor, false))
            .expect("semantic regions render");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for expected in [
            "deploy the service",
            "weighing options",
            "✓ Ran cargo test",
            "✓ Edited lib.rs",
            "- old",
            "+ new",
            "[format] reformatted",
            "Compacting conversation",
            "done",
        ] {
            assert!(rendered.contains(expected), "{expected} missing at {width}");
        }
        // A collapsible result folds into its header; a diff never does.
        assert!(!rendered.contains("│ ok"), "shell body expanded at {width}");
        assert!(
            state
                .transcript_view
                .lines()
                .iter()
                .any(|line| line.contains("Edited lib.rs")),
            "the frame did not publish its painted transcript at {width}"
        );
    }
}

#[test]
fn a_keyboard_selection_is_visible_and_survives_a_streaming_repaint() {
    let mut state = TuiState::new("session");
    state.ready = true;
    state.entries.push(TranscriptEntry {
        id: "assistant".to_owned(),
        revision: 1,
        kind: TranscriptKind::AssistantMessage,
        text: "deployed".to_owned(),
        status: EntryStatus::Streaming,
        details: serde_json::Value::Null,
    });
    let editor = PromptEditor::default();
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).expect("test terminal");
    terminal
        .draw(|frame| draw_test(frame, &mut state, &editor, false))
        .expect("first frame paints the transcript");
    assert!(state.transcript_view.move_selection(1));
    assert_eq!(
        state.transcript_view.selected_text().as_deref(),
        Some("  deployed")
    );

    // The reference reverses the selected cells; a repaint keeps them.
    for text in ["deployed", "deployed twice"] {
        state.entries[0].text = text.to_owned();
        terminal
            .draw(|frame| draw_test(frame, &mut state, &editor, false))
            .expect("repaint keeps the selection");
    }
    let reversed = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .filter(|cell| cell.modifier.contains(Modifier::REVERSED))
        .count();
    assert!(reversed > 0, "the selection was not marked on screen");
}

#[test]
fn failed_effects_render_the_error_indicator_and_never_a_success_header() {
    let backend = TestBackend::new(52, 16);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut state = TuiState::new("session");
    state.ready = true;
    state.entries.push(TranscriptEntry {
        id: "effect".to_owned(),
        revision: 1,
        kind: TranscriptKind::Effect,
        text: "edit".to_owned(),
        status: EntryStatus::Failed,
        details: json!({
            "type": "effect",
            "detail": EffectDetail::for_call("edit", &json!({"path": "src/lib.rs"})),
            "state": {
                "status": "failed",
                "error": {"message": "permission denied"},
                "outputText": "",
                "display": {"success": false, "verb": "Edited", "message": "lib.rs"},
            },
        }),
    });
    let editor = PromptEditor::default();
    terminal
        .draw(|frame| draw_test(frame, &mut state, &editor, false))
        .expect("snapshot renders");
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("✕ Edited lib.rs"), "{text}");
    assert!(text.contains("Error: permission denied"), "{text}");
    assert!(!text.contains('✓'), "{text}");
}

#[test]
fn notices_follow_the_upstream_user_command_message_contract() {
    let entry = TranscriptEntry {
        id: "notice".to_owned(),
        revision: 1,
        kind: TranscriptKind::Notice,
        text: "scheduled loop fired".to_owned(),
        status: EntryStatus::Completed,
        details: serde_json::Value::Null,
    };
    let lines = semantic_lines(&entry, 40, theme(false), true);
    assert_eq!(lines[0].to_string(), "  ⎣ scheduled loop fired");
    assert!(!lines[0].to_string().contains("[NOTICE]"));
}

#[test]
fn user_messages_follow_the_upstream_prompt_and_separator_contract() {
    let entry = TranscriptEntry {
        id: "user".to_owned(),
        revision: 1,
        kind: TranscriptKind::UserMessage,
        text: "/help".to_owned(),
        status: EntryStatus::Completed,
        details: serde_json::Value::Null,
    };
    let lines = semantic_lines(&entry, 24, theme(false), true);
    assert_eq!(lines[0].to_string(), "/ help");
    assert_eq!(lines.len(), 1);
    assert!(!lines.iter().any(|line| line.to_string().contains("[USER]")));

    let mut prompt = entry;
    prompt.text = "hello".to_owned();
    let lines = semantic_lines(&prompt, 24, theme(false), true);
    assert_eq!(lines[0].to_string(), "> hello");
    assert_eq!(lines[1].to_string(), "─".repeat(24));
}

#[test]
fn messages_wrap_without_losing_status_or_content() {
    let entry = TranscriptEntry {
        id: "assistant".to_owned(),
        revision: 1,
        kind: TranscriptKind::AssistantMessage,
        text: "abcdefghijkl".to_owned(),
        status: EntryStatus::Failed,
        details: json!({"error": "network"}),
    };
    let lines = semantic_lines(&entry, 7, theme(false), true);
    assert_eq!(lines[0].to_string(), "  abcde");
    assert_eq!(lines[1].to_string(), "  fghij");
    assert_eq!(lines[2].to_string(), "  kl");
    assert_eq!(lines[3].to_string(), "  (failed: network)");
}

#[test]
fn composer_uses_upstream_chrome_modes_completion_and_footer() {
    let backend = TestBackend::new(84, 30);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut state = TuiState::new("session");
    let mut editor = PromptEditor::default();
    editor.set_text("/he");
    let mut completion = CompletionEngine::default();
    completion
        .refresh(&editor, Path::new("/workspace"))
        .expect("slash completion");

    terminal
        .draw(|frame| {
            draw(
                frame,
                &mut state,
                &editor,
                &completion,
                InputMode::Command,
                theme(true),
                test_context(false),
            );
        })
        .expect("composer renders");

    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains(" default "));
    assert!(text.contains("Show help message"));
    assert!(text.contains("/workspace"));
    assert!(text.contains(&format!(
        "Mistral Vibe v{} · mistral-medium-3.5[high] · Free",
        env!("CARGO_PKG_VERSION")
    )));
    assert!(text.contains("3 models · 0/17 connectors · 0 MCP servers · 53 skills"));
    assert!(text.contains("Type /help for more information"));
    assert!(text.contains("0/200k tokens (0%)"));
    assert!(!text.contains("Transcript"));
    assert!(!text.contains("Prompt"));
    assert!(!text.contains("connected |"));
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((2, 25))
            .expect("composer body cell")
            .bg,
        Color::Reset
    );
}

#[test]
fn voice_chrome_matches_recording_and_flushing_states() {
    let backend = TestBackend::new(84, 30);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut state = TuiState::new("session");
    let mut editor = PromptEditor::default();
    editor.set_text("draft");
    let completion = CompletionEngine::default();
    let mut context = test_context(false);
    context.voice_phase = VoicePhase::Recording;
    context.voice_indicator = 7;

    terminal
        .draw(|frame| {
            draw(
                frame,
                &mut state,
                &editor,
                &completion,
                InputMode::Prompt,
                theme(true),
                context,
            );
        })
        .expect("recording composer renders");
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains(" default "));
    assert!(rendered.contains('█'));
    assert!(!rendered.contains("Recording:"));
    assert_eq!(
        composer_border_style(theme(true), Safety::Neutral, VoicePhase::Recording),
        theme(true).orange()
    );

    context.voice_phase = VoicePhase::Transcribing;
    context.voice_indicator = 3;
    terminal
        .draw(|frame| {
            draw(
                frame,
                &mut state,
                &editor,
                &completion,
                InputMode::Prompt,
                theme(true),
                context,
            );
        })
        .expect("flushing composer renders");
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains('▌'));
    assert!(!rendered.contains("Transcribing:"));
}

#[test]
fn completion_popup_renders_reference_widths_and_scroll_bounds() {
    let visible = [
        "/help",
        "/config",
        "/clear",
        "/compact",
        "/connectors",
        "/continue",
        "/copy",
        "/data-retention",
        "/debug",
        "/exit",
    ];
    for width in [40, 80, 120] {
        let backend = TestBackend::new(width, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut state = TuiState::new("session");
        let mut editor = PromptEditor::default();
        editor.set_text("/");
        let mut completion = CompletionEngine::default();
        completion
            .refresh(&editor, Path::new("/workspace"))
            .expect("slash completion");

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &mut state,
                    &editor,
                    &completion,
                    InputMode::Command,
                    theme(false),
                    test_context(false),
                );
            })
            .expect("completion snapshot");
        let rows = terminal
            .backend()
            .buffer()
            .content
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        for label in visible {
            let displayed = if width == 40 && label == "/data-retention" {
                "/data-reten…"
            } else {
                label
            };
            assert!(
                rows.iter().any(|row| row.contains(displayed)),
                "{displayed} missing at {width} columns"
            );
        }
        assert!(!rows.iter().any(|row| row.contains("/leanstall")));

        let last = completion
            .view()
            .expect("completion view")
            .candidates
            .len()
            .saturating_sub(1);
        for _ in 0..last {
            completion.move_selection(1);
        }
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &mut state,
                    &editor,
                    &completion,
                    InputMode::Command,
                    theme(false),
                    test_context(false),
                );
            })
            .expect("scrolled completion snapshot");
        let scrolled = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(scrolled.contains("/voice"));
        assert!(!scrolled.contains("Show help message"));
    }
}

#[test]
fn skill_descriptions_render_while_empty_loading_and_dismissed_popups_stay_hidden() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut state = TuiState::new("session");
    let mut editor = PromptEditor::default();
    editor.set_text("/rev");
    let mut completion = CompletionEngine::default();
    completion.set_user_skills([("review", "Review the working tree")]);
    completion
        .refresh(&editor, Path::new("/workspace"))
        .expect("skill completion");
    terminal
        .draw(|frame| {
            draw(
                frame,
                &mut state,
                &editor,
                &completion,
                InputMode::Command,
                theme(false),
                test_context(false),
            );
        })
        .expect("skill popup");
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Review the working tree"));

    completion.cancel();
    terminal
        .draw(|frame| {
            draw(
                frame,
                &mut state,
                &editor,
                &completion,
                InputMode::Command,
                theme(false),
                test_context(false),
            );
        })
        .expect("dismissed popup");
    let dismissed = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!dismissed.contains("Review the working tree"));

    let temporary = tempfile::tempdir().expect("temporary workspace");
    editor.set_text("@");
    completion
        .refresh(&editor, temporary.path())
        .expect("loading path request");
    assert!(completion.view().is_none());
}

#[test]
fn context_progress_uses_the_official_compact_format() {
    assert_eq!(
        format_context_progress(TokenState {
            max_tokens: 200_000,
            current_tokens: 12_345,
        }),
        "12k/200k tokens (6%)"
    );
    assert_eq!(
        format_context_progress(TokenState {
            max_tokens: 2_000_000,
            current_tokens: 2_500_000,
        }),
        "2.5M/2.0M tokens (100%)"
    );
}

#[test]
fn composer_renders_all_supported_input_modes_as_prefix_chrome() {
    assert_eq!(InputMode::Prompt.symbol(), '>');
    assert_eq!(InputMode::Shell.symbol(), '!');
    assert_eq!(InputMode::Command.symbol(), '/');
    assert_eq!(InputMode::Teleport.symbol(), '&');
    assert_eq!(InputMode::Prompt.prefix_len(), 0);
    assert_eq!(InputMode::Teleport.prefix_len(), 1);
}

#[test]
fn mode_prefix_and_unicode_cursor_render_at_fixed_reference_widths() {
    for width in [40, 80, 120] {
        let backend = TestBackend::new(width, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut state = TuiState::new("session");
        let mut editor = PromptEditor::default();
        editor.set_text("/界");

        terminal
            .draw(|frame| {
                draw_test_mode(frame, &mut state, &editor, InputMode::Command);
            })
            .expect("mode composer renders");

        let input_height =
            ComposerLayout::for_viewport(&editor, width, 12, InputMode::Command.prefix_len())
                .input_height();
        let body_y = 12_u16.saturating_sub(input_height);
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer.cell((0, body_y)).map(|cell| cell.symbol()),
            Some("/")
        );
        assert_eq!(
            buffer.cell((2, body_y)).map(|cell| cell.symbol()),
            Some("界")
        );
        assert_eq!(
            terminal
                .get_cursor_position()
                .expect("rendered cursor position"),
            ratatui::layout::Position::new(4, body_y)
        );
    }
}

#[test]
fn composer_mouse_coordinates_reject_chrome_and_map_visible_cells() {
    let mut editor = PromptEditor::default();
    editor.set_text("alpha\nbeta");
    let screen = Rect::new(0, 0, 40, 10);
    let body_y = 5;
    assert_eq!(
        editor_mouse_cell(&editor, screen, false, InputMode::Prompt, 4, body_y),
        Some((0, 2))
    );
    assert_eq!(
        editor_mouse_cell(&editor, screen, false, InputMode::Prompt, 1, body_y),
        None
    );
    assert_eq!(
        editor_mouse_cell(&editor, screen, false, InputMode::Prompt, 4, 0),
        None
    );

    editor.set_text("word ".repeat(200));
    let composer = ComposerLayout::for_viewport(
        &editor,
        screen.width,
        screen.height,
        InputMode::Prompt.prefix_len(),
    );
    let body_y = screen
        .height
        .saturating_sub(1)
        .saturating_sub(composer.input_height())
        .saturating_add(1);
    assert!(composer.scroll() > 0);
    assert_eq!(
        editor_mouse_cell(&editor, screen, false, InputMode::Prompt, 4, body_y),
        Some((composer.scroll(), 2))
    );
    assert_eq!(
        editor_mouse_cell(
            &editor,
            screen,
            false,
            InputMode::Prompt,
            PROMPT_WIDTH.saturating_add(
                u16::try_from(composer.width()).expect("composer width fits terminal")
            ),
            body_y,
        ),
        None
    );
}

#[test]
fn latest_diagnostic_is_visible_without_leaving_the_prompt() {
    let backend = TestBackend::new(80, 8);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut state = TuiState::new("session");
    state.push_diagnostic("Keyring unavailable; restart with --setup after repairing access");
    terminal
        .draw(|frame| draw_test(frame, &mut state, &PromptEditor::default(), false))
        .expect("diagnostic renders");
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("Keyring unavailable"));
}

#[test]
fn prompt_snapshot_keeps_selection_and_cursor_visible() {
    let backend = TestBackend::new(30, 9);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut state = TuiState::new("session");
    let mut editor = PromptEditor::default();
    editor.set_text("aβc");
    editor.select(1..2);

    terminal
        .draw(|frame| draw_test(frame, &mut state, &editor, false))
        .expect("prompt renders");

    let input_y = 4;
    let selected = terminal
        .backend()
        .buffer()
        .cell((3, input_y))
        .expect("selected cell");
    assert_eq!(selected.symbol(), "β");
    assert!(selected.modifier.contains(Modifier::REVERSED));
    assert_eq!(
        terminal
            .get_cursor_position()
            .expect("rendered cursor position"),
        ratatui::layout::Position::new(4, input_y)
    );
}

#[test]
fn tall_entry_scrolls_by_semantic_lines() {
    let backend = TestBackend::new(32, 12);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut state = TuiState::new("session");
    state.entries.push(TranscriptEntry {
        id: "tall".to_owned(),
        revision: 1,
        kind: TranscriptKind::AssistantMessage,
        text: [
            "line-one",
            "line-two",
            "line-three",
            "line-four",
            "line-five",
            "line-six",
            "line-seven",
        ]
        .join("\n"),
        status: EntryStatus::Completed,
        details: serde_json::Value::Null,
    });

    terminal
        .draw(|frame| draw_test(frame, &mut state, &PromptEditor::default(), false))
        .expect("latest semantic lines render");
    let latest = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!latest.contains("line-one"));
    assert!(latest.contains("line-seven"));

    assert!(state.scroll_up(2));
    terminal
        .draw(|frame| draw_test(frame, &mut state, &PromptEditor::default(), false))
        .expect("older semantic lines render");
    let scrolled = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(scrolled.contains("line-one"));
    assert!(!scrolled.contains("line-seven"));
}

#[test]
fn one_mebibyte_prompt_keeps_both_ends_and_cursor_visible_at_reference_widths() {
    let text = format!("HEAD{}TAIL", "a".repeat(1024 * 1024 - 8));
    for width in [40, 80, 120] {
        let backend = TestBackend::new(width, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut state = TuiState::new("session");
        let mut editor = PromptEditor::default();
        editor.set_text(&text);

        terminal
            .draw(|frame| draw_test(frame, &mut state, &editor, false))
            .expect("tail viewport renders");
        let tail = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(tail.contains("TAIL"), "tail missing at {width} columns");
        let cursor = terminal
            .get_cursor_position()
            .expect("tail cursor remains visible");
        assert!(cursor.x < width && cursor.y < 24);

        editor.move_home(false);
        terminal
            .draw(|frame| draw_test(frame, &mut state, &editor, false))
            .expect("head viewport renders");
        let head = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(head.contains("HEAD"), "head missing at {width} columns");
    }
}

#[test]
fn one_mebibyte_prompt_edit_and_cached_frames_stay_within_release_budgets() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut state = TuiState::new("session");
    let mut input = ChatInputState::default();
    input.replace_text("a".repeat(1024 * 1024));
    terminal
        .draw(|frame| draw_test(frame, &mut state, input.editor(), false))
        .expect("layout cache warms");

    let edit_started = std::time::Instant::now();
    let effects = input.apply(InputEvent::Key {
        key: KeyName::Char,
        char: Some('z'),
        mods: Vec::new(),
    });
    assert_eq!(effects, vec![InputEffect::HistoryReset]);
    terminal
        .draw(|frame| draw_test(frame, &mut state, input.editor(), false))
        .expect("edited prompt renders");
    let edit_frame = edit_started.elapsed();
    assert!(
        edit_frame < std::time::Duration::from_millis(50),
        "1 MiB edit-and-render frame took {edit_frame:?}"
    );
    assert!(
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|cell| cell.symbol() == "z"),
        "edited tail is not visible"
    );

    let mut frames = Vec::with_capacity(200);
    for _ in 0..200 {
        let started = std::time::Instant::now();
        terminal
            .draw(|frame| draw_test(frame, &mut state, input.editor(), false))
            .expect("cached frame renders");
        frames.push(started.elapsed());
    }
    frames.sort_unstable();
    let p95 = frames[189];
    assert!(
        p95 < std::time::Duration::from_millis(50),
        "1 MiB cached-frame p95 was {p95:?}"
    );
}

#[test]
fn tiny_terminals_keep_long_unbroken_prompts_bounded() {
    let mut editor = PromptEditor::default();
    editor.set_text("界".repeat(4096));
    for (width, height) in [(1, 1), (2, 2), (9, 3)] {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("tiny terminal");
        let mut state = TuiState::new("session");
        terminal
            .draw(|frame| draw_test(frame, &mut state, &editor, false))
            .expect("tiny prompt frame");
        let cursor = terminal
            .get_cursor_position()
            .expect("tiny terminal cursor");
        assert!(cursor.x < width && cursor.y < height);
    }
}

#[test]
fn overflowing_queue_rows_are_keyboard_scrollable() {
    let backend = TestBackend::new(48, 6);
    let mut terminal = Terminal::new(backend).expect("queue terminal");
    let mut lines = vec!["Queued messages".to_owned()];
    lines.extend((0..10).map(|index| format!("› queued item {index}")));

    terminal
        .draw(|frame| draw_queue(frame, frame.area(), &lines, 0, theme(true)))
        .expect("queue start renders");
    let first = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(first.contains("queued item 0"));
    assert!(!first.contains("queued item 9"));
    assert!(first.contains("Alt+PgUp/PgDn"));

    terminal
        .draw(|frame| draw_queue(frame, frame.area(), &lines, 5, theme(true)))
        .expect("queue tail renders");
    let tail = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!tail.contains("queued item 0"));
    assert!(tail.contains("queued item 9"));
}
