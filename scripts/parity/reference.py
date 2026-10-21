"""The reference checkout a capture measures, and how a capture runs its code.

Every capture script resolves the Python reference here, so the pin is checked
one way, a refusal reads the same whichever script raised it, and the pinned
source is imported one way.

The checkout must sit at the pin: its ``HEAD`` is :data:`pin.EXPECTED_COMMIT`,
the test ``vibe_core::parity::off_pin_reason`` applies before a Rust test runs a
live probe, and :data:`pin.RESTORE_COMMAND` returns a checkout that moved.
Nothing weaker serves every capture: a black-box one runs the checkout's own
console scripts, which import its working tree.

A capture that imports ``vibe`` itself can run it from the pinned commit
rather than from that working tree: :func:`reexecute_pinned` extracts the
commit with ``git archive`` under ``.parity/`` and re-runs the script under the
checkout's interpreter with that tree first on ``PYTHONPATH``. The interpreter
supplies the third-party dependencies and the gitignored native harness build;
the tree supplies ``vibe`` exactly as committed, so an untracked module or a
stray edit in the checkout never reaches a corpus, and the checkout itself is
only read.

Oracles that live outside ``scripts/parity`` reach this module by inserting
this directory on ``sys.path``.
"""

from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile

from pin import EXPECTED_COMMIT, RESTORE_COMMAND

#: Where extracted trees are kept between runs: gitignored, at the repository
#: root whichever directory a capture runs from, and keyed by commit so a
#: re-pin extracts a new tree instead of reusing the old one.
DEFAULT_CACHE = Path(__file__).resolve().parents[2] / ".parity"

#: Names an interpreter for a checkout whose virtual environment sits
#: elsewhere, as it does for ``vibe_core::parity::pinned_interpreter``.
INTERPRETER_VARIABLE = "VIBE_PARITY_PYTHON"

#: Set on the re-executed process, so it runs the capture instead of
#: re-executing again.
_REEXEC_MARKER = "VIBE_PARITY_PINNED_TREE"

#: Carries the caller's own ``PYTHONPATH`` across the re-execution, so the
#: re-executed process can put it back.
_CALLER_PATH = "VIBE_PARITY_CALLER_PYTHONPATH"


class OracleError(RuntimeError):
    """Raised when a capture cannot be produced from an authoritative state."""


