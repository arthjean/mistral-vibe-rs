#!/usr/bin/env python3
"""Capture what the reference checkpoint engine answers over scripted scenarios.

The opcode oracle beside this one pins the diff a region identity is numbered
by. This one pins everything built on that identity: which regions each edit
produces, which earlier regions each was built on, what decision is in force
after dragging, what the file looks like with those decisions applied, where a
pending change sits in a rendered diff, and what a truncation would restore.

A scenario is a list of steps against one log, authored here. Each step is
applied to the reference ``Checkpointer``, and what the reference did with it is
recorded: ``ok``, or the family of the error it refused with. Once the steps are
done, the resulting log is interrogated through the reference ``History`` and
its anchors are cross-checked against the reference ``ReviewManager``, which is
the shell an editor client reaches them through. The replay in
``crates/vibe-core/src/checkpoints/checkpoint_parity_tests.rs`` drives the same
steps through this port and compares family by family.

What the corpus holds:

* the steps, verbatim, because they are this repository's own inputs and a
  replay cannot run without them;
* every observation the reference produced, structurally: region identities,
  ordinals, owners, decisions, dependency edges, line spans, anchor
  coordinates, error families and restore-plan orderings;
* every file content the reference produced, as a SHA-256 digest, so no
  reconstructed byte is stored and nothing carries reference-authored prose.

Usage::

    scripts/parity/checkpoints.py --reference /path/to/reference

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it. The capture reads the pinned commit
through ``git archive`` and never moves ``HEAD``.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import sys
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT

#: The pinned-tree mechanics are the tool-execution oracle's, reused rather than
#: copied: every capture must read the same commit the same way.
from tool_execution import (
    OracleError,
    extract_pinned_tree,
    reexecute_with_reference_interpreter,
    resolve_reference,
)

SCHEMA_VERSION = 1
DEFAULT_OUTPUT = Path("crates/vibe-core/tests/checkpoints/engine.json")
DEFAULT_CACHE = Path(".parity")

#: The floor the replay asserts, so a regeneration that captured almost nothing
#: fails instead of reporting a clean but empty run.
MINIMUM_SCENARIOS = 40

#: A turn identifier no scenario opens, used to pin what a restore plan answers
#: for a turn the log does not carry.
ABSENT_TURN = 9_999


# --------------------------------------------------------------------------
# The scenario language
# --------------------------------------------------------------------------


def begin(turn_id: int) -> dict[str, Any]:
    return {"op": "beginTurn", "turnId": turn_id}


def pre(path: str, text: str | None) -> dict[str, Any]:
    return {"op": "preEdit", "path": path, "text": text}


def post(path: str, text: str | None) -> dict[str, Any]:
    return {"op": "postEdit", "path": path, "text": text}


def seal() -> dict[str, Any]:
    return {"op": "sealTurn"}


def reconcile(path: str, text: str | None) -> dict[str, Any]:
    return {"op": "reconcile", "path": path, "text": text}


def decide_region(path: str, version: int, ordinal: int, decision: str) -> dict[str, Any]:
    return {
        "op": "decideRegion",
        "path": path,
        "versionIndex": version,
        "ordinal": ordinal,
        "decision": decision,
    }


def decide_scope(path: str, owner: dict[str, Any], decision: str) -> dict[str, Any]:
    return {"op": "decideScope", "path": path, "owner": owner, "decision": decision}


def decide_file(path: str, decision: str) -> dict[str, Any]:
    return {"op": "decideFile", "path": path, "decision": decision}


def drop_from(turn_id: int) -> dict[str, Any]:
    return {"op": "dropTurnsFrom", "turnId": turn_id}


def clear() -> dict[str, Any]:
    return {"op": "clear"}


def agent(turn_id: int) -> dict[str, Any]:
    return {"kind": "agent", "turnId": turn_id}


def manual(index: int) -> dict[str, Any]:
    return {"kind": "manual", "index": index}


def turn(turn_id: int, path: str, before: str | None, after: str | None) -> list[dict[str, Any]]:
    """One turn rewriting ``path``, which is what most scenarios are made of."""

    return [begin(turn_id), pre(path, before), post(path, after), seal()]


F = "f.txt"
G = "g.txt"

#: Five lines, the body most line-level scenarios edit.
BODY = "one\ntwo\nthree\nfour\nfive\n"
#: The same body with its first and last lines rewritten, which is two regions
#: framed by an equal run rather than one.
BODY_EDGES = "ONE\ntwo\nthree\nfour\nFIVE\n"
#: A byte a decoder refuses, which is what makes a change opaque.
BINARY = "head\x00tail\n"


def scenarios() -> list[dict[str, Any]]:
    """Every scenario, named for the behavior it pins."""

    return [
        # -- Attribution -----------------------------------------------------
        {
            "name": "one-turn-one-region",
            "covers": "attribution",
            "steps": turn(1, F, BODY, "one\nTWO\nthree\nfour\nfive\n"),
        },
        {
            "name": "one-turn-two-regions",
            "covers": "attribution",
            "steps": turn(1, F, BODY, BODY_EDGES),
        },
        {
            "name": "attribution-across-two-turns",
            "covers": "attribution",
            "steps": [
                *turn(1, F, BODY, "one\nTWO\nthree\nfour\nfive\n"),
                *turn(2, F, "one\nTWO\nthree\nfour\nfive\n", "one\nTWO\nthree\nFOUR\nfive\n"),
            ],
        },
        {
            "name": "attribution-across-three-turns-on-two-files",
            "covers": "attribution",
            "steps": [
                *turn(1, F, BODY, "one\nTWO\nthree\nfour\nfive\n"),
                *turn(2, G, "alpha\nbeta\n", "alpha\nBETA\n"),
                *turn(3, F, "one\nTWO\nthree\nfour\nfive\n", "one\nTWO\nthree\nfour\nFIVE\n"),
            ],
        },
        {
            "name": "a-turn-touching-two-files-at-once",
            "covers": "attribution",
            "steps": [
                begin(1),
                pre(F, BODY),
                pre(G, "alpha\n"),
                post(F, "one\nTWO\nthree\nfour\nfive\n"),
                post(G, "ALPHA\n"),
                seal(),
            ],
        },
        {
            "name": "a-turn-that-changed-nothing-appends-no-edit",
            "covers": "attribution",
            "steps": [begin(1), pre(F, BODY), post(F, BODY), seal()],
        },
        {
            "name": "a-path-recorded-but-never-written-back",
            "covers": "attribution",
            "steps": [begin(1), pre(F, BODY), seal()],
        },
        {
            "name": "the-first-touch-of-a-path-owns-the-turn",
            "covers": "attribution",
            "steps": [
                begin(1),
                pre(F, BODY),
                pre(F, "a later tool read this\n"),
                post(F, BODY_EDGES),
                seal(),
            ],
        },
        {
            "name": "a-file-created-by-a-turn",
            "covers": "attribution",
            "steps": turn(1, F, None, "created\n"),
        },
        {
            "name": "an-insertion-at-the-top-and-at-the-bottom",
            "covers": "attribution",
            "steps": turn(1, F, BODY, "zero\n" + BODY + "six\n"),
        },
        # -- Per-turn and per-region decisions -------------------------------
        {
            "name": "per-turn-revert",
            "covers": "per-turn-revert",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                decide_scope(F, agent(1), "revert"),
            ],
        },
        {
            "name": "per-turn-keep",
            "covers": "per-turn-revert",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                decide_scope(F, agent(1), "keep"),
            ],
        },
        {
            "name": "per-region-revert-keeps-the-sibling",
            "covers": "per-region-revert",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                decide_region(F, 2, 0, "revert"),
            ],
        },
        {
            "name": "per-region-keep-keeps-the-sibling-pending",
            "covers": "per-region-revert",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                decide_region(F, 2, 0, "keep"),
            ],
        },
        {
            "name": "incremental-revert-back-to-the-original",
            "covers": "incremental-revert",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                decide_region(F, 2, 0, "revert"),
                decide_region(F, 2, 1, "revert"),
            ],
        },
        {
            "name": "revert-the-whole-file-in-one-call",
            "covers": "incremental-revert",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                *turn(2, F, BODY_EDGES, "ONE\ntwo\nTHREE\nfour\nFIVE\n"),
                decide_file(F, "revert"),
            ],
        },
        {
            "name": "keep-the-whole-file-in-one-call",
            "covers": "bulk-decisions",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                *turn(2, F, BODY_EDGES, "ONE\ntwo\nTHREE\nfour\nFIVE\n"),
                decide_file(F, "keep"),
            ],
        },
        {
            "name": "a-bulk-keep-skips-a-region-already-reverted",
            "covers": "bulk-decisions",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                decide_region(F, 2, 0, "revert"),
                decide_file(F, "keep"),
            ],
        },
        {
            "name": "a-bulk-revert-skips-a-region-already-reverted",
            "covers": "bulk-decisions",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                decide_region(F, 2, 0, "revert"),
                decide_file(F, "revert"),
            ],
        },
        {
            "name": "a-scope-decision-only-touches-its-own-regions",
            "covers": "bulk-decisions",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                *turn(2, F, BODY_EDGES, "ONE\ntwo\nTHREE\nfour\nFIVE\n"),
                decide_scope(F, agent(2), "keep"),
            ],
        },
        # -- The ratchet ------------------------------------------------------
        {
            "name": "a-keep-after-a-revert-is-ignored",
            "covers": "ratchet",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                decide_region(F, 2, 0, "revert"),
                decide_region(F, 2, 0, "keep"),
            ],
        },
        {
            "name": "a-revert-after-a-keep-still-reverts",
            "covers": "ratchet",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                decide_region(F, 2, 0, "keep"),
                decide_region(F, 2, 0, "revert"),
            ],
        },
        {
            "name": "a-bulk-keep-after-a-file-revert-changes-nothing",
            "covers": "ratchet",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                decide_file(F, "revert"),
                decide_file(F, "keep"),
            ],
        },
        # -- Dependencies -----------------------------------------------------
        {
            "name": "a-later-edit-on-the-same-lines-depends-on-the-earlier-one",
            "covers": "dependency-cascade",
            "steps": [
                *turn(1, F, BODY, "one\nTWO\nthree\nfour\nfive\n"),
                *turn(2, F, "one\nTWO\nthree\nfour\nfive\n", "one\nTWO AGAIN\nthree\nfour\nfive\n"),
            ],
        },
        {
            "name": "reverting-a-dependency-drags-its-dependent",
            "covers": "dependency-cascade",
            "steps": [
                *turn(1, F, BODY, "one\nTWO\nthree\nfour\nfive\n"),
                *turn(2, F, "one\nTWO\nthree\nfour\nfive\n", "one\nTWO AGAIN\nthree\nfour\nfive\n"),
                decide_region(F, 2, 0, "revert"),
            ],
        },
        {
            "name": "keeping-a-dependent-pulls-in-what-it-was-built-on",
            "covers": "dependency-cascade",
            "steps": [
                *turn(1, F, BODY, "one\nTWO\nthree\nfour\nfive\n"),
                *turn(2, F, "one\nTWO\nthree\nfour\nfive\n", "one\nTWO AGAIN\nthree\nfour\nfive\n"),
                decide_region(F, 4, 0, "keep"),
            ],
        },
        {
            "name": "a-three-deep-dependency-chain-drags-to-the-end",
            "covers": "dependency-cascade",
            "steps": [
                *turn(1, F, BODY, "one\nA\nthree\nfour\nfive\n"),
                *turn(2, F, "one\nA\nthree\nfour\nfive\n", "one\nB\nthree\nfour\nfive\n"),
                *turn(3, F, "one\nB\nthree\nfour\nfive\n", "one\nC\nthree\nfour\nfive\n"),
                decide_region(F, 2, 0, "revert"),
            ],
        },
        {
            "name": "an-insertion-depends-on-the-line-above-it",
            "covers": "dependency-cascade",
            "steps": [
                *turn(1, F, BODY, "one\nTWO\nthree\nfour\nfive\n"),
                *turn(
                    2,
                    F,
                    "one\nTWO\nthree\nfour\nfive\n",
                    "one\nTWO\ninserted\nthree\nfour\nfive\n",
                ),
            ],
        },
        {
            "name": "an-insertion-at-the-top-depends-on-the-line-below-it",
            "covers": "dependency-cascade",
            "steps": [
                *turn(1, F, BODY, "ONE\ntwo\nthree\nfour\nfive\n"),
                *turn(
                    2,
                    F,
                    "ONE\ntwo\nthree\nfour\nfive\n",
                    "zero\nONE\ntwo\nthree\nfour\nfive\n",
                ),
            ],
        },
        {
            "name": "an-insertion-into-an-untouched-file-depends-on-nothing",
            "covers": "dependency-cascade",
            "steps": turn(1, F, BODY, "one\ninserted\ntwo\nthree\nfour\nfive\n"),
        },
        {
            "name": "a-region-replacing-lines-two-earlier-regions-wrote",
            "covers": "dependency-cascade",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                *turn(2, F, BODY_EDGES, "swallowed\n"),
            ],
        },
        {
            "name": "two-independent-regions-decide-independently",
            "covers": "dependency-cascade",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                *turn(2, F, BODY_EDGES, "ONE\ntwo\nTHREE\nfour\nFIVE\n"),
                decide_region(F, 4, 0, "revert"),
            ],
        },
        # -- Opaque changes ---------------------------------------------------
        {
            "name": "a-binary-rewrite-is-one-whole-file-unit",
            "covers": "opaque",
            "steps": turn(1, F, BODY, BINARY),
        },
        {
            "name": "a-binary-file-rewritten-into-text",
            "covers": "opaque",
            "steps": turn(1, F, BINARY, BODY),
        },
        {
            "name": "a-deletion-is-one-whole-file-unit",
            "covers": "opaque",
            "steps": turn(1, F, BODY, None),
        },
        {
            "name": "reverting-a-deletion-restores-the-bytes",
            "covers": "opaque",
            "steps": [*turn(1, F, BODY, None), decide_region(F, 2, 0, "revert")],
        },
        {
            "name": "an-empty-file-created-is-an-existence-toggle",
            "covers": "opaque",
            "steps": turn(1, F, None, ""),
        },
        {
            "name": "an-empty-file-removed-is-an-existence-toggle",
            "covers": "opaque",
            "steps": turn(1, F, "", None),
        },
        {
            "name": "a-text-edit-on-top-of-a-barrier-depends-on-it",
            "covers": "opaque",
            "steps": [
                *turn(1, F, None, "created\n"),
                *turn(2, F, "created\n", "created\nsecond\n"),
            ],
        },
        {
            "name": "reverting-a-barrier-drags-the-text-edit-above-it",
            "covers": "opaque",
            "steps": [
                *turn(1, F, None, "created\n"),
                *turn(2, F, "created\n", "created\nsecond\n"),
                decide_region(F, 2, 0, "revert"),
            ],
        },
        {
            "name": "an-opaque-edit-depends-on-everything-applied",
            "covers": "opaque",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                *turn(2, F, BODY_EDGES, BINARY),
            ],
        },
        # -- The turn gate ----------------------------------------------------
        {
            "name": "beginning-a-turn-while-one-is-open-is-refused",
            "covers": "turn-gate",
            "steps": [begin(1), pre(F, BODY), begin(2), post(F, BODY_EDGES), seal()],
        },
        {
            "name": "recording-outside-a-turn-is-refused",
            "covers": "turn-gate",
            "steps": [pre(F, BODY), post(F, BODY_EDGES)],
        },
        {
            "name": "deciding-while-a-turn-is-open-is-refused",
            "covers": "turn-gate",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                begin(2),
                decide_region(F, 2, 0, "revert"),
                decide_file(F, "keep"),
                decide_scope(F, agent(1), "keep"),
                seal(),
            ],
        },
        {
            "name": "deciding-a-region-the-file-does-not-carry-is-refused",
            "covers": "turn-gate",
            "steps": [*turn(1, F, BODY, BODY_EDGES), decide_region(F, 7, 3, "revert")],
        },
        {
            "name": "a-pending-decision-is-refused",
            "covers": "turn-gate",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                decide_region(F, 2, 0, "pending"),
                decide_file(F, "pending"),
            ],
        },
        {
            "name": "sealing-with-no-turn-open-does-nothing",
            "covers": "turn-gate",
            "steps": [*turn(1, F, BODY, BODY_EDGES), seal(), seal()],
        },
        # -- Manual edits -----------------------------------------------------
        {
            "name": "a-hand-edit-between-turns-takes-its-own-slot",
            "covers": "manual-edit",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                reconcile(F, BODY_EDGES + "by hand\n"),
            ],
        },
        {
            "name": "reconciling-twice-records-the-drift-once",
            "covers": "manual-edit",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                reconcile(F, BODY_EDGES + "by hand\n"),
                reconcile(F, BODY_EDGES + "by hand\n"),
            ],
        },
        {
            "name": "reconciling-during-a-turn-records-nothing",
            "covers": "manual-edit",
            "steps": [
                begin(1),
                pre(F, BODY),
                reconcile(F, "written mid turn\n"),
                post(F, BODY_EDGES),
                seal(),
            ],
        },
        {
            "name": "a-drift-seen-at-the-start-of-a-turn-is-sealed-before-its-mark",
            "covers": "manual-edit",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                begin(2),
                pre(F, BODY_EDGES + "by hand\n"),
                post(F, BODY_EDGES + "by hand\nby the agent\n"),
                seal(),
            ],
        },
        {
            "name": "a-hand-edit-disjoint-from-a-reverted-turn-survives",
            "covers": "manual-edit-dependency",
            "steps": [
                *turn(1, F, BODY, "one\nTWO\nthree\nfour\nfive\n"),
                reconcile(F, "one\nTWO\nthree\nfour\nfive\nby hand\n"),
                decide_region(F, 2, 0, "revert"),
            ],
        },
        {
            "name": "a-hand-edit-built-on-a-reverted-turn-is-dragged",
            "covers": "manual-edit-dependency",
            "steps": [
                *turn(1, F, BODY, "one\nTWO\nthree\nfour\nfive\n"),
                reconcile(F, "one\nTWO AND HAND\nthree\nfour\nfive\n"),
                decide_region(F, 2, 0, "revert"),
            ],
        },
        {
            "name": "a-hand-edit-keeps-its-slot-when-a-sibling-is-decided",
            "covers": "manual-edit",
            "steps": [
                *turn(1, F, BODY, "one\nTWO\nthree\nfour\nfive\n"),
                reconcile(F, "one\nTWO\nthree\nfour\nfive\nfirst hand\n"),
                *turn(
                    2,
                    F,
                    "one\nTWO\nthree\nfour\nfive\nfirst hand\n",
                    "one\nTWO\nTHREE\nfour\nfive\nfirst hand\n",
                ),
                decide_scope(F, manual(1), "keep"),
                reconcile(F, "one\nTWO\nTHREE\nfour\nfive\nfirst hand\nsecond hand\n"),
            ],
        },
        {
            "name": "a-hand-edit-deleting-the-file",
            "covers": "manual-edit",
            "steps": [*turn(1, F, BODY, BODY_EDGES), reconcile(F, None)],
        },
        # -- Scope diffs ------------------------------------------------------
        {
            "name": "a-scope-diff-of-a-turn-with-two-pending-regions",
            "covers": "scope-diff",
            "steps": turn(1, F, BODY, BODY_EDGES),
        },
        {
            "name": "a-scope-diff-with-one-region-kept-and-one-pending",
            "covers": "scope-diff",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                decide_region(F, 2, 0, "keep"),
            ],
        },
        {
            "name": "a-scope-diff-of-a-fully-decided-turn-is-empty",
            "covers": "scope-diff",
            "steps": [*turn(1, F, BODY, BODY_EDGES), decide_file(F, "keep")],
        },
        {
            "name": "a-scope-diff-of-the-second-of-two-turns",
            "covers": "scope-diff",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                *turn(2, F, BODY_EDGES, "ONE\ntwo\nTHREE\nfour\nFIVE\n"),
                decide_scope(F, agent(1), "keep"),
            ],
        },
        {
            "name": "two-identical-deletions-are-claimed-by-distinct-anchors",
            "covers": "scope-diff",
            "steps": turn(
                1,
                F,
                "same\nkeep me\nsame\ntail\n",
                "keep me\ntail\n",
            ),
        },
        # -- Truncation -------------------------------------------------------
        {
            "name": "truncating-at-a-turn-drops-it-and-everything-after",
            "covers": "truncation",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                *turn(2, F, BODY_EDGES, "ONE\ntwo\nTHREE\nfour\nFIVE\n"),
                drop_from(2),
            ],
        },
        {
            "name": "truncating-drops-the-decisions-taken-after-the-cut",
            "covers": "truncation",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                *turn(2, F, BODY_EDGES, "ONE\ntwo\nTHREE\nfour\nFIVE\n"),
                decide_region(F, 2, 0, "keep"),
                drop_from(2),
            ],
        },
        {
            "name": "truncating-at-an-absent-turn-uses-the-earliest-later-one",
            "covers": "truncation",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                *turn(4, F, BODY_EDGES, "ONE\ntwo\nTHREE\nfour\nFIVE\n"),
                drop_from(3),
            ],
        },
        {
            "name": "truncating-past-every-turn-leaves-the-log-alone",
            "covers": "truncation",
            "steps": [*turn(1, F, BODY, BODY_EDGES), drop_from(9)],
        },
        {
            "name": "truncating-keeps-a-hand-edit-sealed-before-the-cut",
            "covers": "truncation",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                begin(2),
                pre(F, BODY_EDGES + "by hand\n"),
                post(F, BODY_EDGES + "by hand\nby the agent\n"),
                seal(),
                drop_from(2),
            ],
        },
        {
            "name": "truncating-closes-an-open-turn",
            "covers": "truncation",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                begin(2),
                pre(F, BODY_EDGES),
                drop_from(2),
                begin(2),
                pre(F, BODY_EDGES),
                post(F, "ONE\ntwo\nTHREE\nfour\nFIVE\n"),
                seal(),
            ],
        },
        {
            "name": "a-reused-turn-identifier-resolves-to-the-newest-mark",
            "covers": "truncation",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                *turn(1, F, BODY_EDGES, "ONE\ntwo\nTHREE\nfour\nFIVE\n"),
            ],
        },
        {
            "name": "a-restore-plan-over-two-files-and-three-turns",
            "covers": "truncation",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                *turn(2, G, None, "made by turn two\n"),
                *turn(3, F, BODY_EDGES, "ONE\ntwo\nTHREE\nfour\nFIVE\n"),
            ],
        },
        {
            "name": "a-file-only-a-hand-edit-touched-restores-to-the-kept-projection",
            "covers": "truncation",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                *turn(2, G, None, "made by turn two\n"),
                reconcile(F, BODY_EDGES + "by hand\n"),
            ],
        },
        {
            "name": "clearing-the-log-empties-everything-counted-against-it",
            "covers": "truncation",
            "steps": [
                *turn(1, F, BODY, BODY_EDGES),
                clear(),
                *turn(1, F, BODY, "one\nTWO\nthree\nfour\nfive\n"),
            ],
        },
        # -- Encoding ---------------------------------------------------------
        {
            "name": "a-crlf-file-partially-reverted-keeps-its-line-ending",
            "covers": "encoding",
            "steps": [
                *turn(1, F, "one\r\ntwo\r\nthree\r\n", "ONE\r\ntwo\r\nTHREE\r\n"),
                decide_region(F, 2, 0, "revert"),
            ],
        },
        {
            "name": "a-file-carrying-every-boundary-character",
            "covers": "encoding",
            "steps": turn(
                1,
                F,
                "a\vb\fc\x1cd\x1de\x1ef\x85g h i\n",
                "a\vB\fc\x1cd\x1de\x1ef\x85g h i\n",
            ),
        },
        {
            "name": "a-file-above-the-heuristic-threshold",
            "covers": "encoding",
            "steps": turn(
                1,
                F,
                "".join(f"line {index}\n" for index in range(220)),
                "".join(
                    f"line {index}\n" if index != 100 else "changed\n" for index in range(220)
                ),
            ),
        },
    ]


# --------------------------------------------------------------------------
# Driving the reference
# --------------------------------------------------------------------------


def digest_bytes(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()[:32]


def named(value: Any) -> str:
    """The wire spelling of a value the reference models as a name.

    A decision and an opaque reason are enumerations there while a hunk side is
    a literal string, and all three cross the wire as the same lowercase name.
    """

    return str(getattr(value, "value", value))


def state_digest(state: Any) -> str:
    """A file state's identity, with absence answering apart from emptiness."""

    if state.data is None:
        return "absent"
    return digest_bytes(state.data)


