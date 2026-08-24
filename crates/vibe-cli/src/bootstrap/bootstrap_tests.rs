//! The two launches, told apart by what they put on the session options.

use std::path::Path;

use super::{Launch, session_options};

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
