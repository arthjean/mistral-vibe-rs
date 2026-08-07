#!/usr/bin/env python3
"""Capture the opcode sequence every checkpoint region identity is numbered by.

A region in the checkpoint engine is identified by its producing edit's
sequence number and its position in that edit's opcode list. A client sends
that pair back across turns, so the opcode list is a contract rather than a
rendering: a diff of equal quality but different shape resolves a client's
target to a different change. Nothing downstream of it can be measured while
the two sides can disagree about it, which is why this is the first oracle of
the checkpoints work.

Two things are captured per fixture, both from the pinned reference:

* what ``_decode_lines`` answers, which is ``decode_safe`` followed by
  ``str.splitlines(keepends=True)`` and is where the eight non-newline boundary
  characters and the binary and absent states are decided
  (``vibe/core/checkpoints/history.py:25``);
* what ``difflib.SequenceMatcher(autojunk=False).get_opcodes()`` answers over
  those lines, constructed exactly as ``_regions`` constructs it
  (``vibe/core/checkpoints/history.py:52``).

The fixture inputs are authored here, so they are committed as they stand. The
line sequences the reference produced from them are committed as a SHA-256
digest with the per-line lengths beside it, which pins every boundary without
storing a second copy of the text, and the opcode tuples are committed verbatim
because they are the contract. Nothing in the corpus comes from the reference's
prose.

Usage::

    scripts/parity/checkpoint_opcodes.py --reference /path/to/reference

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it. The capture reads the pinned commit
through ``git archive`` and never moves ``HEAD``, so a workstation whose
checkout has moved on can still capture.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import sys
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT

#: The pinned-tree mechanics are the tool-execution oracle's, reused rather than
#: copied: both scripts must read the same commit the same way, and two copies
#: of that logic would be two things to keep in agreement.
from tool_execution import (
    OracleError,
    extract_pinned_tree,
    reexecute_with_reference_interpreter,
    resolve_reference,
)

SCHEMA_VERSION = 1
DEFAULT_OUTPUT = Path("crates/vibe-core/tests/checkpoints/opcodes.json")
DEFAULT_CACHE = Path(".parity")

#: The floor the replay asserts, so a regeneration that captured almost nothing
#: fails instead of reporting a clean but empty run.
MINIMUM_OPCODE_FIXTURES = 30


# --------------------------------------------------------------------------
# Fixtures
# --------------------------------------------------------------------------


def _numbered(count: int, prefix: str = "body") -> str:
    return "".join(f"{prefix} {index}\n" for index in range(count))


def _above_threshold_with_a_popular_line() -> str:
    """250 lines carrying one line five times.

    CPython drops an element of the second sequence occurring more than
    ``len(b) // 100 + 1`` times once that sequence reaches 200 elements. Five
    occurrences in 250 lines is over that bound, so this fixture answers one way
    with the heuristic and another without it, which is the whole point of the
    reference passing ``autojunk=False``.
    """

    lines = []
    for index in range(250):
        lines.append("shared\n" if index % 50 == 10 else f"body {index}\n")
    return "".join(lines)


def line_fixtures() -> list[dict[str, Any]]:
    """Single-side inputs, which measure the split rather than the diff.

    ``text`` of ``None`` is an absent file; a text carrying a NUL is binary. The
    reference answers those two apart, and an engine that confused them would
    report a deleted file as unreadable.
    """

    boundaries = [
        ("vertical-tab", "\v"),
        ("form-feed", "\f"),
        ("file-separator", "\x1c"),
        ("group-separator", "\x1d"),
        ("record-separator", "\x1e"),
        ("next-line", "\x85"),
        ("line-separator", "\u2028"),
        ("paragraph-separator", "\u2029"),
    ]
    fixtures: list[dict[str, Any]] = [
        {"name": "absent", "text": None},
        {"name": "empty", "text": ""},
        {"name": "one-line-with-a-newline", "text": "only line\n"},
        {"name": "one-line-without-a-newline", "text": "only line"},
        {"name": "many-lines", "text": "one\ntwo\nthree\n"},
        {"name": "many-lines-without-a-final-newline", "text": "one\ntwo\nthree"},
        {"name": "blank-lines-only", "text": "\n\n\n"},
        {"name": "crlf-endings", "text": "one\r\ntwo\r\n"},
        {"name": "lone-carriage-returns", "text": "one\rtwo\r"},
        {"name": "mixed-endings", "text": "one\r\ntwo\rthree\n"},
        {"name": "trailing-whitespace", "text": "one   \n\ttwo\n"},
        {"name": "utf8-multibyte", "text": "café\nnaïve\n"},
        {"name": "binary-with-an-embedded-nul", "text": "before\x00after\n"},
        {"name": "binary-with-a-trailing-nul", "text": "text\n\x00"},
    ]
    for name, separator in boundaries:
        fixtures.append({"name": name, "text": f"head{separator}tail"})
        fixtures.append({"name": f"{name}-at-the-end", "text": f"head{separator}"})
    fixtures.append(
        {
            "name": "every-boundary-together",
            "text": "a\vb\fc\x1cd\x1de\x1ef\x85g\u2028h\u2029i",
        }
    )
    fixtures.append({"name": "leading-boundary", "text": "\x85tail"})
    return fixtures


def opcode_fixtures() -> list[dict[str, Any]]:
    """Input pairs, ordered by the shape of the change they exercise."""

    body = "one\ntwo\nthree\nfour\nfive\n"
    return [
        {"name": "identical-single-line", "a": "only\n", "b": "only\n"},
        {"name": "identical-multi-line", "a": body, "b": body},
        {"name": "both-empty", "a": "", "b": ""},
        {"name": "empty-into-content", "a": "", "b": body},
        {"name": "content-into-empty", "a": body, "b": ""},
        {"name": "disjoint-lines", "a": "alpha\nbeta\n", "b": "gamma\ndelta\nepsilon\n"},
        {"name": "shared-prefix", "a": "head\nold\n", "b": "head\nnew\nextra\n"},
        {"name": "shared-suffix", "a": "old\ntail\n", "b": "new\nextra\ntail\n"},
        {
            "name": "shared-prefix-and-suffix",
            "a": "head\nmiddle\ntail\n",
            "b": "head\nchanged\ntail\n",
        },
        {
            "name": "one-line-changed-in-the-middle",
            "a": body,
            "b": "one\ntwo\nTHREE\nfour\nfive\n",
        },
        {"name": "insertion-at-the-top", "a": body, "b": "zero\n" + body},
        {"name": "insertion-at-the-bottom", "a": body, "b": body + "six\n"},
        {
            "name": "insertion-in-the-middle",
            "a": body,
            "b": "one\ntwo\ninserted\nthree\nfour\nfive\n",
        },
        {"name": "deletion-at-the-top", "a": body, "b": "two\nthree\nfour\nfive\n"},
        {"name": "deletion-at-the-bottom", "a": body, "b": "one\ntwo\nthree\nfour\n"},
        {"name": "deletion-in-the-middle", "a": body, "b": "one\ntwo\nfour\nfive\n"},
        {
            "name": "two-separate-insertions",
            "a": body,
            "b": "one\nadded a\ntwo\nthree\nadded b\nfour\nfive\n",
        },
        {"name": "two-separate-deletions", "a": body, "b": "one\nthree\nfive\n"},
        {
            "name": "interleaved-replace-and-equal",
            "a": body,
            "b": "ONE\ntwo\nTHREE\nfour\nFIVE\n",
        },
        {"name": "single-line-into-many", "a": "one\n", "b": "one\ntwo\nthree\nfour\n"},
        {"name": "many-into-a-single-line", "a": "one\ntwo\nthree\nfour\n", "b": "one\n"},
        {"name": "repeated-line-twice", "a": "same\nx\nsame\n", "b": "same\ny\nsame\n"},
        {
            "name": "repeated-block-moved",
            "a": "block\nblock\nhead\ntail\n",
            "b": "head\ntail\nblock\nblock\n",
        },
        {
            "name": "duplicate-blocks",
            "a": "alpha\nbeta\nalpha\nbeta\n",
            "b": "alpha\nbeta\ngamma\nalpha\nbeta\n",
        },
        {"name": "blank-line-added", "a": "one\ntwo\n", "b": "one\n\ntwo\n"},
        {"name": "whitespace-only-change", "a": "value\n", "b": "  value\n"},
        {
            "name": "trailing-newline-added",
            "a": "one\ntwo",
            "b": "one\ntwo\n",
        },
        {
            "name": "trailing-newline-removed",
            "a": "one\ntwo\n",
            "b": "one\ntwo",
        },
        {
            "name": "crlf-normalized-before-the-diff",
            "a": "one\r\ntwo\r\n",
            "b": "one\r\nchanged\r\n",
        },
        {
            "name": "boundary-characters-changed",
            "a": "a\vb\fc\u2028d\n",
            "b": "a\vB\fc\u2028d\n",
        },
        {
            "name": "above-the-threshold-identical",
            "a": _numbered(220),
            "b": _numbered(220),
        },
        {
            "name": "above-the-threshold-single-edit",
            "a": _numbered(220),
            "b": _numbered(220).replace("body 100\n", "changed 100\n"),
        },
        {
            "name": "above-the-threshold-with-a-repeated-line",
            "a": _above_threshold_with_a_popular_line(),
            "b": _above_threshold_with_a_popular_line().replace(
                "body 7\n", "changed 7\n"
            ),
        },
        {
            "name": "above-the-threshold-popular-line-is-the-only-match",
            "a": "first\nshared\nlast\n",
            "b": _above_threshold_with_a_popular_line(),
        },
        {
            "name": "above-the-threshold-block-inserted",
            "a": _numbered(210),
            "b": _numbered(210) + _numbered(20, prefix="tail"),
        },
        {
            "name": "above-the-threshold-uniform-lines",
            "a": "same\n" * 205,
            "b": "same\n" * 200 + "other\n" * 5,
        },
    ]


# --------------------------------------------------------------------------
# Driving the reference
# --------------------------------------------------------------------------


def digest(lines: list[str]) -> str:
    """A line sequence's identity, boundaries included.

    Digesting the concatenation would not do: joining keepends lines rebuilds
    the input exactly, so two different splits of the same text would digest
    alike and the one thing this fixture measures would go unmeasured. Prefixing
    each line with its length makes the boundary set part of the digest.
    """

    body = "".join(f"{len(line)}:{line}" for line in lines)
    return "sha256:" + hashlib.sha256(body.encode("utf-8")).hexdigest()[:32]


def observed_lines(text: str | None) -> tuple[list[str] | None, dict[str, Any] | None]:
    """What the reference's ``_decode_lines`` answers for ``text``."""

    from vibe.core.checkpoints.history import _decode_lines
    from vibe.core.checkpoints.models import FileState

    state = FileState.absent() if text is None else FileState.from_text(text)
    lines = _decode_lines(state)
    if lines is None:
        return None, None
    return lines, {
        "count": len(lines),
        "lengths": [len(line) for line in lines],
        "digest": digest(lines),
    }


