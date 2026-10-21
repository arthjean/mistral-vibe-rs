use super::*;

fn commands(raw: &[&str]) -> Vec<String> {
    raw.iter().map(|command| (*command).to_owned()).collect()
}

#[tokio::test]
async fn one_upgrade_command_exiting_zero_carries_the_whole_upgrade() {
    assert_eq!(
        run_upgrade_commands(&commands(&["exit 1", "exit 0"]), std::future::pending()).await,
        UpgradeOutcome::Succeeded,
        "the reference's any-succeeded rule ignores the commands that failed"
    );
    assert_eq!(
        run_upgrade_commands(&commands(&["exit 1", "exit 3"]), std::future::pending()).await,
        UpgradeOutcome::Failed
    );
    assert_eq!(
        run_upgrade_commands(
            &commands(&["definitely-not-an-installed-command"]),
            std::future::pending()
        )
        .await,
        UpgradeOutcome::Failed,
        "a command the shell cannot run is a command that did not exit zero"
    );
    assert_eq!(
        run_upgrade_commands(&[], std::future::pending()).await,
        UpgradeOutcome::Failed
    );
}

#[tokio::test]
async fn cancelling_an_upgrade_terminates_the_running_command() {
    let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = sender.send(());
    });
    let sleeper = if cfg!(windows) {
        "ping -n 60 127.0.0.1 > NUL"
    } else {
        "sleep 60"
    };
    let started = std::time::Instant::now();
    let outcome = run_upgrade_commands(&commands(&[sleeper, "exit 0"]), async {
        let _ = receiver.await;
    })
    .await;
    assert_eq!(
        outcome,
        UpgradeOutcome::Cancelled,
        "cancellation stops the upgrade instead of running the commands after it"
    );
    assert!(
        started.elapsed() < UPGRADE_TERMINATE_GRACE,
        "a command that answers termination never reaches the kill rung"
    );
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn a_command_that_ignores_termination_is_killed_after_the_grace() {
    // Time is paused, so the two-second grace elapses as soon as nothing
    // else can run: what this measures is the escalation, not the wait.
    let outcome = run_upgrade_commands(
        &commands(&["trap '' TERM; sleep 60"]),
        std::future::ready(()),
    )
    .await;
    assert_eq!(outcome, UpgradeOutcome::Cancelled);
}
