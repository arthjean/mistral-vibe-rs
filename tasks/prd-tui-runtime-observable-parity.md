[PRD]
# PRD: TUI Runtime Observable Parity

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-02 | Arthur Jean | Complete phase B PRD based on the second-pass Python-to-Rust observable parity audit; supersedes the removed 27-gap runtime plan with 34 verified runtime gaps and source navigation on every epic and story |

## Problem Statement

1. The Rust TUI is not behaviorally interchangeable with the official Mistral Vibe implementation outside the chat composer. A second-pass audit verified 34 observable gaps across invocation, trust, worktrees, callbacks, queues, session recovery, integrations, transcript semantics, terminal services, and exit behavior.
2. Several gaps cross safety boundaries. `--worktree` is accepted but ignored, trust is resolved after immutable project context is built, failed tools can be rendered as completed, rewind never restores files, and session deletion has no confirmation state.
3. Existing Rust components expose much of the required transport, but orchestration is distributed through one event loop. Local branches cannot by themselves prove callback order, late-event rejection, cancellation ownership, queue drainage, resynchronization, or terminal cleanup.
4. Phase A, `tasks/prd-chat-input-observable-parity.md`, owns the completed composer surface. Runtime phase B is referenced there but absent from the current tree, leaving no executable delivery contract for the remaining application behavior.

**Why now:** Full feature parity remains the repository objective, while phase A already provides trace schemas, normalization, a deterministic reducer pattern, Ratatui fixtures, and PTY coverage. Extending that contract now prevents new runtime behavior from accumulating on safety-critical divergence.

## Overview

This initiative makes the Rust TUI observably equivalent to the Python reference pinned at commit `99a6efa9ca1fb48671adebe0f6f5d931945bd8c9`. The Rust baseline is `8c586fb84e57`. Equivalence means the same accepted input, visible information, state transition, ordered effect, error classification, filesystem consequence, and terminal cleanup for the same normalized scenario. Byte-identical ANSI and equivalent internal structure are not required.

Implementation extends the existing differential contract from chat input to runtime coordination. A focused runtime state/event/effect boundary inside `vibe-cli` will make overlay priority, callback serialization, queue ownership, cancellation identity, resynchronization, focus state, and semantic presentation replayable. Existing app-server resources remain the first choice; a cross-crate change is allowed only when a failing oracle fixture proves the current public boundary cannot express reference behavior.

Delivery is dependency ordered: safe startup and invocation, active-turn control, reversible sessions, configuration and integrations, semantic transcript, then terminal services. Each story adds pinned Python observations plus reducer, buffer, integration, or PTY evidence proportionate to the boundary.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Close the verified runtime audit | 34 of 34 gaps mapped to an owning story and failing or pending trace | 34 of 34 traces passing with zero accepted divergence |
| Make runtime behavior replayable | Startup, callback, queue, cancellation, and session state serializable | 100% deterministic results across 10 consecutive runs of the runtime corpus |
| Restore safety boundaries | Pre-trust effects, worktree isolation, tool failure status, rewind, and delete guards covered | 0 pre-trust project effects, 0 false-success terminal effects, 0 unconfirmed deletes across the corpus |
| Preserve terminal reliability | PTY matrix covers normal exit, abort, resize, focus, suspend, and signal | 100% of defined exits restore every enabled terminal mode |
| Preserve interaction latency | Runtime benchmark and render corpus established | P99 reducer latency <1 ms and P95 render latency <50 ms |

## Target Users

### Terminal-native Vibe operator

- **Role:** Developer using Python and Rust clients interchangeably in trusted, untrusted, local, worktree, and Vibe Code workflows.
- **Behaviors:** Starts or resumes sessions, approves effects, answers questions, reviews plans, queues prompts and shell commands, rewinds, configures integrations, follows tool output, and leaves long turns running out of focus.
- **Pain points:** Identical flags or keystrokes select different modes, omit safety context, reorder deferred intent, hide failures, or perform irreversible actions with fewer safeguards.
- **Current workaround:** Avoid affected Rust commands or return to Python for worktree, trust, callback, rewind, configuration, MCP, remote-project, and transcript workflows.
- **Success looks like:** The same scenario produces equivalent decisions, visible state, effects, errors, recovery, workspace changes, and exit output in both clients.

### Rust port maintainer

- **Role:** Engineer implementing or reviewing `vibe-cli` runtime behavior and its app-server adapters.
- **Behaviors:** Changes event polling, canonical projection, overlays, workflow calls, terminal rendering, and lifecycle cleanup.
- **Pain points:** Local tests prove isolated mechanics but do not identify the first mismatch in callback order, queue ownership, resync, external effects, or rendering semantics.
- **Current workaround:** Compare both implementations manually and infer behavior from broad event-loop branches.
- **Success looks like:** A failed parity trace names the first divergent event, state field, effect, filesystem delta, semantic region, or terminal observation.

## Research Findings

Key findings that informed this PRD:

### Competitive Context

