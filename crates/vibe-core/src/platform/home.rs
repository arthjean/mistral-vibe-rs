//! The operator's home directory and the vibe home every entry point resolves.
//!
//! Reference `vibe/utils/vibe_home.py`: `VIBE_HOME` wins, `~`-expanded;
//! otherwise the home is `Path.home()` joined with `.vibe`. `Path.home()` reads
//! `USERPROFILE`, then `HOMEDRIVE` plus `HOMEPATH`, on Windows and `HOME`
//! elsewhere; on Windows `HOME` is never read, even when Git Bash exports it
//! (`docs/parity.md`, "Windows observations").

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use thiserror::Error;

#[cfg(test)]
mod home_tests;

/// The variable that names the vibe home outright.
pub const VIBE_HOME_ENV_VAR: &str = "VIBE_HOME";

/// The vibe home cannot be placed: on Windows the environment names no home
/// directory, and `VIBE_HOME` is unset or needs one to expand its `~`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error(
    "cannot locate the vibe home: neither USERPROFILE nor HOMEDRIVE and HOMEPATH is set; set \
     VIBE_HOME to the directory vibe should keep its state in"
)]
pub struct HomeNotFound;

type Environment<'a> = &'a dyn Fn(&str) -> Option<OsString>;

fn process_environment(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// The operator's home directory, or [`None`] when the environment names none.
#[must_use]
pub fn user_home() -> Option<PathBuf> {
    user_home_in(&process_environment, cfg!(windows))
}

fn user_home_in(environment: Environment<'_>, windows: bool) -> Option<PathBuf> {
    let set = |name: &str| environment(name).filter(|value| !value.is_empty());
    if windows {
        set("USERPROFILE").map(PathBuf::from).or_else(|| {
            let mut home = set("HOMEDRIVE")?;
            home.push(set("HOMEPATH")?);
            Some(PathBuf::from(home))
        })
    } else {
        set("HOME").map(PathBuf::from)
    }
}

/// `path` with a leading `~` read as the home directory, as Python's
/// `Path.expanduser` reads it: `~` alone or followed by a separator, which is
/// `/` everywhere and also `\` on Windows. Without a home the path is returned
/// as written.
#[must_use]
pub fn expand_home(path: &Path) -> PathBuf {
    expand_home_with(path, user_home().as_deref(), cfg!(windows))
}

fn home_relative_tail(path: &Path, windows: bool) -> Option<&str> {
    let separators: &[char] = if windows { &['/', '\\'] } else { &['/'] };
    let rest = path.to_str()?.strip_prefix('~')?;
    (rest.is_empty() || rest.starts_with(separators)).then(|| rest.trim_start_matches(separators))
}

fn expand_home_with(path: &Path, home: Option<&Path>, windows: bool) -> PathBuf {
    match (home_relative_tail(path, windows), home) {
        (Some(tail), Some(home)) => {
            let mut expanded = home.to_path_buf();
            expanded.extend(Path::new(tail).components());
            expanded
        }
        _ => path.to_path_buf(),
    }
}

/// The vibe home: `VIBE_HOME` when set, else `.vibe` under the home directory.
///
/// Off Windows, an environment with no `HOME` falls back to `.vibe` under
/// `working_directory`, as every release has. On Windows that directory is
/// also the project's configuration directory, so the resolver refuses
/// instead, and each binary exits on the refusal before it writes any state.
///
/// # Errors
///
/// [`HomeNotFound`] on Windows when neither `USERPROFILE` nor `HOMEDRIVE` plus
/// `HOMEPATH` is set and `VIBE_HOME` does not name a home without them.
pub fn vibe_home(working_directory: &Path) -> Result<PathBuf, HomeNotFound> {
    vibe_home_in(&process_environment, cfg!(windows), working_directory)
}

/// [`vibe_home`] for a caller that has no way to report the refusal.
///
/// The `vibe` and `vibe-acp` binaries call [`vibe_home`] before anything else
/// and exit when it refuses; the environment cannot change inside the process
/// afterward (`unsafe_code` is forbidden, so nothing calls `set_var`), so the
/// workspace-local fallback here is only ever the non-Windows one.
#[must_use]
pub fn vibe_home_or_local(working_directory: &Path) -> PathBuf {
    vibe_home(working_directory).unwrap_or_else(|_| working_directory.join(".vibe"))
}

fn vibe_home_in(
    environment: Environment<'_>,
    windows: bool,
    working_directory: &Path,
) -> Result<PathBuf, HomeNotFound> {
    let home = user_home_in(environment, windows);
    if let Some(value) = environment(VIBE_HOME_ENV_VAR).filter(|value| !value.is_empty()) {
        let named = PathBuf::from(value);
        if windows && home.is_none() && home_relative_tail(&named, windows).is_some() {
            return Err(HomeNotFound);
        }
        return Ok(expand_home_with(&named, home.as_deref(), windows));
    }
    match home {
        Some(home) => Ok(home.join(".vibe")),
        None if windows => Err(HomeNotFound),
        None => Ok(working_directory.join(".vibe")),
    }
}
