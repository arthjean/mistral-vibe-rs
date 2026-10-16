//! Host paths in the form a user types.
//!
//! `fs::canonicalize` on Windows answers with the verbatim prefix (`\\?\C:\...`),
//! which git rejects, editors cannot open, and no reference path ever carries.
//! [`canonicalize`] and [`simplified`] drop it wherever a plain path names the
//! same file, and keep it where dropping it would change the meaning: a UNC
//! share, a reserved device name such as `con.txt`, a component with a
//! trailing dot or space, a path over the legacy length limit, or one that is
//! not Unicode. Everywhere else they are the identity.

use std::io;
use std::path::{Path, PathBuf};

#[cfg(test)]
mod paths_tests;

const VERBATIM_PREFIX: &str = r"\\?\";
const VERBATIM_UNC_PREFIX: &str = r"\\?\UNC\";

/// `fs::canonicalize`, without the verbatim prefix where a plain path is
/// equivalent.
///
/// # Errors
///
/// Whatever `fs::canonicalize` reports for `path`.
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    dunce::canonicalize(path)
}

/// `path` without the verbatim prefix where a plain path is equivalent.
#[must_use]
pub fn simplified(path: &Path) -> &Path {
    dunce::simplified(path)
}

/// The text Python's `Path.resolve()` gives for a canonical path: no verbatim
/// prefix at all, a verbatim UNC path written `\\server\share\...`.
///
/// Only for values compared with what the reference stores or hashes, such as
/// the managed worktree bucket digest and the trusted folder list; a path the
/// port opens goes through [`canonicalize`] instead.
#[must_use]
pub fn strip_verbatim_prefix(value: &str) -> String {
    if let Some(remainder) = value.strip_prefix(VERBATIM_UNC_PREFIX) {
        format!(r"\\{remainder}")
    } else if let Some(remainder) = value.strip_prefix(VERBATIM_PREFIX) {
        remainder.to_owned()
    } else {
        value.to_owned()
    }
}
