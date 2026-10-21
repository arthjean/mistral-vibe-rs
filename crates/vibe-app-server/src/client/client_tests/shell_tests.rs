//! A manual `!` command on a legacy session: `session/shellCommand` runs it
//! behind a `shell` entry, and its result reaches the driver when the session
//! saves nothing.

use super::*;

#[derive(Default)]
struct ContextRecordingDriver {
    context: Mutex<Vec<String>>,
}

impl TurnDriver for ContextRecordingDriver {
    fn run<'a>(&'a self, _reservation: &'a TurnReservation) -> DriverFuture<'a> {
        Box::pin(async { Err(DriverError::Tool("not executed".to_owned())) })
    }

    fn inject_context(
        &self,
        _session_id: &str,
        content: &str,
        _as_message: bool,
        _inject_invoked_skill: bool,
    ) -> Result<(), DriverError> {
        lock(&self.context).push(content.to_owned());
        Ok(())
    }
}

fn legacy_session(
    workspace: &Path,
) -> (
    HeadlessService<ContextRecordingDriver>,
    Arc<ContextRecordingDriver>,
    String,
) {
    let driver = Arc::new(ContextRecordingDriver::default());
    let mut service = HeadlessService::new_shared(Arc::clone(&driver)).expect("service starts");
    let mut session_options = options();
    session_options.working_directory = workspace.to_string_lossy().into_owned();
    let session_id = service
        .start_session(&session_options)
        .expect("session starts");
    assert!(!service.is_unified(&session_id));
    (service, driver, session_id)
}

fn shell_entry(
    service: &mut HeadlessService<ContextRecordingDriver>,
    session_id: &str,
) -> PublicHistoryEntry {
    service
        .session(session_id)
        .expect("session is readable")
        .snapshot
        .expect("session projects its history")
        .history
        .into_iter()
        .find(|entry| entry.metadata().id == "operation")
        .expect("the command projects its entry")
}

#[tokio::test]
async fn a_legacy_manual_command_projects_its_entry_and_hands_its_result_to_the_driver() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (mut service, driver, session_id) = legacy_session(workspace.path());

    let dispatch = service
        .public_call_async(
            "session/shellCommand",
            json!({
                "sessionId": session_id,
                "command": "echo ok",
                "operationId": "operation",
                "action": "run",
            }),
        )
        .await
        .expect("the command runs");

    assert_eq!(dispatch.result["accepted"], json!(true));
    let methods = dispatch
        .notifications
        .iter()
        .map(|notification| notification.method.as_str())
        .collect::<Vec<_>>();
    assert_eq!(methods.first(), Some(&"history/entryAdded"));
    assert!(
        methods[1..]
            .iter()
            .all(|method| *method == "history/entryUpdated")
    );
    let entry = shell_entry(&mut service, &session_id);
    assert!(matches!(
        &entry,
        PublicHistoryEntry::Effect {
            state: PublicEffectState::Completed { output_text, .. },
            ..
        } if output_text.trim_end() == "ok"
    ));
    let context = lock(&driver.context).clone();
    assert_eq!(context.len(), 1);
    assert!(context[0].contains("Command: `echo ok`"));
    assert!(context[0].contains("Exit code: 0"));
}

#[tokio::test]
async fn a_legacy_manual_command_settles_cancelled_once_interrupted() {
    let workspace = tempfile::tempdir().expect("workspace");
    let (mut service, driver, session_id) = legacy_session(workspace.path());

    let run = service
        .begin_public_call(
            "session/shellCommand",
            json!({
                "sessionId": session_id,
                "command": "sleep 30",
                "operationId": "operation",
                "action": "run",
            }),
        )
        .expect("the command is reserved");
    let run = tokio::spawn(run.complete());
    let interrupted = service
        .public_call_async(
            "session/shellCommand",
            json!({
                "sessionId": session_id,
                "operationId": "operation",
                "action": "interrupt",
            }),
        )
        .await
        .expect("the interrupt is answered");
    assert_eq!(interrupted.result["accepted"], json!(true));
    run.await
        .expect("the command task joins")
        .expect("the command is answered");

    assert!(matches!(
        shell_entry(&mut service, &session_id),
        PublicHistoryEntry::Effect {
            state: PublicEffectState::Cancelled { .. },
            ..
        }
    ));
    let context = lock(&driver.context).clone();
    assert!(context[0].contains("Status: interrupted by the user"));
}
