[PRD]
# PRD: Built-in Tools at Full Parity

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-20 | Arthur Jean | Initial draft: take parity row 3 from 92 to 100 |

## Problem Statement

Row 3 of `docs/parity.md` ("Built-in tools") scores 92, restated down from 98 on
2026-08-19. The row names its own reason: "the execution oracle covers
`read_file`, `grep`, `write_file`, `edit` and `todo`, and two published results
outside its reach diverge". A read of both trees at reference commit `b78b451`
confirms the surface is complete and finds that the gap is not in the surface at
all. Names, schemas, argument fixtures and the description digest are all at
full count. What is missing is that six of the eleven in-scope tools are never
executed against the reference, and every one of them diverges:

1. **Six tools of eleven never reach the oracle.** `tool_classes()` in
   `scripts/parity/tool_execution.py:423` imports exactly five classes: `Edit`,
   `Grep`, `ReadFile`, `Todo`, `WriteFile`. The corpus holds 41 cases over those
   five (`crates/vibe-app-server/tests/tool-execution/corpus.json`) and zero over
   `skill`, `web_fetch`, `web_search`, `task`, `ask_user_question` and
   `exit_plan_mode`. Each of the six was left out for a mechanical reason, a
   context dependency or a network dependency, never for a parity reason. The
   five conforming tools are exactly the five measured ones, which is the shape
   of an instrument gap rather than a coincidence.
2. **Five of those six break the universal rendering contract.**
   `vibe/core/agent_loop/_loop.py:2225-2228` renders every tool result the same
   way, one `field: value` line per key of `model_dump(mode="json")`, with
   `get_result_extra` appended after a blank line, and it has no per-tool
   exception. Conformant here: `read_file`, `write_file`, `edit`, `grep`, `todo`,
   `skill`. Divergent: `web_fetch` sends the body alone
   (`crates/vibe-core/src/tools/builtins/web_fetch.rs:184`), `web_search` the
   answer alone (`crates/vibe-core/src/tools/builtins/web_search.rs:124`), `task`
   the response alone
   (`crates/vibe-app-server/src/client/live/delegation.rs:181`),
   `ask_user_question` one `question: answer` line per question
   (`crates/vibe-app-server/src/client/interactive.rs:469`) where the reference
   renders `answers` and `cancelled`, and `exit_plan_mode` the message alone
   (`crates/vibe-app-server/src/client/interactive.rs:622`) where the reference
   renders `switched` and `message`. The model therefore never reads `url`,
   `content_type`, `was_truncated`, `sources`, `turns_used`, `completed`,
   `cancelled` or `switched` on any turn.
3. **The projection layer is absent on both tools that override it.**
   `project_result` (`vibe/core/tools/ui.py:72`, published into
   `ToolResultPresentation.projected_output` at `ui.py:181`) is a second result
   shape, distinct from `model_dump()`. `grep` overrides it to add
   `parsed_matches` (`vibe/core/tools/builtins/grep.py:175`) and `edit` overrides
   it to return `{file, old_string, new_string, occurrences}` with a
   `{start_line, old_text, new_text}` entry per occurrence and no `message`
   (`vibe/core/tools/builtins/edit.py:128`). This port publishes neither, yet the
   app-server census declares both models (`FileEditEffectOutput` and
   `FileSearchEffectOutput` in
   `crates/vibe-app-server/tests/app-server-surface/corpus.json`) and the TUI
   renderer `occurrence_diff_lines`
   (`crates/vibe-cli/src/tui/transcript.rs:666`) reads `occurrences` behind a
   permanent fallback. It is a renderer with no producer. The census does not
   catch it because the `edit` effect probe hand-writes its payload
   (`crates/vibe-app-server/src/app_server_surface_parity_tests.rs:1165`) instead
   of taking what the tool emits.
4. **`task` is the one builtin outside the permission policy.** The handler is
   registered bare at `crates/vibe-app-server/src/client/live/delegation.rs:190`
   where every other builtin is wrapped in `PolicyGuardedTool`
   (`crates/vibe-core/src/tools/builtins.rs:211-283`). The reference declares
   permission `ASK` with allowlist `[explore]` and resolves it per call through
   fnmatch (`vibe/core/tools/builtins/task.py:77`). This port declares the same
   allowlist and denylist at `crates/vibe-core/src/tools/config.rs:380-385` and
   reads neither, so `tools.task.denylist` cannot deny anything.
5. **The delegation guard differs in shape, not only in value.** The reference
   caps depth at 1, enforces it at run time with an error the model reads
   (`vibe/core/tools/builtins/task.py:88`), and keeps `task` advertised to the
   child. This port removes `task` from the child's tool list
   (`crates/vibe-app-server/src/client/live/delegation.rs:231`) and caps at
   `MAX_DELEGATION_DEPTH = 3` (`crates/vibe-core/src/extensions.rs:32`). A
   subagent asked to delegate sees no tool where the reference shows one and
   answers with a refusal it composed itself.
6. **`web_fetch` diverges on three edges no table records.** A `timeout` above
   `max_timeout` raises upstream (`vibe/core/tools/builtins/web_fetch.py:159`)
   and is silently clamped here
   (`crates/vibe-core/src/tools/builtins/web_fetch.rs:89-104`), so an argument
   the reference refuses is accepted. Truncation cuts at `max_content_bytes`
   upstream and at `min(max_content_bytes, remaining_bytes)` here
   (`web_fetch.rs:170`) with a different marker. The request carries `Accept` and
   `Accept-Language` upstream (`web_fetch.py:172-176`) and retries once on HTTP
   403 with a `cf-mitigated: challenge` header (`web_fetch.py:208-210`); this
   port sends neither header and never retries, so a Cloudflare-fronted page the
   reference reads fails here.

