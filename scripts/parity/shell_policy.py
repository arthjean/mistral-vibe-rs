#!/usr/bin/env python3
"""Capture the shell policy the pinned Python reference resolves.

The reference checkout is a read-only behavioral oracle. This script asks it
the four questions EP-033 ports:

* what ``_extract_commands`` returns for a fixed command list, which is the
  bash-grammar extraction the hand-rolled tokenizer in this port replaces;
* which commands have their path operands inspected (``_PATH_COMMANDS``), and
  which predicates make ``find`` an execution rather than a read;
* what ``_collect_outside_dirs`` answers for operands pointing inside the
  workdir, outside it, and into the scratchpad;
* what ``BashTool.resolve_permission`` decides for a fixed command list, down
  to the scope, the two patterns and the label of every requirement it raises.

The corpus is committed, like the permission-vocabulary one: it records command
names, node-kind names, booleans and the answers to cases this repository
authored. Every recorded label is the command text the case itself carries, or
a placeholder substituted for a host path. No reference-authored prose is
recorded, which is what ``NOTICE`` forbids shipping.

Usage::

    scripts/parity/shell_policy.py --reference /path/to/reference
    scripts/parity/shell_policy.py --interpreter /path/to/python

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it.

The wrapper re-executes itself with an interpreter that can import ``vibe``
when the current one cannot.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT

SCHEMA_VERSION = 1
DEFAULT_OUTPUT = Path("crates/vibe-core/tests/shell-policy/policy.json")
INTERPRETER_VARIABLE = "VIBE_PARITY_PYTHON"

#: Command strings whose grammar extraction is recorded. They cover the shapes
#: a tokenizer decides differently from a grammar: a heredoc whose command node
#: sits under a redirect, the four chain operators, quoting of every kind, a
#: command substitution, an assignment prefix, a compound statement, and text
#: the grammar cannot parse.
EXTRACTION_CASES: tuple[str, ...] = (
    "",
    "ls",
    "ls -la",
    "python3 << 'EOF'",
    "python3 <<'EOF'\nprint(1)\nEOF",
    "cat README.md && rm -rf build",
    "grep needle src | wc -l",
    "ls; pwd",
    "cd /tmp || true",
    "cat file.txt > out.txt",
    "wc -l < input.txt",
    "echo 'hello world'",
    'echo "hello world"',
    "echo hello\\ world",
    "ls -la 'my dir'",
    "git config user.name",
    "npm run build -- --watch",
    "find . -name '*.rs' -exec rm {} ;",
    "cat $(which ls)",
    "VAR=1 ./script.sh",
    "if [ -f x ]; then cat x; fi",
    "for f in *.txt; do cat $f; done",
    "/usr/bin/vim notes.txt",
    "sudo apt update",
    "cat 'unterminated",
    "cat file.txt 2>&1",
    "ls & pwd",
    "( cd src && ls )",
)

#: Command strings whose resolved permission is recorded. They cover each of the
#: four lists, the five commands this port used to deny outright, the find
#: execution predicates, the sensitive prefix, and the chains that must not let
#: an approval for one segment cover the next.
RESOLUTION_CASES: tuple[str, ...] = (
    "ls -la",
    "cat README.md",
    "wc -l src/main.rs",
    "git status",
    "git diff --stat",
    "rm -rf build",
    "rm file.txt",
    "dd if=/dev/zero of=disk.img",
    "mkfs.ext4 /dev/sdb1",
    "shutdown -h now",
    "eval echo hi",
    "vim notes.txt",
    "vi",
    "nano",
    "emacs",
    "tmux",
    "screen",
    "gdb ./binary",
    "pdb script.py",
    "passwd",
    "bash -i",
    "sh -i",
    "zsh -i",
    "python3",
    "python",
    "ipython",
    "bash",
    "sh",
    "su",
    "nohup",
    "python3 script.py",
    "/usr/bin/python3",
    "/usr/bin/vim notes.txt",
    "python3 << 'EOF'",
    "python3 <<'EOF'\nprint(1)\nEOF",
    "",
    "cat 'unterminated",
    "sudo apt update",
    "sudo ls",
    "find . -name '*.rs'",
    "find . -exec rm {} ;",
    "find . -execdir rm {} ;",
    "find . -ok rm {} ;",
    "find . -okdir rm {} ;",
    "find . -exec rm {} ; && find . -exec rm {} ;",
    "cargo build",
    "npm run build",
    "npm run test",
    "cat README.md && rm -rf build",
    "cat README.md && cat CHANGELOG.md",
    "echo one && echo two",
    "cargo build && cargo test",
    "unknown-binary --flag",
    "cd src",
    "tree",
    "whoami",
    # The cases where this port withholds a grant the reference gives, each
    # recorded so the divergence is measured rather than asserted.
    "cat $(which ls)",
    "cat file.txt > out.txt",
    "git -c core.pager=sh log",
    "git diff --no-index /etc/passwd /dev/null",
    "rg --pre helper needle .",
    "git reset --hard",
    "git reset --hard -- src",
)

#: Commands whose path operands are resolved against a workdir this script
#: creates. ``<workdir>``, ``<outside>``, ``<scratchpad>`` and ``<home>`` are
#: substituted back into both the case and its answer, so the corpus replays on
#: another machine.
OUTSIDE_DIR_CASES: tuple[str, ...] = (
    "cat <workdir>/inside.txt",
    "cat ./inside.txt",
    "cat inside.txt",
    "cat <outside>/secret.txt",
    "cat <outside>/nested/secret.txt",
    "grep needle <outside>/secret.txt",
    "wc -l <outside>/secret.txt",
    "ls <outside>",
    "cat <scratchpad>/note.txt",
    "rm <outside>/secret.txt",
    "cp <outside>/secret.txt <workdir>/copy.txt",
    "chmod +x <outside>/secret.txt",
    "chmod 755 <outside>/secret.txt",
    "cat -n <outside>/secret.txt",
    "ls --color <outside>",
    "cat <outside>/a.txt && cat <outside>/b.txt",
    "echo <outside>/secret.txt",
    "cat ~/.ssh/id_rsa",
    "cat /etc/passwd",
    # An ascending operand, which the reference folds before it positions it:
    # one that lands outside, one that lands back inside, and one that ascends
    # past the root.
    "cat ../elsewhere/secret.txt",
    "cat <workdir>/../elsewhere/nested/secret.txt",
    "cat sub/../inside.txt",
    "cat ../../../../etc/passwd",
)


class OracleError(RuntimeError):
    """Raised when the corpus cannot be produced from an authoritative state."""


# --------------------------------------------------------------------------
# Reference pinning
# --------------------------------------------------------------------------


def resolve_reference(reference: Path, expected_commit: str | None) -> dict[str, str]:
    if not reference.is_dir():
        raise OracleError(f"reference checkout is missing: {reference}")
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=reference,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise OracleError(
            f"git rev-parse failed in {reference}: {result.stderr.strip()}"
        )
    commit = result.stdout.strip()
    if expected_commit and commit != expected_commit:
        raise OracleError(
            f"reference checkout is at {commit}, not the pinned {expected_commit}"
        )
    return {"commit": commit}


def reexecute_with_reference_interpreter(
    reference: Path, interpreter: Path | None
) -> None:
    """Re-runs this script under an interpreter that can import ``vibe``."""
    try:
        import vibe  # noqa: F401

        return
    except ImportError:
        pass
    candidates = [
        interpreter,
        Path(os.environ[INTERPRETER_VARIABLE])
        if os.environ.get(INTERPRETER_VARIABLE)
        else None,
        reference / ".venv/bin/python",
        reference / ".venv/Scripts/python.exe",
    ]
    candidate = next(
        (path for path in candidates if path is not None and path.is_file()), None
    )
    if candidate is None:
        raise OracleError(
            f"cannot import `vibe` and no reference interpreter under {reference}"
        )
    if Path(sys.executable).resolve() == candidate.resolve():
        raise OracleError(f"{candidate} cannot import `vibe`")
    os.execv(
        str(candidate), [str(candidate), str(Path(__file__).resolve()), *sys.argv[1:]]
    )


# --------------------------------------------------------------------------
# Capture
# --------------------------------------------------------------------------


def capture_extraction(reference: Path) -> list[dict[str, Any]]:
    """What ``_extract_commands`` returns for the authored cases."""
    sys.path.insert(0, str(reference))
    from vibe.core.tools.builtins.bash import _extract_commands

    return [
        {"command": command, "segments": list(_extract_commands(command))}
        for command in EXTRACTION_CASES
    ]


def capture_command_sets(reference: Path) -> dict[str, Any]:
    """The sets that decide which commands have their operands inspected."""
    sys.path.insert(0, str(reference))
    from vibe.core.tools.builtins.bash import (
        _FIND_EXECUTION_PREDICATES,
        _MUTATING_PATH_COMMANDS,
        _PATH_COMMANDS,
        _get_default_allowlist,
        default_read_only_commands,
    )

    readers = default_read_only_commands()
    if not readers:
        raise OracleError("the reference publishes no read-only command")
    return {
        "pathCommands": sorted(_PATH_COMMANDS),
        "mutatingPathCommands": sorted(_MUTATING_PATH_COMMANDS),
        "findExecutionPredicates": sorted(_FIND_EXECUTION_PREDICATES),
        "readOnlyCommands": list(readers),
        "allowlist": list(_get_default_allowlist()),
        # The reference documents `_PATH_COMMANDS` as a superset of the
        # read-only allowlist; the relation is recorded rather than assumed.
        "pathCommandsCoverReaders": all(
            reader in _PATH_COMMANDS for reader in readers
        ),
    }


def _bash_tool(reference: Path, workdir: Path, scratchpad: Path) -> Any:
    sys.path.insert(0, str(reference))
    from vibe.core.config.harness_files import HarnessFilesManager
    from vibe.core.tools.builtins.bash import Bash, BashToolConfig

    config = BashToolConfig()
    return Bash(
        lambda: config,
        None,
        cwd=workdir,
        harness_files=HarnessFilesManager(sources=(), cwd=workdir),
        scratchpad_dir=scratchpad,
    )


def _placeholders(workdir: Path, outside: Path, scratchpad: Path) -> list[tuple[str, str]]:
    """Host paths and the placeholder each is recorded under.

    Longest first, so a nested directory is substituted before its parent.
    """
    pairs = [
        (str(scratchpad.resolve()), "<scratchpad>"),
        (str(outside.resolve()), "<outside>"),
        (str(workdir.resolve()), "<workdir>"),
        (str(Path.home().resolve()), "<home>"),
    ]
    return sorted(pairs, key=lambda pair: len(pair[0]), reverse=True)


def _normalize(text: str, placeholders: list[tuple[str, str]]) -> str:
    for host, placeholder in placeholders:
        text = text.replace(host, placeholder)
    return text


def _expand(text: str, placeholders: list[tuple[str, str]]) -> str:
    for host, placeholder in placeholders:
        text = text.replace(placeholder, host)
    return text


def capture_outside_dirs(reference: Path) -> list[dict[str, Any]]:
    """What ``_collect_outside_dirs`` answers for the authored operands."""
    sys.path.insert(0, str(reference))
    import tempfile

    from vibe.core.tools.builtins.bash import _collect_outside_dirs, _extract_commands

    with tempfile.TemporaryDirectory() as root:
        workdir = Path(root) / "workspace"
        outside = Path(root) / "elsewhere"
        scratchpad = workdir / ".vibe" / "scratchpad"
        (outside / "nested").mkdir(parents=True)
        scratchpad.mkdir(parents=True)
        (workdir / "inside.txt").write_text("inside", encoding="utf-8")
        (outside / "secret.txt").write_text("secret", encoding="utf-8")
        (outside / "nested" / "secret.txt").write_text("secret", encoding="utf-8")
        (scratchpad / "note.txt").write_text("note", encoding="utf-8")
        placeholders = _placeholders(workdir, outside, scratchpad)

        captured = []
        for case in OUTSIDE_DIR_CASES:
            command = _expand(case, placeholders)
            dirs = _collect_outside_dirs(
                _extract_commands(command),
                cwd=workdir,
                project_roots=[workdir],
                scratchpad_dir=scratchpad,
            )
            captured.append(
                {
                    "command": case,
                    "directories": sorted(
                        _normalize(entry, placeholders) for entry in dirs
                    ),
                }
            )
        return captured


def capture_resolutions(reference: Path) -> list[dict[str, Any]]:
    """What ``BashTool.resolve_permission`` decides for the authored commands."""
    sys.path.insert(0, str(reference))
    import tempfile

    from vibe.core.tools.builtins.bash import BashArgs

    with tempfile.TemporaryDirectory() as root:
        workdir = Path(root) / "workspace"
        scratchpad = workdir / ".vibe" / "scratchpad"
        scratchpad.mkdir(parents=True)
        tool = _bash_tool(reference, workdir, scratchpad)

        captured = []
        for command in RESOLUTION_CASES:
            context = tool.resolve_permission(BashArgs(command=command))
            if context is None:
                captured.append({"command": command, "permission": None})
                continue
            captured.append(
                {
                    "command": command,
                    "permission": str(context.permission.value),
                    # The reason is reference prose, so only whether one exists
                    # is recorded. This port writes its own refusal text.
                    "hasReason": context.reason is not None,
                    "requirements": [
                        {
                            "scope": str(required.scope.value),
                            "invocationPattern": required.invocation_pattern,
                            "sessionPattern": required.session_pattern,
                            "label": required.label,
                        }
                        for required in context.required_permissions
                    ],
                }
            )
        return captured


def build_corpus(reference: Path, expected_commit: str | None) -> dict[str, Any]:
    pin = resolve_reference(reference, expected_commit)
    extraction = capture_extraction(reference)
    sets = capture_command_sets(reference)
    outside = capture_outside_dirs(reference)
    resolutions = capture_resolutions(reference)
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": pin,
        "note": (
            "Captured from the pinned reference by "
            "scripts/parity/shell_policy.py. Command names, node-kind names, "
            "scope values, booleans and the answers to cases this repository "
            "authored are observations; no reference-authored description or "
            "refusal text is recorded here."
        ),
        "counts": {
            "extractionCases": len(extraction),
            "outsideDirCases": len(outside),
            "resolutionCases": len(resolutions),
            "pathCommands": len(sets["pathCommands"]),
        },
        "commandSets": sets,
        "extraction": extraction,
        "outsideDirs": outside,
        "resolutions": resolutions,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument(
        "--interpreter",
        type=Path,
        default=None,
        help="Python that can import `vibe`; also read from " + INTERPRETER_VARIABLE,
    )
    parser.add_argument(
        "--allow-unpinned",
        action="store_true",
        help="capture from a checkout at another revision, for a re-pin",
    )
    arguments = parser.parse_args()

    try:
        reexecute_with_reference_interpreter(arguments.reference, arguments.interpreter)
        corpus = build_corpus(
            arguments.reference,
            None if arguments.allow_unpinned else EXPECTED_COMMIT,
        )
    except OracleError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(
        json.dumps(corpus, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    counts = corpus["counts"]
    print(
        f"wrote {arguments.output} "
        f"({counts['extractionCases']} extraction cases, "
        f"{counts['outsideDirCases']} outside-directory cases, "
        f"{counts['resolutionCases']} resolution cases, "
        f"{counts['pathCommands']} path commands)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
