[PRD]
# PRD: Tool Infrastructure at Full Parity

**Reference root:** every `vibe/...` path in this document is relative to the
read-only Python checkout at `/home/arthur/dev/mistral-vibe/`
(`C:\dev\mistral-vibe` on Windows, `VIBE_REFERENCE` overrides both), read at
commit `b78b451` and never from its working tree. See `## Reference Map` for the
read commands and the full symbol table.

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-21 | Arthur Jean | Initial draft: take parity row 4 from 95 to 100 |

## Problem Statement

Row 4 of `docs/parity.md` ("Tool infrastructure (registry, schemas, filtering)")
scores 95. Unlike every other row below 100, its "State and gaps" cell names no
cause: it lists what was measured and stops. Neither `## Open divergences` nor
`## Accepted divergences` carries an entry attributable to it. Five points sit
on the scorecard with nothing behind them.

A read of both trees at reference commit `b78b451` finds the five points, and
finds that four of the five files the row names are conformant where they are
measured. The published surface is empty of divergence: `missingNames`,
`extraNames` and `schemaDivergence` are all zero in
`crates/vibe-app-server/tests/tool-surface/baseline.json`, 92 argument fixtures
replay the reference verdict, and per-tool configuration replays 26/26 classes,
22/22 keys and 146/146 pairs. The gap is in the fifth file, `ui.py`, which no
oracle in this repository has ever executed, and in four contracts that the
three existing oracles step around rather than cover:

1. **No oracle captures a tool's presentation.** `scripts/parity/` holds 22
   capture scripts. Not one calls `get_call_presentation` or
   `get_result_presentation` (`vibe/core/tools/ui.py:161` and `:177`).
   `tool_execution.py` captures the typed result, the rendered model text and
   the projection, and stops there. The presentation is the third published
   result shape and it is unmeasured, which is the same instrument shape row 3
   was restated from.
2. **Every remote tool renders under the wrong contract.** `MCPTool` subclasses
   `ToolUIData` (`vibe/core/tools/remote.py:31-34`), so an MCP or connector call
   never reaches the adapter's `name(args)` fallback: it takes
   `ToolUIData.format_call_display` (`ui.py:48`) and renders the published name
   alone. This port routes every tool with no name mapping to
   `ToolEffectKind::Tool` and renders `generic_call_summary`
   (`crates/vibe-core/src/events/detail.rs:570-583`), so `github_create_issue`
   shows as `github_create_issue(owner='acme', repo='api')` where the reference
   shows `github_create_issue`. This is on screen for every MCP call.
3. **`statusText` is a constant where the reference names the source.** The
   reference publishes `Calling MCP tool <remote name>`
   (`vibe/core/tools/mcp/tools.py:310-312` and `:489-491`) and
   `Calling connector tool <remote name>`
   (`vibe/core/tools/connectors/connector_registry.py:313-316`). Neither string
   exists anywhere in `crates/`. This port publishes the fixed
   `Running tool` from `ToolEffectKind::status_text`
   (`crates/vibe-core/src/events/detail.rs:85`).
4. **`settledMessage` is always null on the wire.** The adapter fills it from
   the summary whenever a tool left it unset (`ui.py:129-130`), and it does that
   for every tool, not only the fallback. This port hardcodes `settled_message:
   None` at `crates/vibe-core/src/events/detail.rs:442` and compensates inside
   `EffectCallDisplay::subject`. Every client that reads the published field
   rather than this port's helper sees a null the reference never sends.
5. **The description override is absent.** `_iter_tool_descriptions`
   (`vibe/core/tools/manager.py:227-259`) reads `<tools-dir>/prompts/*.md` in
   every search path, keyed by file stem, later paths winning, blank files
   ignored, and `available_tool_specs` (`manager.py:610-618`) prefers that text
   over the tool's own description. An operator can therefore redescribe a
   builtin or an MCP tool by writing `.vibe/tools/prompts/read_file.md`. This
   port reads no such file. `tool_paths` is declared at
   `crates/vibe-core/src/config/registry.rs:728` and has no consumer at all:
   the key is accepted, published in the schema, and ignored.
6. **The `enabled_tools` gate reads the wrong value.** The reference tests the
   raw list (`manager.py:311`); this port tests the compiled patterns
   (`crates/vibe-core/src/tools.rs:493` through `NameFilter::is_empty`). With
   `enabled_tools = ["  "]` or `["re:("]` the reference publishes zero tools and
   this port publishes all of them. A filter that fails open where the reference
   fails closed is the wrong direction for the error.
7. **`sensitive_patterns` runs on the wrong matcher.** `utils.py:107` matches
   with `PurePath.match`, which is anchored to the right and segment-aware;
   this port matches with `fnmatch` (`crates/vibe-core/src/policy.rs:1079`),
   which is what the allowlist and denylist correctly use on both sides
   (`utils.py:39-45`). The shipped defaults `**/.env` and `**/.env.*` agree
   under both engines, which is exactly why nothing caught it: the one case the
   permission oracle captures pins `sensitive_patterns=["**/.env"]`
   (`scripts/parity/permission_surface.py:247`). An operator-written `.env`,
   `secrets/*` or `/etc/*` diverges, in both directions.
8. **The argument rejection message is unmeasured.** The fixture carries
   `accepted: bool` and nothing else
   (`crates/vibe-app-server/src/tool_surface_parity_tests.rs:788-797`). The
   reference returns a `ToolError` naming the tool
   (`vibe/core/tools/base.py:240-242`); this port returns
   `schema validation failed at <pointer>: <message>`
   (`crates/vibe-core/src/tools.rs:694`). That string is fed back to the model
   on every rejected call and has never been compared.

**Why now:** rows 1, 2, 3, 23, 24, 26, 27, 28, 29, 31 and 32 are at 100 and each
was taken there by naming its cause first. Row 4 is the highest-scoring row with
no named cause, which makes it the cheapest remaining point and the one most
likely to be wrong in the other direction: a row whose gap is undocumented can
be scored 95 for a reason nobody can reconstruct. Row 4 is also load-bearing for
rows 11, 12 and 21, which read `manager.py`, `permissions.py` and `ui.py`
through it, so the three contracts fixed here move four rows at once.

## Overview

The work is instrument-first, in the shape row 3 established. A fourth oracle,
`scripts/parity/tool_presentation.py`, drives `get_call_presentation` and
`get_result_presentation` over every published tool including a stub MCP tool
and a stub connector tool, and records the eight call-display fields and the
five result-display fields per case. A Rust replay holds this port to that
corpus with an audited ledger and a case floor, so a presentation divergence
fails `cargo test` instead of aging into a wrong score.

