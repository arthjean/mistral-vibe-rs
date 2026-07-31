use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(target_os = "macos")]
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use thiserror::Error;

pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("clipboard is unavailable")]
    Unavailable,
    #[error("clipboard image paste is unsupported on this platform")]
    ImageUnsupported,
    #[error("clipboard image is too large ({actual} bytes, maximum {maximum})")]
    ImageTooLarge { actual: usize, maximum: usize },
    #[error("clipboard operation failed: {0}")]
    Operation(String),
}

pub trait SystemClipboardPort {
    fn copy_text(&self, text: &str) -> Result<(), ClipboardError>;
    fn paste_text(&self) -> Result<String, ClipboardError>;
    fn paste_image(&self, workspace: &Path) -> Result<Option<PathBuf>, ClipboardError>;
    fn supports_images(&self) -> bool;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClipboard;

impl SystemClipboardPort for SystemClipboard {
    fn copy_text(&self, text: &str) -> Result<(), ClipboardError> {
        if text.is_empty() {
            return Err(ClipboardError::Unavailable);
        }
        for (program, arguments) in copy_commands() {
            if write_command(program, arguments, text.as_bytes()).is_ok() {
                return Ok(());
            }
        }
        write_osc52(text)
    }

    fn paste_text(&self) -> Result<String, ClipboardError> {
        for (program, arguments) in paste_commands() {
            if let Ok(output) = Command::new(program).args(*arguments).output()
                && output.status.success()
            {
                return String::from_utf8(output.stdout)
                    .map_err(|error| ClipboardError::Operation(error.to_string()));
            }
        }
        Err(ClipboardError::Unavailable)
    }

    fn paste_image(&self, workspace: &Path) -> Result<Option<PathBuf>, ClipboardError> {
        read_clipboard_image(workspace)
    }

    fn supports_images(&self) -> bool {
        cfg!(target_os = "macos")
    }
}

#[must_use]
pub fn osc52_sequence(text: &str, tmux: bool) -> String {
    let encoded = STANDARD.encode(text.as_bytes());
    let sequence = format!("\u{1b}]52;c;{encoded}\u{7}");
    if tmux {
        format!("\u{1b}Ptmux;\u{1b}{sequence}\u{1b}\\")
    } else {
        sequence
    }
}

fn write_osc52(text: &str) -> Result<(), ClipboardError> {
    #[cfg(unix)]
    {
        let sequence = osc52_sequence(text, std::env::var_os("TMUX").is_some());
        let mut terminal = OpenOptions::new()
            .write(true)
            .open("/dev/tty")
            .map_err(|_| ClipboardError::Unavailable)?;
        terminal
            .write_all(sequence.as_bytes())
            .and_then(|()| terminal.flush())
            .map_err(|error| ClipboardError::Operation(error.to_string()))
    }
    #[cfg(not(unix))]
    {
        let _ = text;
        Err(ClipboardError::Unavailable)
    }
}

fn write_command(program: &str, arguments: &[&str], content: &[u8]) -> Result<(), ClipboardError> {
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ClipboardError::Unavailable)?;
    child
        .stdin
        .take()
        .ok_or(ClipboardError::Unavailable)?
        .write_all(content)
        .map_err(|error| ClipboardError::Operation(error.to_string()))?;
    child
        .wait()
        .map_err(|error| ClipboardError::Operation(error.to_string()))?
        .success()
        .then_some(())
        .ok_or(ClipboardError::Unavailable)
}

fn copy_commands() -> &'static [(&'static str, &'static [&'static str])] {
    #[cfg(target_os = "macos")]
    {
        &[("pbcopy", &[])]
    }
    #[cfg(target_os = "windows")]
    {
        &[("clip.exe", &[])]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    }
}

fn paste_commands() -> &'static [(&'static str, &'static [&'static str])] {
    #[cfg(target_os = "macos")]
    {
        &[("pbpaste", &[])]
    }
    #[cfg(target_os = "windows")]
    {
        &[(
            "powershell.exe",
            &["-NoProfile", "-Command", "Get-Clipboard"],
        )]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        &[
            ("wl-paste", &["--no-newline"]),
            ("xclip", &["-selection", "clipboard", "-o"]),
            ("xsel", &["--clipboard", "--output"]),
        ]
    }
}

