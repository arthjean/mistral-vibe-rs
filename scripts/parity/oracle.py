#!/usr/bin/env python3
"""Record chat-input parity traces from the pinned Python reference.

The reference checkout is a read-only behavioral oracle. This harness drives
the real ``ChatInputContainer`` through Textual's headless pilot, records
normalized state, effect and render observations, and writes a versioned trace
corpus consumed by the Rust differential runner.

Usage::

    scripts/parity/oracle.py --python /path/to/reference/.venv/bin/python

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it.

The wrapper re-executes itself with the reference interpreter when the current
one cannot import ``vibe`` and ``textual``.
"""

from __future__ import annotations

import argparse
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
DEFAULT_OUTPUT = Path("crates/vibe-cli/tests/parity")


class OracleError(RuntimeError):
    """Raised when the corpus cannot be produced from an authoritative state."""


# --------------------------------------------------------------------------
# Reference pinning
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


def resolve_reference(reference: Path, expected_commit: str | None) -> dict[str, str]:
    if not reference.is_dir():
        raise OracleError(f"reference checkout is missing: {reference}")
    commit = _git(reference, "rev-parse", "HEAD")
    tracked_changes = _git(reference, "status", "--porcelain", "--untracked-files=no")
    if tracked_changes:
        raise OracleError(
            f"reference checkout {reference} has uncommitted tracked changes; "
            "the oracle revision must be reproducible"
        )
    if expected_commit and commit != expected_commit:
        raise OracleError(
            f"reference commit mismatch: expected {expected_commit}, found {commit}"
        )
    # The checkout path is host specific and never enters the corpus: the
    # commit and version identify the oracle reproducibly.
    return {"commit": commit, "version": _reference_version(reference)}


def _reference_version(reference: Path) -> str:
    pyproject = reference / "pyproject.toml"
    if not pyproject.is_file():
        raise OracleError(f"reference version is unavailable: {pyproject} is missing")
    for line in pyproject.read_text(encoding="utf-8").splitlines():
        if line.startswith("version = "):
            return line.split("=", 1)[1].strip().strip('"')
    raise OracleError(f"reference version is unavailable: no version in {pyproject}")


# --------------------------------------------------------------------------
# Workspace fixtures
# --------------------------------------------------------------------------

WORKSPACES: dict[str, dict[str, str | None]] = {
    "empty": {},
    "sample": {
        "README.md": "# sample\n",
        "notes.txt": "context\n",
        ".gitignore": "ignored/\n*.log\n",
        ".hidden": "hidden\n",
        "ignored/secret.txt": "secret\n",
        "build.log": "log\n",
        "src/lib.rs": "pub fn nested() {}\n",
        "src/main.rs": "fn main() {}\n",
        "src/inner/deep.rs": "pub fn deep() {}\n",
        "docs/guide.md": "# guide\n",
        "image one.png": "\x89PNG\r\n\x1a\n",
    },
}


def build_workspace(root: Path, name: str) -> None:
    files = WORKSPACES.get(name)
    if files is None:
        raise OracleError(f"unknown workspace fixture: {name}")
    for relative, content in files.items():
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content or "", encoding="utf-8")


# --------------------------------------------------------------------------
# Recording
# --------------------------------------------------------------------------


async def record_scenario(scenario: dict[str, Any], workspace: Path) -> dict[str, Any]:
    # Imported lazily so `--list` and pinning checks work without the reference
    # environment on the current interpreter.
    from harness import ScenarioRunner  # type: ignore[import-not-found]

    runner = ScenarioRunner(scenario, workspace)
    return await runner.run()


def missing_capabilities(scenario: dict[str, Any]) -> list[str]:
    available = {"core"}
    if sys.platform == "darwin":
        available.add("macos-clipboard")
    return sorted(set(scenario.get("capabilities", ["core"])) - available)


# --------------------------------------------------------------------------
# Corpus writing
# --------------------------------------------------------------------------


