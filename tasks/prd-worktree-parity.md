[PRD]
# PRD: Worktree at Full Parity

**Reference root:** every `vibe/...` path in this document is relative to the
read-only Python checkout at `/home/arthur/dev/mistral-vibe/`
(`C:\dev\mistral-vibe` on Windows, `VIBE_REFERENCE` overrides both), read at
commit `b78b451` and never from its working tree. See `## Reference Map` for the
read commands and the full symbol table.

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-22 | Arthur Jean | Initial draft: take parity row 5 from 90 to 100 |

## Problem Statement

Row 5 of `docs/parity.md` ("Worktree (`--worktree`)") scores 90. Its "State and
gaps" cell reads `startup/worktree.rs, full create/reuse/cleanup/branch
lifecycle` and stops. It carries no restatement date, no oracle, and no entry in
`## Open divergences`. Ten points sit on the scorecard with nothing behind them,
and the claim they rest on is wrong in three places that a reader cannot see.

The first thing a differential read finds is that the row has never been
measured. `scripts/parity/` holds twenty capture scripts and none of them names
a worktree. The startup corpus carries one trace,
`worktree-before-trust-and-session`
(`crates/vibe-cli/tests/runtime-parity/startup.json:34`), and that trace proves
nothing about worktrees: `crates/vibe-cli/tests/runtime-parity/startup-oracle.py:95`
drives `vibe.cli.cli.run_cli`, while the whole worktree lifecycle lives in
`vibe/cli/entrypoint.py:293-304`, one frame above it. The trace shows that
argparse accepts the flag and that routing is unaffected by it. Row 5 is
therefore the only row below 100 whose score is pure assertion.

The second thing it finds is a defect that destroys user work. This port counts
new commits with `{base_commit}..{branch}`
(`crates/vibe-cli/src/tui/startup/worktree.rs:385`) where the reference counts
`{base_commit}..HEAD` (`vibe/core/worktree.py:276`), and the reference's own
docstring states why: "Commit counts intentionally use the worktree's current
HEAD instead of the named branch so detached-HEAD commits still block cleanup"
(`worktree.py:264-267`). Measured on a throwaway repository with one commit made
on a detached HEAD inside the worktree:

```
count base..review = 0      # what worktree.rs:385 reads
count base..HEAD   = 1      # what worktree.py:276 reads
status             = (empty)
```

`WorktreeCleanupState::is_clean` therefore returns true, no prompt is shown,
`git worktree remove --force` runs, `git branch -D` follows, and the commit is
gone. This is the only divergence on the row that loses work rather than
changing wording.

The third is that half of the reference's worktree surface has no counterpart at
all. `list_linked_worktrees` (`worktree.py:156`) does not exist here, and it is
not only the backing of a JSON-RPC method: `vibe/app_server/server.py:1267` uses
it to validate an `existing` local workspace selection. `SessionStartParams` is
`#[serde(deny_unknown_fields)]` with no `local_workspace_selection` field
(`crates/vibe-app-server/src/server/wire.rs:31`), so a desktop client that sends
the field the census already records gets `invalid_params`, and no test sees it:
`every_probed_response_validates_against_the_census` validates responses, never
inbound parameters. `CreateLocalWorkspaceSelection` requires `branch` and `name`
as separate fields (`vibe/app_server/protocol.py:207-210`), which is exactly the
`branch != name` case `build_prepared_worktree` cannot express: it assigns
`branch: name.to_owned()` (`worktree.rs:317`).

Under that sit four smaller families, all verified:

1. **Validation.** The reference rejects sixteen name shapes this port accepts.
   Driving `_is_portable_worktree_name` (`worktree.py:307`) directly returns
   false for `aux`, `AUX`, `aux.txt`, `con`, `nul`, `com1`, `lpt9`, `foo.`,
   `foo `, `a<b`, `a|b`, `a"b`, `a?b`, `a*b`, `a:b` and a name holding a tab.
   `validate_worktree_name` (`worktree.rs:401`) accepts all sixteen. It has no
   `git check-ref-format --branch` gate either (`worktree.py:319`), so `-x`,
   `foo..bar`, `foo.lock` and `HEAD`, all four rejected by git and verified as
   such, reach `git worktree add` and surface as raw git stderr after the
   managed directory has already been created. In the other direction,
   `--worktree ""` is falsy upstream and starts a normal session
   (`entrypoint.py:293`); here it is a fatal `InvalidWorktreeName`.
