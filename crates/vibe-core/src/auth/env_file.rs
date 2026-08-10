//! Editing the global dotenv file in place.
//!
//! Reference `api_key_persistence.py` reaches for python-dotenv's `set_key`
//! and `unset_key` on `$VIBE_HOME/.env`: one variable is rewritten or
//! appended, every other line survives byte for byte, and removing from a
//! file that does not exist is a no-op. The written form is `KEY='value'`,
//! which both implementations parse, and a file this port creates is
//! owner-only from the first byte because it can hold a credential.

use std::fs;
use std::io;
use std::path::Path;

use crate::atomic_file::write_atomically;

/// Whether this dotenv line declares `key`.
fn declares(line: &str, key: &str) -> bool {
    let trimmed = line.trim();
    let candidate = trimmed.strip_prefix("export ").unwrap_or(trimmed).trim();
    candidate
        .split_once('=')
        .is_some_and(|(name, _)| name.trim() == key)
}

/// The `KEY='value'` form python-dotenv writes, with the quote the wrapping
/// depends on escaped.
fn render(key: &str, value: &str) -> String {
    format!("{key}='{}'", value.replace('\'', "\\'"))
}

/// Replaces the dotenv's contents the way python-dotenv's `rewrite` does:
/// staged in a sibling file and moved over the path.
///
/// Both properties of that shape are load-bearing for a file that can hold the
/// credential. An interrupted write leaves the previous file whole instead of
/// truncating every other variable the operator keeps there, and the path
/// itself is replaced rather than written through, so a symlinked `.env` never
/// carries the key to the link's target. `rewrite` restores the mode it read
/// through `lstat` when the path was a regular file, and leaves the staging
/// file's owner-only mode otherwise, which is what a fresh file and a
/// symlinked path both get here.
fn write_contents(path: &Path, contents: &str) -> io::Result<()> {
    let original_mode = original_mode(path);
    write_atomically(path, "vibe-env", contents.as_bytes()).map_err(|error| error.source)?;
    restore_mode(path, original_mode)
}

#[cfg(unix)]
fn original_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::symlink_metadata(path).ok()?;
    metadata
        .is_file()
        .then(|| metadata.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn original_mode(_path: &Path) -> Option<u32> {
    None
}

#[cfg(unix)]
fn restore_mode(path: &Path, mode: Option<u32>) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    match mode {
        Some(mode) => fs::set_permissions(path, fs::Permissions::from_mode(mode)),
        None => Ok(()),
    }
}

#[cfg(not(unix))]
fn restore_mode(_path: &Path, _mode: Option<u32>) -> io::Result<()> {
    Ok(())
}

/// Writes `key` into the dotenv at `path`, creating parent directories and
/// the file as needed. An existing line for the key is replaced where it
/// stands; anything else is appended.
///
/// # Errors
///
/// Propagates the failure to create the parent directories, read an existing
/// file, or write the result.
pub fn write_env_file_key(path: &Path, key: &str, value: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let existing = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let mut lines: Vec<String> = existing.lines().map(str::to_owned).collect();
    let mut replaced = false;
    for line in &mut lines {
        if declares(line, key) {
            *line = render(key, value);
            replaced = true;
            break;
        }
    }
    if !replaced {
        lines.push(render(key, value));
    }
    let mut contents = lines.join("\n");
    contents.push('\n');
    write_contents(path, &contents)
}

/// Drops the line declaring `key` from the dotenv at `path`. A file that does
/// not exist, or one that never declared the key, is left as it is.
///
/// # Errors
///
/// Propagates the failure to read or rewrite an existing file.
pub fn remove_env_file_key(path: &Path, key: &str) -> io::Result<()> {
    let existing = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let retained: Vec<&str> = existing
        .lines()
        .filter(|line| !declares(line, key))
        .collect();
    if retained.len() == existing.lines().count() {
        return Ok(());
    }
    let mut contents = retained.join("\n");
    if !contents.is_empty() {
        contents.push('\n');
    }
    write_contents(path, &contents)
}
