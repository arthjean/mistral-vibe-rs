//! What `vibe-acp` does to the processes around it when an editor starts it.
//!
//! An editor may start the agent with no console at all (`DETACHED_PROCESS`).
//! Every console program the agent then starts would get a console of its own,
//! window included, so its children take `CREATE_NO_WINDOW`
//! (`vibe_core::platform::BackgroundCommand`). And whatever environment the
//! editor hands over, the agent reads it without panicking.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "an integration failure must terminate with the output the launch produced"
)]

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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

/// The longest a launch may take to answer, so a hung agent fails the test.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);

/// A port nothing listens on, so every request the agent makes fails at once
/// rather than reaching the network.
const UNREACHABLE: &str = "http://127.0.0.1:9";

fn agent(profile: &Path, project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vibe-acp"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("MISTRAL_API_KEY", "test-key")
        .env(
            "VIBE_API_BASE",
            format!("{UNREACHABLE}/v1/chat/completions"),
        )
        .current_dir(project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if cfg!(windows) {
        for name in WINDOWS_BASE {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command.env("USERPROFILE", profile);
    } else {
        command.env("HOME", profile);
    }
    command
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let root = tempfile::tempdir().expect("fixture root");
    let profile = root.path().join("profile");
    let project = root.path().join("project");
    std::fs::create_dir_all(profile.join(".vibe")).expect("vibe home");
    std::fs::create_dir_all(&project).expect("project directory");
    std::fs::write(
        profile.join(".vibe").join("config.toml"),
        format!(
            "enable_telemetry = false\nenable_update_checks = false\n\
             vibe_base_url = \"{UNREACHABLE}\"\nconsole_base_url = \"{UNREACHABLE}\"\n"
        ),
    )
    .expect("configuration");
    (root, profile, project)
}

/// Every line the agent writes to standard output, read on a thread so a test
/// can wait for one with a deadline.
fn lines(child: &mut Child) -> mpsc::Receiver<String> {
    let stdout = child.stdout.take().expect("piped stdout");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    receiver
}

fn send(child: &mut Child, message: &serde_json::Value) {
    let stdin = child.stdin.as_mut().expect("piped stdin");
    writeln!(stdin, "{message}").expect("the agent reads its standard input");
    stdin.flush().expect("the request reaches the agent");
}

/// The answer to request `id`, or a panic naming what arrived instead.
fn answer(lines: &mpsc::Receiver<String>, id: u64) -> serde_json::Value {
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    let mut seen = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(line) = lines.recv_timeout(left) else {
            break;
        };
        if let Ok(message) = serde_json::from_str::<serde_json::Value>(&line)
            && message["id"] == id
        {
            return message;
        }
        seen.push(line);
    }
    panic!("no answer to request {id}; the agent wrote: {seen:?}");
}

fn initialize() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientCapabilities": {},
            "clientInfo": {"name": "child-process-test", "version": "1.0.0"},
        },
    })
}

/// Closes the agent's input and reaps it, ending it if it outlives the
/// deadline, and returns what it wrote to standard error.
fn finish(mut child: Child) -> String {
    drop(child.stdin.take());
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    while child.try_wait().expect("the agent's status").is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.wait();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut pipe, &mut stderr);
    }
    stderr
}

#[cfg(windows)]
fn not_unicode() -> OsString {
    use std::os::windows::ffi::OsStringExt as _;
    // An unpaired surrogate: valid in a Windows environment, not in UTF-8.
    OsString::from_wide(&[u16::from(b'x'), 0xD800])
}

#[cfg(unix)]
fn not_unicode() -> OsString {
    use std::os::unix::ffi::OsStringExt as _;
    OsString::from_vec(vec![b'x', 0xFF])
}

#[test]
fn an_environment_variable_that_is_not_unicode_leaves_the_agent_serving() {
    let (_root, profile, project) = fixture();
    let mut command = agent(&profile, &project);
    command
        .env("VIBE_TEST_NOT_UNICODE_VALUE", not_unicode())
        .env(not_unicode(), "a name that is not unicode");
    let mut child = command.spawn().expect("vibe-acp launched");
    let lines = lines(&mut child);

    send(&mut child, &initialize());
    let answer = answer(&lines, 0);
    let stderr = finish(child);

    assert!(answer.get("result").is_some(), "{answer}");
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
}

