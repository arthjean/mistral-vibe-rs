use super::*;

impl CoreResourceBackend {
    pub(super) async fn dispatch_shell(
        &self,
        session: &CoreResourceSession,
        command: &ShellCommand,
    ) -> Result<ResourceDispatch, ResourceError> {
        match command {
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
                if let Some(terminal_id) = existing_terminal {
                    let mut output = session
                        .terminals
                        .read(&terminal_id)
                        .await
                        .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
                    if !matches!(output.state, vibe_core::process::TerminalState::Running) {
                        let final_output =
                            session
                                .terminals
                                .wait(&terminal_id)
                                .await
                                .map_err(|error| {
                                    ResourceError::Unavailable(redact(&error.to_string()))
                                })?;
                        output.chunks.extend(final_output.chunks);
                        output.state = final_output.state;
                        output.backpressure_dropped |= final_output.backpressure_dropped;
                        let removed = session
                            .shell_operations
                            .lock()
                            .await
                            .remove(operation_id.as_str())
                            .is_some_and(|candidate| candidate == terminal_id);
                        if removed {
                            session
                                .terminals
                                .release(&terminal_id)
                                .await
                                .map_err(|error| {
                                    ResourceError::Unavailable(redact(&error.to_string()))
                                })?;
                        }
                    }
                    let status = output.state.clone();
                    let state = json!({
                        "operationId": operation_id,
                        "terminalId": terminal_id,
                        "status": status,
                        "output": output,
                    });
                    return Ok(canonical_mutation(
                        "shell",
                        state,
                        "shell/updated",
                        Vec::new(),
                    ));
                }
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
                let analysis = analyze_shell(
                    ShellConfig::default_for(platform).flavor,
                    command,
                    &ShellPolicyContext {
                        platform,
                        working_directory: working_directory.clone(),
                        roots: vec![working_directory],
                    },
                );
                if analysis.mode != PermissionMode::Always {
                    return Err(ResourceError::Conflict(format!(
                        "shell operation `{operation_id}` requires explicit approval: {}",
                        analysis.rationale.join("; ")
                    )));
                }
                let shell = ShellConfig::default_for(platform);
                let mut spec =
                    ProcessSpec::new(shell.executable, PathBuf::from(&session.working_directory));
                spec.arguments = shell
                    .arguments
                    .into_iter()
                    .chain(std::iter::once(command.clone()))
                    .collect();
                let terminal_id = session
                    .terminals
                    .run(spec)
                    .await
                    .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
                if let Err(error) = session.terminals.close_stdin(&terminal_id).await {
                    let _ = session.terminals.interrupt(&terminal_id).await;
                    let _ = session.terminals.release(&terminal_id).await;
                    return Err(ResourceError::Unavailable(redact(&error.to_string())));
                }
                let mut operations = session.shell_operations.lock().await;
                if operations.contains_key(operation_id.as_str()) {
                    drop(operations);
                    let _ = session.terminals.interrupt(&terminal_id).await;
                    let _ = session.terminals.release(&terminal_id).await;
                    return Err(ResourceError::Conflict(format!(
                        "shell operation `{operation_id}` already exists"
                    )));
                }
                operations.insert(operation_id.clone(), terminal_id.clone());
                let state = json!({
                    "operationId": operation_id,
                    "terminalId": terminal_id,
                    "status": "running"
                });
                Ok(canonical_mutation(
                    "shell",
                    state,
                    "shell/updated",
                    Vec::new(),
                ))
            }
            ShellCommand::Interrupt { operation_id } => {
                let terminal_id = session
                    .shell_operations
                    .lock()
                    .await
                    .remove(operation_id.as_str())
                    .ok_or_else(|| {
                        ResourceError::NotFound(format!(
                            "shell operation `{operation_id}` was not found"
                        ))
                    })?;
                let output = session
                    .terminals
                    .interrupt(&terminal_id)
                    .await
                    .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
                session
                    .terminals
                    .release(&terminal_id)
                    .await
                    .map_err(|error| ResourceError::Unavailable(redact(&error.to_string())))?;
                let status = output.state.clone();
                let state = json!({
                    "operationId": operation_id,
                    "terminalId": terminal_id,
                    "status": status,
                    "output": output,
                });
                Ok(canonical_mutation(
                    "shell",
                    state,
                    "shell/updated",
                    Vec::new(),
                ))
            }
        }
    }
}
