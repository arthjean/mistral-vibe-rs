"""Measured debts: the gaps a recapture reveals in the row its corpus measures.

A ledger entry of a replay is a decision someone wrote. A gap of the row its
corpus measures is a measurement instead, so no ledger may keep one
(``vibe_core::parity::replay::assert_ledger``); the corpus keeps it as a
*debt*, a generated file under ``scripts/parity/debt/``. For every replay
heading, every exact scenario and every pointer with its array indices
written ``*``, the debt counts the differences no ledger entry covers. It
holds names and counts only, never reference text.

``repin.py debt <corpus>...`` is the only writer. It runs the tests that
replay the corpora with ``VIBE_PARITY_DEBT_MEASURE`` set, in which a replay's
debt covers every difference no entry covers and writes what it covered to
``target/parity/debt/``; it measures at least twice and refuses a measurement
that moved between runs. The manifest entry records the debt's SHA-256 and the
corpus SHA-256 it was measured against. The ratchet lives in the rules
:func:`ratchet` states: while a corpus is unchanged since its debt was
measured, the debt can only shrink, so a difference the port adds is a
regression to fix, not debt to record; a recapture that changes the corpus is
the only way a debt grows.

``vibe_core::parity::debt`` reads the same files and states the same rules
for the replays. A shared module, not a driver: ``repin.py`` imports it, it
imports no driver.
"""

from __future__ import annotations

import functools
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
from typing import Any

#: The workspace root.
REPOSITORY = Path(__file__).resolve().parents[2]

#: Where debts are committed.
DIRECTORY = REPOSITORY / "scripts" / "parity" / "debt"

#: The debt schema this module writes and the replays read.
SCHEMA = 1

#: The variable that turns a replay's debt into a measurement.
MEASURE = "VIBE_PARITY_DEBT_MEASURE"

#: The least number of measurements a debt is written from.
MINIMUM_RUNS = 2

#: ``{heading: {scenario: {pointer: count}}}``
Tallies = dict[str, dict[str, dict[str, int]]]


class DebtError(RuntimeError):
    """A debt that cannot be measured or written as asked."""


def path_of(corpus: str) -> Path:
    """The debt file of ``corpus``: its path under ``crates/``, each separator
    written ``--``."""

    relative = corpus.removeprefix("crates/")
    return DIRECTORY / relative.replace("/", "--")


def relative(path: Path) -> str:
    return path.relative_to(REPOSITORY).as_posix()


def generalize(pointer: str) -> str:
    return "/".join("*" if segment.isdigit() and segment.isascii() else segment for segment in pointer.split("/"))


def document(corpus: str, measured_against: str, tallies: Tallies) -> dict[str, Any]:
    return {
        "schemaVersion": SCHEMA,
        "corpus": corpus,
        "measuredAgainst": measured_against,
        "tallies": tallies,
    }


def render(content: dict[str, Any]) -> str:
    return json.dumps(content, indent=2, sort_keys=True, ensure_ascii=False) + "\n"


