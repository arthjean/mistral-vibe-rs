[PRD]
# PRD: Managed Shell at Full Parity

**Reference root:** every `vibe/...` path in this document is relative to the
read-only Python checkout at `/home/arthur/dev/mistral-vibe/`
(`C:\dev\mistral-vibe` on Windows, `VIBE_REFERENCE` overrides both), read at
commit `b78b451` and never from its working tree. See `## Reference Map` for the
read commands and the full symbol table.

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-22 | Arthur Jean | Initial draft: take parity row 6 from 92 to 100 |

## Problem Statement

Row 6 of `docs/parity.md` ("Managed shell and terminals") scores 92. Its residual
sentence names three things: "the session behavior is asserted by named tests
rather than by a differential oracle, the Windows families still execute on
pipes, and a host that provides no PTY backend falls back to pipes with the
reduced capability reported as a null `ptyBackend`". One of those three is
false. `process_spec` in `crates/vibe-core/src/tools/shell.rs:611-647` sets
`spec.terminal = managed` with no family condition, so a managed `powershell` or
`git_bash` session gets a PTY on the same terms a `bash` session does. The row
marks itself down for a defect it does not have, which is the clearest evidence
that its remaining eight points were priced by reading rather than by measuring.

The other two are real, and measuring the part that has no oracle finds nine
divergences the row never mentions, several of which a model sees on every
managed shell call.

The published result document is the largest. The reference declares
`ExperimentalBashResult` with thirteen snake_case fields
(`vibe/core/tools/builtins/experimental_bash.py:1251-1266`): `command`,
`session_id`, `status`, `exit_code`, `shell`, `background`, `output`,
`next_cursor`, `truncated`, `output_path`, `stdout`, `stderr`, `returncode`.
This port publishes nine camelCase keys
(`crates/vibe-core/src/tools/shell/session.rs:674-687`) and invents one,
`backpressureDropped`, that the reference has no counterpart for. The legacy
path diverges in the other direction: the reference's `BashResult` carries four
fields and this port adds a fifth, `truncated`
(`crates/vibe-core/src/tools/shell.rs:601-608`). The four session tools each
publish a shape of their own invention rather than the reference's
`BashOutputResult`, `BashStdinResult`, `BashSessionsResult` and
`BashLogFileResult`. Casing is not the whole of it: the documents differ in
which fields exist, so a model reading a managed shell result here reads a
different document from the one the reference writes.

What the model reads on top of that document diverges too. The reference's agent
loop renders every tool result the same way, `"\n".join(f"{k}: {v}")` over
`model_dump(mode="json")` (`vibe/core/agent_loop/_loop.py:2225-2228`), and the
only `get_result_extra` in the tree returns `None`
(`vibe/core/tools/base.py:468-475`), so nothing is appended. This port composes
the model text by hand for all fifteen shell names: raw output plus an
`[output truncated at N bytes]` marker the reference never writes, plus a
`stderr:` block the reference never separates. The corpus at
`crates/vibe-app-server/tests/tool-execution/corpus.json` proves the eleven
non-shell tools already publish snake_case fields the reference's rendering can
consume. The shell family is the outlier, and it is the outlier precisely
because no oracle ever executed it.

The publication gate is missing outright, and it costs more than it looks like.
`local_managed_shell_only` is `True` on all five managed bash classes
(`vibe/core/tools/builtins/experimental_bash.py:1625`, `:1820`, `:1913`,
`:1997`, `:2141`) and the manager withholds any class carrying it when the host
already provides a terminal (`vibe/core/tools/manager.py:325-331`), which
`vibe/app_server/_runtime.py:212-214` computes as `"terminal" not in
client_tools`. Driven directly on the pinned tree, the four quadrants are:

| managed rollout | runtime gate | shell surface | class serving `bash` |
|---|---|---|---|
| off | on | `bash` | `Bash` |
| off | off | `bash` | `Bash` |
| on | on | `bash`, `bash_log_file`, `bash_output`, `bash_sessions`, `bash_stdin` | `ExperimentalBash` |
| on | **off** | **`bash`** | **`Bash`** |

This port has no fourth quadrant. It publishes five names to a client that hosts
its own terminal, where the reference publishes one, and it does so on Linux as
much as on Windows. The surface oracle cannot see this: `surface()` in
`scripts/parity/tool_surface.py:133-200` constructs `ToolManager` without
passing `local_managed_shell_runtime_enabled`, so it captures the default,
`True`, and the one quadrant that diverges is the one it never asks for.

Persisted state diverges in three ways at once, and the three compound. The
reference writes eleven snake_case fields per manifest with ISO-8601 timestamps
through `_now_iso` (`experimental_bash.py:500`, `:1063-1087`); this port writes
twelve camelCase keys with epoch milliseconds
(`crates/vibe-core/src/tools/shell/session.rs:202`). The reference mints
`{prefix}_{YYYYmmdd_HHMMSS}_{uuid4 hex[:8]}` (`:1146-1148`). Both write into the
same directory, `$VIBE_HOME/shell-tool/sessions/`, because this port reproduced
the path correctly (`shell.rs:68`, `session.rs:58`) while reproducing neither
the manifest nor the identifier. The result is two processes sharing a directory
and silently ignoring each other's orphans: this port's `load_orphaned_manifests`
filters on `sessionId` (`session.rs:68-107`) and the reference's filters on
`session_id` (`:1088-1112`), so each one's records are invisible to the other and
neither reports a session it cannot read.

Four execution-semantics divergences finish the list. The managed inline window
reads `max_inline_bytes` here (30 000, `session.rs:285`) where the reference's
managed command reads `max_output_bytes` (16 000, `experimental_bash.py:1716`),
which is not a copy of the polling window it shares a module with: the four
polling tools do read `max_inline_bytes` (`:1884`, `:2092`, `:2224`), so this is
one call site diverging and not a config-wide rename. The managed success check
tests the exit code alone (`session.rs:313-319`) where the reference requires
`status == "completed"` and a zero code (`:1795-1810`), so a session killed on
its way out reports success here. A log file that is not there raises here
(`crates/vibe-core/src/tools/shell/decode.rs:111-140`) and returns an empty
chunk there (`:1117-1143`). And the reference publishes both `output`, raw, and
`stdout`, CRLF-normalized, where this port publishes one field for both jobs.

On Windows the PTY story is smaller than the row claims but not empty. The
reference names its backends `"ConPTY"` and `"WinPTY"`
(`vibe/core/tools/builtins/managed_shell/_windows.py:21-22`), tries them in that
order, and raises `ManagedShellBackendError` when neither starts (`:313-344`).
This port publishes one lowercase backend name and degrades to pipes rather than
failing, so a client reading `ptyBackend` reads a value the reference never
emits and a host with no PTY at all is reported as a working session with
reduced capability.

None of this is in `## Open divergences`. That section has six rows and not one
is about the shell. `## Accepted divergences` has exactly one shell row, and it
covers the three commands the policy asks about rather than anything in this
list. Eight points are missing from row 6 and the document names none of them.

## Overview

The work is instrument-first, in the shape rows 3, 4 and 5 established, and for
the same reason: every divergence above lives in the one part of the shell
subsystem that no oracle executes. Three oracles already read it from outside.
`scripts/parity/tool_surface.py` compares the published names and schemas,
`crates/vibe-core/tests/tool-config/defaults.json` compares the configuration
keys, and `scripts/parity/tool_presentation.py` compares the display shape. What
none of them does is run a command and compare what came back.

A new oracle, `scripts/parity/shell_session.py`, drives the pinned reference's
own `ExperimentalBash`, `BashOutput`, `BashStdin`, `BashSessions` and
`BashLogFile` handlers over scripted commands under a hermetic `VIBE_HOME`, and
records four artifacts per case: the typed result as `model_dump(mode="json")`,
the model-facing text the agent loop would render from it, the outcome kind, and
where the case touches persisted state, the manifest and the `SessionInfo` it
produced. Volatile fields are normalized by rule rather than dropped, so a
session identifier is compared as a shape and a timestamp as a format. The same
change teaches `surface()` the gate parameter it never had, which turns the
three-quadrant surface census into four.

With the instrument in place, four behavioral epics land. The result documents
are rewritten to the reference's shapes, all six of them, and the model-facing
text stops being composed by hand and becomes what the reference's rendering
rule produces from the typed result. Persisted state converges: eleven fields,
ISO-8601 stamps, the reference's identifier format, and orphan loading that
reads the key the reference writes, which as a side effect makes the shared
directory actually shared instead of two implementations pretending the other's
files are not there. The publication gate arrives, modeled on the client's
terminal capability the way the app-server models it, with the legacy variant
selected by priority when the managed one is withheld. And the four execution
divergences close: the managed window, the completed-status condition, the empty
chunk for a missing log, plus the Windows backend names, the WinPTY fallback and
the failure that is a failure.

The last epic restates the row from the widened measurement. It deletes the
false clause about Windows pipes, records by name what will not be ported, and
prints the score from the same line the replay prints.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Row 6 score in `docs/parity.md` | 100, restated from a measurement | 100, still measured on every CI run |
| Shell session cases replayed against the reference | 70 or more, 0 unledgered divergences | Same, with the floor raised as cases are added |
| Reference shell handlers with an executable oracle | 5 of 5 | 5 of 5 |
| Shell result fields this port publishes and the reference does not | 0 | 0 |
| Surface quadrants captured | 4 of 4 | 4 of 4 |
| False claims in row 6 | 0 | 0 |

