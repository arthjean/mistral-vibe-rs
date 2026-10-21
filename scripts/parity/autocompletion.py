#!/usr/bin/env python3
"""Capture what the pinned Python reference's file index answers.

The reference splits `@` completion into a store, a rules compiler, a watcher
and a ranking completer, and three of those four take everything they need as
arguments. ``FileIndexStore`` receives its ignore rules and its stats object,
``IgnoreRules.should_ignore`` is a pure function of three strings once a root
is compiled, and ``PathCompleter._score_matches`` takes a list of entries and a
search context. A capture therefore drives the real reference objects over
scratch trees this script writes itself, with no watcher thread, no network and
no editor. The store lists a git work tree through `git ls-files` and walks
the tree only outside one, so the walked scratch trees must sit outside any
work tree: this script fails rather than record the git-backed index under
those names. The git-backed index has families of its own, measured over a
repository this script initializes with every git configuration source but
its own pinned, so a workstation's global excludes never reach the corpus.

Thirteen families come out, and are what the Rust replay compares:

``constants``     the caps, the threshold, the ASCII limit and the 36 defaults
``ignoreRules``   the ``should_ignore`` verdict per ``(rel, name, is_dir)``
``walk``          the entry set a rebuild holds per fixture tree
``changes``       the entry set and the stats after each ``apply_changes`` call
``ranking``       the ordered candidates and the ``MatchRank`` tuple per query
``gitWalk``       the entry set a rebuild holds inside a scripted git work tree
``gitChanges``    the dirty flag, entries and stats around a watched change
``collect``       the labels and the range ``PathCompleter`` answers per prompt
``controller``    what the path controller shows and answers per key
``inlineSkill``   the mid-prompt skill ghost and what accepting it replaces
``pathPrompt``    the resources ``build_path_prompt_payload`` finds per message
``watchFilter``   the ``watchfiles.DefaultFilter`` verdict per changed path
``fuzzy``         ``fuzzy_match`` over a seeded sweep of pattern and text pairs

Two artifacts come out of a run::

    .parity/autocompletion-corpus.json                the full capture, gitignored
    crates/vibe-cli/tests/autocompletion/corpus.json  the committed corpus

Every name in the corpus is supplied by the fixtures of
``autocompletion_cases.py``, which this script authors: no reference prose, no
reference source and no machine path is recorded, only relative paths,
verdicts, counts and scores.

Two normalizations are applied and both are recorded in the corpus ``note``.
The reference holds its index in ``scandir`` order after a rebuild and in
relative-path order only after an incremental update invalidated the cached
order, so entry lists are sorted by relative path before being recorded and the
ranking family is driven over a sorted entry list. Fuzzy scores are floats
upstream and integers here, so a score is recorded in hundredths, which is the
scale ``crates/vibe-cli/src/tui/completion/fuzzy.rs`` already works in; the
capture fails if a score is not an exact hundredth.

Usage::

    scripts/parity/autocompletion.py --reference /path/to/reference --corpus

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
import subprocess
import sys
import tempfile
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT
from reference import DEFAULT_CACHE, OracleError, reexecute_pinned
from inprocess import SocketGuard, patched_environ
import host_env  # noqa: E402
from autocompletion_cases import (
    CHANGE_SEQUENCES,
    COLLECT_OUTSIDE,
    COLLECT_QUERIES,
    COLLECT_ROOT,
    FILE_BODY,
    FIXTURES,
    FUZZY_ALPHABET,
    FUZZY_COUNT,
    FUZZY_SEED,
    GIT_CHANGE_SEQUENCES,
    GIT_FIXTURE,
    GIT_ROOTS,
    IGNORE_PROBES,
    INLINE_CASES,
    INLINE_SKILLS,
    PROMPT_MESSAGES,
    PROMPT_TREE,
    RANKING_QUERIES,
    WATCH_FILTER_PROBES,
    fuzzy_pairs,
)

SCHEMA_VERSION = 1
DEFAULT_OUTPUT = Path(".parity/autocompletion-corpus.json")
DEFAULT_CORPUS = Path("crates/vibe-cli/tests/autocompletion/corpus.json")
#: Fuzzy scores are floats upstream. Hundredths make them exact integers, which
#: is the scale this port's matcher already computes in.
SCORE_SCALE = 100


# --------------------------------------------------------------------------
# Isolation
# --------------------------------------------------------------------------


GUARD = SocketGuard()


# --------------------------------------------------------------------------
# Fixtures
# --------------------------------------------------------------------------


def materialize(root: Path, fixture: dict[str, Any]) -> None:
    """Writes one fixture tree under `root`."""

    for entry in fixture["tree"]:
        if entry.endswith("/"):
            (root / entry.rstrip("/")).mkdir(parents=True, exist_ok=True)
            continue
        target = root / entry
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(FILE_BODY, encoding="utf-8", newline="")
    gitignore = fixture.get("gitignore")
    if gitignore is not None:
        (root / ".gitignore").write_text(gitignore, encoding="utf-8", newline="")


def mutate(root: Path, mutation: dict[str, Any]) -> None:
    """Applies one scripted filesystem mutation to a materialized tree."""

    operation = mutation["op"]
    target = root / mutation["path"]
    if operation == "createFile":
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(FILE_BODY, encoding="utf-8", newline="")
        return
    if operation == "createDir":
        target.mkdir(parents=True, exist_ok=True)
        return
    if operation == "modifyFile":
        target.write_text(FILE_BODY + "modified\n", encoding="utf-8", newline="")
        return
    if operation == "delete":
        if target.is_dir():
            host_env.remove_tree(target)
        else:
            target.unlink(missing_ok=True)
        return
    if operation == "rename":
        destination = root / mutation["to"]
        destination.parent.mkdir(parents=True, exist_ok=True)
        target.rename(destination)
        return
    raise OracleError(f"unknown fixture mutation `{operation}`")


# --------------------------------------------------------------------------
# Capture
# --------------------------------------------------------------------------


def _entries(snapshot: list[Any]) -> list[dict[str, Any]]:
    """One entry list, normalized to relative-path order.

    The reference holds a rebuilt index in `scandir` order and a mutated one in
    relative-path order, so its order is not a contract; sorting both sides is
    the same normalization the `grep` row of `docs/parity.md` already records.
    """

    return [
        {"rel": entry.rel, "name": entry.name, "isDir": entry.is_dir}
        for entry in sorted(snapshot, key=lambda entry: entry.rel)
    ]


def capture_constants() -> dict[str, Any]:
    import inspect

    from vibe.cli.autocompletion.completers import DEFAULT_TARGET_MATCHES, PathCompleter
    from vibe.cli.autocompletion.file_indexer.ignore_rules import (
        DEFAULT_IGNORE_PATTERNS,
        WALK_SKIP_DIR_NAMES,
        IgnoreRules,
    )
    from vibe.cli.autocompletion.file_indexer.indexer import FileIndexer
    from vibe.cli.autocompletion.file_indexer.store import ASCII_CODEPOINT_LIMIT

    # The processing cap is no longer a module constant: it is the default of
    # the completer's own parameter, which is `None` (uncapped) at the pin
    # (vibe/cli/autocompletion/completers.py:117-125) and serializes as null.
    max_entries = (
        inspect.signature(PathCompleter.__init__)
        .parameters["max_entries_to_process"]
        .default
    )

    indexer = FileIndexer()
    try:
        threshold = indexer._store._mass_change_threshold
    finally:
        indexer.shutdown()

    compiled = [
        {
            "stripped": pattern.stripped,
            "isExclude": pattern.is_exclude,
            "dirOnly": pattern.dir_only,
            "nameOnly": pattern.name_only,
            "anchorRoot": pattern.anchor_root,
        }
        for pattern in IgnoreRules()._compile_default_patterns()
    ]
    if len(compiled) != len(DEFAULT_IGNORE_PATTERNS):
        raise OracleError("the compiled default patterns lost an entry")

    return {
        "maxEntriesToProcess": max_entries,
        "targetMatches": DEFAULT_TARGET_MATCHES,
        "massChangeThreshold": threshold,
        "asciiCodepointLimit": ASCII_CODEPOINT_LIMIT,
        "scoreScale": SCORE_SCALE,
        "defaultIgnorePatterns": [
            {"raw": raw, "isExclude": is_exclude}
            for raw, is_exclude in DEFAULT_IGNORE_PATTERNS
        ],
        "compiledDefaultPatterns": compiled,
        "walkSkipDirNames": sorted(WALK_SKIP_DIR_NAMES),
    }


def _centis(value: float) -> int:
    scaled = value * SCORE_SCALE
    rounded = round(scaled)
    if abs(scaled - rounded) > 1e-6:
        raise OracleError(f"{value} is not an exact hundredth")
    return int(rounded)


def capture_ignore_rules(scratch: Path) -> list[dict[str, Any]]:
    from vibe.cli.autocompletion.file_indexer.ignore_rules import IgnoreRules

    records: list[dict[str, Any]] = []
    for fixture_id, root in _roots(scratch).items():
        probes = [probe for probe in IGNORE_PROBES if probe[0] == fixture_id]
        if not probes:
            continue
        rules = IgnoreRules()
        rules.ensure_for_root(root)
        for _, rel, is_dir in probes:
            name = rel.rsplit("/", 1)[-1]
            records.append(
                {
                    "case": f"{fixture_id}/{rel}{'/' if is_dir else ''}",
                    "fixture": fixture_id,
                    "rel": rel,
                    "name": name,
                    "isDir": is_dir,
                    "ignored": rules.should_ignore(rel, name, is_dir),
                }
            )
    return records


def capture_walk(scratch: Path) -> list[dict[str, Any]]:
    from vibe.cli.autocompletion.file_indexer.ignore_rules import IgnoreRules
    from vibe.cli.autocompletion.file_indexer.store import FileIndexStats, FileIndexStore

    records: list[dict[str, Any]] = []
    for fixture_id, root in _roots(scratch).items():
        stats = FileIndexStats()
        store = FileIndexStore(IgnoreRules(), stats)
        _rebuild_walked(store, root)
        records.append(
            {
                "case": fixture_id,
                "fixture": fixture_id,
                "entries": _entries(store.snapshot()),
                "stats": {
                    "rebuilds": stats.rebuilds,
                    "incrementalUpdates": stats.incremental_updates,
                },
            }
        )
    return records


def capture_changes() -> list[dict[str, Any]]:
    from vibe.cli.autocompletion.file_indexer.ignore_rules import IgnoreRules
    from vibe.cli.autocompletion.file_indexer.store import FileIndexStats, FileIndexStore
    # The watcher binds `Change` only for type checking at the pin
    # (vibe/cli/autocompletion/file_indexer/watcher.py:6-9); the store compares
    # against watchfiles' own members by identity (store.py:127,146).
    from watchfiles import Change

    kinds = {
        "added": Change.added,
        "modified": Change.modified,
        "deleted": Change.deleted,
    }
    records: list[dict[str, Any]] = []
    for sequence in CHANGE_SEQUENCES:
        case = sequence["case"]
        with tempfile.TemporaryDirectory(prefix="autocompletion-changes-") as scratch_dir:
            enclosure = Path(scratch_dir)
            root = enclosure / "root"
            root.mkdir()
            materialize(root, FIXTURES[sequence["fixture"]])
            stats = FileIndexStats()
            store = FileIndexStore(IgnoreRules(), stats)
            _rebuild_walked(store, root)
            steps: list[dict[str, Any]] = []
            for index, step in enumerate(sequence["steps"]):
                for mutation in step["mutations"]:
                    mutate(root, mutation)
                changes = [
                    (kinds[kind], (root / relative).resolve())
                    for kind, relative in step["changes"]
                ]
                store.apply_changes(changes)
                steps.append(
                    {
                        "step": index,
                        "mutations": step["mutations"],
                        "changes": step["changes"],
                        "entries": _entries(store.snapshot()),
                        "stats": {
                            "rebuilds": stats.rebuilds,
                            "incrementalUpdates": stats.incremental_updates,
                        },
                    }
                )
        records.append(
            {"case": case, "fixture": sequence["fixture"], "steps": steps}
        )
    return records


def capture_ranking(scratch: Path) -> list[dict[str, Any]]:
    from vibe.cli.autocompletion.completers import PathCompleter
    from vibe.cli.autocompletion.file_indexer.ignore_rules import IgnoreRules
    from vibe.cli.autocompletion.file_indexer.store import FileIndexStats, FileIndexStore

    roots = _roots(scratch)
    indexes: dict[str, list[Any]] = {}
    for fixture_id, root in roots.items():
        store = FileIndexStore(IgnoreRules(), FileIndexStats())
        _rebuild_walked(store, root)
        indexes[fixture_id] = sorted(store.snapshot(), key=lambda entry: entry.rel)

    completer = PathCompleter()
    try:
        records: list[dict[str, Any]] = []
        for fixture_id, query in RANKING_QUERIES:
            context = completer._build_search_context(query)
            scored = completer._score_matches(indexes[fixture_id], context)
            records.append(
                {
                    "case": f"{fixture_id}/{query or '<empty>'}",
                    "fixture": fixture_id,
                    "query": query,
                    "candidates": [
                        {"label": label, "rank": _rank(rank)} for label, rank in scored
                    ],
                }
            )
        return records
    finally:
        completer._indexer.shutdown()


def _rebuild_walked(store: Any, root: Path) -> None:
    """Rebuild through the ignore-rule walk this corpus measures.

    A rebuild tries `git ls-files` first and walks the tree only when that
    fails (vibe/cli/autocompletion/file_indexer/store.py:80-103). Inside a work
    tree the defaults and the rules compiler are bypassed and a change only
    marks the store dirty (store.py:116-124), which would record a different
    contract under the same case names, so a git-backed rebuild fails the run.
    """

    if not store.rebuild(root):
        raise OracleError(f"the rebuild of {root.name} was canceled")
    if store.is_git_backed:
        raise OracleError(
            f"{root.name} sits inside a git work tree, so the index came from "
            "`git ls-files` instead of the ignore-rule walk; move TMPDIR outside it"
        )


def _rank(rank: Any) -> dict[str, Any]:
    return {
        "exactDirectory": int(rank.exact_directory),
        "immediateChildOfExactPath": int(rank.immediate_child_of_exact_path),
        "exactFilename": int(rank.exact_filename),
        "preferredStemMatch": int(rank.preferred_stem_match),
        "exactStem": int(rank.exact_stem),
        "stemPrefix": int(rank.stem_prefix),
        "namePrefix": int(rank.name_prefix),
        "extensionMatch": int(rank.extension_match),
        "fuzzyScore": _centis(rank.fuzzy_score),
        "shallowPath": rank.shallow_path,
    }


def _roots(scratch: Path) -> dict[str, Path]:
    return {fixture_id: scratch / fixture_id for fixture_id in FIXTURES}


# --------------------------------------------------------------------------
# The git-backed index
# --------------------------------------------------------------------------

def _run_git(repository: Path, *arguments: str) -> None:
    # No system or global configuration, so only what this script writes into
    # the repository reaches the listing.
    environment = {**os.environ, **host_env.git_isolated()}
    result = subprocess.run(
        ["git", *arguments],
        cwd=repository,
        capture_output=True,
        text=True,
        check=False,
        env=environment,
    )
    if result.returncode != 0:
        raise OracleError(f"git {' '.join(arguments)} failed: {result.stderr.strip()}")


def materialize_git(enclosure: Path) -> Path:
    """Writes `GIT_FIXTURE` as a repository under `enclosure` and returns it."""

    repository = enclosure / "repo"
    repository.mkdir()
    for relative in GIT_FIXTURE["tracked"] + GIT_FIXTURE["untracked"]:
        target = repository / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(FILE_BODY, encoding="utf-8", newline="")
    for relative in GIT_FIXTURE["emptyDirs"]:
        (repository / relative).mkdir(parents=True, exist_ok=True)
    for relative, text in GIT_FIXTURE["gitignores"].items():
        (repository / relative).write_text(text, encoding="utf-8", newline="")
    _run_git(repository, "init", "-q")
    _run_git(repository, "add", "-f", "--", *GIT_FIXTURE["tracked"])
    for relative in GIT_FIXTURE["deleted"]:
        (repository / relative).unlink()
    info = repository / ".git" / "info"
    info.mkdir(parents=True, exist_ok=True)
    (info / "exclude").write_text(GIT_FIXTURE["infoExclude"], encoding="utf-8", newline="")
    excludes = enclosure / "excludes"
    excludes.write_text(GIT_FIXTURE["excludesFile"], encoding="utf-8", newline="")
    _run_git(repository, "config", "core.excludesFile", excludes.as_posix())
    return repository


def capture_git_walk() -> dict[str, Any]:
    from vibe.cli.autocompletion.file_indexer.ignore_rules import IgnoreRules
    from vibe.cli.autocompletion.file_indexer.store import FileIndexStats, FileIndexStore

    records: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory(prefix="autocompletion-git-") as scratch_dir:
        repository = materialize_git(Path(scratch_dir))
        # The reference's own `git` calls inherit this process's environment.
        with patched_environ(host_env.git_isolated()):
            for relative in GIT_ROOTS:
                root = repository / relative if relative else repository
                stats = FileIndexStats()
                store = FileIndexStore(IgnoreRules(), stats)
                if not store.rebuild(root):
                    raise OracleError(f"the git rebuild of `{relative}` was canceled")
                records.append(
                    {
                        "case": relative or "<root>",
                        "root": relative,
                        "gitBacked": store.is_git_backed,
                        "entries": _entries(store.snapshot()),
                        "stats": {
                            "rebuilds": stats.rebuilds,
                            "incrementalUpdates": stats.incremental_updates,
                        },
                    }
                )
    if not all(record["gitBacked"] for record in records):
        raise OracleError("a git walk root was not listed through `git ls-files`")
    return {"fixture": GIT_FIXTURE, "cases": records}


def capture_git_changes() -> dict[str, Any]:
    from vibe.cli.autocompletion.file_indexer import FileIndexer
    from watchfiles import Change

    kinds = {
        "added": Change.added,
        "modified": Change.modified,
        "deleted": Change.deleted,
    }
    records: list[dict[str, Any]] = []
    for sequence in GIT_CHANGE_SEQUENCES:
        with tempfile.TemporaryDirectory(prefix="autocompletion-gitchanges-") as scratch_dir:
            repository = materialize_git(Path(scratch_dir))
            with patched_environ(host_env.git_isolated()):
                indexer = FileIndexer()
                try:
                    root = repository.resolve()
                    indexer.get_index(root)
                    for mutation in sequence["mutations"]:
                        mutate(repository, mutation)
                    indexer._handle_watch_changes(
                        root,
                        [
                            (kinds[kind], str(root / relative))
                            for kind, relative in sequence["changes"]
                        ],
                    )
                    applied = {
                        "dirty": indexer._store.is_dirty,
                        "entries": _entries(indexer._store.snapshot()),
                        "stats": {
                            "rebuilds": indexer.stats.rebuilds,
                            "incrementalUpdates": indexer.stats.incremental_updates,
                        },
                    }
                    queried = _entries(indexer.get_index(root))
                    after = {
                        "dirty": indexer._store.is_dirty,
                        "entries": queried,
                        "stats": {
                            "rebuilds": indexer.stats.rebuilds,
                            "incrementalUpdates": indexer.stats.incremental_updates,
                        },
                    }
                finally:
                    indexer.shutdown()
        records.append(
            {
                "case": sequence["case"],
                "mutations": sequence["mutations"],
                "changes": sequence["changes"],
                "applied": applied,
                "queried": after,
            }
        )
    return {"cases": records}


# --------------------------------------------------------------------------
# The completer, its controllers and the prompt payload
# --------------------------------------------------------------------------


class _Chdir:
    def __init__(self, target: Path) -> None:
        self._target = target

    def __enter__(self) -> None:
        self._saved = Path.cwd()
        os.chdir(self._target)

    def __exit__(self, *exc: Any) -> None:
        os.chdir(self._saved)


def materialize_collect(enclosure: Path, fixture_id: str) -> Path:
    """Writes a fixture at `top/root` under `enclosure`, with the directories a
    `..` query lists around it."""

    root = enclosure / COLLECT_ROOT
    root.mkdir(parents=True)
    materialize(root, FIXTURES[fixture_id])
    for relative in COLLECT_OUTSIDE:
        target = enclosure / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(FILE_BODY, encoding="utf-8", newline="")
    return root


def capture_collect() -> dict[str, Any]:
    from vibe.cli.autocompletion.completers import PathCompleter

    records: list[dict[str, Any]] = []
    for fixture_id in dict.fromkeys(query[0] for query in COLLECT_QUERIES):
        with tempfile.TemporaryDirectory(prefix="autocompletion-collect-") as scratch_dir:
            root = materialize_collect(Path(scratch_dir), fixture_id)
            with _Chdir(root):
                completer = PathCompleter()
                try:
                    for query_fixture, text, cursor in COLLECT_QUERIES:
                        if query_fixture != fixture_id:
                            continue
                        position = len(text) if cursor is None else cursor
                        labels = [
                            entry.label
                            for entry in completer.get_completion_items(text, position)
                        ]
                        replacement = completer.get_replacement_range(text, position)
                        records.append(
                            {
                                "case": f"{fixture_id}/{text}@{position}",
                                "fixture": fixture_id,
                                "text": text,
                                "cursor": position,
                                "labels": labels,
                                "replacementRange": (
                                    list(replacement) if replacement else None
                                ),
                            }
                        )
                finally:
                    completer._indexer.shutdown()
    return {"root": COLLECT_ROOT, "outside": COLLECT_OUTSIDE, "cases": records}


class _RecordingView:
    """The completion view the controllers draw into, reduced to a log."""

    def __init__(self) -> None:
        self.suggestions: list[str] | None = None
        self.selected: int | None = None
        self.ghost: str | None = None
        self.replaced: list[Any] | None = None

    def render_completion_suggestions(self, suggestions: Any, selected_index: int) -> None:
        self.suggestions = [entry.label for entry in suggestions]
        self.selected = selected_index

    def clear_completion_suggestions(self) -> None:
        self.suggestions = None
        self.selected = None

    def show_inline_suggestion(self, suggestion: str) -> None:
        self.ghost = suggestion

    def clear_inline_suggestion(self) -> None:
        self.ghost = None

    def replace_completion_range(
        self, start: int, end: int, replacement: str, *, suppress_update: bool = False
    ) -> None:
        self.replaced = [start, end, replacement]


def _key(name: str) -> Any:
    from textual import events

    return events.Key(name, None)


def capture_controller() -> list[dict[str, Any]]:
    from concurrent.futures import Future

    from vibe.cli.autocompletion.completers import PathCompleter
    from vibe.cli.autocompletion.path_completion import PathCompletionController

    records: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory(prefix="autocompletion-controller-") as scratch_dir:
        root = materialize_collect(Path(scratch_dir), "wide")
        with _Chdir(root):
            completer = PathCompleter()
            try:
                view = _RecordingView()
                controller = PathCompletionController(completer, view)
                controller.on_text_changed("@entry", 6)
                records.append(
                    {
                        "case": "a-wide-answer-keeps-every-match",
                        "observed": {
                            "count": len(view.suggestions or []),
                            "selected": view.selected,
                        },
                    }
                )
                controller._move_selection(1)
                controller.on_text_changed("@entr", 5)
                records.append(
                    {
                        "case": "the-same-list-keeps-the-highlight",
                        "observed": {
                            "count": len(view.suggestions or []),
                            "selected": view.selected,
                        },
                    }
                )
                controller.on_text_changed("@entry-12", 9)
                records.append(
                    {
                        "case": "a-different-list-resets-the-highlight",
                        "observed": {
                            "count": len(view.suggestions or []),
                            "selected": view.selected,
                        },
                    }
                )
                for key in ("tab", "enter", "down", "escape"):
                    pending = PathCompletionController(completer, _RecordingView())
                    pending._pending_future = Future()
                    records.append(
                        {
                            "case": f"pending-{key}",
                            "observed": {
                                "result": str(
                                    pending.on_key(_key(key), "@entry", 6)
                                ),
                            },
                        }
                    )
                for key in ("tab", "enter"):
                    shown = PathCompletionController(completer, _RecordingView())
                    shown.on_text_changed("@entry-00", 9)
                    shown._pending_future = Future()
                    records.append(
                        {
                            "case": f"pending-{key}-over-shown-suggestions",
                            "observed": {
                                "result": str(
                                    shown.on_key(_key(key), "@entry-00", 9)
                                ),
                            },
                        }
                    )
            finally:
                completer._indexer.shutdown()
    return records


def capture_inline_skill() -> dict[str, Any]:
    from vibe.cli.autocompletion.inline_skill_completion import (
        InlineSkillCompletionController,
    )

    records: list[dict[str, Any]] = []
    for text, cursor, default_mode in INLINE_CASES:
        position = len(text) if cursor is None else cursor
        view = _RecordingView()
        controller = InlineSkillCompletionController(
            lambda: list(INLINE_SKILLS), view, lambda: default_mode
        )
        controller.on_text_changed(text, position)
        ghost = view.ghost
        result = str(controller.on_key(_key("tab"), text, position))
        records.append(
            {
                "case": f"{text}@{position}{'' if default_mode else '#mode'}",
                "text": text,
                "cursor": position,
                "defaultMode": default_mode,
                "ghost": ghost,
                "accept": {"result": result, "replaced": view.replaced},
            }
        )
    # A ghost computed for one prompt and accepted against another, the way a
    # caret move that never reached the manager leaves it.
    view = _RecordingView()
    controller = InlineSkillCompletionController(
        lambda: list(INLINE_SKILLS), view, lambda: True
    )
    controller.on_text_changed("fix /imp", 8)
    stale = str(controller.on_key(_key("tab"), "fix /imx", 8))
    records.append(
        {
            "case": "stale-ghost",
            "text": "fix /imx",
            "cursor": 8,
            "defaultMode": True,
            "ghost": None,
            "accept": {"result": stale, "replaced": view.replaced},
            "shownFor": "fix /imp",
        }
    )
    return {"skills": [list(skill) for skill in INLINE_SKILLS], "cases": records}


def capture_path_prompt() -> dict[str, Any]:
    from vibe.core.autocompletion.path_prompt import build_path_prompt_payload

    records: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory(prefix="autocompletion-prompt-") as scratch_dir:
        root = Path(scratch_dir) / "root"
        root.mkdir()
        materialize(root, {"tree": PROMPT_TREE, "gitignore": None})
        resolved_root = root.resolve()

        def placed(path: Path) -> str:
            try:
                return path.relative_to(resolved_root).as_posix()
            except ValueError as error:
                raise OracleError(f"{path} resolved outside the prompt fixture") from error

        for template in PROMPT_MESSAGES:
            message = template.replace("{root}", resolved_root.as_posix())
            payload = build_path_prompt_payload(message, base_dir=root)
            stats_types: dict[str, int] = {}
            stats_extensions: dict[str, int] = {}
            for resource in payload.all_resources:
                stats_types[resource.kind] = stats_types.get(resource.kind, 0) + 1
                if resource.kind == "file":
                    suffix = resource.path.suffix
                    stats_extensions[suffix] = stats_extensions.get(suffix, 0) + 1
            records.append(
                {
                    "case": template or "<empty>",
                    "message": template,
                    "resources": [
                        {
                            "alias": resource.alias.replace(
                                resolved_root.as_posix(), "{root}"
                            ),
                            "kind": resource.kind,
                            "path": placed(resource.path),
                        }
                        for resource in payload.resources
                    ],
                    "allAliases": [
                        resource.alias.replace(resolved_root.as_posix(), "{root}")
                        for resource in payload.all_resources
                    ],
                    "mentions": {
                        "count": len(payload.all_resources),
                        "contextTypes": stats_types,
                        "fileExtensions": stats_extensions,
                    },
                }
            )
    return {"tree": PROMPT_TREE, "cases": records}


def capture_watch_filter() -> list[dict[str, Any]]:
    from watchfiles import Change, DefaultFilter

    accept = DefaultFilter()
    return [
        {
            "case": "/".join(segments),
            "segments": segments,
            "allowed": accept(Change.added, os.sep.join(["", "watched", *segments])),
        }
        for segments in WATCH_FILTER_PROBES
    ]


def capture_fuzzy() -> dict[str, Any]:
    from vibe.cli.autocompletion.fuzzy import fuzzy_match

    pairs = fuzzy_pairs()
    scores: list[Any] = []
    for pattern, text in pairs:
        try:
            result = fuzzy_match(pattern, text)
        except IndexError:
            scores.append("raises")
            continue
        scores.append(_centis(result.score) if result.matched else None)
    return {
        "seed": FUZZY_SEED,
        "count": FUZZY_COUNT,
        "alphabet": FUZZY_ALPHABET,
        "canary": [list(pair) for pair in pairs[:8]],
        "scores": scores,
    }


def capture_fixtures() -> list[dict[str, Any]]:
    return [
        {
            "id": fixture_id,
            "tree": fixture["tree"],
            "gitignore": fixture["gitignore"],
            "fileBody": FILE_BODY,
        }
        for fixture_id, fixture in FIXTURES.items()
    ]


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
            "also write the committed corpus, which the Rust replay reads "
            f"unconditionally (default {DEFAULT_CORPUS})"
        ),
    )
    parser.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def build_corpus(reference: dict[str, str], families: dict[str, Any]) -> dict[str, Any]:
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": {"commit": reference["commit"]},
        "note": (
            "Autocompletion corpus: what the pinned reference's ignore rules, index store and "
            "path ranking answer for each scripted fixture tree, change sequence and query. "
            "Every name here is supplied by the fixtures scripts/parity/autocompletion.py "
            "authors; no reference-authored text, no reference source and no machine path is "
            "recorded. Two normalizations apply on both sides of the replay: entry lists are "
            "sorted by relative path, because the reference holds a rebuilt index in scandir "
            "order and a mutated one in relative-path order, so its order is not a contract; "
            "and fuzzy scores are recorded in hundredths, the integer scale this port's "
            "matcher computes in. The git families run in a repository the script initializes "
            "with the system and global git configuration disabled; the collect and controller "
            "families run with the working directory at a fixture root, because the reference "
            "completes against the process working directory; the fuzzy family records only the "
            "seed, the alphabet and the scores of a sweep both sides draw from the same SplitMix64 "
            "stream. Regenerate with scripts/parity/autocompletion.py --corpus "
            "when the pinned reference moves."
        ),
        **families,
    }


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
        GUARD.install()
        with tempfile.TemporaryDirectory(prefix="autocompletion-oracle-") as scratch_dir:
            scratch = Path(scratch_dir)
            for fixture_id, root in _roots(scratch).items():
                root.mkdir()
                materialize(root, FIXTURES[fixture_id])
            constants = capture_constants()
            ignore_rules = capture_ignore_rules(scratch)
            walk = capture_walk(scratch)
            ranking = capture_ranking(scratch)
        changes = capture_changes()
        git_walk = capture_git_walk()
        git_changes = capture_git_changes()
        collect = capture_collect()
        controller = capture_controller()
        inline_skill = capture_inline_skill()
        path_prompt = capture_path_prompt()
        watch_filter = capture_watch_filter()
        fuzzy = capture_fuzzy()
    except OracleError as error:
        print(f"autocompletion capture failed: {error}", file=sys.stderr)
        return 1

    if GUARD.attempts:
        print(
            f"autocompletion capture failed: network attempts {GUARD.attempts}",
            file=sys.stderr,
        )
        return 1

    shortfalls = []
    if len(ignore_rules) < 40:
        shortfalls.append(f"ignoreRules holds {len(ignore_rules)} probes, under 40")
    if len(changes) < 12:
        shortfalls.append(f"changes holds {len(changes)} sequences, under 12")
    if len(ranking) < 30:
        shortfalls.append(f"ranking holds {len(ranking)} queries, under 30")
    if shortfalls:
        print(
            "autocompletion capture failed: " + "; ".join(shortfalls),
            file=sys.stderr,
        )
        return 1

    families = {
        "constants": constants,
        "fixtures": capture_fixtures(),
        "ignoreRules": ignore_rules,
        "walk": walk,
        "changes": changes,
        "ranking": ranking,
        "gitWalk": git_walk,
        "gitChanges": git_changes,
        "collect": collect,
        "controller": controller,
        "inlineSkill": inline_skill,
        "pathPrompt": path_prompt,
        "watchFilter": watch_filter,
        "fuzzy": fuzzy,
    }
    corpus = build_corpus(reference, families)

    full = {
        **corpus,
        "reference": reference,
        "platform": platform.system().lower(),
        "python": platform.python_version(),
    }
    _write(arguments.output, full)
    if arguments.corpus is not None:
        _write(arguments.corpus, corpus)

    counted = {
        "ignoreRules": len(ignore_rules),
        "walk": len(walk),
        "changes": sum(len(record["steps"]) for record in changes),
        "ranking": len(ranking),
        "gitWalk": len(git_walk["cases"]),
        "gitChanges": len(git_changes["cases"]),
        "collect": len(collect["cases"]),
        "controller": len(controller),
        "inlineSkill": len(inline_skill["cases"]),
        "pathPrompt": len(path_prompt["cases"]),
        "watchFilter": len(watch_filter),
        "fuzzy": len(fuzzy["scores"]),
    }
    total = sum(counted.values())
    print(
        f"captured {total} scenarios across {len(counted)} families plus the constants block "
        f"from {reference['commit'][:12]} into {arguments.output}"
    )
    for family, count in counted.items():
        print(f"  {family}: {count}")
    if arguments.corpus is not None:
        print(f"wrote the committed corpus to {arguments.corpus}")
    return 0


def _write(path: Path, document: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    staged = path.with_name(f"{path.name}.{os.getpid()}.tmp")
    staged.write_text(
        json.dumps(document, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
        encoding="utf-8",
        newline="",
    )
    os.replace(staged, path)


if __name__ == "__main__":
    sys.exit(main())
