//! The shell tool families.
//!
//! The reference publishes three of them, one per shell it knows how to drive:
//! `bash` on a POSIX host, and `git_bash` or `powershell` on Windows. Each
//! publishes two variants of its command tool and picks between them with a
//! rollout gate. The legacy one runs a single command to completion; the
//! managed one starts a session that outlives the call and is polled, fed and
//! listed through `<family>_output`, `<family>_stdin`, `<family>_sessions` and
//! `<family>_log_file`. Every variant and every session tool is built here on
//! [`TerminalManager`], the process abstraction this workspace already owns, so
//! the shell surface adds tool families rather than a second way to spawn a
//! child.
//!
//! Which family a host publishes is data, not a compilation target: a
//! [`HostShells`] value carries the platform and the executables the Windows
//! families need, so the surface a Windows operator sees is decided by the same
//! function on every host and can be proven from a POSIX one.
//!
//! Two invariants shape the module. Every command is analyzed by
//! [`analyze_shell`] before it runs, and a command the analysis does not permit
//! outright reaches the operator as an approval request; nothing executes at a
//! looser mode than the analysis returns, and an override the analysis of the
//! command text cannot see (`cwd`, `shell`, `env`) lowers that mode itself. And
//! every terminal this family opens is owned by a guard, so a dropped turn
//! terminates the process group instead of leaving it behind.

use std::collections::BTreeMap;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::platform::{Platform, parse_policy_path};
use crate::policy::{
    ApprovalAgent, PermissionContext, PermissionMode, PermissionRequirement, PermissionStore,
    PolicyGuardedTool, ToolGuard,
};
use crate::process::{
    ClientShellRequest, ClientToolIo, ProcessChunk, ProcessError, ProcessSpec, ProcessStream,
    TerminalManager, TerminalState,
};
use crate::schema::{ObjectSchema, Property};
use crate::shell::{
    ShellAnalysis, ShellCommandLists, ShellConfig, ShellFlavor, ShellPolicyContext, analyze_shell,
};
use crate::tools::config::{
    ShellCommandConfig, ShellInlineConfig, ShellOutputConfig, ToolConfigResolver, declared_document,
};
use crate::tools::{
    OwnedToolHandlerFuture, RegistrationOutcome, ToolAvailability, ToolCondition, ToolError,
    ToolExecutionOutput, ToolHandler, ToolHandlerFuture, ToolInvocation, ToolOutputSink,
    ToolPresentationKind, ToolRegistry, ToolSource, ToolSpec,
};

/// Reference `TerminalSessionManager.base_dir`, relative to the Vibe home.
const LOG_DIRECTORY: &str = "shell-tool";
/// Reference `TerminalSessionManager.sessions_dir`, relative to [`LOG_DIRECTORY`].
const SESSIONS_DIRECTORY: &str = "sessions";
/// How often the pump drains a managed session's terminal into its log.
const PUMP_INTERVAL: Duration = Duration::from_millis(25);
/// Reference `BaseTool.selection_priority`, carried by the legacy variant.
const LEGACY_SELECTION_PRIORITY: i32 = 0;
/// Reference `ExperimentalBash.selection_priority`, which is what makes the
/// managed variant win the family name when both are registered.
const MANAGED_SELECTION_PRIORITY: i32 = 10;

/// Reference `ControlKey`, in declaration order, paired with the bytes each one
/// writes. The schema publishes the names; the stdin tool writes the bytes.
const CONTROL_KEYS: [(&str, &[u8]); 40] = [
    ("ctrl_@", b"\x00"),
    ("ctrl_a", b"\x01"),
    ("ctrl_b", b"\x02"),
    ("ctrl_c", b"\x03"),
    ("ctrl_d", b"\x04"),
    ("ctrl_e", b"\x05"),
    ("ctrl_f", b"\x06"),
    ("ctrl_g", b"\x07"),
    ("ctrl_h", b"\x08"),
    ("ctrl_i", b"\x09"),
    ("tab", b"\x09"),
    ("ctrl_j", b"\x0a"),
    ("enter", b"\r"),
    ("return", b"\r"),
    ("ctrl_k", b"\x0b"),
    ("ctrl_l", b"\x0c"),
    ("ctrl_m", b"\r"),
    ("ctrl_n", b"\x0e"),
    ("ctrl_o", b"\x0f"),
    ("ctrl_p", b"\x10"),
    ("ctrl_q", b"\x11"),
    ("ctrl_r", b"\x12"),
    ("ctrl_s", b"\x13"),
    ("ctrl_t", b"\x14"),
    ("ctrl_u", b"\x15"),
    ("ctrl_v", b"\x16"),
    ("ctrl_w", b"\x17"),
    ("ctrl_x", b"\x18"),
    ("ctrl_y", b"\x19"),
    ("ctrl_z", b"\x1a"),
    ("esc", b"\x1b"),
    ("escape", b"\x1b"),
    ("backspace", b"\x7f"),
    ("delete", b"\x1b[3~"),
    ("up", b"\x1b[A"),
    ("down", b"\x1b[B"),
    ("right", b"\x1b[C"),
    ("left", b"\x1b[D"),
    ("home", b"\x1b[H"),
    ("end", b"\x1b[F"),
];

/// Which variant of the host's shell family the session publishes.
///
/// The reference resolves this from a remote experiment whose default variant
/// is `legacy`, and the Rust port has no experiment client, so the managed
/// family stays absent unless an operator asks for it. The environment
/// variable is that ask: it is the only local switch, and it mirrors how
/// `web_search` resolves its credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellRollout {
    /// Reference `MANAGED_SHELL_TOOLS_LEGACY`, the default variant.
    #[default]
    Legacy,
    /// Reference `MANAGED_SHELL_TOOLS_MANAGED`.
    Managed,
}

impl ShellRollout {
    /// The variant `variable` selects, defaulting to [`ShellRollout::Legacy`]
    /// exactly as the reference experiment does when nothing resolves.
    #[must_use]
    pub fn from_environment(variable: &str) -> Self {
        match std::env::var(variable).ok().as_deref() {
            Some("1" | "true" | "managed") => Self::Managed,
            _ => Self::Legacy,
        }
    }
}