## Target Users

### Model running a command through a managed session

- **Role:** the agent itself, reading whatever the shell tool returns.
- **Behaviors:** runs a command, reads the result, polls with `<family>_output`
  when the command backgrounds, writes a control key through `<family>_stdin`,
  and occasionally lists sessions.
- **Pain points:** the result document names nine keys where the reference names
  thirteen, so `shell`, `background`, `stderr` and `returncode` are simply not
  there to read; the text rendered on top is a raw output blob rather than the
  labeled field list every other tool produces, so the model cannot tell an
  empty output from a failed command without a second call; a session killed
  mid-shutdown reports success.
- **Current workaround:** none available to a model. It reads what it is given.
- **Success looks like:** the same command produces the same document and the
  same rendered text it would produce against the reference, so a prompt written
  for one works on the other.

### Editor or desktop client that hosts its own terminal

- **Role:** an ACP or app-server client declaring the `terminal` client tool.
- **Behaviors:** advertises its capability at initialization, then expects the
  server to publish the tool surface appropriate to a host that already runs
  commands.
- **Pain points:** this port publishes five managed shell tools to it where the
  reference publishes one, so the client is offered session management it is
  supposed to own; on Linux the effect is the same as on Windows, which the
  first read of this gap missed.
- **Current workaround:** filter the surface client-side, which no reference
  client does.
- **Success looks like:** declaring `terminal` collapses the family to the one
  delegating command tool, exactly as it does against the Python app-server.

### Operator inspecting sessions on disk

- **Role:** whoever opens `$VIBE_HOME/shell-tool/sessions/` after a crash, or
  runs both implementations on the same machine.
- **Behaviors:** reads a manifest to find out what a leftover session was, lists
  sessions through `<family>_sessions`, reads a log through
  `<family>_log_file`.
- **Pain points:** the two implementations write mutually unreadable manifests
  into the same directory and each silently ignores the other's orphans, so a
  session left by one is invisible to the other and the directory grows records
  nobody reclaims; timestamps are epoch milliseconds here and ISO-8601 there, so
  the two cannot even be sorted together; a log file deleted by hand turns a
  read into an error instead of an empty result.
- **Current workaround:** delete the directory.
- **Success looks like:** one manifest format, one identifier format, and an
  orphan written by either implementation listed and reclaimed by both.

### Reader of the scorecard

- **Role:** anyone deciding whether this port can replace the reference for
  shell work.
- **Behaviors:** reads `docs/parity.md` row by row, trusts a score only when the
  row names how it was measured.
- **Pain points:** row 6 says 92, names one residual that is factually wrong,
  and names none of the nine divergences a model actually meets; the divergence
  tables carry nothing about the shell beyond the three commands the policy asks
  about.
- **Current workaround:** treat the number as noise.
- **Success looks like:** row 6 says 100, names the oracle and the command that
  reproduces it, and every remaining difference is in a divergence table with
  the test that fails when it stops reproducing.

## Research Findings

Research for this PRD was a differential read of the reference checkout at the
pin plus direct execution of the pinned tree, not a market survey: the only
comparable product is the reference, it is readable in full, and it is runnable.
Web research was not run and no library documentation was needed. Every
dependency involved is already in the workspace, `portable-pty` included, and no
story here adds one.

### Competitive Context

- **Mistral Vibe (Python reference, v2.24.0 at `b78b451`)**: the behavioral
  oracle. Its managed shell is one 2 252-line module,
  `vibe/core/tools/builtins/experimental_bash.py`, holding the session manager,
  the five tool classes and the five result models, with two thin
  platform-specific subclass files (`git_bash.py`, `windows_shell.py`) and a
  `managed_shell/` package for the PTY backends. The design choice worth naming
  is that the five result models are plain Pydantic models with no alias
  generator, so what the manager serializes is exactly what the class declares,
  and the agent loop renders it without knowing anything about shells. This port
  put the rendering inside each tool, which is why the divergences concentrate
  in the rendered text and the published document rather than in the process
  handling.
- **Market gap:** none. This is parity work with a single, fully readable and
  fully runnable reference.

### Measurements taken for this PRD

Delegation to research subagents was not used; the measurements below are
primary, taken against the pinned tree with the reference's own interpreter,
which is a stronger source than any secondary one for a behavioral-parity
document.

- The four-quadrant surface probe in `## Problem Statement`, driven by
  constructing `ToolManager` directly on the pinned tree with
  `managed_shell_tools_enabled` and `local_managed_shell_runtime_enabled` set to
  each of the four combinations, reading `get_available_tools()` and the class
  serving `bash`. The fourth quadrant collapses five names to one, on Linux.
- `git grep -n "local_managed_shell_only" b78b451` returns six lines: the
  `False` default on `vibe/core/tools/base.py:165` and `True` on the five
  managed bash classes. The Git Bash and PowerShell managed classes inherit it
  through `ExperimentalBash`; `GitBash` (`git_bash.py:193`) and `WindowsShell`
  (`windows_shell.py:849`) do not, which is why the gate leaves a name behind on
  Windows and not on Linux.
- `selection_priority` is declared twice in the tree: `0` on
  `vibe/core/tools/base.py:160` and `10` on `experimental_bash.py:1623`. That
  single pair is the whole arbitration between the legacy and managed classes
  publishing the same name.
- The byte-window call sites read one at a time: `experimental_bash.py:1716` is
  `self.config.max_output_bytes` and `:1884`, `:2092`, `:2224` are
  `args.max_bytes or self.config.max_inline_bytes`. The defaults are 16 000 and
  30 000. A grep alone would have suggested a config-wide rename; reading the
  four sites shows one diverging call site.
- The typed-result key casing of the eleven tools already under the execution
  oracle, read straight out of
  `crates/vibe-app-server/tests/tool-execution/corpus.json`: all snake_case,
  from `file_path` and `num_lines` to `was_truncated` and `bytes_written`. The
  shell family is the only camelCase publisher in the tree.
- A grep for `alias_generator` and `to_camel` across the reference: present in
  `vibe/permissions.py`, `vibe/questions.py` and `vibe/user_content.py`, absent
  from every tool result model. This closed the one hypothesis that would have
  invalidated the casing finding.
- Reference file sizes at the pin, to size the port surface honestly:
  `experimental_bash.py` 2 252 lines, `windows_shell.py` 1 102, `git_bash.py`
  447, `bash.py` 631, `managed_shell/_windows.py` 385, `managed_shell/_posix.py`
  158.

### Best Practices Applied

- Widen the instrument before changing behavior. Row 3 was restated from 92
  after its oracle was widened to the six tools it never executed, row 4 from 95
  and row 5 from 90 on the same shape. Row 6 is the same case one part over: the
  three oracles that touch the shell all read it from outside, and the part they
  do not execute is where every divergence lives.
- A parity claim comes from a measurement, and the measurement runs wide enough
  to cover what changed (`AGENTS.md`, "The behavioral oracle").
- A ledger fails both on an undeclared divergence and on an entry that no longer
  reproduces, so a fix cannot leave a stale exception behind (pattern from
  `crates/vibe-app-server/src/tool_execution_parity_tests.rs`).
- A corpus floor makes a green replay a claim about coverage rather than about
  whatever the last capture happened to contain (pattern from
  `assert_corpus_floor` in the same file).
- Reference-authored prose never enters this repository. A captured error
  sentence is stored as a SHA-256 digest with a structural marker, never as
  text, and the rendered model text is compared as structure where it carries a
  reference-authored fragment.
- Falsify the row before pricing it. The Windows-pipes clause survived one read
  and died on the second, which is the argument for making the third read a test
  rather than a read.

## Assumptions & Constraints

### Assumptions (to validate)

- The reference's five shell handlers can be driven headlessly under a
  temporary `VIBE_HOME` without a terminal on the capture process's own stdin,
  because the session opens its own PTY rather than inheriting one. Evidence:
  `managed_shell/_posix.py:40` sets `pty_backend` from the backend the session
  creates. **Risk: MEDIUM.** Validated by US-289.
- Session identifiers and timestamps can be normalized by shape rather than
  dropped, so a corpus stays comparable across runs without losing the fields
  most likely to diverge. Evidence: the identifier is
  `{prefix}_{stamp}_{8 hex}` by construction (`experimental_bash.py:1146-1148`)
  and the stamp is `datetime.isoformat` output. **Risk: LOW.** Validated by
  US-289.
- Rendering the model text from the typed result with the reference's join rule
  is a mechanical format and not authored prose, so reproducing it does not
  touch the `NOTICE` boundary. Evidence: `_loop.py:2225-2228` is four lines of
  formatting over arbitrary keys, and the tool contributes no text through
  `get_result_extra`, which returns `None` for every tool in the tree
  (`base.py:468-475`). **Risk: LOW.** Validated by US-295.
- Switching the fifteen shell tools from camelCase to the reference's
  snake_case breaks no consumer that the existing suite does not already cover,
  because the eleven non-shell tools already publish snake_case and the TUI
  presentation reads the display document rather than the typed one. Evidence:
  the corpus key listing above, and `crates/vibe-core/tests/tool-presentation/corpus.json`
  comparing `display` and `projectedOutput` separately from the typed result.
  **Risk: MEDIUM.** Validated by US-292.