def lines_digest(lines: tuple[str, ...]) -> str:
    """A line sequence's identity, boundaries included.

    Length-prefixed for the reason the opcode corpus digests the same way:
    joining keepends lines rebuilds the input, so two different splits of one
    text would otherwise digest alike.
    """

    body = "".join(f"{len(line)}:{line}" for line in lines)
    return digest_bytes(body.encode("utf-8"))


def _state(text: str | None) -> Any:
    from vibe.core.checkpoints.models import FileState

    return FileState.absent() if text is None else FileState.from_text(text)


def _owner(spec: dict[str, Any]) -> Any:
    from vibe.core.checkpoints.models import AgentTurn, ManualEdit

    if spec["kind"] == "agent":
        return AgentTurn(spec["turnId"])
    return ManualEdit(spec["index"])


def _owner_json(owner: Any) -> dict[str, Any]:
    from vibe.core.checkpoints.models import AgentTurn

    if isinstance(owner, AgentTurn):
        return {"kind": "agent", "turnId": owner.turn_id}
    return {"kind": "manual", "index": owner.index}


def _decision(name: str) -> Any:
    from vibe.core.checkpoints.models import Decision

    return {
        "pending": Decision.PENDING,
        "keep": Decision.KEEP,
        "revert": Decision.REVERT,
    }[name]


