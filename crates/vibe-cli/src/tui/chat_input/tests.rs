use super::*;

fn key(name: KeyName) -> InputEvent {
    InputEvent::Key {
        key: name,
        char: None,
        mods: Vec::new(),
    }
}

fn character(value: char) -> InputEvent {
    InputEvent::Key {
        key: KeyName::Char,
        char: Some(value),
        mods: Vec::new(),
    }
}

fn type_text(state: &mut ChatInputState, value: &str) -> Vec<InputEffect> {
    value
        .chars()
        .flat_map(|character_value| state.apply(character(character_value)))
        .collect()
}

/// Types `value`, then answers the completion request it produced.
fn complete(
    state: &mut ChatInputState,
    value: &str,
    candidates: Vec<CompletionCandidate>,
) -> Vec<InputEffect> {
    let request = type_text(state, value)
        .into_iter()
        .rev()
        .find_map(|effect| match effect {
            InputEffect::RequestCompletion { request } => Some(request),
            _ => None,
        })
        .expect("typing a token requests a completion");
    state.apply(InputEvent::CompletionResolved {
        resolution: CompletionResolution::Results {
            request,
            candidates,
        },
    })
}

fn mention(label: &str) -> CompletionCandidate {
    CompletionCandidate {
        id: format!("mention:{label}"),
        kind: CompletionKind::Mention,
        label: label.to_owned(),
        insertion: label.to_owned(),
        description: String::new(),
    }
}

#[test]
fn transitions_are_deterministic_and_serializable() {
    let mut left = ChatInputState::new();
    let mut right = ChatInputState::new();
    for character_value in "abc".chars() {
        assert_eq!(
            left.apply(character(character_value)),
            right.apply(character(character_value))
        );
    }
    assert_eq!(left.observe(), right.observe());
    let encoded = serde_json::to_string(&left.observe()).expect("observation serializes");
    let decoded: StateObservation =
        serde_json::from_str(&encoded).expect("observation round-trips");
    assert_eq!(decoded, left.observe());
}

#[test]
fn filesystem_work_is_requested_as_an_effect() {
    let mut state = ChatInputState::new();
    let effects = type_text(&mut state, "@src");
    assert!(effects.iter().any(|effect| matches!(
        effect,
        InputEffect::RequestCompletion { request } if request.query == "@src"
    )));
    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect, InputEffect::Submit { .. }))
    );
}

#[test]
fn external_editor_and_clipboard_leave_the_boundary_as_effects() {
    let mut state = ChatInputState::new();
    type_text(&mut state, "draft");
    let effects = state.apply(InputEvent::Key {
        key: KeyName::Char,
        char: Some('g'),
        mods: vec![Modifier::Ctrl],
    });
    assert!(effects.iter().any(|effect| matches!(
        effect,
        InputEffect::OpenExternalEditor { text } if text == "draft"
    )));
    let effects = state.apply(InputEvent::Paste {
        text: String::new(),
    });
    assert_eq!(
        effects,
        vec![InputEffect::ClipboardImageRequested {
            notify_when_empty: false
        }]
    );

    state.set_command_context(CommandContext::default().with_clipboard_image_supported(true));
    for modifier in [Modifier::Ctrl, Modifier::Meta] {
        let effects = state.apply(InputEvent::Key {
            key: KeyName::Char,
            char: Some('v'),
            mods: vec![modifier],
        });
        assert_eq!(
            effects,
            vec![InputEffect::ClipboardImageRequested {
                notify_when_empty: true
            }]
        );
    }
}

#[test]
fn a_cancelled_external_edit_keeps_the_prompt() {
    let mut state = ChatInputState::new();
    type_text(&mut state, "draft");
    let effects = state.apply(InputEvent::ExternalEditor { text: None });
    assert_eq!(effects, Vec::new());
    assert_eq!(state.observe().text, "draft");
}

#[test]
fn external_editing_preserves_the_current_mode_during_follow_up_typing() {
    let mut state = ChatInputState::new();
    state.apply(InputEvent::ExternalEditor {
        text: Some("/c".to_owned()),
    });
    state.apply(character('o'));
    assert_eq!(state.observe().mode, InputMode::Prompt);
    assert_eq!(state.observe().text, "/co");
}

