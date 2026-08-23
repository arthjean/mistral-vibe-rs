[PRD]
# PRD: CLI Surface at Full Parity

**Reference root:** every `vibe/...` path in this document is relative to the
read-only Python checkout at `/home/arthur/dev/mistral-vibe/`
(`C:\dev\mistral-vibe` on Windows, `VIBE_REFERENCE` overrides both), read at
commit `b78b451` and never from its working tree. See `## Reference Map` for the
read commands and the full symbol table.

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-23 | Arthur Jean | Initial draft: take parity row 7 from 92 to 100 |

## Problem Statement

Row 7 of `docs/parity.md` ("CLI surface (flags, modes)") scores 92 and prices
its residual in one sentence: "Every flag present, filtering conformant. Gap:
`vibe mcp add` and its 12 flags". Measured against the pin, that sentence is
one true claim, one half-true claim, and one gap that is four times larger than
it says.

1. **The flags half of row 7 has no oracle at all.** `scripts/parity/` holds 23
   capture scripts and not one of them renders an argument parser.
   `crates/vibe-cli/tests/runtime-parity/startup-oracle.py` is the closest
   thing, and its `CASES` are dicts of already-parsed argument values, never
   argv strings, so the ten traces in `startup.json` measure the *modes* half of
   the row and never touch the parser that produces them. Row 7's own method
   line promises "an oracle is reproduced with `cargo test -p <crate>
   --all-features <filter>`". For half the row there is nothing to reproduce.
   Every number in the 92 was priced by reading.

2. **"Every flag present" is true in one direction and false in the other.**
   The reference declares 19 optionals plus one positional
   (`vibe/cli/entrypoint.py:26-179`), and this port declares all 19
   (`crates/vibe-cli/src/lib.rs:46-125`). It also declares nine more the
   reference has no counterpart for: `--allowed-tool`, `--provider-style`,
   `--model`, `--input-price`, `--output-price`, `--api-base`,
   `--credential-environment`, `--session-root` and `--fake-response`, plus an
   `ndjson` alias on `--output streaming` that the reference's
   `choices=["text", "json", "streaming"]` does not accept. All nine are
   `hide = true`, which hides them from `--help` and not from argv: `vibe --model
   x` is accepted here and rejected there. A flag surface is what a parser
   accepts, not what its help prints.

3. **`vibe mcp add` is not the gap; `vibe mcp` is.** The entire command in this
   port is `crates/vibe-cli/src/mcp_command.rs`, 116 lines that recognize
   exactly one argv shape, `["remove", name]`, and answer every other shape with
   the same string, `Usage: vibe mcp remove <name>`, on stderr with exit 1. The
   reference builds a real sub-command parser
   (`vibe/cli/mcp_command.py:73-157`) with two sub-parsers, `-h` on all three
   levels, argparse's exit-2 error grammar, and a bare `vibe mcp` that prints
   help on stdout and exits 0. Measured side by side, seven argv shapes diverge
   in exit code and all seven diverge in text:

   | argv | reference exit | this port exit |
   |---|---|---|
   | `mcp` | 0, help on stdout | 1, usage on stderr |
   | `mcp --help` | 0, help on stdout | 1, usage on stderr |
   | `mcp list` | 2, `invalid choice: 'list'` | 1, usage on stderr |
   | `mcp remove` | 2, `the following arguments are required: NAME` | 1, usage on stderr |
   | `mcp remove a b` | 2, `unrecognized arguments: b` | 1, usage on stderr |
   | `mcp add docs --url ...` | 0, server persisted | 1, "is not implemented" |
   | `mcp add docs --transport bogus` | 2, `invalid choice: 'bogus'` | 1, "is not implemented" |

4. **`vibe mcp remove` is present but not conformant.** Three differences, each
   observable on a single invocation. The reference answers `MCP server \`x\`
   is not configured in the user config.` and `Removed MCP server \`x\`.`
   (`vibe/cli/mcp_command.py:252-259`); this port drops "in the user config" and
   both trailing periods (`mcp_command.rs:38-42`). The reference deletes OAuth
   credentials before the config entry, in that order and for a stated reason
   (`vibe/core/tools/mcp/management.py:36-49`); this port deletes no credentials
   at all (`crates/vibe-core/src/config/integrations.rs:265-304`), so removing an
   OAuth server here leaves its token in the keyring.

5. **Four parse semantics diverge, and three of them accept-or-reject
   differently.** argparse runs with `allow_abbrev=True`, so `vibe --max-tur 3`
   is accepted upstream and rejected here with "unexpected argument". `type=int`
   accepts a negative value, so `--max-turns -5` runs upstream and is rejected
   here. `_run_programmatic_mode` gates on Python truthiness
   (`vibe/cli/cli.py:147-150`), so `vibe -p "   "` runs upstream and is rejected
   here by `trim().is_empty()` (`crates/vibe-cli/src/lib.rs:487`). And
   `get_prompt_from_stdin` reopens `/dev/tty` as stdin after draining a pipe
   (`vibe/cli/cli.py:53-67`) so that a piped prompt still leaves an interactive
   session with a keyboard; `populate_piped_prompt` does not
   (`crates/vibe-cli/src/tui/startup/invocation.rs:121-131`), so `echo hi | vibe`
   mounts a TUI reading a closed stdin.

6. **The failure surface diverges in exit code and in destination.** The
   reference exits 1 on a bad `--workdir`, printing `Error: --workdir does not
   exist or is not a directory: {resolved}` (`entrypoint.py:281-288`), and 1 on
   a bad `--add-dir` printing the *raw* argument rather than the resolved one
   (`:317-325`). This port answers both with one generic sentence, ``Error:
   startup I/O failed at `/x`: No such file or directory (os error 2)``
   (`crates/vibe-cli/src/tui/startup.rs:43-48`). The reference's guard for a
   deleted working directory (`entrypoint.py:306-315`, two lines, the second
   naming `--workdir` as the way out) has no counterpart here at all. And
   `CliError::InvalidArguments` maps to exit 2
   (`crates/vibe-cli/src/lib.rs:988`) where the reference's post-parse
   validation failures all exit 1.

7. **Nothing above is written down.** `docs/parity.md` has an "Open
   divergences" table and an "Accepted divergences" table, both machine-checked
   by `crates/vibe-core/src/parity/ledger_tests.rs`, and neither contains a
   single row-7 entry. Eight points of residual are recorded nowhere a test can
   read them.

**Why now:** rows 3, 4, 6 and 12 were each taken to a measured score by building
the oracle first and letting it find what reading had missed. Row 7 is the
largest remaining row whose instrument does not exist, and it is the row a user
touches before any other: it is the argv they type. Every other row's oracle
runs behind a CLI that has never been compared to the one it reimplements.

## Overview

This PRD builds the missing instrument, then closes what it finds.

The instrument is `crates/vibe-cli/tests/runtime-parity/cli-surface-oracle.py`, a capture that does to
argparse what `tool_surface.py` does to `ToolManager`: it introspects the three
parsers the reference builds (the root parser and the `add` and `remove`
sub-parsers), records every action's `option_strings`, `dest`, `nargs`, `const`,
`default`, `choices`, `metavar` and `required`, records the three `format_help()`
renders at a fixed width, and then drives a matrix of argv vectors through
`parse_args` and through `run_mcp_cli`, recording for each the exit code and the
shape of what reached stdout and stderr. A Rust module replays that corpus
against this port's own parser unconditionally, with an audited ledger and a
case floor, and skips only the live probe when the checkout is absent or
off-pin. That is the same skippable shape
`crates/vibe-cli/src/tui/runtime_parity_tests.rs:46` already uses, so a machine
with no reference checkout still passes `cargo test`.

What the instrument then forces is grouped into four behavior epics. `vibe mcp`
becomes a real sub-command parser with the reference's three help surfaces, its
exit-2 error grammar and its two sub-commands, including `add` with its
positional, its twelve options, its eleven validation rules, its OAuth login and
its five persistence outcomes. The top-level parser gains the reference's
metavars, declaration order and epilog, and its failures gain the reference's
exit codes, destinations and dedicated messages. The parse semantics gain
prefix inference, negative numeric values, Python-truthiness prompt gating and
the `/dev/tty` reopen.

One decision shapes every story: `NOTICE` forbids pasting reference-authored
text, and help bodies are reference-authored prose. The rule this PRD applies,
and states in each story that needs it, is that **structure is reproduced and
prose is rewritten**. Flag names, metavars, `nargs`, defaults, choices, ordering,
section headings, exit codes and the argparse message templates that come from
CPython's standard library are all observations of a public boundary and are
reproduced exactly. The sentences the reference authors as `help=` strings and as
`MCPCommandError` arguments are rewritten originally, and the difference is
recorded in "Accepted divergences" with the licensing reason. The corpus stores
digests for anything that would otherwise carry a reference sentence into this
repository, exactly as the tool-surface baseline already does.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Row 7 score, backed by a named oracle | 100 | 100 |
| argv cases replayed against the reference | 120 or more | 120 or more |
| Reference argparse actions with a recorded declaration | 34 of 34 | 34 of 34 |
| `vibe mcp` argv shapes whose exit code matches | 7 of 7 | 7 of 7 |
| Row-7 residual with no entry in a divergence table | 0 points | 0 points |

## Target Users

### The contributor changing a flag

- **Role:** anyone adding, renaming or retyping an argument in
  `crates/vibe-cli/src/lib.rs`.
- **Behaviors:** edits the `Arguments` derive, runs `cargo test`, reads
  `docs/parity.md` to decide whether the change is a parity fix or a
  divergence.
- **Pain points:** nothing in the suite compares the derive to the reference's
  parser, so an added flag, a changed metavar or a dropped `nargs` passes
  silently. The nine hidden Rust-only flags reached `main` this way.
- **Current workaround:** open `vibe/cli/entrypoint.py` by hand, count
  `add_argument` calls, and hope the count is the whole comparison.
- **Success looks like:** `cargo test --workspace --all-features` fails with the
  flag name, the JSON pointer and the reference line when the derive drifts.

### The user typing `vibe mcp`

- **Role:** anyone configuring an MCP server from a shell rather than from a
  session.
- **Behaviors:** reads the reference's documentation or its `--help`, types
  `vibe mcp add docs --url https://...`.
- **Pain points:** the command exists, prints a usage line that names only
  `remove`, and exits 1. `vibe mcp --help` does the same. There is no way to
  discover from the binary that `add` was ever intended.
