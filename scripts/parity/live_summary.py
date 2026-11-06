#!/usr/bin/env python3
"""Summarizes what a test run measured against the reference, for a CI job
summary.

The Rust gate ``vibe_core::parity::live`` appends a line to
``target/parity/probes.jsonl`` for every live probe that ran and to
``skips.jsonl`` for every one that skipped; every corpus replay appends one to
``replays.jsonl``; every capture whose reference side ran appends one to
``drivers.jsonl`` (``scripts/parity/reference.py``). This script counts them,
and with a nextest JUnit report classes every failed test:

* **provenance**: a corpus outside the manifest's rules (the messages of
  ``vibe_core::parity::manifest``),
* **environment**: a live probe refused or skipped for want of an attested
  environment, or a materialization that failed,
* **behavior**: any other failure of a test that ran a live probe,
* **other**: a failure outside parity.

Usage::

    python3 scripts/parity/live_summary.py [--records target/parity] [--junit <junit.xml>]
        [--title <heading>] [--require-live] [--require-drivers] [--materialization-failed]
        [--timed-out]

``--require-live`` exits non-zero when any probe skipped, ``--require-drivers``
when a driver the manifest names never ran its reference side, and
``--materialization-failed`` reports an environment failure without any
behavior result, and ``--timed-out`` fails naming the last probe whose test
completed. A shared tool, not a driver: it imports no driver.
"""

from __future__ import annotations

import argparse
from collections import Counter
import json
import os
from pathlib import Path
import sys
from typing import Any
import xml.etree.ElementTree as ElementTree

import provenance

#: Substrings of a failure message that class it.
PROVENANCE_MARKERS = (
    "the corpus breaks its provenance",
    "is not in scripts/parity/manifest.json",
    "matches no entry of scripts/parity/manifest.json",
    "holds no scenarios",
)
ENVIRONMENT_MARKERS = (
    "no longer matches its attestation",
    "no attested environment for",
    "VIBE_PARITY_REQUIRE_LIVE=1 and",
    "partial environment",
)


def default_records() -> Path:
    """``parity/`` under the Cargo target directory, where the Rust gate
    (``vibe_core::parity::live::records``) and ``reference.py`` write."""

    target = Path(os.environ.get("CARGO_TARGET_DIR") or provenance.REPOSITORY / "target")
    if not target.is_absolute():
        target = provenance.REPOSITORY / target
    return target / "parity"


def read_lines(path: Path) -> list[dict[str, Any]]:
    if not path.is_file():
        return []
    lines = []
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        try:
            lines.append(json.loads(line))
        except ValueError:
            continue
    return lines


def completed(junit: Path | None) -> set[str]:
    """Every test case the report holds, passed or failed."""

    if junit is None or not junit.is_file():
        return set()
    return {
        case.get("name") or ""
        for case in ElementTree.parse(junit).getroot().iter("testcase")
        if case.find("skipped") is None
    }


def failures(junit: Path | None) -> list[tuple[str, str]]:
    """Every failed test case of the report, with its failure text."""

    if junit is None or not junit.is_file():
        return []
    found = []
    for case in ElementTree.parse(junit).getroot().iter("testcase"):
        failure = case.find("failure")
        if failure is None:
            failure = case.find("error")
        if failure is None:
            continue
        text = " ".join(
            part
            for part in (
                failure.get("message") or "",
                failure.text or "",
                *(element.text or "" for element in case.iter("system-err")),
                *(element.text or "" for element in case.iter("system-out")),
            )
            if part
        )
        found.append((case.get("name") or "", text))
    return found


def classify(name: str, text: str, probing: set[str]) -> str:
    if any(marker in text for marker in PROVENANCE_MARKERS) or "manifest_tests::" in name:
        return "provenance"
    if any(marker in text for marker in ENVIRONMENT_MARKERS):
        return "environment"
    if name in probing or any(name.endswith(test) or test.endswith(name) for test in probing):
        return "behavior"
    return "other"