With the instrument in place, four behavioral epics land. The presentation of a
remote tool is routed by `ToolSource` rather than by tool name, so an MCP call
renders the published name, a connector call renders `connector <tool>` when it
settles, and `statusText` names the remote server the way the reference does.
`settledMessage` is filled at the point the display is built rather than
compensated for in each client. The description override reads
`<tools-dir>/prompts/<name>.md` from the project directory, the user directory
and `tool_paths`, which also gives `tool_paths` its first consumer. The two
filters that diverge are corrected against their reference call sites, and the
argument rejection message is captured and held.

The last epic restates the row from the widened measurement and records, by
name, the one part of row 4 that will never reach parity: loading Python tool
classes from `.py` files is out of reach by construction, so `ToolSource::Custom`
stays a type with no producer and the `.py` half of `_iter_tool_classes` becomes
an accepted divergence with a stated reason, rather than five unexplained points.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Row 4 score in `docs/parity.md` | 100, restated from a measurement | 100, still measured on every CI run |
| Tool presentations replayed against the reference | 40 or more cases, 0 unledgered divergences | Same, with the floor raised as tools are added |
| Row 4 contracts covered by an executable oracle | 5 of 5 reference files | 5 of 5 |
| Unexplained points on row 4 | 0 (every gap named in a divergence table or closed) | 0 |
| Rows reading row 4 that gain a measured contract | 3 (rows 11, 12, 21) | 3 |

## Target Users

### Contributor changing tool infrastructure

- **Role:** whoever edits `crates/vibe-core/src/tools.rs`,
  `crates/vibe-core/src/events/detail.rs`, `crates/vibe-core/src/policy.rs` or
  the config registry.
- **Behaviors:** reads the reference module first, writes Rust, runs the CI
  sequence from the workspace root, reads `docs/parity.md` to know what is
  already claimed.
- **Pain points:** four of the five files row 4 names have no executable
  oracle, so a presentation or filter change is reviewed by reading rather than
  by measurement; the row's score cell names no gap, so there is no list of
  what is still open.
- **Current workaround:** open both trees side by side and compare by eye, then
  hope the reviewer opens the same two files.
- **Success looks like:** editing the presentation of a remote tool fails
  `cargo test` with a named pointer when it diverges, and passes when it does
  not.

### Operator configuring the tool surface

- **Role:** whoever writes `tools.*`, `enabled_tools`, `disabled_tools`,
  `tool_paths` and `connectors` in a Vibe configuration file.
- **Behaviors:** copies a configuration that worked against the Python client
  and expects the Rust client to answer the same way.
- **Pain points:** `tool_paths` is accepted and silently ignored; a
  `sensitive_patterns` entry that guards a file upstream guards nothing here;
  an `enabled_tools` list of blanks publishes every tool instead of none; a
  description override written under `.vibe/tools/prompts/` has no effect.
- **Current workaround:** none available. Each of these fails silently, which
  is the failure mode with no workaround.
- **Success looks like:** the same configuration file produces the same
  published surface and the same approval prompts under both clients.

### Reader of the scorecard

- **Role:** anyone deciding whether this port can replace the reference for a
  given workflow.
- **Behaviors:** reads `docs/parity.md` top to bottom, trusts a score only when
  the row names how it was measured.
- **Pain points:** row 4 says 95 and gives no reason, so the reader cannot tell
  whether the missing five points block their workflow.
- **Current workaround:** treat the number as noise.
- **Success looks like:** row 4 says 100 and names the oracle, or names the
  divergence and why it is permanent.

## Research Findings

Research for this PRD was a differential read of the reference checkout at the
pin, not a market survey: the only comparable product is the reference itself,
and it is readable in full. Web research was not run, and no library
documentation was needed: every dependency involved is already in the
workspace.

### Competitive Context

- **Mistral Vibe (Python reference, v2.24.0 at `b78b451`)**: the behavioral
  oracle. It publishes tool presentations through a single adapter shared by
  builtins, MCP tools and connector tools, so presentation is a property of the
  tool class rather than of the tool name. This port derives presentation from
  a 12-variant `ToolEffectKind` keyed on tool name, which is a stronger internal
  design and diverges at exactly the point where the reference keys on class.
- **Market gap:** none. This is parity work with a single, fully readable
  reference.

### Best Practices Applied

- Widen the instrument before changing behavior. Row 3 was restated from 92
  after its PRD found that the six tools that diverged were exactly the six the
  oracle never executed. Row 4 has the same shape: the one file with no oracle
  is the one file that diverges.
- A parity claim comes from a measurement, and the measurement runs wide enough
  to cover what changed (`AGENTS.md`, "The behavioral oracle").
- A ledger fails both on an undeclared divergence and on an entry that no
  longer reproduces, so a fix cannot leave a stale exception behind
  (pattern established by `crates/vibe-app-server/src/tool_execution_parity_tests.rs`).
- Reference-authored prose never enters this repository. A captured description
  or error message is stored as a digest or as a structural marker, never as
  text (`NOTICE`, `AGENTS.md` "Licensing boundary").

## Assumptions & Constraints

### Assumptions (to validate)

- A stub MCP tool class built by `build_http_tool` with a hand-written
  `RemoteTool` is enough to capture the remote presentation contract without a
  live MCP server. Evidence: `vibe/core/tools/mcp/tools.py:216-313` builds the
  class from `remote.name`, `remote.description` and `remote.input_schema`
  alone, and `scripts/parity/tool_execution.py` already builds tools with
  scripted collaborators. **Risk: MEDIUM.** Validated by US-254.
- `PurePath.match` semantics are reproducible in Rust without a path library:
  the rule is right-anchored component matching with `fnmatch` per component
  and no `**` recursion. Evidence: the pattern is compared component by
  component from the right, and the reference runs on both Linux and Windows
  with the same call. **Risk: MEDIUM.** Validated by US-263.
- Filling `settledMessage` at the source breaks no existing client, because
  `EffectCallDisplay::subject` already resolves to the same string.
  **Risk: LOW.**
- Reading `<tools-dir>/prompts/*.md` at session start costs under 5 ms for a
  directory of 50 files, so no cache is needed. **Risk: LOW.**

### Hard Constraints

- `NOTICE` forbids copying reference source, prompt files or tool description
  text. Every corpus that would carry reference-authored text stores a digest
  or a structural marker instead, and any cleartext corpus stays gitignored
  under `.parity/`.
- The reference checkout is read-only and is read at the pin, never from the
  working tree.
- A missing or off-pin reference checkout must never fail `cargo test`: the
  corpus replay runs unconditionally, the live probe skips with a printed
  reason.
- The pin lives in exactly two places and this PRD does not move it. Every
  corpus written here carries `b78b451c39eab9213393ad2f45908e8562a5c5e7`.
- Layering holds: `vibe-protocol` and `vibe-core` first, `vibe-app-server`
  second, `vibe-cli` and `vibe-acp` third. Presentation logic stays in
  `vibe-core`.