- **Current workaround:** start a session and type `/mcp add <url>`, which is
  what this port's own error message tells them to do, and which is not
  available in a script.
- **Success looks like:** the same argv works against both binaries, including
  the failure cases, and `vibe mcp add --help` explains the twelve options.

### The person reading the scorecard

- **Role:** Arthur, deciding what "parity" currently means and what is left.
- **Behaviors:** reads `docs/parity.md`, reproduces a row's oracle, decides
  whether a score is trustworthy.
- **Pain points:** row 7 scores 92 with no reproducible measurement behind the
  flags half, names one gap where there are six, and carries no entry in either
  divergence table, so nothing fails when the row goes stale.
- **Current workaround:** re-derive the comparison by hand, which is what this
  PRD's audit was.
- **Success looks like:** the row cites a command, the command prints the
  numbers the row quotes, and every difference the command finds is either fixed
  or listed with a reason.

## Research Findings

### Competitive Context

- **argparse (CPython standard library)** is the parser the reference uses. Its
  public behaviors that matter here are `allow_abbrev=True` (prefix inference
  with ambiguity detection), `nargs="?"` with `const`, `action="append"` with a
  `None` default when unused, `RawDescriptionHelpFormatter` (verbatim epilog),
  `add_mutually_exclusive_group`, `parser.error()` rendering usage plus
  `{prog}: error: {message}` on stderr with exit 2, and `-h` auto-added on every
  parser and sub-parser. Its error message templates are CPython's, PSF-licensed
  standard library text, not text the reference authored.
- **clap 4 derive** is the parser this port uses. It reaches every one of those
  behaviors except the error message grammar: `Command::infer_long_args(true)`
  is argparse's `allow_abbrev`, `allow_negative_numbers(true)` covers
  `--max-turns -5`, `value_name` is `metavar`, `after_help` is a verbatim
  epilog, `num_args(0..=1)` with `default_missing_value` is `nargs="?"` with
  `const`, `conflicts_with` is a mutually exclusive group, and `display_order`
  or field order fixes the help ordering. What clap does not offer is argparse's
  sentence templates; it renders `error: unexpected argument '--bogus' found`
  where argparse renders `vibe: error: unrecognized arguments: --bogus`.
- **Market gap:** every drop-in reimplementation of a Python CLI hits the same
  wall, and the two survivable answers are to hand-roll the parser or to
  re-render clap's typed `Error` into the other grammar. This PRD takes the
  second for the 19-flag top-level parser and the first for the small closed
  `vibe mcp` grammar, because the mcp parser is being written from scratch
  anyway and matching exactly costs nothing there.

### Best Practices Applied

- **Introspect the parser, do not scrape its help.** argparse exposes
  `parser._actions` with every attribute the help render is derived from.
  Capturing the action table makes the corpus stable against a help-formatter
  change and makes each divergence nameable by `dest` rather than by a diff
  line. The help render is captured too, at a pinned `COLUMNS`, as a second
  independent assertion.
- **Capture outcomes, not just declarations.** A parser is defined by which argv
  it accepts. The corpus therefore carries a matrix of argv vectors with their
  exit codes, and the declaration table is the explanation of that matrix rather
  than a substitute for it.
- **Normalize by shape, never drop.** The pattern
  `scripts/parity/tool_execution.py` established: a volatile value becomes a
  marker naming its format, and a reference-authored sentence becomes a
  SHA-256 digest with a `<described>` marker naming its length. Nothing that
  would be a licensing problem is stored in cleartext, and nothing is silently
  omitted either.
- **A ledger entry names one pointer and one row.** The existing
  `every_ledger_entry_names_what_closes_it` and
  `every_recorded_divergence_names_a_scorecard_row` tests make a divergence a
  typed, expiring object rather than a comment. Every intentional difference
  this work leaves behind gets one.

## Assumptions & Constraints

### Assumptions (to validate)

- The reference's three parsers can be built without side effects, so the
  capture can introspect them in-process. Evidence: `parse_arguments` and
  `_build_parser` construct and return a parser with no I/O and no config load;
  `run_mcp_cli` calls `_ensure_harness_files_manager()` only *after*
  `parse_args` returns (`vibe/cli/mcp_command.py:47-58`). US-308 asserts this as
  its first criterion.
- Driving `run_mcp_cli` for the error matrix does not touch the user's real
  config, because every failing case raises before `_ensure_harness_files_manager`
  or inside `_parse_add_args`, both before any write. US-309 pins `VIBE_HOME`
  into a temporary directory and asserts the real home is untouched regardless.
- clap's typed `Error` exposes enough context (`ErrorKind` plus `ContextKind`
  values) to re-render the argparse sentence for the kinds the corpus records.
  This is the load-bearing assumption of US-320 and it is written to produce a
  recorded answer either way.
- Short factual status and validation sentences are observations of user-visible
  behavior rather than reference-authored prose, and are reproduced. Evidence:
  `crates/vibe-core/src/config/mcp.rs:715-763` already reproduces two of them
  almost word for word, and `docs/parity.md` measures "user-visible output" by
  name. See `## Open Questions` for the boundary and its default.

### Hard Constraints

- `NOTICE`: no reference source, prompt file or tool description text enters
  this repository. Help bodies and multi-sentence guidance are rewritten
  originally; structure, names and CPython's own message templates are
  reproduced.
- The pin stays at `b78b451c39eab9213393ad2f45908e8562a5c5e7` (v2.24.0). It
  lives in exactly two places and a third copy fails
  `crates/vibe-core/src/parity/parity_tests.rs`.
- A missing or off-pin reference checkout must never fail `cargo test`. Only the
  live probe skips, with a printed reason.
- The reference checkout is read-only. Reads go through `git show b78b451:<path>`
  or `git archive`, never through its working tree.
- Layering: `vibe-cli` is layer 3. Argument parsing stays in `vibe-cli`;
  anything the `vibe mcp` command needs from config or MCP contracts lives in
  `vibe-core` and is called, not duplicated.
