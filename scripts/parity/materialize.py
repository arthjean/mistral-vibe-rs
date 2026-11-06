"""Reference environments built from a commit, and the attestation that proves
what they hold.

A live probe measures the Python reference, so its verdict is only as good as
the environment it ran. The shared checkout used to be that environment: its
``HEAD`` was checked and its editable install ran whatever its working tree
held. This module replaces it with environments materialized per commit:

1. ``git archive`` extracts the commit from the checkout, which is only read.
2. ``uv sync --locked --no-editable`` builds a relocatable virtual environment
   from a scratch copy of that tree, so the install reads nothing mutable and
   fails when the lockfile and the project disagree.
3. A pristine copy of the tree is kept beside the environment for the drivers
   that read reference sources, and ``attestation.json`` records the commit,
   the lockfile digest, the interpreter, the platform, the tool versions, the
   installed distributions and binaries, and the size, modification time and
   SHA-256 of every file of both trees.
4. The staged directory is renamed into place only after the attestation is
   written, so an interrupted materialization never looks finished.

The layout mirrors a checkout, so whatever reads ``<root>/.venv`` keeps
working::

    .parity/reference-<sha12>/
        attestation.json
        source/     the commit as ``git archive`` exports it
        .venv/      the environment ``uv sync`` built

:func:`verify` is what every capture runs before it measures, and the Rust gate
``vibe_core::parity::live`` checks the same attestation before a live probe
starts. Both compare sizes and modification times first and hash only a file
whose metadata moved, which keeps a warm check within seconds; ``full=True``
hashes every file.

A shared module, not a driver: drivers and ``repin.py`` import it, it imports
no driver.
"""

from __future__ import annotations

import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import time
from typing import Any, Callable

import host_env

#: The workspace root, whichever directory a driver runs from.
REPOSITORY = Path(__file__).resolve().parents[2]

#: Where materialized environments live: gitignored, and excluded from the
#: Cargo workspace so the reference's own Cargo packages build on their own.
DEFAULT_CACHE = REPOSITORY / ".parity"

#: The attestation file at the root of an environment.
ATTESTATION = "attestation.json"

#: The attestation schema this module writes and reads.
ATTESTATION_SCHEMA = 1

#: The command a message names for building or rebuilding an environment.
MATERIALIZE = "python3 scripts/parity/repin.py materialize"

#: The two attested trees, relative to the environment root.
SOURCE = "source"
VENV = ".venv"

#: Directory names no attestation covers: the interpreter writes bytecode
#: caches next to the modules it imports, and a cache is derived, not shipped.
UNATTESTED_DIRECTORIES = frozenset({"__pycache__"})

#: The console scripts every reference publishes, which the attestation lists
#: among its binaries.
CONSOLE_SCRIPTS = ("vibe", "vibe-acp", "vibe-app-server")


class AttestationError(RuntimeError):
    """An environment that cannot serve as the oracle for its commit.

    ``kind`` is ``missing`` when nothing is materialized for the commit,
    ``drift`` when the environment no longer matches its attestation, and
    ``build`` when materializing failed.
    """

    def __init__(self, kind: str, message: str) -> None:
        super().__init__(message)
        self.kind = kind


def short(commit: str) -> str:
    return commit[:12]


def environment_root(commit: str, cache: Path = DEFAULT_CACHE) -> Path:
    """Where the environment of ``commit`` is materialized."""

    return (cache / f"reference-{short(commit)}").resolve()


def scripts_directory(root: Path) -> Path:
    """The directory of an environment's interpreter and console scripts, in
    its Windows layout or its POSIX one."""

    windows = root / VENV / "Scripts"
    return windows if windows.is_dir() else root / VENV / "bin"


def interpreter(root: Path) -> Path:
    directory = scripts_directory(root)
    return directory / ("python.exe" if directory.name == "Scripts" else "python")


def console_script(root: Path, name: str) -> Path | None:
    directory = scripts_directory(root)
    for candidate in (directory / f"{name}.exe", directory / name):
        if candidate.is_file():
            return candidate
    return None


