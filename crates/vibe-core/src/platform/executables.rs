//! Where a program named without a directory is found on `PATH`.
//!
//! One search serves every configured program this port launches by name: an
//! MCP stdio server, git, and the runtimes plugin materialization probes for.
//! Each caller keeps its own acceptance rule (git refuses a binary inside the
//! project); the names tried and their order are decided here.

use std::path::{Path, PathBuf};

/// The extensions Windows runs when `PATHEXT` is unset.
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// The file names `name` is looked up under, in the order they are tried.
///
/// On Windows a name without an extension is tried with each `PATHEXT`
/// extension, as `shutil.which` tries them: `CreateProcess` only appends
/// `.exe`, and the shim npm installs for `npx` is `npx.cmd`. A name carrying an
/// extension, and any name elsewhere, is tried as written.
#[must_use]
pub fn executable_names(name: &str) -> Vec<String> {
    if !cfg!(windows) || Path::new(name).extension().is_some() {
        return vec![name.to_owned()];
    }
    std::env::var("PATHEXT")
        .ok()
        .filter(|extensions| !extensions.is_empty())
        .as_deref()
        .unwrap_or(DEFAULT_PATHEXT)
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| format!("{name}{}", extension.to_lowercase()))
        .collect()
}

/// Every path `name` may resolve to, in search order: each `PATH` entry, its
/// surrounding quotes removed, joined with each of the [`executable_names`].
#[must_use]
pub fn path_candidates(name: &str) -> Vec<PathBuf> {
    let names = executable_names(name);
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(strip_quotes)
        .flat_map(|entry| names.iter().map(move |name| entry.join(name)))
        .collect()
}

/// The first of the [`path_candidates`] that is an executable file.
#[must_use]
pub fn find_executable(name: &str) -> Option<PathBuf> {
    path_candidates(name)
        .into_iter()
        .find(|candidate| is_executable_file(candidate))
}

/// Whether `path` is a file this host would run: any file on Windows, which
/// decides by extension, and one carrying an execute bit elsewhere.
#[must_use]
pub fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

/// A `PATH` entry written between double quotes, as Windows tolerates, read
/// without them.
fn strip_quotes(entry: PathBuf) -> PathBuf {
    let text = entry.to_string_lossy();
    match text
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
    {
        Some(inner) => PathBuf::from(inner),
        None => entry,
    }
}

#[cfg(test)]
mod executables_tests;
