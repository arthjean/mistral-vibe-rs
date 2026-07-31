[PRD]
# PRD: Chat Input Observable Parity

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-07-31 | Arthur Jean | Initial draft based on the Python to Rust parity audit |
| 1.1 | 2026-08-01 | Arthur Jean | Track latest stable Rust and dependencies instead of pinning 1.85 / Ratatui 0.29 / Crossterm 0.28; quality gates run on `stable` |

## Problem Statement

1. The Rust TUI chat input is not behaviorally interchangeable with the Python reference. The audit identified 21 observable gaps across command discovery, completion, history, editing, input modes, paste and attachment handling, voice, feedback, safety states, and long-prompt rendering.
2. These gaps alter user intent. Examples include `/c` resolving to clear instead of config, Up/Down recalling history while the cursor is inside a multiline prompt, text mentions becoming embedded resources, and prompts beyond 64 KiB becoming visually truncated.
3. The port has no executable parity contract. Similar internal components can still diverge because key events, async results, state transitions, effects, and rendered output are not compared against the Python implementation as a black-box oracle.
4. Maintainers cannot safely close the gaps one by one without a deterministic state model and differential fixtures that prevent a correction on one platform or input mode from regressing another.

**Why now:** The repository states that full feature parity is the first objective before adding capabilities. The Rust TUI already exposes the chat composer as a production surface, so every new behavior built on the current gaps increases migration cost and user-visible inconsistency.

## Overview

This initiative makes the Rust chat input observably equivalent to the Python reference for the audited surface. Parity is defined as equivalent state, effects, submissions, notifications, and rendered terminal output for the same initial conditions and normalized event sequence. Python remains the behavioral oracle. Rust structure and idioms may differ.

Implementation will center the existing `PromptEditor`, completion engine, command registry, clipboard port, event loop, and renderer around a deterministic `ChatInputState + InputEvent -> InputEffect` transition boundary inside `vibe-cli`. Operating-system and asynchronous work remains behind effects and adapters. No new cross-crate protocol, app-server capability, or dependency is assumed. Voice is gated by a validation story because its existing public boundary is the only high-risk architectural assumption.

The release gate is a differential fixture suite plus targeted reducer, snapshot, and PTY tests. Completion means zero unexplained mismatches across the canonical corpus, including Unicode, wrapped multiline prompts, async completion races, paste control sequences, and terminal teardown paths.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Eliminate audited behavioral gaps | 21 gaps encoded as failing or pending fixtures, at least 12 passing in Rust | 21 of 21 passing with zero accepted divergence |
| Establish an executable parity contract | At least 60 canonical event traces with stable schema | At least 120 traces across Linux, macOS adapters, Windows adapters, and terminal widths |
| Prevent input regressions | Differential, reducer, snapshot, and PTY suites enforced on touched epics | Zero parity regression escaping the canonical suite for 90 consecutive days |
| Preserve interaction latency | No synchronous OS work on the input path | P99 non-I/O transition under 1 ms and P95 frame render under 50 ms for a 1 MiB prompt |

## Target Users

### Terminal-native Vibe operator

- **Role:** Developer using Vibe interactively in a terminal, including users moving between the Python and Rust clients.
- **Behaviors:** Writes multiline prompts, uses slash commands and `@` paths, recalls prompt history, pastes terminal content, attaches images, and relies on keyboard-first control.
- **Pain points:** The same key sequence or prompt text can submit different content, select a different command, lose visible content, or produce a different attachment depending on the implementation.
- **Current workaround:** Memorize Rust-specific exceptions, avoid multiline history navigation, avoid external mentions and clipboard images, or return to the Python client.
- **Success looks like:** Existing Python muscle memory and workflows produce the same observable result in Rust without implementation-specific exceptions.

### Rust port maintainer

- **Role:** Engineer implementing or reviewing the TUI port.
- **Behaviors:** Changes event handling, completion, rendering, workflow adapters, and platform-specific clipboard behavior.
- **Pain points:** Behavior is distributed across the event loop and helpers, while tests prove isolated mechanics rather than parity against the reference.
- **Current workaround:** Read both implementations manually and add local tests that may omit state sequences or rendering consequences.
- **Success looks like:** A failed differential trace identifies the first mismatching event, state field, effect, and render snapshot before a change is merged.

## Research Findings

Key findings that informed this PRD:

### Competitive Context