- `unsafe_code` is forbidden; `panic`, `unimplemented` and `dbg_macro` are
  denied outside tests.

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
git -C /home/arthur/dev/mistral-vibe show b78b451:vibe/core/tools/ui.py
git -C /home/arthur/dev/mistral-vibe archive b78b451 vibe/ | tar -x -C <scratch>
```

**Every line number below is anchored at the pin.** The local checkout may sit
at another revision, where the same symbol has moved.

**Every `vibe/...` path in this document resolves against that root**, in the
table below and in each story's `Reference:` line alike. The root is spelled in
full once per story so a reader who opens a single story can navigate without
scrolling back here.

| Symbol | Path at the pin | Line | Read by |
|---|---|---|---|
| `ToolUIData` | `vibe/core/tools/ui.py` | 31-93 | US-254, US-256 |
| `ToolUIData._display_name` | `vibe/core/tools/ui.py` | 34-37 | US-256 |
| `ToolUIData.format_call_display` | `vibe/core/tools/ui.py` | 47-49 | US-256 |
| `ToolUIData.get_call_display` | `vibe/core/tools/ui.py` | 51-64 | US-256 |
| `ToolUIData.format_result_display` | `vibe/core/tools/ui.py` | 66-68 | US-259 |
| `ToolUIDataAdapter` | `vibe/core/tools/ui.py` | 95-192 | US-254 |
| adapter fill-in rules | `vibe/core/tools/ui.py` | 122-132 | US-256, US-258 |
| `ToolUIDataAdapter.get_result_display` | `vibe/core/tools/ui.py` | 134-148 | US-259 |
| `ToolUIDataAdapter.get_status_text` | `vibe/core/tools/ui.py` | 150-159 | US-257 |
| `get_call_presentation` | `vibe/core/tools/ui.py` | 161-175 | US-254 |
| `get_result_presentation` | `vibe/core/tools/ui.py` | 177-192 | US-254 |
| `MCPTool` bases | `vibe/core/tools/remote.py` | 31-34 | US-254, US-256 |
| `MCPTool.get_server_name` | `vibe/core/tools/remote.py` | 39-41 | US-256 |
| `MCPTool.get_remote_name` | `vibe/core/tools/remote.py` | 43-45 | US-257, US-259 |
| `MCPTool.is_connector` | `vibe/core/tools/remote.py` | 47-49 | US-256 |
| MCP `get_result_display` | `vibe/core/tools/mcp/tools.py` | 300-308 | US-259 |
| MCP `get_status_text` (http) | `vibe/core/tools/mcp/tools.py` | 310-312 | US-257 |
| MCP `get_status_text` (stdio) | `vibe/core/tools/mcp/tools.py` | 489-491 | US-257 |
| `build_http_tool` | `vibe/core/tools/mcp/tools.py` | 190-314 | US-254 |
| connector `get_result_display` | `vibe/core/tools/connectors/connector_registry.py` | 302-311 | US-259 |
| connector `get_status_text` | `vibe/core/tools/connectors/connector_registry.py` | 313-316 | US-257 |
| `_compute_search_paths` | `vibe/core/tools/manager.py` | 146-162 | US-260 |
| `_iter_tool_classes` | `vibe/core/tools/manager.py` | 164-225 | US-260, US-267 |
| `_iter_tool_descriptions` | `vibe/core/tools/manager.py` | 227-259 | US-260, US-261 |
| `available_tools` filters | `vibe/core/tools/manager.py` | 295-322 | US-262, US-265 |
| `_build_source_disable_index` | `vibe/core/tools/manager.py` | 415-441 | US-266 |
| `_is_source_disabled` | `vibe/core/tools/manager.py` | 443-457 | US-266 |
| `available_tool_specs` | `vibe/core/tools/manager.py` | 599-618 | US-261, US-265 |
| `BaseTool.invoke` validation | `vibe/core/tools/base.py` | 236-242 | US-264 |
| `resolve_path_permission` | `vibe/core/tools/utils.py` | 30-46 | US-263 |
| `resolve_file_tool_permission` | `vibe/core/tools/utils.py` | 70-139 | US-263 |
| sensitive loop | `vibe/core/tools/utils.py` | 105-119 | US-263 |
| `name_matches` | `vibe/core/utils/matching.py` | 16-35 | US-262 |
| `user_tools_dirs` | `vibe/core/config/harness_files/_harness_manager.py` | 111-116 | US-260 |
| `project_tools_dirs` | `vibe/core/config/harness_files/_harness_manager.py` | 140-143 | US-260 |
| `tool_paths` field | `vibe/core/config/vibe_schema.py` | 295 | US-260 |
| `ConnectorConfig` | `vibe/core/config/models.py` | 393-405 | US-266 |
| `compute_connector_counts` | `vibe/core/tools/connectors/counts.py` | 10-25 | US-266 |

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
python3 scripts/parity/tool_presentation.py --check   # re-run must be byte-identical
```

## Epics & User Stories

### EP-080: An oracle over the presentation contract

Build the missing instrument before changing any rendering, so every later epic
is proven by a corpus that predates it.

**Definition of Done:** `scripts/parity/tool_presentation.py` drives
`get_call_presentation` and `get_result_presentation` over every published tool
plus a stub MCP tool and a stub connector tool; the committed corpus replays in
`cargo test --workspace --all-features` with an audited ledger and a case floor;
a re-run with no change in between is byte-identical.