/// The published shell families, each owning five reference names built on its
/// own prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellFamily {
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
    fn name(self) -> &'static str {
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
    fn uses_posix_shell(self) -> bool {
        matches!(self, Self::Bash | Self::GitBash)
    }

    fn tool_name(self, suffix: &str) -> String {
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
    fn environment(self, managed: bool) -> Vec<(String, String)> {
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
fn find_git_bash(directories: &[PathBuf]) -> Option<PathBuf> {
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
fn is_wsl_launcher(candidate: &Path) -> bool {
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
fn find_powershell(directories: &[PathBuf]) -> Option<PathBuf> {
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
fn published_family(host: &HostShells, rollout: ShellRollout) -> Option<(ShellFamily, bool)> {
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
fn family_config(family: ShellFamily, host: &HostShells) -> Option<ShellConfig> {
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
fn windows_shell_arguments(executable: &Path) -> Vec<String> {
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

/// The shell tools, and the terminals and sessions they keep per Vibe session.
///
/// One instance serves every session, like [`crate::tools::builtins`]: the
/// managed sessions are keyed by session id so a re-registration after an agent
/// switch finds the sessions it left running.
/// What the host offers, re-read on demand.
///
/// The reference availability predicates probe the machine on every
/// publication, so the family a session publishes is a question asked again at
/// each turn rather than an answer frozen at registration.
pub type HostResolver = Arc<dyn Fn() -> HostShells + Send + Sync>;

#[derive(Clone)]
pub struct ShellTools {
    vibe_home: PathBuf,
    rollout: ShellRollout,
    host: HostResolver,
    sessions: Arc<StdMutex<BTreeMap<String, Arc<SessionShell>>>>,
}

impl std::fmt::Debug for ShellTools {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShellTools")
            .field("vibe_home", &self.vibe_home)
            .field("rollout", &self.rollout)
            .field("host", &(self.host)())
            .finish_non_exhaustive()
    }
}

/// One Vibe session's shell state: the terminals it opened and the managed
/// sessions still addressable by the model.
struct SessionShell {
    family: ShellFamily,
    terminals: TerminalManager,
    managed: Mutex<BTreeMap<String, Arc<ManagedSession>>>,
    /// The sessions a previous process left behind, by id, holding the manifest
    /// each one wrote before its client stopped. Guarded by a blocking lock
    /// because the family loads them while it is being constructed, which is
    /// synchronous.
    orphaned: StdMutex<BTreeMap<String, Value>>,
    log_root: PathBuf,
}

impl SessionShell {
    fn sessions_directory(&self) -> PathBuf {
        self.log_root.join(SESSIONS_DIRECTORY)
    }

    /// Reads the manifests this family owns and records what they describe.
    ///
    /// Reference `_load_orphaned_manifests` rewrites a manifest that still says
    /// `running`, because the process that would have settled it is gone. A
    /// manifest that cannot be read is skipped rather than failing the load, so
    /// one corrupt file never hides the sessions beside it.
    fn load_orphaned_manifests(&self) {
        let directory = self.sessions_directory();
        let Ok(entries) = std::fs::read_dir(&directory) else {
            return;
        };
        let mut orphaned = BTreeMap::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let Some(mut metadata) = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .filter(Value::is_object)
            else {
                continue;
            };
            let Some(id) = metadata
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|id| is_family_session_id(self.family, id))
                .map(str::to_owned)
            else {
                continue;
            };
            if metadata.get("status").and_then(Value::as_str)
                == Some(SessionStatus::Running.as_str())
            {
                metadata["status"] = Value::String(SessionStatus::Orphaned.as_str().to_owned());
                metadata["updatedAtMs"] = Value::String(now_ms().to_string());
                if let Ok(rendered) = serde_json::to_vec_pretty(&metadata) {
                    let _ = std::fs::write(&path, rendered);
                }
            }
            orphaned.insert(id, metadata);
        }
        if let Ok(mut store) = self.orphaned.lock() {
            *store = orphaned;
        }
    }

    /// The manifest recorded for `session_id`, if it names an orphan.
    fn orphan(&self, session_id: &str) -> Option<Value> {
        self.orphaned
            .lock()
            .ok()
            .and_then(|store| store.get(session_id).cloned())
    }

    fn orphans(&self) -> Vec<Value> {
        self.orphaned
            .lock()
            .map(|store| store.values().cloned().collect())
            .unwrap_or_default()
    }

    fn forget_orphan(&self, session_id: &str) {
        if let Ok(mut store) = self.orphaned.lock() {
            store.remove(session_id);
        }
    }
}

impl ShellTools {
    #[must_use]
    pub fn new(vibe_home: impl Into<PathBuf>, rollout: ShellRollout) -> Self {
        Self::with_host_resolver(vibe_home, rollout, Arc::new(HostShells::detect))
    }

    /// The same tools against a stated host, which is how a POSIX test drives
    /// the Windows families.
    #[must_use]
    pub fn with_host(
        vibe_home: impl Into<PathBuf>,
        rollout: ShellRollout,
        host: HostShells,
    ) -> Self {
        Self::with_host_resolver(vibe_home, rollout, Arc::new(move || host.clone()))
    }

    /// The same tools against a host answered anew at every publication, which
    /// is what [`ShellTools::new`] installs and what lets a test move the
    /// machine under a running session.
    #[must_use]
    pub fn with_host_resolver(
        vibe_home: impl Into<PathBuf>,
        rollout: ShellRollout,
        host: HostResolver,
    ) -> Self {
        Self {
            vibe_home: vibe_home.into(),
            rollout,
            host,
            sessions: Arc::new(StdMutex::new(BTreeMap::new())),
        }
    }

    /// Publishes this host's shell family for one session.
    ///
    /// The legacy variant registers first, matching the reference rollout gate,
    /// which keeps `legacy` available wherever the managed rollout is off. When
    /// the managed rollout is on, the managed variant registers too and wins
    /// the family name by selection priority, and the four session tools join
    /// it. A host with no family to publish registers nothing.
    pub fn register(
        &self,
        session_id: &str,
        working_directory: &Path,
        registry: &ToolRegistry,
        client_io: Option<ClientToolIo>,
        guard: &ToolGuard,
    ) -> Result<Vec<RegistrationOutcome>, ToolError> {
        let ToolGuard {
            policy,
            approval,
            config,
            scratchpad,
        } = guard;
        let host = (self.host)();
        let Some((family, managed)) = published_family(&host, self.rollout) else {
            return Ok(Vec::new());
        };
        let Some(shell_config) = family_config(family, &host) else {
            return Ok(Vec::new());
        };
        // The three shell lists are composed from the interpreter this family
        // drives, not from the host, so the resolver follows the family. The
        // settings cache is shared, so an operator's `tools.<family>` entry
        // still reaches every tool published below.
        let config = config.clone().with_posix_shell(family.uses_posix_shell());
        let shell = self.session_shell(session_id, family)?;
        let platform = host.platform;
        let working_directory = working_directory.to_path_buf();
        // Reference `git_bash_shell_available` and `powershell_shell_available`
        // are re-read on every publication, so a family whose interpreter is
        // uninstalled mid-session drops out of the surface at the next turn.
        // The POSIX family has no such probe: reference `Bash.is_available` is
        // the inherited default.
        let condition: Option<ToolCondition> = match family {
            ShellFamily::Bash => None,
            ShellFamily::GitBash | ShellFamily::PowerShell => {
                let resolver = self.host.clone();
                let rollout = self.rollout;
                Some(Arc::new(move || {
                    published_family(&resolver(), rollout)
                        .is_some_and(|(published, _)| published == family)
                }))
            }
        };
        let publish = |spec, handler| match &condition {
            Some(condition) => registry.register_conditional(spec, handler, condition.clone()),
            None => registry.register(spec, handler),
        };
        let mut outcomes = vec![publish(
            command_spec(family, false),
            guarded_command(CommandWiring {
                family,
                shell: shell.clone(),
                shell_config: shell_config.clone(),
                tool_config: config.clone(),
                working_directory: working_directory.clone(),
                platform,
                policy: policy.clone(),
                approval: approval.clone(),
                managed: false,
                client_io: client_io.clone(),
                scratchpad: scratchpad.clone(),
            }),
        )?];
        if !managed {
            return Ok(outcomes);
        }
        outcomes.push(publish(
            command_spec(family, true),
            guarded_command(CommandWiring {
                family,
                shell: shell.clone(),
                shell_config,
                tool_config: config.clone(),
                working_directory,
                platform,
                policy: policy.clone(),
                approval: approval.clone(),
                managed: true,
                client_io,
                scratchpad: scratchpad.clone(),
            }),
        )?);
        // The three polling tools are configured `always` upstream and produce
        // no granular requirement, so the guard in front of them is exactly
        // what reads that permission and the lists beside it.
        outcomes.push(publish(
            output_spec(family),
            guarded_session(
                family.tool_name("output"),
                policy,
                approval,
                session_handler(
                    shell.clone(),
                    config.clone(),
                    family.tool_name("output"),
                    run_output,
                ),
            ),
        )?);
        outcomes.push(publish(
            stdin_spec(family),
            guarded_session(
                family.tool_name("stdin"),
                policy,
                approval,
                session_handler(
                    shell.clone(),
                    config.clone(),
                    family.tool_name("stdin"),
                    run_stdin,
                ),
            ),
        )?);
        outcomes.push(publish(
            sessions_spec(family),
            guarded_session(
                family.tool_name("sessions"),
                policy,
                approval,
                session_handler(
                    shell.clone(),
                    config.clone(),
                    family.tool_name("sessions"),
                    run_sessions,
                ),
            ),
        )?);
        let log_shell = shell.clone();
        outcomes.push(publish(
            log_file_spec(family),
            Arc::new(PolicyGuardedTool::new(
                family.tool_name("log_file"),
                policy.clone(),
                approval.clone(),
                Arc::new(move |invocation| {
                    log_file_requirements(&log_shell, &invocation.arguments)
                }),
                session_handler(
                    shell,
                    config.clone(),
                    family.tool_name("log_file"),
                    run_log_file,
                ),
            )),
        )?);
        Ok(outcomes)
    }

    /// Terminates every process this session started and forgets its state.
    ///
    /// A managed session outlives the call that started it by design, so
    /// session teardown is the only place that can stop it.
    pub async fn close_session(&self, session_id: &str) -> Result<(), ToolError> {
        let Some(shell) = self.take_session_shell(session_id)? else {
            return Ok(());
        };
        // Each session's manifest is settled before its terminal is torn down,
        // so a later process reads what happened to it rather than reporting a
        // session this one deliberately stopped as orphaned.
        for session in shell.managed.lock().await.values() {
            if session.is_running() {
                session.settle(SessionStatus::Killed, session.snapshot().1);
            }
        }
        shell.managed.lock().await.clear();
        shell
            .terminals
            .cleanup_all()
            .await
            .map(drop)
            .map_err(|error| ToolError::Execution(error.to_string()))
    }

    fn session_shell(
        &self,
        session_id: &str,
        family: ShellFamily,
    ) -> Result<Arc<SessionShell>, ToolError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| ToolError::Execution("the shell session lock is poisoned".to_owned()))?;
        Ok(sessions
            .entry(session_id.to_owned())
            .or_insert_with(|| {
                let shell = Arc::new(SessionShell {
                    family,
                    terminals: TerminalManager::default(),
                    managed: Mutex::new(BTreeMap::new()),
                    orphaned: StdMutex::new(BTreeMap::new()),
                    log_root: self.vibe_home.join(LOG_DIRECTORY),
                });
                // Reference `TerminalSessionManager.__init__` reads the
                // manifests as the family is built, so the first `sessions`
                // call of a fresh process already lists what the previous one
                // left running.
                shell.load_orphaned_manifests();
                shell
            })
            .clone())
    }

    fn take_session_shell(&self, session_id: &str) -> Result<Option<Arc<SessionShell>>, ToolError> {
        self.sessions
            .lock()
            .map_err(|_| ToolError::Execution("the shell session lock is poisoned".to_owned()))
            .map(|mut sessions| sessions.remove(session_id))
    }
}

// --------------------------------------------------------------------------
// Specifications
// --------------------------------------------------------------------------

/// Directive coverage for a family's command tool, whose reference description
/// this port must cover without reproducing (`NOTICE`).
///
/// | Reference directive | Covered by |
/// |---|---|
/// | The tool runs one shell command in the workspace | "Run one shell command in the working directory" |
/// | Prefer the file tools over shell equivalents for reading and searching | "reach for read_file and grep instead of cat and grep(1)" |
/// | Quote paths that carry spaces | "Quote every path that carries a space" |
/// | A command that needs approval is paused until the operator answers | "A command the policy does not allow outright waits for approval" |
/// | The timeout is optional and bounded | the `timeout` description |
/// | The managed variant can return a live session instead of waiting | the `background` description |
/// | The Windows families name the shell they drive | the family sentence |
///
/// The legacy Windows variants publish the four overrides the POSIX one does
/// not, because reference `GitBashArgs` and `WindowsShellArgs` carry them even
/// though `BashArgs` does not.
fn command_spec(family: ShellFamily, managed: bool) -> ToolSpec {
    let description = format!(
        "Run one {} command in the working directory. Reach for read_file and grep instead of cat \
         and grep(1), quote every path that carries a space, and expect that a command the policy \
         does not allow outright waits for approval before it runs.",
        match family {
            ShellFamily::Bash => "shell",
            ShellFamily::GitBash => "Git Bash",
            ShellFamily::PowerShell => "PowerShell",
        }
    );
    let overrides = managed || family != ShellFamily::Bash;
    let mut schema = ObjectSchema::new().required(
        "command",
        Property::string().described(if managed {
            "The shell command to run"
        } else {
            "The shell command to execute"
        }),
    );
    schema = schema.optional(
        "timeout",
        Property::integer()
            .described(if managed {
                "How long to wait, in seconds, before the process group is killed"
            } else {
                "How long to wait, in seconds, before the command is abandoned"
            })
            .with_default(Value::Null)
            .nullable(),
    );
    if managed {
        schema = schema.optional(
            "background",
            Property::boolean()
                .described("Return a live session immediately instead of waiting for the exit")
                .with_default(false),
        );
    }
    if overrides {
        schema = schema
            .optional(
                "timeout_seconds",
                Property::number()
                    .constrained("minimum", 0)
                    .described("How long to wait in the foreground before the session is left running or killed")
                    .with_default(Value::Null)
                    .nullable(),
            );
    }
    if managed {
        schema = schema.optional(
            "hard_timeout",
            Property::boolean()
                .described("Kill the process group when timeout_seconds expires instead of leaving the session running")
                .with_default(false),
        );
    }
    if overrides {
        schema = schema
            .optional(
                "cwd",
                Property::string()
                    .described("Run somewhere other than the working directory")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "env",
                Property::map(Property::string())
                    .described("Environment variables added to the ones the session inherits")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "shell",
                Property::string()
                    .described("Run the command through another shell executable")
                    .with_default(Value::Null)
                    .nullable(),
            );
    }
    ToolSpec {
        name: family.name().to_owned(),
        description,
        input_schema: schema.build(),
        output_schema: None,
        config: declared_document(family.name()),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Shell,
        source: ToolSource::BuiltIn,
        selection_priority: if managed {
            MANAGED_SELECTION_PRIORITY
        } else {
            LEGACY_SELECTION_PRIORITY
        },
    }
}

/// Directive coverage for a family's `_output` tool.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | Poll a running or finished session for its output | "Read what a session has written" |
/// | Pass back the cursor to read only what is new | the `cursor` description |
/// | Waiting is optional and bounded | the `wait_seconds` description |
/// | A finished session still answers with its last output and status | "A session that has exited still answers" |
fn output_spec(family: ShellFamily) -> ToolSpec {
    ToolSpec {
        name: family.tool_name("output"),
        description: format!(
            "Read what a {} session has written since a cursor. A session that has exited still \
             answers, with its final output and its exit status.",
            family.name()
        ),
        input_schema: ObjectSchema::new()
            .required("session_id", Property::string())
            .optional(
                "cursor",
                Property::integer()
                    .constrained("minimum", 0)
                    .described("The next_cursor a previous read returned")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "wait_seconds",
                Property::number().constrained("minimum", 0).with_default(0),
            )
            .optional(
                "max_bytes",
                Property::integer()
                    .constrained("exclusiveMinimum", 0)
                    .described("How many bytes to read at most")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .build(),
        output_schema: None,
        config: declared_document(&family.tool_name("output")),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Shell,
        source: ToolSource::BuiltIn,
        selection_priority: LEGACY_SELECTION_PRIORITY,
    }
}

/// Directive coverage for a family's `_stdin` tool.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | Feed a running session's standard input | "Write to a running session" |
/// | Text is sent verbatim, newline included | the `text` description |
/// | Control keys are named rather than encoded | the `control` description |
/// | Raw bytes travel as base64 | the `bytes_base64` description |
/// | Exactly one of the three inputs is supplied | "Supply exactly one of text, control or bytes_base64" |
fn stdin_spec(family: ShellFamily) -> ToolSpec {
    ToolSpec {
        name: family.tool_name("stdin"),
        description: format!(
            "Write to a running {} session. Supply exactly one of text, control or bytes_base64; \
             supplying none or several is refused rather than resolved by precedence.",
            family.name()
        ),
        input_schema: ObjectSchema::new()
            .required("session_id", Property::string())
            .optional(
                "text",
                Property::string()
                    .described("Text sent exactly as written; end it with \\n to press Enter")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "control",
                Property::array(Property::string().constrained("enum", json!(control_key_names())))
                    .described(
                        "Named control sequences, for example ctrl_c, ctrl_d, esc, tab, enter or \
                         the arrow keys",
                    ),
            )
            .optional(
                "bytes_base64",
                Property::string()
                    .described("Raw bytes to write, base64 encoded")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .build(),
        output_schema: None,
        config: declared_document(&family.tool_name("stdin")),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Shell,
        source: ToolSource::BuiltIn,
        selection_priority: LEGACY_SELECTION_PRIORITY,
    }
}

/// Directive coverage for a family's `_sessions` tool.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | `list` reports this family's sessions | the `action` description |
/// | `inspect` and `kill` need a session id and act on that one session | the `action` and `session_id` descriptions |
/// | `reset` stops every session in the family | the `action` description |
/// | `clear_logs` only applies to `reset` | the `clear_logs` description |
fn sessions_spec(family: ShellFamily) -> ToolSpec {
    ToolSpec {
        name: family.tool_name("sessions"),
        description: format!("Inspect and stop {} sessions.", family.name()),
        input_schema: ObjectSchema::new()
            .optional(
                "action",
                Property::string()
                    .constrained("enum", json!(["list", "inspect", "kill", "reset"]))
                    .described(
                        "`list` reports every session of this family, `inspect` reads the one \
                         named by session_id, `kill` stops exactly that one, and `reset` stops \
                         them all",
                    )
                    .with_default("list"),
            )
            .optional(
                "session_id",
                Property::string()
                    .described("Required by `inspect` and `kill`, ignored by `list` and `reset`")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "clear_logs",
                Property::boolean()
                    .described("Used by `reset` alone: also delete the stored session logs")
                    .with_default(false),
            )
            .optional(
                "max_bytes",
                Property::integer()
                    .constrained("exclusiveMinimum", 0)
                    .described("How many bytes of output to read at most")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .build(),
        output_schema: None,
        config: declared_document(&family.tool_name("sessions")),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Shell,
        source: ToolSource::BuiltIn,
        selection_priority: LEGACY_SELECTION_PRIORITY,
    }
}

/// Directive coverage for a family's `_log_file` tool.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | Read, write or append a file in the shell-tool directory | "Read, write or append a file the sessions keep" |
/// | A session id names that session's own log | "session_id names that session's log" |
/// | A relative path stays inside the shell-tool directory | "a relative path stays inside the session log directory" |
/// | Reading resumes from an offset and is bounded | the `offset` and `max_bytes` descriptions |
fn log_file_spec(family: ShellFamily) -> ToolSpec {
    ToolSpec {
        name: family.tool_name("log_file"),
        description: format!(
            "Read, write or append a file the {} sessions keep. session_id names that session's \
             log, and a relative path stays inside the session log directory.",
            family.name()
        ),
        input_schema: ObjectSchema::new()
            .required(
                "action",
                Property::string().constrained("enum", json!(["read", "write", "append"])),
            )
            .optional(
                "session_id",
                Property::string().with_default(Value::Null).nullable(),
            )
            .optional(
                "relative_path",
                Property::string().with_default(Value::Null).nullable(),
            )
            .optional(
                "offset",
                Property::integer()
                    .constrained("minimum", 0)
                    .described("Where to start reading, in bytes")
                    .with_default(0),
            )
            .optional(
                "max_bytes",
                Property::integer()
                    .constrained("exclusiveMinimum", 0)
                    .described("How many bytes to read at most")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .optional(
                "content",
                Property::string().with_default(Value::Null).nullable(),
            )
            .build(),
        output_schema: None,
        config: declared_document(&family.tool_name("log_file")),
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Shell,
        source: ToolSource::BuiltIn,
        selection_priority: LEGACY_SELECTION_PRIORITY,
    }
}

fn control_key_names() -> Vec<&'static str> {
    CONTROL_KEYS.iter().map(|(name, _)| *name).collect()
}

// --------------------------------------------------------------------------
// Policy
// --------------------------------------------------------------------------

/// Resolves one call's arguments to the analysis its command runs under.
type ShellAnalyzer = Arc<dyn Fn(&Value) -> Result<ShellAnalysis, ToolError> + Send + Sync>;

/// Runs [`analyze_shell`] and routes the call by what it decides.
///
/// The reference resolves a command to `ALWAYS`, `ASK` or `NEVER` before it
/// runs; this reproduces that split on top of the workspace's own analyzer. An
/// `Always` command executes directly, an `Ask` command goes through the
/// permission store, and a `Never` command is refused before a process exists.
struct ShellPolicyGuard {
    analysis: ShellAnalyzer,
    guarded: Arc<PolicyGuardedTool>,
    inner: Arc<dyn ToolHandler>,
}

impl ToolHandler for ShellPolicyGuard {
    fn invoke<'a>(
        &'a self,
        invocation: &'a ToolInvocation,
        output: ToolOutputSink,
    ) -> ToolHandlerFuture<'a> {
        Box::pin(async move {
            let analysis = (self.analysis)(&invocation.arguments)?;
            match analysis.mode {
                PermissionMode::Always => self.inner.invoke(invocation, output).await,
                PermissionMode::Ask => self.guarded.invoke(invocation, output).await,
                // The rationale travels with the refusal: a model that learns
                // why can propose something else instead of retrying.
                PermissionMode::Never => Err(ToolError::Execution(format!(
                    "the command is refused by the shell policy: {}",
                    analysis.rationale.join("; ")
                ))),
            }
        })
    }
}

/// What one command variant needs to reach a process under policy.
struct CommandWiring {
    family: ShellFamily,
    shell: Arc<SessionShell>,
    /// The interpreter this family drives.
    shell_config: ShellConfig,
    /// The `tools.<family>` settings the call reads its limits and lists from.
    tool_config: ToolConfigResolver,
    working_directory: PathBuf,
    platform: Platform,
    policy: PermissionStore,
    approval: Arc<dyn ApprovalAgent>,
    managed: bool,
    client_io: Option<ClientToolIo>,
    /// The session's own scratchpad, whose paths raise no requirement.
    scratchpad: Option<PathBuf>,
}

fn guarded_command(wiring: CommandWiring) -> Arc<dyn ToolHandler> {
    let CommandWiring {
        family,
        shell,
        shell_config,
        tool_config,
        working_directory,
        platform,
        policy,
        approval,
        managed,
        client_io,
        scratchpad,
    } = wiring;
    let inner = command_handler(
        shell,
        shell_config.clone(),
        tool_config.clone(),
        family.name().to_owned(),
        working_directory.clone(),
        managed,
        client_io,
    );
    // Reference `GitBashArgs` and `WindowsShellArgs` publish `cwd`, `shell` and
    // `env` on the legacy variant too, so the Windows families answer for them
    // whichever variant is selected.
    let overrides = managed || family != ShellFamily::Bash;
    let requirement_root = working_directory.clone();
    let requirement_flavor = shell_config.flavor;
    let requirement_config = tool_config.clone();
    let requirement_tool = family.name().to_owned();
    let requirement_scratchpad = scratchpad.clone();
    let guarded = Arc::new(PolicyGuardedTool::new(
        family.name(),
        policy,
        approval,
        Arc::new(move |invocation: &ToolInvocation| {
            let command = command_argument(&invocation.arguments)?;
            let settings: ShellCommandConfig = requirement_config.view(&requirement_tool);
            let lists = ShellCommandLists::from_config(&settings);
            let analysis = analyze(
                requirement_flavor,
                platform,
                &requirement_root,
                requirement_scratchpad.clone(),
                &command,
                &lists,
            );
            // The analysis already composed what the operator answers: one
            // requirement per session pattern, one per directory the call
            // leaves the workspace for, and one per `find` that runs a program.
            // Rebuilding them here would be a second vocabulary to keep in step
            // with the first.
            let mut requirements = analysis.requirements;
            if requirements.is_empty() {
                // An analysis that composed none still needs something to
                // approve, which is the whole command under its own pattern.
                requirements.push(PermissionRequirement::command(&command));
            }
            if overrides {
                requirements.extend(override_requirements(
                    &invocation.arguments,
                    &requirement_root,
                ));
            }
            let mut context = PermissionContext::asking(requirements);
            // A `cwd` override is a directory the call reaches, so it travels
            // on the context and is positioned against the trust roots the way
            // a file tool's path is. A root the operator revoked refuses the
            // call rather than becoming one more thing an approval reopens.
            if overrides && let Some(directory) = string_argument(&invocation.arguments, "cwd") {
                context.paths.push(PathBuf::from(directory));
            }
            Ok(context)
        }),
        inner.clone(),
    ));
    let analysis_root = working_directory;
    let analysis_flavor = shell_config.flavor;
    let analysis_config = tool_config;
    let analysis_tool = family.name().to_owned();
    let analysis_scratchpad = scratchpad;
    Arc::new(ShellPolicyGuard {
        analysis: Arc::new(move |arguments: &Value| {
            let command = command_argument(arguments)?;
            let settings: ShellCommandConfig = analysis_config.view(&analysis_tool);
            let mut analysis = analyze(
                analysis_flavor,
                platform,
                &analysis_root,
                analysis_scratchpad.clone(),
                &command,
                &ShellCommandLists::from_config(&settings),
            );
            // The command text is not all that runs: an override decides where
            // it runs, what interprets it and what it inherits, none of which
            // the analysis of the text can see. So a call carrying one stops
            // being allowed outright and reaches the operator instead.
            if overrides && !override_requirements(arguments, &analysis_root).is_empty() {
                analysis.mode = analysis.mode.min(PermissionMode::Ask);
                analysis.rationale.push(
                    "the call overrides the working directory, the shell or the environment"
                        .to_owned(),
                );
            }
            Ok(analysis)
        }),
        guarded,
        inner,
    })
}

fn analyze(
    flavor: ShellFlavor,
    platform: Platform,
    working_directory: &Path,
    scratchpad: Option<PathBuf>,
    command: &str,
    lists: &ShellCommandLists,
) -> ShellAnalysis {
    let Ok(root) = parse_policy_path(platform, &working_directory.to_string_lossy()) else {
        // A working directory the policy cannot parse is not a reason to run
        // unanalyzed: the call falls back to asking.
        return ShellAnalysis {
            mode: PermissionMode::Ask,
            rationale: vec!["the working directory is not a policy path".to_owned()],
            commands: Vec::new(),
            path_operands: Vec::new(),
            requirements: Vec::new(),
        };
    };
    analyze_shell(
        flavor,
        command,
        &ShellPolicyContext::new(platform, root).with_scratchpad(scratchpad),
        lists,
    )
}

/// What the managed variant's overrides require on top of the command itself.
///
/// The reference resolves the permission of a managed call from more than the
/// command text: a custom shell and a custom environment each carry their own
/// requirement (`_build_context_permissions`), and a working directory outside
/// the session root carries an outside-directory one (`_collect_outside_dirs`).
/// None of the three is visible to an analysis of the command string, so an
/// allowlisted command would otherwise run somewhere else, under another
/// interpreter, with an environment the operator never saw.
fn override_requirements(arguments: &Value, root: &Path) -> Vec<PermissionRequirement> {
    let mut requirements = Vec::new();
    if let Some(directory) = string_argument(arguments, "cwd")
        && !is_inside(root, Path::new(directory))
    {
        // Reference `_collect_outside_dirs` names the directory itself, joined
        // with `*`, rather than the file-shaped parent a file tool names.
        requirements.push(PermissionRequirement::outside_directory(
            &Path::new(directory).join("*").display().to_string(),
        ));
    }
    if let Some(shell) = string_argument(arguments, "shell") {
        // Reference `_build_context_permissions` carries the override verbatim
        // as both patterns, so approving one interpreter never approves another.
        requirements.push(PermissionRequirement::exact_command(&format!(
            "shell override: {shell}"
        )));
    }
    if let Some(names) = environment_names(arguments) {
        // The environment override is the one context permission the reference
        // widens for the session: the names change per call, so the session
        // pattern covers any of them.
        requirements.push(PermissionRequirement {
            scope: crate::policy::PermissionScope::CommandPattern,
            invocation_pattern: format!("env override: {names}"),
            session_pattern: "env override *".to_owned(),
            label: format!("env override: {names}"),
        });
    }
    requirements
}

/// The overridden variable names, sorted, or `None` when nothing is overridden.
fn environment_names(arguments: &Value) -> Option<String> {
    let overrides = arguments.get("env")?.as_object()?;
    if overrides.is_empty() {
        return None;
    }
    Some(overrides.keys().cloned().collect::<Vec<_>>().join(", "))
}

/// Whether `candidate` resolves inside `root`, answering `false` for anything
/// that cannot be resolved: an unresolvable directory is not a known-safe one.
fn is_inside(root: &Path, candidate: &Path) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    candidate
        .canonicalize()
        .is_ok_and(|resolved| resolved.starts_with(&root))
}

/// The log a `*_log_file` call touches, positioned against the trust roots.
///
/// The reference declares no requirement of its own here, so what decides is
/// the configured permission plus whether the log sits inside the workspace: a
/// session whose log directory was redirected outside it is asked about.
fn log_file_requirements(
    shell: &SessionShell,
    arguments: &Value,
) -> Result<PermissionContext, ToolError> {
    let path = resolve_log_path(shell, arguments)?;
    Ok(PermissionContext::deferred().over_paths(vec![path]))
}

// --------------------------------------------------------------------------
// Arguments
// --------------------------------------------------------------------------

fn command_argument(arguments: &Value) -> Result<String, ToolError> {
    let command = arguments["command"].as_str().unwrap_or_default().trim();
    if command.is_empty() {
        return Err(ToolError::SchemaViolation {
            path: "/command".to_owned(),
            message: "must not be empty".to_owned(),
        });
    }
    Ok(command.to_owned())
}

fn string_argument<'a>(arguments: &'a Value, name: &str) -> Option<&'a str> {
    arguments.get(name).and_then(Value::as_str)
}

/// The foreground wait, in seconds, bounded by the configured maximum.
fn timeout_argument(arguments: &Value, settings: &ShellCommandConfig) -> u64 {
    // The reference reads `args.timeout or default`, so a zero is falsy and
    // means the default rather than an instant timeout.
    let requested = arguments["timeout"]
        .as_u64()
        .filter(|seconds| *seconds > 0)
        .or_else(|| {
            arguments["timeout_seconds"].as_f64().map(|seconds| {
                // A fractional wait rounds up: waiting less than asked would report
                // a timeout the operator did not request.
                seconds.ceil().max(0.0) as u64
            })
        })
        .unwrap_or(settings.default_timeout);
    let ceiling = settings.max_timeout_seconds.max(1.0) as u64;
    requested.clamp(1, ceiling)
}

/// The read window one inline answer may carry: what the call asked for,
/// bounded by the configured window and by what the turn's budget has left.
fn byte_limit(arguments: &Value, sink: &ToolOutputSink, max_inline_bytes: usize) -> usize {
    let requested = arguments["max_bytes"]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(max_inline_bytes);
    requested
        .min(max_inline_bytes)
        .min(sink.remaining_bytes().max(1))
}

// --------------------------------------------------------------------------
// The command tool
// --------------------------------------------------------------------------

/// Owns a terminal until the call gives it up.
///
/// A cancelled turn drops the tool future, which drops this guard, which
/// terminates the process group. Without it a long command would survive the
/// turn that started it.
struct TerminalGuard {
    terminals: TerminalManager,
    terminal_id: Option<String>,
}

impl TerminalGuard {
    fn new(terminals: TerminalManager, terminal_id: String) -> Self {
        Self {
            terminals,
            terminal_id: Some(terminal_id),
        }
    }

    /// Hands ownership back: the caller has terminated or handed off the
    /// terminal itself.
    fn disarm(&mut self) {
        self.terminal_id = None;
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let Some(terminal_id) = self.terminal_id.take() else {
            return;
        };
        let terminals = self.terminals.clone();
        // Termination is asynchronous and `Drop` is not, so it runs as a task.
        // Outside a runtime there is nothing to spawn onto and nothing that
        // could have started a process either.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = terminals.interrupt(&terminal_id).await;
                let _ = terminals.release(&terminal_id).await;
            });
        }
    }
}

