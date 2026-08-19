[PRD]
# PRD: Slash Commands at Full Parity

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-20 | Arthur Jean | Initial draft: take parity row 2 from 98 to 100 |

## Problem Statement

Row 2 of `docs/parity.md` ("Slash commands") scores 98. The row states its own
reason for not being 100: it was "measured by inventory diff rather than by
oracle". A deep re-read of both trees at reference commit `b78b451` confirms the
inventory is right and finds five behavioral defects underneath it, none of
which an inventory diff can see:

1. **`get_help_text()` is not ported.** The reference builds a Markdown document
   with three `###` sections and mounts it into the transcript
   (`vibe/cli/commands.py`, mounted by `_show_help` in
   `vibe/cli/textual_ui/app.py:2452`). Its command section lists every registry
   key, sorted by key, carrying every alias of each command with the canonical
   `/name` first. This port answers `/help` with a modal picker
   (`crates/vibe-cli/src/tui/pickers.rs:36`) holding seven shortcut rows and one
   alias per command in declaration order, with no special-features section.
   `/help` is the single most-used slash command and the one whose output
   diverges most.
2. **Nothing echoes the command line.** The reference mounts a
   `SlashCommandMessage` before running any handler, showing the operator what
   ran (`vibe/cli/textual_ui/app.py:1638-1652`). It also normalizes what it
   shows: a slash line loses its prefix and keeps its arguments, a bare alias is
   replaced by the registry key, so `:q` displays as `exit`. This port mounts
   nothing (`crates/vibe-cli/src/tui/shortcuts.rs:533`), so a scrolled-back
   transcript cannot say which command produced which output.
3. **Telemetry reports the wrong name, at the wrong time, and skips one
   command.** The reference records the registry key
   (`{"command": cmd_name.lstrip("/")}`), so `/connectors` reports `mcp`, `/new`
   reports `clear` and `:q` reports `exit`. This port reports the alias the
   operator typed with its slash stripped
   (`crates/vibe-cli/src/tui/telemetry.rs:43`,
   `crates/vibe-core/src/telemetry/records.rs:979`), so the same command reaches
   the dashboard under three names. It also reports before the busy check
   (`crates/vibe-cli/src/tui/workflow.rs:131` runs ahead of line 137), so a
   refused command emits an event the reference never emits, and
   `is_exit_command` (`crates/vibe-cli/src/tui/mod.rs:352`) short-circuits the
   literal `/exit` before dispatch, so that one command emits nothing at all
   while its four siblings emit.
4. **The parser is ASCII-case-insensitive where the reference is
   Unicode-lowercasing.** `get_command_name` calls `user_input.lower()`, which
   folds every character Unicode says folds; `parse_command_in`
   (`crates/vibe-cli/src/tui/commands.rs:255`) compares with
   `eq_ignore_ascii_case`. Measured: `/THIN\u{212A}ING`, spelled with the Kelvin
   sign, resolves to `thinking` upstream and does not parse here.
5. **Two completer edges answer differently.** Measured against the reference's
   `CommandCompleter`: `//` and `///` return no candidates upstream and return
   the entire command list here, because `ranked_slash_candidates`
   (`crates/vibe-cli/src/tui/completion.rs`) strips every leading slash instead
   of one; and text `/help` with the cursor at offset 0 returns the full list
   with replacement range (0,0) upstream and returns nothing here.

Underneath all five sits the reason the row cannot be certified either way:
**there is no oracle over `commands.py`.** The chat-input corpus in
`crates/vibe-cli/tests/parity/` drives `ChatInputContainer` through Textual's
headless pilot. It can see a popup row, and it cannot see `parse_command`,
`get_help_text`, availability filtering or telemetry, because none of those
belong to the widget. Its fifteen slash traces are all at `parity` and none of
the five defects above is reachable from any of them.

**Why now:** row 2 is the highest-traffic surface in the port that no
differential instrument reads. Every score in `docs/parity.md` that reaches 100
does so from a replay against a committed corpus; row 2 reaches 98 from a
hand-diffed list of names, which is exactly the evidence class the 2026-08-19
audit demonstrated can be wrong by 45 points. The row is also the cheapest
oracle left to build: `CommandRegistry` is a frozen dataclass registry whose
only import at the pin is one constant, so the capture needs neither Textual
nor a virtualenv.

## Overview

This PRD closes row 2 in four movements, then restates the row.

**Build the instrument first.** `scripts/parity/commands.py` drives the pinned
`CommandRegistry` directly and writes `crates/vibe-cli/tests/commands/corpus.json`:
an alias and key inventory, a parse matrix over inputs chosen to hit the
reference's own branches (trimming, whitespace splitting, bare aliases with and
without arguments, Unicode case folding, empty and whitespace-only input), an
availability matrix over the four `CommandContext` values the CLI can produce,
and the help document reduced to its structure plus a per-line digest.
`crates/vibe-cli/src/tui/commands_parity_tests.rs` replays it in the shape
`crates/vibe-cli/src/tui/promo_parity_tests.rs` already proves: families, a
divergence ledger that fails both on an unrecorded divergence and on a stale
entry, a comparison floor, a live probe that skips through
`vibe_core::parity::off_pin_reason`, and a guard asserting this port's prose can
never equal a reference digest. Every later story is measured by it rather than
argued.

**Port the help surface.** `/help` stops opening a modal and starts writing a
Markdown message into the transcript, which `crates/vibe-cli/src/tui/render/markdown.rs:9`
already renders with headings, bullets and code spans. The document carries the
reference's three sections in the reference's order, with the reference's line
counts and its sorted-by-key command list showing every alias per command,
canonical `/name` first. The words are this port's own, because the reference's
lines are authored prose `NOTICE` forbids shipping: the corpus records each
reference line as a length and a SHA-256, the replay compares structure for
equality and prose for permanent inequality, and `docs/parity.md` gains the
divergence row that makes the split explicit. This is the same treatment the
scorecard already applies to the two builtin skill bodies.

**Make a submitted command line observable.** The transcript gains the echo the
reference mounts, with the reference's own display rule. Telemetry moves behind
the busy check, learns the registry key from the parsed identifier, and stops
being skipped for `/exit`. The refusal a busy runtime emits splits into the
reference's two reasons, busy and paused-queue, instead of collapsing both into
one sentence.

**Close the parser and completer edges.** One leading slash is stripped instead
of all of them, a slash line completes with the cursor anywhere in it including
offset 0, and alias resolution lowercases the way the reference lowercases. Five
new chat-input scenarios cover what the widget harness can observe; the parse
matrix in the new corpus covers what it cannot.

