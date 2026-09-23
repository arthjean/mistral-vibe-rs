#!/usr/bin/env python3
"""Capture the shell policy the pinned Python reference resolves.

The reference checkout is a read-only behavioral oracle. This script asks it
the questions its policy ports:

* what ``_extract_commands`` returns for a fixed command list, which is the
  bash-grammar extraction the hand-rolled tokenizer in this port replaces;
* which commands have their path operands inspected (``_PATH_COMMANDS``), and
  which predicates make ``find`` an execution rather than a read;
* what ``_collect_outside_dirs`` answers for operands pointing inside the
  workdir, outside it, and into the scratchpad;
* what ``BashTool.resolve_permission`` decides for a fixed command list, down
  to the scope, the two patterns and the label of every requirement it raises;
* what the same resolver decides for git commands run inside repositories whose
  configuration can start a program, one fixture repository per setting;
* what the managed resolver (``ExperimentalBash``) decides once a call carries
  its own cwd, shell or environment;
* what ``BashStdin.resolve_permission`` decides for input to a session, given
  the command the session runs;
* what the PowerShell grammar helpers of ``windows_shell.py`` answer: part
  splitting, tokens, match forms, policy patterns, path expansion and option
  detection, all pure string functions a POSIX host evaluates.

Since 2.25.4 the reference keeps its ``find`` gate as a set local to
``_find_policy`` (``vibe/core/tools/builtins/_shell_command_policy.py``), which
nothing can import. The predicates are therefore measured rather than read:
every GNU ``find`` action, plus the ``-files0-from`` option, is offered to
``analyze_shell_command_policy`` and the ones it gates are recorded.

The corpus is committed, like the permission-vocabulary one: it records command
names, node-kind names, booleans and the answers to cases this repository
authored. A label survives verbatim only when it is one of its requirement's two
patterns, which is the command text the case itself carries; every other label
(the outside-workdir label, and the exact-command label that names the syntax
requiring approval) is reference-authored text and is committed as
``{"described": "sha256:...", "length": n}``, which still fails the replay on
any change. No reference-authored prose is recorded, which is what ``NOTICE``
forbids shipping.

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
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT

SCHEMA_VERSION = 3
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
    # Words the bash grammar reads literally and zsh expands, which the
    # reference drops from a command's words.
    "=ls -la",
    "ls ~root",
    "echo a==b",
    "ls ***/x",
    # ANSI-C quoting, which the reference keeps in the words for the guardrails.
    "find . $'-exec' rm {} ;",
    "echo $'a\\tb'",
    "git $SUB",
    "echo $HOME ${HOME}",
    "ls {a,b}",
    "echo $((1+2))",
    "diff <(ls) <(ls)",
    "f() { ls; }",
    "ls >/dev/null 2>&1",
    "ls >&2",
    "cat <<< text",
    "[[ -n $FOO ]]",
    "x=1",
    "ls \\\n -la",
    "eval 'sort -o x y'",
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
    # The per-program guardrails: an option that writes, runs or sets
    # something withholds the grant the allowlist gives, and each negative
    # control stays granted.
    "sort -o out.txt in.txt",
    "sort in.txt",
    "sort --random-source=/etc/hosts in.txt",
    "date -s 10:00",
    "date 0101120025",
    "date +%s",
    "less -k keys notes.txt",
    "less notes.txt",
    "more +:e notes.txt",
    "uniq a.txt b.txt",
    "uniq a.txt",
    "tree -o out.txt",
    "tree -L 2",
    "md5sum -c sums.txt",
    "md5sum notes.txt",
    "wc --files0-from=list.txt",
    "du -sh .",
    "du --files0-from=list.txt",
    "file -z archive.gz",
    "file notes.txt",
    "grep -f /etc/hosts notes.txt",
    "diff -X /etc/hosts a.txt b.txt",
    "find . -delete",
    "find . -fprint out.txt",
    "git log --ext-diff",
    "git log --output=out.txt",
    "git log",
    "git diff --diff-merges=remerge",
    "git status && sort -o x y && sort -o x y",
    "sudo ls && sudo ls",
    # What a wrapper runs is checked as if it ran directly.
    "eval git log --ext-diff",
    "eval 'sort -o x y'",
    "exec sort -o x y",
    "exec -a name sort -o x y",
    "exec",
    "eval vim notes.txt",
    "exec -- vim notes.txt",
    "eval python3",
    # The directory a git reader runs in, followed through `cd`, `pushd` and
    # `popd`, and given up on when it cannot be known.
    "cd <outside> && git status",
    "cd $X && git status",
    "pushd <outside> && popd && git status",
    "popd +1 && git status",
    "cd -- <outside> && git status",
    # Syntax that needs approval: where the words still name the command, the
    # command pattern carries the approval; where they do not, only the text
    # as written can be approved.
    "git $SUB",
    "sudo $CMD",
    "git log $REF",
    "echo $HOME",
    "ls {a,b}",
    "HOME=/x git status",
    "ls &",
    "( ls )",
    "ls >/dev/null 2>&1",
    "ls >&2",
    "ls >&file",
    "cat <<< text",
    "echo $'a'",
    "=ls",
    "ls ~root",
    "f() { ls; }",
    "ls \\\n -la",
    "[[ -n $FOO ]]",
    "env FOO=1 ls",
    ". ./script.sh",
    "npm run $TASK",
    "npm run build $ARGS",
    "cargo $CMD",
    "docker compose up $SVC",
    'echo "$(date)"',
    "cat <outside>/secret.txt $X",
)

#: Repository configurations a git reader is resolved in, each a `.git/config`
#: written into the workdir. The reference reads them without invoking git, and
#: an executable helper withholds the grant `git status` would otherwise get.
REPOSITORY_FIXTURES: tuple[tuple[str, str], ...] = (
    ("plain", "[core]\n\tbare = false\n"),
    ("pager", "[core]\n\tpager = less\n"),
    ("pager-off", "[core]\n\tpager = off\n"),
    ("include", "[include]\n\tpath = other.config\n"),
    ("log-pager", "[pager]\n\tlog = cat\n"),
    ("fsmonitor", "[core]\n\tfsmonitor\n"),
    ("diff-driver", '[diff "x"]\n\ttextconv = cat\n'),
    ("gpg", "[gpg]\n\tprogram = gpg2\n"),
)

#: The git readers resolved in every repository fixture.
REPOSITORY_COMMANDS: tuple[str, ...] = (
    "git status",
    "git diff",
    "git log",
    "cd nested && git log",
    "cd <outside> && git status",
)

#: Calls the managed resolver answers, as `(command, cwd, shell, env names)`:
#: a `cwd` is where operands resolve and is itself an outside directory when it
#: leaves the workspace, the shell and environment overrides carry their own
#: requirements, and the denylist also matches a program's basename.
MANAGED_CASES: tuple[tuple[str, str | None, str | None, tuple[str, ...]], ...] = (
    ("pwd", None, None, ()),
    ("pwd", "<outside>", None, ()),
    ("pwd", "nested", None, ()),
    ("pwd", "<scratchpad>", None, ()),
    ("cat secret.txt", "<outside>", None, ()),
    ("cat nested/secret.txt", "<outside>", None, ()),
    ("cat ../elsewhere/secret.txt", "<workdir>", None, ()),
    ("pwd", None, "/bin/zsh", ()),
    ("pwd", None, None, ("B", "A")),
    ("rm -rf build", None, "/bin/zsh", ("X",)),
    ("# only a comment", None, "/bin/zsh", ()),
    ("/usr/bin/vim notes.txt", None, None, ()),
    ("/usr/bin/python3", None, None, ()),
    ("git status", "<outside>", None, ()),
    ("ls $X", None, "/bin/zsh", ()),
    ("sudo ls", "<outside>", None, ("A",)),
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

#: Every action GNU findutils documents (``man find``, ACTIONS), plus the
#: ``-files0-from`` option, offered one at a time to the reference ``find``
#: policy. The ones it gates are what ``findExecutionPredicates`` records; the
#: rest (``-print``, ``-prune`` and the like) are the negative controls.
FIND_PRIMARY_CANDIDATES: tuple[str, ...] = (
    "-delete",
    "-exec",
    "-execdir",
    "-files0-from",
    "-fls",
    "-fprint",
    "-fprint0",
    "-fprintf",
    "-ls",
    "-ok",
    "-okdir",
    "-print",
    "-print0",
    "-printf",
    "-prune",
    "-quit",
)


#: PowerShell command strings whose grammar is recorded: the parts a command
#: splits into, each part's tokens and the forms the lists match it under, and
#: the output redirections that can write a file. The helpers are pure string
#: functions, so a POSIX host answers for them as a Windows host would.
WINDOWS_GRAMMAR_CASES: tuple[str, ...] = (
    "",
    "Get-ChildItem",
    "dir C:\\work && type notes.txt",
    "ls; cat a.txt | sls needle",
    "echo a & echo b",
    "cmd 2>&1 | more",
    "Get-Content x *>&1",
    "Write-Output $(Get-Date)",
    "Write-Output \"$(Get-Date) and $(whoami)\"",
    "Invoke-Command { Remove-Item x; ls }",
    "(Get-Item a).Name",
    "echo '$(not a subexpression)'",
    "echo `& not a separator",
    "echo a ^& b",
    "& 'C:\\Program Files\\tool.exe' -Flag",
    "& C:\\tools\\rm.exe -rf x",
    "C:\\tools\\RM.EXE x",
    "rm.exe x",
    "Remove-Item x",
    "gci -Recurse",
    "type > out.txt",
    "echo a > $null",
    "echo a > NUL",
    "echo a 2> 'err log.txt'",
    "echo a >> C:\\logs\\x.txt",
    "echo a >&2",
    "echo 'a > b'",
    "echo a >`\"q`\" x",
    "notepad",
    "powershell -NoExit",
    "git status\r\ngit log",
    "@{a=1}; ${x}",
    "echo ${env:PATH}",
)

#: Policy patterns whose match forms are recorded.
WINDOWS_PATTERN_CASES: tuple[str, ...] = (
    "rm",
    "Remove-Item",
    "cmd /k",
    "powershell -NoExit",
    "ls -la",
    "C:\\tools\\rm.exe",
    "git status",
    "",
)

#: Tokens whose PowerShell expansion is recorded, under a fixed environment and
#: a fixed working directory.
WINDOWS_EXPANSION_CASES: tuple[str, ...] = (
    "$HOME\\notes.txt",
    "$env:APPDATA\\x",
    "${env:APPDATA}\\x",
    "$Env:appdata\\x",
    "$PWD\\sub",
    "$pwd",
    "$other\\x",
    "${global:x}",
    "$env:MISSING\\x",
    "'C:\\quoted path'",
    "@(1)",
    "cost$",
    "plain.txt",
)

#: The environment the expansion cases resolve against.
WINDOWS_EXPANSION_ENVIRONMENT: dict[str, str] = {
    "USERPROFILE": "C:\\Users\\me",
    "AppData": "C:\\Users\\me\\AppData\\Roaming",
}

#: Tokens whose option, path and attached-value shape is recorded.
WINDOWS_TOKEN_CASES: tuple[str, ...] = (
    "-Path",
    "-Path:C:\\x",
    "-Path=C:\\x",
    "/s",
    "/s:x",
    "//server",
    "/c/x",
    "C:\\x",
    "C:",
    "x:y",
    "\\\\server\\share",
    "~\\x",
    ".\\x",
    "notes.txt",
    "'C:\\quoted'",
    "-",
)


#: Session commands the `*_stdin` permission is resolved against, per shell
#: family: input to a session that may be a pager asks, anything else defers.
#: `None` stands for a session the family does not know.
STDIN_PERMISSION_CASES: tuple[tuple[str, str | None], ...] = (
    ("posix", "git log"),
    ("posix", "less notes.txt"),
    ("posix", "more notes.txt"),
    ("posix", "cat notes.txt"),
    ("posix", "sleep 30; git log"),
    ("posix", "eval git log"),
    ("posix", "exec less notes.txt"),
    ("posix", "/usr/bin/GIT.exe log"),
    ("posix", "echo git"),
    ("posix", "gitk"),
    ("posix", "(git log)"),
    ("posix", "cat $(which less)"),
    ("posix", None),
    ("git_bash", "git log"),
    ("git_bash", "cat notes.txt"),
    ("powershell", "git log"),
    ("powershell", "& 'C:\\tools\\git.exe' log"),
    ("powershell", "type a | more"),
    ("powershell", "Get-Content a"),
    ("powershell", "less.exe x"),
    ("powershell", None),
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
    from vibe.core.tools.builtins._shell_command_policy import (
        analyze_shell_command_policy,
    )
    from vibe.core.tools.builtins.bash import (
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
        "findExecutionPredicates": sorted(
            candidate
            for candidate in FIND_PRIMARY_CANDIDATES
            if analyze_shell_command_policy(["find", ".", candidate]).requires_approval
        ),
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
    from vibe.core.workspace import Workspace

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
                workspace=Workspace.for_session(workdir, [workdir]),
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


def _operand_tree(root: Path) -> tuple[Path, Path, Path]:
    """The workdir, outside and scratchpad directories every case resolves in."""
    workdir = root / "workspace"
    outside = root / "elsewhere"
    scratchpad = workdir / ".vibe" / "scratchpad"
    (outside / "nested").mkdir(parents=True)
    (workdir / "nested").mkdir(parents=True)
    scratchpad.mkdir(parents=True)
    (workdir / "inside.txt").write_text("inside", encoding="utf-8")
    (outside / "secret.txt").write_text("secret", encoding="utf-8")
    (outside / "nested" / "secret.txt").write_text("secret", encoding="utf-8")
    (scratchpad / "note.txt").write_text("note", encoding="utf-8")
    return workdir, outside, scratchpad


def _resolution(
    command: str, context: Any, placeholders: list[tuple[str, str]]
) -> dict[str, Any]:
    """One resolution, its host paths recorded under their placeholders."""
    if context is None:
        return {"command": command, "permission": None}
    return {
        "command": command,
        "permission": str(context.permission.value),
        # The reason is reference prose, so only whether one exists is
        # recorded. This port writes its own refusal text.
        "hasReason": context.reason is not None,
        "requirements": [
            {
                "scope": str(required.scope.value),
                "invocationPattern": _normalize(
                    required.invocation_pattern, placeholders
                ),
                "sessionPattern": _normalize(required.session_pattern, placeholders),
                "label": committed_label(required, placeholders),
            }
            for required in context.required_permissions
        ],
    }


def capture_resolutions(reference: Path) -> list[dict[str, Any]]:
    """What ``BashTool.resolve_permission`` decides for the authored commands."""
    sys.path.insert(0, str(reference))
    import tempfile

    from vibe.core.tools.builtins.bash import BashArgs

    with tempfile.TemporaryDirectory() as root:
        workdir, outside, scratchpad = _operand_tree(Path(root))
        placeholders = _placeholders(workdir, outside, scratchpad)
        tool = _bash_tool(reference, workdir, scratchpad)
        return [
            _resolution(
                command,
                tool.resolve_permission(
                    BashArgs(command=_expand(command, placeholders))
                ),
                placeholders,
            )
            for command in RESOLUTION_CASES
        ]


def capture_repository_resolutions(reference: Path) -> list[dict[str, Any]]:
    """What the git readers resolve to in each repository fixture."""
    sys.path.insert(0, str(reference))
    import tempfile

    from vibe.core.tools.builtins.bash import BashArgs

    captured = []
    for fixture, config in REPOSITORY_FIXTURES:
        with tempfile.TemporaryDirectory() as root:
            workdir, outside, scratchpad = _operand_tree(Path(root))
            (workdir / ".git").mkdir()
            (workdir / ".git" / "config").write_text(config, encoding="utf-8")
            placeholders = _placeholders(workdir, outside, scratchpad)
            tool = _bash_tool(reference, workdir, scratchpad)
            for command in REPOSITORY_COMMANDS:
                resolved = tool.resolve_permission(
                    BashArgs(command=_expand(command, placeholders))
                )
                captured.append(
                    {"fixture": fixture, **_resolution(command, resolved, placeholders)}
                )
    return captured


def capture_managed_resolutions(reference: Path) -> list[dict[str, Any]]:
    """What the managed resolver decides for the authored calls."""
    sys.path.insert(0, str(reference))
    import tempfile

    from vibe.core.config.harness_files import HarnessFilesManager
    from vibe.core.tools.builtins.experimental_bash import (
        ExperimentalBash,
        ExperimentalBashArgs,
        ExperimentalBashToolConfig,
    )

    with tempfile.TemporaryDirectory() as root:
        workdir, outside, scratchpad = _operand_tree(Path(root))
        placeholders = _placeholders(workdir, outside, scratchpad)
        config = ExperimentalBashToolConfig()
        tool = ExperimentalBash(
            lambda: config,
            None,
            cwd=workdir,
            harness_files=HarnessFilesManager(sources=(), cwd=workdir),
            scratchpad_dir=scratchpad,
        )
        captured = []
        for command, cwd, shell, env in MANAGED_CASES:
            arguments = ExperimentalBashArgs(
                command=command,
                cwd=_expand(cwd, placeholders) if cwd is not None else None,
                shell=shell,
                env={name: "1" for name in env} or None,
            )
            captured.append(
                {
                    "cwd": cwd,
                    "shell": shell,
                    "env": list(env),
                    **_resolution(
                        command, tool.resolve_permission(arguments), placeholders
                    ),
                }
            )
        return captured


def capture_stdin_permissions(reference: Path) -> list[dict[str, Any]]:
    """What ``BashStdin.resolve_permission`` decides for each session command."""
    sys.path.insert(0, str(reference))
    from types import SimpleNamespace

    from vibe.core.tools.builtins.experimental_bash import (
        BashStdin,
        BashStdinArgs,
        ManagedShellError,
    )

    def manager_for(command: str | None) -> Any:
        def info(_session_id: str) -> Any:
            if command is None:
                raise ManagedShellError("unknown session")
            return SimpleNamespace(command=command)

        return SimpleNamespace(info=info)

    captured = []
    for family, command in STDIN_PERMISSION_CASES:
        tool = SimpleNamespace(
            shell_family=family,
            _PAGER_SESSION_COMMANDS=BashStdin._PAGER_SESSION_COMMANDS,
            _pager_input_permission=BashStdin._pager_input_permission,
            _session_manager=lambda command=command: manager_for(command),
        )
        context = BashStdin.resolve_permission(
            tool, BashStdinArgs(session_id="session_1", text="q")
        )
        captured.append(
            {"family": family, **_resolution(command or "", context, [])}
            | {"known": command is not None}
        )
    return captured


def capture_windows_grammar(reference: Path) -> dict[str, Any]:
    """What the PowerShell grammar helpers answer for the authored cases."""
    sys.path.insert(0, str(reference))
    from vibe.core.tools.builtins import windows_shell as windows

    commands = []
    for command in WINDOWS_GRAMMAR_CASES:
        parts = windows._split_windows_command_parts(command)
        commands.append(
            {
                "command": command,
                "parts": [
                    {
                        "part": part,
                        "tokens": windows._split_windows_command_tokens(part),
                        "forms": windows._windows_command_match_forms(part),
                        "formsWithoutBasename": windows._windows_command_match_forms(
                            part, include_path_basename=False
                        ),
                        "commandName": windows._windows_command_name(
                            windows._windows_invoked_command(
                                windows._split_windows_command_tokens(part)
                            )[0]
                        )
                        if windows._split_windows_command_tokens(part)
                        else None,
                        "fileRedirections": windows._windows_file_redirection_targets(
                            part
                        ),
                    }
                    for part in parts
                ],
            }
        )
    home = str(Path.home())
    expansions = []
    for token in WINDOWS_EXPANSION_CASES:
        value, unresolved = windows._expand_windows_powershell_path(
            token,
            command_cwd=Path("C:\\work"),
            environment=dict(WINDOWS_EXPANSION_ENVIRONMENT),
        )
        expansions.append(
            {
                "token": token,
                "value": value.replace(home, "<home>"),
                "unresolved": unresolved,
            }
        )
    return {
        "commands": commands,
        "patterns": [
            {"pattern": pattern, "forms": windows._windows_policy_pattern_forms(pattern)}
            for pattern in WINDOWS_PATTERN_CASES
        ],
        "expansions": expansions,
        "tokens": [
            {
                "token": token,
                "option": windows._windows_looks_like_option(token),
                "path": windows._windows_looks_like_path(token),
                "attachedValue": windows._windows_attached_parameter_value(token),
            }
            for token in WINDOWS_TOKEN_CASES
        ],
    }


def describe(value: str) -> dict[str, Any]:
    """The committable form of a string that may carry reference-authored prose."""
    return {
        "described": "sha256:" + hashlib.sha256(value.encode("utf-8")).hexdigest()[:32],
        "length": len(value),
    }


def committed_label(
    required: Any, placeholders: list[tuple[str, str]]
) -> str | dict[str, Any]:
    """A requirement's label, verbatim only when it is one of its own patterns.

    Host paths are replaced by their placeholders first, so a label naming a
    directory digests the same on every machine.
    """
    label = _normalize(required.label, placeholders)
    patterns = (
        _normalize(required.invocation_pattern, placeholders),
        _normalize(required.session_pattern, placeholders),
    )
    if label in patterns:
        return label
    return describe(label)


def build_corpus(reference: Path, expected_commit: str | None) -> dict[str, Any]:
    pin = resolve_reference(reference, expected_commit)
    extraction = capture_extraction(reference)
    sets = capture_command_sets(reference)
    outside = capture_outside_dirs(reference)
    resolutions = capture_resolutions(reference)
    repository = capture_repository_resolutions(reference)
    managed = capture_managed_resolutions(reference)
    windows = capture_windows_grammar(reference)
    stdin = capture_stdin_permissions(reference)
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": pin,
        "note": (
            "Captured from the pinned reference by "
            "scripts/parity/shell_policy.py. Command names, node-kind names, "
            "scope values, booleans and the answers to cases this repository "
            "authored are observations; a label that is not one of its "
            "requirement's patterns is recorded as a digest and a length, and "
            "no reference-authored description, label or refusal text is "
            "recorded here."
        ),
        "counts": {
            "extractionCases": len(extraction),
            "outsideDirCases": len(outside),
            "resolutionCases": len(resolutions),
            "repositoryCases": len(repository),
            "managedCases": len(managed),
            "windowsGrammarCases": len(windows["commands"]),
            "stdinPermissionCases": len(stdin),
            "pathCommands": len(sets["pathCommands"]),
        },
        "commandSets": sets,
        "extraction": extraction,
        "outsideDirs": outside,
        "resolutions": resolutions,
        "repositoryResolutions": repository,
        "managedResolutions": managed,
        "windowsGrammar": windows,
        "stdinPermissions": stdin,
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
        f"{counts['repositoryCases']} repository cases, "
        f"{counts['managedCases']} managed cases, "
        f"{counts['stdinPermissionCases']} stdin cases, "
        f"{counts['pathCommands']} path commands)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