fn command_handler(
    shell: Arc<SessionShell>,
    config: ShellConfig,
    tool_config: ToolConfigResolver,
    tool_name: String,
    working_directory: PathBuf,
    managed: bool,
    client_io: Option<ClientToolIo>,
) -> Arc<dyn ToolHandler> {
    Arc::new(
        move |invocation: &ToolInvocation, output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let shell = shell.clone();
            let config = config.clone();
            // Read per call, so a raised timeout or output budget applies to
            // the next command rather than to the next session.
            let settings: ShellCommandConfig = tool_config.view(&tool_name);
            let working_directory = working_directory.clone();
            let arguments = invocation.arguments.clone();
            let client = client_io.clone();
            let call_id = invocation.call_id.clone();
            Box::pin(async move {
                // A client terminal is one command's terminal: it is created,
                // waited on and released inside this call, which is why the
                // delegation covers the legacy variant and leaves the managed
                // sessions the model addresses later on this host.
                if !managed
                    && let Some(delegated) = delegated_command(
                        client.as_ref(),
                        &working_directory,
                        &arguments,
                        &settings,
                        &output,
                        &call_id,
                    )
                    .await?
                {
                    return Ok(delegated);
                }
                if managed {
                    run_managed_command(
                        &shell,
                        &config,
                        &working_directory,
                        &arguments,
                        &settings,
                        &output,
                    )
                    .await
                } else {
                    run_legacy_command(
                        &shell,
                        &config,
                        &working_directory,
                        &arguments,
                        &settings,
                        &output,
                    )
                    .await
                }
            })
        },
    )
}