**Why now:** the score was restated down on 2026-08-19 on the strength of two
divergences visible from outside the oracle. That restatement was a lower bound,
not a measurement: this read finds five more of the same kind in the same blind
spot. Every further claim about row 3 will be argued rather than measured until
the instrument covers all eleven tools, and every day the blind spot stays open
is a day a new tool can be added inside it. The projection gap is already
shipping a renderer that no producer feeds.

## Overview

Widen the tool execution oracle from five tools to eleven and from one result
shape to two, then close every divergence the widened oracle exposes, then
restate row 3 from the measurement rather than from a read. The work is
instrument-first: the capture script and its replay land before any behavior
changes, so each subsequent change is proven by a corpus that already existed
when the change was written.

## Goals

1. `scripts/parity/tool_execution.py` drives all eleven in-scope tools and
   records both the rendered model text and the projected result.
2. Every in-scope tool renders its result to the model as the reference does,
   one field per line over the published result model.
3. `grep` and `edit` publish the projection the reference publishes, and the TUI
   renderer that already reads it stops depending on a fallback.
4. `task` resolves through the permission policy and reads its configured
   allowlist and denylist.
5. `web_fetch` refuses an over-cap timeout, truncates at the declared bound, and
   makes the request the reference makes.
6. Row 3 of `docs/parity.md` is restated to 100 from the widened oracle, with
   every surviving divergence held by a ledger entry or an accepted-divergence
   row.

## Target Users

| User | Pain today | Workaround today |
|---|---|---|
| The person reading the scorecard | Row 3's number is a lower bound derived from a read, not from a measurement, and the row says so in its own text | Re-reading both trees by hand, which is what produced this PRD and does not scale |
| An agent implementing in `vibe/core/tools/builtins/` | Six tools have no differential test, so a change to any of them is unverifiable at the boundary that matters | Reading the reference module before editing, which catches shape but never rendering |
| CI | `cargo test --workspace --all-features` is green while five published results diverge, so green does not mean parity for this row | None: the failure is invisible to the suite |
| An operator of the TUI | `edit` occurrences never render as a diff because nothing produces them, and `web_fetch` accepts a timeout the reference refuses | Reading the raw result text |

## Research Findings

No competitive landscape applies: this row has a single oracle, the Python
reference pinned at `b78b451c39eab9213393ad2f45908e8562a5c5e7` (v2.24.0), read
module by module for this PRD. Findings that shaped the decisions:

- The rendering contract at `_loop.py:2225-2228` is universal and unconditional.
  `model_dump` is called without `by_alias`, so a camelCase-aliased model still
  renders snake_case field names. Any per-tool rendering here is a divergence by
  construction, not a judgment call.
- `project_result` is overridden by exactly two builtins. Every other tool
  inherits the identity projection, so the projection family is small and
  bounded.
- The tool surface oracle replaces every description with `DESCRIBED` in both
  capture and replay (`scripts/parity/tool_surface.py`), so the 11 in-scope
  `prompts/*.md` files carry zero scoring weight. Prose is out of scope by
  construction, not by concession.
- The four context-dependent tools each need one collaborator and nothing else:
  `skill` a `SkillManager`, `ask_user_question` and `exit_plan_mode` a scripted
  answer source, `task` a `SubagentRunnerPort`. None needs a model.
- The existing replay already carries the ledger machinery this work needs:
  `LEDGER` at `crates/vibe-app-server/src/tool_execution_parity_tests.rs:78`,
  a staleness check at line 610, and a `MINIMUM_CASES` floor at line 60.

## Assumptions & Constraints

- **A1.** The reference checkout stays readable at the pin through `git show` and
  `git archive` even when the working tree is at another revision. Verified: the
  local checkout is at v2.24.2 and every reading for this PRD went through the
  pin.
- **A2.** The four context-dependent tools can be driven with a scripted context
  and no model. Risk: MEDIUM, validated by US-239 itself, whose first criterion
  is that each of the four returns a result.
- **A3.** A loopback HTTP server can serve both network tools deterministically.
  Risk: MEDIUM, validated by US-240.
- **A4.** `NOTICE` forbids reproducing reference message text. Every result
  string that is authored prose stays this port's own, held by a ledger entry
  scoped to one field.
- **A5.** Row 3 owns the eleven non-shell builtins plus `vibe/questions.py` and
  `vibe/core/subagents.py`. The shell families belong to row 6 and
  `vibe/core/tools/ui.py` to row 4. Only the per-tool overrides of `ui.py` hooks,
  which live in row-3 files, are treated as row 3's.
- **C1.** The reference checkout is read-only. No story writes to it.
- **C2.** A missing reference checkout must never fail `cargo test`. Live probes
  skip; corpus replays run unconditionally.
