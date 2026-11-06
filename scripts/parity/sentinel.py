"""The upstream sentinel: how far the pin is behind upstream's newest release,
and what that release moved at the public boundaries.

``git ls-remote`` lists upstream's tags. When the newest release is not the
pin, both commits are fetched into a store of this repository's own
(``.parity/upstream.git``, never the shared checkout), exported with
``git archive``, and compared through the static census of
``scripts/parity/census.py``. Nothing fetched is executed, which is what lets
the daily workflow run it with a read-only token.

``scripts/parity/repin.py census`` records the result in
``scripts/parity/upstream-census.json`` and in the "against upstream ``HEAD``"
row of ``docs/parity.md``; ``crates/vibe-core/src/parity/scorecard_tests.rs``
fails when that row names an older tag than the file. With ``--check`` it
records nothing and fails when the pin is behind or upstream cannot be read.

A shared module, not a driver: ``repin.py`` imports it, it imports no driver.
"""

from __future__ import annotations

import datetime
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
from typing import Any

import census
import host_env
from pin import EXPECTED_COMMIT, EXPECTED_VERSION

#: The workspace root, whichever directory this runs from.
REPOSITORY = Path(__file__).resolve().parents[2]

UPSTREAM = "https://github.com/mistralai/mistral-vibe"
CENSUS = REPOSITORY / "scripts/parity/upstream-census.json"
SCORECARD = REPOSITORY / "docs/parity.md"
UPSTREAM_ROW = "| Weighted score against upstream `HEAD` |"
RELEASE = re.compile(r"^v(\d+)\.(\d+)\.(\d+)$")


class SentinelError(RuntimeError):
    """Upstream could not be read, so the drift is unknown."""


def release(tag: str) -> tuple[int, ...] | None:
    match = RELEASE.match(tag)
    return tuple(int(part) for part in match.groups()) if match else None