- Removing `backpressureDropped` loses no signal, because the reference carries
  a nullable `reader_error` on `SessionInfo` that is the natural place for it.
  **Risk: MEDIUM.** Validated by US-296.
- The client terminal capability is reachable where the shell tools are
  registered, because `delegated_command` already reads
  `client.supports_terminal()` two functions away
  (`crates/vibe-core/src/tools/shell.rs:469-500`). **Risk: LOW.** Validated by
  US-299.
- A WinPTY fallback can be expressed through `portable-pty` without a second
  dependency, or it cannot and the fallback becomes a recorded divergence rather
  than a silent absence. **Risk: HIGH.** Validated by US-304, which is written
  to produce a recorded answer either way.

### Hard Constraints

- `NOTICE` forbids copying reference source, prompt files or tool description
  text. Every corpus that would carry a reference-authored sentence stores a
  digest or a structural marker instead, and any cleartext corpus stays
  gitignored under `.parity/`.
- The reference checkout is read-only and is read at the pin, never from the
  working tree.
- A missing or off-pin reference checkout must never fail `cargo test`: the
  corpus replay runs unconditionally and the live probe skips with a printed
  reason from `vibe_core::parity::off_pin_reason`.
- The pin lives in exactly two places and this PRD does not move it. Every
  corpus written here carries `b78b451c39eab9213393ad2f45908e8562a5c5e7`.
- Layering holds: `vibe-protocol` and `vibe-core` first, `vibe-app-server`
  second, `vibe-cli` and `vibe-acp` third. The shell contract stays in
  `vibe-core` and nothing above it re-implements a piece.
- `unsafe_code` is forbidden; `panic`, `unimplemented` and `dbg_macro` are
  denied outside tests.
- No capture and no test may write into the user's real `$VIBE_HOME` or into the
  reference checkout. Every one of them sets `VIBE_HOME` under its own temporary
  directory, and a test that fails to do so fails on the assertion rather than
  on the side effect.
- No new workspace dependency. `portable-pty` is already present; anything the
  Windows backend work cannot express through it becomes a recorded divergence.

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
git -C /home/arthur/dev/mistral-vibe show b78b451:vibe/core/tools/builtins/experimental_bash.py
git -C /home/arthur/dev/mistral-vibe archive b78b451 vibe/ | tar -x -C <scratch>
```

**Every line number below is anchored at the pin.** The local checkout may sit
at another revision, where the same symbol has moved. At v2.24.2 this subsystem
has been reorganized; see `## Non-Goals`.

**Every `vibe/...` path in this document resolves against that root**, in the
table below and in each story's `Reference:` line alike. The root is spelled in
full once per story so a reader who opens a single story can navigate without
scrolling back here.

| Symbol | Reference path | Lines | Stories |
|---|---|---|---|
| `selection_priority` default | `vibe/core/tools/base.py` | 160 | US-300 |
| `local_managed_shell_only` default | `vibe/core/tools/base.py` | 165 | US-299 |
| `get_result_extra` | `vibe/core/tools/base.py` | 468-475 | US-295 |
| `local_managed_shell_runtime_enabled` | `vibe/core/tools/manager.py` | 92-107 | US-291, US-299 |
| the runtime gate | `vibe/core/tools/manager.py` | 325-331 | US-299 |
| `_is_enabled_for_shell_rollout` | `vibe/core/tools/manager.py` | 341-353 | US-300 |
| `_select_available_variant` | `vibe/core/tools/manager.py` | 361-380 | US-300 |
| `_tool_selection_priority` | `vibe/core/tools/manager.py` | 384-385 | US-300 |
| gate resolved from client tools | `vibe/app_server/_runtime.py` | 212-214 | US-299 |
| generic result rendering | `vibe/core/agent_loop/_loop.py` | 2225-2228 | US-295 |
| skill-path result rendering | `vibe/core/agent_loop/_loop.py` | 1753 | US-295 |
| `BashToolConfig.max_output_bytes` | `vibe/core/tools/builtins/bash.py` | 282-320 | US-301 |
| `Bash.shell_rollout` | `vibe/core/tools/builtins/bash.py` | 327 | US-300 |
| `Bash._build_result` | `vibe/core/tools/builtins/bash.py` | 538-552 | US-293 |
| silent output cap | `vibe/core/tools/builtins/bash.py` | 585-590, 605-607 | US-293 |
| `DEFAULT_INLINE_BYTES` | `vibe/core/tools/builtins/experimental_bash.py` | 62-64 | US-301 |
| `SessionInfo` | `vibe/core/tools/builtins/experimental_bash.py` | 480-493 | US-296 |
| `OutputChunk` | `vibe/core/tools/builtins/experimental_bash.py` | 494-499 | US-303 |
| `_now_iso` | `vibe/core/tools/builtins/experimental_bash.py` | 500-503 | US-296 |
| `session_prefix`, `base_dir`, `sessions_dir` | `vibe/core/tools/builtins/experimental_bash.py` | 570-578 | US-297, US-298 |
| `_session_info_locked` | `vibe/core/tools/builtins/experimental_bash.py` | 1063-1077 | US-296 |
| `_session_metadata` | `vibe/core/tools/builtins/experimental_bash.py` | 1078-1081 | US-296 |
| `_save_manifest` | `vibe/core/tools/builtins/experimental_bash.py` | 1082-1087 | US-296 |
| `_load_orphaned_manifests` | `vibe/core/tools/builtins/experimental_bash.py` | 1088-1112 | US-298 |
| `_read_file_chunk` | `vibe/core/tools/builtins/experimental_bash.py` | 1117-1143 | US-303 |
| `_new_session_id` | `vibe/core/tools/builtins/experimental_bash.py` | 1146-1148 | US-297 |
| `ExperimentalBashToolConfig.max_inline_bytes` | `vibe/core/tools/builtins/experimental_bash.py` | 1181-1184 | US-301 |
| `ExperimentalBashResult` | `vibe/core/tools/builtins/experimental_bash.py` | 1251-1266 | US-292 |
| `BashOutputResult` | `vibe/core/tools/builtins/experimental_bash.py` | 1281-1290 | US-294 |
| `BashStdinResult` | `vibe/core/tools/builtins/experimental_bash.py` | 1320-1325 | US-294 |
| `BashSessionsResult` | `vibe/core/tools/builtins/experimental_bash.py` | 1355-1364 | US-294 |
| `BashLogFileResult` | `vibe/core/tools/builtins/experimental_bash.py` | 1379-1387 | US-294 |
| `ExperimentalBash` class vars | `vibe/core/tools/builtins/experimental_bash.py` | 1611-1630 | US-299, US-300 |
| `ExperimentalBash.run` | `vibe/core/tools/builtins/experimental_bash.py` | 1707-1773 | US-289, US-301 |
| `_result_from_session` | `vibe/core/tools/builtins/experimental_bash.py` | 1774-1812 | US-292, US-302 |
| `BashOutput`, `BashStdin`, `BashSessions`, `BashLogFile` | `vibe/core/tools/builtins/experimental_bash.py` | 1813, 1906, 1990, 2134 | US-289, US-294 |
| polling window call sites | `vibe/core/tools/builtins/experimental_bash.py` | 1884, 2092, 2224 | US-301 |
| `GIT_BASH_SESSION_PREFIX` | `vibe/core/tools/builtins/git_bash.py` | 59 | US-297 |
| `GitBash`, managed and ungated | `vibe/core/tools/builtins/git_bash.py` | 193-200 | US-300 |
| `ExperimentalGitBash` | `vibe/core/tools/builtins/git_bash.py` | 329-336 | US-299 |
| `WINDOWS_SESSION_PREFIX` | `vibe/core/tools/builtins/windows_shell.py` | 79 | US-297 |
| `WindowsShell`, managed and ungated | `vibe/core/tools/builtins/windows_shell.py` | 849-858 | US-300 |
| `ExperimentalWindowsShell` | `vibe/core/tools/builtins/windows_shell.py` | 986-993 | US-299 |
| backend name constants | `vibe/core/tools/builtins/managed_shell/_windows.py` | 21-22 | US-304 |
| ConPTY then WinPTY, then failure | `vibe/core/tools/builtins/managed_shell/_windows.py` | 313-344 | US-304, US-305 |
| POSIX backend name | `vibe/core/tools/builtins/managed_shell/_posix.py` | 40 | US-304 |

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
python3 scripts/parity/shell_session.py --check   # re-run must be byte-identical
python3 scripts/parity/tool_surface.py --check    # unchanged for the three existing quadrants
```

## Epics & User Stories

### EP-092: The shell session oracle

Build the instrument that no shell divergence in this document could have been
found without, and widen the surface census to the quadrant it never asked for,
before changing any behavior.

**Definition of Done:** `scripts/parity/shell_session.py` drives five reference
handlers over scripted commands and commits a corpus; the corpus replays inside
`cargo test --workspace --all-features` with an audited ledger, a case floor of
70 and a per-tool floor of 5; `scripts/parity/tool_surface.py` captures four
quadrants instead of three; a re-run of either capture with no change in between
is byte-identical.

#### US-289: Capture the reference managed shell session contract
**Description:** As a person reading the scorecard, I want the reference's own five shell handlers driven over scripted commands and their results committed, so that row 6's session behavior is compared instead of assumed.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1611-1812` for the managed command handler, `:1813`, `:1906`, `:1990`, `:2134` for the four session tools, `:1063-1112` for the manifest it writes, `:480-499` for `SessionInfo` and `OutputChunk`. Pattern to copy: `resolve_reference`, `extract_pinned_tree`, `reexecute_with_reference_interpreter`, `stabilize`, `build_corpus` and `--check` in `scripts/parity/tool_execution.py`.