- **C3.** Layering holds: `vibe-protocol`/`vibe-core` then `vibe-app-server` then
  `vibe-cli`/`vibe-acp`.

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
git -C /home/arthur/dev/mistral-vibe show b78b451:vibe/core/tools/builtins/task.py
git -C /home/arthur/dev/mistral-vibe archive b78b451 vibe/ | tar -x -C <scratch>
```

**Every line number below is anchored at the pin.** The local checkout may sit at
another revision, where the same symbol has moved.

**Every `vibe/...` path in this document resolves against that root**, in the
table below and in each story's `Reference:` line alike, and is read at the pin
rather than from the working tree. The root is spelled in full once per story so
a reader who opens a single story can navigate without scrolling back here.

| Symbol | Path at the pin | Line | Read by |
|---|---|---|---|
| result rendering | `vibe/core/agent_loop/_loop.py` | 2225-2228 | US-239, US-240, US-243, US-244, US-245 |
| `project_result` | `vibe/core/tools/ui.py` | 72 | US-241 |
| `get_result_presentation` | `vibe/core/tools/ui.py` | 177-181 | US-241 |
| `Grep.project_result` | `vibe/core/tools/builtins/grep.py` | 175 | US-241, US-246 |
| `GrepResult.parsed_matches` | `vibe/core/tools/builtins/grep.py` | 141-159 | US-246 |
| `Edit.project_result` | `vibe/core/tools/builtins/edit.py` | 128 | US-241, US-247 |
| `EditResult.ui_start_lines` | `vibe/core/tools/builtins/edit.py` | 58-62 | US-247 |
| `SkillResult` | `vibe/core/tools/builtins/skill.py` | 32-40 | US-239, US-252 |
| `already_loaded_result` | `vibe/core/tools/builtins/skill.py` | 85 | US-239, US-252 |
| `_MAX_LISTED_FILES` | `vibe/core/tools/builtins/skill.py` | 21 | US-239 |
| `TaskToolConfig` | `vibe/core/tools/builtins/task.py` | 24 | US-248 |
| `Task.resolve_permission` | `vibe/core/tools/builtins/task.py` | 77 | US-248 |
| `Task.run` depth guard | `vibe/core/tools/builtins/task.py` | 88 | US-249 |
| `TaskArgs` / `TaskResult` | `vibe/core/subagents.py` | 16-32 | US-239, US-244 |
| `SubagentRunnerPort` | `vibe/core/subagents.py` | 34 | US-239 |
| `SubagentRunAccumulator` | `vibe/core/subagents.py` | 41-76 | US-244 |
| `WebFetchResult` | `vibe/core/tools/builtins/web_fetch.py` | 56 | US-243 |
| `WebFetchConfig` | `vibe/core/tools/builtins/web_fetch.py` | 63-67 | US-250 |
| truncation | `vibe/core/tools/builtins/web_fetch.py` | 132-137 | US-250 |
| `_validate_args` | `vibe/core/tools/builtins/web_fetch.py` | 146-161 | US-250 |
| `_resolve_timeout` | `vibe/core/tools/builtins/web_fetch.py` | 164-167 | US-250 |
| request headers | `vibe/core/tools/builtins/web_fetch.py` | 172-176 | US-251 |
| challenge retry | `vibe/core/tools/builtins/web_fetch.py` | 199-210 | US-251 |
| `WebSearchResult` | `vibe/core/tools/builtins/web_search.py` | 45 | US-243 |
| `WebSearch.is_available` | `vibe/core/tools/builtins/web_search.py` | 67 | US-240 |
| `ExitPlanModeResult` | `vibe/core/tools/builtins/exit_plan_mode.py` | 34 | US-245, US-252 |
| `ExitPlanMode.run` | `vibe/core/tools/builtins/exit_plan_mode.py` | 64-97 | US-245 |
| `AskUserQuestion` config | `vibe/core/tools/builtins/ask_user_question.py` | 23 | US-239 |
| `UserQuestionResult` | `vibe/questions.py` | 66 | US-245 |
| `UserAnswer` | `vibe/questions.py` | 58 | US-245 |

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
python3 scripts/parity/tool_execution.py --check   # re-run must be byte-identical
```

## Epics & User Stories

### EP-074: An oracle over all eleven tools

Widen the instrument before changing any behavior, so every later epic is proven
by a corpus that predates it.

**Definition of Done:** `scripts/parity/tool_execution.py` drives eleven tools
and records the projected result alongside the rendered text; the committed
corpus replays in `cargo test --workspace --all-features` with an audited ledger
and a raised floor; a re-run with no change in between is byte-identical.

#### US-239: Drive the four context-dependent tools
**Description:** As a person reading the scorecard, I want `skill`, `ask_user_question`, `exit_plan_mode` and `task` executed against the reference, so that their results can be compared instead of read.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/skill.py` whole, `vibe/core/tools/builtins/ask_user_question.py` whole, `vibe/core/tools/builtins/exit_plan_mode.py` whole, `vibe/core/tools/builtins/task.py` whole, plus `vibe/core/subagents.py:34` `SubagentRunnerPort` for the runner shape and `vibe/questions.py:47-66` for the answer models. Pattern to copy: `run_case` and `build_tool` in `scripts/parity/tool_execution.py:437-500`.

**Acceptance Criteria:**
- [ ] Given the pinned tree, when the capture runs, then `tool_classes()` imports `Skill`, `AskUserQuestion`, `ExitPlanMode` and `Task` alongside the five it already imports, and `build_tool` supplies each one's collaborator.
- [ ] Given `skill` is driven, when the case list runs, then it covers at least a loaded skill with fewer than 10 files, a skill with more than 10 files, a skill already loaded earlier in the conversation, and a name that does not exist.
- [ ] Given `ask_user_question` is driven, when the case list runs, then it covers a single-select answer, a multi-select answer, an "other" free-text answer, and a cancellation, each driven from a scripted answer source and never from a terminal.
- [ ] Given `exit_plan_mode` is driven, when the case list runs, then it covers each of the six outcomes the reference can return, selected by the scripted answer rather than by branch inspection.
- [ ] Given `task` is driven, when the case list runs, then it covers a completed run, a run that used more than one turn, a run that ended incomplete, an unknown agent name, and an agent that is not a subagent, with a scripted `SubagentRunnerPort` and no model call.
- [ ] Given any of the four raises, when the case is recorded, then the record carries `outcome: "raised"` with the error type and a digest of the message, never the message text.
- [ ] Given a case returns, when the record is written, then it carries `typedResult` and `modelText` built by the same `_loop.py` rendering the existing five cases use, with no per-tool branch.
- [ ] Given the capture attempts a network connection, when the socket guard fires, then the capture fails naming the attempt and writes no partial corpus.
- [ ] Given the reference checkout is absent, when the capture runs, then it exits non-zero naming the expected path and the `VIBE_REFERENCE` override.

#### US-240: Drive the two network tools against a loopback server
**Description:** As a person reading the scorecard, I want `web_fetch` and `web_search` executed against a controlled HTTP endpoint, so that their results and their request shapes are both observable.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/web_fetch.py` whole for the request and the truncation, `vibe/core/tools/builtins/web_search.py:67` `is_available` and `:150` `_parse_response` for the endpoint contract. Pattern to copy: the socket guard in `scripts/parity/experiments.py`.

