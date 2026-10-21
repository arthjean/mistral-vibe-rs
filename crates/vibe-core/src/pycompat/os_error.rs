//! `str(OSError)`.

use std::io;
use std::path::Path;

use super::repr::repr_str;

/// `str(OSError)` for `error`: `[Errno N] strerror`, followed by `: ` and the
/// `repr` of the path the call named, when it named one.
///
/// Rust renders an operating system error as `strerror (os error N)`, from the
/// same C library text Python reads, so the description is that rendering
/// without its suffix. An error that carries no error number is not an
/// `OSError` Python would raise, and reads as Rust renders it.
#[must_use]
pub fn os_error(error: &io::Error, path: Option<&Path>) -> String {
    let described = error.to_string();
    let Some(code) = error.raw_os_error() else {
        return described;
    };
    let strerror = described
        .strip_suffix(&format!(" (os error {code})"))
        .unwrap_or(&described);
    match path {
        Some(path) => format!(
            "[Errno {code}] {strerror}: {}",
            repr_str(&path.to_string_lossy())
        ),
        None => format!("[Errno {code}] {strerror}"),
    }
}