def apply_step(log: Any, step: dict[str, Any]) -> str:
    """One step against the log, answering what the reference did with it."""

    from vibe.core.checkpoints.models import FileStateError, RegionId, TurnStateError

    operation = step["op"]
    try:
        if operation == "beginTurn":
            log.begin_turn(step["turnId"])
        elif operation == "preEdit":
            log.record_pre_edit(step["path"], _state(step["text"]))
        elif operation == "postEdit":
            log.record_post_edit(step["path"], _state(step["text"]))
        elif operation == "sealTurn":
            log.seal_turn()
        elif operation == "reconcile":
            log.reconcile(step["path"], _state(step["text"]))
        elif operation == "decideRegion":
            log.decide_region(
                step["path"],
                RegionId(step["versionIndex"], step["ordinal"]),
                _decision(step["decision"]),
            )
        elif operation == "decideScope":
            log.decide_scope(
                step["path"], _owner(step["owner"]), _decision(step["decision"])
            )
        elif operation == "decideFile":
            log.decide_file(step["path"], _decision(step["decision"]))
        elif operation == "dropTurnsFrom":
            log.drop_turns_from(step["turnId"])
        elif operation == "clear":
            log.clear()
        else:
            raise OracleError(f"unknown step {operation}")
    except TurnStateError:
        return "turnState"
    except FileStateError:
        return "fileState"
    return "ok"