def capture_line_fixture(fixture: dict[str, Any]) -> dict[str, Any]:
    lines, summary = observed_lines(fixture["text"])
    return {
        "name": fixture["name"],
        "text": fixture["text"],
        "binary": lines is None,
        "lines": summary,
    }


def capture_opcode_fixture(fixture: dict[str, Any]) -> dict[str, Any]:
    from difflib import SequenceMatcher

    from vibe.core.checkpoints.history import _regions

    sides = {}
    resolved = {}
    for key in ("a", "b"):
        lines, summary = observed_lines(fixture[key])
        if lines is None or summary is None:
            raise OracleError(
                f"{fixture['name']}: side {key} is binary, which carries no opcodes"
            )
        resolved[key] = lines
        sides[key] = {"text": fixture[key], "lines": summary}

    opcodes = SequenceMatcher(
        a=resolved["a"], b=resolved["b"], autojunk=False
    ).get_opcodes()

    # The reference's own consumer must agree with the raw opcode list, or this
    # corpus would pin something `_regions` does not actually use.
    regions = list(_regions(resolved["a"], resolved["b"]))
    changes = [opcode for opcode in opcodes if opcode[0] != "equal"]
    if len(regions) != len(changes):
        raise OracleError(
            f"{fixture['name']}: {len(changes)} non-equal opcodes but "
            f"{len(regions)} regions"
        )

    return {
        "name": fixture["name"],
        "a": sides["a"],
        "b": sides["b"],
        "opcodes": [list(opcode) for opcode in opcodes],
    }