**Acceptance Criteria:**
- [ ] Given the capture starts, when the loopback server binds, then it binds an ephemeral port on `127.0.0.1` and the recorded corpus carries no port number, so a re-run on another port is byte-identical.
- [ ] Given the socket guard is installed, when a connection to `127.0.0.1` is attempted, then it is allowed and every other destination still fails the capture naming the attempt.
- [ ] Given `web_fetch` is driven, when the case list runs, then it covers an HTML page, a plain-text page, a JSON body, a body larger than `max_content_bytes`, a redirect chain, a 404, a 403 carrying `cf-mitigated: challenge`, a URL with no scheme, an empty URL, and a `timeout` above `max_timeout`.
- [ ] Given a `web_fetch` case runs, when the request reaches the loopback server, then the corpus records the request method, the ordered header names and the header values the reference set, so a missing `Accept` header is a recorded difference rather than an invisible one.
- [ ] Given `web_search` is driven, when the case list runs, then it covers a string-content response, a chunked response with `tool_reference` citations, a response with duplicate citation URLs, a response with no text, and a non-2xx status.
- [ ] Given the search endpoint requires credentials, when the capture runs, then the API key is supplied to the loopback server only and no real endpoint is contacted.
- [ ] Given a captured body would carry reference-authored prose, when the record is written, then only bodies this capture itself authored are stored in cleartext.
- [ ] Given the capture is run twice with no change in between, when the two corpora are compared, then they are byte-identical.

#### US-241: Capture the projected result alongside the rendered one
**Description:** As a person reading the scorecard, I want `project_result` captured for every case, so that the second published result shape is measured rather than assumed to equal the first.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-239, Blocked by US-240
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/ui.py:72` `project_result` and `:177-181` `get_result_presentation` for where the projection is published; `vibe/core/tools/builtins/grep.py:175` and `vibe/core/tools/builtins/edit.py:128` for the two overrides. Pattern to copy: `stabilize` in `scripts/parity/tool_execution.py:511` for the one nondeterminism the grep projection carries.

**Acceptance Criteria:**
- [ ] Given a case returns, when the record is written, then it carries `projectedResult` taken from the tool's `project_result`, for all eleven tools and not only the two that override it.
- [ ] Given a `grep` case returns, when `projectedResult` is recorded, then it carries `parsed_matches` with the same stabilized ordering the typed result uses.
- [ ] Given an `edit` case returns, when `projectedResult` is recorded, then it carries `file`, `old_string`, `new_string` and an `occurrences` array of `{start_line, old_text, new_text}`, and carries no `message` key.
- [ ] Given a tool that does not override the hook, when `projectedResult` is recorded, then it equals `typedResult`, so an accidental future override is visible as a change.
- [ ] Given a case raises, when the record is written, then no `projectedResult` key is present, matching the existing shape for `outcome: "raised"`.
- [ ] Given the corpus gains a key, when the schema version is written, then it is incremented, so a stale replay fails rather than silently ignoring the new family.

#### US-242: Replay the widened corpus with a raised floor
**Description:** As a person reading the scorecard, I want the widened corpus replayed on every test run, so that a divergence in any of the eleven tools fails the build instead of aging into a wrong score.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-241
**Reference:** none read for this story: it is Rust against a committed corpus. Pattern to copy: `crates/vibe-app-server/src/tool_execution_parity_tests.rs` in full, including `LEDGER` at line 78, the staleness check at line 610 and `MINIMUM_CASES` at line 60.

**Acceptance Criteria:**
- [ ] Given the committed corpus, when the replay runs, then it asserts the schema version and that `referenceCommit` equals `vibe_core::parity::REFERENCE_COMMIT`, failing when either drifts.
- [ ] Given a case carries `projectedResult`, when the replay runs, then this port's projection is compared field by field and a difference fails unless a ledger entry covers that pointer.
- [ ] Given the replay completes, when the case count is below 90, then it fails naming the count, so a shrunken corpus cannot pass as a green one.
- [ ] Given a ledger entry whose divergence is now fixed, when the staleness check runs, then it fails naming the entry.
- [ ] Given every ledger entry, when the audit test runs, then each names either a story ID in this PRD or the licensing boundary, and no entry is scoped wider than one pointer on one case.
- [ ] Given the reference checkout is absent or off-pin, when the live probe runs, then it prints the reason from `vibe_core::parity::off_pin_reason` and returns without failing, and the corpus replay still runs.
- [ ] Given the reference checkout is on-pin, when the live probe runs, then it recaptures into `target/` and asserts the fresh corpus equals the committed one.

---

### EP-075: The result the model actually reads

Make every in-scope tool render its result the one way the reference renders
every result.

**Definition of Done:** the widened corpus replays with zero `/modelText`
divergences outside licensing-scoped ledger entries, for all eleven tools.

#### US-243: Render web_fetch and web_search field per line
**Description:** As a model consuming a tool result, I want `web_fetch` and `web_search` results rendered as the reference renders them, so that the metadata the reference publishes reaches the conversation.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-242
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/web_fetch.py:56` `WebFetchResult` and `vibe/core/tools/builtins/web_search.py:45` `WebSearchResult` for the field names and order; `vibe/core/agent_loop/_loop.py:2225-2228` for the rendering. Pattern to copy: `reference_text::joined` as used in `crates/vibe-core/src/tools/builtins/todo.rs:150-160`.

