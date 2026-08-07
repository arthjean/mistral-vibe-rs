[PRD]
# PRD: Checkpoints Parity

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-07 | Arthur Jean | Initial PRD from the measured checkpoints audit against the Python reference at commit `68ff32e`: the 1 344-line `vibe/core/checkpoints/` module has no counterpart beyond whole-file byte snapshots, the six `review/*` methods answer from a stub whose only writer is a test, and the two `session/rewind*` methods address turns by `messageIndex` where the reference addresses them by `entryId` |

## Problem Statement

1. **The review surface answers from a stub that production never writes to.** The six `review/*` methods route to `ResourceService::dispatch` (`crates/vibe-app-server/src/resources.rs:317`), which reads a private `BTreeMap<String, ReviewFile { baseline, current, approved }>` (`resources.rs:289`). That map's only writer, `record_review_change` (`resources.rs:456`), is called from exactly one place in the repository: a unit test at `resources.rs:1453`. In production `review/state` therefore returns `{"files": [], "scopes": []}` for every session, and `review/baseline`, `review/hunks` and `review/turnDiff` answer `NotFound` for every path. An editor integration that renders agent changes for approval renders nothing.

2. **The checkpoint engine that does exist is disconnected from that surface.** `ReviewManager` (`crates/vibe-core/src/workspace/review.rs`, 524 lines) captures per-turn whole-file byte baselines and is driven for real: `begin_turn_at` at `crates/vibe-app-server/src/server.rs:3277`, `seal_turn` at `server.rs:3465`, and the rewind path through `restorable_paths_at`, `fork_at` and `stage_restore_to_message` (`crates/vibe-app-server/src/server/session_management.rs:300,379,410`). But `ReviewManager::view()`, `approve()` and `revert()` have no caller outside `#[cfg(test)]`. The engine serves rewind only; the review methods never consult it.

3. **There is no region model, so the wire vocabulary is 25 models wide and the port emits 3 of them.** `crates/vibe-app-server/tests/app-server-surface/corpus.json` records 25 `Review*` models carrying 82 fields. What the stub emits is one synthetic region per file, hard-coded to `versionIndex: 0`, `ordinal: 0`, `owner: {"kind": "agent", "turnId": 0}`, `decision: "pending"`, `dependsOn: []` (`resources.rs:756-772`), with `scopes` always `[]` (`resources.rs:323`). Never emitted, at all: `OpaqueReviewRegion` and its `reason`, `ReviewManualOwner`, `ReviewScope`, `ReviewScopeFile`, a `decision` other than `pending`, a non-empty `dependsOn`. The reference publishes every one of them from `vibe/app_server/_review.py:160-197`.

4. **The seven review targets collapse to two behaviors.** `review_mutate` (`resources.rs:645-669`) branches on `target.kind` into "all files" and "one file". `ReviewLastTurnsTarget.count`, `ReviewScopeTarget.owner`, `ReviewRegionTarget.versionIndex`/`ordinal` and `ReviewRegionsTarget.regions` are parsed and discarded. `review/turnDiff` ignores its required `owner` parameter entirely (`resources.rs:720`), where the reference makes it the whole point of the method: `scope_file_diff(path, owner)` returns that owner's own change to the file, kept hunks against kept plus pending ([vibe/core/review/manager.py:269](/home/arthur/dev/mistral-vibe/vibe/core/review/manager.py)).

5. **There is no diff engine to build regions on.** `unified_diff` (`crates/vibe-core/src/workspace.rs:1869`) computes a common prefix and a common suffix and emits a single hunk. `Cargo.lock` contains no diff crate at all. The reference computes every region, every stable identifier and every anchor from `difflib.SequenceMatcher(autojunk=False).get_opcodes()`, called at three sites ([vibe/core/checkpoints/history.py:53,67,675](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/history.py)). `RegionId(version_index, ordinal)` is that opcode sequence's index, and it is written into `dependsOn`, into `ReviewRegionTarget` and into every decision the client sends back, so a different opcode sequence is a different contract.

6. **Manual edits are invisible.** The reference treats a change the user made by hand as a first-class owner with its own permanent review slot: `ManualEdit(index)` ([models.py:70](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/models.py)), captured by `reconcile` at every acting boundary ([checkpointer.py:177](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/checkpointer.py)) and sealed just before the turn marker when the drift preceded the turn ([checkpointer.py:100](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/checkpointer.py)). The port has no equivalent. A user who edits a file between two turns has that edit silently absorbed into the next turn's baseline, and reverting that turn destroys their work.

7. **Rewind addresses turns by the wrong key and answers in the wrong shape.** `SessionRewindParams` declares `entryId`; `release3.rs:1265` reads `messageIndex` and falls back to `keepMessages`. `SessionRewindReadResponse` declares exactly `{hasFileChanges, paths}`, which is what `RewindManager.restorable_paths_at` produces ([vibe/core/rewind/manager.py:39](/home/arthur/dev/mistral-vibe/vibe/core/rewind/manager.py)); `rewind_read` (`release3.rs:1367`) answers `{messageCount, statistics, messages, restoreSupported}`. `SessionRewindResponse` requires `state` and `sessionLog`; `hydrated_result` (`release3.rs:1992`) emits neither. `restoreErrors` is hard-coded to `[]` in both code paths (`release3.rs:1363`, `session_management.rs:106`).

8. **Nothing measures any of it.** Four differential oracles exist in this repository, for the tool surface, the configuration surface, the app-server wire surface and tool execution. None covers checkpoints. `docs/parity.md` scores the part 50 from a reading of module presence, and the three parts it drags with it, Review and turn diff at 80, Sessions and rewind at 80, the app-server protocol at 95, are all scored without a single assertion over region identity, dependency edges or decision closure. `cargo test --workspace --all-features` passes green against a `review/state` that answers empty in production.

**Why now:** `docs/parity.md` places Checkpoints at rank 9 of its execution order, and the stated reason it sits there is that it depends on `write_file` and `edit` to capture mutations, both of which shipped with rank 1. That dependency is now satisfied, and the cost of further deferral is the ordering principle's own: `RegionId` is a stable identity a client sends back across turns, exactly like the tool names of rank 1, and every week the surface answers with a synthetic `(0, 0)` is a week of client code written against an identity that will change. Ranks 10 and 11 are both blocked behind unrelated work, so this is the next rank whose dependencies are clear. The four existing oracles also make the instrument cheap: `scripts/parity/tool_execution.py` already captures reference behavior over a fixture tree through `git archive` at the pinned commit, and this work reuses its shape rather than inventing one.

## Overview

This initiative makes the Rust checkpoint engine behaviorally equivalent to the Python reference and puts the review and rewind surfaces on top of it. Equivalence is defined mechanically and narrowly: for a given sequence of turns, edits, manual drifts and decisions, this port must produce the same `RegionId` set, the same `dependsOn` edges, the same effective decision per region, the same reconstructed file bytes and the same hunk anchors as the reference. Everything downstream, the `review/*` responses and the `session/rewind*` responses, is then a projection of that shared state onto the wire models the app-server corpus already records.

The sequencing puts two instruments first, because the whole model rests on one algorithm. The first epic ports `difflib.SequenceMatcher` with its junk heuristic permanently disabled and Python's `str.splitlines(keepends=True)` line splitting, then captures an opcode corpus from the pinned reference and replays it. Nothing else can be measured until opcodes agree: `version_index` is an edit's sequence number, `ordinal` is its position in that edit's opcode list, and every dependency edge, anchor and target references that pair. The second epic builds the pure engine: domain types, the append-only log of turn marks, edits and decisions, region computation, dependency computation from line provenance, effective decisions with drag closure, reconstruction with re-encoding, and hunk anchors. The third adds manual-edit reconciliation, truncation and the recorder shell, and closes with the engine oracle: `scripts/parity/checkpoints.py` drives the reference `Checkpointer` and `ReviewManager` over scripted scenarios and records, per scenario, the region identities, the dependency edges, the decisions, the projections and the anchors as a committed corpus.

The fourth epic deletes the `ResourceService` review stub and projects the six methods from the real engine, and the fifth aligns `session/rewind` and `session/rewind/read` on `entryId` with the reference response shapes, migrating the TUI picker that currently depends on the divergent one, then remeasures `docs/parity.md`.

The reference is a read-only checkout pinned for this PRD at commit `68ff32e6a92e80a874c8153312f0aa8ae4955477` (v2.23.3), which every measurement in this document was taken from. Its location is machine-dependent: `C:\dev\mistral-vibe` on Windows and `/home/arthur/dev/mistral-vibe` on Linux. Reference links below use the Linux form as the canonical spelling and resolve against whichever checkout is local; the parity scripts read `VIBE_REFERENCE` as an override and `--reference` wins over both, and Rust tests reach it through `vibe_core::parity::reference_root`. The module is [vibe/core/checkpoints](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints), 1 344 lines across 8 files, and splits into four parts every story navigates back to: [models.py](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/models.py) and [_events.py](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/_events.py) declare the domain, [history.py](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/history.py) is the pure read model at 703 lines, [checkpointer.py](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/checkpointer.py) owns the log and its lifecycle, and [recorder.py](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/recorder.py), [file_store.py](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/file_store.py) and [fs.py](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/fs.py) are the impure shells. Three contracts reach outside the module and stay in scope: [vibe/core/review/manager.py](/home/arthur/dev/mistral-vibe/vibe/core/review/manager.py) projects the engine for review, [vibe/core/rewind/manager.py](/home/arthur/dev/mistral-vibe/vibe/core/rewind/manager.py) for rewind, and [vibe/utils/io.py:103](/home/arthur/dev/mistral-vibe/vibe/utils/io.py) supplies the `decode_safe` and `encode_safe` round trip that partial reverts re-encode through.

