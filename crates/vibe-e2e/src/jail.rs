//! The network jail every e2e process runs in on Linux.
//!
//! [`enter`] re-executes the test binary, with its arguments unchanged, under
//! the first wrapper that works: `bwrap --unshare-net`, then a nested
//! `unshare` (an outer user namespace maps root to bring loopback up, an inner
//! one maps the invoking uid back). Inside, only loopback exists, so a trial
//! cannot reach a real host whatever a binary does. The workspace forbids
//! `unsafe`, which rules out entering namespaces in-process.

#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

/// Set by the harness on the re-executed binary.
pub const JAILED_VARIABLE: &str = "VIBE_E2E_JAILED";

/// Runs the trials without a jail, with the sentinel only.
pub const UNJAILED_VARIABLE: &str = "VIBE_E2E_UNJAILED";

/// Where the binary runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Jail {
    /// Inside a network namespace that holds only loopback.
    Jailed,
    /// Without a namespace, by opt-out or because the platform has none.
    Unjailed,
}

/// The bubblewrap prefix: a network namespace of its own, and an empty
/// `/dev/snd`, so no trial reaches a sound card whatever its ALSA
/// configuration names ([`crate::voice`]).
#[cfg(target_os = "linux")]
fn bwrap() -> Vec<String> {
    let mut prefix = vec!["bwrap", "--unshare-net", "--dev-bind", "/", "/"];
    if std::path::Path::new(SOUND_DEVICES).is_dir() {
        prefix.extend(["--tmpfs", SOUND_DEVICES]);
    }
    prefix.extend(["--die-with-parent", "--"]);
    prefix.into_iter().map(str::to_owned).collect()
}

/// Where Linux exposes sound cards.
pub const SOUND_DEVICES: &str = "/dev/snd";

/// The sound devices a trial could open: none inside the bubblewrap jail.
#[must_use]
pub fn sound_devices() -> Vec<String> {
    std::fs::read_dir(SOUND_DEVICES)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The wrapper prefixes, in order of preference.
#[cfg(target_os = "linux")]
fn wrappers() -> Vec<(&'static str, Vec<String>)> {
    let (uid, gid) = ids();
    vec![
        ("bwrap", bwrap()),
        (
            "unshare",
            vec![
                "unshare".to_owned(),
                "-rn".to_owned(),
                "sh".to_owned(),
                "-c".to_owned(),
                format!(
                    "ip link set lo up && exec unshare --user --map-user={uid} --map-group={gid} \"$@\""
                ),
                "sh".to_owned(),
            ],
        ),
    ]
}

#[cfg(target_os = "linux")]
fn ids() -> (u32, u32) {
    (
        nix::unistd::getuid().as_raw(),
        nix::unistd::getgid().as_raw(),
    )
}

/// Whether `prefix` starts a jailed `true` within one second, or the error it
/// printed.
#[cfg(target_os = "linux")]
fn probe(prefix: &[String]) -> Result<(), String> {
    let mut child = Command::new(&prefix[0])
        .args(&prefix[1..])
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{} cannot start: {error}", prefix[0]))?;
    let until = Instant::now() + Duration::from_secs(1);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = std::io::Read::read_to_string(&mut pipe, &mut stderr);
                }
                return Err(format!("{status}: {}", stderr.trim()));
            }
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("did not finish within 1 s".to_owned());
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// The network interfaces this process sees besides loopback.
#[cfg(target_os = "linux")]
pub fn foreign_interfaces() -> Vec<String> {
    std::fs::read_to_string("/proc/net/dev")
        .unwrap_or_default()
        .lines()
        .skip(2)
        .filter_map(|line| line.split(':').next())
        .map(|name| name.trim().to_owned())
        .filter(|name| name != "lo")
        .collect()
}

/// Puts the binary in the jail, re-executing it when it is not there yet, or
/// says why it cannot run. Never returns on a successful re-execution.
#[cfg(target_os = "linux")]
pub fn enter() -> Result<Jail, String> {
    use std::os::unix::process::CommandExt;

    if std::env::var_os(JAILED_VARIABLE).is_some_and(|v| v == "1") {
        let foreign = foreign_interfaces();
        return if foreign.is_empty() {
            Ok(Jail::Jailed)
        } else {
            Err(format!(
                "{JAILED_VARIABLE}=1 is set but the network namespace holds {foreign:?} besides \
                 loopback"
            ))
        };
    }
    if std::env::var_os(UNJAILED_VARIABLE).is_some_and(|v| v == "1") {
        eprintln!(
            "e2e: {UNJAILED_VARIABLE}=1: running without a network jail, with the egress sentinel \
             only"
        );
        return Ok(Jail::Unjailed);
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("cannot locate the test binary: {error}"))?;
    let mut errors = Vec::new();
    for (name, prefix) in wrappers() {
        match probe(&prefix) {
            Ok(()) => {
                let error = Command::new(&prefix[0])
                    .args(&prefix[1..])
                    .arg(&executable)
                    .args(std::env::args_os().skip(1))
                    .env(JAILED_VARIABLE, "1")
                    .exec();
                errors.push(format!("{name}: {error}"));
            }
            Err(error) => errors.push(format!("{name}: {error}")),
        }
    }
    Err(format!(
        "jail unavailable: install bubblewrap or set {UNJAILED_VARIABLE}=1\n  {}",
        errors.join("\n  ")
    ))
}

/// Other platforms have no network namespace: the sentinel alone guards egress.
#[cfg(not(target_os = "linux"))]
pub fn enter() -> Result<Jail, String> {
    eprintln!(
        "e2e: no network namespace on {}; trials run with the egress sentinel only",
        std::env::consts::OS
    );
    Ok(Jail::Unjailed)
}
