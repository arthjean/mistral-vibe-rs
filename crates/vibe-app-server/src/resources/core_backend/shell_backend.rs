use super::*;
use vibe_core::process::{ProcessRead, TerminalState};

impl CoreResourceBackend {
    pub(super) async fn dispatch_shell(
        &self,
        session: &CoreResourceSession,
        command: &ShellCommand,
    ) -> Result<ResourceDispatch, ResourceError> {
        let state = match command {
            ShellCommand::Run {
                operation_id,
                command,
            } => {
                let existing_terminal = session
                    .shell_operations
                    .lock()
                    .await
                    .get(operation_id.as_str())
                    .cloned();
                match existing_terminal {
                    Some(terminal_id) => poll_shell(session, operation_id, terminal_id).await?,
                    None => start_shell(session, operation_id, command).await?,
                }
            }
            ShellCommand::Interrupt { operation_id } => {
                interrupt_shell(session, operation_id).await?
            }
        };
        Ok(canonical_mutation("shell", state, Vec::new()))
    }
}

/// What a started operation printed so far. Once it has ended, the rest of
/// its output is collected and its terminal released, so the read that
/// reports the end is the operation's last.
async fn poll_shell(
    session: &CoreResourceSession,
    operation_id: &str,
    terminal_id: String,
) -> Result<Value, ResourceError> {
    let mut output = session
        .terminals
        .read(&terminal_id)
        .await
        .map_err(terminal_failure)?;
    if !matches!(output.state, TerminalState::Running) {
        let final_output = session
            .terminals
            .wait(&terminal_id)
            .await
            .map_err(terminal_failure)?;
        output.chunks.extend(final_output.chunks);
        output.state = final_output.state;
        output.backpressure_dropped |= final_output.backpressure_dropped;
        let removed = session
            .shell_operations
            .lock()
            .await
            .remove(operation_id)
            .is_some_and(|candidate| candidate == terminal_id);
        if removed {
            session
                .terminals
                .release(&terminal_id)
                .await
                .map_err(terminal_failure)?;
        }
    }
    Ok(operation_state(operation_id, &terminal_id, &output))
}

/// Starts `command` as `operation_id`, which only a trusted workspace may do
/// and only for a command the shell analysis allows outright.
async fn start_shell(
    session: &CoreResourceSession,
    operation_id: &str,
    command: &str,
) -> Result<Value, ResourceError> {
    if !matches!(
        session
            .policy
            .try_trust_decision(&session.working_directory)
            .map_err(policy_error)?,
        Some(TrustDecision::Trusted | TrustDecision::SessionTrusted)
    ) {
        return Err(ResourceError::Conflict(
            "manual shell requires a trusted workspace".to_owned(),
        ));
    }
    let platform = host_platform();
    let working_directory = parse_policy_path(platform, &session.working_directory)
        .map_err(|error| ResourceError::InvalidParams(error.to_string()))?;
    // The manual shell resource reads the same `bash` lists the tool does,
    // through the session's own permission store.
    let lists = ShellCommandLists::from_config(
        &session
            .policy
            .tool_config()
            .view::<ShellCommandConfig>("bash"),
    );
    let shell = ShellConfig::default_for(platform);
    let analysis = analyze_shell(
        shell.flavor,
        command,
        // The manual shell resource only ever runs a command the analysis
        // allows outright, so it names no scratchpad: a scratchpad operand
        // raises a requirement here and the operation is refused rather than
        // run unattended.
        &ShellPolicyContext::new(platform, working_directory),
        &lists,
    );
    if analysis.mode != PermissionMode::Always {
        return Err(ResourceError::Conflict(format!(
            "shell operation `{operation_id}` requires explicit approval: {}",
            analysis.rationale.join("; ")
        )));
    }
    let mut spec = ProcessSpec::new(shell.executable, PathBuf::from(&session.working_directory));
    spec.arguments = shell
        .arguments
        .into_iter()
        .chain(std::iter::once(command.to_owned()))
        .collect();
    let terminal_id = session
        .terminals
        .run(spec)
        .await
        .map_err(terminal_failure)?;
    if let Err(error) = session.terminals.close_stdin(&terminal_id).await {
        discard_terminal(session, &terminal_id).await;
        return Err(terminal_failure(error));
    }
    let mut operations = session.shell_operations.lock().await;
    if operations.contains_key(operation_id) {
        drop(operations);
        discard_terminal(session, &terminal_id).await;
        return Err(ResourceError::Conflict(format!(
            "shell operation `{operation_id}` already exists"
        )));
    }
    operations.insert(operation_id.to_owned(), terminal_id.clone());
    Ok(json!({
        "operationId": operation_id,
        "terminalId": terminal_id,
        "status": "running"
    }))
}

async fn interrupt_shell(
    session: &CoreResourceSession,
    operation_id: &str,
) -> Result<Value, ResourceError> {
    let terminal_id = session
        .shell_operations
        .lock()
        .await
        .remove(operation_id)
        .ok_or_else(|| {
            ResourceError::NotFound(format!("shell operation `{operation_id}` was not found"))
        })?;
    let output = session
        .terminals
        .interrupt(&terminal_id)
        .await
        .map_err(terminal_failure)?;
    session
        .terminals
        .release(&terminal_id)
        .await
        .map_err(terminal_failure)?;
    Ok(operation_state(operation_id, &terminal_id, &output))
}

/// Ends and forgets a terminal no operation will own, best effort: the
/// failure that made it unowned is the one the caller reports.
async fn discard_terminal(session: &CoreResourceSession, terminal_id: &str) {
    let _ = session.terminals.interrupt(terminal_id).await;
    let _ = session.terminals.release(terminal_id).await;
}

/// The `shell` state of an operation whose output was read.
fn operation_state(operation_id: &str, terminal_id: &str, output: &ProcessRead) -> Value {
    json!({
        "operationId": operation_id,
        "terminalId": terminal_id,
        "status": output.state,
        "output": output,
    })
}

fn terminal_failure(error: vibe_core::process::ProcessError) -> ResourceError {
    ResourceError::Unavailable(redact(&error.to_string()))
}