- `unsafe_code` is forbidden, and `panic`, `unimplemented` and `dbg_macro` are
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
git -C /home/arthur/dev/mistral-vibe show b78b451:vibe/cli/entrypoint.py
git -C /home/arthur/dev/mistral-vibe archive b78b451 vibe/ | tar -x -C <scratch>
```

**Every line number below is anchored at the pin.** The local checkout may sit
at another revision (it currently sits at v2.24.3), where the same symbol has
moved.

**Every `vibe/...` path in this document resolves against that root**, in the
table below and in each story's `Reference:` line alike.

| Symbol | Reference path | Lines | Stories |
|---|---|---|---|
| `parse_arguments`, parser and epilog | `vibe/cli/entrypoint.py` | 26-40 | US-308, US-318 |
| the 19 optionals and the positional | `vibe/cli/entrypoint.py` | 41-179 | US-308, US-318, US-322 |
| `--teleport`, suppressed | `vibe/cli/entrypoint.py` | 160 | US-318 |
| the continuation group | `vibe/cli/entrypoint.py` | 162-179 | US-308, US-318 |
| `main`, `mcp` intercepted before parsing | `vibe/cli/entrypoint.py` | 258-268 | US-311 |
| `--workdir` resolution and failure | `vibe/cli/entrypoint.py` | 279-289 | US-319 |
| worktree narration, stderr, and its failure on stdout | `vibe/cli/entrypoint.py` | 291-304 | US-319 |
| the deleted working directory guard | `vibe/cli/entrypoint.py` | 306-315 | US-319 |
| `--add-dir` resolution and failure | `vibe/cli/entrypoint.py` | 317-325 | US-319 |
| `init_harness_files_manager("user", "project")` | `vibe/cli/entrypoint.py` | 329 | US-311 |
| `_run_cli_with_worktree_cleanup` | `vibe/cli/entrypoint.py` | 334-365 | US-309 |
| `get_prompt_from_stdin` and the `/dev/tty` reopen | `vibe/cli/cli.py` | 53-67 | US-324 |
| `_session_intent` | `vibe/cli/cli.py` | 112-131 | US-309 |
| `_run_programmatic_mode` and its truthiness gate | `vibe/cli/cli.py` | 132-150 | US-320, US-323 |
| programmatic `SessionOptions` | `vibe/cli/cli.py` | 151-192 | US-321 |
| the four programmatic failure exits | `vibe/cli/cli.py` | 195-206 | US-320 |
| `_run_interactive_mode` | `vibe/cli/cli.py` | 209-272 | US-321, US-324 |
| `run_cli` order, setup before stdin | `vibe/cli/cli.py` | 382-426 | US-324 |
| `run_mcp_cli` | `vibe/cli/mcp_command.py` | 47-70 | US-311, US-312 |
| `_build_parser`, root and sub-parsers | `vibe/cli/mcp_command.py` | 73-80, 153-157 | US-311, US-314 |
| the `add` positional and its 12 options | `vibe/cli/mcp_command.py` | 81-152 | US-314 |
| `_parse_add_args` | `vibe/cli/mcp_command.py` | 160-165 | US-315 |
| `_build_remote_add_command` | `vibe/cli/mcp_command.py` | 166-210 | US-315 |
| `_add_mcp_server` and the OAuth flow | `vibe/cli/mcp_command.py` | 211-250 | US-317 |
| `_remove_mcp_server` | `vibe/cli/mcp_command.py` | 252-259 | US-313 |
| `_add_result_message` | `vibe/cli/mcp_command.py` | 261-265 | US-316 |
| `_build_remote_server` | `vibe/cli/mcp_command.py` | 267-282 | US-315, US-316 |
| `_build_stdio_server` | `vibe/cli/mcp_command.py` | 284-316 | US-315, US-316 |
| `_apply_timeouts` | `vibe/cli/mcp_command.py` | 318-323 | US-315 |
| `_build_static_auth` | `vibe/cli/mcp_command.py` | 325-355 | US-315 |
| `_parse_headers` | `vibe/cli/mcp_command.py` | 357-368 | US-315 |
| `_parse_env` | `vibe/cli/mcp_command.py` | 370-381 | US-315 |
| `_ensure_harness_files_manager`, user scope only | `vibe/cli/mcp_command.py` | 383-390 | US-311, US-313 |
| `persist_oauth_mcp_server` | `vibe/core/config/mcp_servers.py` | 51-92 | US-316, US-317 |
| `persist_remote_mcp_server` | `vibe/core/config/mcp_servers.py` | 93-121 | US-316 |
| `persist_stdio_mcp_server` | `vibe/core/config/mcp_servers.py` | 122-136 | US-316 |
| `remove_mcp_server` | `vibe/core/config/mcp_servers.py` | 151-185 | US-313 |
| `find_persisted_oauth_mcp_server` | `vibe/core/config/mcp_servers.py` | 186-206 | US-313 |
| `normalize_mcp_server_url`, `_parse_mcp_server_url` | `vibe/core/config/mcp_servers.py` | 216-220, 260-287 | US-316 |
| `_remote_servers_equivalent`, `_url_key` | `vibe/core/config/mcp_servers.py` | 326-337 | US-316 |
| `_resolve_new_server_name`, `_suggest_server_name`, `_dedupe_server_name` | `vibe/core/config/mcp_servers.py` | 248-259, 362-390 | US-316 |
| `add_mcp_server` | `vibe/core/tools/mcp/management.py` | 18-35 | US-317 |
| `remove_mcp_server_and_credentials`, credentials first | `vibe/core/tools/mcp/management.py` | 36-49 | US-313 |
| `SessionOptions.headless` | `vibe/app_server/protocol.py` | 230 | US-321 |

Local counterparts, for the same navigation:

| Symbol | Path | Lines | Stories |
|---|---|---|---|
| `Arguments` derive | `crates/vibe-cli/src/lib.rs` | 46-125 | US-310, US-318, US-322, US-323 |
| `validate_arguments` | `crates/vibe-cli/src/lib.rs` | 487-520 | US-320, US-323 |
| `CliError::exit_code` | `crates/vibe-cli/src/lib.rs` | 988-996 | US-320 |
| the `mcp` intercept | `crates/vibe-cli/src/main.rs` | 16-28 | US-311 |
| the preparation failure render | `crates/vibe-cli/src/main.rs` | 41-50 | US-319 |
| the whole `vibe mcp` surface | `crates/vibe-cli/src/mcp_command.rs` | 1-116 | US-311, US-312, US-313 |
| `StartupError::Io` | `crates/vibe-cli/src/tui/startup.rs` | 43-48 | US-319 |
| `populate_piped_prompt`, `apply_piped_prompt` | `crates/vibe-cli/src/tui/startup/invocation.rs` | 121-143 | US-324 |
| `PreparedInvocation::prepare` order | `crates/vibe-cli/src/tui/startup/invocation.rs` | 48-73 | US-324 |
| `LaunchWorkspace::prepare`, `canonical_directory` | `crates/vibe-cli/src/tui/startup/worktree.rs` | 62-105, 290-303 | US-319 |
| `session_options` | `crates/vibe-cli/src/bootstrap.rs` | 131-164 | US-321 |
| `SessionOptions` | `crates/vibe-app-server/src/client.rs` | 139-181 | US-321 |
| `headless`, accepted and unread | `crates/vibe-app-server/src/server/wire.rs` | 88-91 | US-321 |
| `preflight_mcp_add` | `crates/vibe-core/src/config/mcp.rs` | 715-763 | US-316 |
| `persist_mcp_remove` | `crates/vibe-core/src/config/integrations.rs` | 265-304 | US-313 |
| `for_runtime_session_root`, project untrusted | `crates/vibe-app-server/src/workspace.rs` | 478-496 | US-311 |
| the skippable live probe | `crates/vibe-cli/src/tui/runtime_parity_tests.rs` | 46 | US-310 |
| the two ledger tests | `crates/vibe-core/src/parity/ledger_tests.rs` | 1-220 | US-325 |
| the startup corpus and its oracle | `crates/vibe-cli/tests/runtime-parity/` | startup.json, startup-oracle.py | US-309, US-326 |

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

For any story that edits a capture script, add:

```sh
python3 -m compileall -q scripts/parity/ crates/vibe-cli/tests/runtime-parity/
python3 crates/vibe-cli/tests/runtime-parity/cli-surface-oracle.py --check   # re-run must be byte-identical
```

For any story that changes what `--help` prints, add:

```sh
cargo run -p vibe-cli -- --help          # inspected, not diffed against the reference
cargo test -p vibe-cli --all-features completion   # the four shipped completions still generate
```

## Epics & User Stories

### EP-098: The CLI surface oracle

Build the instrument the flags half of row 7 has never had, and prove it
replays without the reference checkout, before changing a single flag.

**Definition of Done:** `crates/vibe-cli/tests/runtime-parity/cli-surface-oracle.py` captures the three
reference parsers' action tables, their three help renders and an argv outcome
matrix into a committed corpus; the corpus replays inside `cargo test
--workspace --all-features` with an audited ledger, a case floor of 120 and an
action floor of 34; the live probe skips with a printed reason when the checkout
is absent or off-pin; a re-run of the capture with no change in between is
byte-identical.

#### US-308: Capture the reference argument grammar
**Description:** As a person reading the scorecard, I want the reference's three argument parsers introspected and their declarations committed, so that row 7's flag surface is compared instead of counted by hand.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/entrypoint.py:26-179` for the root parser, `/home/arthur/dev/mistral-vibe/vibe/cli/mcp_command.py:73-157` for the two sub-parsers. Pattern to copy: `resolve_reference`, `extract_pinned_tree`, `reexecute_with_reference_interpreter`, `stabilize`, `build_corpus` and `--check` in `scripts/parity/tool_surface.py`.

**Acceptance Criteria:**
- [ ] Given the pinned tree, when the capture runs, then it re-executes itself under the reference interpreter and refuses to run against a checkout at any commit other than `EXPECTED_COMMIT` imported from `scripts/parity/pin.py`.
- [ ] Given `parse_arguments` and `_build_parser`, when the capture builds them, then it asserts before the first record that neither wrote a file, read `$VIBE_HOME`, or loaded a configuration, and fails the run naming the side effect if one occurred.
- [ ] Given each of the three parsers, when its actions are recorded, then every action carries `option_strings`, `dest`, `nargs`, `const`, `default`, `choices`, `metavar`, `required`, `type` by name, the action class name, and whether its help is suppressed.
- [ ] Given the mutually exclusive continuation group, when it is recorded, then the record names the two `dest` values it contains and marks the group not required.
- [ ] Given each parser, when its help is recorded, then `format_help()` is captured at `COLUMNS=80` with `TERM=dumb`, `NO_COLOR=1` and `FORCE_COLOR` unset, and the render is stored as a line-by-line structure with each `help=` body replaced by a SHA-256 digest carrying a `<described>` marker naming its character length.
- [ ] Given the epilog, when it is recorded, then its block headings and the bare names it lists (the command names and the environment variable names) are stored in cleartext and its sentences are stored as digests.
- [ ] Given the reference checkout is absent, when the capture runs, then it exits non-zero naming the expected path and the `VIBE_REFERENCE` override, and writes no partial corpus.
- [ ] Given the capture is run twice with no change in between, when the two corpora are compared, then they are byte-identical, verified by `--check`.
- [ ] Given the corpus is written, then it carries `schemaVersion`, and a `reference` block with `commit`, `version` and `sourceFiles`, matching the shape `crates/vibe-cli/tests/runtime-parity/startup.json` already uses.

#### US-309: Capture the argv outcome matrix
**Description:** As a contributor, I want a matrix of argv vectors driven through the reference's parsers with their exit codes recorded, so that "which argv is accepted" is a measurement rather than a reading of the declarations.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** US-308
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/mcp_command.py:47-70` for the `run_mcp_cli` entry, `/home/arthur/dev/mistral-vibe/vibe/cli/entrypoint.py:26-179` for the root parser. argparse's `parser.error` renders usage plus `{prog}: error: {message}` on stderr and raises `SystemExit(2)`.

**Acceptance Criteria:**
- [ ] Given an argv vector, when it is driven, then the record carries the vector, the exit code, whether each stream received output, and the last line of stderr with any reference-authored fragment digested and any CPython-authored template kept in cleartext.
- [ ] Given a vector that parses, when it is driven, then the record also carries the resulting namespace as a sorted `dest` to value mapping, so an accepted vector is compared by what it produced and not only by the fact that it parsed.
- [ ] Given the capture drives `run_mcp_cli`, when it runs, then `VIBE_HOME` points inside its own temporary directory and the capture asserts the user's real config file was neither read nor written before the first case.
- [ ] Given the matrix, when the capture runs, then it covers at minimum: each of the 19 optionals passed alone with a valid value; each optional passed with no value; `--output` with each of its three choices and with one invalid; `-c` with `--resume`; `--resume` bare and with a value; a repeated `--enabled-tools`, `--disabled-tools` and `--add-dir`; an unknown flag; an unambiguous long-flag prefix; an ambiguous long-flag prefix; a negative value for each of `--max-turns`, `--max-tokens` and `--max-price`; a non-numeric value for each; `-p` with an empty string; `-p` with whitespace only; `--help`; `-h`; `-v`; `--version`; and a bare invocation.
- [ ] Given the `vibe mcp` matrix, when the capture runs, then it covers at minimum: bare `mcp`; `mcp -h`; `mcp --help`; `mcp add -h`; `mcp remove -h`; an unknown sub-command; `remove` with no name; `remove` with two names; `add` with no name; `add` with an invalid `--transport`; `add` with `--url` and no value; and one accepted vector per transport.
- [ ] Given a vector whose outcome depends on a real network or keyring, when the matrix is built, then it is listed in an `unavailable` block with a reason rather than driven, and the block's entries each carry a non-empty id and reason.
- [ ] Given the capture is run twice with no change in between, when the two corpora are compared, then they are byte-identical.
- [ ] Given the matrix contains at least 120 vectors, when the capture finishes, then it prints the count and fails below it.

#### US-310: Replay the CLI surface corpus with a ledger and a floor
**Description:** As a contributor, I want the committed CLI corpus replayed against this port's own parser on every test run, so that a flag divergence fails `cargo test` instead of aging into a wrong score.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** US-308, US-309
**Reference:** the corpus written by US-308 and US-309. Pattern to copy: the strict `deny_unknown_fields` corpus structs and the story-id assertion in `crates/vibe-cli/src/tui/startup/invocation.rs:157-249`, the skippable live probe in `crates/vibe-cli/src/tui/runtime_parity_tests.rs:46`, and the `Divergence` struct with `tool`, `case`, `pointer`, `closed_by`, `row` and `why` plus `every_ledger_entry_names_what_closes_it` in `crates/vibe-app-server/src/tool_execution_parity_tests.rs`.

**Acceptance Criteria:**
- [ ] Given the committed corpus, when `cargo test --workspace --all-features` runs, then the replay executes unconditionally and does not depend on the reference checkout being present.
- [ ] Given the reference checkout is absent or off-pin, when the replay runs, then only the live probe skips, with a printed reason naming both commits and the restore command from `vibe_core::parity::RESTORE_COMMAND`.
- [ ] Given a recorded action, when it is replayed, then the comparison covers the option strings, the value count, the default, the accepted values where the reference declares `choices`, the value name and whether the argument is hidden, resolved from `clap::CommandFactory` on `crate::Arguments` rather than from a second hand-written table.
- [ ] Given an argv vector, when it is replayed, then the comparison covers the exit code, which streams received output, and for an accepted vector the resulting field values.
- [ ] Given a difference no ledger entry names, when the replay runs, then it fails and prints the parser, the case and the JSON pointer, with `unlisted` asserted empty.
- [ ] Given a ledger entry, when the suite runs, then it names exactly one JSON pointer on one named case with no wildcard, and carries a `row` naming an existing row of `docs/parity.md` and a `why`.
- [ ] Given a ledger entry whose difference no longer reproduces, when the replay runs, then it fails naming the stale entry.
- [ ] Given the corpus, when the replay runs, then it fails below 120 argv cases, below 34 recorded actions, or below 3 parsers.
- [ ] Given the replay passes, when it finishes, then it prints one line carrying the matched count, the total, the pinned commit, the action count and the number of ledger entries exercised, and every number row 7 later quotes is read off that line.
- [ ] Given the corpus schema changes without a version bump, when the replay runs, then it fails naming the expected and found versions.
- [ ] Given the corpus is audited for cleartext, when the audit runs, then no reference-authored sentence is present and every digested field carries its `<described>` marker.

---

### EP-099: The `vibe mcp` command shape

Replace the 116-line single-shape intercept with the sub-command parser the
reference builds, its three help surfaces and its exit-2 error grammar, and
bring `remove` to its full contract.

**Definition of Done:** all seven diverging `vibe mcp` argv shapes from the
problem statement match the reference's exit code; `vibe mcp`, `vibe mcp add
-h` and `vibe mcp remove -h` each print help on stdout and exit 0; every
argument failure prints usage plus a `{prog}: error: {message}` line on stderr
and exits 2; `remove` deletes OAuth credentials before the config entry.

#### US-311: Route `vibe mcp` through a real sub-command parser
**Description:** As a user typing `vibe mcp`, I want the command to expose its sub-commands and their help, so that the binary is discoverable the way the reference's is.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/mcp_command.py:47-58` for the dispatch and the bare-invocation help, `:73-80` and `:153-157` for the three parsers, `:383-390` for the user-scope harness init. `/home/arthur/dev/mistral-vibe/vibe/cli/entrypoint.py:258-268` for the intercept that runs before `parse_arguments`. Local: `crates/vibe-cli/src/mcp_command.rs:1-116` and `crates/vibe-cli/src/main.rs:16-28`.

