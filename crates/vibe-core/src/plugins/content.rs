//! The content digest a plugin package is pinned by.
//!
//! Reference `vibe/core/plugins/_content.py`. Every entry below the root is
//! folded in path order as `<tag> NUL <relative path> NUL <sha256> NUL`: `f` for
//! a regular file with the digest of its bytes, `l` for a symbolic link with
//! the digest of its target text, and `o` for anything else, with no digest.
//! Directories contribute only through the entries they hold, symbolic links to
//! directories are not followed, paths are NFC-normalized POSIX spellings, and
//! mode bits are left out, so one tree digests the same on every host.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::canonical::normalize_nfc;
use crate::text::sha256_hex;

/// Digests a plugin tree, skipping every entry with a path component in
/// `ignored_names`.
///
/// # Errors
///
/// A directory that cannot be listed, a file that cannot be read, or a link
/// whose target cannot be read.
pub fn digest_plugin_tree(root: &Path, ignored_names: &BTreeSet<String>) -> io::Result<String> {
    let mut entries = Vec::new();
    collect(root, &mut entries)?;
    let mut keyed: Vec<(String, PathBuf, Vec<String>)> = entries
        .into_iter()
        .map(|path| {
            let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            let parts = relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            (posix_relative(&relative), path, parts)
        })
        .collect();
    keyed.sort_by(|left, right| left.0.cmp(&right.0));
    let mut digest = Sha256::new();
    for (relative, path, parts) in keyed {
        if parts.iter().any(|part| ignored_names.contains(part)) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path)?;
            entry(
                &mut digest,
                b"l",
                &relative,
                &sha256_hex(target.to_string_lossy().as_bytes()),
            );
            continue;
        }
        if metadata.is_dir() {
            continue;
        }
        if !metadata.is_file() {
            entry(&mut digest, b"o", &relative, "");
            continue;
        }
        entry(&mut digest, b"f", &relative, &digest_file(&path)?);
    }
    Ok(crate::text::hex_encode(&digest.finalize()))
}

/// The package files the package's own root `mcp.json` launches through a
/// `./` command of a `stdio` server, which a checkout keeps runnable. Reference
/// `PluginPackageStore._launched_executables`
/// (`harness/runtimes/python/python/mistralai_vibe_local_harness/vibe/plugins/_store.py`):
/// the command loses its `./`, is normalized as `posixpath.normpath` and then
/// NFC, and counts only when it names a regular file of the tree. A missing or
/// unreadable configuration launches nothing.
#[must_use]
pub fn launched_executables(root: &Path) -> BTreeSet<PathBuf> {
    let mut entries = Vec::new();
    if collect(root, &mut entries).is_err() {
        return BTreeSet::new();
    }
    let files: BTreeMap<String, PathBuf> = entries
        .into_iter()
        .filter(|path| fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file()))
        .map(|path| {
            let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            (posix_relative(&relative), path)
        })
        .collect();
    let Some(configuration) = files.get("mcp.json") else {
        return BTreeSet::new();
    };
    let Ok(bytes) = fs::read(configuration) else {
        return BTreeSet::new();
    };
    let text = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    let Ok(document) = serde_json::from_slice::<Value>(text) else {
        return BTreeSet::new();
    };
    let Some(servers) = document.get("mcpServers").and_then(Value::as_object) else {
        return BTreeSet::new();
    };
    servers
        .values()
        .filter(|server| server.get("type").and_then(Value::as_str) == Some("stdio"))
        .filter_map(|server| server.get("command")?.as_str()?.strip_prefix("./"))
        .filter_map(|command| files.get(&normalize_nfc(&posix_normpath(command))))
        .cloned()
        .collect()
}

/// Python `posixpath.normpath`.
fn posix_normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        "//"
    } else if path.starts_with('/') {
        "/"
    } else {
        ""
    };
    let mut components: Vec<&str> = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." if initial.is_empty() && components.last().is_none_or(|last| *last == "..") => {
                components.push(component);
            }
            ".." => {
                components.pop();
            }
            _ => components.push(component),
        }
    }
    let normalized = format!("{initial}{}", components.join("/"));
    if normalized.is_empty() {
        ".".to_owned()
    } else {
        normalized
    }
}

/// The NFC POSIX spelling of a path relative to a plugin root.
#[must_use]
pub fn posix_relative(relative: &Path) -> String {
    let joined = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    normalize_nfc(&joined)
}

/// Every path below `directory`, without descending into a symbolic link.
fn collect(directory: &Path, entries: &mut Vec<PathBuf>) -> io::Result<()> {
    for child in fs::read_dir(directory)? {
        let path = child?.path();
        let metadata = fs::symlink_metadata(&path)?;
        entries.push(path.clone());
        if metadata.is_dir() {
            collect(&path, entries)?;
        }
    }
    Ok(())
}

fn entry(digest: &mut Sha256, tag: &[u8], relative: &str, content: &str) {
    digest.update(tag);
    digest.update(b"\0");
    digest.update(relative.as_bytes());
    digest.update(b"\0");
    if !content.is_empty() {
        digest.update(content.as_bytes());
        digest.update(b"\0");
    }
}

fn digest_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(crate::text::hex_encode(&digest.finalize()))
}
