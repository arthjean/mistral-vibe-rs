use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "macos", test))]
use std::process::ExitStatus;
use std::process::{Command, Stdio};
#[cfg(any(target_os = "macos", test))]
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use thiserror::Error;
#[cfg(test)]
use vibe_core::images::MAX_IMAGE_BYTES;
use vibe_core::images::{ImageDigest, validate_image_size};

#[cfg(target_os = "macos")]
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(5);
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("clipboard is unavailable")]
    Unavailable,
    #[error("clipboard image paste is unsupported on this platform")]
    ImageUnsupported,
    #[error("clipboard image is too large ({actual} bytes, maximum {maximum})")]
    ImageTooLarge { actual: usize, maximum: usize },
    #[error("clipboard operation timed out")]
    Timeout,
    #[error("clipboard operation failed: {0}")]
    Operation(String),
    #[error("failed to save clipboard image: {0}")]
    Save(String),
}

pub trait SystemClipboardPort {
    fn copy_text(&self, text: &str) -> Result<(), ClipboardError>;
    fn paste_text(&self) -> Result<String, ClipboardError>;
    fn paste_image(&self) -> Result<Option<Vec<u8>>, ClipboardError>;
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

    fn paste_image(&self) -> Result<Option<Vec<u8>>, ClipboardError> {
        read_clipboard_image()
    }

    fn supports_images(&self) -> bool {
        cfg!(target_os = "macos")
    }
}

pub(crate) async fn copy_text_bounded(text: String) -> Result<(), ClipboardError> {
    if text.is_empty() {
        return Err(ClipboardError::Unavailable);
    }
    for (program, arguments) in copy_commands() {
        if super::external_action::run_command(program, arguments, Some(text.as_bytes().to_vec()))
            .await
            .is_ok()
        {
            return Ok(());
        }
    }
    write_osc52(&text)
}

#[derive(Debug)]
pub(crate) struct CapturedClipboardImage {
    pub path: PathBuf,
    pub bytes: usize,
    pub digest: ImageDigest,
}