/// Runs one command on a client terminal, or `None` when the client hosts none.
async fn delegated_command(
    client: Option<&ClientToolIo>,
    working_directory: &Path,
    arguments: &Value,
    settings: &ShellCommandConfig,
    output: &ToolOutputSink,
    call_id: &str,
) -> Result<Option<ToolExecutionOutput>, ToolError> {
    let Some(client) = client.filter(|client| client.supports_terminal()) else {
        return Ok(None);
    };
    let command = command_argument(arguments)?;
    let timeout = timeout_argument(arguments, settings);
    let limit = settings
        .max_output_bytes
        .min(output.remaining_bytes().max(1));
    let result = client
        .run_shell(ClientShellRequest {
            tool_call_id: Some(call_id.to_owned()),
            command: command.clone(),
            args: None,
            env: None,
            cwd: working_directory.to_string_lossy().into_owned(),
            output_byte_limit: u64::try_from(limit).unwrap_or(u64::MAX),
            timeout: Duration::from_secs(timeout),
        })
        .await
        .map_err(|error| {
            ToolError::Execution(format!("the client terminal failed: {error}: `{command}`"))
        })?;
    command_output(
        &command,
        result.stdout,
        result.stderr,
        result.returncode,
        result.truncated,
        limit,
    )
    .map(Some)
}

