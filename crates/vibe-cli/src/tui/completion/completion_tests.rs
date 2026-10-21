use super::*;

#[test]
fn completions_are_stable_and_racing_results_are_rejected() {
    let mut engine = CompletionEngine::default();
    let candidates = vec![
        CompletionCandidate {
            id: "skill".to_owned(),
            kind: CompletionKind::Skill,
            label: "review".to_owned(),
            insertion: "/review".to_owned(),
            description: "Review changes".to_owned(),
        },
        CompletionCandidate {
            id: "command".to_owned(),
            kind: CompletionKind::SlashCommand,
            label: "reload".to_owned(),
            insertion: "/reload".to_owned(),
            description: "Reload configuration".to_owned(),
        },
    ];
    let stale = engine.complete("re", candidates.clone());
    let current = engine.complete("rev", candidates);
    let mut editor = PromptEditor::default();
    assert!(matches!(
        engine.apply(&mut editor, &stale, 0),
        Err(InputError::StaleCompletion)
    ));
    engine
        .apply(&mut editor, &current, 0)
        .expect("current completion applies");
    assert_eq!(editor.text(), "/review");
}

#[test]
fn runtime_completion_applies_slash_and_at_mention_candidates() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    fs::create_dir(temporary.path().join("src")).expect("path fixture");
    let mut engine = CompletionEngine::default();
    let mut editor = PromptEditor::default();

    editor.set_text("/hel");
    assert!(
        engine
            .complete_prompt(&mut editor, temporary.path())
            .expect("slash completion")
    );
    assert_eq!(editor.text(), "/help");

    editor.set_text("inspect sr");
    assert!(
        !engine
            .complete_prompt(&mut editor, temporary.path())
            .expect("bare paths do not activate completion")
    );
    assert_eq!(editor.text(), "inspect sr");

    editor.set_text("inspect @sr");
    assert!(
        engine
            .complete_prompt(&mut editor, temporary.path())
            .expect("mention completion")
    );
    assert_eq!(editor.text(), "inspect @src/");
}

#[test]
fn live_completion_tracks_selection_and_applies_official_spacing() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    fs::write(temporary.path().join("notes.txt"), "context").expect("mention fixture");
    fs::create_dir(temporary.path().join("src")).expect("nested directory");
    fs::write(temporary.path().join("src/lib.rs"), "pub fn nested() {}")
        .expect("nested mention fixture");
    let mut engine = CompletionEngine::default();
    let mut editor = PromptEditor::default();

    editor.set_text("/cl");
    engine
        .refresh(&editor, temporary.path())
        .expect("slash suggestions");
    let view = engine.view().expect("visible slash suggestions");
    assert_eq!(view.candidates[view.selected].label, "/clear");
    let suggestion_count = view.candidates.len();
    assert!(engine.move_selection(-1));
    assert_eq!(
        engine.view().expect("wrapped selection").selected,
        suggestion_count - 1
    );
    assert!(engine.move_selection(1));
    assert_eq!(engine.view().expect("rewrapped selection").selected, 0);
    assert_eq!(
        engine.accept(&mut editor).expect("slash completion"),
        CompletionAction::Submit
    );
    assert_eq!(editor.text(), "/clear");
    assert!(engine.view().is_none());

    editor.set_text("inspect @no");
    engine
        .refresh(&editor, temporary.path())
        .expect("mention suggestions");
    engine
        .wait_for_pending(&editor)
        .expect("completed mention search");
    assert_eq!(
        engine.accept(&mut editor).expect("mention completion"),
        CompletionAction::Applied
    );
    assert_eq!(editor.text(), "inspect @notes.txt ");

    editor.set_text("inspect @lib");
    engine
        .refresh(&editor, temporary.path())
        .expect("recursive mention suggestions");
    engine
        .wait_for_pending(&editor)
        .expect("completed recursive search");
    assert_eq!(
        engine
            .view()
            .expect("recursive suggestion")
            .candidates
            .first()
            .map(|candidate| candidate.label.as_str()),
        Some("@src/lib.rs")
    );

    editor.set_text("inspect @noXYZ");
    editor.move_left(false);
    editor.move_left(false);
    editor.move_left(false);
    engine
        .refresh(&editor, temporary.path())
        .expect("partial mention suggestions");
    engine
        .wait_for_pending(&editor)
        .expect("completed partial search");
    engine.accept(&mut editor).expect("partial completion");
    assert_eq!(editor.text(), "inspect @notes.txt XYZ");
}

