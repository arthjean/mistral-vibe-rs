"""What Windows needs in a child environment a driver builds from scratch.

A driver hands the port (or the reference) an environment reduced to the
variables a scenario controls. On Windows that reduction also removes what the
operating system itself reads: without ``SystemRoot`` Winsock fails to
initialize (``WSAStartup`` error 10106), so every loopback connection fails and
the replay measures the driver instead of the port. Executable lookup, shells,
and temporary files need the rest of the base set.

On every other platform :func:`windows_base` adds nothing, so a Linux capture
builds byte-for-byte the environment it built before.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import re
import shutil
import stat
import sys

#: Carried over from the driver's own environment on Windows.
WINDOWS_BASE = ("SystemRoot", "SystemDrive", "windir", "COMSPEC", "PATHEXT", "TEMP", "TMP")

#: Also carried over, though the base set of US-002 does not name them: the
#: per-user application directories, without which a ``python3`` that is the
#: Python Install Manager alias reinstalls an interpreter instead of running.
WINDOWS_USER_DIRECTORIES = ("APPDATA", "LOCALAPPDATA")


def windows_base(home: str | None = None) -> dict[str, str]:
    """The Windows base set, plus ``USERPROFILE`` pointing at the fixture home.

    Windows reads the home from ``USERPROFILE``, so a scenario that isolates
    ``HOME`` passes the same directory here.
    """

    if os.name != "nt":
        return {}
    base = {
        name: os.environ[name]
        for name in WINDOWS_BASE + WINDOWS_USER_DIRECTORIES
        if name in os.environ
    }
    if home is not None:
        base["USERPROFILE"] = home
    return base


def remove_tree(path: str | os.PathLike[str]) -> None:
    """``shutil.rmtree`` that also removes read-only files.

    Git writes its objects read-only; POSIX deletes them through the writable
    directory, while Windows refuses until the file itself is writable.
    """

    def retry(function, target, _error) -> None:
        os.chmod(target, stat.S_IWRITE)
        function(target)

    if sys.version_info >= (3, 12):
        shutil.rmtree(Path(path), onexc=retry)
    else:
        shutil.rmtree(Path(path), onerror=retry)


def path_spellings(path: str) -> list[str]:
    """Every spelling of a fixture path a Windows capture can hold.

    A path reaches a payload as written, escaped inside a JSON string, or with
    forward slashes. On every other platform the path is its only spelling.
    """

    if os.name != "nt" or "\\" not in path:
        return [path]
    return [path, json.dumps(path)[1:-1], path.replace("\\", "/")]


def posix_tails(value: str, placeholders: list[str]) -> str:
    """Write the path after each placeholder with ``/``, as a Linux corpus does.

    The tail is what follows a fixture root up to the next space or quote; a
    JSON-escaped separator counts as one. Off Windows the value is unchanged.
    """

    if os.name != "nt" or "\\" not in value:
        return value
    pattern = re.compile(
        "("
        + "|".join(re.escape(placeholder) for placeholder in placeholders)
        + r")((?:\\\\|\\)[^\s\"'`<>]*)"
    )
    return pattern.sub(
        lambda match: match.group(1) + match.group(2).replace("\\\\", "/").replace("\\", "/"),
        value,
    )
