#!/usr/bin/env python3
"""Capture the permission vocabulary the pinned Python reference speaks.

The reference checkout is a read-only behavioral oracle. This script asks it
the questions whose answers are the contract EP-032 ports:

* which scopes ``PermissionScope`` and ``PathGrantScope`` declare, and under
  which wire values;
* which fields ``RequiredPermission`` carries, under which aliases, and which
  of them stay off the wire;
* what the arity table holds, entry by entry;
* what ``build_session_pattern`` and ``wildcard_match`` answer for a fixed case
  list, so the two functions are replayed rather than re-read;
* how a path grant is encoded and matched, what ``PermissionStore.covers``
  answers for a stored rule, and what the file-tool chain answers for a path
  outside the working directory and for an allowlist or denylist entry.

The corpus is committed, like the tool-configuration one: it records enum
values, field names, command names, integers and the answers to cases this
repository authored, all of which are observations. No reference-authored prose
is recorded, which is what ``NOTICE`` forbids shipping.

Usage::

    scripts/parity/permission_surface.py --reference /path/to/reference
    scripts/parity/permission_surface.py --interpreter /path/to/python

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

SCHEMA_VERSION = 5
DEFAULT_OUTPUT = Path("crates/vibe-core/tests/permission-surface/vocabulary.json")
INTERPRETER_VARIABLE = "VIBE_PARITY_PYTHON"

#: Token lists whose session pattern is recorded. They cover the three shapes
#: the table produces (a prefix longer than the arity, a prefix equal to it, a
#: first token absent from the table) plus the empty list, which the reference
#: answers with the empty string rather than by raising.
SESSION_PATTERN_CASES: tuple[tuple[str, ...], ...] = (
    (),
    ("ls",),
    ("ls", "-la"),
    ("npm", "run", "build"),
    ("npm", "run"),
    ("npm",),
    ("git", "config", "user.name"),
    ("git", "status"),
    ("cargo", "run", "--release"),
    ("docker", "compose", "up", "-d"),
    ("kubectl", "rollout", "restart", "deployment/api"),
    ("aws", "s3", "ls", "s3://bucket"),
    ("rm", "-rf", "build"),
    ("uv", "run", "pytest"),
    ("yarn", "dlx", "prettier", "--write", "."),
    ("terraform", "workspace", "select", "prod"),
    ("openssl", "x509", "-in", "cert.pem"),
    ("unknown-binary", "--flag", "value"),
    ("./local-script.sh",),
    ("bun", "x", "vite", "build"),
)

#: ``(invocation pattern, session pattern)`` pairs whose verdict is recorded.
#: They cover the plain glob, the trailing-argument form the reference accepts
#: with and without its arguments, and the near misses that must stay refused.
WILDCARD_CASES: tuple[tuple[str, str], ...] = (
    ("npm run build", "npm run *"),
    ("npm run", "npm run *"),
    ("npm", "npm run *"),
    ("npm running", "npm run *"),
    ("git status", "git *"),
    ("git", "git *"),
    ("ls -la", "ls *"),
    ("ls", "ls *"),
    ("lsof", "ls *"),
    ("/workspace/plans/*", "/workspace/plans/*"),
    ("/workspace/plans/notes.md", "/workspace/plans/*"),
    ("/etc/*", "/workspace/plans/*"),
    (".env", "*"),
    ("anything at all", "*"),
    ("example.com", "example.com"),
    ("api.example.com", "example.com"),
    ("", "*"),
    ("", ""),
    ("find . -exec rm {} ;", "find . -exec rm {} ;"),
)


#: ``(pattern, absolute path)`` pairs whose verdict is recorded under the three
#: matchers the reference uses. ``sensitive_patterns`` is matched with
#: ``matches_sensitive_pattern``, which folds case and then runs
#: ``PurePath.match``, right-anchored and component by component
#: (``vibe/core/tools/utils.py:45-48`` at the pin). The allowlist is matched with
#: ``path_pattern_matches`` since v2.25.8 (``vibe/core/tools/utils.py:161-163``),
#: which anchors an absolute glob at the root, and the denylist stays on
#: ``fnmatch``, whose ``*`` crosses a separator and whose match is whole-string.
#: The pairs are chosen so most of them answer differently under two of the
#: three, which is what makes the corpus able to fail a port that runs one
#: matcher for all of them. The last two
#: separate ``**`` from ``*`` at the root, where only the first stands in for a
#: component that is not there, and pin ``**`` to a single component rather than
#: to a run of them.
SENSITIVE_CASES: tuple[tuple[str, str], ...] = (
    ("**/.env", "/srv/app/.env"),
    ("**/.env.*", "/srv/app/.env.local"),
    (".env", "/srv/app/.env"),
    (".env", "/srv/app/.env.local"),
    ("secrets/*", "/srv/app/secrets/token.json"),
    ("secrets", "/srv/app/secrets"),
    ("/etc/*", "/etc/passwd"),
    ("/etc/*", "/etc/ssl/private/key.pem"),
    ("/srv/*/.env", "/srv/app/.env"),
    ("/srv/*/.env", "/srv/a/b/.env"),
    ("app/*/.env", "/srv/app/config/.env"),
    ("*.pem", "/srv/app/certs/server.pem"),
    ("*", "/srv/app/.env"),
    ("**/config/*.json", "/srv/app/config/db.json"),
    ("**/.env", "/.env"),
    ("*/.env", "/.env"),
    ("a/**/b", "/srv/a/x/b"),
    ("a/**/b", "/srv/a/x/y/b"),
    ("", "/srv/app/.env"),
    ("[", "/srv/app/.env"),
)

#: ``(pattern, relative path)`` pairs run through the whole file-tool chain,
#: with the path inside the working directory so the sensitive branch is the
#: only thing that can require a permission. They are the end-to-end half of
#: :data:`SENSITIVE_CASES`: the matcher answers above, the chain answers here.
SENSITIVE_CHAIN_CASES: tuple[tuple[str, str], ...] = (
    ("**/.env", ".env"),
    (".env", "app/.env"),
    ("secrets/*", "app/secrets/token.json"),
    ("secrets/*", "app/secrets/nested/token.json"),
    ("*.pem", "app/certs/server.pem"),
    ("/etc/*", "app/.env"),
)


#: ``(path, scope)`` pairs whose encoded grant is recorded. They cover the
#: POSIX normalization (repeated and trailing separators, ``.`` and ``..``,
#: the empty path), a glob character kept literal, and the Windows paths the
#: reference recognizes by a drive, a leading pair of separators or a
#: backslash, which it folds to lowercase backslashes.
PATH_GRANT_ENCODING_CASES: tuple[tuple[str, str], ...] = (
    ("/srv/app/notes.txt", "exact"),
    ("/srv/app/", "directory_recursive"),
    ("/srv//app/./x/../y", "exact"),
    ("/srv/app/*.txt", "exact"),
    ("relative/./a/..", "exact"),
    ("", "exact"),
    ("..", "exact"),
    ("/..", "exact"),
    ("a/../../b", "exact"),
    ("//lead", "exact"),
    ("///triple", "exact"),
    ("C:\\Users\\Me\\File.txt", "exact"),
    ("c:/users/me", "directory_recursive"),
    ("\\\\server\\share\\X", "exact"),
    ("C:\\a\\..\\..\\b", "exact"),
    ("c:..\\x", "exact"),
    ("\\Windows\\..\\x", "exact"),
)

#: ``(path, pattern)`` pairs whose ``path_pattern_matches`` verdict is
#: recorded: the exact and recursive grants, the separator a recursive grant
#: stops at, malformed encodings, which fall back to the glob reading, the
#: absolute globs it anchors at the root and the relative ones it reads as
#: ``fnmatch`` does, and the same questions over Windows paths, matched
#: without case.
PATH_MATCH_CASES: tuple[tuple[str, str], ...] = (
    ("/srv/app/a.txt", "vibe-path:exact:/srv/app/a.txt"),
    ("/srv/app/b.txt", "vibe-path:exact:/srv/app/a.txt"),
    ("/srv/app/./a.txt", "vibe-path:exact:/srv/app/a.txt"),
    ("/srv/app/a.txt", "vibe-path:exact:/srv/app/./a.txt"),
    ("/srv/app/a.txt", "vibe-path:exact:/srv/app"),
    ("/srv/app", "vibe-path:directory_recursive:/srv/app"),
    ("/srv/app/x/y.txt", "vibe-path:directory_recursive:/srv/app"),
    ("/srv/application/y.txt", "vibe-path:directory_recursive:/srv/app"),
    ("/srv/app/x", "vibe-path:directory_recursive:/srv/app/"),
    ("/srv/app", "vibe-path:directory_recursive:/"),
    ("/srv", "vibe-path:directory_recursive:/srv/app"),
    ("/srv/app/x.txt", "vibe-path:exact:/srv/app/*.txt"),
    ("/srv/app/*.txt", "vibe-path:exact:/srv/app/*.txt"),
    ("/srv/app/a.txt", "vibe-path:EXACT:/srv/app/a.txt"),
    ("/srv/app/a.txt", "vibe-path:exact"),
    ("/srv/app/a.txt", "vibe-path:exact:"),
    ("/x", "vibe-path:directory_recursive:"),
    ("/srv/app/a.txt", "other:exact:/srv/app/a.txt"),
    ("/srv/app/a.txt", "/srv/app/*"),
    ("/srv/app/x/a.txt", "/srv/app/*"),
    ("/srv/app/a.txt", "/srv/*"),
    ("/srv/app/a.txt", "/srv/app/a.txt"),
    ("/srv/app/a.txt", "/srv/app/A.txt"),
    ("/srv/app/a.txt", "/srv/app/a.txt/"),
    ("/srv/app/./a.txt", "/srv/app/*"),
    ("/srv/a/a.txt", "/srv/**/a.txt"),
    ("/srv/a/b/a.txt", "/srv/**/a.txt"),
    ("/srv/a.txt", "/srv/**/a.txt"),
    ("/", "/"),
    ("/srv/app/a.txt", "*.txt"),
    ("/srv/app/a.txt", "app/*"),
    ("/srv/app/a.txt", "*/app/*"),
    ("/srv/app/a.txt", "*"),
    ("/srv/app/a.txt", ""),
    ("/srv/app/a.txt", "["),
    ("relative/a.txt", "relative/*"),
    ("relative/a.txt", "vibe-path:exact:relative/a.txt"),
    ("C:\\Work\\A.txt", "vibe-path:exact:c:/work/a.txt"),
    ("C:\\Work\\Sub\\A.txt", "vibe-path:directory_recursive:C:\\Work"),
    ("C:\\Workshop\\A.txt", "vibe-path:directory_recursive:C:\\Work"),
    ("C:\\Work\\A.txt", "C:\\Work\\*"),
    ("C:\\Work\\Sub\\A.txt", "C:\\Work\\*"),
    ("C:\\Work\\A.txt", "c:/work/*.TXT"),
    ("C:\\Work\\A.txt", "*.txt"),
    ("C:\\Work\\A.txt", "work\\*"),
    ("//srv/a", "//srv/*"),
    ("/srv/a", "vibe-path:exact://srv/a"),
)

#: ``(rule scope, rule pattern, requirement scope, invocation pattern,
#: literal)`` cases whose ``PermissionStore.covers`` verdict is recorded. An
#: outside-directory requirement is matched by path since v2.25.8
#: (``vibe/core/tools/permissions.py:41-42``), so a path glob stops at a
#: separator and ``literal`` does not apply; every other scope keeps the
#: wildcard rule and the literal reading.
COVERS_CASES: tuple[tuple[str, str, str, str, bool], ...] = (
    ("outside_directory", "vibe-path:exact:/srv/app/a.txt", "outside_directory", "/srv/app/a.txt", False),
    ("outside_directory", "vibe-path:exact:/srv/app/a.txt", "outside_directory", "/srv/app/b.txt", False),
    ("outside_directory", "vibe-path:exact:/srv/app/a.txt", "outside_directory", "/srv/app/a.txt", True),
    ("outside_directory", "vibe-path:directory_recursive:/srv/app", "outside_directory", "/srv/app/x/y.txt", False),
    ("outside_directory", "vibe-path:directory_recursive:/srv/app", "outside_directory", "/srv/application/y.txt", False),
    ("outside_directory", "/srv/app/*", "outside_directory", "/srv/app/x", False),
    ("outside_directory", "/srv/app/*", "outside_directory", "/srv/app/x/y", False),
    ("outside_directory", "/srv/app/*", "outside_directory", "/srv/app/*", False),
    ("outside_directory", "vibe-path:exact:/srv/app/a.txt", "command_pattern", "/srv/app/a.txt", False),
    ("command_pattern", "npm run *", "command_pattern", "npm run build", False),
    ("command_pattern", "git log *", "command_pattern", "git log *", True),
    ("command_pattern", "git log *", "command_pattern", "git log -p", True),
    ("command_pattern", "git log *", "command_pattern", "git log -p", False),
    ("file_pattern", "/srv/app/[.]env", "file_pattern", "/srv/app/.env", False),
)

#: ``(list, pattern, path)`` cases run through the whole file-tool chain with
#: the pattern as the tool's only ``allowlist`` or ``denylist`` entry.
#: ``{workdir}`` and ``{outside}`` stand for the working directory and a
#: directory outside it, which the corpus records under those names. They pin
#: that the allowlist reads the encoded grant a permanent approval writes there
#: and anchors an absolute glob, while the denylist keeps ``fnmatch``.
LIST_CHAIN_CASES: tuple[tuple[str, str, str], ...] = (
    ("allowlist", "{workdir}/notes/*", "notes/a.txt"),
    ("allowlist", "{workdir}/notes/*", "notes/sub/a.txt"),
    ("denylist", "{workdir}/notes/*", "notes/sub/a.txt"),
    ("allowlist", "vibe-path:exact:{workdir}/notes/a.txt", "notes/a.txt"),
    ("allowlist", "vibe-path:exact:{workdir}/notes/a.txt", "notes/b.txt"),
    ("allowlist", "vibe-path:directory_recursive:{workdir}/notes", "notes/sub/a.txt"),
    ("allowlist", "vibe-path:directory_recursive:{workdir}/no", "notes/a.txt"),
    ("denylist", "vibe-path:exact:{workdir}/notes/a.txt", "notes/a.txt"),
    ("allowlist", "*.txt", "notes/a.txt"),
    ("allowlist", "notes/*", "notes/a.txt"),
    ("allowlist", "vibe-path:exact:{outside}/secret.txt", "{outside}/secret.txt"),
    ("allowlist", "vibe-path:exact:{outside}/secret.txt", "{outside}/other.txt"),
)

#: The targets outside the working directory the chain is asked about: a file
#: that does not exist, one that does, and a directory, the one shape that
#: offers a recursive grant root.
OUTSIDE_TARGETS: tuple[str, ...] = ("missing-file", "file", "directory")


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


def capture_vocabulary(reference: Path) -> dict[str, Any]:
    """The scope values and the requirement fields, with their wire aliases.

    A field the model excludes from serialization is recorded with
    ``excluded`` set, since it is declared on the model and read on input but
    never crosses the wire: ``literal`` at ``vibe/permissions.py:27``.
    """
    sys.path.insert(0, str(reference))
    from vibe.permissions import PathGrantScope, PermissionScope, RequiredPermission

    scopes = [str(member.value) for member in PermissionScope]
    if not scopes:
        raise OracleError("the reference declares no permission scope")
    path_grant_scopes = [str(member.value) for member in PathGrantScope]
    fields = [
        {
            "name": name,
            "alias": field.alias or name,
            "required": field.is_required(),
            "excluded": bool(field.exclude),
        }
        for name, field in RequiredPermission.model_fields.items()
    ]
    configuration = RequiredPermission.model_config
    return {
        "scopes": scopes,
        "pathGrantScopes": path_grant_scopes,
        "requirement": {
            "fields": fields,
            "forbidsExtra": configuration.get("extra") == "forbid",
        },
    }


def capture_arity(reference: Path) -> dict[str, int]:
    """The arity table, keyed by the command prefix it answers for."""
    sys.path.insert(0, str(reference))
    from vibe.core.tools.arity import ARITY

    if not ARITY:
        raise OracleError("the reference arity table is empty")
    return {prefix: int(ARITY[prefix]) for prefix in sorted(ARITY)}


def capture_session_patterns(reference: Path) -> list[dict[str, Any]]:
    """What ``build_session_pattern`` answers for the recorded cases."""
    sys.path.insert(0, str(reference))
    from vibe.core.tools.arity import build_session_pattern

    return [
        {"tokens": list(tokens), "pattern": build_session_pattern(list(tokens))}
        for tokens in SESSION_PATTERN_CASES
    ]


def capture_wildcard_matches(reference: Path) -> list[dict[str, Any]]:
    """What ``wildcard_match`` answers for the recorded cases."""
    sys.path.insert(0, str(reference))
    from vibe.core.tools.permissions import wildcard_match

    return [
        {"text": text, "pattern": pattern, "matches": bool(wildcard_match(text, pattern))}
        for text, pattern in WILDCARD_CASES
    ]


def capture_file_tool_labels(reference: Path) -> dict[str, Any]:
    """The requirements the shared file-tool chain composes.

    They are recorded as format strings this repository re-derives rather than
    as reference prose: each is a fixed word joined to a value the call itself
    carries, which is an observation of the shape and not of any authored text.
    The sensitive requirement names the resolved file and its ``glob.escape``
    form (``vibe/core/tools/utils.py:217-227`` at the pin), so the temporary
    working directory is recorded as ``<workdir>``. The outside requirement names
    the resolved path itself since v2.25.8 (``vibe/core/tools/utils.py:230-242``
    at the pin) rather than a glob over its parent, grants it by its exact
    encoded path and names the directory a recursive grant would reach, so the
    temporary outside directory is recorded as ``<outside>`` whatever the
    requirement builds on it. One case is asked per target shape, each under a
    directory of its own.
    """
    sys.path.insert(0, str(reference))
    import glob
    import tempfile

    from vibe.core.tools.base import ToolPermission
    from vibe.core.tools.utils import resolve_file_tool_permission
    from vibe.core.workspace import Workspace

    with tempfile.TemporaryDirectory() as workdir:
        root = str(Path(workdir).resolve())
        sensitive = resolve_file_tool_permission(
            ".env",
            tool_name="read_file",
            allowlist=[],
            denylist=[],
            config_permission=ToolPermission.ALWAYS,
            sensitive_patterns=["**/.env"],
            workspace=Workspace.for_session(Path(workdir)),
            scratchpad_dir=Path(workdir) / "scratchpad",
        )
        if sensitive is None:
            raise OracleError("the reference file-tool chain produced no context")
        sensitive_required = sensitive.required_permissions[0]
        outside_cases = []
        for target in OUTSIDE_TARGETS:
            with tempfile.TemporaryDirectory() as outside:
                outside_root = str(Path(outside).resolve())
                path = Path(outside) / {
                    "missing-file": "secret.txt",
                    "file": "present.txt",
                    "directory": "folder",
                }[target]
                if target == "file":
                    path.write_text("secret", encoding="utf-8")
                elif target == "directory":
                    path.mkdir()
                escaping = resolve_file_tool_permission(
                    str(path),
                    tool_name="read_file",
                    allowlist=[],
                    denylist=[],
                    config_permission=ToolPermission.ALWAYS,
                    sensitive_patterns=[],
                    workspace=Workspace.for_session(Path(workdir)),
                    scratchpad_dir=Path(workdir) / "scratchpad",
                )
                if escaping is None:
                    raise OracleError("the reference file-tool chain produced no context")
                required = escaping.required_permissions[0]
                outside_cases.append(
                    {
                        "target": target,
                        "permission": str(escaping.permission.value),
                        "scope": str(required.scope.value),
                        "invocationPattern": required.invocation_pattern.replace(
                            outside_root, "<outside>"
                        ),
                        "sessionPattern": required.session_pattern.replace(
                            outside_root, "<outside>"
                        ),
                        "label": required.label.replace(outside_root, "<outside>"),
                        "pathScopeRoot": None
                        if required.path_scope_root is None
                        else required.path_scope_root.replace(outside_root, "<outside>"),
                    }
                )
        return {
            "sensitiveScope": str(sensitive_required.scope.value),
            "sensitiveInvocationPattern": sensitive_required.invocation_pattern.replace(
                root, "<workdir>"
            ),
            "sensitiveSessionPattern": sensitive_required.session_pattern.replace(
                glob.escape(root), "<workdir>"
            ),
            "sensitiveLabel": sensitive_required.label.replace("read_file", "<tool>"),
            "permission": str(sensitive.permission.value),
            "outside": outside_cases,
        }


def capture_sensitive_matches(reference: Path) -> list[dict[str, Any]]:
    """What each matcher answers for the recorded pattern and path pairs.

    ``resolve_file_tool_permission`` runs ``matches_sensitive_pattern`` over
    ``sensitive_patterns``, ``path_pattern_matches`` over the allowlist and
    ``fnmatch`` over the denylist, so the three verdicts are captured side by
    side, each through the function the reference itself calls. A pattern the
    sensitive matcher refuses outright is recorded as the exception it raised
    rather than as a verdict, which is what says the sensitive branch has an
    unmatchable input to survive.
    """
    sys.path.insert(0, str(reference))
    import fnmatch

    from vibe.core.tools.utils import matches_sensitive_pattern
    from vibe.permissions import path_pattern_matches

    captured: list[dict[str, Any]] = []
    for pattern, path in SENSITIVE_CASES:
        entry: dict[str, Any] = {"pattern": pattern, "path": path}
        try:
            entry["sensitiveMatches"] = bool(matches_sensitive_pattern(path, [pattern]))
        except Exception as error:  # noqa: BLE001 - the refusal is the measurement
            entry["sensitiveMatches"] = None
            entry["sensitiveRaises"] = type(error).__name__
        entry["allowMatches"] = bool(path_pattern_matches(path, pattern))
        entry["denyMatches"] = bool(fnmatch.fnmatch(path, pattern))
        captured.append(entry)
    return captured


def capture_sensitive_chain(reference: Path) -> list[dict[str, Any]]:
    """Whether the whole file-tool chain requires a permission for each case.

    Every path here is inside the working directory, so the workdir branch adds
    nothing and the only requirement a case can carry is the sensitive one. What
    is recorded is the resolved permission and the scope of each requirement,
    which are enum values rather than authored text.
    """
    sys.path.insert(0, str(reference))
    import tempfile

    from vibe.core.tools.base import ToolPermission
    from vibe.core.tools.utils import resolve_file_tool_permission
    from vibe.core.workspace import Workspace

    captured: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory() as workdir:
        for pattern, path in SENSITIVE_CHAIN_CASES:
            context = resolve_file_tool_permission(
                path,
                tool_name="read_file",
                allowlist=[],
                denylist=[],
                config_permission=ToolPermission.ALWAYS,
                sensitive_patterns=[pattern],
                workspace=Workspace.for_session(Path(workdir)),
                scratchpad_dir=Path(workdir) / "scratchpad",
            )
            scopes = (
                [
                    str(required.scope.value)
                    for required in context.required_permissions
                ]
                if context is not None
                else []
            )
            captured.append(
                {
                    "pattern": pattern,
                    "path": path,
                    "permission": str(context.permission.value)
                    if context is not None
                    else None,
                    "scopes": scopes,
                }
            )
    return captured


def capture_path_grants(reference: Path) -> dict[str, Any]:
    """What ``path_grant_pattern`` and ``path_pattern_matches`` answer.

    Both are pure functions of strings, so the cases are recorded as written
    and replayed without a filesystem.
    """
    sys.path.insert(0, str(reference))
    from vibe.permissions import PathGrantScope, path_grant_pattern, path_pattern_matches

    return {
        "encodings": [
            {
                "path": path,
                "scope": scope,
                "pattern": path_grant_pattern(path, PathGrantScope(scope)),
            }
            for path, scope in PATH_GRANT_ENCODING_CASES
        ],
        "matches": [
            {"path": path, "pattern": pattern, "matches": bool(path_pattern_matches(path, pattern))}
            for path, pattern in PATH_MATCH_CASES
        ],
    }


def capture_covers(reference: Path) -> list[dict[str, Any]]:
    """What ``PermissionStore.covers`` answers for one stored rule."""
    sys.path.insert(0, str(reference))
    from vibe.core.tools.models import ApprovedRule
    from vibe.core.tools.permissions import PermissionStore
    from vibe.permissions import PermissionScope, RequiredPermission

    captured: list[dict[str, Any]] = []
    for rule_scope, rule_pattern, scope, invocation, literal in COVERS_CASES:
        store = PermissionStore()
        store.add_rule(
            ApprovedRule(
                tool_name="read_file",
                scope=PermissionScope(rule_scope),
                session_pattern=rule_pattern,
            )
        )
        requirement = RequiredPermission(
            scope=PermissionScope(scope),
            invocation_pattern=invocation,
            session_pattern=invocation,
            label=invocation,
            literal=literal,
        )
        captured.append(
            {
                "ruleScope": rule_scope,
                "rulePattern": rule_pattern,
                "scope": scope,
                "invocationPattern": invocation,
                "literal": literal,
                "covers": bool(store.covers("read_file", requirement)),
            }
        )
    return captured


def capture_list_chain(reference: Path) -> list[dict[str, Any]]:
    """What the whole file-tool chain answers with one list entry configured.

    The answer is the resolved permission and the scope of each requirement,
    or ``None`` when the chain decided nothing and the configured permission
    would. The working directory and the outside directory are temporary, so
    the cases are recorded with their placeholders rather than their paths.
    """
    sys.path.insert(0, str(reference))
    import tempfile

    from vibe.core.tools.base import ToolPermission
    from vibe.core.tools.utils import resolve_file_tool_permission
    from vibe.core.workspace import Workspace

    captured: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory() as workdir, tempfile.TemporaryDirectory() as outside:
        roots = {
            "workdir": str(Path(workdir).resolve()),
            "outside": str(Path(outside).resolve()),
        }
        for kind, pattern, path in LIST_CHAIN_CASES:
            entry = pattern.format(**roots)
            context = resolve_file_tool_permission(
                path.format(**roots),
                tool_name="read_file",
                allowlist=[entry] if kind == "allowlist" else [],
                denylist=[entry] if kind == "denylist" else [],
                config_permission=ToolPermission.ASK,
                sensitive_patterns=[],
                workspace=Workspace.for_session(Path(workdir)),
                scratchpad_dir=Path(workdir) / "scratchpad",
            )
            captured.append(
                {
                    "list": kind,
                    "pattern": pattern,
                    "path": path,
                    "permission": None
                    if context is None
                    else str(context.permission.value),
                    "scopes": []
                    if context is None
                    else [
                        str(required.scope.value)
                        for required in context.required_permissions
                    ],
                }
            )
    return captured


def build_corpus(reference: Path, expected_commit: str | None) -> dict[str, Any]:
    pin = resolve_reference(reference, expected_commit)
    vocabulary = capture_vocabulary(reference)
    arity = capture_arity(reference)
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": pin,
        "note": (
            "Captured from the pinned reference by "
            "scripts/parity/permission_surface.py. Scope values, requirement "
            "field names, command names, arities and the answers to cases this "
            "repository authored, path grant encodings and verdicts are "
            "observations; no reference-authored "
            "description text is recorded here."
        ),
        "counts": {
            "scopes": len(vocabulary["scopes"]),
            "arityEntries": len(arity),
            "sessionPatternCases": len(SESSION_PATTERN_CASES),
            "wildcardCases": len(WILDCARD_CASES),
            "sensitiveCases": len(SENSITIVE_CASES),
            "sensitiveChainCases": len(SENSITIVE_CHAIN_CASES),
            "pathGrantEncodingCases": len(PATH_GRANT_ENCODING_CASES),
            "pathMatchCases": len(PATH_MATCH_CASES),
            "coversCases": len(COVERS_CASES),
            "listChainCases": len(LIST_CHAIN_CASES),
            "outsideTargets": len(OUTSIDE_TARGETS),
        },
        "scopes": vocabulary["scopes"],
        "pathGrantScopes": vocabulary["pathGrantScopes"],
        "requirement": vocabulary["requirement"],
        "arity": arity,
        "sessionPatterns": capture_session_patterns(reference),
        "wildcardMatches": capture_wildcard_matches(reference),
        "fileToolChain": capture_file_tool_labels(reference),
        "sensitiveMatches": capture_sensitive_matches(reference),
        "sensitiveChain": capture_sensitive_chain(reference),
        "pathGrants": capture_path_grants(reference),
        "covers": capture_covers(reference),
        "listChain": capture_list_chain(reference),
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
        f"({counts['scopes']} scopes, {counts['arityEntries']} arity entries, "
        f"{counts['sessionPatternCases']} session-pattern cases, "
        f"{counts['wildcardCases']} wildcard cases, "
        f"{counts['sensitiveCases']} sensitive-pattern cases, "
        f"{counts['sensitiveChainCases']} chain cases, "
        f"{counts['pathGrantEncodingCases']} grant encodings, "
        f"{counts['pathMatchCases']} path matches, "
        f"{counts['coversCases']} covers cases, "
        f"{counts['listChainCases']} list chain cases)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