#[test]
fn stale_completion_results_are_ignored_without_panicking() {
    let mut state = ChatInputState::new();
    type_text(&mut state, "@s");
    let effects = state.apply(InputEvent::CompletionResolved {
        resolution: CompletionResolution::Results {
            request: CompletionRequest {
                generation: 0,
                query: "@s".to_owned(),
                range: [0, 2],
            },
            candidates: vec![mention("@src/")],
        },
    });
    assert_eq!(effects, Vec::new());
    assert!(!state.observe().completion.open);
}

#[test]
fn completion_results_require_the_current_query_range() {
    let mut state = ChatInputState::new();
    let mut request = type_text(&mut state, "see(@src")
        .into_iter()
        .rev()
        .find_map(|effect| match effect {
            InputEffect::RequestCompletion { request } => Some(request),
            _ => None,
        })
        .expect("path request");
    request.range = [0, request.range[1]];
    let effects = state.apply(InputEvent::CompletionResolved {
        resolution: CompletionResolution::Results {
            request,
            candidates: vec![mention("@src/")],
        },
    });
    assert_eq!(effects, Vec::new());
    assert!(!state.observe().completion.open);
}

#[test]
fn current_completion_failure_is_silent_and_keeps_the_prompt_editable() {
    let mut state = ChatInputState::new();
    let request = type_text(&mut state, "@src")
        .into_iter()
        .rev()
        .find_map(|effect| match effect {
            InputEffect::RequestCompletion { request } => Some(request),
            _ => None,
        })
        .expect("path request");
    assert_eq!(
        state.apply(InputEvent::CompletionResolved {
            resolution: CompletionResolution::Failed {
                request,
                reason: "scan failed".to_owned(),
            },
        }),
        Vec::new()
    );
    state.apply(character('x'));
    assert_eq!(state.observe().text, "@srcx");
    assert!(!state.observe().completion.open);
}

#[test]
fn out_of_order_completion_results_keep_the_newest_popup() {
    let mut state = ChatInputState::new();
    let older = type_text(&mut state, "@")
        .into_iter()
        .find_map(|effect| match effect {
            InputEffect::RequestCompletion { request } => Some(request),
            _ => None,
        })
        .expect("first path request");
    let newer = type_text(&mut state, "s")
        .into_iter()
        .find_map(|effect| match effect {
            InputEffect::RequestCompletion { request } => Some(request),
            _ => None,
        })
        .expect("second path request");

    state.apply(InputEvent::CompletionResolved {
        resolution: CompletionResolution::Results {
            request: newer,
            candidates: vec![mention("@src/")],
        },
    });
    let current = state.observe().completion;
    assert!(current.open);

    assert_eq!(
        state.apply(InputEvent::CompletionResolved {
            resolution: CompletionResolution::Results {
                request: older,
                candidates: vec![mention("@stale/")],
            },
        }),
        Vec::new()
    );
    assert_eq!(state.observe().completion, current);
}

#[test]
fn invalid_events_keep_state_valid() {
    let mut state = ChatInputState::new();
    let effects = state.apply(InputEvent::Key {
        key: KeyName::Char,
        char: None,
        mods: Vec::new(),
    });
    assert_eq!(
        effects,
        vec![InputEffect::Rejected {
            reason: "character key without a character".to_owned()
        }]
    );
    let effects = state.apply(InputEvent::PasteNormalized {
        snapshot: EditorSnapshot {
            text: String::new(),
            cursor: 0,
            selection: None,
        },
        text: "@image.png".to_owned(),
    });
    assert_eq!(
        effects,
        vec![InputEffect::Rejected {
            reason: "no paste to normalize".to_owned()
        }]
    );
    assert_eq!(state.observe().text, "");
    assert_eq!(state.observe().cursor, 0);
}

