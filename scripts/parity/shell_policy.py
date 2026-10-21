#!/usr/bin/env python3
"""Capture the shell policy the pinned Python reference resolves.

The reference checkout is a read-only behavioral oracle. This script asks it
the questions its policy ports:

* what ``_extract_commands`` returns for a fixed command list, which is the
  bash-grammar extraction the hand-rolled tokenizer in this port replaces;
* which commands have their path operands inspected (``_PATH_COMMANDS``), and
  which predicates make ``find`` an execution rather than a read;
* what ``_collect_outside_paths`` answers for operands pointing inside the
  workdir, outside it, into the scratchpad and through a symlink;
* what ``BashTool.resolve_permission`` decides for a fixed command list, down
  to the scope, the two patterns and the label of every requirement it raises;
* what the same resolver decides for git commands run inside repositories whose
  configuration can start a program, one fixture repository per setting;
* what the managed resolver (``ExperimentalBash``) decides once a call carries
  its own cwd, shell or environment;
* what both resolvers decide once the shell's allowlist carries path grants:
  the encoded grants a permanent approval writes there and the absolute path
  globs an operator may write, beside command prefixes that must never read as
  one, and what ``path_grant_pattern_matches`` answers for each kind of entry;
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

The wrapper re-executes itself with the reference interpreter, importing the
pinned commit extracted with ``git archive`` (``scripts/parity/reference.py``).
"""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import sys
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT
from reference import INTERPRETER_VARIABLE, OracleError, reexecute_pinned
from shell_policy_cases import (
    ALLOWLIST_GRANT_CASES,
    EXTRACTION_CASES,
    FIND_PRIMARY_CANDIDATES,
    MANAGED_CASES,
    OUTSIDE_DIR_CASES,
    PATH_GRANT_MATCH_CASES,
    REPOSITORY_COMMANDS,
    REPOSITORY_FIXTURES,
    RESOLUTION_CASES,
    STDIN_PERMISSION_CASES,
    WINDOWS_EXPANSION_CASES,
    WINDOWS_EXPANSION_ENVIRONMENT,
    WINDOWS_GRAMMAR_CASES,
    WINDOWS_PATTERN_CASES,
    WINDOWS_TOKEN_CASES,
)

SCHEMA_VERSION = 4
DEFAULT_OUTPUT = Path("crates/vibe-core/tests/shell-policy/policy.json")


# --------------------------------------------------------------------------
# Capture
# --------------------------------------------------------------------------


def capture_extraction() -> list[dict[str, Any]]:
    """What ``_extract_commands`` returns for the authored cases."""
    from vibe.core.tools.builtins.bash import _extract_commands

    return [
        {"command": command, "segments": list(_extract_commands(command))}
        for command in EXTRACTION_CASES
    ]


def capture_command_sets() -> dict[str, Any]:
    """The sets that decide which commands have their operands inspected."""
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


def _bash_tool(
    workdir: Path,
    scratchpad: Path,
    allowlist: list[str] | None = None,
) -> Any:
    from vibe.core.config.harness_files import HarnessFilesManager
    from vibe.core.tools.builtins.bash import Bash, BashToolConfig

    config = BashToolConfig()
    if allowlist is not None:
        config.allowlist[:] = allowlist
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


def capture_outside_paths() -> list[dict[str, Any]]:
    """What ``_collect_outside_paths`` answers for the authored operands.

    The reference keeps ``_collect_outside_dirs`` beside it only for its own
    tests; ``resolve_permission`` reads this one.
    """
    import tempfile

    from vibe.core.tools.builtins.bash import _collect_outside_paths, _extract_commands
    from vibe.core.workspace import Workspace

    with tempfile.TemporaryDirectory() as root:
        workdir, outside, scratchpad = _operand_tree(Path(root))
        placeholders = _placeholders(workdir, outside, scratchpad)

        captured = []
        for case in OUTSIDE_DIR_CASES:
            command = _expand(case, placeholders)
            paths = _collect_outside_paths(
                _extract_commands(command),
                workspace=Workspace.for_session(workdir, [workdir]),
                scratchpad_dir=scratchpad,
            )
            captured.append(
                {
                    "command": case,
                    "paths": sorted(_normalize(entry, placeholders) for entry in paths),
                }
            )
        return captured