#[test]
fn completion_candidates_are_not_truncated_at_the_old_sixty_four_item_bound() {
    let mut engine = CompletionEngine::default();
    let result = engine.complete(
        "/",
        (0..128).map(|index| CompletionCandidate {
            id: format!("candidate-{index}"),
            kind: CompletionKind::SlashCommand,
            label: format!("/candidate-{index}"),
            insertion: format!("/candidate-{index}"),
            description: String::new(),
        }),
    );
    assert_eq!(result.candidates.len(), 128);
    engine.cancel();
    assert!(matches!(
        engine.apply(&mut PromptEditor::default(), &result, 0),
        Err(InputError::StaleCompletion)
    ));

    let mut editor = PromptEditor::default();
    editor.set_text("abcd");
    editor.move_left(false);
    editor.move_left(true);
    assert_eq!(editor.selection(), Some(2..3));
    editor.move_right(true);
    assert_eq!(editor.selection(), None);
    editor.move_right(true);
    assert_eq!(editor.selection(), Some(3..4));
}

#[test]
fn duplicate_slash_aliases_keep_one_entry_and_the_last_sorted_description() {
    let mut engine = CompletionEngine::default();
    let result = engine.complete(
        "/",
        [
            CompletionCandidate {
                id: "command:review".to_owned(),
                kind: CompletionKind::SlashCommand,
                label: "/review".to_owned(),
                insertion: "/review".to_owned(),
                description: "Built-in review".to_owned(),
            },
            CompletionCandidate {
                id: "skill:review".to_owned(),
                kind: CompletionKind::Skill,
                label: "/review".to_owned(),
                insertion: "/review".to_owned(),
                description: "Skill review".to_owned(),
            },
        ],
    );
    assert_eq!(result.candidates.len(), 1);
    assert_eq!(result.candidates[0].description, "Skill review");
}

#[test]
fn capability_updates_preserve_command_exclusions() {
    let mut engine = CompletionEngine::default();
    engine.set_command_context(CommandContext::default().with_excluded(["voice"]));
    let labels = engine
        .prompt_candidates(Path::new("/workspace"), "/")
        .expect("in-memory slash candidates")
        .into_iter()
        .map(|candidate| candidate.label)
        .collect::<Vec<_>>();
    assert!(labels.contains(&"/teleport".to_owned()));
    assert!(!labels.contains(&"/voice".to_owned()));
}

#[test]
fn a_path_popup_keeps_every_ranked_candidate() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    for index in 0..15 {
        fs::write(
            temporary.path().join(format!("file-{index:02}.txt")),
            "fixture",
        )
        .expect("path candidate fixture");
    }
    let mut engine = CompletionEngine::default();
    let candidates = engine
        .prompt_candidates(temporary.path(), "@")
        .expect("path candidates");
    assert_eq!(candidates.len(), 15);
    engine.install(0, 0..1, "@", candidates);
    assert_eq!(engine.view().expect("path popup").candidates.len(), 15);
}

/// Reference `PathCompletionController.on_key` since v2.25.7: a mention
/// query in flight owns Tab and Enter, so neither submits the prompt.
#[test]
fn tab_and_enter_wait_for_a_mention_query_in_flight() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    fs::write(temporary.path().join("notes.md"), "fixture").expect("fixture");
    let mut engine = CompletionEngine::default();
    let mut editor = PromptEditor::default();
    editor.insert("@no");
    let (range, query) = active_token(&editor).expect("a mention token");
    engine.requery();
    let request = CompletionRequest::new(engine.generation(), range, query);
    assert!(
        engine
            .dispatch_request(request, temporary.path())
            .expect("dispatched")
            .is_none(),
        "a mention query runs on the worker"
    );
    for key in [CompletionKey::Tab, CompletionKey::Enter] {
        assert_eq!(
            engine.handle_key(key, &mut editor).expect("handled"),
            CompletionKeyOutcome::Consumed
        );
    }
    assert_eq!(
        engine
            .handle_key(CompletionKey::Down, &mut editor)
            .expect("handled"),
        CompletionKeyOutcome::Ignored
    );
    engine
        .wait_for_pending(&editor)
        .expect("the answer arrives");
    assert_eq!(
        engine
            .handle_key(CompletionKey::Enter, &mut editor)
            .expect("handled"),
        CompletionKeyOutcome::Refresh,
        "once answered, Enter accepts the highlighted row"
    );
}

/// A mention query is answered either on the completion worker or through
/// the synchronous adapter. Both read the same index, so an unchanged root
/// is walked once however many queries arrive on either path.
#[test]
fn both_query_paths_share_one_walk_of_the_workspace() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    fs::write(temporary.path().join("alpha.txt"), "fixture").expect("path fixture");
    let mut engine = CompletionEngine {
        generation: 1,
        ..CompletionEngine::default()
    };

    for query in ["@a", "@al"] {
        let request = CompletionRequest::new(1, 0..query.chars().count(), query.to_owned());
        engine
            .dispatch_request(request, temporary.path())
            .expect("the worker accepts the request");
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut resolved = Vec::new();
    while std::time::Instant::now() < deadline && resolved.is_empty() {
        resolved.extend(engine.poll());
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!resolved.is_empty(), "the worker answered a mention query");

    let request = CompletionRequest::new(1, 0..2, "@a".to_owned());
    let resolution = engine.resolve_request(request, temporary.path());
    assert!(
        matches!(resolution, CompletionResolution::Results { ref candidates, .. } if candidates
            .iter()
            .any(|candidate| candidate.label == "@alpha.txt")),
        "the synchronous adapter answers from the same index"
    );
    assert_eq!(
        engine.path_index.rebuilds(),
        1,
        "one root, one walk, whichever path answered"
    );
}