2. **Hardening.** `_cleanup_failed_prepare` (`worktree.py:210`) removes the
   worktree and deletes the branch when construction fails after `worktree add`
   succeeded; nothing here does. `_has_linked_path_component`
   (`worktree.py:414`), the four checks in `_target_cwd` (`worktree.py:429-454`)
   and the `-z`-to-newline fallback on git usage error 129
   (`worktree.py:470`) have no counterpart. `_primary_worktree_root`
   (`worktree.py:516`) asks whether it is looking at the primary checkout and
   raises for a linked worktree on a separate git directory; `worktree.rs:242`
   always takes `common_git_dir.parent()`, which is the wrong repository root
   under `--separate-git-dir` and makes the cleanup's `git -C repo_root worktree
   remove` fail. `git_status` (`worktree.rs:457`) reports any non-zero exit as
   "branch does not exist", where `_branch_exists` (`worktree.py:327`)
   discriminates status 1 from a real failure.
3. **Lifecycle text and streams.** `Preparing worktree {name!r}...`
   (`entrypoint.py:296`) and `Removing worktree: {root}` (`entrypoint.py:247`)
   are never printed here. The commit reason reads `added to the branch during
   this session` (`worktree.rs:53`) against `added during this session`
   (`worktree.py:105`). The preparation error is the one line in the block the
   reference sends to stdout rather than stderr (`entrypoint.py:300`, the only
   `rprint` there with no `file=`), and it goes to stderr here. `--worktree` has
   no help text (`crates/vibe-cli/src/lib.rs:96`) against three sentences
   upstream (`entrypoint.py:138-140`).
4. **When cleanup is offered.** The reference offers it when `run_cli` returned
   or exited with code 0 or None (`entrypoint.py:344`), and two of its paths
   reach that without a session: the startup update prompt answered "quit"
   (`vibe/cli/cli.py:296`) and the `KeyboardInterrupt`/`EOFError` handler
   (`cli.py:424-426`). This port requires a constructed runtime
   (`crates/vibe-cli/src/tui/interactive.rs:504`, `crates/vibe-cli/src/main.rs:76`),
   so a worktree created before an interrupted startup survives silently. In the
   other direction, `cleanup_terminal` reopens `/dev/tty` when stdin is not a
   terminal (`worktree.rs:147`) and asks anyway, where `input()` raises
   `EOFError` and declines (`entrypoint.py:196`), so a piped invocation prompts
   here and keeps the worktree there.

Two adjacent contracts are missing with them: the sentence appended to the
no-previous-sessions error when the working directory sits under the managed
root (`vibe/app_server/_runtime.py:643-647`), which has no counterpart around
`StorageError::NoSessions` (`crates/vibe-core/src/storage.rs:1139`,
`crates/vibe-app-server/src/workspace.rs:799`), and the worktree semantics the
reference states to the agent in its `vibe` skill (`vibe/core/skills/builtins/vibe.py:642`),
reduced here to the flag's name in a list (`crates/vibe-core/src/skills/assets/vibe.md:110`).

## Overview

The work is instrument-first, in the shape rows 3 and 4 established, and it
begins with a move the layering forces. The whole worktree contract lives in
`crates/vibe-cli/src/tui/startup/worktree.rs`, which is layer 3, while
`vibe-app-server` at layer 2 is where the session methods that need it run. The
contract moves to `crates/vibe-core/src/worktree.rs` unchanged in behavior, and
`vibe-cli` keeps only the flag handling that belongs to an adapter.

A new oracle, `scripts/parity/worktree.py`, drives the reference's own
`prepare_worktree_session`, `list_linked_worktrees`,
`inspect_worktree_for_cleanup`, `_is_portable_worktree_name`, `_validate_branch`
and `_target_cwd` over scripted git repositories built under a hermetic git
environment with pinned commit dates, so the same case produces the same commit
hash twice. The committed corpus records verdicts, normalized paths and
structural observations, never a reference sentence in cleartext. A replay in
`crates/vibe-core/src/worktree/worktree_parity_tests.rs` holds this port to it
with an audited ledger and a case floor, so a worktree divergence fails
`cargo test` instead of aging into a wrong score.

With the instrument in place, four behavioral epics land. The commit count moves
to `HEAD` so a detached commit blocks cleanup, a failed prepare rolls back, the
primary checkout is resolved the way the reference resolves it, and a branch
probe stops swallowing git failures. Name validation gains the portable-filename
rule and the `check-ref-format` gate, the target working directory gains its
four guards, and the managed root is resolved and confined the way
`_worktree_root` confines it. `list_linked_worktrees` is written, which lets
`workspace/worktrees/list` be routed and lets `localWorkspaceSelection` be
accepted on `session/start` and refused on resume and continue, with a created
worktree cleaned up when startup fails afterward. The terminal lifecycle regains
its two missing lines, its stream assignment, its wording and its exit-code gate.

The last epic restates the row from the widened measurement and records, by name,
the two things that will not be ported: this port never changes the process
working directory, so the reference's guard against removing the directory it is
standing in has no counterpart, and the reference renders its lifecycle lines
through `rich` markup that ratatui and a plain stderr writer cannot reproduce as
cells.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Row 5 score in `docs/parity.md` | 100, restated from a measurement | 100, still measured on every CI run |
| Worktree cases replayed against the reference | 60 or more, 0 unledgered divergences | Same, with the floor raised as cases are added |
| Reference worktree functions with an executable oracle | 6 of 6 public entry points | 6 of 6 |
| Ways to lose a commit through `--worktree` | 0 | 0 |
| Unexplained points on row 5 | 0 | 0 |

## Target Users

### Contributor working in a throwaway branch

- **Role:** whoever runs `vibe --worktree fix-123` to keep an experiment off the
  checkout they are standing in.
- **Behaviors:** creates a worktree, lets the agent edit inside it, sometimes
  commits by hand, sometimes checks out a specific revision to compare, then
  quits and answers the cleanup prompt.
- **Pain points:** a commit made while HEAD is detached inside the worktree is
  invisible to the cleanup check and is deleted without a prompt; a worktree
  created before an interrupted startup is never offered for removal and
  accumulates under `$VIBE_HOME/worktrees`; a name like `aux` or `foo.lock` is
  accepted and then fails inside git with a message that names neither the flag
  nor the rule.
- **Current workaround:** commit and push before quitting, and never detach.
- **Success looks like:** every commit made inside the worktree blocks the
  removal until it is answered for, and an invalid name is refused before
  anything is created.

### Editor or desktop client opening a session in a worktree

- **Role:** an ACP or app-server client that lists the worktrees of a project
  and starts a session inside one.
- **Behaviors:** calls `workspace/worktrees/list`, shows the result, then calls
  `session/start` with `localWorkspaceSelection`.
- **Pain points:** the list method is declared and unrouted, so the call is a
  method-not-found; the selection field is rejected by `deny_unknown_fields`, so
  starting a session in a worktree fails with `invalid_params`; creating one
  through the desktop needs `branch` and `name` to differ, which this port
  cannot express.
- **Current workaround:** none. Both calls fail, and the client has no second
  path to a worktree session.
- **Success looks like:** the same two calls that work against the Python
  app-server work here, and a failed startup does not leave a worktree behind.

### Reader of the scorecard

- **Role:** anyone deciding whether this port can replace the reference for a
  worktree-based workflow.
- **Behaviors:** reads `docs/parity.md` row by row, trusts a score only when the
  row names how it was measured.
- **Pain points:** row 5 says 90, names no gap, names no oracle, and carries no
  restatement date, so the reader cannot tell whether the missing ten points
  cost them a commit or a help string.
- **Current workaround:** treat the number as noise.
- **Success looks like:** row 5 says 100, names the oracle and the command that
  reproduces it, and the two permanent divergences are in the accepted table
  with their reasons.

## Research Findings

Research for this PRD was a differential read of the reference checkout at the
pin plus two direct measurements against `git` itself, not a market survey: the
only comparable product is the reference, and it is readable in full. Web
research was not run and no library documentation was needed; every dependency
involved is already in the workspace, and `git` is invoked as a subprocess here
exactly as it is there.

### Competitive Context

- **Mistral Vibe (Python reference, v2.24.0 at `b78b451`)**: the behavioral
  oracle. It concentrates the whole contract in one module, `vibe/core/worktree.py`,
  536 lines, and three consumers read it: the terminal entrypoint, the
  app-server session methods and the app-server host methods. Its design choice
  worth naming is that the module is provider-neutral and sits under `core/`,
  which is what lets both the CLI and the app-server use it. This port put the
  same contract in the CLI, which is why the app-server half is missing rather
  than merely incomplete.
- **Market gap:** none. This is parity work with a single, fully readable
  reference.

### Measurements taken for this PRD

- Detached-HEAD commit counting, on a throwaway repository with a linked
  worktree and one commit made after `git checkout --detach`:
  `rev-list --count base..review` is 0, `rev-list --count base..HEAD` is 1, and
  `status --porcelain --untracked-files=all` is empty. The port's `is_clean()`
  is therefore true where the reference's is false.
- `_is_portable_worktree_name` driven directly on the pinned tree over 29 names.
  It rejects the sixteen listed in the problem statement and accepts `-x`,
  `foo..bar`, `foo.lock`, `HEAD`, `review`, `très` and an emoji name.
- `git check-ref-format --branch` on the seven it accepts: `-x`, `foo..bar`,
  `foo.lock` and `HEAD` fail, `review`, `très` and the emoji name pass. That is
  the exact set `_validate_branch` catches and this port does not.

### Best Practices Applied

- Widen the instrument before changing behavior. Row 3 was restated from 92
  after its PRD found the six divergent tools were the six the oracle never
  executed, and row 4 from 95 after the same shape. Row 5 is the extreme case:
  the oracle never executed any of it.
- A parity claim comes from a measurement, and the measurement runs wide enough
  to cover what changed (`AGENTS.md`, "The behavioral oracle").
- A ledger fails both on an undeclared divergence and on an entry that no longer
  reproduces, so a fix cannot leave a stale exception behind (pattern from
  `crates/vibe-app-server/src/tool_execution_parity_tests.rs`).
- Reference-authored prose never enters this repository. A captured error
  sentence is stored as a SHA-256 digest with a structural marker, never as text
  (`NOTICE`, `AGENTS.md` "Licensing boundary").
- A capture that builds git repositories runs them under a hermetic environment,
  because a developer's `~/.gitconfig`, `init.templateDir` or hook set otherwise
  reaches into the corpus.

## Assumptions & Constraints

### Assumptions (to validate)

- A scripted git repository with pinned `GIT_AUTHOR_DATE` and
  `GIT_COMMITTER_DATE`, a fixed identity and fixed file content produces the
  same commit hash on every run, so the corpus can record a real hash rather
  than a placeholder. Evidence: a commit hash is a pure function of tree,
  parents, identity and dates. **Risk: LOW.** Validated by US-269.
- The managed directory name is a pure function of the common git directory
  string, so the naming rule can be captured over synthetic paths and replayed
  without either side owning the same temporary directory. Evidence:
  `worktree.py:349-351` hashes `str(common_git_dir)` and nothing else.
  **Risk: LOW.** Validated by US-269.
- Moving the worktree contract into `vibe-core` is behavior-preserving for the
  CLI, because the seven existing unit tests move with it unchanged. Evidence:
  the module's only non-test dependency on `vibe-cli` is `Arguments`, read in
  two places (`worktree.rs:68`, `:260`). **Risk: MEDIUM.** Validated by US-270.
- Routing `workspace/worktrees/list` is a net improvement to row 17 rather than
  a regression, because the accepted divergence that holds it open describes it
  as deferred work rather than as a decision. Evidence: `docs/parity.md:260`,
  "routing them is app-server parity work rather than compaction work".
  **Risk: LOW.** Validated by US-281.
- `PureWindowsPath` single-segment semantics are reproducible in Rust without a
  Windows-path crate: the rule is one component, no drive letter, no separator
  of either kind, no reserved device name on the part before the first dot, no
  trailing space or dot, and every character printable. Evidence: the 29-name
  probe above separates the rule cleanly. **Risk: MEDIUM.** Validated by US-276.
- Reopening `/dev/tty` was added for a reason that no longer applies once the
  cleanup gate moves to the exit code, because the interactive path always has a
  terminal on stdin unless a prompt was piped in. **Risk: MEDIUM.** Validated by
  US-286.

### Hard Constraints

- `NOTICE` forbids copying reference source, prompt files or tool description
  text. Every corpus that would carry a reference-authored sentence stores a
  digest or a structural marker instead, and any cleartext corpus stays
  gitignored under `.parity/`. The short operational labels this port already
  reproduces (`Keeping worktree:`, `Removed worktree:`, `Kept branch:`,
  `Remove worktree? [y/N] `) stay as they are; the lines this PRD adds are
  written in the same register and nothing longer than a label is copied.
- The reference checkout is read-only and is read at the pin, never from the
  working tree.
- A missing or off-pin reference checkout must never fail `cargo test`: the
  corpus replay runs unconditionally, the live probe skips with a printed reason
  from `vibe_core::parity::off_pin_reason`.
- The pin lives in exactly two places and this PRD does not move it. Every
  corpus written here carries `b78b451c39eab9213393ad2f45908e8562a5c5e7`.
- Layering holds: `vibe-protocol` and `vibe-core` first, `vibe-app-server`
  second, `vibe-cli` and `vibe-acp` third. The worktree contract lands in
  `vibe-core` and nothing above it re-implements a piece.
- `unsafe_code` is forbidden; `panic`, `unimplemented` and `dbg_macro` are
  denied outside tests.
- No capture and no test may write into `$VIBE_HOME`, into the user's git
  configuration, or into the reference checkout. Every one of them sets
  `VIBE_HOME`, `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` to paths under its
  own temporary directory.

## Reference Map

The Python reference is a read-only checkout outside this repository.

- Linux: `/home/arthur/dev/mistral-vibe` (canonical spelling in this document)
- Windows: `C:\dev\mistral-vibe`
- Override: `VIBE_REFERENCE`, and `--reference` over that for capture scripts
- Pin: `vibe_core::parity::REFERENCE_COMMIT` and `EXPECTED_COMMIT` in
  `scripts/parity/pin.py`, both `b78b451c39eab9213393ad2f45908e8562a5c5e7`
  (v2.24.0)

Read at the pin, never from the working tree:

```sh
git -C /home/arthur/dev/mistral-vibe show b78b451:vibe/core/worktree.py
git -C /home/arthur/dev/mistral-vibe archive b78b451 vibe/ | tar -x -C <scratch>
```

**Every line number below is anchored at the pin.** The local checkout may sit
at another revision, where the same symbol has moved. At v2.24.2 this module no
longer exists; see `## Non-Goals`.