**Acceptance Criteria:**
- [ ] Given the pinned tree, when the capture runs, then it re-executes itself under the reference interpreter and refuses to run against a checkout at any other commit than `EXPECTED_COMMIT`.
- [ ] Given the capture runs, when it resolves the session directory, then `VIBE_HOME` points inside its own temporary directory, and the user's real `$VIBE_HOME/shell-tool/` is neither read nor written, asserted by the capture itself before the first case.
- [ ] Given a case for the managed command handler, when it runs, then the record carries the typed result as `model_dump(mode="json")` with every declared field present including the null ones, the model-facing text the agent loop would render from it, and the outcome kind.
- [ ] Given a case that leaves a session on disk, when it runs, then the record also carries the manifest as written and the `SessionInfo` the manager reports, both with their full key sets.
- [ ] Given a volatile field, when it is recorded, then it is normalized by shape and not dropped: a session identifier becomes its prefix plus a marker naming the stamp format and the hex length, an ISO-8601 timestamp becomes a marker naming its format and offset, and an absolute path is relativized against the case's temporary root.
- [ ] Given the case list, when the capture runs, then it covers at minimum a foreground command that exits zero, one that exits non-zero, one that backgrounds past the soft timeout, one killed by the hard timeout, one whose output exceeds the byte window, one writing to stderr only, one producing no output at all, a poll before any output, a poll after completion, a poll with an explicit `max_bytes`, a stdin write to a running session, a stdin write to a session that has exited, a list with no session, a list with two, a kill, a reset, a log read from the start, a log read from a cursor past the end and a log read whose file was deleted between the run and the read.
- [ ] Given a case raises, when the record is written, then it stores the exception class name and a SHA-256 digest of the message, never the sentence, and the digest carries a `<described>` marker naming its length.
- [ ] Given the reference checkout is absent, when the capture runs, then it exits non-zero naming the expected path and the `VIBE_REFERENCE` override, and writes no partial corpus.
- [ ] Given the capture is run twice with no change in between, when the two corpora are compared, then they are byte-identical, verified by `--check`.
- [ ] Given a session the capture started is still running, when the capture ends for any reason including an exception, then every process it started is terminated, asserted by a final sweep that fails the run if any survives.

#### US-290: Replay the shell session corpus with a ledger and a floor
**Description:** As a contributor, I want the committed shell corpus replayed against this port's own handlers on every test run, so that a shell divergence fails `cargo test` instead of aging into a wrong score.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** US-289
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1251-1387` for the five result shapes being compared. Pattern to copy: `observe`, `Case::document`, the `Divergence` struct with `tool`, `case`, `pointer`, `closed_by`, `row` and `why`, `assert_corpus_floor` and `every_ledger_entry_names_what_closes_it` in `crates/vibe-app-server/src/tool_execution_parity_tests.rs`.

**Acceptance Criteria:**
- [ ] Given the committed corpus, when `cargo test --workspace --all-features` runs, then the replay executes unconditionally and does not depend on the reference checkout being present.
- [ ] Given the reference checkout is absent or off-pin, when the replay runs, then only the live probe skips, with a printed reason naming both commits and the restore command.
- [ ] Given a case, when it is replayed, then the comparison covers the typed result field by field, the rendered model text, the outcome kind, and where the case carries them, the manifest and the session info.
- [ ] Given a difference no ledger entry names, when the replay runs, then it fails and prints the tool, the case and the JSON pointer, with `unlisted` asserted empty.
- [ ] Given a ledger entry, when the suite runs, then it names exactly one JSON pointer on one named case with no wildcard, and carries a `row` naming an existing row of `docs/parity.md` and a `why`.
- [ ] Given a ledger entry whose difference no longer reproduces, when the replay runs, then it fails naming the stale entry.
- [ ] Given the corpus, when the replay runs, then it fails below 70 cases, below 5 tools, or below 5 cases for any one tool.
- [ ] Given the replay passes, when it finishes, then it prints one line carrying the matched count, the total, the pinned commit, the tool count and the number of ledger entries exercised, and every number row 6 later quotes is read off that line.
- [ ] Given the corpus schema changes without a version bump, when the replay runs, then it fails naming the expected and found versions.
- [ ] Given the corpus is audited for cleartext, when the audit runs, then no reference-authored sentence is present and every recorded message is a digest.

#### US-291: Capture the fourth surface quadrant
**Description:** As a person reading the surface census, I want `surface()` parameterized on the managed-shell runtime gate, so that the one quadrant where the published surface diverges stops being the one the capture never asks for.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:92-107` for the parameter and its `True` default, `:325-331` for the gate it feeds. Local: `scripts/parity/tool_surface.py:133-200` and `crates/vibe-app-server/tests/tool-surface/baseline.json`.

**Acceptance Criteria:**
- [ ] Given `surface()`, when it constructs `ToolManager`, then it passes `local_managed_shell_runtime_enabled` explicitly rather than inheriting the default.
- [ ] Given the four combinations of the managed rollout and the runtime gate, when the capture runs, then all four are recorded, each labeled by the two flags that produced it.
- [ ] Given the fourth quadrant on Linux, when it is captured, then the recorded shell surface is exactly one name and the recorded class serving it is the legacy one.
- [ ] Given the three quadrants captured before this story, when the capture re-runs, then their recorded surfaces are unchanged, so the widening is additive and no existing assertion moves.
- [ ] Given the baseline is regenerated, when the replay runs, then `missingNames` and `extraNames` are compared per quadrant rather than once for the whole capture.
- [ ] Given the reference checkout is absent, when the capture runs, then it exits non-zero naming the expected path and writes no partial baseline.

### EP-093: The documents a model actually reads

Rewrite the six published result shapes and the text rendered on top of them, so
that a prompt written against the reference reads the same document here.

**Definition of Done:** all fifteen shell tools publish the reference's field
names and field sets; no shell tool publishes a field the reference does not
declare; the model-facing text is derived from the typed result by the
reference's rendering rule rather than composed by hand; the corpus from EP-092
replays these six shapes with zero unledgered pointers.

#### US-292: Publish the managed command result with the reference's thirteen fields
**Description:** As the model, I want the managed shell result to carry the fields the reference declares, so that `shell`, `background`, `stderr` and `returncode` are there to read.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** US-290
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1251-1266` for the thirteen fields, `:1774-1812` for how each is filled from the session, in particular `output` raw against `stdout` with CRLF normalized, `stderr` always empty and `returncode` defaulting to zero. Local: `crates/vibe-core/src/tools/shell/session.rs:674-687`.

**Acceptance Criteria:**
- [ ] Given a managed command completes, when its result is published, then the typed document carries exactly `command`, `session_id`, `status`, `exit_code`, `shell`, `background`, `output`, `next_cursor`, `truncated`, `output_path`, `stdout`, `stderr` and `returncode`, in snake_case, with no additional key.
- [ ] Given a field the reference declares as nullable is null, when the result is published, then the key is present with a null value rather than omitted.
- [ ] Given the session produced output containing CRLF, when the result is published, then `output` carries it unchanged and `stdout` carries it with CRLF normalized to LF.
- [ ] Given the session has no separate error stream, when the result is published, then `stderr` is the empty string rather than null.
- [ ] Given the session has no exit code yet, when the result is published, then `returncode` is zero and `exit_code` is null, which are two different fields answering two different questions.
- [ ] Given `backpressureDropped` was published before this story, when the result is published after it, then the key is gone from the typed document and the condition it reported is carried by `reader_error` on the session info instead.
- [ ] Given the corpus from US-289, when the replay runs, then every pointer under this result is either matched or named by a ledger entry.
- [ ] Given a consumer reading the previous camelCase keys exists anywhere in the workspace, when the change lands, then it is updated in the same commit and the full suite passes unfiltered.

#### US-293: Publish the legacy command result with the reference's four fields
**Description:** As the model, I want the legacy shell result to carry four fields and no invented fifth, so that the delegating path matches the reference too.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** US-290
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/bash.py:282-320` for `BashResult` and the 16 000 byte default, `:538-552` for the non-zero exit raising, `:585-590` and `:605-607` for the silent cap that writes no marker. Local: `crates/vibe-core/src/tools/shell.rs:601-608`.

**Acceptance Criteria:**
- [ ] Given a legacy command completes, when its result is published, then the typed document carries exactly `command`, `stdout`, `stderr` and `returncode`, in snake_case.
- [ ] Given the output exceeds the byte window, when the result is published, then it is truncated silently, `truncated` is absent from the document, and no `[output truncated at N bytes]` marker appears anywhere in the result or the rendered text.
- [ ] Given the command exits non-zero, when the result is built, then it is an error carrying the same three streams rather than a successful result with a non-zero code.
- [ ] Given the command writes to stderr and exits zero, when the result is published, then `stderr` carries the text and the call is a success.
- [ ] Given the corpus from US-289, when the replay runs, then every pointer under this result is either matched or named by a ledger entry.
- [ ] Given the truncation marker is removed, when `CHANGELOG.md` is written, then the removal is recorded as a user-visible behavior change under `## Unreleased`.

