"""The normalization half of the sensitivity pass.

A replay's sensitivity pass (`crates/vibe-core/src/parity/sensitivity.rs`)
mutates the port's normalized observations and asks whether the replay would
have failed. What it cannot see is a change the driver's normalization erased
before the replay ever compared anything: an integer where the reference sends
a timestamp string, a counter that resets, a list that lost its order. This
module measures that layer. A black-box driver run with `--raw` keeps beside
each normalized observation the raw run it came from; the same driver run
with `--renormalize <raw> --mutate <operator>` re-normalizes mutated copies of
those raw runs, without starting the subject again, and reports for every
operator how many mutations the normalization erased (the normalized
observation came out unchanged) and where.

The operators, the site ranking and the seed are the ones the Rust pass uses,
so the same raw document samples the same sites on every host.
"""

from __future__ import annotations

import argparse
from collections.abc import Callable
import copy
import json
from pathlib import Path
import sys
from typing import Any

#: The seed every pass samples from; `SEED` in the Rust module.
SEED = 0x5E25_1717_AB1E_0023
#: How many sites per operator a pass mutates by default.
DEFAULT_SAMPLES = 24
#: The key the add-key operator inserts.
PROBE_KEY = "__sensitivityProbe"
#: The operators, in the order the matrix lists them.
OPERATORS = (
    "delete-key",
    "add-key",
    "change-value",
    "change-type",
    "change-length",
    "reorder",
    "drop-element",
)

MASK = (1 << 64) - 1


def rank(seed: int, parts: list[str]) -> int:
    """The Rust pass's `rank`: FNV-1a over the parts, each followed by a zero
    byte, then the SplitMix64 finalizer."""
    value = 0xCBF2_9CE4_8422_2325 ^ seed
    for part in parts:
        for byte in [*part.encode("utf-8"), 0]:
            value ^= byte
            value = (value * 0x0100_0000_01B3) & MASK
    value = ((value ^ (value >> 30)) * 0xBF58_476D_1CE4_E5B9) & MASK
    value = ((value ^ (value >> 27)) * 0x94D0_49BB_1331_11EB) & MASK
    return value ^ (value >> 31)


def is_digest(value: Any) -> bool:
    return (
        isinstance(value, dict)
        and len(value) == 2
        and "sha256" in value
        and "prose" in value
    )


def escape(key: str) -> str:
    return key.replace("~", "~0").replace("/", "~1")


def sites(value: Any, operator: str, pointer: str = "") -> list[str]:
    """The pointers of `value` at which `operator` applies."""
    found: list[str] = []
    scalar = operator in ("change-value", "change-type", "change-length")
    if is_digest(value):
        return [pointer] if scalar else []
    if isinstance(value, dict):
        if operator == "add-key" and PROBE_KEY not in value:
            found.append(pointer)
        for key, child in value.items():
            child_pointer = f"{pointer}/{escape(str(key))}"
            if operator == "delete-key":
                found.append(child_pointer)
            found.extend(sites(child, operator, child_pointer))
    elif isinstance(value, list):
        if operator == "reorder" and any(a != b for a, b in zip(value, value[1:], strict=False)):
            found.append(pointer)
        for index, child in enumerate(value):
            child_pointer = f"{pointer}/{index}"
            if operator == "drop-element":
                found.append(child_pointer)
            found.extend(sites(child, operator, child_pointer))
    elif isinstance(value, str):
        if scalar:
            found.append(pointer)
    elif isinstance(value, bool | int | float):
        if operator in ("change-value", "change-type"):
            found.append(pointer)
    elif value is None and operator == "change-type":
        found.append(pointer)
    return found


def _segments(pointer: str) -> list[str]:
    return [part.replace("~1", "/").replace("~0", "~") for part in pointer.split("/")[1:]]


def _resolve(document: Any, segments: list[str]) -> Any:
    for segment in segments:
        document = document[int(segment)] if isinstance(document, list) else document[segment]
    return document


def _flip_last(text: str) -> str:
    return text[:-1] + ("y" if text[-1] == "x" else "x")


def _changed_scalar(value: Any, operator: str) -> Any:
    if is_digest(value):
        changed = dict(value)
        if operator == "change-value":
            changed["sha256"] = _flip_last(str(value["sha256"]))
        elif operator == "change-length":
            changed["prose"] = value["prose"] + 1
        else:
            return ""
        return changed
    if operator == "change-value":
        if isinstance(value, bool):
            return not value
        if isinstance(value, int):
            return value + 1
        if isinstance(value, float):
            return value + 1.0
        if isinstance(value, str):
            return _flip_last(value) if value else "x"
    if operator == "change-type":
        if isinstance(value, str):
            return len(value)
        if isinstance(value, bool):
            return str(value).lower()
        if isinstance(value, int | float):
            return str(value)
        if value is None:
            return False
    if operator == "change-length" and isinstance(value, str):
        return value + "x"
    raise ValueError(f"{operator} does not apply to {value!r}")