What this PRD deliberately does not close is the second column of the scorecard.
Upstream `HEAD` has already moved this exact module: it adds a command key,
relocates the `/retry` prompt builder out of `commands.py`, and rewrites one
description and one shortcut line. Closing row 2 against the pin is the work;
closing it against `HEAD` is a re-pin, and `AGENTS.md` prices that at
regenerating every committed corpus in one change.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Parity row 2 score against the pin | 100, every sub-claim citing a measurement | 100 held across one re-pin |
| Registry behaviors replayed by an oracle | at least 240 comparisons (baseline 0) | at least 240 |
| Slash-command defects reachable by no instrument | 0 of 5 (baseline 5 of 5) | 0 |
| Slash traces in the chat-input corpus | 20 (baseline 15) | 20 |
| Row-2 divergences with no ledger entry | 0 (baseline 3) | 0 |

## Target Users

### A person running a slash command
- **Role:** a daily operator of `vibe` in a terminal.
- **Behaviors:** types `/` to see what exists, picks a command from the popup, scrolls back later to read what happened.
- **Pain points:** `/help` opens a modal that vanishes on Escape and leaves nothing behind, lists one alias per command so `/new`, `/connectors` and `:q` are undiscoverable, and never mentions that `!` runs a shell command or that `@` completes a path. A scrolled-back transcript shows a command's output with no record of the command.
- **Current workaround:** reopen `/help` and read it again, or read `crates/vibe-cli/src/tui/commands.rs` to learn the aliases.
- **Success looks like:** `/help` leaves a searchable, selectable, copyable message in the transcript listing every alias of every available command, and every command that runs leaves its own line above its output.

### A person reading the parity scorecard
- **Role:** anyone deciding whether this port can replace the reference, Arthur included.
- **Behaviors:** reads a row's score, then its evidence column, then runs the command the row names.
- **Pain points:** row 2 says 98 and names no command to run. There is nothing to execute that would confirm or refute it, and the row's own note admits the measurement class is weaker than every 100 on the page.
- **Current workaround:** re-read both trees by hand, which is what produced this PRD and costs a working day.
- **Success looks like:** row 2 carries a command, the command replays a committed corpus, and the divergences that remain are rows in a ledger that fails when one of them silently closes.

### Whoever reads the telemetry
- **Role:** the owner of the `vibe.slash_command_used` series.
- **Behaviors:** groups events by `command` to rank what operators use.
- **Pain points:** one command arrives under up to three names, `/exit` never arrives at all, and refused commands inflate every count. Nothing joins this port's series to the reference's.
- **Current workaround:** none; the divergence is undocumented.
- **Success looks like:** one event per executed command, named by the registry key, comparable to the reference's series row for row.

## Research Findings

### Method note

Row 2 is a closed parity target against a private pinned checkout, so the
competitive landscape a general PRD would research does not exist: there is
exactly one implementation to match and it is on disk. Phase-2 web research was
substituted for measurement against the pin, which is what `AGENTS.md` requires
for a parity claim and a stronger source than any external survey. Every finding
below is a reading of the reference at `b78b451` or of this repository, with the
path that produced it. Two external inputs were used and are marked as such.

### The reference contract, read at the pin

- `vibe/cli/commands.py` publishes `CommandContext`, `build_retry_prompt`, `Command` and `CommandRegistry`, with 28 registry keys and 35 aliases: 31 slash-prefixed and 4 bare (`exit`, `quit`, `:q`, `:quit`). Two aliases ride on a key they do not name, `/connectors` on `mcp` and `/continue` on `resume`.
- `CommandRegistry.commands` is the filtered set: `refresh` rebuilds it excluding disabled names and unavailable commands, so availability governs the popup, the parser and the help document at once.
- `parse_command` strips, splits on the first whitespace run, resolves through a lowercased alias map, and refuses a bare alias followed by arguments so a prompt beginning with `exit` is not swallowed.
- `get_help_text` emits three `###` sections in a fixed order, then one line per key sorted by key, each listing every alias with the canonical `/name` sorted first.
- `vibe/cli/autocompletion/slash_command.py` publishes the popup controller: it claims a line starting with `/`, guards an out-of-range cursor, and maps Tab, Enter, Down and Up onto handled, submit and wrapping selection.
- `vibe/cli/autocompletion/completers.py` publishes the ranking: only slash-prefixed aliases are candidates, the promotion boosts are `/help` 2.0 and `/config` 1.0, the sort is stable, and the replacement range ends at the cursor or the first space, whichever is earlier.
- `vibe/cli/textual_ui/widgets/chat_input/input_kinds.py` orders classification so a builtin command beats a same-named skill.
- `vibe/cli/textual_ui/app.py` owns the three behaviors the row's Reference column does not name but which only a slash command reaches: the telemetry record, the transcript echo, and the two refusal reasons a busy or paused queue produces.

### What is already at parity, measured

Confirmed identical before this PRD was written, and therefore out of scope: all
28 keys and all 35 aliases including the two that ride on another key; every
description, proven by the four `commands-*` popup traces that record each one;
availability gating on macOS clipboard support, on `vibe_code_enabled` and on
the excluded set; the trimming, splitting and argument-preservation rules of
`parse_command` and its refusal of a bare alias with arguments; the completer's
head-word extraction, replacement range, promotion boosts, stable sort and
description deduplication; the popup's Tab, Enter, Up, Down and Escape behavior
including wrapping; the joining of user-invocable skills into the same surface;
and `crates/vibe-cli/src/tui/completion/fuzzy.rs` as an integer-hundredths port
of the reference's 2.0 / 1.8 / 1.3 multipliers. The chat-input corpus stands at
305 of 306 dimensions at `parity` with `UNMODELED_STATE_PATHS` empty, so nothing
is masked.

### Instruments this repository already has

- `crates/vibe-cli/src/tui/promo_parity_tests.rs` is the replay pattern to copy: `FAMILIES` declaring inputs and answers, `DIVERGENCES` keyed by `family/field/case` with a wildcard prefix, an `audit`/`settle` pair failing on both an unrecorded divergence and a stale entry, `MINIMUM_COMPARISONS` as a coverage floor, a live probe that recaptures and compares only when the checkout is present and on-pin, and `this_ports_prose_never_matches_a_reference_digest` as the license guard.
- `scripts/parity/experiments.py` is the capture pattern for a checkout that is off-pin: it extracts the pinned tree with `git archive` into `.parity/reference-<commit12>`, re-executes itself against it, blocks the socket module so no capture can reach the network, reduces authored prose to `{"length", "digest"}` and scrubs versions and platform identifiers to placeholders.
- `scripts/parity/pin.py` exports `EXPECTED_COMMIT`, `EXPECTED_VERSION`, `DEFAULT_REFERENCE` and `RESTORE_COMMAND`, and `load()` puts itself on `sys.path` for out-of-tree oracles.
- `vibe_core::parity` exports `reference_root()`, `off_pin_reason(root, probe)` and `pinned_interpreter(root)`, which is the entire skip contract a new parity test needs.
- `crates/vibe-cli/src/tui/render/markdown.rs:9` already renders headings, bullets, blockquotes, fenced code, tables and rules into the transcript, and `assistant_markdown_is_rendered_semantically_instead_of_literally` in `crates/vibe-cli/src/tui/tui_parity_tests.rs:301` is the test pattern for asserting it.