#### US-254: Capture the presentation of every published tool
**Description:** As a person reading the scorecard, I want the eight call-display fields and the five result-display fields captured for every tool the reference publishes, so that presentation is compared instead of assumed.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/ui.py:95-192` for the adapter and both presentation entry points, `:31-93` for the base class the builtins inherit; `vibe/core/tools/remote.py:31-49` for the remote bases; `vibe/core/tools/mcp/tools.py:190-314` for `build_http_tool`. Pattern to copy: `resolve_reference`, `extract_pinned_tree`, `reexecute_with_reference_interpreter` and `build_corpus` in `scripts/parity/tool_execution.py`, plus its `--check` mode.

**Acceptance Criteria:**
- [ ] Given the pinned tree, when the capture runs, then it constructs a `ToolUIDataAdapter` per tool and records `kind`, `summary`, `content`, `suffix`, `verb`, `message`, `settledVerb`, `settledMessage` and `statusText` for each call case.
- [ ] Given a result case, when the record is written, then it carries `success`, `verb`, `message`, `warnings`, `suffix` and `projectedOutput` from `get_result_presentation`.
- [ ] Given the tool list is built, when it runs, then it covers every builtin the Linux surface publishes plus one stub MCP tool built by `build_http_tool` and one stub connector tool built by the connector factory, so the remote contract is captured without a live server.
- [ ] Given a stub remote tool is built, when the capture runs, then its `RemoteTool` name, description and input schema are authored by this capture and never read from a live server, so no third-party text enters the corpus.
- [ ] Given a case list per tool, when it runs, then it covers valid arguments, absent arguments, arguments of the wrong type, a successful result, an error result and a skipped result.
- [ ] Given a display field would carry reference-authored prose, when the record is written, then it is stored as a SHA-256 digest with a `<described>`-style marker, never as text.
- [ ] Given a display field carries a value this capture itself authored, when the record is written, then it is stored in cleartext so a divergence is readable.
- [ ] Given the capture attempts a network connection, when the socket guard fires, then the capture fails naming the attempt and writes no partial corpus.
- [ ] Given the reference checkout is absent, when the capture runs, then it exits non-zero naming the expected path and the `VIBE_REFERENCE` override.
- [ ] Given the capture is run twice with no change in between, when the two corpora are compared, then they are byte-identical.

#### US-255: Replay the presentation corpus with a ledger and a floor
**Description:** As a person reading the scorecard, I want the presentation corpus replayed on every test run, so that a rendering divergence fails the build instead of aging into a wrong score.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-254
**Reference:** none read for this story: it is Rust against a committed corpus. Pattern to copy: `crates/vibe-app-server/src/tool_execution_parity_tests.rs` in full, including its `LEDGER`, its staleness check and its `MINIMUM_CASES` floor.

**Acceptance Criteria:**
- [ ] Given the committed corpus, when the replay runs, then it asserts the schema version and that the recorded commit equals `vibe_core::parity::REFERENCE_COMMIT`, failing when either drifts.
- [ ] Given a call case, when the replay runs, then this port's `EffectCallDisplay` is compared field by field and any difference fails unless a ledger entry covers that exact field on that exact case.
- [ ] Given a result case, when the replay runs, then this port's `EffectResultDisplay` is compared field by field under the same rule.
- [ ] Given the replay completes, when the case count is below 40, then it fails naming the count, so a shrunken corpus cannot pass as a green one.
- [ ] Given a ledger entry whose divergence no longer reproduces, when the staleness check runs, then it fails naming the entry.
- [ ] Given every ledger entry, when the audit test runs, then each names either a story ID in this PRD or the licensing boundary, and no entry is scoped wider than one field on one case.
- [ ] Given the reference checkout is absent or off-pin, when the live probe runs, then it prints the reason from `vibe_core::parity::off_pin_reason` and returns without failing, and the corpus replay still runs.
- [ ] Given the reference checkout is on-pin, when the live probe runs, then it recaptures into `target/` and asserts the fresh corpus equals the committed one.

---

### EP-081: The presentation of a tool with no dedicated UI class

Make a remote tool render the way the reference renders it, which is by its
class contract rather than by a name table.

**Definition of Done:** the presentation corpus replays with zero unledgered
divergences on the stub MCP tool and the stub connector tool, and
`settledMessage` is non-null on every published call display.

#### US-256: Route the call display by tool source
**Description:** As an operator watching an MCP call, I want the header to show the tool name the way the reference shows it, so that the terminal and the editor bridge render what the reference renders.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-255
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/ui.py:47-49` `format_call_display` for the summary a class with no override produces, `:34-37` `_display_name` for what it resolves to, `:111-132` for the adapter path and the fill-in rules, and `vibe/core/tools/remote.py:31-34` for the fact that `MCPTool` inherits `ToolUIData` and therefore never takes the `name(args)` branch.

**Acceptance Criteria:**
- [ ] Given a tool published by an MCP server, when its call display is built, then `summary` is the published tool name alone, with no argument rendering.
- [ ] Given a tool published by a connector, when its call display is built, then `summary` is the published tool name alone.
- [ ] Given a tool that is neither builtin nor remote, when its call display is built, then the generic fallback still applies and renders the first three arguments in insertion order.
- [ ] Given the generic fallback renders a non-string argument, when the summary is built, then a boolean renders as `True` or `False`, a null as `None`, and a nested object with single-quoted keys, matching what the reference's `repr` produces.
- [ ] Given a remote call arrived with no arguments, when its call display is built, then `summary` is still the published tool name and no empty parentheses are rendered.
- [ ] Given a remote call arrived with arguments that fail to decode, when its call display is built, then the display is produced without panicking and `summary` is still the published name.
- [ ] Given the presentation corpus, when the replay runs, then the MCP and connector call cases pass with no ledger entry.

#### US-257: Publish the status text the source names
**Description:** As an operator watching a call run, I want the loading indicator to name the remote server and tool, so that two concurrent MCP calls are distinguishable while they run.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-256
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/mcp/tools.py:310-312` and `:489-491` for the MCP status text over the HTTP and stdio proxies, `vibe/core/tools/connectors/connector_registry.py:313-316` for the connector one, `vibe/core/tools/remote.py:43-45` `get_remote_name` for which name they use, and `vibe/core/tools/ui.py:150-159` for the adapter fallback that applies to neither.

**Acceptance Criteria:**
- [ ] Given a tool published by an MCP server, when its call display is built, then `statusText` names the MCP tool by its remote name, unprefixed by the server alias.
- [ ] Given a tool published by a connector, when its call display is built, then `statusText` names the connector tool by its remote name, unprefixed by the connector alias.
- [ ] Given a builtin tool, when its call display is built, then `statusText` is unchanged from what `ToolEffectKind::status_text` publishes today.
- [ ] Given a remote tool whose remote name is empty, when its call display is built, then `statusText` falls back to the published name rather than rendering a dangling label.
- [ ] Given the presentation corpus, when the replay runs, then every `statusText` field matches with no ledger entry.

#### US-258: Fill the settled message at the source
**Description:** As a client reading the published protocol, I want `settledMessage` to carry a value on every call display, so that a client that does not reimplement this port's fallback renders the settled header correctly.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-256
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/ui.py:122-132`, where the adapter fills `message` from `summary` when it is `None`, `settled_message` from `summary` under the same rule, `verb` with the running default and `settled_verb` with the settled default, and applies all four to every tool rather than only to the fallback.

**Acceptance Criteria:**
- [ ] Given any tool, when its call display is built, then `settledMessage` is non-null.
- [ ] Given a tool whose presentation sets no settled message, when the display is built, then `settledMessage` equals `summary`, not `message`.
- [ ] Given a tool whose presentation sets a settled message, when the display is built, then that value is kept unchanged.
- [ ] Given a call whose arguments never arrived, when the display is built, then `message` and `settledMessage` both equal `summary` and neither is an empty string.
- [ ] Given a stored session recorded before this change, when it is hydrated, then a null `settledMessage` still deserializes and renders through the existing subject fallback.
- [ ] Given the presentation corpus, when the replay runs, then every `settledMessage` field matches with no ledger entry.

