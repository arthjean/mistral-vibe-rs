use std::process::Stdio;
use std::time::Duration;

use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub(super) const EXTERNAL_ACTION_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error, PartialEq, Eq)]
pub(super) enum ExternalActionError {
    #[error("external helper is unavailable")]
    Unavailable,
    #[error("external helper timed out")]
    Timeout,
    #[error("external helper exited unsuccessfully")]
    Failed,
    #[error("external helper I/O failed")]
    Io,
}

pub(super) async fn run_command(
    program: &str,
    arguments: &[&str],
    input: Option<Vec<u8>>,
) -> Result<(), ExternalActionError> {
    run_command_with_timeout(program, arguments, input, EXTERNAL_ACTION_TIMEOUT).await
}

async fn run_command_with_timeout(
    program: &str,
    arguments: &[&str],
    input: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<(), ExternalActionError> {
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| ExternalActionError::Unavailable)?;
    let mut stdin = child.stdin.take();
    let outcome = tokio::time::timeout(timeout, async {
        if let Some(input) = input {
            stdin
                .as_mut()
                .ok_or(ExternalActionError::Io)?
                .write_all(&input)
                .await
                .map_err(|_| ExternalActionError::Io)?;
        }
        drop(stdin);
        child.wait().await.map_err(|_| ExternalActionError::Io)
    })
    .await;
    match outcome {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(_)) => Err(ExternalActionError::Failed),
        Ok(Err(error)) => Err(error),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(ExternalActionError::Timeout)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_and_blocked_helpers_fail_after_being_reaped() {
        assert_eq!(
            run_command_with_timeout("/bin/sh", &["-c", "exit 7"], None, Duration::from_secs(1),)
                .await,
            Err(ExternalActionError::Failed)
        );
        assert_eq!(
            run_command_with_timeout(
                "/bin/sh",
                &["-c", "sleep 60"],
                None,
                Duration::from_millis(50),
            )
            .await,
            Err(ExternalActionError::Timeout)
        );
    }
}