/// The head word the popup searches for, measured at the boundary the
/// reference measures it at: `CommandCompleter._head_word` reads
/// `head[1:min(cursor, len(head))]` and `_fuzzy_filter` reads
/// `search_str[1:]`, so a command line loses exactly one marker no matter
/// how many it carries, and a caret before the marker searches for nothing.
#[test]
fn one_marker_is_stripped_and_the_caret_bounds_the_head_word() {
    assert_eq!(
        slash_token("//", 2),
        Some((0..2, "//".to_owned())),
        "the second marker stays inside the query"
    );
    assert_eq!(slash_token("///", 3), Some((0..3, "///".to_owned())));
    assert_eq!(
        slash_token("/help", 0),
        Some((0..5, "/".to_owned())),
        "a caret before the marker searches for the empty head word"
    );
    assert_eq!(
        slash_token("/mcp add x", 3),
        Some((0..4, "/mc".to_owned())),
        "the head word ends at the caret and the range at the first whitespace"
    );
    assert_eq!(slash_token("/mcp\nnext", 2), Some((0..4, "/m".to_owned())));
    assert_eq!(slash_token("/", 1), Some((0..1, "/".to_owned())));
    assert_eq!(
        slash_token("/help", 6),
        None,
        "`SlashCommandController.on_text_changed` drops a caret past the text"
    );
    assert_eq!(slash_token("help", 4), None, "a bare line is not a command");
}

/// Reference `SlashCommandController.on_text_changed`: a re-query that
/// answers the same list keeps the highlighted row, and one that answers
/// another list starts over at the top.
#[test]
fn a_requery_with_the_same_list_keeps_the_highlight() {
    let workspace = Path::new("/workspace");
    let mut editor = PromptEditor::default();
    editor.set_text("/co x");
    let mut engine = CompletionEngine::default();
    engine
        .refresh(&editor, workspace)
        .expect("slash completion");
    assert!(engine.move_selection(1));
    editor.move_left(false);
    engine
        .refresh(&editor, workspace)
        .expect("slash completion");
    assert_eq!(engine.view().map(|view| view.selected), Some(1));

    editor.set_text("/c x");
    engine
        .refresh(&editor, workspace)
        .expect("slash completion");
    assert_eq!(engine.view().map(|view| view.selected), Some(0));
}

/// `ChatInputContainer._format_insertion`, branch by branch.
#[test]
fn an_accepted_candidate_takes_a_separator_only_when_a_tail_follows_it() {
    assert_eq!(insertion_separator("/mcp", ""), "");
    assert_eq!(insertion_separator("/mcp", "p add x"), " ");
    assert_eq!(insertion_separator("/mcp", " add x"), "");
    assert_eq!(insertion_separator("@src/", ""), "");
    assert_eq!(insertion_separator("@src/", "tail"), "");
    assert_eq!(
        insertion_separator("@src/main.rs", ""),
        " ",
        "a file mention closes with a space even at the end of the line"
    );
    assert_eq!(insertion_separator("@src/main.rs", " tail"), "");
}

/// What those head words rank to, read from the engine the composer drives.
#[test]
fn the_slash_popup_answers_the_reference_edges_of_a_command_line() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let mut engine = CompletionEngine::default();
    let available = command_aliases_in(engine.command_context()).count();
    assert!(available > 1, "the default context publishes commands");
    let mut editor = PromptEditor::default();

    for typo in ["//", "///"] {
        editor.set_text(typo);
        engine
            .refresh(&editor, temporary.path())
            .expect("a doubled marker is answered");
        assert!(
            engine.view().is_none(),
            "{typo} matches no alias, so the reference shows no popup"
        );
    }

    editor.set_text("/");
    engine
        .refresh(&editor, temporary.path())
        .expect("a bare marker is answered");
    assert_eq!(
        engine
            .view()
            .expect("a bare marker opens the popup")
            .candidates
            .len(),
        available,
        "a bare marker lists every available command"
    );

    editor.set_text("/help");
    editor.select(0..0);
    engine
        .refresh(&editor, temporary.path())
        .expect("a caret before the marker is answered");
    let view = engine.view().expect("offset zero still opens the popup");
    assert_eq!(view.candidates.len(), available);
    assert_eq!(
        view.candidates[view.selected].label, "/help",
        "the empty head word keeps the reference's promotion order"
    );
}
