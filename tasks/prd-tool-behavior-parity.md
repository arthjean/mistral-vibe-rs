[PRD]
# PRD: Tool Behavior Parity

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-06 | Arthur Jean | Initial draft |

## Problem Statement

`tasks/prd-tool-surface-parity.md` closed the published surface of
`vibe/core/tools`: 12 of 12 base names and schemas, 16 of 16 under the managed
shell rollout, 10 of 10 Windows names, 38 of 38 against the committed digest.
That PRD declared behavioral conformance a non-goal. The consequence is now the
ceiling: every tool announces the right contract and several of them honor a
different one.

1. **The per-tool configuration has no reader.** The reference merges
   `config.tools[<name>]` into a `BaseToolConfig` carrying `permission`,
   `allowlist`, `denylist`, `sensitive_patterns` plus each tool's own limits
   (`vibe/core/tools/manager.py:620`). This port declares `tools` and
   `tool_paths` at `crates/vibe-core/src/config/registry.rs:667` and never reads
   either: a workspace-wide grep for `config.tools` returns zero non-test hits.
   Every limit is a `const`. Asking the reference itself what it exposes
   (`ToolManager.discover_tool_defaults` over the builtin directory) returns 146
   `(tool, key)` pairs across 26 tool classes, drawn from 22 distinct keys: 4
   shared by every tool and 18 tool-specific. None has a reader here. An
   operator who writes `[tools.grep] default_max_matches = 500` gets silence.
2. **The permission vocabulary is disjoint.** The reference speaks four scopes
   (`command_pattern`, `outside_directory`, `file_pattern`, `url_pattern`,
   `vibe/permissions.py:11`) and carries `invocation_pattern` plus
   `session_pattern` on every requirement. This port speaks six unrelated kinds
   (`crates/vibe-core/src/policy.rs:42`) and carries neither pattern. The
   138-entry arity table that turns an approved `npm run build` into the session
   rule `npm run build *` (`vibe/core/tools/arity.py:145`) has no counterpart,
   so the granularity of every persisted approval diverges. (Count and example
   corrected 2026-08-07 from the capture; see the note under US-106.)
3. **The shell policy is hardcoded and wrong in both directions.**
   `crates/vibe-core/src/shell.rs:173` denies `rm`, `rmdir`, `dd`, `mkfs`,
   `shutdown` and `eval` outright; none of the six is on any reference denylist,
   so upstream they reach an approval prompt. Conversely `vim`, `nano`, `emacs`,
   `tmux`, `screen`, `gdb`, `passwd` and the standalone `python`, `bash`, `sh`,
   `su` are refused upstream and pass here. The read-only allowlist holds 10
   entries against 44.
4. **`grep` runs a different engine.** The reference shells out to `rg` with
   `--smart-case`, `--no-binary`, `--max-count=n+1`, 23 configurable exclusion
   globs and a `.vibeignore` file (`vibe/core/tools/builtins/grep.py:37`). This
   port walks the tree with the `regex` crate, case-sensitive, against a
   two-entry hardcoded ignore set (`crates/vibe-core/src/workspace.rs:364`). The
   same query returns different answers.
5. **The managed shell has no terminal.** `bash_stdin` publishes 40 control keys
   and the four session tools publish conformant schemas, but no PTY sits behind
   them: the reference opens a real one on both platforms
   (`vibe/core/tools/builtins/managed_shell/_posix.py:66`). Session manifests,
   the `orphaned` status and its recovery are absent too.
6. **Two argument fixtures of 92 still diverge.** `edit/replace_all` and
   `grep/use_default_ignore` are rejected here and accepted upstream, because
   Pydantic coerces a booleanish string and `validate_type`
   (`crates/vibe-core/src/tools.rs:1034`) does not.
7. **The oracle is silent.** The reference checkout sits at `b78b451` (v2.24.0)
   while six `REFERENCE_COMMIT` constants pin `68ff32e` (v2.23.3). Every live
   probe skips with a message nobody reads; only the committed corpora still
   answer.

**Why now:** the surface work is finished and its own PRD names behavioral
parity as the next instrument. Three of these gaps are load-bearing for
everything after them: the configuration reader is a prerequisite for the
builtin limits and the shell lists, and the permission vocabulary is persisted
into approved rules and spoken on `session/requestPermission`, so every session
written before the migration is a session to migrate afterward. Deferring costs
more each week, which is the ordering principle `docs/parity.md` already
applies.

## Overview

This PRD makes the bodies match the contracts. It is organized around one
conviction stated in `AGENTS.md`: a parity claim comes from a measurement, not
from a reading. So the first epic builds the instrument. `tool_surface.py`
proves that `grep` publishes the right schema; nothing today proves that `grep`
returns the right matches. EP-030 adds an execution oracle that drives both
implementations over a fixture tree and diffs the observable result, then
re-aligns the reference checkout so the live probes speak again.

The next two epics are the load-bearing ones. EP-031 gives every tool a resolved
configuration composed from the layered config, the `tools.<name>` table and the
session override, which is the single dependency shared by the builtin limits
and the shell lists. EP-032 replaces the permission vocabulary wholesale rather
than mapping onto it: the four reference scopes, the four-field
`RequiredPermission`, the arity table and the wildcard rule that lets an
approved `npm run build *` cover a later bare `npm run build`. Replacing rather
than mapping is the more expensive choice today and the cheaper one in three
months, because the vocabulary is persisted.

EP-033 through EP-035 then close the bodies: the shell policy on tree-sitter-bash
with four configurable lists, `grep` on the ripgrep library crates, the file
tools on their reference contracts, and finally a real PTY behind the managed
shell family plus an MCP sampling handler. Descriptions stay out of scope:
`NOTICE` forbids shipping reference prose, and this port covers directives
instead, a posture already recorded in `docs/parity.md`.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Argument fixtures replayed with the reference verdict | 92/92 | 92/92 |
| Builtin execution traces matching the reference oracle | 40/40 on the committed corpus | 40/40, corpus extended to 80 |
| Reference `(tool, key)` configuration pairs with a live reader | 146/146 | 146/146 |
| Permission scopes spoken on the wire matching the reference set | 4/4 | 4/4 |
| Shell policy list entries matching the reference defaults | 4 lists, 0 divergent entries | 4 lists, 0 divergent entries |
| `docs/parity.md` score for Built-in tools and Tool infrastructure | 98 | 100 |

## Target Users

### Vibe operator switching between clients

- **Role:** an engineer who runs `vibe` in a terminal on one machine and the
  Python client on another, against the same repository and the same
  `~/.vibe/config.toml`.
- **Behaviors:** writes per-tool configuration by hand, approves shell commands
  interactively, expects an approval granted in one session to still apply in
  the next.
- **Pain points:** `[tools.grep] default_max_matches` is silently ignored here.
  `rm -rf build/` is refused outright with no way to approve it, while `vim` is
  allowed and hangs the turn. An approval for `npm run build` does not cover
  `npm run test` because no session pattern is derived.
- **Current workaround:** keeps a separate configuration file per client, and
  wraps refused commands in a subshell to get past the hardcoded denial, which
  defeats the guard rather than satisfying it.