#[test]
fn completion_selection_and_acceptance_stay_in_bounds() {
    let mut state = ChatInputState::new();
    complete(
        &mut state,
        "@sr",
        vec![mention("@src/"), mention("@srv.rs")],
    );
    let observation = state.observe();
    assert!(observation.completion.open);
    assert_eq!(observation.completion.items.len(), 2);
    assert_eq!(observation.completion.selected, 0);

    state.apply(key(KeyName::Up));
    assert_eq!(state.observe().completion.selected, 1);
    state.apply(key(KeyName::Down));
    assert_eq!(state.observe().completion.selected, 0);

    let accepted = state.observe().completion.items[0].label.clone();
    state.apply(key(KeyName::Tab));
    assert_eq!(state.observe().text, accepted);
    assert!(!state.observe().completion.open);
}

#[test]
fn navigation_inside_the_popup_does_not_report_a_reset() {
    let mut state = ChatInputState::new();
    complete(
        &mut state,
        "@sr",
        vec![mention("@src/"), mention("@srv.rs")],
    );
    assert_eq!(state.apply(key(KeyName::Down)), Vec::new());
    assert!(state.observe().completion.open);
    assert_eq!(state.apply(key(KeyName::Escape)), Vec::new());
    assert!(state.observe().completion.open);
}

#[test]
fn a_closed_popup_never_reports_a_reset() {
    let mut state = ChatInputState::new();
    let effects = type_text(&mut state, "plain");
    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect, InputEffect::CompletionReset)),
        "{effects:?}"
    );
}

#[test]
fn submission_reports_the_payload_and_history_entry() {
    let mut state = ChatInputState::new();
    type_text(&mut state, " spaced ");
    let effects = state.apply(key(KeyName::Enter));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        InputEffect::SubmitRequested { text } if text == "spaced"
    )));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        InputEffect::RecordHistory { entry } if entry == "spaced"
    )));
    assert_eq!(state.observe().text, "");
}

#[test]
fn an_empty_submission_is_observed_but_does_not_clear_or_record_a_turn() {
    let mut state = ChatInputState::new();
    type_text(&mut state, "   ");
    let effects = state.apply(key(KeyName::Enter));
    assert_eq!(
        effects,
        vec![
            InputEffect::SubmitRequested {
                text: String::new()
            },
            InputEffect::Submit {
                text: String::new()
            }
        ]
    );
    assert_eq!(state.observe().text, "   ");
    assert!(state.history_entries().is_empty());
}

#[test]
fn switching_blocks_submission_and_keeps_the_prompt() {
    let mut state = ChatInputState::new();
    type_text(&mut state, "queued");
    state.apply(InputEvent::Switching { active: true });
    let effects = state.apply(key(KeyName::Enter));
    assert_eq!(
        effects,
        vec![InputEffect::SubmitRequested {
            text: "queued".to_owned()
        }]
    );
    assert_eq!(state.observe().text, "queued");
}

#[test]
fn history_navigation_is_observed_from_the_editor() {
    let mut state = ChatInputState::new();
    type_text(&mut state, "first");
    state.apply(key(KeyName::Enter));
    assert!(!state.observe().history.navigating);
    state.apply(key(KeyName::Up));
    assert!(!state.observe().history.navigating);
    assert!(state.observe().history.loaded_entry);
    assert_eq!(state.observe().text, "first");
    state.apply(key(KeyName::Down));
    assert!(!state.observe().history.navigating);
}

#[test]
fn teleport_mode_is_capability_gated_and_resets_without_losing_follow_up_text() {
    let mut unavailable = ChatInputState::new();
    unavailable.apply(character('&'));
    assert_eq!(unavailable.observe().mode, InputMode::Prompt);
    assert_eq!(unavailable.observe().text, "&");

    let mut available = ChatInputState::new();
    available.set_teleport_available(true);
    assert_eq!(
        available.apply(character('&')),
        vec![InputEffect::ModeChanged {
            mode: InputMode::Teleport
        }]
    );
    assert_eq!(available.observe().mode, InputMode::Teleport);
    available.apply(key(KeyName::Backspace));
    available.apply(character('x'));
    assert_eq!(available.observe().mode, InputMode::Prompt);
    assert_eq!(available.observe().text, "x");
}