**Every `vibe/...` path in this document resolves against that root**, in the
table below and in each story's `Reference:` line alike. The root is spelled in
full once per story so a reader who opens a single story can navigate without
scrolling back here.

| Symbol | Path at the pin | Line | Read by |
|---|---|---|---|
| `WorktreeError`, `WorktreeNotFoundError`, `GitUnavailableError` | `vibe/core/worktree.py` | 26-40 | US-269, US-282 |
| `PreparedWorktree` | `vibe/core/worktree.py` | 43-63 | US-269, US-270 |
| `LinkedWorktree` | `vibe/core/worktree.py` | 66-77 | US-280 |
| `WorktreeCleanupState.reasons` | `vibe/core/worktree.py` | 96-106 | US-286 |
| `prepare_worktree_session` | `vibe/core/worktree.py` | 113-153 | US-269, US-273, US-282 |
| `list_linked_worktrees` | `vibe/core/worktree.py` | 156-183 | US-280 |
| `_create_worktree` | `vibe/core/worktree.py` | 194-207 | US-273 |
| `_cleanup_failed_prepare` | `vibe/core/worktree.py` | 210-220 | US-273 |
| `_build_prepared` and its comment | `vibe/core/worktree.py` | 223-245 | US-272 |
| `inspect_worktree_for_cleanup` | `vibe/core/worktree.py` | 263-285 | US-272 |
| `remove_worktree` | `vibe/core/worktree.py` | 288-297 | US-283, US-287 |
| `_validate_worktree_name` | `vibe/core/worktree.py` | 300-304 | US-276 |
| `_is_portable_worktree_name` | `vibe/core/worktree.py` | 307-316 | US-276 |
| `_validate_branch` | `vibe/core/worktree.py` | 319-324 | US-277 |
| `_branch_exists` | `vibe/core/worktree.py` | 327-335 | US-275 |
| `_common_git_dir`, `_resolve_git_dir` | `vibe/core/worktree.py` | 338-346 | US-279 |
| `_worktree_root` | `vibe/core/worktree.py` | 349-358 | US-279 |
| `_relative_base` | `vibe/core/worktree.py` | 361-368 | US-278 |
| `_validate_existing_worktree` | `vibe/core/worktree.py` | 371-411 | US-274, US-278 |
| `_has_linked_path_component` | `vibe/core/worktree.py` | 414-426 | US-278 |
| `_target_cwd` | `vibe/core/worktree.py` | 429-454 | US-278 |
| `_worktree_records` and the `-z` fallback | `vibe/core/worktree.py` | 464-476 | US-280 |
| `_parse_worktree_records` | `vibe/core/worktree.py` | 486-513 | US-280 |
| `_primary_worktree_root` | `vibe/core/worktree.py` | 516-526 | US-274 |
| `_leave_worktree_if_current_directory` | `vibe/core/worktree.py` | 529-536 | US-287 |
| `--worktree` declaration and help | `vibe/cli/entrypoint.py` | 135-141 | US-284 |
| `_prompt_remove_worktree` | `vibe/cli/entrypoint.py` | 181-201 | US-286 |
| `_prompt_delete_attached_branch` | `vibe/cli/entrypoint.py` | 203-219 | US-286 |
| `_cleanup_worktree_on_exit` | `vibe/cli/entrypoint.py` | 221-255 | US-284, US-286 |
| worktree preparation block | `vibe/cli/entrypoint.py` | 291-304 | US-284 |
| `_run_cli_with_worktree_cleanup` | `vibe/cli/entrypoint.py` | 334-356 | US-285 |
| `run_cli` exit-zero paths | `vibe/cli/cli.py` | 296-297, 424-426 | US-285 |
| implicit trust from the flag | `vibe/cli/cli.py` | 184, 248 | US-284 |
| `worktree_list_response` | `vibe/app_server/_host.py` | 385-408 | US-281 |
| host method entry | `vibe/app_server/_host.py` | 101, 289-292 | US-281 |
| `_resolve_local_workspace` | `vibe/app_server/server.py` | 895-903 | US-282 |
| `_cleanup_local_workspace` | `vibe/app_server/server.py` | 923-936 | US-283 |
| `_reject_local_workspace_selection` | `vibe/app_server/server.py` | 1238-1247 | US-282 |
| `resolve_local_workspace_selection` | `vibe/app_server/server.py` | 1250-1290 | US-282 |
| selection models | `vibe/app_server/protocol.py` | 202-222 | US-282 |
| `WorkspaceWorktreeListParams` and response | `vibe/app_server/protocol.py` | 894-907 | US-281 |
| worktree hint on the no-sessions error | `vibe/app_server/_runtime.py` | 640-648 | US-284 |
| `WORKTREES_DIR` | `vibe/core/paths/_vibe_home.py` | 9 | US-279 |
| `get_vibe_home` | `vibe/utils/paths.py` | 20-23 | US-279 |
| worktree semantics stated to the agent | `vibe/core/skills/builtins/vibe.py` | 642 | US-284 |

## Quality Gates

Run from the workspace root before proposing any commit:

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

`--all-features` is load-bearing: `vibe-app-server`'s `test-fixtures` feature
gates the fixture binary `tests/mcp_stdio_e2e.rs` drives.

For any story that edits `scripts/parity/`, add:

```sh
python3 -m compileall -q scripts/parity/
python3 scripts/parity/worktree.py --check   # re-run must be byte-identical
```

## Epics & User Stories

### EP-086: The worktree contract, measured and reachable

Build the missing instrument and put the contract where both clients can reach
it, before changing any behavior, so every later epic is proven by a corpus that
predates it.

**Definition of Done:** `scripts/parity/worktree.py` drives six reference entry
points over scripted repositories and commits a corpus; the contract lives in
`crates/vibe-core/src/worktree.rs` with `vibe-cli` reduced to an adapter; the
corpus replays inside `cargo test --workspace --all-features` with an audited
ledger and a case floor of 60; a re-run with no change in between is
byte-identical.

#### US-269: Capture the reference worktree contract
**Description:** As a person reading the scorecard, I want the reference's own worktree functions driven over scripted git repositories and their verdicts committed, so that row 5 is compared instead of assumed.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/worktree.py:113-153` for preparation, `:156-183` for enumeration, `:263-285` for cleanup inspection, `:307-316` and `:319-324` for validation, `:429-454` for the target directory, `:349-358` for the managed root naming. Pattern to copy: `resolve_reference`, `extract_pinned_tree`, `reexecute_with_reference_interpreter`, `build_corpus` and `--check` in `scripts/parity/tool_execution.py`.

**Acceptance Criteria:**
- [ ] Given the pinned tree, when the capture runs, then it re-executes itself under the reference interpreter and refuses to run against a checkout at any other commit than `EXPECTED_COMMIT`.
- [ ] Given a scripted repository is built, when git runs, then `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, `GIT_AUTHOR_DATE`, `GIT_COMMITTER_DATE`, the author identity and `init.defaultBranch` are all fixed by the capture, so no developer configuration reaches the corpus.
- [ ] Given the capture runs, when it resolves the managed root, then `VIBE_HOME` points inside its own temporary directory and the user's real `$VIBE_HOME` is neither read nor written.
- [ ] Given a name list authored by this capture, when it runs, then it records the `_is_portable_worktree_name` verdict and the `_validate_branch` verdict for each name, covering at minimum the sixteen the reference rejects on portability and the four `check-ref-format` rejects.
- [ ] Given a preparation case, when it runs, then it records `name`, `branch`, `created`, `branch_created`, `base_commit`, and `root`, `path` and `repo_root` relativized against the case's temporary root, so the corpus is machine-independent.
- [ ] Given the dates and identity are pinned, when the capture runs twice, then the recorded `base_commit` values are identical.
- [ ] Given the managed-root naming rule, when it runs, then it records the rule as an input string and its `repo_root.name` plus twelve-hex-digit output over at least four synthetic common-git-dir paths, so the replay recomputes the function rather than comparing a temporary path.
- [ ] Given a cleanup case, when it runs, then it records `has_uncommitted_changes`, `has_untracked_files`, `new_commit_count`, `is_clean` and `reasons`, and the case list covers clean, uncommitted, untracked, a commit on the branch and a commit made after `git checkout --detach`.
- [ ] Given an enumeration case, when it runs, then it records the ordered `LinkedWorktree` list for a repository holding a primary checkout, two linked worktrees, one detached worktree and one prunable record.
- [ ] Given a target-directory case, when it runs, then it records the `_target_cwd` verdict for a base inside a subdirectory, a base whose directory is missing in the worktree, a base that resolves outside the worktree through a symlink and a path holding a nested `.git`.
- [ ] Given a case raises, when the record is written, then it stores the exception class name and a SHA-256 digest of the message, never the sentence, and the digest carries a `<described>` marker naming its length.
- [ ] Given the reference checkout is absent, when the capture runs, then it exits non-zero naming the expected path and the `VIBE_REFERENCE` override, and writes no partial corpus.
- [ ] Given the capture is run twice with no change in between, when the two corpora are compared, then they are byte-identical.

#### US-270: Move the worktree contract into `vibe-core`
**Description:** As a contributor, I want the worktree contract to live one layer below the CLI, so that the app-server can use the same implementation instead of going without one.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/worktree.py` in full, as the module placement this mirrors: the reference puts the contract under `core/` and its three consumers import it. Pattern to copy: `crates/vibe-app-server/src/projects/git.rs` for a subprocess-git module with its own timeouts and bounded output.

