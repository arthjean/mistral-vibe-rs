#!/usr/bin/env python3
"""Capture the permission vocabulary the pinned Python reference speaks.

The reference checkout is a read-only behavioral oracle. This script asks it
four questions whose answers are the contract EP-032 ports:

* which scopes ``PermissionScope`` declares, and under which wire values;
* which fields ``RequiredPermission`` carries, and under which aliases;
* what the arity table holds, entry by entry;
* what ``build_session_pattern`` and ``wildcard_match`` answer for a fixed case
  list, so the two functions are replayed rather than re-read.

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

SCHEMA_VERSION = 2
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


#: ``(pattern, absolute path)`` pairs whose verdict is recorded under both
#: matchers the reference uses. ``sensitive_patterns`` is matched with
#: ``PurePath.match``, which is right-anchored and compares component by
#: component, while the allowlist and the denylist stay on ``fnmatch``, whose
#: ``*`` crosses a separator and whose match is whole-string. The pairs are
#: chosen so most of them answer differently under the two, which is what makes
#: the corpus able to fail a port that runs one matcher for both. The last two
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
    """The scope values and the requirement fields, with their wire aliases."""
    sys.path.insert(0, str(reference))
    from vibe.permissions import PermissionScope, RequiredPermission

    scopes = [str(member.value) for member in PermissionScope]
    if not scopes:
        raise OracleError("the reference declares no permission scope")
    fields = [
        {
            "name": name,
            "alias": field.alias or name,
            "required": field.is_required(),
        }
        for name, field in RequiredPermission.model_fields.items()
    ]
    configuration = RequiredPermission.model_config
    return {
        "scopes": scopes,
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


def capture_file_tool_labels(reference: Path) -> dict[str, str]:
    """The two labels the shared file-tool chain composes.

    They are recorded as format strings this repository re-derives rather than
    as reference prose: each is a fixed word joined to a value the call itself
    carries, which is an observation of the shape and not of any authored text.
    """
    sys.path.insert(0, str(reference))
    import tempfile

    from vibe.core.tools.base import ToolPermission
    from vibe.core.tools.utils import resolve_file_tool_permission

    with tempfile.TemporaryDirectory() as workdir:
        sensitive = resolve_file_tool_permission(
            ".env",
            tool_name="read_file",
            allowlist=[],
            denylist=[],
            config_permission=ToolPermission.ALWAYS,
            sensitive_patterns=["**/.env"],
            cwd=Path(workdir),
            project_roots=[],
            scratchpad_dir=Path(workdir) / "scratchpad",
        )
        with tempfile.TemporaryDirectory() as outside:
            escaping = resolve_file_tool_permission(
                str(Path(outside) / "secret.txt"),
                tool_name="read_file",
                allowlist=[],
                denylist=[],
                config_permission=ToolPermission.ALWAYS,
                sensitive_patterns=[],
                cwd=Path(workdir),
                project_roots=[],
                scratchpad_dir=Path(workdir) / "scratchpad",
            )
            if sensitive is None or escaping is None:
                raise OracleError("the reference file-tool chain produced no context")
            sensitive_required = sensitive.required_permissions[0]
            escaping_required = escaping.required_permissions[0]
            outside_glob = str(Path(outside).resolve() / "*")
            return {
                "sensitiveScope": str(sensitive_required.scope.value),
                "sensitiveInvocationPattern": sensitive_required.invocation_pattern,
                "sensitiveSessionPattern": sensitive_required.session_pattern,
                "sensitiveLabel": sensitive_required.label.replace(
                    "read_file", "<tool>"
                ),
                "outsideScope": str(escaping_required.scope.value),
                "outsideInvocationPattern": escaping_required.invocation_pattern.replace(
                    outside_glob, "<glob>"
                ),
                "outsideSessionPattern": escaping_required.session_pattern.replace(
                    outside_glob, "<glob>"
                ),
                "outsideLabel": escaping_required.label.replace(outside_glob, "<glob>"),
                "permission": str(sensitive.permission.value),
            }


def capture_sensitive_matches(reference: Path) -> list[dict[str, Any]]:
    """What each matcher answers for the recorded pattern and path pairs.

    ``resolve_file_tool_permission`` runs ``PurePath.match`` over
    ``sensitive_patterns`` and ``fnmatch`` over the allowlist and the denylist,
    so the two verdicts are captured side by side. A pattern the matcher refuses
    outright is recorded as the exception it raised rather than as a verdict,
    which is what says the sensitive branch has an unmatchable input to survive.
    """
    sys.path.insert(0, str(reference))
    import fnmatch
    from pathlib import PurePath

    captured: list[dict[str, Any]] = []
    for pattern, path in SENSITIVE_CASES:
        entry: dict[str, Any] = {"pattern": pattern, "path": path}
        try:
            entry["sensitiveMatches"] = bool(PurePath(path).match(pattern))
        except Exception as error:  # noqa: BLE001 - the refusal is the measurement
            entry["sensitiveMatches"] = None
            entry["sensitiveRaises"] = type(error).__name__
        entry["listMatches"] = bool(fnmatch.fnmatch(path, pattern))
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
                cwd=Path(workdir),
                project_roots=[],
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
            "repository authored are observations; no reference-authored "
            "description text is recorded here."
        ),
        "counts": {
            "scopes": len(vocabulary["scopes"]),
            "arityEntries": len(arity),
            "sessionPatternCases": len(SESSION_PATTERN_CASES),
            "wildcardCases": len(WILDCARD_CASES),
            "sensitiveCases": len(SENSITIVE_CASES),
            "sensitiveChainCases": len(SENSITIVE_CHAIN_CASES),
        },
        "scopes": vocabulary["scopes"],
        "requirement": vocabulary["requirement"],
        "arity": arity,
        "sessionPatterns": capture_session_patterns(reference),
        "wildcardMatches": capture_wildcard_matches(reference),
        "fileToolChain": capture_file_tool_labels(reference),
        "sensitiveMatches": capture_sensitive_matches(reference),
        "sensitiveChain": capture_sensitive_chain(reference),
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
        f"{counts['sensitiveChainCases']} chain cases)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