Two constraints shaped the plan. `NOTICE` declares that no upstream implementation source is copied, translated, vendored, linked, or shipped. The corpus records region identities, dependency edges, decision values, line spans and content digests, never docstrings or message text, exactly as the four existing corpora already do. Prose that must exist in Rust is written originally. Second, the reference's own algorithm is CPython's `difflib`, which is not the upstream implementation and carries no such restriction; the port is written from the published algorithm and verified against a captured opcode corpus rather than transcribed.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Reproduce the opcode sequence | 0 divergent opcode tuples over the captured corpus, on every fixture including files above the 200-line heuristic threshold | 0 maintained, replay wired into CI |
| Reproduce region identity | 100 % of `RegionId` pairs equal to the reference over every captured scenario | 0 scenarios where a client-supplied `(versionIndex, ordinal)` resolves differently |
| Reproduce the dependency graph | 100 % of `dependsOn` edge sets equal, including whole-file barriers | 0 maintained |
| Reproduce decision closure | 100 % of effective decisions equal after drag, with revert proven to be a ratchet | 0 maintained |
| Publish the review vocabulary | 25 of 25 `Review*` models emitted with all 82 fields, 7 of 7 targets honored | 0 synthetic values on the review surface |
| Reproduce the rewind contract | `entryId` addressing on both methods, `SessionRewindReadResponse` and `SessionRewindResponse` validating against the census | 0 divergent fields |
| Make conformance mechanically enforced | Corpus replays at least 40 scenarios and fails on any divergence outside a named ledger | Ledger empty or every entry justified by `NOTICE` |
| Raise the measured score | `docs/parity.md` Checkpoints from 50 to 100, measured by the new oracle | Review and turn diff from 80 to 95, Sessions and rewind from 80 to 90 |

## Target Users

### Editor integration author rendering agent changes for approval

- **Role:** Author of an IDE extension or agent bridge speaking JSON-RPC to the app-server, written against the reference protocol documentation.
- **Behaviors:** Calls `review/state` after each turn to list changed files and their regions, renders an inline accept or revert control per hunk from `review/hunks`, sends `review/approve` with a `ReviewRegionTarget` when the user accepts one hunk, and reads `review/turnDiff` to show what a single turn contributed.
- **Pain points:** `review/state` returns an empty file list in every production session, so the panel is permanently empty. Working around that by driving the fixture path yields one synthetic region per file with `versionIndex: 0`, `ordinal: 0` and an empty `dependsOn`, so the extension cannot address a specific hunk, cannot group hunks that decide together, and cannot distinguish a binary change from a text change because `OpaqueReviewRegion` is never emitted.
- **Current workaround:** Bypass the review surface entirely and shell out to `git diff`, which loses the turn attribution, the manual-edit attribution and the accepted-baseline notion the protocol exists to provide.
- **Success looks like:** The panel lists real files with real regions, each region carries the turn or the manual edit that produced it, accepting one hunk pulls in the hunks it was built on, and reverting one drags its dependents.

### Operator recovering from an agent turn that went wrong

- **Role:** Developer running `vibe` interactively who wants to undo part of what the agent just did without discarding the rest.
- **Behaviors:** Reviews the turn's changes, keeps two of five hunks, reverts the other three, then continues the conversation. Occasionally edits a file by hand between turns.
- **Pain points:** There is no per-hunk decision at all; the engine's coarse `approve()` and `revert()` are all-or-nothing and unreachable from any surface. A hand edit made between turns is absorbed into the next turn's baseline, so reverting that turn silently destroys it. Rewinding is addressed by `messageIndex`, which is a position in a mutable message list rather than a stable entry identity, so a rewind after a compaction can land on the wrong turn.
- **Current workaround:** Commit before every turn and use `git checkout -p`, which reintroduces exactly the manual bookkeeping the tool is supposed to remove.
- **Success looks like:** Partial acceptance works per hunk, a hand edit survives a turn revert unless it sits on the reverted lines, and rewinding targets a stable entry.

### Parity maintainer certifying the port

- **Role:** Maintainer running the parity suite before proposing a commit, reading `docs/parity.md` as the record of what is proven.
- **Behaviors:** Runs the CI sequence, reads the per-family conformance counts each oracle prints, and updates the scorecard only from a measurement.
- **Pain points:** The Checkpoints score of 50 comes from reading module presence, not from a measurement, so it is neither falsifiable nor improvable in a defensible way. Three adjacent scores depend on the same unmeasured behavior.
- **Current workaround:** None. The part is the largest unmeasured surface left in the scorecard.
- **Success looks like:** One command prints the conformance counts, a divergence names the scenario and the field, and the score is a number the command produced.

## Research Findings

Key findings that informed this PRD:

### Algorithm landscape