**Acceptance Criteria:**
- [ ] Given `vibe mcp` with no sub-command, when it runs, then it prints the root help on stdout and exits 0.
- [ ] Given `vibe mcp -h` or `vibe mcp --help`, when it runs, then it prints the same root help on stdout and exits 0.
- [ ] Given `vibe mcp add -h` or `vibe mcp remove -h`, when it runs, then it prints that sub-command's help on stdout and exits 0.
- [ ] Given the root help, when it is rendered, then its usage line names both sub-commands in declaration order, its positional block lists them with a one-line description each, and its options block lists the help flag, matching the structure recorded by US-308 while the descriptive sentences are written originally for this repository.
- [ ] Given the intercept in `main`, when argv begins with `mcp`, then the top-level parser never runs, so `mcp` is never read as a positional prompt, and the intercept is still the first thing after `mark_process_start`.
- [ ] Given any `vibe mcp` invocation that reaches persistence, when it resolves its configuration target, then it writes the user configuration and not the project configuration, matching the reference's user-only harness scope.
- [ ] Given the corpus from EP-098, when the replay runs, then every `vibe mcp` help case matches on exit code and on which stream received the output.
- [ ] Given a sub-command name the parser does not declare, when it runs, then it does not print help and does not exit 0.

#### US-312: Report argument failures the way the reference reports them
**Description:** As a user mistyping a `vibe mcp` argument, I want the failure to name the sub-command, the argument and the reason and to exit 2, so that a script can tell a usage error from a runtime failure.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** US-311
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/mcp_command.py:64-69` for the `parser.error(str(exc))` funnel. argparse's own templates: `argument {dest}: invalid choice: {value!r} (choose from {choices})`, `the following arguments are required: {names}`, `unrecognized arguments: {extras}`, `argument {flag}: expected one argument`.

**Acceptance Criteria:**
- [ ] Given an unknown sub-command, when it runs, then stderr carries the root usage line followed by `vibe mcp: error: argument mcp_command: invalid choice: 'list' (choose from 'add', 'remove')` and the exit code is 2.
- [ ] Given `remove` with no name, when it runs, then stderr carries the `remove` usage line followed by `vibe mcp remove: error: the following arguments are required: NAME` and the exit code is 2.
- [ ] Given `add` with an invalid `--transport`, when it runs, then stderr carries the `add` usage line followed by `vibe mcp add: error: argument --transport: invalid choice: 'bogus' (choose from 'http', 'streamable-http', 'stdio')` and the exit code is 2.
- [ ] Given `--url` with no value, when it runs, then stderr carries `vibe mcp add: error: argument --url: expected one argument` and the exit code is 2.
- [ ] Given extra positional arguments after a complete sub-command, when it runs, then the error names the *root* prog, not the sub-command, and reads `vibe mcp: error: unrecognized arguments: b`, matching where argparse raises it.
- [ ] Given a validation failure raised after parsing (any `MCPCommandError`, add failure, remove failure or concurrency conflict), when it runs, then it is reported through the same funnel: usage line, `{prog}: error: {message}`, exit 2, never a bare message on exit 1.
- [ ] Given any of these failures, when it runs, then nothing is written to the user configuration, asserted by comparing the file's bytes before and after.
- [ ] Given the corpus from EP-098, when the replay runs, then every `vibe mcp` failure case matches on exit code and on the stderr last line, with any divergence in a reference-authored sentence carrying a ledger entry rather than failing silently.

#### US-313: Bring `vibe mcp remove` to its reference contract
**Description:** As a user removing an OAuth MCP server, I want its stored credentials deleted with it, so that removing a server does not leave a live token in the keyring.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** US-311
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/tools/mcp/management.py:36-49` for the ordering and its stated reason, `/home/arthur/dev/mistral-vibe/vibe/core/config/mcp_servers.py:151-185` for the removal, `:186-206` for the OAuth lookup, `/home/arthur/dev/mistral-vibe/vibe/cli/mcp_command.py:252-259` for the two outcome messages. Local: `crates/vibe-core/src/config/integrations.rs:265-304`.

**Acceptance Criteria:**
- [ ] Given a configured OAuth server, when it is removed, then its credentials are deleted before the configuration entry, in that order, and the order is asserted by a test that fails the credential deletion and observes the configuration entry still present.
- [ ] Given credential deletion fails, when the removal runs, then the configuration is unchanged, the failure names the server, and the exit code is the one US-312 defines for a remove failure.
- [ ] Given a configured server with no stored credentials, when it is removed, then the removal succeeds and no credential lookup failure is reported.
- [ ] Given a server that is not configured, when it is removed, then the message states that it is not configured in the user configuration and the exit code is 0.
- [ ] Given a server that is configured, when it is removed, then the message states that it was removed and names it, and the exit code is 0.
- [ ] Given both messages, when they are rendered, then their sentence terminators match the reference's, which the current implementation drops.
- [ ] Given a name that normalizes to empty, when it is removed, then the failure is reported through the US-312 funnel rather than as a bare error string.
- [ ] Given a project configuration that also declares the same server, when `vibe mcp remove` runs, then the project file is not modified.

---

### EP-100: `vibe mcp add`

Implement the sub-command row 7 names as its only gap: one positional, twelve
options, eleven validation rules, five persistence outcomes and an OAuth login.

**Definition of Done:** every argv shape the reference's `add` accepts is
accepted here with the same effect on the user configuration; every shape it
rejects is rejected with the same exit code; the OAuth path opens a browser,
narrates the URL, and persists before login the way the reference does.

#### US-314: Declare the add flag surface
**Description:** As a user scripting `vibe mcp add`, I want the twelve options and their value shapes accepted exactly as the reference accepts them, so that a command that works there works here.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** US-311
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/mcp_command.py:81-152` for the positional and the twelve options, including the `--api-key-env` / `--bearer-token-env-var` pair sharing one destination.

**Acceptance Criteria:**
- [ ] Given the `add` parser, when its declarations are compared to the corpus, then it carries one required positional named `NAME` and twelve options across thirteen spellings, with no option the reference does not declare and none missing.
- [ ] Given `--transport`, when it is omitted, then the value is the reference's default, and when it is supplied, then only the reference's three choices are accepted.
- [ ] Given `--arg`, `--env` and `--header`, when each is repeated, then every occurrence is retained in order rather than the last one winning.
- [ ] Given `--header`, when it is omitted entirely, then the parsed value is an empty collection and not a null, matching the reference's explicit default.
- [ ] Given `--bearer-token-env-var`, when it is used, then it sets the same destination `--api-key-env` sets, and supplying both is resolved the way the corpus records.
- [ ] Given `--no-login`, when it is present, then it takes no value, and when absent, then its value is false.
- [ ] Given the two timeout options, when each is supplied a non-numeric value, then the failure is reported through the US-312 funnel with exit 2.
- [ ] Given `add --help`, when it runs, then every option is listed with the reference's value name, and each description is written originally for this repository.
- [ ] Given the corpus, when the replay runs, then each of the thirteen spellings is exercised by at least one accepted vector.

#### US-315: Validate the add arguments
**Description:** As a user combining incompatible `vibe mcp add` flags, I want the command to refuse before it writes anything, so that a malformed server never reaches the configuration file.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** US-314
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/mcp_command.py:160-165` for the transport split, `:166-210` for the remote rules, `:284-316` for the stdio rules, `:318-323` for the timeouts, `:325-355` for the static auth rules, `:357-368` and `:370-381` for the header and environment parsing.

