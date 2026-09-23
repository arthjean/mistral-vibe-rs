use super::*;

fn context(teleport: bool) -> CommandContext {
    let context = CommandContext::default();
    if teleport {
        context
    } else {
        context.with_excluded(["teleport"])
    }
}

#[test]
fn classification_follows_the_reference_order_on_a_stripped_line() {
    let offered = context(true);
    assert_eq!(classify("!", &offered, None), Submission::EmptyShell);
    assert_eq!(
        classify("!ls -la", &offered, None),
        Submission::Shell("ls -la".to_owned())
    );
    // The command is what follows the `!`, spaces included, the way reference
    // `classify` slices it.
    assert_eq!(
        classify("! ls", &offered, None),
        Submission::Shell(" ls".to_owned())
    );
    assert_eq!(
        classify("&ship it", &offered, None),
        Submission::Teleport("ship it".to_owned())
    );
    assert_eq!(
        classify("&ship it", &context(false), None),
        Submission::Prompt
    );
    assert_eq!(
        classify("/status", &offered, None),
        Submission::Command { side_channel: true }
    );
    assert_eq!(
        classify("exit", &offered, None),
        Submission::Command { side_channel: true }
    );
    assert_eq!(
        classify("/model", &offered, None),
        Submission::Command {
            side_channel: false
        }
    );
    // A slash line that names neither a command nor a skill is a prompt.
    assert_eq!(classify("/unknown", &offered, None), Submission::Prompt);
    assert_eq!(classify("hello", &offered, None), Submission::Prompt);
}

#[test]
fn a_running_shell_refuses_everything_but_the_side_channel() {
    let shell = Occupancy {
        shell: true,
        ..Occupancy::default()
    };
    assert_eq!(
        route(Submission::Command { side_channel: true }, shell, true),
        Route::Command
    );
    assert_eq!(
        route(Submission::Prompt, shell, true),
        Route::Refuse {
            reason: Refusal::ShellRunning,
            hint: Hint::Busy
        }
    );
    assert_eq!(
        route(Submission::EmptyShell, shell, true),
        Route::Refuse {
            reason: Refusal::ShellRunning,
            hint: Hint::Busy
        }
    );
}

#[test]
fn a_paused_queue_queues_a_prompt_and_resumes() {
    let paused = Occupancy {
        paused: true,
        turn: true,
        ..Occupancy::default()
    };
    assert_eq!(
        route(Submission::Prompt, paused, true),
        Route::Queue {
            skill: false,
            resume: true
        }
    );
    assert_eq!(
        route(Submission::Shell("ls".to_owned()), paused, true),
        Route::Refuse {
            reason: Refusal::Shell,
            hint: Hint::Paused
        }
    );
    assert_eq!(
        route(Submission::Command { side_channel: true }, paused, false),
        Route::Refuse {
            reason: Refusal::SideChannelBusy,
            hint: Hint::None
        }
    );
}