**Acceptance Criteria:**
- [ ] Given the module moves, when it lands, then `PreparedWorktree`, `WorktreeCleanupState`, preparation, inspection and removal live in `crates/vibe-core/src/worktree.rs` and are public.
- [ ] Given the move, when `vibe-cli` is compiled, then `crates/vibe-cli/src/tui/startup/worktree.rs` holds only the `Arguments`-facing adapter: `LaunchWorkspace`, `resolve_additional_directories`, `expand_user_path` and the terminal cleanup wrapper.
- [ ] Given the seven existing unit tests, when the move lands, then each one still exists, exercises the same scenario and passes, moved to whichever crate now owns the code it drives.
- [ ] Given the layering rule, when the workspace is compiled, then `vibe-core` gains no dependency on `vibe-cli` or `vibe-app-server`, and `Arguments` is not referenced from `vibe-core`.
- [ ] Given a caller passes a vibe home directory, when preparation runs, then the directory is a parameter of the core function rather than resolved from CLI arguments inside it.
- [ ] Given the move is behavior-preserving, when the full suite runs unfiltered, then no test outside the moved module changes its result.
- [ ] Given git is absent from `PATH`, when preparation runs, then it returns a typed error naming git rather than an io error surfaced from `Command::new`.

#### US-271: Replay the worktree corpus with a ledger and a floor
**Description:** As a person reading the scorecard, I want the worktree corpus replayed on every test run, so that a worktree divergence fails the build instead of aging into a wrong score.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-269, US-270
**Reference:** none read for this story: it is Rust against a committed corpus. Pattern to copy: `crates/vibe-app-server/src/tool_execution_parity_tests.rs` in full, including its `LEDGER`, its staleness check and its `MINIMUM_CASES` floor; `crates/vibe-cli/src/tui/runtime_parity_tests.rs:46` for the skip shape.

**Acceptance Criteria:**
- [ ] Given the committed corpus, when the replay runs, then it asserts the schema version and that the recorded commit equals `vibe_core::parity::REFERENCE_COMMIT`, failing when either drifts.
- [ ] Given each case family, when the replay runs, then it rebuilds the same scripted repository in Rust under `tempfile` with the same hermetic git environment and compares this port's verdict field by field.
- [ ] Given a difference, when the replay runs, then it fails unless a ledger entry covers that exact field on that exact case.
- [ ] Given the replay completes, when the case count is below 60, then it fails naming the count, so a shrunken corpus cannot pass as a green one.
- [ ] Given a ledger entry whose divergence no longer reproduces, when the staleness check runs, then it fails naming the entry.
- [ ] Given every ledger entry, when the audit test runs, then each names either a story ID in this PRD or the licensing boundary, and no entry is scoped wider than one field on one case.
- [ ] Given an error case, when the replay runs, then it compares the error class the corpus records and never the message, and asserts this port produces an error of the matching category.
- [ ] Given the reference checkout is absent or off-pin, when the live probe runs, then it prints the reason from `vibe_core::parity::off_pin_reason` and returns without failing, and the corpus replay still runs.
- [ ] Given the reference checkout is on-pin, when the live probe runs, then it recaptures into `target/` and asserts the fresh corpus equals the committed one.

---

### EP-087: The four defects in the lifecycle

Correct what the current implementation gets wrong, starting with the one that
destroys work.

**Definition of Done:** a commit made on a detached HEAD blocks cleanup; a
failed preparation leaves nothing behind; the repository root is resolved the way
the reference resolves it; a failing branch probe is an error rather than a
false negative. All four replay from the corpus with no ledger entry.

#### US-272: Count session commits against the worktree HEAD
**Description:** As a contributor working in a throwaway branch, I want every commit I made inside the worktree to block its removal, so that detaching HEAD to compare a revision does not silently cost me a commit.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-271
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/worktree.py:263-285`, in particular the docstring at `:264-267` stating that the current HEAD is used so detached-HEAD commits still block cleanup, and `:223-245` where `base_commit` is taken from the worktree's own HEAD at session start.

**Acceptance Criteria:**
- [ ] Given a worktree whose HEAD is detached and carries one commit made this session, when cleanup inspection runs, then `new_commit_count` is 1 and `is_clean()` is false.
- [ ] Given the same worktree, when the user is prompted, then declining keeps both the worktree and the branch.
- [ ] Given a worktree on its named branch with two commits made this session, when inspection runs, then `new_commit_count` is 2.
- [ ] Given a worktree reset to a commit older than `base_commit`, when inspection runs, then `new_commit_count` is 0 and no error is raised.
- [ ] Given a worktree whose HEAD cannot be resolved, when inspection runs, then it returns a typed error naming the worktree and cleanup is not attempted.
- [ ] Given the corpus, when the replay runs, then all five detached and attached cases match the reference with no ledger entry.
- [ ] Given the change lands, when `CHANGELOG.md` is read, then `## Unreleased` records that a worktree carrying a detached-HEAD commit is now kept until the prompt is answered.

#### US-273: Roll back a worktree whose preparation failed
**Description:** As a contributor, I want a worktree that was created and then failed to finish preparing to be removed with its branch, so that a failed run does not leave a half-built checkout under `$VIBE_HOME/worktrees`.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-271
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/worktree.py:140-153` for the try/except around construction and `:210-220` for `_cleanup_failed_prepare`, including that a failing cleanup attaches a note to the original error rather than replacing it.

**Acceptance Criteria:**
- [ ] Given `git worktree add` succeeded and construction then fails, when preparation returns, then the worktree has been removed with `--force`.
- [ ] Given the branch was created by this preparation, when the rollback runs, then the branch is deleted too.
- [ ] Given the branch existed before this preparation, when the rollback runs, then the branch survives.
- [ ] Given the rollback itself fails, when preparation returns, then the original error is returned with the rollback failure attached to it, and neither is swallowed.
- [ ] Given preparation failed before `git worktree add` ran, when it returns, then no removal is attempted.
- [ ] Given a reused worktree fails validation, when preparation returns, then nothing is removed, because this run did not create it.

#### US-274: Resolve the primary checkout the way the reference resolves it
**Description:** As a contributor whose repository uses a separate git directory, I want the repository root computed from the primary checkout, so that cleanup can find the repository it must run `worktree remove` in.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-271
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/worktree.py:516-526`, including the comment explaining that a `--separate-git-dir` repository reports that directory as the first worktree, and `:371-411` for the existing-worktree validation that shares the resolution.

**Acceptance Criteria:**
- [ ] Given the invocation is inside the primary checkout, when the root is resolved, then it is that checkout's working directory.
- [ ] Given the invocation is inside a linked worktree of an ordinary repository, when the root is resolved, then it is the parent of the common git directory.
- [ ] Given the invocation is inside a linked worktree of a repository using a separate git directory, when the root is resolved, then a typed error names that the primary checkout cannot be determined, and no worktree is created.
- [ ] Given a repository created with `--separate-git-dir`, when a worktree is created and later removed, then the removal succeeds, which it does not today.
- [ ] Given an existing worktree is revalidated, when the common git directory is compared, then both sides are resolved before comparison so a symlinked path does not read as a different repository.
- [ ] Given the corpus, when the replay runs, then the three resolution shapes match the reference with no ledger entry.

#### US-275: Discriminate a missing branch from a failing branch probe
**Description:** As a contributor, I want a git failure while checking whether a branch exists to be reported, so that a broken repository does not silently take the create-a-branch path.