**Acceptance Criteria:**
- [ ] Given a remote transport with any stdio-only flag, when it runs, then it is refused naming every offending flag in the reference's order, and nothing is written.
- [ ] Given a remote transport with no `--url`, when it runs, then it is refused naming both remote transports.
- [ ] Given `--transport stdio` with any remote-only flag, when it runs, then it is refused naming every offending flag.
- [ ] Given `--transport stdio` with no `--command`, when it runs, then it is refused.
- [ ] Given a `--header` value with no `=`, when it runs, then it is refused naming the required shape, and the same rule applies to `--env`.
- [ ] Given two `--header` values whose names differ only by case, when it runs, then it is refused as a duplicate, and given two `--env` values with the same name, then it is refused as a duplicate.
- [ ] Given `--api-key-header` or `--api-key-format` with no `--api-key-env`, when it runs, then each is refused by its own message.
- [ ] Given a `--header` whose name collides with the resolved API key header, when it runs, then it is refused.
- [ ] Given `--no-login` together with any static authentication option, when it runs, then it is refused because OAuth and static authentication cannot be combined.
- [ ] Given a URL that is not `https`, has a fragment, carries credentials, or omits a scheme or host, when it runs, then it is refused, and a loopback host is accepted over `http`.
- [ ] Given a name that contains no letter or digit, when it runs, then it is refused.
- [ ] Given any of these refusals, when it runs, then the exit code is 2, the message reaches stderr through the US-312 funnel, the user configuration bytes are unchanged, and the sentence is written originally for this repository with its ledger entry recorded once for the class.

#### US-316: Persist the server with the reference's outcomes
**Description:** As a user re-running the same `vibe mcp add`, I want the second run to be a no-op that says so, so that the command is safe in a provisioning script.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** US-315
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/core/config/mcp_servers.py:93-121` for the remote outcomes, `:122-136` for the stdio outcomes, `:216-220` and `:260-287` for URL normalization, `:326-337` for equivalence and the URL key, `:248-259` and `:362-390` for name suggestion and de-duplication, `/home/arthur/dev/mistral-vibe/vibe/cli/mcp_command.py:261-265` for the two result messages. Local: `crates/vibe-core/src/config/mcp.rs:715-763`, which currently rejects every collision and has no equivalence path.

**Acceptance Criteria:**
- [ ] Given a name and options identical to an existing remote entry, when `add` runs, then nothing is written, the message reports the server already configured, and the exit code is 0.
- [ ] Given a name matching an existing remote entry whose URL matches but whose other options differ, when `add` runs, then it is refused because the same name is configured with different options.
- [ ] Given a name matching an existing entry of a different kind, when `add` runs, then it is refused as a name collision.
- [ ] Given a URL matching an existing entry under a different name, when `add` runs, then it is refused naming the existing server.
- [ ] Given a URL matching an existing entry that uses static authentication, when `add` runs, then the refusal states that only OAuth servers are supported by this path.
- [ ] Given a name and command identical to an existing stdio entry, when `add` runs, then nothing is written and the message reports it already configured.
- [ ] Given a name matching an existing stdio entry with a different command, when `add` runs, then it is refused as a name collision.
- [ ] Given a URL whose host case, default port or trailing slash differs from an existing entry, when the comparison runs, then it normalizes to the same key and the collision is detected.
- [ ] Given a new server, when it is persisted, then the entry is appended to the user configuration, the message reports it added and names it, and the exit code is 0.
- [ ] Given a write that loses its compare-and-swap, when `add` runs, then it is reported through the US-312 funnel and the file is left as the winning writer left it.
- [ ] Given the two result messages, when they are rendered, then their sentence terminators match the reference's.

#### US-317: Run the OAuth login the add path performs
**Description:** As a user adding an OAuth MCP server, I want the browser opened and the URL printed, so that the login completes without me reading a log file.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** US-316
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/mcp_command.py:211-250` for the flow, its callbacks and its failure paths, `/home/arthur/dev/mistral-vibe/vibe/core/tools/mcp/management.py:18-35` for `add_mcp_server`, `/home/arthur/dev/mistral-vibe/vibe/core/config/mcp_servers.py:51-92` for the persist-then-login ordering.

**Acceptance Criteria:**
- [ ] Given an OAuth server without `--no-login`, when `add` runs, then the entry is persisted before the login begins, and the persistence message is printed at that moment rather than at the end.
- [ ] Given the login needs a browser, when it starts, then the URL is printed on stdout with the reference's blank-line-and-indent layout, and the browser is opened.
- [ ] Given the browser cannot be opened, when it is attempted, then the failure is reported on stderr and the flow continues, because the URL is already printed.
- [ ] Given the login completes, when it finishes, then a completion line is printed and the exit code is 0.
- [ ] Given the login fails, when it finishes, then the failure is reported on stderr, the message tells the user how to authenticate from a session, the entry stays persisted, and the exit code is 1.
- [ ] Given `--no-login` on an OAuth server, when `add` runs, then the entry is persisted, no browser is opened, and the message tells the user how to authenticate later.
- [ ] Given no reachable OAuth endpoint in the test environment, when the suite runs, then the flow is exercised against an in-process fake and the case is recorded in the corpus's `unavailable` block with a reason rather than skipped silently.
- [ ] Given the flow is interrupted before the login returns, when the process exits, then the persisted entry remains and no partial credential is stored.

---

### EP-101: The top-level surface

Give the 19 reference flags the reference's help structure, give the failures
the reference's exit codes and destinations, and close the one programmatic
option that `cli.py` sets and this port does not.

**Definition of Done:** `vibe --help` renders the reference's declaration order,
value names and epilog structure; a bad `--workdir`, a bad `--add-dir` and a
deleted working directory each produce their own message and exit 1; every
post-parse validation failure exits 1; the programmatic session carries the
reference's headless flag and its two forced tool exclusions.

#### US-318: Render the top-level help with the reference's structure
**Description:** As a user reading `vibe --help`, I want each flag to show what kind of value it takes and the flags to appear in the reference's order, so that the two binaries' help can be read interchangeably.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** US-310
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/entrypoint.py:26-40` for the parser and its epilog, `:41-179` for the declaration order and every `metavar`, `:160` for the suppressed flag. Local: `crates/vibe-cli/src/lib.rs:46-125`.

**Acceptance Criteria:**
- [ ] Given the help render, when it lists an option the reference gives a `metavar`, then this port shows the same value name, covering `PROMPT`, `TEXT`, `N`, `DOLLARS`, `TOOL`, `NAME`, `DIR` and `SESSION_ID`.
- [ ] Given the help render, when the options are listed, then their order matches the reference's declaration order, with the positional in its declared place.
- [ ] Given `--teleport`, when the help renders, then it is hidden, as it is upstream.
- [ ] Given each of the nine flags this port declares that the reference does not, when the help renders, then it stays hidden, and each is named once in the ledger entry US-325 records for the class.
- [ ] Given the epilog, when the help renders, then it carries the reference's two block headings and lists the same command names and the same environment variable names in the same order, with the surrounding sentences written originally for this repository.
- [ ] Given every option, when the help renders, then it carries a non-empty description written originally for this repository, so that no option shows an empty body as they all do today.
- [ ] Given `--output`, when the help renders, then it lists the reference's three accepted values, and the `ndjson` alias is not advertised.
- [ ] Given the four shipped completion files, when the help structure changes, then they regenerate and their parity test still passes.
- [ ] Given the corpus from EP-098, when the replay runs, then the recorded value names, hidden flags, defaults and accepted values match, and the help *prose* is compared by structure only, with its divergence carrying one ledger entry for the class.

#### US-319: Report the working-directory failures the reference reports
**Description:** As a user passing a path that is not there, I want to be told which flag was wrong and what was wrong with it, so that I can fix the invocation without reading a system error code.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/entrypoint.py:279-289` for `--workdir`, which prints the *resolved* path, `:306-315` for the deleted working directory guard and its two lines, `:317-325` for `--add-dir`, which prints the *raw* argument, `:291-304` for the worktree narration on stderr and its failure on stdout. Local: `crates/vibe-cli/src/tui/startup/worktree.rs:62-105`, `:290-303`, and `crates/vibe-cli/src/tui/startup.rs:43-48`.

**Acceptance Criteria:**
- [ ] Given `--workdir` naming a path that does not exist or is not a directory, when the launch runs, then the message names the flag and the resolved path, and the exit code is 1.
- [ ] Given `--add-dir` naming a path that does not exist or is not a directory, when the launch runs, then the message names the flag and the argument exactly as typed rather than resolved, and the exit code is 1.
- [ ] Given several `--add-dir` values where the second is bad, when the launch runs, then it fails on the second and names it, so the loop reports the first failure rather than an aggregate.
- [ ] Given a working directory that was deleted while the shell still points at it, when the launch runs, then it prints the reference's two-line explanation, the second line offering `--workdir` as the way out, and exits 1, which this port currently does not do at all.
- [ ] Given `--workdir` naming a path with a leading tilde, when it resolves, then it expands before the existence check, as it does today.
- [ ] Given a worktree preparation failure, when it is reported, then the message goes to stdout while the two narration lines go to stderr, matching where the reference sends each.
- [ ] Given each of these failures, when it is reported, then no generic I/O sentence naming an operating system error number is printed.
- [ ] Given the corpus from EP-098, when the replay runs, then each of these four failures matches on exit code and on which stream received the output.