def _operand_tree(root: Path) -> tuple[Path, Path, Path]:
    """The workdir, outside and scratchpad directories every case resolves in.

    Two directory links lead somewhere else: ``<workdir>/escape`` out of the
    workspace, and ``<outside>/approved/link`` out of a directory a recursive
    grant may name.
    """
    workdir = root / "workspace"
    outside = root / "elsewhere"
    scratchpad = workdir / ".vibe" / "scratchpad"
    (outside / "nested").mkdir(parents=True)
    (outside / "approved").mkdir(parents=True)
    (workdir / "nested").mkdir(parents=True)
    scratchpad.mkdir(parents=True)
    (workdir / "inside.txt").write_text("inside", encoding="utf-8", newline="")
    (outside / "secret.txt").write_text("secret", encoding="utf-8", newline="")
    (outside / "nested" / "secret.txt").write_text("secret", encoding="utf-8", newline="")
    (scratchpad / "note.txt").write_text("note", encoding="utf-8", newline="")
    (workdir / "escape").symlink_to(outside, target_is_directory=True)
    (outside / "approved" / "link").symlink_to(
        outside / "nested", target_is_directory=True
    )
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
                "pathScopeRoot": None
                if required.path_scope_root is None
                else _normalize(required.path_scope_root, placeholders),
            }
            for required in context.required_permissions
        ],
    }


def capture_resolutions() -> list[dict[str, Any]]:
    """What ``BashTool.resolve_permission`` decides for the authored commands."""
    import tempfile

    from vibe.core.tools.builtins.bash import BashArgs

    with tempfile.TemporaryDirectory() as root:
        workdir, outside, scratchpad = _operand_tree(Path(root))
        placeholders = _placeholders(workdir, outside, scratchpad)
        tool = _bash_tool(workdir, scratchpad)
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


def capture_repository_resolutions() -> list[dict[str, Any]]:
    """What the git readers resolve to in each repository fixture."""
    import tempfile

    from vibe.core.tools.builtins.bash import BashArgs

    captured = []
    for fixture, config in REPOSITORY_FIXTURES:
        with tempfile.TemporaryDirectory() as root:
            workdir, outside, scratchpad = _operand_tree(Path(root))
            (workdir / ".git").mkdir()
            (workdir / ".git" / "config").write_text(config, encoding="utf-8", newline="")
            placeholders = _placeholders(workdir, outside, scratchpad)
            tool = _bash_tool(workdir, scratchpad)
            for command in REPOSITORY_COMMANDS:
                resolved = tool.resolve_permission(
                    BashArgs(command=_expand(command, placeholders))
                )
                captured.append(
                    {"fixture": fixture, **_resolution(command, resolved, placeholders)}
                )
    return captured


def capture_managed_resolutions() -> list[dict[str, Any]]:
    """What the managed resolver decides for the authored calls."""
    import tempfile

    from vibe.core.tools.builtins.experimental_bash import ExperimentalBashArgs

    with tempfile.TemporaryDirectory() as root:
        workdir, outside, scratchpad = _operand_tree(Path(root))
        placeholders = _placeholders(workdir, outside, scratchpad)
        tool = _managed_tool(workdir, scratchpad)
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


def _managed_tool(
    workdir: Path,
    scratchpad: Path,
    allowlist: list[str] | None = None,
) -> Any:
    from vibe.core.config.harness_files import HarnessFilesManager
    from vibe.core.tools.builtins.experimental_bash import (
        ExperimentalBash,
        ExperimentalBashToolConfig,
    )

    config = ExperimentalBashToolConfig()
    if allowlist is not None:
        config.allowlist[:] = allowlist
    return ExperimentalBash(
        lambda: config,
        None,
        cwd=workdir,
        harness_files=HarnessFilesManager(sources=(), cwd=workdir),
        scratchpad_dir=scratchpad,
    )