def materialize_command(commit: str) -> str:
    return f"{MATERIALIZE} {commit}"


# --------------------------------------------------------------------------
# Snapshots
# --------------------------------------------------------------------------


def _digest(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            hasher.update(block)
    return hasher.hexdigest()


def _walk(root: Path, tree: str) -> list[str]:
    """Every file under ``root / tree``, as a POSIX path relative to ``root``,
    outside :data:`UNATTESTED_DIRECTORIES`."""

    found = []
    base = root / tree
    for directory, subdirectories, files in os.walk(base):
        subdirectories[:] = sorted(
            name for name in subdirectories if name not in UNATTESTED_DIRECTORIES
        )
        relative = Path(directory).relative_to(root).as_posix()
        # A link that resolves nowhere is not a file, as the Rust gate reads it.
        found.extend(
            f"{relative}/{name}"
            for name in sorted(files)
            if os.path.isfile(os.path.join(directory, name))
        )
    return found


def snapshot(root: Path, trees: tuple[str, ...] = (SOURCE, VENV)) -> dict[str, list[Any]]:
    """``[size, mtime_ns, sha256]`` for every attested file under ``root``."""

    files = {}
    for tree in trees:
        for relative in _walk(root, tree):
            path = root / relative
            status = path.stat()
            files[relative] = [status.st_size, status.st_mtime_ns, _digest(path)]
    return files


# --------------------------------------------------------------------------
# Verification
# --------------------------------------------------------------------------


def read_attestation(root: Path, commit: str) -> dict[str, Any]:
    """The attestation of the environment at ``root``, refused unless it
    names ``commit``."""

    path = root / ATTESTATION
    if not root.is_dir():
        raise AttestationError(
            "missing",
            f"no attested environment for {short(commit)}; run `{materialize_command(commit)}`",
        )
    try:
        attestation = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise AttestationError(
            "missing",
            f"{root} holds no attestation, so it is a partial environment; "
            f"run `{materialize_command(commit)}`",
        ) from None
    except (OSError, UnicodeDecodeError, ValueError) as error:
        raise AttestationError("drift", _drifted(commit, f"its attestation is unreadable: {error}")) from None
    if not isinstance(attestation, dict) or attestation.get("schemaVersion") != ATTESTATION_SCHEMA:
        raise AttestationError(
            "drift", _drifted(commit, f"its attestation is not at schema {ATTESTATION_SCHEMA}")
        )
    if attestation.get("commit") != commit:
        raise AttestationError(
            "drift",
            _drifted(commit, f"its attestation names {attestation.get('commit')}"),
        )
    return attestation


def _drifted(commit: str, detail: str) -> str:
    return (
        f"environment for {short(commit)} no longer matches its attestation ({detail}); "
        f"rerun `{materialize_command(commit)}`"
    )


def verify(root: Path, commit: str, full: bool = False) -> dict[str, Any]:
    """The attestation of the environment at ``root``, after checking that
    every attested file is still what it records and that no file joined
    either tree since.

    A file whose size and modification time match is trusted unless ``full``
    is set; one whose modification time moved is hashed, so touching a file
    without changing it is not drift.
    """

    attestation = read_attestation(root, commit)
    files: dict[str, list[Any]] = attestation.get("files") or {}
    if not files:
        raise AttestationError("drift", _drifted(commit, "its attestation lists no files"))
    present = set()
    for tree in (SOURCE, VENV):
        present.update(_walk(root, tree))
    for relative, (size, mtime, digest) in files.items():
        path = root / relative
        if relative not in present:
            raise AttestationError("drift", _drifted(commit, f"{relative} is missing"))
        status = path.stat()
        if status.st_size != size:
            raise AttestationError("drift", _drifted(commit, f"{relative} changed size"))
        if (full or status.st_mtime_ns != mtime) and _digest(path) != digest:
            raise AttestationError("drift", _drifted(commit, f"{relative} changed"))
    added = sorted(present - files.keys())
    if added:
        raise AttestationError(
            "drift", _drifted(commit, f"{added[0]} is not in the attestation")
        )
    return attestation


def attested(commit: str, cache: Path = DEFAULT_CACHE, full: bool = False) -> Path:
    """The root of the verified environment of ``commit``."""

    root = environment_root(commit, cache)
    verify(root, commit, full)
    return root


# --------------------------------------------------------------------------
# Materialization
# --------------------------------------------------------------------------


def _run(command: list[str], **options: Any) -> subprocess.CompletedProcess[str]:
    return subprocess.run(command, capture_output=True, text=True, check=False, **options)


def _tool_version(command: list[str]) -> str | None:
    try:
        result = _run(command)
    except OSError:
        return None
    return result.stdout.strip() or None if result.returncode == 0 else None


def resolve_commit(checkout: Path, revision: str) -> str:
    try:
        result = _run(["git", "-C", str(checkout), "rev-parse", "--verify", f"{revision}^{{commit}}"])
    except OSError as error:
        raise AttestationError("build", f"git cannot run: {error}") from None
    if result.returncode != 0:
        raise AttestationError(
            "build",
            f"{checkout} does not hold commit {revision}; fetch it there first "
            f"({result.stderr.strip()})",
        )
    return result.stdout.strip()


def _export(checkout: Path, commit: str, destination: Path) -> None:
    destination.mkdir(parents=True)
    archive = destination.with_suffix(".tar")
    result = _run(["git", "-C", str(checkout), "archive", "--format=tar", "-o", str(archive), commit])
    if result.returncode != 0:
        raise AttestationError("build", f"git archive {commit} failed: {result.stderr.strip()}")
    with tarfile.open(archive) as bundle:
        bundle.extractall(destination, filter="data")
    archive.unlink()


def _version(source: Path) -> str | None:
    for line in (source / "pyproject.toml").read_text(encoding="utf-8").splitlines():
        if line.startswith("version = "):
            return line.split("=", 1)[1].strip().strip('"')
    return None


def _distributions(root: Path) -> dict[str, dict[str, str]]:
    """Every distribution installed in the environment, with the SHA-256 of
    the ``RECORD`` that lists its files."""

    found = {}
    for record in sorted((root / VENV).rglob("*.dist-info/RECORD")):
        metadata = record.with_name("METADATA")
        name = version = None
        if metadata.is_file():
            for line in metadata.read_text(encoding="utf-8", errors="replace").splitlines():
                if line.startswith("Name: "):
                    name = line[len("Name: "):].strip()
                elif line.startswith("Version: "):
                    version = line[len("Version: "):].strip()
                if name and version:
                    break
        found[name or record.parent.name] = {
            "version": version or "",
            "record": _digest(record),
        }
    return found


def _binaries(root: Path, files: dict[str, list[Any]]) -> dict[str, str]:
    """The console scripts, the bundled terminal client and every native
    extension, with their SHA-256."""

    names = {f"{script}{suffix}" for script in CONSOLE_SCRIPTS for suffix in ("", ".exe")}
    binaries = {}
    for relative, (_, _, digest) in files.items():
        path = Path(relative)
        if (
            (path.parent.name in ("bin", "Scripts") and path.name in names)
            or "/vibe/_bin/" in relative
            or path.suffix in (".so", ".pyd", ".dll")
        ):
            binaries[relative] = digest
    return binaries


def _discard_partials(cache: Path, commit: str, log: Callable[[str], None]) -> None:
    for partial in sorted(cache.glob(f"reference-{short(commit)}.partial-*")):
        log(f"discarding the partial environment at {partial}")
        host_env.remove_tree(partial, missing_ok=True)


def _rename(staged: Path, root: Path) -> None:
    """Moves the staged environment into place. Windows refuses a directory
    rename while a scanner holds one of its fresh files open, so the move is
    retried for a short while."""

    for attempt in range(20):
        try:
            os.replace(staged, root)
            return
        except PermissionError:
            if attempt == 19:
                raise
            time.sleep(0.5)


def materialize(
    revision: str,
    checkout: Path,
    cache: Path = DEFAULT_CACHE,
    log: Callable[[str], None] = lambda line: print(line, file=sys.stderr),
) -> Path:
    """The root of an attested environment for ``revision``, built from
    ``checkout`` unless one is already there and still matches its
    attestation."""

    commit = resolve_commit(checkout, revision)
    root = environment_root(commit, cache)
    cache.mkdir(parents=True, exist_ok=True)
    _discard_partials(cache, commit, log)
    if root.exists():
        started = time.monotonic()
        try:
            verify(root, commit)
            log(f"{root} is attested for {commit} (verified in {time.monotonic() - started:.1f} s)")
            return root
        except AttestationError as error:
            log(f"rebuilding: {error}")
            host_env.remove_tree(root, missing_ok=True)

    staged = root.with_name(f"{root.name}.partial-{os.getpid()}")
    timings: dict[str, float] = {}
    try:
        started = time.monotonic()
        build = staged / "build"
        _export(checkout, commit, build)
        _export(checkout, commit, staged / SOURCE)
        timings["export"] = time.monotonic() - started

        started = time.monotonic()
        environment = {key: value for key, value in os.environ.items() if key != "VIRTUAL_ENV"}
        environment["UV_PROJECT_ENVIRONMENT"] = str(staged / VENV)
        environment["UV_VENV_RELOCATABLE"] = "1"
        created = _run(["uv", "venv", "--relocatable", str(staged / VENV)], cwd=build, env=environment)
        if created.returncode != 0:
            raise AttestationError("build", f"uv venv failed:\n{created.stderr.strip()}")
        synced = _run(
            ["uv", "sync", "--locked", "--no-editable", "--no-dev"], cwd=build, env=environment
        )
        if synced.returncode != 0:
            raise AttestationError(
                "build",
                f"uv sync --locked --no-editable failed for {commit}:\n{synced.stderr.strip()[-4000:]}",
            )
        timings["sync"] = time.monotonic() - started

        python = interpreter(staged)
        probe = _run(
            [str(python), "-c", "import sys, vibe; print(vibe.__file__); print(sys.version.split()[0])"],
            cwd=staged,
            env={key: value for key, value in environment.items() if key != "PYTHONPATH"},
        )
        if probe.returncode != 0:
            raise AttestationError("build", f"the environment cannot import vibe:\n{probe.stderr.strip()}")
        location, python_version = probe.stdout.split()
        if not Path(location).resolve().is_relative_to((staged / VENV).resolve()):
            raise AttestationError(
                "build", f"vibe imports from {location}, outside the environment: the install is editable"
            )
        host_env.remove_tree(build)

        started = time.monotonic()
        files = snapshot(staged)
        timings["attest"] = time.monotonic() - started
        attestation = {
            "schemaVersion": ATTESTATION_SCHEMA,
            "commit": commit,
            "version": _version(staged / SOURCE),
            "uvLockSha256": _digest(staged / SOURCE / "uv.lock"),
            "python": python_version,
            "platform": f"{sys.platform}-{platform.machine().lower()}",
            "tools": {
                "uv": _tool_version(["uv", "--version"]),
                "cargo": _tool_version(["cargo", "--version"]),
                "rustc": _tool_version(["rustc", "--version"]),
                "git": _tool_version(["git", "--version"]),
            },
            "materializedAt": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
            "timings": {step: round(seconds, 1) for step, seconds in timings.items()},
            "distributions": _distributions(staged),
            "binaries": _binaries(staged, files),
            "files": files,
        }
        staged_attestation = staged / f"{ATTESTATION}.partial"
        staged_attestation.write_text(json.dumps(attestation, indent=1) + "\n", encoding="utf-8")
        os.replace(staged_attestation, staged / ATTESTATION)
        _rename(staged, root)
    except BaseException:
        host_env.remove_tree(staged, missing_ok=True)
        raise
    log(
        f"materialized {commit} at {root}: "
        + ", ".join(f"{step} {seconds:.0f} s" for step, seconds in timings.items())
    )
    return root
