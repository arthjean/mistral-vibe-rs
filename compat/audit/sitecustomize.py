from __future__ import annotations

import atexit
import json
import os
from pathlib import Path
import sys


_violations: set[str] = set()


def _write_log() -> None:
    raw = os.environ.get("VIBE_AUDIT_LOG")
    if raw:
        Path(raw).write_text(
            json.dumps(sorted(_violations), separators=(",", ":")),
            encoding="utf-8",
        )


def _allowed(path: object, roots: tuple[Path, ...]) -> bool:
    if isinstance(path, int):
        return True
    try:
        resolved = Path(os.fsdecode(path)).resolve()
    except (OSError, TypeError, ValueError):
        return False
    return any(resolved == root or root in resolved.parents for root in roots)


def install() -> None:
    roots = tuple(
        Path(raw).resolve()
        for raw in os.environ.get("VIBE_AUDIT_ROOTS", "").split(os.pathsep)
        if raw
    )
    if not roots:
        return

    def audit(event: str, args: tuple[object, ...]) -> None:
        violation: str | None = None
        if event == "socket.connect":
            violation = f"network:{args[1]!r}"
        elif event in {"subprocess.Popen", "os.system"}:
            violation = f"process:{args[0]!r}"
        elif event in {"open", "os.listdir", "os.scandir"} and args:
            if not _allowed(args[0], roots):
                violation = f"host-file:{args[0]!r}"
        if violation is not None:
            _violations.add(violation)
            _write_log()
            raise PermissionError(f"hermetic oracle denied {violation}")

    sys.addaudithook(audit)
    atexit.register(_write_log)


install()