### Two external inputs

- **Python's `str.lower()` is Unicode-aware and folds characters outside ASCII onto ASCII letters.** Verified locally: `"/THIN\u{212A}ING".lower()` equals `"/thinking"`, where `\u{212A}` is the Kelvin sign. This is what makes defect 4 above reachable rather than theoretical, and it is the only place in row 2 where a language runtime difference produces a behavioral one.
- **Upstream drift on this exact module.** `git diff --stat b78b451..HEAD` over `vibe/cli/commands.py` and `vibe/cli/autocompletion/` in the reference checkout reports 9 files and roughly +328 / -39 lines. In `commands.py` alone: one registry key added, the `/retry` prompt builder relocated out of the module, one description rewritten and one shortcut line reworded. Recorded as a risk, not as scope.

## Assumptions & Constraints

### Assumptions (to validate)

- **`CommandRegistry` is importable from an extracted tree without a virtualenv**, based on its only non-stdlib import at the pin being one constant from `vibe/cli/constants.py` and one tag constant from `vibe/utils`. Validated by US-229 on first run; if the `vibe/utils` package pulls a dependency, the capture falls back to `pinned_interpreter` like every other oracle, which changes no criterion.
- **Retiring `OverlayKind::Help` is a compile-time-total change**, based on `policy()` in `crates/vibe-cli/src/tui/workflow/keys.rs:76` being an exhaustive match over every kind. Validated by US-232: removing the variant either compiles everywhere or names each site.
- **Writing the help into the transcript does not disturb session persistence**, based on the entry carrying the same shape as the local notices `push_local_notice` already writes. Validated by US-231's criteria on a reloaded session.
- **The four `CommandContext` values the CLI can produce are the full availability space**, based on `_build_command_registry` in `vibe/cli/textual_ui/app.py:755` passing only `vibe_code_enabled` and the port adding only clipboard support and the excluded set. If a fifth appears, the matrix grows and the floor with it.

### Hard Constraints

- `NOTICE` forbids copying reference source or authored prose. The help document's shortcut lines, feature lines and section headings are authored prose upstream: this port writes its own covering the same directives, and the corpus records the reference's as a length plus a SHA-256 only.
- The reference checkout is read-only. No step of this PRD writes to it.
- The pin does not move in this PRD. Re-pinning requires regenerating every committed corpus in the same change and is out of scope.
- Committed corpora replay unconditionally; only the live recapture probe may skip when the checkout is absent or off-pin. Every new parity test follows that rule through `off_pin_reason`.
- The declared layering in `[workspace.metadata.vibe] dependency-layers` holds: the command registry stays in `vibe-cli`, and nothing it needs is added to `vibe-core`.
- Descriptions in `COMMANDS` are already byte-identical to the reference's and are not authored prose this port may rewrite: they are the observable contract the popup traces assert. No story touches them.

## Reference Map

Every story names the reference files to open before writing Rust, so the
implementer reads the declaration instead of grepping for it.

**Root.** `/home/arthur/dev/mistral-vibe` on Linux, `C:\dev\mistral-vibe` on
Windows. `VIBE_REFERENCE` overrides both, `--reference` overrides that for the
capture scripts, and Rust resolves it through `vibe_core::parity::reference_root()`.
Paths below are written in the Linux spelling, which `AGENTS.md` declares
canonical.

**Pin.** `b78b451c39eab9213393ad2f45908e8562a5c5e7`, reference version `2.24.0`.
The local checkout is not guaranteed to sit on it: at the time of writing it is
at `5e6aa0f`, which is `2.24.2`, and that version has already changed this
module. Read at the pin rather than at the working tree:

```sh
REF="${VIBE_REFERENCE:-/home/arthur/dev/mistral-vibe}"
git -C "$REF" show b78b451:vibe/cli/commands.py            # whole file at the pin
git -C "$REF" show b78b451:vibe/cli/commands.py | sed -n '268,300p'   # one symbol
```

**Every line number in this PRD is anchored at the pin, not at the working
tree.** Upstream `HEAD` has already moved `vibe/cli/commands.py`, so opening the
checkout as it sits and jumping to a line number below lands on the wrong
declaration. Read through `git show` or restore the pin with the command
`vibe_core::parity::RESTORE_COMMAND` documents.

**Where row 2 lives upstream.** The scorecard's Reference column names
`vibe/cli/commands.py` and `vibe/cli/autocompletion/slash_command.py`. Three
files it does not name publish behavior only a slash command reaches and are
read by this PRD: `vibe/cli/autocompletion/completers.py` for ranking and the
replacement range, `vibe/cli/textual_ui/widgets/chat_input/input_kinds.py` for
the builtin-beats-skill precedence, and `vibe/cli/textual_ui/app.py` for the
telemetry record, the transcript echo and the two refusal reasons. US-238 adds
all three to the column.

**Symbol table, verified at the pin.** Every declaration a story sends the
implementer to, with the line it starts on under
`/home/arthur/dev/mistral-vibe/`:

| Symbol | Path at the pin | Line | Read by |
|---|---|---|---|
| `CommandContext` | `vibe/cli/commands.py` | 12 | US-229 |
| `build_retry_prompt` | `vibe/cli/commands.py` | 19 | context only, out of scope |
| `Command` | `vibe/cli/commands.py` | 34 | US-229 |
| `CommandRegistry.__init__` | `vibe/cli/commands.py` | 43 | US-229 |
| `CommandRegistry._build_commands` | `vibe/cli/commands.py` | 54 | US-229 |
| `CommandRegistry.commands` | `vibe/cli/commands.py` | 216 | US-229 |
| `CommandRegistry.refresh` | `vibe/cli/commands.py` | 219 | US-229, US-231 |
| `CommandRegistry._is_command_available` | `vibe/cli/commands.py` | 228 | US-229, US-231 |
| `CommandRegistry._alias_map` | `vibe/cli/commands.py` | 233 | US-229, US-230 |
| `CommandRegistry.get_command_name` | `vibe/cli/commands.py` | 246 | US-230, the Unicode lowercase rule |
| `CommandRegistry.parse_command` | `vibe/cli/commands.py` | 249 | US-229, US-233, US-234 |
| `CommandRegistry.get_help_text` | `vibe/cli/commands.py` | 268 | US-229, US-231, US-232 |
| the three `###` section literals | `vibe/cli/commands.py` | 270, 281, 286 | US-231, US-232 |
| `CLIPBOARD_IMAGE_PASTE_SUPPORTED_SYSTEM` | `vibe/cli/constants.py` | 3 | US-229, US-237 |
| `CommandCompleter` | `vibe/cli/autocompletion/completers.py` | 33 | US-236 |
| `CommandCompleter._head_word` | `vibe/cli/autocompletion/completers.py` | 43 | US-236 |
| `CommandCompleter._fuzzy_filter` | `vibe/cli/autocompletion/completers.py` | 49 | context, already at parity |
| `CommandCompleter.get_completion_items` | `vibe/cli/autocompletion/completers.py` | 73 | US-236 |
| `CommandCompleter.get_replacement_range` | `vibe/cli/autocompletion/completers.py` | 84 | US-236 |
| `SlashCommandController.can_handle` | `vibe/cli/autocompletion/slash_command.py` | 16 | US-236, US-237 |
| `SlashCommandController.on_text_changed` | `vibe/cli/autocompletion/slash_command.py` | 25 | US-237 |
| `SlashCommandController.on_key` | `vibe/cli/autocompletion/slash_command.py` | 44 | US-237 |
| `classify` | `vibe/cli/textual_ui/widgets/chat_input/input_kinds.py` | 43 | context, already at parity |
| `_REJECT_HINT_BUSY` | `vibe/cli/textual_ui/app.py` | 476 | US-235 |
| `_REJECT_HINT_PAUSED` | `vibe/cli/textual_ui/app.py` | 477 | US-235 |
| the busy refusal call site | `vibe/cli/textual_ui/app.py` | 1061 | US-235 |
| `_handle_paused_submit` | `vibe/cli/textual_ui/app.py` | 1104 | US-235 |
| `_handle_command` | `vibe/cli/textual_ui/app.py` | 1638 | US-233, US-234 |
| `_show_help` | `vibe/cli/textual_ui/app.py` | 2452 | US-231 |

`_show_help` mounts a different message type than `_handle_command` does, which
is why US-231 and US-233 are separate stories rather than one.

## Quality Gates

These commands must pass for every user story:
- `cargo fmt --all -- --check` - formatting is uniform across the workspace
- `cargo check --workspace --all-targets --all-features` - every target compiles, including the feature-gated fixture binaries
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` - no lint warning survives
- `cargo test --workspace --all-features` - the full suite, not a filtered subset, because row-2 fixtures are read from more than one module

For stories that add or change a capture script:
- `python3 -m compileall -q scripts/parity/` - every capture script parses
- the script runs to completion against the pinned tree and rewrites its corpus byte-identically on a second run

## Epics & User Stories

### EP-069: An oracle over the command registry

Give row 2 the instrument every 100 on the scorecard already has, so the four
epics after it are measured rather than argued.

**Definition of Done:** a committed corpus captured from the pinned
`CommandRegistry` replays in `cargo test --workspace --all-features` with an
audited ledger and a comparison floor, and the live probe recaptures and
compares byte-for-byte when the checkout is on-pin.

#### US-229: Capture the command registry from the pinned reference
**Description:** As a person reading the scorecard, I want row 2's contract captured from the reference into a committed corpus, so that a claim about it can be replayed instead of re-read.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/commands.py` at the pin, read whole: `CommandRegistry.__init__:43`, `_build_commands:54`, `commands:216`, `refresh:219`, `_is_command_available:228`, `_alias_map:233`, `get_command_name:246`, `parse_command:249`, `get_help_text:268`; and `vibe/cli/constants.py:3` for the clipboard platform constant. Pattern to copy: `scripts/parity/experiments.py` for the extracted-tree capture and the prose digest, `scripts/parity/tool_config.py` for the minimal script shape.

**Acceptance Criteria:**
- [ ] Given the pinned tree, when `scripts/parity/commands.py` runs, then it writes `crates/vibe-cli/tests/commands/corpus.json` carrying `schemaVersion`, `reference: {commit, version}` and one array per family.
- [ ] Given the capture runs, when the inventory family is written, then it records every registry key and every alias of every command, with each alias attributed to its key, and the counts it observed rather than counts written by hand.
- [ ] Given the capture runs, when the parse family is written, then it records for each probe input the resolved key, the matched alias and the arguments, or an explicit no-match, over at least 40 inputs covering: leading and trailing whitespace, an interior whitespace run, a bare alias alone, a bare alias followed by arguments, a slash alias followed by arguments, empty input, whitespace-only input, an unknown alias, an alias in uppercase, and an alias spelled with a non-ASCII character whose Unicode lowercase is an ASCII letter.
- [ ] Given the capture runs, when the availability family is written, then it records the available key set for each of the four contexts the CLI can produce, crossing `vibe_code_enabled` with clipboard support, plus one context with a non-empty excluded set.
- [ ] Given the capture runs, when the help family is written, then it records the document's structure: the ordered section headings with their heading level, the line count of each section, and for each command line the key it belongs to and its ordered alias list; and it records every reference-authored line as a `{length, digest}` pair and never as text.
- [ ] Given the local checkout is off-pin or dirty, when the capture runs, then it extracts the pinned tree with `git archive` and captures from that, rather than failing or silently capturing another revision.
- [ ] Given the capture runs, when any code path attempts a network connection, then the capture fails naming the attempt, mirroring the socket guard `scripts/parity/experiments.py` already installs.
- [ ] Given the capture is run twice with no change in between, when the two corpora are compared, then they are byte-identical.
- [ ] Given the reference checkout is absent, when the capture runs, then it exits non-zero naming the expected path and the `VIBE_REFERENCE` override, and writes no partial corpus.