#### US-294: Publish the four session-tool results with their declared shapes
**Description:** As the model, I want `<family>_output`, `<family>_stdin`, `<family>_sessions` and `<family>_log_file` to publish the reference's four result shapes, so that polling and session management read the same documents everywhere.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** US-290
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1281-1290` for `BashOutputResult`, `:1320-1325` for `BashStdinResult`, `:1355-1364` for `BashSessionsResult`, `:1379-1387` for `BashLogFileResult`, and `:480-493` for the `SessionInfo` the sessions result embeds. Local: `crates/vibe-core/src/tools/shell/session_tools.rs:104-110`, `:195`, `:235`, `:282`.

**Acceptance Criteria:**
- [ ] Given a poll returns, when its result is published, then the document carries the seven fields `BashOutputResult` declares, in snake_case, with nullable fields present and null rather than omitted.
- [ ] Given a stdin write returns, when its result is published, then the document carries the three fields `BashStdinResult` declares and no invented `bytes_written` variant name.
- [ ] Given a sessions call returns, when its result is published, then the document carries the seven fields `BashSessionsResult` declares, nullable ones included, and every embedded session is a full `SessionInfo` rather than a subset.
- [ ] Given a log-file read returns, when its result is published, then the document carries the six fields `BashLogFileResult` declares.
- [ ] Given a sessions action the reference does not declare is requested, when the call runs, then it is refused with the same argument-boundary error the other tools use rather than answered with an invented action key.
- [ ] Given the corpus from US-289, when the replay runs, then every pointer under these four results is either matched or named by a ledger entry.

#### US-295: Render the model text from the typed result
**Description:** As the model, I want the shell tools' text to be the labeled field list every other tool produces, so that an empty output is distinguishable from a failed command without a second call.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** US-292, US-293, US-294
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py:2225-2228` for the rendering rule, `:1753` for the same rule on the skill path, `/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py:468-475` for the hook that returns `None` for every tool in the tree. Local: `crates/vibe-core/src/tools/shell.rs:601-608` and `crates/vibe-core/src/tools/shell/session.rs:674-687`.

**Acceptance Criteria:**
- [ ] Given any of the fifteen shell tools returns a result, when its model text is built, then it is the typed document rendered as one `key: value` line per field, in declaration order, joined by single newlines.
- [ ] Given a field is null, when the text is rendered, then the line is present with the value the reference's JSON dump produces for it rather than skipped.
- [ ] Given the tool contributes no extra, when the text is rendered, then nothing is appended after the field list, and no `stderr:` block, truncation marker or dropped-output notice is added.
- [ ] Given output was truncated, when the text is rendered, then the fact is carried by the `truncated` field on the line the field list already produces and by nothing else.
- [ ] Given a shell tool renders text, when the rendering code is read, then it is one shared function over the typed document rather than a per-tool string composition, so a new shell tool cannot diverge by omission.
- [ ] Given the corpus from US-289, when the replay runs, then the rendered text matches for every case or is named by a ledger entry scoped to that case's text pointer.

### EP-094: Persisted state both implementations can read

Converge the manifest, the identifier and the orphan scan, which as a side
effect makes the shared session directory shared in fact rather than in name.

**Definition of Done:** manifests carry the reference's eleven snake_case fields
with ISO-8601 timestamps; identifiers are minted in the reference's format; an
orphan written by either implementation is listed and reclaimed by both, proven
by a test that writes a reference-shaped manifest by hand and reads it back.

#### US-296: Write the manifest with the reference's eleven fields
**Description:** As an operator, I want a session manifest in the reference's format, so that a leftover session is readable by whichever implementation finds it.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** US-290
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:480-493` for the eleven fields, `:500-503` for the timestamp format, `:1063-1087` for how the info, the metadata and the manifest are produced. Local: `crates/vibe-core/src/tools/shell/session.rs:202` and its writer.

**Acceptance Criteria:**
- [ ] Given a session is saved, when the manifest is written, then it carries exactly `session_id`, `command`, `cwd`, `shell`, `pty_backend`, `status`, `exit_code`, `output_path`, `created_at`, `updated_at` and `reader_error`, in snake_case.
- [ ] Given a timestamp is written, when the manifest is read, then it is an ISO-8601 string with an explicit UTC offset rather than an epoch-millisecond number or string.
- [ ] Given `backpressureDropped` was a manifest key before this story, when a manifest is written after it, then the key is gone and a dropped-output condition is reported through `reader_error` instead.
- [ ] Given a manifest is written, when it is compared byte for byte with the reference's writer output for the same values, then the two agree on key order and on indentation.
- [ ] Given a session has not exited, when the manifest is written, then `exit_code` and `reader_error` are present and null rather than omitted.
- [ ] Given a manifest written by this port, when the reference's loader shape is applied to it in a test, then every field it requires is present and typed as it expects.
- [ ] Given the corpus from US-289, when the replay runs, then every manifest pointer is either matched or named by a ledger entry.

#### US-297: Mint session identifiers in the reference's format
**Description:** As an operator, I want session identifiers in the reference's format, so that a filename on disk means the same thing to both implementations.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** US-290
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1146-1148` for the format, `:570-578` for the prefix and the directory, `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/git_bash.py:59` and `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/windows_shell.py:79` for the two other prefixes.

**Acceptance Criteria:**
- [ ] Given a session is created, when its identifier is minted, then it is the family prefix, an underscore, a UTC stamp formatted `%Y%m%d_%H%M%S`, an underscore, and eight lowercase hexadecimal characters.
- [ ] Given the three families, when identifiers are minted, then the prefixes are `bash`, `git_bash` and `powershell`, matching the three the reference declares.
- [ ] Given two sessions are created inside the same second, when their identifiers are compared, then they differ, and no counter or sequence appears in the format.
- [ ] Given an identifier, when the orphan scan filters on it, then the prefix test is `starts_with("{prefix}_")` on the identifier and not on the file name alone.
- [ ] Given an identifier of the previous format exists on disk, when the scan runs, then it is ignored rather than crashing the scan, and the case is covered by a test.

#### US-298: Read orphans written by either implementation
**Description:** As an operator, I want the orphan scan to read the key the reference writes, so that a session left behind by one implementation is not invisible to the other.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** US-296, US-297
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1088-1112` for the scan, its `session_id` filter, its prefix test and its status and timestamp rewrite. Local: `crates/vibe-core/src/tools/shell/session.rs:68-107`.

**Acceptance Criteria:**
- [ ] Given a manifest in the sessions directory, when the scan runs, then it is selected on the presence of `session_id` and on the family prefix, matching the reference's two conditions.
- [ ] Given an orphan is loaded, when it is rewritten, then `status` becomes `orphaned` and `updated_at` becomes the current ISO-8601 stamp, and no other field is touched.
- [ ] Given a manifest written by the reference implementation, when this port's scan runs, then the session is listed as `orphaned` with all eleven fields readable.
- [ ] Given a manifest this port wrote, when it is read by a loader built to the reference's shape in a test, then the session is listed as `orphaned` with all eleven fields readable.
- [ ] Given a file in the directory that is not JSON, or is JSON without `session_id`, when the scan runs, then it is skipped and the scan continues rather than failing.
- [ ] Given the scan rewrites a manifest, when the write fails, then the failure is reported for that session and the remaining orphans are still loaded.

### EP-095: The gate that decides what gets published

Model the reference's `local_managed_shell_only` and the priority arbitration
behind it, so a client hosting its own terminal is offered the surface the
reference offers it.

**Definition of Done:** a client declaring the `terminal` capability receives one
shell name per family instead of five, on every platform; the class serving that
name is chosen by the reference's priority rule; the fourth quadrant captured by
US-291 replays with zero divergence.

#### US-299: Withhold the managed variants when the client hosts a terminal
**Description:** As an editor client that runs its own terminal, I want the managed session tools withheld, so that I am not offered session management I already own.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** US-291
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py:165` for the flag, `/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:325-331` for the gate, `/home/arthur/dev/mistral-vibe/vibe/app_server/_runtime.py:212-214` for how the runtime resolves it from the client tools, `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1625` and the four siblings for which classes carry it. Local: `crates/vibe-core/src/tools/shell.rs:144-300` and `:469-500`.

**Acceptance Criteria:**
- [ ] Given the managed rollout is on and the client declares a terminal, when the shell tools are registered, then the four session tools are not published and the managed command variant is not published.
- [ ] Given the managed rollout is on and the client declares no terminal, when the shell tools are registered, then all five names are published, unchanged from today.
- [ ] Given the managed rollout is off, when the shell tools are registered, then the gate changes nothing, matching the two upper quadrants of the probe.
- [ ] Given the host is Linux, when the gate withholds the managed variants, then the collapse to one name happens there too and not only on Windows.
- [ ] Given the gate is evaluated, when the capability is read, then it comes from the same client capability `delegated_command` already reads rather than from a second source of truth.
- [ ] Given no client is attached at all, when the shell tools are registered, then the gate does not withhold anything, because no host provides a terminal.
- [ ] Given the fourth quadrant of the baseline from US-291, when the surface replay runs, then it matches with no entry in `missingNames` or `extraNames`.