def build_corpus(commit: str) -> dict[str, Any]:
    lines = [capture_line_fixture(fixture) for fixture in line_fixtures()]
    opcodes = [capture_opcode_fixture(fixture) for fixture in opcode_fixtures()]
    if len(opcodes) < MINIMUM_OPCODE_FIXTURES:
        raise OracleError(
            f"{len(opcodes)} opcode fixtures, below the floor of "
            f"{MINIMUM_OPCODE_FIXTURES}"
        )
    names = [fixture["name"] for fixture in lines + opcodes]
    if len(names) != len(set(names)):
        raise OracleError("two fixtures share a name, so a divergence could not name one")
    return {
        "schemaVersion": SCHEMA_VERSION,
        "referenceCommit": commit,
        "note": (
            "Checkpoint opcode corpus: what the pinned reference's _decode_lines and "
            "difflib.SequenceMatcher(autojunk=False) answer for fixtures authored in "
            "scripts/parity/checkpoint_opcodes.py. Inputs are this repository's own and are "
            "committed as they stand; the line sequences the reference produced are committed "
            "as a SHA-256 digest with their per-line lengths, and the opcode tuples verbatim "
            "because a region ordinal is that tuple's position. No reference-authored text is "
            "stored. Regenerate with scripts/parity/checkpoint_opcodes.py when the pinned "
            "reference moves."
        ),
        "lineFixtures": lines,
        "opcodeFixtures": opcodes,
    }


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--python", type=Path, default=None)
    parser.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        reference = resolve_reference(arguments.reference, arguments.expected_commit)
        pinned = extract_pinned_tree(
            arguments.reference, reference["commit"], arguments.cache
        )
        reexecute_with_reference_interpreter(arguments.reference, arguments.python, pinned)
        corpus = build_corpus(reference["commit"])
    except OracleError as error:
        print(f"checkpoint-opcode capture failed: {error}", file=sys.stderr)
        return 1
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    staged = arguments.output.with_name(f"{arguments.output.name}.{os.getpid()}.tmp")
    staged.write_text(
        json.dumps(corpus, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    os.replace(staged, arguments.output)
    print(
        f"captured {len(corpus['lineFixtures'])} line fixtures and "
        f"{len(corpus['opcodeFixtures'])} opcode fixtures from "
        f"{reference['commit'][:12]} into {arguments.output}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