async fn run_legacy_command(
    shell: &SessionShell,
    config: &ShellConfig,
    working_directory: &Path,
    arguments: &Value,
    settings: &ShellCommandConfig,
    output: &ToolOutputSink,
) -> Result<ToolExecutionOutput, ToolError> {
    let command = command_argument(arguments)?;
    let timeout = timeout_argument(arguments, settings);
    let terminal_id = shell
        .terminals
        .run(process_spec(
            shell.family,
            config,
            working_directory,
            &command,
            None,
            settings.max_output_bytes,
            false,
        ))
        .await
        .map_err(process_error)?;
    let mut guard = TerminalGuard::new(shell.terminals.clone(), terminal_id.clone());
    // Nothing feeds this command, so its standard input reads EOF rather than
    // blocking forever on a prompt no one answers.
    shell
        .terminals
        .close_stdin(&terminal_id)
        .await
        .map_err(process_error)?;
    let waited = tokio::time::timeout(
        Duration::from_secs(timeout),
        shell.terminals.wait(&terminal_id),
    )
    .await;
    let Ok(read) = waited else {
        shell
            .terminals
            .interrupt(&terminal_id)
            .await
            .map_err(process_error)?;
        let _ = shell.terminals.release(&terminal_id).await;
        guard.disarm();
        return Err(ToolError::Execution(format!(
            "the command timed out after {timeout}s and its process group was terminated: \
             `{command}`"
        )));
    };
    let read = read.map_err(process_error)?;
    guard.disarm();
    let _ = shell.terminals.release(&terminal_id).await;

    let limit = settings
        .max_output_bytes
        .min(output.remaining_bytes().max(1));
    let (stdout, stdout_truncated) = render_stream(&read.chunks, ProcessStream::Stdout, limit);
    let (stderr, stderr_truncated) = render_stream(&read.chunks, ProcessStream::Stderr, limit);
    let truncated = stdout_truncated || stderr_truncated || read.backpressure_dropped;
    let status = exit_status(&read.state);
    command_output(&command, stdout, stderr, status, truncated, limit)
}

/// What one finished command reports, whether this host ran it or a client did.
///
/// A non-zero status is a tool failure rather than a result, so the two paths
/// share this rather than each deciding when a command counts as failed.
fn command_output(
    command: &str,
    stdout: String,
    stderr: String,
    status: i32,
    truncated: bool,
    limit: usize,
) -> Result<ToolExecutionOutput, ToolError> {
    if status != 0 {
        return Err(ToolError::Execution(format!(
            "the command failed with exit status {status}: `{command}`\nstderr:\n{stderr}\n\
             stdout:\n{stdout}"
        )));
    }
    let mut model_text = stdout.clone();
    if !stderr.is_empty() {
        model_text.push_str("\nstderr:\n");
        model_text.push_str(&stderr);
    }
    if truncated {
        model_text.push_str(&format!("\n[output truncated at {limit} bytes]"));
    }
    Ok(ToolExecutionOutput {
        model_text,
        display: json!({"kind": "shell", "command": command}),
        typed_result: json!({
            "command": command,
            "stdout": stdout,
            "stderr": stderr,
            "returncode": status,
            "truncated": truncated,
        }),
        chunks: Vec::new(),
    })
}

fn process_spec(
    family: ShellFamily,
    config: &ShellConfig,
    working_directory: &Path,
    command: &str,
    environment: Option<&Value>,
    max_output_bytes: usize,
    managed: bool,
) -> ProcessSpec {
    let mut spec = ProcessSpec::new(&config.executable, working_directory);
    spec.arguments = config
        .arguments
        .iter()
        .cloned()
        .chain(std::iter::once(command.to_owned()))
        .collect();
    // Both streams share one budget in the reader, so the spec carries what the
    // two rendered streams may need together.
    spec.max_output_bytes = max_output_bytes.saturating_mul(2);
    // A managed session outlives its call and is fed control keys, which only a
    // terminal turns into signals, so it asks for one; the legacy variant runs
    // one command to completion and needs none.
    spec.terminal = managed;
    // The family's own variables go in first: the reference merges the call's
    // overrides over them, so a call may still ask for a pager it will read.
    for (key, value) in family.environment(managed) {
        spec.environment.insert(key, value);
    }
    if let Some(Value::Object(overrides)) = environment {
        for (key, value) in overrides {
            if let Some(value) = value.as_str() {
                spec.environment.insert(key.clone(), value.to_owned());
            }
        }
    }
    spec
}

fn process_error(error: ProcessError) -> ToolError {
    ToolError::Execution(error.to_string())
}

fn exit_status(state: &TerminalState) -> i32 {
    match state {
        TerminalState::Exited { code, .. } | TerminalState::Interrupted { code } => {
            code.unwrap_or(-1)
        }
        TerminalState::Running => 0,
        TerminalState::Failed { .. } => -1,
    }
}

/// The bytes one stream produced, decoded and bounded by `limit`.
fn render_stream(chunks: &[ProcessChunk], stream: ProcessStream, limit: usize) -> (String, bool) {
    let mut bytes = Vec::new();
    for chunk in chunks.iter().filter(|chunk| chunk.stream == stream) {
        bytes.extend_from_slice(&chunk.bytes);
    }
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    (decode_output(&bytes), truncated)
}

/// Decodes captured console output the way reference `decode_safe` does for a
/// subprocess: a byte-order mark decides the codec, and anything unmarked is
/// read as UTF-8 with replacement.
///
/// PowerShell emits UTF-16 often enough that this is the difference between
/// text and a string of interleaved NULs, so a stream that carries no mark but
/// is plainly UTF-16 is decoded as such too. That is what charset detection
/// buys the reference, narrowed here to the one encoding a Windows shell
/// actually produces.
fn decode_output(bytes: &[u8]) -> String {
    if let Some(body) = bytes.strip_prefix(b"\xef\xbb\xbf") {
        return String::from_utf8_lossy(body).into_owned();
    }
    if let Some(body) = bytes.strip_prefix(b"\xff\xfe") {
        return decode_utf16(body, true);
    }
    if let Some(body) = bytes.strip_prefix(b"\xfe\xff") {
        return decode_utf16(body, false);
    }
    match utf16_endianness(bytes) {
        Some(little_endian) => decode_utf16(bytes, little_endian),
        None => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> String {
    let units = bytes.chunks_exact(2).map(|pair| {
        let (low, high) = (pair.first().copied(), pair.get(1).copied());
        let pair = [low.unwrap_or(0), high.unwrap_or(0)];
        if little_endian {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    });
    char::decode_utf16(units)
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Whether an unmarked stream is UTF-16, and in which order.
///
/// Text encoded in it leaves a NUL in every other byte, a shape UTF-8 output
/// never has. Requiring the other parity to carry no NUL at all keeps binary
/// output, which has them everywhere, from being read as text.
fn utf16_endianness(bytes: &[u8]) -> Option<bool> {
    let sampled = bytes.len().min(512) & !1;
    if sampled < 4 {
        return None;
    }
    let (mut trailing, mut leading) = (0_usize, 0_usize);
    for (index, byte) in bytes.iter().take(sampled).enumerate() {
        if *byte == 0 {
            if index.is_multiple_of(2) {
                leading += 1;
            } else {
                trailing += 1;
            }
        }
    }
    let expected = sampled / 2;
    let threshold = expected - expected / 4;
    if trailing >= threshold && leading == 0 {
        return Some(true);
    }
    (leading >= threshold && trailing == 0).then_some(false)
}

// --------------------------------------------------------------------------
// Managed sessions
// --------------------------------------------------------------------------

/// Reference `Status`, the states a managed session reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionStatus {
    Running,
    Completed,
    Killed,
    TimedOut,
    /// Reference `orphaned`: a session a previous process left behind, whose
    /// manifest and log survive it while the terminal that produced them does
    /// not.
    Orphaned,
}

impl SessionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Killed => "killed",
            Self::TimedOut => "timed_out",
            Self::Orphaned => "orphaned",
        }
    }
}

#[derive(Debug)]
struct SessionState {
    status: SessionStatus,
    exit_code: Option<i32>,
    backpressure_dropped: bool,
    updated_at_ms: u128,
}

struct ManagedSession {
    id: String,
    terminal_id: String,
    command: String,
    working_directory: String,
    shell: String,
    log_path: PathBuf,
    manifest_path: PathBuf,
    created_at_ms: u128,
    /// Reference `SessionInfo.pty_backend`, absent when the host provided no
    /// terminal and the session fell back to pipes.
    pty_backend: Option<&'static str>,
    /// Reference `SessionInfo.reader_error`, which this port also uses to name
    /// the reason a terminal could not be opened.
    reader_error: Option<String>,
    state: StdMutex<SessionState>,
}

impl ManagedSession {
    fn snapshot(&self) -> (SessionStatus, Option<i32>, bool) {
        self.state
            .lock()
            .map_or((SessionStatus::Running, None, false), |state| {
                (state.status, state.exit_code, state.backpressure_dropped)
            })
    }

    fn info(&self) -> Value {
        let (status, exit_code, dropped) = self.snapshot();
        let updated_at_ms = self
            .state
            .lock()
            .map_or(self.created_at_ms, |state| state.updated_at_ms);
        json!({
            "sessionId": self.id,
            "command": self.command,
            "cwd": self.working_directory,
            "shell": self.shell,
            "ptyBackend": self.pty_backend,
            "status": status.as_str(),
            "exitCode": exit_code,
            "outputPath": self.log_path.to_string_lossy(),
            "createdAtMs": self.created_at_ms.to_string(),
            "updatedAtMs": updated_at_ms.to_string(),
            "backpressureDropped": dropped,
            "readerError": self.reader_error,
        })
    }

    fn is_running(&self) -> bool {
        self.snapshot().0 == SessionStatus::Running
    }

    fn settle(&self, status: SessionStatus, exit_code: Option<i32>) {
        if let Ok(mut state) = self.state.lock() {
            state.status = status;
            state.exit_code = exit_code;
            state.updated_at_ms = now_ms();
        }
        self.save_manifest();
    }

    /// Records what a later process needs to report this session as orphaned.
    ///
    /// Reference `_save_manifest` writes it beside the log at every state
    /// change, so a client that dies between two of them still leaves a
    /// manifest describing the session as it last was.
    fn save_manifest(&self) {
        let Ok(rendered) = serde_json::to_vec_pretty(&self.info()) else {
            return;
        };
        let _ = std::fs::write(&self.manifest_path, rendered);
    }
}

/// The wall-clock milliseconds a session records its transitions at.
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or_default()
}