def manifest_drivers() -> set[str]:
    drivers = set()
    for entry in provenance.load_manifest()["entries"]:
        driver = entry.get("driver")
        if driver and driver[0].endswith(".py"):
            drivers.add(Path(driver[0]).name)
    return drivers


def summarize(arguments: argparse.Namespace) -> tuple[list[str], int]:
    records = arguments.records
    probes = read_lines(records / "probes.jsonl")
    skips = read_lines(records / "skips.jsonl")
    replays = read_lines(records / "replays.jsonl")
    drivers = read_lines(records / "drivers.jsonl")
    probing = {line["test"] for line in probes if line.get("test")}
    failed = failures(arguments.junit)
    classes = Counter(classify(name, text, probing) for name, text in failed)
    if arguments.materialization_failed:
        classes["environment"] += 1

    lines = [f"### {arguments.title}", ""]
    if arguments.materialization_failed:
        lines += [
            "Materializing a reference environment failed, so no behavior result is reported.",
            "",
        ]
    lines += [
        "| Measure | Count |",
        "|---|---|",
        f"| Corpus replays run | {len(replays)} ({len({line.get('corpus') for line in replays})} corpora) |",
        f"| Live probes run | {len(probes)} |",
        f"| Live probes skipped | {len(skips)} |",
        f"| Reference drivers run | {len({line.get('driver') for line in drivers})} |",
    ]
    if arguments.junit is not None:
        lines.append(f"| Tests failed | {len(failed)} |")
        for name in ("behavior", "environment", "provenance", "other"):
            lines.append(f"| Failed by class: {name} | {classes.get(name, 0)} |")
    lines.append("")
    if skips:
        by_class = Counter(line.get("class", "unknown") for line in skips)
        lines += ["Skips by class: " + ", ".join(f"{name} {count}" for name, count in sorted(by_class.items())), ""]
        lines += ["| Skipped probe | Class | Reason |", "|---|---|---|"]
        for line in skips:
            reason = str(line.get("reason", "")).replace("|", "\\|")
            lines.append(f"| {line.get('probe')} | {line.get('class')} | {reason} |")
        lines.append("")
    if failed:
        lines += ["| Failed test | Class |", "|---|---|"]
        for name, text in failed:
            lines.append(f"| `{name}` | {classify(name, text, probing)} |")
        lines.append("")
    if probes:
        lines += [f"Last probe started: `{probes[-1].get('probe')}` ({probes[-1].get('test')})", ""]
    if arguments.timed_out:
        finished = completed(arguments.junit)
        last = next(
            (line for line in reversed(probes) if line.get("test") in finished), None
        )
        if last:
            named = f"`{last.get('probe')}` ({last.get('test')})"
        elif probes and not finished:
            # Without a report, completion is unknown: name what started last.
            named = (
                f"unknown, no test report was written; the last probe started was "
                f"`{probes[-1].get('probe')}` ({probes[-1].get('test')})"
            )
        else:
            named = "none"
        lines += [f"**The run timed out. Last completed probe: {named}.**", ""]

    status = 0
    if arguments.require_live and skips:
        lines += [f"**{len(skips)} probes skipped under the require-live mode.**", ""]
        status = 1
    if arguments.require_drivers:
        missing = sorted(manifest_drivers() - {line.get("driver") for line in drivers})
        if missing:
            lines += [
                "**Drivers whose reference side never ran:** " + ", ".join(f"`{name}`" for name in missing),
                "",
            ]
            status = 1
    if arguments.materialization_failed or arguments.timed_out:
        status = 1
    return lines, status


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--records", type=Path, default=default_records())
    parser.add_argument("--junit", type=Path, default=None)
    parser.add_argument("--title", default="Parity measurement")
    parser.add_argument("--require-live", action="store_true")
    parser.add_argument("--require-drivers", action="store_true")
    parser.add_argument("--materialization-failed", action="store_true")
    parser.add_argument("--timed-out", action="store_true")
    arguments = parser.parse_args()
    lines, status = summarize(arguments)
    print("\n".join(lines))
    return status


if __name__ == "__main__":
    sys.exit(main())
