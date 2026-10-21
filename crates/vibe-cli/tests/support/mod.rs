//! The child environment the integration tests build from scratch.
//!
//! A test that clears the environment still has to hand Windows the variables
//! the operating system itself reads: without `SystemRoot`, Winsock fails to
//! initialize (`WSAStartup` error 10106) and every network call reports a
//! failure the product never produces on a real host. Without `PATHEXT`,
//! `COMSPEC`, `TEMP`, and `TMP`, executable lookup, shells, and temporary files
//! misbehave the same way.
//!
//! The `vibe-acp` tests compile this module too, through a `#[path]`
//! attribute, so both binaries launch from the same base set.

#![allow(
    dead_code,
    reason = "every integration binary compiles this module and uses a part of it"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

pub mod test_root;

/// What Windows needs in any child environment, carried over from the parent.
pub const WINDOWS_BASE: [&str; 7] = [
    "SystemRoot",
    "SystemDrive",
    "windir",
    "COMSPEC",
    "PATHEXT",
    "TEMP",
    "TMP",
];

pub trait FixtureEnvironment {
    /// Clears the environment, keeping `PATH` and, on Windows, the base set.
    fn env_clear_keeping_base(&mut self) -> &mut Self;

    /// Points the child's home at `home`: `HOME` everywhere, and `USERPROFILE`
    /// on Windows, where the home is read from it.
    fn env_home(&mut self, home: &Path) -> &mut Self;
}

impl FixtureEnvironment for Command {
    fn env_clear_keeping_base(&mut self) -> &mut Self {
        self.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default());
        if cfg!(windows) {
            for name in WINDOWS_BASE {
                if let Some(value) = std::env::var_os(name) {
                    self.env(name, value);
                }
            }
        }
        self
    }

    fn env_home(&mut self, home: &Path) -> &mut Self {
        self.env("HOME", home);
        if cfg!(windows) {
            self.env("USERPROFILE", home);
        }
        self
    }
}

/// A launch root holding the profile directory a binary resolves its home
/// under, what Git Bash exports as `HOME`, which Windows resolution ignores,
/// and a project to start in.
pub struct HomeLayout {
    _root: tempfile::TempDir,
    pub profile: PathBuf,
    pub bash_home: PathBuf,
    pub project: PathBuf,
}

pub fn home_layout() -> HomeLayout {
    let root = tempfile::tempdir().expect("fixture root");
    let [profile, bash_home, project] =
        ["profile", "bash-home", "project"].map(|name| root.path().join(name));
    for directory in [&profile, &bash_home, &project] {
        std::fs::create_dir_all(directory).expect("fixture directory");
    }
    HomeLayout {
        _root: root,
        profile,
        bash_home,
        project,
    }
}

impl HomeLayout {
    /// The environment that names the profile as the home: `USERPROFILE` on
    /// Windows, with Git Bash's `HOME` beside it, and `HOME` elsewhere.
    pub fn profile_environment(&self) -> Vec<(&'static str, &Path)> {
        if cfg!(windows) {
            vec![("USERPROFILE", &self.profile), ("HOME", &self.bash_home)]
        } else {
            vec![("HOME", &self.profile)]
        }
    }
}

/// The log file a launch opens first, under `vibe_home`, which shows where the
/// home resolved even when the launch fails right after.
pub fn log_file(vibe_home: &Path) -> PathBuf {
    vibe_home.join("logs").join("vibe.log")
}

/// `path` resolved through the filesystem, in the form a user types and the
/// binary reports: Windows answers a canonical path with the `\\?\` verbatim
/// prefix, which only a drive-letter path can drop without changing meaning.
pub fn canonical(path: &Path) -> PathBuf {
    let resolved = vibe_core::platform::canonicalize(path).expect("the fixture path resolves");
    let text = resolved.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => resolved,
    }
}