**Priority:** P2
**Size:** XS (1 pt)
**Dependencies:** Blocked by US-271
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/worktree.py:327-335`, where exit status 1 means absent and any other status is raised as a worktree error.

**Acceptance Criteria:**
- [ ] Given `show-ref --verify --quiet` exits 1, when the probe runs, then the branch is reported absent.
- [ ] Given it exits 0, when the probe runs, then the branch is reported present.
- [ ] Given it exits with any other status, when the probe runs, then a typed error is returned naming the branch and carrying git's stderr, and no worktree is created.
- [ ] Given git cannot be spawned at all, when the probe runs, then the same typed error is returned rather than a false negative.

---

### EP-088: What a name and a path are allowed to be

Refuse upstream what the reference refuses, before anything is created on disk.

**Definition of Done:** the name and branch verdicts replay from the corpus with
zero divergences across the full authored name list, the target directory
carries its four guards, and the managed root is resolved and confined.

#### US-276: Refuse a name the reference calls unportable
**Description:** As a contributor, I want an unportable worktree name refused with a message naming the flag, so that a name like `aux` or `foo.` fails before a directory is created rather than inside git.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-271
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/worktree.py:300-316`, including the reserved-name set and the invalid-character set it reads at `:20-24`, and the double `Path` plus `PureWindowsPath` single-segment test at `:316`.

**Acceptance Criteria:**
- [ ] Given a name that is empty, `.` or `..`, when it is validated, then it is refused.
- [ ] Given a name ending in a space or a dot, when it is validated, then it is refused.
- [ ] Given a name holding any of `<`, `>`, `:`, `"`, `/`, `\`, `|`, `?` or `*`, when it is validated, then it is refused.
- [ ] Given a name holding a non-printable character, when it is validated, then it is refused.
- [ ] Given a name whose part before the first dot is a Windows reserved device name in any case, when it is validated, then it is refused, so `aux`, `AUX` and `aux.txt` are all refused.
- [ ] Given a name that is a single segment under both POSIX and Windows path rules, when it is validated, then it is accepted, so `très` and an emoji name pass.
- [ ] Given a refusal, when the message is read, then it names `--worktree NAME` and the single-portable-segment rule, and it is this port's own sentence rather than the reference's.
- [ ] Given a refusal, when preparation returns, then no directory under the managed root has been created.
- [ ] Given the corpus, when the replay runs, then every name in the authored list produces the reference verdict with no ledger entry.

#### US-277: Validate the branch before creating anything
**Description:** As a contributor, I want a name that git will not accept as a branch refused up front, so that `foo.lock` or `HEAD` fails with a message about the branch rather than with raw git stderr after a directory exists.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-276
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/worktree.py:319-324`, which runs `git check-ref-format --branch` against the branch and raises a typed error, and `:113-119` for its position in the sequence: after the name check and before anything else.

**Acceptance Criteria:**
- [ ] Given a branch name `git check-ref-format --branch` rejects, when preparation runs, then a typed error naming the branch is returned.
- [ ] Given that error, when preparation returns, then no directory has been created, no branch exists and no `worktree add` was attempted.
- [ ] Given `-x`, `foo..bar`, `foo.lock` and `HEAD`, when each is validated, then each is refused, and `review` is accepted.
- [ ] Given a branch that differs from the worktree name, when preparation runs, then the branch is what is validated.
- [ ] Given git cannot run the check, when preparation runs, then the error names git rather than reporting the branch as invalid.

#### US-278: Guard the working directory the worktree hands back
**Description:** As a contributor invoking from a subdirectory, I want the directory the session opens in to be inside the worktree and to belong to it, so that a symlink or a nested repository does not silently move the session somewhere else.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-271
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/worktree.py:429-454` for the four checks, `:414-426` for the linked-component walk including its comment on root-level aliases such as macOS `/tmp`, and `:361-368` for the relative base it is given.

**Acceptance Criteria:**
- [ ] Given the relative base does not exist in the worktree, when preparation runs, then a typed error names the path.
- [ ] Given it exists and is not a directory, when preparation runs, then a typed error names it.
- [ ] Given it resolves outside the worktree through a symlink, when preparation runs, then a typed error names the worktree it escaped.
- [ ] Given any directory between the worktree root and the target holds a `.git` entry, when preparation runs, then a typed error reports that the path belongs to a different repository.
- [ ] Given the worktree path holds a symbolic link or a junction below the first component after the anchor, when an existing worktree is validated, then a typed error names it, and a root-level alias alone does not trigger it.
- [ ] Given a base that is the checkout root itself, when preparation runs, then the worktree root is returned unchanged.
- [ ] Given the invocation directory is outside the checkout, when preparation runs, then a typed error names both paths.

#### US-279: Resolve and confine the managed root
**Description:** As a contributor whose `VIBE_HOME` is a tilde path or a symlink, I want the managed worktree root resolved before it is used, so that the directory a worktree lands in is the same one the reference would choose.

**Priority:** P2
**Size:** S (2 pts)
**Dependencies:** Blocked by US-271
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/worktree.py:349-358` for the hash, the directory name and the containment assertion, `:338-346` for how the common git directory is resolved before hashing, and `/home/arthur/dev/mistral-vibe/vibe/utils/paths.py:20-23` for `VIBE_HOME` expansion.

**Acceptance Criteria:**
- [ ] Given `VIBE_HOME` holds a leading tilde, when the managed root is resolved, then it is expanded before use.
- [ ] Given `VIBE_HOME` is a symlink, when the managed root is resolved, then it is resolved to its target before the directory name is built.
- [ ] Given the resolved repository directory would fall outside the managed root, when preparation runs, then a typed error names both paths and nothing is created.
- [ ] Given a common git directory string, when the directory name is built, then it is `{repo_root_name}-{first twelve hex digits of its SHA-256}`.
- [ ] Given the platform is Windows, when the common git directory is canonicalized, then the verbatim `\\?\` prefix is stripped before hashing, so the directory name matches the reference's.
- [ ] Given `--worktree ""`, when the CLI starts, then it is treated as no worktree and a normal session opens, matching `vibe/cli/entrypoint.py:293`.
- [ ] Given the corpus, when the naming rule is replayed over the synthetic paths, then every digest matches with no ledger entry.

---

### EP-089: The half of the contract that does not exist here

Write the enumeration and open the app-server boundary, which is where every
editor client meets this row.

**Definition of Done:** `list_linked_worktrees` exists in `vibe-core` and
replays from the corpus; `workspace/worktrees/list` answers the declared
response shape and its ledger entry is retired in the same change;
`localWorkspaceSelection` is accepted on `session/start` for both kinds and
refused on resume and continue; a session that fails to start after creating a
worktree leaves none behind.

#### US-280: Enumerate the linked worktrees of a checkout
**Description:** As an editor client, I want the linked worktrees of a project enumerated the way the reference enumerates them, so that a worktree picker shows the same list under both clients.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-271, US-274, US-278
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/worktree.py:156-183` for the filter and the ordering, `:464-476` for the `-z` attempt and the fallback on git usage error status 129, `:486-513` for the record parser and its `refs/heads/` stripping, and `:66-77` for the record shape.

**Acceptance Criteria:**
- [ ] Given `git worktree list --porcelain -z` succeeds, when enumeration runs, then records are parsed on the null separator.
- [ ] Given that command exits with git's usage error status 129, when enumeration runs, then it retries without `-z` and parses on newlines.
- [ ] Given it fails with any other status, when enumeration runs, then a typed error is returned naming the failure.
- [ ] Given the record list, when it is filtered, then the first record is skipped as the primary checkout.
- [ ] Given a record with no branch or marked prunable, when it is filtered, then it is excluded.
- [ ] Given a record that fails existing-worktree validation, when it is filtered, then it is excluded rather than failing the whole call.
- [ ] Given a branch value of `refs/heads/topic`, when it is recorded, then the branch reads `topic`.
- [ ] Given several worktrees survive the filter, when the list is returned, then it is ordered by the string form of each worktree's working path.
- [ ] Given the invocation directory is not inside a git repository, when enumeration runs, then a typed not-found error is returned rather than a panic or an empty success.
- [ ] Given the corpus, when the replay runs, then the ordered list for a repository holding two linked worktrees, one detached and one prunable matches the reference with no ledger entry.

#### US-281: Route `workspace/worktrees/list`
**Description:** As an editor client, I want the declared worktree listing method to answer, so that the name I read in the method inventory is a call I can make.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-280
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/app_server/_host.py:385-408` for the response and its swallowing of the not-a-repository and git-unavailable errors into an empty list, `:101` and `:289-292` for its position among the methods reachable without a session, and `/home/arthur/dev/mistral-vibe/vibe/app_server/protocol.py:894-907` for the parameter and response shapes.

**Acceptance Criteria:**
- [ ] Given a client calls the method with a `cwd` inside a repository holding linked worktrees, when it answers, then the response carries one entry per worktree with `name`, `branch`, `cwd`, `root` and `repoRoot`.
- [ ] Given the `cwd` is not inside a git repository, when it answers, then the response carries an empty list rather than an error.
- [ ] Given git is unavailable, when it answers, then the response carries an empty list rather than an error.
- [ ] Given the method is called before any session exists, when it answers, then it answers, because it is a host method.
- [ ] Given the response, when the census validator runs, then it validates against `WorkspaceWorktreeListResponse` with no surplus and no missing field.
- [ ] Given the method becomes routed, when `the_unrouted_reference_methods_are_exactly_the_recorded_backlog` runs, then it passes, because its `UNROUTED_METHODS` entry was removed in this same change.
- [ ] Given the method becomes routed, when `docs/parity.md` is read, then the accepted divergence at `docs/parity.md:260` names `identity/read` alone and the worktree half of the sentence is gone.
- [ ] Given the method is advertised, when a client reads the handshake, then the name appears in the advertised surface.

#### US-282: Accept a local workspace selection on session start
**Description:** As a desktop client, I want to start a session inside an existing worktree or a newly created one, so that opening a project in a worktree is a supported call instead of an `invalid_params`.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-280
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/app_server/protocol.py:202-222` for the discriminated union and the required `branch` and `name` on the create kind, `/home/arthur/dev/mistral-vibe/vibe/app_server/server.py:1250-1290` for the resolution including the linked-worktree membership test on the existing kind, `:895-903` for running it off the request task, and `:1238-1247` for the refusal on resume and continue.