def observe_region(region: Any) -> dict[str, Any]:
    from vibe.core.checkpoints.models import OpaqueChange

    change = region.change
    if isinstance(change, OpaqueChange):
        observed: dict[str, Any] = {
            "kind": "opaque",
            "reason": named(change.reason),
            "baseline": state_digest(change.baseline),
            "current": state_digest(change.current),
        }
    else:
        observed = {
            "kind": "text",
            "baselineStart": change.baseline_start,
            "baselineLineCount": len(change.baseline_lines),
            "baselineLines": lines_digest(change.baseline_lines),
            "currentStart": change.current_start,
            "currentLineCount": len(change.current_lines),
            "currentLines": lines_digest(change.current_lines),
        }
    return {
        "versionIndex": region.region_id.version_index,
        "ordinal": region.region_id.ordinal,
        "owner": _owner_json(region.owner),
        "decision": named(region.decision),
        "dependsOn": [[dep.version_index, dep.ordinal] for dep in region.depends_on],
        "change": observed,
    }


def observe_anchors(anchors: Any) -> list[dict[str, Any]]:
    return [
        {
            "side": named(anchor.side),
            "line": anchor.line,
            "regions": [[region.version_index, region.ordinal] for region in anchor.regions],
        }
        for anchor in anchors
    ]