def _git(reference: Path, *arguments: str) -> str:
    result = subprocess.run(
        ["git", *arguments],
        cwd=reference,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise OracleError(
            f"git {' '.join(arguments)} failed in {reference}: {result.stderr.strip()}"
        )
    return result.stdout.strip()


def require_pin(reference: Path, expected: str | None = EXPECTED_COMMIT) -> str:
    """The commit the checkout at ``reference`` sits at, refused unless it is
    ``expected``.

    An empty or ``None`` ``expected`` accepts whatever commit the checkout
    holds, which is how a capture is pointed at a checkout off the pin on
    purpose, for a re-pin.
    """

    if not reference.is_dir():
        raise OracleError(
            f"no reference checkout at {reference}; set VIBE_REFERENCE to the checkout "
            "path or pass --reference"
        )
    commit = _git(reference, "rev-parse", "HEAD")
    if expected and commit != expected:
        raise OracleError(
            f"{reference} is at {commit}, not the pinned {expected}; "
            f"restore it with `{RESTORE_COMMAND}`"
        )
    return commit


def reference_interpreter(reference: Path, override: Path | None = None) -> Path:
    """The interpreter that runs reference code: ``override``, then
    :data:`INTERPRETER_VARIABLE`, then the checkout's virtual environment in
    its POSIX layout and then its Windows one."""

    candidates = [override] if override else []
    if os.environ.get(INTERPRETER_VARIABLE):
        candidates.append(Path(os.environ[INTERPRETER_VARIABLE]))
    candidates += [reference / ".venv/bin/python", reference / ".venv/Scripts/python.exe"]
    found = next((candidate for candidate in candidates if candidate.is_file()), None)
    if found is None:
        raise OracleError(
            f"no reference interpreter: looked for {INTERPRETER_VARIABLE} and a virtual "
            f"environment in {reference}; run `uv sync --frozen` there"
        )
    return found


def pinned_tree(reference: Path, commit: str, cache: Path = DEFAULT_CACHE) -> Path:
    """The source tree of ``commit``, extracted once and reused across runs.

    The extraction lands in a per-process directory and is moved into place at
    the end, so a concurrent run reads a complete tree or builds its own rather
    than importing a half-written one.
    """

    # Absolute, because `_git` runs inside the checkout and a relative output
    # path would land there.
    tree = (cache / f"reference-{commit[:12]}").resolve()
    marker = Path("vibe") / "__init__.py"
    if (tree / marker).is_file():
        return tree
    staged = tree.with_name(f"{tree.name}.{os.getpid()}.partial")
    staged.mkdir(parents=True, exist_ok=True)
    archive = staged.with_suffix(".tar")
    _git(reference, "archive", "--format=tar", "-o", str(archive), commit)
    with tarfile.open(archive) as bundle:
        bundle.extractall(staged, filter="data")
    archive.unlink(missing_ok=True)
    if not (staged / marker).is_file():
        raise OracleError(f"the extracted tree at {staged} carries no `vibe` package")
    try:
        staged.rename(tree)
    except OSError:
        # Another run finished first, which is the only way the destination
        # exists by now. Its tree is the same commit, so theirs wins.
        shutil.rmtree(staged, ignore_errors=True)
    if not (tree / marker).is_file():
        raise OracleError(f"the extracted tree at {tree} carries no `vibe` package")
    return tree


def reexecute_pinned(
    reference: Path,
    expected: str | None = EXPECTED_COMMIT,
    *,
    cache: Path = DEFAULT_CACHE,
    override: Path | None = None,
    source_paths: tuple[Path, ...] = (),
) -> tuple[str, Path]:
    """Re-runs this script under :func:`reference_interpreter` importing the
    pinned tree, and returns the commit and that tree to the process that then
    runs the capture.

    ``source_paths`` are directories of the tree that also go on the path, ahead
    of whatever the interpreter would otherwise find.

    POSIX replaces the process. Windows has no such call: ``os.execv`` starts a
    new process and exits, so whoever ran the script would see that exit code
    instead of the capture's. There the script waits for the capture and exits
    with its code.

    The tree reaches the interpreter through ``PYTHONPATH`` at startup, and the
    re-executed process then restores the environment it was given, so a
    capture that reads its environment or hands it to a child observes the
    caller's, not the re-execution's.
    """

    commit = require_pin(reference, expected)
    tree = pinned_tree(reference, commit, cache)
    if os.environ.get(_REEXEC_MARKER) == str(tree):
        del os.environ[_REEXEC_MARKER]
        caller = os.environ.pop(_CALLER_PATH, None)
        if caller is None:
            del os.environ["PYTHONPATH"]
        else:
            os.environ["PYTHONPATH"] = caller
        if not _imports_pinned_vibe(tree):
            raise OracleError(f"the reference interpreter did not import `vibe` from {tree}")
        return commit, tree
    executable = str(reference_interpreter(reference, override))
    environment = dict(os.environ)
    environment[_REEXEC_MARKER] = str(tree)
    caller = environment.get("PYTHONPATH")
    if caller is not None:
        environment[_CALLER_PATH] = caller
    environment["PYTHONPATH"] = os.pathsep.join(
        [str(tree), *(str(tree / path) for path in source_paths), *([caller] if caller else [])]
    )
    command = [executable, str(Path(sys.argv[0]).resolve()), *sys.argv[1:]]
    if os.name == "nt":
        sys.exit(subprocess.run(command, env=environment, check=False).returncode)
    os.execve(executable, command, environment)


def _imports_pinned_vibe(tree: Path) -> bool:
    try:
        import vibe
    except Exception:
        return False
    return Path(vibe.__file__).resolve().is_relative_to(tree.resolve())
