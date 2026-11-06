"""Where every committed corpus came from, and the guards that keep it so.

``scripts/parity/manifest.json`` lists every corpus a replay reads, with the
driver command that regenerates it, the test that replays it, the scorecard row
it answers for, the reference commit it was captured from, its SHA-256, and a
floor per family of observations. ``scripts/parity/repin.py`` is the only
sanctioned writer of that file and of the corpora it lists; the Rust
meta-test ``crates/vibe-core/src/parity/manifest_tests.rs`` checks the same
rules without running Python.

This module holds the rules both halves agree on: how a corpus names its
commit, how its families are counted, and how a floor follows a recapture. It
also holds the two guards every driver passes through:

* :func:`port_output` refuses a port-mode run (one that drives this port's
  binary) that would write a manifest-listed path, so a forgotten ``--output``
  cannot turn a reference corpus into a recording of the port.
* :func:`expected_commit` refuses a reference capture pointed off the pin
  unless ``repin.py capture --commit`` started it, which records the commit and
  marks the entry as captured off-target.

A shared module, not a driver: drivers import it, it imports no driver.
"""

from __future__ import annotations

import hashlib
import json
import math
import os
from pathlib import Path
import sys
from typing import Any

#: The workspace root, whichever directory a driver runs from.
REPOSITORY = Path(__file__).resolve().parents[2]

#: The manifest, relative to :data:`REPOSITORY`.
MANIFEST_RELATIVE = "scripts/parity/manifest.json"
MANIFEST = REPOSITORY / MANIFEST_RELATIVE

#: The schema version of the manifest this module reads and writes.
MANIFEST_SCHEMA = 1

#: Set by ``repin.py capture`` on the driver it runs, to the commit the capture
#: is allowed to record. A driver started any other way measures the pin.
CAPTURE_VARIABLE = "VIBE_PARITY_REPIN_COMMIT"

#: The share of a family a recapture may lose before the floor stops it.
FLOOR_RATIO = 0.95

#: Top-level keys that describe a corpus rather than hold its observations, so
#: they are not counted as families. ``unavailable`` lists what the host could
#: not capture: it shrinking is progress, not lost coverage.
METADATA_KEYS = frozenset(
    {
        "commit",
        "note",
        "platform",
        "reference",
        "referenceCommit",
        "schemaVersion",
        "unavailable",
        "version",
    }
)

#: The flags through which a driver drives this port's binary instead of the
#: reference.
PORT_FLAGS = ("--server", "--agent", "--binary", "--live-binary", "--vibe", "--vibe-acp")


def recorded_commit(corpus: Any) -> str | None:
    """The reference commit ``corpus`` says it was captured from: its
    ``reference.commit``, else its ``referenceCommit``, else its ``commit``."""

    if not isinstance(corpus, dict):
        return None
    reference = corpus.get("reference")
    if isinstance(reference, dict) and isinstance(reference.get("commit"), str):
        return reference["commit"]
    for key in ("referenceCommit", "commit"):
        if isinstance(corpus.get(key), str):
            return corpus[key]
    return None


def names_reference(corpus: Any) -> bool:
    """Whether ``corpus`` says it came from the reference: a ``reference``
    key or a recorded commit. Such a file under a crate's ``tests/`` has to
    be listed in the manifest."""

    return isinstance(corpus, dict) and (
        "reference" in corpus or recorded_commit(corpus) is not None
    )


def families(corpus: Any) -> dict[str, int]:
    """How many observations each top-level family of ``corpus`` holds: the
    length of every array or object outside :data:`METADATA_KEYS`."""

    if not isinstance(corpus, dict):
        return {}
    return {
        key: len(value)
        for key, value in sorted(corpus.items())
        if key not in METADATA_KEYS and isinstance(value, (list, dict))
    }


def minimum_floor(count: int) -> int:
    """The weakest floor a family of ``count`` observations may carry."""

    return math.ceil(count * FLOOR_RATIO)


def raised_floors(previous: dict[str, int], counts: dict[str, int]) -> dict[str, int]:
    """The floors after a capture that counted ``counts``: a floor only rises,
    so a recapture that lost coverage still fails against the old one."""

    return {
        family: max(previous.get(family, 0), minimum_floor(count))
        for family, count in counts.items()
    } | {family: floor for family, floor in previous.items() if family not in counts}


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load_manifest(path: Path = MANIFEST) -> dict[str, Any]:
    manifest = json.loads(path.read_text(encoding="utf-8"))
    if manifest.get("schemaVersion") != MANIFEST_SCHEMA:
        raise ValueError(
            f"{path} is at schema {manifest.get('schemaVersion')}, not {MANIFEST_SCHEMA}"
        )
    return manifest


def manifest_paths(path: Path = MANIFEST) -> dict[Path, str]:
    """Every path the manifest lists, resolved, mapped to how it spells it."""

    try:
        manifest = load_manifest(path)
    except (OSError, ValueError) as error:
        sys.exit(f"cannot read the parity manifest {path}: {error}")
    return {
        (REPOSITORY / entry["corpus"]).resolve(): entry["corpus"]
        for entry in manifest["entries"]
    }


def port_output(output: Path | None, default: Path, **flags: Any) -> Path:
    """Where a driver writes, refusing a port-mode run that would overwrite a
    committed corpus.

    ``flags`` maps each port flag's argparse name (``server``, ``agent``,
    ``binary``, ``live_binary``) to the value it was given. With none set this
    is a reference capture, written to ``output`` or the committed ``default``.
    With one set, ``output`` is required and may not name a manifest path:
    the driver exits before capturing anything.
    """

    given = [
        "--" + name.replace("_", "-") for name, value in flags.items() if value is not None
    ]
    if not given:
        return output if output is not None else default
    if output is None:
        sys.exit(
            f"port-mode capture needs `--output` outside the manifest paths: {given[0]} "
            "drives this port, and the default output is a committed corpus"
        )
    listed = manifest_paths().get(output.resolve())
    if listed is not None:
        sys.exit(
            f"port-mode capture would overwrite the committed corpus {listed}: {given[0]} "
            "drives this port; pass `--output` outside the manifest paths"
        )
    return output


def expected_commit(requested: str | None, pinned: str) -> str:
    """The commit a reference capture has to find the checkout at.

    ``requested`` is what the driver was asked for, ``pinned`` the pin. Inside
    ``repin.py capture`` the commit it set in :data:`CAPTURE_VARIABLE` wins, so
    an off-target capture is always one the manifest records. Anywhere else a
    capture measures the pin: an empty ``--expected-commit``, ``--allow-unpinned``
    or another commit is refused, since the corpus it wrote would record a
    commit no replay accepts.
    """

    sanctioned = os.environ.get(CAPTURE_VARIABLE)
    if sanctioned:
        return sanctioned
    if requested == pinned:
        return pinned
    shown = f"`{requested}`" if requested else "any commit"
    raise PermissionError(
        f"a capture asked for {shown} instead of the pinned {pinned}; an off-pin capture runs "
        "only through `python3 scripts/parity/repin.py capture <corpus> --commit <sha>`, which "
        "records the commit in the manifest"
    )
