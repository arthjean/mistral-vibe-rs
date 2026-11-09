#!/usr/bin/env python3
"""Relays a JSON-RPC app-server over stdio and records the method sequence.

The reference's terminal client starts its app-server from the command line
``VIBE_APP_SERVER_CMD`` names (``vibe/cli-rust/src/startup/launch.rs``). Pointed
at this relay, the client talks to the real server through it, and the relay
appends one JSON line per message to ``--log``: who sent it (``client`` or
``server``), its kind (``request``, ``notification``, ``response``), its method
when it has one and its id. No parameter, result or error text is kept, so a
log holds names and numbers only. This is the fallback oracle of
``tasks/prd-trustworthy-parity-v3.md`` US-019: the protocol the client issues,
measured without reading its screens.

Usage::

    python3 scripts/parity/app_server_tap.py --log <file> -- <server> [<argument>...]

A tool, not a driver: it captures no corpus and imports no driver.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import sys
import threading
from typing import IO, Any


def describe(line: bytes, sender: str) -> dict[str, Any]:
    """What the log keeps of one message line."""

    try:
        message = json.loads(line)
    except ValueError:
        return {"from": sender, "kind": "unparsed"}
    if not isinstance(message, dict):
        return {"from": sender, "kind": "unparsed"}
    if "method" in message:
        kind = "request" if "id" in message else "notification"
    else:
        kind = "response" if "result" in message else "error"
    entry: dict[str, Any] = {"from": sender, "kind": kind}
    if isinstance(message.get("method"), str):
        entry["method"] = message["method"]
    if isinstance(message.get("id"), (int, str)):
        entry["id"] = message["id"]
    return entry


def pump(source: IO[bytes], sink: IO[bytes], sender: str, log: IO[str], lock: threading.Lock) -> None:
    for line in iter(source.readline, b""):
        sink.write(line)
        sink.flush()
        if line.strip():
            with lock:
                log.write(json.dumps(describe(line, sender)) + "\n")
                log.flush()
    try:
        sink.close()
    except OSError:
        pass


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--log", type=Path, required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    arguments = parser.parse_args()
    command = arguments.command[1:] if arguments.command[:1] == ["--"] else arguments.command
    if not command:
        parser.error("name the server to relay after `--`")
    server = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    assert server.stdin is not None and server.stdout is not None
    lock = threading.Lock()
    with arguments.log.open("a", encoding="utf-8") as log:
        inbound = threading.Thread(
            target=pump, args=(sys.stdin.buffer, server.stdin, "client", log, lock), daemon=True
        )
        inbound.start()
        pump(server.stdout, sys.stdout.buffer, "server", log, lock)
        return server.wait()


if __name__ == "__main__":
    sys.exit(main())