pub(crate) fn capture_clipboard_image(
    clipboard: &dyn SystemClipboardPort,
) -> Result<Option<CapturedClipboardImage>, ClipboardError> {
    if !clipboard.supports_images() {
        return Err(ClipboardError::ImageUnsupported);
    }
    let Some(bytes) = clipboard.paste_image()? else {
        return Ok(None);
    };
    if !bytes.starts_with(PNG_MAGIC) {
        return Ok(None);
    }
    let bytes = bounded_image(bytes)?;
    let digest = ImageDigest::of(&bytes);
    let path =
        write_clipboard_image(&bytes).map_err(|error| ClipboardError::Save(error.to_string()))?;
    Ok(Some(CapturedClipboardImage {
        path,
        bytes: bytes.len(),
        digest,
    }))
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
fn read_clipboard_image() -> Result<Option<Vec<u8>>, ClipboardError> {
    Err(ClipboardError::ImageUnsupported)
}

#[cfg(target_os = "macos")]
fn read_clipboard_image() -> Result<Option<Vec<u8>>, ClipboardError> {
    let Ok(directory) = create_private_capture_directory("mistral-vibe-clipboard") else {
        return Ok(None);
    };
    let deadline = Instant::now() + CLIPBOARD_TIMEOUT;
    let result = match read_macos_image(&directory, deadline) {
        Err(ClipboardError::Timeout | ClipboardError::Operation(_)) => Ok(None),
        result => result,
    };
    let _ = std::fs::remove_dir_all(&directory);
    result
}

#[cfg(target_os = "macos")]
fn read_macos_image(
    directory: &Path,
    deadline: Instant,
) -> Result<Option<Vec<u8>>, ClipboardError> {
    if let Ok(Some(png)) = read_macos_class("PNGf", directory, deadline)
        && png.starts_with(PNG_MAGIC)
    {
        return bounded_image(png).map(Some);
    }
    let tiff = match read_macos_class("TIFF", directory, deadline) {
        Ok(Some(tiff)) => tiff,
        Ok(None) | Err(ClipboardError::Timeout | ClipboardError::Operation(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    select_macos_image(None, Some(tiff), |tiff| {
        convert_tiff_to_png(tiff, directory, deadline)
    })
}

#[cfg(target_os = "macos")]
fn read_macos_class(
    four_cc: &str,
    directory: &Path,
    deadline: Instant,
) -> Result<Option<Vec<u8>>, ClipboardError> {
    let path = directory.join(format!("{four_cc}.bin"));
    create_private_file(&path)?;
    let escaped = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let script = format!(
        "set targetFile to POSIX file \"{escaped}\"\n\
         try\n\
             set imgData to the clipboard as «class {four_cc}»\n\
         on error\n\
             return\n\
         end try\n\
         set fh to open for access targetFile with write permission\n\
         set eof of fh to 0\n\
         write imgData to fh\n\
         close access fh"
    );
    let mut command = Command::new("osascript");
    command.args(["-e", &script]);
    if !run_command_until(&mut command, deadline)?.success() {
        return Ok(None);
    }
    read_nonempty(&path)
}

#[cfg(target_os = "macos")]
fn convert_tiff_to_png(
    tiff: &[u8],
    directory: &Path,
    deadline: Instant,
) -> Result<Option<Vec<u8>>, ClipboardError> {
    let source = directory.join("source.tiff");
    let target = directory.join("converted.png");
    write_private_file(&source, tiff)?;
    create_private_file(&target)?;
    let source_display = source.to_string_lossy().into_owned();
    let target_display = target.to_string_lossy().into_owned();
    let mut command = Command::new("sips");
    command.args([
        "-s",
        "format",
        "png",
        &source_display,
        "--out",
        &target_display,
    ]);
    if !run_command_until(&mut command, deadline)?.success() {
        return Ok(None);
    }
    read_nonempty(&target)
}

fn bounded_image(bytes: Vec<u8>) -> Result<Vec<u8>, ClipboardError> {
    validate_image_size(bytes.len()).map_err(|error| ClipboardError::ImageTooLarge {
        actual: error.actual,
        maximum: error.maximum,
    })?;
    Ok(bytes)
}

#[cfg(any(target_os = "macos", test))]
fn select_macos_image(
    png: Option<Vec<u8>>,
    tiff: Option<Vec<u8>>,
    convert_tiff: impl FnOnce(&[u8]) -> Result<Option<Vec<u8>>, ClipboardError>,
) -> Result<Option<Vec<u8>>, ClipboardError> {
    if let Some(png) = png.filter(|bytes| bytes.starts_with(PNG_MAGIC)) {
        return bounded_image(png).map(Some);
    }
    let Some(tiff) = tiff else {
        return Ok(None);
    };
    match convert_tiff(&tiff) {
        Ok(Some(png)) if png.starts_with(PNG_MAGIC) => bounded_image(png).map(Some),
        Ok(_) | Err(ClipboardError::Timeout | ClipboardError::Operation(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "macos")]
fn read_nonempty(path: &Path) -> Result<Option<Vec<u8>>, ClipboardError> {
    let bytes =
        std::fs::read(path).map_err(|error| ClipboardError::Operation(error.to_string()))?;
    Ok((!bytes.is_empty()).then_some(bytes))
}

#[cfg(any(target_os = "macos", test))]
fn run_command_until(
    command: &mut Command,
    deadline: Instant,
) -> Result<ExitStatus, ClipboardError> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| ClipboardError::Operation(error.to_string()))?;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| ClipboardError::Operation(error.to_string()))?
        {
            return Ok(status);
        }
        let now = Instant::now();
        if now >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ClipboardError::Timeout);
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(10)));
    }
}

fn write_clipboard_image(bytes: &[u8]) -> Result<PathBuf, ClipboardError> {
    write_clipboard_image_in(&std::env::temp_dir().join("vibe-pasted-images"), bytes)
}

fn write_clipboard_image_in(directory: &Path, bytes: &[u8]) -> Result<PathBuf, ClipboardError> {
    ensure_private_directory(directory)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    for suffix in 0..1_000 {
        let name = if suffix == 0 {
            format!("clipboard-{stamp}.png")
        } else {
            format!("clipboard-{stamp}-{suffix}.png")
        };
        let path = directory.join(name);
        match open_private_file(&path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
                    let _ = std::fs::remove_file(&path);
                    return Err(ClipboardError::Operation(error.to_string()));
                }
                return match std::fs::canonicalize(&path) {
                    Ok(path) => Ok(path),
                    Err(error) => {
                        let _ = std::fs::remove_file(&path);
                        Err(ClipboardError::Operation(error.to_string()))
                    }
                };
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(ClipboardError::Operation(error.to_string())),
        }
    }
    Err(ClipboardError::Operation(
        "could not allocate a unique clipboard image path".to_owned(),
    ))
}