#[test]
fn mode_prefix_is_not_editable_while_body_text_remains() {
    let mut state = ChatInputState::new();
    type_text(&mut state, "/body");
    state.apply(key(KeyName::Home));
    assert_eq!(state.observe().cursor, 1);
    state.apply(key(KeyName::Backspace));
    assert_eq!(state.observe().text, "/body");
    assert_eq!(state.observe().mode, InputMode::Command);
    state.apply(InputEvent::Key {
        key: KeyName::Left,
        char: None,
        mods: vec![Modifier::Shift],
    });
    assert_eq!(state.observe().selection, None);
}

#[test]
fn mouse_selection_is_bounded_and_out_of_bounds_events_are_inert() {
    let mut state = ChatInputState::new();
    type_text(&mut state, "select me");
    state.apply(InputEvent::Mouse {
        x: 0,
        y: 0,
        extend_selection: false,
    });
    state.apply(InputEvent::Mouse {
        x: 3,
        y: 0,
        extend_selection: true,
    });
    assert_eq!(state.observe().selection, Some([0, 3]));
    let before = state.observe();
    state.apply(InputEvent::Mouse {
        x: 0,
        y: 99,
        extend_selection: false,
    });
    assert_eq!(state.observe(), before);
}

#[test]
fn mouse_selection_field_uses_the_canonical_camel_case_schema() {
    let event: InputEvent = serde_json::from_value(serde_json::json!({
        "type": "mouse",
        "x": 7,
        "y": 1,
        "extendSelection": true
    }))
    .expect("mouse event decodes");
    assert!(matches!(
        event,
        InputEvent::Mouse {
            extend_selection: true,
            ..
        }
    ));
}

#[test]
fn secret_input_closes_and_suppresses_completion() {
    let mut state = ChatInputState::new();
    complete(&mut state, "@sr", vec![mention("@src/")]);
    assert!(state.observe().completion.open);
    state.set_secret_input(true);
    assert!(!state.observe().completion.open);
    let effects = type_text(&mut state, "c");
    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect, InputEffect::RequestCompletion { .. })),
        "{effects:?}"
    );
}

#[test]
fn image_mentions_use_spacing_at_the_actual_insertion_point() {
    let mut state = ChatInputState::new();
    type_text(&mut state, "body");
    state.apply(key(KeyName::Home));

    assert!(state.insert_image_mention(Path::new("/tmp/image one.png")));
    assert_eq!(state.observe().text, "@'/tmp/image one.png' body");
}

#[test]
fn stale_path_normalization_cannot_replace_newer_input() {
    let mut state = ChatInputState::new();
    let effects = type_text(&mut state, "/tmp/old.png");
    let snapshot = effects
        .into_iter()
        .find_map(|effect| match effect {
            InputEffect::NormalizeCurrentText { snapshot } => Some(snapshot),
            _ => None,
        })
        .expect("path normalization snapshot");
    state.replace_text("newer input");

    assert_eq!(
        state.apply(InputEvent::TextNormalized {
            snapshot,
            text: "@/tmp/old.png".to_owned(),
        }),
        Vec::new()
    );
    assert_eq!(state.observe().text, "newer input");
}

#[test]
fn path_normalization_requires_the_original_cursor_snapshot() {
    let mut state = ChatInputState::new();
    let effects = type_text(&mut state, "/tmp/old.png");
    let snapshot = effects
        .into_iter()
        .find_map(|effect| match effect {
            InputEffect::NormalizeCurrentText { snapshot } => Some(snapshot),
            _ => None,
        })
        .expect("path normalization snapshot");
    state.apply(key(KeyName::Left));

    state.apply(InputEvent::TextNormalized {
        snapshot,
        text: "@/tmp/old.png".to_owned(),
    });

    assert_eq!(state.observe().text, "/tmp/old.png");
    assert_eq!(state.observe().cursor, 11);
}

#[test]
fn feedback_keys_are_consumed_while_printable_dismissal_is_reinserted() {
    let mut state = ChatInputState::new();
    state.apply(InputEvent::Feedback { active: true });
    assert_eq!(
        state.apply(character('2')),
        vec![InputEffect::FeedbackRating { rating: 2 }]
    );
    assert_eq!(state.observe().text, "");

    assert_eq!(
        state.apply(character('x')),
        vec![InputEffect::FeedbackDismissed, InputEffect::HistoryReset]
    );
    assert_eq!(state.observe().text, "x");
}

