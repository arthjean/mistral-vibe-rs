"""The hooks a black-box capture of the app server installs, and what they run.

A scenario's hook is a ``[[hooks]]`` entry (:func:`hook`) whose command runs a
small recorder written beside the scenario (:func:`host_recorder`): it keeps
the invocation it read on stdin and answers with what the scenario scripted for
that call (:func:`answer`, :func:`write_responses`), so a hook can deny,
rewrite, replace, append, fail, time out or answer garbage.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import sys
from typing import Any

import app_server_client


#: The recorder every hook command runs. It is this repository's own code: it
#: appends the invocation to `stdin/<name>.jsonl` and answers with the entry of
#: `responses/<name>.json` at the index of that invocation, the last entry
#: repeating once the list runs out.
RECORDER = r'''
import fcntl, json, os, sys, time
name = sys.argv[1]
root = os.path.dirname(os.path.abspath(__file__))
data = sys.stdin.buffer.read()
os.makedirs(os.path.join(root, "stdin"), exist_ok=True)
with open(os.path.join(root, "stdin", name + ".jsonl"), "ab+") as log:
    fcntl.flock(log, fcntl.LOCK_EX)
    log.seek(0)
    index = log.read().count(b"\n")
    log.write(data + b"\n")
try:
    with open(os.path.join(root, "responses", name + ".json"), encoding="utf-8") as spec:
        responses = json.load(spec)
except FileNotFoundError:
    responses = [{}]
answer = responses[min(index, len(responses) - 1)]
if answer.get("sleep"):
    time.sleep(answer["sleep"])
    # Only a hook that outlived its wait gets here: a killed one never does.
    with open(os.path.join(root, "finished.log"), "a", encoding="utf-8", newline="") as finished:
        finished.write(name + "\n")
sys.stdout.write(answer.get("stdout", ""))
sys.stderr.write(answer.get("stderr", ""))
sys.stdout.flush()
sys.stderr.flush()
sys.exit(answer.get("exit", 0))
'''


def host_recorder() -> str:
    """The recorder as this host runs it.

    Windows has no ``fcntl`` and writes text streams through the console code
    page with CRLF line endings, so there the recorder locks with ``msvcrt``
    and answers in UTF-8 bytes. Elsewhere it is :data:`RECORDER`.
    """

    if os.name != "nt":
        return RECORDER
    return (
        RECORDER.replace(
            "import fcntl, json, os, sys, time",
            "import json, msvcrt, os, sys, time",
        )
        .replace(
            "    fcntl.flock(log, fcntl.LOCK_EX)\n",
            "    log.seek(0)\n    msvcrt.locking(log.fileno(), msvcrt.LK_LOCK, 1)\n",
        )
        .replace(
            'sys.stdout.write(answer.get("stdout", ""))\nsys.stderr.write(answer.get("stderr", ""))',
            'sys.stdout.buffer.write(answer.get("stdout", "").encode("utf-8"))\n'
            'sys.stderr.buffer.write(answer.get("stderr", "").encode("utf-8"))',
        )
    )


def host_commands(text: str) -> str:
    """Hook configuration whose ``python3`` is this interpreter on Windows.

    A Windows ``python3`` is often the Store alias, which prints to the hook's
    output or installs an interpreter instead of running the recorder.
    """

    if os.name != "nt" or " " in sys.executable:
        return text
    return text.replace("'python3 ", f"'{sys.executable} ")


def write_hooks(session: app_server_client.Session, hooks: dict[str, str]) -> None:
    world = session.world
    targets = {
        "project": world.workspace / ".vibe/hooks.toml",
        "user": world.vibe_home / "hooks.toml",
    }
    for where, text in hooks.items():
        target = targets[where]
        if text is None:
            if target.exists():
                target.unlink()
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(host_commands(session.substitute(text)), encoding="utf-8", newline="")


def write_responses(root: Path, responses: dict[str, list[dict[str, Any]]]) -> None:
    directory = root / "responses"
    directory.mkdir(parents=True, exist_ok=True)
    for name, answers in responses.items():
        (directory / f"{name}.json").write_text(json.dumps(answers), encoding="utf-8", newline="")


def hook(name: str, kind: str, recorder: str | None = None, **extra: Any) -> str:
    """One `[[hooks]]` entry running the recorder under `recorder` (the hook's
    own name by default)."""

    lines = [
        "[[hooks]]",
        f"name = {json.dumps(name)}",
        f"type = {json.dumps(kind)}",
        f"command = 'python3 \"$ROOT/hook.py\" {recorder or name}'",
    ]
    for key, value in extra.items():
        lines.append(f"{key} = {json.dumps(value)}")
    return "\n".join(lines) + "\n\n"


def answer(**fields: Any) -> dict[str, Any]:
    return {"stdout": json.dumps(fields)}
