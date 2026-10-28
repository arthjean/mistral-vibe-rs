//! Child processes of a world: started with a cleared environment and the null
//! device as stdin, leading their own process group on Unix, captured by the
//! harness, bounded by deadlines, and killed with their group on expiry or
//! teardown.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{ChildStdin, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::world::World;
use crate::{DEFAULT_DEADLINE, Failure, Outcome, scaled, tail};

/// How much output a deadline failure quotes from each stream.
const QUOTED: usize = 4096;

/// How long a terminated group gets before it is killed.
#[cfg(unix)]
const GRACE: Duration = Duration::from_secs(2);

type Buffer = Arc<Mutex<Vec<u8>>>;

fn snapshot(buffer: &Buffer) -> Vec<u8> {
    buffer
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// What the world keeps about each child for failure artifacts.
#[derive(Clone)]
pub(crate) struct ChildRecord {
    pub label: String,
    pub pid: u32,
    pub env: Vec<(String, String)>,
    pub stdout: Buffer,
    pub stderr: Buffer,
}

/// A command a world will start.
pub struct Command {
    world: World,
    program: PathBuf,
    label: String,
    args: Vec<OsString>,
    env: Vec<(String, String)>,
    removed: Vec<String>,
    cwd: PathBuf,
    stdin: bool,
    deadline: Duration,
}

impl Command {
    pub(crate) fn new(world: World, program: PathBuf, label: String) -> Self {
        let cwd = world.work();
        Self {
            world,
            program,
            label,
            args: Vec::new(),
            env: Vec::new(),
            removed: Vec::new(),
            cwd,
            stdin: false,
            deadline: DEFAULT_DEADLINE,
        }
    }

    #[must_use]
    pub fn arg(mut self, argument: impl Into<OsString>) -> Self {
        self.args.push(argument.into());
        self
    }

    #[must_use]
    pub fn args<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(arguments.into_iter().map(Into::into));
        self
    }

    /// Adds or replaces one variable on top of the world's environment.
    #[must_use]
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }

    /// Removes one variable of the world's environment.
    #[must_use]
    pub fn env_remove(mut self, name: impl Into<String>) -> Self {
        self.removed.push(name.into());
        self
    }

    /// The working directory; the world's work tree by default.
    #[must_use]
    pub fn current_dir(mut self, directory: impl Into<PathBuf>) -> Self {
        self.cwd = directory.into();
        self
    }

    /// Pipes stdin instead of the null device.
    #[must_use]
    pub fn stdin_piped(mut self) -> Self {
        self.stdin = true;
        self
    }

    /// How long [`Child::wait`] waits, before [`scaled`] applies (30 s by
    /// default).
    #[must_use]
    pub fn deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    pub fn spawn(self) -> Outcome<Child> {
        let mut env = self.world.env();
        env.retain(|(name, _)| !self.removed.contains(name));
        for (name, value) in self.env {
            env.retain(|(existing, _)| *existing != name);
            env.push((name, value));
        }
        let mut command = std::process::Command::new(&self.program);
        command
            .args(&self.args)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .current_dir(&self.cwd)
            .stdin(if self.stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut process = command.spawn().map_err(|error| {
            Failure(format!(
                "cannot start {} ({}): {error}",
                self.label,
                self.program.display()
            ))
        })?;
        let stdout: Buffer = Arc::default();
        let stderr: Buffer = Arc::default();
        let mut readers = Vec::new();
        if let Some(pipe) = process.stdout.take() {
            readers.push(capture(pipe, Arc::clone(&stdout)));
        }
        if let Some(pipe) = process.stderr.take() {
            readers.push(capture(pipe, Arc::clone(&stderr)));
        }
        self.world.register(ChildRecord {
            label: self.label.clone(),
            pid: process.id(),
            env,
            stdout: Arc::clone(&stdout),
            stderr: Arc::clone(&stderr),
        });
        Ok(Child {
            label: self.label,
            stdin: process.stdin.take(),
            process,
            stdout,
            stderr,
            readers,
            deadline: self.deadline,
            cursor: 0,
            status: None,
        })
    }

    /// Starts the command and waits for it within its deadline.
    pub fn run(self) -> Outcome<Output> {
        self.spawn()?.wait()
    }
}

