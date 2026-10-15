//! The child environment the integration tests build from scratch.
//!
//! A test that clears the environment still has to hand Windows the variables
//! the operating system itself reads: without `SystemRoot`, Winsock fails to
//! initialize (`WSAStartup` error 10106) and every network call reports a
//! failure the product never produces on a real host. Without `PATHEXT`,
//! `COMSPEC`, `TEMP`, and `TMP`, executable lookup, shells, and temporary files
//! misbehave the same way.

#![allow(
    dead_code,
    reason = "every integration binary compiles this module and uses a part of it"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// What Windows needs in any child environment, carried over from the parent.
const WINDOWS_BASE: [&str; 7] = [
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

/// `path` resolved through the filesystem, in the form a user types and the
/// binary reports: Windows answers a canonical path with the `\\?\` verbatim
/// prefix, which only a drive-letter path can drop without changing meaning.
pub fn canonical(path: &Path) -> PathBuf {
    let resolved = std::fs::canonicalize(path).expect("the fixture path resolves");
    let text = resolved.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => resolved,
    }
}