**Acceptance Criteria:**
- [ ] Given a successful fetch, when the result is rendered, then `model_text` carries `url`, `content`, `content_type` and `was_truncated`, one field per line, in the reference's declaration order.
- [ ] Given the typed result is published, when its keys are read, then they are `url`, `content`, `content_type` and `was_truncated`, replacing the `contentType` and `wasTruncated` spellings recorded as an open divergence in `docs/parity.md`.
- [ ] Given a successful search, when the result is rendered, then `model_text` carries `query`, `answer` and `sources`, one field per line.
- [ ] Given `sources` is empty, when the result is rendered, then the line is still present carrying the empty-list rendering the reference produces, rather than being omitted.
- [ ] Given a fetch or a search fails, when the error is returned, then the error path is unchanged and no partial field-per-line body is rendered.
- [ ] Given the open-divergence row for the `web_fetch` result keys, when this story lands, then that row is removed from `docs/parity.md`.

#### US-244: Render task from the reference result model
**Description:** As a model that delegated a task, I want the subagent result rendered as the reference renders it, so that turn count and completion reach the conversation.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-242
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/subagents.py:26` `TaskResult` for the three fields and `extra="forbid"`, and `:41-76` `SubagentRunAccumulator` for how `turns_used` and `completed` are derived. Pattern to copy: `reference_text::joined` and `reference_text::boolean`.

**Acceptance Criteria:**
- [ ] Given a delegated task returns, when the result is rendered, then `model_text` carries `response`, `turns_used` and `completed`, one field per line.
- [ ] Given the task tool publishes a typed result, when its keys are read, then they are exactly those three and the delegation effect fields move to the display payload rather than to the model transcript.
- [ ] Given a task ends without completing, when the result is rendered, then `completed` renders false and `response` carries whatever the run produced, rather than an error.
- [ ] Given `turns_used` is derived, when a single-turn run is rendered, then it renders 1 and not 0.
- [ ] Given the open-divergence row for the `task` result shape, when this story lands, then that row is removed from `docs/parity.md`.
- [ ] Given the TUI reads the delegation effect for its own display, when the typed result changes shape, then the transcript still renders the delegation without reading a key that no longer exists.

#### US-245: Render ask_user_question and exit_plan_mode field per line
**Description:** As a model that asked the operator something, I want the answer rendered as the reference renders it, so that cancellation and mode switching are readable as fields rather than inferred from prose.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-242
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/questions.py:58` `UserAnswer` and `:66` `UserQuestionResult` for the answer shape, and `vibe/core/tools/builtins/exit_plan_mode.py:34` `ExitPlanModeResult` plus `:64-97` `run` for the two fields and the six outcomes.

**Acceptance Criteria:**
- [ ] Given the operator answered, when the result is rendered, then `model_text` carries `answers` and `cancelled`, one field per line, where `answers` is the list of `{question, answer, is_other}` entries the reference publishes.
- [ ] Given the operator chose the free-text option, when the answer is rendered, then `is_other` renders true for that entry and false for the others.
- [ ] Given the operator cancelled, when the result is rendered, then `cancelled` renders true and `answers` renders the empty list, replacing the single cancellation sentence this port sends today.
- [ ] Given a plan review completes, when the result is rendered, then `model_text` carries `switched` and `message`, one field per line.
- [ ] Given the operator declined the plan, when the result is rendered, then `switched` renders false and the mode is unchanged.
- [ ] Given the operator supplied feedback instead of a decision, when the result is rendered, then `switched` renders false and the feedback reaches `message`.

---

### EP-076: The projection the UI reads

Publish the second result shape, and let the renderer that already reads it stop
guessing.

**Definition of Done:** `grep` and `edit` publish the reference projection, the
app-server census probes take that payload from the tool rather than
hand-writing it, and `occurrence_diff_lines` renders from a real producer.

#### US-246: Publish the grep projection
**Description:** As a client reading a search result, I want `parsed_matches` published, so that a match list can be rendered without re-parsing the text.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-242
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/grep.py:101` `GrepMatch`, `:141` `GrepResult`, `:159` `parsed_matches` and `:175` `project_result`.

**Acceptance Criteria:**
- [ ] Given a search returns matches, when the projection is published, then it carries `parsed_matches` with one entry per match in the same order the typed result uses.
- [ ] Given a search returns no matches, when the projection is published, then `parsed_matches` is present and empty rather than absent.
- [ ] Given a match line carries a colon inside the matched text, when the entry is built, then the path and line number are split on the first two separators only and the remaining text is kept whole.
- [ ] Given `FileSearchEffectOutput` in the app-server census, when the effect is produced, then its `parsed_matches` field is populated by the tool rather than declared and left empty.

#### US-247: Publish the edit projection and drop the renderer fallback
**Description:** As an operator reading an applied edit, I want the occurrence list published, so that the diff the TUI already knows how to draw actually has data.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-242
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/edit.py:46` `EditResult`, `:58` `ui_start_lines`, `:62` `ui_occurrences` and `:128` `project_result` for the exact projection, including that `message` is dropped.