def observe_file(history: Any, path: str) -> dict[str, Any]:
    regions = history.regions(path)
    owners: list[Any] = []
    for region in regions:
        if region.owner not in owners:
            owners.append(region.owner)
    scopes = []
    for owner in owners:
        pair = history.scope_pending_diff(path, owner)
        scopes.append(
            {
                "owner": _owner_json(owner),
                "diff": None
                if pair is None
                else {"baseline": state_digest(pair[0]), "current": state_digest(pair[1])},
                "anchors": observe_anchors(history.pending_hunks(path, owner)),
            }
        )
    return {
        "path": path,
        "regions": [observe_region(region) for region in regions],
        "content": state_digest(history.project(path, only_kept=False)),
        "acceptedBaseline": state_digest(history.project(path, only_kept=True)),
        "original": state_digest(history.original(path)),
        "fullyReviewed": history.is_fully_reviewed(path),
        "anchors": observe_anchors(history.pending_hunks(path)),
        "scopes": scopes,
    }


def observe_restore_plans(history: Any, turn_ids: list[int]) -> list[dict[str, Any]]:
    return [
        {
            "turnId": turn_id,
            "hasTurn": history.has_turn(turn_id),
            "plan": [
                {"path": path, "state": state_digest(state)}
                for path, state in history.restore_plan_to_turn(turn_id).items()
            ],
        }
        for turn_id in turn_ids
    ]