#### US-259: Settle a remote call from its remote result
**Description:** As an operator reading a finished MCP call, I want the settled header to name the remote tool and to report failure when the remote reported failure, so that a failed remote call does not render as a success.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-256
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/mcp/tools.py:300-308` for the MCP result display, `vibe/core/tools/connectors/connector_registry.py:302-311` for the connector one including its prefix, `vibe/core/tools/remote.py:23-29` for the `MCPToolResult` fields those two read, and `vibe/core/tools/ui.py:134-148` for the error and skip branches that run before either.

**Acceptance Criteria:**
- [ ] Given an MCP call that returned a result, when its result display is built, then `verb` is the settled running verb and `message` is the remote tool name.
- [ ] Given a connector call that returned a result, when its result display is built, then `message` carries the connector prefix ahead of the remote tool name.
- [ ] Given a remote result whose ok flag is false, when the result display is built, then `success` is false.
- [ ] Given a remote call that errored, when the result display is built, then `success` is false and `message` carries the error, taking precedence over the remote result branch.
- [ ] Given a remote call that was skipped, when the result display is built, then `success` is false and `message` carries the skip reason, or the default skip label when none was given.
- [ ] Given a remote call whose output does not deserialize into the remote result shape, when the result display is built, then `success` is false and no panic occurs.
- [ ] Given the presentation corpus, when the replay runs, then every remote result case matches with no ledger entry.

---

### EP-082: The description override an operator can write

Give `tool_paths` its first consumer and let an operator redescribe any
published tool the way the reference lets them.

**Definition of Done:** a description written to
`<tools-dir>/prompts/<name>.md` in a project directory, a user directory or a
`tool_paths` entry replaces the published description of the matching tool, with
later search paths winning, and a differential test proves the precedence order
against the reference resolution rules.

#### US-260: Resolve the tool search paths
**Description:** As an operator, I want `tool_paths`, the project tools directory and the user tools directory resolved in the reference order, so that a configuration key the schema accepts stops being ignored.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-255
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:146-162` `_compute_search_paths` for the order and the deduplication by resolved path, `:164-183` `_iter_tool_classes` for how a `.py` entry and a directory entry differ, `vibe/core/config/harness_files/_harness_manager.py:111-116` and `:140-143` for the user and project directories and the source gating that empties them.

**Acceptance Criteria:**
- [ ] Given a configuration with `tool_paths`, when the search paths are resolved, then they are the builtin directory, then each `tool_paths` entry in order, then each project tools directory, then each user tools directory.
- [ ] Given two entries resolving to the same directory, when the search paths are resolved, then the directory appears once, at the position of its first occurrence.
- [ ] Given the user configuration source is disabled, when the search paths are resolved, then no user tools directory is included.
- [ ] Given a `tool_paths` entry naming a directory that does not exist, when the search paths are resolved, then it is skipped without error.
- [ ] Given a `tool_paths` entry naming a `.py` file, when the search paths are resolved, then the entry is kept and its sibling directory is what the description reader will use.
- [ ] Given a `tool_paths` entry naming a path that cannot be canonicalized, when the search paths are resolved, then it is matched as written rather than dropping the whole list.
- [ ] Given a relative `tool_paths` entry, when the search paths are resolved, then it resolves against the session working directory, not the process working directory.

#### US-261: Apply the description override to the published surface
**Description:** As an operator, I want a description file under a tools directory to replace the description the model reads, so that a builtin or an MCP tool can be redescribed without patching the binary.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-260
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:227-259` `_iter_tool_descriptions` for the key, the precedence and the blank-file rule, and `:599-618` `available_tool_specs` for the point at which the override wins over the tool's own description.

**Acceptance Criteria:**
- [ ] Given `<tools-dir>/prompts/<name>.md` exists and is not blank, when the tool surface is published, then the tool named by the file stem carries that file's text as its description.
- [ ] Given the same stem exists in two search paths, when the tool surface is published, then the text from the later search path wins.
- [ ] Given the file is empty or contains only whitespace, when the tool surface is published, then the tool keeps its own description rather than publishing an empty one.
- [ ] Given the file cannot be read, when the tool surface is published, then the tool keeps its own description and the session does not fail.
- [ ] Given a file stem matching no published tool, when the tool surface is published, then it is ignored without error.
- [ ] Given a file stem matching an MCP or connector tool by its published name, when the tool surface is published, then that remote tool's description is replaced too.
- [ ] Given a description file is added while a session is running, when the surface is published again, then the change is picked up rather than served from a stale cache.
- [ ] Given a search path holds 50 description files, when the surface is published, then resolution completes in under 5 ms measured by a test that fails above that bound.

---

### EP-083: The three contracts nothing measures

Correct the three row-4 behaviors that no oracle covers, each against its
reference call site, each with the measurement that would have caught it.

**Definition of Done:** the `enabled_tools` gate, the sensitive-pattern matcher
and the argument rejection message each have a differential test that fails when
the behavior regresses, and each passes against the reference.

#### US-262: Gate `enabled_tools` on the written list
**Description:** As an operator, I want an `enabled_tools` list that matches nothing to publish nothing, so that a malformed allowlist fails closed rather than open.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-255
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:295-322` `available_tools`, where the narrowing runs on the truthiness of the raw list and the matching runs through `name_matches`, and `vibe/core/utils/matching.py:16-35`, where a blank entry and an uncompilable regex are both skipped during matching rather than during the gate.

**Acceptance Criteria:**
- [ ] Given `enabled_tools` carries only blank entries, when the surface is published, then no tool is published.
- [ ] Given `enabled_tools` carries only an uncompilable `re:` entry, when the surface is published, then no tool is published.
- [ ] Given `enabled_tools` is absent or an empty list, when the surface is published, then every available tool is published.
- [ ] Given `enabled_tools` carries one blank entry and one matching glob, when the surface is published, then the tools matching the glob are published and the blank entry narrows nothing further.
- [ ] Given `disabled_tools` carries only blank entries, when the surface is published, then no tool is withheld, matching the reference's separate treatment of the deny list.
- [ ] Given a name matched by both lists, when the surface is published, then it is withheld, because the deny list is applied last.
- [ ] Given a differential test over the four gate combinations, when it runs, then it compares this port's published names against the reference's for the same configuration.