def sha256(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def problems(content: Any, corpus: str, measured_against: str) -> list[str]:
    """What makes ``content`` something other than the debt of ``corpus``
    measured against the corpus SHA-256 ``measured_against``."""

    found = []
    if not isinstance(content, dict):
        return ["it is not a JSON object"]
    if content.get("schemaVersion") != SCHEMA:
        found.append(f"its schemaVersion is not {SCHEMA}")
    if content.get("corpus") != corpus:
        found.append(f"it names the corpus {content.get('corpus')!r}")
    if content.get("measuredAgainst") != measured_against:
        found.append(f"it was measured against {content.get('measuredAgainst')!r}")
    tallies = content.get("tallies")
    if not isinstance(tallies, dict):
        return [*found, "it holds no tallies"]
    if not tallies:
        found.append("it holds no tallies; a corpus without debt keeps no file")
    for heading, scenarios in tallies.items():
        if not isinstance(scenarios, dict) or not scenarios:
            found.append(f"`{heading}` holds no scenarios")
            continue
        for scenario, pointers in scenarios.items():
            if not isinstance(pointers, dict) or not pointers:
                found.append(f"`{heading}` {scenario} holds no pointers")
                continue
            for pointer, count in pointers.items():
                if not pointer.startswith("/") or generalize(pointer) != pointer:
                    found.append(f"`{heading}` {scenario} {pointer} is not a pointer with its indices generalized")
                if not isinstance(count, int) or isinstance(count, bool) or count <= 0:
                    found.append(f"`{heading}` {scenario} {pointer} declares no positive count")
    return found


def named_in_sources(heading: str) -> bool:
    """Whether a Rust source under ``crates/`` names ``heading`` as a string
    literal, which is how a replay that asserts no figures line passes its
    heading to ``Debt::load``."""

    literal = f'"{heading}"'
    for path in (REPOSITORY / "crates").rglob("*.rs"):
        try:
            if literal in path.read_text(encoding="utf-8"):
                return True
        except (OSError, UnicodeDecodeError):
            continue
    return False


def verify(entry: dict[str, Any], corpus_sha256: str, headings: set[str]) -> list[str]:
    """Every rule the debt of the manifest entry ``entry`` breaks, given the
    corpus's SHA-256 and the headings the figures table carries. A heading no
    figures line carries is still read when a Rust source names it."""

    corpus = entry["corpus"]
    record = entry.get("debt")
    path = path_of(corpus)
    if record is None:
        if path.exists():
            return [f"{relative(path)} is the debt of {corpus}, which the manifest records no debt for"]
        return []
    command = f"`python3 scripts/parity/repin.py debt {corpus}`"
    if set(record) != {"sha256", "measuredAgainst"}:
        return [f"{corpus}: the manifest's debt record holds {sorted(record)}, not sha256 and measuredAgainst"]
    found = []
    if record["measuredAgainst"] != corpus_sha256:
        found.append(f"{corpus} was recaptured since its debt was measured; measure it again with {command}")
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        return [*found, f"{relative(path)} is unreadable ({error}); {command} writes it"]
    if sha256(text) != record["sha256"]:
        found.append(f"{relative(path)} changed outside {command}, the only sanctioned way to change a debt")
    try:
        content = json.loads(text)
    except ValueError as error:
        return [*found, f"{relative(path)} is not JSON: {error}"]
    found.extend(f"{relative(path)}: {problem}" for problem in problems(content, corpus, record["measuredAgainst"]))
    for heading in sorted((content.get("tallies") or {}).keys() - headings):
        if not named_in_sources(heading):
            found.append(
                f"{relative(path)} keeps a debt for `{heading}`, which neither a line of the figures "
                "table nor a Rust source of the workspace names"
            )
    return found


def orphans(listed: set[str]) -> list[str]:
    """Debt files no manifest entry owns."""

    owned = {path_of(corpus).name for corpus in listed}
    return [
        f"{relative(path)} belongs to no corpus the manifest lists; remove it"
        for path in sorted(DIRECTORY.glob("*.json"))
        if path.name not in owned
    ]


# --------------------------------------------------------------------------
# Measuring
# --------------------------------------------------------------------------


@functools.cache
def package_of(test_file: str) -> tuple[str, Path]:
    """The Cargo package that owns ``test_file``, and its directory."""

    directory = (REPOSITORY / test_file).parent
    while directory != REPOSITORY and not (directory / "Cargo.toml").is_file():
        directory = directory.parent
    manifest = directory / "Cargo.toml"
    match = re.search(r'^name\s*=\s*"([^"]+)"', manifest.read_text(encoding="utf-8"), re.M)
    if not match:
        raise DebtError(f"{relative(manifest)} names no package")
    return match.group(1), directory


def invocation(test_file: str) -> tuple[str, list[str], str | None]:
    """How ``cargo test`` reaches the tests of ``test_file``: the package, the
    target flags, and the module filter (``None`` for an integration test,
    whose binary holds only that file)."""

    package, directory = package_of(test_file)
    path = (REPOSITORY / test_file).relative_to(directory)
    if path.parts[0] == "tests" and len(path.parts) == 2:
        return package, ["--test", path.stem], None
    if path.parts[0] != "src":
        raise DebtError(f"{test_file} is neither an integration test nor a module of src/")
    module = "::".join(path.with_suffix("").parts[1:])
    target = ["--lib"] if (directory / "src" / "lib.rs").is_file() else ["--bins"]
    return package, target, f"{module}::"


@functools.cache
def sources() -> tuple[tuple[Path, str], ...]:
    """Every Rust source under ``crates/``, with its text."""

    found = []
    for path in sorted((REPOSITORY / "crates").rglob("*.rs")):
        try:
            found.append((path, path.read_text(encoding="utf-8")))
        except (OSError, UnicodeDecodeError):
            continue
    return tuple(found)


def including(path: Path, package: Path) -> list[str]:
    """The test files whose run compiles the Rust source ``path`` of the
    package at ``package``: the file itself for a module of ``src/`` or an
    integration test, and for a support module under ``tests/`` every
    integration test that includes it, through ``#[path]`` or through the
    ``mod`` of its directory."""

    inside = path.relative_to(package)
    if inside.parts[0] == "src" or (inside.parts[0] == "tests" and len(inside.parts) == 2):
        return [relative(path)]
    if inside.parts[0] != "tests":
        return []
    tests = package / "tests"
    top = inside.parts[1].removesuffix(".rs")
    nested = len(inside.parts) == 3 and inside.name != "mod.rs"
    module = path.stem
    found = []
    for candidate, text in sources():
        if candidate.parent.name != "tests" or candidate.parent.parent.parent != REPOSITORY / "crates":
            continue
        attributes = re.findall(r'#\[path\s*=\s*"([^"]+)"\s*\]', text)
        if any((candidate.parent / attribute).resolve() == path.resolve() for attribute in attributes):
            found.append(relative(candidate))
            continue
        if candidate.parent != tests or not re.search(rf"^\s*(?:pub\s+)?mod\s+{re.escape(top)}\s*;", text, re.M):
            continue
        if nested:
            parent = tests / top / "mod.rs"
            declared = parent.is_file() and re.search(
                rf"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+{re.escape(module)}\s*;",
                parent.read_text(encoding="utf-8"),
                re.M,
            )
            if not declared:
                continue
        found.append(relative(candidate))
    return found


def readers(corpus: str, test_file: str) -> list[str]:
    """Every test file whose run replays ``corpus``: the manifest's own
    ``test_file``, and every one that compiles a Rust source spelling the
    corpus path as the manifest lists it, relative to the source's package
    (``CORPUS_RELATIVE``) or relative to the source's directory
    (``include_str!``). A replay passes its corpus to ``Debt::load`` by the
    listed path, so a replay that reads the corpus without spelling it is a
    sibling module of one that does, which the module filter of
    :func:`invocation` runs with it."""

    target = REPOSITORY / corpus
    found = {test_file}
    for path, text in sources():
        _, package = package_of(relative(path))
        spellings = {
            corpus,
            target.relative_to(package).as_posix() if target.is_relative_to(package) else corpus,
            Path(os.path.relpath(target, path.parent)).as_posix(),
        }
        if any(f'"{spelling}"' in text for spelling in spellings):
            found.update(including(path, package))
    return sorted(found)


def measurements_directory(records: Path) -> Path:
    return records / "debt"


def run_tests(test_files: list[str], records: Path, measure: bool, log: Path) -> int:
    """Runs every test of ``test_files``, one ``cargo test`` per target, with
    the debt measured when ``measure`` is set; the exit status of the first
    run that failed, or 0. The output goes to ``log``."""

    groups: dict[tuple[str, tuple[str, ...]], list[str]] = {}
    for test_file in sorted(set(test_files)):
        package, target, module = invocation(test_file)
        filters = groups.setdefault((package, tuple(target)), [])
        if module is not None:
            filters.append(module)
    status = 0
    home = Path(tempfile.mkdtemp(prefix="vibe-debt-home-"))
    try:
        environment = {
            key: value for key, value in os.environ.items() if key not in {"FORCE_COLOR", "MISTRAL_API_KEY", MEASURE}
        }
        environment["VIBE_HOME"] = str(home)
        if measure:
            environment[MEASURE] = "1"
        with log.open("a", encoding="utf-8") as output:
            for (package, target), filters in groups.items():
                command = [
                    "cargo", "test", "-p", package, "--all-features", *target, "--",
                    *filters, "--test-threads=1",
                ]
                output.write(f"$ {' '.join(command)}\n")
                output.flush()
                completed = subprocess.run(
                    command, cwd=REPOSITORY, env=environment, stdout=output, stderr=subprocess.STDOUT, check=False
                )
                if completed.returncode and not status:
                    status = completed.returncode
    finally:
        shutil.rmtree(home, ignore_errors=True)
    return status


def read_measurements(records: Path) -> dict[str, Tallies]:
    """What the measuring replays covered, by corpus."""

    found: dict[str, Tallies] = {}
    for path in sorted(measurements_directory(records).glob("*.json")):
        content = json.loads(path.read_text(encoding="utf-8"))
        tallies = found.setdefault(content["corpus"], {})
        if content["heading"] in tallies:
            raise DebtError(f"two replays measured `{content['heading']}` over {content['corpus']}")
        if content["groups"]:
            tallies[content["heading"]] = content["groups"]
    return found


def measure(test_files: list[str], records: Path, runs: int, log: Path) -> dict[str, Tallies]:
    """The debt of every corpus the tests of ``test_files`` replay, measured
    ``runs`` times and refused unless every run measured the same."""

    if runs < MINIMUM_RUNS:
        raise DebtError(f"a debt is measured at least {MINIMUM_RUNS} times, so a count that moves between runs is caught")
    first: dict[str, Tallies] | None = None
    for run in range(1, runs + 1):
        shutil.rmtree(measurements_directory(records), ignore_errors=True)
        status = run_tests(test_files, records, measure=True, log=log)
        if status:
            raise DebtError(
                f"measuring run {run} failed (exit {status}), so its ledgers are not sound enough to measure "
                f"over; read {log}"
            )
        measured = read_measurements(records)
        if first is None:
            first = measured
        elif measured != first:
            moved = sorted(
                f"{corpus} `{heading}` {scenario} {pointer}"
                for corpus in set(first) | set(measured)
                for heading, scenario, pointer in groups_moved(first.get(corpus, {}), measured.get(corpus, {}))
            )
            raise DebtError(
                f"run {run} measured another debt than run 1, so the replay counts something the load moves: "
                + "; ".join(moved[:10])
            )
    return first or {}


def groups_moved(left: Tallies, right: Tallies) -> list[tuple[str, str, str]]:
    keys = {
        (heading, scenario, pointer)
        for tallies in (left, right)
        for heading, scenarios in tallies.items()
        for scenario, pointers in scenarios.items()
        for pointer in pointers
    }
    count = lambda tallies, key: tallies.get(key[0], {}).get(key[1], {}).get(key[2], 0)  # noqa: E731
    return sorted(key for key in keys if count(left, key) != count(right, key))


def ratchet(previous: Tallies | None, measured: Tallies, unchanged: bool) -> tuple[list[str], list[str]]:
    """The groups ``measured`` adds or raises over ``previous``, and those it
    lowers or removes, each as ``heading scenario pointer: before -> after``.

    While the corpus is ``unchanged`` since ``previous`` was measured, a
    group added or raised is a regression of the port: :func:`write` refuses
    it."""

    previous = previous or {}
    grown, shrunk = [], []
    for heading, scenario, pointer in groups_moved(previous, measured):
        before = previous.get(heading, {}).get(scenario, {}).get(pointer, 0)
        after = measured.get(heading, {}).get(scenario, {}).get(pointer, 0)
        line = f"`{heading}` {scenario} {pointer}: {before} -> {after}"
        (grown if after > before else shrunk).append(line)
    return grown, shrunk


def write(entry: dict[str, Any], corpus_sha256: str, measured: Tallies) -> tuple[list[str], list[str]]:
    """Records ``measured`` as the debt of ``entry``, or removes the debt
    when nothing is measured, after :func:`ratchet` admits it; returns what
    grew and what shrank."""

    corpus = entry["corpus"]
    path = path_of(corpus)
    record = entry.get("debt")
    previous = None
    if record is not None and path.is_file():
        previous = json.loads(path.read_text(encoding="utf-8")).get("tallies")
    unchanged = record is not None and record.get("measuredAgainst") == corpus_sha256
    grown, shrunk = ratchet(previous, measured, unchanged)
    if unchanged and grown:
        raise DebtError(
            f"{corpus} has not changed since its debt was measured, so these differences are regressions "
            "of the port to fix, not debt to record:\n  " + "\n  ".join(grown)
        )
    if not measured:
        path.unlink(missing_ok=True)
        entry.pop("debt", None)
        return grown, shrunk
    text = render(document(corpus, corpus_sha256, measured))
    path.parent.mkdir(parents=True, exist_ok=True)
    staged = path.with_suffix(".json.tmp")
    staged.write_text(text, encoding="utf-8", newline="\n")
    staged.replace(path)
    entry["debt"] = {"sha256": sha256(text), "measuredAgainst": corpus_sha256}
    return grown, shrunk


def summary(measured: Tallies) -> str:
    groups = sum(len(pointers) for scenarios in measured.values() for pointers in scenarios.values())
    differences = sum(
        count for scenarios in measured.values() for pointers in scenarios.values() for count in pointers.values()
    )
    return f"{differences} differences in {groups} groups over {len(measured)} replays"


if __name__ == "__main__":
    sys.exit("debt.py is a shared module; run `python3 scripts/parity/repin.py debt <corpus>`")