async fn run_managed_command(
    shell: &SessionShell,
    config: &ShellConfig,
    working_directory: &Path,
    arguments: &Value,
    settings: &ShellCommandConfig,
    output: &ToolOutputSink,
) -> Result<ToolExecutionOutput, ToolError> {
    let command = command_argument(arguments)?;
    let requested_directory = string_argument(arguments, "cwd")
        .map_or_else(|| working_directory.to_path_buf(), PathBuf::from);
    let mut config = config.clone();
    if let Some(executable) = string_argument(arguments, "shell") {
        config.executable = PathBuf::from(executable);
        // Reference `build_windows_shell_argv` derives the argument form from
        // the executable it was handed, so an override carries its own flags
        // rather than the ones the family resolved. The POSIX family has no
        // such rule: its arguments stay whatever the session resolved.
        if shell.family != ShellFamily::Bash {
            config.arguments = windows_shell_arguments(&config.executable);
        }
    }
    let session = start_managed_session(
        shell,
        &config,
        &requested_directory,
        &command,
        arguments.get("env"),
        settings.max_output_bytes,
    )
    .await?;
    let background = arguments["background"].as_bool().unwrap_or(false);
    if background {
        return managed_output(
            &session,
            0,
            byte_limit(arguments, output, settings.max_inline_bytes),
        );
    }
    let hard_timeout =
        arguments["hard_timeout"].as_bool().unwrap_or(false) || arguments["timeout"].is_u64();
    let timeout = timeout_argument(arguments, settings);
    let deadline = Instant::now() + Duration::from_secs(timeout);
    while session.is_running() && Instant::now() < deadline {
        tokio::time::sleep(PUMP_INTERVAL).await;
    }
    if session.is_running() {
        if !hard_timeout {
            // A soft timeout leaves the session running: the model polls it
            // with the family's output tool instead of losing the work.
            return managed_output(
                &session,
                0,
                byte_limit(arguments, output, settings.max_inline_bytes),
            );
        }
        kill_managed_session(shell, &session, SessionStatus::TimedOut).await?;
        let rendered = managed_output(
            &session,
            0,
            byte_limit(arguments, output, settings.max_inline_bytes),
        )?;
        return Err(ToolError::Execution(format!(
            "the command timed out after {timeout}s and its process group was terminated: \
             `{command}`\nsession_id: {}\noutput:\n{}",
            session.id, rendered.model_text
        )));
    }
    let rendered = managed_output(
        &session,
        0,
        byte_limit(arguments, output, settings.max_inline_bytes),
    )?;
    let status = rendered.typed_result["exitCode"].as_i64().unwrap_or(0);
    if status != 0 {
        return Err(ToolError::Execution(format!(
            "the command failed with exit status {status}: `{command}`\nsession_id: {}\noutput:\n{}",
            session.id, rendered.model_text
        )));
    }
    Ok(rendered)
}

async fn start_managed_session(
    shell: &SessionShell,
    config: &ShellConfig,
    working_directory: &Path,
    command: &str,
    environment: Option<&Value>,
    max_output_bytes: usize,
) -> Result<Arc<ManagedSession>, ToolError> {
    if !working_directory.is_dir() {
        return Err(ToolError::Execution(format!(
            "`{}` is not a directory",
            working_directory.display()
        )));
    }
    let sessions_directory = shell.sessions_directory();
    std::fs::create_dir_all(&sessions_directory).map_err(|error| {
        ToolError::Execution(format!(
            "the session log directory `{}` cannot be created: {error}",
            sessions_directory.display()
        ))
    })?;
    let id = new_session_id(shell.family);
    let log_path = sessions_directory.join(format!("{id}.log"));
    let manifest_path = sessions_directory.join(format!("{id}.json"));
    std::fs::write(&log_path, b"").map_err(|error| {
        ToolError::Execution(format!(
            "the session log `{}` cannot be created: {error}",
            log_path.display()
        ))
    })?;
    let terminal_id = shell
        .terminals
        .run(process_spec(
            shell.family,
            config,
            working_directory,
            command,
            environment,
            max_output_bytes,
            true,
        ))
        .await
        .map_err(process_error)?;
    let backend = shell
        .terminals
        .backend(&terminal_id)
        .await
        .unwrap_or_default();
    let created_at_ms = now_ms();
    let session = Arc::new(ManagedSession {
        id,
        terminal_id,
        command: command.to_owned(),
        working_directory: working_directory.to_string_lossy().into_owned(),
        shell: config.executable.to_string_lossy().into_owned(),
        log_path,
        manifest_path,
        created_at_ms,
        pty_backend: backend.pty,
        reader_error: backend.degraded,
        state: StdMutex::new(SessionState {
            status: SessionStatus::Running,
            exit_code: None,
            backpressure_dropped: false,
            updated_at_ms: created_at_ms,
        }),
    });
    session.save_manifest();
    // A session id that was orphaned by a previous process and is now live
    // again answers from the live entry rather than from the manifest.
    shell.forget_orphan(&session.id);
    shell
        .managed
        .lock()
        .await
        .insert(session.id.clone(), session.clone());
    spawn_pump(shell.terminals.clone(), session.clone());
    Ok(session)
}

/// Drains a session's terminal into its log until the process exits.
///
/// The terminal queue is bounded, so nothing but a reader draining it keeps a
/// chatty background command from losing output. The log is the cursor's source
/// of truth, which is what lets the output and log-file tools answer for a
/// session long after it exited.
fn spawn_pump(terminals: TerminalManager, session: Arc<ManagedSession>) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        loop {
            let Ok(read) = terminals.read(&session.terminal_id).await else {
                session.settle(SessionStatus::Killed, None);
                return;
            };
            append_chunks(&session, &read.chunks, read.backpressure_dropped);
            if !matches!(read.state, TerminalState::Running) {
                if let Ok(final_read) = terminals.wait(&session.terminal_id).await {
                    append_chunks(
                        &session,
                        &final_read.chunks,
                        final_read.backpressure_dropped,
                    );
                    if session.is_running() {
                        session.settle(
                            SessionStatus::Completed,
                            Some(exit_status(&final_read.state)),
                        );
                    }
                }
                // The output is captured, so the child is reaped now rather
                // than waiting for the session to be killed or closed.
                let _ = terminals.release(&session.terminal_id).await;
                return;
            }
            tokio::time::sleep(PUMP_INTERVAL).await;
        }
    });
}

fn append_chunks(session: &ManagedSession, chunks: &[ProcessChunk], dropped: bool) {
    if dropped && let Ok(mut state) = session.state.lock() {
        state.backpressure_dropped = true;
    }
    if chunks.is_empty() {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .append(true)
        .open(&session.log_path)
    else {
        return;
    };
    let mut ordered = chunks.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|chunk| chunk.cursor);
    for chunk in ordered {
        let _ = file.write_all(&chunk.bytes);
    }
}

/// A session id for `family`.
///
/// Reference `TerminalSessionManager.session_prefix` is per family, and the
/// families share one session directory, so the prefix is what keeps one
/// family's tools from reading, feeding or killing another's session.
fn new_session_id(family: ShellFamily) -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or_default();
    let mut suffix = [0_u8; 4];
    // A collision would let one session read another's log, so the id carries
    // real entropy rather than a counter.
    if getrandom::fill(&mut suffix).is_err() {
        suffix = (stamp as u32).to_le_bytes();
    }
    format!(
        "{}_{stamp}_{}",
        family.name(),
        suffix
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

/// Wraps one managed-family handler, which all share the same shape: a session
/// store, the call arguments, and the turn's output budget.
/// What a session tool reads off its own `tools.<name>` entry.
///
/// The four session tools declare different subsets: `_stdin` declares neither
/// limit, `_output` declares both, and `_sessions` and `_log_file` only the
/// read window. One value carries all of them so the four handlers share a
/// signature; a tool that declares neither reads its base declaration and uses
/// nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SessionLimits {
    max_inline_bytes: usize,
    max_poll_seconds: f64,
}

impl SessionLimits {
    fn resolve(config: &ToolConfigResolver, tool: &str) -> Self {
        let inline: ShellInlineConfig = config.view(tool);
        let polling: ShellOutputConfig = config.view(tool);
        Self {
            max_inline_bytes: inline.max_inline_bytes,
            max_poll_seconds: polling.max_poll_seconds,
        }
    }
}

fn session_handler<F, Fut>(
    shell: Arc<SessionShell>,
    config: ToolConfigResolver,
    tool: String,
    run: F,
) -> Arc<dyn ToolHandler>
where
    F: Fn(Arc<SessionShell>, Value, ToolOutputSink, SessionLimits) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<ToolExecutionOutput, ToolError>> + Send + 'static,
{
    Arc::new(
        move |invocation: &ToolInvocation, output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let limits = SessionLimits::resolve(&config, &tool);
            let future = run(shell.clone(), invocation.arguments.clone(), output, limits);
            Box::pin(future)
        },
    )
}

/// Wraps a session tool in the permission guard, which is what reads its
/// configured permission: the three polling tools produce no requirement of
/// their own, so the configuration is the whole decision.
fn guarded_session(
    tool: String,
    policy: &PermissionStore,
    approval: &Arc<dyn ApprovalAgent>,
    inner: Arc<dyn ToolHandler>,
) -> Arc<dyn ToolHandler> {
    Arc::new(PolicyGuardedTool::new(
        tool,
        policy.clone(),
        approval.clone(),
        Arc::new(|_invocation| Ok(PermissionContext::deferred())),
        inner,
    ))
}

/// The session `session_id` names, live or left behind.
///
/// Reference `_live_session` separates the two: a live session is driven, an
/// orphaned one is only described, and an id that is neither is unknown. The
/// three answers are distinct here for the same reason, so a tool that needs a
/// process says so rather than reporting the id as missing.
enum SessionHandle {
    Live(Arc<ManagedSession>),
    Orphaned(Value),
}

async fn session_handle(
    shell: &SessionShell,
    session_id: &str,
) -> Result<SessionHandle, ToolError> {
    let sessions = shell.managed.lock().await;
    if let Some(session) = sessions.get(session_id) {
        return Ok(SessionHandle::Live(session.clone()));
    }
    drop(sessions);
    if let Some(manifest) = shell.orphan(session_id) {
        return Ok(SessionHandle::Orphaned(manifest));
    }
    let active = shell
        .managed
        .lock()
        .await
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let listed = if active.is_empty() {
        "none".to_owned()
    } else {
        active.join(", ")
    };
    Err(ToolError::Execution(format!(
        "unknown session `{session_id}`; active sessions: {listed}"
    )))
}

async fn managed_session(
    shell: &SessionShell,
    session_id: &str,
) -> Result<Arc<ManagedSession>, ToolError> {
    match session_handle(shell, session_id).await? {
        SessionHandle::Live(session) => Ok(session),
        SessionHandle::Orphaned(_) => Err(ToolError::Execution(format!(
            "session `{session_id}` was left running by a previous process and has no live \
             terminal; read its log instead"
        ))),
    }
}