class _FakeFilesystem:
    """The disk the reference review shell reads, held in memory.

    It is seeded from the projection before each capture, so driving the shell
    observes the log rather than changing it: reconciliation against content
    equal to the projection appends nothing.
    """

    def __init__(self, content: dict[str, bytes | None]) -> None:
        self._content = content

    def read_bytes(self, path: str) -> bytes | None:
        return self._content.get(path)

    def write_bytes(self, path: str, data: bytes) -> None:
        self._content[path] = data

    def remove(self, path: str) -> None:
        self._content.pop(path, None)

    def exists(self, path: str) -> bool:
        return self._content.get(path) is not None


def cross_check_review_shell(name: str, log: Any, history: Any) -> None:
    """The anchors the editor-facing shell answers, against the read model's.

    ``ReviewManager`` is what an editor client reaches these through, and it
    reconciles disk before answering. Seeded with the projection it has nothing
    to reconcile, so it must answer exactly what the read model does; if it ever
    does not, the corpus below would be pinning something no client can see.
    """

    from vibe.core.checkpoints.file_store import FileStore
    from vibe.core.review.manager import ReviewManager

    if log.has_open_turn:
        return
    disk = {
        path: history.project(path, only_kept=False).data
        for path in history.tracked_paths
    }
    manager = ReviewManager(log, FileStore(_FakeFilesystem(disk)))
    for path in history.tracked_paths:
        through_shell = [
            {
                "side": named(hunk.side),
                "line": hunk.line,
                "regions": [list(region) for region in hunk.regions],
            }
            for hunk in manager.file_hunks(path)
        ]
        if through_shell != observe_anchors(history.pending_hunks(path)):
            raise OracleError(
                f"{name}: the review shell and the read model disagree about {path}"
            )


