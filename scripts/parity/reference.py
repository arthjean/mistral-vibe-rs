"""The reference environment a capture measures, and how a capture runs its
code.

Every capture script resolves the Python reference here, so the environment is
checked one way, a refusal reads the same whichever script raised it, and the
pinned source is imported one way.

A capture runs an environment materialized for its commit
(``scripts/parity/materialize.py``), never the shared checkout: the checkout is
only the object store ``git archive`` reads, so its ``HEAD`` and its working
tree cannot reach a corpus. :func:`environment` finds the environment and
checks it against its attestation, the same check
``vibe_core::parity::live`` applies before a Rust test runs a live probe.
``--reference`` names either an environment root, as the Rust probes pass it,
or anything else (the checkout, by default), in which case the environment of
the commit under :data:`DEFAULT_CACHE` is used.

A black-box capture starts the environment's own console scripts
(:func:`reference_command`), installed without an editable link. A capture
that imports ``vibe`` itself re-runs under the environment's interpreter with
the attested source tree first on ``PYTHONPATH`` (:func:`reexecute_pinned`).

Every resolution appends one line to ``target/parity/drivers.jsonl``, so a
live run can show that each driver's reference side executed.

Oracles that live outside ``scripts/parity`` reach this module by inserting
this directory on ``sys.path``.
"""

from __future__ import annotations

import datetime
import json
import os
from pathlib import Path
import subprocess
import sys

import materialize
import provenance
from materialize import DEFAULT_CACHE
from pin import EXPECTED_COMMIT

__all__ = [
    "DEFAULT_CACHE",
    "INTERPRETER_VARIABLE",
    "OracleError",
    "environment",
    "reexecute_pinned",
    "reference_command",
    "reference_interpreter",
    "require_pin",
]

#: Names the interpreter a capture should run under. Only the attested
#: environment's own interpreter is accepted, so the variable can select it
#: but never substitute another.
INTERPRETER_VARIABLE = "VIBE_PARITY_PYTHON"

#: Set on the re-executed process, so it runs the capture instead of
#: re-executing again.
_REEXEC_MARKER = "VIBE_PARITY_PINNED_TREE"

#: Carries the caller's own ``PYTHONPATH`` across the re-execution, so the
#: re-executed process can put it back.
_CALLER_PATH = "VIBE_PARITY_CALLER_PYTHONPATH"

#: Where a capture records that its reference side ran, under the Cargo target
#: directory the Rust gate records its probes in.
RECORD = "parity/drivers.jsonl"


class OracleError(RuntimeError):
    """Raised when a capture cannot be produced from an authoritative state."""


def _target_commit(expected: str | None) -> str:
    try:
        return provenance.expected_commit(expected, EXPECTED_COMMIT)
    except PermissionError as error:
        raise OracleError(str(error)) from None


def _record(commit: str, root: Path) -> None:
    target = Path(os.environ.get("CARGO_TARGET_DIR") or provenance.REPOSITORY / "target")
    if not target.is_absolute():
        target = provenance.REPOSITORY / target
    line = {
        "driver": Path(sys.argv[0]).name,
        "commit": commit,
        "environment": str(root),
        "at": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
    }
    try:
        (target / RECORD).parent.mkdir(parents=True, exist_ok=True)
        with (target / RECORD).open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(line) + "\n")
    except OSError:
        pass


def environment(
    reference: Path, expected: str | None = EXPECTED_COMMIT, *, cache: Path = DEFAULT_CACHE
) -> tuple[str, Path]:
    """The commit this capture may record and the root of its attested
    environment, refused when no environment for that commit exists or when
    one no longer matches its attestation.

    The commit is the pin, whatever ``expected`` says, unless
    ``scripts/parity/repin.py capture`` started the capture: an empty or
    ``None`` ``expected`` (``--expected-commit ''``, ``--allow-unpinned``) or
    another commit is refused anywhere else, since the corpus it wrote would
    record a commit no replay accepts (:func:`provenance.expected_commit`).
    """

    commit = _target_commit(expected)
    reference = Path(reference)
    root = (
        reference.resolve()
        if (reference / materialize.ATTESTATION).is_file()
        else materialize.environment_root(commit, cache)
    )
    try:
        materialize.verify(root, commit)
    except materialize.AttestationError as error:
        raise OracleError(str(error)) from None
    _record(commit, root)
    return commit, root


