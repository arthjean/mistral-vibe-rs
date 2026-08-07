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

SCHEMA_VERSION = 1
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
        },
        "scopes": vocabulary["scopes"],
        "requirement": vocabulary["requirement"],
        "arity": arity,
        "sessionPatterns": capture_session_patterns(reference),
        "wildcardMatches": capture_wildcard_matches(reference),
        "fileToolChain": capture_file_tool_labels(reference),
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
        f"{counts['wildcardCases']} wildcard cases)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
