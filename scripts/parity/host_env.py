"""The child environment a driver builds from scratch, and what Windows needs in it.

A driver hands the port (or the reference) an environment reduced to the
variables a scenario controls, starting from :func:`isolated`. On Windows that
reduction also removes what the operating system itself reads: without
``SystemRoot`` Winsock fails to initialize (``WSAStartup`` error 10106), so
every loopback connection fails and the replay measures the driver instead of
the port. Executable lookup, shells, and temporary files need the rest of the
base set.

On every other platform :func:`windows_base` adds nothing, so a Linux capture
builds byte-for-byte the environment it built before.

The directories a scenario runs in come from :func:`scratch_root`, which
removes them whatever the run left behind (:func:`remove_tree`).
"""

from __future__ import annotations

from collections.abc import Iterator
import contextlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile

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


def isolated(
    home: str | os.PathLike[str], vibe_home: str | os.PathLike[str], **variables: str
) -> dict[str, str]:
    """The environment a fixture child starts from.

    It carries this machine's ``PATH``, the fixture homes, ``variables`` (what
    a scenario points the child at, such as its provider), and switches that
    keep the host out of the run: no color, a dumb terminal, CI behavior, and a
    session bus that does not exist, so no keyring answers. A caller adds
    anything else after these.
    """

    home = str(home)
    return {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": home,
        **windows_base(home),
        "VIBE_HOME": str(vibe_home),
        **variables,
        "LANG": "C.UTF-8",
        "TERM": "dumb",
        "NO_COLOR": "1",
        "CI": "true",
        "DBUS_SESSION_BUS_ADDRESS": "unix:path=/nonexistent",
    }


def remove_tree(path: str | os.PathLike[str], missing_ok: bool = False) -> None:
    """``shutil.rmtree`` that also removes read-only entries, and with
    ``missing_ok`` accepts a ``path`` that does not exist.

    POSIX removes an entry through its directory, so a directory left without
    write permission keeps what it holds: the plugin package store a server
    hardens under ``<vibe-home>/logs/session/plugins/packages`` is one. Windows
    instead refuses a read-only file itself, as Git writes its objects. A
    refused removal makes both the parent and the entry writable, then retries;
    a symbolic link is left as it is, since its mode would be its target's.
    """

    if missing_ok and not os.path.lexists(path):
        return

    def retry(function, target, _error) -> None:
        os.chmod(os.path.dirname(target), stat.S_IRWXU)
        if not os.path.islink(target):
            os.chmod(target, stat.S_IRWXU)
        function(target)

    if sys.version_info >= (3, 12):
        shutil.rmtree(Path(path), onexc=retry)
    else:
        shutil.rmtree(Path(path), onerror=retry)


@contextlib.contextmanager
def scratch_root(prefix: str) -> Iterator[Path]:
    """A new directory under the temporary root, named from ``prefix`` and
    resolved, removed with :func:`remove_tree` when the block ends.

    A failed removal raises rather than leaving the directory behind
    unnoticed.
    """

    root = Path(tempfile.mkdtemp(prefix=prefix)).resolve()
    try:
        yield root
    finally:
        remove_tree(root)


def git_isolated(global_config: str | None = os.devnull, **variables: str) -> dict[str, str]:
    """The variables that keep the host's git configuration out of a fixture
    repository, then ``variables``.

    No system file is read, and ``global_config`` is the global one: the null
    device by default, or ``None`` for a scenario whose fixture ``HOME``
    carries the ``.gitconfig`` git should read.
    """

    isolated = {"GIT_CONFIG_NOSYSTEM": "1"}
    if global_config is not None:
        isolated["GIT_CONFIG_GLOBAL"] = global_config
    return {**isolated, **variables}


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