#### US-320: Exit and report the way the reference exits
**Description:** As a script calling `vibe`, I want a usage error and a runtime error to be distinguishable by exit code, so that a wrapper can retry one and not the other.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** US-310
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/cli.py:147-150` for the missing-prompt exit, `:195-206` for the four programmatic failure exits, all 1. argparse exits 2 for a parse failure and 0 for help and version. Local: `crates/vibe-cli/src/lib.rs:988-996`, where `InvalidArguments` maps to 2.

**Acceptance Criteria:**
- [ ] Given a post-parse validation failure such as a missing programmatic prompt, when it is reported, then the exit code is 1 and not 2.
- [ ] Given a parse failure, when it is reported, then the exit code stays 2, matching argparse.
- [ ] Given `--help` or `--version`, when it runs, then the exit code is 0 and the output goes to stdout.
- [ ] Given the missing programmatic prompt, when it is reported, then the message is prefixed the way the reference prefixes it rather than with this port's internal error-kind name.
- [ ] Given a clap parse failure of a kind the corpus records, when it is rendered, then it is re-rendered as a usage line followed by `vibe: error: {message}` on stderr, covering at minimum an unknown flag, an invalid choice, a missing value, a conflicting pair and a bad value type.
- [ ] Given a clap error kind the re-render does not cover, when it occurs, then clap's own render is used unchanged and the fallback is exercised by a test, so an unrecognized kind degrades rather than panicking.
- [ ] Given the re-render cannot recover the argument name from clap's error context for some kind, when that is discovered, then the kind is recorded as a ledger entry naming the constraint rather than the story stalling.
- [ ] Given the corpus from EP-098, when the replay runs, then every recorded argv case matches on exit code, with any remaining difference in an argparse-versus-clap sentence carrying a ledger entry.

#### US-321: Carry the programmatic session options the reference sets
**Description:** As a user running `vibe -p`, I want the session told that no human is available and the two interactive tools withheld, so that a headless run does not stall on a question nobody can answer.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** None
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/cli.py:151-192` for the programmatic options, including `headless=True` and the two names appended to `disabled_tools`, `:209-272` for the interactive path that sets neither, `/home/arthur/dev/mistral-vibe/vibe/app_server/protocol.py:230` for the field's default. Local: `crates/vibe-cli/src/bootstrap.rs:131-164`, `crates/vibe-app-server/src/client.rs:139-181`, `crates/vibe-app-server/src/server/wire.rs:88-91`, and `crates/vibe-acp/src/session/settings.rs:198`, which is the only place this port already does it.

**Acceptance Criteria:**
- [ ] Given a programmatic launch, when its session options are built, then the headless flag is set and reaches the server, where it is currently accepted and unread.
- [ ] Given a programmatic launch, when its disabled tools are built, then the two interactive tool names are appended to whatever the user passed, without discarding the user's own values and without duplicating a name the user already passed.
- [ ] Given an interactive launch, when its session options are built, then neither the headless flag nor the two appended names is set.
- [ ] Given the headless flag reaches the prompt composition, when the prompt is composed, then the headless section is present, which is the behavior `crates/vibe-core/src/prompt.rs:41-80` already implements for a caller that sets it.
- [ ] Given this change also moves row 13, when the work lands, then `docs/parity.md` records that in the same commit rather than leaving row 13 stale.
- [ ] Given a programmatic launch that passes one of the two names in `--enabled-tools`, when the session starts, then the outcome matches what the corpus records for the reference rather than being resolved by guess.

---

### EP-102: Parse semantics

Close the three places where the two parsers accept different argv, and the one
place where reading stdin costs the session its keyboard.

**Definition of Done:** an unambiguous long-flag prefix is accepted and an
ambiguous one is refused; a negative numeric value is accepted; a
whitespace-only programmatic prompt behaves as it does upstream; a piped prompt
leaves an interactive session with a usable terminal.

#### US-322: Accept unambiguous flag prefixes and refuse ambiguous ones
**Description:** As a user typing `vibe --max-tur 3`, I want the abbreviation accepted the way argparse accepts it, so that a habit formed against the reference does not fail here.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** US-310
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/entrypoint.py:27` builds the parser without disabling `allow_abbrev`, so prefix inference is on. clap's counterpart is `Command::infer_long_args`.

**Acceptance Criteria:**
- [ ] Given an unambiguous long-flag prefix, when it is parsed, then it resolves to the full flag, covering at minimum `--max-tur`, `--work` and `--disabled`.
- [ ] Given a prefix that matches more than one declared flag, when it is parsed, then it is refused rather than resolving to the first match.
- [ ] Given a prefix that would be unambiguous upstream but is ambiguous here because of the nine extra hidden flags, when it is parsed, then the outcome is recorded as a ledger entry naming the prefix and the extra flag rather than left as an unlisted difference.
- [ ] Given a full flag name that is also a prefix of a longer one, when it is parsed, then the exact match wins.
- [ ] Given a short flag, when it is parsed, then nothing about it changes.
- [ ] Given the corpus from EP-098, when the replay runs, then both the unambiguous and the ambiguous prefix cases match on exit code.

#### US-323: Accept the argument values argparse accepts
**Description:** As a user passing a negative limit or a whitespace prompt, I want the same acceptance the reference gives, so that identical argv does not diverge on a value.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** US-310
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/entrypoint.py:60-73` for the three numeric options, all plain `type=int` or `type=float` with no bound; `/home/arthur/dev/mistral-vibe/vibe/cli/cli.py:147-150` for the truthiness gate on the programmatic prompt. Local: `crates/vibe-cli/src/lib.rs:487-520`.

**Acceptance Criteria:**
- [ ] Given a negative value for `--max-turns`, `--max-tokens` or `--max-price`, when it is parsed, then it is accepted at the parse boundary the way `type=int` and `type=float` accept it.
- [ ] Given a value the reference's parser accepts but this port's downstream cannot represent, when it is parsed, then the refusal happens after parsing with exit 1, not at the parse boundary with exit 2, and it is recorded as a ledger entry naming the type.
- [ ] Given a non-numeric value for any of the three, when it is parsed, then it is refused with exit 2 on both sides.
- [ ] Given a whitespace-only programmatic prompt, when it runs, then the behavior matches what the corpus records for the reference, and if it diverges by design then the divergence carries a ledger entry rather than a silent trim.
- [ ] Given an empty programmatic prompt with nothing on stdin, when it runs, then it is refused with exit 1, matching the reference.
- [ ] Given the Rust-only finite-and-non-negative validation on the price options, when this story lands, then it either goes away or is recorded as an accepted divergence with its reason.
- [ ] Given the corpus from EP-098, when the replay runs, then every numeric and empty-value case matches on exit code.

#### US-324: Read piped stdin the way the reference reads it
**Description:** As a user running `echo prompt | vibe`, I want the session to keep a usable keyboard, so that a piped prompt starts an interactive session instead of one that cannot be typed into.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/cli.py:53-67` for the drain and the `/dev/tty` reopen inside a suppressed `OSError`, `:382-402` for the order in which `run_cli` reaches stdin, after onboarding and after the upgrade check. Local: `crates/vibe-cli/src/tui/startup/invocation.rs:48-73` and `:121-143`.

**Acceptance Criteria:**
- [ ] Given a piped prompt and an interactive route, when the session mounts, then the process's standard input is reattached to the controlling terminal, so the session accepts typed input.
- [ ] Given no controlling terminal to reattach to, when the reattach is attempted, then the failure is swallowed and the launch continues, matching the reference's suppressed error.
- [ ] Given a programmatic route, when the pipe is drained, then no reattach is attempted, because the run never reads the keyboard.
- [ ] Given `--setup`, when it runs with something on stdin, then onboarding runs and the pipe is never drained, matching the order `run_cli` uses and unlike this port, which drains during preparation.
- [ ] Given `--check-upgrade`, when it runs with something on stdin, then the same holds.
- [ ] Given stdin is a terminal, when the launch runs, then nothing is read from it during preparation.
- [ ] Given an empty or whitespace-only pipe, when it is drained, then no prompt is set and the existing precedence between the positional prompt and the piped one is unchanged.
- [ ] Given the reattach is not possible on a platform without `/dev/tty`, when the launch runs, then the platform difference is recorded as an accepted divergence rather than a silent no-op.

---

### EP-103: Restate row 7 from its oracle

Turn the audit into ledger entries a test can read, and rewrite the row from
what the instrument prints rather than from what a reader believes.

**Definition of Done:** every intentional difference this work leaves behind has
an entry in `docs/parity.md` naming a scorecard row and a reason; row 7 cites
the command that reproduces its score and quotes numbers printed by that
command; both ledger tests pass.

#### US-325: Record the CLI surface divergences in the tables that hold them
**Description:** As a person reading the scorecard, I want every deliberate difference in the CLI surface listed where a test checks it, so that a difference cannot age into an unnoticed regression.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** US-310
**Reference:** local. `docs/parity.md` "Open divergences" and "Accepted divergences", and `crates/vibe-core/src/parity/ledger_tests.rs`, whose `every_recorded_divergence_names_a_scorecard_row` and `every_accepted_divergence_still_names_evidence_that_exists` both currently pass with zero row-7 entries.

**Acceptance Criteria:**
- [ ] Given the nine hidden Rust-only flags, when they are recorded, then one accepted-divergence entry names all nine, names row 7, cites `crates/vibe-cli/src/lib.rs`, and states why they exist.
- [ ] Given the `ndjson` alias on `--output`, when it is recorded, then it has its own entry naming row 7 and row 13.
- [ ] Given the four shipped shell completion files the reference does not ship, when they are recorded, then one entry names them and cites the existing `completion_parity_tests` header, which already states there is no upstream oracle.
- [ ] Given the help prose and the validation sentences written originally under `NOTICE`, when they are recorded, then one entry covers the class, names the licensing reason, and cites the corpus field that stores digests instead of text.
- [ ] Given clap's parse-error grammar where it still differs from argparse after US-320, when it is recorded, then the entry names the error kinds that remain and the reason.
- [ ] Given every entry, when the suite runs, then `every_recorded_divergence_names_a_scorecard_row` passes and each cited path exists.
- [ ] Given a divergence this work closes, when it is closed, then no stale entry describing it remains.
- [ ] Given an entry describing something still open rather than accepted, when it is written, then it goes in the open table and names what would close it.

#### US-326: Remeasure and restate row 7
**Description:** As Arthur reading the scorecard, I want row 7 to cite a command whose output produces its numbers, so that the score survives the next change to the parser.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** US-311, US-312, US-313, US-314, US-315, US-316, US-317, US-318, US-319, US-320, US-321, US-322, US-323, US-324, US-325
**Reference:** local. `docs/parity.md` row 7 and its "Method" section, which promises a reproducible oracle per part.

**Acceptance Criteria:**
- [ ] Given the full CI sequence run unfiltered from the workspace root, when it passes, then row 7 is rewritten, and it is not rewritten from a filtered run.
- [ ] Given the row's residual sentence, when it is rewritten, then it names the oracle command, the case count and the action count printed by the replay, and it no longer claims `vibe mcp add` as the only gap.
- [ ] Given the row's score, when it is set to 100, then the row states what would make that score wrong, so a future reader can falsify it.
- [ ] Given rows 11 and 13, which this work touches through `vibe mcp add` and through the programmatic session options, when row 7 is restated, then both are re-read and either restated from the same evidence or explicitly left where they are with a reason.
- [ ] Given the "Measured volumes" section, when the row is restated, then the new capture script and its corpus are counted there.
- [ ] Given the version drift section, which is measured against a commit one release behind the local checkout, when this work lands, then the row does not depend on it and the staleness is noted rather than silently inherited.
- [ ] Given the restatement, when it is written, then it quotes no number that the replay does not print.
- [ ] Given `CHANGELOG.md`, when this work lands, then the user-visible changes are recorded under `## Unreleased`: the new `vibe mcp add`, the changed `vibe mcp` exit codes, the changed help output and the changed startup error messages.

