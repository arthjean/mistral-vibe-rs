#!/usr/bin/env python3
"""Capture what the pinned Python reference's compaction still computes in Python.

Through 2.26.0 the reference compacted in Python: the pure half lived in
``vibe/core/compaction/context.py`` and ``vibe/core/utils/tokens.py``, the
orchestrating half in ``vibe/core/compaction/manager.py``, and this capture
drove both. At 3.0.0 compaction moved into the Rust harness
(``harness/core/src/core/features/compaction/``: ``context.rs``,
``orchestration.rs``, ``execution.rs``, ``overflow.rs``, ``projection.rs``,
``prompts.rs``), ``manager.py`` and ``tokens.py`` were deleted, and
``context.py`` kept a single function this capture can drive,
``extract_summary``, which ``vibe/app_server/_unified_vibe_code.py`` still
calls on a teleport summary.

One family is therefore recorded, and the Rust replay compares it:

``summaryExtractions`` the summary parser and its empty-summary signal

The families that measured what left the package are dropped, not emulated:
``constants`` (the preservation budget), ``tokenCounts`` and ``truncations``
(``tokens.py``), ``roundDrops``, ``envelopeRenders``, ``envelopeParses``,
``envelopeProse`` and ``messageSelections`` (the envelope functions of
``context.py``), and ``managerScenarios`` with ``placeholderSummary``
(``manager.py``). What the harness compacts is reached through the session
surfaces, by the drivers that own them. Schema version 4 is that smaller shape.

Two artifacts come out of a run::

    .parity/compaction-corpus.json          the full capture, gitignored
    crates/vibe-core/tests/compaction/corpus.json   the committed projection

The parser answers with substrings of the text the case supplied, so the
committed projection is the capture without its run metadata, and carries no
reference-authored text.

Usage::

    scripts/parity/compaction.py --reference /path/to/reference --corpus

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it. The wrapper re-executes itself with
the reference interpreter, reading the pinned commit through ``git archive`` so
the checkout is never moved.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import platform
import sys
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT
from reference import DEFAULT_CACHE, OracleError, reexecute_pinned
from compaction_cases import summary_cases

SCHEMA_VERSION = 4
DEFAULT_OUTPUT = Path(".parity/compaction-corpus.json")
DEFAULT_CORPUS = Path("crates/vibe-core/tests/compaction/corpus.json")
FAMILIES = ("summaryExtractions",)


# --------------------------------------------------------------------------
# Capture
# --------------------------------------------------------------------------


def capture_pure() -> dict[str, Any]:
    from vibe.core.compaction.context import extract_summary

    return {
        "summaryExtractions": [
            {**case, "summary": extract_summary(case["text"])} for case in summary_cases()
        ],
    }


def build_corpus(capture: dict[str, Any], reference: dict[str, str]) -> dict[str, Any]:
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": {"commit": reference["commit"]},
        "note": (
            "Compaction corpus: what the pinned reference's summary parser answers for each "
            "scripted input. Since 3.0.0 the rest of compaction runs in the reference's Rust "
            "harness, outside what this capture drives. Every value is a case input or a "
            "substring of one. Regenerate with scripts/parity/compaction.py --corpus when the "
            "pinned reference moves."
        ),
        **capture,
    }


def _write(path: Path, document: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    staged = path.with_name(f"{path.name}.{os.getpid()}.tmp")
    staged.write_text(
        json.dumps(document, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
        encoding="utf-8",
        newline="",
    )
    os.replace(staged, path)


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--python", type=Path, default=None)
    parser.add_argument(
        "--corpus",
        type=Path,
        nargs="?",
        const=DEFAULT_CORPUS,
        default=None,
        help=(
            "also write the committed corpus, which the Rust replay reads unconditionally "
            f"(default {DEFAULT_CORPUS})"
        ),
    )
    parser.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        commit, _ = reexecute_pinned(
            arguments.reference,
            arguments.expected_commit,
            cache=arguments.cache,
            override=arguments.python,
        )
        reference = {"commit": commit, "path": str(arguments.reference)}
        try:
            capture = capture_pure()
        except ImportError as error:
            raise OracleError(
                f"the reference at {commit[:12]} no longer exposes the summary parser: {error}"
            ) from None
    except OracleError as error:
        print(f"compaction capture failed: {error}", file=sys.stderr)
        return 1

    _write(
        arguments.output,
        {
            "schemaVersion": SCHEMA_VERSION,
            "reference": reference,
            "platform": platform.system().lower(),
            "python": platform.python_version(),
            **capture,
        },
    )
    if arguments.corpus is not None:
        _write(arguments.corpus, build_corpus(capture, reference))
    total = sum(len(capture[family]) for family in FAMILIES)
    print(
        f"captured {total} scenarios across {len(FAMILIES)} families "
        f"from {reference['commit'][:12]} into {arguments.output}"
    )
    for family in FAMILIES:
        print(f"  {family}: {len(capture[family])}")
    if arguments.corpus is not None:
        print(f"wrote the committed corpus to {arguments.corpus}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