#[cfg(not(target_os = "macos"))]
fn read_clipboard_image(_workspace: &Path) -> Result<Option<PathBuf>, ClipboardError> {
    Err(ClipboardError::ImageUnsupported)
}

#[cfg(target_os = "macos")]
fn read_clipboard_image(workspace: &Path) -> Result<Option<PathBuf>, ClipboardError> {
    let (target, mut target_file) = create_unique_image_file(workspace)?;
    let (capture_directory, capture) = match create_private_capture_path() {
        Ok(capture) => capture,
        Err(error) => {
            let _ = std::fs::remove_file(&target);
            return Err(error);
        }
    };
    let escaped = capture
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let script = format!(
        "set targetFile to POSIX file \"{escaped}\"\n\
         try\n\
             set imgData to the clipboard as «class PNGf»\n\
         on error\n\
             return\n\
         end try\n\
         set fh to open for access targetFile with write permission\n\
         set eof of fh to 0\n\
         write imgData to fh\n\
         close access fh"
    );
    let output = match Command::new("osascript").args(["-e", &script]).output() {
        Ok(output) => output,
        Err(error) => {
            let _ = std::fs::remove_file(&target);
            let _ = std::fs::remove_dir_all(&capture_directory);
            return Err(ClipboardError::Operation(error.to_string()));
        }
    };
    let captured = if output.status.success() && capture.is_file() {
        std::fs::metadata(&capture)
            .map_err(|error| ClipboardError::Operation(error.to_string()))
            .and_then(|metadata| {
                let actual = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
                if actual > MAX_IMAGE_BYTES {
                    Err(ClipboardError::ImageTooLarge {
                        actual,
                        maximum: MAX_IMAGE_BYTES,
                    })
                } else {
                    std::fs::read(&capture)
                        .map(Some)
                        .map_err(|error| ClipboardError::Operation(error.to_string()))
                }
            })
    } else {
        Ok(None)
    };
    let _ = std::fs::remove_dir_all(&capture_directory);
    let bytes = match captured {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            let _ = std::fs::remove_file(&target);
            return Ok(None);
        }
        Err(error) => {
            let _ = std::fs::remove_file(&target);
            return Err(error);
        }
    };
    let actual = bytes.len();
    if actual > MAX_IMAGE_BYTES {
        let _ = std::fs::remove_file(&target);
        return Err(ClipboardError::ImageTooLarge {
            actual,
            maximum: MAX_IMAGE_BYTES,
        });
    }
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        let _ = std::fs::remove_file(&target);
        return Ok(None);
    }
    if let Err(error) = target_file
        .write_all(&bytes)
        .and_then(|()| target_file.sync_all())
    {
        let _ = std::fs::remove_file(&target);
        return Err(ClipboardError::Operation(error.to_string()));
    }
    Ok(Some(target))
}

#[cfg(target_os = "macos")]
fn create_unique_image_file(workspace: &Path) -> Result<(PathBuf, std::fs::File), ClipboardError> {
    use std::os::unix::fs::OpenOptionsExt;

    let workspace = std::fs::canonicalize(workspace)
        .map_err(|error| ClipboardError::Operation(error.to_string()))?;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    for suffix in 0..1_000 {
        let name = if suffix == 0 {
            format!("clipboard-{millis}.png")
        } else {
            format!("clipboard-{millis}-{suffix}.png")
        };
        let candidate = workspace.join(format!(".vibe-{name}"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(ClipboardError::Operation(error.to_string())),
        }
    }
    Err(ClipboardError::Operation(
        "could not allocate a unique clipboard image path".to_owned(),
    ))
}

#[cfg(target_os = "macos")]
fn create_private_capture_path() -> Result<(PathBuf, PathBuf), ClipboardError> {
    use std::os::unix::fs::DirBuilderExt;

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    for suffix in 0..1_000 {
        let directory = std::env::temp_dir().join(format!(
            "mistral-vibe-clipboard-{}-{nanos}-{suffix}",
            std::process::id()
        ));
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&directory) {
            Ok(()) => {
                let capture = directory.join("capture.png");
                return Ok((directory, capture));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(ClipboardError::Operation(error.to_string())),
        }
    }
    Err(ClipboardError::Operation(
        "could not allocate a private clipboard capture directory".to_owned(),
    ))
}
