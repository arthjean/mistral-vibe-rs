[PRD]
# PRD: TUI Runtime Observable Parity

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-01 | Arthur Jean | Phase B of the 27-gap Python-to-Rust TUI parity program, covering startup, callbacks, queueing, sessions, config, MCP, transcript semantics, updates, notifications, and narration |

## Problem Statement

1. The Rust TUI is not behaviorally interchangeable with the official Mistral Vibe reference outside the chat composer. The parity audit identified observable differences in startup trust, positional prompts, resume intent, callbacks, queued turns, interruption, rewind, session deletion, configuration, MCP authentication, event rendering, transcript interaction, and terminal integrations.
2. Several gaps cross safety boundaries. Rust can enter a workspace before the reference trust decision, callbacks lack the complete approval and question interactions, and irreversible actions such as session deletion do not use the reference confirmation state machine.
3. Several existing Rust adapters already expose the required server capabilities, but runtime state is coordinated inside a broad event loop. Adding isolated branches would make ordering, cancellation, resynchronization, and cleanup harder to prove.
4. The active phase A PRD, `tasks/prd-chat-input-observable-parity.md`, owns composer gaps. This document owns the remaining runtime and transcript gaps and provides one coverage matrix for all 27 audited differences.

**Why now:** Full feature parity is the repository's stated objective. Phase A has already established oracle traces, a deterministic input reducer, and a differential runner. Extending that executable contract now is cheaper and safer than layering more features on divergent startup and active-turn behavior.

## Overview

This initiative makes the Rust TUI observably equivalent to the official Python implementation pinned at Git commit `99a6efa9ca1fb48671adebe0f6f5d931945bd8c9` (tag `v2.23.2`). The target baseline is Rust commit `08eaf9cebada5c6df9b941c52bb293b94a433e72`. Equivalent behavior means the same accepted inputs, user-visible output, state transitions, ordered effects, error handling, and terminal cleanup for the same normalized scenario. Internal structure and language idioms may differ.

Implementation will extend the proven `State + Event -> Effect` boundary from `crates/vibe-cli/src/tui/chat_input.rs` to runtime coordination. Existing config, MCP, session, callback, clipboard, shell, and app-server adapters remain effect executors. The runtime reducer owns deterministic overlay priority, callback serialization, queue ownership, cancellation identity, resynchronization, focus state, and semantic presentation state. No protocol or server change is assumed.

Delivery is dependency ordered: safe startup, callbacks and queue control, reversible session management, config and MCP overlays, then transcript semantics and terminal integrations. Every story adds oracle traces plus reducer, Ratatui, or PTY evidence appropriate to its boundary.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Close the full audit | All 27 gaps mapped to an owning story and canonical trace | 27 of 27 gaps pass with zero accepted divergence |
| Make runtime behavior replayable | Startup, callback, queue, cancel, and session transitions serializable | 100 percent of runtime parity traces deterministic over 10 consecutive runs |
| Preserve safety boundaries | Trust and destructive-action traces enforced before dependent effects | Zero project capability loaded before trust and zero unconfirmed session deletion |
| Preserve terminal reliability | PTY tests cover focus, cancellation, external actions, and normal exit | 100 percent of defined panic, signal, cancel, and error paths restore terminal modes |
| Keep interaction responsive | No blocking I/O in reducers or render functions | P99 reducer latency below 1 ms and P95 frame render below 50 ms |

## Target Users

### Terminal-native Vibe operator

- **Role:** Developer using the Python and Rust clients interchangeably across trusted and untrusted workspaces.
- **Behaviors:** Starts with a prompt, resumes sessions, approves tools, answers agent questions, queues work, rewinds, edits configuration, authenticates MCP sources, follows links, and relies on terminal notifications.
- **Pain points:** Identical commands can enter different startup modes, skip a picker, expose incomplete callbacks, lose queued intent, or perform an irreversible action with fewer safeguards.
- **Current workaround:** Return to Python for trust, callback, rewind, configuration, MCP, and transcript workflows.
- **Success looks like:** The same scenario and keystrokes produce equivalent decisions, visible state, effects, errors, and recovery in both clients.

### Rust port maintainer

- **Role:** Engineer implementing or reviewing `vibe-cli` runtime and TUI behavior.
- **Behaviors:** Changes event polling, app-server event projection, overlays, workflow adapters, rendering, and terminal lifecycle.
- **Pain points:** Ordering and ownership are implicit in one runtime loop, so local tests can pass while callback, queue, cancellation, or resync sequences diverge.
- **Current workaround:** Manually compare Python widgets and Rust branches without a complete cross-surface trace.
- **Success looks like:** A failed trace identifies the first divergent event, state field, effect, semantic region, or terminal lifecycle observation.

## Research Findings

### Competitive Context

