"""The scenarios ``checkpoints.py`` replays through the pinned reference's
checkpoint engine, and the step language they are written in.

A step is one call on a ``Checkpointer`` log: a turn's edits, a reconcile, a
decision, a truncation. Every path, text and decision here is authored for the
corpus.
"""

from __future__ import annotations

from typing import Any


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
