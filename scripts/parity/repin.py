#!/usr/bin/env python3
"""Lists, verifies and recaptures the committed parity corpora.

Every corpus a replay reads is an entry of ``scripts/parity/manifest.json``:
the driver command that regenerates it, the test that replays it, the
scorecard row it answers for, the reference commit it was captured from, its
SHA-256, its observation count, and a floor per family of observations
(``scripts/parity/provenance.py`` states the rules). This script is the only
sanctioned way to change a listed corpus, so a re-pin or a recapture is
verified mechanically instead of from memory.

Usage::

    python3 scripts/parity/repin.py list
    python3 scripts/parity/repin.py plan <sha>
    python3 scripts/parity/repin.py open-transition
    python3 scripts/parity/repin.py verify
    python3 scripts/parity/repin.py capture <corpus> [--commit <sha>] [--accept-shrink]
    python3 scripts/parity/repin.py remove <corpus>
    python3 scripts/parity/repin.py add <corpus> --driver "<script> <args>" \\
        --test <file>::<function> --row <n> [--output-flag <flag>] [--kind <kind>]
    python3 scripts/parity/repin.py materialize [<sha>... | --accepted] [--checkout <path>]
    python3 scripts/parity/repin.py attest <sha> [--full]
    python3 scripts/parity/repin.py check [<corpus>...] [--unprobed] [--records <dir>]
    python3 scripts/parity/repin.py accepted
    python3 scripts/parity/repin.py census [--check] [--url <upstream>]
    python3 scripts/parity/repin.py shingles [<sha>] [--check]
    python3 scripts/parity/repin.py debt (<corpus>... | --all) [--runs <n>]

``<corpus>`` is a manifest path or a suffix of exactly one, such as
``acp-parity/corpus.json``. ``capture`` runs the entry's driver (and refreshes
every entry the same command writes), stages each output beside its corpus,
checks the commit it recorded, moves it into place, and updates the manifest in
the same invocation. ``--commit`` captures a checkout at another commit, for a
re-pin; the entry is then marked as captured off-target until the pin names
that commit. ``--accept-shrink`` lowers the floors of a capture that lost
observations on purpose; without it the floors only rise.

A re-pin is rolling. The pin names its target (``EXPECTED_COMMIT``) and, while
the manifest's ``transition`` list names the corpora not yet recaptured, the
commit it left (``PREVIOUS_COMMIT``): a corpus at that commit is accepted only
while the list names it, a capture at the target drops its corpus from the
list, and the transition is closed by clearing ``PREVIOUS_COMMIT`` once the
list is empty. ``open-transition``, run once both pin sources name the
target and the previous commit, lists every entry not yet at the target.
``plan <sha>`` prints every entry with its commit, its driver
command and the story that owns its recapture (``scripts/parity/repin_plan.json``),
in the order a re-pin to ``<sha>`` recaptures them.

``materialize`` builds the attested reference environment of a commit under
``.parity/`` (``scripts/parity/materialize.py``), which every capture and every
live probe runs instead of the shared checkout; ``--accepted`` builds one per
commit the manifest accepts. ``attest`` checks an environment against its
attestation, every file hashed with ``--full``.

``check`` recaptures entries through their drivers, as ``capture`` does, and
compares what they write with the committed corpora without replacing
anything; ``--unprobed`` adds one entry of every driver whose reference side
the run under ``--records`` did not record, which is how the weekly live run
reaches the drivers no live probe calls. An ``assembled`` entry's driver is
run for its reference side only. Each outcome goes to ``checks.jsonl``.

``census`` compares the pin with upstream's newest release through a static
census of both trees (``scripts/parity/sentinel.py``) and records it in
``scripts/parity/upstream-census.json`` and the "against upstream ``HEAD``" row
of ``docs/parity.md``; with ``--check`` it records nothing and exits non-zero
when the pin is behind, which is what the daily sentinel runs.

``shingles`` writes the digests of every 8-word run of the reference's prose
at ``<sha>`` (the pin by default; ``scripts/parity/shingles.py``), read from
the pristine tree of its attested environment, to
``crates/vibe-core/tests/licensing/``, refusing a set above 4 MB, and beside
it the third-party layer: the digests a locked third-party distribution of
that environment also ships, with the distributions that do. ``--check``
recomputes both and fails on any difference, writing nothing, which the
weekly live run does. The clean-room guard
``crates/vibe-core/src/parity/licensing_tests.rs`` subtracts the layer from
the set, fails on any tracked run that shares what remains outside its
baseline, and writes the baseline it measured to
``target/parity/licensing/baseline.txt``.

``debt`` measures the debt of corpora (``scripts/parity/debt.py``): the gaps
of the row a corpus measures that no ledger entry may keep, counted per
replay heading, exact scenario and generalized pointer. It runs the tests that
replay them in measure mode ``--runs`` times (at least twice) and refuses a
measurement that moved; while a corpus is unchanged since its debt was
measured, it refuses a group that grew or appeared, so the debt of an
unchanged corpus only shrinks. It records each debt's SHA-256 and the corpus
SHA-256 it was measured against in the manifest, then runs the replays once
more as they run in CI.

The Rust meta-test ``crates/vibe-core/src/parity/manifest_tests.rs`` checks the
same rules as ``verify`` without running Python.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
from typing import Any

import debt
import materialize
import provenance
import sentinel
import shingles
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT, PREVIOUS_COMMIT
from provenance import MANIFEST, REPOSITORY

SCORECARD = REPOSITORY / "docs/parity.md"
REPIN = "python3 scripts/parity/repin.py"
PLAN = REPOSITORY / "scripts/parity/repin_plan.json"


class RepinError(RuntimeError):
    """A manifest, a corpus, or a capture that breaks a provenance rule."""


# --------------------------------------------------------------------------
# Reading
# --------------------------------------------------------------------------


def accepted_commits(manifest: dict[str, Any]) -> list[str]:
    """The pin's commits a corpus may record: the target, then the previous
    commit while a transition is open."""

    del manifest
    return [EXPECTED_COMMIT, *([PREVIOUS_COMMIT] if PREVIOUS_COMMIT else [])]


def accepted_for(manifest: dict[str, Any], corpus: str, previous: str = PREVIOUS_COMMIT) -> list[str]:
    """The commits the corpus ``corpus`` may record: the target, then
    ``previous`` while the transition list names it."""

    listed = bool(previous) and corpus in manifest.get("transition", [])
    return [EXPECTED_COMMIT, *([previous] if listed else [])]


def transition_problems(manifest: dict[str, Any], previous: str = PREVIOUS_COMMIT) -> list[str]:
    """Every rule of a rolling re-pin the manifest breaks, as
    ``vibe_core::parity::manifest::transition_problems`` states them."""

    problems = []
    transition = manifest.get("transition", [])
    if previous and not transition:
        problems.append(
            f"the transition list is empty but the pin still names the previous commit {previous}; "
            "clear `PREVIOUS_COMMIT` in crates/vibe-core/src/parity.rs and scripts/parity/pin.py "
            "to close the transition"
        )
    if not previous and transition:
        problems.append(
            f"the transition list names {len(transition)} corpora but `PREVIOUS_COMMIT` is empty; "
            "declare the commit the re-pin left in both pin sources, or empty the list"
        )
    if previous and previous == EXPECTED_COMMIT:
        problems.append(
            f"`PREVIOUS_COMMIT` is the target {EXPECTED_COMMIT}; a transition leaves another commit"
        )
    entries = {entry["corpus"]: entry for entry in manifest["entries"]}
    seen: set[str] = set()
    for corpus in transition:
        if corpus in seen:
            problems.append(f"the transition list names {corpus} twice")
        seen.add(corpus)
        entry = entries.get(corpus)
        if entry is None:
            problems.append(f"the transition list names {corpus}, which is not a manifest entry")
        elif entry.get("commit") == EXPECTED_COMMIT:
            problems.append(
                f"the transition list names {corpus}, which is already at the target "
                f"{EXPECTED_COMMIT}; drop it from the list"
            )
    return problems


def tracked_corpora() -> list[str]:
    """Every JSON file tracked under ``crates/*/tests/`` that names the
    reference it came from (a ``reference`` key or a recorded commit), so it
    has to be listed."""

    listing = subprocess.run(
        ["git", "ls-files", "-z", "--", "crates"],
        cwd=REPOSITORY,
        capture_output=True,
        check=False,
    )
    if listing.returncode != 0:
        raise RepinError(f"git ls-files failed: {listing.stderr.decode(errors='replace')}")
    found = []
    for name in listing.stdout.decode().split("\0"):
        parts = name.split("/")
        if len(parts) < 4 or parts[2] != "tests" or not name.endswith(".json"):
            continue
        try:
            corpus = json.loads((REPOSITORY / name).read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        if provenance.names_reference(corpus):
            found.append(name)
    return sorted(found)


def tracked_files() -> set[str]:
    listing = subprocess.run(
        ["git", "ls-files", "-z"], cwd=REPOSITORY, capture_output=True, check=False
    )
    return set(listing.stdout.decode().split("\0"))


def scorecard_rows() -> set[str]:
    rows = set()
    for line in SCORECARD.read_text(encoding="utf-8").splitlines():
        match = re.match(r"^\| (\d+) \|", line)
        if match:
            rows.add(match.group(1))
    return rows


def figures_headings() -> set[str]:
    """The replay headings the "Measured figures" table of the scorecard
    carries."""

    headings = set()
    inside = False
    for line in SCORECARD.read_text(encoding="utf-8").splitlines():
        if line.startswith("## "):
            inside = line == "## Measured figures"
            continue
        match = re.match(r"^\| `([^`]+)` \|", line) if inside else None
        if match:
            headings.add(match.group(1))
    return headings


def resolves_to_test(test: str) -> bool:
    """Whether ``<file>::<function>`` names a function under ``#[test]`` (or
    an async test attribute) in that file."""

    path, _, function = test.partition("::")
    try:
        source = (REPOSITORY / path).read_text(encoding="utf-8")
    except OSError:
        return False
    for match in re.finditer(rf"\bfn {re.escape(function)}\s*[<(]", source):
        before = source[: match.start()]
        before = before.rstrip()
        for keyword in ("async", "pub"):
            if re.search(rf"\b{keyword}$", before):
                before = before[: -len(keyword)].rstrip()
        attributes = []
        while before.endswith("]"):
            depth, index = 0, len(before) - 1
            while index >= 0:
                if before[index] == "]":
                    depth += 1
                elif before[index] == "[":
                    depth -= 1
                    if depth == 0:
                        break
                index -= 1
            if index < 1 or before[index - 1] != "#":
                break
            attributes.append(before[index - 1 :])
            before = before[: index - 1].rstrip()
        if any(re.match(r"#\[(?:\w+::)?test\b", attribute) for attribute in attributes):
            return True
    return False


def find_entry(manifest: dict[str, Any], name: str) -> dict[str, Any]:
    name = name.replace("\\", "/")
    exact = [entry for entry in manifest["entries"] if entry["corpus"] == name]
    if exact:
        return exact[0]
    matching = [
        entry for entry in manifest["entries"] if entry["corpus"].endswith("/" + name)
    ]
    if len(matching) != 1:
        raise RepinError(
            f"`{name}` names {len(matching)} manifest entries; give the corpus path "
            "`repin.py list` prints"
        )
    return matching[0]


def driver_line(entry: dict[str, Any]) -> str:
    driver = entry.get("driver")
    return "python3 " + shlex.join(driver) if driver else "(hand-owned)"


# --------------------------------------------------------------------------
# Verifying
# --------------------------------------------------------------------------


def verify(manifest: dict[str, Any]) -> list[str]:
    """Every provenance rule the manifest and the corpora break."""

    problems: list[str] = transition_problems(manifest)
    entries = manifest["entries"]
    names = [entry["corpus"] for entry in entries]
    if names != sorted(names):
        problems.append("the manifest entries are not sorted by corpus path")
    if len(set(names)) != len(names):
        problems.append("the manifest lists a corpus twice")
    listed = set(names)
    for corpus in tracked_corpora():
        if corpus not in listed:
            problems.append(
                f"{corpus} names the reference it came from but is not in the manifest; add it with "
                f"`{REPIN} add {corpus} --driver \"<script> <args>\" --test <file>::<function> "
                "--row <n>`"
            )
    tracked = tracked_files()
    rows = scorecard_rows()
    headings = figures_headings()
    for entry in entries:
        problems.extend(verify_entry(entry, accepted_for(manifest, entry["corpus"]), tracked, rows))
        if (REPOSITORY / entry["corpus"]).is_file():
            problems.extend(debt.verify(entry, provenance.sha256(REPOSITORY / entry["corpus"]), headings))
    problems.extend(debt.orphans(listed))
    return problems


def verify_entry(
    entry: dict[str, Any], accepted: list[str], tracked: set[str], rows: set[str]
) -> list[str]:
    corpus = entry["corpus"]
    problems = []
    if corpus not in tracked:
        return [f"{corpus} is in the manifest but not tracked by git"]
    if not resolves_to_test(entry["test"]):
        problems.append(
            f"{corpus}: the manifest names the test `{entry['test']}`, which is no `#[test]` "
            "function in that file"
        )
    if entry["row"] not in rows:
        problems.append(f"{corpus}: the manifest names row {entry['row']}, which docs/parity.md does not carry")
    document = json.loads((REPOSITORY / corpus).read_text(encoding="utf-8"))
    recorded = provenance.recorded_commit(document)
    if entry["kind"] == "fixture":
        if recorded is not None and recorded not in accepted:
            problems.append(off_commit(corpus, recorded, accepted))
        return problems
    capture = f"`{REPIN} capture {corpus}`"
    digest = provenance.sha256(REPOSITORY / corpus)
    if digest != entry["sha256"]:
        problems.append(
            f"{corpus} changed outside the capture tool: its SHA-256 is {digest} and the manifest "
            f"records {entry['sha256']}; {capture} is the only sanctioned way to change it"
        )
    if recorded is None:
        problems.append(f"{corpus} records no reference commit")
    elif recorded not in accepted:
        problems.append(off_commit(corpus, recorded, accepted))
    elif recorded != entry["commit"]:
        problems.append(
            f"{corpus} records commit {recorded} and the manifest records {entry['commit']}; "
            f"recapture it with {capture}"
        )
    counts = provenance.families(document)
    if sum(counts.values()) == 0:
        problems.append(f"{corpus} holds no observations; its case lists are empty")
    for family, floor in entry["floors"].items():
        count = counts.get(family, 0)
        if count < floor:
            problems.append(
                f"{corpus} holds {count} {family}, below the floor of {floor}; recapture what "
                f"was lost, or lower the floor on purpose with {capture[:-1]} --accept-shrink`"
            )
    for family, count in counts.items():
        if entry["floors"].get(family, 0) < provenance.minimum_floor(count):
            problems.append(
                f"{corpus}: the floor of {family} is below {provenance.FLOOR_RATIO:.0%} of its "
                f"{count} observations; {capture} raises it"
            )
    if entry.get("count") != sum(counts.values()):
        problems.append(f"{corpus}: the manifest count disagrees with the corpus; {capture}")
    return problems


def off_commit(corpus: str, recorded: str, accepted: list[str]) -> str:
    return f"{corpus} was captured from {recorded}; accepted: {', '.join(accepted)}"


# --------------------------------------------------------------------------
# Writing
# --------------------------------------------------------------------------


def write_manifest(manifest: dict[str, Any]) -> None:
    manifest["entries"].sort(key=lambda entry: entry["corpus"])
    staged = MANIFEST.with_name(f"{MANIFEST.name}.{os.getpid()}.staged")
    staged.write_text(
        json.dumps(manifest, indent=2, ensure_ascii=False) + "\n", encoding="utf-8", newline=""
    )
    os.replace(staged, MANIFEST)


def record(entry: dict[str, Any], path: Path, accept_shrink: bool) -> None:
    """Writes what ``path`` holds into ``entry``: its digest, commit, count
    and floors."""

    document = json.loads(path.read_text(encoding="utf-8"))
    counts = provenance.families(document)
    entry["commit"] = provenance.recorded_commit(document)
    entry["sha256"] = provenance.sha256(path)
    entry["count"] = sum(counts.values())
    previous = {} if accept_shrink else entry.get("floors", {})
    entry["floors"] = {
        family: floor
        for family, floor in provenance.raised_floors(previous, counts).items()
        if family in counts or not accept_shrink
    }
    if entry["commit"] != EXPECTED_COMMIT:
        entry["offTarget"] = True
    else:
        entry.pop("offTarget", None)


def leave_transition(manifest: dict[str, Any], corpora: list[str]) -> None:
    """Drops ``corpora``, just captured at the target, from the transition
    list, which names only what a rolling re-pin has not recaptured yet."""

    if manifest.get("transition"):
        manifest["transition"] = [
            corpus for corpus in manifest["transition"] if corpus not in corpora
        ]


def load_plan(path: Path = PLAN) -> list[dict[str, Any]]:
    """The stages of ``scripts/parity/repin_plan.json``, in recapture order."""

    try:
        return json.loads(path.read_text(encoding="utf-8"))["stages"]
    except (OSError, ValueError, KeyError) as error:
        raise RepinError(f"cannot read the re-pin plan {path}: {error}") from None


def plan(manifest: dict[str, Any], target: str, stages: list[dict[str, Any]]) -> list[str]:
    """One line per manifest entry, in the order a re-pin to ``target``
    recaptures it: by the stage that owns its driver and the driver's place in
    it, a driver no stage names last, then by corpus. Each line gives the entry's commit (``done`` once it
    is at ``target``), its driver command and the story that owns it."""

    owner: dict[str, tuple[int, str]] = {}
    for rank, stage in enumerate(stages):
        for position, driver in enumerate(stage["drivers"]):
            # A stage lists its drivers in the order they have to run: one
            # driver reads what another captured (the protocol probes read
            # the app-server surface census).
            owner.setdefault(driver, (rank * 1000 + position, stage["story"]))
    unplanned = (len(stages) * 1000, "unplanned")

    def stage_of(entry: dict[str, Any]) -> tuple[int, str]:
        driver = entry.get("driver")
        if not driver:
            return unplanned
        return owner.get(Path(driver[0]).name, unplanned)

    ordered = sorted(manifest["entries"], key=lambda entry: (stage_of(entry)[0], entry["corpus"]))
    transition = set(manifest.get("transition", []))
    lines = []
    for entry in ordered:
        commit = entry.get("commit") or "-"
        state = "done" if commit == target else ("transition" if entry["corpus"] in transition else "pending")
        command = (
            f"{REPIN} capture {entry['corpus']}"
            + ("" if target == EXPECTED_COMMIT else f" --commit {target}")
            if entry["kind"] == "corpus"
            else driver_line(entry)
        )
        lines.append(
            f"{stage_of(entry)[1]}  {entry['corpus']}  {commit[:12]} ({state})  {entry['kind']}  "
            f"{driver_line(entry)}  ->  {command}"
        )
    return lines


def driver_command(driver: list[str]) -> list[str]:
    """The argv that runs ``driver``: a Python script under this interpreter,
    anything else (``cargo test ...``) as written."""

    if driver[0].endswith(".py"):
        return [sys.executable, str(REPOSITORY / driver[0]), *driver[1:]]
    return list(driver)


def capture(
    name: str, commit: str, accept_shrink: bool, hand_assembled: bool, compare: bool = False
) -> list[str]:
    """Runs the driver of the entry ``name`` and moves every corpus it wrote
    into place, returning the paths it refreshed.

    With ``compare``, nothing is replaced or recorded: the corpora the driver
    wrote are compared with the committed ones, every staged file is removed,
    and the paths returned are the corpora whose recapture differs.

    Every entry the same driver command writes is refreshed together. A driver
    with an output flag per corpus writes each one beside it under a staged
    name; one whose output flag names a directory (``outputRoot``) writes a
    staged copy of that directory; one with no output flag writes in place,
    over a backup that is put back when it fails. Nothing replaces a corpus
    until the driver succeeded, recorded ``commit`` in every file, and no file
    changed while it ran.
    """

    manifest = provenance.load_manifest()
    entry = find_entry(manifest, name)
    if entry["kind"] == "fixture":
        raise RepinError(f"{entry['corpus']} is a hand-owned fixture; edit it directly")
    if entry["kind"] == "assembled":
        if not hand_assembled:
            raise RepinError(
                f"{entry['corpus']} is assembled by hand from what `{driver_line(entry)}` "
                "measures; merge the measurement, then record the result with "
                f"`{REPIN} capture {entry['corpus']} --hand-assembled`"
            )
        path = REPOSITORY / entry["corpus"]
        recorded = provenance.recorded_commit(json.loads(path.read_text(encoding="utf-8")))
        if recorded != commit:
            raise RepinError(f"{entry['corpus']} records commit {recorded}, not {commit}")
        record(entry, path, accept_shrink)
        leave_transition(manifest, [entry["corpus"]] if entry["commit"] == EXPECTED_COMMIT else [])
        write_manifest(manifest)
        return [entry["corpus"]]
    if hand_assembled:
        raise RepinError(f"{entry['corpus']} is captured by its driver, not assembled by hand")
    group = [
        other
        for other in manifest["entries"]
        if other["kind"] == "corpus" and other["driver"] == entry["driver"]
    ]
    before: dict[str, str | None] = {}
    for member in group:
        path = REPOSITORY / member["corpus"]
        before[member["corpus"]] = provenance.sha256(path) if path.exists() else None
        if before[member["corpus"]] not in (None, member["sha256"]):
            raise RepinError(
                f"{member['corpus']} changed outside the capture tool since the manifest "
                f"recorded it; restore it with `git checkout -- {member['corpus']}` and rerun"
            )
    workspace = REPOSITORY / ".parity" / f"repin-{os.getpid()}"
    workspace.mkdir(parents=True, exist_ok=True)
    command = driver_command(entry["driver"])
    staged: dict[str, Path] = {}
    roots = {member.get("outputRoot") for member in group}
    flags = {member.get("output") for member in group}
    in_place = flags == {None}
    if in_place:
        for member in group:
            backup = workspace / "backup" / member["corpus"]
            backup.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(REPOSITORY / member["corpus"], backup)
            staged[member["corpus"]] = REPOSITORY / member["corpus"]
    elif roots != {None}:
        if len(roots) != 1 or len(flags) != 1:
            raise RepinError("the entries of one directory driver disagree on its output")
        (root,) = roots
        (flag,) = flags
        directory = workspace / "output"
        shutil.copytree(REPOSITORY / root, directory)
        command += [flag, str(directory)]
        for member in group:
            staged[member["corpus"]] = directory / Path(member["corpus"]).relative_to(root)
            staged[member["corpus"]].unlink(missing_ok=True)
    else:
        for member in group:
            path = REPOSITORY / member["corpus"]
            staged[member["corpus"]] = path.with_name(f"{path.name}.{os.getpid()}.staged")
            command += [member["output"], str(staged[member["corpus"]])]
    environment = {key: value for key, value in os.environ.items() if key != "FORCE_COLOR"}
    environment[provenance.CAPTURE_VARIABLE] = commit
    print(f"capturing with: {shlex.join(command)}", file=sys.stderr)
    replaced = False
    try:
        result = subprocess.run(command, cwd=REPOSITORY, env=environment, check=False)
        if result.returncode != 0:
            raise RepinError(f"the driver exited with {result.returncode}; nothing was replaced")
        for member in group:
            path = staged[member["corpus"]]
            if not path.is_file():
                raise RepinError(f"the driver wrote nothing to {path}; nothing was replaced")
            recorded = provenance.recorded_commit(json.loads(path.read_text(encoding="utf-8")))
            if recorded != commit:
                raise RepinError(
                    f"the driver recorded commit {recorded} for {member['corpus']}, not {commit}; "
                    "nothing was replaced"
                )
        current = provenance.load_manifest()
        for member in group:
            path = REPOSITORY / member["corpus"]
            now = provenance.sha256(path) if path.exists() else None
            listed = find_entry(current, member["corpus"])
            moved = not in_place and now != before[member["corpus"]]
            if moved or listed["sha256"] != member["sha256"]:
                raise RepinError(f"{member['corpus']} changed during capture; rerun")
        if compare:
            read = lambda path: json.loads(path.read_text(encoding="utf-8"))  # noqa: E731
            return [
                member["corpus"]
                for member in group
                if read(staged[member["corpus"]]) != read(REPOSITORY / member["corpus"])
            ]
        for member in group:
            record(member, staged[member["corpus"]], accept_shrink)
            if not in_place:
                os.replace(staged[member["corpus"]], REPOSITORY / member["corpus"])
        leave_transition(manifest, [member["corpus"] for member in group if member["commit"] == EXPECTED_COMMIT])
        write_manifest(manifest)
        replaced = True
    finally:
        if in_place and not replaced:
            for member in group:
                shutil.copy2(workspace / "backup" / member["corpus"], REPOSITORY / member["corpus"])
        if not in_place:
            for path in staged.values():
                if workspace not in path.parents:
                    path.unlink(missing_ok=True)
        shutil.rmtree(workspace, ignore_errors=True)
    return [member["corpus"] for member in group]


def driver_name(entry: dict[str, Any]) -> str | None:
    """The script name a capture records in ``drivers.jsonl``
    (``reference.py``), or ``None`` for an entry no Python driver writes."""

    driver = entry.get("driver")
    return Path(driver[0]).name if driver and driver[0].endswith(".py") else None


def ran_drivers(records: Path) -> set[str]:
    """The drivers whose reference side a run recorded under ``records``."""

    path = records / "drivers.jsonl"
    if not path.is_file():
        return set()
    names = set()
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        try:
            names.add(json.loads(line).get("driver"))
        except ValueError:
            continue
    return names


def check(names: list[str], unprobed: bool, records: Path) -> int:
    """Recaptures the reference for each named entry, or with ``unprobed``
    for one entry of every driver no live probe ran, and compares what it
    writes with the committed corpus without replacing anything.

    An ``assembled`` corpus is merged by hand from what its driver measures,
    so its driver is run for its reference side and nothing is compared.
    Every outcome is appended to ``checks.jsonl`` under ``records`` for the
    live summary. Returns the exit status: non-zero when any recapture differs
    or fails.
    """

    manifest = provenance.load_manifest()
    entries = [find_entry(manifest, name) for name in names]
    if unprobed:
        ran = ran_drivers(records)
        seen: set[str] = set()
        for entry in manifest["entries"]:
            name = driver_name(entry)
            if name and name not in ran and name not in seen and entry["kind"] != "fixture":
                seen.add(name)
                entries.append(entry)
    if not entries:
        print("every driver's reference side already ran", file=sys.stderr)
        return 0
    status = 0
    for entry in entries:
        corpus = entry["corpus"]
        outcome: dict[str, Any] = {"driver": driver_name(entry), "corpus": corpus}
        try:
            if entry["kind"] == "assembled":
                result = subprocess.run(
                    driver_command(entry["driver"]),
                    cwd=REPOSITORY,
                    env={key: value for key, value in os.environ.items() if key != "FORCE_COLOR"},
                    capture_output=True,
                    text=True,
                    check=False,
                )
                if result.returncode != 0:
                    raise RepinError(
                        f"the driver exited with {result.returncode}: {result.stderr.strip()[-2000:]}"
                    )
                outcome["outcome"] = "ran"
            else:
                # A corpus a rolling re-pin has not recaptured yet is checked
                # against the commit it still records.
                at = (
                    entry["commit"]
                    if entry.get("commit") in accepted_for(manifest, corpus)
                    else EXPECTED_COMMIT
                )
                differing = capture(corpus, at, False, False, compare=True)
                outcome["outcome"] = "differs" if differing else "matches"
                if differing:
                    outcome["detail"] = "a fresh capture differs for " + ", ".join(differing)
        except (RepinError, materialize.AttestationError, OSError) as error:
            outcome["outcome"] = "failed"
            outcome["detail"] = str(error)
        if outcome["outcome"] in ("differs", "failed"):
            status = 1
        print(f"{corpus}: {outcome['outcome']}" + (f" ({outcome['detail']})" if "detail" in outcome else ""))
        try:
            records.mkdir(parents=True, exist_ok=True)
            with (records / "checks.jsonl").open("a", encoding="utf-8") as handle:
                handle.write(json.dumps(outcome) + "\n")
        except OSError:
            pass
    return status


def default_records() -> Path:
    """``parity/`` under the Cargo target directory, where the Rust gate and
    ``reference.py`` record what ran."""

    target = Path(os.environ.get("CARGO_TARGET_DIR") or REPOSITORY / "target")
    return (target if target.is_absolute() else REPOSITORY / target) / "parity"


def add(arguments: argparse.Namespace) -> str:
    manifest = provenance.load_manifest()
    corpus = arguments.corpus.replace("\\", "/")
    if any(entry["corpus"] == corpus for entry in manifest["entries"]):
        raise RepinError(f"{corpus} is already in the manifest")
    path = REPOSITORY / corpus
    if not path.is_file():
        raise RepinError(f"no file at {corpus}")
    entry: dict[str, Any] = {"corpus": corpus, "kind": arguments.kind}
    if arguments.kind != "fixture":
        if not arguments.driver:
            raise RepinError("a corpus needs `--driver`, the command that regenerates it")
        entry["driver"] = shlex.split(arguments.driver)
        entry["output"] = arguments.output_flag or None
    entry["test"] = arguments.test
    entry["row"] = arguments.row
    if arguments.kind != "fixture":
        record(entry, path, accept_shrink=False)
    problems = verify_entry(
        entry, accepted_for(manifest, corpus), tracked_files() | {corpus}, scorecard_rows()
    )
    if problems:
        raise RepinError("\n".join(problems))
    manifest["entries"].append(entry)
    write_manifest(manifest)
    return corpus


def remove(arguments: argparse.Namespace) -> str:
    """Deletes a corpus whose oracle is gone, such as the legacy half of a
    legacy and unified pair once the reference ships only one backend: its
    file, its entry, and its name in the transition list. The test that
    replayed it has to go in the same change, which `manifest_tests` holds."""
    manifest = provenance.load_manifest()
    corpus = arguments.corpus.replace("\\", "/")
    entries = [entry for entry in manifest["entries"] if entry["corpus"] == corpus]
    if not entries:
        raise RepinError(f"{corpus} is not in the manifest")
    manifest["entries"] = [entry for entry in manifest["entries"] if entry["corpus"] != corpus]
    if corpus in manifest.get("transition", []):
        manifest["transition"] = [name for name in manifest["transition"] if name != corpus]
    (REPOSITORY / corpus).unlink(missing_ok=True)
    debt.path_of(corpus).unlink(missing_ok=True)
    write_manifest(manifest)
    return corpus


def measure_debts(names: list[str], every: bool, runs: int) -> int:
    """Measures and records the debt of the corpora ``names`` (every corpus a
    replay reads with ``every``), as ``scripts/parity/debt.py`` states."""

    if not sys.platform.startswith("linux"):
        raise RepinError(
            "a debt is measured on Linux, where the corpora are captured and every replay compares "
            "every scenario; a host that replays a subset would record a smaller debt"
        )
    manifest = provenance.load_manifest()
    if every:
        entries = [entry for entry in manifest["entries"] if entry["kind"] != "fixture"]
    elif names:
        entries = [find_entry(manifest, name) for name in dict.fromkeys(names)]
    else:
        raise RepinError("name the corpora whose debt to measure, or pass `--all`")
    for entry in entries:
        corpus = entry["corpus"]
        if entry["kind"] == "fixture":
            raise RepinError(f"{corpus} is a hand-owned fixture, whose differences its ledger decides")
        if provenance.sha256(REPOSITORY / corpus) != entry.get("sha256"):
            raise RepinError(
                f"{corpus} changed outside the capture tool; `{REPIN} capture {corpus}` first, so the debt "
                "is measured against a corpus the manifest records"
            )
    records = default_records()
    log = records / "debt.log"
    log.parent.mkdir(parents=True, exist_ok=True)
    log.write_text("", encoding="utf-8")
    test_files = sorted(
        {reader for entry in entries for reader in debt.readers(entry["corpus"], entry["test"].partition("::")[0])}
    )
    print(f"measuring the debt of {len(entries)} corpora, {runs} runs of {len(test_files)} test files; log: {log}")
    for test_file in test_files:
        print(f"  {test_file}")
    measured = debt.measure(test_files, records, runs, log)
    outcomes = {}
    for entry in entries:
        corpus = entry["corpus"]
        record = entry.get("debt")
        path = debt.path_of(corpus)
        previous = json.loads(path.read_text(encoding="utf-8")).get("tallies") if record and path.is_file() else None
        unchanged = bool(record) and record.get("measuredAgainst") == entry["sha256"]
        grown, shrunk = debt.ratchet(previous, measured.get(corpus, {}), unchanged)
        if unchanged and grown:
            raise RepinError(
                f"{corpus} has not changed since its debt was measured, so these differences are "
                "regressions of the port to fix, not debt to record; nothing was written:\n  " + "\n  ".join(grown)
            )
        outcomes[corpus] = (grown, shrunk)
    for entry in entries:
        corpus = entry["corpus"]
        debt.write(entry, entry["sha256"], measured.get(corpus, {}))
        grown, shrunk = outcomes[corpus]
        tallies = measured.get(corpus, {})
        print(f"{corpus}: {debt.summary(tallies) if tallies else 'no debt'} ({len(grown)} groups added or raised, {len(shrunk)} lowered or paid)")
        for line in grown:
            print(f"  + {line}")
    write_manifest(manifest)
    status = debt.run_tests(test_files, records, measure=False, log=log)
    if status:
        print(
            f"the debts are recorded, and the replays fail as CI runs them (exit {status}): a figures line "
            f"to restate after a recapture, or a replay to fix; read {log}",
            file=sys.stderr,
        )
        return 1
    print("the replays pass over the recorded debts")
    return 0


def write_shingles(commit: str, check: bool) -> int:
    """Writes the digest set of ``commit`` and its third-party layer
    (``scripts/parity/shingles.py``), or with ``check`` recomputes both and
    compares them with the committed files, writing nothing.

    The digest set is a function of the reference tree and is compared
    byte for byte. The layer names the distributions that ship each digest,
    and a platform installs a few distributions of its own (`pywin32` on
    Windows), so a check on another platform than the one the layer was
    measured on compares its digests and lockfile only.
    """

    root = materialize.environment_root(commit)
    attestation = materialize.verify(root, commit)
    values, files = shingles.digests(root / materialize.SOURCE)
    subtracted, read, scanned = shingles.third_party(root, values)
    content = shingles.layer(commit, attestation, subtracted, read)
    summary = (
        f"{len(values)} shingle digests from {files} reference files, {len(subtracted)} of them "
        f"shipped by {len(read)} locked third-party distributions ({scanned} files read), "
        f"measured on {attestation['platform']}"
    )
    digest_path = shingles.digest_file(commit)
    layer_path = shingles.layer_file(commit)
    if not check:
        shingles.write(commit, values)
        shingles.write_layer(commit, content)
        print(
            f"{digest_path.relative_to(REPOSITORY).as_posix()} ({digest_path.stat().st_size} bytes) "
            f"and {layer_path.relative_to(REPOSITORY).as_posix()}: {summary}"
        )
        return 0
    problems = []
    if not digest_path.is_file() or digest_path.read_bytes() != shingles.encode(values):
        problems.append(
            f"{digest_path.relative_to(REPOSITORY).as_posix()} no longer matches the reference "
            f"tree at {commit[:12]}"
        )
    try:
        committed = json.loads(layer_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        committed = None
        problems.append(f"{layer_path.relative_to(REPOSITORY).as_posix()} is unreadable: {error}")
    if committed is not None:
        name = layer_path.relative_to(REPOSITORY).as_posix()
        if committed.get("uvLockSha256") != content["uvLockSha256"]:
            problems.append(f"{name} was measured on another uv.lock than {commit[:12]} holds")
        ours, theirs = set(content["digests"]), set(committed.get("digests") or {})
        if ours != theirs:
            problems.append(
                f"{name} subtracts {len(theirs - ours)} digests no locked distribution ships here "
                f"and misses {len(ours - theirs)} that one does: "
                f"{sorted(theirs ^ ours)[:5]}"
            )
        if committed.get("measuredOn") == content["measuredOn"]:
            if shingles.render_layer(committed) != shingles.render_layer(content):
                problems.append(f"{name} attributes its digests to other distributions than this environment")
        else:
            print(
                f"{name} was measured on {committed.get('measuredOn')}; this host is "
                f"{content['measuredOn']}, so only its digests and lockfile were compared"
            )
    for problem in problems:
        print(problem, file=sys.stderr)
    if problems:
        print(f"regenerate both with `python3 scripts/parity/repin.py shingles {commit}`", file=sys.stderr)
        return 1
    print(f"the committed digest set and third-party layer match: {summary}")
    return 0


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("list", help="one line per manifest entry")
    planning = commands.add_parser(
        "plan", help="every entry in re-pin order, with its commit, driver and owning story"
    )
    planning.add_argument("commit", help="the commit the re-pin targets")
    commands.add_parser(
        "open-transition",
        help="list every entry not at the target in the transition list, after a re-pin edit",
    )
    commands.add_parser("accepted", help="the commits the manifest accepts, pin first")
    materializing = commands.add_parser(
        "materialize", help="build the attested reference environment of a commit"
    )
    materializing.add_argument("commits", nargs="*", help="commits or revisions to materialize")
    materializing.add_argument(
        "--accepted",
        action="store_true",
        help="materialize every commit the manifest accepts",
    )
    materializing.add_argument(
        "--checkout",
        type=Path,
        default=DEFAULT_REFERENCE,
        help="the reference checkout the commits are read from (never written)",
    )
    censusing = commands.add_parser("census", help="compare the pin with the newest upstream release")
    censusing.add_argument("--check", action="store_true", help="record nothing; fail when the pin is behind")
    censusing.add_argument("--url", default=sentinel.UPSTREAM, help="the upstream repository")
    censusing.add_argument(
        "--store",
        type=Path,
        default=materialize.DEFAULT_CACHE / "upstream.git",
        help="the bare repository upstream commits are fetched into",
    )
    checking = commands.add_parser(
        "check", help="recapture entries and compare them with the committed corpora"
    )
    checking.add_argument("corpora", nargs="*", help="manifest paths or unique suffixes")
    checking.add_argument(
        "--unprobed",
        action="store_true",
        help="also check one entry of every driver whose reference side the run did not record",
    )
    checking.add_argument(
        "--records",
        type=Path,
        default=default_records(),
        help="where the run recorded its drivers (`drivers.jsonl`) and checks go",
    )
    attesting = commands.add_parser("attest", help="check an environment against its attestation")
    attesting.add_argument("commit")
    attesting.add_argument("--full", action="store_true", help="hash every attested file")
    commands.add_parser("verify", help="check every provenance rule")
    shingling = commands.add_parser(
        "shingles", help="write the digests of the reference's 8-word prose runs at a commit"
    )
    shingling.add_argument(
        "commit",
        nargs="?",
        default=EXPECTED_COMMIT,
        help="a commit with an attested environment (the pin by default)",
    )
    shingling.add_argument(
        "--check",
        action="store_true",
        help="recompute both files and compare them with the committed ones, writing nothing",
    )
    owing = commands.add_parser("debt", help="measure and record the debt of corpora")
    owing.add_argument("corpora", nargs="*")
    owing.add_argument("--all", action="store_true", help="every corpus a replay reads")
    owing.add_argument(
        "--runs",
        type=int,
        default=debt.MINIMUM_RUNS,
        help=f"how many measurements must agree (at least {debt.MINIMUM_RUNS})",
    )
    capturing = commands.add_parser("capture", help="regenerate one entry through its driver")
    capturing.add_argument("corpus")
    capturing.add_argument(
        "--commit",
        default=EXPECTED_COMMIT,
        help="the commit the checkout is at, when it is not the pin (a re-pin)",
    )
    capturing.add_argument(
        "--accept-shrink",
        action="store_true",
        help="lower the floors to what this capture counted",
    )
    capturing.add_argument(
        "--hand-assembled",
        action="store_true",
        help="record an assembled corpus as it now stands, after merging its measurement",
    )
    removing = commands.add_parser("remove", help="delete a corpus whose oracle is gone")
    removing.add_argument("corpus")
    adding = commands.add_parser("add", help="list a corpus that is not in the manifest yet")
    adding.add_argument("corpus")
    adding.add_argument("--driver", help="the script and arguments that regenerate it")
    adding.add_argument(
        "--output-flag",
        default="--output",
        help="the flag that names where the driver writes; empty when it writes in place",
    )
    adding.add_argument("--test", required=True, help="<file>::<function> that replays it")
    adding.add_argument("--row", required=True, help="the docs/parity.md row it answers for")
    adding.add_argument(
        "--kind",
        choices=["corpus", "assembled", "fixture"],
        default="corpus",
        help="captured by its driver, merged by hand from its driver's measurement, or hand-owned",
    )
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        if arguments.command == "list":
            for entry in provenance.load_manifest()["entries"]:
                commit = (entry.get("commit") or "-")[:12]
                print(
                    f"{entry['corpus']}  {driver_line(entry)}  {entry['test']}  "
                    f"row {entry['row']}  {commit}{'  off-target' if entry.get('offTarget') else ''}"
                )
            return 0
        if arguments.command == "plan":
            manifest = provenance.load_manifest()
            target = arguments.commit
            if len(target) != 40 or any(char not in "0123456789abcdef" for char in target):
                raise RepinError(f"`{target}` is not a full commit; give the 40-character SHA")
            for line in plan(manifest, target, load_plan()):
                print(line)
            return 0
        if arguments.command == "open-transition":
            manifest = provenance.load_manifest()
            if not PREVIOUS_COMMIT:
                raise RepinError(
                    "`PREVIOUS_COMMIT` is empty; name the commit the re-pin leaves in "
                    "crates/vibe-core/src/parity.rs and scripts/parity/pin.py first"
                )
            manifest["transition"] = sorted(
                entry["corpus"]
                for entry in manifest["entries"]
                if entry.get("commit") == PREVIOUS_COMMIT
                or (
                    entry["kind"] == "fixture"
                    and provenance.recorded_commit(
                        json.loads((REPOSITORY / entry["corpus"]).read_text(encoding="utf-8"))
                    )
                    == PREVIOUS_COMMIT
                )
            )
            write_manifest(manifest)
            print(f"the transition list names {len(manifest['transition'])} corpora at {PREVIOUS_COMMIT[:12]}")
            return 0
        if arguments.command == "census":
            entry = sentinel.take(arguments.url, arguments.store.resolve())
            print("\n".join(sentinel.report(entry)))
            if arguments.check:
                return 1 if entry["releasesBehind"] else 0
            sentinel.record(entry)
            print("\nrecorded scripts/parity/upstream-census.json and the upstream row of docs/parity.md")
            return 0
        if arguments.command == "accepted":
            print(" ".join(dict.fromkeys(accepted_commits(provenance.load_manifest()))))
            return 0
        if arguments.command == "materialize":
            commits = list(arguments.commits)
            if arguments.accepted:
                commits += accepted_commits(provenance.load_manifest())
            if not commits:
                raise RepinError("name the commits to materialize, or pass `--accepted`")
            for commit in dict.fromkeys(commits):
                print(materialize.materialize(commit, arguments.checkout))
            return 0
        if arguments.command == "check":
            if not arguments.corpora and not arguments.unprobed:
                raise RepinError("name the corpora to check, or pass `--unprobed`")
            return check(arguments.corpora, arguments.unprobed, arguments.records)
        if arguments.command == "attest":
            commit = arguments.commit
            root = materialize.environment_root(commit)
            attestation = materialize.verify(root, commit, arguments.full)
            print(
                f"{root} matches its attestation for {commit}: {len(attestation['files'])} files, "
                f"python {attestation['python']}, {attestation['platform']}"
            )
            return 0
        if arguments.command == "shingles":
            return write_shingles(arguments.commit, arguments.check)
        if arguments.command == "debt":
            return measure_debts(arguments.corpora, arguments.all, arguments.runs)
        if arguments.command == "verify":
            manifest = provenance.load_manifest()
            problems = verify(manifest)
            for problem in problems:
                print(problem, file=sys.stderr)
            if problems:
                return 1
            print(f"{len(manifest['entries'])} manifest entries verified")
            return 0
        if arguments.command == "capture":
            refreshed = capture(
                arguments.corpus, arguments.commit, arguments.accept_shrink, arguments.hand_assembled
            )
            manifest = provenance.load_manifest()
            for corpus in refreshed:
                print(f"captured {corpus}")
                if find_entry(manifest, corpus).get("debt"):
                    print(f"  its debt was measured against the corpus it replaced: `{REPIN} debt {corpus}`")
            return 0
        if arguments.command == "remove":
            print(f"removed {remove(arguments)}")
            return 0
        print(f"added {add(arguments)}")
        return 0
    except RepinError as error:
        print(f"repin: {error}", file=sys.stderr)
        return 1
    except shingles.ShingleError as error:
        print(f"repin: {error}", file=sys.stderr)
        return 1
    except debt.DebtError as error:
        print(f"repin: {error}", file=sys.stderr)
        return 1
    except sentinel.SentinelError as error:
        print(f"repin: {error}", file=sys.stderr)
        return 1
    except materialize.AttestationError as error:
        print(f"repin: {error}", file=sys.stderr)
        return 2 if error.kind == "drift" else 1


if __name__ == "__main__":
    raise SystemExit(main())
