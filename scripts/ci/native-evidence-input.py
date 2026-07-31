from __future__ import annotations

import argparse
import json
from pathlib import Path


SUITES = {
    "linux-x86_64": [
        "acp",
        "cli",
        "filesystem",
        "keyring_failure",
        "managed_terminal",
        "package_uninstall",
        "persistence",
        "posix_shell",
        "proxy_tls",
        "signals",
        "tui",
    ],
    "linux-aarch64": [
        "acp",
        "cli",
        "filesystem",
        "keyring_failure",
        "managed_terminal",
        "package_uninstall",
        "persistence",
        "posix_shell",
        "proxy_tls",
        "signals",
        "tui",
    ],
    "macos-x86_64": [
        "acp",
        "cli",
        "filesystem",
        "gatekeeper",
        "homebrew_paths",
        "keychain",
        "login_shells",
        "notifications",
        "persistence",
        "proxy_tls",
        "pty",
        "shell",
        "terminal_encodings",
        "tui",
    ],
    "macos-aarch64": [
        "acp",
        "cli",
        "filesystem",
        "gatekeeper",
        "homebrew_paths",
        "keychain",
        "login_shells",
        "notifications",
        "persistence",
        "proxy_tls",
        "pty",
        "shell",
        "terminal_encodings",
        "tui",
    ],
    "windows-x86_64": [
        "acp",
        "cli",
        "cmd",
        "console_resize",
        "conpty",
        "credential_store",
        "filesystem",
        "git_bash",
        "locked_files",
        "path_matrix",
        "persistence",
        "powershell",
        "process_tree",
        "proxy_tls",
        "signals",
        "tui",
    ],
}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", choices=sorted(SUITES), required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--artifact", type=Path, required=True)
    parser.add_argument("--minimum-runtime", required=True)
    parser.add_argument("--cleanup", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--notarized", action="store_true")
    arguments = parser.parse_args()
    cleanup = json.loads(arguments.cleanup.read_text())
    payload = {
        "target": arguments.target,
        "sourceRevision": arguments.source_revision,
        "artifact": arguments.artifact.as_posix(),
        "minimumRuntime": arguments.minimum_runtime,
        "suites": {suite: True for suite in SUITES[arguments.target]},
        "cleanup": cleanup,
        "signing": {
            "checksumVerified": True,
            "signed": True,
            "notarized": arguments.notarized,
            "cleanHostDownload": True,
        },
    }
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
