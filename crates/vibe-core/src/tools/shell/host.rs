//! Which shell family a host publishes, and what drives it.
//!
//! The reference reads all of this off the machine, so the surface a Windows
//! operator sees would only be observable on Windows. Carrying the platform and
//! the two Windows executables in a [`HostShells`] value instead makes the
//! availability rule a function of data: the same function decides on every
//! host, and the Windows surface can be measured against the reference from a
//! POSIX one.

use std::path::{Path, PathBuf};

use crate::platform::Platform;
use crate::shell::{ShellConfig, ShellFlavor};
use crate::tools::config::ToolConfigResolver;

/// Which variant of the host's shell family the session publishes.
///
/// The reference resolves this from `managed_shell_tools_enabled`, which its
/// own remote experiment writes and whose default variant is `legacy`. The
/// configuration field is the single switch on both sides: an operator sets it
/// in a file, or the experiments layer sets it below every file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellRollout {
    /// Reference `MANAGED_SHELL_TOOLS_LEGACY`, the default variant.
    #[default]
    Legacy,
    /// Reference `MANAGED_SHELL_TOOLS_MANAGED`.
    Managed,
}

impl ShellRollout {
    /// The variant the session configuration selects, defaulting to
    /// [`ShellRollout::Legacy`] exactly as the reference experiment does when
    /// nothing resolves.
    #[must_use]
    pub fn from_config(config: &ToolConfigResolver) -> Self {
        if config.managed_shell_tools_enabled() {
            Self::Managed
        } else {
            Self::Legacy
        }
    }
}

/// The published shell families, each owning five reference names built on its
/// own prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ShellFamily {
    /// Reference `Bash` and `ExperimentalBash`.
    Bash,
    /// Reference `GitBash` and `ExperimentalGitBash`.
    GitBash,
    /// Reference `WindowsShell` and `ExperimentalWindowsShell`.
    PowerShell,
}

impl ShellFamily {
    /// The name the family's command tool publishes, which is also the prefix
    /// of its four session tools and of every session id it mints.
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::GitBash => "git_bash",
            Self::PowerShell => "powershell",
        }
    }

    /// Reference `uses_posix_shell`, which answers for the interpreter the
    /// session drives rather than for the operating system: a Windows host
    /// publishing Git Bash composes the POSIX shell lists, not the Windows
    /// ones.
    pub(super) fn uses_posix_shell(self) -> bool {
        matches!(self, Self::Bash | Self::GitBash)
    }

    pub(super) fn tool_name(self, suffix: &str) -> String {
        format!("{}_{suffix}", self.name())
    }

    /// What the family forces into a child's environment.
    ///
    /// Reference `_get_git_bash_env_overrides` and `_get_windows_env_overrides`
    /// pin the same three interactivity switches and a pager that exits, so a
    /// command that would wait for a terminal no operator is watching fails or
    /// finishes instead of hanging the session. The POSIX family inherits the
    /// process environment untouched, as the reference `Bash` does.
    ///
    /// A managed session composes a different set: reference
    /// `TerminalSessionManager._build_env` is the only environment source on
    /// that path and it keeps the terminal interactive rather than declaring it
    /// absent, because a session the model feeds control keys to is one an
    /// operator would otherwise be sitting in front of. `TERM`, `COLUMNS` and
    /// `LINES` are read back from the process environment first, so a host that
    /// already states them keeps its own.
    pub(super) fn environment(self, managed: bool) -> Vec<(String, String)> {
        let inherited = |key: &str, fallback: &str| {
            (
                key.to_owned(),
                std::env::var(key)
                    .ok()
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| fallback.to_owned()),
            )
        };
        let owned = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect::<Vec<_>>()
        };
        match (self, managed) {
            (Self::Bash, false) => Vec::new(),
            (Self::Bash | Self::GitBash, true) => {
                let mut environment = vec![
                    inherited("TERM", "xterm-256color"),
                    inherited("COLUMNS", "120"),
                    inherited("LINES", "40"),
                ];
                environment.extend(owned(&[
                    ("GIT_PAGER", "cat"),
                    ("PAGER", "cat"),
                    ("LESS", "-FX"),
                    ("DEBIAN_FRONTEND", "noninteractive"),
                ]));
                environment
            }
            (Self::GitBash, false) => owned(&[
                ("CI", "true"),
                ("NONINTERACTIVE", "1"),
                ("NO_TTY", "1"),
                ("TERM", "dumb"),
                ("GIT_PAGER", "cat"),
                ("PAGER", "cat"),
                ("LESS", "-FX"),
            ]),
            (Self::PowerShell, false) => owned(&[
                ("CI", "true"),
                ("NONINTERACTIVE", "1"),
                ("NO_TTY", "1"),
                ("GIT_PAGER", "more"),
                ("PAGER", "more"),
            ]),
            (Self::PowerShell, true) => owned(&[("GIT_PAGER", "more"), ("PAGER", "more")]),
        }
    }
}

/// What the host offers the shell families: the platform it runs, and the
/// executables the two Windows families are published against.
///
/// The reference reads all three from the machine (`is_windows`,
/// `git_bash_shell_available`, `powershell_shell_available`). Carrying them in
/// one value makes the availability rule a function of data instead of a
/// compilation target, which is what lets the Windows surface be measured
/// against the reference from a POSIX host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostShells {
    pub platform: Platform,
    pub git_bash: Option<PathBuf>,
    pub powershell: Option<PathBuf>,
}

