//! The two launches, told apart by what they put on the session options.

use std::path::Path;

use super::{Budgets, Launch, session_options};

/// An interactive launch declares nothing headless and withholds nothing.
///
/// It is the only other caller of this constructor
/// (`crates/vibe-cli/src/tui/session.rs:58`), so what it passes decides what
/// an interactive session opens with. The reference sets neither on that
/// branch (`vibe/cli/cli.py:209-272`).
#[test]
fn an_interactive_launch_sets_neither_the_flag_nor_the_two_names() {
    let mut arguments = crate::arguments_for_test();
    arguments.disabled_tools = vec!["shell".to_owned()];
    let options = session_options(
        &arguments,
        Path::new("/workspace"),
        "test".to_owned(),
        None,
        None,
        Launch::Interactive,
    );

    assert!(!options.headless);
    assert_eq!(options.disabled_tools, ["shell"]);
}

/// A programmatic launch appends the two names to what the user wrote, and
/// appends neither twice.
#[test]
fn a_programmatic_launch_appends_the_two_names_once_each() {
    let mut arguments = crate::arguments_for_test();
    arguments.disabled_tools = vec!["shell".to_owned(), "exit_plan_mode".to_owned()];
    let options = session_options(
        &arguments,
        Path::new("/workspace"),
        "test".to_owned(),
        None,
        None,
        Launch::Programmatic,
    );

    assert!(options.headless);
    assert_eq!(
        options.disabled_tools,
        ["shell", "exit_plan_mode", "ask_user_question"],
        "the user's order is kept and neither name is repeated"
    );
}

/// The budgets are programmatic options: an interactive launch drops them, as
/// the reference's interactive session options name none
/// (`vibe/cli/cli.py:271-279`).
#[test]
fn only_a_programmatic_launch_carries_the_budgets() {
    let mut arguments = crate::arguments_for_test();
    arguments.max_turns = Some(3);
    arguments.max_tokens = Some(100);
    arguments.max_price = Some(0.5);
    let launch = |launch| {
        session_options(
            &arguments,
            Path::new("/workspace"),
            "test".to_owned(),
            None,
            None,
            launch,
        )
    };
    let interactive = launch(Launch::Interactive);
    assert_eq!(
        (
            interactive.max_turns,
            interactive.max_tokens,
            interactive.max_price_micros
        ),
        (None, None, None)
    );
    let programmatic = launch(Launch::Programmatic);
    assert_eq!(
        (
            programmatic.max_turns,
            programmatic.max_tokens,
            programmatic.max_price_micros
        ),
        (Some(3), Some(100), Some(500_000))
    );
}

/// The reference compares each budget as given: a turn budget at or below zero
/// stops before the first turn, and so does a token or price budget below zero,
/// which is already exceeded (`vibe/core/middleware.py:48-96`).
#[test]
fn the_budgets_keep_their_sign_as_the_reference_compares_them() {
    let budgets = |turns, tokens, price| {
        let mut arguments = crate::arguments_for_test();
        arguments.max_turns = turns;
        arguments.max_tokens = tokens;
        arguments.max_price = price;
        Budgets::of(&arguments)
    };
    // A budget below zero is one the session spent before it starts, which
    // the middleware answers with its own limit rather than the turn budget.
    assert_eq!(budgets(Some(-5), None, None).max_turns, Some(-5));
    let tokens = budgets(Some(7), Some(-1), None);
    assert_eq!((tokens.max_turns, tokens.max_tokens), (Some(7), Some(-1)));
    let price = budgets(None, None, Some(-2.5));
    assert_eq!(
        (price.max_turns, price.max_price_micros),
        (None, Some(-2_500_000))
    );
    let zero = budgets(None, Some(0), Some(0.0));
    assert_eq!(
        (zero.max_turns, zero.max_tokens, zero.max_price_micros),
        (None, Some(0), Some(0))
    );
    // A price that is not a number never compares greater; an infinite one
    // saturates into the value that means no budget.
    assert_eq!(budgets(None, None, Some(f64::NAN)).max_price_micros, None);
    assert_eq!(
        budgets(None, None, Some(f64::INFINITY)).max_price_micros,
        Some(i64::MAX)
    );
}