**Acceptance Criteria:**
- [ ] Given an edit applies once, when the projection is published, then it carries `file`, `old_string`, `new_string` and one `occurrences` entry with `start_line`, `old_text` and `new_text`.
- [ ] Given an edit applies to several occurrences, when the projection is published, then it carries one entry per occurrence with each `start_line` counted in the file as it stood before the edit.
- [ ] Given the projection is published, when its keys are read, then `message` is absent, matching the reference.
- [ ] Given `occurrence_diff_lines` in `crates/vibe-cli/src/tui/transcript.rs` runs, when the effect output carries `occurrences`, then the diff renders from it and the `old_string`/`new_string` fallback path is removed.
- [ ] Given an edit produced no occurrence, when the transcript renders, then it renders the empty case without panicking and without falling back to whole-string diffing.
- [ ] Given the `edit` effect probe at `crates/vibe-app-server/src/app_server_surface_parity_tests.rs:1165`, when the census runs, then the probe payload is produced by the tool rather than written by hand.

---

### EP-077: Delegation under the permission policy

Make `task` the twelfth tool the policy sees, and align the depth guard's shape.

**Definition of Done:** a `task` call resolves through `PolicyStore`, the
configured allowlist and denylist decide the outcome, and a depth-exhausted
child sees the tool and reads the refusal.

#### US-248: Route task through the permission policy
**Description:** As an operator, I want `task` to ask before delegating unless the agent is allowlisted, so that the tool obeys the same policy every other builtin obeys.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-242
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/task.py:24` `TaskToolConfig` for the `ASK` default and the `[EXPLORE]` allowlist, and `:77` `resolve_permission` for the fnmatch order: denylist first to NEVER, then allowlist to ALWAYS, then no decision. Pattern to copy: the `PolicyGuardedTool` wrapping in `crates/vibe-core/src/tools/builtins.rs:211-283`.

**Acceptance Criteria:**
- [ ] Given the task handler is registered, when the tool is published, then it is wrapped in `PolicyGuardedTool` like every other builtin.
- [ ] Given `tools.task.allowlist` holds an agent name pattern, when that agent is delegated to, then the call is granted without asking.
- [ ] Given `tools.task.denylist` holds an agent name pattern, when that agent is delegated to, then the call is refused, and the denylist is consulted before the allowlist so a name matching both is refused.
- [ ] Given an agent matches neither list, when the call is made, then the operator is asked, matching the `ASK` default.
- [ ] Given the operator declines, when the call resolves, then no subagent is started and the tool returns the refusal the policy produces.
- [ ] Given a pattern uses a wildcard, when it is matched, then the match uses fnmatch semantics rather than exact equality.
- [ ] Given `tools.task.allowlist` is unset, when the defaults resolve, then the effective allowlist is `["explore"]`.

#### US-249: Reconcile the delegation depth guard
**Description:** As a subagent, I want the delegation limit enforced at call time with the tool still visible, so that the refusal I read matches what the reference tells a subagent.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-248
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/task.py:88` `run` for the depth check and the two agent-kind errors, and `crates/vibe-app-server/src/client/live/delegation.rs:231` here for the tool filtering this story removes.

**Acceptance Criteria:**
- [ ] Given a subagent's tool list is built, when it is published, then `task` is present, matching the reference, rather than filtered out.
- [ ] Given a subagent calls `task`, when the depth limit is already reached, then the call returns an error the model reads rather than the tool being absent.
- [ ] Given the depth limit, when it is resolved, then it is one level of delegation, and `MAX_DELEGATION_DEPTH` either carries that value or is removed as unused.
- [ ] Given an unknown agent name, when `task` is called, then the error names the unknown agent.
- [ ] Given the named agent is not a subagent, when `task` is called, then the error says so in this port's own wording, and the case is recorded in the corpus so the difference is a ledger entry rather than a silent one.
- [ ] Given a top-level call, when it delegates, then it still succeeds, so the tightened limit does not regress the normal path.

---

### EP-078: web_fetch's contract edges

Close the three fetch divergences the widened oracle now sees.

**Definition of Done:** an over-cap timeout is refused, truncation cuts at the
declared bound, and the recorded request matches the reference's header set and
retry.

#### US-250: Refuse an over-cap timeout and truncate at the declared bound
**Description:** As an operator, I want an out-of-range timeout refused rather than silently reduced, so that the argument I passed either runs or is reported.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-242
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/web_fetch.py:146-161` `_validate_args` for both refusals, `:164-167` `_resolve_timeout`, `:63-67` `WebFetchConfig` for `max_timeout` defaulting to 120, and `:132-137` for the truncation bound.

**Acceptance Criteria:**
- [ ] Given a `timeout` above `max_timeout`, when the tool runs, then it returns a validation error naming the cap rather than clamping.
- [ ] Given a `timeout` of zero or negative, when the tool runs, then it returns a validation error, matching the reference's positivity check.
- [ ] Given a `timeout` within range, when the tool runs, then that value is used unchanged.
- [ ] Given a body larger than `max_content_bytes`, when the content is truncated, then it is cut at `max_content_bytes` and not at a bound that also depends on the remaining buffer.
- [ ] Given content is truncated, when the marker is appended, then `was_truncated` is true and the marker is this port's own wording, recorded as a ledger entry scoped to that field.
- [ ] Given a body exactly at `max_content_bytes`, when it is returned, then `was_truncated` is false.

#### US-251: Send the reference request and retry the challenge
**Description:** As an operator fetching a Cloudflare-fronted page, I want the request the reference makes, so that a page the reference reads does not fail here.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-240
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/web_fetch.py:172-176` for `Accept` and `Accept-Language`, `:33` `_HONEST_USER_AGENT`, `:199` for redirect following, and `:208-210` for the 403 plus `cf-mitigated: challenge` retry.