#### US-300: Select the surviving variant by the reference's priority rule
**Description:** As a client, I want the class serving a shell name chosen the way the reference chooses it, so that withholding the managed variant leaves the right tool behind rather than nothing.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** US-299
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py:160` for the default priority, `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1623` for the managed priority, `/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:361-385` for the arbitration and `:341-353` for the rollout filter, `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/git_bash.py:193-200` and `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/windows_shell.py:849-858` for the two managed classes that do not carry the gate.

**Acceptance Criteria:**
- [ ] Given two classes publish the same shell name and both are available, when the surface is built, then the one with the higher priority wins, and ties are broken by discovery order.
- [ ] Given the managed variant is withheld by the gate, when the surface is built, then the legacy variant is selected for that name rather than the name disappearing.
- [ ] Given the host is Windows and the gate withholds the managed Git Bash and PowerShell variants, when the surface is built, then `git_bash` and `powershell` are still published, served by the classes that carry the managed rollout without the gate.
- [ ] Given the host is Linux and the managed rollout is on with the gate withholding, when the surface is built, then `bash` is published and served by the legacy class, matching the fourth quadrant of the probe.
- [ ] Given the legacy variant is unavailable on the host, when the managed one is withheld, then the name is not published at all rather than published by an unavailable class.
- [ ] Given the arbitration is implemented, when the surface replay runs across all four quadrants, then the class serving each name matches the captured one.

### EP-096: Execution semantics and PTY backends

Close the four execution divergences and the two Windows backend gaps, each of
which changes what a caller observes rather than only what it reads.

**Definition of Done:** the managed window is the one the reference's managed
command uses; a session that did not complete is not reported as a success; a
missing log answers an empty chunk; the Windows backends are named and ordered
the way the reference names and orders them, and a host with no PTY fails rather
than degrading silently or the degradation is a recorded divergence with a test.

#### US-301: Bound the managed inline window with the reference's field
**Description:** As a caller, I want the managed command to read the window the reference reads, so that the same command truncates at the same byte on both sides.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** US-290
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1716` for the managed command reading `max_output_bytes`, `:1884`, `:2092` and `:2224` for the three polling tools reading `max_inline_bytes`, `:1181-1184` and `:62-64` for the 30 000 default, `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/bash.py:282-320` for the 16 000 default. Local: `crates/vibe-core/src/tools/shell/session.rs:285`.

**Acceptance Criteria:**
- [ ] Given a managed command runs, when its inline window is computed, then it comes from the tool's `max_output_bytes` and not from `max_inline_bytes`.
- [ ] Given the defaults are unchanged, when a managed command produces 20 000 bytes, then the result is truncated, where the same output through a poll is not.
- [ ] Given a poll, a sessions call or a log read runs, when its window is computed, then it stays `args.max_bytes` when given and `max_inline_bytes` otherwise, unchanged from today.
- [ ] Given a configuration sets `max_output_bytes` explicitly, when a managed command runs, then that value is what bounds it, asserted by a test that sets it to a small number.
- [ ] Given the two windows now differ, when `crates/vibe-core/tests/tool-config/defaults.json` is regenerated, then both keys are declared with a live reader and the config replay passes.
- [ ] Given the corpus from US-289, when the replay runs, then the truncation cases match at the same boundary.

#### US-302: Require a completed status before reporting success
**Description:** As a caller, I want a session that was killed on its way out to be reported as a failure, so that a zero exit code on an incomplete session stops looking like success.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** US-290
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1774-1812` for the condition, which requires both a completed status and a zero return code. Local: `crates/vibe-core/src/tools/shell/session.rs:313-319`.

**Acceptance Criteria:**
- [ ] Given a managed command whose session status is not `completed`, when success is evaluated, then the call is an error regardless of the exit code.
- [ ] Given a session with status `completed` and a non-zero code, when success is evaluated, then the call is an error, unchanged from today.
- [ ] Given a session with status `completed` and a zero code, when success is evaluated, then the call succeeds.
- [ ] Given a session that backgrounded past the soft timeout, when the result is returned, then it is not evaluated against this condition at all, because it is a poll-style return rather than a finished command.
- [ ] Given a session killed by the hard timeout, when the result is returned, then it is an error naming the timeout rather than a success carrying the code the kill produced.
- [ ] Given the corpus from US-289, when the replay runs, then the killed and timed-out cases match on outcome kind.

#### US-303: Answer an empty chunk for a log file that is not there
**Description:** As a caller, I want reading a log that no longer exists to answer an empty chunk, so that a deleted file is an empty read rather than a failed call.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** US-290
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1117-1143` for the chunk reader, which returns an empty chunk at the requested cursor when the path is missing, and `:494-499` for `OutputChunk`. Local: `crates/vibe-core/src/tools/shell/decode.rs:111-140`.

**Acceptance Criteria:**
- [ ] Given the log path does not exist, when a chunk is read, then the result is an empty output at the requested cursor with `truncated` false, and no error is raised.
- [ ] Given the log path exists but cannot be opened for another reason, when a chunk is read, then the error is still raised, so the empty answer is scoped to absence and not to every failure.
- [ ] Given the cursor is past the end of an existing file, when a chunk is read, then the answer is an empty output at the file size, unchanged from today.
- [ ] Given a session whose log was deleted while it ran, when a poll runs, then the poll answers rather than failing, and the session status is still reported.
- [ ] Given the corpus from US-289, when the replay runs, then the deleted-log case matches on both the typed result and the outcome kind.

#### US-304: Name and order the Windows PTY backends the way the reference does
**Description:** As a Windows client, I want `pty_backend` to carry the value the reference emits, so that a client branching on the backend name branches the same way here.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** US-296
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/managed_shell/_windows.py:21-22` for the two constants, `:313-344` for the ConPTY-then-WinPTY order, `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/managed_shell/_posix.py:40` for the POSIX name.

**Acceptance Criteria:**
- [ ] Given a POSIX session, when its backend is reported, then it is `posix`, matching the reference's spelling.
- [ ] Given a Windows session started on ConPTY, when its backend is reported, then it is `ConPTY` with the reference's exact casing.
- [ ] Given ConPTY cannot start and a second backend is available, when the session starts, then it is tried next and reported as `WinPTY`.
- [ ] Given `portable-pty` cannot express the second backend without a new dependency, when this story lands, then the fallback is recorded as an accepted divergence naming the constraint, and the first two criteria still hold.
- [ ] Given the backend name is written to a manifest, when the manifest is read, then it carries the same spelling the result carries, so the two never disagree.
- [ ] Given the backend names are asserted, when the test runs, then it fails on a lowercase spelling, which is what the current implementation emits.

#### US-305: Fail the session when no PTY backend starts
**Description:** As a caller, I want a host with no working PTY to fail the session, so that a reduced capability is not reported as a working one.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** US-304
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/managed_shell/_windows.py:313-344` for the loop that collects each backend's failure and raises once all are exhausted.

**Acceptance Criteria:**
- [ ] Given no PTY backend starts, when a managed session is requested, then the call is an error rather than a session running on pipes.
- [ ] Given the error is raised, when its payload is inspected, then it names each backend that was tried, without reproducing the reference's sentence.
- [ ] Given a backend does start, when the session runs, then nothing about this story changes its behavior.
- [ ] Given the pipe fallback is removed for the managed path, when the legacy path runs, then it still executes on pipes, because it never claimed a PTY.
- [ ] Given `ptyBackend` was reported as null in the fallback case before this story, when the change lands, then no code path reports a null backend on a running managed session.
- [ ] Given the removal changes an observable behavior, when `CHANGELOG.md` is written, then it is recorded under `## Unreleased`.

### EP-097: Restate row 6 from its oracle

Price the row from the measurement rather than from a reading, correct the claim
that is false, and put every remaining difference in a divergence table.

**Definition of Done:** row 6 reads 100 with a restatement date, names the
oracle and the command that reproduces it, quotes only numbers the replay
prints, and carries no clause contradicted by the tree; every remaining
difference has a row in `## Open divergences` or `## Accepted divergences` bound
to a failing test.

#### US-306: Record the shell divergences in the tables that hold them
**Description:** As a reader of the scorecard, I want every remaining shell difference in a divergence table, so that the row's score is explained by named entries rather than by a residual sentence.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** US-292, US-293, US-294, US-295, US-296, US-297, US-298, US-299, US-300, US-301, US-302, US-303, US-304, US-305
**Reference:** `docs/parity.md:213` for the open table and `:230` for the accepted one. Pattern to copy: the binding in `crates/vibe-core/src/parity/ledger_tests.rs` that fails both on an entry naming a row nobody wrote and on a row the ledger no longer reaches.