#### US-263: Match sensitive patterns the way the reference matches them
**Description:** As an operator, I want a `sensitive_patterns` entry to guard the same files it guards under the reference, so that an approval prompt appears in the same cases.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-255
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/utils.py:105-119` for the sensitive loop and its first-match break, `:30-46` `resolve_path_permission` for the allow and deny lists, which stay on the other matcher and must not be changed, and `:70-104` for the order the whole chain runs in.

**Acceptance Criteria:**
- [ ] Given a pattern with no separator, when a path whose final component matches it is checked, then the sensitive requirement is raised, where the current matcher raises nothing.
- [ ] Given a pattern with a leading separator, when a path with more components than the pattern is checked, then no sensitive requirement is raised, where the current matcher raises one.
- [ ] Given a relative pattern of two components, when a path whose last two components match is checked, then the requirement is raised regardless of how deep the path is.
- [ ] Given the shipped defaults, when a dotfile environment path and a suffixed environment path are checked, then both still raise the requirement, so the default behavior is unchanged.
- [ ] Given a pattern list where two entries match, when the chain runs, then exactly one requirement is raised, matching the reference's break after the first match.
- [ ] Given the allow list and the deny list, when they are matched, then they still use the unanchored matcher, and a test asserts the two matchers stay distinct.
- [ ] Given the permission oracle, when it is extended, then it captures at least six pattern and path pairs chosen to separate the two matchers, and the committed corpus records the reference verdict for each.
- [ ] Given a pattern that is not valid under either matcher, when the chain runs, then no requirement is raised and no panic occurs.

#### US-264: Measure and hold the argument rejection message
**Description:** As a model reading a rejected call, I want the rejection to name the tool the way the reference names it, so that the text a turn is retried from carries the same information.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-255
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py:236-242`, where `invoke` validates against the argument model and wraps the validation failure in a tool error naming the tool. Read the wrapper's shape and the naming, not the validator's own rendering.

**Acceptance Criteria:**
- [ ] Given a rejected argument fixture, when the capture runs, then the corpus records the error type and a structural summary of the message: whether the tool name appears, which argument pointers appear, and a digest of the full text.
- [ ] Given a rejected argument fixture, when the replay runs, then this port's error is compared against that structural summary and a difference fails unless a ledger entry covers it.
- [ ] Given a rejected call, when the error reaches the model, then the message names the tool that rejected it.
- [ ] Given a rejected call naming several invalid arguments, when the error is built, then every invalid argument pointer appears, not only the first.
- [ ] Given an accepted argument fixture, when the replay runs, then no error is produced and the fixture count is unchanged from 92.
- [ ] Given the recorded message would carry reference-authored text, when the corpus is written, then only the digest and the structural markers are stored.

---

### EP-084: The surface order and the connector gate

Close the two remaining `manager.py` behaviors, one cosmetic and one that
changes which tools a session publishes.

**Definition of Done:** the published tool order matches the reference's
discovery order, and a connector's configuration entry decides whether its tools
are published, with a stale entry inert rather than fatal.

#### US-265: Publish tools in discovery order
**Description:** As a person comparing two transcripts, I want the tool list published in the reference's order, so that a diff of two model requests is not dominated by ordering noise.

**Priority:** P2
**Size:** S (2 pts)
**Dependencies:** Blocked by US-255
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:599-618` `available_tool_specs`, which iterates the registry's insertion-ordered map, and `:295-322`, which preserves that order through both filters.

**Acceptance Criteria:**
- [ ] Given a session with builtins only, when the surface is published, then the order is the discovery order, not the alphabetical order.
- [ ] Given a session with builtins and MCP tools, when the surface is published, then the MCP tools follow the builtins in the order they were integrated.
- [ ] Given a session with connectors, when the surface is published, then the connector tools follow the MCP tools.
- [ ] Given two tools registered with the same name and different priorities, when the surface is published, then the selected variant occupies the position of the first registration of that name.
- [ ] Given a tool is withheld by a filter, when the surface is published, then the remaining order is unchanged from what it would have been.
- [ ] Given the same session is opened twice with no configuration change, when both surfaces are published, then the two orders are identical.

#### US-266: Decide connector publication from the configuration entry
**Description:** As an operator, I want a connector with no configuration entry to publish nothing and a stale disabled-tool entry to be inert, so that the same configuration file yields the same connector surface under both clients.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-255
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:415-441` `_build_source_disable_index`, where a connector known to the registry but absent from the configuration is added to the disabled set, and `:443-457` `_is_source_disabled`, which keys on the remote name rather than the published one; corroborated by `vibe/core/tools/connectors/counts.py:10-25`, which counts a connector as connected only when its configuration entry exists and is not disabled.

**Acceptance Criteria:**
- [ ] Given a connector known to the registry with no matching configuration entry, when the surface is published, then none of its tools are published.
- [ ] Given a connector whose configuration entry sets the disabled flag, when the surface is published, then none of its tools are published.
- [ ] Given a connector entry listing a disabled tool by its remote name, when the surface is published, then that tool is withheld and the connector's other tools are published.
- [ ] Given a connector entry listing a disabled tool that the connector does not expose, when the session initializes, then the entry is ignored and initialization succeeds, where it currently fails.
- [ ] Given a connector entry that both sets the disabled flag and lists disabled tools, when the surface is published, then the connector is fully withheld and the list is not consulted.
- [ ] Given a previously persisted session state disagrees with the configuration entry, when the surface is published, then the configuration entry wins.
- [ ] Given the change alters which connectors publish by default, when it lands, then `CHANGELOG.md` records it under `## Unreleased` as a behavior change for operators.

---

### EP-085: Restate row 4 from its oracle

Close the row by naming what was measured and what will never be measured.

**Definition of Done:** row 4 reads 100 with a cell that names its oracle and its
counts; the two permanent divergences are recorded by name; no row-4 claim rests
on a read rather than a measurement.

#### US-267: Record the permanent divergences by name
**Description:** As a reader of the scorecard, I want the parts of row 4 that will never reach parity written down with their reason, so that a score of 100 is not read as a claim that nothing differs.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-261, Blocked by US-264, Blocked by US-266
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:164-225` `_iter_tool_classes`, whose `.py` import path is the one behavior this port cannot reproduce, and `:260-278` `discover_tool_defaults`, which reads its defaults from the same imported classes.

**Acceptance Criteria:**
- [ ] Given `## Accepted divergences` in `docs/parity.md`, when it is read, then it carries an entry for Python tool class loading, naming what the reference does, what this port does instead, and why the difference is permanent.
- [ ] Given that entry, when it is read, then it states that `tool_paths` and the tools directories are honored for descriptions and ignored for implementations, so an operator knows which half works.
- [ ] Given `ToolSource::Custom`, when the entry is read, then it names the variant as reserved with no producer, or the variant is removed, and the entry says which was chosen.
- [ ] Given each entry, when it is read, then it names the reference symbol and path it diverges from, so a future reader can verify it at the pin.
- [ ] Given a divergence recorded here is later closed, when the row is next remeasured, then the entry is removed rather than left standing.