#[test]
fn voice_effects_are_generation_aware_and_preserve_the_prompt() {
    let mut state = ChatInputState::new();
    state.set_voice_enabled(true);
    type_text(&mut state, "say: ");

    assert_eq!(
        state.apply(InputEvent::Key {
            key: KeyName::Char,
            char: Some('r'),
            mods: vec![Modifier::Ctrl],
        }),
        vec![InputEffect::RecordingStartRequested]
    );
    let generation = state.voice_generation();
    assert_eq!(state.voice_phase(), VoicePhase::Starting);
    assert_eq!(
        state.apply(InputEvent::VoiceStartResolved {
            generation,
            error: None,
        }),
        Vec::new()
    );
    assert_eq!(state.voice_phase(), VoicePhase::Recording);

    assert_eq!(
        state.apply(character('x')),
        vec![InputEffect::RecordingStopRequested]
    );
    assert_eq!(state.observe().text, "say: ");
    assert_eq!(state.voice_phase(), VoicePhase::Transcribing);
    assert_eq!(
        state.apply(InputEvent::Transcript {
            text: "hello".to_owned(),
            generation: Some(generation),
        }),
        vec![InputEffect::HistoryReset]
    );
    assert_eq!(state.observe().text, "say: hello");
    assert_eq!(state.voice_phase(), VoicePhase::Idle);

    let effects = state.apply(InputEvent::Transcript {
        text: " stale".to_owned(),
        generation: Some(generation),
    });
    assert!(matches!(effects.as_slice(), [InputEffect::Rejected { .. }]));
    assert_eq!(state.observe().text, "say: hello");
}

#[test]
fn voice_start_failure_recovers_to_idle_with_bounded_feedback() {
    let mut state = ChatInputState::new();
    state.set_voice_enabled(true);
    state.apply(InputEvent::Key {
        key: KeyName::Char,
        char: Some('r'),
        mods: vec![Modifier::Ctrl],
    });
    let generation = state.voice_generation();
    assert_eq!(
        state.apply(InputEvent::VoiceStartResolved {
            generation,
            error: Some("No microphone is available".to_owned()),
        }),
        vec![InputEffect::Notify {
            message: "No microphone is available".to_owned(),
            severity: Severity::Warning,
        }]
    );
    assert_eq!(state.voice_phase(), VoicePhase::Idle);
}

#[test]
fn streaming_voice_deltas_survive_stop_and_cancel_invalidates_late_results() {
    let mut state = ChatInputState::new();
    state.set_voice_enabled(true);
    type_text(&mut state, "say: ");
    state.apply(InputEvent::Key {
        key: KeyName::Char,
        char: Some('r'),
        mods: vec![Modifier::Ctrl],
    });
    let generation = state.voice_generation();
    state.apply(InputEvent::VoiceStartResolved {
        generation,
        error: None,
    });

    assert_eq!(
        state.apply(InputEvent::VoiceTranscriptDelta {
            text: "hello".to_owned(),
            generation,
        }),
        vec![InputEffect::HistoryReset]
    );
    assert_eq!(state.observe().text, "say: hello");
    assert_eq!(
        state.apply(character('x')),
        vec![InputEffect::RecordingStopRequested]
    );
    assert_eq!(state.observe().text, "say: hello");
    assert_eq!(
        state.apply(InputEvent::VoiceStopResolved {
            generation,
            error: None,
        }),
        Vec::new()
    );
    assert_eq!(state.voice_phase(), VoicePhase::Transcribing);
    assert_eq!(
        state.apply(InputEvent::VoiceTranscriptDelta {
            text: " world".to_owned(),
            generation,
        }),
        vec![InputEffect::HistoryReset]
    );
    assert_eq!(state.apply(character('z')), Vec::new());
    assert_eq!(state.observe().text, "say: hello world");

    assert_eq!(
        state.apply(InputEvent::Key {
            key: KeyName::Char,
            char: Some('c'),
            mods: vec![Modifier::Ctrl],
        }),
        vec![InputEffect::RecordingCancelRequested]
    );
    assert_eq!(state.voice_phase(), VoicePhase::Idle);
    assert_eq!(
        state.apply(InputEvent::VoiceTranscriptDelta {
            text: " stale".to_owned(),
            generation,
        }),
        Vec::new()
    );
    assert_eq!(
        state.apply(InputEvent::VoiceDone { generation }),
        Vec::new()
    );
    assert_eq!(
        state.apply(InputEvent::VoiceStopResolved {
            generation,
            error: Some("late failure".to_owned()),
        }),
        Vec::new()
    );
    assert_eq!(state.observe().text, "say: hello world");
}