**Acceptance Criteria:**
- [ ] Given a difference the ledger records after EP-093 through EP-096, when the tables are read, then it has exactly one row, in the open table if it is intended to close and in the accepted one if it is a decision.
- [ ] Given a row is written, when it is read, then it names the test that fails when the difference stops reproducing.
- [ ] Given a ledger entry names a row, when the binding test runs, then it fails if that row does not exist, and it fails if a row exists that the ledger no longer reaches.
- [ ] Given the WinPTY fallback could not be expressed, when the tables are read, then the accepted row names the dependency constraint and the test that fails if a second backend is ever added without updating it.
- [ ] Given a difference is licensing-bound rather than technical, when its row is written, then it names `NOTICE` as the reason and the corpus records a digest rather than the text.
- [ ] Given the tables are updated, when the full suite runs, then no ledger entry is unbound and no divergence is unlisted.

#### US-307: Remeasure and restate row 6
**Description:** As a reader of the scorecard, I want row 6 restated from the widened measurement, so that its score means what the other restated rows mean.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** US-306
**Reference:** `docs/parity.md:117` for the row as it stands, and rows 3, 4 and 5 for the restatement register. Local: `crates/vibe-core/src/tools/shell.rs:611-647`, which disproves the row's Windows-pipes clause.

**Acceptance Criteria:**
- [ ] Given the full CI sequence runs unfiltered, when it passes, then the row is restated, and not before.
- [ ] Given the row is restated, when it is read, then it carries the restatement date, the previous score, and the reason the previous score was wrong.
- [ ] Given the row quotes a number, when that number is checked, then it is read off the line the replay prints rather than counted by hand.
- [ ] Given the row named three residuals, when it is restated, then the Windows-pipes clause is gone, because `spec.terminal = managed` has no family condition, and the removal is stated as a correction rather than silently dropped.
- [ ] Given the row names an oracle, when a reader follows it, then the printed command reproduces the measurement, including the new shell session replay and the four-quadrant surface census.
- [ ] Given other rows read the same evidence, when they are checked, then rows 3, 4 and 12 are re-read and each is either confirmed unchanged or restated with its own reason, so this work does not leave a stale claim one row over.
- [ ] Given the score is set to 100, when the row is read, then it names what would make the score wrong, so a later reader can falsify it the way this pass falsified the previous one.

## Functional Requirements

- FR-01: The managed command result must carry the thirteen fields the reference
  declares, in snake_case, with nullable fields present.
- FR-02: The legacy command result must carry four fields and must truncate
  silently, writing no marker.
- FR-03: The four session tools must publish the four result shapes the
  reference declares.
- FR-04: No shell tool may publish a field the reference does not declare.
- FR-05: The model-facing text of every shell tool must be the typed document
  rendered as one `key: value` line per field, with nothing appended.
- FR-06: A session manifest must carry the reference's eleven snake_case fields
  with ISO-8601 timestamps.
- FR-07: A session identifier must be the family prefix, a UTC `%Y%m%d_%H%M%S`
  stamp and eight hexadecimal characters, joined by underscores.
- FR-08: The orphan scan must select on `session_id` and the family prefix, and
  must rewrite only `status` and `updated_at`.
- FR-09: A client that declares a terminal must not be offered the managed
  session tools, on any platform.
- FR-10: When two classes publish the same shell name, the higher
  `selection_priority` must win and the discovery order must break ties.
- FR-11: The managed command's inline window must come from `max_output_bytes`
  and each polling tool's from `max_inline_bytes`.
- FR-12: A managed command must be a success only when the session completed and
  the return code is zero.
- FR-13: Reading a log file that does not exist must answer an empty chunk at
  the requested cursor.
- FR-14: A PTY backend name must be reported with the reference's spelling, and
  a managed session must never report a null backend.
- FR-15: A managed session must fail when no PTY backend starts.
- FR-16: The surface capture must record all four combinations of the managed
  rollout and the runtime gate.
- FR-17: Every corpus committed by this work must carry the pinned reference
  commit and must fail its replay when that commit and
  `vibe_core::parity::REFERENCE_COMMIT` disagree.
- FR-18: No corpus committed by this work may contain reference-authored prose
  in cleartext.

## Non-Functional Requirements

- **Performance:** the shell session replay adds under 20 seconds to
  `cargo test --workspace --all-features` on the CI runner, process spawn time
  included, asserted by a test that fails above that bound.
- **Performance:** a managed command producing 1 MB of output completes its
  inline read in under 500 ms, so the window change costs no throughput.
- **Performance:** the orphan scan over a directory holding 200 manifests
  completes in under 200 ms.
- **Security:** no capture and no test writes into the user's real `$VIBE_HOME`;
  each sets `VIBE_HOME` inside its own temporary directory, verified by a test
  that fails when the variable is unset during a capture.
- **Security:** no process started by a capture or a test outlives it; a final
  sweep fails the run if one does.
- **Security:** a session log path accepted by `<family>_log_file` must resolve
  inside the sessions directory, asserted before every read, so the shape
  conversion in EP-094 cannot widen the accepted path set.
- **Reliability:** a missing or off-pin reference checkout never fails
  `cargo test`; the corpus replay runs unconditionally and the live probe skips
  with a printed reason.
- **Reliability:** both capture scripts are byte-identical across two
  consecutive runs with no change in between, verified by their `--check` modes.
- **Reliability:** a manifest written by either implementation is readable by
  the other, asserted in both directions by a test that writes one shape and
  reads it with the other loader.
- **Compatibility:** a session recorded before this work is either readable or
  skipped without failing the scan, and the skip is covered by a test.
- **Compatibility:** the three surface quadrants captured before US-291 replay
  unchanged, so the census widening is additive.
- **Maintainability:** no ledger entry is scoped wider than one field on one
  case, and every entry names a story ID or the licensing boundary.
- **Maintainability:** the model-text rendering is one function shared by all
  fifteen shell tools, so a new shell tool cannot diverge by omission.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Reference checkout absent | `VIBE_REFERENCE` unset and default path missing | Corpus replays; live probe skips | Printed skip reason naming the path and the override |
| 2 | Reference checkout off-pin | Local checkout at another commit | Corpus replays; live probe skips | Printed reason naming both commits and the restore command |
| 3 | Capture leaves a process behind | A session outlives the case that started it | Final sweep terminates it and fails the run | Error naming the case and the surviving process |
| 4 | Command produces no output | `true` | Result carries an empty `output` and a rendered line for it | Field list with an empty value, not an empty text |
| 5 | Command writes only to stderr | `echo x 1>&2` on the managed path | `output` carries it, `stderr` stays empty | Field list, no separate stderr block |
| 6 | Output exceeds the managed window | 20 000 bytes with default configuration | Truncated at `max_output_bytes` | `truncated: true` on its own line |
| 7 | Output exceeds a poll window | Same output read through `<family>_output` | Truncated at `max_inline_bytes` | `truncated: true` on its own line |
| 8 | Output ends mid-codepoint | A multibyte character straddling the window | Incomplete suffix trimmed before decoding | None |
| 9 | Session killed on shutdown | Hard timeout during a running command | Error, not a success carrying the kill's code | Error naming the timeout |
| 10 | Session backgrounds | Soft timeout elapses | Poll-style result, success condition not applied | Field list carrying `background: true` |
| 11 | Log file deleted while running | `rm` on the session log | Empty chunk at the requested cursor | None |
| 12 | Log path outside the sessions directory | A crafted `<family>_log_file` argument | Refused before any read | Message naming the sessions directory |
| 13 | Manifest written by the other implementation | Both binaries used on one machine | Listed as `orphaned` with all eleven fields | None |
| 14 | Manifest in the previous format | Upgrade over an existing `$VIBE_HOME` | Skipped, scan continues | None |
| 15 | Manifest that is not JSON | A truncated write after a crash | Skipped, scan continues | None |
| 16 | Two sessions created in one second | A scripted burst | Distinct identifiers | None |
| 17 | Client declares a terminal, Linux | ACP client with the `terminal` client tool | One shell name published, served by the legacy class | None |
| 18 | Client declares a terminal, Windows | Same, on Windows | `git_bash` and `powershell` published by the ungated managed classes | None |
| 19 | No client attached | CLI run with no editor host | Gate withholds nothing | None |
| 20 | Legacy variant unavailable on the host | Managed withheld and no legacy class available | Name not published at all | None |
| 21 | ConPTY unavailable | Older Windows host | WinPTY tried next, backend reported as `WinPTY` | None |
| 22 | No PTY backend starts | Container with no console device | Managed session fails | Error naming the backends tried |
| 23 | Backpressure drops output | A session outrunning its buffer | Reported through `reader_error` | Field list carrying the reader error |
| 24 | Stdin write to an exited session | `<family>_stdin` after completion | The reference's outcome for that case, replayed from the corpus | Whatever the corpus records, by kind |
| 25 | Sessions action the reference does not declare | A crafted `action` value | Refused at the argument boundary | Message naming the tool and the argument |
| 26 | Config sets `max_output_bytes` small | Explicit 100 in configuration | Managed command truncates at 100 | `truncated: true` |
| 27 | Corpus schema drift | A capture adds a field without a version bump | Replay fails | Error naming the expected and found versions |
| 28 | Stale ledger entry | A divergence was fixed but its entry remains | Staleness check fails | Error naming the entry |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | The capture cannot drive the reference's session handlers headlessly, because something in the path expects a terminal on the capture process's own stdin | Med | High | US-289 opens with this as its first criterion and the session opens its own PTY by construction; if it fails, the capture drives the `TerminalSessionManager` directly and records at that boundary, which still covers every result shape |
| 2 | Switching fifteen tools from camelCase to snake_case breaks a consumer the suite does not cover | Med | High | US-292 requires the full suite unfiltered and requires every in-workspace consumer updated in the same commit; the presentation corpus compares the display document separately, so the TUI path is already asserted |
| 3 | Removing `backpressureDropped` loses a real signal on a real failure mode | Med | Med | US-296 moves the condition to `reader_error`, which is the reference's own nullable field for exactly this, and edge case 23 asserts it |
| 4 | Removing the truncation marker makes silent truncation genuinely invisible to a model | Med | Med | The `truncated` field is on its own rendered line after US-295, so the fact is more visible than it was inside a suffix; US-293 records the change in `CHANGELOG.md` |
| 5 | The gate collapses the surface for a client that actually wants the session tools, and someone reads it as a regression | Med | Med | It is the reference's behavior and the probe measured it in all four quadrants; US-299 lands with the captured quadrant as its assertion, so the change is provable rather than argued |
| 6 | `portable-pty` cannot express a WinPTY fallback and the story stalls | High | Low | US-304 is written to produce a recorded answer either way, and its fourth criterion makes the constraint an accepted divergence rather than a blocked story |
| 7 | Failing the session when no PTY starts breaks a container workflow that works today on pipes | Med | High | US-305 scopes the failure to the managed path and leaves the legacy path on pipes; a host with no PTY still has a working shell tool through the legacy variant |
| 8 | The corpus captures a reference error sentence and it reaches the repository | Low | High | US-289 stores every message as a digest with a structural marker and US-290 audits the corpus for cleartext, mirroring the existing digest test on the tool surface |
| 9 | Process-spawning cases make the suite noticeably slower or flaky on CI | Med | Med | A 20 second bound is an explicit non-functional requirement; cases use short deterministic commands, and every timing-dependent case is captured by outcome kind rather than by wall clock |
| 10 | Row 6 turns out to have a tenth gap this pass missed, so 100 is claimed too early | Med | High | US-307 requires the full CI sequence unfiltered and requires the row to name what would make the score wrong; the oracle now covers the part that had none, which is where the unknown most plausibly lived, and this pass already found one false claim by falsifying the previous read |
| 11 | Changing the managed window to `max_output_bytes` surprises anyone relying on 30 000 bytes inline | Low | Low | US-301 asserts both windows separately and the config replay declares both keys; the reference's own two windows differ for the same reason |
| 12 | The shared sessions directory means a fix here changes what the reference implementation sees on the same machine | Low | Med | That is the intended outcome: US-298 asserts readability in both directions, so convergence is proven rather than assumed |

