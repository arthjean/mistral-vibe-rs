//! Running one hook process.
//!
//! Reference `vibe/core/hooks/executor.py`: the command runs through the
//! platform shell in a session of its own, in the session's directory, reads
//! its invocation on stdin, and answers on stdout within its timeout. Each
//! stream is kept up to a mebibyte and drained past it, so a chatty hook is
//! truncated rather than blocked. A hook is under no obligation to read its
//! stdin, so a broken pipe while writing it is not a failure: the hook's own
//! output and exit status answer for it.

use std::io;
use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;

use super::models::{HookConfig, HookExecutionResult};
use crate::child::ChildGroup;

/// Reference `_MAX_OUTPUT_BYTES`.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Runs `hook` with `stdin` as its input. Reference `HookExecutor.run`.
pub(crate) async fn run_hook(hook: &HookConfig, stdin: &[u8], cwd: &Path) -> HookExecutionResult {
    let failed = |message: String| HookExecutionResult {
        hook_name: hook.name.clone(),
        exit_code: Some(1),
        stdout: String::new(),
        stderr: message,
        timed_out: false,
    };
    let mut command = shell_command(&hook.command);
    command.current_dir(cwd);
    let (mut child, pipes) = match ChildGroup::spawn(&mut command) {
        Ok(spawned) => spawned,
        Err(error) => return failed(format!("Failed to start: {}", python_os_error(&error, cwd))),
    };
    let Some(mut child_stdin) = pipes.stdin else {
        let _ = child.signal(true);
        let _ = child.wait().await;
        return failed("Failed to start: stdin stream unavailable".to_owned());
    };
    let stdout = pipes.stdout;
    let stderr = pipes.stderr;
    let exchange = async {
        let written = async {
            child_stdin.write_all(stdin).await?;
            child_stdin.flush().await
        }
        .await;
        drop(child_stdin);
        if let Err(error) = written
            && !matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
            )
        {
            return Err(error);
        }
        let (stdout, stderr) = tokio::join!(read_capped(stdout), read_capped(stderr));
        let status = child.wait().await?;
        Ok((stdout?, stderr?, status))
    };
    let deadline = timeout_duration(hook.timeout);
    let outcome = match deadline {
        Some(deadline) => tokio::time::timeout(deadline, exchange).await.ok(),
        None => Some(exchange.await),
    };
    match outcome {
        Some(Ok((stdout, stderr, status))) => HookExecutionResult {
            hook_name: hook.name.clone(),
            exit_code: status.code.or_else(|| status.signal.map(|signal| -signal)),
            stdout: decode_console(&stdout).trim().to_owned(),
            stderr: decode_console(&stderr).trim().to_owned(),
            timed_out: false,
        },
        Some(Err(error)) => {
            let _ = child.signal(true);
            let _ = child.wait().await;
            failed(format!("Failed to start: {error}"))
        }
        None => {
            // Reference `kill_async_subprocess`: SIGKILL to the whole group.
            let _ = child.signal(true);
            let _ = child.wait().await;
            HookExecutionResult {
                hook_name: hook.name.clone(),
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: true,
            }
        }
    }
}

/// `asyncio.wait_for` times out at once for a deadline of zero or less and
/// never for an infinite or NaN one.
fn timeout_duration(seconds: f64) -> Option<Duration> {
    if seconds.is_nan() || seconds.is_infinite() && seconds > 0.0 {
        return None;
    }
    Some(Duration::try_from_secs_f64(seconds.max(0.0)).unwrap_or(Duration::MAX))
}

/// Reference `create_subprocess_shell`: `/bin/sh -c` on POSIX, `cmd.exe /c`
/// through `COMSPEC` on Windows.
fn shell_command(command_line: &str) -> Command {
    #[cfg(unix)]
    {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(command_line);
        command
    }
    #[cfg(windows)]
    {
        let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
        let mut command = Command::new(shell);
        command.raw_arg(format!("/c {command_line}"));
        command
    }
}

/// Reference `_read_capped`: keeps the first mebibyte and drains the rest.
async fn read_capped(stream: Option<impl AsyncRead + Unpin>) -> io::Result<Vec<u8>> {
    let Some(mut stream) = stream else {
        return Ok(Vec::new());
    };
    let mut kept = Vec::new();
    let mut buffer = vec![0_u8; 65_536];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let room = MAX_OUTPUT_BYTES.saturating_sub(kept.len()).min(read);
        kept.extend_from_slice(buffer.get(..room).unwrap_or_default());
    }
    Ok(kept)
}

/// Reference `decode_console_safe`: the encoding a byte-order mark names, with
/// the mark dropped, then UTF-8, then Latin-1, which every byte decodes, with
/// line endings left as written.
fn decode_console(bytes: &[u8]) -> String {
    let marked = if let Some(rest) = bytes.strip_prefix(b"\xef\xbb\xbf") {
        std::str::from_utf8(rest).ok().map(str::to_owned)
    } else if let Some(rest) = bytes.strip_prefix(b"\xff\xfe\x00\x00") {
        decode_utf32(rest, u32::from_le_bytes)
    } else if let Some(rest) = bytes.strip_prefix(b"\x00\x00\xfe\xff") {
        decode_utf32(rest, u32::from_be_bytes)
    } else if let Some(rest) = bytes.strip_prefix(b"\xff\xfe") {
        decode_utf16(rest, u16::from_le_bytes)
    } else if let Some(rest) = bytes.strip_prefix(b"\xfe\xff") {
        decode_utf16(rest, u16::from_be_bytes)
    } else {
        None
    };
    if let Some(text) = marked {
        return text;
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => bytes.iter().map(|byte| char::from(*byte)).collect(),
    }
}

/// UTF-16 in the byte order a mark named, or `None` when it does not decode.
fn decode_utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> Option<String> {
    let units = bytes
        .chunks(2)
        .map(|pair| <[u8; 2]>::try_from(pair).ok().map(unit))
        .collect::<Option<Vec<_>>>()?;
    String::from_utf16(&units).ok()
}

/// UTF-32 in the byte order a mark named, or `None` when it does not decode.
fn decode_utf32(bytes: &[u8], unit: fn([u8; 4]) -> u32) -> Option<String> {
    bytes
        .chunks(4)
        .map(|quad| {
            <[u8; 4]>::try_from(quad)
                .ok()
                .map(unit)
                .and_then(char::from_u32)
        })
        .collect()
}

/// Python's `str(OSError)` for a failed spawn, which names the path involved.
fn python_os_error(error: &io::Error, cwd: &Path) -> String {
    let description = error.to_string();
    let description = description
        .split_once(" (os error")
        .map_or(description.as_str(), |(text, _)| text);
    match error.raw_os_error() {
        Some(code) => format!(
            "[Errno {code}] {description}: {}",
            crate::mcp::render::python_string(&cwd.to_string_lossy())
        ),
        None => description.to_owned(),
    }
}
