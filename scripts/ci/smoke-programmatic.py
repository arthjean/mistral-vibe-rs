#!/usr/bin/env python3
"""Runs one programmatic turn of an installed `vibe` against a local stand-in
for the chat-completions API.

The release smoke job runs this on every published package after `--version`:
it proves the binary reaches a configured provider, streams a completion and
prints the answer, without a network or a key. The stand-in serves one
streamed answer to every completion request, the home is a temporary directory
whose `config.toml` points the Mistral provider at it, and the run must exit 0
with that answer on standard output.

Usage: smoke-programmatic.py <path to vibe>
"""

from __future__ import annotations

import http.server
import json
import os
import subprocess
import sys
import tempfile
import threading
from pathlib import Path
from typing import Any

ANSWER = "The smoke test reached the stand-in backend."
TIMEOUT_SECONDS = 120


def chunk(model: str, delta: dict[str, Any], finish: str | None = None) -> bytes:
    body: dict[str, Any] = {
        "id": "cmpl-smoke",
        "object": "chat.completion.chunk",
        "created": 1_700_000_000,
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    }
    if finish:
        body["usage"] = {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    return b"data: " + json.dumps(body).encode() + b"\n\n"


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    requests = 0

    def log_message(self, *_: Any) -> None:
        return

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(length) if length else b""
        if not self.path.rstrip("/").endswith("/chat/completions"):
            self.send_error(404)
            return
        Handler.requests += 1
        model = str(json.loads(raw or b"{}").get("model", "model"))
        payload = (
            chunk(model, {"role": "assistant", "content": ""})
            + chunk(model, {"content": ANSWER})
            + chunk(model, {"content": ""}, finish="stop")
            + b"data: [DONE]\n\n"
        )
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    vibe = sys.argv[1]
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="vibe-smoke-") as root:
            vibe_home = Path(root) / "vibe-home"
            workspace = Path(root) / "workspace"
            vibe_home.mkdir()
            workspace.mkdir()
            (vibe_home / "config.toml").write_text(
                'active_model = "mistral-medium-3.5"\n'
                "enable_telemetry = false\n"
                "enable_update_checks = false\n"
                "\n[[providers]]\n"
                'name = "mistral"\n'
                f'api_base = "http://127.0.0.1:{server.server_address[1]}/v1"\n'
                'api_key_env_var = "MISTRAL_API_KEY"\n'
                'backend = "mistral"\n',
                encoding="utf-8",
            )
            env = {
                **os.environ,
                "VIBE_HOME": str(vibe_home),
                "MISTRAL_API_KEY": "smoke-key",
                "NO_COLOR": "1",
                "CI": "true",
            }
            env.pop("FORCE_COLOR", None)
            run = subprocess.run(
                [vibe, "-p", "Say something."],
                cwd=workspace,
                env=env,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                timeout=TIMEOUT_SECONDS,
            )
    finally:
        server.shutdown()
        server.server_close()
    stdout = run.stdout.decode("utf-8", errors="replace")
    stderr = run.stderr.decode("utf-8", errors="replace")
    if run.returncode != 0 or ANSWER not in stdout or Handler.requests == 0:
        print(
            f"vibe -p exited {run.returncode} after {Handler.requests} completion request(s)\n"
            f"stdout:\n{stdout}\nstderr:\n{stderr}",
            file=sys.stderr,
        )
        return 1
    print(f"vibe -p answered through the stand-in backend ({Handler.requests} request(s))")
    return 0


if __name__ == "__main__":
    sys.exit(main())