- Official Mistral Vibe treats trust, interactive prompts, resumable sessions, queued messages, permissions, tool streaming, notifications, and recovery as one continuous CLI contract: [Mistral Vibe repository](https://github.com/mistralai/mistral-vibe).
- Gemini CLI resolves trusted folders before loading project environment, hooks, MCP, commands, or auto-approvals, and its checkpointing restores both workspace and conversational intent: [trusted folders](https://geminicli.com/docs/cli/trusted-folders/), [checkpointing](https://geminicli.com/docs/cli/checkpointing/).
- Claude Code separates one-time and persistent approval scopes and starts from constrained permissions: [Claude Code security](https://code.claude.com/docs/fr/security).
- Codex app-server models tool lifecycle with stable thread, turn, and item identities and distinguishes cancellation from denial: [Codex app-server protocol](https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md).
- **Market gap:** The differentiator is executable interchangeability with Mistral Vibe, not additional Rust-only features.

### Best Practices Applied

- Keep Python as a pinned temporary oracle. Replay normalized scenarios with deterministic clocks, IDs, paths, terminal sizes, external effects, and app-server responses.
- Separate semantic assertions from visual buffers. Test runtime transitions independently, widgets directly on `Buffer`, integrated layouts with Ratatui `TestBackend`, and real signals or terminal teardown through PTY scenarios: [Textual testing](https://textual.textualize.io/guide/testing/), [Ratatui testing](https://ratatui.rs/recipes/testing/).
- Inject focus, paste, resize, key-kind, and application-signal events. Crossterm does not expose generic OS signals as terminal events, so signal sources must be modeled separately.
- Treat golden changes as reviewed behavior changes. Never bulk-regenerate traces or snapshots to make mismatches disappear.

## Assumptions & Constraints

### Assumptions to Validate

- The pinned Python behavior remains authoritative even where Rust is stricter, more compact, or already exposes additional commands.
- Equivalent rendering requires equal information, order, status, interaction affordance, cursor target, and viewport behavior at a fixed size, not equal widget structure or raw ANSI bytes.
- Existing app-server resources can express most trust, callback, session, configuration, remote-project, and loop behavior. Any missing OAuth or immutable-trust boundary is changed only after a failing fixture proves it.
- Existing workspace HTTP, terminal, clipboard, audio, and async capabilities are sufficient for updates, notifications, links, and narration without a new dependency.
- External services and OS effects can be replaced by deterministic ports without changing user-visible state or error semantics.

### Hard Constraints

- Observable behavior, not internal structure, defines parity.
- `/home/arthur/dev/mistral-vibe` is a read-only oracle.
- Project discovery, settings, instructions, skills, hooks, MCP, environment, session listing, opening, and resumption occur only after the reference trust decision.
- Invalid, duplicate, stale, or reordered callbacks never default to approval or mutate another turn.
- Destructive session deletion requires the reference confirmation state machine; rewind sends the user-selected restore-files choice.
- Runtime reducers and render functions perform no filesystem, network, browser, clipboard, notification, audio, subprocess, or timer work.
- No new dependency is introduced by default. A dependency requires a separately approved decision backed by a failing parity fixture.
- Each trace records the full Python SHA and rejects unknown schema fields, mixed revisions, and unexplained skips.

## Audit Coverage Matrix

| Gap | Verified observable difference | Owning story |
|-----|--------------------------------|--------------|
| 1 | `--worktree` is parsed but ignored | US-021 |
| 2 | Trust has no pre-session decision and post-start trust cannot rebuild immutable project context | US-020 |
| 3 | Dangerous-directory warning is absent | US-020 |
| 4 | Positional and stdin prompts select a different interaction route | US-022 |
| 5 | Bare `--resume` does not open the reference picker | US-023 |
| 6 | Startup `--teleport` is ignored | US-022 |
| 7 | `--check-upgrade` and `/update` do not perform update discovery | US-037 |
| 8 | Initialization and MCP failures occur outside or after the reference startup flow | US-023 |
| 9 | Approvals omit effect, permission, specialized preview, and grace behavior | US-024 |
| 10 | Questions lack structured tabs, selection, free text, and grace behavior | US-025 |
| 11 | Concurrent callbacks are selected lexicographically rather than FIFO | US-025 |
| 12 | Plan review loses live file synchronization and editor action | US-026 |
| 13 | Queued prompts are not grouped and only the count is rendered | US-027 |
| 14 | Slash commands and Teleport can execute during busy turns unlike Python | US-027 |
| 15 | Interruption has no persistent finalized transcript marker | US-028 |
| 16 | Local shell output is not streamed while running | US-028 |
| 17 | Rewind always sends `restoreFiles: false` | US-029 |
| 18 | Session deletion has no confirmation state | US-030 |
| 19 | Configuration editing loses schema-specific controls and layer clarity | US-031 |
| 20 | Proxy configuration omits bypass and certificate-directory variables | US-031 |
| 21 | MCP OAuth and per-tool toggles are rejected by the current backend | US-032 |
| 22 | Vibe Code project and Teleport flows are command/JSON driven instead of picker/status driven | US-033 |
| 23 | Tool history is reduced to generic effect regions | US-034 |
| 24 | Failed or cancelled effects can render as completed | US-034 |
| 25 | Turn errors lose reference classification and recovery guidance | US-035 |
| 26 | Transcript selection, copy, and safe clickable links are incomplete | US-036 |
| 27 | Loading activity and context usage are stale until turn completion | US-035 |
| 28 | Focus-aware completion and action-required notifications are absent | US-038 |
| 29 | Narrator preference is persisted but has no runtime effect | US-038 |
| 30 | Theme selection is limited to three values without reference preview | US-039 |
| 31 | Debug console is a fixed snapshot instead of a live paginated view | US-036 |
| 32 | Scheduled-loop output is compact JSON instead of reference messages and tables | US-033 |
| 33 | Exit summary is absent and `ask_confirmation_on_exit` is ignored | US-039 |
| 34 | `Ctrl+Z` suspend is absent | US-039 |

## Quality Gates

These gates apply proportionately to each story and completely to each epic:

- `cargo +stable fmt --all -- --check` - verify workspace formatting.
- `cargo +stable check --workspace --all-targets --all-features` - compile all targets and feature combinations.
- `cargo +stable clippy --workspace --all-targets --all-features -- -D warnings` - enforce the workspace lint policy with zero warnings.
- `cargo +stable test --workspace --all-features` - run the integrated release suite.
- Differential oracle traces pinned to the Python SHA - reject unknown fields, mixed revisions, and unexplained skips.
- Pure runtime reducer tests - cover event ordering, stale identities, overlay priority, queue ownership, cancellation, and resynchronization.
- Ratatui `Buffer` and `TestBackend` assertions at 40, 80, and 120 columns - cover every new overlay and transcript semantic region.
- Linux PTY scenarios - cover startup intent, trust abort, resize, focus, suspend, signals, external effects, and terminal restoration.
- Injected failure tests - cover filesystem, Git, server, network, clipboard, URL opener, notification, audio, update cache, and external-service ports.

## Epics & User Stories

### EP-006: Secure Startup and Invocation

Align trust, worktree isolation, initial actions, startup diagnostics, and resume intent before the main runtime accepts input.

**Definition of Done:** The same working directory, CLI arguments, trust state, saved sessions, and startup failures open, resume, select, abort, or close the same interactive flow as Python, with no project-scoped effect before trust.

**Mistral Vibe source navigation:** [startup.py:87](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/startup.py:87), [cli.py:248](/home/arthur/dev/mistral-vibe/vibe/cli/cli.py:248), [entrypoint.py:284](/home/arthur/dev/mistral-vibe/vibe/cli/entrypoint.py:284), [app.py:804](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:804)

#### US-020: Gate startup on workspace trust and location safety

**Description:** As a terminal operator, I want workspace trust and sensitive-location warnings resolved before project capabilities load so that Rust preserves the reference safety boundary.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by Phase A US-003

**Mistral Vibe source navigation:** [startup.py:87](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/startup.py:87), [trusted_folders.py:119](/home/arthur/dev/mistral-vibe/vibe/core/trusted_folders.py:119), [app.py:830](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:830), [app.py:3774](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:3774)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/mod.rs`, `crates/vibe-cli/src/tui/setup.rs`, `crates/vibe-app-server/src/release3.rs`, `crates/vibe-app-server/src/resources.rs`, `crates/vibe-cli/tests/tui_pty.rs`.

**Acceptance Criteria:**

- [x] Given an unknown, explicitly untrusted, repository-root, or sensitive workspace without an override, when interactive startup begins, then the matching trust or location warning appears before session or project discovery.
- [x] Given a supported trust decision, when the user confirms it, then the exact scope is persisted once and the runtime is built with only the capabilities permitted by that decision.
- [x] Given cancellation, malformed trust details, persistence failure, or terminal abort, when startup resolves, then no session or project-scoped effect starts and terminal modes are restored.

#### US-021: Execute sessions inside the requested worktree

**Description:** As a terminal operator, I want `--worktree NAME` to prepare, enter, and safely clean the same isolated Git worktree as Python so that agent file effects never target the original checkout by mistake.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-020

**Mistral Vibe source navigation:** [entrypoint.py:284](/home/arthur/dev/mistral-vibe/vibe/cli/entrypoint.py:284), [entrypoint.py:327](/home/arthur/dev/mistral-vibe/vibe/cli/entrypoint.py:327), [worktree.py:62](/home/arthur/dev/mistral-vibe/vibe/core/worktree.py:62), [worktree.py:133](/home/arthur/dev/mistral-vibe/vibe/core/worktree.py:133)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/main.rs`, `crates/vibe-cli/src/lib.rs`, `crates/vibe-cli/src/tui/mod.rs`, a focused worktree adapter under `crates/vibe-cli/src`, and isolated tests.

**Acceptance Criteria:**

- [x] Given a valid name inside a Git repository, when `--worktree` is used, then Rust creates or reuses the reference worktree, changes the effective working directory before trust and session discovery, and exposes that path to all tools.
- [x] Given a worktree created by this invocation, when a started session exits, then clean and dirty, ahead, reused, and pre-existing-branch cleanup choices match Python and execute at most once.
- [x] Given an invalid name, non-Git directory, conflicting path, Git failure, exit before session start, or declined cleanup, when handled, then the original checkout is unchanged and the reported result matches the reference.

#### US-022: Match interactive prompt and startup action routing

**Description:** As a terminal operator, I want positional, stdin, and Teleport startup intent to remain inside the interactive TUI so that supplying initial work does not silently change the execution model.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-020, US-021

**Mistral Vibe source navigation:** [cli.py:53](/home/arthur/dev/mistral-vibe/vibe/cli/cli.py:53), [cli.py:248](/home/arthur/dev/mistral-vibe/vibe/cli/cli.py:248), [app.py:874](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:874), [app.py:951](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:951)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/main.rs`, `crates/vibe-cli/src/lib.rs`, `crates/vibe-cli/src/tui/mod.rs`, `crates/vibe-cli/tests/tui_pty.rs`.

**Acceptance Criteria:**

- [x] Given an interactive terminal and a positional or stdin prompt, when startup settles, then the TUI mounts before dispatch and the prompt enters through the same idle-input path as Python.
- [x] Given startup Teleport intent with or without a prompt, when the command is available, then project resolution and Teleport begin after mount with the same visible initial state and without an agent turn.
- [x] Given empty stdin, incompatible flags, unavailable Teleport, or failure before mount, when intent resolves, then no hidden headless switch or partial submission occurs and the error identifies the failed intent.

#### US-023: Match resume intent and visible initialization

**Description:** As a returning operator, I want direct resume, bare resume, session selection, and initialization failures to follow Python so that startup never opens an unintended session or fails outside the interaction surface.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-020

**Mistral Vibe source navigation:** [startup.py:99](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/startup.py:99), [session_picker.py:98](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/session_picker.py:98), [app.py:804](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:804), [app.py:877](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:877)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/setup.rs`, `crates/vibe-cli/src/tui/pickers.rs`, `crates/vibe-cli/src/tui/workflow.rs`, `crates/vibe-cli/src/tui/mod.rs`, `crates/vibe-core/src/storage.rs`.

**Acceptance Criteria:**

- [x] Given `--resume <id>` or continue intent, when startup succeeds, then the identified or latest eligible session opens directly with reference-equivalent resumed state.
- [x] Given bare `--resume`, when saved sessions exist, then the picker preserves reference ordering, directory context, preview, selection, cancellation, and start-new behavior.
- [x] Given no sessions, ambiguous or missing IDs, configuration issues, MCP initialization failure, or fatal host error, when handled, then Rust starts new, remains recoverable, or closes exactly as Python and never resumes another session implicitly.

---

### EP-007: Callback and Active-Turn Control

Make approvals, questions, plan review, queued intent, shell streaming, interruption, and resynchronization deterministic.

**Definition of Done:** Active and pending callbacks are serialized in arrival order, queued prompt and shell work drains with one owner, plan state remains synchronized, and cancellation produces the same finalized transcript and recoverable state as Python.

**Mistral Vibe source navigation:** [app.py:1749](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:1749), [app.py:1791](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:1791), [message_queue.py:40](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/message_queue.py:40), [event_handler.py:258](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/handlers/event_handler.py:258)

#### US-024: Implement informed approval interactions

**Description:** As a terminal operator, I want approvals to show the effect, required permissions, specialized details, and all scopes so that consent is informed and cannot be triggered by stale typing.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-023

**Mistral Vibe source navigation:** [approval_app.py:85](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/approval_app.py:85), [approval_app.py:172](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/approval_app.py:172), [tool_widgets.py:459](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/tool_widgets.py:459), [app.py:1812](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:1812)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/controls.rs`, `crates/vibe-cli/src/tui/state.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [x] Given an approval callback, when it becomes active, then tool-specific command, arguments or diff, permission labels, and one-time, session, permanent, and deny choices match Python.
- [x] Given a choice after the grace period, when submitted, then session, turn, callback, permission scope, and retry identities are preserved and exactly one response is sent.
- [x] Given early input, Escape, duplicate or stale callback, response rejection, or resync, when handled, then approval fails closed and the wrong turn or permission policy is never mutated.

#### US-025: Implement structured questions and FIFO callback ownership

**Description:** As a terminal operator, I want callbacks processed in arrival order and questions rendered as structured single, multiple, and free-form inputs so that ordinary prompts cannot be consumed as answers.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-024

**Mistral Vibe source navigation:** [app.py:1749](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:1749), [app.py:1773](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:1773), [question_app.py:54](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/question_app.py:54), [question_app.py:224](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/question_app.py:224)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/controls.rs`, `crates/vibe-cli/src/tui/interaction.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [x] Given callbacks arriving in any identifier order, when overlays are scheduled, then one callback is active at a time and pending callbacks activate FIFO by arrival.
- [x] Given one or multiple questions, when answered, then tabs, cursor restoration, single-select, multi-select, Other text, shortcuts, ordered payload, and cancellation match Python.
- [x] Given active composer typing, empty required Other text, incomplete selection, grace-period input, stale callback, or server failure, when handled, then no ordinary prompt or partial answer is emitted as a callback response.

#### US-026: Preserve live plan-review state and editor actions

**Description:** As a terminal operator, I want plan review bound to its live file with explicit review completion and editor access so that the plan cannot diverge from what the agent is waiting on.

**Priority:** P0  
**Size:** M (3 pts)  
**Dependencies:** Blocked by US-025

**Mistral Vibe source navigation:** [event_handler.py:258](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/handlers/event_handler.py:258), [event_handler.py:413](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/handlers/event_handler.py:413), [app.py:3973](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:3973)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/controls.rs`, `crates/vibe-cli/src/tui/state.rs`, `crates/vibe-cli/src/tui/workflow.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [x] Given `PlanReviewStarted` with a file path, when reduced, then the active plan path and current contents remain visible and file changes refresh the review state without starting another turn.
- [x] Given the external-editor shortcut or a review choice, when activated, then editor and callback effects preserve plan and callback identity and settle once.
- [x] Given a missing file, watcher failure, editor failure, stale end event, or cancellation, when handled, then no obsolete plan is approved and the composer returns to a recoverable state.

#### US-027: Match typed queue, grouping, presentation, and command gating

**Description:** As a terminal operator, I want deferred prompts and shell commands to preserve type, order, grouping, and visible content while unsafe commands remain blocked during active work.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-025 and Phase A US-013

**Mistral Vibe source navigation:** [message_queue.py:40](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/message_queue.py:40), [message_queue.py:199](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/message_queue.py:199), [message_queue.py:314](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/message_queue.py:314), [app.py:1043](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:1043)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/interaction.rs`, `crates/vibe-cli/src/tui/workflow.rs`, `crates/vibe-cli/src/tui/state.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/commands.rs`.

**Acceptance Criteria:**

- [x] Given busy agent or shell work, when prompts, prepared prompts, images, skills, or shell commands are submitted, then typed items and visible pending rows append FIFO with the reference queue header and content.
- [x] Given the runtime becomes eligible, when drainage starts, then one owner groups consecutive prompts into the reference turn, preserves shell boundaries and IDs, and continues in FIFO order.
- [x] Given slash or Teleport input while busy, pause, pop-last, unsupported images, callback activity, cancellation, shutdown, or injection failure, when handled, then execution, rejection, retention, or requeue behavior matches Python without duplication or loss.

#### US-028: Finalize cancellation and stream local shell output

**Description:** As a terminal operator, I want active work to stream progress and cancel exactly once with a truthful terminal marker so that I can continue without stale or hidden state.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-027

**Mistral Vibe source navigation:** [app.py:1620](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:1620), [app.py:2285](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:2285), [messages.py:464](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/messages.py:464), [event_handler.py:176](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/handlers/event_handler.py:176)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/shell.rs`, `crates/vibe-cli/src/tui/state.rs`, `crates/vibe-cli/src/tui/controls.rs`, `crates/vibe-cli/src/tui/workflow.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [x] Given a running local shell command, when output chunks arrive, then stdout and stderr update the same transcript entry before process termination and remain scrollable.
- [x] Given active agent, shell, callback, plan, or queue work, when cancellation occurs, then only the highest-priority identity receives one cancel effect and final state includes the reference interrupted or cancelled marker.
- [x] Given duplicate cancel, late stream patch, rejection, disconnect, process kill failure, or shutdown, when handled, then stale effects cannot revive work, queued intent is preserved where eligible, and terminal modes remain valid.

---

### EP-008: Reversible Session Management

Align rewind and session deletion with reference safeguards and recovery paths.

**Definition of Done:** Operators can choose a rewind target and file restoration explicitly, cancel without mutation, and delete only eligible sessions after confirmation with recoverable failure.

**Mistral Vibe source navigation:** [app.py:3208](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:3208), [rewind_app.py:80](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/rewind_app.py:80), [session_picker.py:295](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/session_picker.py:295)

#### US-029: Match rewind navigation and file restoration choice

**Description:** As a terminal operator, I want rewind to distinguish conversation editing from workspace restoration so that reversing one state never silently leaves the other inconsistent.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-028

**Mistral Vibe source navigation:** [app.py:3208](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:3208), [app.py:3356](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:3356), [rewind_app.py:80](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/rewind_app.py:80)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/workflow.rs`, `crates/vibe-cli/src/tui/interaction.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [x] Given editable user messages, when rewind mode opens, then previous and next navigation, preview, scroll, cancel, and selection preserve the original transcript until acceptance.
- [x] Given a target with file changes, when options render, then restore-and-edit and edit-without-restore are distinct; without file changes only the applicable edit action appears.
- [x] Given acceptance, cancellation, invalid target, server failure, or file-restoration failure, when handled, then the selected restore flag is sent once and failure leaves transcript and workspace unchanged with a visible error.

#### US-030: Require confirmed and recoverable session deletion

**Description:** As a returning operator, I want session deletion to require the same confirmation and active-session guard as Python so that one accidental key cannot erase history.

**Priority:** P0  
**Size:** M (3 pts)  
**Dependencies:** Blocked by US-023

**Mistral Vibe source navigation:** [session_picker.py:295](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/session_picker.py:295), [startup.py:99](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/startup.py:99)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/pickers.rs`, `crates/vibe-cli/src/tui/workflow.rs`, `crates/vibe-cli/src/tui/render.rs`.

**Acceptance Criteria:**

- [x] Given a highlighted non-active session, when delete is requested once, then the row enters confirmation without server mutation; the second matching request sends exactly one delete effect.
- [x] Given the active session, changed selection, Escape, or picker cancellation, when confirmation exists, then deletion is blocked or cleared before any broader action.
- [x] Given server failure or deletion of the final saved session, when the result returns, then failure restores the row while success removes it and returns the reference start-new result where applicable.

---

### EP-009: Configuration and External Integrations

Make typed settings, proxy, MCP, Vibe Code, Teleport, and scheduled-loop workflows equivalent through explicit resource contracts.

**Definition of Done:** Settings target valid layers and types, proxy fields preserve the reference environment contract, MCP supports auth and tool control, and remote or scheduled operations present structured progress and errors instead of raw JSON.

**Mistral Vibe source navigation:** [config_screen.py:174](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/screens/config/config_screen.py:174), [proxy_setup.py:7](/home/arthur/dev/mistral-vibe/vibe/core/proxy_setup.py:7), [mcp_app.py:176](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/mcp_app.py:176), [app.py:2065](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:2065)

#### US-031: Implement typed configuration, layers, and complete proxy settings

**Description:** As a terminal operator, I want schema-aware settings and complete proxy controls saved to an explicit layer so that valid configuration produces the same effective environment as Python.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-020, US-023

**Mistral Vibe source navigation:** [config_screen.py:174](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/screens/config/config_screen.py:174), [edit.py:183](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/screens/config/edit.py:183), [proxy_setup.py:7](/home/arthur/dev/mistral-vibe/vibe/core/proxy_setup.py:7), [proxy_setup_app.py:40](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/proxy_setup_app.py:40)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/workflow/config.rs`, `crates/vibe-cli/src/tui/pickers.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-app-server/src/resources.rs`.

**Acceptance Criteria:**

- [x] Given introspected fields, when settings open, then search, categories, effective values, source layers, booleans, enums, numbers, strings, lists, objects, optional values, and descriptions are keyboard operable.
- [x] Given a valid user or project edit, when saved, then the typed patch and expected fingerprint are sent once; HTTP, HTTPS, ALL, NO_PROXY, SSL certificate file, and certificate directory produce the same persisted environment contract as Python.
- [x] Given invalid input, unsupported or secret field, untrusted project layer, stale fingerprint, invalid proxy key, server rejection, or refresh failure, when handled, then no false success appears and current values remain recoverable.

#### US-032: Implement MCP and connector lifecycle with authentication

**Description:** As a terminal operator, I want MCP servers and connectors listed, toggled, configured, and authenticated like Python so that external tools expose the same visible lifecycle.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-020, US-031

**Mistral Vibe source navigation:** [mcp_app.py:176](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/mcp_app.py:176), [mcp_oauth_app.py:43](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/mcp_oauth_app.py:43), [connector_auth_app.py:39](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/connector_auth_app.py:39), [app.py:2388](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:2388)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/workflow/mcp.rs`, `crates/vibe-cli/src/tui/pickers.rs`, `crates/vibe-cli/src/tui/mod.rs`, `crates/vibe-app-server/src/resources.rs`.

**Acceptance Criteria:**

- [x] Given servers and connectors, when MCP management opens, then deterministic groups, source details, connection, enabled, needs-auth, needs-setup, unavailable, disabled, and per-tool states match Python.
- [x] Given toggle, setup, connector auth, or MCP OAuth, when selected, then the correct source receives the effect and OAuth supports open, copy, show URL, completion refresh, logout, and close behavior.
- [x] Given an untrusted workspace, unknown source, malformed or expired URL, opener or clipboard failure, auth cancellation, unsupported backend capability, server error, or stale refresh, when handled, then nothing is enabled implicitly and the overlay remains recoverable.

#### US-033: Match Vibe Code, Teleport, and scheduled-loop workflows

**Description:** As a terminal operator, I want remote projects, Teleport, and loops presented as structured workflows so that I can select targets, follow progress, approve pushes, and inspect schedules without parsing JSON.

**Priority:** P1  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-025, US-031

**Mistral Vibe source navigation:** [app.py:2065](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:2065), [app.py:2086](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:2086), [app.py:2190](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:2190), [scheduled_loop_runner.py:31](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/scheduled_loop_runner.py:31)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/pickers.rs`, `crates/vibe-cli/src/tui/workflow.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [x] Given linked, saved, stale, or multiple Vibe Code projects, when selection or Teleport starts, then target resolution, picker state, progress messages, saved-link behavior, and push approval match Python.
- [x] Given loop create, list, cancel-one, or cancel-all, when complete, then intervals, next-run durations, prompts, IDs, counts, and success messages render in the reference table or message form.
- [x] Given no project, stale link, network failure, rejected push, Teleport cancellation, malformed interval, missing loop ID, or server failure, when handled, then raw event JSON is not the primary UI and the workflow stays recoverable.

---

### EP-010: Semantic Transcript and Observability

Project canonical history into truthful semantic regions with actionable errors, live progress, transcript interaction, and diagnostic continuity.

**Definition of Done:** Every audited event and error has stable reference-equivalent presentation, failed effects cannot appear successful, live state updates before turn completion, and long transcripts or logs remain inspectable under resize and external-effect failure.

**Mistral Vibe source navigation:** [event_handler.py:176](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/handlers/event_handler.py:176), [tool_widgets.py:459](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/tool_widgets.py:459), [app.py:2019](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:2019), [debug_console.py:188](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/debug_console.py:188)

#### US-034: Render semantic history and authoritative terminal statuses

**Description:** As a terminal operator, I want messages, tools, diffs, notices, and terminal states rendered by meaning so that failed or cancelled work can never look completed.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-028

**Mistral Vibe source navigation:** [event_handler.py:176](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/handlers/event_handler.py:176), [event_handler.py:443](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/handlers/event_handler.py:443), [tools.py:287](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/tools.py:287), [tool_widgets.py:459](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/tool_widgets.py:459)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/state.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/render/markdown.rs`, `crates/vibe-cli/src/tui/mod.rs`, `crates/vibe-core/src/events.rs`.

**Acceptance Criteria:**

- [x] Given added or updated history, when projected, then user, assistant, reasoning, shell, read, write, edit, grep, todo, question, web, diff, hook, loop, compaction, context, Teleport, checkpoint, and notice regions appear in reference order with equivalent labels and grouping.
- [x] Given pending, running, blocked, completed, failed, cancelled, skipped, or expired effects, when rendered, then the authoritative nested state determines status and default collapse behavior matches Python.
- [x] Given duplicate, out-of-order, malformed, unknown, resync, or late terminal events, when reduced, then known content is not duplicated or dropped and no failed, cancelled, or unknown effect is represented as success.

#### US-035: Render actionable errors, live activity, and live context usage

**Description:** As a terminal operator, I want failures classified with recovery guidance and progress updated during the turn so that I can distinguish waiting, retrying, context pressure, and terminal failure.

**Priority:** P0  
**Size:** M (3 pts)  
**Dependencies:** Blocked by US-034

**Mistral Vibe source navigation:** [app.py:845](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:845), [app.py:1884](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:1884), [app.py:2019](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:2019), [event_handler.py:158](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/handlers/event_handler.py:158)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/state.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [x] Given cancellation, context exhaustion, refusal, auth, rate limit, transport, model, server, shell, or tool failure, when rendered, then severity, deduplication, muting, message, and recovery hint match the reference semantic class.
- [x] Given streaming stats and effect updates, when they arrive, then activity text, active effect, token count, context percentage, warning state, and loading visibility update before turn completion.
- [x] Given unknown error code, malformed details, duplicate tool and turn error, retry, resume, or resync, when handled, then no secret or raw payload leaks, important failure remains visible, and stale loading state clears without deleting evidence.

#### US-036: Match transcript interaction and live debug diagnostics

**Description:** As a terminal operator, I want transcript selection, links, scrolling, resize, and a live paginated debug console so that long sessions remain inspectable and external actions remain safe.

**Priority:** P1  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-034 and Phase A US-005

**Mistral Vibe source navigation:** [app.py:3941](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:3941), [links.py:33](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/links.py:33), [debug_console.py:163](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/debug_console.py:163), [debug_console.py:188](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/debug_console.py:188)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/interaction.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/clipboard.rs`, `crates/vibe-cli/src/tui/pickers.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [x] Given a transcript larger than the viewport, when keyboard or mouse scroll, collapse, selection, copy, auto-copy, load-more, or resize occurs, then anchor, bounds, focus, selected text, and visible status match Python.
- [x] Given a safe URL, file result, tool output, or debug entry, when activated, then link opening or copy occurs only through validated effects; debug logs poll, paginate older entries, and preserve selection.
- [x] Given a tiny viewport, invalid URL, missing opener, clipboard denial, disappearing entry, log read failure, or resize during streaming, when handled, then state remains valid and the scoped error does not discard selection or history.

---

### EP-011: Terminal Services and Lifecycle

Close update, notification, narration, theme, suspend, and exit-contract gaps without making external services or terminal capabilities mandatory for core sessions.

**Definition of Done:** Update state is authoritative and non-blocking, attention services honor focus and preference, narration has deterministic cancellation, themes expose the reference choices, and all normal or abnormal exits restore the terminal and print the expected summary.

**Mistral Vibe source navigation:** [cli.py:343](/home/arthur/dev/mistral-vibe/vibe/cli/cli.py:343), [app.py:1780](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:1780), [app.py:1912](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:1912), [app.py:3980](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:3980)

#### US-037: Implement update discovery and What's New behavior

**Description:** As a terminal operator, I want forced and background update checks plus release-note state to follow Python so that update intent has a truthful result without blocking normal startup.

**Priority:** P1  
**Size:** M (3 pts)  
**Dependencies:** Blocked by US-023, US-034

**Mistral Vibe source navigation:** [cli.py:343](/home/arthur/dev/mistral-vibe/vibe/cli/cli.py:343), [cli.py:395](/home/arthur/dev/mistral-vibe/vibe/cli/cli.py:395), [app.py:3800](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:3800), [app.py:3920](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:3920)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/main.rs`, `crates/vibe-cli/src/tui/setup.rs`, `crates/vibe-cli/src/tui/state.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/mod.rs`.

**Acceptance Criteria:**

- [x] Given `--check-upgrade`, when discovery resolves, then Rust reports current or available version through the reference prompt and exits without starting a session.
- [x] Given background discovery or unseen release notes, when the TUI settles, then cached and fresh state, current/latest version, What's New message, dismissal, and `/update` affordance match Python without blocking input.
- [x] Given offline operation, timeout, malformed version, corrupt cache, gateway failure, dismissed or current version, or update-command failure, when handled, then startup remains usable and no stale or invalid version becomes authoritative.

#### US-038: Implement focus-aware notifications and narrator lifecycle

**Description:** As a terminal operator, I want background turns to request attention and eligible completed turns to be narrated according to preference so that long-running sessions remain observable outside the focused terminal.

**Priority:** P1  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-028, US-034

**Mistral Vibe source navigation:** [app.py:1780](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:1780), [app.py:1912](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:1912), [app.py:1929](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:1929), [app.py:2017](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:2017)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/terminal.rs`, `crates/vibe-cli/src/tui/state.rs`, `crates/vibe-cli/src/tui/workflow.rs`, `crates/vibe-cli/src/tui/render.rs`, existing audio and model ports.

**Acceptance Criteria:**

- [x] Given notifications enabled and focus lost, when a turn completes or requests action, then the reference title, bell, context, throttle, and focus reset effects occur exactly once; focused or disabled states emit none.
- [x] Given narrator enabled and an eligible turn lifecycle, when user and assistant events arrive, then Idle, Summarizing, Speaking, stop, cancel, disable, and shutdown behavior matches Python with identity-based late-result rejection.
- [x] Given missing focus support, terminal write failure, rapid focus changes, summarizer failure, audio device or permission failure, or shutdown, when handled, then the core turn remains successful, terminal escape state stays valid, and prompts, audio, and secrets do not leak to diagnostics.

#### US-039: Match themes, suspend, exit confirmation, and session summary

**Description:** As a terminal operator, I want terminal preferences and exit behavior to follow Python so that visual choice, shell suspension, confirmation, cleanup, and resume information remain predictable.

**Priority:** P2  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-031, US-038

**Mistral Vibe source navigation:** [theme_picker.py:23](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/theme_picker.py:23), [app.py:3711](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:3711), [app.py:3980](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:3980), [session_exit.py:19](/home/arthur/dev/mistral-vibe/vibe/cli/session_exit.py:19)  
**Probable Rust delivery surfaces:** `crates/vibe-cli/src/tui/terminal.rs`, `crates/vibe-cli/src/tui/pickers.rs`, `crates/vibe-cli/src/tui/render.rs`, `crates/vibe-cli/src/tui/mod.rs`, `crates/vibe-cli/src/main.rs`.

**Acceptance Criteria:**

- [x] Given theme selection, when the picker opens, then every supported reference theme is searchable or navigable, previews before confirmation, persists on acceptance, and restores the original theme on cancellation.
- [x] Given Unix suspend capability, exit preference, active work, or normal completion, when `Ctrl+Z`, `Ctrl+D`, quit, or host completion occurs, then suspend, confirmation ladder, cleanup, token summary, and resume command match Python.
- [x] Given unsupported suspend, terminal failure, summary calculation failure, repeated exit, signal, panic, or cleanup adapter failure, when handled, then no duplicate exit effect occurs and every enabled terminal mode is restored before bounded diagnostics print.

## Functional Requirements

- FR-01: Interactive startup must resolve location safety and workspace trust before every project-scoped or session-scoped effect.
- FR-02: `--worktree` must prepare or reuse the named Git worktree, set it as the effective working directory, and apply reference cleanup ownership.
- FR-03: Positional, stdin, and Teleport initial intent must remain interactive unless an explicit non-interactive mode is selected.
- FR-04: Bare resume must open a picker; identified and continue intent must resolve directly according to the reference.
- FR-05: Initialization, configuration, MCP, and fatal startup state must be visible through the reference interaction flow.
- FR-06: Runtime state must serialize callbacks FIFO and expose one deterministic overlay priority.
- FR-07: Approval callbacks must show specialized effects, permissions, scopes, and grace-period protection.
- FR-08: Question callbacks must support multiple questions, single and multiple selection, Other text, cancellation, and exact ordered answers.
- FR-09: Plan review must retain its file identity, refresh on changes, and expose the reference editor action.
- FR-10: The queue must preserve typed prompt, prepared prompt, image, skill, and shell items, render their content, group consecutive prompts, and gate commands while busy.
- FR-11: Cancellation must target one active identity, settle streaming and callbacks once, preserve eligible queued intent, and render a persistent terminal marker.
- FR-12: Local shell output must stream before terminal completion.
- FR-13: Rewind must expose conditional file restoration, editable-target navigation, cancellation, and recoverable failure.
- FR-14: Session deletion must require the reference confirmation sequence and block the active session.
- FR-15: Configuration must render typed fields, effective layers, complete proxy variables, validation, and optimistic fingerprints.
- FR-16: MCP and connectors must expose reference statuses, per-tool toggles, setup, OAuth, connector auth, URL actions, refresh, and logout.
- FR-17: Vibe Code, Teleport, and scheduled loops must expose structured selection, progress, approval, success, and failure state.
- FR-18: Canonical history must project into semantic transcript regions with authoritative effect status, stable ordering, grouping, collapse, and resync.
- FR-19: Turn and tool errors must be classified, deduplicated, bounded, non-secret, and paired with reference recovery hints.
- FR-20: Activity, active effect, token count, and context pressure must update during the turn.
- FR-21: Transcript interaction must preserve scroll anchors, selection, copy, links, resize, and live paginated debug state.
- FR-22: Forced and background update discovery must be asynchronous, cached, dismissible, and non-blocking on failure.
- FR-23: Notifications must honor focus and preference; narration must use explicit summary and playback effects with identity-based cancellation.
- FR-24: Themes, suspend, exit confirmation, terminal restoration, session summary, and resume output must match the reference contract.
- FR-25: Every new behavior must have a pinned Python trace and reject unknown schema or unexplained skips.

## Non-Functional Requirements

- **Performance:** P99 non-I/O runtime transition latency must stay below 1 ms over 10,000 reducer iterations; P95 render time must stay below 50 ms over 200 frames with 10,000 transcript entries; no reducer or render function may perform synchronous external work.
- **Security:** 100% of unknown-workspace traces must show zero project-scoped effects before trust; 100% of invalid callback traces must fail closed; 100% of failed or cancelled effect traces must retain non-success status; secrets, OAuth URLs after closure, prompts, tool payloads, audio, and clipboard content must not enter diagnostics.
- **Accessibility:** 100% of overlays and transcript actions must be keyboard operable; trust, approval, denial, pending, failure, focus, update, and narrator states must include text or symbols in addition to color; mouse support must not be required.
- **Scalability:** Session, config, MCP, and project pickers must remain within the render target at 1,000 items; transcript and debug windowing must remain bounded at 10,000 entries.
- **Reliability:** Zero duplicate callback response, queue injection, cancellation, notification, narrator completion, delete, or rewind effect may occur in 1,000 randomized out-of-order sequences; 100% of defined PTY exits must restore raw mode, alternate screen, mouse, focus, paste, and title state.
- **Portability:** Core reducer and buffer tests must be platform-independent; Linux PTY coverage is mandatory, with injected macOS and Windows adapter tests for unsupported native behaviors.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Trust cancelled | Escape, Ctrl+C, closed dialog, or persistence failure | Start no session, load no project capability, restore terminal | Reference cancellation or scoped failure |
| 2 | Worktree unsafe or unavailable | Invalid name, non-Git directory, conflicting path, dirty cleanup | Leave original checkout untouched and preserve or remove only according to ownership | `Worktree could not be prepared: {reason}` |
| 3 | Conflicting invocation | Interactive prompt plus incompatible explicit mode | Reject before mount or submission | `Conflicting invocation modes` |
| 4 | Empty or ambiguous resume | Bare resume with zero sessions or non-unique selector | Start new or show picker/error exactly as Python, never choose implicitly | Scoped session message |
| 5 | Stale callback | Response after callback, turn, or session changed | Emit no response, resync canonical state | `Callback is no longer active` |
| 6 | Incomplete question | Missing required free text or selection | Keep callback open, emit no partial answer | Reference inline guidance |
| 7 | Queue interruption | Callback, cancellation, model restriction, or shutdown during drain | Pause or requeue uncommitted items exactly once | Scoped queue warning where applicable |
| 8 | Late stream after cancel | History or shell patch arrives after terminal state | Ignore by identity or resync without reviving work | None |
| 9 | Rewind failure | Invalid target, server rejection, or file restore failure | Leave transcript and workspace unchanged | `Rewind failed: {reason}` |
| 10 | Active session deletion | Delete requested on current session | Perform no mutation and render active-session feedback | Reference active-session message |
| 11 | Config or proxy conflict | Invalid typed value, stale fingerprint, unsupported variable | Reject write and retain recoverable current values | Scoped validation error |
| 12 | Expired auth URL | OAuth closes or refreshes before open/copy | Do not expose or use stale credentials; refresh state | `Authentication link expired` |
| 13 | Remote dependency failure | Vibe Code, Teleport, loop, update, notification, or narrator service unavailable | Keep core session usable and preserve retryable intent | Scoped non-secret error or silent reference fallback |
| 14 | Unknown history state | Malformed event or unsupported effect status | Preserve known content and never render success | Explicit bounded fallback only where reference does |
| 15 | External action failure | Clipboard, URL opener, editor, notification, or audio adapter fails | Preserve transcript, selection, and core turn | `Could not {action}: {reason}` |
| 16 | Tiny or resized terminal | Width or height below normal layout during streaming | Keep state valid, preserve anchor, render bounded fallback | None |
| 17 | Missing focus or suspend support | Terminal emits no focus events or cannot suspend | Suppress unsupported effect without corrupting terminal | Reference unsupported behavior |
| 18 | Abnormal exit | Signal, panic, disconnect, or cleanup failure after modes enabled | Restore every enabled mode before diagnostics | Diagnostic only if restoration fails |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Runtime reducer becomes a whole-TUI rewrite | Medium | High | Limit state to audited observations, reuse adapters, migrate dependency-ordered slices, require a trace for every moved rule |
| 2 | Trust still occurs after immutable project discovery | Medium | Critical | Make trust the first startup effect and assert zero pre-trust project/resource calls in every negative trace |
| 3 | Callback, queue, or cancellation races duplicate or reorder intent | High | High | Carry session, turn, callback, effect, client, and generation IDs; enforce one overlay and one drain owner; run randomized sequence tests |
| 4 | Worktree cleanup removes user-owned state | Low | Critical | Track created versus reused worktrees and branches explicitly; default to keeping uncertain state; test every cleanup branch |
| 5 | Resync overwrites local overlay, queue, focus, scroll, or selection | Medium | High | Separate canonical and local ownership and test resync during every overlay and external effect |
| 6 | OAuth or trust parity requires a cross-crate contract change | High | High | Start from existing resources, attach a failing fixture, then make the smallest reviewed boundary change rather than adding a fallback |
| 7 | Semantic projection hides failure or duplicates errors | Medium | High | Treat terminal effect state as authoritative, encode reference muting rules, and keep unknown failure visible and bounded |
| 8 | Terminal integrations emit unsafe escapes or fail cleanup | Medium | High | Centralize validated effects, track enabled modes and original title, PTY-test every exit |
| 9 | Reference behavior changes during delivery | Medium | High | Pin the full SHA, reject mixed traces, and update only through a reviewed changelog and scoped parity decision |

## Non-Goals

- Refactoring the entire CLI, app server, protocol, agent loop, core history model, or workspace architecture.
- Structural, class, widget, or line-for-line equivalence with Python.
- Adding commands, callback kinds, config fields, MCP capabilities, remote operations, update channels, notification transports, or narrator features absent from the pinned reference.
- Byte-identical ANSI output across terminals.
- Automatically installing updates or publishing releases.
- Redesigning the phase A chat composer or reopening its completed stories without a new failing runtime fixture.
- Keeping parallel legacy and parity paths after an owning story lands.

## Files NOT to Modify

- `/home/arthur/dev/mistral-vibe/**` - read-only behavioral oracle.
- `crates/vibe-cli/src/tui/chat_input.rs`, `crates/vibe-cli/src/tui/input.rs`, and `crates/vibe-cli/src/tui/completion.rs` - phase A contracts; modify only if a runtime fixture proves a direct dependency.
- `crates/vibe-protocol/**` - modify only after a failing fixture proves the public resource or callback contract insufficient.
- `Cargo.toml` and `Cargo.lock` - no dependency changes without a separate explicit decision.
- User sessions, memories, config, trust records, worktrees, update caches, credentials, audio files, plugin caches, and other app-managed state - tests use isolated temporary roots and injected ports only.

## Technical Considerations

- **Architecture:** Should runtime coordination become a focused `RuntimeState + RuntimeEvent -> Vec<RuntimeEffect>` module inside `vibe-cli`? Recommended: yes, while keeping `tui/mod.rs` as event collection and effect execution.
- **State ownership:** Which state is canonical versus local? Recommended: app-server owns history and turn state; callback order, pending queue, overlays, focus, selection, scroll, update, notification, and narration presentation have explicit local ownership.
- **Effect identity:** Which effects require IDs or generations? Recommended: callbacks, cancellation, queue injection, config writes, MCP auth, remote operations, opener, clipboard, update, notification, summary, and audio effects.
- **Trust boot order:** What is the smallest host surface required before trust? Recommended: query and persist trust only, then build immutable project and session resources after resolution.
- **Worktree boundary:** Should worktree preparation live inside TUI state? Recommended: no; use a pre-runtime adapter that returns the effective directory and explicit cleanup ownership.
- **App-server changes:** When may `vibe-app-server` or `vibe-core` change? Recommended: only when the owning story has a pinned failing fixture proving an existing public boundary insufficient, notably immutable trust or MCP OAuth/tool toggles.
- **Rendering:** Which test level proves each behavior? Recommended: direct `Buffer` for semantic regions, `Terminal<TestBackend>` for layouts and resize, PTY for real input, focus, signals, suspend, external actions, and cleanup.
- **Migration:** How should existing branches move? Recommended: one dependency-ordered story at a time, delete the superseded path immediately after its traces pass, and avoid compatibility fallbacks not present in Python.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|--------------------|--------|-----------|-------------|
| Verified runtime gap pass rate | 0 of 34 formally complete | 34 of 34, zero accepted divergence | Month 6 | Audit matrix and differential manifest |
| Runtime parity trace determinism | No complete runtime corpus | 100% equal over 10 consecutive runs | Month 6 | Conformance runner |
| Pre-trust project effects | Runtime context built before an interactive trust decision | 0 across all unknown and untrusted scenarios | Month 1 onward | Startup effect assertions |
| Worktree routing errors | Option accepted without changing effective directory | 0 across create, reuse, failure, and cleanup scenarios | Month 1 onward | Temporary Git repository tests and PTY traces |
| False-success effect statuses | Failed/cancelled terminal states can map to completed | 0 across every terminal status variant | Month 1 onward | History projection manifest |
| Duplicate callback, queue, or cancel effects | Not systematically measured | 0 in 1,000 randomized out-of-order sequences | Month 1 onward | Reducer property tests |
| Unconfirmed destructive deletes | One-step deletion possible | 0 | Month 1 onward | Picker reducer and fake server tests |
| Semantic transcript coverage | Generic effect rendering for audited variants | 100% of pinned event and error variants | Month 6 | Manifest coverage and fixed-width buffer tests |
| Terminal teardown coverage | Partial PTY coverage | 100% of defined exits | Month 6 | PTY normal, abort, resize, suspend, signal, panic, and adapter-failure matrix |
| Interaction latency | No runtime reducer gate | P99 <1 ms reducer; P95 <50 ms render | Month 6 | Deterministic benchmark harness |

## Open Questions

- Can the current trust resource construct a new immutable project context after the pre-session decision? Owner: US-020 implementer, due before US-020 leaves TODO. A negative result permits the smallest app-server boundary change backed by the fixture.
- Which current public resource must change to support MCP OAuth and per-tool toggles? Owner: US-032 implementer, due before implementation. The answer must cite a failing reference trace and keep secrets out of history and diagnostics.
- Can current audio and model ports express narrator summarization, playback, cancellation, and late-result identity without a new dependency? Answered by US-038: summarization, cancellation, and late-result identity reuse the existing `narration/summarize` resource and a local reducer; playback cannot, because this port has no speech transport and the pinned reference synthesizes through the Mistral SDK audio API. The narrator therefore settles after summarizing and reports one bounded notice, recorded as an unavailable dimension in `crates/vibe-cli/tests/runtime-parity/terminal-services-ep011.json`. Restoring spoken output requires a separate speech-transport decision.
- Which native terminals form the final focus, title, bell, suspend, opener, and clipboard matrix? Owner: release owner, still open after EP-011. Linux PTY now covers focus reporting, the OSC title, suspend, exit, and restoration; `SIGTSTP` delivery itself stays environment-dependent because POSIX discards stop signals in an orphaned process group, so a native terminal check remains the only proof for real job control.
- Which Python behavior wins after the pinned revision changes? Owner: release owner, due when detected. Default: keep the pin, open a scoped parity update, and never regenerate fixtures silently.
[/PRD]