#[test]
fn empty_and_failed_transcriptions_recover_without_losing_the_prompt() {
    let mut state = ChatInputState::new();
    state.set_voice_enabled(true);
    type_text(&mut state, "draft");

    for error in [None, Some("Transcription failed".to_owned())] {
        let failed = error.is_some();
        state.apply(InputEvent::Key {
            key: KeyName::Char,
            char: Some('r'),
            mods: vec![Modifier::Ctrl],
        });
        let generation = state.voice_generation();
        state.apply(InputEvent::VoiceStartResolved {
            generation,
            error: None,
        });
        let effects = if failed {
            state.apply(InputEvent::VoiceStopResolved { generation, error })
        } else {
            state.apply(InputEvent::VoiceDone { generation })
        };
        if failed {
            assert!(matches!(effects.as_slice(), [InputEffect::Notify { .. }]));
        } else {
            assert_eq!(effects, Vec::new());
        }
        assert_eq!(state.voice_phase(), VoicePhase::Idle);
        assert_eq!(state.observe().text, "draft");
        assert_eq!(
            state.apply(InputEvent::VoiceTranscriptDelta {
                text: "late".to_owned(),
                generation,
            }),
            Vec::new()
        );
        assert_eq!(state.observe().text, "draft");
    }
}

#[test]
fn one_thousand_out_of_order_voice_sequences_apply_no_stale_result() {
    let mut seed = 0x4d59_5df4_d0f3_3173_u64;
    for _ in 0..1_000 {
        let mut state = ChatInputState::new();
        state.set_voice_enabled(true);
        type_text(&mut state, "draft");
        state.apply(InputEvent::Key {
            key: KeyName::Char,
            char: Some('r'),
            mods: vec![Modifier::Ctrl],
        });
        let stale_generation = state.voice_generation();
        state.apply(InputEvent::Key {
            key: KeyName::Char,
            char: Some('c'),
            mods: vec![Modifier::Ctrl],
        });
        state.apply(InputEvent::Key {
            key: KeyName::Char,
            char: Some('r'),
            mods: vec![Modifier::Ctrl],
        });
        let current_generation = state.voice_generation();
        state.apply(InputEvent::VoiceStartResolved {
            generation: current_generation,
            error: None,
        });

        for _ in 0..16 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let event = match seed % 5 {
                0 => InputEvent::VoiceTranscriptDelta {
                    text: "stale".to_owned(),
                    generation: stale_generation,
                },
                1 => InputEvent::VoiceDone {
                    generation: stale_generation,
                },
                2 => InputEvent::VoicePeak {
                    generation: stale_generation,
                    level: 7,
                },
                3 => InputEvent::VoiceStartResolved {
                    generation: stale_generation,
                    error: Some("late start".to_owned()),
                },
                _ => InputEvent::VoiceStopResolved {
                    generation: stale_generation,
                    error: Some("late stop".to_owned()),
                },
            };
            assert_eq!(state.apply(event), Vec::new());
            assert_eq!(state.observe().text, "draft");
            assert_eq!(state.voice_phase(), VoicePhase::Recording);
            assert_eq!(state.voice_generation(), current_generation);
        }
    }
}

#[test]
fn safety_switching_and_recording_are_observable_render_states() {
    let mut state = ChatInputState::new();
    state.set_agent_name("agent");
    state.set_safety(Safety::Destructive);
    let render = state.observe_render();
    assert_eq!(render.border_classes, ["border-warning"]);
    assert_eq!(render.border_title, " agent ");

    state.apply(InputEvent::Switching { active: true });
    let render = state.observe_render();
    assert_eq!(render.prompt, None);
    assert_eq!(render.wrap_width, 0);
}