**Acceptance Criteria:**
- [ ] Given `session/start` carries `localWorkspaceSelection` with kind `existing`, when the working path is one of the checkout's linked worktrees, then the session opens with `cwd` and the workspace roots set to it.
- [ ] Given kind `existing` naming a path that is not a linked worktree of the base, when it resolves, then the call fails with `invalid_params` naming the path.
- [ ] Given kind `create` with a `name` and a `branch` that differ, when it resolves, then a worktree named by `name` is created on the branch named by `branch`.
- [ ] Given kind `create` whose name is unportable or whose branch git refuses, when it resolves, then the call fails with `invalid_params` and nothing is created.
- [ ] Given the field is absent, when a session starts, then behavior is unchanged from today.
- [ ] Given `session/resume` or `session/continue` carries the field, when it is dispatched, then the call fails with `invalid_params` stating the field is only supported when starting a session.
- [ ] Given `cwd` is absent from the options, when the selection resolves, then the app-server process working directory is used as the base.
- [ ] Given the base path is not a directory, when the selection resolves, then the call fails with `invalid_params` naming it.
- [ ] Given the resolution succeeds, when the resolved options are read, then `localWorkspaceSelection` has been cleared so nothing downstream resolves it twice.
- [ ] Given the client cancels the request while the worktree is being created, when the cancellation lands, then the creation still completes and is cleaned up by US-283 rather than leaving an orphan.

#### US-283: Clean up a worktree a failed startup created
**Description:** As a desktop client, I want a worktree created for a session that then failed to start to be removed, so that a retry does not accumulate half-open worktrees.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-282
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/app_server/server.py:923-936`, which removes the prepared worktree with `delete_branch` taken from `branch_created` and logs rather than raising when the removal itself fails, and `/home/arthur/dev/mistral-vibe/vibe/core/worktree.py:288-297` for the removal.

**Acceptance Criteria:**
- [ ] Given a worktree was created for the session and startup then fails, when the failure is handled, then the worktree is removed.
- [ ] Given the branch was created with it, when the cleanup runs, then the branch is deleted; given the branch existed before, then it survives.
- [ ] Given the selection resolved to an existing worktree, when startup fails, then nothing is removed.
- [ ] Given the removal itself fails, when the cleanup runs, then the original startup error is what reaches the client and the removal failure is logged.
- [ ] Given startup succeeds, when the session closes normally, then the app-server removes nothing, because worktree cleanup on exit is the terminal client's contract.

---

### EP-090: The lifecycle the operator sees

Say what the reference says, on the stream it says it on, and offer cleanup when
it offers it.

**Definition of Done:** the three narration lines, the flag help, the reason
wording and the stream assignment match the reference; cleanup is offered on the
exit code rather than on a constructed runtime; the prompts read stdin and
decline at end of file.

#### US-284: Say what the reference says while preparing and removing
**Description:** As a contributor, I want the worktree lifecycle narrated the way the reference narrates it, so that a scripted invocation reading stderr sees the same lines and `--help` explains the flag.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-270
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/entrypoint.py:291-304` for the two preparation lines and the stream each uses, `:221-255` for the five cleanup lines including `Removing worktree:` before the removal, `:135-141` for the flag's metavar and help, `/home/arthur/dev/mistral-vibe/vibe/app_server/_runtime.py:640-648` for the hint appended under the managed root, and `/home/arthur/dev/mistral-vibe/vibe/core/skills/builtins/vibe.py:642` for the semantics stated to the agent.

**Acceptance Criteria:**
- [ ] Given preparation starts, when it runs, then a line naming the requested worktree is written to stderr before any git command runs.
- [ ] Given preparation succeeds, when it returns, then the line naming the working path is written to stderr, as it is today.
- [ ] Given preparation fails, when the error is reported, then it is written to stdout, matching the one line in the reference block that carries no `file=`, and the process exits 1.
- [ ] Given a removal is about to run, when cleanup proceeds, then a line naming the worktree root is written to stderr before `worktree remove`, so an interrupted removal is still attributable.
- [ ] Given inspection fails, when cleanup runs, then the reported line names inspection specifically rather than clean-up generally, and the worktree is kept.
- [ ] Given `vibe --help`, when it is printed, then `--worktree` shows the metavar `NAME` and help text covering the managed location, the branch, the implicit trust and that it is ignored with `--setup` and `--check-upgrade`, written as this port's own sentences.
- [ ] Given a resume or continue finds no sessions and the working directory is under the managed root, when the error is reported, then it carries the additional sentence about the worktree having no sessions yet; given it is elsewhere, then it does not.
- [ ] Given the `vibe` skill asset, when it is read, then it states that the worktree is created or reused under the managed root on a branch of that name, that the session is implicitly trusted, and that automatic cleanup covers only a worktree this run created.

#### US-285: Offer cleanup on a clean exit, not on a constructed runtime
**Description:** As a contributor, I want the cleanup prompt when the run ends cleanly, so that quitting at the update prompt or interrupting startup does not leave a worktree behind without asking.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-270
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/entrypoint.py:334-356` for the gate and its comment on why a startup failure must not delete a reused worktree, and `/home/arthur/dev/mistral-vibe/vibe/cli/cli.py:296-297` and `:424-426` for the two paths that reach exit code 0 without a session.

**Acceptance Criteria:**
- [ ] Given the run exits with code 0 or with no code, when the process ends, then cleanup is offered for a worktree this run created.
- [ ] Given the run exits non-zero, when the process ends, then no cleanup is offered and the worktree is kept.
- [ ] Given the user quits at the startup update prompt, when the process ends, then cleanup is offered, which it is not today.
- [ ] Given the user interrupts during startup with Ctrl-C, when the process ends, then cleanup is offered.
- [ ] Given `--prompt` was used, when the process ends, then no cleanup is offered, as today.
- [ ] Given the worktree was reused rather than created, when the process ends, then no cleanup is offered and no prompt appears.
- [ ] Given cleanup itself fails, when the process ends, then the failure is reported and the exit code of the run is preserved.

#### US-286: Ask on stdin and decline at end of file
**Description:** As a contributor running Vibe with a piped prompt, I want the cleanup question skipped rather than asked on the controlling terminal, so that a non-interactive stdin means the worktree is kept.

**Priority:** P2
**Size:** S (2 pts)
**Dependencies:** Blocked by US-285
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/entrypoint.py:181-201` and `:203-219`, both reading `input()` and returning false on `EOFError` or `KeyboardInterrupt`, and `/home/arthur/dev/mistral-vibe/vibe/core/worktree.py:96-106` for the reason wording.

**Acceptance Criteria:**
- [ ] Given stdin is at end of file, when the removal question is asked, then it is declined and the worktree is kept.
- [ ] Given stdin is not a terminal, when cleanup runs, then no controlling terminal is opened and the answer comes from stdin alone.
- [ ] Given the user interrupts the question, when it returns, then it is declined and the worktree is kept.
- [ ] Given a commit count of one, when the reasons are built, then the phrase matches the reference's singular form without the extra words this port adds today, and two commits produce the plural.
- [ ] Given uncommitted changes, untracked files and new commits together, when the reasons are built, then they appear in the reference's order.
- [ ] Given the branch existed before the session, when the branch question is asked and declined, then the worktree is removed and the branch is kept.

---

### EP-091: Restate row 5 from its oracle

Close the row from the measurement rather than from a reading, and name what
will not be ported.

**Definition of Done:** `docs/parity.md` row 5 reads 100, names the oracle and
the command that reproduces it, and both permanent divergences are in the
accepted table with the file that holds each in place.