#### US-268: Remeasure and restate row 4
**Description:** As a reader of the scorecard, I want row 4 restated from the widened measurement, so that its score names how it was reached.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-255, Blocked by US-257, Blocked by US-258, Blocked by US-259, Blocked by US-262, Blocked by US-263, Blocked by US-265, Blocked by US-267
**Reference:** none read for this story: it is documentation against measurements this PRD produced.

**Acceptance Criteria:**
- [ ] Given the full CI sequence, when it is run from the workspace root, then all four commands pass with no filtered test selection.
- [ ] Given row 4 of `docs/parity.md`, when it is rewritten, then it names the presentation oracle, its case count, the extended permission cases and the extended argument fixtures, each with the number measured.
- [ ] Given row 4's score, when it is written, then it is 100 and the cell names what would have to break for it to fall.
- [ ] Given rows 11, 12 and 21, when they are reviewed, then each states whether the contracts fixed here changed its score, and any change is stated with its measurement.
- [ ] Given `## Method` in `docs/parity.md`, when it is read, then the presentation oracle appears alongside the existing capture scripts.
- [ ] Given `CHANGELOG.md`, when it is read, then every user-visible change from this PRD appears under `## Unreleased`.
- [ ] Given the reference checkout is absent, when the full test suite runs, then it still passes, and a test asserts that the presentation replay ran from the corpus.

## Functional Requirements

- FR-01: The system must publish a call display whose `summary` for a remote
  tool is the published tool name alone.
- FR-02: The system must publish a `statusText` that names the remote tool for
  MCP and connector calls, and the effect kind for builtin calls.
- FR-03: The system must publish a non-null `settledMessage` on every call
  display.
- FR-04: The system must report a remote call as failed when the remote result
  reports failure.
- FR-05: The system must read `<tools-dir>/prompts/<name>.md` from the builtin
  directory, each `tool_paths` entry, each project tools directory and each user
  tools directory, in that order, and must let a later path win.
- FR-06: The system must NOT publish a blank description because a description
  file was empty.
- FR-07: When `enabled_tools` is a non-empty list, the system must publish only
  the tools it matches, including publishing none when it matches none.
- FR-08: The system must match `sensitive_patterns` with right-anchored
  component matching, and must NOT use that matcher for the allow and deny
  lists.
- FR-09: The system must name the rejecting tool in every argument validation
  error returned to the model.
- FR-10: The system must publish tools in discovery order.
- FR-11: The system must withhold every tool of a connector that has no
  configuration entry or whose entry is disabled.
- FR-12: The system must NOT fail session initialization because a connector's
  configured disabled-tool entry names a tool the connector does not expose.
- FR-13: Every corpus committed by this work must carry the pinned reference
  commit and must fail its replay when that commit and
  `vibe_core::parity::REFERENCE_COMMIT` disagree.
- FR-14: No corpus committed by this work may contain reference-authored prose
  in cleartext.

## Non-Functional Requirements

- **Performance:** description override resolution completes in under 5 ms for a
  search path holding 50 files, asserted by a test that fails above that bound.
  Publishing the tool surface for 12 builtins plus 20 remote tools completes in
  under 10 ms.
- **Performance:** the presentation corpus replay adds under 2 seconds to
  `cargo test --workspace --all-features` on the CI runner.
- **Security:** a `sensitive_patterns` change must not widen access: a
  differential test asserts that every path the reference guards under a given
  pattern is also guarded here, with zero paths guarded upstream and unguarded
  here. `enabled_tools` fails closed: a list that compiles to zero usable
  patterns publishes zero tools.
- **Reliability:** a missing or off-pin reference checkout never fails
  `cargo test`; the corpus replay runs unconditionally and the live probe skips
  with a printed reason.
- **Reliability:** every capture script is byte-identical across two consecutive
  runs with no change in between, verified by its `--check` mode.
- **Reliability:** a session whose configuration names an unreadable description
  file or an unresolvable `tool_paths` entry still opens; the entry is skipped.
- **Compatibility:** a session recorded before this work hydrates without error,
  including one whose stored call display carries a null `settledMessage`.
- **Maintainability:** no ledger entry is scoped wider than one field on one
  case, and every entry names a story ID or the licensing boundary.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Reference checkout absent | `VIBE_REFERENCE` unset and default path missing | Corpus replays; live probe skips | Printed skip reason naming the path and the override |
| 2 | Reference checkout off-pin | Local checkout at another commit | Corpus replays; live probe skips | Printed reason naming both commits and the restore command |
| 3 | Empty description file | `prompts/read_file.md` is blank | Tool keeps its own description | None |
| 4 | Unreadable description file | Permissions deny the read | Tool keeps its own description, session opens | None |
| 5 | Description file for an unknown tool | `prompts/weather.md` with no `weather` tool | Ignored | None |
| 6 | `enabled_tools` of blanks | `enabled_tools = ["  "]` | Zero tools published | Model receives an empty tool list |
| 7 | `enabled_tools` with an uncompilable regex | `enabled_tools = ["re:("]` | Zero tools published | Model receives an empty tool list |
| 8 | Sensitive pattern matching nothing under either matcher | Malformed pattern | No requirement raised, no panic | None |
| 9 | Remote call with undecodable arguments | Malformed JSON from the model | Call display built from the published name | Header shows the tool name |
| 10 | Remote result that fails to deserialize | Server returns an unexpected shape | Result display reports failure | Settled header shows failure |
| 11 | Connector with no configuration entry | Registry knows it, config does not | Its tools are not published | None |
| 12 | Stale connector disabled-tool entry | Entry names a tool the connector dropped | Entry ignored, session opens | None |
| 13 | Duplicate search paths | `tool_paths` repeats the project directory | Directory read once | None |
| 14 | Relative `tool_paths` entry | `tool_paths = ["./tools"]` | Resolved against the session working directory | None |
| 15 | Capture attempts a network call | A tool constructor reaches out | Capture fails, no partial corpus written | Error naming the attempted destination |
| 16 | Corpus schema drift | A capture adds a field without a version bump | Replay fails | Error naming the expected and found versions |
| 17 | Stale ledger entry | A divergence was fixed but its entry remains | Staleness check fails | Error naming the entry |
| 18 | Session recorded before this work | Stored display has a null settled message | Hydrates and renders through the subject fallback | None |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Changing remote call summaries regresses the TUI transcript or the ACP bridge, both of which read the display today | Med | High | US-255 lands the corpus first; `crates/vibe-cli/src/tui/transcript.rs` and `crates/vibe-acp/src/tests/updates.rs` already assert on these fields, so a regression fails the existing tests before the new ones |
| 2 | `PurePath.match` semantics are subtler than the component rule assumed, so the new matcher diverges in a third direction | Med | High | US-263 extends the permission oracle first and captures the reference verdict for at least six separating cases, so the implementation is written against measurements rather than against a reading |
| 3 | Building a stub MCP tool inside the capture pulls in the MCP client library and fails to import in the pinned tree | Med | Med | US-254 builds the class through the reference's own factory with a hand-written remote descriptor; if the import fails, the epic falls back to capturing the base class contract and the ledger records the remote half as unmeasured with a named reason |
| 4 | Making connector publication opt-in withholds connectors that operators rely on today | Med | High | US-266 records the change in `CHANGELOG.md` as a behavior change; the criterion set makes the configuration entry authoritative over persisted state so the fix is discoverable rather than silent |
| 5 | Publishing in discovery order destabilizes a snapshot test or a corpus that assumed alphabetical order | Low | Med | US-265 is P2 and sequenced last among the behavioral epics; the full suite is run rather than a filtered selection, per the oracle rule |
| 6 | Row 4 turns out to have a sixth gap this read missed, so 100 is claimed too early | Med | High | US-268 requires the full CI sequence unfiltered and requires the row to name what would have to break; the presentation oracle covers the one file that had none, which is where the unknown most plausibly lives |
| 7 | A captured display field carries reference prose and reaches the repository | Low | High | US-254 stores prose as digests with a structural marker, and US-255 audits the corpus for cleartext, mirroring the existing digest test on the tool surface |
| 8 | The description override reads a file on every publication and shows up in session-open latency | Low | Low | US-261 asserts a 5 ms bound on 50 files; if it fails, a per-session cache invalidated on configuration reload is the fallback |