## Functional Requirements

- FR-01: The system must capture the reference's three argument parsers'
  declarations, help renders and argv outcomes into a committed corpus.
- FR-02: The system must replay that corpus against this port's own parser on
  every `cargo test --workspace --all-features` run, without requiring the
  reference checkout.
- FR-03: The system must skip only the live probe, with a printed reason, when
  the reference checkout is absent or off-pin.
- FR-04: The system must fail the test suite when a recorded difference has no
  ledger entry, and when a ledger entry no longer reproduces.
- FR-05: `vibe mcp` with no sub-command must print help on stdout and exit 0.
- FR-06: `vibe mcp`, `vibe mcp add` and `vibe mcp remove` must each answer `-h`
  and `--help` with their own help on stdout and exit 0.
- FR-07: Every `vibe mcp` argument failure must print a usage line and a
  `{prog}: error: {message}` line on stderr and exit 2.
- FR-08: `vibe mcp add` must accept one positional name and twelve options
  across thirteen spellings, and no others.
- FR-09: `vibe mcp add` must apply the reference's eleven validation rules
  before writing anything.
- FR-10: `vibe mcp add` must be idempotent on an identical entry and must refuse
  a name or URL collision that is not identical.
- FR-11: `vibe mcp add` must persist an OAuth entry before it begins the login,
  print the login URL, and open a browser unless `--no-login` was given.
- FR-12: `vibe mcp remove` must delete stored OAuth credentials before the
  configuration entry, and must leave the configuration untouched if that
  deletion fails.
- FR-13: Every `vibe mcp` write must target the user configuration and must not
  modify the project configuration.
- FR-14: `vibe --help` must show the reference's value names, declaration order
  and epilog structure, with prose written originally for this repository.
- FR-15: The system must not advertise in help any flag the reference does not
  declare.
- FR-16: A bad `--workdir`, a bad `--add-dir` and a deleted working directory
  must each produce their own message and exit 1.
- FR-17: A post-parse validation failure must exit 1; a parse failure must exit
  2; help and version must exit 0.
- FR-18: A programmatic launch must set the headless flag and withhold the two
  interactive tools.
- FR-19: An unambiguous long-flag prefix must resolve; an ambiguous one must be
  refused.
- FR-20: A negative numeric value must be accepted at the parse boundary for the
  three numeric options.
- FR-21: A piped prompt on an interactive route must leave the process with
  standard input reattached to the controlling terminal where one exists.
- FR-22: `--setup` and `--check-upgrade` must run without draining standard
  input.
- FR-23: The system must not write any reference-authored sentence into this
  repository, in source, in documentation or in a corpus.

## Non-Functional Requirements

- **Performance:** the CLI surface replay adds at most 2 seconds to
  `cargo test --workspace --all-features` on the development machine, measured
  by the test's own printed duration. The capture completes in under 30 seconds.
- **Performance:** `vibe mcp remove` and a `vibe mcp add --no-login` complete in
  under 500 ms excluding any network call, measured from process start to exit.
- **Reliability:** the replay is deterministic: 20 consecutive runs produce
  identical output, asserted once during US-310 and not on every run.
- **Reliability:** the capture is byte-reproducible: two runs with no change in
  between produce identical corpora, enforced by `--check` in CI.
- **Portability:** the corpus replays unchanged on Linux, macOS and Windows.
  Any case whose outcome is platform-specific, such as the `/dev/tty` reattach,
  is recorded with its platform and asserted per platform.
- **Coverage:** at least 120 argv cases, at least 34 recorded actions and at
  least 3 parsers, enforced as floors by the replay.
- **Security:** no credential, token, API key value or absolute user path
  reaches the committed corpus. Environment values captured from `--env` cases
  use synthetic names and values only, asserted by the corpus audit.
- **Licensing:** zero reference-authored sentences in the repository, asserted
  by the corpus cleartext audit in US-310 and by every help and error string
  being written originally.
- **Compatibility:** the four shipped completion files continue to generate and
  their parity test continues to pass after the help structure changes.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Reference checkout absent | `VIBE_REFERENCE` unset and default path missing | Corpus replays; live probe skips | Printed skip reason naming the path and the override |
| 2 | Reference checkout off-pin | Local checkout at v2.24.3 | Corpus replays; live probe skips | Printed reason naming both commits and the restore command |
| 3 | Bare `vibe mcp` | No sub-command | Root help on stdout, exit 0 | Help |
| 4 | Unknown `vibe mcp` sub-command | `vibe mcp list` | Usage plus an invalid-choice line, exit 2 | Names the two valid choices |
| 5 | `vibe mcp remove` with no name | Missing positional | Usage plus a required-argument line, exit 2 | Names `NAME` |
| 6 | `vibe mcp remove` with two names | Extra positional | Usage plus an unrecognized-arguments line under the root prog, exit 2 | Names the extra |
| 7 | Removing a server that is not configured | Any unknown name | Message and exit 0, no write | States it is not configured in the user configuration |
| 8 | Removing an OAuth server whose keyring is locked | Credential deletion fails | Configuration unchanged, exit 2 through the failure funnel | Names the server and the credential failure |
| 9 | `--url` with no value | Flag at end of argv | Usage plus an expected-one-argument line, exit 2 | Names `--url` |
| 10 | Invalid `--transport` | `--transport bogus` | Usage plus an invalid-choice line, exit 2 | Names the three valid choices |
| 11 | stdio flags on a remote transport | `--command` with the default transport | Refused before any write, exit 2 | Names every offending flag |
| 12 | Remote flags on stdio | `--url` with `--transport stdio` | Refused before any write, exit 2 | Names every offending flag |
| 13 | Malformed header or environment pair | `--header Authorization` | Refused, exit 2 | Names the required `NAME=VALUE` shape |
| 14 | Duplicate header differing only by case | `--header a=1 --header A=2` | Refused as a duplicate, exit 2 | Names the header |
| 15 | OAuth options with static authentication | `--no-login` with `--api-key-env` | Refused, exit 2 | States the two cannot be combined |
| 16 | Adding the same server twice | Identical second `add` | No write, exit 0 | States it is already configured |
| 17 | Same URL under a different name | A second `add` with a new name | Refused, exit 2 | Names the existing server |
| 18 | Same name with different options | A second `add` with a changed header | Refused, exit 2 | States the name is configured with different options |
| 19 | Concurrent write to the user configuration | Two `add` calls racing | The loser is refused, the file holds the winner's entry | Reported through the failure funnel |
| 20 | Browser cannot open | Headless host during OAuth login | URL already printed, flow continues | Failure on stderr, not fatal |
| 21 | OAuth login fails | Provider rejects the exchange | Entry stays persisted, exit 1 | Names how to authenticate from a session |
| 22 | `--workdir` that does not exist | Any missing path | Exit 1 before any session work | Names the flag and the resolved path |
| 23 | `--add-dir` that does not exist | Any missing path | Exit 1 before any session work | Names the flag and the raw argument |
| 24 | Working directory deleted under the shell | `rmdir` on the current directory, then `vibe` | Exit 1 | Two lines, the second offering `--workdir` |
| 25 | Ambiguous flag prefix | A prefix matching two flags | Refused, exit 2 | Names the ambiguity |
| 26 | Prefix made ambiguous by a hidden Rust-only flag | A prefix unambiguous upstream | Recorded as a ledger entry, outcome asserted | Whatever the ledger records |
| 27 | Negative numeric value | `--max-turns -5` | Accepted at the parse boundary | None at parse time |
| 28 | Empty programmatic prompt with empty stdin | `vibe -p ""` with a closed pipe | Exit 1 | States no prompt was provided |
| 29 | Piped prompt with no controlling terminal | `echo hi \| vibe` inside a daemon | Reattach fails silently, launch continues | None |
| 30 | `--setup` with something on stdin | `echo hi \| vibe --setup` | Onboarding runs, pipe never drained | None |
| 31 | Corpus schema drift | A capture adds a field without a version bump | Replay fails | Names the expected and found versions |
| 32 | Stale ledger entry | A divergence was fixed but its entry remains | Staleness check fails | Names the entry |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | clap's `Error` does not expose enough context to re-render every argparse sentence, so US-320 cannot reach exact text | High | Med | US-320 is written to produce a recorded answer either way: unrecoverable kinds become ledger entries naming the constraint, and the exit codes, which are what a script reads, are conformant regardless |
| 2 | Reproducing argparse's message templates is read as copying reference text | Low | High | Those templates are CPython standard library text, not reference-authored, and the distinction is stated once in `## Overview` and once in the ledger entry; every `vibe`-authored sentence is rewritten and every captured one is stored as a digest |
| 3 | Writing original help prose for 19 flags produces help that reads differently from the reference's, and someone calls that a parity failure | Med | Med | US-318 compares structure and the ledger carries one entry for the prose class with the licensing reason; the row's restatement in US-326 names the boundary explicitly |
| 4 | `vibe mcp add` pulls in an OAuth flow larger than the epic, because the login path touches the keyring, a browser and a callback server | High | High | US-317 is last in its epic, is the only P1 there, and is scoped to the CLI's own behavior: persistence ordering, the printed URL, the browser attempt and the two failure messages, with the exchange itself driven against an in-process fake and any live case recorded in `unavailable` |
| 5 | Enabling prefix inference changes which argv this port accepts and breaks a habit or a script built on today's binary | Med | Med | The nine hidden flags are the only new ambiguity source and US-322 makes each one a recorded case rather than a surprise; `CHANGELOG.md` records the change under US-326 |
| 6 | Reattaching stdin to `/dev/tty` misbehaves under a terminal multiplexer, a CI runner or a container with no controlling terminal | Med | High | US-324 requires the failure to be swallowed and the launch to continue, which is the reference's own behavior, and the no-terminal case is edge case 29 with its own assertion |
| 7 | Changing `InvalidArguments` from exit 2 to exit 1 breaks a wrapper that reads 2 | Low | Med | It is a parity fix toward the reference, exit 2 stays reserved for parse failures where both agree, and `CHANGELOG.md` records it |
| 8 | The capture writes to the user's real config or keyring while driving `run_mcp_cli` | Low | High | US-309 pins `VIBE_HOME` into a temporary directory and asserts the real config was untouched before the first case, mirroring the guard the shell session capture uses |
| 9 | The corpus captures a real URL, token or absolute home path | Low | High | Synthetic values only, absolute paths relativized against the case root, and the cleartext audit in US-310 asserts it |
| 10 | Row 7 turns out to have a seventh gap this pass missed, so 100 is claimed too early | Med | High | US-326 requires the full unfiltered CI sequence and requires the row to name what would falsify it; the oracle now covers the half that had none, which is where the unknown most plausibly lived, and this pass already found five errors by falsifying its own first read |
| 11 | The `vibe mcp` parser written by hand drifts from the top-level clap parser's conventions and becomes a second style to maintain | Med | Low | Its grammar is closed (two sub-commands, thirteen spellings) and the corpus is its specification, so drift fails a test rather than accumulating |
| 12 | US-321 moves row 13 without that row's own oracle, so its score becomes half-measured | Med | Low | US-321's last criterion requires row 13 to be updated in the same commit, and US-326 requires it to be re-read rather than silently inherited |