/// Reads one session's log from `cursor` and reports where the next read starts.
fn managed_output(
    session: &ManagedSession,
    cursor: u64,
    limit: usize,
) -> Result<ToolExecutionOutput, ToolError> {
    let (output, next_cursor, truncated) =
        read_file_window(&session.log_path, cursor, limit, session.is_running())?;
    let (status, exit_code, dropped) = session.snapshot();
    let mut model_text = output.clone();
    if truncated {
        model_text.push_str(&format!("\n[output truncated at {limit} bytes]"));
    }
    if dropped {
        model_text.push_str("\n[output was dropped while the session outran its buffer]");
    }
    Ok(ToolExecutionOutput {
        model_text,
        display: json!({"kind": "shell", "command": session.command}),
        typed_result: json!({
            "sessionId": session.id,
            "command": session.command,
            "status": status.as_str(),
            "exitCode": exit_code,
            "output": output,
            "nextCursor": next_cursor,
            "truncated": truncated,
            "outputPath": session.log_path.to_string_lossy(),
            "backpressureDropped": dropped,
        }),
        chunks: Vec::new(),
    })
}

/// Reads at most `limit` bytes of `path` starting at `cursor`.
///
/// A window bounded by bytes can end mid-character. Reference `_read_file_chunk`
/// drops the dangling lead so the next read picks it up whole, whenever more
/// bytes are coming: either because the file is longer than the window, or
/// because the session is still writing to it. Without that, a poll of a live
/// log emits a replacement character the next poll cannot take back.
fn read_file_window(
    path: &Path,
    cursor: u64,
    limit: usize,
    running: bool,
) -> Result<(String, u64, bool), ToolError> {
    let mut file = std::fs::File::open(path).map_err(|error| {
        ToolError::Execution(format!("`{}` cannot be read: {error}", path.display()))
    })?;
    let size = file
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    if cursor >= size {
        return Ok((String::new(), size, false));
    }
    file.seek(SeekFrom::Start(cursor)).map_err(|error| {
        ToolError::Execution(format!("`{}` cannot be read: {error}", path.display()))
    })?;
    let mut buffer = vec![0_u8; limit];
    let read = file.read(&mut buffer).map_err(|error| {
        ToolError::Execution(format!("`{}` cannot be read: {error}", path.display()))
    })?;
    buffer.truncate(read);
    if running || size > cursor.saturating_add(read as u64) {
        trim_incomplete_utf8_suffix(&mut buffer);
    }
    let next_cursor = cursor.saturating_add(buffer.len() as u64);
    Ok((decode_output(&buffer), next_cursor, size > next_cursor))
}

/// How many bytes the UTF-8 sequence led by `lead` occupies, if it leads one.
fn utf8_sequence_length(lead: u8) -> Option<usize> {
    match lead {
        0x00..=0x7f => Some(1),
        0xc0..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf7 => Some(4),
        _ => None,
    }
}

/// Drops a trailing sequence the window cut short, reference
/// `_trim_incomplete_utf8_suffix`.
fn trim_incomplete_utf8_suffix(buffer: &mut Vec<u8>) {
    for back in 1..=buffer.len().min(4) {
        let byte = buffer[buffer.len() - back];
        if (0x80..0xc0).contains(&byte) {
            continue;
        }
        if utf8_sequence_length(byte).is_some_and(|expected| back < expected) {
            buffer.truncate(buffer.len() - back);
        }
        return;
    }
}

/// Moves `cursor` forward off the continuation bytes of a character that starts
/// before it, reference `_skip_utf8_continuation_prefix`.
///
/// A tail window is positioned by subtracting a byte budget from the file size,
/// so unlike a cursor the model supplies it can land inside a character. Reading
/// from there would decode the tail of one character as a replacement.
fn skip_utf8_continuation_prefix(path: &Path, cursor: u64) -> u64 {
    if cursor == 0 {
        return cursor;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return cursor;
    };
    if file.seek(SeekFrom::Start(cursor)).is_err() {
        return cursor;
    }
    let mut prefix = [0_u8; 3];
    let Ok(read) = file.read(&mut prefix) else {
        return cursor;
    };
    for (index, byte) in prefix[..read].iter().enumerate() {
        if (0x80..0xc0).contains(byte) {
            continue;
        }
        return cursor.saturating_add(index as u64);
    }
    cursor.saturating_add(read as u64)
}

async fn kill_managed_session(
    shell: &SessionShell,
    session: &ManagedSession,
    status: SessionStatus,
) -> Result<(), ToolError> {
    let read = shell.terminals.interrupt(&session.terminal_id).await;
    let exit_code = match read {
        Ok(read) => {
            append_chunks(session, &read.chunks, read.backpressure_dropped);
            Some(exit_status(&read.state))
        }
        // A terminal the pump already released is a session that exited on its
        // own; its status is whatever the pump recorded.
        Err(_) => session.snapshot().1,
    };
    let _ = shell.terminals.release(&session.terminal_id).await;
    session.settle(status, exit_code);
    Ok(())
}

// --------------------------------------------------------------------------
// <family>_output, _stdin, _sessions and _log_file
// --------------------------------------------------------------------------

async fn run_output(
    shell: Arc<SessionShell>,
    arguments: Value,
    output: ToolOutputSink,
    limits: SessionLimits,
) -> Result<ToolExecutionOutput, ToolError> {
    let session_id = string_argument(&arguments, "session_id")
        .unwrap_or_default()
        .to_owned();
    let cursor = arguments["cursor"].as_u64().unwrap_or(0);
    let limit = byte_limit(&arguments, &output, limits.max_inline_bytes);
    // Reference `read_output` answers an orphan from its manifest and its log,
    // which is what makes a build a previous process left running readable
    // rather than lost.
    let session = match session_handle(&shell, &session_id).await? {
        SessionHandle::Live(session) => session,
        SessionHandle::Orphaned(manifest) => return orphan_output(&manifest, cursor, limit),
    };
    let wait = arguments["wait_seconds"]
        .as_f64()
        .unwrap_or(0.0)
        .clamp(0.0, limits.max_poll_seconds);
    let deadline = Instant::now() + Duration::from_secs_f64(wait);
    // Reference `read_output` waits for the session to exit, for a full window
    // to accumulate, or for the deadline. Returning on the first byte instead
    // would answer a poll of an interactive session with the echo of what was
    // just written to it rather than with what the program then said.
    while Instant::now() < deadline
        && session.is_running()
        && log_size(&session.log_path).saturating_sub(cursor) < limit as u64
    {
        tokio::time::sleep(PUMP_INTERVAL).await;
    }
    managed_output(&session, cursor, limit)
}

/// What an orphaned session answers with: its manifest and whatever its log
/// still holds.
fn orphan_output(
    manifest: &Value,
    cursor: u64,
    limit: usize,
) -> Result<ToolExecutionOutput, ToolError> {
    let log_path = PathBuf::from(
        manifest
            .get("outputPath")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let (output, next_cursor, truncated) = read_file_window(&log_path, cursor, limit, false)?;
    let command = manifest
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let mut model_text = output.clone();
    if truncated {
        model_text.push_str(&format!("\n[output truncated at {limit} bytes]"));
    }
    Ok(ToolExecutionOutput {
        model_text,
        display: json!({"kind": "shell", "command": command}),
        typed_result: json!({
            "sessionId": manifest.get("sessionId").cloned().unwrap_or(Value::Null),
            "command": command,
            "status": SessionStatus::Orphaned.as_str(),
            "exitCode": manifest.get("exitCode").cloned().unwrap_or(Value::Null),
            "output": output,
            "nextCursor": next_cursor,
            "truncated": truncated,
            "outputPath": log_path.to_string_lossy(),
            "backpressureDropped": false,
        }),
        chunks: Vec::new(),
    })
}

fn log_size(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or_default()
}

async fn run_stdin(
    shell: Arc<SessionShell>,
    arguments: Value,
    _output: ToolOutputSink,
    _limits: SessionLimits,
) -> Result<ToolExecutionOutput, ToolError> {
    let session_id = string_argument(&arguments, "session_id")
        .unwrap_or_default()
        .to_owned();
    // The payload is decoded before the session is even looked up, so a
    // malformed one never reaches a process.
    let bytes = stdin_bytes(&arguments)?;
    let session = managed_session(&shell, &session_id).await?;
    if !session.is_running() {
        return Err(ToolError::Execution(format!(
            "session `{session_id}` has exited and no longer reads input"
        )));
    }
    shell
        .terminals
        .write(&session.terminal_id, &bytes)
        .await
        .map_err(process_error)?;
    Ok(ToolExecutionOutput {
        model_text: format!("Wrote {} bytes to session {session_id}", bytes.len()),
        display: json!({"kind": "shell", "command": session.command}),
        typed_result: json!({
            "sessionId": session_id,
            "bytesWritten": bytes.len(),
            "status": session.snapshot().0.as_str(),
        }),
        chunks: Vec::new(),
    })
}

/// The bytes one stdin call writes.
///
/// The reference model accepts exactly one of the three inputs and rejects
/// anything else, so there is no precedence to apply.
fn stdin_bytes(arguments: &Value) -> Result<Vec<u8>, ToolError> {
    let text = string_argument(arguments, "text");
    let control = arguments
        .get("control")
        .and_then(Value::as_array)
        .filter(|keys| !keys.is_empty());
    let encoded = string_argument(arguments, "bytes_base64");
    let supplied = usize::from(text.is_some())
        + usize::from(control.is_some())
        + usize::from(encoded.is_some());
    if supplied != 1 {
        return Err(ToolError::SchemaViolation {
            path: "/".to_owned(),
            message: "supply exactly one of text, control or bytes_base64".to_owned(),
        });
    }
    if let Some(text) = text {
        return Ok(text.as_bytes().to_vec());
    }
    if let Some(keys) = control {
        let mut bytes = Vec::new();
        for (index, key) in keys.iter().enumerate() {
            let name = key.as_str().unwrap_or_default();
            let Some((_, sequence)) = CONTROL_KEYS.iter().find(|(known, _)| *known == name) else {
                return Err(ToolError::SchemaViolation {
                    path: format!("/control/{index}"),
                    message: format!("`{name}` is not a control key"),
                });
            };
            bytes.extend_from_slice(sequence);
        }
        return Ok(bytes);
    }
    let encoded = encoded.unwrap_or_default();
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| ToolError::SchemaViolation {
            path: "/bytes_base64".to_owned(),
            message: format!("is not valid base64: {error}"),
        })
}