#### US-230: Replay the registry corpus with an audited ledger
**Description:** As a person reading the scorecard, I want the committed registry corpus replayed against this port on every test run, so that a divergence fails the build instead of aging into a wrong score.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-229
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/commands.py:246` `get_command_name` for the Unicode lowercase rule the parse family asserts; otherwise this story is Rust against a committed corpus. Pattern to copy: `crates/vibe-cli/src/tui/promo_parity_tests.rs` in full.

**Acceptance Criteria:**
- [ ] Given the committed corpus, when `crates/vibe-cli/src/tui/commands_parity_tests.rs` runs, then it asserts `schemaVersion` and that `reference.commit` equals `vibe_core::parity::REFERENCE_COMMIT`, failing when either drifts.
- [ ] Given a corpus case whose answer this port produces differently, when the replay runs, then it fails naming the family, the field and the case, unless a ledger entry records that divergence.
- [ ] Given a ledger entry whose case now conforms, when the replay runs, then it fails as stale, so a closed divergence cannot stay recorded.
- [ ] Given the replay completes, when the comparison count is below 240, then it fails naming the count, so a shrunken corpus cannot pass as a green one.
- [ ] Given the reference checkout is absent or off-pin, when the live probe runs, then it prints the reason from `vibe_core::parity::off_pin_reason` and returns without failing, and the corpus replay above still runs.
- [ ] Given the reference checkout is present and on-pin, when the live probe runs, then it re-runs `scripts/parity/commands.py` into `target/` and asserts the fresh corpus equals the committed one.
- [ ] Given the parse family replays, when an alias is spelled with a non-ASCII character whose Unicode lowercase is an ASCII letter, then this port resolves the same key as the reference, which requires alias comparison to lowercase rather than to compare ASCII-insensitively.
- [ ] Given `crates/vibe-cli/tests/commands/corpus.json`, when it is searched for text, then it holds no reference-authored sentence in cleartext, and a test asserts every one of this port's help lines is unequal to every recorded digest.

---

### EP-070: The help surface the reference publishes

Replace the modal help with the document the reference mounts, written in this
port's own words and asserted on its structure.

**Definition of Done:** `/help` writes a Markdown message into the transcript
carrying three sections in the reference's order with the reference's line
counts and its sorted-by-key command list, the modal help overlay no longer
exists, and the prose split is a row in the scorecard's divergence table.

#### US-231: Answer /help with a Markdown transcript message
**Description:** As a person running a slash command, I want `/help` to leave a message in the transcript, so that I can scroll back to it, select it and copy it instead of reopening a modal.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-230
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/commands.py:268` `get_help_text` for the document's structure and its ordering rules, and `_show_help` at `vibe/cli/textual_ui/app.py:2452` for how it is mounted.

**Acceptance Criteria:**
- [ ] Given `/help` is submitted, when it is handled, then a transcript entry is appended and no overlay is opened.
- [ ] Given the help entry is rendered at width 80, when the frame is drawn, then its three section headings render as headings and its list lines render as bullets, asserted the way `assistant_markdown_is_rendered_semantically_instead_of_literally` asserts assistant Markdown.
- [ ] Given the help document is built, when its sections are read in order, then they are keyboard shortcuts, special features and commands, at the same heading level, with the same line counts the corpus recorded for the reference: eight, two, and one per available command.
- [ ] Given the help document is built, when the command section is read, then its lines are sorted by registry key, each line lists every alias of that command with the canonical `/name` first and the remainder sorted, and each alias is rendered as a code span.
- [ ] Given a context where a command is unavailable, when the help document is built, then that command contributes no line, so `/paste-image` is absent off macOS and `/teleport` is absent without Vibe Code.
- [ ] Given a context where every command is excluded, when the help document is built, then the three headings still render and the command section holds zero lines rather than the document being empty or panicking.
- [ ] Given a session is reloaded from disk, when the transcript is restored, then the help entry restores like any other local entry and is not duplicated.
- [ ] Given the help document is compared to the corpus, when the replay runs, then its structure matches the reference's and every one of its lines is unequal to the reference digest for that line.

#### US-232: Retire the help overlay and reconcile the shortcut inventory
**Description:** As a person running a slash command, I want the help to describe the chords this binary actually binds, so that it never advertises a key that does nothing here or hides one that works.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-231
**Reference:** `/home/arthur/dev/mistral-vibe/vibe/cli/commands.py:268` `get_help_text` for the eight shortcut slots and the two feature lines, whose section literals start at lines 270, 281 and 286.

**Acceptance Criteria:**
- [ ] Given `OverlayKind::Help` is removed, when the workspace compiles, then the exhaustive `policy()` match in `crates/vibe-cli/src/tui/workflow/keys.rs:76` and every construction site named it, so no dead help overlay survives.
- [ ] Given the shortcut section is built, when each line is read, then it names a chord this port binds, verified against `crates/vibe-cli/src/tui/shortcuts.rs`, and the section holds the same eight lines the corpus recorded.
- [ ] Given a help line naming a chord no key handler binds, when the test suite runs, then it fails naming that line, so help can never advertise a key this binary ignores.
- [ ] Given this port binds `Ctrl+D` to quit (`crates/vibe-cli/src/tui/shortcuts.rs:316`) and the reference lists no such chord, when the shortcut section is built, then `Ctrl+D` rides on the quit line rather than adding a ninth, and the difference is a ledger entry rather than an unrecorded one.
- [ ] Given this port binds `Esc Esc` to rewind on an empty prompt (`crates/vibe-cli/src/tui/shortcuts.rs:525`), when the shortcut section is built, then that line is present and states the empty-prompt condition.
- [ ] Given the special-features section is built, when its two lines are read, then one names the shell prefix and one names the path-completion prefix, matching what `crates/vibe-cli/src/tui/submission.rs:32` and the path completer actually accept.
- [ ] Given a shortcut named in the help is pressed in the running TUI, when the key is handled, then it performs the action the help line states, asserted for each of the eight lines.

---

### EP-071: What a submitted command line does

Make a slash command leave the same three traces the reference leaves: a line in
the transcript, one telemetry event under the registry's own name, and a refusal
that says which of the two reasons applied.

**Definition of Done:** every executed command echoes and emits exactly one
event named by its registry key, no refused command emits one, and a busy
runtime and a paused queue give different reasons.

#### US-233: Echo the submitted command line into the transcript
**Description:** As a person running a slash command, I want the command line to appear above its output, so that a scrolled-back transcript says what produced what.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-230
**Reference:** `_handle_command` in `vibe/cli/textual_ui/app.py:1638-1652` for the mount point, the ordering relative to the handler, and the display rule.

**Acceptance Criteria:**
- [ ] Given `/mcp add server` is submitted, when it is dispatched, then a transcript entry carrying `mcp add server` is appended before the handler runs, so the handler's own output follows it.
- [ ] Given a bare alias such as `:q` is submitted, when it is dispatched, then the entry carries the registry key `exit` rather than the alias typed, matching the reference's display rule.
- [ ] Given `/HELP` is submitted, when it is dispatched, then the entry carries the text the operator typed with its leading slash removed, because the reference removes the prefix without recasing.
- [ ] Given a command is refused because the runtime is busy, when it is dispatched, then no entry is appended and the draft is restored to the composer.
- [ ] Given a line that parses to no command, when it is submitted, then no entry is appended and the line is routed as a prompt, a skill or a shell command as before.
- [ ] Given the echoed entry, when the transcript is rendered, then it carries its own entry kind, so a rendered frame can be asserted to hold exactly 1 command echo and 0 user prompts for a submitted slash line.