## Non-Goals

- Re-pinning the reference to v2.24.3. The pin stays at `b78b451`; a bump would
  require regenerating every committed corpus in the same change.
- Reproducing the reference's `help=` prose, epilog sentences, tool descriptions
  or `MCPCommandError` wording. `NOTICE` forbids it; structure is reproduced,
  prose is rewritten, and the corpus stores digests.
- Taking row 13 (programmatic mode) to a new score. US-321 closes the one option
  `cli.py` sets that this port does not, and US-326 re-reads the row; neither
  restates it beyond what this work's evidence forces.
- Taking row 11 (MCP) to 100. `vibe mcp add` is shared between rows 7 and 11, so
  closing it moves both, but the transport and OAuth oracles row 11 names as its
  own gap are not built here.
- Removing the nine hidden Rust-only flags. They serve the test harness and the
  distribution; they become a recorded accepted divergence, and only their
  effect on prefix inference is asserted.
- Removing the four shipped completion files. The reference ships none; this is
  an additive divergence with an existing test and it stays.
- Hand-rolling the top-level 19-flag parser. US-320 re-renders clap's typed error
  instead, which is bounded and testable where a rewrite is neither.
- A Windows CI job. The corpus records its platform and the platform-specific
  cases assert per platform; a later Windows capture is additive.
- Changing the session, trust, worktree or resume behavior the ten existing
  `startup.json` traces already assert. Those traces stay exactly as they are and
  the new corpus sits beside them.

## Files NOT to Modify

- `crates/vibe-core/src/parity.rs`: carries the pin; changing it invalidates
  every committed corpus at once.
- `scripts/parity/pin.py`: the second pin source; the parity test fails when the
  two disagree or when a third copy appears.
- `NOTICE`: declares the licensing boundary this work operates under.
- `crates/vibe-cli/tests/runtime-parity/startup.json` and its oracle: the ten
  mode traces are measured and conformant; the flags half gets a corpus of its
  own rather than widening this one.
- `crates/vibe-app-server/tests/tool-surface/baseline.json`: a different row's
  oracle; nothing here changes what tools are published.
- `crates/vibe-core/src/shell/` and `crates/vibe-core/src/tools/shell/`: row 6's
  territory, untouched by an argument parser.
- `action.yml`, `.github/workflows/action.yml`, `scripts/install.sh`,
  `scripts/install.ps1`: they carry the hand-written version string, and this
  work does not bump a version.
- `vibe/**` in the reference checkout: read-only oracle, never written.

## Technical Considerations

- **Architecture:** should the CLI corpus live in
  `crates/vibe-cli/tests/runtime-parity/` beside `startup.json`, or in a new
  directory? Recommended: beside `startup.json`, as `cli-surface.json` with
  `cli-surface-oracle.py`, because the two halves of row 7 then reproduce from
  one place and the existing directory already has the corpus-plus-oracle
  convention.
- **Architecture:** the capture could live in `scripts/parity/` with the other
  23, or beside the corpus like the six runtime-parity oracles. Recommended:
  beside the corpus, following `startup-oracle.py`, and importing
  `EXPECTED_COMMIT` from `scripts/parity/pin.py` the way that file already does.
- **Architecture:** the `vibe mcp` parser could be a second clap `Command` or a
  hand-rolled parser. Recommended: hand-rolled. Its grammar is closed and small,
  the corpus is its specification, and it is the only way to reach argparse's
  exact error sentences without fighting clap's renderer. The top-level parser
  stays on clap derive.
- **API Design:** should the argparse-shaped error render live in `vibe-cli` or
  in `vibe-core`? Recommended: `vibe-cli`, next to the `Arguments` derive it
  renders errors for, because nothing below layer 3 parses argv.
- **Data Model:** the corpus needs three record kinds (an action declaration, a
  help render, an argv outcome) under one `schemaVersion`. Recommended: one file
  with three arrays rather than three files, so a schema bump is atomic and the
  replay reads one path.
- **Data Model:** an argv outcome for an accepted vector needs the resulting
  values, which means mapping argparse `dest` names to Rust field names.
  Recommended: record the reference `dest` and keep the mapping in the Rust
  replay, so the corpus stays a pure observation of the reference.
- **Dependencies:** none new. clap already provides `infer_long_args`,
  `allow_negative_numbers`, `value_name`, `after_help` and `display_order`, and
  `CommandFactory` already backs the completion parity test, so the replay reads
  the same builder rather than a second hand-written table.
- **Migration:** the observable changes are `vibe mcp`'s exit codes and output,
  the arrival of `vibe mcp add`, the help render, the startup error messages, the
  exit code for post-parse validation failures, and prefix inference. All six are
  recorded in `CHANGELOG.md` by US-326.
- **Sequencing:** EP-098 blocks the assertions but not the implementations.
  US-319, US-321 and US-324 have no dependency and can land first. EP-099 blocks
  EP-100 entirely. EP-101 and EP-102 depend on EP-098 for their replay
  assertions. EP-103 requires all of them.
- **Licensing:** the boundary applied throughout is that names, structure,
  metavars, defaults, choices, ordering, exit codes and CPython's own message
  templates are observations and are reproduced, while sentences the reference
  authors are rewritten and stored as digests in the corpus. Stated once here,
  cited by every story that renders text.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Row 7 score | 92, flags half priced by reading | 100, with a named oracle | Month-1 | `docs/parity.md` row 7 |
| Capture scripts covering the CLI argument surface | 0 | 1 | Month-1 | `crates/vibe-cli/tests/runtime-parity/` inventory |
| argv cases replayed | 0 | 120 or more | Month-1 | Case count printed by the replay test |
| Reference argparse actions with a recorded declaration | 0 of 34 | 34 of 34 | Month-1 | Action count printed by the replay test |
| `vibe mcp` argv shapes whose exit code matches | 0 of 7 | 7 of 7 | Month-1 | The `vibe mcp` cases in the replay |
| `vibe mcp` sub-commands implemented | 1 of 2 | 2 of 2 | Month-1 | `crates/vibe-cli/src/mcp_command.rs` |
| `vibe mcp add` options accepted | 0 of 12 | 12 of 12 | Month-1 | The `add` action records in the replay |
| `vibe mcp add` validation rules enforced | 0 of 11 | 11 of 11 | Month-1 | The refusal cases in the replay |
| OAuth credentials orphaned by a remove | Every OAuth server | 0 | Month-1 | The ordering test in US-313 |
| Options showing a value name in `--help` | 1 of 19 | 12 of 12 that declare one upstream | Month-1 | `vibe --help` against the recorded render |
| Options showing an empty description in `--help` | 19 of 19 | 0 of 19 | Month-1 | Same |
| Startup failures with a dedicated message | 0 of 3 | 3 of 3 | Month-1 | The workdir, add-dir and deleted-cwd cases |
| Post-parse validation failures exiting 1 | 0 | All | Month-1 | `CliError::exit_code` and the replay |
| Row-7 divergences with no ledger entry | All of them | 0 | Month-1 | `docs/parity.md` divergence sections |
| Full CI sequence, unfiltered | Passing | Passing | Month-6 | Four commands from the workspace root |

## Open Questions

- Where exactly is the line between a reference-authored sentence and an
  observation of user-visible behavior? Owner: Arthur Jean, before US-315.
  Defaulted to: a single factual status or validation line is an observation and
  is reproduced (which is what `crates/vibe-core/src/config/mcp.rs:715-763`
  already does, and what `docs/parity.md` means by "user-visible output"), while
  a `help=` body, an epilog paragraph or any multi-sentence guidance is
  reference-authored and is rewritten. If the answer is stricter, US-315 and
  US-316 rewrite their sentences and one ledger entry covers the class instead of
  none.
- Should `vibe --help` keep clap's own section headings and usage grammar, or
  reproduce argparse's? Owner: Arthur Jean, before US-318. Defaulted to keeping
  clap's, because the headings are the renderer's and not the reference's
  declaration, and US-318 asserts the parts that are declared: value names,
  order, hidden flags, accepted values and the epilog's structure.
- Should the nine hidden Rust-only flags move behind a build feature or an
  environment variable so they leave the argv surface entirely? Owner: Arthur
  Jean, before US-322. Defaulted to leaving them and recording the divergence,
  because they serve the test harness and moving them is a larger change than
  this row's residual justifies. If they move, US-322's third criterion becomes
  unnecessary.
- Should `vibe -p "   "` be accepted, matching Python truthiness, or refused as
  it is today? Owner: Arthur Jean, before US-323. Defaulted to whatever the
  corpus records for the reference, because that is what a measurement is for;
  if the reference accepts it and this port keeps refusing, the refusal needs a
  ledger entry and a reason.
- Should `vibe mcp add`'s OAuth login be exercised against a live provider in any
  environment, or only against the in-process fake? Owner: Arthur Jean, before
  US-317. Defaulted to the fake only, with the live case recorded in the corpus's
  `unavailable` block, because a network-dependent test in the default suite
  would violate the determinism requirement.
[/PRD]