**Acceptance Criteria:**
- [ ] Given a fetch is issued, when the request headers are read at the loopback server, then they carry the `Accept` and `Accept-Language` values the reference sets.
- [ ] Given a response is HTTP 403 carrying `cf-mitigated: challenge`, when the tool runs, then it retries once and returns the retry's result.
- [ ] Given a response is HTTP 403 without that header, when the tool runs, then it does not retry and returns the error.
- [ ] Given the retry also fails, when the tool returns, then it returns an error rather than retrying again, so the retry is bounded to one.
- [ ] Given a redirect chain, when it is followed, then the recorded final URL matches the reference's, and the redirect cap does not refuse a chain the reference follows.
- [ ] Given the content type check, when a response declares `text/html` with a charset parameter, then it is treated as HTML by the same test the reference applies rather than by a broader substring match.

---

### EP-079: Restate row 3 from its oracle

Record what cannot be ported and restate the score from the measurement.

**Definition of Done:** every surviving text divergence is an accepted-divergence
row bounded by name, the two open-divergence rows this PRD closes are gone, and
row 3 reads 100 with its evidence.

#### US-252: Record the authored-text divergences by name
**Description:** As a person reading the scorecard, I want each surviving prose difference recorded and bounded, so that a licensing residue is a decision rather than an open question.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-245, Blocked by US-249, Blocked by US-250
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/skill.py:40` `render_skill_result` and `:85` `already_loaded_result` for the two rendered lines and the reuse sentence; `vibe/core/tools/builtins/exit_plan_mode.py:64-97` for the six messages; `vibe/core/tools/builtins/task.py:88` for the two agent-kind errors.

**Acceptance Criteria:**
- [ ] Given the accepted-divergence row for warning and applied-edit message wording, when it is rewritten, then it names every tool whose authored text diverges rather than only `read_file` and `edit`.
- [ ] Given the skill result prose, when the divergence is recorded, then it names the two rendered guidance lines, the already-loaded sentence and the not-found error, each scoped to one field.
- [ ] Given the skill not-found error, when the available list is built, then the cap this port applies is either removed to match the reference's uncapped list or recorded as its own row with its number.
- [ ] Given each recorded divergence, when the replay runs, then a ledger entry holds it scoped to one pointer on one case, and a sentinel test fails if the divergence closes and the row stays.
- [ ] Given a reader searches the corpus for text, when they do, then no reference-authored sentence is present in cleartext and every recorded prose difference is a digest.

#### US-253: Remeasure and restate row 3
**Description:** As a person reading the scorecard, I want row 3 restated from the widened oracle, so that the number is a measurement with its instrument named.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-243, Blocked by US-244, Blocked by US-246, Blocked by US-247, Blocked by US-248, Blocked by US-251, Blocked by US-252
**Reference:** none: this story reads this repository's own measurements.

**Acceptance Criteria:**
- [ ] Given the full quality gate sequence runs, when it completes, then it is green and the replay prints a conforming count over all eleven tools.
- [ ] Given row 3 is rewritten, when it states its score, then it states 100 with the tool count, the case count, the projection count and the ledger size, each taken from the printed replay rather than written by hand.
- [ ] Given the two open-divergence rows this PRD closes, when the document is rewritten, then both are removed from `## Open divergences`.
- [ ] Given the restatement is written, when it names its method, then it names the widened `scripts/parity/tool_execution.py` and the case floor, so a later reader can reproduce it.
- [ ] Given a change lands that reduces coverage, when the replay runs, then the floor fails the build, so the restated number cannot silently become stale.
- [ ] Given `CHANGELOG.md`, when this work lands, then the user-visible parts are recorded under `## Unreleased`.

## Functional Requirements

| ID | Requirement | Story |
|---|---|---|
| FR-1 | The execution capture drives all eleven in-scope tools | US-239, US-240 |
| FR-2 | The capture records the projected result for every case | US-241 |
| FR-3 | The capture records the outgoing HTTP request shape for network tools | US-240 |
| FR-4 | The replay compares rendered text, typed result and projection, with an audited ledger | US-242 |
| FR-5 | Every in-scope tool renders one result field per line, in the reference's order | US-243, US-244, US-245 |
| FR-6 | `web_fetch` publishes `content_type` and `was_truncated` | US-243 |
| FR-7 | `task` publishes `response`, `turns_used` and `completed` | US-244 |
| FR-8 | `grep` publishes `parsed_matches` in its projection | US-246 |
| FR-9 | `edit` publishes `occurrences` and omits `message` from its projection | US-247 |
| FR-10 | `task` resolves through the permission policy with its configured lists | US-248 |
| FR-11 | The delegation limit is enforced at call time with the tool visible | US-249 |
| FR-12 | `web_fetch` refuses an out-of-range timeout | US-250 |
| FR-13 | `web_fetch` sends the reference headers and retries the challenge once | US-251 |
| FR-14 | Every surviving text divergence is a named row and a scoped ledger entry | US-252 |
| FR-15 | Row 3 states 100 from the printed measurement | US-253 |

## Non-Functional Requirements

- **NFR-1.** The committed execution corpus holds at least 90 cases, at least 4
  per tool, over at least 11 tools. Enforced by the floor in US-242.
- **NFR-2.** Two consecutive captures with no change in between produce
  byte-identical files.
- **NFR-3.** The full capture completes in under 120 seconds on the development
  machine, so it stays runnable by hand.
- **NFR-4.** `cargo test --workspace --all-features` grows by no more than 30
  seconds of wall time from this work.
- **NFR-5.** No test requires the reference checkout: a missing or off-pin
  checkout skips the live probe only, and the corpus replay still runs.
- **NFR-6.** No capture reaches a destination other than `127.0.0.1`. Any other
  attempt fails the capture.
- **NFR-7.** Zero reference-authored sentences are committed in cleartext; every
  prose difference is a `{length, digest}` pair.
- **NFR-8.** The ledger holds at most 20 entries when this PRD is complete, each
  scoped to one pointer on one case.

## Edge Cases & Error States