- **`difflib` crate 0.4.0** ([DimaKudosh/difflib](https://github.com/DimaKudosh/difflib)): a Rust port of CPython's `difflib`, MIT, exposing `find_longest_match`, `get_matching_blocks` and `get_opcodes` with the same queue-based recursion. Disqualified on inspection: it applies the popular-element heuristic unconditionally, with no `autojunk` switch, and the filter at `src/sequencematcher.rs:128-136` keeps `indexes.len() > test_len` where CPython deletes exactly those elements. The reference disables the heuristic entirely with `autojunk=False`, and the branch triggers on any sequence of 200 or more elements, which is most source files. Last released 2018-07-22.
- **`similar`, `imara-diff`, `dissimilar`**: all implement Myers, patience or histogram diffing. These produce valid diffs but a different opcode sequence from `difflib`'s leftmost-longest-block recursion, so region ordinals and therefore `RegionId` values would diverge. Disqualified by the contract, not by quality.
- **Conclusion:** the algorithm is roughly 250 lines and its correctness must be corpus-verified whatever its source, so a first-party port with the heuristic permanently off costs less than an unmaintained dependency that still needs the same verification. This also respects the repository's stated complexity budget, which admits a dependency only for a current requirement that forces it.

### Best practices applied

- **Instrument before implementation.** All four existing oracles in this repository were built before the work they measure, and `docs/parity.md` records that as the reason the measured parts score 95 and above while the unmeasured ones sit at 50 to 80. This PRD builds the opcode oracle in epic 1 and the engine oracle at the end of epic 3, both before the surface that consumes them is rebuilt.
- **Capture through `git archive`, never by moving HEAD.** `scripts/parity/tool_execution.py` reads the pinned commit without creating a branch or a worktree, so a workstation whose checkout has moved on can still capture. The new script follows it.
- **Commit observations, digest everything else.** The tool-execution corpus commits a captured string verbatim only when it is a value the case supplied, a normalized path or an identifier-shaped token, and stores every other string as a SHA-256 digest. Region identities, line spans, decision values and dependency edges are all structural, so they commit verbatim; file contents commit as digests.
- **Ledger the divergences, and fail when the ledger goes stale.** `STRICTER_THAN_THE_REFERENCE` in the shell oracle and the `LICENSING` entries in the tool-execution oracle both fail the suite when a listed divergence disappears, which is what keeps the record honest.

### Pure core, impure shell

The reference module is explicitly split: `History` and `Checkpointer` never touch disk and take content in and hand content out, while `CheckpointRecorder`, `FileStore` and the `Filesystem` protocol do the reading and writing. Every read shell observes the same `Checkpointer`. That split is what makes 80 reference test functions possible without a filesystem, and the port keeps it: the engine lands in `vibe-core` with no `Workspace` dependency, and the existing `Workspace` becomes its filesystem port.

*Full research sources available in project documentation.*

## Assumptions & Constraints

### Assumptions (to validate)

- **The opcode port can reach byte-exact agreement with CPython's `difflib` on realistic inputs.** Based on the algorithm being fully specified in the CPython source and deterministic, with no floating point and no hash-order dependence once `b2j` is built from a stable map. US-121 validates this before anything depends on it; if it fails, every region identity in this PRD is unreachable and the whole plan must be reconsidered.
- **Python's `str.splitlines(keepends=True)` boundary set is the only line-splitting subtlety.** After `decode_safe` normalizes line endings to `\n`, the remaining separators Python still splits on are `\v`, `\f`, `\x1c`, `\x1d`, `\x1e`, `\x85`, `\u2028` and `\u2029`. Based on reading `normalize_newlines` at [vibe/utils/io.py:94](/home/arthur/dev/mistral-vibe/vibe/utils/io.py) and the CPython documentation of `str.splitlines`.
- **No persisted session on disk carries a region identity today.** Based on the stub never writing regions to storage and `record_review_change` having no production caller, so there is no migration to perform. If a stored session is found to carry one, US-138 gains a migration criterion.
- **The engine can hold its log in memory for a session's lifetime.** Based on the reference doing exactly that, unbounded, and on the log being cleared on every message-list reset. The 512 MiB ceiling in the NFRs is the guard, not the expectation.

### Hard constraints

- `NOTICE` forbids copying, translating, vendoring, linking or shipping upstream implementation source, prompt files or tool description text. The corpus commits structural observations only; every Rust doc comment and error message is written originally.
- The reference pin lives in exactly two places, `vibe_core::parity::REFERENCE_COMMIT` and `EXPECTED_COMMIT` in `scripts/parity/pin.py`, held equal by `crates/vibe-core/src/parity/parity_tests.rs`. This PRD does not re-pin; a re-pin regenerates all corpora as its own change.
- A missing or off-pin reference checkout must never fail `cargo test`. Committed corpora replay unconditionally; only the live recapture probe skips.
- The layering in `[workspace.metadata.vibe] dependency-layers` holds: the engine belongs in `vibe-core`, its projection in `vibe-app-server`, and `vibe-cli` and `vibe-acp` are adapters.
- `unsafe_code` is forbidden workspace-wide; `panic`, `unimplemented` and `dbg_macro` are denied outside tests.
- `[workspace.package] version` is not bumped by this work.

## Quality Gates

These commands must pass for every user story:

- `cargo fmt --all -- --check` - formatting
- `cargo check --workspace --all-targets --all-features` - compilation of every target including the fixture binaries
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` - lint set with warnings denied
- `cargo test --workspace --all-features` - the full suite, never a filtered subset, because parity fixtures are read from more than one module

Stories that touch a parity corpus additionally report their conformance counts:

- `cargo test -p vibe-core --all-features checkpoint_parity_tests -- --nocapture` - engine conformance counts
- `cargo test -p vibe-app-server --all-features app_server_surface_parity_tests -- --nocapture` - wire census, which the review and rewind models are validated against

## Reference Map

Every file an implementer opens before writing Rust, at the pinned commit `68ff32e`. Paths use the Linux canonical spelling and resolve against whichever checkout is local, through `VIBE_REFERENCE` or `--reference`. Each story below names its own anchor; this is the whole surface in one place. Reading these is required by `AGENTS.md`, and grepping them does not replace opening the declaration they point at.

### The module (8 files, 1 344 lines)

| File | L. | What it owns |
|---|---|---|
| [checkpoints/history.py](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/history.py) | 703 | The pure read model: regions (`:52`), dependencies (`:293`), effective decisions (`:247`), reconstruction (`:528`), provenance (`:559`), anchors (`:645`), restore plans (`:382`) |
| [checkpoints/checkpointer.py](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/checkpointer.py) | 279 | The log and its lifecycle (`:76`), reconciliation (`:177`), truncation (`:169`), decisions (`:218`), rollback (`:151`) |
| [checkpoints/models.py](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/models.py) | 120 | `FileState`, `Region`, `OpaqueChange`, `RegionId`, `TurnRegion`, `Owner`, `Decision`, `HunkAnchor` |
| [checkpoints/recorder.py](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/recorder.py) | 67 | The write shell: carried paths at turn start, per-path tolerance at seal |
| [checkpoints/\_\_init\_\_.py](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/__init__.py) | 47 | The 22 exported names, which is the module's public surface |
| [checkpoints/_events.py](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/_events.py) | 47 | `_TurnMark`, `_Edit`, `_Decide` and the region ordering key |
| [checkpoints/file_store.py](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/file_store.py) | 41 | Reading a state and applying a plan (`:18`) |
| [checkpoints/fs.py](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/fs.py) | 40 | The `Filesystem` port and its absent-versus-unreadable contract (`:8`) |

### What reaches into it

| File | Anchor | Why it matters here |
|---|---|---|
| [core/tools/base.py](/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py) | `:445`, `:454` | `get_file_snapshot` is the capture hook every tool inherits, defaulting to nothing, and `get_file_snapshot_for_path` is the helper the two overriders call. This is the contract US-131 reproduces |
| [core/tools/builtins/write_file.py](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/write_file.py) | `:91` | One of exactly two tools that override the hook. This port already captures at the same two points (`crates/vibe-core/src/workspace.rs:1203`) |
| [core/tools/builtins/edit.py](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/edit.py) | `:95` | The other one (`crates/vibe-core/src/workspace.rs:1125` here) |
| [core/agent_loop/_loop.py](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py) | `:569`, `:1133`, `:1148`, `:2139` | One `Checkpointer` is constructed and shared with the recorder, the review manager and the rewind manager; the turn boundaries call `create_checkpoint` and `seal_turn`; a tool's snapshot is collected before it runs |
| [core/review/manager.py](/home/arthur/dev/mistral-vibe/vibe/core/review/manager.py) | 453 L. | The read and mutate shell EP-039 reproduces: `review_state` (`:235`), the decision path (`:340`), `scope_file_diff` (`:269`), `file_hunks` (`:291`) |
| [core/rewind/manager.py](/home/arthur/dev/mistral-vibe/vibe/core/rewind/manager.py) | 147 L. | The shell EP-040 reproduces: `restorable_paths_at` (`:39`), `rewind_to_message` (`:75`), `_on_messages_reset` (`:130`) |
| [utils/io.py](/home/arthur/dev/mistral-vibe/vibe/utils/io.py) | `:94`, `:103`, `:128` | `normalize_newlines`, `decode_safe` and `encode_safe`, the round trip a partial revert re-encodes through. Already reproduced at `crates/vibe-core/src/workspace/text_file.rs` over a narrower codec set |

### What publishes it on the wire

| File | Anchor | Why it matters here |
|---|---|---|
| [app_server/review.py](/home/arthur/dev/mistral-vibe/vibe/app_server/review.py) | 135 L. | The 16 wire model declarations, which the app-server census already records as 25 `Review*` entries with 82 fields |
| [app_server/_review.py](/home/arthur/dev/mistral-vibe/vibe/app_server/_review.py) | 271 L. | The dispatch (`:75`) and every projection: state (`:133`), region (`:160`), owner (`:200`), target (`:216`), status (`:244`), decision (`:256`) |
| [app_server/_handler.py](/home/arthur/dev/mistral-vibe/vibe/app_server/_handler.py) | `:164`, `:715`, `:723`, `:769` | Where the review handler is constructed with its session and idle gates, and where rewind resolves an entry identifier to a message index |

### The behavioral inventory

The reference test suite is the scenario list US-132 captures from. It is read for scenario names and shapes; no assertion text is copied.

| File | L. | Test functions |
|---|---|---|
| `tests/core/test_checkpointer_per_turn.py` | 680 | 51 |
| `tests/core/test_review_manager.py` | 631 | 21 |
| `tests/core/test_checkpoint_recorder.py` | 136 | 5 |
| `tests/core/test_review_manager_properties.py` | 211 | 3 |
| `tests/core/rewind/test_rewind_manager.py` | 838 | rewind lifecycle |
| `tests/core/rewind/test_rewind_integration.py` | 384 | rewind against the engine |
| `tests/app_server/test_review.py` | 165 | the six methods end to end |
| `tests/app_server/test_rewind.py` | 120 | both rewind methods |
| `tests/acp/test_review.py` | 150 | proves ACP delegates review to the app-server rather than reimplementing it |

## Epics & User Stories

### EP-036: The Opcode Engine and Its Oracle

Port the diff algorithm every region identity derives from, and prove it against a captured corpus before anything is built on it. Nothing downstream is measurable while opcodes can differ.

**Definition of Done:** A `SequenceMatcher` in `vibe-core` produces opcode tuples identical to the reference on every fixture in a committed corpus, including fixtures above the 200-element heuristic threshold, and line splitting matches Python's on every boundary character.

#### US-119: Reproduce Python line splitting for checkpoint text

**Description:** As a parity maintainer, I want the engine to split decoded text into lines exactly as `str.splitlines(keepends=True)` does so that region line spans address the same lines the reference addresses.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None
**Reference:** [vibe/utils/io.py:94](/home/arthur/dev/mistral-vibe/vibe/utils/io.py) for `normalize_newlines`, and [vibe/core/checkpoints/history.py:25](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/history.py) for `_decode_lines`, which returns keepends lines for text, nothing for binary and an empty list for an absent file

**Acceptance Criteria:**
- [ ] Given text containing only `\n` separators, when it is split, then each line retains its trailing `\n` and the result matches `text.split_inclusive('\n')` with no empty trailing element
- [ ] Given text containing `\v`, `\f`, `\x1c`, `\x1d`, `\x1e`, `\x85`, `\u2028` or `\u2029`, when it is split, then each of those characters ends a line, matching Python
- [ ] Given an empty string, when it is split, then the result is an empty list, not a list holding one empty string
- [ ] Given text ending without a separator, when it is split, then the final line is present and carries no separator
- [ ] Given a `FileState` holding no data, when lines are requested, then the result is an empty list and the call does not fail
- [ ] Given a `FileState` whose bytes contain a NUL, when lines are requested, then the result signals binary rather than returning lines
- [ ] The joined lines reconstruct the input exactly for every fixture in the round-trip test

#### US-120: Port SequenceMatcher with the junk heuristic permanently disabled

**Description:** As a parity maintainer, I want a `SequenceMatcher` over line sequences that reproduces `difflib`'s leftmost-longest-block recursion so that region ordinals are the reference's ordinals.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-119
**Reference:** [vibe/core/checkpoints/history.py:52](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/history.py) for `_regions` and `_base_to_result`, the three call sites that pass `autojunk=False` being lines 53, 67 and 675. The algorithm itself is CPython's `difflib`, not reference source

**Acceptance Criteria:**
- [ ] Given two line sequences, when opcodes are requested, then the result is the ordered `(tag, i1, i2, j1, j2)` list with `tag` in `equal`, `replace`, `delete`, `insert`, and adjacent opcodes never both non-equal with the same tag
- [ ] Given a sequence of 200 or more lines with a line repeated more than one percent of the time, when opcodes are requested, then that line still participates in matching, because the popular-element heuristic is not applied
- [ ] Given two identical sequences, when opcodes are requested, then the result is a single `equal` opcode spanning both
- [ ] Given an empty first sequence, when opcodes are requested, then the result is a single `insert` covering the second, and the symmetric case yields a single `delete`
- [ ] Given two sequences sharing no line, when opcodes are requested, then the result is a single `replace` spanning both
- [ ] The matching-block list always terminates with the sentinel triple `(len(a), len(b), 0)`, as the reference algorithm requires
- [ ] No new entry appears in `Cargo.toml` or `Cargo.lock` for this story

#### US-121: Capture and replay the opcode corpus

**Description:** As a parity maintainer, I want the reference's opcodes captured from the pinned checkout and replayed against this port so that a divergence is a test failure naming the fixture.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-120
**Reference:** [vibe/core/checkpoints/history.py:52](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/history.py), plus `scripts/parity/tool_execution.py` in this repository for the capture shape to reuse

**Acceptance Criteria:**
- [ ] Given the pinned reference, when the capture script runs, then it records opcode tuples for at least 30 line-sequence fixtures covering identical, disjoint, prefix-shared, suffix-shared, repeated-line, above-threshold and empty inputs
- [ ] The capture reads the pinned commit through `git archive` and never moves `HEAD`, creates a branch or adds a worktree
- [ ] The corpus commits line sequences as content digests and opcode tuples verbatim, and holds no reference-authored prose
- [ ] Given a committed corpus, when the replay runs, then it executes unconditionally and reports the count of matching fixtures and the count of divergent ones
- [ ] Given a missing or off-pin reference checkout, when the suite runs, then only the recapture probe skips and the replay still runs
- [ ] Given one divergent fixture, when the replay runs, then it fails naming the fixture, the expected opcode list and the produced one
- [ ] The script accepts `--reference` and honors `VIBE_REFERENCE`, and cites the pin from one of the two pin sources rather than spelling a third

---

### EP-037: The Event Log and the Pure Read Model

Build the engine the reference calls `Checkpointer` and `History`: an append-only log resolved into file states, regions, dependencies, decisions and anchors, with no disk access anywhere in it.

**Definition of Done:** A pure engine in `vibe-core` answers every read the reference `History` answers, over a log it never mutates, with no `Workspace` or filesystem dependency in the module.

#### US-122: Declare the checkpoint domain types

**Description:** As an engine author, I want the domain vocabulary the reference declares so that every later story names the same things.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-119
**Reference:** [vibe/core/checkpoints/models.py](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/models.py) for the whole domain and [vibe/core/checkpoints/_events.py](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/_events.py) for the three event variants. [vibe/core/checkpoints/__init__.py:24](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/__init__.py) lists the 22 exported names

**Acceptance Criteria:**
- [ ] `FileState` wraps optional bytes and answers whether it exists and whether it is binary, with binary meaning the bytes contain a NUL
- [ ] `Region` carries a baseline start with its lines and a current start with its lines, and the spans are half-open
- [ ] `OpaqueChange` carries a reason of `missing` or `binary_or_undecodable` plus both sides' states, so a revert can restore from it
- [ ] `RegionId` pairs a version index with an ordinal, orders by that pair, and is usable as a map key
- [ ] `Owner` is either an agent turn carrying a turn identifier or a manual edit carrying a one-based index, and the two are distinguishable on the wire
- [ ] `Decision` holds `pending`, `keep` and `revert` and serializes to those exact lowercase strings
- [ ] `HunkAnchor` carries a side of `additions` or `deletions`, a zero-based line and the region identifiers it decides together
- [ ] Given a decision of `pending` passed where a keep or revert is required, when it is converted, then the call fails with a stated error rather than defaulting

#### US-123: The append-only log and the turn lifecycle

**Description:** As an engine author, I want a log of turn marks, edits and decisions with a strict turn lifecycle so that every later read resolves from one ordered source.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-122
**Reference:** [vibe/core/checkpoints/checkpointer.py:76](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/checkpointer.py) through line 165 for `begin_turn`, `record_pre_edit`, `record_post_edit`, `seal_turn`, `clear` and `atomic`

**Acceptance Criteria:**
- [ ] Given no open turn, when a turn begins, then a mark is appended carrying the turn identifier and an empty pre-edit map
- [ ] Given an open turn, when a turn begins again, then the call fails with a turn-state error and the log is unchanged
- [ ] Given an open turn, when a pre-edit state is recorded for a path already recorded this turn, then the first recording is kept and the second is ignored
- [ ] Given no open turn, when a pre-edit or post-edit state is recorded, then the call fails with a turn-state error
- [ ] Given an open turn with recorded pre-edit states, when the turn is sealed, then one edit is appended per path whose post state differs from its pre state, owned by that turn, and paths with no change append nothing
- [ ] Given a path recorded pre-edit but never post-edit, when the turn is sealed, then its post state defaults to its pre state and no edit is appended
- [ ] Given no open turn, when the turn is sealed, then the call succeeds and does nothing, so sealing is idempotent
- [ ] Given a body that fails inside the atomic scope, when the scope exits, then the log, the sequence counter, the manual index and the open turn are all restored to their prior values and the failure propagates
- [ ] Given a log with events, when it is cleared, then the events, the sequence counter, the manual index and the open turn are all reset

#### US-124: Compute regions and classify opacity

**Description:** As an engine author, I want each edit resolved into either line regions or one whole-file unit so that binary, deleted and empty-toggle changes are reviewable as units.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-121, US-123
**Reference:** [vibe/core/checkpoints/history.py:155](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/history.py) for `_is_opaque`, `_compute_changes` and `_opaque_reason`

**Acceptance Criteria:**
- [ ] Given a text edit, when its changes are computed, then they are the non-equal opcodes in order, each carrying both sides' lines, with the ordinal equal to its position in that list
- [ ] Given an edit where either side is binary, when its changes are computed, then the result is one opaque change with reason `binary_or_undecodable`
- [ ] Given an edit whose after state is absent, when its changes are computed, then the result is one opaque change with reason `missing`
- [ ] Given an edit that toggles existence without any textual difference, such as creating an empty file, when its changes are computed, then the result is one opaque change rather than an empty region list
- [ ] Given an edit whose before state is absent and whose after state has content, when its changes are computed, then the reason is `missing`
- [ ] Given the same edit computed twice, when changes are requested, then the second call returns the memoized result and produces no additional diff work
- [ ] Given a file with no edits at all, when its regions are requested, then the result is empty and the call does not fail

#### US-125: Compute dependencies from line provenance

**Description:** As an engine author, I want each new region to declare the earlier regions it was built on so that a decision on one never leaves a superseded region stranded.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-124
**Reference:** [vibe/core/checkpoints/history.py:293](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/history.py) for `compute_deps` and line 345 for `_applied_barriers`

**Acceptance Criteria:**
- [ ] Given a projection carrying per-line provenance, when a new edit replaces lines, then each of its regions depends on the distinct producers of the lines it replaces, sorted by region identifier
- [ ] Given an insertion, when its dependencies are computed, then it depends on the producer of the line above it, or of the line below when it inserts at the top
- [ ] Given an insertion into a file with no prior edit, when its dependencies are computed, then the result is empty rather than a dangling reference
- [ ] Given an applied whole-file barrier such as a deletion or a binary rewrite, when a later text edit is appended, then every region of that edit depends on the barrier
- [ ] Given an opaque edit, when its dependencies are computed, then it depends on every region currently applied, sorted
- [ ] Given a region whose provenance lines were written by more than one earlier region, when its dependencies are computed, then all of them appear exactly once
- [ ] Dependencies are fixed when the edit is appended and never recomputed as the log grows

#### US-126: Resolve effective decisions with drag and the revert ratchet

**Description:** As an engine author, I want a region's effective decision derived by closure so that reverting one region drags everything built on it without storing the closure.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-125
**Reference:** [vibe/core/checkpoints/history.py:247](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/history.py) for `effective` and line 481 for `_explicit_decisions`, plus [vibe/core/checkpoints/checkpointer.py:218](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/checkpointer.py) through line 279 for `decide_region`, `decide_scope`, `decide_file` and `_decide`

**Acceptance Criteria:**
- [ ] Given a region explicitly reverted, when effective decisions are resolved, then it is `revert`
- [ ] Given a region any of whose dependencies is effectively reverted, when decisions are resolved, then it is `revert` even though nothing was recorded against it
- [ ] Given a region explicitly kept and no reverted dependency, when decisions are resolved, then it is `keep`
- [ ] Given a region with no recorded decision, when decisions are resolved, then it is `pending`
- [ ] Given a region already reverted, when a keep is recorded against it, then the keep is ignored and the region stays reverted, so revert is a ratchet
- [ ] Given a keep on a region with pending dependencies, when the decision is recorded, then those dependencies are recorded as kept too, in region-identifier order
- [ ] Given a revert on a region with dependents, when the decision is recorded, then only the target is recorded and the dependents are dragged at read
- [ ] Given a decision naming a region the file does not carry, when it is recorded, then the call fails naming the path and the region and the log is unchanged
- [ ] Given an open turn, when any decision is recorded, then the call fails with a turn-state error

#### US-127: Reconstruct file states from applied regions

**Description:** As an engine author, I want the file rebuilt from the regions currently applied so that both the on-disk projection and the accepted baseline come from one code path.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-126
**Reference:** [vibe/core/checkpoints/history.py:491](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/history.py) for `_applied` and `_applied_before`, line 528 for `_reconstruct`, line 559 for `_reconstruct_with_prov`, and line 38 for `_reencode`

**Acceptance Criteria:**
- [ ] Given every region pending and none reverted, when the file is projected, then the result equals the last edit's after state byte for byte
- [ ] Given only kept regions selected, when the file is projected, then the result is the accepted baseline holding exactly those regions
- [ ] Given a set of applied regions, when the file is reconstructed, then each text edit's spans are rebased through its own before state onto the running result and spliced from right to left
- [ ] Given an applied opaque edit, when the file is reconstructed, then it replaces the whole file and later edits splice onto that
- [ ] Given a region whose dependencies are not all applied, when applied regions are selected, then it is excluded, so every splice lands on its exact base
- [ ] Given a file whose earliest concrete state used CRLF and a single-byte codec, when a partial revert reconstructs it, then the result is re-encoded with that line ending and that codec rather than rewritten to UTF-8 with LF
- [ ] Given a file with no edits, when it is projected, then the result is its original state
- [ ] Given every region reverted, when the file is projected, then the result equals the original state byte for byte

#### US-128: Anchor pending hunks in rendered diff coordinates

**Description:** As an editor integration author, I want each pending change located in the rendered diff so that an inline accept or revert control can be pinned to it.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-127
**Reference:** [vibe/core/checkpoints/history.py:645](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/history.py) for `_pending_hunks`, line 619 for `_pending_components`, line 119 for `_match_deletions`, and line 355 for `scope_pending_diff`

**Acceptance Criteria:**
- [ ] Given a pending addition or edit, when anchors are computed, then it anchors on the current side with the line index of the block's last line
- [ ] Given a pending pure deletion, when anchors are computed, then it anchors on the baseline side with the line index of the removed block's last line
- [ ] Given two independent deletions of identical text, when anchors are computed, then each is claimed by a distinct non-overlapping run and neither is decided by the other's control
- [ ] Given pending regions connected by dependency edges, when anchors are computed, then each anchor's target is the full connected component, so deciding it never strands a superseded region
- [ ] Given an owner, when anchors are computed for that scope, then the diff is that owner's kept regions against its kept plus pending regions
- [ ] Given an owner with no pending region, when anchors are computed for that scope, then the result is empty
- [ ] Given a diff block that attributes to no pending region, when anchors are computed, then no anchor is emitted for it
- [ ] Given a file with no edits, when anchors are computed, then the result is empty and the call does not fail

---

### EP-038: Manual Edits, Truncation and the Engine Oracle

Capture what the user changed by hand, support truncating the log at a turn boundary, drive the whole thing from the turn lifecycle, and prove the result against the reference.

**Definition of Done:** A hand edit is a first-class owner with its own review slot, a truncation restores every affected file to its state at the cut, and a committed corpus replays the reference's answers with the conformance counts printed.

#### US-129: Capture manual edits by reconciliation

**Description:** As an operator, I want a file I edited by hand recorded as my own change so that reverting an agent turn does not destroy my work.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-127
**Reference:** [vibe/core/checkpoints/checkpointer.py:177](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/checkpointer.py) for `reconcile` and line 83 through 113 for `record_pre_edit` and `_insert_local_before_mark`, which seals a between-turn drift just before the turn mark

**Acceptance Criteria:**
- [ ] Given live content differing from the projection and no open turn, when reconciliation runs for that path, then a manual edit is appended from the projection to the live content, carrying the next one-based manual index
- [ ] Given live content equal to the projection, when reconciliation runs, then nothing is appended, so reconciliation is idempotent
- [ ] Given an open turn, when reconciliation runs, then nothing is appended, because that disk belongs to the turn
- [ ] Given a path that drifted since it was last seen, when a pre-edit state is recorded for it during a turn, then the drift is appended as a manual edit ordered immediately before that turn's mark
- [ ] Given a manual edit sealed before a turn mark, when the log is truncated to that turn, then the manual edit is kept and only the turn and later events are dropped
- [ ] Given a manual edit disjoint from a turn's regions, when that turn is reverted, then the manual edit survives
- [ ] Given a manual edit overlapping a turn's regions, when that turn is reverted, then the manual edit is dragged with it
- [ ] Each distinct owner keeps a permanent slot in log order, so resolving one scope never renumbers another

#### US-130: Truncate the log and plan a restore

**Description:** As an operator, I want the log truncated at a turn boundary and every affected file restored to its state at that cut so that rewinding undoes file changes as well as messages.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-123
**Reference:** [vibe/core/checkpoints/checkpointer.py:169](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/checkpointer.py) for `drop_turns_from`, and [vibe/core/checkpoints/history.py:382](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/history.py) through line 418 for `restore_plan`, `restore_plan_to_turn` and `event_index_of_turn`

**Acceptance Criteria:**
- [ ] Given a turn identifier present in the log, when the log is truncated from it, then that turn's mark and every later event are dropped, decisions included, and any open turn is closed
- [ ] Given a turn identifier absent from the log, when the log is truncated from it, then the earliest later turn is used, and when there is none the log is unchanged
- [ ] Given a turn identifier carried by more than one mark after a transcript reset reused it, when the index is resolved, then the newest exact match wins
- [ ] Given a truncation index, when a restore plan is computed, then each file touched by a dropped turn maps to that turn's recorded pre state, taking the earliest dropped turn's recording
- [ ] Given a file touched only by a dropped manual edit, when a restore plan is computed, then it maps to the projection of the kept log
- [ ] Given a restore plan, when it is compared against disk, then only paths whose current content differs are reported as restorable
- [ ] Given a turn identifier the log does not carry at all, when a restore plan to that turn is requested, then the plan is empty
- [ ] Given a truncation, when decisions recorded after the cut are considered, then they are dropped with it

#### US-131: Drive the engine from the turn lifecycle through a filesystem port

**Description:** As an engine author, I want the impure shell that reads and writes disk kept behind a port so that the engine stays testable without a filesystem and one unreadable file cannot lose another file's change.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-130
**Reference:** [vibe/core/checkpoints/recorder.py](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/recorder.py) for the recorder, [vibe/core/checkpoints/file_store.py:18](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/file_store.py) for `apply` and [vibe/core/checkpoints/fs.py:8](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/fs.py) for the `Filesystem` protocol. The capture hook is declared outside the module: [vibe/core/tools/base.py:445](/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py) for `get_file_snapshot`, defaulting to nothing, and line 454 for `get_file_snapshot_for_path`, overridden only by [vibe/core/tools/builtins/write_file.py:91](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/write_file.py) and [vibe/core/tools/builtins/edit.py:95](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/edit.py). The agent loop drives it at [vibe/core/agent_loop/_loop.py:569](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py) where one `Checkpointer` is shared with the review and rewind managers, line 1133 and 1148 for the turn boundaries, and line 2139 where a tool's snapshot is collected before it runs

**Acceptance Criteria:**
- [ ] Given a turn starting, when the recorder creates a checkpoint, then it re-reads every path the previous turn tracked and records each as a pre-edit state, so a file mutated by a tool that produces no snapshot is still captured
- [ ] Given a tool about to mutate a file, when it hands a snapshot to the recorder, then that snapshot becomes the path's pre-edit state for the turn
- [ ] Given a turn sealing, when post-edit states are read, then each path is read independently and a failure on one is logged and skipped without preventing the others from being recorded
- [ ] Given a turn sealing where every read fails, when the turn is sealed, then the seal still happens and the turn closes
- [ ] Given a path that does not exist, when its state is read through the port, then the result is an absent state rather than an error
- [ ] Given a path that exists but cannot be read, when its state is read through the port, then the call fails rather than reporting the file as deleted
- [ ] Given a restore plan, when it is applied, then absent targets are deleted, present targets are written, targets already matching are skipped, and the paths actually changed are reported
- [ ] The engine module compiles with no dependency on `Workspace` or on any filesystem type

#### US-132: Capture and replay the engine corpus

**Description:** As a parity maintainer, I want the reference engine's answers captured over scripted scenarios and replayed against this port so that the Checkpoints score is a number a command produced.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-128, US-129, US-130, US-131
**Reference:** [vibe/core/checkpoints](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints) as a whole, plus the reference test inventory that names the scenarios: `tests/core/test_checkpointer_per_turn.py` (51 functions), `tests/core/test_review_manager.py` (21), `tests/core/test_review_manager_properties.py` (3) and `tests/core/test_checkpoint_recorder.py` (5)

**Acceptance Criteria:**
- [ ] Given the pinned reference, when `scripts/parity/checkpoints.py` runs, then it drives the reference `Checkpointer` and `ReviewManager` over at least 40 scenarios and records, per scenario, the region identities with owners and decisions, the dependency edge sets, the effective decisions, the projections as content digests, the accepted baselines as digests and the hunk anchors
- [ ] The scenario inventory covers, by name, attribution across turns, per-turn revert, per-region revert, incremental revert to the original, dependency cascade in both directions, opaque changes including binary, deletion and empty-file toggle, the turn gate, manual-edit dependency, the revert ratchet, scope pending diffs, bulk decisions skipping already-decided regions, and rewind truncation
- [ ] The capture reads the pinned commit through `git archive` and never moves `HEAD`
- [ ] The corpus commits region identities, ordinals, owners, decisions, line spans, dependency edges and anchor coordinates verbatim, and every file content as a SHA-256 digest, with no reference-authored prose
- [ ] Given a committed corpus, when the replay runs, then it executes unconditionally and prints, per family, the count conforming and the count divergent
- [ ] Given a missing or off-pin checkout, when the suite runs, then only the recapture probe skips
- [ ] Given a divergence outside the named ledger, when the replay runs, then it fails naming the scenario, the family and the field
- [ ] Given a ledger entry whose divergence has been fixed, when the replay runs, then it fails, so a stale ledger cannot survive
- [ ] `cargo test -p vibe-core --all-features checkpoint_parity_tests -- --nocapture` reproduces the counts

---

### EP-039: The Review Surface on the Real Engine

Delete the stub and project the six review methods from the engine, so the vocabulary the corpus records is the vocabulary the server speaks.

**Definition of Done:** `ResourceService` holds no review state, all six methods read the session's engine, and every response validates against the app-server census.

#### US-133: Project review state from the engine and delete the stub

**Description:** As an editor integration author, I want `review/state` to list the real changed files with their real regions so that the review panel shows what the agent did.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-132
**Reference:** [vibe/core/review/manager.py:235](/home/arthur/dev/mistral-vibe/vibe/core/review/manager.py) for `review_state`, line 318 for `_review_scopes`, line 425 for `_review_file` and line 440 for `_status`, projected onto the wire by [vibe/app_server/_review.py:133](/home/arthur/dev/mistral-vibe/vibe/app_server/_review.py) for `_project_state` and line 160 for `_project_region`. The wire models are declared in [vibe/app_server/review.py](/home/arthur/dev/mistral-vibe/vibe/app_server/review.py)

**Acceptance Criteria:**
- [ ] Given a session whose engine holds pending regions, when `review/state` is called, then each tracked file with at least one pending region is listed with its status and every one of its regions
- [ ] Given a file whose regions are all decided, when `review/state` is called, then the file is absent from the list
- [ ] Given a text region, when it is projected, then it carries `kind: "text"`, its version index, its ordinal, its owner, both line starts, both line counts, its decision and its dependency references
- [ ] Given an opaque region, when it is projected, then it carries `kind: "opaque"`, its reason of `missing` or `binary_or_undecodable`, and no line coordinates
- [ ] Given a manual edit owner, when it is projected, then it carries `kind: "manual"` with its index, and an agent turn carries `kind: "agent"` with its turn identifier
- [ ] Given every owner that ever produced a change, when scopes are projected, then each keeps a slot in log order with its still-pending files and their region counts, and a fully decided owner keeps its slot with no files
- [ ] Given a deleted file, when its status is projected, then it is `deleted`; a file with a binary or undecodable region is `binary_or_undecodable`; a file whose original state was absent is `created`; otherwise `modified`
- [ ] Given a session with no tracked file, when `review/state` is called, then it answers with empty files and empty scopes rather than failing
- [ ] Rendering is an acting boundary: manual drift is reconciled before the state is projected
- [ ] `ResourceService` no longer declares a review map, `record_review_change` is gone, and no review method reads `ResourceService` state

#### US-134: Honor the seven review targets with atomic persistence

**Description:** As an operator, I want to accept or revert changes at the granularity I chose so that partial acceptance works and a failed disk write never leaves a committed decision behind.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-133
**Reference:** [vibe/core/review/manager.py:250](/home/arthur/dev/mistral-vibe/vibe/core/review/manager.py) for `approve_review` and `revert_review`, line 340 for `_decide` with its reconcile-then-atomic order, line 358 for `_decide_target` over the seven targets, line 397 for `_decide_last_turns` and line 406 for `_persist`. The wire mapping is [vibe/app_server/_review.py:216](/home/arthur/dev/mistral-vibe/vibe/app_server/_review.py) for `_core_target` and line 122 for the idle gate

**Acceptance Criteria:**
- [ ] Given a region target, when it is approved or reverted, then exactly that region is decided in that file
- [ ] Given a regions target, when it is decided, then every listed region in that file is decided together
- [ ] Given a file target, when it is decided, then every still-pending region of that file is decided and already-decided regions are untouched
- [ ] Given a scope-file target, when it is decided, then only that owner's pending regions in that file are decided
- [ ] Given a scope target, when it is decided, then that owner's pending regions are decided across every file it touched, and the affected paths are reported
- [ ] Given an all target, when it is decided, then every file carrying a pending region is decided
- [ ] Given a last-turns target with a count, when it is decided, then the last `count` turns after the accepted frontier are decided, and a count of zero or less decides nothing
- [ ] Given an approval, when it is recorded, then disk is left untouched, because approved content already sits there
- [ ] Given a revert, when it is recorded, then the reconstructed file is written to disk immediately and dependent later regions are dragged
- [ ] Given a disk write that fails during a decision, when the failure is handled, then the log is rolled back to its prior state and the call answers `invalid_params` with the failure
- [ ] Given an active turn, when any review mutation is attempted, then it is refused, and the refusal is the reference's idle requirement rather than a new one
- [ ] Deciding is an acting boundary: manual drift is reconciled before the decision lands

#### US-135: Answer baseline, turn diff and hunks with owner scoping

**Description:** As an editor integration author, I want the accepted baseline, a single scope's diff and the anchored hunks so that the panel can render an inline control per change.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-133
**Reference:** [vibe/core/review/manager.py:262](/home/arthur/dev/mistral-vibe/vibe/core/review/manager.py) for `baseline_text`, line 269 for `scope_file_diff` and line 291 for `file_hunks`, with the wire projection at [vibe/app_server/_review.py:87](/home/arthur/dev/mistral-vibe/vibe/app_server/_review.py) through line 113

**Acceptance Criteria:**
- [ ] Given a path, when `review/baseline` is called, then it answers the decoded text of the accepted baseline, holding kept regions only
- [ ] Given a path with no kept region, when `review/baseline` is called, then it answers the original text, and an absent baseline answers an empty string rather than failing
- [ ] Given a path and an owner, when `review/turnDiff` is called, then it answers that owner's own change: its kept regions as the baseline against its kept plus pending regions as the current
- [ ] Given an owner that did not touch the path, when `review/turnDiff` is called, then it answers `modified` with both sides empty
- [ ] Given either side binary or undecodable, when `review/turnDiff` is called, then the status is `binary_or_undecodable` and both texts are empty
- [ ] Given a baseline that is absent, when `review/turnDiff` is called, then the status is `created`; a current that is absent yields `deleted`
- [ ] Given a path and no owner, when `review/hunks` is called, then the anchors come from the whole-file diff of the accepted baseline against disk
- [ ] Given a path and an owner, when `review/hunks` is called, then the anchors come from that scope's diff
- [ ] Each anchor carries its side, its zero-based line and the region references it decides together
- [ ] `review/hunks` reconciles before anchoring, so whole-file anchors line up with the content the panel renders
- [ ] All six review responses validate against the app-server census with 0 missing required fields and 0 surplus aliases

---

### EP-040: Rewind on Entry Identity and the Scorecard

Address rewind by the stable entry identity the protocol declares, answer in the declared shapes, migrate the one live caller, and remeasure.

**Definition of Done:** Both rewind methods speak `entryId`, both responses validate against the census, the TUI picker works against the new shapes, and `docs/parity.md` carries the remeasured scores and every accepted divergence.

#### US-136: Answer rewind read by entry identity

**Description:** As an operator, I want to ask whether rewinding to a given history entry would change files so that the picker can warn me before I commit to it.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-131
**Reference:** [vibe/core/rewind/manager.py:39](/home/arthur/dev/mistral-vibe/vibe/core/rewind/manager.py) for `restorable_paths_at` and `has_file_changes_at`, answered by [vibe/app_server/_handler.py:715](/home/arthur/dev/mistral-vibe/vibe/app_server/_handler.py) for `_rewind_read` and line 769 for `_rewind_index`, which resolves an entry identifier through `history_user_message_index`

**Acceptance Criteria:**
- [ ] Given a session and an entry identifier, when `session/rewind/read` is called, then it answers exactly `hasFileChanges` and `paths`
- [ ] Given an entry whose restore plan matches disk on every path, when it is read, then `hasFileChanges` is false and `paths` is empty
- [ ] Given an entry whose restore plan differs on some paths, when it is read, then `paths` lists exactly those and `hasFileChanges` is true
- [ ] Given an entry identifier no rewindable user entry carries, when it is read, then the call answers `not_found` naming the entry
- [ ] Given a session with no engine attached, when it is read, then `hasFileChanges` is false and `paths` is empty rather than failing
- [ ] The TUI rewind picker sources its entry list from `history/list` and no longer reads `messages` from `session/rewind/read`
- [ ] The response validates against the census with 0 surplus fields

#### US-137: Rewind by entry identity in the declared response shape

**Description:** As an operator, I want rewinding to target a stable entry and report what it restored so that a rewind after a compaction lands where I pointed.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-136
**Reference:** [vibe/core/rewind/manager.py:75](/home/arthur/dev/mistral-vibe/vibe/core/rewind/manager.py) for `rewind_to_message` with its fork and in-place strategies, line 122 for `_truncate` and line 130 for `_on_messages_reset`, which is where a mid-compaction turn is reopened. The handler is [vibe/app_server/_handler.py:723](/home/arthur/dev/mistral-vibe/vibe/app_server/_handler.py)

**Acceptance Criteria:**
- [ ] Given a session and an entry identifier, when `session/rewind` is called, then it resolves that entry to its message index and answers `message`, `restoreErrors`, `restoredPaths`, `state` and `sessionLog`
- [ ] Given `restoreFiles` true, when the rewind runs, then the restore plan is applied and `restoredPaths` lists the paths whose content actually changed
- [ ] Given a restore where some paths could not be written, when the rewind answers, then `restoreErrors` carries one entry per failure and the rewind still completes for the rest
- [ ] Given `restoreFiles` false, when the rewind runs, then no file is touched and both lists are empty
- [ ] Given `inplace` true, when the rewind runs, then the truncated history is persisted under the same session and no new session is created
- [ ] Given `inplace` false, when the rewind runs, then the full history is saved first and the session forks, preserving the original as a parent
- [ ] Given a rewind, when the log is truncated, then the message-list reset does not also clear the log, because the rewind's own truncation owns it
- [ ] Given a message-list reset that is not a rewind, such as a clear or a compaction, when it happens, then the log is cleared, and when a turn was open a fresh turn is opened so the remaining tool loop keeps recording
- [ ] Given an entry identifier no rewindable user entry carries, when the rewind is called, then it answers `not_found` naming the entry and nothing is truncated or restored
- [ ] The TUI rewind command sends `entryId` and no longer sends `keepMessages`
- [ ] The response validates against the census with 0 missing required fields

#### US-138: Bound the log, ledger the divergences and remeasure the scorecard

**Description:** As a parity maintainer, I want the remaining divergences named and the score remeasured so that the record says what is proven and what is decided.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-134, US-135, US-137
**Reference:** `docs/parity.md` in this repository, and [vibe/core/checkpoints/checkpointer.py:56](/home/arthur/dev/mistral-vibe/vibe/core/checkpoints/checkpointer.py) where the reference log is constructed unbounded, which is what the retention ceiling diverges from

**Acceptance Criteria:**
- [ ] Given a session log, when its retained file bytes would exceed the ceiling, then further capture is refused, a warning is published, and no event is silently dropped
- [ ] The previous silent truncation at 64 checkpoints is gone, and a test asserts that a log is never shortened behind the caller's back
- [ ] Given the ceiling reached, when a restore plan is requested, then it either answers correctly or reports that the cut is beyond what the log retains, never a plan built from a truncated log
- [ ] `docs/parity.md` records the remeasured Checkpoints score with the command that produced it, and updates Review and turn diff and Sessions and rewind from the same run
- [ ] Every divergence that remains is a row in the accepted-divergences table naming why it stands and what in the repository holds it in place, including the retention ceiling and the narrower codec set
- [ ] `CHANGELOG.md` records the user-visible change under `## Unreleased`
- [ ] The execution-order table marks rank 9 with its status and the PRD that delivered it
- [ ] Running the full CI sequence from the workspace root passes with no filtered test invocation

---

## Functional Requirements

- FR-01: The system must resolve every read from a single append-only log of turn marks, edits and decisions, and must never mutate that log while reading it.
- FR-02: The system must identify every region by a version index equal to its producing edit's sequence number and an ordinal equal to its position in that edit's opcode list, and that identity must not change as the log grows.
- FR-03: The system must compute regions from an opcode sequence identical to the reference's, with the popular-element heuristic disabled.
- FR-04: The system must record, for each region at append time, the earlier regions it was built on, and must never recompute them afterward.
- FR-05: When a region is reverted, the system must treat every region depending on it as reverted, derived at read rather than stored.
- FR-06: When a region is kept, the system must also keep the pending regions it depends on.
- FR-07: The system must not offer an un-revert: a region recorded as reverted stays reverted until the log is truncated or a fresh edit supersedes it.
- FR-08: The system must refuse any review decision while a turn is open, and must refuse to begin a turn while one is open.
- FR-09: The system must capture a change made outside a turn as a manual edit owned by its own permanent slot, and must do so idempotently.
- FR-10: The system must re-encode a reconstructed file with the codec and line ending its reference state carried.
- FR-11: The system must treat a binary side, an absent after state, or an existence toggle with no textual difference as one whole-file unit rather than as line regions.
- FR-12: The system must persist a revert to disk immediately and must leave disk untouched on an approval.
- FR-13: The system must roll the log back when persisting a decision fails, so a decision is never committed against a file that was not changed.
- FR-14: The system must answer `review/state`, `review/baseline`, `review/hunks`, `review/turnDiff`, `review/approve` and `review/revert` from the session's engine and must NOT hold review state anywhere else.
- FR-15: The system must address `session/rewind` and `session/rewind/read` by entry identifier and must answer in the shapes the app-server census records.
- FR-16: The system must clear the log on a message-list reset that is not a rewind, and must reopen a turn when one was open at the time.
- FR-17: The system must not silently drop log events; when a retention ceiling is reached it must refuse capture and report it.

## Non-Functional Requirements

- **Correctness:** 0 divergent opcode tuples over the committed opcode corpus, and 0 divergent fields over the committed engine corpus outside a named ledger. Both counts are printed by their replay command.
- **Performance:** projecting a 5 000-line file carrying 200 applied regions completes in under 50 ms in a release build. `review/state` over 50 tracked files and 200 sealed turns answers in under 300 ms. Computing dependencies for one edit against a 5 000-line projection completes in under 20 ms.
- **Memory:** a session log retains at most 512 MiB of file bytes; reaching that ceiling refuses further capture and publishes one warning rather than dropping events. A session tracking 100 files of 100 KiB across 200 turns stays under 256 MiB.
- **Determinism:** two runs of either replay over the same corpus produce byte-identical reports, and the engine performs no hash-order-dependent iteration over region identifiers.
- **Reliability:** a restore that fails partway rolls every already-restored path back, and reports both the cause and the rollback outcome. One unreadable file at seal time never prevents another file's change from being recorded.
- **Security:** every path the engine reads or writes resolves inside the workspace root through the existing confinement, with 0 escapes via symlink or `..` traversal, asserted by a test per surface.
- **Compatibility:** a missing or off-pin reference checkout adds 0 failures to `cargo test --workspace --all-features`.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Empty review state | Session with no tracked file | `review/state` answers empty files and empty scopes | none |
| 2 | Fully decided file | Every region kept or reverted | File drops out of `review/state` entirely | none |
| 3 | Decision during a turn | Client sends `review/approve` while a turn runs | Refused with `invalid_params`, log unchanged | "Review decisions are not available while a turn is running." |
| 4 | Unknown region | Client sends a `(versionIndex, ordinal)` the file does not carry | Refused with `invalid_params` naming path and region | "That change is no longer part of this file." |
| 5 | Disk write fails on revert | Read-only file or full disk during persistence | Log rolled back, call fails, disk left as found | "The revert could not be written, so it was not applied." |
| 6 | Unreadable file at seal | Permission removed mid-turn | That path is skipped and logged, every other path still seals | none |
| 7 | Manual edit between turns | User edits a file outside the agent | Captured as its own owner with its own review slot | none |
| 8 | Manual edit during a turn | User edits while the agent runs | Not captured as manual; that disk belongs to the turn | none |
| 9 | Binary file changed | Agent rewrites a file containing a NUL | One opaque region, reason `binary_or_undecodable`, revertible as a unit | none |
| 10 | File deleted by the agent | Agent removes a tracked file | One opaque region, reason `missing`, revert restores the bytes | none |
| 11 | Empty file created | Agent creates a file with no content | Opaque existence toggle rather than an empty region list | none |
| 12 | File above the heuristic threshold | Any file of 200 lines or more with a repeated line | Opcodes still match the reference, because the heuristic is disabled | none |
| 13 | Reused turn identifier | Transcript reset reuses an identifier with a turn still open | Newest exact mark wins when resolving the index | none |
| 14 | Compaction mid-turn | Message list resets while a turn is open | Log cleared and a fresh turn opened so the tool loop keeps recording | none |
| 15 | Rewind to an unknown entry | Client sends an entry identifier that is not a rewindable user entry | `not_found`, nothing truncated, nothing restored | "That point in the conversation is no longer available." |
| 16 | Retention ceiling reached | Session accumulates more than 512 MiB of tracked bytes | Capture refused, one warning published, no event dropped | "File history for this session has reached its size limit; new changes are no longer tracked." |
| 17 | Reference checkout absent | Parity suite run on a machine without the reference | Committed corpora replay; only the recapture probe skips | none |
| 18 | Two turns editing one file | Concurrent tool calls on the same path | The existing per-path write lock serializes them, and the first touch owns the turn's pre state | none |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | The opcode port diverges from CPython on inputs the corpus does not cover, so region identities silently differ in production | Medium | High | US-121 captures at least 30 fixtures chosen for the algorithm's known inflection points, including above-threshold and repeated-line cases; the replay is unconditional and runs on every `cargo test` |
| 2 | The `difflib` crate is adopted later as a shortcut, reintroducing the unconditional heuristic | Low | High | US-120 carries an explicit criterion that no dependency is added, and the opcode replay would fail on any above-threshold fixture if the heuristic returned |
| 3 | Python's `splitlines` boundary set is wider than expected, so line spans drift on files with exotic control characters | Medium | Medium | US-119 tests each of the eight boundary characters by name; the engine corpus includes a fixture carrying them |
| 4 | The unbounded log grows past what a long session can hold | Medium | High | 512 MiB ceiling with refusal and a published warning rather than silent truncation, asserted by US-138 |
| 5 | Deleting the `ResourceService` review stub breaks tests written against it | High | Low | Those tests are the stub's only consumers; US-133 removes them with it, and the census validation replaces the coverage they provided |
| 6 | Migrating `session/rewind` to `entryId` breaks the TUI picker, which reads `messages[]` from `session/rewind/read` at `crates/vibe-cli/src/tui/pickers.rs` | High | Medium | US-136 and US-137 each carry a criterion migrating the picker and the command in the same story, and `history/list` is already routed as the replacement source |
| 7 | Scope: 20 stories and 77 points in one PRD, at the stated limit | Medium | Medium | Epics are dependency-ordered so each is independently shippable; EP-036 through EP-038 deliver a measured engine even if EP-039 and EP-040 slip, and EP-040 is P1 |
| 8 | Reference prose leaks into the corpus through an error message or a docstring | Low | High | The corpus schema commits structural values and content digests only, and US-132 carries an explicit criterion that no reference-authored prose is stored |
| 9 | `review/turnDiff` and `review/hunks` cannot be census-validated because a bare probe session reaches neither | Medium | Medium | Validate them from a repository fixture with a written session, the way the nine `projectLinks/*` answers are already validated |
| 10 | The engine's per-read recomputation makes `review/state` too slow on a long session | Medium | Medium | Memoize per read model instance as the reference does, and hold the NFR at 300 ms with a benchmark test |

## Non-Goals

Explicit boundaries. What this version does NOT include:

- **A CLI review panel.** The reference ships no review UI in `vibe/cli` either, only `diff_rendering.py`. The review surface is for editor clients and ACP. Revisit if a TUI review mode is requested separately.
- **Persisting the checkpoint log across sessions.** The reference holds it in memory for the session's lifetime and clears it on reset. Durable checkpoints are a different feature with a different contract.
- **Widening the codec set.** `edit` recognizes the byte-order mark, UTF-8 and Latin-1 as the fallback that always decodes; the reference additionally tries the locale codec and a statistical detector. This is already a recorded divergence in `docs/parity.md` and stays one.
- **Reproducing warning and applied-edit message wording.** `NOTICE` forbids shipping upstream prose. Messages are held to naming the same cause, value and limit.
- **Re-pinning the reference.** A re-pin regenerates all corpora and is its own change.
- **`mcp/authUrl`, automatic compaction, or any other rank.** Rank 9 only.
- **Changing `[workspace.package] version`.** No release is cut by this work.

## Files NOT to Modify

- `crates/vibe-core/src/parity.rs` and `scripts/parity/pin.py` - the two pin sources, held equal by a test. Editing either is a re-pin, which is out of scope.
- `crates/vibe-app-server/tests/app-server-surface/corpus.json` - the wire census this work validates against. It is regenerated only by its own capture script, and a review model changing here would mean the reference changed.
- `crates/vibe-core/tests/config-surface/corpus.json` and `crates/vibe-app-server/tests/tool-surface/baseline.json` - unrelated corpora; a diff here means something went wrong.
- `crates/vibe-protocol/src/lib.rs` `SERVER_METHODS` - the six review methods and the two rewind methods are already declared and routed. This work changes their implementations, not the inventory.
- `crates/vibe-core/src/workspace.rs` `Workspace` core (`confined`, `atomic_replace`, `write_lock`) - the confinement and locking every file tool depends on. The engine consumes it as a port; it does not reshape it.
- `NOTICE` - the licensing boundary this PRD is written under.

## Technical Considerations

Framed as questions for engineering input, not mandates:

- **Architecture:** where does the engine live? Recommended: a new `crates/vibe-core/src/checkpoints/` module holding the pure log and read model with no filesystem dependency, and the impure shells reaching disk through the existing `Workspace`. This keeps the reference's pure-core split, which is what makes the 80 reference test functions possible without a filesystem. Engineering to confirm the module boundary against `dependency-layers`.
- **Diff algorithm:** first-party port or dependency? Recommended: first-party, roughly 250 lines, junk heuristic permanently off. Evidence: `difflib` 0.4.0 applies the heuristic unconditionally with the filter inverted relative to CPython (`src/sequencematcher.rs:128-136`), was last released 2018-07-22, and would need the same corpus verification anyway. Alternative if the port proves harder than estimated: vendor the algorithm behind the same trait and swap later, since the corpus makes the swap safe.
- **Data model:** how is the log keyed? Recommended: a `Vec<Event>` with a monotonic sequence, matching the reference, since `version_index` is that sequence and a map would lose the list order that `_applied_before` depends on. Trade-off: a captured drift edit can carry a higher sequence than the turn mark it was inserted before, so list order and sequence order are not the same relation and both are needed.
- **Memoization:** the reference builds a fresh `History` per read and memoizes inside it. Recommended: mirror that, with the caches owned by the read model rather than the log, so a stale cache is structurally impossible. Engineering to confirm the 300 ms NFR is reachable without cross-read caching.
- **Wire projection:** where does the `Review*` projection live? Recommended: `vibe-app-server`, next to the other projections, with the engine exposing domain types only. This mirrors the reference, where `_review.py` does the mapping and `vibe/core/review/manager.py` stays protocol-free.
- **Rewind entry identity:** what is the entry identifier here? Recommended: the `id` on `PublicEntryMetadata` that `history/list` already publishes, resolved to a message index the way the reference resolves it. Engineering to confirm the identifier is stable across a fork and a compaction.
- **Migration:** no persisted data carries a region identity today, so no migration is expected. Backward compatibility for `session/rewind`: the `messageIndex` parameter is a local divergence with one in-tree caller, so it is replaced rather than deprecated. Rollback plan: the change is additive in the engine and confined to two dispatch functions on the wire.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| `docs/parity.md` Checkpoints score | 50 | 100 | Month-1 | `cargo test -p vibe-core --all-features checkpoint_parity_tests -- --nocapture` |
| `docs/parity.md` Review and turn diff score | 80 | 95 | Month-1 | Same run plus the app-server census |
| `docs/parity.md` Sessions, resume, fork, history score | 80 | 90 | Month-1 | App-server census over the two rewind methods |
| Weighted parity total | 76 | 80 | Month-1 | `docs/parity.md` recomputation |
| Engine scenarios replayed | 0 | 40 or more, 0 divergent outside the ledger | Month-1 | Replay output |
| Opcode fixtures replayed | 0 | 30 or more, 0 divergent | Month-1 | Replay output |
| `Review*` models emitted with real values | 3 of 25 | 25 of 25 | Month-1 | App-server census over a written session |
| Review targets honored | 2 of 7 behaviors | 7 of 7 | Month-1 | One integration test per target |
| Production `review/state` non-empty on a session with changes | Never | Always | Month-1 | Integration test driving a turn then reading state |
| Reference test functions with a Rust counterpart | 0 of 80 | 80 of 80 covered by a corpus scenario or a named test | Month-6 | Scenario inventory cross-check in US-132 |

## Open Questions

- Is the `PublicEntryMetadata.id` published by `history/list` stable across a fork and across a compaction? Engineering to confirm before US-136 lands, since the whole rewind addressing rests on it. If it is not, US-136 gains a story to make it stable.
- Should the 512 MiB retention ceiling be configurable? Product and engineering to decide by US-138. A configuration key would need a corpus entry in the configuration surface, which is a separate PRD's instrument; a constant needs none.
- Does any ACP client depend on the current `session/rewind/read` shape? Engineering to confirm before US-136, by checking whether `vibe-acp` forwards it. The audit found no forwarding, but absence of a caller in this repository does not prove absence in a consumer.
- Does the reference ever emit an anchor whose target set spans more than one file? Reading suggests not, since anchors are computed per path, but the corpus should record the answer rather than the reading. To be settled by the US-132 capture.
[/PRD]