fn capture(mut pipe: impl Read + Send + 'static, buffer: Buffer) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut chunk = [0_u8; 8192];
        while let Ok(read) = pipe.read(&mut chunk) {
            if read == 0 {
                break;
            }
            buffer
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(&chunk[..read]);
        }
    })
}

/// What a finished child left.
#[derive(Debug, Clone)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn code(&self) -> Option<i32> {
        self.status.code()
    }

    pub fn success(&self) -> bool {
        self.status.success()
    }
}

/// A running child of a world.
pub struct Child {
    label: String,
    process: std::process::Child,
    stdin: Option<ChildStdin>,
    stdout: Buffer,
    stderr: Buffer,
    readers: Vec<JoinHandle<()>>,
    deadline: Duration,
    cursor: usize,
    status: Option<ExitStatus>,
}

impl Child {
    pub fn id(&self) -> u32 {
        self.process.id()
    }

    pub fn write(&mut self, bytes: impl AsRef<[u8]>) -> Outcome {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| Failure(format!("{} has no piped stdin", self.label)))?;
        stdin.write_all(bytes.as_ref())?;
        stdin.flush()?;
        Ok(())
    }

    pub fn write_line(&mut self, line: &str) -> Outcome {
        self.write(format!("{line}\n"))
    }

    /// Closes stdin, as an editor that hangs up does.
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&snapshot(&self.stdout)).into_owned()
    }

    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&snapshot(&self.stderr)).into_owned()
    }

    fn quoted(&self) -> String {
        format!(
            "last {QUOTED} bytes of stdout:\n{}\nlast {QUOTED} bytes of stderr:\n{}",
            tail(&snapshot(&self.stdout), QUOTED),
            tail(&snapshot(&self.stderr), QUOTED)
        )
    }

    /// The next complete stdout line, without its newline.
    pub fn next_line(&mut self, deadline: Duration) -> Outcome<String> {
        let until = Instant::now() + scaled(deadline);
        loop {
            let bytes = snapshot(&self.stdout);
            if let Some(end) = bytes[self.cursor..].iter().position(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(&bytes[self.cursor..self.cursor + end])
                    .trim_end_matches('\r')
                    .to_owned();
                self.cursor += end + 1;
                return Ok(line);
            }
            if self.exited() {
                return Err(Failure(format!(
                    "{} exited before writing another line; {}",
                    self.label,
                    self.quoted()
                )));
            }
            if Instant::now() >= until {
                return Err(self.expire(scaled(deadline), "writing another line"));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits until stdout satisfies `predicate`, and returns it.
    pub fn wait_for_stdout(
        &mut self,
        predicate: impl Fn(&str) -> bool,
        deadline: Duration,
    ) -> Outcome<String> {
        let until = Instant::now() + scaled(deadline);
        loop {
            let stdout = self.stdout();
            if predicate(&stdout) {
                return Ok(stdout);
            }
            if self.exited() {
                return Err(Failure(format!(
                    "{} exited before its stdout matched; {}",
                    self.label,
                    self.quoted()
                )));
            }
            if Instant::now() >= until {
                return Err(self.expire(scaled(deadline), "matching its stdout"));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn exited(&mut self) -> bool {
        if self.status.is_none() {
            self.status = self.process.try_wait().ok().flatten();
        }
        self.status.is_some()
    }

    /// Kills the group and describes the wait that expired.
    fn expire(&mut self, deadline: Duration, what: &str) -> Failure {
        terminate(&mut self.process);
        self.status = self.process.try_wait().ok().flatten();
        Failure(format!(
            "{} did not finish {what} within {deadline:?}, so its process group was killed; {}",
            self.label,
            self.quoted()
        ))
    }

    /// Waits for the child within its command's deadline.
    pub fn wait(self) -> Outcome<Output> {
        let deadline = self.deadline;
        self.wait_within(deadline)
    }

    /// Waits for the child within `deadline`, scaled; on expiry the group is
    /// killed and the failure quotes the end of both streams.
    pub fn wait_within(mut self, deadline: Duration) -> Outcome<Output> {
        let until = Instant::now() + scaled(deadline);
        while !self.exited() {
            if Instant::now() >= until {
                return Err(self.expire(scaled(deadline), "exiting"));
            }
            thread::sleep(Duration::from_millis(10));
        }
        self.stdin = None;
        // A grandchild that inherited a pipe keeps it open past the exit, so
        // the readers get a short while to drain and are then left running.
        let drained = Instant::now() + Duration::from_millis(500);
        while self.readers.iter().any(|r| !r.is_finished()) && Instant::now() < drained {
            thread::sleep(Duration::from_millis(5));
        }
        let status = self
            .status
            .ok_or_else(|| Failure(format!("{} has no exit status", self.label)))?;
        Ok(Output {
            status,
            stdout: self.stdout(),
            stderr: self.stderr(),
        })
    }

    /// Terminates the child's process group now.
    pub fn kill(&mut self) {
        terminate(&mut self.process);
        self.status = self.process.try_wait().ok().flatten();
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if !self.exited() {
            terminate(&mut self.process);
        }
    }
}

/// Ends a child and its group: SIGTERM, then SIGKILL after [`GRACE`] on Unix;
/// `taskkill /T /F` on Windows.
fn terminate(process: &mut std::process::Child) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        let Ok(group) = i32::try_from(process.id()) else {
            let _ = process.kill();
            let _ = process.wait();
            return;
        };
        let group = Pid::from_raw(group);
        let _ = killpg(group, Signal::SIGTERM);
        let until = Instant::now() + GRACE;
        while Instant::now() < until && matches!(process.try_wait(), Ok(None)) {
            thread::sleep(Duration::from_millis(10));
        }
        let _ = killpg(group, Signal::SIGKILL);
        let _ = process.wait();
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &process.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = process.kill();
        let _ = process.wait();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = process.kill();
        let _ = process.wait();
    }
}

/// The processes whose environment carries `WORLD_VARIABLE=<world>`, with
/// their command lines.
#[cfg(target_os = "linux")]
pub(crate) fn survivors(world: &str) -> Vec<(u32, String)> {
    let needle = format!("{}={world}", crate::world::WORLD_VARIABLE).into_bytes();
    let own = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| pid != own)
        .filter(|pid| {
            std::fs::read(format!("/proc/{pid}/environ"))
                .is_ok_and(|environ| environ.split(|&b| b == 0).any(|var| var == needle))
        })
        .map(|pid| {
            let command = std::fs::read(format!("/proc/{pid}/cmdline"))
                .map(|bytes| {
                    String::from_utf8_lossy(&bytes)
                        .split('\0')
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            (pid, command)
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn survivors(_world: &str) -> Vec<(u32, String)> {
    Vec::new()
}

/// Kills every process still carrying the world's marker after a short
/// settling time, and names each one.
pub(crate) fn reap_survivors(world: &str) -> Vec<String> {
    let until = Instant::now() + Duration::from_secs(1);
    let mut found = survivors(world);
    while !found.is_empty() && Instant::now() < until {
        thread::sleep(Duration::from_millis(50));
        found = survivors(world);
    }
    found
        .into_iter()
        .map(|(pid, command)| {
            #[cfg(unix)]
            if let Ok(raw) = i32::try_from(pid) {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(raw),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            format!("process {pid} ({command}) carrying this world's marker survived teardown")
        })
        .collect()
}

#[cfg(test)]
mod process_tests;