- **Success looks like:** one configuration file, one set of approvals, the same
  answer from `grep` on both clients.

### Rust port maintainer

- **Role:** the engineer closing parity, working against a read-only reference
  checkout and the committed corpora.
- **Behaviors:** reads the reference module before writing Rust, runs the
  surface oracle, updates `docs/parity.md` from measurements.
- **Pain points:** no instrument answers "does this tool behave the same". Every
  behavioral claim is a reading, which the scorecard itself flags as its next
  ceiling. Six `REFERENCE_COMMIT` constants sit in five crates and drift
  silently when the checkout moves.
- **Current workaround:** reads both implementations side by side and reasons
  about the difference, which does not survive a refactor and cannot run in CI.
- **Success looks like:** `cargo test --workspace --all-features` reports a
  behavioral conformance count the way it already reports a surface one.

## Research Findings

Key findings that informed this PRD:

### Competitive Context

The only comparable is the reference implementation itself, treated as a
behavioral oracle rather than a competitor. Two adjacent ports informed the
method:

- **`tasks/prd-tool-surface-parity.md`** (DONE): established capture-then-replay
  with a gitignored corpus and a committed prose-free digest. This PRD reuses
  the pattern for execution rather than declaration.
- **`tasks/prd-config-parity.md`** (DONE): established that declaring a
  configuration key is not implementing its feature, and that each key arrives
  with the feature that reads it. EP-031 is that arrival for 24 of them.
- **Market gap:** no instrument in this repository compares tool *output*. The
  three existing oracles compare declarations: method inventories, configuration
  censuses, tool schemas.

### Best Practices Applied

- **Library over subprocess for search.** The ripgrep documentation confirms
  `RegexMatcherBuilder` exposes smart case directly, `ignore::WalkBuilder`
  honors `.gitignore`, and `Searcher::search_slice` drives a custom `Sink` that
  carries line numbers and can stop after N matches. Using the crates removes a
  runtime dependency on an external binary that the reference already treats as
  optional through its GNU grep fallback.
- **Grammar over tokenizer for shell parsing.** `tree-sitter-bash` 0.25.1
  exposes `LANGUAGE.into()` and `node.kind()`, and the reference relies on a
  `redirected_statement` parent check to keep `python3 << 'EOF'` from being read
  as a bare `python3`. The hand-rolled tokenizer at
  `crates/vibe-core/src/shell.rs:322` has no notion of heredocs.
- **PTY through a maintained abstraction.** `portable-pty` documents
  `native_pty_system()`, `openpty(PtySize)`, `spawn_command`,
  `try_clone_reader`, a single-call `take_writer`, and `ChildKiller::kill`. The
  API is synchronous, so the tokio bridge is a decision to make rather than a
  detail to discover.
- **Measure the oracle, do not reason about it.** The coercion table below was
  obtained by running the reference's own Pydantic, not by reading its
  documentation.

*Full research sources available in project documentation.*

## Assumptions & Constraints

### Assumptions (to validate)

- ~~The external score that reports below 100 weighs names, schemas and
  observable behavior, and does not weigh description text.~~ **CLOSED
  2026-08-06 by US-099**, in two halves. For the score this repository publishes
  in `docs/parity.md`: refuted, so `NOTICE` costs it nothing. The `## Method`
  section weighs names and schemas and never reads a description, and the
  instrument that produces the tool scores substitutes `<described>` for every
  description in both the capture and the replay, so prose carries exactly zero
  weight by construction rather than by convention. For the third-party 35/100
  score: not determinable, because `tasks/prd-tool-surface-parity.md` records its
  method as unpublished. Its exposure is bounded by measurement instead of
  guessed, at 13 776 bytes of tool prompt prose and 1 735 bytes of parameter
  descriptions against 3 789 bytes of description-free schema, and recorded in
  `docs/parity.md` under accepted divergences. EP-034 proceeds against the
  measured score, not the unpublished one.
- The reference scalar coercion is exactly what its Pydantic reports. Measured
  on 2026-08-06 against the reference interpreter: `bool` accepts
  `yes/no/on/off/true/false/t/f/y/n/1/0` in any case plus integer `0`/`1` and
  float `0.0`/`1.0`; `str` accepts only `str` and `bytes`; `int` accepts an
  integral string, a float with a zero fractional part, and `bool`; `float`
  accepts a numeric string, `int` and `bool`; `null` is never coerced. LOW risk,
  reproducible.
- `portable-pty` can kill a whole process group, not only the direct child. The
  fetched documentation covers `ChildKiller::kill` on the child and is silent on
  the group. MEDIUM risk: the reference declares `process_group_kill` as a
  backend capability, and a `hard_timeout` that leaves grandchildren alive is a
  visible divergence. US-116 validates empirically before the implementation
  settles.
- Adding a C toolchain requirement for `tree-sitter-bash` does not break CI.
  LOW risk: the build already needs ALSA headers for `cpal`, so CI images
  already carry a compiler.

### Hard Constraints

- `NOTICE` forbids copying, translating, vendoring, linking or shipping upstream
  implementation source. Tool descriptions stay original text held to directive
  coverage. Captured corpora carrying reference-authored prose stay gitignored
  under `.parity/`.
- The reference checkout is read-only. No story creates, modifies or deletes any
  file under it, and that includes its virtual environment: a probe that needs
  the reference interpreter runs it out of tree.
- Workspace dependency layers hold (`[workspace.metadata.vibe] dependency-layers`
  in `Cargo.toml`): `vibe-protocol`/`vibe-core`, then `vibe-app-server`, then
  `vibe-cli`/`vibe-acp`. Tool bodies belong to `vibe-core` and may not reach
  upward.
- `unsafe_code` is forbidden workspace-wide; `panic`, `unimplemented` and
  `dbg_macro` are denied. A new dependency may use `unsafe` internally, but no
  handler may panic and none may introduce an `unsafe` block here.
- Tools stay bounded by `DEFAULT_MAX_TOOL_OUTPUT_BYTES` and `ToolOutputSink`
  (`crates/vibe-core/src/tools.rs:22`). A PTY pump may not bypass it.
- A missing reference checkout must never fail `cargo test`. Every new parity
  test replays a committed corpus unconditionally and skips only the live probe,
  the way `crates/vibe-cli/src/tui/runtime_parity_tests.rs:46` does.
- `cancelled` stays British wherever it names the reference concept; every other
  repository artifact is US English.

## Quality Gates

These commands must pass for every user story:

- `cargo fmt --all -- --check` - formatting matches the CI gate
- `cargo check --workspace --all-targets --all-features` - the workspace compiles including tests and benches
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` - no lint regression under the workspace lint policy
- `cargo test --workspace --all-features` - full suite, including every parity oracle whose corpus is committed

## Epics & User Stories

### EP-030: Behavioral Oracle and Measurable Foundations

Build the instrument before the work it measures. Today no test compares tool
*output* between the two implementations, so every behavioral claim in
`docs/parity.md` is declarative. This epic also re-aligns the reference pin so
the live probes stop skipping silently.

**Definition of Done:** an execution oracle captures reference tool output over
a fixture tree, a committed corpus replays it unconditionally, the 92 argument
fixtures all return the reference verdict, and the six `REFERENCE_COMMIT`
constants agree with a checkout that actually sits at that commit.

#### US-099: Re-align the reference pin and its six constants
**Description:** As the port maintainer, I want the pinned commit and the local
checkout to agree so that a live probe either runs or says why, instead of
skipping into silence.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given `grep -rn 'REFERENCE_COMMIT: &str' crates`, when the six sites are enumerated, then every one carries the same value and that value is recorded in one place the others cite
- [ ] Given a decision to stay at `68ff32e`, when the local checkout is at another commit, then a single documented command restores it, and `docs/parity.md` records the pinned version alongside the package version
- [ ] Given a decision to re-pin to `b78b451`, when the pin moves, then all three corpora (`tool-surface`, `config-surface`, `app-server-surface`) are regenerated in the same change and every replay still passes
- [ ] Given the checkout sits at a commit other than the pin, when `cargo test --workspace --all-features` runs, then the skip message names both the expected and the found commit and the suite still passes
- [ ] Given the open question on description weighting, when the external scoring method is confirmed or refuted, then the answer is recorded in `docs/parity.md` under accepted divergences and this PRD's assumption is closed

#### US-100: Reproduce the reference scalar coercion
**Description:** As an operator whose model emits `"replace_all": "yes"`, I want
the call to be accepted exactly where the reference accepts it so that a prompt
tuned against one client works against the other.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given a schema declaring `boolean`, when the value is one of `yes`, `no`, `on`, `off`, `true`, `false`, `t`, `f`, `y`, `n`, `1`, `0` in any case, or integer `0`/`1`, or float `0.0`/`1.0`, then validation accepts it and the handler receives the coerced boolean
- [ ] Given a schema declaring `boolean`, when the value is `2`, `-1`, the empty string, an unrecognized word, or `null`, then validation rejects it naming the property path
- [ ] Given a schema declaring `string`, when the value is a number or a boolean, then validation rejects it, because the reference coerces neither
- [ ] Given a schema declaring `integer`, when the value is an integral string, a float with a zero fractional part, or a boolean, then validation accepts it; when the value is `17.5` or `"17.5"`, then it is rejected
- [ ] Given the 92 committed argument fixtures, when `arguments_the_reference_rejects_are_rejected_here_too` replays them, then it reports 0 wrongly accepted and 0 stricter than the reference
- [ ] Given a coerced value, when the handler reads it, then it reads the coerced form and not the raw one, proven by a test that asserts the payload the handler received

#### US-101: Build the tool execution oracle
**Description:** As the port maintainer, I want an oracle that compares tool
output between both implementations so that a behavioral parity claim comes from
a measurement.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-099

**Acceptance Criteria:**
- [ ] Given a deterministic fixture tree checked into the repository, when `scripts/parity/tool_execution.py` runs against the pinned checkout, then it records, per case, the tool name, the arguments, the normalized typed result, the model-facing text and the raise-or-return outcome
- [ ] Given a capture, when it is written, then reference-authored prose stays in the gitignored `.parity/` artifact and the committed corpus holds only names, pointers, counts and values authored for the corpus
- [ ] Given the committed corpus, when `cargo test --workspace --all-features` runs without any reference checkout, then the replay runs unconditionally and reports a conforming count
- [ ] Given a case whose Rust output diverges, when the replay runs, then it fails naming the case, the JSON pointer and both values, and a divergence listed in the epic ledger is tolerated while an unlisted one fails
- [ ] Given a ledger entry whose divergence has been fixed, when the replay runs, then the stale entry fails the suite so the ledger cannot rot
- [ ] Given host-specific values (absolute paths, timings, temporary directories), when a result is recorded, then they are normalized so the corpus replays identically on another machine

---

### EP-031: Per-Tool Configuration

Give every tool a configuration resolved from the layered config, the
`tools.<name>` table and the session override, the way
`vibe/core/tools/manager.py:620` composes one. This is the single dependency
shared by the builtin limits and the shell policy lists, which is why it comes
before both.

**Definition of Done:** the 146 reference `(tool, key)` configuration pairs have
a live reader, an operator edit changes observable behavior, and no builtin limit
remains a `const` where the reference exposes it.

#### US-102: Resolve a tool configuration from the layers
**Description:** As an operator, I want `[tools.grep] default_max_matches = 500`
in my configuration to reach the grep handler so that the file I already write
for the Python client works here.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-101

**Acceptance Criteria:**
- [ ] Given a tool name, when its configuration is resolved, then the result composes the tool's declared defaults, the merged `tools.<name>` table and the session permission override, in that precedence order
- [ ] Given `tools.<name>` is absent and no session override exists, when the configuration is resolved, then the tool's declared defaults are returned unchanged, matching the reference early return
- [ ] Given `register_workspace_tools` at `crates/vibe-app-server/src/server.rs:1759`, when it publishes the three families, then it hands each one a resolver rather than a frozen snapshot, so a configuration change between turns is observed without re-registration
- [ ] Given a `tools.<name>` entry carrying a key the tool does not declare, when the configuration is resolved, then the unknown key is preserved rather than dropped, matching the divergence already recorded for `ConfigSnapshot::unregistered_keys`
- [ ] Given a `tools.<name>` entry whose value has the wrong type, when the configuration is resolved, then the tool falls back to its declared default and a diagnostic names the tool, the key and the offending value, without failing session startup
- [ ] Given the resolver, when the configuration lock is contended, then resolution never blocks a running turn for more than 50 ms and never panics on a poisoned lock

#### US-103: Wire the builtin limits onto the resolved configuration
**Description:** As an operator, I want each builtin's limits to come from my
configuration so that raising a read budget does not require rebuilding the
binary.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-102

**Acceptance Criteria:**
- [ ] Given `read_file`, when its configuration is resolved, then `permission`, `sensitive_patterns` and `max_read_bytes` are read from it, with the reference defaults `always`, `["**/.env", "**/.env.*"]` and 51200
- [ ] Given `write_file`, when its configuration is resolved, then `permission`, `sensitive_patterns`, `max_write_bytes` (64000) and `create_parent_dirs` (true) are read from it
- [ ] Given `grep`, when its configuration is resolved, then `max_output_bytes` (64000), `default_max_matches` (100), `default_timeout` (60), `exclude_patterns` (23 entries) and `codeignore_file` (`.vibeignore`) are read from it
- [ ] Given `todo`, when its configuration is resolved, then `max_todos` (100) is read from it; `web_search` reads `timeout` (120) and `model`; `web_fetch` reads `default_timeout`, `max_timeout`, `max_content_bytes` and `user_agent`
- [ ] Given every tool the reference declares defaults for, when the wiring is complete, then all 146 `(tool, key)` pairs have a reader, asserted by a test that fails when a pair loses one
- [ ] Given a configured limit set below the value a call needs, when the tool runs, then it fails with the reference-shaped message naming the limit rather than truncating silently
- [ ] Given a configured limit of zero or a negative number, when the configuration is resolved, then the value is refused and the declared default applies, with a diagnostic naming the key

#### US-104: Publish the discovered tool configuration defaults
**Description:** As an operator opening the settings screen, I want to see every
tool's configurable keys and their defaults so that I can write a valid
`tools.<name>` entry without reading the source.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-103

**Acceptance Criteria:**
- [ ] Given the registered tools, when their defaults are enumerated, then each name maps to its declared configuration keys with default values, matching what `discover_tool_defaults` returns upstream
- [ ] Given a tool that declares no configuration, when the defaults are enumerated, then it appears with an empty map rather than being omitted
- [ ] Given a tool whose default enumeration fails, when the defaults are collected, then that tool is skipped with a diagnostic naming it and the remaining tools are still returned
- [ ] Given the published defaults, when they are compared against the reference enumeration, then the 26 tool classes, the 22 distinct keys and the 146 pairs all match, including the Windows-only families a POSIX host never publishes
- [ ] Given a Windows-only tool class on a POSIX host, when defaults are enumerated, then it still appears with its keys, because the reference enumerates declarations rather than the published surface

---

### EP-032: Conformant Permission Model

Replace the permission vocabulary rather than mapping onto it. The four
reference scopes, the four-field requirement, the arity-derived session pattern
and the wildcard rule are all persisted or spoken on the wire, so a mapping
would leave two truths and a migration that grows with every written session.

**Definition of Done:** the four reference scopes are the only ones spoken,
every requirement carries an invocation pattern and a session pattern, an
approval granted for a session covers the invocations the reference says it
covers, and the file tools resolve permission through the reference chain.

#### US-105: Publish the four reference permission scopes
**Description:** As an operator approving a tool call, I want the request to name
the same scope the Python client names so that the approval I grant means the
same thing on both.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-102

**Acceptance Criteria:**
- [ ] Given a permission requirement, when it reaches the wire, then its scope is one of `command_pattern`, `outside_directory`, `file_pattern`, `url_pattern` and nothing else
- [ ] Given a requirement, when it is serialized, then it carries exactly `scope`, `invocationPattern`, `sessionPattern` and `label`, camelCased, with no additional field
- [ ] Given a requirement built for a path outside the working directory, when its label is rendered, then it reads `outside workdir (<glob>)` where the glob is the parent directory joined with `*`
- [ ] Given the existing `PermissionRequirement` variants, when the replacement lands, then no call site still constructs `Mcp` or `Destructive`, and any behavior they carried is expressed through one of the four scopes
- [ ] Given a session that persisted approvals under the previous vocabulary, when it is resumed after the change, then the stale rules are dropped rather than misapplied, and the operator is asked again instead of being silently granted or silently denied
- [ ] Given the app-server surface corpus, when `app_server_surface_parity_tests` replays it, then the scope enum vocabulary is compared against the reference set and reports 0 missing and 0 invented values

#### US-106: Port the arity table and the session pattern
**Description:** As an operator who approved `npm run build` for the session, I
want a later `npm run build --watch` to be covered so that I am not asked once
per argument list.

> **Corrected 2026-08-07 by measurement.** Two statements below were derived by
> reading rather than by running, and the capture in
> `crates/vibe-core/tests/permission-surface/vocabulary.json` refutes both. The
> table holds **138** entries, not 142: `len(ARITY)` is 138 and the count of 142
> was the line number of the closing brace. And the arity is a *token count to
> keep*, not a prefix depth, so `npm run` mapping to 3 makes `npm run build`
> reduce to `npm run build *` and `git config user.name` to
> `git config user.name *`. An approval for `npm run build` therefore does not
> cover `npm run test` upstream either, which is why this story's description
> now names the case the reference actually covers. The criteria below are the
> measured ones.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-105

**Acceptance Criteria:**
- [ ] Given the arity table, when it is compared against the reference, then it holds the same 138 entries with the same values, asserted by a test that fails when either side changes
- [ ] Given the tokens of a command, when a session pattern is built, then the longest matching prefix in the table selects the arity, and the pattern is the first *arity* tokens followed by ` *`
- [ ] Given `npm run build`, when a session pattern is built, then it is `npm run build *`; given `git config user.name`, then it is `git config user.name *`; given `ls -la`, then it is `ls *`
- [ ] Given a command whose first token is absent from the table, when a session pattern is built, then it is that token followed by ` *`
- [ ] Given an empty token list, when a session pattern is built, then the result is the empty string and no panic occurs
- [ ] Given several commands in one chain that reduce to the same session pattern, when requirements are built, then the pattern appears once rather than per command

#### US-107: Cover an invocation with an approved session rule
**Description:** As an operator, I want an approval I granted for the session to
cover the matching later invocations so that the guard stops re-asking what I
already answered.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-106

**Acceptance Criteria:**
- [ ] Given a stored rule for a tool and a scope, when a later requirement of the same tool and scope has an invocation pattern matching the rule's session pattern, then the call proceeds without a prompt
- [ ] Given a rule whose session pattern ends with ` *`, when the invocation pattern equals the pattern with that suffix removed, then it is still covered, matching the reference wildcard rule
- [ ] Given a rule for one tool, when a different tool produces an identically shaped requirement, then it is not covered and the operator is asked
- [ ] Given a requirement with several scopes, when only some are covered by stored rules, then the prompt lists exactly the uncovered ones
- [ ] Given trust is revoked mid-session, when a covered invocation is re-validated before its side effect, then it is refused, and the existing atomicity of revocation is preserved
- [ ] Given a session ends, when a new one starts, then session-scoped rules do not carry over while permanently persisted ones do

#### US-108: Resolve permission for a file tool
**Description:** As an operator, I want a read of `.env` to ask even though
`read_file` is set to always, so that a sensitive file is never opened behind my
back.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-107

**Acceptance Criteria:**
- [ ] Given a path inside the scratchpad directory, when permission is resolved, then it is granted outright without consulting any list
- [ ] Given a path matching the tool's denylist, when permission is resolved, then it is refused; given a path matching the allowlist and no denylist entry, then it is granted; the denylist is consulted first
- [ ] Given a path matching a `sensitive_patterns` glob, when permission is resolved, then a `file_pattern` requirement is produced with the file name as invocation pattern and `*` as session pattern, even when the configured permission is always
- [ ] Given a path outside the working directory and every project root, when permission is resolved, then an `outside_directory` requirement is produced carrying the parent directory joined with `*` as both patterns
- [ ] Given a path outside the working directory and a configured permission of never, when permission is resolved, then the call is refused without producing a requirement
- [ ] Given a path that cannot be resolved at all, when permission is resolved, then it is treated as outside the working directory rather than as inside it

---

### EP-033: Conformant Shell Policy

Replace the hardcoded policy at `crates/vibe-core/src/shell.rs:173` with the
reference's four configurable lists, parsed by the same grammar. This epic
loosens guards as well as tightening them, so every loosened case is named by a
test.

**Definition of Done:** commands are extracted by tree-sitter-bash, the four
lists come from configuration with the reference defaults, path operands outside
the working directory produce requirements, and no policy decision remains
hardcoded where the reference exposes it.

#### US-109: Extract shell commands with the reference grammar
**Description:** As an operator running `python3 << 'EOF'`, I want the heredoc to
be recognized so that the call is not refused as a bare standalone `python3`.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-102

**Acceptance Criteria:**
- [ ] Given a shell command, when it is parsed, then each `command` node yields its command name, words, strings, raw strings and concatenations joined by single spaces, matching the reference extraction
- [ ] Given a command whose node has a `redirected_statement` parent, when it is extracted, then a redirect marker is appended so it is no longer a bare standalone command
- [ ] Given `python3 << 'EOF'`, when policy is resolved, then it is not refused by the standalone denylist
- [ ] Given a chain joined by `&&`, `||`, `;` or a pipe, when it is parsed, then each segment is extracted separately so approving one does not approve the rest
- [ ] Given a command the grammar cannot parse, when it is extracted, then the result is empty and the caller falls back to asking rather than to allowing
- [ ] Given the parser, when it is constructed, then it is built once and reused, and parsing a 64 KB command completes in under 100 ms

#### US-110: Replace the hardcoded policy with the four configurable lists
**Description:** As an operator, I want `rm -rf build/` to reach an approval
prompt and `vim` to be refused so that this client guards what the Python one
guards, and nothing else.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-109, US-106

**Acceptance Criteria:**
- [ ] Given the four lists, when their defaults are compared to the reference, then `allowlist` holds the 7 common entries plus the 37 POSIX read-only commands, `denylist` the 13 POSIX entries, `denylist_standalone` the 8 POSIX entries and `sensitive_patterns` the single `sudo` entry, with 0 divergence
- [ ] Given a command matching a denylist entry by full command or by basename, when policy is resolved, then it is refused with a reason naming the offending segment and the matched pattern
- [ ] Given a single-token command whose basename is on the standalone denylist, when policy is resolved, then it is refused; given the same command with arguments, then it is not refused by that rule
- [ ] Given `rm -rf build/`, `dd`, `mkfs`, `shutdown` or `eval`, when policy is resolved, then the call reaches an approval prompt rather than an outright refusal, and a named regression test records each of these five loosenings
- [ ] Given a command whose first token is on `sensitive_patterns`, when policy is resolved, then it always produces a requirement even when the configured permission is always, and it is never covered by an allowlist match
- [ ] Given `find` with `-exec`, `-execdir`, `-ok` or `-okdir`, when policy is resolved, then a `command_pattern` requirement carrying the whole segment is produced, deduplicated across repeated identical segments
- [ ] Given an operator who set the allowlist to the empty list, when any command runs, then every segment produces a requirement rather than the tool becoming unusable

#### US-111: Collect path operands that leave the working directory
**Description:** As an operator, I want `grep secret /etc/passwd` to ask before
it runs so that an allowlisted reader cannot read outside the workspace
unnoticed.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-110

**Acceptance Criteria:**
- [ ] Given a command whose first token is on the path-inspecting set, when its arguments are scanned, then each token that looks like a path is resolved against the command's working directory and checked against the working directory and every project root
- [ ] Given the path-inspecting set, when it is compared to the reference, then it is a superset of the read-only allowlist, so no auto-allowed command escapes inspection
- [ ] Given a token starting with `-`, or a `chmod` mode token starting with `+`, when arguments are scanned, then it is skipped rather than resolved as a path
- [ ] Given a resolved path inside the scratchpad, when it is scanned, then it produces no requirement
- [ ] Given a resolved path outside every root, when a requirement is built, then it names the parent directory for a file and the directory itself for a directory, joined with `*`, and identical directories are emitted once
- [ ] Given a managed call overriding `cwd`, `shell` or `env`, when policy is resolved, then each override produces its own requirement, so an allowlisted command cannot run elsewhere or under another interpreter without approval

---

### EP-034: Builtin Bodies

Close the observable behavior of the tools whose contracts are already
conformant. Every story in this epic is verified by the EP-030 execution oracle
rather than by a reading.

**Definition of Done:** the execution corpus reports 0 unlisted divergences for
`grep`, `read_file`, `write_file`, `edit`, `skill`, `todo` and `web_search`.

#### US-112: Reimplement grep on the ripgrep library crates
**Description:** As an operator, I want `grep` to return the matches the Python
client returns so that the same query gives the same answer on both.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-103, US-101

**Acceptance Criteria:**
- [ ] Given a lowercase pattern, when the search runs, then it matches case-insensitively; given a pattern carrying an uppercase character, then it matches case-sensitively, reproducing smart case
- [ ] Given a directory target, when the search runs, then `.gitignore` and `.ignore` entries are honored unless `use_default_ignore` is false, and the configured exclusion globs apply in both cases
- [ ] Given a `.vibeignore` file in the working directory, when the search runs, then its non-comment, non-blank lines join the exclusion set
- [ ] Given a binary file in the walked tree, when the search runs, then it is skipped rather than producing matches or an error
- [ ] Given more matches than the effective cap, when the result is built, then exactly the cap is returned, `match_count` reports that number and the truncation flag is set; the same flag is set when the byte budget clips the output
- [ ] Given a search exceeding the configured timeout, when it runs, then it fails with a message naming the timeout, and the partial result is discarded rather than returned as complete
- [ ] Given an invalid regular expression, when the search runs, then it fails naming the pattern error, before walking any file
- [ ] Given a path that does not exist, when the search runs, then it fails naming the path rather than returning zero matches

#### US-113: Align read_file with its reference contract
**Description:** As an operator reading a file in a subdirectory carrying its own
`AGENTS.md`, I want those instructions injected once so that the model follows
the same directory rules it follows upstream.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-103, US-108

**Acceptance Criteria:**
- [ ] Given a read whose path sits under directories carrying `AGENTS.md`, when the result is returned, then their contents are appended to the model-facing text once per directory per session, and a progress event names the discovered files
- [ ] Given the same directory read a second time in the same session, when the result is returned, then its `AGENTS.md` is not injected again
- [ ] Given a file whose content is empty, when it is read, then the result carries the empty-file warning; given an offset past the last line, then the result names the total line count; given a file shorter than the offset by one line, the two cases stay distinct
- [ ] Given rendered output larger than the configured read budget, when the result is built, then the call fails naming both sizes and suggesting offset and limit, rather than truncating
- [ ] Given a path that is a directory, or does not exist, or is an empty string, when the tool runs, then each fails with its own distinct message
- [ ] Given the execution corpus, when the `read_file` cases replay, then line numbering, the selected range and the truncation flag match the reference for every case

#### US-114: Align write_file and edit with their reference contracts
**Description:** As an operator editing a CRLF file written in Latin-1, I want
the edit to preserve both so that a one-line change does not rewrite the whole
file's encoding.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-103, US-108

**Acceptance Criteria:**
- [ ] Given an existing file, when `write_file` targets it, then the call is refused pointing at `edit`, and the refusal happens both before and during the write so a race cannot overwrite it
- [ ] Given a missing parent directory and `create_parent_dirs` true, when `write_file` runs, then the directory is created; with the flag false, then the call fails naming the missing parent
- [ ] Given content larger than the configured write budget, when `write_file` runs, then it fails naming the limit before touching the filesystem
- [ ] Given a file whose detected encoding or line ending is not UTF-8 or LF, when `edit` writes it back, then both are preserved
- [ ] Given `old_string` absent from the file, matching more than once without `replace_all`, empty, or equal to `new_string`, when `edit` runs, then each fails with its own distinct message naming the cause
- [ ] Given a concurrent edit of the same file, when two calls overlap, then a write lock serializes them and neither observes a partially written file
- [ ] Given a mutating call, when it is about to run, then a snapshot of the target file is captured before the handler executes, so a later revert can restore the prior state

#### US-115: Align skill, todo and web_search
**Description:** As an operator whose model reloads a skill it already loaded, I
want a short acknowledgment instead of the whole body so that the context is not
spent twice on the same instructions.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-103

**Acceptance Criteria:**
- [ ] Given a skill already loaded earlier in the conversation, when it is requested again, then the result is the short reuse acknowledgment carrying the skill directory, not the rendered body
- [ ] Given a skill directory holding nested files, when the body is rendered, then the file list walks recursively, excludes `SKILL.md`, is sorted, and stops at 10 entries
- [ ] Given a skill with no directory on disk, when the body is rendered, then the base-directory lines are omitted rather than rendered with an empty path
- [ ] Given a todo write exceeding the configured maximum, when it runs, then it fails naming the limit; given duplicate identifiers, then it fails naming the repeated identifier
- [ ] Given a `web_search` call, when the request is issued, then it carries the request metadata and the user-agent header the reference sends, and the configured model and timeout
- [ ] Given a `web_search` response carrying no text, when it is parsed, then the call fails rather than returning an empty answer, and the failure names no credential

---

### EP-035: Live Channels: PTY and MCP Sampling

The two surfaces where the reference holds an open channel rather than running a
request to completion. Both are currently declared and unbacked: the session
tools publish control keys with no terminal, and `sampling_enabled` is honored by
refusing either way.

**Definition of Done:** an interactive program driven through `bash_stdin`
responds to a control key, a session survives a restart as `orphaned` and can be
inspected, and an MCP server that requests a completion receives one.

#### US-116: Back the managed shell with a real PTY
**Description:** As an operator running an interactive program in a background
session, I want `ctrl_c` to reach it so that the control keys the tool advertises
actually do something.

**Priority:** P2
**Size:** L (5 pts)
**Dependencies:** Blocked by US-110

**Acceptance Criteria:**
- [ ] Given a managed session, when it starts, then the child runs under a PTY and a program that checks for a terminal behaves as it does under one
- [ ] Given a running session, when `bash_stdin` sends `ctrl_c`, then the foreground program receives the interrupt and the session reports the resulting status
- [ ] Given a session with `hard_timeout` and an expired `timeout_seconds`, when the timeout fires, then the whole process group is terminated and no grandchild survives, asserted by a test spawning a child that outlives its parent
- [ ] Given a platform where the PTY backend is unavailable, when the shell family is published, then it falls back to the pipe-based path rather than failing session startup, and the reduced capability is reported
- [ ] Given a session producing output faster than it is drained, when the pump runs, then the output stays bounded by the tool output contract and the excess is recorded as dropped rather than buffered without limit
- [ ] Given the PTY writer, when the session is closed, then it is released exactly once and a second close is a no-op rather than an error

#### US-117: Persist sessions and recover the orphaned ones
**Description:** As an operator whose client restarted while a build was running,
I want the session listed as orphaned so that I can find its log instead of
losing it.

**Priority:** P2
**Size:** M (3 pts)
**Dependencies:** Blocked by US-116

**Acceptance Criteria:**
- [ ] Given a started session, when it starts, then a manifest recording its identifier, command, shell, working directory and start time is written next to its log
- [ ] Given manifests left by a previous process, when the tool family loads, then those sessions are listed with the orphaned status and their logs remain readable
- [ ] Given an orphaned session, when it is inspected or killed, then the call answers from the manifest rather than failing on a missing live process
- [ ] Given a reset with log clearing requested, when it runs, then the logs and manifests are removed; without it, the logs survive
- [ ] Given an output read at a cursor that falls inside a multi-byte character, when the window is returned, then the boundary is adjusted so no replacement character is produced, in both the leading and trailing direction
- [ ] Given a corrupt or unreadable manifest, when the family loads, then that entry is skipped with a diagnostic and the remaining sessions still load

#### US-118: Answer MCP sampling requests
**Description:** As an operator using an MCP server that asks the client for a
completion, I want the request served so that the server works here as it works
upstream.

**Priority:** P2
**Size:** M (3 pts)
**Dependencies:** Blocked by US-102

**Acceptance Criteria:**
- [ ] Given a server entry with sampling enabled, when the outbound MCP session initializes, then the client advertises the sampling capability; with sampling disabled, then it does not
- [ ] Given an inbound sampling request, when it is served, then the messages are mapped to the engine's message type, a system prompt is prepended when present, and the active model answers with the requested temperature and token budget
- [ ] Given a request carrying a message role that is neither user nor assistant, when it is mapped, then it is treated as assistant and the anomaly is logged rather than failing the request
- [ ] Given a request carrying content blocks that are not text, when it is mapped, then the text blocks are joined and the others are skipped
- [ ] Given the backend fails or times out, when the request is served, then a structured MCP error is returned to the server and no partial completion is sent
- [ ] Given a server entry with sampling disabled, when an inbound sampling request arrives, then it is refused with the capability-absent error rather than silently answered

## Functional Requirements

- FR-01: The system must resolve a per-tool configuration composed of declared defaults, the merged `tools.<name>` table and the session override, and must re-read it at each publication rather than freezing it at registration.
- FR-02: The system must coerce scalar arguments exactly as the reference Pydantic does, and must hand the handler the coerced value.
- FR-03: The system must speak exactly four permission scopes and must carry an invocation pattern and a session pattern on every requirement.
- FR-04: The system must derive a session pattern from the 138-entry arity table, falling back to the first token followed by ` *`.
- FR-05: When an approved session rule's pattern matches an invocation pattern, including the optional trailing-argument form, the system must not prompt again.
- FR-06: The system must extract shell command segments with the bash grammar, and must mark a segment whose node has a redirected-statement parent.
- FR-07: The system must resolve shell policy from four configurable lists whose defaults equal the reference defaults.
- FR-08: The system must NOT deny a shell command outright unless a denylist entry matches it.
- FR-09: The system must inspect the path arguments of every path-inspecting command, and that set must remain a superset of the read-only allowlist.
- FR-10: The system must capture a snapshot of a file before a mutating tool writes to it.
- FR-11: The system must inject a subdirectory's `AGENTS.md` at most once per directory per session.
- FR-12: The system must run a managed shell session under a PTY when the platform provides one, and must fall back to pipes otherwise rather than failing.
- FR-13: The system must persist a manifest per shell session and must report a session left by a previous process as orphaned.
- FR-14: The system must advertise the MCP sampling capability only for server entries that enable it.
- FR-15: Every parity test must replay its committed corpus unconditionally and must skip only the probe requiring the reference checkout.
- FR-16: The system must NOT ship reference-authored prose in any committed file.

## Non-Functional Requirements

- **Performance:** per-tool configuration resolution completes in under 5 ms at P95 and never blocks a turn for more than 50 ms. Parsing a 64 KB shell command completes in under 100 ms. A `grep` over a 10,000-file tree completes within its 60-second configured timeout. The PTY pump drains at a 25 ms interval, matching the existing shell pump constant.
- **Security:** a shell command is denied only on an explicit denylist match; every other non-allowlisted command produces an approval requirement, so the guard fails toward asking. A path outside every project root always produces an `outside_directory` requirement before the side effect. A sensitive-pattern match always produces a requirement even at permission `always`. No error message, log line or corpus entry contains an API key, an OAuth token or a resolved credential. Trust revocation applies before the next side effect, preserving the existing atomicity.
- **Accessibility:** not applicable; this PRD adds no user interface. Tool failure messages name the cause, the offending value and the applicable limit in a single line under 200 characters so they render in a narrow terminal without wrapping past two lines.
- **Scalability:** the tool registry holds at least 500 registered tools without publication exceeding 10 ms. The managed shell holds at least 32 concurrent sessions per family. `grep` bounds output at the configured 64,000 bytes and matches at the configured 100 regardless of tree size.
- **Reliability:** a missing reference checkout never fails `cargo test`. A handler never panics; a panicking handler is caught and reported as a tool error, preserving the existing guarantee. A corrupt session manifest is skipped without dropping the other sessions. An MCP backend failure returns a structured error rather than a partial completion. Tool output stays bounded by `DEFAULT_MAX_TOOL_OUTPUT_BYTES` on every path including the PTY pump.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Empty state: no reference checkout | CI, or a fresh clone | Corpora replay unconditionally; only the live probe skips | "the tool-surface oracle needs <path> at <commit>, found <other>" |
| 2 | Empty state: no configuration | `tools.<name>` absent | Declared defaults apply, unchanged | n/a |
| 3 | Empty state: no skills on disk | `skill` called on an empty catalog | Failure lists what does exist, or "none" | "skill `x` was not found; available skills: none" |
| 4 | Error state: malformed configuration value | `[tools.grep] default_max_matches = "many"` | Declared default applies, session still starts | "tools.grep.default_max_matches expects an integer, found a string; using 100" |
| 5 | Error state: invalid regular expression | `grep` with an unbalanced group | Fails before walking any file | "the search pattern is not a valid regular expression: <cause>" |
| 6 | Network degradation: MCP backend timeout | Sampling request while the provider is slow | Structured MCP error, no partial completion | "sampling failed: the model did not answer within <n>s" |
| 7 | Permission change: trust revoked mid-turn | Operator revokes while a call is authorized | Re-validation before the side effect refuses it | "the workspace trust was revoked before this call ran" |
| 8 | Concurrent modification: two edits, one file | Two turns editing the same path | Write lock serializes; neither sees a partial file | "the file changed while this edit was preparing; re-read it and retry" |
| 9 | Boundary value: output at exactly the limit | `grep` output of exactly 64,000 bytes | Returned whole, truncation flag false | n/a |
| 10 | Boundary value: budget below the need | `max_read_bytes` set to 100 | Fails naming both sizes and suggesting offset and limit | "the rendered output is <a> bytes, over the <b>-byte budget; narrow it with offset and limit" |
| 11 | Boundary value: zero or negative limit | `max_todos = 0` | Value refused, declared default applies | "tools.todo.max_todos must be positive; using 100" |
| 12 | Undo: revert after a mutating call | Operator reverts a turn | The pre-call snapshot restores the prior content | n/a |
| 13 | Interrupted flow: client restarts mid-build | Managed session outlives its process | Session listed as orphaned, log still readable | "session <id> was left running by a previous process" |
| 14 | Interrupted flow: cursor inside a character | Output read at a multi-byte boundary | Window adjusted, no replacement character emitted | n/a |
| 15 | External dependency: no PTY backend | Platform without one, or headless container | Falls back to pipes, reduced capability reported | "this platform provides no terminal; the session runs without one" |
| 16 | External dependency: no C toolchain at build | CI image without a compiler | Build fails at compile time with the crate's own error | n/a |
| 17 | Loosened guard: previously denied command | `rm -rf build/` after US-110 | Reaches an approval prompt instead of an outright refusal | "approve `rm -rf build/` for this call, for the session, or deny" |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | The external score weighs description text, which `NOTICE` caps permanently | Med | High | US-099 confirms or refutes the weighting before EP-034 starts. If confirmed, the residue is recorded in `docs/parity.md` as an accepted divergence and the target becomes 100 minus the description weight, stated explicitly rather than chased. |
| 2 | Aligning the shell policy loosens five currently denied commands | High | High | US-110 requires a named regression test per loosening, so each is a recorded decision rather than a side effect. The guard still fails toward asking: nothing becomes silently allowed. |
| 3 | Replacing the permission vocabulary orphans persisted approvals | Med | Med | US-105 drops stale rules rather than misapplying them and re-asks the operator. Doing it now is cheaper than after more sessions are written, which is why EP-032 precedes EP-033 and EP-034. |
| 4 | `portable-pty` cannot kill a process group, only the child | Med | Med | US-116 validates empirically before the implementation settles. If it cannot, the platform-specific kill sits behind the same backend trait the reference uses, isolated to one module. |
| 5 | The execution oracle proves too costly to keep green | Med | High | The corpus is deterministic by construction: a checked-in fixture tree, normalized host-specific values, and a divergence ledger per epic so known gaps do not block unrelated work. |
| 6 | Twenty stories at 70 points overruns before the value lands | Med | Med | Epics are ordered by cost of deferral, not by score. EP-030 through EP-032 carry the load-bearing work and can ship as a release on their own; EP-035 is P2 and separable. |
| 7 | `tree-sitter-bash` adds a C toolchain requirement to every build | Low | Med | The build already needs ALSA headers for `cpal`, so CI images carry a compiler. Verified during US-109 on the CI image before the dependency is merged. |
| 8 | The reference moves again mid-effort | Med | Low | US-099 consolidates the six `REFERENCE_COMMIT` constants so a re-pin is one edit plus a corpus regeneration, rather than five crates to find. |

## Non-Goals

Explicit boundaries. What this version does NOT include:

- **Byte-identical tool descriptions.** `NOTICE` forbids shipping upstream prose. Descriptions stay original text held to directive coverage, the posture `docs/parity.md` already records. Revisit only if the licensing posture changes, which is an operator decision and not an implementation one.
- **Custom tool discovery from `.vibe/tools/*.py`.** The reference loads user-authored Python tools at runtime. This port has no equivalent extension mechanism and inventing one is a product decision, not a parity one. Deferred to a Phase 2 alongside the next item.
- **Description overrides from `<tools-dir>/prompts/*.md`.** Same search-path mechanism as the previous item, so the two ship together or not at all.
- **MCP OAuth login flows and connector authentication changes.** Sampling is in scope because it is a per-call contract; the credential lifecycle is a separate surface scored separately in `docs/parity.md`.
- **Windows managed shell execution equivalence.** The two Windows families publish conformant schemas and this PRD adds the POSIX PTY. Proving Windows execution equivalence needs a Windows CI host this repository does not have.
- **The `experimental_bash` internal implementation depth.** 2,252 lines upstream. This PRD closes the observable behavior the execution oracle can measure, not the internal structure.
- **Byte-identical error message text.** Messages must name the same cause, value and limit; they are held to that, not to the reference wording, for the same licensing reason as descriptions.

## Files NOT to Modify

- `/home/arthur/dev/mistral-vibe/**`: the read-only behavioral oracle. No story creates, modifies or deletes any file under it, and that includes its virtual environment: a probe needing the reference interpreter runs it out of tree.
- `NOTICE`: the licensing posture is a deliberate project decision. Changing it is an explicit operator call, not an implementation side effect.
- `crates/vibe-protocol/src/lib.rs`: owns the JSON-RPC envelopes and the routed method inventory. Every envelope struct denies unknown fields, which is what lets the untagged `Envelope` discriminate its variants. This PRD changes tool behavior, not the wire inventory.
- `crates/vibe-app-server/tests/app-server-surface/corpus.json` and `crates/vibe-core/tests/config-surface/corpus.json`: captures from the pinned reference. They change only through a regeneration, which is US-099's job and no other story's.
- `crates/vibe-app-server/tests/tool-surface/digest.json`: the committed conformance target for the published surface. This PRD must not move it: a change here would mean the surface regressed.

## Technical Considerations

Framed as questions for engineering input, not mandates:

- **Architecture: where does the resolved configuration live?** Recommended: a `ToolConfigResolver` in `vibe-core` handed to the three `register` entry points (`crates/vibe-core/src/tools/builtins.rs:176`, `crates/vibe-core/src/tools/shell.rs:466`, `crates/vibe-core/src/workspace.rs:724`) from the single call site at `crates/vibe-app-server/src/server.rs:1759`. Alternative: widen `ToolSpec.config` from a decorative blob into a live handle. Trade-off: the resolver keeps `ToolSpec` a pure declaration, at the cost of one more parameter on three signatures. Engineering to confirm the resolver can be re-read per publication without a lock held across a turn.
- **Data model: does `PermissionRequirement` become the four scopes, or wrap them?** Recommended: replace, so one vocabulary exists. The six current variants have call sites in `vibe-app-server` and `vibe-cli` that this changes. Alternative: keep both behind a conversion. Trade-off: the conversion is cheaper this week and leaves two truths persisted in approved rules.
- **API design: does the execution oracle drive the tools in-process or over the app-server?** In-process is faster and deterministic; over the app-server exercises the real path a client takes. Recommended: in-process for the corpus, with a smaller end-to-end case set over the app-server, mirroring how the tool-surface oracle separates the census from the probe.
- **Dependencies:** `grep-searcher`, `grep-regex`, `ignore`, `globset` (the ripgrep library crates, pure Rust) for US-112. `tree-sitter` 0.25 plus `tree-sitter-bash` 0.25.1 for US-109, which adds a C toolchain requirement at build time. `portable-pty` for US-116, whose synchronous API needs a `spawn_blocking` or dedicated-thread bridge to tokio. Alternatives evaluated: invoking the `rg` binary (rejected: adds a runtime dependency), a hand-rolled bash parser (rejected: the current one already misses heredocs), `pty-process` and `expectrl` (not evaluated in depth; engineering may prefer one if the process-group kill is cleaner).
- **Migration: what happens to sessions written before EP-032?** Recommended: drop rules carrying the retired vocabulary and re-ask, rather than translating them. Backward compatibility requirement: a session resumed after the change must not be silently granted nor silently denied. Rollback plan: the vocabulary change is one commit touching one enum and its call sites; reverting it restores the previous behavior without touching persisted session files, since the retired rules are dropped rather than rewritten.
- **Testing: how is a loosened guard proven safe?** Recommended: each of the five loosenings in US-110 gets a named test asserting the command now reaches an approval prompt, so the change is visible in the suite rather than buried in a list diff.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Argument fixtures returning the reference verdict | 90/92 | 92/92 | Month-1 | `cargo test -p vibe-app-server --all-features tool_surface_parity_tests -- --nocapture` |
| Builtin execution cases matching the reference | 0 (no oracle exists) | 40/40 | Month-1 | the EP-030 execution corpus replay |
| `tools.<name>` `(tool, key)` pairs with a live reader | 0/146 | 146/146 | Month-1 | a test asserting each key changes observable behavior, counted the way `discover_tool_defaults` enumerates them |
| Permission scope vocabulary matching the reference | 0/4 (six unrelated kinds) | 4/4 | Month-1 | the app-server surface corpus enum comparison |
| Shell policy list entries diverging from the reference defaults | 44 read-only entries missing, 5 commands wrongly denied, 11 wrongly allowed | 0 divergent entries across the four lists | Month-1 | a test diffing the four defaults against the reference |
| `docs/parity.md` Built-in tools score | 95 | 98 (Month-1), 100 (Month-6) | Month-1 / Month-6 | rescored from the execution oracle, not from a reading |
| `docs/parity.md` Tool infrastructure score | 95 | 98 (Month-1), 100 (Month-6) | Month-1 / Month-6 | same |
| `docs/parity.md` Managed shell score | 90 | 92 (Month-1), 100 (Month-6) | Month-1 / Month-6 | same, once the PTY lands |
| Reference commit constants disagreeing with the checkout | 6 | 0 | Month-1 | `grep -rn 'REFERENCE_COMMIT: &str' crates` plus `git rev-parse` in the checkout |

## Open Questions

- ~~Does the external 100-point score weigh tool description text?~~ **ANSWERED 2026-08-06.** Not for the score this repository publishes, which is blind to descriptions by construction; not determinable for the third-party one, whose method is unpublished. Both recorded in `docs/parity.md` under accepted divergences, with the exposure bounded by measurement. EP-034 targets the measured score.
- ~~Does the pin stay at `68ff32e` (v2.23.3) or move to `b78b451` (v2.24.0)?~~ **ANSWERED 2026-08-06: it stays.** The checkout was restored to the pin and every live probe now runs, with zero skips across the suite. Moving was rejected on measurement rather than on the estimate recorded here: the single commit between the two is 140 files and +10 547/-743, adding `identity.py`, `session_index.py` and a config layer, so it displaces the configuration and app-server surfaces as well as `manager.py` and reopens the recorded `identity/read` divergence. The claim that "the behavioral cost of moving is small" held only for `vibe/core/tools`. Re-pinning to v2.24.0 is its own PRD.
- Should the retired `Mcp` and `Destructive` permission kinds map onto `command_pattern`, or disappear with the behavior they carried re-expressed elsewhere? Owner: engineering, during US-105. This determines whether MCP tool approvals keep a distinct prompt shape.
- Is a Windows CI host worth adding to prove the Windows shell families execute equivalently? Owner: Arthur, after EP-035. Without one, the two families stay schema-conformant and execution-unproven, which is the current recorded position.
[/PRD]
