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
    python3 scripts/parity/repin.py verify
    python3 scripts/parity/repin.py capture <corpus> [--commit <sha>] [--accept-shrink]
    python3 scripts/parity/repin.py add <corpus> --driver "<script> <args>" \\
        --test <file>::<function> --row <n> [--output-flag <flag>] [--kind <kind>]
    python3 scripts/parity/repin.py materialize [<sha>... | --accepted] [--checkout <path>]
    python3 scripts/parity/repin.py attest <sha> [--full]
    python3 scripts/parity/repin.py check [<corpus>...] [--unprobed] [--records <dir>]
    python3 scripts/parity/repin.py accepted
    python3 scripts/parity/repin.py census [--check] [--url <upstream>]
    python3 scripts/parity/repin.py shingles <sha>

``<corpus>`` is a manifest path or a suffix of exactly one, such as
``acp-parity/corpus.json``. ``capture`` runs the entry's driver (and refreshes
every entry the same command writes), stages each output beside its corpus,
checks the commit it recorded, moves it into place, and updates the manifest in
the same invocation. ``--commit`` captures a checkout at another commit, for a
re-pin; the entry is then marked as captured off-target until the pin names
that commit. ``--accept-shrink`` lowers the floors of a capture that lost
observations on purpose; without it the floors only rise.

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
at ``<sha>`` (``scripts/parity/shingles.py``), read from the pristine tree of
its attested environment, to ``crates/vibe-core/tests/licensing/``, refusing a
set above 4 MB. The clean-room guard
``crates/vibe-core/src/parity/licensing_tests.rs`` fails on any tracked run
that shares one outside its baseline, and writes the baseline it measured to
``target/parity/licensing/baseline.txt``.

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

import materialize
import provenance
import sentinel
import shingles
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT
from provenance import MANIFEST, REPOSITORY

SCORECARD = REPOSITORY / "docs/parity.md"
REPIN = "python3 scripts/parity/repin.py"


class RepinError(RuntimeError):
    """A manifest, a corpus, or a capture that breaks a provenance rule."""


# --------------------------------------------------------------------------
# Reading
# --------------------------------------------------------------------------


def accepted_commits(manifest: dict[str, Any]) -> list[str]:
    """The pin, then every commit the transition list still accepts."""

    return [EXPECTED_COMMIT, *manifest.get("transition", [])]


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

    problems: list[str] = []
    accepted = accepted_commits(manifest)
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
    for entry in entries:
        problems.extend(verify_entry(entry, accepted, tracked, rows))
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
                differing = capture(corpus, EXPECTED_COMMIT, False, False, compare=True)
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
        entry, accepted_commits(manifest), tracked_files() | {corpus}, scorecard_rows()
    )
    if problems:
        raise RepinError("\n".join(problems))
    manifest["entries"].append(entry)
    write_manifest(manifest)
    return corpus


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("list", help="one line per manifest entry")
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
    shingling.add_argument("commit", help="a commit with an attested environment")
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
            commit = arguments.commit
            root = materialize.environment_root(commit)
            materialize.verify(root, commit)
            values, files = shingles.digests(root / materialize.SOURCE)
            target = shingles.write(commit, values)
            print(
                f"{target.relative_to(REPOSITORY).as_posix()}: {len(values)} shingle digests "
                f"from {files} files, {target.stat().st_size} bytes"
            )
            return 0
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
            for corpus in refreshed:
                print(f"captured {corpus}")
            return 0
        print(f"added {add(arguments)}")
        return 0
    except RepinError as error:
        print(f"repin: {error}", file=sys.stderr)
        return 1
    except shingles.ShingleError as error:
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
