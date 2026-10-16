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
import signal
import stat
import subprocess
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

    A path reaches a payload as written, escaped inside a JSON string, with
    forward slashes, or case-folded as ``ntpath.normcase`` folds it (the
    ``vibe-path:`` session patterns). On every other platform the path is its
    only spelling.
    """

    if os.name != "nt" or "\\" not in path:
        return [path]
    folded = path.lower()
    spellings = [path, json.dumps(path)[1:-1], path.replace("\\", "/")]
    if folded != path:
        spellings += [folded, json.dumps(folded)[1:-1]]
    return spellings


#: A scenario value that is wholly a path under a fixture root.
_ROOTED_PATH = re.compile(r"\$(?:WS|ROOT|HOME|VIBE_HOME|VH)/[^\s\"'`<>]*")


def native_path(template: str, value: str) -> str:
    """``value``, substituted from ``template``, with a Windows client's separators.

    A scenario writes a path argument as ``$WS/notes.txt``. Substituted on
    Windows that reads ``C:\\...\\workspace/notes.txt``, a spelling no Windows
    client sends and one an exact-match path key (the review checkpoints) does
    not find. Only a value that is a rooted path as a whole is respelled; off
    Windows the value is unchanged.
    """

    if os.name != "nt" or not _ROOTED_PATH.fullmatch(template):
        return value
    return value.replace("/", "\\")


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


#: Run in a helper process: join the console of the process named by the first
#: argument and raise Ctrl+C there. The helper ignores the event itself.
_CTRL_C = """
import ctypes, sys
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
kernel32.FreeConsole()
if not kernel32.AttachConsole(int(sys.argv[1])):
    sys.exit("AttachConsole failed: %d" % ctypes.get_last_error())
kernel32.SetConsoleCtrlHandler(None, True)
if not kernel32.GenerateConsoleCtrlEvent(0, 0):
    sys.exit("GenerateConsoleCtrlEvent failed: %d" % ctypes.get_last_error())
"""


def interruptible() -> dict[str, object]:
    """``Popen`` arguments for a child :func:`interrupt` will reach.

    Windows has no SIGINT to send: Ctrl+C is a console event raised for every
    process attached to a console. The child gets a hidden console of its own,
    so the event reaches it without reaching the driver. A process started in a
    new process group (as a test runner may start the driver) ignores Ctrl+C,
    and a child inherits that, so the driver clears it first. Off Windows
    nothing is added.
    """

    if os.name != "nt":
        return {}
    import ctypes

    ctypes.WinDLL("kernel32").SetConsoleCtrlHandler(None, False)
    startup = subprocess.STARTUPINFO()
    startup.dwFlags |= subprocess.STARTF_USESHOWWINDOW
    startup.wShowWindow = 0  # SW_HIDE
    return {"creationflags": subprocess.CREATE_NEW_CONSOLE, "startupinfo": startup}


def interrupt(process: subprocess.Popen) -> None:
    """Deliver what a terminal's Ctrl+C delivers: SIGINT, or on Windows the
    console Ctrl+C event (the child must come from :func:`interruptible`)."""

    if os.name != "nt":
        process.send_signal(signal.SIGINT)
        return
    subprocess.run(
        [sys.executable, "-c", _CTRL_C, str(process.pid)],
        check=True,
        timeout=30,
        creationflags=subprocess.CREATE_NO_WINDOW,
    )


def forget_credential(service: str, account: str) -> None:
    """Remove what a scenario stored for ``account`` in the Windows vault.

    Linux captures turn the keyring off through ``DBUS_SESSION_BUS_ADDRESS``;
    the Windows Credential Manager has no such switch, so a credential a
    scenario stores outlives the run and reaches the next one. This removes
    the entries the reference's ``WinVaultKeyring`` and the port write for the
    account (the plain ``service`` target only while ``account`` holds it, the
    ``account@service`` target, and the earlier port's ``account.service``)
    and leaves every other holder in place. Off Windows nothing happens.
    """

    if os.name != "nt":
        return
    import ctypes
    from ctypes import wintypes

    class Credential(ctypes.Structure):
        _fields_ = [
            ("Flags", wintypes.DWORD),
            ("Type", wintypes.DWORD),
            ("TargetName", wintypes.LPWSTR),
            ("Comment", wintypes.LPWSTR),
            ("LastWritten", wintypes.FILETIME),
            ("CredentialBlobSize", wintypes.DWORD),
            ("CredentialBlob", ctypes.c_void_p),
            ("Persist", wintypes.DWORD),
            ("AttributeCount", wintypes.DWORD),
            ("Attributes", ctypes.c_void_p),
            ("TargetAlias", wintypes.LPWSTR),
            ("UserName", wintypes.LPWSTR),
        ]

    advapi32 = ctypes.WinDLL("advapi32", use_last_error=True)
    generic = 1  # CRED_TYPE_GENERIC
    found = ctypes.POINTER(Credential)()
    if advapi32.CredReadW(service, generic, 0, ctypes.byref(found)):
        holder = found.contents.UserName
        advapi32.CredFree(found)
        if holder == account:
            advapi32.CredDeleteW(service, generic, 0)
    for target in (f"{account}@{service}", f"{account}.{service}"):
        advapi32.CredDeleteW(target, generic, 0)