#### US-234: Report the registry key, once, only when the command runs
**Description:** As whoever reads the telemetry, I want one event per executed command named by its registry key, so that the series is comparable to the reference's and to itself.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-230
**Reference:** `_handle_command` in `vibe/cli/textual_ui/app.py:1638-1644` for the field value and the point at which it is recorded.

**Acceptance Criteria:**
- [ ] Given `/connectors` is submitted, when it is dispatched, then the recorded `command` is `mcp`, and given `/new` it is `clear`, and given `:q` it is `exit`.
- [ ] Given `/HELP` is submitted, when it is dispatched, then the recorded `command` is `help`, because the value comes from the definition rather than from the typed text.
- [ ] Given a command is refused because the runtime is busy or the queue is paused, when it is dispatched, then no event is recorded, because the reference never reaches its record in that path.
- [ ] Given the literal `/exit` is submitted while idle, when it is dispatched, then exactly one event is recorded naming `exit`, which requires the short-circuit in `crates/vibe-cli/src/tui/mod.rs:352` to stop bypassing dispatch or to record before returning.
- [ ] Given any of the 28 commands is submitted through any of its aliases, when it is dispatched, then the recorded `command` equals that command's registry key, asserted over the full alias set rather than a sample.
- [ ] Given a line that parses to no command, when it is submitted, then no builtin event is recorded.

#### US-235: Give the two refusals their two reasons
**Description:** As a person running a slash command, I want the refusal to say whether the runtime is busy or the queue is paused, so that I know whether to wait or to clear.

**Priority:** P1
**Size:** XS (1 pt)
**Dependencies:** Blocked by US-233
**Reference:** the two refusal hints and their two call sites in `vibe/cli/textual_ui/app.py:476-477`, `:1061-1066` and `:1104-1110`, and `_handle_queue_submit` for which input kinds refuse rather than queue.

**Acceptance Criteria:**
- [ ] Given a slash command is submitted while a turn is running, when it is refused, then the message states that the current job must finish.
- [ ] Given a slash command is submitted while the prompt queue is paused, when it is refused, then the message states that the queue must be cleared or the input removed, and is distinct from the busy message.
- [ ] Given a teleport line is submitted in either condition, when it is refused, then it carries the same two reasons, because the reference refuses both kinds through one path.
- [ ] Given either refusal, when it is emitted, then the submitted text is restored to the composer and nothing is queued.
- [ ] Given the port surfaces refusals as diagnostics where the reference raises a warning notification, when the divergence is recorded, then it is a ledger row naming the channel difference rather than an unrecorded one.

---

### EP-072: The completer's edges

Close the two measured edges where the popup answers differently, and give the
chat-input corpus the traces that would have caught them.

**Definition of Done:** `//` returns nothing, a slash line completes with the
cursor anywhere in it, and five new traces replay at `parity`.

#### US-236: Strip one slash, and complete from any cursor offset
**Description:** As a person running a slash command, I want the popup to answer the way the reference answers at the edges of a slash line, so that a typo does not produce a list the reference would not show.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-230
**Reference:** `CommandCompleter` at `/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/completers.py:33`, specifically `_head_word:43`, `get_completion_items:73` and `get_replacement_range:84`.

**Acceptance Criteria:**
- [ ] Given the composer holds `//`, when candidates are ranked, then none are returned, because exactly one leading slash is stripped and the remaining slash matches no alias.
- [ ] Given the composer holds `///`, when candidates are ranked, then none are returned.
- [ ] Given the composer holds `/help` with the cursor at offset 0, when candidates are ranked, then the full available command list is returned with a replacement range of (0,0).
- [ ] Given the composer holds `/mcp add x` with the cursor inside `mcp`, when candidates are ranked, then the query is the text up to the cursor and the replacement range ends at the cursor, not at the first space.
- [ ] Given a cursor offset beyond the text length, when candidates are ranked, then nothing is returned and nothing panics, matching the guard the reference's controller applies.
- [ ] Given the composer holds `/` alone, when candidates are ranked, then the full available command list is returned, unchanged from today.

#### US-237: Cover the slash edges in the chat-input corpus
**Description:** As a person reading the scorecard, I want the widget-level corpus to carry the slash cases it was missing, so that a regression at those edges fails a replay instead of waiting for a re-read.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-236
**Reference:** `SlashCommandController` at `/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/slash_command.py:9`, its `can_handle:16`, `on_text_changed:25` and `on_key:44`, plus `completers.py:73` for the items each trace records. Pattern to copy: the existing scenario dicts in `scripts/parity/scenarios.py:413`.

**Acceptance Criteria:**
- [ ] Given `scripts/parity/scenarios.py`, when the slash scenarios are read, then it declares five new ones: a double slash, a mid-token cursor, a cursor moved to offset 0 on a slash line, an alias typed in uppercase, and a context where clipboard image support is on.
- [ ] Given the corpus is recaptured, when `crates/vibe-cli/tests/parity/manifest.json` is read, then the slash trace count is 20 and every new trace names an existing gap and story.
- [ ] Given the clipboard-support scenario, when the capture runs on a machine without that capability, then it is written to the manifest's `unavailable` list with its missing capability, exactly as `clipboard-explicit-ctrl-v` already is, rather than silently recording a popup missing one row.
- [ ] Given `crates/vibe-cli/tests/parity/expectations.json`, when the new traces are added, then each carries an explicit verdict per dimension and none is left undeclared.
- [ ] Given the full corpus replays, when the run completes, then no previously passing trace has changed verdict and `UNMODELED_STATE_PATHS` is still empty.

---

### EP-073: Restate row 2 from what now measures it

Move the number only after something can reproduce it, and record what this port
answers differently.

**Definition of Done:** row 2 reads 100, carries the command that reproduces it,
and every remaining difference is a ledger row that fails when it closes.

#### US-238: Remeasure and restate parity row 2
**Description:** As a person reading the scorecard, I want row 2 to state a score, the instrument that produced it and the divergences that remain, so that the number can be checked rather than trusted.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-230, US-232, US-234, US-235, US-237
**Reference:** none to read; this story edits this repository's documentation.

**Acceptance Criteria:**
- [ ] Given `docs/parity.md` row 2, when it is read, then it states 100, names the corpus and the replay module, and carries the command that reproduces the measurement.
- [ ] Given row 2's Reference column, when it is read, then it names `vibe/cli/autocompletion/completers.py`, `vibe/cli/textual_ui/widgets/chat_input/input_kinds.py` and `vibe/cli/textual_ui/app.py` alongside the two files it names today, because all three publish row-2 behavior.
- [ ] Given row 2's note, when it is read, then it states 35 aliases rather than 31, distinguishing the 31 slash-prefixed from the 4 bare ones, correcting the count the row carries today.
- [ ] Given the accepted-divergence table, when it is read, then it holds one row for the help document's prose split, one for the `Ctrl+D` shortcut this port binds and the reference does not, and one for the refusal channel, each naming the artifact that fails if the divergence silently closes.
- [ ] Given the header's weighted total, when it is recomputed, then it accounts for row 2 moving 98 to 100 at its declared weight and the arithmetic is shown, as every previous restatement shows it.
- [ ] Given `CHANGELOG.md`, when the `## Unreleased` section is read, then it records the user-visible changes: the help output, the command echo and the telemetry name.
- [ ] Given the score against upstream `HEAD`, when the drift section is read, then it states that `vibe/cli/commands.py` has already moved at `HEAD` and that row 2's second column does not close with this work.