- **Python Textual reference:** The focused widget owns multiline editing, selection, paste normalization, history interception, completion control, feedback, and voice key handling. Parent widgets add safety, switching, and recording presentation. Primary anchors: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/chat_input/text_area.py:32`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/chat_input/body.py:40`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/chat_input/container.py:19`.
- **Current Rust port:** Editing and completion state live in `crates/vibe-cli/src/tui/input.rs:22`, event interpretation in `crates/vibe-cli/src/tui/mod.rs:720`, presentation in `crates/vibe-cli/src/tui/render.rs:73`, commands in `crates/vibe-cli/src/tui/commands.rs:64`, and clipboard effects in `crates/vibe-cli/src/tui/clipboard.rs:26`.
- **Observed gap clusters:** Python persists 100 prompt-history entries while Rust keeps an in-memory vector (`/home/arthur/dev/mistral-vibe/vibe/cli/history_manager.py:14`, `crates/vibe-cli/src/tui/input.rs:34`); Python recognizes four modes while Rust visually promotes slash only (`/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/chat_input/text_area.py:32`, `crates/vibe-cli/src/tui/render.rs:73`); Rust caps paste, scan, and completion candidates (`crates/vibe-cli/src/tui/input.rs:22`) where the reference behavior differs; Rust command availability is only platform-gated for image paste (`crates/vibe-cli/src/tui/commands.rs:230`).
- **Market gap:** The relevant differentiator is not another terminal editor implementation. It is a port whose behavior is provably interchangeable with its reference across event sequences and platforms.

### Best Practices Applied

- Model parity at the event, state, effect, and render boundary. Textual resolves bindings through the focused widget and ancestors, while Ratatui only renders application-owned state. Sources: [Textual input and bindings](https://textual.textualize.io/guide/input/), [Textual TextArea](https://textual.textualize.io/widgets/text_area/), [Ratatui user-input example](https://ratatui.rs/examples/apps/user_input/).
- Treat bracketed paste as an atomic event and filter key event kinds so pasted control characters and release events cannot execute shortcuts. Source: [Crossterm event API](https://docs.rs/crossterm/latest/crossterm/event/).
- Keep asynchronous completion generation-aware and apply results only through the event loop. The current Rust engine already rejects stale generations at `crates/vibe-cli/src/tui/input.rs:483`; this behavior becomes part of the explicit contract.
- Separate grapheme positions from terminal cell width. The current editor is grapheme-aware at `crates/vibe-cli/src/tui/input.rs:225`; parity fixtures must additionally cover wide characters, combining marks, wrapped visual lines, and mouse coordinates.
- Use deterministic widget buffers and PTY tests for presentation and terminal lifecycle. Sources: [Ratatui snapshot testing](https://ratatui.rs/recipes/testing/snapshots/), [Textual Pilot](https://textual.textualize.io/api/pilot/).

*Full research is represented by the source links and code anchors above. The Python repository is a read-only oracle, not an implementation target.*

## Assumptions & Constraints

### Assumptions (to validate)

- The Python behavior at the pinned source revision is authoritative even where Rust currently exposes additional commands or stricter limits.
- Equivalent terminal output means the same information, selection, cursor target, status, and visibility at a fixed viewport. Byte-identical ANSI output is not required.
- Existing `vibe-cli` public boundaries can support voice capture and transcription without modifying `vibe-app-server`, `vibe-protocol`, or `vibe-core`. This is high risk and must be validated by US-015 before implementation.
- Python history JSONL can be reproduced without importing unrelated Python storage internals. Corrupt records may be skipped with diagnostics if valid order and the latest 100 entries are preserved.
- Platform-specific clipboard behavior can be verified through injected ports on Linux CI and one native macOS validation before release.

### Hard Constraints

- Observable behavior, not internal structure, defines parity.
- The Python repository under `/home/arthur/dev/mistral-vibe` is read-only.
- Changes are restricted to the `vibe-cli` crate and focused tests unless a separately approved PRD changes dependency layering.
- Edition 2024, the lint policy, and workspace dependency rules remain in force. The toolchain and dependency set track latest stable (see Changelog 1.1); a downgrade, not an upgrade, is what needs a separate decision.
- Input processing must not block the terminal event loop on filesystem scans, clipboard subprocesses, image conversion, microphone capture, or transcription.
- No new dependency is introduced by default. A dependency requires a separate decision with concrete parity evidence.

## Quality Gates

These commands must pass for every user story:

- `cargo +stable fmt --all -- --check` - verify workspace formatting
- `cargo +stable check --workspace --all-targets --all-features` - verify all targets and feature combinations compile
- `cargo +stable clippy --workspace --all-targets --all-features -- -D warnings` - enforce the repository lint policy with zero warnings
- `cargo +stable test --workspace --all-features` - run the workspace test suite
- Targeted Ratatui `TestBackend` snapshots at 40, 80, and 120 columns - verify composer, completion, state chrome, cursor, and long-prompt viewport output
- Targeted PTY scenarios on Linux plus injected macOS and Windows adapter tests - verify key event kinds, bracketed paste, resize, focus, and terminal-mode teardown

## Epics & User Stories

### EP-001: Observable Parity Contract

Create the executable oracle and deterministic Rust boundary required to measure every later correction.

**Definition of Done:** A versioned fixture corpus generated from the pinned Python reference can be replayed against Rust, and every mismatch reports the first divergent event, state field, effect, or render observation.

#### US-001: Capture Python oracle traces

**Description:** As a port maintainer, I want canonical traces from the Python reference so that parity is defined by reproducible observations instead of interpretation.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**

- [ ] Given the 21 audited gaps, when the oracle corpus is generated, then every gap maps to at least one versioned trace containing initial state, normalized events, state observations, effects, and render assertions.
- [ ] Given timestamps, temporary paths, or platform-specific key names, when a trace is serialized, then configured nondeterministic fields are normalized without removing user-visible content.
- [ ] Given an unavailable Python revision, missing capability, or incompatible trace schema, when generation runs, then it fails with the missing revision or field and writes no partial fixture as authoritative.

#### US-002: Introduce a deterministic chat-input transition boundary

**Description:** As a port maintainer, I want input decisions expressed as deterministic state transitions and explicit effects so that equivalent event sequences can be replayed without a live terminal or operating-system service.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**

- [ ] Given a `ChatInputState` and normalized `InputEvent`, when the transition function runs, then its next state and ordered effects are deterministic and serializable for differential comparison.
- [ ] Given filesystem, clipboard, editor, microphone, transcription, or timer work, when the transition requests it, then the work is represented as an effect and no blocking call occurs inside the transition.
- [ ] Given an invalid cursor, selection, stale generation, or effect response, when it is reduced, then state invariants remain valid and the event is rejected or diagnosed without panic.

#### US-003: Build the differential conformance runner

**Description:** As a reviewer, I want Python traces replayed against Rust so that a parity claim has an automated pass or an actionable mismatch.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001, US-002

**Acceptance Criteria:**

- [ ] Given a canonical trace, when Rust replay completes, then normalized state, ordered effects, submission payload, notifications, and render assertions are compared at every observation point.
- [ ] Given a mismatch, when the runner fails, then it reports the trace ID, first divergent event, expected value, actual value, and source fixture revision.
- [ ] Given an unknown field, skipped event, missing adapter response, or schema-version mismatch, when replay starts, then the trace fails explicitly instead of being treated as passing.

---

### EP-002: Editing, Modes, and Prompt History

Restore Python-compatible keyboard, mouse, multiline, mode, submission, and history behavior while preserving Unicode correctness.

**Definition of Done:** Canonical traces for prompt editing, four input modes, submission normalization, external editing, multiline navigation, and persisted history pass with cursor and selection parity.

#### US-004: Restore input modes and submission semantics

**Description:** As a terminal operator, I want `>`, `!`, `/`, and `&` to behave like the Python composer so that prompt, shell, command, and teleport intent is preserved.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**

- [ ] Given an empty composer, when its first character is `>`, `!`, `/`, or an available `&`, then the mode, prefix rendering, completion source, backspace behavior, and submitted input kind match Python.
- [ ] Given leading or trailing whitespace around a non-empty submission, when Enter submits, then the emitted text is stripped exactly as in Python and an all-whitespace prompt emits no turn.
- [ ] Given `&` when teleport is unavailable or a mode prefix removed back to the default state, when the user continues editing, then no unavailable action runs and the remaining text is preserved.

#### US-005: Restore cursor, selection, word, mouse, and external-editor behavior

**Description:** As a terminal operator, I want Python-compatible cursor and selection controls so that editing the same Unicode prompt produces the same text and caret position.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**

- [ ] Given ASCII, combining marks, emoji sequences, and double-width characters, when character, word, line, Home, End, Shift-selection, deletion, and mouse-selection actions run, then text, grapheme selection, and visible cursor match the oracle.
- [ ] Given external-editor output different from the input, when the effect returns, then content, cursor, selection, history-navigation state, and completion state match Python.
- [ ] Given unchanged external-editor output, an out-of-bounds mouse coordinate, or unsupported mouse capture, when the result is handled, then the prompt and cursor are not reset and no panic or synthetic selection occurs.

#### US-006: Persist bounded compatible prompt history

**Description:** As a terminal operator, I want prompt history to survive restarts so that Up/Down recalls the same latest entries as the Python client.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**

- [ ] Given successful non-empty submissions, when history is persisted, then `VIBE_HOME/vibehistory` contains JSONL compatible records with consecutive duplicates suppressed and only the latest 100 entries retained.
- [ ] Given a restart, when the composer initializes, then valid entries are loaded in reference order and navigation begins from the current draft.
- [ ] Given missing permissions, corrupt lines, interrupted replacement, or concurrent writers, when history is read or written, then valid existing entries remain recoverable, prompt submission still succeeds, and a diagnostic contains no prompt content.

#### US-007: Match visual-line history navigation and draft restoration

**Description:** As a terminal operator, I want Up/Down to move inside multiline text before recalling history so that editing and history navigation do not conflict.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-005, US-006

**Acceptance Criteria:**

- [ ] Given hard or visually wrapped multiline text, when Up/Down can move to another visual line, then the cursor moves within the prompt and history is not recalled.
- [ ] Given the cursor at a reference history boundary, when Up/Down recalls entries, then cursor placement, draft restoration, boundary clamping, and edit-after-recall reset behavior match Python.
- [ ] Given history recall or an edit to a recalled entry, when the state changes, then completion stays closed until a subsequent qualifying user edit and stale history indices cannot overwrite the current draft.

---

### EP-003: Commands and Completions

Make command discovery, slash ranking, skill metadata, path search, popup control, and async completion state match the reference.

**Definition of Done:** The command and path candidate sets, order, descriptions, availability, acceptance, dismissal, and stale-result behavior match Python for the canonical workspace fixtures.

#### US-008: Align the command registry and availability rules

**Description:** As a terminal operator, I want the same command surface in both clients so that aliases never execute a different action or expose unavailable functionality.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**

- [ ] Given every Python command and alias, when commands are listed or parsed, then Rust matches canonical name, aliases, description, argument handling, and `/continue` semantics.
- [ ] Given capability, configuration, platform, and exclusion states, when command availability is evaluated, then Vibe Code commands, paste-image, and excluded commands are shown or hidden exactly as in Python.
- [ ] Given Rust-only aliases or commands such as `/close`, `/quit`, `/title`, `/approve`, `/deny`, `/fork`, `/history`, `/setup`, `/settings`, `/trust`, or `/update`, when they are absent from the pinned Python oracle, then they are not discoverable or executable through the parity surface.

#### US-009: Align slash ranking, skill metadata, acceptance, and dismissal

**Description:** As a terminal operator, I want slash suggestions ordered and controlled like Python so that abbreviated commands remain predictable.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-008

**Acceptance Criteria:**

- [ ] Given `/`, `/c`, and other partial queries, when suggestions appear, then priority boosts, fuzzy ranking, candidate count, descriptions, and skill descriptions match Python, including `/c` selecting config rather than clear.
- [ ] Given an open slash popup, when Up, Down, Tab, Enter, Right, Escape, or an unrelated editing key is pressed, then selection, acceptance, submission, and dismissal match the oracle.
- [ ] Given more than 64 matching commands and skills, duplicate aliases, or a selected item removed by a refresh, when the popup updates, then the reference-visible candidate set is preserved, selection is valid, and no index panic occurs.

#### US-010: Align path triggers, search semantics, and indexed corpus

**Description:** As a terminal operator, I want `@` path completion to search the same workspace corpus from the same cursor context so that referenced paths are predictable.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**

- [ ] Given the last active `@` anywhere before the cursor, including after punctuation and within a partial nested path, when completion runs, then trigger range, query, global matching, ranking, and insertion match Python.
- [ ] Given `.gitignore`, Python default ignores, hidden files, symlinks, nested directories, and up to 32,000 indexed entries, when a workspace is scanned, then the candidate corpus and deterministic order match the oracle.
- [ ] Given unreadable directories, symlink cycles, paths disappearing during scan, or a corpus above the reference bound, when indexing runs, then the UI remains responsive, traversal terminates, and the resulting diagnostic or truncation matches the fixture.

#### US-011: Preserve async completion state and popup rendering

**Description:** As a terminal operator, I want completion results and popup presentation tied to my current edit so that late work cannot replace newer intent.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-009, US-010

**Acceptance Criteria:**

- [ ] Given overlapping completion generations, when results arrive out of order, then only the current generation and token range can update candidates, selection, or rendering.
- [ ] Given command, path, skill, empty, loading, and dismissed states at 40, 80, and 120 columns, when the popup renders, then item count, maximum height, descriptions, selection, clipping, and cursor-relative placement match Python.
- [ ] Given worker startup failure, scan failure, history recall, secret input, or cancellation, when completion refreshes, then no stale popup appears, the prompt remains editable, and one bounded diagnostic is emitted where the oracle exposes one.

---

### EP-004: Paste, Mentions, and Images

Match atomic text paste, drag-and-drop normalization, mention submission, attachment conversion, and macOS clipboard-image behavior without executing pasted control characters.

**Definition of Done:** Text and image paste traces produce the same prompt text, attachments, notifications, validation failures, temporary-file lifecycle, and submission payload as Python.

#### US-012: Align text paste and drag-and-drop normalization

**Description:** As a terminal operator, I want pasted content inserted as one edit with Python-compatible path normalization so that paste cannot trigger shortcuts or silently change content.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**

- [ ] Given bracketed paste containing newlines, tabs, escape bytes, control-like text, or shortcut characters, when it is received, then it is inserted atomically and no contained character dispatches a key binding.
- [ ] Given a single pasted absolute or terminal-escaped image path, including an external path or an embedded path inside a recognized drag-and-drop form, when normalization runs, then Python-compatible `@` rewriting and spacing are applied.
- [ ] Given empty paste, invalid UTF-8 from a clipboard adapter, an unrecognized path, or content at and above the reference boundary, when paste runs, then existing text is preserved and the result or warning matches the oracle without a Rust-only 256 KiB rejection.

#### US-013: Align mention and attachment submission semantics

**Description:** As a terminal operator, I want `@` references converted exactly like Python so that text files, directories, binaries, external paths, and images keep their intended meaning.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-012

**Acceptance Criteria:**

- [ ] Given mentions of text files, directories, non-image binaries, missing paths, and external paths, when a prompt is submitted, then they remain prompt text with the same normalization as Python and are not rejected or injected as text resources.
- [ ] Given a supported native image mention, when the active model accepts images and validation succeeds, then only that image becomes an attachment and the visible prompt text and request payload match the oracle.
- [ ] Given an unsupported image type, oversized image, unreadable image, unsupported model, or path that changes during submission, when conversion runs, then no partial turn is sent, the prompt remains recoverable, and the user receives the reference-equivalent error.

#### US-014: Align clipboard-image behavior and lifecycle

**Description:** As a macOS terminal operator, I want image paste triggers and feedback to match Python so that clipboard images enter the prompt safely and predictably.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-012, US-013

**Acceptance Criteria:**

- [ ] Given macOS with a PNG or TIFF clipboard image, when Command+V, Ctrl+V, an empty bracketed paste, or `/paste-image` triggers the capability, then capture completes off the UI path within 5 seconds and inserts the temporary absolute `@` path with reference spacing and notification.
- [ ] Given image size, model capability, temporary-file permissions, and cleanup, when capture succeeds or the prompt is discarded, then the 10 MiB guard, private file mode, attachment conversion, and unused-file cleanup match Python.
- [ ] Given unsupported OS, denied clipboard access, text-only or empty clipboard, subprocess timeout, invalid image bytes, or insertion failure, when paste is explicit or implicit, then warning versus silence matches Python and no orphan file or blocked event loop remains.

---

### EP-005: Voice, Feedback, Visual States, and Release Validation

Close the remaining interaction and rendering gaps, then enforce the cross-platform release gate.

**Definition of Done:** Voice, feedback, safety, switching, and long-prompt traces pass, and the complete canonical corpus has zero unexplained mismatch across reducer, snapshot, adapter, and PTY coverage.

#### US-015: Validate the existing voice boundary

**Description:** As a port maintainer, I want proof that voice parity fits existing crate boundaries so that implementation does not create an unplanned protocol or server dependency.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**

- [ ] Given the Python voice state machine and Rust public ports, when the spike maps start, stop, cancel, transcription, device, error, and transcript effects, then each required observation has a concrete `vibe-cli` integration point.
- [ ] Given at least one mocked recording-to-transcript trace, when it is replayed through the proposed boundary, then no modification to `vibe-core`, `vibe-app-server`, or `vibe-protocol` is required.
- [ ] Given a missing public capability or a required new dependency, when the spike concludes, then US-016 remains blocked and a separate decision document names the exact missing boundary instead of shipping a fake or partial voice path.

#### US-016: Implement voice input state and effects

**Description:** As a voice-enabled terminal operator, I want recording controls and transcript insertion to match Python so that speech is another recoverable composer input path.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002, US-015

**Acceptance Criteria:**

- [ ] Given voice is enabled and idle, when Ctrl+R is pressed, then recording starts and the composer presents the same recording state and instructions as Python.
- [ ] Given recording or transcription is active, when a normal key, Ctrl+C, stop completion, or transcript result occurs, then stop, cancel, key consumption, transcript insertion, spacing, and state reset match the oracle.
- [ ] Given no device, denied permission, empty transcript, timeout, transcription failure, or cancellation race, when the effect resolves, then the prompt remains recoverable, no late transcript is inserted, and reference-equivalent feedback is shown.

#### US-017: Align feedback, safety, and model-switching states

**Description:** As a terminal operator, I want state-specific shortcuts and chrome to match Python so that feedback and safety context are visible and model transitions cannot submit invalid work.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**

- [ ] Given feedback is active, when `1`, `2`, `3`, `0`, Escape, or another printable character is pressed, then rating, dismissal, reinsertion, and event consumption match Python.
- [ ] Given safe, approval-required, and unsafe contexts, when the composer renders, then border color, label, and fallback text expose the same state without relying on color alone.
- [ ] Given model switching is active, when the composer renders or Enter is pressed, then the spinner and switching label are shown, submission is blocked, and existing prompt text is preserved until the state resolves.
- [ ] Given feedback persistence or model switching fails, when the failure returns, then the composer exits the transient state, preserves input, and emits one actionable diagnostic without duplicating a turn.

#### US-018: Render long prompts without data loss

**Description:** As a terminal operator, I want the composer viewport to follow the cursor for long prompts so that every editable grapheme remains reachable and visible.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-002, US-005

**Acceptance Criteria:**

- [ ] Given prompts from 64 KiB through 1 MiB, when editing and rendering at 40, 80, and 120 columns, then no text is discarded, the viewport contains the cursor, and movement can reach both ends.
- [ ] Given multiline text with tabs, combining marks, emoji, double-width cells, and resize events, when the viewport changes, then cursor cell coordinates and selection rendering match the oracle.
- [ ] Given a terminal too small for the normal composer or a cursor after a very long unbroken grapheme sequence, when rendering occurs, then frame work remains bounded, no panic occurs, and a valid one-cell cursor target is retained when any input cell exists.

#### US-019: Enforce the cross-platform parity release gate

**Description:** As a release owner, I want all canonical traces and platform lifecycle scenarios enforced together so that parity cannot be declared from isolated unit tests.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-003, US-004, US-005, US-006, US-007, US-008, US-009, US-010, US-011, US-012, US-013, US-014, US-015, US-016, US-017, US-018

**Acceptance Criteria:**

- [ ] Given the pinned corpus, when the release suite runs, then every trace passes with zero unexplained state, effect, submission, notification, or render mismatch.
- [ ] Given Linux PTY runs and injected macOS and Windows adapters, when key press/repeat/release, focus, resize, mouse, bracketed paste, clipboard, and interruption scenarios run, then capability-specific behavior matches the oracle contract.
- [ ] Given normal exit, panic capture, adapter failure, Ctrl+C, timeout, or test cancellation, when the terminal session ends, then raw mode, mouse capture, focus reporting, and bracketed paste are restored in every asserted path.
- [ ] Given an unsupported or intentionally divergent behavior, when the suite evaluates it, then release fails until the Python revision, fixture, and approved scope decision are updated together.

## Functional Requirements

- FR-01: The system must replay normalized input events through a deterministic state and effect boundary.
- FR-02: The system must compare Rust observations against versioned traces from a pinned Python reference revision.
- FR-03: The system must implement `>`, `!`, `/`, and capability-gated `&` input modes with reference-compatible prefix, backspace, completion, and submission behavior.
- FR-04: The system must strip submitted text like Python and must not emit an all-whitespace turn.
- FR-05: The system must support reference-compatible character, word, line, selection, and mouse editing for Unicode grapheme clusters and terminal cell widths.
- FR-06: The system must preserve unchanged external-editor content, cursor, selection, history state, and completion state.
- FR-07: The system must persist the latest 100 prompt-history records at `VIBE_HOME/vibehistory` using recoverable JSONL replacement semantics.
- FR-08: Up/Down must navigate visual prompt lines before entering history and must restore the draft at the reference boundary.
- FR-09: The command registry must match Python aliases, descriptions, conditional availability, exclusions, and handler semantics.
- FR-10: Slash completion must match Python ranking, skill descriptions, candidate visibility, selection, acceptance, and dismissal.
- FR-11: Path completion must trigger from the last eligible `@`, search partial paths globally, and honor Python `.gitignore`, default-ignore, ordering, and corpus-bound rules.
- FR-12: Asynchronous completion must discard stale generations, stale token ranges, and results produced after cancellation or history recall.
- FR-13: Paste must be atomic, preserve control characters as text, and match Python drag-and-drop image-path normalization without a Rust-only 256 KiB failure.
- FR-14: Submission must convert only supported image mentions into attachments; text files, directories, non-image binaries, missing paths, and external paths must retain Python prompt-text semantics.
- FR-15: macOS clipboard-image paste must support Python triggers, PNG and TIFF sources, a 5-second timeout, a 10 MiB guard, model validation, private temporary files, and cleanup.
- FR-16: Voice must support start, stop, cancel, transcription, transcript insertion, recording presentation, and recoverable errors through existing approved boundaries.
- FR-17: Feedback mode must implement Python numeric shortcuts, dismissal, and printable-character reinsertion.
- FR-18: Safety and model-switching states must expose equivalent labels and chrome, and switching must block submission without clearing the prompt.
- FR-19: Escape must dismiss or clear the same active layer as Python in the same priority order.
- FR-20: The composer must render and edit prompts through 1 MiB without truncating stored text and must keep the cursor visible.
- FR-21: Terminal input must handle only intended key event kinds and must restore every enabled terminal mode on all exit paths.

## Non-Functional Requirements

- **Performance:** For prompts up to 10,000 grapheme clusters, P99 non-I/O transition latency must be below 1 ms over 10,000 reducer iterations on CI hardware; the event handler must perform zero synchronous filesystem, clipboard, microphone, transcription, or subprocess calls; P95 render time for a 1 MiB prompt must be below 50 ms over 200 frames; completion dispatch must return control to the event loop within 16 ms.
- **Security:** 100 percent of paste fixtures containing C0, C1, escape, newline, tab, and shortcut-like bytes must insert text without dispatching a shortcut; clipboard images must be rejected above 10 MiB; on Unix, created image files must use mode `0600` and private directories mode `0700`; diagnostics and history-write errors must contain zero prompt bodies or attachment bytes.
- **Accessibility:** 100 percent of chat-input actions must be reachable by keyboard; safety, switching, recording, feedback, completion selection, and error states must each include a text or symbol distinction in addition to color; mouse support must not be required for any editing outcome.
- **Scalability:** Prompt history must retain exactly the latest 100 valid entries; path completion must handle the reference corpus bound of 32,000 entries without blocking the event loop for more than 16 ms per dispatch; the visible popup must allocate no more rows than the current viewport.
- **Reliability:** Zero stale completion or voice result may be applied in 1,000 randomized out-of-order sequences; 100 percent of PTY normal-exit, error, timeout, Ctrl+C, and cancellation cases must restore terminal modes; the complete canonical corpus must produce identical normalized results in 10 consecutive runs.

## Edge Cases & Error States

Systematic coverage of unhappy paths.

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Empty submission | Enter on empty or whitespace-only text | Emit no turn and retain idle composer state | None |
| 2 | Mode removed | Backspace at the start of `>`, `!`, `/`, or `&` | Return to the Python-defined prior mode without losing remaining text | None |
| 3 | Multiline history boundary | Up/Down inside hard or visually wrapped lines | Move the cursor until the reference history boundary, then recall or restore draft | None |
| 4 | Corrupt history | Invalid JSONL mixed with valid records | Load valid records, preserve order, continue interaction, and avoid logging prompt bodies | "Some prompt history entries could not be loaded" |
| 5 | Completion race | Older filesystem result arrives after a newer edit | Discard the result with no popup or selection change | None |
| 6 | Completion worker failure | Thread creation, scan, or permission error | Close completion and keep editing available | "Path completion is unavailable: {reason}" |
| 7 | Atomic control paste | Paste includes escape or shortcut-like characters | Insert the payload as text and dispatch zero bindings from its contents | None |
| 8 | Oversized or invalid image | Mention or clipboard image exceeds 10 MiB or fails decoding | Send no partial turn, preserve prompt, remove unused temporary file | "Image could not be attached: {reason}" |
| 9 | Unsupported model image | Valid image with a text-only model | Preserve prompt and send no turn | "The active model does not support images" |
| 10 | Clipboard unavailable | Unsupported OS, permission denial, timeout, or non-image clipboard | Explicit action warns; implicit empty paste remains silent where Python does | Reference-equivalent warning or none |
| 11 | Voice interruption | Any key, Ctrl+C, timeout, or late transcript during recording | Stop or cancel per Python, consume the correct event, and ignore late results | Reference-equivalent recording or error notice |
| 12 | Model switching | Enter during an active switch | Block submission and retain the complete prompt | "Switching model" |
| 13 | Tiny terminal | Composer has zero or one usable content cell | Avoid panic, retain state, and render a cursor whenever one cell exists | None |
| 14 | Long Unicode prompt | Resize or selection in a prompt above 64 KiB | Keep all text, maintain valid grapheme and cell coordinates, and follow the cursor | None |
| 15 | Terminal interruption | Exit or failure after enabling terminal modes | Restore raw mode, mouse capture, focus reporting, and bracketed paste | Diagnostic only if restoration fails |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Unicode grapheme positions diverge from wrapped terminal cell coordinates | High | High | Keep logical and visual coordinates separate; add oracle fixtures for combining marks, emoji sequences, tabs, and wide cells; add randomized invariant tests |
| 2 | Voice parity requires a new cross-crate API or dependency | High | High | Make US-015 a blocking spike; stop US-016 and produce a separate boundary decision if existing ports are insufficient |
| 3 | The 21-gap scope creates long integration chains | Medium | High | Deliver five dependency-ordered epics; require each story to land executable parity fixtures; reserve US-019 for integrated release proof |
| 4 | Python behavior changes during implementation | Medium | High | Pin the oracle revision and schema; update fixtures only with a changelog entry and reviewed behavior decision |
| 5 | Platform clipboard and terminal behavior is under-tested on Linux CI | High | Medium | Inject platform ports for deterministic tests and require one native macOS clipboard validation plus Linux PTY coverage before release |
| 6 | Persistent history exposes prompt content through logs or unsafe file replacement | Low | High | Match atomic replacement, bound records, apply restrictive permissions, and prohibit prompt bodies in diagnostics |
| 7 | A pure transition boundary becomes an adjacent TUI rewrite | Medium | Medium | Restrict it to observable chat-input state and effects; reuse existing editor, completion, command, workflow, clipboard, and render modules |

## Non-Goals

Explicit boundaries for this version:

- Refactoring the entire Ratatui application, transcript model, runtime, or callback system.
- Achieving structural or line-for-line equivalence with Python internals.
- Adding commands, aliases, input modes, attachment types, or voice features not present in the pinned Python reference.
- Changing app-server methods, protocol schemas, core agent behavior, model APIs, or saved conversation history.
- Replacing Ratatui, Crossterm, the async runtime, or existing workspace dependencies.
- Byte-identical ANSI output across terminal implementations. The contract is equivalent information, state, effects, cursor target, and viewport rendering.
- General-purpose editor features beyond what the reference exposes.

## Files NOT to Modify

- `/home/arthur/dev/mistral-vibe/**` - read-only Python oracle; fixture generation may execute it but must not modify it.
- `crates/vibe-core/**` - core agent behavior is outside the chat-input boundary.
- `crates/vibe-app-server/**` - app-server APIs and runtime behavior are outside scope; a missing voice boundary blocks US-016.
- `crates/vibe-protocol/**` - protocol schemas must not change for UI parity.
- `Cargo.toml` and `Cargo.lock` - no chat-input story may *add* a dependency; adding one still needs a separate approved decision. Toolchain and version bumps are handled outside this PRD in their own commit.
- Session, memory, database, plugin-cache, browser-state, `.sandbox*`, and temporary application-managed data - test fixtures must use isolated test directories only.

## Technical Considerations

Frame these as engineering decisions to confirm during implementation:

- **Architecture:** Should the deterministic boundary be a new focused module or an extension of `tui/input.rs`? Recommended: a focused `chat_input` state/effect module within `vibe-cli` that composes the existing `PromptEditor` and `CompletionEngine`, because the event loop at `tui/mod.rs:720` is already too broad to serve as a replay boundary.
- **Data Model:** Should history records reuse Python-compatible JSON fields directly or use a Rust type with a compatibility serializer? Recommended: a typed Rust record with exact compatibility fixtures, atomic replacement, and latest-100 truncation.
- **API Design:** How should async filesystem, clipboard, editor, voice, and timer results re-enter the reducer? Recommended: tagged effect IDs and generation tokens delivered through the existing event-loop message path, with stale responses rejected.
- **Dependencies:** Is any new crate required for visual-line geometry, TIFF conversion, or voice? Recommended: no new dependency unless US-015 or a parity fixture proves the current stack cannot express required behavior. Prefer existing workspace crates and platform commands already wrapped by ports.
- **Migration:** How is existing in-memory Rust history introduced without duplicating the current prompt? Recommended: load Python-compatible persisted history at startup, append only successful new submissions, keep one draft outside the persisted list, and tolerate absent files.
- **Testing:** Should oracle traces assert full screen buffers or semantic regions? Recommended: semantic state and effect assertions for all traces, plus fixed-width `TestBackend` snapshots for composer, popup, status chrome, cursor, and viewport cases.
- **Reference pinning:** Which Python commit defines v1 parity? Recommended: record its full Git SHA in fixture metadata before US-001 is considered done and reject mixed-revision fixture generation.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Audited observable gaps passing | 0 of 21 formally gated | 21 of 21 | Month 6 | Differential trace mapping and US-019 report |
| Canonical trace pass rate | No shared oracle corpus | 100 percent with zero unexplained skip | Month 6 | Differential conformance runner |
| Canonical corpus breadth | 0 traces | At least 60 Month 1, 120 Month 6 | Month 1 and Month 6 | Versioned fixture manifest |
| Stale async results applied | Not systematically measured | 0 in 1,000 randomized sequences | Month 1 onward | Reducer sequence tests |
| Terminal teardown coverage | Partial PTY coverage | 100 percent of defined normal and failure exits | Month 6 | Linux PTY and adapter test matrix |
| Long-prompt visible data loss | Rust truncates the rendered input path beyond its current bound | 0 through 1 MiB | Month 6 | Fixed-width snapshots and cursor reachability tests |
| Input-path blocking OS calls | Clipboard and related effects can be synchronous | 0 | Month 1 onward | Transition instrumentation and focused tests |
| Reported Python-to-Rust chat-input regressions | Baseline established at rollout | 0 for 90 consecutive days | Month 6 | Issue labels and fixture additions |

## Open Questions

- Which full Python Git SHA will be the v1 oracle? Owner: implementation lead. Due before US-001 completion. All fixtures and parity decisions depend on it.
- Can current `vibe-cli` ports express microphone capture and transcription without cross-crate changes? Owner: US-015 implementer. Due before US-016 starts. A negative answer creates a separate architecture decision and keeps voice blocked.
- Does native macOS validation confirm both PNG and TIFF clipboard sources, all four triggers, the 5-second timeout, and cleanup? Owner: US-014 reviewer. Due before US-019. Injected adapter tests do not close this question alone.
- Which Python behavior wins if the reference changes after the pinned revision? Owner: release owner. Due when such a change is detected. Default: keep the pin, open a scoped parity update, and do not silently regenerate fixtures.
[/PRD]