#[cfg(target_os = "macos")]
fn create_private_capture_directory(prefix: &str) -> Result<PathBuf, ClipboardError> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    for suffix in 0..1_000 {
        let directory =
            std::env::temp_dir().join(format!("{prefix}-{}-{stamp}-{suffix}", std::process::id()));
        match create_private_directory(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(ClipboardError::Operation(error.to_string())),
        }
    }
    Err(ClipboardError::Operation(
        "could not allocate a private clipboard capture directory".to_owned(),
    ))
}

fn ensure_private_directory(path: &Path) -> Result<(), ClipboardError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(ClipboardError::Operation(
                "clipboard image directory is not a private directory".to_owned(),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_private_directory(path)
                .map_err(|error| ClipboardError::Operation(error.to_string()))?;
        }
        Err(error) => return Err(ClipboardError::Operation(error.to_string())),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| ClipboardError::Operation(error.to_string()))?;
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

#[cfg(target_os = "macos")]
fn create_private_file(path: &Path) -> Result<(), ClipboardError> {
    open_private_file(path)
        .map(drop)
        .map_err(|error| ClipboardError::Operation(error.to_string()))
}

#[cfg(target_os = "macos")]
fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), ClipboardError> {
    let mut file =
        open_private_file(path).map_err(|error| ClipboardError::Operation(error.to_string()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| ClipboardError::Operation(error.to_string()))
}

fn open_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ClipboardFixture {
        image: Result<Option<Vec<u8>>, ClipboardError>,
        supported: bool,
    }

    impl SystemClipboardPort for ClipboardFixture {
        fn copy_text(&self, _text: &str) -> Result<(), ClipboardError> {
            Ok(())
        }

        fn paste_text(&self) -> Result<String, ClipboardError> {
            Ok(String::new())
        }

        fn paste_image(&self) -> Result<Option<Vec<u8>>, ClipboardError> {
            match &self.image {
                Ok(image) => Ok(image.clone()),
                Err(error) => Err(ClipboardError::Operation(error.to_string())),
            }
        }

        fn supports_images(&self) -> bool {
            self.supported
        }
    }

    #[test]
    fn capture_rejects_invalid_oversize_and_unsupported_images_before_writing() {
        let windows_like = ClipboardFixture {
            image: Ok(Some([PNG_MAGIC, b"image"].concat())),
            supported: false,
        };
        assert!(matches!(
            capture_clipboard_image(&windows_like),
            Err(ClipboardError::ImageUnsupported)
        ));

        let invalid = ClipboardFixture {
            image: Ok(Some(b"not-png".to_vec())),
            supported: true,
        };
        assert!(
            capture_clipboard_image(&invalid)
                .expect("invalid clipboard is not an adapter error")
                .is_none()
        );

        let mut oversized = Vec::from(PNG_MAGIC);
        let maximum = usize::try_from(MAX_IMAGE_BYTES).expect("10 MiB fits supported targets");
        oversized.resize(maximum + 1, 0);
        let oversized = ClipboardFixture {
            image: Ok(Some(oversized)),
            supported: true,
        };
        assert!(matches!(
            capture_clipboard_image(&oversized),
            Err(ClipboardError::ImageTooLarge { .. })
        ));
    }

    #[test]
    fn macos_image_selection_prefers_png_and_converts_tiff_fallback() {
        let png = [PNG_MAGIC, b"png"].concat();
        let conversion_called = std::cell::Cell::new(false);
        let selected = select_macos_image(Some(png.clone()), Some(b"tiff".to_vec()), |_| {
            conversion_called.set(true);
            Ok(None)
        })
        .expect("PNG selection succeeds");
        assert_eq!(selected, Some(png));
        assert!(!conversion_called.get());

        let converted = [PNG_MAGIC, b"converted"].concat();
        let selected = select_macos_image(None, Some(b"II*\0tiff".to_vec()), |tiff| {
            assert_eq!(tiff, b"II*\0tiff");
            Ok(Some(converted.clone()))
        })
        .expect("TIFF conversion succeeds");
        assert_eq!(selected, Some(converted));
    }

    #[cfg(unix)]
    #[test]
    fn command_timeout_kills_the_child_within_the_deadline() {
        let started = Instant::now();
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 1"]);
        let result = run_command_until(&mut command, Instant::now() + Duration::from_millis(25));
        assert!(matches!(result, Err(ClipboardError::Timeout)));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[cfg(unix)]
    #[test]
    fn persisted_clipboard_images_use_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().expect("isolated clipboard root");
        let directory = temporary.path().join("vibe-pasted-images");
        let image = [PNG_MAGIC, b"image"].concat();
        let captured =
            write_clipboard_image_in(&directory, &image).expect("clipboard image persists");
        let directory_mode = std::fs::metadata(captured.parent().expect("image directory"))
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(&captured)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }
}