def write_corpus(
    output: Path,
    reference: dict[str, str],
    gaps: list[dict[str, str]],
    traces: list[dict[str, Any]],
    unavailable: list[dict[str, Any]],
) -> None:
    staging = Path(tempfile.mkdtemp(prefix=".parity-corpus-", dir=output.parent))
    try:
        trace_directory = staging / "traces"
        trace_directory.mkdir(parents=True)
        entries = []
        for trace in traces:
            name = f"{trace['id']}.json"
            (trace_directory / name).write_text(
                json.dumps(trace, indent=2, ensure_ascii=False, sort_keys=True) + "\n",
                encoding="utf-8",
            )
            entries.append(
                {
                    "id": trace["id"],
                    "gap": trace["gap"],
                    "story": trace["story"],
                    "file": f"traces/{name}",
                    "capabilities": trace["capabilities"],
                }
            )
        manifest = {
            "schemaVersion": SCHEMA_VERSION,
            "reference": reference,
            "workspaces": WORKSPACES,
            "gaps": gaps,
            "traces": sorted(entries, key=lambda entry: entry["id"]),
            "unavailable": sorted(unavailable, key=lambda entry: entry["id"]),
        }
        (staging / "manifest.json").write_text(
            json.dumps(manifest, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
        )
        previous = output.with_name(f"{output.name}.previous")
        shutil.rmtree(previous, ignore_errors=True)
        if output.exists():
            # Preserve hand-owned files (expectations) across regeneration.
            for keep in output.iterdir():
                if keep.name in {"manifest.json", "traces"}:
                    continue
                shutil.copy2(keep, staging / keep.name)
            output.rename(previous)
        staging.rename(output)
        shutil.rmtree(previous, ignore_errors=True)
    finally:
        shutil.rmtree(staging, ignore_errors=True)


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def parse_arguments(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    # The pin is the default, so a capture from an off-pin checkout fails here
    # rather than writing a corpus the Rust replay would then reject. Passing an
    # explicit value still overrides it, and an empty one disables the check.
    parser.add_argument("--expect-commit", default=EXPECTED_COMMIT)
    parser.add_argument("--only", default=None, help="record a single scenario id")
    parser.add_argument("--list", action="store_true", help="list scenario ids and exit")
    parser.add_argument("--dump", action="store_true", help="print the trace recorded by --only")
    parser.add_argument(
        "--python",
        type=Path,
        default=None,
        help="interpreter owning the reference environment",
    )
    return parser.parse_args(argv)


def reexecute_with(interpreter: Path, argv: list[str]) -> int:
    command = [str(interpreter), str(Path(__file__).resolve()), *argv]
    return subprocess.run(command, check=False).returncode


def reference_environment_available() -> bool:
    try:
        import textual  # noqa: F401
        import vibe  # noqa: F401
    except ImportError:
        return False
    return True


def main(argv: list[str]) -> int:
    arguments = parse_arguments(argv)
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from scenarios import GAPS, SCENARIOS  # type: ignore[import-not-found]

    if arguments.list:
        for scenario in SCENARIOS:
            print(f"{scenario['id']}\t{scenario['gap']}\t{scenario['story']}")
        return 0

    reference = arguments.reference.resolve()
    if not reference_environment_available():
        interpreter = arguments.python or (reference / ".venv/bin/python")
        if not interpreter.is_file():
            raise OracleError(
                "reference Python environment is unavailable; pass --python with an "
                f"interpreter that can import vibe and textual (tried {interpreter})"
            )
        forwarded = [argument for argument in argv if not argument.startswith("--python")]
        return reexecute_with(interpreter, forwarded)

    pinned = resolve_reference(reference, arguments.expect_commit)
    selected = [
        scenario
        for scenario in SCENARIOS
        if arguments.only is None or scenario["id"] == arguments.only
    ]
    if arguments.only and not selected:
        raise OracleError(f"unknown scenario id: {arguments.only}")

    identifiers = [scenario["id"] for scenario in selected]
    if len(set(identifiers)) != len(identifiers):
        raise OracleError("scenario ids must be unique")

    import asyncio

    traces: list[dict[str, Any]] = []
    unavailable: list[dict[str, Any]] = []
    origin = Path.cwd()
    for scenario in selected:
        missing = missing_capabilities(scenario)
        if missing:
            # A scenario whose capability is absent is never written as an
            # authoritative fixture. It is declared in the manifest so the
            # differential runner reports an explicit hole instead of a pass.
            if arguments.only is not None:
                raise OracleError(
                    f"scenario {scenario['id']} requires unavailable capabilities "
                    f"{missing} on this host"
                )
            unavailable.append({
                "id": scenario["id"],
                "gap": scenario["gap"],
                "story": scenario["story"],
                "capabilities": scenario.get("capabilities", ["core"]),
                "missingCapabilities": missing,
            })
            print(
                f"skipped {scenario['id']}: missing capabilities {missing}",
                file=sys.stderr,
            )
            continue
        with tempfile.TemporaryDirectory(prefix="vibe-parity-") as directory:
            workspace = Path(directory)
            build_workspace(workspace, scenario.get("workspace", "empty"))
            os.chdir(workspace)
            try:
                trace = asyncio.run(record_scenario(scenario, workspace))
            finally:
                os.chdir(origin)
        trace["schemaVersion"] = SCHEMA_VERSION
        trace["reference"] = pinned
        traces.append(trace)
        print(
            f"recorded {trace['id']} ({len(trace['observations'])} observations)",
            file=sys.stderr,
        )

    known_gaps = {gap["id"] for gap in GAPS}
    covered = {trace["gap"] for trace in traces}
    if arguments.only is None:
        uncovered = sorted(known_gaps - covered)
        if uncovered:
            raise OracleError(f"gaps without a trace: {uncovered}")
        unknown = sorted(covered - known_gaps)
        if unknown:
            raise OracleError(f"traces reference unknown gaps: {unknown}")

    output = (origin / arguments.output).resolve()
    if arguments.only is not None:
        if arguments.dump:
            print(json.dumps(traces[0], indent=2, ensure_ascii=False))
        print("single-scenario run: corpus was not rewritten", file=sys.stderr)
        return 0
    output.parent.mkdir(parents=True, exist_ok=True)
    write_corpus(output, pinned, GAPS, traces, unavailable)
    print(f"wrote {len(traces)} traces to {output}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except OracleError as error:
        print(f"oracle: {error}", file=sys.stderr)
        raise SystemExit(2) from error