def capture_scenario(scenario: dict[str, Any]) -> dict[str, Any]:
    from vibe.core.checkpoints.checkpointer import Checkpointer

    log = Checkpointer()
    outcomes = [apply_step(log, step) for step in scenario["steps"]]
    history = log.view()
    turn_ids = sorted(
        {step["turnId"] for step in scenario["steps"] if step["op"] == "beginTurn"}
    ) + [ABSENT_TURN]
    cross_check_review_shell(scenario["name"], log, history)
    return {
        "name": scenario["name"],
        "covers": scenario["covers"],
        "steps": scenario["steps"],
        "outcomes": outcomes,
        "hasOpenTurn": log.has_open_turn,
        "trackedPaths": list(history.tracked_paths),
        "lastTurnPaths": list(history.last_turn_paths()),
        "scopes": [_owner_json(owner) for owner in history.scopes],
        "files": [observe_file(history, path) for path in history.tracked_paths],
        "restorePlans": observe_restore_plans(history, turn_ids),
    }


def build_corpus(commit: str) -> dict[str, Any]:
    captured = [capture_scenario(scenario) for scenario in scenarios()]
    if len(captured) < MINIMUM_SCENARIOS:
        raise OracleError(
            f"{len(captured)} scenarios, below the floor of {MINIMUM_SCENARIOS}"
        )
    names = [scenario["name"] for scenario in captured]
    if len(names) != len(set(names)):
        raise OracleError("two scenarios share a name, so a divergence could not name one")
    return {
        "schemaVersion": SCHEMA_VERSION,
        "referenceCommit": commit,
        "note": (
            "Checkpoint engine corpus: what the pinned reference's Checkpointer, History and "
            "ReviewManager answered for scenarios authored in scripts/parity/checkpoints.py. The "
            "steps are this repository's own inputs and are committed as they stand, because a "
            "replay cannot run without them; every observation is structural, and every file "
            "content the reference produced is a SHA-256 digest. No reference-authored text is "
            "stored. Regenerate with scripts/parity/checkpoints.py when the pinned reference "
            "moves."
        ),
        "absentTurn": ABSENT_TURN,
        "scenarios": captured,
    }


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--python", type=Path, default=None)
    parser.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        reference = resolve_reference(arguments.reference, arguments.expected_commit)
        pinned = extract_pinned_tree(
            arguments.reference, reference["commit"], arguments.cache
        )
        reexecute_with_reference_interpreter(arguments.reference, arguments.python, pinned)
        corpus = build_corpus(reference["commit"])
    except OracleError as error:
        print(f"checkpoint-engine capture failed: {error}", file=sys.stderr)
        return 1
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    staged = arguments.output.with_name(f"{arguments.output.name}.{os.getpid()}.tmp")
    staged.write_text(
        json.dumps(corpus, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    os.replace(staged, arguments.output)
    print(
        f"captured {len(corpus['scenarios'])} engine scenarios from "
        f"{reference['commit'][:12]} into {arguments.output}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