def require_pin(reference: Path, expected: str | None = EXPECTED_COMMIT) -> str:
    """The commit this capture may record, refused as :func:`environment`
    refuses it."""

    return environment(reference, expected)[0]


def _is_attested_interpreter(candidate: Path, root: Path) -> bool:
    normalized = os.path.normcase(os.path.abspath(candidate.parent))
    return normalized == os.path.normcase(os.path.abspath(materialize.scripts_directory(root)))


def reference_interpreter(
    reference: Path, override: Path | None = None, expected: str | None = EXPECTED_COMMIT
) -> Path:
    """The interpreter that runs reference code: the attested environment's.

    ``override`` and :data:`INTERPRETER_VARIABLE` may name it, as callers that
    already run under it do, but an interpreter outside the environment is
    refused rather than trusted.
    """

    _, root = environment(reference, expected)
    found = materialize.interpreter(root)
    for candidate in (override, os.environ.get(INTERPRETER_VARIABLE)):
        if candidate and not _is_attested_interpreter(Path(candidate), root):
            raise OracleError(
                f"{candidate} is not the interpreter of the attested environment {root}; "
                f"captures run only under {found}"
            )
    if not found.is_file():
        raise OracleError(f"the attested environment {root} has no interpreter at {found}")
    return found


def reference_command(
    reference: Path, script: str, expected: str | None = EXPECTED_COMMIT
) -> tuple[str, list[str]]:
    """The commit this capture may record and the command that runs the
    attested environment's console script ``script``."""

    commit, root = environment(reference, expected)
    found = materialize.console_script(root, script)
    if found is None:
        raise OracleError(
            f"no reference entry point `{script}` in {materialize.scripts_directory(root)}; "
            f"rerun `{materialize.materialize_command(commit)}`"
        )
    return commit, [str(found)]


def reexecute_pinned(
    reference: Path,
    expected: str | None = EXPECTED_COMMIT,
    *,
    cache: Path = DEFAULT_CACHE,
    override: Path | None = None,
    source_paths: tuple[Path, ...] = (),
) -> tuple[str, Path]:
    """Re-runs this script under the attested environment's interpreter
    importing its source tree, and returns the commit and that tree to the
    process that then runs the capture.

    The source tree is the commit as ``git archive`` exported it, checked
    against the attestation like the environment, so an untracked module or a
    stray edit never reaches a corpus. ``source_paths`` are directories of the
    tree that also go on the path, ahead of whatever the interpreter would
    otherwise find.

    POSIX replaces the process. Windows has no such call: ``os.execv`` starts a
    new process and exits, so whoever ran the script would see that exit code
    instead of the capture's. There the script waits for the capture and exits
    with its code.

    The tree reaches the interpreter through ``PYTHONPATH`` at startup, and the
    re-executed process then restores the environment it was given, so a
    capture that reads its environment or hands it to a child observes the
    caller's, not the re-execution's.
    """

    commit, root = environment(reference, expected, cache=cache)
    tree = root / materialize.SOURCE
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
    executable = str(reference_interpreter(root, override, commit))
    environment_variables = dict(os.environ)
    environment_variables[_REEXEC_MARKER] = str(tree)
    caller = environment_variables.get("PYTHONPATH")
    if caller is not None:
        environment_variables[_CALLER_PATH] = caller
    environment_variables["PYTHONPATH"] = os.pathsep.join(
        [str(tree), *(str(tree / path) for path in source_paths), *([caller] if caller else [])]
    )
    command = [executable, str(Path(sys.argv[0]).resolve()), *sys.argv[1:]]
    if os.name == "nt":
        sys.exit(subprocess.run(command, env=environment_variables, check=False).returncode)
    os.execve(executable, command, environment_variables)


def _imports_pinned_vibe(tree: Path) -> bool:
    try:
        import vibe
    except Exception:
        return False
    return Path(vibe.__file__).resolve().is_relative_to(tree.resolve())