## Functional Requirements

- FR-01: A capture script must record the pinned `CommandRegistry`'s alias inventory, parse results, availability sets and help-document structure into a committed corpus, and must record reference-authored prose only as a length and a SHA-256.
- FR-02: A replay module must compare that corpus against this port on every `cargo test` run, must fail on a divergence no ledger entry records, and must fail on a ledger entry whose divergence has stopped reproducing.
- FR-03: The replay must fail when its comparison count falls below 240.
- FR-04: `/help` must append a transcript entry and must not open an overlay.
- FR-05: The help document must carry three sections in the reference's order, with the reference's line counts, and its command section must be sorted by registry key with every alias of each command listed, canonical `/name` first.
- FR-06: The help document must omit every unavailable command and must remain well-formed when no command is available.
- FR-07: The system must NOT ship the reference's help lines; it must write its own and a test must hold the two permanently unequal.
- FR-08: Every dispatched command must append a transcript entry carrying the reference's display value before its handler runs.
- FR-09: Exactly one telemetry event must be recorded per executed command, naming the registry key, and none must be recorded for a refused command or a line that parses to no command.
- FR-10: Alias resolution must lowercase the input the way the reference lowercases it, so a character whose Unicode lowercase is an ASCII letter resolves.
- FR-11: Slash completion must strip exactly one leading slash and must return candidates for a slash line at any cursor offset within it, including 0.
- FR-12: A refused slash command must state which of the two conditions refused it and must restore the submitted text to the composer.
- FR-13: `OverlayKind::Help` must not exist after EP-070.

## Non-Functional Requirements

- **Performance:** the registry replay must complete in under 2 seconds on the CI runner, because it compares committed data and starts no process.
- **Performance:** the help document is built exactly once per `/help` invocation and rebuilt 0 times per frame, so drawing a transcript holding it costs what drawing an assistant message of equal length costs.
- **Correctness:** the help document must render at terminal width 80 with 0 truncated command lines: a line longer than the width wraps.
- **Security:** the capture must make 0 network requests, enforced by a socket guard that fails the run on the first attempt.
- **Licensing:** the committed corpus must hold 0 bytes of reference-authored prose in cleartext, and the inequality guard must cover 100% of this port's help lines.
- **Coverage:** the registry corpus must replay at least 240 comparisons and the parse family must carry at least 40 probe inputs; the chat-input corpus must carry 20 slash traces.
- **Reliability:** a missing or off-pin reference checkout must fail 0 tests: the replay runs unconditionally and only the recapture probe skips.
- **Determinism:** two runs of the capture against the same pinned tree must produce byte-identical corpora.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Empty state | Every command excluded from the registry | The help document keeps its three headings and an empty command section | the three sections, no command lines |
| 2 | Empty state | `/` typed with no skills installed | The popup lists the available builtins only, unchanged | none |
| 3 | Boundary value | Input is empty or whitespace only | No command parses, no event, no entry | none |
| 4 | Boundary value | `//` and `///` | No candidates, popup stays closed | none |
| 5 | Boundary value | Cursor at offset 0 on a slash line | Full list with replacement range (0,0) | none |
| 6 | Boundary value | Cursor beyond the text length | No candidates and no panic | none |
| 7 | Boundary value | A very long argument after a slash alias | The alias resolves, the argument is preserved whole, the echo wraps | the echoed line, wrapped |
| 8 | Malformed input | Bare alias followed by arguments, such as `exit now` | No command parses; the line is sent as a prompt | none |
| 9 | Malformed input | Alias spelled with a non-ASCII character folding to ASCII | Resolves to the same key the reference resolves | the command runs |
| 10 | Interrupted flow | Slash command submitted while a turn is running | Refused, draft restored, no event, no entry | the busy reason |
| 11 | Interrupted flow | Slash command submitted while the queue is paused | Refused, draft restored, no event, no entry | the paused reason |
| 12 | Permission change | `vibe_code_enabled` flips on reload | The registry refreshes; the popup, the parser and the help all change together | none |
| 13 | Platform difference | `/paste-image` off macOS | Absent from the popup, from the help and from the parser | none |
| 14 | Ambiguity | A user skill named like a builtin command | The builtin wins, as it does upstream | the builtin runs |
| 15 | External dependency | Reference checkout absent or off-pin | The corpus still replays; only the recapture probe skips, naming the reason | the skip reason on stderr |
| 16 | Version and compatibility | The committed corpus names a commit other than the pin | The replay fails on load rather than comparing across revisions | the two commits |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | The help document's prose is reference-authored, and reproducing its structure invites reproducing its words | High | High | The corpus records every reference line as a length plus a digest, the replay compares structure only, and a guard asserts permanent inequality with this port's lines. The precedent is the builtin skill prose row already in the scorecard |
| 2 | Retiring the help overlay breaks another surface that shares the `Overlay` type | Low | Med | `policy()` in `crates/vibe-cli/src/tui/workflow/keys.rs:76` is an exhaustive match over every kind, so removal is a compile error at each site rather than a silent behavior change |
| 3 | Upstream `HEAD` has already changed `commands.py`, so this work closes the pinned column and not the product column | High | Med | Stated in US-238's criteria and in the drift section rather than hidden. The second column closes on a re-pin, which is a separate PRD priced at regenerating every corpus |
| 4 | Recapturing the chat-input corpus needs Textual and a reference virtualenv, and the local checkout is off-pin | Med | Med | US-229 proves the `git archive` extraction path first; US-237 either reuses it or restores the pin with `vibe_core::parity::RESTORE_COMMAND`. Either way the committed traces keep replaying while the capture is unavailable |
| 5 | Changing the telemetry name breaks continuity of an existing series | Med | Low | The series has no published consumer and today emits one command under three names, so continuity is already absent. US-238 records the change in `CHANGELOG.md` |
| 6 | Lowercasing alias resolution the Unicode way changes matching for inputs nobody types | Low | Low | The corpus records the exact inputs, so the change is measured rather than assumed, and the cost is one comparison per alias on a 28-entry table |
| 7 | The comparison floor is set from an estimate and could pass a corpus that shrank | Med | Med | The floor is asserted against the count the first real capture produces, raised to it if the capture exceeds 240, so it is a measurement rather than a guess after US-229 lands |