def mutate(value: Any, operator: str, pointer: str) -> Any:
    """A copy of `value` with `operator` applied at `pointer`, a site
    :func:`sites` returned."""
    mutated = copy.deepcopy(value)
    segments = _segments(pointer)
    if operator in ("delete-key", "drop-element"):
        parent = _resolve(mutated, segments[:-1])
        if isinstance(parent, list):
            del parent[int(segments[-1])]
        else:
            del parent[segments[-1]]
        return mutated
    if operator == "add-key":
        _resolve(mutated, segments)[PROBE_KEY] = True
        return mutated
    if operator == "reorder":
        items = _resolve(mutated, segments)
        items.append(items.pop(0))
        return mutated
    replacement = _changed_scalar(_resolve(mutated, segments), operator)
    if not segments:
        return replacement
    parent = _resolve(mutated, segments[:-1])
    if isinstance(parent, list):
        parent[int(segments[-1])] = replacement
    else:
        parent[segments[-1]] = replacement
    return mutated


def add_arguments(parser: argparse.ArgumentParser) -> None:
    """The two flags every black-box driver that keeps `--raw` accepts."""
    parser.add_argument(
        "--renormalize",
        type=Path,
        default=None,
        help="re-normalize mutated copies of the raw runs this capture (taken with --raw) holds",
    )
    parser.add_argument(
        "--mutate",
        choices=[*OPERATORS, "all"],
        default="all",
        help="the operator --renormalize applies",
    )
    parser.add_argument(
        "--report",
        type=Path,
        default=None,
        help="where --renormalize writes its report (stdout otherwise)",
    )
    parser.add_argument(
        "--samples",
        default=str(DEFAULT_SAMPLES),
        help="sites per operator --renormalize mutates, or `all`",
    )


Normalize = Callable[[dict[str, Any], dict[str, Any]], Any]


def renormalize(
    raw: dict[str, Any],
    normalize: Normalize,
    operators: list[str],
    samples: int | None,
    driver: str,
) -> dict[str, Any]:
    """What normalization erases: for every operator, the mutations of the
    raw runs `raw` holds whose normalized observation came out unchanged."""
    entries = [entry for entry in raw.get("scenarios", []) if "raw" in entry]
    if not entries:
        raise ValueError("the capture holds no raw run; capture it again with --raw")
    baseline = [normalize(entry["scenario"], entry["raw"]) for entry in entries]
    report: dict[str, Any] = {
        "driver": driver,
        "seed": f"{SEED:#018x}",
        "samples": "all" if samples is None else samples,
        "operators": {},
    }
    for operator in operators:
        candidates = sorted(
            (rank(SEED, [driver, operator, entry["name"], pointer]), index, pointer)
            for index, entry in enumerate(entries)
            for pointer in sites(entry["raw"], operator)
        )
        chosen = candidates if samples is None else candidates[:samples]
        erased: list[dict[str, str]] = []
        refused = 0
        for _, index, pointer in chosen:
            entry = entries[index]
            mutated = mutate(entry["raw"], operator, pointer)
            if mutated == entry["raw"]:
                continue
            try:
                observed = normalize(entry["scenario"], mutated)
            except Exception:  # noqa: BLE001 - a raw run the normalizer cannot read is visible
                refused += 1
                continue
            if observed == baseline[index]:
                erased.append({"scenario": entry["name"], "pointer": pointer})
        report["operators"][operator] = {
            "sites": len(candidates),
            "applied": len(chosen),
            "refused": refused,
            "erased": len(erased),
            "examples": erased[:10],
        }
    return report


def main(arguments: argparse.Namespace, normalize: Normalize, driver: str) -> int | None:
    """Runs `--renormalize` when the driver was asked for it, writing the
    report to `--output` (or stdout), and returns the exit status; returns
    `None` when the flag is absent, so the driver captures as usual."""
    if arguments.renormalize is None:
        return None
    raw = json.loads(arguments.renormalize.read_text(encoding="utf-8"))
    operators = list(OPERATORS) if arguments.mutate == "all" else [arguments.mutate]
    samples = None if arguments.samples == "all" else int(arguments.samples)
    try:
        report = renormalize(raw, normalize, operators, samples, driver)
    except ValueError as error:
        print(f"{driver}: {error}", file=sys.stderr)
        return 1
    text = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if arguments.report is not None:
        arguments.report.parent.mkdir(parents=True, exist_ok=True)
        arguments.report.write_text(text, encoding="utf-8")
    else:
        sys.stdout.write(text)
    return 0