## Non-Goals

- Loading Python tool classes from `.py` files. Executing reference source is
  forbidden by `NOTICE` and embedding an interpreter is out of proportion. This
  is recorded as an accepted divergence by US-267 instead.
- A plugin system for custom tools in any other language. `ToolSource::Custom`
  stays a reserved variant or is removed; no producer is written.
- Re-pinning the reference. The pin stays at `b78b451`; a bump would require
  regenerating every committed corpus in the same change.
- Byte-identical tool description text. The licensing boundary makes that
  permanently impossible and it is already an accepted divergence on the
  scorecard.
- Rows 11, 12 and 21 beyond the contracts row 4 owns. US-268 states whether
  their scores moved; it does not take them to 100.
- Reworking `ToolEffectKind` into a class-keyed hierarchy. The name-keyed
  vocabulary is the stronger internal design and is retained; only the routing
  of remote tools changes.
- Windows-specific presentation. The presentation corpus is captured on Linux,
  matching the existing tool-surface baseline's platform field.

## Files NOT to Modify

- `crates/vibe-core/src/parity.rs`: carries the pin; changing it invalidates
  every committed corpus at once.
- `scripts/parity/pin.py`: the second pin source; the parity test fails when
  the two disagree or when a third copy appears.
- `NOTICE`: declares the licensing boundary this work operates under.
- `crates/vibe-app-server/tests/tool-surface/baseline.json`: all three sets are
  empty and must stay empty; this work adds a corpus rather than reopening that
  one.
- `crates/vibe-core/tests/tool-config/defaults.json`: the per-tool
  configuration corpus replays at full count and nothing here changes it.
- `crates/vibe-core/src/matching.rs`: verified conformant to `name_matches`;
  US-262 changes the gate at the call site, not the matcher.
- `vibe/**` in the reference checkout: read-only oracle, never written.

## Technical Considerations

- **Architecture:** should the remote presentation be routed by a `ToolSource`
  carried on the effect detail, or by a presentation trait resolved at
  registration? Recommended: carry the source, because the projection already
  builds the display from the tool name and the arguments and adding one field
  is smaller than inverting the ownership. Engineering to confirm the source is
  available at the point `EffectDetail` is built.
- **Data Model:** the presentation corpus needs a shape for a field that is a
  digest and a field that is cleartext. Options: a tagged union per field, or a
  parallel map of digested field names. Trade-off: the union is self-describing
  and larger, the parallel map is compact and easier to get wrong. Recommended:
  the tagged union, matching how the tool-surface digest already marks described
  fields.
- **API Design:** should `settledMessage` become non-optional on the wire?
  Recommended: keep it optional in the type so a session recorded before this
  work still deserializes, and assert non-null at construction. Engineering to
  confirm no client depends on the null.
- **Dependencies:** none new. The component matcher for `sensitive_patterns` is
  written against the existing `matches_glob` in
  `crates/vibe-core/src/matching.rs` rather than pulling in a path-glob crate,
  because the port already owns a conformant `fnmatch`.
- **Migration:** no persisted state changes shape. Sessions recorded before this
  work hydrate unchanged. The one behavior change an operator can observe is
  connector publication becoming opt-in, which US-266 records in the changelog.
  Rollback is per-epic: each behavioral epic is independent of the others once
  EP-080 has landed.
- **Sequencing:** EP-080 blocks everything. EP-081 through EP-084 are mutually
  independent and can run in parallel. EP-085 requires all of them.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Row 4 score | 95, unexplained | 100, with a named oracle | Month-1 | `docs/parity.md` row 4 |
| Presentation cases replayed | 0 | 40 or more | Month-1 | Case count printed by the replay test |
| Row-4 reference files with an executable oracle | 4 of 5 (`ui.py` has none) | 5 of 5 | Month-1 | `scripts/parity/` inventory |
| Unledgered presentation divergences | Unknown (unmeasured) | 0 | Month-1 | Replay test failure count |
| Permission oracle cases separating the two matchers | 0 | 6 or more | Month-1 | `crates/vibe-core/tests/permission-surface/` case count |
| Configuration keys accepted and ignored in row 4's scope | 1 (`tool_paths`) | 0 | Month-1 | Grep for the key's consumers |
| Row-4 gaps with no entry in a divergence table | 5 points' worth | 0 | Month-1 | `docs/parity.md` divergence sections |
| Full CI sequence, unfiltered | Passing | Passing | Month-6 | Four commands from the workspace root |

## Open Questions

- Should `ToolSource::Custom` be removed or documented as reserved? Owner:
  Arthur Jean, before US-267. Removing it is cleaner; keeping it documents the
  shape a future plugin system would take. US-267 requires the choice to be
  stated either way.
- Should connector publication becoming opt-in ship behind a transitional
  warning for one release? Owner: Arthur Jean, before US-266. Blocking question
  for that story only; the rest of EP-084 does not depend on it.
- Does any external ACP client read `settledMessage` and depend on its null?
  Owner: Arthur Jean, before US-258. If one does, the fill happens at the
  projection boundary rather than in the type.
- Should the presentation oracle also capture the Windows tool families, the way
  the tool-surface digest does? Owner: Arthur Jean, before US-254. Deferred by
  default; the corpus records its platform so a later Windows capture is
  additive.
[/PRD]