## Non-Goals

Explicit boundaries: what this version does NOT include.

- **Re-pinning the reference to v2.24.2.** `AGENTS.md` requires regenerating every committed corpus in the same change, which would pull in every other row. That is a separate PRD, and it is what the second scorecard column waits on.
- **The command handlers themselves.** `/status`, `/whoami`, `/mcp`, `/model` and the rest publish their own contracts measured by their own rows. This PRD covers what resolves a command line, what it echoes, what it reports and what `/help` prints, not what any handler does afterward. The unrouted `identity/read` behind `/whoami` belongs to the app-server row.
- **The `/retry` prompt text.** Its prose is deliberately this port's own under `NOTICE`, its warning tag and its three directives already match, and upstream `HEAD` has moved the builder out of the module entirely. Nothing here changes it.
- **A toast or notification surface.** The reference refuses a queued command through a Textual warning notification; this port has no notification concept and building one to match a two-line message is not proportionate. The channel becomes a ledger row instead.
- **The `exits` field on the reference's `Command`.** It is dead code upstream, read by nothing. Omitting it is correct and stays omitted.
- **`excluded_commands` as a configurable surface.** The reference's CLI never populates it either. The port keeps the parameter, and no story exposes it.
- **Skill resolution and skill ranking.** They join the same popup and are measured by the skills row and by GAP-09's existing traces.

## Files NOT to Modify

- `/home/arthur/dev/mistral-vibe` and every path inside it: the behavioral oracle is read-only, on every platform and under every `VIBE_REFERENCE` value.
- `crates/vibe-core/src/parity.rs` and `scripts/parity/pin.py`: the two pin sources. Re-pinning is out of scope and moving one without the other fails `the_python_mirror_agrees_with_the_rust_pin`.
- Every committed corpus other than `crates/vibe-cli/tests/parity/` and the new `crates/vibe-cli/tests/commands/`: regenerating a corpus this PRD does not measure would hide a change it did not make.
- The `description` field of every entry in `COMMANDS` (`crates/vibe-cli/src/tui/commands.rs`): those strings are the observable contract the popup traces already assert byte for byte, not prose this port may rewrite.
- `crates/vibe-cli/src/tui/completion/fuzzy.rs`: the ranking port is already measured at `parity` and no story here touches scoring.
- `NOTICE`: the licensing boundary this PRD works within rather than adjusts.

## Technical Considerations

- **Architecture:** the capture's interpreter, recommended: extract the pinned tree with `git archive` the way `scripts/parity/experiments.py` does and run under the ambient Python, because `CommandRegistry` imports one constant and needs neither Textual nor pydantic. Engineering to confirm on first run whether `vibe/utils` pulls a dependency at import; if it does, fall back to `vibe_core::parity::pinned_interpreter`, which every other oracle already uses and which changes no criterion.
- **Architecture:** the replay's home, recommended: `crates/vibe-cli/src/tui/commands_parity_tests.rs` beside `commands.rs`, following the repository rule that differential tests live next to the code they cover. It stays in `vibe-cli` because the registry does, so no layering question arises.
- **Data Model:** the help document's representation. Option A: a function returning `String` that the transcript renders as Markdown, mirroring the reference's own shape. Option B: a structured document type the renderer walks, which would let the replay assert structure without parsing. Trade-off: A is smaller and reuses the existing Markdown path unchanged; B makes US-231's structural assertions direct instead of derived from a re-parse. A is recommended, with the structural fields the corpus needs derived in the test rather than modeled in production.
- **Data Model:** the telemetry key. `CommandDefinition.name` (`crates/vibe-cli/src/tui/commands.rs:130`) already holds the registry key and `ParsedCommand` already carries `id`, so the fix threads an existing value rather than adding one. No record shape changes; only the value written to `command` does.
- **API Design:** the echoed entry's kind. Recommended: a distinct transcript kind rather than reusing the local-notice kind, so the renderer can style it as the reference styles its `SlashCommandMessage` and so a future story can attach handler output to it, which is what upstream does by passing the message into the handler. Engineering to confirm the persistence format tolerates a new kind.
- **Dependencies:** none. The capture uses the standard library, the replay uses the JSON support already in the test tree, and the Markdown renderer already exists.
- **Migration:** none. No persisted format changes, no configuration key is added, and the only stored artifact touched is the transcript, which gains entries it did not have.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Parity row 2 score against the pin | 98, from an inventory diff | 100, from a replay | Month-1 | `docs/parity.md` row 2 after US-238 |
| Registry comparisons replayed | 0 | at least 240 | Month-1 | the floor assertion in `crates/vibe-cli/src/tui/commands_parity_tests.rs` |
| Parse probe inputs in the corpus | 0 | at least 40 | Month-1 | the parse family in `crates/vibe-cli/tests/commands/corpus.json` |
| Slash traces in the chat-input corpus | 15 | 20 | Month-1 | `crates/vibe-cli/tests/parity/manifest.json` |
| Aliases reachable from `/help` output | 28 of 35 | 35 of 35 | Month-1 | the help family in the registry corpus |
| Telemetry names per command | up to 3 | 1 | Month-1 | the parse and telemetry criteria of US-234 |
| Row-2 divergences with no ledger entry | 3 (help prose, `Ctrl+D`, refusal channel) | 0 | Month-1 | the accepted-divergence table in `docs/parity.md` |
| Corpus bytes of reference prose in cleartext | not applicable, no corpus | 0 | Month-1 | the digest-inequality guard in US-230 |

## Open Questions

- **Does the help entry belong to the session transcript that persists, or to the local-only entries?** The reference mounts a widget with no persistence question because its transcript is not reloaded the same way. Recommended: persist it like any local notice, so a reloaded session still shows what the operator asked for. Maintainer to decide before US-231; the answer changes one field and no other criterion.
- **Should `/help` also remain reachable as an overlay behind a chord?** US-232 removes the overlay outright, which is the parity answer. Retaining both would be a Rust-only surface with no reference counterpart and a second place for the content to drift. Maintainer to decide before US-232; the default is removal.
- **Is the comparison floor 240 or the count the first capture produces?** The number is an estimate from the families this PRD specifies. Recommended: land US-229, read the real count, and set the floor to it. Answered by running the capture once, and it blocks nothing.
- **Does the port want the reference's Unicode lowercasing or its own ASCII rule recorded as a divergence?** Matching is one line and closes a measured difference, which is why US-230 requires it. The alternative is a ledger row stating that alias resolution here is ASCII-only. Recommended: match, because the cost is a lowercase over a 28-entry table once per submitted line.
[/PRD]