- Official Mistral Vibe documents interactive startup, initial prompts, resume, slash commands, permissions, and project-scoped behavior as one continuous CLI workflow: [Mistral Vibe CLI](https://docs.mistral.ai/vibe/code/cli/work-with-cli).
- Claude Code, Codex CLI, and Gemini CLI expose the same baseline expectations: explicit interactive and headless modes, resumable sessions, visible tool activity, permissions, project instructions, cancellation, and recovery. Sources: [Claude Code CLI](https://code.claude.com/docs/en/cli-usage), [Codex CLI](https://developers.openai.com/codex/cli), [Gemini CLI](https://geminicli.com/docs/).
- Trust is a precondition, not a cosmetic overlay. Gemini's trusted-folder and hook documentation independently reinforces resolving trust before project hooks, MCP, settings, environment, or extensions are loaded: [trusted folders](https://geminicli.com/docs/cli/trusted-folders/), [hooks](https://geminicli.com/docs/hooks/).
- The differentiator is executable interchangeability with Mistral Vibe, not a broader feature set. Rust-only behavior remains outside the parity surface unless separately approved.

### Best Practices Applied

- Exercise the shipped binary through PTY scenarios and record initial state, normalized input, viewport, environment, ordered semantic events, filesystem deltas, final screen, and exit status.
- Keep semantic assertions separate from pixel snapshots. Freeze model responses, clocks, IDs, paths, latency, color, and locale. Use fixed-size buffers for rendering and PTY tests for real input, focus, signals, links, clipboard, and cleanup.
- Ratatui `TestBackend` supports deterministic frame assertions and explicit resize scenarios: [TestBackend](https://docs.rs/ratatui/0.30.2/ratatui/backend/struct.TestBackend.html), [Terminal](https://docs.rs/ratatui/0.30.2/ratatui/struct.Terminal.html).
- Model runtime coordination as typed events reduced into state and ordered effects. Preserve callback, turn, and generation identities. Invalid or stale callbacks fail closed and trigger canonical resynchronization.
- Never rely on raw ANSI snapshots alone, timing sleeps, callback reentrancy, ambient permissions, or best-effort terminal teardown.

### Codebase Findings

- The current runtime interleaves polling, callbacks, queue drain, rendering, terminal input, cancellation, and cleanup in `crates/vibe-cli/src/tui/mod.rs:265`.
- The port already has reusable seams: deterministic input transitions in `crates/vibe-cli/src/tui/chat_input.rs:1`, ordered canonical state in `crates/vibe-cli/src/tui/state.rs:167`, callback validation in `crates/vibe-cli/src/tui/controls.rs:123`, typed workflow adapters in `crates/vibe-cli/src/tui/workflow/config.rs:125` and `crates/vibe-cli/src/tui/workflow/mcp.rs:34`, Ratatui fixtures in `crates/vibe-cli/tests/tui_parity.rs:149`, and PTY lifecycle coverage in `crates/vibe-cli/tests/tui_pty.rs:11`.
- Python resolves trust before opening, listing, starting, or resuming a session at `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/startup.py:87`.
- Python serializes active and pending callbacks at `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:1791` and owns typed prompt and shell queue items at `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/message_queue.py:27`.

## Assumptions & Constraints

### Assumptions to Validate

- Existing app-server resources can express trust decisions, callback responses, session resume/delete/rewind, config patches, and MCP CRUD/auth without changes to `vibe-protocol`, `vibe-core`, or `vibe-app-server`.
- Existing client and workspace dependencies can implement update checks, terminal notifications, URL opening, clipboard copy, and narrator effects without adding a crate. US-036 validates the narrator boundary before US-037 begins.
- Semantic render parity means equal information, order, status, interaction affordances, and viewport behavior. Byte-identical ANSI output is not required.
- Reference network effects can be replaced by deterministic ports in tests without changing observable UI state or error semantics.

### Hard Constraints

- Observable behavior, not internal structure, defines parity.
- `/home/arthur/dev/mistral-vibe` is a read-only oracle.
- Trust is resolved before project settings, hooks, MCP, skills, environment, session listing, session opening, or session resumption.
- Invalid, duplicate, or stale callbacks never default to approval or mutate a different turn.
- Destructive session deletion requires the reference two-step confirmation and cannot target the active session.
- Reducers and render functions perform no filesystem, network, clipboard, browser, audio, subprocess, or timer work.
- No new dependency is introduced by default. A dependency requires a separate approved decision backed by a failing parity fixture.
- Primary ownership is limited to `crates/vibe-cli/src/tui/**`, `crates/vibe-cli/tests/**`, and `scripts/parity/**` unless a fixture proves a public boundary insufficient.

## Audit Coverage Matrix

| Gap | Observable difference | Owning delivery story |
|-----|-----------------------|-----------------------|
| 1 | Workspace trust is not resolved before interactive startup | US-020 |
| 2 | A positional prompt selects headless execution instead of remaining in the TUI | US-021 |
| 3 | Bare `--resume` does not open the reference session picker | US-022 |
| 4 | Approval and user-question callbacks lack their complete interactive state machines | US-023, US-024 |
| 5 | Text and non-image mentions are coerced into different attachment semantics | Phase A US-013 |
| 6 | Queued work lacks typed item, ownership, pause, requeue, and drain semantics | US-025 |
| 7 | Interruption does not finalize active turns and pending state like the reference | US-026 |
| 8 | Rewind lacks the conditional restore-files decision | US-027 |
| 9 | Session deletion lacks reference confirmation and recovery states | US-028 |
| 10 | Prompt, shell, slash, and teleport modes diverge | Phase A US-004 |
| 11 | Submission normalization and empty-input behavior diverge | Phase A US-004 |
| 12 | Cursor, selection, word, mouse, and Unicode editing diverge | Phase A US-005 |
| 13 | Prompt history is not reference-compatible and persistent | Phase A US-006 |
| 14 | Multiline visual navigation conflicts with history recall | Phase A US-007 |
| 15 | External-editor state restoration diverges | Phase A US-005 |
| 16 | Slash ranking, metadata, selection, and dismissal diverge | Phase A US-009 |
| 17 | Path search, async generations, and popup state diverge | Phase A US-010, US-011 |
| 18 | Text paste and drag-and-drop normalization diverge | Phase A US-012 |
| 19 | Clipboard-image capture, validation, and cleanup diverge | Phase A US-014 |
| 20 | Voice recording and transcription behavior is absent | Phase A US-015, US-016 |
| 21 | Feedback, safety, and model-switching presentation diverge | Phase A US-017 |
| 22 | Long prompts lose visible data or cursor reachability | Phase A US-018 |
| 23 | Command names, aliases, availability, and handlers diverge | Phase A US-008 |
| 24 | Config layers and MCP or connector control flows are incomplete | US-029, US-030 |
| 25 | History events, tools, notices, and turn errors lose semantic presentation | US-031, US-032 |
| 26 | Transcript scrolling, selection, copy, links, and resize behavior diverge | US-033 |
| 27 | Update discovery, terminal notifications, and narration are absent or incomplete | US-034, US-035, US-036, US-037 |

## Quality Gates

These gates apply proportionately to each story and completely to each epic:

- `cargo +stable fmt --all -- --check` for workspace formatting.
- `cargo +stable check --workspace --all-targets --all-features` for all target and feature combinations.
- `cargo +stable clippy --workspace --all-targets --all-features -- -D warnings` for the repository lint policy.
- `cargo +stable test --workspace --all-features` for the integrated release gate.
- Differential oracle traces pinned to the Python SHA, with unknown fields and unexplained skips treated as failures.
- Pure reducer tests for event ordering, stale identities, overlay priority, queue ownership, cancellation, and resynchronization.
- Ratatui `TestBackend` assertions at 40, 80, and 120 columns for every new overlay and transcript semantic region.
- Linux PTY scenarios for startup intent, trust exit, resize, focus, Ctrl+C, signals, external URL and clipboard effects, and terminal-mode restoration.
- Injected failure tests for filesystem, server, network, clipboard, URL opener, notification, audio, and cache ports.

## Epics & User Stories

### EP-006: Secure Startup and Invocation

Align trust, initial-prompt, and resume intent before the main runtime begins.

**Definition of Done:** The same CLI arguments and trust state open, resume, select, cancel, or close the same interactive flow as Python, with no project-scoped effect before trust.

#### US-020: Gate interactive startup on workspace trust

**Description:** As a terminal operator, I want unknown workspaces resolved before Vibe loads project capabilities so that the Rust client has the same safety boundary as the official client.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by Phase A US-003

**Official Mistral Vibe reference navigation:**
- Directories: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui`, `/home/arthur/dev/mistral-vibe/vibe/setup/trusted_folders`, `/home/arthur/dev/mistral-vibe/vibe/core`
- Files: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/startup.py`, `/home/arthur/dev/mistral-vibe/vibe/setup/trusted_folders/trust_folder_dialog.py`, `/home/arthur/dev/mistral-vibe/vibe/core/trusted_folders.py`, `/home/arthur/dev/mistral-vibe/tests/cli/textual_ui/test_startup.py`

**Probable Rust delivery surfaces:** `crates/vibe-cli/src/lib.rs`, `crates/vibe-cli/src/tui/mod.rs`, `crates/vibe-cli/src/tui/setup.rs`, `crates/vibe-cli/tests/tui_pty.rs`.

**Acceptance Criteria:**

- [ ] Given an unknown or explicitly untrusted workspace without a trust override, when interactive startup begins, then the trust decision is rendered before session listing, opening, starting, or resuming and before any project settings, hooks, MCP, skills, or environment are loaded.
- [ ] Given workspace, repository, or explicit untrusted decisions offered by the server, when the user confirms one, then the exact decision is sent once, persisted through the existing resource, and only the capabilities allowed by that decision become visible.
- [ ] Given trust cancellation, terminal abort, malformed trust details, or a decision failure, when startup resolves, then no session starts, the host closes, terminal modes are restored, and no project-scoped effect executes.

#### US-021: Keep positional prompts in interactive mode

**Description:** As a terminal operator, I want an initial prompt to open and submit inside the TUI so that passing text does not silently switch the interaction model.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-020

**Official Mistral Vibe reference navigation:**
- Directories: `/home/arthur/dev/mistral-vibe/vibe/cli`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui`
- Files: `/home/arthur/dev/mistral-vibe/vibe/cli/cli.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py`, `/home/arthur/dev/mistral-vibe/tests/cli/textual_ui/test_startup_prompt.py`

**Probable Rust delivery surfaces:** `crates/vibe-cli/src/main.rs`, `crates/vibe-cli/src/lib.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [ ] Given an interactive terminal and a positional or stdin initial prompt, when startup completes, then the TUI mounts first and dispatches the prompt through the same idle-input path as Python.
- [ ] Given an explicit non-interactive mode, when a prompt is supplied, then only that explicit mode selects headless execution and its exit and output contract remains unchanged.
- [ ] Given empty input, conflicting invocation flags, or failure before the TUI mounts, when arguments are resolved, then no hidden mode switch or partial prompt submission occurs and the error identifies the conflicting intent.

#### US-022: Match direct and picker-based resume intent

**Description:** As a returning operator, I want bare and identified resume invocations to follow Python so that I can select, resume, or start a session predictably.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-020

**Official Mistral Vibe reference navigation:**
- Directories: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets`
- Files: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/startup.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/session_picker.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/cli.py`, `/home/arthur/dev/mistral-vibe/tests/cli/textual_ui/test_session_picker.py`

**Probable Rust delivery surfaces:** `crates/vibe-cli/src/lib.rs`, `crates/vibe-cli/src/tui/pickers.rs`, `crates/vibe-cli/src/tui/workflow.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [ ] Given `--resume <session-id>` or continue intent, when startup succeeds, then the identified or latest session opens directly and resumed state matches Python.
- [ ] Given bare `--resume`, when saved sessions exist, then a keyboard-operable picker shows the same ordering, current-directory context, previews, selection, cancellation, and start-new result as Python.
- [ ] Given no saved sessions, an aborted picker, a missing session, or a list or resume error, when intent resolves, then Rust starts new, closes, or reports failure exactly as the reference and never resumes a different session implicitly.

---

### EP-007: Callback and Active-Turn Control

Make approvals, questions, queued intent, interruption, and resynchronization deterministic.

**Definition of Done:** Active and pending callbacks are serialized, queued prompt and shell work drains with one owner, and cancellation produces the same finalized transcript and recoverable state as Python.

#### US-023: Implement the complete approval interaction

**Description:** As a terminal operator, I want tool approvals to show the action, required permissions, and all reference choices so that consent is informed and scoped correctly.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-022

**Official Mistral Vibe reference navigation:**
- Directories: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets`, `/home/arthur/dev/mistral-vibe/tests/cli`, `/home/arthur/dev/mistral-vibe/tests/e2e`
- Files: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/approval_app.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/tool_widgets.py`, `/home/arthur/dev/mistral-vibe/tests/cli/test_approval_app_grace_period.py`, `/home/arthur/dev/mistral-vibe/tests/e2e/test_cli_tui_tool_approval.py`

**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/controls.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/state.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [ ] Given an approval callback, when it becomes active, then Rust renders tool-specific input or diff, required permission labels, and Allow once, Allow for session, Always allow, and Deny choices with matching keyboard and numeric navigation.
- [ ] Given an accepted choice after the input grace period, when the response is sent, then its callback, session, turn, permission scope, and retry identities are preserved and exactly one response is emitted.
- [ ] Given early input, Escape, a duplicate or stale callback, server rejection, or a response failure, when handled, then approval fails closed, the wrong turn is never mutated, and canonical resynchronization restores the next valid overlay.

#### US-024: Implement the complete user-question interaction

**Description:** As a terminal operator, I want agent questions to support single, multiple, and free-form answers so that the Rust client can complete the same callbacks as Python.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-023

**Official Mistral Vibe reference navigation:**
- Directories: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets`, `/home/arthur/dev/mistral-vibe/tests/cli`, `/home/arthur/dev/mistral-vibe/tests/snapshots`
- Files: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/question_app.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py`, `/home/arthur/dev/mistral-vibe/tests/cli/test_question_app.py`, `/home/arthur/dev/mistral-vibe/tests/snapshots/test_ui_snapshot_question_app.py`

**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/controls.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/interaction.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [ ] Given one or multiple questions, when the callback opens, then tabs, cursor restoration, single-select, multi-select, optional Other input, numeric shortcuts, Vim navigation, and submit availability match Python.
- [ ] Given complete answers, when the user submits, then answer order, labels, free-form text, `is_other`, and callback identity match the reference payload exactly.
- [ ] Given empty required free-form input, incomplete multi-select, grace-period input, Escape, stale callback, or server failure, when handled, then no partial answer is emitted and the composer and pending callback queue remain recoverable.

#### US-025: Match typed queue and drain semantics

**Description:** As a terminal operator, I want prompts and shell commands queued during busy turns to preserve type, order, presentation, and cancellation so that deferred intent is never reinterpreted.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-024 and Phase A US-013

**Official Mistral Vibe reference navigation:**
- Directories: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets`, `/home/arthur/dev/mistral-vibe/tests/cli/textual_ui`
- Files: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/message_queue.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/messages.py`, `/home/arthur/dev/mistral-vibe/tests/cli/textual_ui/test_message_queue.py`

**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/interaction.rs`, `crates/vibe-cli/src/tui/workflow.rs`, `crates/vibe-cli/src/tui/state.rs`, `crates/vibe-cli/src/tui/render.rs`.

**Acceptance Criteria:**

- [ ] Given a busy turn or shell task, when prompt, prepared prompt with images, skill invocation, or shell input is submitted, then a typed item and matching pending widget are appended in reference order with one visible queue header and count.
- [ ] Given the runtime becomes idle, when drainage starts, then exactly one drain owner injects eligible head prompts, runs the tail item, links consecutive messages, preserves client IDs and telemetry, and continues in FIFO order.
- [ ] Given pause, pop-last, unsupported queued images, callback activity, cancellation, drain shutdown, or an injection failure, when handled, then pending items are retained, requeued, removed, or resumed exactly as Python without duplication or silent loss.

#### US-026: Finalize interruption and cancellation deterministically

**Description:** As a terminal operator, I want Ctrl+C and cancellation to stop the intended active work and leave a truthful transcript so that I can continue without stale state.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-025

**Official Mistral Vibe reference navigation:**
- Directories: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/handlers`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets`
- Files: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/handlers/event_handler.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/messages.py`, `/home/arthur/dev/mistral-vibe/tests/cli/textual_ui/test_resumed_terminal_entries.py`

**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/state.rs`, `crates/vibe-cli/src/tui/controls.rs`, `crates/vibe-cli/src/tui/workflow.rs`, `crates/vibe-cli/src/tui/mod.rs`, `crates/vibe-cli/src/tui/render.rs`.

**Acceptance Criteria:**

- [ ] Given an active agent turn, shell task, approval, question, or queue drain, when Ctrl+C or the matching cancel action occurs, then only the highest-priority active operation receives one cancellation effect and its visible state matches Python.
- [ ] Given server cancellation confirmation and terminal history events, when the turn finalizes, then loading, streaming, callbacks, tool widgets, queue count, and cancelled status settle once before new input or queue drainage resumes.
- [ ] Given duplicate cancellation, a late stream patch, cancellation rejection, disconnected server, or shutdown during cancellation, when handled, then stale effects are ignored or resynchronized, queued intent is not silently lost, and terminal modes are restored.

---

### EP-008: Reversible Session Management

Align rewind and deletion with the reference safeguards and recovery paths.

**Definition of Done:** Operators can navigate rewind targets, choose file restoration explicitly, cancel safely, and delete only non-active sessions after confirmation with failure recovery.

#### US-027: Match rewind navigation and restore choice

**Description:** As a terminal operator, I want rewind to distinguish editing from file restoration so that reverting conversation state does not unexpectedly change my workspace.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-026

**Official Mistral Vibe reference navigation:**
- Directories: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets`, `/home/arthur/dev/mistral-vibe/vibe/core/rewind`, `/home/arthur/dev/mistral-vibe/tests/core/rewind`
- Files: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/rewind_app.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py`, `/home/arthur/dev/mistral-vibe/vibe/core/rewind/manager.py`, `/home/arthur/dev/mistral-vibe/tests/cli/test_rewind_app_vim.py`

**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/workflow.rs`, `crates/vibe-cli/src/tui/interaction.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [ ] Given editable user messages, when rewind mode opens, then previous and next navigation, message preview, scrolling, quit, and selection shortcuts match Python and preserve the original transcript until acceptance.
- [ ] Given a target with file changes, when options render, then Edit and restore files and Edit without restoring files are distinct; without file changes only Edit message from here is offered.
- [ ] Given acceptance, cancellation, an invalid target, or server rewind failure, when handled, then the correct restore flag is sent once, accepted text returns to the composer, and failure leaves conversation and workspace state unchanged with a visible error.

#### US-028: Require reference-equivalent session deletion confirmation

**Description:** As a returning operator, I want saved-session deletion to be confirmable and recoverable so that an accidental key press cannot erase a session.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-022

**Official Mistral Vibe reference navigation:**
- Directories: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets`, `/home/arthur/dev/mistral-vibe/tests/cli/textual_ui`, `/home/arthur/dev/mistral-vibe/tests/snapshots`
- Files: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/session_picker.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/startup.py`, `/home/arthur/dev/mistral-vibe/tests/cli/textual_ui/test_session_picker.py`, `/home/arthur/dev/mistral-vibe/tests/snapshots/test_ui_snapshot_session_picker.py`

**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/pickers.rs`, `crates/vibe-cli/src/tui/workflow.rs`, `crates/vibe-cli/src/tui/render.rs`.

**Acceptance Criteria:**

- [ ] Given a highlighted non-active session, when delete is requested once, then the row enters confirmation without server mutation; the second matching request enters pending and sends exactly one delete effect.
- [ ] Given the active session, changed selection, Escape, or picker cancellation, when delete state exists, then deletion is blocked or confirmation is cleared before any broader action.
- [ ] Given server failure or deletion of the last saved session, when the result returns, then failure restores the row and reports the error, while success removes the row and returns the reference start-new result when none remain.

---

### EP-009: Configuration and External Control Surfaces

Make typed configuration and MCP or connector management usable through the same resource contracts as Python.

**Definition of Done:** Config fields target explicit layers with validation and conflict recovery, while MCP servers and connectors expose reference ordering, statuses, toggles, setup, and authentication flows.

#### US-029: Implement typed configuration and layer targeting

**Description:** As a terminal operator, I want searchable typed settings with explicit persistence scope so that configuration changes are valid and land in the intended layer.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-022

**Official Mistral Vibe reference navigation:**
- Directories: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/screens/config`, `/home/arthur/dev/mistral-vibe/tests/cli`, `/home/arthur/dev/mistral-vibe/tests/core/config`
- Files: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/screens/config/config_screen.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/screens/config/edit.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/screens/config/_common.py`, `/home/arthur/dev/mistral-vibe/tests/cli/test_ui_config_screen.py`

**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/workflow/config.rs`, `crates/vibe-cli/src/tui/pickers.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [ ] Given introspected config fields, when the config screen opens, then search, categories, effective values, source layers, booleans, enums, numbers, strings, optional values, and descriptions render and navigate like Python.
- [ ] Given a valid edit and chosen user or project target, when saved, then the typed patch and expected fingerprint are sent through the existing config resource, the effective value refreshes, and secrets are never echoed or logged.
- [ ] Given invalid input, a read-only or unsupported field, trust restriction, stale fingerprint, server rejection, or refresh failure, when handled, then no false success appears and current values remain recoverable with a scoped error.

#### US-030: Implement MCP and connector management with authentication

**Description:** As a terminal operator, I want MCP servers and connectors listed, toggled, configured, and authenticated like Python so that external tools have the same visible lifecycle.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-020, US-029

**Official Mistral Vibe reference navigation:**
- Directories: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets`, `/home/arthur/dev/mistral-vibe/vibe/core/auth`, `/home/arthur/dev/mistral-vibe/tests/core/auth`
- Files: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/mcp_app.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/mcp_oauth_app.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/connector_auth_app.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/mcp_commands.py`, `/home/arthur/dev/mistral-vibe/tests/cli/test_mcp_oauth_app.py`

**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/workflow/mcp.rs`, `crates/vibe-cli/src/tui/pickers.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [ ] Given local servers and connectors, when MCP management opens, then groups, deterministic ordering, Connected, Enabled, Needs auth, Needs setup, Unavailable, and Disabled states plus source detail match Python.
- [ ] Given toggle, setup, connector authentication, or MCP OAuth, when selected, then existing resources receive the correct source kind and name; OAuth supports open, copy, show URL, completion refresh, and close behavior.
- [ ] Given an untrusted workspace, unknown source, malformed or expired auth URL, opener or clipboard failure, authentication cancellation, server error, or stale refresh, when handled, then no capability is enabled implicitly and the overlay remains recoverable with reference-equivalent status.

---

### EP-010: Semantic Transcript and Terminal Integrations

Project canonical history into equivalent widgets and close the update, focus-notification, and narrator gaps.

**Definition of Done:** Every audited history and error type has stable semantic presentation, transcript interactions survive resize and external-effect failure, update and notification state is deterministic, and narration is implemented through an approved existing boundary.

#### US-031: Project canonical history into semantic transcript regions

**Description:** As a terminal operator, I want messages, tools, notices, diffs, hooks, compaction, and context state rendered by meaning so that the transcript communicates the same execution state as Python.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-026

**Official Mistral Vibe reference navigation:**
- Directories: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/handlers`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets`, `/home/arthur/dev/mistral-vibe/tests/cli/textual_ui`
- Files: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/handlers/event_handler.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/messages.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/tools.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/tool_widgets.py`, `/home/arthur/dev/mistral-vibe/tests/cli/textual_ui/test_event_handler_grouping.py`

**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/state.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/render/markdown.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [ ] Given canonical added and updated history entries, when reduced, then user, assistant, reasoning, shell, effect, tool, diff, hook, loop, compaction, context, teleport, and notice content appears in reference order with equivalent labels and grouping.
- [ ] Given streaming patches and effect transitions, when updates arrive, then text appends monotonically, tool input and output update in place, terminal status settles once, collapse state persists, and Unicode remains valid through resize.
- [ ] Given duplicate, out-of-order, malformed, unknown, or resync events, when reduced, then state invariants hold, known content is not duplicated or dropped, and any explicit fallback is safe, bounded, and distinguishable from a successful tool result.

#### US-032: Render specialized turn and tool errors

**Description:** As a terminal operator, I want failures classified and presented like Python so that cancellation, context limits, tool errors, and retryable failures suggest the correct recovery.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-031

**Official Mistral Vibe reference navigation:**
- Directories: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/handlers`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets`, `/home/arthur/dev/mistral-vibe/tests/cli/textual_ui`
- Files: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/handlers/event_handler.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/messages.py`, `/home/arthur/dev/mistral-vibe/tests/cli/textual_ui/test_event_handler_error_muting.py`, `/home/arthur/dev/mistral-vibe/tests/cli/textual_ui/test_tool_error_muting_widgets.py`

**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/state.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [ ] Given cancellation, token or context exhaustion, authentication, rate limit, transport, model, server, shell, or tool failure, when rendered, then severity, message, muting, collapse, and recovery hint match the reference semantic class.
- [ ] Given a tool error already represented by its tool widget, when a related turn error arrives, then duplicate generic errors are muted exactly where Python mutes them and unrelated failures remain visible.
- [ ] Given unknown error codes, malformed details, retry completion, resume, or resynchronization, when handled, then no secret or raw payload leaks, the failure is never shown as success, and stale error chrome clears without deleting transcript evidence.

#### US-033: Match transcript navigation, copy, links, and resize

**Description:** As a terminal operator, I want transcript navigation and external actions to follow Python so that long sessions remain inspectable and operable without a mouse.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-031 and Phase A US-005

**Official Mistral Vibe reference navigation:**
- Directories: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/windowing`, `/home/arthur/dev/mistral-vibe/tests/cli/textual_ui`
- Files: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/links.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/messages.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/windowing/history.py`, `/home/arthur/dev/mistral-vibe/tests/cli/textual_ui/test_chat_scroll.py`, `/home/arthur/dev/mistral-vibe/tests/cli/textual_ui/test_tool_result_link.py`

**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/interaction.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/clipboard.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [ ] Given a transcript larger than the viewport, when keyboard or mouse scrolling, load-more, collapse, selection, or resize occurs, then the reference anchor, bounds, focus, selected text, and visible cursor or status region are preserved.
- [ ] Given a link, file result, tool output, or attachment affordance, when activated, then URL decoding, open, copy, and attachment presentation match Python and external work occurs only through effects.
- [ ] Given a tiny viewport, invalid URL, missing opener, clipboard denial, disappearing entry, or resize during streaming, when handled, then the TUI does not panic, terminal state stays valid, and the user receives the scoped reference-equivalent error.

#### US-034: Implement update discovery and What's New presentation

**Description:** As a terminal operator, I want update and release-note state surfaced like Python so that I can act on a new version without disrupting the current session.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-022, US-031

**Official Mistral Vibe reference navigation:**
- Directories: `/home/arthur/dev/mistral-vibe/vibe/cli/update_notifier`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui`, `/home/arthur/dev/mistral-vibe/tests/update_notifier`
- Files: `/home/arthur/dev/mistral-vibe/vibe/cli/update_notifier/update.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/update_notifier/whats_new.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py`, `/home/arthur/dev/mistral-vibe/tests/update_notifier/test_ui_update_notification.py`

**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/setup.rs`, `crates/vibe-cli/src/tui/state.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [ ] Given a cached or freshly discovered newer version, when startup and the main screen settle, then update availability, current and latest versions, release-note or plan message, dismissal, and command affordance match Python without blocking input.
- [ ] Given no update, a dismissed version, or a current version, when discovery resolves, then no stale notification or What's New message is rendered and cache state follows reference expiry semantics.
- [ ] Given offline operation, timeout, malformed version, cache corruption, gateway error, or update-command failure, when handled, then startup continues, errors remain bounded and non-secret, and no invalid version becomes authoritative.

#### US-035: Implement focus-aware terminal notifications

**Description:** As a terminal operator, I want completion and attention signals only when useful so that background turns notify me without terminal spam.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-026

**Official Mistral Vibe reference navigation:**
- Directories: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/notifications`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui`, `/home/arthur/dev/mistral-vibe/tests/cli`
- Files: `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/notifications/adapters/textual_notification_adapter.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/notifications/ports/notification_port.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py`, `/home/arthur/dev/mistral-vibe/tests/cli/test_bell_notifications.py`

**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/setup.rs`, `crates/vibe-cli/src/tui/terminal.rs`, `crates/vibe-cli/src/tui/state.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [ ] Given notifications are enabled and the app is unfocused, when a turn completes or requires attention, then Rust applies the reference context title, terminal title change, bell, and throttle interval exactly once.
- [ ] Given focus returns, notifications are disabled, or events repeat inside the throttle window, when notification state reduces, then title and focus state reset as in Python and no redundant bell is emitted.
- [ ] Given missing focus reporting, unsupported terminal title control, write failure, rapid focus flapping, or shutdown, when handled, then the core turn succeeds, terminal escape state is not corrupted, and notification failure remains non-fatal.

#### US-036: Validate the existing narrator boundary

**Description:** As a port maintainer, I want a concrete boundary proof for summarization and audio playback so that narrator parity does not force an unplanned cross-crate redesign.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by Phase A US-003

**Official Mistral Vibe reference navigation:**
- Directories: `/home/arthur/dev/mistral-vibe/vibe/cli/narrator_manager`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets`, `/home/arthur/dev/mistral-vibe/tests/narrator_manager`
- Files: `/home/arthur/dev/mistral-vibe/vibe/cli/narrator_manager/narrator_manager_port.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/narrator_manager/narrator_manager.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/narrator_status.py`, `/home/arthur/dev/mistral-vibe/tests/narrator_manager/test_narrator_manager.py`

**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/state.rs`, `crates/vibe-cli/src/tui/workflow.rs`, existing workspace audio or model ports if present.

**Acceptance Criteria:**

- [ ] Given Python narrator start, summarize, speak, stop, cancel, and error observations, when mapped to current Rust public ports, then every required input, state, effect, and cleanup has a concrete integration point or an explicitly proven missing boundary.
- [ ] Given deterministic fake summarizer and audio-player responses, when an oracle trace is replayed, then the proposed effect schema can represent ordering, cancellation identity, late results, device failure, and terminal presentation without blocking the event loop.
- [ ] Given a missing public capability or required dependency, when the spike concludes, then US-037 remains blocked and a separate scoped decision records the smallest boundary change; no speculative protocol or dependency edit is made in this story.

#### US-037: Implement narrator lifecycle and presentation

**Description:** As a terminal operator, I want completed turns summarized and spoken with visible control state so that narrator behavior matches the official client.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-031, US-036

**Official Mistral Vibe reference navigation:**
- Directories: `/home/arthur/dev/mistral-vibe/vibe/cli/narrator_manager`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets`, `/home/arthur/dev/mistral-vibe/tests/snapshots`
- Files: `/home/arthur/dev/mistral-vibe/vibe/cli/narrator_manager/narrator_manager.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/narrator_manager/narrator_manager_port.py`, `/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/narrator_status.py`, `/home/arthur/dev/mistral-vibe/tests/snapshots/test_ui_snapshot_narrator_flow.py`

**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/state.rs`, `crates/vibe-cli/src/tui/workflow.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [ ] Given narrator is enabled and an eligible turn completes, when narration starts, then Idle, Summarizing, and Speaking transitions, visible status frames, transcript selection, and audio ordering match Python.
- [ ] Given a new turn, disable action, manual stop, cancellation, or shutdown, when narration is active, then summarization and playback stop once, late results are ignored by identity, and the status returns to idle.
- [ ] Given summarizer, audio device, codec, playback, or permission failure, when handled, then the session and composer remain usable, no audio data or transcript leaks to diagnostics, and reference-equivalent error or silent recovery is applied.

## Functional Requirements

- FR-01: Interactive startup must resolve workspace trust before every project-scoped or session-scoped effect.
- FR-02: Positional and stdin prompts must stay in the TUI unless the user explicitly selects non-interactive execution.
- FR-03: Bare resume must open a session picker; identified and continue intents must resolve directly according to the reference.
- FR-04: Runtime state must serialize active and pending callbacks and expose one deterministic overlay priority.
- FR-05: Approval callbacks must support four scoped choices, tool-specific details, grace-period protection, and fail-closed stale handling.
- FR-06: Question callbacks must support multiple questions, single and multiple selection, Other input, cancellation, and exact ordered answers.
- FR-07: The queue must preserve typed prompt, prepared prompt, skill, image, and shell items plus pending presentation in FIFO order.
- FR-08: Exactly one queue drain owner may inject, run, pause, requeue, pop, or shut down queued items.
- FR-09: Cancellation must target one active identity, settle streaming and callbacks once, and preserve eligible queued intent.
- FR-10: Rewind must expose conditional file restoration, editable-target navigation, cancellation, and recoverable failure.
- FR-11: Session deletion must require two matching actions, block the active session, and recover the row on failure.
- FR-12: Configuration must render typed fields, effective values and layers, explicit user or project targets, validation, and optimistic fingerprints.
- FR-13: MCP and connector management must render reference statuses and support toggle, setup, OAuth, connector auth, URL open, copy, show, and refresh.
- FR-14: Canonical history must project into semantic transcript regions with stable ordering, streaming, grouping, collapse, and resync behavior.
- FR-15: Turn and tool errors must be classified, deduplicated, bounded, non-secret, and paired with reference-equivalent recovery hints.
- FR-16: Transcript interaction must preserve scroll anchors, focus, selection, copy, links, attachments, and resize behavior.
- FR-17: Update discovery and What's New state must be asynchronous, cached, dismissible, and non-blocking on failure.
- FR-18: Terminal notifications must honor user preference, focus, context, title reset, bell, and throttling.
- FR-19: Narration must use explicit summarization and playback effects with identity-based cancellation and visible Idle, Summarizing, and Speaking state.
- FR-20: All OS, network, server, clipboard, opener, audio, and timer work must occur behind effects and re-enter through typed results.
- FR-21: Every new behavior must have a pinned Python trace and reject unknown schema or unexplained skips.

## Non-Functional Requirements

- **Performance:** P99 non-I/O runtime transition latency must stay below 1 ms over 10,000 reducer iterations; P95 render time must stay below 50 ms over 200 frames with 10,000 transcript entries; no reducer or render function may perform synchronous filesystem, network, clipboard, opener, audio, subprocess, or server work.
- **Security:** 100 percent of unknown-workspace traces must show zero project-scoped effects before trust; 100 percent of invalid or stale callback traces must fail closed; secrets, OAuth URLs after closure, prompts, tool payloads, audio, and clipboard content must not appear in diagnostics.
- **Accessibility:** Every overlay and transcript action must be keyboard operable; trust, approval, denial, pending, failure, focus, update, and narrator states must include a textual or symbolic distinction in addition to color; mouse support must not be required.
- **Scalability:** Session and MCP pickers must remain responsive with 1,000 items; transcript projection and windowing must remain bounded at 10,000 entries; notification and update state must retain only current cache and throttle metadata.
- **Reliability:** Zero duplicate callback response, queue injection, cancellation, notification, or narrator completion may occur in 1,000 randomized out-of-order sequences; all defined PTY exits must restore raw mode, alternate screen, mouse, focus, and paste modes.
- **Portability:** Core reducer and rendering tests must be platform independent; terminal, opener, clipboard, update, notification, and audio behavior must use injected adapters with Linux PTY coverage and native validation where the OS behavior cannot be emulated.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Trust cancelled | Escape, Ctrl+C, or closed dialog | Close host, start no session, load no project capability, restore terminal | None or reference cancellation text |
| 2 | Trust service failure | Missing details or failed decision | Abort startup without treating the workspace as trusted | `Workspace trust could not be resolved: {reason}` |
| 3 | Conflicting invocation | Interactive prompt plus explicit incompatible mode | Reject ambiguous intent before partial submission | `Conflicting invocation modes` |
| 4 | Empty resume corpus | Bare `--resume` with no saved sessions | Start a new session as Python does | None |
| 5 | Stale approval | Response after turn or callback changed | Emit no approval, resync canonical state | `Approval is no longer active` |
| 6 | Incomplete question | Empty Other or no multi-select choice | Keep callback open and emit no partial answer | Reference inline guidance |
| 7 | Queue image unsupported | Queued prepared prompt on text-only model | Requeue, pause, retain widgets, explain model switch and resume | Reference model warning |
| 8 | Drain interrupted | Shutdown or cancellation during head injection or tail turn | Preserve or requeue uncommitted items exactly once | None unless injection failed |
| 9 | Late stream after cancel | Patch arrives after terminal cancellation | Ignore by identity or resync without reviving loading state | None |
| 10 | Rewind failure | Invalid target or server rejection | Keep transcript and worktree unchanged, leave recoverable UI | `Rewind failed: {reason}` |
| 11 | Active session deletion | Delete requested on current session | Perform no delete and show reference feedback state | Reference active-session feedback |
| 12 | Config conflict | Fingerprint changed between read and patch | Reject stale write, refresh effective values | `Configuration changed; review and retry` |
| 13 | Expired OAuth URL | Auth flow closes or refreshes before action | Do not open or copy stale credentials, refresh status | `Authentication link expired` |
| 14 | Unknown history event | Unsupported or malformed canonical entry | Preserve known transcript, bounded explicit fallback, no panic | `Unsupported event` only where reference is explicit |
| 15 | Clipboard or opener failure | Copy or link activation adapter fails | Keep selection and transcript stable, report scoped failure | `Could not copy/open: {reason}` |
| 16 | Offline update check | Timeout, corrupt cache, invalid version | Continue startup and ignore non-authoritative update | None or bounded debug diagnostic |
| 17 | Focus reporting absent | Terminal emits no focus events | Suppress unsafe focus assumptions and keep notification failure non-fatal | None |
| 18 | Narrator late result | Stop followed by late summary or playback completion | Ignore stale result, remain idle, play no audio | None |
| 19 | Terminal failure | Panic, signal, error, or abort after modes enabled | Restore every enabled mode and original title | Diagnostic only if restoration fails |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Runtime reducer becomes a whole-TUI rewrite | Medium | High | Limit state to audited behavior, reuse existing adapters, migrate dependency-ordered slices, require a trace for every moved branch |
| 2 | Trust occurs after configuration or capability loading | Medium | Critical | Make trust the first startup effect, add negative-effect traces, fail startup closed |
| 3 | Callback and queue races duplicate user intent | High | High | Preserve session, turn, callback, effect, and client IDs; enforce one overlay and one drain owner; randomized sequence tests |
| 4 | Resync overwrites local overlay, queue, focus, or selection state | Medium | High | Merge canonical server state with explicitly local state, as `state.rs` already does; test resync during every overlay |
| 5 | Config or MCP parity forces protocol changes | Low | High | Use existing public resources first; prove any missing contract with a fixture and separate approved decision |
| 6 | Terminal integrations emit unsafe escape sequences or fail cleanup | Medium | High | Centralize terminal effects, validate payloads, track enabled modes and original title, PTY-test all exits |
| 7 | Update checks or narrator add unwanted dependencies | Medium | Medium | Use current workspace capabilities; US-036 blocks narrator implementation if the boundary is missing; require separate dependency approval |
| 8 | Python reference changes during delivery | Medium | High | Pin full SHA in fixtures, reject mixed revisions, update through a reviewed changelog and scoped parity decision |
| 9 | Semantic error deduplication hides important failures | Medium | High | Encode explicit muting rules from oracle traces; unknown failures remain visible and bounded |

## Non-Goals

- Refactoring the entire CLI, app server, protocol, agent loop, core history model, or workspace architecture.
- Achieving structural, class, widget, or line-for-line equivalence with Python.
- Adding commands, callback kinds, config fields, MCP capabilities, notification transports, update channels, or narrator features absent from the pinned reference.
- Byte-identical ANSI output across terminals.
- Automatically installing updates or publishing releases.
- Opening a browser directly from reducers or tests. URL opening remains an injected user-triggered effect.
- Redesigning the phase A chat composer; this document only consumes its completed contracts.
- Supporting destructive session operations beyond the official delete flow.

## Files NOT to Modify

- `/home/arthur/dev/mistral-vibe/**` because it is the read-only official oracle.
- `crates/vibe-protocol/**` unless a separately approved decision proves the existing callback or resource contract insufficient.
- `crates/vibe-core/**` unless a separately approved decision proves an existing public capability cannot express the audited behavior.
- `crates/vibe-app-server/**` unless a parity fixture proves its current trust, config, MCP, session, rewind, callback, update, or narration resource is insufficient.
- `Cargo.toml` and `Cargo.lock` for adding dependencies. Any required dependency is a separate explicit decision.
- Session databases, memories, plugin caches, browser state, computer-use state, `.sandbox*`, user config, trust records, update caches, audio files, and other app-managed state. Tests use isolated temporary roots and injected ports only.

## Technical Considerations

- **Architecture:** Introduce a focused runtime state, event, and effect module inside `vibe-cli`, modeled after `tui/chat_input.rs`. Keep `tui/mod.rs` as orchestration and effect execution rather than the source of transition rules.
- **Overlay priority:** Encode one explicit priority such as fatal startup, trust, approval, question, rewind, picker/config/MCP, queue pause, composer. Derive the exact order from oracle traces instead of widget structure.
- **State ownership:** Canonical transcript and turn state comes from app-server events. Callback queues, pending input queue, overlays, focus, selection, scroll anchor, update notification, and narrator presentation have explicit local ownership and survive resync where appropriate.
- **Effect identity:** Server callbacks, cancellations, queue injections, config patches, MCP auth, opener, clipboard, update, notification, summary, and audio effects carry stable IDs or generations. Late results cannot mutate newer state.
- **Trust boot order:** Construct only the minimal host required to query trust. Delay session listing and all project-scoped resource reads until a trusted or explicit untrusted decision resolves.
- **Rendering:** Render from immutable semantic state. Use direct buffer tests for small widgets, `Terminal<TestBackend>` for integrated layouts and resize, and PTY for real terminal input and cleanup.
- **Testing:** Extend `scripts/parity/scenarios.py`, `scripts/parity/oracle.py`, `crates/vibe-cli/tests/parity`, `tui_parity.rs`, and `tui_pty.rs`. Normalize clocks, IDs, paths, versions, latency, terminal capabilities, and external-effect results.
- **Migration:** Move one dependency-ordered behavior at a time behind the reducer. Preserve current public command behavior until its owning trace passes. Do not maintain parallel legacy and parity paths after a story lands.
- **Dependency policy:** No new crate is currently forced. Prefer existing workspace HTTP, serialization, terminal, clipboard, and async facilities plus injected ports.

## Success Metrics

| Metric | Baseline | Target | Timeframe | How Measured |
|--------|----------|--------|-----------|-------------|
| Full audited gap pass rate | 0 of 27 formally complete across both phases | 27 of 27 | Month 6 | Coverage matrix plus phase A US-019 and phase B epic reports |
| Runtime parity trace pass rate | No complete runtime corpus | 100 percent, zero unexplained skip | Month 6 | Differential conformance runner |
| Pre-trust project effects | Not formally gated | 0 across all unknown and untrusted traces | Month 1 onward | Startup effect log assertions |
| Duplicate callback, queue, or cancel effects | Not systematically measured | 0 in 1,000 randomized sequences | Month 1 onward | Reducer property and sequence tests |
| Unconfirmed destructive deletes | One-step behavior possible | 0 | Month 1 onward | Picker reducer and app-server fake tests |
| Semantic transcript coverage | Generic rendering for audited variants | 100 percent of pinned event and error variants | Month 6 | Manifest coverage and fixed-width buffer tests |
| Terminal teardown coverage | Partial PTY coverage | 100 percent of defined exits | Month 6 | PTY normal, abort, signal, panic, timeout, and adapter-failure matrix |
| Interaction latency | Not gated for runtime reducer | P99 below 1 ms; P95 render below 50 ms | Month 6 | Deterministic benchmark harness |
| User-reported Python-to-Rust TUI regressions | Baseline at rollout | 0 for 90 consecutive days | Month 6 | Issues linked to a new parity trace |

## Open Questions

- Does the current app-server callback response surface preserve all four approval scopes and required permission details? Owner: US-023 implementer. Due before US-023 leaves TODO. If not, attach a failing fixture and request a separate boundary decision.
- Which existing workspace capability should execute update discovery without a new dependency or blocking startup? Owner: US-034 implementer. Due before US-034 starts. The default is an injected async gateway and cache port inside `vibe-cli`.
- Can current Rust public ports express summarization, audio playback, cancellation, and device errors? Owner: US-036 implementer. Due before US-037 starts. A negative answer blocks US-037 and creates the smallest separate boundary decision.
- Which native terminals form the final focus, title, bell, opener, and clipboard validation matrix? Owner: release owner. Due before EP-010 completion. Linux PTY remains mandatory; native validation is added only where deterministic adapters cannot prove behavior.
- Which Python behavior wins if the reference changes after the pinned revision? Owner: release owner. Due when detected. Default: keep the pin, open a scoped parity update, and never regenerate fixtures silently.
[/PRD]
