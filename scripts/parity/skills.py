#!/usr/bin/env python3
"""Capture how the pinned Python reference answers the skills surface.

The reference checkout is a read-only behavioral oracle. This script drives the
real ``parse_skill_markdown``, ``SkillMetadata``, ``SkillManager``, the registry
store and the registry manifests over synthetic inputs and temporary directory
trees, and records what each one answers. The Rust differential runner in
``crates/vibe-core/src/skills/skills_parity_tests.rs`` replays that corpus
unconditionally.

The reference is read through ``git archive`` at the pinned commit, so the
checkout's HEAD is observed and never moved. The capture runs against a
scratch ``HOME`` and ``VIBE_HOME``, so no real user state is read or written.

Committed observations are scenario-supplied values, field names, verdicts,
counts and digests. Reference-authored prose never enters the corpus: builtin
skill bodies and descriptions are recorded as length plus SHA-256, error
messages are normalized to a kind, and the registry store's fallback
description is masked behind a marker and recorded as a digest.

Usage::

    scripts/parity/skills.py --reference /path/to/reference
    scripts/parity/skills.py --python /path/to/venv/python

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it. The wrapper re-executes itself with
an interpreter that can import ``vibe`` from the pinned tree when the current
one cannot.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT

SCHEMA_VERSION = 1
DEFAULT_OUTPUT = Path("crates/vibe-core/tests/skills/corpus.json")
DEFAULT_CACHE = Path(".parity")
_REEXEC_MARKER = "VIBE_PARITY_PINNED_TREE"

#: Stands where the registry store's own fallback description was, so the
#: corpus records the structure of the generated file without a byte of the
#: reference's sentence. The digest travels beside it.
FALLBACK_DESCRIPTION_MARK = "{fallbackDescription}"

NOTE = (
    "Captured from the pinned reference by scripts/parity/skills.py. "
    "Scenario-supplied values, field names, verdicts, counts and digests are "
    "observations; no reference-authored prose is recorded. Builtin bodies and "
    "descriptions appear as length plus SHA-256 only, and the registry store's "
    "fallback description is masked as {fallbackDescription}. Normalizations: "
    "published skill lists are sorted by name because the reference walks "
    "directories in filesystem order; the winner of a duplicate name within a "
    "single root is filesystem-order dependent, so its path is recorded null; "
    "symlinked roots are mapped to the label of the directory they resolve to."
)


class OracleError(RuntimeError):
    """Raised when the corpus cannot be produced from an authoritative state."""


# --------------------------------------------------------------------------
# Reference pinning, following scripts/parity/tool_execution.py
# --------------------------------------------------------------------------


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


def resolve_reference(reference: Path, expected: str) -> dict[str, str]:
    """The pinned commit, required to exist in the checkout without depending
    on where its HEAD sits."""

    if not reference.is_dir():
        raise OracleError(f"no reference checkout at {reference}")
    try:
        _git(reference, "cat-file", "-e", f"{expected}^{{commit}}")
    except OracleError as error:
        raise OracleError(
            f"{reference} does not contain the pinned commit {expected}: {error}"
        ) from error
    return {"commit": expected}


def extract_pinned_tree(reference: Path, commit: str, cache: Path) -> Path:
    """The pinned source tree, materialized out of tree and reused across runs.

    ``git archive`` writes the commit's contents without moving HEAD, creating a
    branch or adding a worktree, so the checkout is observed and never modified.
    """

    import tarfile

    tree = (cache / f"reference-{commit[:12]}").resolve()
    marker = tree / "vibe" / "__init__.py"
    if marker.is_file():
        return tree
    tree.mkdir(parents=True, exist_ok=True)
    archive = tree.with_suffix(".tar")
    _git(reference, "archive", "--format=tar", "-o", str(archive), commit)
    with tarfile.open(archive) as bundle:
        bundle.extractall(tree, filter="data")
    archive.unlink(missing_ok=True)
    if not marker.is_file():
        raise OracleError(f"the extracted tree at {tree} carries no `vibe` package")
    return tree


def reexecute_with_reference_interpreter(
    reference: Path, override: Path | None, tree: Path
) -> None:
    """Re-runs this script under an interpreter importing the *pinned* tree."""

    if os.environ.get(_REEXEC_MARKER) == str(tree):
        if not _imports_pinned_vibe(tree):
            raise OracleError(
                f"the reference interpreter did not import `vibe` from {tree}"
            )
        return
    candidates = [override] if override else []
    if os.environ.get("VIBE_PARITY_PYTHON"):
        candidates.append(Path(os.environ["VIBE_PARITY_PYTHON"]))
    candidates += [
        reference / ".venv/bin/python",
        reference / ".venv/Scripts/python.exe",
    ]
    interpreter = next((c for c in candidates if c and c.is_file()), None)
    if interpreter is None:
        raise OracleError(
            f"no interpreter can import `vibe`; looked for a virtual environment in {reference}"
        )
    environment = dict(os.environ)
    environment[_REEXEC_MARKER] = str(tree)
    environment["PYTHONPATH"] = os.pathsep.join(
        [str(tree), *([environment["PYTHONPATH"]] if environment.get("PYTHONPATH") else [])]
    )
    os.execve(str(interpreter), [str(interpreter), *sys.argv], environment)


def _imports_pinned_vibe(tree: Path) -> bool:
    try:
        import vibe
    except Exception:
        return False
    return Path(vibe.__file__).resolve().is_relative_to(tree.resolve())


# --------------------------------------------------------------------------
# Small helpers
# --------------------------------------------------------------------------


def digest_of(text: str) -> dict[str, Any]:
    return {
        "length": len(text),
        "digest": hashlib.sha256(text.encode("utf-8")).hexdigest(),
    }


def jsonable(value: Any, context: str) -> Any:
    """``value`` if JSON can carry it verbatim, or a loud failure.

    PyYAML resolves some scalars into types JSON cannot represent (dates,
    binary). No authored case uses them, and this guard keeps that true.
    """

    if isinstance(value, (str, int, float, bool)) or value is None:
        return value
    if isinstance(value, list):
        return [jsonable(item, context) for item in value]
    if isinstance(value, dict):
        return {str(key): jsonable(item, context) for key, item in value.items()}
    raise OracleError(f"{context}: {type(value).__name__} is not JSON-representable")


class StubTrust:
    """Trust store double: every folder trusted, or none.

    ``HarnessFilesManager`` only calls ``is_trusted``, so the double keeps the
    scenario hermetic without touching the real trusted-folders file.
    """

    def __init__(self, trusted: bool) -> None:
        self._trusted = trusted

    def is_trusted(self, _path: Path) -> bool:
        return self._trusted


# --------------------------------------------------------------------------
# Family: frontmatter (parse_skill_markdown)
# --------------------------------------------------------------------------

FRONTMATTER_CASES: list[tuple[str, str]] = [
    ("valid-minimal", "---\nname: probe\ndescription: A probe skill\n---\n\nBody line.\n"),
    (
        "all-fields-nested-metadata",
        "---\n"
        "name: full-probe\n"
        "description: A probe with every field\n"
        "license: MIT\n"
        "compatibility: Requires nothing\n"
        "metadata:\n"
        "  display-name: Full Probe\n"
        "  default-prompt: run it\n"
        "allowed-tools: bash read_file\n"
        "user-invocable: false\n"
        "---\n\nInstructions here.\n",
    ),
    ("missing-frontmatter", "Just markdown content without frontmatter\n"),
    ("unclosed-frontmatter", "---\nname: incomplete\ndescription: no closing fence\n"),
    ("invalid-yaml", "---\nname: [broken\ndescription: oops\n---\n\nBody.\n"),
    ("sequence-document", "---\n- item-one\n- item-two\n---\n\nBody.\n"),
    ("scalar-document", "---\njust a scalar\n---\n\nBody.\n"),
    ("empty-frontmatter", "---\n---\n\nBody content.\n"),
    ("comment-only-frontmatter", "---\n# nothing but a comment\n---\n\nBody.\n"),
    ("no-body", "---\nname: minimal\ndescription: no body\n---\n"),
    (
        "leading-bom",
        "\ufeff---\nname: bom-probe\ndescription: BOM stripped\n---\n\nBody.\n",
    ),
    ("boundary-four-hyphens", "----\nname: probe\ndescription: wide fence\n----\n\nBody.\n"),
    (
        "boundary-trailing-whitespace",
        "---  \nname: probe\ndescription: padded fence\n---\t\n\nBody.\n",
    ),
    ("text-before-boundary", "prelude\n---\nname: probe\ndescription: late fence\n---\n\nBody.\n"),
    (
        "block-sequence-allowed-tools",
        "---\nname: probe\ndescription: list form\nallowed-tools:\n  - bash\n  - read_file\n---\n\nBody.\n",
    ),
    (
        "folded-scalar-description",
        "---\nname: probe\ndescription: >\n  folded across\n  two lines\n---\n\nBody.\n",
    ),
    (
        "literal-scalar-description",
        "---\nname: probe\ndescription: |\n  literal line one\n  literal line two\n---\n\nBody.\n",
    ),
    ("user-invocable-no", "---\nname: probe\ndescription: d\nuser-invocable: no\n---\n\nBody.\n"),
    ("user-invocable-cap-no", "---\nname: probe\ndescription: d\nuser-invocable: No\n---\n\nBody.\n"),
    ("user-invocable-off", "---\nname: probe\ndescription: d\nuser-invocable: OFF\n---\n\nBody.\n"),
    ("user-invocable-on", "---\nname: probe\ndescription: d\nuser-invocable: on\n---\n\nBody.\n"),
    ("user-invocable-y", "---\nname: probe\ndescription: d\nuser-invocable: y\n---\n\nBody.\n"),
    ("user-invocable-yes", "---\nname: probe\ndescription: d\nuser-invocable: yes\n---\n\nBody.\n"),
    (
        "user-invocable-quoted-no",
        '---\nname: probe\ndescription: d\nuser-invocable: "no"\n---\n\nBody.\n',
    ),
    (
        "duplicate-key-last-wins",
        "---\nname: first\nname: second\ndescription: d\n---\n\nBody.\n",
    ),
    (
        "body-keeps-later-fences",
        "---\nname: probe\ndescription: d\n---\n\nBody before.\n---\nBody after a fence.\n",
    ),
    (
        "fence-inside-literal-scalar",
        "---\nname: probe\ndescription: |\n  line one\n---\n  line two\n---\n\nBody.\n",
    ),
    ("tab-indented-yaml", "---\nname: probe\ndescription:\n\tvalue\n---\n\nBody.\n"),
    (
        "crlf-line-endings",
        "---\r\nname: probe\r\ndescription: carriage returns\r\n---\r\n\r\nBody.\r\n",
    ),
    (
        "numeric-scalars-kept",
        "---\nname: probe\ndescription: d\nmetadata:\n  version: 1.0\n  count: 42\n---\n\nBody.\n",
    ),
]


def capture_frontmatter() -> list[dict[str, Any]]:
    from vibe.core.skills.parser import SkillParseError, parse_skill_markdown

    def error_kind(reason: str) -> str:
        # The reason strings are reference-authored; only the branch that
        # raised is recorded, never the sentence.
        if reason.startswith("Missing or invalid"):
            return "boundary"
        if reason.startswith("Invalid YAML"):
            return "yaml"
        if "mapping" in reason:
            return "mapping"
        raise OracleError(f"unknown SkillParseError branch: {reason!r}")

    captured: list[dict[str, Any]] = []
    for case, content in FRONTMATTER_CASES:
        record: dict[str, Any] = {"case": case, "content": content}
        try:
            frontmatter, body = parse_skill_markdown(content)
        except SkillParseError as error:
            record["error"] = error_kind(error.reason)
        else:
            record["frontmatter"] = jsonable(frontmatter, f"frontmatter/{case}")
            record["body"] = body
        captured.append(record)
    return captured


# --------------------------------------------------------------------------
# Family: metadata (SkillMetadata.model_validate)
# --------------------------------------------------------------------------

METADATA_CASES: list[tuple[str, dict[str, Any]]] = [
    ("minimal", {"name": "probe", "description": "A probe"}),
    (
        "all-fields",
        {
            "name": "full-probe",
            "description": "Everything set",
            "license": "MIT",
            "compatibility": "none needed",
            "metadata": {"display-name": "Full Probe"},
            "allowed-tools": ["bash", "read_file"],
            "user-invocable": False,
        },
    ),
    ("allowed-tools-string", {"name": "p", "description": "d", "allowed-tools": "bash read_file grep"}),
    ("allowed-tools-list", {"name": "p", "description": "d", "allowed-tools": ["bash", "read_file"]}),
    ("allowed-tools-null", {"name": "p", "description": "d", "allowed-tools": None}),
    ("allowed-tools-empty-string", {"name": "p", "description": "d", "allowed-tools": ""}),
    ("allowed-tools-underscore-spelling", {"name": "p", "description": "d", "allowed_tools": "bash"}),
    (
        "allowed-tools-both-spellings",
        {"name": "p", "description": "d", "allowed-tools": "bash", "allowed_tools": "grep"},
    ),
    ("user-invocable-hyphen", {"name": "p", "description": "d", "user-invocable": False}),
    ("user-invocable-underscore", {"name": "p", "description": "d", "user_invocable": False}),
    (
        "user-invocable-both-spellings",
        {"name": "p", "description": "d", "user-invocable": False, "user_invocable": True},
    ),
    ("user-invocable-string-false", {"name": "p", "description": "d", "user-invocable": "false"}),
    ("user-invocable-string-no", {"name": "p", "description": "d", "user-invocable": "no"}),
    ("user-invocable-string-junk", {"name": "p", "description": "d", "user-invocable": "maybe"}),
    ("metadata-non-string-values", {"name": "p", "description": "d", "metadata": {"version": 1.0, "count": 42, "flag": True, "empty": None}}),
    ("metadata-null", {"name": "p", "description": "d", "metadata": None}),
    ("metadata-int-keys", {"name": "p", "description": "d", "metadata": {1: "one"}}),
    ("extra-key-ignored", {"name": "p", "description": "d", "unknown-key": "ignored"}),
    ("name-64-chars", {"name": "a" * 64, "description": "d"}),
    ("description-1024-chars", {"name": "p", "description": "a" * 1024}),
    ("compatibility-500-chars", {"name": "p", "description": "d", "compatibility": "c" * 500}),
    ("license-null", {"name": "p", "description": "d", "license": None}),
    ("name-uppercase", {"name": "Probe-Skill", "description": "d"}),
    ("name-underscore", {"name": "probe_skill", "description": "d"}),
    ("name-consecutive-hyphens", {"name": "probe--skill", "description": "d"}),
    ("name-leading-hyphen", {"name": "-probe", "description": "d"}),
    ("name-trailing-hyphen", {"name": "probe-", "description": "d"}),
    ("name-65-chars", {"name": "a" * 65, "description": "d"}),
    ("name-empty", {"name": "", "description": "d"}),
    ("name-missing", {"description": "d"}),
    ("name-integer", {"name": 5, "description": "d"}),
    ("description-missing", {"name": "p"}),
    ("description-empty", {"name": "p", "description": ""}),
    ("description-1025-chars", {"name": "p", "description": "a" * 1025}),
    ("compatibility-501-chars", {"name": "p", "description": "d", "compatibility": "c" * 501}),
    ("license-integer", {"name": "p", "description": "d", "license": 7}),
    ("metadata-list", {"name": "p", "description": "d", "metadata": ["not", "a", "mapping"]}),
    ("allowed-tools-integer", {"name": "p", "description": "d", "allowed-tools": 3}),
]


def capture_metadata() -> list[dict[str, Any]]:
    from pydantic import ValidationError

    from vibe.core.skills.models import SkillMetadata

    captured: list[dict[str, Any]] = []
    for case, frontmatter in METADATA_CASES:
        record: dict[str, Any] = {
            "case": case,
            "frontmatter": jsonable(frontmatter, f"metadata/{case}"),
        }
        try:
            meta = SkillMetadata.model_validate(frontmatter)
        # Broad on purpose: a `metadata:` list makes the reference's before
        # validator raise a bare AttributeError, which the manager's equally
        # broad `except Exception` turns into a skipped skill. The verdict is
        # the observation; the exception class is an implementation detail.
        except Exception:
            record["accepted"] = False
        else:
            record["accepted"] = True
            record["fields"] = jsonable(meta.model_dump(), f"metadata/{case}")
        captured.append(record)
    return captured


# --------------------------------------------------------------------------
# Families over temporary trees: discovery, filtering, command
# --------------------------------------------------------------------------


def skill_file(name: str, description: str, *, invocable: bool | None = None, body: str = "Probe body.") -> str:
    lines = [f"name: {name}", f"description: {description}"]
    if invocable is not None:
        lines.append(f"user-invocable: {'true' if invocable else 'false'}")
    return "---\n" + "\n".join(lines) + "\n---\n\n" + body + "\n"


DISCOVERY_SCENARIOS: list[dict[str, Any]] = [
    {"case": "empty-everywhere", "tree": {}},
    {
        "case": "project-vibe-skills",
        "tree": {"project": {".vibe/skills/alpha/SKILL.md": skill_file("alpha", "From project .vibe")}},
    },
    {
        "case": "project-agents-skills",
        "tree": {"project": {".agents/skills/beta/SKILL.md": skill_file("beta", "From project .agents")}},
    },
    {
        "case": "project-both-roots",
        "tree": {
            "project": {
                ".vibe/skills/alpha/SKILL.md": skill_file("alpha", "From project .vibe"),
                ".agents/skills/beta/SKILL.md": skill_file("beta", "From project .agents"),
            }
        },
    },
    {
        "case": "project-vibe-beats-agents",
        "tree": {
            "project": {
                ".vibe/skills/shared/SKILL.md": skill_file("shared", "Winner from .vibe"),
                ".agents/skills/shared/SKILL.md": skill_file("shared", "Loser from .agents"),
            }
        },
    },
    {
        "case": "user-vibe-skills",
        "tree": {"home": {".vibe/skills/gamma/SKILL.md": skill_file("gamma", "From user .vibe")}},
    },
    {
        "case": "user-agents-skills",
        "tree": {"home": {".agents/skills/delta/SKILL.md": skill_file("delta", "From user .agents")}},
    },
    {
        "case": "user-vibe-beats-agents",
        "tree": {
            "home": {
                ".vibe/skills/shared/SKILL.md": skill_file("shared", "Winner from user .vibe"),
                ".agents/skills/shared/SKILL.md": skill_file("shared", "Loser from user .agents"),
            }
        },
    },
    {
        "case": "legacy-extensions-root-unread",
        "tree": {"home": {".vibe/extensions/skills/ghost/SKILL.md": skill_file("ghost", "Invented path")}},
    },
    {
        "case": "configured-path",
        "skillPaths": ["${configured}"],
        "tree": {"configured": {"epsilon/SKILL.md": skill_file("epsilon", "From configured")}},
    },
    {
        "case": "configured-beats-project-beats-user",
        "skillPaths": ["${configured}"],
        "tree": {
            "configured": {"shared/SKILL.md": skill_file("shared", "Winner from configured")},
            "project": {".vibe/skills/shared/SKILL.md": skill_file("shared", "Loser from project")},
            "home": {".vibe/skills/shared/SKILL.md": skill_file("shared", "Loser from user")},
        },
    },
    {
        "case": "project-beats-user",
        "tree": {
            "project": {".vibe/skills/shared/SKILL.md": skill_file("shared", "Winner from project")},
            "home": {".vibe/skills/shared/SKILL.md": skill_file("shared", "Loser from user")},
        },
    },
    {
        "case": "two-configured-paths-in-order",
        "skillPaths": ["${configured}", "${configured2}"],
        "tree": {
            "configured": {"shared/SKILL.md": skill_file("shared", "Winner from first path")},
            "configured2": {
                "shared/SKILL.md": skill_file("shared", "Loser from second path"),
                "zeta/SKILL.md": skill_file("zeta", "Only in second path"),
            },
        },
    },
    {
        "case": "nonexistent-configured-path",
        "skillPaths": ["${configured}/missing"],
        "tree": {"project": {".vibe/skills/alpha/SKILL.md": skill_file("alpha", "Still found")}},
    },
    {
        "case": "configured-path-is-a-file",
        "skillPaths": ["${configured}/plain.txt"],
        "tree": {
            "configured": {"plain.txt": "not a directory\n"},
            "project": {".vibe/skills/alpha/SKILL.md": skill_file("alpha", "Still found")},
        },
    },
    {
        "case": "relative-configured-path",
        "skillPaths": ["./rel-skills"],
        "tree": {"project": {"rel-skills/eta/SKILL.md": skill_file("eta", "From relative path")}},
    },
    {
        "case": "tilde-configured-path",
        "skillPaths": ["~/tilde-skills"],
        "tree": {"home": {"tilde-skills/theta/SKILL.md": skill_file("theta", "From tilde path")}},
    },
    {
        "case": "symlinked-root-walked-once",
        "skillPaths": ["${configured}", "${configured}-link"],
        "symlinks": [{"link": "configured-link", "target": "configured"}],
        "tree": {"configured": {"iota/SKILL.md": skill_file("iota", "Behind a symlink too")}},
    },
    {
        "case": "duplicate-name-within-one-root",
        "pathUnrecorded": True,
        "tree": {
            "project": {
                ".vibe/skills/dir-a/SKILL.md": skill_file("twin", "Same either way"),
                ".vibe/skills/dir-b/SKILL.md": skill_file("twin", "Same either way"),
            }
        },
    },
    {
        "case": "clutter-ignored",
        "tree": {
            "project": {
                ".vibe/skills/no-manifest/README.md": "not a skill\n",
                ".vibe/skills/stray-file.md": "just a file\n",
                ".vibe/skills/alpha/SKILL.md": skill_file("alpha", "The only real one"),
            }
        },
    },
    {
        "case": "malformed-skill-becomes-issue",
        "tree": {
            "project": {
                ".vibe/skills/broken/SKILL.md": "no frontmatter here\n",
                ".vibe/skills/alpha/SKILL.md": skill_file("alpha", "Still loads"),
            }
        },
    },
    {
        "case": "missing-description-becomes-issue",
        "tree": {"project": {".vibe/skills/bare/SKILL.md": "---\nname: bare\n---\n\nBody.\n"}},
    },
    {
        "case": "three-issues-accumulate",
        "tree": {
            "project": {
                ".vibe/skills/bad-a/SKILL.md": "nope\n",
                ".vibe/skills/bad-b/SKILL.md": "---\nname: [\n---\n",
                ".vibe/skills/bad-c/SKILL.md": "---\nname: bad-c\n---\n\nBody.\n",
            }
        },
    },
    {
        "case": "builtin-name-reserved",
        "tree": {"project": {".vibe/skills/vibe/SKILL.md": skill_file("vibe", "Impostor")}},
    },
    {
        "case": "frontmatter-name-wins-over-directory",
        "tree": {"project": {".vibe/skills/folder-name/SKILL.md": skill_file("real-name", "Named by frontmatter")}},
    },
    {
        "case": "untrusted-project-not-walked",
        "trusted": False,
        "tree": {
            "project": {".vibe/skills/alpha/SKILL.md": skill_file("alpha", "Should not load")},
            "home": {".vibe/skills/gamma/SKILL.md": skill_file("gamma", "Still loads")},
        },
    },
]

#: The labels a discovery tree can address. ``home`` doubles as the scratch
#: ``HOME`` and ``VIBE_HOME``'s parent, so ``~`` and the user roots land in it.
_ROOT_LABELS = ("home", "project", "configured", "configured2")


def _materialize_case(scratch: Path, scenario: dict[str, Any]) -> dict[str, Path]:
    home = Path(os.environ["HOME"])
    for stale in (".vibe/skills", ".agents/skills", ".vibe/extensions", "tilde-skills"):
        shutil.rmtree(home / stale, ignore_errors=True)
    case_root = scratch / "cases" / scenario["case"]
    shutil.rmtree(case_root, ignore_errors=True)
    roots = {"home": home}
    for label in _ROOT_LABELS[1:]:
        roots[label] = case_root / label
        roots[label].mkdir(parents=True)
    for label, files in scenario.get("tree", {}).items():
        for relative, content in files.items():
            target = roots[label] / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content, encoding="utf-8")
    for link in scenario.get("symlinks", []):
        (case_root / link["link"]).symlink_to(roots[link["target"]])
    return roots


def _expand_spec(spec: str, case_root: Path, roots: dict[str, Path]) -> str:
    for label in _ROOT_LABELS:
        spec = spec.replace("${" + label + "}", str(roots[label]))
    return spec.replace("${case}", str(case_root))


def _label_path(path: Path, roots: dict[str, Path]) -> tuple[str, str] | None:
    resolved = Path(path).resolve()
    best: tuple[str, str] | None = None
    for label, root in roots.items():
        root = root.resolve()
        if resolved == root or root in resolved.parents:
            relative = str(resolved.relative_to(root)).replace(os.sep, "/")
            if best is None or len(relative) < len(best[1]):
                best = (label, relative)
    return best


def _build_manager(scenario: dict[str, Any], roots: dict[str, Path], case_root: Path):
    from vibe.core.config import VibeConfigSchema
    from vibe.core.config.harness_files import HarnessFilesManager
    from vibe.core.skills.manager import SkillManager

    harness = HarnessFilesManager(
        sources=("user", "project"),
        cwd=roots["project"],
        trust_store=StubTrust(scenario.get("trusted", True)),
    )
    previous = Path.cwd()
    os.chdir(roots["project"])
    try:
        # The config is built under the scenario's working directory, not the
        # capture's. `skill_paths` carries a before-validator that resolves
        # every entry at validation time (vibe_schema.py:213), so a relative
        # entry anchors to whatever the cwd was when the document was
        # validated. Production validates and discovers in one process at one
        # cwd; building the config out here would record the launcher's
        # directory instead of the scenario's, which is both wrong and not
        # reproducible across machines.
        config = VibeConfigSchema(
            skill_paths=[
                _expand_spec(spec, case_root, roots)
                for spec in scenario.get("skillPaths", [])
            ],
            enabled_skills=scenario.get("enabledSkills", []),
            disabled_skills=scenario.get("disabledSkills", []),
        )
        return SkillManager(lambda: config, harness_files=harness)
    finally:
        os.chdir(previous)


def capture_discovery(scratch: Path) -> list[dict[str, Any]]:
    from vibe.core.skills.builtins import BUILTIN_SKILLS

    captured: list[dict[str, Any]] = []
    for scenario in DISCOVERY_SCENARIOS:
        case = scenario["case"]
        case_root = scratch / "cases" / case
        roots = _materialize_case(scratch, scenario)
        manager = _build_manager(scenario, roots, case_root)

        published = []
        for info in manager.available_skills.values():
            builtin = info.name in BUILTIN_SKILLS
            location = None if info.skill_path is None else _label_path(info.skill_path, roots)
            if scenario.get("pathUnrecorded"):
                location = None
            published.append({
                "name": info.name,
                "source": info.source.value,
                "scope": info.scope.value,
                "root": None if location is None else location[0],
                "relPath": None if location is None else location[1],
                "userInvocable": info.user_invocable,
                "description": None if builtin else info.description,
            })
        published.sort(key=lambda entry: entry["name"])

        issues = sorted(
            (_label_path(issue.file, roots) or ("unknown", str(issue.file)))
            for issue in manager.config_issues
        )
        search_paths = [
            list(_label_path(path, roots) or ("unknown", str(path)))
            for path in manager._search_paths
        ]
        captured.append({
            "case": case,
            "skillPaths": scenario.get("skillPaths", []),
            "enabledSkills": scenario.get("enabledSkills", []),
            "disabledSkills": scenario.get("disabledSkills", []),
            "projectTrusted": scenario.get("trusted", True),
            "symlinks": scenario.get("symlinks", []),
            "tree": scenario.get("tree", {}),
            "searchPaths": search_paths,
            "published": published,
            "issues": [{"root": root, "relPath": rel} for root, rel in issues],
            "customSkillsCount": manager.custom_skills_count,
        })
    return captured


FILTERING_SCENARIOS: list[dict[str, Any]] = [
    {"case": "no-filters", "skills": ["alpha", "beta"]},
    {"case": "enabled-only", "skills": ["alpha", "beta", "gamma"], "enabled": ["alpha", "gamma"]},
    {"case": "disabled-only", "skills": ["alpha", "beta", "gamma"], "disabled": ["beta"]},
    {
        "case": "enabled-precedence",
        "skills": ["alpha", "beta"],
        "enabled": ["alpha"],
        "disabled": ["alpha"],
    },
    {"case": "glob-pattern", "skills": ["search-code", "search-docs", "other"], "enabled": ["search-*"]},
    {"case": "glob-case-insensitive", "skills": ["search-code", "other"], "enabled": ["SEARCH-*"]},
    {"case": "regex-pattern", "skills": ["skill-v1", "skill-v2", "other"], "enabled": ["re:skill-v\\d+"]},
    {"case": "regex-case-insensitive", "skills": ["skill-v1", "other"], "enabled": ["re:SKILL-V\\d+"]},
    {"case": "invalid-regex-matches-nothing", "skills": ["alpha", "beta"], "enabled": ["re:["]},
    {"case": "invalid-regex-disables-nothing", "skills": ["alpha", "beta"], "disabled": ["re:["]},
    {"case": "enabled-can-select-a-builtin", "skills": ["alpha"], "enabled": ["vibe"]},
    {"case": "disabled-can-withhold-a-builtin", "skills": ["alpha"], "disabled": ["vibe", "skill-creator"]},
    {"case": "blank-patterns-ignored", "skills": ["alpha", "beta"], "disabled": ["", "  "]},
]


def capture_filtering(scratch: Path) -> list[dict[str, Any]]:
    captured: list[dict[str, Any]] = []
    for scenario in FILTERING_SCENARIOS:
        case = f"filtering-{scenario['case']}"
        tree = {
            "configured": {
                f"{name}/SKILL.md": skill_file(name, f"Skill {name}")
                for name in scenario["skills"]
            }
        }
        spec = {
            "case": case,
            "tree": tree,
            "skillPaths": ["${configured}"],
            "enabledSkills": scenario.get("enabled", []),
            "disabledSkills": scenario.get("disabled", []),
        }
        case_root = scratch / "cases" / case
        roots = _materialize_case(scratch, spec)
        manager = _build_manager(spec, roots, case_root)
        withheld = sorted(set(scenario["skills"]) - set(manager.available_skills))
        captured.append({
            "case": scenario["case"],
            "skills": scenario["skills"],
            "enabledSkills": scenario.get("enabled", []),
            "disabledSkills": scenario.get("disabled", []),
            "kept": sorted(manager.available_skills),
            "customSkillsCount": manager.custom_skills_count,
            # get_skill and the filter can never disagree; recorded so the
            # replay can hold the port's lookup to the same rule.
            "withheldLookupMisses": all(
                manager.get_skill(name) is None for name in withheld
            ),
        })
    return captured


COMMAND_FIXTURES: list[dict[str, Any]] = [
    {"name": "probe", "description": "A probe", "invocable": True, "body": "Run the probe checklist."},
    {"name": "hidden", "description": "Model only", "invocable": False, "body": "Hidden body."},
]

COMMAND_CASES: list[tuple[str, str]] = [
    ("plain-text", "hello world"),
    ("slash-only", "/"),
    ("unknown-skill", "/nonexistent"),
    ("simple", "/probe"),
    ("with-args", "/probe fix the bug"),
    ("multiple-spaces-kept-in-args", "/probe   spaced   args"),
    ("uppercase-name", "/PROBE"),
    ("leading-and-trailing-whitespace", "   /probe trailing   "),
    ("newline-separated-args", "/probe\nsecond line"),
    ("not-user-invocable", "/hidden"),
    ("builtin-skill-creator", "/skill-creator"),
    ("builtin-vibe-not-invocable", "/vibe"),
    ("slash-inside-text", "run /probe later"),
]


def capture_command(scratch: Path) -> dict[str, Any]:
    tree = {
        "configured": {
            f"{fixture['name']}/SKILL.md": skill_file(
                fixture["name"],
                fixture["description"],
                invocable=fixture["invocable"],
                body=fixture["body"],
            )
            for fixture in COMMAND_FIXTURES
        }
    }
    spec = {"case": "command-fixtures", "tree": tree, "skillPaths": ["${configured}"]}
    case_root = scratch / "cases" / spec["case"]
    roots = _materialize_case(scratch, spec)
    manager = _build_manager(spec, roots, case_root)

    cases = []
    for case, prompt in COMMAND_CASES:
        parsed = manager.parse_skill_command(prompt)
        cases.append({
            "case": case,
            "prompt": prompt,
            "result": None
            if parsed is None
            else {
                "name": parsed.name,
                "extraInstructions": parsed.extra_instructions,
                "content": digest_of(parsed.content),
            },
        })
    return {"skills": COMMAND_FIXTURES, "cases": cases}


# --------------------------------------------------------------------------
# Family: projection (the SkillSummary wire shape)
# --------------------------------------------------------------------------

PROJECTION_CASES: list[tuple[str, dict[str, Any]]] = [
    ("local-defaults", {"name": "probe", "description": "A probe", "prompt": "Body."}),
    (
        "local-not-invocable",
        {"name": "hidden", "description": "Model only", "prompt": "Body.", "user_invocable": False},
    ),
    (
        "builtin-shape",
        {"name": "inline", "description": "Builtin shape", "prompt": "Inline prompt.", "source": "builtin", "user_invocable": False},
    ),
    (
        "registry-source",
        {"name": "remote", "description": "From the registry", "prompt": "Body.", "source": "registry"},
    ),
    ("unicode-fields", {"name": "accents", "description": "Précis à jour", "prompt": "Corps du texte."}),
    ("empty-prompt", {"name": "empty", "description": "No body", "prompt": ""}),
    (
        "rich-model-fields-do-not-reach-the-summary",
        {
            "name": "rich",
            "description": "Carries every model field",
            "prompt": "Body.",
            "license": "MIT",
            "compatibility": "anything",
            "metadata": {"display-name": "Rich"},
            "allowed_tools": ["bash"],
        },
    ),
    ("multiline-prompt", {"name": "long", "description": "d", "prompt": "line one\nline two\n"}),
]


def capture_projection(scratch: Path) -> list[dict[str, Any]]:
    from vibe.app_server.models import SkillSummary
    from vibe.core.skills.models import SkillInfo

    captured: list[dict[str, Any]] = []
    for case, fields in PROJECTION_CASES:
        info = SkillInfo.model_validate(fields)
        # The same dict _projection.project_skills builds at
        # vibe/app_server/_projection.py:231, driven without an agent loop.
        summary = SkillSummary.model_validate({
            "name": info.name,
            "description": info.description,
            "prompt": info.prompt,
            "user_invocable": info.user_invocable,
            "source": info.source.value,
        })
        captured.append({
            "case": case,
            "skill": jsonable(fields, f"projection/{case}"),
            "summary": jsonable(summary.model_dump(mode="json"), f"projection/{case}"),
        })
    return captured


# --------------------------------------------------------------------------
# Family: store (the registry version store)
# --------------------------------------------------------------------------


def _item(spec: dict[str, Any]):
    from vibe.core.skills.registry.models import RegistrySkillItem

    payload: dict[str, Any] = {
        "skillId": spec.get("skillId", "id-1"),
        "version": spec.get("version", 1),
        "skill": {
            "skillName": spec.get("skillName", ""),
            "skillDescription": spec.get("description", ""),
            "skillBody": spec.get("body", "Registry body."),
            "skillAssets": {
                asset["path"]: {
                    **(
                        {"textContent": asset["text"]}
                        if "text" in asset
                        else {"rawContent": asset["base64"]}
                    ),
                    "isExecutable": asset.get("executable", False),
                }
                for asset in spec.get("assets", [])
            },
        },
    }
    if spec.get("metadataName"):
        payload["metadata"] = {"name": spec["metadataName"]}
    return RegistrySkillItem.model_validate(payload)


STORE_CASES: list[dict[str, Any]] = [
    {"case": "unsafe-id-empty", "op": "skillDir", "id": "", "version": 1},
    {"case": "unsafe-id-dot", "op": "skillDir", "id": ".", "version": 1},
    {"case": "unsafe-id-dotdot", "op": "skillDir", "id": "..", "version": 1},
    {"case": "unsafe-id-traversal", "op": "skillDir", "id": "../escape", "version": 1},
    {"case": "unsafe-id-separator", "op": "skillDir", "id": "a/b", "version": 1},
    {"case": "unsafe-id-inner-traversal", "op": "skillDir", "id": "foo/../bar", "version": 1},
    {"case": "safe-id", "op": "skillDir", "id": "ok", "version": 3},
    {
        "case": "materialize-basic",
        "op": "materialize",
        "item": {"skillId": "abc", "version": 1, "description": "does X", "body": "# Hi"},
        "name": "my-skill",
    },
    {
        "case": "materialize-fallback-description",
        "op": "materialize",
        "item": {"skillId": "nod", "version": 1, "body": "# Hi"},
        "name": "nameless",
    },
    {
        "case": "materialize-empty-body",
        "op": "materialize",
        "item": {"skillId": "emp", "version": 1, "body": "   "},
        "name": "empty",
    },
    {
        "case": "empty-body-drops-existing-cache",
        "op": "materialize",
        "prior": [{"item": {"skillId": "emp", "version": 1, "body": "# was here"}, "name": "empty"}],
        "item": {"skillId": "emp", "version": 1, "body": "  "},
        "name": "empty",
    },
    {
        "case": "strips-embedded-frontmatter",
        "op": "materialize",
        "item": {
            "skillId": "fm",
            "version": 1,
            "description": "kept",
            "body": "---\nname: ignored\ndescription: ignored\n---\n\n# Real",
        },
        "name": "skill-one",
    },
    {
        "case": "assets-and-exec-bit",
        "op": "materialize",
        "item": {
            "skillId": "tool",
            "version": 1,
            "description": "with assets",
            "body": "# Hi",
            "assets": [
                {"path": "ref/notes.md", "text": "n"},
                {
                    "path": "bin/run",
                    "base64": base64.b64encode(b"#!/bin/sh\n").decode(),
                    "executable": True,
                },
            ],
        },
        "name": "tooling",
    },
    {
        "case": "unsafe-assets-skipped",
        "op": "materialize",
        "item": {
            "skillId": "safe",
            "version": 1,
            "description": "d",
            "body": "# real",
            "assets": [
                {"path": "../escape.txt", "text": "no"},
                {"path": "/absolute.txt", "text": "no"},
                {"path": ".", "text": "no"},
                {"path": "sub/../SKILL.md", "text": "PWNED"},
                {"path": "SKILLS.MD", "text": "PWNED"},
                {"path": "nested/SKILL.md", "text": "allowed deeper"},
                {"path": "undecodable.bin", "base64": "!!not-base64!!"},
                {"path": "kept.md", "text": "kept"},
            ],
        },
        "name": "safe",
    },
    {
        "case": "rematerialize-drops-stale-assets",
        "op": "materialize",
        "prior": [
            {
                "item": {
                    "skillId": "re",
                    "version": 1,
                    "description": "d",
                    "body": "# v1",
                    "assets": [
                        {"path": "keep.md", "text": "k"},
                        {"path": "old.md", "text": "o"},
                    ],
                },
                "name": "re",
            }
        ],
        "item": {
            "skillId": "re",
            "version": 1,
            "description": "d",
            "body": "# v1 again",
            "assets": [{"path": "keep.md", "text": "k"}],
        },
        "name": "re",
    },
    {
        "case": "failed-write-preserves-previous",
        "op": "materialize",
        "failWrite": True,
        "prior": [{"item": {"skillId": "pre", "version": 1, "description": "d", "body": "# good"}, "name": "pre"}],
        "item": {"skillId": "pre", "version": 1, "description": "d", "body": "# new"},
        "name": "pre",
    },
    {
        "case": "latest-materialized",
        "op": "latestMaterialized",
        "prior": [
            {"item": {"skillId": "v", "version": 1, "body": "# one"}, "name": "v"},
            {"item": {"skillId": "v", "version": 3, "body": "# three"}, "name": "v"},
        ],
        "id": "v",
    },
    {"case": "latest-materialized-missing", "op": "latestMaterialized", "id": "none"},
    {
        "case": "export-local",
        "op": "exportLocal",
        "prior": [
            {
                "item": {
                    "skillId": "exp",
                    "version": 2,
                    "metadataName": "exported",
                    "description": "shipped out",
                    "body": "# Exported",
                    "assets": [{"path": "notes.md", "text": "n"}],
                },
                "name": "exported",
            }
        ],
        "id": "exp",
        "version": 2,
    },
    {
        "case": "prune-keeps-active",
        "op": "prune",
        "prior": [
            {"item": {"skillId": "a", "version": 1, "body": "# a1"}, "name": "a"},
            {"item": {"skillId": "a", "version": 2, "body": "# a2"}, "name": "a"},
            {"item": {"skillId": "b", "version": 1, "body": "# b1"}, "name": "b"},
        ],
        "active": [["a", 2]],
    },
]


def _tree_of(root: Path, mask: str | None) -> list[dict[str, Any]]:
    entries = []
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        content = path.read_text(encoding="utf-8")
        if mask is not None:
            content = content.replace(mask, FALLBACK_DESCRIPTION_MARK)
        mode = path.stat().st_mode
        entries.append({
            "path": str(path.relative_to(root)).replace(os.sep, "/"),
            "content": content,
            "exec": {
                "user": bool(mode & 0o100),
                "group": bool(mode & 0o010),
                "other": bool(mode & 0o001),
            },
        })
    return entries


def capture_store() -> dict[str, Any]:
    from vibe.core.skills.parser import parse_skill_markdown
    from vibe.core.skills.registry import _store

    fallback_description: dict[str, Any] | None = None
    cases: list[dict[str, Any]] = []
    for spec in STORE_CASES:
        shutil.rmtree(_store.store_root(), ignore_errors=True)
        record: dict[str, Any] = json.loads(json.dumps(spec))
        for prior in spec.get("prior", []):
            _store._materialize(_item(prior["item"]), prior["name"])

        if spec["op"] == "skillDir":
            try:
                path = _store.skill_dir(spec["id"], spec["version"])
            except ValueError:
                record["error"] = "unsafeId"
            else:
                record["relPath"] = str(path.relative_to(_store.store_root().resolve())).replace(os.sep, "/")
        elif spec["op"] == "materialize":
            mask = None
            original_write = _store._write_assets
            if spec.get("failWrite"):
                def _boom(*_args: Any, **_kwargs: Any) -> None:
                    raise OSError("scripted write failure")

                _store._write_assets = _boom
            try:
                destination = _store._materialize(_item(spec["item"]), spec["name"])
            except OSError:
                record["outcome"] = "raised"
            else:
                record["outcome"] = "stored" if destination is not None else "skipped"
            finally:
                _store._write_assets = original_write
            skill_root = _store.skill_dir(spec["item"]["skillId"], spec["item"]["version"])
            if skill_root.is_dir():
                if spec["case"] == "materialize-fallback-description":
                    frontmatter, _body = parse_skill_markdown(
                        (skill_root / "SKILL.md").read_text(encoding="utf-8")
                    )
                    mask = frontmatter["description"]
                    fallback_description = digest_of(mask)
                record["tree"] = _tree_of(skill_root, mask)
            else:
                record["tree"] = None
            # No staging or backup directory may survive any outcome.
            record["leftoverEntries"] = sorted(
                entry.name
                for entry in skill_root.parent.iterdir()
                if entry.name != skill_root.name
            ) if skill_root.parent.is_dir() else []
        elif spec["op"] == "latestMaterialized":
            record["latest"] = _store._latest_materialized(spec["id"])
        elif spec["op"] == "exportLocal":
            with tempfile.TemporaryDirectory(prefix="vibe-skills-export-") as scratch:
                target = Path(scratch) / "exported"
                _store._export_local(spec["id"], spec["version"], target)
                record["tree"] = _tree_of(target, None)
        elif spec["op"] == "prune":
            _store._prune({(skill, version) for skill, version in spec["active"]})
            record["surviving"] = sorted(
                str(path.relative_to(_store.store_root())).replace(os.sep, "/")
                for path in _store.store_root().rglob("*")
                if path.is_file()
            )
        else:
            raise OracleError(f"unknown store op {spec['op']!r}")
        cases.append(record)

    if fallback_description is None:
        raise OracleError("the fallback-description case did not run")
    shutil.rmtree(_store.store_root(), ignore_errors=True)
    return {"fallbackDescription": fallback_description, "cases": cases}


# --------------------------------------------------------------------------
# Family: manifest (the two registry manifest scopes)
# --------------------------------------------------------------------------


def capture_manifest(scratch: Path) -> list[dict[str, Any]]:
    from types import SimpleNamespace

    from vibe.core.skills.registry import _manifest
    from vibe.core.skills.registry._manifest import ManifestEntry, SkillManifest

    home = Path(os.environ["HOME"])
    workdir = scratch / "manifest"
    shutil.rmtree(workdir, ignore_errors=True)
    workdir.mkdir(parents=True)

    def dump(manifest: SkillManifest) -> list[dict[str, Any]]:
        return jsonable(
            [entry.model_dump() for entry in manifest.skills], "manifest/dump"
        )

    captured: list[dict[str, Any]] = []

    entry_latest = ManifestEntry(name="a", skill_id="x", version="latest")
    entry_frozen = ManifestEntry(name="a", skill_id="x", version=3)
    captured.append({
        "case": "alias-for-string-version",
        "version": "latest",
        "alias": entry_latest.alias,
        "defaultVersionApplied": ManifestEntry(name="d", skill_id="y").version,
    })
    captured.append({"case": "alias-for-integer-version", "version": 3, "alias": entry_frozen.alias})

    manifest = SkillManifest()
    manifest.upsert(ManifestEntry(name="a", skill_id="x", version=1))
    manifest.upsert(ManifestEntry(name="a", skill_id="x", version=2))
    manifest.upsert(ManifestEntry(name="b", skill_id="y", version="latest"))
    captured.append({"case": "upsert-replaces-by-name", "skills": dump(manifest)})

    removed_hit = manifest.remove("a")
    removed_miss = manifest.remove("missing")
    captured.append({
        "case": "remove-by-name",
        "removedExisting": removed_hit,
        "removedMissing": removed_miss,
        "skills": dump(manifest),
    })

    saved_path = workdir / "skills.toml"
    to_save = SkillManifest(
        skills=[
            ManifestEntry(name="grill-me", skill_id="uuid-1", version=3, description="pinned"),
            ManifestEntry(name="fresh", skill_id="uuid-2", version="latest", description=""),
        ]
    )
    _manifest._save(saved_path, to_save)
    captured.append({
        "case": "save-toml-shape",
        "skills": dump(to_save),
        "toml": saved_path.read_text(encoding="utf-8"),
    })
    captured.append({
        "case": "save-load-roundtrip",
        "lossless": _manifest._load(saved_path) == to_save,
        "skills": dump(_manifest._load(saved_path)),
    })

    nested = workdir / "nested" / "deeper" / "skills.toml"
    _manifest._save(nested, SkillManifest())
    captured.append({"case": "save-creates-parents", "created": nested.is_file()})

    captured.append({
        "case": "load-missing-returns-empty",
        "skills": dump(_manifest._load(workdir / "absent.toml")),
    })

    malformed = workdir / "bad.toml"
    malformed.write_text("this is = = not valid toml [[[", encoding="utf-8")
    captured.append({
        "case": "load-malformed-returns-empty",
        "skills": dump(_manifest._load(malformed)),
    })

    wrong_shape = workdir / "wrong-shape.toml"
    wrong_shape.write_text('[[skills]]\nname = "only-a-name"\n', encoding="utf-8")
    captured.append({
        "case": "load-invalid-entry-returns-empty",
        "toml": wrong_shape.read_text(encoding="utf-8"),
        "skills": dump(_manifest._load(wrong_shape)),
    })

    extra_keys = workdir / "extra.toml"
    extra_keys.write_text(
        '[[skills]]\nname = "a"\nskill_id = "x"\nversion = 2\nsurplus = "ignored"\n',
        encoding="utf-8",
    )
    captured.append({
        "case": "load-ignores-unknown-keys",
        "toml": extra_keys.read_text(encoding="utf-8"),
        "skills": dump(_manifest._load(extra_keys)),
    })

    captured.append({
        "case": "global-manifest-under-vibe-home",
        "relPath": str(
            _manifest.global_manifest_path().relative_to(home)
        ).replace(os.sep, "/"),
    })

    project_a = workdir / "proj-a"
    project_b = workdir / "proj-b"
    roots = [home, project_a, project_b, project_a]
    original = _manifest.get_harness_files_manager
    _manifest.get_harness_files_manager = lambda: SimpleNamespace(project_roots=roots)
    try:
        labeled = {home: "home", project_a: "projectA", project_b: "projectB"}
        resolved = _manifest._project_manifest_paths()
        names = []
        for path in resolved:
            owner = next(
                label
                for root, label in labeled.items()
                if path.is_relative_to(root.resolve())
            )
            names.append({
                "root": owner,
                "relPath": str(
                    path.relative_to(next(r for r in labeled if labeled[r] == owner).resolve())
                ).replace(os.sep, "/"),
            })
        captured.append({
            "case": "project-paths-dedup-and-drop-global",
            "roots": ["home", "projectA", "projectB", "projectA"],
            "paths": names,
        })
    finally:
        _manifest.get_harness_files_manager = original

    return captured


# --------------------------------------------------------------------------
# Constants: the builtin catalog, by digest only
# --------------------------------------------------------------------------


def capture_builtins() -> dict[str, Any]:
    from vibe.core.skills.builtins import BUILTIN_SKILLS

    skills = []
    for name in sorted(BUILTIN_SKILLS):
        info = BUILTIN_SKILLS[name]
        skills.append({
            "name": info.name,
            "userInvocable": info.user_invocable,
            "hasPath": info.skill_path is not None,
            "source": info.source.value,
            "scope": info.scope.value,
            "description": digest_of(info.description),
            "prompt": digest_of(info.prompt),
        })
    return {"count": len(BUILTIN_SKILLS), "skills": skills}


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def build_corpus(commit: str, scratch: Path) -> dict[str, Any]:
    # Config construction resolves the system prompt through the harness files
    # singleton, so it must exist. Scoped to the scratch user layer: every path
    # it can reach lives under the scratch HOME.
    from vibe.core.config.harness_files import init_harness_files_manager

    init_harness_files_manager("user")
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": {"commit": commit},
        "note": NOTE,
        "builtins": capture_builtins(),
        "frontmatter": capture_frontmatter(),
        "metadata": capture_metadata(),
        "discovery": capture_discovery(scratch),
        "filtering": capture_filtering(scratch),
        "command": capture_command(scratch),
        "projection": capture_projection(scratch),
        "store": capture_store(),
        "manifest": capture_manifest(scratch),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    parser.add_argument(
        "--python",
        type=Path,
        default=None,
        help="interpreter that can import `vibe`; also read from VIBE_PARITY_PYTHON",
    )
    arguments = parser.parse_args()

    try:
        reference = resolve_reference(arguments.reference, EXPECTED_COMMIT)
        pinned = extract_pinned_tree(arguments.reference, reference["commit"], arguments.cache)
        reexecute_with_reference_interpreter(arguments.reference, arguments.python, pinned)

        with tempfile.TemporaryDirectory(prefix="vibe-skills-parity-") as raw_scratch:
            # The scratch home isolates every path the capture touches: HOME
            # feeds `~` and the `.agents` root, VIBE_HOME feeds the `.vibe`
            # roots and the registry store. Set before any `vibe.core` import,
            # because the agents home is frozen at import time.
            scratch = Path(raw_scratch)
            home = scratch / "home"
            home.mkdir()
            os.environ["HOME"] = str(home)
            os.environ["USERPROFILE"] = str(home)
            os.environ["VIBE_HOME"] = str(home / ".vibe")
            corpus = build_corpus(reference["commit"], scratch)
    except OracleError as error:
        print(f"skills capture failed: {error}", file=sys.stderr)
        return 1

    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    staged = arguments.output.with_name(f"{arguments.output.name}.{os.getpid()}.tmp")
    staged.write_text(
        json.dumps(corpus, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    os.replace(staged, arguments.output)

    counts = {
        family: len(corpus[family]["cases"] if isinstance(corpus[family], dict) else corpus[family])
        for family in (
            "frontmatter",
            "metadata",
            "discovery",
            "filtering",
            "command",
            "projection",
            "store",
            "manifest",
        )
    }
    total = sum(counts.values())
    summary = ", ".join(f"{family} {count}" for family, count in counts.items())
    print(f"wrote {arguments.output} ({total} scenarios: {summary})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
