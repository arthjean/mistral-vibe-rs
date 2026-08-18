//! What the machine reports about itself.
//!
//! Every telemetry envelope carries the operating system, its version and the
//! terminal the session runs in. None of that is a telemetry decision: it is a
//! property of the host, discovered per platform and cached where discovery
//! costs a process. It sits beside the telemetry surface rather than inside it
//! so a case can assert what a given environment resolves to without building
//! an envelope.

use std::sync::OnceLock;

/// Reference `get_platform_id`: the canonical lowercase platform identifier.
/// Rust names macOS `macos` where the reference names it `darwin`, and agrees
/// everywhere else.
#[must_use]
pub fn platform_id() -> String {
    match std::env::consts::OS {
        "macos" => "darwin".to_owned(),
        other => other.to_owned(),
    }
}

/// Reference `get_platform_version`: the distribution version on Linux, the
/// product version on macOS and the system version on Windows.
///
/// Resolved once per process: the macOS and Windows branches read a system
/// tool, which a per-event census must not do.
#[must_use]
pub fn platform_version() -> Option<String> {
    static VERSION: OnceLock<Option<String>> = OnceLock::new();
    VERSION.get_or_init(resolve_platform_version).clone()
}

#[cfg(target_os = "linux")]
fn resolve_platform_version() -> Option<String> {
    let release = std::fs::read_to_string("/etc/os-release").ok()?;
    let field = |key: &str| {
        release.lines().find_map(|line| {
            line.strip_prefix(key)
                .map(|value| value.trim_matches('"').to_owned())
        })
    };
    field("VERSION_ID=")
        .or_else(|| field("VERSION="))
        .filter(|value| !value.is_empty())
}

#[cfg(target_os = "macos")]
fn resolve_platform_version() -> Option<String> {
    command_output("sw_vers", &["-productVersion"])
}

#[cfg(target_os = "windows")]
fn resolve_platform_version() -> Option<String> {
    // `cmd /C ver` prints `Microsoft Windows [Version 10.0.19045.1234]`, and
    // the reference reports the bracketed number alone.
    let printed = command_output("cmd", &["/C", "ver"])?;
    let version = printed.split_once('[')?.1.rsplit_once(']')?.0;
    version
        .rsplit_once(' ')
        .map(|(_, value)| value.to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn resolve_platform_version() -> Option<String> {
    None
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn command_output(program: &str, arguments: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(arguments)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let printed = String::from_utf8(output.stdout).ok()?;
    let printed = printed.trim().to_owned();
    (!printed.is_empty()).then_some(printed)
}

/// Reference `detect_terminal`: the terminal the process is attached to, named
/// from the vocabulary `vibe_protocol::TerminalEmulator` publishes, and
/// `unknown` when nothing identifies one.
#[must_use]
pub fn detect_terminal_emulator() -> &'static str {
    terminal_emulator_from(&|name| std::env::var(name).ok())
}

/// The environment markers, in the order the reference consults them:
/// `TERM_PROGRAM` first, with the Cursor and Insiders splits under `vscode`,
/// then the per-terminal variables, then JetBrains.
pub(crate) fn terminal_emulator_from(lookup: &dyn Fn(&str) -> Option<String>) -> &'static str {
    let value = |name: &str| lookup(name).unwrap_or_default().to_ascii_lowercase();
    let program = value("TERM_PROGRAM");
    if program == "vscode" {
        if [
            "VSCODE_GIT_ASKPASS_NODE",
            "VSCODE_GIT_ASKPASS_MAIN",
            "VSCODE_IPC_HOOK_CLI",
            "VSCODE_NLS_CONFIG",
        ]
        .into_iter()
        .any(|name| value(name).contains("cursor"))
        {
            return "cursor";
        }
        if value("TERM_PROGRAM_VERSION").ends_with("-insider") {
            return "vscode_insiders";
        }
        return "vscode";
    }
    for (marker, terminal) in [
        ("apple_terminal", "apple_terminal"),
        ("iterm.app", "iterm2"),
        ("wezterm", "wezterm"),
        ("ghostty", "ghostty"),
        ("alacritty", "alacritty"),
        ("kitty", "kitty"),
        ("hyper", "hyper"),
    ] {
        if program == marker {
            return terminal;
        }
    }
    for (variable, terminal) in [
        ("WEZTERM_PANE", "wezterm"),
        ("GHOSTTY_RESOURCES_DIR", "ghostty"),
        ("KITTY_WINDOW_ID", "kitty"),
        ("ALACRITTY_SOCKET", "alacritty"),
        ("ALACRITTY_LOG", "alacritty"),
        ("WT_SESSION", "windows_terminal"),
        ("WT_PROFILE_ID", "windows_terminal"),
    ] {
        if !value(variable).is_empty() {
            return terminal;
        }
    }
    if value("TERMINAL_EMULATOR").contains("jetbrains") {
        return "jetbrains";
    }
    "unknown"
}

// --------------------------------------------------------------------------
// The events this port authors
// --------------------------------------------------------------------------