/// Reports, into the file its one argument names, whether the console the
/// script runs in has a window and whether it shows, then the processes
/// attached to that console. The report is renamed into place, so a reader
/// never sees half of it.
#[cfg(windows)]
const CONSOLE_PROBE: &str = r#"param([string]$Out)
Add-Type -Namespace VibeProbe -Name Console -MemberDefinition '[DllImport("kernel32.dll")] public static extern System.IntPtr GetConsoleWindow(); [DllImport("user32.dll")] public static extern bool IsWindowVisible(System.IntPtr window); [DllImport("kernel32.dll")] public static extern uint GetConsoleProcessList(uint[] list, uint count);'
$window = [VibeProbe.Console]::GetConsoleWindow()
$state = if ($window -eq [System.IntPtr]::Zero) { "none" } elseif ([VibeProbe.Console]::IsWindowVisible($window)) { "visible" } else { "hidden" }
$attached = New-Object 'uint32[]' 256
$count = [VibeProbe.Console]::GetConsoleProcessList($attached, 256)
Set-Content -LiteralPath "$Out.partial" -Value "$state;$(($attached | Select-Object -First $count) -join ',')"
Move-Item -LiteralPath "$Out.partial" -Destination $Out
"#;

/// What the console of an MCP server the agent starts looks like.
#[cfg(windows)]
struct ServerConsole {
    /// `none`, `hidden` or `visible`.
    window: String,
    /// The processes attached to it.
    attached: Vec<u32>,
    /// The agent's own process.
    agent: u32,
}

/// Starts the agent with `creation_flags`, has it start an MCP server that
/// runs [`CONSOLE_PROBE`], and reads the probe's report.
#[cfg(windows)]
fn mcp_server_console(creation_flags: u32) -> ServerConsole {
    use std::os::windows::process::CommandExt as _;

    let (root, profile, project) = fixture();
    let probe = root.path().join("probe.ps1");
    let report = root.path().join("console.txt");
    std::fs::write(&probe, CONSOLE_PROBE).expect("probe script");
    // The command is split the way the reference's `shlex.split` splits it,
    // which drops backslashes, so the path is written with forward slashes.
    let powershell = format!(
        "{}/System32/WindowsPowerShell/v1.0/powershell.exe",
        std::env::var("SystemRoot")
            .expect("SystemRoot is set")
            .replace('\\', "/")
    );

    let mut command = agent(&profile, &project);
    command.creation_flags(creation_flags);
    let mut child = command.spawn().expect("vibe-acp launched");
    let agent = child.id();
    let lines = lines(&mut child);
    send(&mut child, &initialize());
    answer(&lines, 0);
    send(
        &mut child,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/new",
            "params": {
                "cwd": project,
                "mcpServers": [{
                    "name": "console-probe",
                    "command": powershell,
                    "args": ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", probe, report],
                    "env": [],
                }],
            },
        }),
    );
    let created = answer(&lines, 1);
    // The servers connect when a turn first lists its tools; the model
    // request that follows fails against the unreachable base.
    send(
        &mut child,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/prompt",
            "params": {
                "sessionId": created["result"]["sessionId"],
                "prompt": [{"type": "text", "text": "hello"}],
            },
        }),
    );
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    while !report.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let stderr = finish(child);
    let printed = std::fs::read_to_string(&report).unwrap_or_else(|error| {
        panic!("the MCP server never reported ({error}); stderr: {stderr}")
    });
    let (window, attached) = printed
        .trim()
        .split_once(';')
        .unwrap_or_else(|| panic!("the probe reported {printed:?}"));
    ServerConsole {
        window: window.to_owned(),
        attached: attached
            .split(',')
            .filter_map(|pid| pid.parse().ok())
            .collect(),
        agent,
    }
}

#[cfg(windows)]
#[test]
fn an_editor_without_a_console_sees_no_window_from_an_mcp_server() {
    /// `DETACHED_PROCESS`: the agent starts with no console, as an editor may
    /// start it.
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let console = mcp_server_console(DETACHED_PROCESS);

    assert_ne!(
        console.window, "visible",
        "the MCP server opened a console window"
    );
    assert!(!console.attached.contains(&console.agent));
}

#[cfg(windows)]
#[test]
fn an_agent_with_a_console_shares_it_with_its_children() {
    // A console of the agent's own that has no window, which is how most
    // editors start a console program.
    let console = mcp_server_console(vibe_core::platform::CREATE_NO_WINDOW);

    assert!(
        console.attached.contains(&console.agent),
        "the MCP server runs in a console of its own: {:?} lacks the agent {}",
        console.attached,
        console.agent
    );
}
