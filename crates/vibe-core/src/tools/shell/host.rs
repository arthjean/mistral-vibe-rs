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
use crate::tools::ToolError;
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

    /// What the family removes from a child's environment: reference
    /// `_shell_environment` drops `LC_ALL` on a POSIX host so its `LC_CTYPE`
    /// takes effect.
    pub(super) fn unset_environment(self, managed: bool) -> Vec<String> {
        if self == Self::Bash && !managed && !cfg!(windows) {
            vec!["LC_ALL".to_owned()]
        } else {
            Vec::new()
        }
    }

    pub(super) fn tool_name(self, suffix: &str) -> String {
        format!("{}_{suffix}", self.name())
    }

    /// What the family forces into a child's environment.
    ///
    /// Reference `_get_git_bash_env_overrides` and `_get_windows_env_overrides`
    /// pin the same three interactivity switches and a pager that exits, so a
    /// command that would wait for a terminal no operator is watching fails or
    /// finishes instead of hanging the session, and reference
    /// `_shell_environment` does the same for the legacy `bash` tool.
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
            // Reference `_shell_environment`, which `spawn_shell_command`
            // hands the legacy `bash` tool: a non-interactive child with a
            // pager that exits and UTF-8 output. `LC_ALL` is removed by the
            // caller, since it would override `LC_CTYPE`.
            (Self::Bash, false) if cfg!(windows) => owned(&[
                ("CI", "true"),
                ("NONINTERACTIVE", "1"),
                ("NO_TTY", "1"),
                ("GIT_PAGER", "more"),
                ("PAGER", "more"),
            ]),
            (Self::Bash, false) => owned(&[
                ("CI", "true"),
                ("NONINTERACTIVE", "1"),
                ("NO_TTY", "1"),
                ("TERM", "dumb"),
                ("DEBIAN_FRONTEND", "noninteractive"),
                ("GIT_PAGER", "cat"),
                ("PAGER", "cat"),
                ("LESS", "-FX"),
                ("LC_CTYPE", "C.UTF-8"),
            ]),
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
        ShellFamily::Bash => Some(legacy_bash_config(host)),
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

/// Reference `PosixManagedShellBackend.resolve_shell`'s fallback ladder: zsh,
/// then bash, then sh, each first by name on `PATH` and then at its two usual
/// absolute locations. `$SHELL` is not consulted.
const POSIX_SHELL_LADDER: [&str; 9] = [
    "zsh",
    "/bin/zsh",
    "/usr/bin/zsh",
    "bash",
    "/bin/bash",
    "/usr/bin/bash",
    "sh",
    "/bin/sh",
    "/usr/bin/sh",
];

/// The shell a managed session starts, resolved the way the family's reference
/// backend resolves it: the call's `shell` argument, then the tool's `shell`
/// configuration key, then the family's own default.
///
/// A requested or configured shell that does not resolve is refused rather
/// than replaced: reference `resolve_shell` raises, so the session never starts
/// under a shell the caller did not ask for. `default` is the family's
/// configuration on this host, which carries the executable a Windows family
/// was published against.
pub(super) fn resolve_session_shell(
    family: ShellFamily,
    default: &ShellConfig,
    requested: Option<&str>,
    configured: Option<&str>,
) -> Result<ShellConfig, ToolError> {
    let requested = requested.filter(|value| !value.is_empty());
    let configured = configured.filter(|value| !value.is_empty());
    match family {
        ShellFamily::Bash => {
            let executable = if let Some(requested) = requested {
                resolve_posix_executable(requested).ok_or_else(|| {
                    ToolError::Execution(format!("requested shell is not executable: {requested}"))
                })?
            } else if let Some(configured) = configured {
                resolve_posix_executable(configured).ok_or_else(|| {
                    ToolError::Execution(format!(
                        "configured shell is not executable: {configured}"
                    ))
                })?
            } else {
                POSIX_SHELL_LADDER
                    .iter()
                    .find_map(|candidate| resolve_posix_executable(candidate))
                    .ok_or_else(|| {
                        ToolError::Execution(
                            "no POSIX shell found; expected zsh, bash, or sh".to_owned(),
                        )
                    })?
            };
            Ok(ShellConfig {
                flavor: default.flavor,
                executable,
                arguments: vec!["-lc".to_owned()],
            })
        }
        ShellFamily::GitBash | ShellFamily::PowerShell => {
            let Some(source) = requested.or(configured) else {
                return Ok(default.clone());
            };
            let Some(executable) = resolve_windows_executable(source) else {
                let kind = if requested.is_some() {
                    "requested"
                } else {
                    "configured"
                };
                return Err(ToolError::Execution(format!(
                    "{kind} shell is not executable: {source}"
                )));
            };
            let resolved_family = windows_shell_family(&executable);
            match (family, resolved_family) {
                (ShellFamily::GitBash, WindowsShellKind::Bash)
                | (ShellFamily::PowerShell, WindowsShellKind::PowerShell) => {}
                (ShellFamily::GitBash, _) => {
                    return Err(ToolError::Execution(format!(
                        "Git Bash shell override must resolve to bash.exe, got {source}"
                    )));
                }
                _ => {
                    return Err(ToolError::Execution(format!(
                        "PowerShell shell override must resolve to pwsh.exe or powershell.exe, \
                         got {source}"
                    )));
                }
            }
            Ok(ShellConfig {
                flavor: default.flavor,
                arguments: windows_shell_arguments(&executable),
                executable,
            })
        }
    }
}