## Non-Goals

- Re-pinning the reference to v2.24.2. The pin stays at `b78b451`; a bump would
  require regenerating every committed corpus in the same change.
- Porting the v2.24.2 reorganization of this subsystem. The pinned
  `experimental_bash.py` is one module and this PRD targets its contract, not a
  later split of it.
- Reproducing the reference's tool description text, prompt files or error
  sentences. `NOTICE` forbids it; the corpus records digests and the tables
  record the reason.
- A Windows CI job. The corpus is captured on Linux and records its platform; the
  Windows-shaped rules in EP-096 are covered by unit tests, and a later Windows
  capture is additive.
- Adding a PTY dependency to reach WinPTY. US-304 records the constraint instead.
- Changing the shell policy. The 28 grammar extractions, 45 path-inspecting
  commands, 23 escaping-operand cases and 60 of 63 resolutions already measured
  by `shell_parity_tests` stay exactly as they are, including the three ledgered
  places this port asks where the reference grants.
- Widening the delegated-command path to carry `args` and `env`. It is out of
  scope for this row's residual and nothing measured here depends on it.
- Taking rows 3, 4 or 12 to a new score. US-307 re-reads them and states whether
  they moved; it does not restate them beyond what this work's evidence forces.

## Files NOT to Modify

- `crates/vibe-core/src/parity.rs`: carries the pin; changing it invalidates
  every committed corpus at once.
- `scripts/parity/pin.py`: the second pin source; the parity test fails when the
  two disagree or when a third copy appears.
- `NOTICE`: declares the licensing boundary this work operates under.
- `crates/vibe-core/src/shell/` and its `shell_parity_tests.rs`: the policy is
  measured and conformant; this work is about what the tools publish, not about
  what they allow.
- `crates/vibe-app-server/tests/tool-execution/corpus.json`: the eleven non-shell
  tools keep their own oracle; the shell gets a corpus of its own rather than
  widening this one.
- `crates/vibe-core/tests/tool-presentation/corpus.json`: regenerated only if a
  display document changes, which no story here requires.
- `vibe/**` in the reference checkout: read-only oracle, never written.

## Technical Considerations

- **Architecture:** should the shell corpus live under `crates/vibe-core/tests/shell-session/`
  or beside the existing tool-execution corpus in `vibe-app-server`? Recommended:
  `vibe-core`, because the handlers being compared live in
  `crates/vibe-core/src/tools/shell/` and the replay should sit beside the code
  it holds, the way the worktree corpus sits beside `worktree.rs`.
- **Architecture:** the model-text rendering could stay per-tool with each one
  updated, or become one function over the typed document. Recommended: one
  function. The reference's rendering knows nothing about shells, and a shared
  function is the only version where a sixteenth shell tool cannot diverge by
  forgetting.
- **Data Model:** the typed result for the shell family moves to snake_case,
  which makes it consistent with the eleven tools already measured. The
  `#[serde(rename_all = "camelCase")]` on `ToolExecutionOutput` itself is
  untouched: that attribute names the envelope's own fields, not the payload's.
- **Data Model:** `backpressureDropped` has no reference counterpart, and
  deleting the field without moving the condition would lose a real signal. It
  becomes a `reader_error` value, which is the reference's nullable field for a
  reader that failed, and the nearest honest home for it.
- **Data Model:** timestamps move from epoch milliseconds to ISO-8601 with an
  explicit UTC offset. Existing manifests are not migrated; the scan skips what
  it cannot read, which US-297 covers, because a session manifest is
  process-lifetime state and not durable user data.
- **API Design:** should the gate be a parameter on the shell registration
  function or a field on the host? Recommended: read it from the client
  capability the registration already has access to, the way
  `delegated_command` reads `supports_terminal()`, so there is one source of
  truth for what the client hosts.
- **Dependencies:** none new. `portable-pty` is already in the workspace and
  US-304 is written so that what it cannot express becomes a recorded
  divergence.
- **Migration:** the observable changes are the published field names, the
  removed truncation marker, the manifest format and the surface a
  terminal-hosting client receives. Three of the four are recorded in
  `CHANGELOG.md` by their stories; the fourth, the surface, is a parity fix a
  client cannot have depended on because no reference client does.
- **Sequencing:** EP-092 blocks everything except US-291, which is independent
  and can land first. EP-093 and EP-094 are independent of each other. EP-095
  needs US-291 only. EP-096 needs EP-092, and US-304 needs US-296 because the
  backend name is written into the manifest. EP-097 requires all of them.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Row 6 score | 92, priced by reading | 100, with a named oracle | Month-1 | `docs/parity.md` row 6 |
| Shell session cases replayed | 0 | 70 or more | Month-1 | Case count printed by the replay test |
| Reference shell handlers with an executable oracle | 0 of 5 | 5 of 5 | Month-1 | `scripts/parity/` inventory |
| Fields a managed shell result publishes that the reference does not | 1 | 0 | Month-1 | The result pointer set in the replay |
| Fields the reference publishes that a managed shell result omits | 5 | 0 | Month-1 | Same |
| Surface quadrants captured | 3 of 4 | 4 of 4 | Month-1 | `crates/vibe-app-server/tests/tool-surface/baseline.json` |
| Shell names published to a terminal-hosting client on Linux | 5 | 1 | Month-1 | The fourth quadrant of the surface replay |
| Manifests readable by both implementations | 0 of 2 directions | 2 of 2 | Month-1 | The bidirectional test in US-298 |
| False claims in row 6 | 1 | 0 | Month-1 | `docs/parity.md` row 6 against `shell.rs:611-647` |
| Row-6 gaps with no entry in a divergence table | 8 points' worth | 0 | Month-1 | `docs/parity.md` divergence sections |
| Full CI sequence, unfiltered | Passing | Passing | Month-6 | Four commands from the workspace root |

## Open Questions

- Should `backpressureDropped` survive as a `reader_error` value or disappear
  entirely? Owner: Arthur Jean, before US-296. Defaulted to surviving as a
  `reader_error` value, because dropping output silently is worse than a
  divergence in what fills a nullable field.
- Should the legacy truncation marker be kept as a deliberate accepted
  divergence rather than removed? Owner: Arthur Jean, before US-293. Defaulted
  to removal, because the `truncated` field renders on its own line after US-295
  and carries the same fact in the shape the reference carries it.
- Should a Windows CI job run the shell replay so EP-096 is observed rather than
  asserted? Owner: Arthur Jean, after US-307. Blocking nothing; the corpus
  records its platform so a Windows capture is additive.
- Does anything outside this workspace consume the camelCase shell result keys?
  Owner: Arthur Jean, before US-292. Nothing in the tree suggests one, and the
  eleven other tools already publish snake_case, so the shell family is the
  anomaly rather than the convention.
[/PRD]