| Case | Expected | Story |
|---|---|---|
| A skill directory holds more than 10 files | The listed sample is capped at 10 and the sampling note is present | US-239 |
| A skill was already loaded this conversation | The result is the reuse form, not a re-read | US-239 |
| The operator cancels a question | `cancelled` renders true with an empty answer list | US-245 |
| The operator picks the free-text option | `is_other` renders true for that entry only | US-245 |
| A subagent tries to delegate at the limit | The tool is present and returns a refusal the model reads | US-249 |
| An agent name matches both allowlist and denylist | Refused: the denylist is consulted first | US-248 |
| A fetched body is exactly at the byte cap | `was_truncated` renders false | US-250 |
| A fetch times out above the cap | Validation error naming the cap, no request issued | US-250 |
| A 403 without the challenge header | No retry, error returned | US-251 |
| A search response carries duplicate citation URLs | Sources are deduplicated, first title wins | US-240 |
| A search response carries no text | Error, not an empty answer | US-240 |
| A grep match line contains a colon in the matched text | Split on the first two separators only | US-246 |
| An edit applied zero occurrences | The projection carries an empty list and the transcript renders it | US-247 |
| The loopback port differs between runs | The corpus is unchanged | US-240 |

## Risks & Mitigations

| Risk | Probability | Impact | Mitigation |
|---|---|---|---|
| The loopback server makes the capture nondeterministic | Medium | High | Ephemeral port, no port recorded, byte-identical re-run asserted in US-240 |
| Widening the oracle drops the visible score before the fixes land | High | Low | Expected and accepted: the current 92 is already a restated lower bound, and EP-074 lands with ledger entries naming the stories that close each gap |
| Driving `task` measures the runner rather than the tool | Medium | Medium | The runner is scripted on both sides and no model is called, so only rendering and error paths are compared |
| Changing the `task` typed result breaks the TUI delegation view | Medium | Medium | US-244 moves the effect fields to the display payload and asserts the transcript still renders |
| Enforcing the timeout refusal breaks an existing caller | Low | Medium | The refusal matches the reference and is covered by a corpus case; no default configuration produces an over-cap value |
| The header and retry work needs credentials to test | Low | Medium | Both are exercised against the loopback server, never a real endpoint |
| The projection change churns the app-server census | Medium | Low | US-247 replaces the hand-written probe payload, so the census is regenerated once and then stable |

## Non-Goals

1. **Byte-identical reference message text.** `NOTICE` forbids it. Every prose
   difference stays this port's own wording and is recorded as a bounded
   divergence.
2. **The shell tool families.** They belong to row 6 of the scorecard and are not
   touched here.
3. **`vibe/core/tools/ui.py` itself.** Row 4 owns it. Only the per-tool overrides
   that live in row-3 files are in scope.
4. **The tool description prompts.** The surface oracle substitutes every
   description before diffing, so prose carries zero scoring weight for this row.
5. **A live network test suite.** No test contacts a real endpoint, now or later.
6. **Re-pinning the reference.** The pin stays at `b78b451`; a re-pin is its own
   change that regenerates every corpus.

## Files NOT to Modify

- `/home/arthur/dev/mistral-vibe/**` and any reference checkout: read-only,
  always through the pin.
- `NOTICE`: the boundary this PRD works inside.
- `crates/vibe-core/src/parity.rs` and `scripts/parity/pin.py`: the pin is not
  changed by this work.
- `crates/vibe-core/src/shell/**` and the shell corpora: row 6.
- `Cargo.toml` `[workspace.package] version` and the five hand-written version
  carriers: no release is part of this work.

## Technical Considerations

- Should the loopback server live in `scripts/parity/tool_execution.py` or in a
  shared helper other capture scripts can reuse? Only this script needs it today.
- Should `projectedResult` be a new key on the existing case records, or a
  separate family in the corpus? A key keeps one case per probe, which the
  ledger's per-case pointer scheme already assumes.
- Does moving the delegation effect out of `task`'s typed result require a
  change to the app-server census, and can both land in one commit without
  making the census diff unreadable?
- Is `MAX_DELEGATION_DEPTH` read anywhere outside delegation, and can it be
  removed rather than retuned?
- Does the `ask_user_question` result change affect the ACP adapter, which serves
  the same tool to editor clients?
- Should the skill available-list cap be removed to match the reference exactly,
  or kept and recorded, given that a workspace with hundreds of skills produces a
  very long error either way?

## Success Metrics

| Metric | Baseline (2026-08-20) | Target | Timeframe |
|---|---|---|---|
| Row 3 score in `docs/parity.md` | 92 | 100 | End of this PRD |
| In-scope tools driven by the execution oracle | 5 of 11 | 11 of 11 | End of EP-074 |
| Execution corpus cases | 41 | 90 or more | End of EP-074 |
| Tools whose rendered result matches the reference | 6 of 11 | 11 of 11 | End of EP-075 |
| Published projections matching the reference | 0 of 2 | 2 of 2 | End of EP-076 |
| Builtins outside the permission policy | 1 | 0 | End of EP-077 |
| Row 3 open-divergence rows | 2 | 0 | End of EP-079 |
| Ledger entries not closed by a story or by licensing | unknown | 0 | End of EP-079 |

## Open Questions

1. Who owns the per-tool display hooks (`get_call_display`, `get_result_display`,
   `get_status_text`, the `(scratchpad)` suffix)? They are declared in row-3
   files, published through row 4's `ui.py`, and rendered by row 14. This PRD
   captures them under row 3 because that is where they are declared, and the
   assignment should be settled in `docs/parity.md` before the restatement.
2. Should the widened corpus stay one file, or split per tool once it passes 90
   cases? A single file keeps the ledger simple and the diff large.
3. Does the ACP adapter need its own corpus for the two interactive tools, or is
   the app-server measurement sufficient given both go through the same handler?
[/PRD]
