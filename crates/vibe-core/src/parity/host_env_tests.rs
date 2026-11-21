//! What `scripts/parity/host_env.py` keeps out of a driver's child on
//! Windows, checked from any host: the module decides from `os.name`, which a
//! check can set to `"nt"`, and reaches the Credential Manager only through a
//! vault object a check can stand in for.
//!
//! The Windows job runs the same checks against the real `os.name`; only it
//! can show that the vault calls themselves (`WindowsVault`) reach the
//! Credential Manager.

use std::process::Command;

use super::replay::repository;

/// Runs `body` under `python3` with `scripts/parity` importable, and fails
/// with what the interpreter printed unless it exits cleanly.
fn python(body: &str) {
    let scripts = repository().join("scripts/parity");
    let script = format!("import sys\nsys.path.insert(0, sys.argv[1])\n{body}\nprint('checked')\n");
    let output = Command::new("python3")
        .arg("-I")
        .arg("-B")
        .arg("-c")
        .arg(&script)
        .arg(&scripts)
        .current_dir(repository())
        .output()
        .expect("python3 runs");
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("checked"),
        "the host_env check failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// On Windows the child environment names the `keyring` library's fail
/// backend, which the reference loads before any other and this port honors
/// (`vibe_core::auth::StorelessBackend`), so neither reads nor writes the
/// Credential Manager; on every other platform the environment is the one it
/// was, so a Linux capture is unchanged.
#[test]
fn a_windows_child_environment_names_the_backend_that_stores_nothing() {
    python(
        r#"
from unittest import mock
import host_env

fail = "keyring.backends.fail.Keyring"
assert host_env.NO_KEYRING_BACKEND == fail
assert host_env.keyring_isolation("nt") == {"PYTHON_KEYRING_BACKEND": fail}
assert host_env.keyring_isolation("posix") == {}

with mock.patch.object(host_env.os, "name", "nt"):
    windows = host_env.isolated("/fixture/home", "/fixture/vibe-home", PYTHON_KEYRING_BACKEND="x")
assert windows["PYTHON_KEYRING_BACKEND"] == fail, windows
assert windows["USERPROFILE"] == "/fixture/home", windows
assert windows["DBUS_SESSION_BUS_ADDRESS"] == "unix:path=/nonexistent", windows

with mock.patch.object(host_env.os, "name", "posix"):
    linux = host_env.isolated("/fixture/home", "/fixture/vibe-home")
assert "PYTHON_KEYRING_BACKEND" not in linux, linux
assert "USERPROFILE" not in linux, linux
"#,
    );
}

/// The sweep removes exactly what a capture writes for its account, in the
/// reference's layout and in both of the port's, and leaves another holder's
/// entries in place; off Windows, without a vault, it touches nothing.
#[test]
fn forgetting_a_credential_removes_only_what_a_capture_wrote() {
    python(
        r#"
import host_env

class Vault:
    def __init__(self, entries):
        self.entries = dict(entries)
        self.deleted = []

    def holder(self, target):
        return self.entries.get(target)

    def delete(self, target):
        self.deleted.append(target)
        self.entries.pop(target, None)

left = Vault({
    "ai.mistral.vibe": "OTHER_ORACLE_KEY",
    "OTHER_ORACLE_KEY@ai.mistral.vibe": "OTHER_ORACLE_KEY",
    "OTHER_ORACLE_KEY.ai.mistral.vibe": "OTHER_ORACLE_KEY",
    "MISTRAL_API_KEY@ai.mistral.vibe": "MISTRAL_API_KEY",
})
host_env.forget_credential("ai.mistral.vibe", "OTHER_ORACLE_KEY", left)
assert left.entries == {"MISTRAL_API_KEY@ai.mistral.vibe": "MISTRAL_API_KEY"}, left.entries

held = Vault({"ai.mistral.vibe": "MISTRAL_API_KEY"})
host_env.forget_credential("ai.mistral.vibe", "OTHER_ORACLE_KEY", held)
assert held.entries == {"ai.mistral.vibe": "MISTRAL_API_KEY"}, held.entries
assert "ai.mistral.vibe" not in held.deleted, held.deleted

if host_env.os.name != "nt":
    host_env.forget_credential("ai.mistral.vibe", "OTHER_ORACLE_KEY")
"#,
    );
}

/// The protocol driver sweeps the vault before its first capture, so a run
/// killed inside `flows/setup` is cleaned by the next run, and again when
/// that scenario ends.
#[test]
fn the_protocol_driver_sweeps_the_vault_before_it_captures() {
    python(
        r#"
import tempfile
from pathlib import Path
import app_server_protocol as driver

calls = []
driver.host_env.forget_credential = lambda service, account, vault=None: calls.append(("sweep", service, account))

def run_scenario(scenario, command, quiet):
    calls.append(("capture", scenario["name"]))
    raise driver.OracleError("stopped after the first capture")

driver.run_scenario = run_scenario
with tempfile.TemporaryDirectory() as scratch:
    driver.sys.argv = [
        "app_server_protocol.py",
        "--server", sys.executable,
        "--output", str(Path(scratch) / "port.json"),
        "--only", "flows/setup",
        "--jobs", "1",
    ]
    try:
        driver.main()
    except Exception:
        pass
assert calls[0] == ("sweep", "ai.mistral.vibe", "OTHER_ORACLE_KEY"), calls
assert ("capture", "flows/setup") in calls[1:], calls
"#,
    );
}