async fn run_sessions(
    shell: Arc<SessionShell>,
    arguments: Value,
    output: ToolOutputSink,
    limits: SessionLimits,
) -> Result<ToolExecutionOutput, ToolError> {
    let action = string_argument(&arguments, "action").unwrap_or("list");
    let limit = byte_limit(&arguments, &output, limits.max_inline_bytes);
    match action {
        "list" => {
            let sessions = shell.managed.lock().await;
            let mut infos = sessions
                .values()
                .map(|session| session.info())
                .collect::<Vec<_>>();
            // Reference `list_sessions` appends the orphans a live session does
            // not shadow, so a client that restarted still sees what it left.
            infos.extend(shell.orphans().into_iter().filter(|manifest| {
                manifest
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .is_none_or(|id| !sessions.contains_key(id))
            }));
            infos.sort_by(|left, right| {
                created_at(left)
                    .cmp(&created_at(right))
                    .then_with(|| session_id_of(left).cmp(&session_id_of(right)))
            });
            Ok(ToolExecutionOutput {
                model_text: format!("{} {} sessions", infos.len(), shell.family.name()),
                display: json!({
                    "kind": "shell",
                    "command": shell.family.tool_name("sessions list"),
                }),
                typed_result: json!({"action": "list", "sessions": infos}),
                chunks: Vec::new(),
            })
        }
        "inspect" => {
            // Reference `inspect_session` positions the window at the end of
            // the log rather than at its start: an inspection reports what a
            // session is doing now, not how it began.
            let (info, log_path, running) =
                match required_handle(&shell, &arguments, "inspect").await? {
                    SessionHandle::Live(session) => (
                        session.info(),
                        session.log_path.clone(),
                        session.is_running(),
                    ),
                    SessionHandle::Orphaned(manifest) => {
                        let log_path = PathBuf::from(
                            manifest
                                .get("outputPath")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                        );
                        (manifest, log_path, false)
                    }
                };
            let cursor = skip_utf8_continuation_prefix(
                &log_path,
                log_size(&log_path).saturating_sub(limit as u64),
            );
            let (output, next_cursor, truncated) =
                read_file_window(&log_path, cursor, limit, running)?;
            let command = info
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Ok(ToolExecutionOutput {
                model_text: output.clone(),
                display: json!({"kind": "shell", "command": command}),
                typed_result: json!({
                    "action": "inspect",
                    "session": info,
                    "output": output,
                    "nextCursor": next_cursor,
                    "truncated": truncated,
                }),
                chunks: Vec::new(),
            })
        }
        "kill" => {
            // A session is owned by the Vibe session, not by the turn that
            // started it, which is what the reference does: any turn may
            // stop any session of the family.
            let session = required_session(&shell, &arguments, "kill").await?;
            kill_managed_session(&shell, &session, SessionStatus::Killed).await?;
            shell.managed.lock().await.remove(&session.id);
            Ok(ToolExecutionOutput {
                model_text: format!("Killed session {}", session.id),
                display: json!({"kind": "shell", "command": session.command}),
                typed_result: json!({"action": "kill", "session": session.info()}),
                chunks: Vec::new(),
            })
        }
        "reset" => {
            let sessions = shell
                .managed
                .lock()
                .await
                .values()
                .cloned()
                .collect::<Vec<_>>();
            let mut killed = Vec::new();
            for session in &sessions {
                if session.is_running() {
                    kill_managed_session(&shell, session, SessionStatus::Killed).await?;
                }
                killed.push(session.info());
            }
            shell.managed.lock().await.clear();
            if arguments["clear_logs"].as_bool().unwrap_or(false) {
                // Reference `reset` clears the orphans with the logs, because
                // the manifests it would read them back from are what it just
                // deleted.
                for session in &sessions {
                    let _ = std::fs::remove_file(&session.log_path);
                    let _ = std::fs::remove_file(&session.manifest_path);
                }
                for manifest in shell.orphans() {
                    if let Some(id) = manifest.get("sessionId").and_then(Value::as_str) {
                        let directory = shell.sessions_directory();
                        let _ = std::fs::remove_file(directory.join(format!("{id}.log")));
                        let _ = std::fs::remove_file(directory.join(format!("{id}.json")));
                    }
                }
                if let Ok(mut orphaned) = shell.orphaned.lock() {
                    orphaned.clear();
                }
            }
            Ok(ToolExecutionOutput {
                model_text: format!("Stopped {} {} sessions", killed.len(), shell.family.name()),
                display: json!({
                    "kind": "shell",
                    "command": shell.family.tool_name("sessions reset"),
                }),
                typed_result: json!({"action": "reset", "sessions": killed}),
                chunks: Vec::new(),
            })
        }
        other => Err(ToolError::Execution(format!(
            "unknown {} action `{other}`; use `list`, `inspect`, `kill` or `reset`",
            shell.family.tool_name("sessions")
        ))),
    }
}

async fn required_session(
    shell: &SessionShell,
    arguments: &Value,
    action: &str,
) -> Result<Arc<ManagedSession>, ToolError> {
    managed_session(shell, required_session_id(arguments, action)?).await
}

async fn required_handle(
    shell: &SessionShell,
    arguments: &Value,
    action: &str,
) -> Result<SessionHandle, ToolError> {
    session_handle(shell, required_session_id(arguments, action)?).await
}

fn required_session_id<'a>(arguments: &'a Value, action: &str) -> Result<&'a str, ToolError> {
    string_argument(arguments, "session_id").ok_or_else(|| ToolError::SchemaViolation {
        path: "/session_id".to_owned(),
        message: format!("is required by the `{action}` action"),
    })
}

/// When a listed session was created, which is the order the reference lists
/// them in. A manifest that lost the field sorts first rather than failing.
fn created_at(info: &Value) -> u128 {
    info.get("createdAtMs")
        .and_then(Value::as_str)
        .and_then(|stamp| stamp.parse().ok())
        .unwrap_or_default()
}

fn session_id_of(info: &Value) -> String {
    info.get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

async fn run_log_file(
    shell: Arc<SessionShell>,
    arguments: Value,
    output: ToolOutputSink,
    limits: SessionLimits,
) -> Result<ToolExecutionOutput, ToolError> {
    let action = string_argument(&arguments, "action").unwrap_or_default();
    let path = resolve_log_path(&shell, &arguments)?;
    match action {
        "read" => {
            let offset = arguments["offset"].as_u64().unwrap_or(0);
            let limit = byte_limit(&arguments, &output, limits.max_inline_bytes);
            // Reference `read_log_file` trims a cut character only while the
            // session behind the path is still writing to it.
            let running = is_running_log(&shell, &path).await;
            let (content, next_cursor, truncated) =
                read_file_window(&path, offset, limit, running)?;
            Ok(ToolExecutionOutput {
                model_text: content.clone(),
                display: json!({
                    "kind": "shell",
                    "command": shell.family.tool_name("log_file read"),
                }),
                typed_result: json!({
                    "action": "read",
                    "path": path.to_string_lossy(),
                    "content": content,
                    "nextCursor": next_cursor,
                    "truncated": truncated,
                }),
                chunks: Vec::new(),
            })
        }
        "write" | "append" => {
            refuse_live_session_log(&shell, &path).await?;
            let content = string_argument(&arguments, "content").unwrap_or_default();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    ToolError::Execution(format!(
                        "`{}` cannot be created: {error}",
                        parent.display()
                    ))
                })?;
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(action == "append")
                .truncate(action == "write")
                .open(&path)
                .map_err(|error| {
                    ToolError::Execution(format!("`{}` cannot be written: {error}", path.display()))
                })?;
            file.write_all(content.as_bytes()).map_err(|error| {
                ToolError::Execution(format!("`{}` cannot be written: {error}", path.display()))
            })?;
            Ok(ToolExecutionOutput {
                model_text: format!("Wrote {} bytes to {}", content.len(), path.display()),
                display: json!({
                    "kind": "shell",
                    "command": shell.family.tool_name(&format!("log_file {action}")),
                }),
                typed_result: json!({
                    "action": action,
                    "path": path.to_string_lossy(),
                    "bytesWritten": content.len(),
                }),
                chunks: Vec::new(),
            })
        }
        other => Err(ToolError::Execution(format!(
            "unknown {} action `{other}`; use `read`, `write` or `append`",
            shell.family.tool_name("log_file")
        ))),
    }
}

/// The file a `<family>_log_file` call addresses.
///
/// A session id resolves to that session's own log. A relative path is joined
/// to the shell-tool directory and refused before any filesystem access when it
/// climbs out of it or names another family's session file.
fn resolve_log_path(shell: &SessionShell, arguments: &Value) -> Result<PathBuf, ToolError> {
    if let Some(session_id) = string_argument(arguments, "session_id") {
        // A session id names a file inside the session directory, so it is held
        // to the same rule as a relative path: one component, this family's.
        if !is_family_session_id(shell.family, session_id) {
            return Err(ToolError::Execution(format!(
                "the log path must name a {} session file",
                shell.family.name()
            )));
        }
        return Ok(shell.sessions_directory().join(format!("{session_id}.log")));
    }
    let Some(relative) = string_argument(arguments, "relative_path") else {
        return Err(ToolError::SchemaViolation {
            path: "/relative_path".to_owned(),
            message: "is required when session_id is absent".to_owned(),
        });
    };
    let candidate = Path::new(relative);
    if candidate
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ToolError::Execution(
            "the log path escapes the session log directory".to_owned(),
        ));
    }
    let resolved = shell.log_root.join(candidate);
    // A file directly under `sessions/` belongs to a shell family, and this
    // tool answers only for its own.
    if resolved.parent() == Some(shell.sessions_directory().as_path())
        && !resolved
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".log"))
            .is_some_and(|name| is_family_session_id(shell.family, name))
    {
        return Err(ToolError::Execution(format!(
            "the log path must name a {} session file",
            shell.family.name()
        )));
    }
    Ok(resolved)
}

/// Whether `candidate` is one plain name belonging to `family`.
fn is_family_session_id(family: ShellFamily, candidate: &str) -> bool {
    let mut components = Path::new(candidate).components();
    matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
        && candidate.starts_with(&format!("{}_", family.name()))
}

/// Whether `path` is the log of a session still writing to it.
async fn is_running_log(shell: &SessionShell, path: &Path) -> bool {
    shell
        .managed
        .lock()
        .await
        .values()
        .any(|session| session.log_path == path && session.is_running())
}

async fn refuse_live_session_log(shell: &SessionShell, path: &Path) -> Result<(), ToolError> {
    let sessions = shell.managed.lock().await;
    if sessions
        .values()
        .any(|session| session.log_path == path && session.is_running())
    {
        return Err(ToolError::Execution(format!(
            "a live session log cannot be written; use {} or wait for the session to exit",
            shell.family.tool_name("stdin")
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