/// Reference `_posix.py` `_resolve_executable`: a candidate carrying a path
/// separator must name an executable file, `~` expanded; a bare name is looked
/// up on `PATH` the way `shutil.which` looks it up.
pub(super) fn resolve_posix_executable(candidate: &str) -> Option<PathBuf> {
    let expanded = expand_home(candidate);
    if candidate.contains('/') {
        return is_executable_file(&expanded).then_some(expanded);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(candidate))
        .find(|found| is_executable_file(found))
}

/// Reference `_windows.py` `_resolve_executable`: a candidate that looks like a
/// path (either separator or a drive colon) must name a file; a bare name is
/// looked up on `PATH`, trying the `PATHEXT` suffixes a Windows `which` tries.
fn resolve_windows_executable(candidate: &str) -> Option<PathBuf> {
    let expanded = expand_home(candidate);
    if candidate.contains(['/', '\\', ':']) {
        return expanded.is_file().then_some(expanded);
    }
    let path = std::env::var_os("PATH")?;
    let extensions = std::env::var("PATHEXT")
        .ok()
        .filter(|_| cfg!(windows))
        .map(|value| {
            value
                .split(';')
                .filter(|suffix| !suffix.is_empty())
                .map(str::to_lowercase)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    std::env::split_paths(&path).find_map(|directory| {
        let exact = directory.join(candidate);
        if exact.is_file() {
            return Some(exact);
        }
        extensions
            .iter()
            .map(|suffix| directory.join(format!("{candidate}{suffix}")))
            .find(|found| found.is_file())
    })
}

/// `Path.expanduser`: a leading `~` names the home directory.
fn expand_home(candidate: &str) -> PathBuf {
    let home = || std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    if candidate == "~" {
        if let Some(home) = home() {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = candidate
        .strip_prefix("~/")
        .or_else(|| candidate.strip_prefix("~\\"))
        && let Some(home) = home()
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(candidate)
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Reference `_shell_family_from_executable`, read off the basename on either
/// separator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WindowsShellKind {
    PowerShell,
    Bash,
    Custom,
}

pub(super) fn windows_shell_family(executable: &Path) -> WindowsShellKind {
    let name = executable
        .to_string_lossy()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_lowercase();
    match name.as_str() {
        "pwsh" | "pwsh.exe" | "powershell" | "powershell.exe" => WindowsShellKind::PowerShell,
        "bash" | "bash.exe" => WindowsShellKind::Bash,
        _ => WindowsShellKind::Custom,
    }
}

/// The interpreter the `bash` family drives.
///
/// On a POSIX host reference `spawn_shell_command` hands the command to
/// `asyncio.create_subprocess_shell` with `$SHELL` as the executable, which
/// runs `<shell> -c <command>`, and `/bin/sh` when `$SHELL` is unset. The
/// executable is re-read at every call (see [`legacy_posix_shell`]); this is
/// the family's shape. On Windows reference `resolve_windows_shell` prefers a
/// detected Git Bash, driven with `-c`, and otherwise runs `cmd.exe /d /c`.
/// A managed session resolves its own shell through [`resolve_session_shell`]
/// and keeps only the flavor from here.
pub(super) fn legacy_bash_config(host: &HostShells) -> ShellConfig {
    match host.platform {
        Platform::Posix | Platform::GitBash => ShellConfig {
            flavor: ShellFlavor::Posix,
            executable: legacy_posix_shell(),
            arguments: vec!["-c".to_owned()],
        },
        Platform::Windows => match &host.git_bash {
            Some(bash) => ShellConfig {
                flavor: ShellFlavor::GitBash,
                executable: bash.clone(),
                arguments: vec!["-c".to_owned()],
            },
            None => ShellConfig {
                flavor: ShellFlavor::Cmd,
                executable: windows_cmd_path(),
                arguments: vec!["/d".to_owned(), "/c".to_owned()],
            },
        },
    }
}

/// Reference `create_subprocess_shell(executable=os.environ.get("SHELL"))`.
pub(super) fn legacy_posix_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .map_or_else(|| PathBuf::from("/bin/sh"), PathBuf::from)
}

/// Reference `_get_windows_cmd_path`: `COMSPEC` when it names `cmd`, then
/// `%SystemRoot%\System32\cmd.exe`, then the bare name.
fn windows_cmd_path() -> PathBuf {
    if let Some(comspec) = std::env::var("COMSPEC").ok().filter(|comspec| {
        let name = comspec
            .trim_matches('"')
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .to_lowercase();
        name == "cmd" || name == "cmd.exe"
    }) {
        return PathBuf::from(comspec);
    }
    if let Ok(root) = std::env::var("SystemRoot") {
        return PathBuf::from(format!("{}\\System32\\cmd.exe", root.trim_matches('"')));
    }
    PathBuf::from("cmd.exe")
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