def capture_allowlist_grants() -> list[dict[str, Any]]:
    """What both resolvers decide with path grants in the shell's allowlist."""
    import tempfile

    from vibe.core.tools.builtins.bash import BashArgs
    from vibe.core.tools.builtins.experimental_bash import ExperimentalBashArgs

    captured = []
    with tempfile.TemporaryDirectory() as root:
        workdir, outside, scratchpad = _operand_tree(Path(root))
        placeholders = _placeholders(workdir, outside, scratchpad)
        for resolver, entries, command, cwd in ALLOWLIST_GRANT_CASES:
            allowlist = [_expand(entry, placeholders) for entry in entries]
            expanded = _expand(command, placeholders)
            if resolver == "legacy":
                tool = _bash_tool(workdir, scratchpad, allowlist)
                resolved = tool.resolve_permission(BashArgs(command=expanded))
            else:
                tool = _managed_tool(workdir, scratchpad, allowlist)
                resolved = tool.resolve_permission(
                    ExperimentalBashArgs(
                        command=expanded,
                        cwd=_expand(cwd, placeholders) if cwd is not None else None,
                    )
                )
            captured.append(
                {
                    "resolver": resolver,
                    "allowlist": list(entries),
                    "cwd": cwd,
                    **_resolution(command, resolved, placeholders),
                }
            )
    return captured


def capture_path_grant_matches() -> list[dict[str, Any]]:
    """What ``path_grant_pattern_matches`` answers for each authored pair."""
    from vibe.permissions import path_grant_pattern_matches

    return [
        {
            "path": path,
            "pattern": pattern,
            "matches": path_grant_pattern_matches(path, pattern),
        }
        for path, pattern in PATH_GRANT_MATCH_CASES
    ]


def capture_stdin_permissions() -> list[dict[str, Any]]:
    """What ``BashStdin.resolve_permission`` decides for each session command."""
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


def capture_windows_grammar() -> dict[str, Any]:
    """What the PowerShell grammar helpers answer for the authored cases."""
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


def build_corpus(commit: str) -> dict[str, Any]:
    pin = {"commit": commit}
    extraction = capture_extraction()
    sets = capture_command_sets()
    outside = capture_outside_paths()
    resolutions = capture_resolutions()
    repository = capture_repository_resolutions()
    managed = capture_managed_resolutions()
    windows = capture_windows_grammar()
    stdin = capture_stdin_permissions()
    grants = capture_allowlist_grants()
    matches = capture_path_grant_matches()
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
            "outsidePathCases": len(outside),
            "resolutionCases": len(resolutions),
            "repositoryCases": len(repository),
            "managedCases": len(managed),
            "windowsGrammarCases": len(windows["commands"]),
            "stdinPermissionCases": len(stdin),
            "allowlistGrantCases": len(grants),
            "pathGrantMatchCases": len(matches),
            "pathCommands": len(sets["pathCommands"]),
        },
        "commandSets": sets,
        "extraction": extraction,
        "outsidePaths": outside,
        "resolutions": resolutions,
        "repositoryResolutions": repository,
        "managedResolutions": managed,
        "windowsGrammar": windows,
        "stdinPermissions": stdin,
        "allowlistGrants": grants,
        "pathGrantMatches": matches,
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
        commit, _ = reexecute_pinned(
            arguments.reference,
            None if arguments.allow_unpinned else EXPECTED_COMMIT,
            override=arguments.interpreter,
        )
        corpus = build_corpus(commit)
    except OracleError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(
        json.dumps(corpus, indent=2, sort_keys=True) + "\n", encoding="utf-8", newline=""
    )
    counts = corpus["counts"]
    print(
        f"wrote {arguments.output} "
        f"({counts['extractionCases']} extraction cases, "
        f"{counts['outsidePathCases']} outside-path cases, "
        f"{counts['resolutionCases']} resolution cases, "
        f"{counts['repositoryCases']} repository cases, "
        f"{counts['managedCases']} managed cases, "
        f"{counts['stdinPermissionCases']} stdin cases, "
        f"{counts['allowlistGrantCases']} allowlist grant cases, "
        f"{counts['pathGrantMatchCases']} path grant verdicts, "
        f"{counts['pathCommands']} path commands)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