#### US-287: Record the permanent divergences by name
**Description:** As a reader of the scorecard, I want the two worktree behaviors that will not be ported recorded with their reasons, so that row 5 reaching 100 is a decision I can audit rather than an omission.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-284, US-286
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/worktree.py:529-536` for the working-directory guard this port has no need of, and `/home/arthur/dev/mistral-vibe/vibe/cli/entrypoint.py:221-255` for the `rich` markup every lifecycle line carries.

**Acceptance Criteria:**
- [ ] Given `docs/parity.md`, when the accepted table is read, then it carries an entry stating that this port never changes the process working directory, so the reference's guard against removing the directory it is standing in has no counterpart, and it names the absence of any `set_current_dir` in `crates/` as the evidence.
- [ ] Given the same table, when it is read, then it carries an entry stating that the lifecycle lines are written as plain text where the reference renders them through `rich` markup, on the same terms as the existing onboarding entry.
- [ ] Given each new entry, when it is read, then it names the file in this repository that holds the divergence in place.
- [ ] Given the open divergences table, when it is read, then no worktree entry remains, because each has either been closed or moved to accepted.
- [ ] Given an entry describes a divergence that has stopped reproducing, when the row is reviewed, then it is removed rather than left standing.

#### US-288: Remeasure and restate row 5
**Description:** As a reader of the scorecard, I want row 5 restated from the widened measurement, so that its score names how it was obtained and what would have to break for it to be wrong.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-271, US-272, US-273, US-274, US-276, US-277, US-278, US-280, US-281, US-282, US-283, US-285, US-287
**Reference:** none read for this story: it is a restatement from measurements already taken. Pattern to copy: the "Restated 2026-08-21 from 95" cell on row 4 and the "**Measured by a differential oracle**" cell on row 17 of `docs/parity.md`.

**Acceptance Criteria:**
- [ ] Given the full CI sequence is run unfiltered from the workspace root, when it completes, then all four commands pass.
- [ ] Given row 5, when it is read, then it says 100, carries the restatement date, says it is measured by a differential oracle, and names the command that reproduces it.
- [ ] Given row 5's reference column, when it is read, then it still names `vibe/core/worktree.py` at the pin and still records that the module moved at v2.24.2.
- [ ] Given the method section, when it is read, then the capture-script and replay-module counts have been recounted rather than incremented by hand.
- [ ] Given row 17, when it is read, then it states whether routing `workspace/worktrees/list` moved its score, without taking that row to 100.
- [ ] Given the replay, when the case count is printed, then it is 60 or more and the ledger holds only entries this PRD names.
- [ ] Given a reader asks what would make the 100 wrong, when the row is read, then it names the re-pin to v2.24.2 as the answer.

## Functional Requirements

- FR-01: The system must count commits added during a session against the
  worktree's current HEAD, so a commit made while HEAD is detached blocks
  cleanup.
- FR-02: The system must remove a worktree it created and delete a branch it
  created when preparation fails after the worktree was added.
- FR-03: The system must refuse a worktree name that is not a single portable
  path segment, before creating anything.
- FR-04: The system must validate the branch with `git check-ref-format
  --branch` before creating anything.
- FR-05: The system must treat an empty `--worktree` value as no worktree.
- FR-06: The system must resolve the primary checkout from the working directory
  when the invocation is inside it, and must refuse to guess for a linked
  worktree on a separate git directory.
- FR-07: The system must report a failing branch probe as an error rather than
  as an absent branch.
- FR-08: The system must refuse a working directory that does not exist in the
  worktree, is not a directory, escapes the worktree, or sits under a nested
  repository.
- FR-09: The system must resolve `VIBE_HOME` and the managed root before use and
  must refuse a repository directory that falls outside the managed root.
- FR-10: The system must enumerate the linked worktrees of a checkout, skipping
  the primary checkout, branchless records and prunable records, ordered by
  working path.
- FR-11: The system must answer `workspace/worktrees/list` with the declared
  response shape, and must answer an empty list rather than an error when the
  path is not a repository or git is unavailable.
- FR-12: The system must accept `localWorkspaceSelection` on `session/start` for
  both the existing and create kinds, and must refuse it on resume and continue.
- FR-13: The system must remove a worktree it created for a session whose
  startup then failed.
- FR-14: The system must offer cleanup when the run ends with exit code 0 or no
  code, for a worktree this run created and when no prompt was passed.
- FR-15: The system must decline the cleanup question when stdin is at end of
  file rather than opening the controlling terminal.
- FR-16: The system must narrate preparation, use and removal on the streams the
  reference uses.
- FR-17: Every corpus committed by this work must carry the pinned reference
  commit and must fail its replay when that commit and
  `vibe_core::parity::REFERENCE_COMMIT` disagree.
- FR-18: No corpus committed by this work may contain reference-authored prose
  in cleartext.

## Non-Functional Requirements

- **Performance:** preparing a worktree in a repository holding fewer than
  10 000 tracked files completes in under 2 seconds excluding git's own checkout
  time, asserted by a test that fails above that bound.
- **Performance:** enumerating the linked worktrees of a checkout holding 50
  worktrees completes in under 200 ms.
- **Performance:** the worktree corpus replay adds under 5 seconds to
  `cargo test --workspace --all-features` on the CI runner, git subprocess time
  included.
- **Security:** no path outside the managed root is ever passed to
  `git worktree remove --force`, asserted by a containment check that runs
  before every removal.
- **Security:** no capture and no test writes to the user's `$VIBE_HOME`, git
  configuration or reference checkout; each sets `VIBE_HOME`,
  `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` inside its own temporary
  directory, verified by a test that fails when either variable is unset during
  a capture.
- **Reliability:** a missing or off-pin reference checkout never fails
  `cargo test`; the corpus replay runs unconditionally and the live probe skips
  with a printed reason.
- **Reliability:** the capture script is byte-identical across two consecutive
  runs with no change in between, verified by its `--check` mode.
- **Reliability:** no code path removes a worktree without either a clean
  inspection or an answered prompt, asserted by a test per removal call site.
- **Compatibility:** a session recorded before this work hydrates without error,
  including one whose recorded working directory is inside a worktree.
- **Compatibility:** `session/start` without `localWorkspaceSelection` behaves
  exactly as it does today, asserted by the existing app-server tests running
  unchanged.
- **Maintainability:** no ledger entry is scoped wider than one field on one
  case, and every entry names a story ID or the licensing boundary.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Reference checkout absent | `VIBE_REFERENCE` unset and default path missing | Corpus replays; live probe skips | Printed skip reason naming the path and the override |
| 2 | Reference checkout off-pin | Local checkout at another commit | Corpus replays; live probe skips | Printed reason naming both commits and the restore command |
| 3 | Commit made on a detached HEAD | `git checkout --detach` then commit inside the worktree | Cleanup blocked, prompt shown | Reasons name the commit count |
| 4 | Preparation fails after `worktree add` | Target directory removed between add and inspection | Worktree removed, created branch deleted | Original error, rollback failure attached if any |
| 5 | Rollback fails too | Repository locked by another git process | Original error returned with a note | Both failures named, neither swallowed |
| 6 | Unportable name | `--worktree aux` | Refused before anything is created | Message naming the flag and the portable-segment rule |
| 7 | Branch git refuses | `--worktree foo.lock` | Refused before anything is created | Message naming the branch |
| 8 | Empty flag value | `--worktree ""` | Normal session, no worktree | None |
| 9 | Separate git directory, primary checkout | `git init --separate-git-dir` | Root resolved from the working directory, removal succeeds | None |
| 10 | Separate git directory, linked worktree | Invoked from a linked worktree of such a repository | Refused | Message stating the primary checkout cannot be determined |
| 11 | Branch probe fails | Corrupted refs, `show-ref` exits 128 | Typed error, nothing created | Message naming the branch and carrying git's stderr |
| 12 | Target path escapes the worktree | Subdirectory is a symlink pointing outside | Refused | Message naming the worktree it escaped |
| 13 | Nested repository on the path | A `.git` between the worktree root and the target | Refused | Message reporting a different repository |
| 14 | Worktree path holds a symlink component | Managed root re-created as a link | Existing worktree refused | Message naming the linked component |
| 15 | Old git without `-z` | `worktree list --porcelain -z` exits 129 | Retried without `-z`, parsed on newlines | None |
| 16 | Prunable or detached record | A worktree directory deleted by hand | Excluded from the list | None |
| 17 | Listing a non-repository | `workspace/worktrees/list` with a plain directory | Empty list | None |
| 18 | Selection naming an unlinked path | `existing` kind with an unrelated directory | `invalid_params` | Message naming the path |
| 19 | Selection on resume | `session/resume` carrying the field | `invalid_params` | Message stating start-only support |
| 20 | Startup fails after creating a worktree | Bad configuration after the create kind resolved | Worktree removed, created branch deleted | Original startup error reaches the client |
| 21 | Quit at the update prompt | Answering quit before any session | Cleanup offered | The normal cleanup prompt |
| 22 | Interrupted startup | Ctrl-C before the runtime opens | Cleanup offered | The normal cleanup prompt |
| 23 | Piped stdin at cleanup | `echo prompt \| vibe --worktree x` | Question declined, worktree kept | Line naming the kept worktree |
| 24 | `VIBE_HOME` is a tilde path | `VIBE_HOME=~/vibe` | Expanded and resolved before use | None |
| 25 | Windows verbatim path | `fs::canonicalize` returns a `\\?\` prefix | Prefix stripped before hashing | None |
| 26 | Corpus schema drift | A capture adds a field without a version bump | Replay fails | Error naming the expected and found versions |
| 27 | Stale ledger entry | A divergence was fixed but its entry remains | Staleness check fails | Error naming the entry |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Moving the contract into `vibe-core` breaks the CLI startup path, which nothing outside the moved module tests today | Med | High | US-270 moves the seven existing unit tests with the code and requires the full suite unfiltered; the move is behavior-preserving by construction and lands before any behavioral change |
| 2 | Routing `workspace/worktrees/list` fails the app-server surface replay, which asserts it stays unrouted | High | Med | Certain, not a risk to avoid: US-281 removes the `UNROUTED_METHODS` entry and the `docs/parity.md:260` sentence in the same change, and the test failing otherwise is the intended signal |
| 3 | Counting against HEAD makes cleanup stricter and users read the new prompt as a regression | Med | Med | US-272 records the change in `CHANGELOG.md` as a behavior change; the prompt is the reference's behavior and the alternative loses commits |
| 4 | The capture builds git repositories and picks up a developer's global configuration, hooks or template directory, making the corpus machine-dependent | High | High | US-269 pins `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, both dates, the identity and the default branch, and its `--check` mode fails on any difference between two runs |
| 5 | `base_commit` varies between runs and the corpus can never be byte-identical | Med | Med | US-269 pins the commit dates and the identity, which makes the hash deterministic; if it still varies, the field is normalized to a symbolic marker and the ledger records the reason |
| 6 | `PureWindowsPath` semantics are subtler than the component rule assumed, so the validator diverges in a third direction | Med | Med | US-269 captures the reference verdict for the full authored name list first, so US-276 is written against measurements; the list already separates sixteen rejections from seven acceptances |
| 7 | `localWorkspaceSelection` on `SessionStartParams` collides with `deny_unknown_fields` on a nested shape and silently accepts a malformed selection | Low | High | US-282 requires the discriminated union to reject an unknown `kind` and requires the create kind to reject a missing `branch` or `name`, both as explicit criteria |
| 8 | Moving the cleanup gate to the exit code offers removal in a path where the worktree is still in use | Low | High | US-285 keeps the created-this-run and no-prompt conditions unchanged and only replaces the runtime condition; the prompt still governs every removal that is not provably clean |
| 9 | Row 5 turns out to have an eleventh gap this read missed, so 100 is claimed too early | Med | High | US-288 requires the full CI sequence unfiltered and requires the row to name what would make the score wrong; the oracle covers the module that had none, which is where the unknown most plausibly lives |
| 10 | The corpus captures a reference error sentence and it reaches the repository | Low | High | US-269 stores every message as a digest with a structural marker and US-271 audits the corpus for cleartext, mirroring the existing digest test on the tool surface |
| 11 | The replay's git subprocesses make the test suite noticeably slower on CI | Med | Low | A 5 second bound is an explicit non-functional requirement; scripted repositories are minimal, built once per case family and reused within it |

