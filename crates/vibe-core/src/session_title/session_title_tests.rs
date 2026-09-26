use super::*;

#[test]
fn the_opening_title_waits_for_the_turn_to_answer_or_the_step_ceiling() {
    let mut cadence = TitleCadence::default();
    assert!(cadence.begin_if_due(true, false).is_none());
    assert!(
        cadence.begin_if_due(true, true).is_some(),
        "the answer titles it"
    );

    let mut tool_heavy = TitleCadence::default();
    assert!(tool_heavy.begin_if_due(true, false).is_none());
    assert!(tool_heavy.begin_if_due(true, false).is_none());
    assert!(
        tool_heavy.begin_if_due(true, false).is_some(),
        "the third step titles a turn still calling tools"
    );
}

#[test]
fn the_fast_model_refreshes_periodically_and_a_fallback_is_capped() {
    let mut fast = TitleCadence::default();
    assert!(fast.begin_if_due(true, true).is_some());
    for _ in 0..REFRESH_EVERY_STEPS - 1 {
        assert!(fast.begin_if_due(true, true).is_none());
    }
    assert!(fast.begin_if_due(true, true).is_some());

    let mut capped = TitleCadence::default();
    assert!(capped.begin_if_due(false, true).is_some());
    for _ in 0..3 * REFRESH_EVERY_STEPS {
        assert!(capped.begin_if_due(false, true).is_none());
    }
    capped.mark_compaction();
    assert!(capped.begin_if_due(false, true).is_some());
    capped.mark_compaction();
    assert!(
        capped.begin_if_due(false, true).is_none(),
        "the fallback model titles at most twice"
    );
}

#[test]
fn a_title_that_did_not_land_is_due_again() {
    let mut cadence = TitleCadence::default();
    let ticket = cadence.begin_if_due(true, true).expect("due");
    cadence.restore(ticket);
    assert!(
        cadence.begin_if_due(true, true).is_some(),
        "the next answer titles it again, well before a periodic refresh"
    );
}

#[test]
fn the_transcript_skips_the_system_prompt_and_keeps_both_ends_of_a_long_one() {
    let messages = [
        ModelMessage::System {
            content: "prompt".to_owned(),
        },
        ModelMessage::user("  Plan the parser  "),
        ModelMessage::user(""),
    ];
    assert_eq!(build_title_transcript(&messages), "user: Plan the parser");

    let long: Vec<ModelMessage> = (0..10)
        .map(|index| ModelMessage::user(format!("{index}{}", "x".repeat(1_900))))
        .collect();
    let transcript = build_title_transcript(&long);
    assert!(transcript.starts_with("user: 0"));
    assert!(transcript.contains(ELISION));
    assert!(transcript.ends_with(&"x".repeat(100)));
    assert_eq!(
        transcript.chars().count(),
        MAX_TRANSCRIPT_CHARS + ELISION.chars().count()
    );
}

#[test]
fn a_title_is_one_clean_line_and_a_generic_one_is_none() {
    assert_eq!(
        clean_title(Some("  \"Parser   refactor  plan\"\nsecond line")),
        Some("Parser refactor plan".to_owned())
    );
    // A control character is dropped before whitespace is collapsed, so a tab
    // joins the words it separated, as the reference's does.
    assert_eq!(
        clean_title(Some("Parser\trefactor")),
        Some("Parserrefactor".to_owned())
    );
    assert_eq!(clean_title(Some("New Session")), None);
    assert_eq!(clean_title(Some("   ")), None);
    assert_eq!(clean_title(None), None);
    let capped = clean_title(Some(&"a".repeat(100))).expect("a title");
    assert_eq!(capped.chars().count(), MAX_TITLE_CHARS + 1);
    assert!(capped.ends_with('…'));
}

#[test]
fn a_previous_title_is_offered_for_refinement() {
    assert_eq!(user_prompt("user: hi", None), "user: hi");
    assert!(user_prompt("user: hi", Some("Greeting")).starts_with("Previous title: Greeting"));
}