impl HostShells {
    /// What this machine offers. Only a Windows host is probed: the reference
    /// resolvers answer `None` off Windows before looking at anything.
    #[must_use]
    pub fn detect() -> Self {
        if !cfg!(windows) {
            return Self {
                platform: Platform::Posix,
                git_bash: None,
                powershell: None,
            };
        }
        let directories = std::env::var_os("PATH")
            .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
            .unwrap_or_default();
        Self {
            platform: Platform::Windows,
            git_bash: find_git_bash(&directories),
            powershell: find_powershell(&directories),
        }
    }
}

/// Reference `get_windows_bash_path`.
///
/// Every `PATH` entry is scanned rather than only the first hit, because the
/// WSL launcher shadows a real Git Bash and forwards into another filesystem;
/// a Git for Windows install is then found through `git.exe`, and finally at
/// the usual install roots.
pub(super) fn find_git_bash(directories: &[PathBuf]) -> Option<PathBuf> {
    let scanned = directories
        .iter()
        .map(|directory| directory.join("bash.exe"))
        .find(|candidate| candidate.is_file() && !is_wsl_launcher(candidate));
    if scanned.is_some() {
        return scanned;
    }
    // Git for Windows lays out `<git>\cmd\git.exe` with bash under `<git>\bin`.
    if let Some(root) = directories
        .iter()
        .map(|directory| directory.join("git.exe"))
        .find(|candidate| candidate.is_file())
        .as_deref()
        .and_then(Path::parent)
        .and_then(Path::parent)
    {
        let sibling = ["bin/bash.exe", "usr/bin/bash.exe"]
            .into_iter()
            .map(|relative| root.join(relative))
            .find(|candidate| candidate.is_file());
        if sibling.is_some() {
            return sibling;
        }
    }
    ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
        .into_iter()
        .filter_map(std::env::var_os)
        .flat_map(|base| {
            ["Git/bin/bash.exe", "Programs/Git/bin/bash.exe"]
                .map(|relative| PathBuf::from(&base).join(relative))
        })
        .find(|candidate| candidate.is_file())
}

/// Reference `_is_wsl_launcher`: the stubs that forward into a Linux VM with
/// its own filesystem, which is not a drop-in shell for the workspace.
pub(super) fn is_wsl_launcher(candidate: &Path) -> bool {
    let normalized = candidate
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase();
    normalized.ends_with("/system32/bash.exe")
        || normalized.ends_with("/system32/bash")
        || normalized.ends_with("/microsoft/windowsapps/bash.exe")
        || normalized.ends_with("/microsoft/windowsapps/bash")
}

/// Reference `WINDOWS_POWERSHELL_DEFAULT_SHELLS`, in its order: PowerShell 7
/// is preferred over the one Windows ships.
pub(super) fn find_powershell(directories: &[PathBuf]) -> Option<PathBuf> {
    ["pwsh.exe", "powershell.exe"].into_iter().find_map(|name| {
        directories
            .iter()
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

/// The family a host publishes under a rollout, and whether the managed
/// variant and its four session tools come with it.
///
/// Reference `_is_enabled_for_shell_rollout` decides the first half: the
/// `legacy` POSIX variant is withheld once the managed rollout is on and the
/// host is Windows, and every Windows-family tool carries the `managed`
/// rollout, so none of them publishes under `legacy`. Reference
/// `_powershell_treatment_available` decides the second: a Windows host that
/// has Git Bash publishes that family and nothing else, and PowerShell is
/// reached only where no Git Bash resolves.
pub(super) fn published_family(
    host: &HostShells,
    rollout: ShellRollout,
) -> Option<(ShellFamily, bool)> {
    let managed = rollout == ShellRollout::Managed;
    if host.platform != Platform::Windows {
        return Some((ShellFamily::Bash, managed));
    }
    if !managed {
        return Some((ShellFamily::Bash, false));
    }
    if host.git_bash.is_some() {
        return Some((ShellFamily::GitBash, true));
    }
    host.powershell
        .is_some()
        .then_some((ShellFamily::PowerShell, true))
}

/// The shell `family` drives on `host`, or `None` when the host carries no
/// executable for it.
pub(super) fn family_config(family: ShellFamily, host: &HostShells) -> Option<ShellConfig> {
    match family {
        ShellFamily::Bash => Some(ShellConfig::default_for(host.platform)),
        ShellFamily::GitBash => host.git_bash.clone().map(|executable| ShellConfig {
            flavor: ShellFlavor::GitBash,
            arguments: windows_shell_arguments(&executable),
            executable,
        }),
        ShellFamily::PowerShell => host.powershell.clone().map(|executable| ShellConfig {
            flavor: ShellFlavor::PowerShell,
            arguments: windows_shell_arguments(&executable),
            executable,
        }),
    }
}

/// Reference `build_windows_shell_argv`, which reads the argument form from the
/// executable's own name rather than from the family that resolved it: an
/// override pointing a family at another interpreter still gets that
/// interpreter's flags.
pub(super) fn windows_shell_arguments(executable: &Path) -> Vec<String> {
    // A Windows path reaches this from the reference resolvers, from an
    // operator's configuration and from a call override, so the basename is
    // taken on both separators rather than on the host's.
    let name = executable
        .to_string_lossy()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_lowercase();
    match name.as_str() {
        "pwsh" | "pwsh.exe" | "powershell" | "powershell.exe" => {
            vec![
                "-NoLogo".to_owned(),
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
            ]
        }
        "bash" | "bash.exe" => vec!["-c".to_owned()],
        _ => Vec::new(),
    }
}