## Non-Goals

- Re-pinning the reference to v2.24.2. The pin stays at `b78b451`; a bump would
  require regenerating every committed corpus in the same change, and at
  `5e6aa0f` this module no longer exists.
- Porting the v2.24.2 worktree subsystem: `ManagedWorktree`, `WorktreeRelease`,
  `WorktreeReleaseOutcome`, `WorktreeRepository`, the on-disk worktree records
  and automatic naming from prompt text. That is roughly 1 480 lines against the
  pin's 536 and it is a different contract, not a deeper version of this one.
- `workspace/worktrees/remove`. It arrived upstream after the pin and is
  recorded in the drift section as one of the fourteen methods this port has not
  learned.
- Changing the process working directory to match the reference's `os.chdir`.
  This port carries the working directory instead, which is the stronger design,
  and US-287 records the consequence.
- Byte-identical lifecycle sentences beyond the short operational labels already
  in the tree. The licensing boundary governs anything longer.
- `rich` markup rendering. Recorded as an accepted divergence by US-287 on the
  same terms as the onboarding entry.
- Row 17 beyond routing one method. US-288 states whether its score moved; it
  does not take that row to 100.
- Windows-specific end-to-end verification. The corpus is captured on Linux; the
  two Windows-shaped rules, reserved device names and the verbatim path prefix,
  are covered by unit tests rather than by a Windows capture.

## Files NOT to Modify

- `crates/vibe-core/src/parity.rs`: carries the pin; changing it invalidates
  every committed corpus at once.
- `scripts/parity/pin.py`: the second pin source; the parity test fails when the
  two disagree or when a third copy appears.
- `NOTICE`: declares the licensing boundary this work operates under.
- `crates/vibe-app-server/tests/app-server-surface/corpus.json`: the census
  already records every worktree model and both selection kinds; this work makes
  them reachable rather than redeclaring them.
- `crates/vibe-cli/tests/runtime-parity/startup.json` and its oracle: the
  worktree trace there measures invocation routing and keeps doing exactly that;
  the worktree contract gets its own corpus rather than widening this one.
- `crates/vibe-protocol/src/methods.rs`: `workspace/worktrees/list` is already
  declared; US-281 routes it without touching the inventory.
- `vibe/**` in the reference checkout: read-only oracle, never written.

## Technical Considerations

- **Architecture:** should the worktree module land as
  `crates/vibe-core/src/worktree.rs` with an inline test module, or as a
  directory with `naming`, `records` and `repository` submodules the way v2.24.2
  splits it? Recommended: a single module. The pinned contract is 536 Python
  lines and the port of it is smaller; splitting now imports a structure from a
  version this PRD explicitly does not target. Engineering to revisit at the
  re-pin.
- **Architecture:** the app-server needs preparation and enumeration, the CLI
  needs preparation, enumeration is not needed by the CLI today. Recommended:
  publish both from `vibe-core` regardless, because `list_linked_worktrees` is
  what validates an `existing` selection and splitting the two would put half
  the contract behind a feature.
- **Data Model:** the corpus needs to record a path that differs per machine.
  Options: relativize against the case's temporary root, or record a digest.
  Recommended: relativize, because a divergence in a path must be readable to be
  diagnosable, and a temporary-root-relative path carries no user data.
- **Data Model:** the managed-directory naming rule is captured over synthetic
  input strings rather than real paths, so the replay recomputes SHA-256 on the
  same input. This is the only way both sides can agree without owning the same
  temporary directory.
- **API Design:** should the core preparation function take a vibe home
  directory or resolve it itself? Recommended: take it. `vibe-core` has no
  business reading CLI arguments, and the app-server resolves its own home
  differently from the CLI.
- **API Design:** should `WorktreeError` be one enum or three types mirroring
  the reference's three exception classes? Recommended: one enum with variants
  covering not-found and git-unavailable, because the app-server's only use of
  the distinction is deciding whether to answer an empty list.
- **Dependencies:** none new. Git is invoked as a subprocess exactly as it is
  today and as `crates/vibe-app-server/src/projects/git.rs` already does.
- **Migration:** no persisted state changes shape. The one behavior change a
  contributor can observe is that a worktree carrying a detached-HEAD commit is
  now kept until the prompt is answered, which US-272 records in the changelog.
  Rollback is per-epic: EP-087 through EP-090 are independent once EP-086 has
  landed.
- **Sequencing:** EP-086 blocks everything. EP-087 and EP-088 are independent of
  each other. EP-089 depends on EP-088's target-directory and primary-root work
  because enumeration validates each record through the same path. EP-090 needs
  only the move. EP-091 requires all of them.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Row 5 score | 90, unexplained and unmeasured | 100, with a named oracle | Month-1 | `docs/parity.md` row 5 |
| Worktree cases replayed | 0 | 60 or more | Month-1 | Case count printed by the replay test |
| Reference worktree entry points with an executable oracle | 0 of 6 | 6 of 6 | Month-1 | `scripts/parity/` inventory |
| Ways a `--worktree` session can lose a commit | 1 (detached HEAD) | 0 | Month-1 | The detached-HEAD case in the replay |
| Reference worktree functions with no counterpart here | 1 (`list_linked_worktrees`) | 0 | Month-1 | Symbol diff against `vibe/core/worktree.py` |
| App-server worktree calls that fail | 2 (list, selection) | 0 | Month-1 | Probe both against the running server |
| Name shapes accepted here and refused upstream | 16 | 0 | Month-1 | The authored name list in the corpus |
| Row-5 gaps with no entry in a divergence table | 10 points' worth | 0 | Month-1 | `docs/parity.md` divergence sections |
| Full CI sequence, unfiltered | Passing | Passing | Month-6 | Four commands from the workspace root |

## Open Questions

- Should the CLI gain a way to list worktrees now that `vibe-core` can, for
  instance a `/worktrees` command? Owner: Arthur Jean, after US-280. Out of
  scope here because the reference has no such command at the pin; asked because
  the capability arrives with US-280 either way.
- Should the app-server's `session/start` also accept a bare `cwd` pointing into
  a worktree without a selection, as it does today, or should that become an
  explicit selection? Owner: Arthur Jean, before US-282. Deferred by default:
  the reference accepts both and this port must not narrow it.
- Does any current consumer depend on `--worktree ""` failing? Owner: Arthur
  Jean, before US-279. Nothing in the tree suggests one; the criterion is
  written to match the reference regardless.
- Should the two Windows-shaped rules be verified by a Windows CI job rather
  than by unit tests? Owner: Arthur Jean, after US-288. Blocking nothing; the
  corpus records its platform so a later Windows capture is additive.
[/PRD]