def upstream_tags(url: str) -> dict[str, str]:
    """Every release tag upstream publishes, with the commit it names."""

    try:
        result = subprocess.run(
            ["git", "ls-remote", "--tags", url],
            capture_output=True,
            text=True,
            timeout=120,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise SentinelError(f"upstream unreachable: {error}; drift unknown") from None
    if result.returncode != 0:
        raise SentinelError(f"upstream unreachable: {result.stderr.strip()}; drift unknown")
    tags: dict[str, str] = {}
    peeled: dict[str, str] = {}
    for line in result.stdout.splitlines():
        commit, _, reference = line.partition("\t")
        name = reference.removeprefix("refs/tags/")
        if name.endswith("^{}"):
            peeled[name[:-3]] = commit
        else:
            tags[name] = commit
    found = {name: peeled.get(name, commit) for name, commit in tags.items() if release(name)}
    if not found:
        raise SentinelError(f"upstream unreachable: {url} lists no release tag; drift unknown")
    return found


def _git(*arguments: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["git", *arguments], capture_output=True, text=True, check=False)


def fetched_tree(store: Path, url: str, commit: str, destination: Path) -> Path:
    """The tree of ``commit``, fetched into ``store`` and exported to
    ``destination``."""

    if not (store / "HEAD").is_file():
        _git("init", "--quiet", "--bare", str(store))
    if _git("-C", str(store), "cat-file", "-e", f"{commit}^{{commit}}").returncode != 0:
        fetched = _git("-C", str(store), "fetch", "--quiet", "--depth", "1", "--no-tags", url, commit)
        if fetched.returncode != 0:
            raise SentinelError(
                f"upstream unreachable: fetching {commit} failed: {fetched.stderr.strip()}; drift unknown"
            )
    host_env.remove_tree(destination, missing_ok=True)
    destination.mkdir(parents=True)
    archive = destination.with_suffix(".tar")
    exported = _git("-C", str(store), "archive", "--format=tar", "-o", str(archive), commit)
    if exported.returncode != 0:
        raise SentinelError(f"git archive {commit} failed: {exported.stderr.strip()}")
    with tarfile.open(archive) as bundle:
        bundle.extractall(destination, filter="data")
    archive.unlink()
    return destination


def take(url: str, store: Path) -> dict[str, Any]:
    """The pin against the newest upstream release, as the census file
    records it."""

    tags = upstream_tags(url)
    newest = max(tags, key=lambda name: release(name) or ())
    pinned = release(f"v{EXPECTED_VERSION}") or ()
    behind = sorted((name for name in tags if (release(name) or ()) > pinned), key=release)
    notes = []
    pin_tag = f"v{EXPECTED_VERSION}"
    if pin_tag in tags and tags[pin_tag] != EXPECTED_COMMIT:
        notes.append(f"tag {pin_tag} now names {tags[pin_tag]}; the pin is unaffected")
    record: dict[str, Any] = {
        "schemaVersion": 1,
        "note": "Written by scripts/parity/repin.py census; scorecard_tests checks the upstream row of docs/parity.md against it.",
        "checkedAt": datetime.date.today().isoformat(),
        "pin": {"commit": EXPECTED_COMMIT, "version": EXPECTED_VERSION},
        "newest": {"tag": newest, "commit": tags[newest]},
        "releasesBehind": behind,
        "notes": notes,
        "summary": "up to date",
        "diff": None,
    }
    if not behind:
        return record
    workspace = store.parent / f"census-{os.getpid()}"
    try:
        older = census.census(fetched_tree(store, url, EXPECTED_COMMIT, workspace / "pin"))
        newer = census.census(fetched_tree(store, url, tags[newest], workspace / "newest"))
    finally:
        host_env.remove_tree(workspace, missing_ok=True)
    record["diff"] = census.diff(older, newer)
    record["summary"] = census.summary(record["diff"])
    return record


def report(record: dict[str, Any]) -> list[str]:
    """The Markdown a job summary shows."""

    behind = record["releasesBehind"]
    lines = [
        "### Upstream sentinel",
        "",
        f"Pin: `{record['pin']['commit'][:7]}` (v{record['pin']['version']}). Newest upstream tag: "
        f"`{record['newest']['tag']}` (`{record['newest']['commit'][:7]}`).",
        "",
    ]
    lines.extend(f"- {note}" for note in record["notes"])
    if not behind:
        lines.append("The pin is the newest upstream release.")
        return lines
    lines += [f"**{len(behind)} releases behind:** " + ", ".join(f"`{tag}`" for tag in behind), ""]
    lines += ["| Boundary | Added | Removed |", "|---|---|---|"]
    for boundary in census.BOUNDARIES[:-1]:
        change = record["diff"][boundary]
        lines.append(f"| {boundary} | {len(change['added'])} | {len(change['removed'])} |")
    missing = record["diff"]["scorecardPaths"]["missing"]
    lines += ["", f"Scorecard paths missing at `{record['newest']['tag']}`: {len(missing)}"]
    for boundary in census.BOUNDARIES[:-1]:
        change = record["diff"][boundary]
        if change["added"] or change["removed"]:
            lines += ["", f"<details><summary>{boundary}</summary>", ""]
            lines += [f"- added `{name}`" for name in change["added"]]
            lines += [f"- removed `{name}`" for name in change["removed"]]
            lines += ["", "</details>"]
    return lines


def row(record: dict[str, Any]) -> str:
    """The "against upstream ``HEAD``" row of the scorecard header."""

    pin = f"`{record['pin']['commit'][:7]}`, v{record['pin']['version']}"
    newest = f"newest upstream tag `{record['newest']['tag']}` (`{record['newest']['commit'][:7]}`)"
    if not record["releasesBehind"]:
        return (
            f"{UPSTREAM_ROW} Equal to the pinned score: the pin ({pin}) is the {newest} on "
            f"{record['checkedAt']}, per `scripts/parity/upstream-census.json` (`repin.py census`) |"
        )
    return (
        f"{UPSTREAM_ROW} Not measured: the pin ({pin}) is {len(record['releasesBehind'])} releases "
        f"behind the {newest}. The static census of that tag, committed by `repin.py census` on "
        f"{record['checkedAt']} in `scripts/parity/upstream-census.json`, differs from the pin by "
        f"{record['summary']}; the daily upstream sentinel retakes it |"
    )


def record(entry: dict[str, Any]) -> None:
    """Writes ``entry`` to the census file and the upstream row."""

    CENSUS.write_text(json.dumps(entry, indent=2) + "\n", encoding="utf-8", newline="\n")
    lines = SCORECARD.read_text(encoding="utf-8").split("\n")
    index = next((number for number, line in enumerate(lines) if line.startswith(UPSTREAM_ROW)), None)
    if index is None:
        raise SentinelError(f"docs/parity.md carries no `{UPSTREAM_ROW}` row")
    lines[index] = row(entry)
    SCORECARD.write_text("\n".join(lines), encoding="utf-8", newline="\n")
