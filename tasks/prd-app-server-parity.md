[PRD]
# PRD: App-Server Surface Parity

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-05 | Arthur Jean | Initial PRD from the measured app-server audit against the Python reference: 114 contract points with 87 reproduced, 7 invented names, 8 absent notifications, and an `initialize` handshake that rejects a conforming reference client |

## Problem Statement

1. **A conforming reference client cannot complete the handshake.** `ClientCapabilities` declares three fields upstream ([vibe/app_server/_connection_protocol.py:36](/home/arthur/dev/mistral-vibe/vibe/app_server/_connection_protocol.py)); the Rust struct declares two and carries `deny_unknown_fields` (`crates/vibe-protocol/src/lib.rs:391`). Any client sending `capabilities.disabledNotifications`, which the reference client library always may, has its `initialize` answered with `invalid_params`. No later parity work is observable until this is fixed, because no reference client reaches the second frame.

2. **The method inventory diverges by 19 absences and 3 inventions.** `SERVER_METHODS` upstream holds 91 names ([vibe/app_server/protocol.py:82](/home/arthur/dev/mistral-vibe/vibe/app_server/protocol.py)); the Rust constant holds 82 (`crates/vibe-protocol/src/lib.rs:27`). Absent: `identity/read`, `telemetry/record`, `workspace/worktrees/list`, the 9 `projectLinks/*` and, outside `SERVER_METHODS`, the 7 `clientTool/*` server-to-client methods of `ClientToolMethod`. Invented: `config/batchWrite`, `connectors/toggle`, `mcp/auth/complete`, none of which appears anywhere in the reference tree. `ServerCapabilities.methods` therefore advertises a set that is neither a subset nor a superset of the contract.

3. **Eight of fifteen notifications are never emitted, and four names are invented.** Absent: `session/snapshot`, `session/updated`, `session/statsUpdated`, `session/contextCleared`, `turn/retrying`, `runtime/updated`, `mcp/authUrl`, `warning`. The consequences compound: `_turns.py:772` emits `session/updated` on every status transition, `_turns.py:742` emits `session/statsUpdated` on every turn, and `server.py:642` emits `runtime/updated` after any response marked `runtime_updated`. Without those three, session status and token accounting are never pushed and a client must poll. That is consistent with `crates/vibe-app-server/src/server/projection.rs:68`, which hard-codes `"model": null, "agent": null, "tokenUsage": null` into every published session. Invented in their place: `mcp/updated`, `workspace/trust/updated`, `connectors/updated`, `shell/updated`.

4. **The entry detail unions are untyped.** `EffectDetail` upstream is a 12-variant discriminated union, each variant carrying a typed `input` and an 8-field `display: EffectCallDisplay` ([vibe/app_server/_effect_models.py:232](/home/arthur/dev/mistral-vibe/vibe/app_server/_effect_models.py)). The port emits one shape, `{kind:"tool", toolCallId, toolName, arguments}` (`crates/vibe-core/src/events.rs:693`): no `display`, `input` renamed `arguments`, `toolCallId` added, and 11 of 12 `kind` values never produced. `NoticeDetail` is 8 variants upstream ([models.py:772](/home/arthur/dev/mistral-vibe/vibe/app_server/models.py)) and 2 here. `EffectResultDisplay`, `PublicRetryCategory` (5 values) and `TurnErrorCode` (9 values) have no Rust counterpart at all.

5. **Response envelopes do not validate against the reference models.** `vibe/app_server/_model.py:10` sets `extra="forbid"` on every protocol model, so a surplus field is a rejection exactly like a missing required one. Measured divergences: `config/read` returns `{snapshot}` against `{config, baseConfig, strippedHistoryImages}`; `config/patch` returns `{rejected, failures, changedKeys}` against `{failures, rejected, runtime, strippedHistoryImages}`; `connectors/read` returns `{counts, connectors}` against `{counts}`; `agents/list` omits the required `active`; `skills/list` adds `issues`; `session/settings/update` accepts 8 parameters against 3. `ConfigView` publishes 16 of 21 fields, `AgentStatsSnapshot` 14 of 17, and `MCPSourceStatus` shares 1 of 6 values with the reference vocabulary while adding two fields and hard-coding `kind` to `"server"`.

6. **Three methods answer with a shell.** `runtime/read` (`crates/vibe-app-server/src/resources.rs:855`) is the single call the reference TUI makes to render everything, and the port returns `empty_config()` for both `config` and `baseConfig`, a hard-coded `activeAgent`, `agents: []`, `skills: []`, zeroed `stats`, `contextWindow: 0` and `hooksCount: 0`. Only `tools` is real. `stats/read` returns the same zeroed snapshot and `account/read` always reports `missing_key`. The port is internally inconsistent about it: `agents/list` and `skills/list` return real catalogs for the objects `runtime/read` reports as empty.

7. **`invalid_params` carries no structured detail.** The reference fills `data` with `{errorCount, issues:[{path, message}]}` ([server.py:613](/home/arthur/dev/mistral-vibe/vibe/app_server/server.py)); grep for `errorCount` across `crates/` returns nothing. A client cannot point at the offending field.

8. **Nothing measures any of the above.** `scripts/parity/` holds `oracle.py`, `tool_surface.py` and `config_surface.py`, and none of them covers the protocol. `docs/parity.md` scores the app-server 78 by counting method names, which is why problems 3 through 7 do not appear in it, and why `identity/read` and `workspace/worktrees/list` are absent from the document entirely. `cargo test --workspace --all-features` passes at full green with a handshake that rejects a conforming client.

**Why now:** `docs/parity.md` ranks missing protocol notifications third in execution order, ahead of everything that follows, on the stated ground that everything written afterwards emits or consumes them. That reasoning applies with more force to the entry detail unions of problem 4: `EffectDetail` and `NoticeDetail` cross the wire in every history entry and are already written into persisted sessions, so each week of deferral multiplies the traces to migrate, exactly as it did for tool names in rank 1. The two parts that reached 95 in this repository did so because a differential oracle measures them; the instrument that makes this work verifiable is a direct reuse of `scripts/parity/config_surface.py` and `crates/vibe-core/src/config/surface_parity_tests.rs`, both of which already handle the pinned checkout, the conditional live probe and the committed corpus.

## Overview

This initiative makes the Rust app-server contract-equivalent to the Python reference at the protocol boundary. Equivalence is defined mechanically: for every method the reference routes, the request parameters this port accepts and the response body it produces validate against the reference Pydantic model with `extra="forbid"`, and for every notification the reference emits, this port emits the same name with a body that validates the same way. Behavior beyond the wire shape is out of scope; this is a surface contract, not an output oracle.

The sequencing puts the instrument first. The first epic builds `scripts/parity/app_server_surface.py`, which drives the reference `protocol` and `_connection_protocol` modules to record every method name, every model's field census with aliases and required flags, every notification name and the enum vocabularies, then commits that capture as `crates/vibe-app-server/tests/app-server-surface/corpus.json`. Field names, aliases, JSON pointers, enum values and required flags are observations rather than authored prose, so they are committable under `NOTICE` on the same reasoning that already applies to `crates/vibe-core/tests/config-surface/corpus.json`. Descriptions and docstrings are never captured. A Rust module replays the corpus unconditionally and skips only the live probe that recaptures from the pinned checkout, matching `config::surface_parity_tests`. The same epic fixes the handshake, structures `invalid_params` and moves the three invented method names out of the advertised inventory into a `LOCAL_EXTENSION_METHODS` constant, so `SERVER_METHODS` becomes exactly the reference inventory and the local extensions stay visible and bounded.

The second epic restores the notification contract, the third types the entry detail unions, the fourth fills the response envelopes and makes `runtime/read` report live state, and the fifth and sixth add the absent method families. Every story after the first is verified by replaying the corpus rather than by hand-written assertions, so a divergence is reported as a method name plus a JSON pointer plus the expected shape.

The reference is a read-only checkout pinned for this PRD at commit `68ff32e6a92e80a874c8153312f0aa8ae4955477` (v2.23.3), which every measurement in this document was taken from. Its location is machine-dependent: `C:\dev\mistral-vibe` on Windows and `/home/arthur/dev/mistral-vibe` on Linux. Reference links below use the Linux form as the canonical spelling and resolve against whichever checkout is local; the parity scripts read `VIBE_REFERENCE` as an override and `--reference` wins over both. The module is [vibe/app_server](/home/arthur/dev/mistral-vibe/vibe/app_server), 17 913 lines across 56 files, and splits into five parts every story navigates back to: [protocol.py](/home/arthur/dev/mistral-vibe/vibe/app_server/protocol.py) declares the method inventory and 250 wire models; [_connection_protocol.py](/home/arthur/dev/mistral-vibe/vibe/app_server/_connection_protocol.py) owns the handshake and the client-tool methods; [models.py](/home/arthur/dev/mistral-vibe/vibe/app_server/models.py) and [_effect_models.py](/home/arthur/dev/mistral-vibe/vibe/app_server/_effect_models.py) type the projection; [events.py](/home/arthur/dev/mistral-vibe/vibe/app_server/events.py) and [_turns.py](/home/arthur/dev/mistral-vibe/vibe/app_server/_turns.py) own the notification sequence; [server.py](/home/arthur/dev/mistral-vibe/vibe/app_server/server.py) owns dispatch, attachment and the event watermark. Two contracts reach outside the module and stay in scope: [vibe/utils/tool_presentation.py:33](/home/arthur/dev/mistral-vibe/vibe/utils/tool_presentation.py) defines `EffectCallDisplay` and `EffectResultDisplay`, and [vibe/questions.py](/home/arthur/dev/mistral-vibe/vibe/questions.py) supplies the argument model behind `UserQuestionEffectDetail`.

One constraint shaped the plan. `NOTICE` declares that no upstream implementation source is copied, translated, vendored, linked, or shipped. The corpus records names, aliases, pointers, enum values, required flags and field counts, never docstrings or description text, exactly as the configuration corpus already does. Prose that must exist in Rust, such as error messages, is written originally.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Reproduce the method inventory | 91 of 91 reference methods in `SERVER_METHODS`, 0 invented names inside it | 91 of 91 maintained, every local extension listed in `LOCAL_EXTENSION_METHODS` with a recorded divergence entry |
| Interoperate with a conforming client | `initialize` accepts every field of the reference `ClientCapabilities`, 0 handshake rejections on valid input | 0 maintained, asserted by a corpus scenario |
| Reproduce the notification contract | 15 of 15 reference notifications emitted, 0 invented names | 15 of 15 maintained |
| Type the projection unions | 12 of 12 `EffectDetail` kinds and 8 of 8 `NoticeDetail` kinds published | 0 entry details serialized as an untyped value |
| Make envelopes validate | 91 of 91 responses validating against the reference model under `extra="forbid"` | 0 methods answering with a shell payload |
| Make conformance mechanically enforced | Corpus replays at least 91 method shapes plus 15 notification shapes and fails on any divergence | Oracle wired into CI, no wire model changed without a corpus entry |

## Target Users

### Editor integration author driving the app-server

- **Role:** Author of an IDE extension or agent bridge speaking JSON-RPC to the app-server, written against the reference protocol documentation.
- **Behaviors:** Sends `initialize` with the capabilities the reference declares, subscribes to notifications, renders session status from `session/updated`, renders token usage from `session/statsUpdated`, delegates file reads to the editor through `clientTool/readTextFile`.
- **Pain points:** `initialize` is rejected outright when the client declares `disabledNotifications`. If the client works around that, session status and token counts never arrive, so the UI shows a session that never leaves its initial state. The client-tool delegation the integration exists for is not implemented at all.
- **Current workaround:** Fork the client library to strip a field the reference protocol declares, then poll `session/read` on a timer to approximate the notifications.
- **Success looks like:** The same client binary drives either implementation with no branching.

### Rust port maintainer landing a protocol change

- **Role:** Engineer adding a method, a notification or a field to the app-server.
- **Behaviors:** Adds the name to `SERVER_METHODS`, writes a handler, adds a hand-written unit test asserting the shape they just wrote.
- **Pain points:** Nothing states what the shape should be. The test asserts the implementation against itself, which is why `runtime/read` can return eight empty objects at full green. Divergence is discovered when a client fails, months later.
- **Current workaround:** Read the reference source per field, which is exactly the work this PRD automates.
- **Success looks like:** A failing replay names the method, the JSON pointer and the expected shape, before the change is committed.

### Vibe operator switching between clients on one machine

- **Role:** Developer running the Python client and the Rust binary against the same sessions and the same configuration.
- **Behaviors:** Starts a session in one client, resumes it in the other, expects the transcript to render identically.
- **Pain points:** History entries written by the Rust binary carry an effect detail with no `display` and a single `kind`, so the reference client renders every tool call through the generic path and loses the per-tool presentation. Notices written by one client are unreadable by the other for 6 of 8 kinds.
- **Current workaround:** Pick one client per project and never switch.
- **Success looks like:** A session written by either binary renders the same way in both.

## Research Findings

Key findings that informed this PRD:

### Reference Contract

- The full surface is 114 contract points: 91 `SERVER_METHODS`, 7 `clientTool/*`, 15 server-to-client notifications and 1 server-to-client request (`callback/call`). Lifecycle methods (`initialize`, `initialized`, `shutdown`, `exit`) sit outside the negotiated inventory in both implementations. The port reproduces 87 of the 114 by name, or 76 percent.
- `extra="forbid"` on `ProtocolModel` ([_model.py:10](/home/arthur/dev/mistral-vibe/vibe/app_server/_model.py)) makes the contract symmetric: a surplus field fails validation exactly like a missing required field. This is what turns `{counts, connectors}` on `connectors/read` into a hard incompatibility rather than a tolerated extension.
- Notifications are sequenced, not fire-and-forget. `_sequence_notification` ([server.py:1185](/home/arthur/dev/mistral-vibe/vibe/app_server/server.py)) assigns a per-session monotonic `eventId` to every `EventNotificationParams` and rewrites the embedded `state.eventId` for snapshot and handoff params. `ClientProjection._next_event_id` ([events.py:294](/home/arthur/dev/mistral-vibe/vibe/app_server/events.py)) raises on a gap, so an implementation that skips a notification breaks the client's sequence rather than degrading it.
- Attachment buffers rather than drops. `_begin_attachment` / `_finish_attachment` ([server.py:576](/home/arthur/dev/mistral-vibe/vibe/app_server/server.py)) queue notifications raised while a session attaches and flush them once attached, and `_redeliver_open_callbacks` replays open callbacks to the newly attached client.
- `disabled_notifications` is a client-side mute list the server honors, with one exception: `_notify` ([server.py:1059](/home/arthur/dev/mistral-vibe/vibe/app_server/server.py)) never mutes an `EventNotificationParams`, so the sequenced event stream cannot be silenced and the client's gap detection stays sound.
- `session/snapshot` is not emitted on a timer. It is the snapshot form of the sequenced stream, consumed by `ClientProjection.consume` and validated against its own notification for session identity and watermark ([events.py:311](/home/arthur/dev/mistral-vibe/vibe/app_server/events.py)).

### Instrumentation Precedent

- The two parts scoring 95 in `docs/parity.md` are the two backed by a differential oracle. Both follow the same shape: a Python capture script that re-executes itself under the reference interpreter, a committed corpus, an unconditional Rust replay, and a live probe that skips when the checkout is absent or off-pin.
- `crates/vibe-core/src/config/surface_parity_tests.rs:39` is the only current parity test reading `VIBE_REFERENCE`; the others hardcode the Linux path. A new parity test reads the variable.
- CI runs the two conformance suites as named steps with `--nocapture` so the conforming count lands in the job log (`.github/workflows/ci.yml:32`, `:39`). A third step follows the same pattern.

### Best Practices Applied

- Capture observations, not prose. The configuration corpus records strategies, merge keys and editor kinds but never a field description, which is what keeps it committable under `NOTICE`. The same line is drawn here between a field alias, which is an observation, and a docstring, which is not.
- Fail on arrival, not on drift. `config::surface_parity_tests` fails when a registry field has no corpus entry, so a new field cannot land unmeasured. The app-server corpus applies the same rule to a new method or notification name.

## Assumptions & Constraints

### Assumptions (to validate)

- The reference `protocol` and `_connection_protocol` modules import cleanly under the pinned checkout's interpreter without starting a server. Verified during this audit: 250 models were enumerated from a plain import. Risk if wrong: the capture script needs a running server, which changes its shape but not the corpus.
- Every wire model reachable from a method's params or response is reachable by walking `model_fields` from the top-level model. Unverified for the deeply nested unions (`EffectDetail` inside `PublicEffectEntry` inside `PublicHistoryPage` inside `PublicSessionState`). US-079 validates this by asserting the walk reaches all 250 models.
- Migrating the 11 `vibe-cli` call sites off the `{snapshot}` envelope of `config/read` onto `config/fields/read` loses no information the TUI renders. `ConfigFieldWire` carries per-layer values, which is what those sites read.
- No persisted session on disk carries an effect detail in the current untyped shape that a reader must keep understanding. If wrong, US-087 needs a read-side compatibility branch.

### Hard Constraints

- `NOTICE` forbids copying, translating, vendoring, linking or shipping reference implementation source. The corpus carries names, aliases, JSON pointers, enum values, required flags and counts. It never carries description text, docstrings or prompt text.
- The reference checkout is read-only and pinned at `68ff32e6a92e80a874c8153312f0aa8ae4955477`. Re-pinning means regenerating every corpus and updating every `REFERENCE_COMMIT` constant in the same change; `grep -rn 'REFERENCE_COMMIT: &str' crates` enumerates them.
- Parity tests replay the committed corpus unconditionally. Only the live probe skips, and only when the checkout is absent or off-pin. A missing checkout must never fail `cargo test`.
- The layering in `[workspace.metadata.vibe] dependency-layers` holds: `vibe-protocol` and `vibe-core` cannot depend on `vibe-app-server`, and the wire models that live in `vibe-core::events` stay there.
- Every envelope struct in `vibe-protocol` keeps `deny_unknown_fields`; it is what lets the untagged `Envelope` discriminate its variants.
- `vibe-cli` and `vibe-acp` are adapters. A shape change lands in `vibe-core` or `vibe-app-server` and the adapters follow in the same change.

## Quality Gates

These commands must pass for every user story:

- `cargo fmt --all -- --check` - formatting
- `cargo check --workspace --all-targets --all-features` - compilation across every target and feature
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` - lint set with warnings denied
- `cargo test --workspace --all-features` - full suite, `--all-features` gates the app-server fixture binary
- `cargo test -p vibe-app-server --all-features app_server_surface_parity_tests -- --nocapture` - app-server surface conformance, from US-080 onward

## Epics & User Stories

### EP-024: The Surface Oracle and the Handshake Contract

Stand up the differential instrument that measures the app-server wire contract, then close the two defects that make the port unreachable for a conforming client: the rejected handshake and the advertised inventory.

**Definition of Done:** `scripts/parity/app_server_surface.py` captures the reference surface, the committed corpus replays unconditionally in CI, `initialize` accepts every reference capability field, `invalid_params` carries structured detail, and `SERVER_METHODS` contains exactly the 91 reference names.

#### US-079: Capture the reference app-server surface into a committed corpus

**Description:** As a Rust port maintainer, I want the reference method inventory, wire model census and notification vocabulary captured into a committed corpus so that conformance is measured against an authoritative record instead of read by hand.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given the pinned checkout, when `scripts/parity/app_server_surface.py` runs, then it writes `crates/vibe-app-server/tests/app-server-surface/corpus.json` recording the 91 `SERVER_METHODS` names, the 7 `ClientToolMethod` values, the 15 notification names and the 12 `ProtocolErrorCode` values
- [ ] Given each of the 250 protocol models, when the census is written, then each entry carries the model name, every field's camelCase alias, its required flag and its declared type kind, and carries no description, docstring or other reference-authored prose
- [ ] Given a discriminated union such as `EffectDetail` or `NoticeDetail`, when the census walks it, then every variant is recorded with its discriminator field and value, and the walk reaches all 250 models transitively from the top-level params and response models
- [ ] Given the enum vocabularies, when the capture runs, then `MCPSourceStatus`, `PublicRetryCategory`, `TurnErrorCode`, `ToolEffectKind`, `AccountStatus`, `PublicTurnStopReason` and `PublicEntryGenerationStatus` are recorded with their exact wire values
- [ ] Given `VIBE_REFERENCE` pointing at a checkout, when it wins over the default path and `--reference` wins over both, then the script re-executes itself under an interpreter that can import `vibe`
- [ ] Given a checkout at any commit other than `68ff32e6a92e80a874c8153312f0aa8ae4955477`, when the script runs, then it exits with an error naming the expected and actual commits rather than writing a corpus
- [ ] Given the corpus, when it is inspected, then it contains no text authored in the reference tree, satisfying `NOTICE`

#### US-080: Replay the corpus against the Rust surface in CI

**Description:** As a Rust port maintainer, I want a test module that replays the committed corpus against the served surface so that any divergence fails the build with the method name and JSON pointer that diverged.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-079

**Acceptance Criteria:**
- [ ] Given the committed corpus, when `cargo test -p vibe-app-server --all-features app_server_surface_parity_tests` runs, then it replays unconditionally and reports the conforming count per family (methods, notifications, models, enums)
- [ ] Given a method present in the corpus and absent from `SERVER_METHODS`, when the replay runs, then it fails naming the method
- [ ] Given a method routed by the port and absent from both the corpus and `LOCAL_EXTENSION_METHODS`, when the replay runs, then it fails naming the invented method
- [ ] Given a response body produced for a corpus method, when its keys are compared against the model census, then a missing required alias or a surplus alias fails the test naming the JSON pointer
- [ ] Given the reference checkout absent or at a different commit, when the suite runs, then the corpus replay still executes and only the live recapture probe skips, with a message naming why
- [ ] Given a new wire model added to the port without a corpus entry, when the replay runs, then it fails naming the model rather than passing silently
- [ ] Given `.github/workflows/ci.yml`, when the workflow runs, then an `App-server-surface conformance` step executes the suite with `--nocapture`

#### US-081: Accept the reference handshake and advertise the reference inventory

**Description:** As an editor integration author, I want `initialize` to accept every field the reference `ClientCapabilities` declares so that my client completes the handshake without forking its protocol library.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-080

**Acceptance Criteria:**
- [ ] Given an `initialize` carrying `capabilities.disabledNotifications`, when the server answers, then it returns an `InitializeResponse` rather than `invalid_params`
- [ ] Given a client that disabled a notification name, when the server would emit that notification, then it is suppressed, except that a sequenced event notification carrying `eventId` is always emitted regardless of the mute list
- [ ] Given a muted non-event notification, when the mute is applied, then the per-session event watermark is unchanged, so the client's sequence has no gap
- [ ] Given `ServerCapabilities.methods`, when `initialize` answers, then it lists exactly the methods this build routes from `SERVER_METHODS` and no local extension
- [ ] Given a capability field the reference does not declare, when a client sends it, then the server still answers `invalid_params`, so `deny_unknown_fields` keeps its discriminating role
- [ ] Given the corpus census for `ClientCapabilities`, `ServerCapabilities`, `ClientInfo` and `InitializeResponse`, when the replay runs, then all four validate

#### US-082: Structure `invalid_params` detail and bound the local extensions

**Description:** As an editor integration author, I want a rejected request to name the offending field so that I can correct the call instead of guessing, and I want the port's local method extensions to be visible rather than mixed into the contract.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-080

**Acceptance Criteria:**
- [ ] Given a request whose params fail to deserialize, when the server answers `invalid_params`, then `data` carries `errorCount` and an `issues` array whose entries carry `path` and `message`
- [ ] Given a failure at a nested path, when the issue is reported, then `path` is the sequence of field names and array indices leading to the offending value, not a flattened string
- [ ] Given a request that fails for a reason other than deserialization, when the server answers, then `data` stays absent from the wire rather than serializing as null
- [ ] Given `config/batchWrite`, `connectors/toggle` and `mcp/auth/complete`, when the server starts, then they are routed from `LOCAL_EXTENSION_METHODS`, are absent from `SERVER_METHODS`, and are absent from `ServerCapabilities.methods`
- [ ] Given a call to a local extension method by an already-connected `vibe-cli` or `vibe-acp` client, when it is dispatched, then it behaves as before this change
- [ ] Given `docs/parity.md`, when the change lands, then each local extension has a row in Accepted divergences naming why it exists and what holds it in place

---

### EP-025: The Notification Contract

Restore the eight absent notifications, retire the four invented ones, and make the sequenced event stream behave the way a reference client's projection expects.

**Definition of Done:** All 15 reference notifications are emitted with validating bodies, no invented notification name remains, and a reference `ClientProjection` consumes a full session without raising a sequence error.

#### US-083: Emit `session/updated` and `session/snapshot` with a sound event sequence

**Description:** As an editor integration author, I want session state changes pushed as sequenced notifications so that my UI reflects status transitions without polling.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-081

**Acceptance Criteria:**
- [ ] Given a turn that starts, blocks on a callback and completes, when the session status changes, then `session/updated` is emitted for each transition carrying `sessionId`, `eventId`, `emittedAt` and a JSON patch replacing `/status` and `/updatedAt`
- [ ] Given a blocked status, when the patch is built, then the status value carries `activeTurnId`, `callbackId` and `reason`, and the running status carries `activeTurnId`
- [ ] Given a client attaching to a session, when attachment completes, then `session/snapshot` is emitted carrying the full `PublicSessionState`, and its embedded `state.eventId` equals the notification's `eventId`
- [ ] Given notifications raised while a session is attaching, when attachment completes, then they are flushed in order rather than dropped, and open callbacks are redelivered to the newly attached client
- [ ] Given a sequence of emitted notifications for one session, when their `eventId` values are read, then they are strictly increasing by one with no gap, so a reference `ClientProjection` consumes them without raising `EventSequenceError`
- [ ] Given a session that fails, when the status is published, then the failed status carries its message rather than being reported as idle

#### US-084: Emit `session/statsUpdated` and publish live token accounting

**Description:** As an editor integration author, I want token usage and turn statistics pushed as they change so that my UI can show context consumption during a turn.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-083

**Acceptance Criteria:**
- [ ] Given a turn that starts and completes, when statistics change, then `session/statsUpdated` is emitted carrying `sessionId`, `eventId`, `emittedAt`, `stats` and `contextWindow`
- [ ] Given the emitted `stats`, when its keys are compared to the census, then all 17 `AgentStatsSnapshot` fields are present, including `sessionCachedTokens`, `cachedInputPricePerMillion` and `lastTurnCachedTokens`
- [ ] Given a published `PublicSession`, when `tokenUsage` is read, then it carries the session's real input, output and total token counts rather than null
- [ ] Given a model with no configured context window, when `contextWindow` is published, then it is reported as zero rather than failing the notification
- [ ] Given a session with no completed turn, when statistics are published, then the last-turn fields are zero rather than absent

#### US-085: Emit `session/contextCleared` and `turn/retrying` with their vocabularies

**Description:** As an editor integration author, I want context-clearing handoffs and retry attempts pushed so that my UI can explain why a turn is stalling or why the transcript changed.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-083

**Acceptance Criteria:**
- [ ] Given a context-clearing handoff, when it completes, then `session/contextCleared` is emitted carrying `oldSessionId`, `sessionId`, `state`, `sessionLog`, `eventId`, `emittedAt` and `planFilePath`
- [ ] Given `session/compacted` and `session/contextCleared`, when either is emitted, then `sessionId` differs from `oldSessionId`, `state.session.id` equals `sessionId` and `state.eventId` equals the notification `eventId`, so a reference projection accepts the handoff
- [ ] Given a request the backend retries, when the retry is scheduled, then `turn/retrying` is emitted carrying `sessionId`, a `category` drawn from the 5 `PublicRetryCategory` values and a `detail` string
- [ ] Given a retry whose cause maps to no known category, when the notification is emitted, then `category` is `unknown` rather than an invented value
- [ ] Given a turn that fails, when the error is published, then its `code` is drawn from the 9 `TurnErrorCode` values

#### US-086: Emit `runtime/updated`, `mcp/authUrl` and `warning`, and retire the invented names

**Description:** As an editor integration author, I want runtime changes and MCP authorization URLs delivered under the reference notification names so that my client does not have to learn names invented by this port.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-084

**Acceptance Criteria:**
- [ ] Given a request that mutates runtime state, when its response is sent, then `runtime/updated` is emitted afterwards carrying `sessionId` and the full `RuntimeSnapshot`
- [ ] Given an MCP source requiring authorization, when the URL is obtained, then `mcp/authUrl` is emitted carrying `name` and `url`
- [ ] Given a recoverable server-side problem, when it is reported, then `warning` is emitted carrying a `PublicError`
- [ ] Given the server after this change, when the emitted notification names are enumerated, then `mcp/updated`, `workspace/trust/updated`, `connectors/updated` and `shell/updated` are absent
- [ ] Given `vibe-cli` and `vibe-acp` call sites that consumed the four retired names, when the change lands, then they consume `runtime/updated` and `mcp/authUrl` and no consumer is left listening for a name that is never emitted
- [ ] Given a runtime mutation that fails, when the failure is returned, then no `runtime/updated` is emitted, so the client is not told about a change that did not happen

---

### EP-026: Typed Entry Details

Replace the untyped `detail` values on history entries with the reference discriminated unions, so a transcript written by either implementation renders the same way in both.

**Definition of Done:** `EffectDetail` publishes all 12 kinds with their call and result displays, `NoticeDetail` publishes all 8 kinds, and the callback detail and output unions validate against the census.

#### US-087: Publish the 12-variant `EffectDetail` with its call display

**Description:** As a Vibe operator switching between clients, I want tool effects to carry their typed detail and presentation so that the reference client renders each tool through its specific path rather than the generic one.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-080

**Acceptance Criteria:**
- [ ] Given a tool call, when the effect entry is published, then its `detail` carries `kind`, `toolName`, `display` and `input`, and carries neither `toolCallId` nor `arguments`
- [ ] Given each of the 12 `ToolEffectKind` values, when a matching tool runs, then the published `kind` is that value and `input` validates against that variant's input model
- [ ] Given the published `display`, when its keys are compared to the census, then all 8 `EffectCallDisplay` fields are present
- [ ] Given a tool with no dedicated variant, when its effect is published, then `kind` is `tool` and `input` carries the raw arguments
- [ ] Given a subagent tool call, when its effect is published, then `detail` carries `childSessionId`
- [ ] Given a completed, failed, cancelled or skipped effect state, when it is published, then it carries an `EffectResultDisplay` in `display`, except for a cancelled state with no result, where `display` is null
- [ ] Given the tool-call correlation the port previously read from `detail.toolCallId`, when the projection reduces a tool stream or result, then it still finds the matching entry

#### US-088: Publish the 8-variant `NoticeDetail`

**Description:** As a Vibe operator switching between clients, I want notice entries to carry their typed detail so that hook results, agent switches and plan reviews are readable in either client.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-087

**Acceptance Criteria:**
- [ ] Given a hook run, when the notice is published, then `detail.kind` is one of `hook_run_started`, `hook_run_completed`, `hook_started` or `hook_completed`, and it carries `scope`, `toolName`, `toolCallId`, `hookName`, `status` and `content`
- [ ] Given an agent switch, a context clear, a title update, a plan review start or end, a waiting-for-input request or a fired scheduled loop, when the notice is published, then `detail.kind` is the matching reference value and its variant fields are present
- [ ] Given a notice whose level is published, when it is read, then it is one of `info`, `warning` or `error`
- [ ] Given a notice the port raises for a condition the reference has no variant for, when it is published, then it is not emitted as a notice entry with an invented `kind`
- [ ] Given the corpus census for the 8 variants, when the replay runs, then each validates

#### US-089: Validate the callback detail and output unions

**Description:** As an editor integration author, I want callback entries and their responses to validate against the reference models so that approving a tool or answering a question works identically in both implementations.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-087

**Acceptance Criteria:**
- [ ] Given an approval callback, when it is published, then `detail` carries `kind: "approval"`, the typed `effect`, `requiredPermissions`, `choices` and `relatedEntryId`
- [ ] Given a user-input callback, when it is published, then `detail` carries `kind: "user_input"` and a `request` validating against the reference question model
- [ ] Given `callback/respond`, when a client answers, then the `output` is accepted in both the approval and user-input forms, and an output whose type does not match the open callback is rejected with `invalid_params`
- [ ] Given an open, answered, cancelled or expired callback, when its state is published, then it carries the matching status and its variant fields
- [ ] Given a client that did not declare a callback kind in its capabilities, when the server would raise that kind, then it refuses rather than emitting a callback the client cannot answer

---

### EP-027: Response Envelopes and Live Runtime

Make every response body validate against its reference model, and replace the shell payloads with live state.

**Definition of Done:** All 91 method responses validate under `extra="forbid"`, and `runtime/read`, `stats/read` and `account/read` report real state.

#### US-090: Report live runtime and statistics

**Description:** As an editor integration author, I want `runtime/read` to report the session's real configuration, agents, skills, statistics and hooks so that a client can render the session without calling five other methods and still missing data.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-084

**Acceptance Criteria:**
- [ ] Given a started session, when `runtime/read` answers, then `agents` and `skills` carry the same catalogs `agents/list` and `skills/list` return for that session
- [ ] Given a started session, when `runtime/read` answers, then `activeAgent` is the agent the session actually runs, `stats` is the live snapshot, `contextWindow` is the active model's threshold and `hooksCount` is the number of registered hooks
- [ ] Given a started session, when `runtime/read` answers, then `sessionLog` reports the session's real logging state across all 6 `SessionLogSummary` fields rather than a fixed disabled summary
- [ ] Given `stats/read`, when it answers, then `stats` is the same live snapshot and `contextWindow` the same threshold as `runtime/read` reports for that session
- [ ] Given a configured API key, when `account/read` answers, then `status` reflects the real account state drawn from the 4 `AccountStatus` values rather than always `missing_key`
- [ ] Given a session whose configuration failed to load, when `runtime/read` answers, then `issues` names the offending file and the response still validates

#### US-091: Publish the complete `ConfigView` and the reference config envelopes

**Description:** As an editor integration author, I want `config/read` and `config/patch` to answer in the reference shape so that a settings screen written against the reference protocol works here.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-090

**Acceptance Criteria:**
- [ ] Given `config/read` or `config/reload`, when either answers, then the body is `{config, baseConfig, strippedHistoryImages}` and both views validate as a `ConfigView`
- [ ] Given a published `ConfigView`, when its keys are compared to the census, then all 21 fields are present, including `activeModelPinned`, `defaultModelAlias`, `showGreeting`, `transcribeModels` and `ttsModels`
- [ ] Given `config/patch`, when it answers, then the body is `{failures, rejected, runtime, strippedHistoryImages}`, and `runtime` is the post-write `RuntimeSnapshot`
- [ ] Given `config/thinking/write` and `config/proxy/write`, when either answers, then the body carries `runtime` and `strippedHistoryImages` as the reference mutation response declares
- [ ] Given the 11 `vibe-cli` call sites reading the previous `{snapshot}` envelope, when the change lands, then they read `config/fields/read` and no site reads a key that is no longer published
- [ ] Given a patch the preflight rejects, when it answers, then `rejected` is true, `failures` is empty and no file on disk changed

#### US-092: Publish the reference MCP and connector vocabularies

**Description:** As an editor integration author, I want MCP and connector state to use the reference status vocabulary so that my client can distinguish a source needing authorization from one needing setup.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-090

**Acceptance Criteria:**
- [ ] Given an MCP source, when `mcp/read` answers, then its `status` is one of the 6 `MCPSourceStatus` values and the entry carries exactly `name`, `kind`, `transport`, `status` and `tools`
- [ ] Given a connector-backed source, when it is published in the MCP state, then `kind` is `connector` rather than `server`
- [ ] Given a source whose discovery failed, when `mcp/read` answers, then `discoveryErrors` maps that source's name to its message rather than being an empty object
- [ ] Given `connectors/read`, when it answers, then the body is `{counts}` and the surplus `connectors` key is absent
- [ ] Given `connectors/refresh`, when it answers, then the body is `{runtime, toolCount}`
- [ ] Given `mcp/add`, when it answers, then the body is `{created, name, runtime, url}`
- [ ] Given a source that is disabled and one that failed, when both are published, then their statuses differ, so a disabled source is not reported as broken

#### US-093: Align the remaining request and response envelopes

**Description:** As an editor integration author, I want the remaining methods to accept and return exactly the reference fields so that no call needs an implementation-specific branch.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-090

**Acceptance Criteria:**
- [ ] Given `agents/list`, when it answers, then the body carries `active` and `agents`
- [ ] Given `skills/list`, when it answers, then the body carries `skills` and the surplus `issues` key is absent
- [ ] Given `session/settings/update`, when it is called, then it accepts exactly `sessionId`, `maxTurns` and `maxTokens`, and a call carrying a field the reference does not declare is answered with `invalid_params`
- [ ] Given `session/start`, `session/resume` and `session/continue`, when any is called with `localWorkspaceSelection`, then the existing or create selection is honored
- [ ] Given a published `PublicSession`, when `model` and `agent` are read, then they carry the session's real model name and agent summary rather than null
- [ ] Given every method in the corpus, when the replay compares its response keys against the census, then all 91 validate with zero missing required and zero surplus aliases

---

### EP-028: Client Tools

Implement the server-to-client tool delegation that lets an editor host file access and terminals on behalf of the agent.

**Definition of Done:** All 7 `clientTool/*` methods are issued as server-to-client requests, gated on the capabilities the client declared.

#### US-094: Delegate file reads and writes to the client

**Description:** As an editor integration author, I want the server to route file access through my client so that the agent sees the editor's unsaved buffers rather than the files on disk.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-081

**Acceptance Criteria:**
- [ ] Given a client declaring `filesystem/read`, when the agent reads a file, then the server issues a `clientTool/readTextFile` request carrying `sessionId`, `path`, `line` and `limit`, and uses the returned `content`
- [ ] Given a client declaring `filesystem/write`, when the agent writes a file, then the server issues `clientTool/writeTextFile` carrying `sessionId`, `path` and `content`
- [ ] Given a client that declared neither capability, when the agent reads or writes, then the server uses its own filesystem access and issues no client request
- [ ] Given a client that does not answer a `clientTool` request, when the call times out, then the tool reports a failure naming the unanswered delegation rather than hanging the turn
- [ ] Given a client that answers with a malformed body, when the response is parsed, then the tool fails with a message naming the offending field
- [ ] Given `line` or `limit` below 1, when a request would be issued, then it is rejected before being sent, matching the reference bounds

#### US-095: Delegate terminals to the client

**Description:** As an editor integration author, I want the server to run shell commands through my client's terminal so that the user sees the command in their editor rather than in a hidden process.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-094

**Acceptance Criteria:**
- [ ] Given a client declaring `terminal`, when the agent runs a command, then the server issues `clientTool/terminal/create` carrying `sessionId`, `command`, `args`, `env`, `cwd`, `outputByteLimit` and `toolCallId`, and receives a `terminalId`
- [ ] Given a created terminal, when the command runs, then `clientTool/terminal/wait` returns `exitCode` and `signal`, and `clientTool/terminal/output` returns `output` and `truncated`
- [ ] Given a turn that is interrupted, when the terminal is still running, then `clientTool/terminal/kill` is issued and then `clientTool/terminal/release`
- [ ] Given a completed command, when the tool finishes, then `clientTool/terminal/release` is issued exactly once for that terminal
- [ ] Given `outputByteLimit` at or below zero, when a create request would be issued, then it is rejected before being sent
- [ ] Given a client that fails a terminal request mid-command, when the failure arrives, then the terminal is released and the tool reports the failure rather than leaking the terminal

---

### EP-029: Project Links, Identity, Telemetry and Worktrees

Add the remaining absent method families and re-score the parity document from the measured result.

**Definition of Done:** All 19 previously absent methods are routed and validating, and `docs/parity.md` records the app-server score from a corpus run rather than a name count.

#### US-096: Add `identity/read`, `telemetry/record` and `workspace/worktrees/list`

**Description:** As an editor integration author, I want the account identity, telemetry sink and worktree listing so that my client can show who is signed in and which worktrees a session spans.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-090

**Acceptance Criteria:**
- [ ] Given a configured API key, when `identity/read` is called, then it answers `{identity}` carrying `id`, `email`, `firstName`, `lastName`, `workspace` and `organization`
- [ ] Given no configured key or an unreachable identity endpoint, when `identity/read` is called, then it answers with a null identity rather than failing the request
- [ ] Given `telemetry/record`, when a client records an event, then it accepts `sessionId`, `name`, `properties` and `correlateLastRequest`, and answers empty
- [ ] Given telemetry disabled by configuration, when `telemetry/record` is called, then it answers empty and records nothing
- [ ] Given a session in a git repository with linked worktrees, when `workspace/worktrees/list` is called, then each entry carries `name`, `root`, `cwd`, `repoRoot` and `branch`
- [ ] Given a session outside a git repository, when `workspace/worktrees/list` is called, then it answers an empty list rather than an error

#### US-097: Add the `projectLinks` read surface

**Description:** As a Vibe Code user, I want to resolve and inspect a repository root and browse its candidate projects without an open session so that the picker works before a session exists.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-096

**Acceptance Criteria:**
- [ ] Given `projectLinks/list`, when it is called with no session, then it answers `{projects}` with each saved link's repo root, repo URL, project id and project name
- [ ] Given `projectLinks/resolveRoot`, when a path is resolved, then it answers `{root, eligible, rejectReason}`, and an ineligible root carries a reason drawn from the reference vocabulary rather than a free-form string
- [ ] Given `projectLinks/inspectRoot`, when a root with a stale saved link is inspected, then the answer carries `savedLink`, `staleLinkCleared` and `staleLinkClearFailed`
- [ ] Given `projectLinks/picker/load` and `projectLinks/picker/loadMore`, when candidates are paged, then each candidate carries its `matchKind` and the page carries its cursor
- [ ] Given a path that is not a git repository, when any read method is called on it, then the answer reports ineligibility rather than raising an internal error
- [ ] Given an unauthorized or unreachable Vibe Code backend, when a read method is called, then it answers `unauthorized` or `internal_error` as the reference classifies it, not a generic failure

#### US-098: Add the `projectLinks` mutation surface and re-score parity

**Description:** As a Vibe Code user, I want to create, link, save and unlink a project from a repository root so that the whole session-less linking flow works, and I want the parity document to state the measured result.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-097

**Acceptance Criteria:**
- [ ] Given `projectLinks/create`, when called with `rootPath`, `name` and `defaultBranch`, then a project is created and the answer carries the resulting `link`
- [ ] Given `projectLinks/link` and `projectLinks/save`, when either is called, then the answer carries `{link}`, and `save` rejects when `expectedRepoUrl` does not match the root's current remote
- [ ] Given `projectLinks/unlink`, when called on a linked root, then the answer reports the removal, and when called on an unlinked root, then it answers `unlinked: true` rather than failing
- [ ] Given a store deletion that fails, when unlinking, then the failure is reported on the response rather than being swallowed
- [ ] Given the full corpus, when the replay runs after this story, then it reports 91 of 91 methods, 15 of 15 notifications and 0 invented names inside the inventory
- [ ] Given `docs/parity.md`, when the change lands, then the app-server row states the score, the corpus run it comes from and the command that reproduces it, and the Execution order table marks ranks 3 and 8 done

## Functional Requirements

- FR-01: `SERVER_METHODS` must contain exactly the 91 method names the reference declares, in sorted order, with no additions.
- FR-02: Method names this port routes but the reference does not declare must live in `LOCAL_EXTENSION_METHODS`, must be absent from `ServerCapabilities.methods`, and must each have a row in the Accepted divergences table of `docs/parity.md`.
- FR-03: `initialize` must accept every field the reference `ClientCapabilities` declares, and must continue to reject a field it does not declare.
- FR-04: The server must honor a client's `disabledNotifications` for every notification except a sequenced event notification, and muting must not advance or skip the per-session event watermark.
- FR-05: Every response body must validate against the reference model for that method under `extra="forbid"`: no missing required alias and no surplus alias.
- FR-06: The server must emit all 15 reference notifications under their reference names, and must emit no notification name the reference does not declare.
- FR-07: Sequenced event notifications must carry a per-session `eventId` strictly increasing by one, and a snapshot or handoff notification must carry an embedded `state.eventId` equal to its own.
- FR-08: History entry details must be published as the reference discriminated unions: 12 `EffectDetail` kinds, 8 `NoticeDetail` kinds, and the approval and user-input callback detail forms.
- FR-09: `runtime/read`, `stats/read` and `account/read` must report live session state, and must not return a fixed payload independent of the session.
- FR-10: An `invalid_params` error must carry `data` with `errorCount` and an `issues` array of `{path, message}`, and `data` must stay off the wire when it is null.
- FR-11: The capture script must refuse to write a corpus from a checkout at any commit other than the pinned one.
- FR-12: The corpus must record no reference-authored prose: no description, docstring, prompt or message text.
- FR-13: The Rust replay must run unconditionally from the committed corpus, and only the live recapture probe may skip when the checkout is absent or off-pin.
- FR-14: A wire model, method or notification added to the port without a corpus entry must fail the replay naming what is unmeasured.
- FR-15: `clientTool/*` requests must be issued only for capabilities the client declared during `initialize`.

## Non-Functional Requirements

- **Conformance:** 91 of 91 methods, 15 of 15 notifications, 7 of 7 client-tool methods and 12 of 12 error codes replayed with zero divergence. Zero invented names inside `SERVER_METHODS`.
- **Performance:** The corpus replay adds at most 5 seconds to `cargo test --workspace --all-features` on the CI runner. Notification emission adds at most 1 millisecond of median latency per event to a turn, measured by `crates/vibe-app-server/src/streaming_benchmark.rs`.
- **Reliability:** A missing or off-pin reference checkout never fails `cargo test`; it skips exactly one live probe and prints why. Corpus replay is deterministic: two runs on the same commit produce byte-identical output.
- **Compatibility:** A session persisted before this PRD must load and project after it, verified by a fixture session in the existing storage tests. Zero `vibe-cli` or `vibe-acp` call sites left reading a key or listening for a notification name that is no longer published.
- **Security:** No corpus, log or error message carries an API key, an identity email, a file path outside the workspace, or an MCP authorization URL query string. `redact` already covers diagnostics and its coverage extends to the new surfaces.
- **Licensing:** Zero bytes of reference-authored prose in any committed artifact, asserted by a corpus check that fails on any captured string longer than 200 characters that is not a name, alias, pointer or enum value.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Reference checkout absent | Fresh clone, no Python reference on disk | Corpus replay runs, live probe skips | "Reference checkout not found; replaying committed corpus only" |
| 2 | Reference checkout off-pin | Reference updated past the pinned commit | Capture refuses to write; replay still runs | "Reference is at {actual}, corpus was captured from {expected}" |
| 3 | Client mutes a sequenced event | `disabledNotifications` lists `session/updated` | Notification is emitted anyway; watermark unbroken | — |
| 4 | Client mutes a non-event notification | `disabledNotifications` lists `warning` | Notification suppressed, watermark unchanged | — |
| 5 | Notification raised during attachment | Turn emits while a session attaches | Queued and flushed in order after attachment | — |
| 6 | Client disconnects mid-turn | Transport drops while notifications are pending | Pending notifications dropped, session survives, open callbacks redelivered on reattach | — |
| 7 | Unknown tool with no `EffectDetail` variant | MCP tool the reference has no kind for | Published as `kind: "tool"` with raw input | — |
| 8 | Notice for a condition with no reference variant | Port-specific event | Not published as a notice with an invented kind | — |
| 9 | Client tool unanswered | Editor stops responding to `clientTool/terminal/wait` | Terminal released, tool fails naming the delegation | "The client did not answer {method} within {n}s" |
| 10 | Client tool answers malformed | Editor returns a body missing `content` | Tool fails naming the field | "Client tool response is missing {field}" |
| 11 | Config patch rejected by preflight | Patch would produce an invalid merged config | `rejected: true`, `failures` empty, no file changed | "{field}: {reason}" |
| 12 | Config patch partially fails | One target writable, another not | `rejected: false`, `failures` names the target that did not land | "{target}: {reason}" |
| 13 | Identity endpoint unreachable | Network down or key rejected | Null identity, request succeeds | — |
| 14 | `projectLinks` on a non-git path | User points the picker at a plain directory | Ineligible with a reference reject reason | "{path} is not a git repository" |
| 15 | Stale saved project link | Repository remote changed since the link was saved | Link cleared, `staleLinkCleared: true`; clear failure reported, not swallowed | "The saved project link no longer matches this repository" |
| 16 | Session persisted before this PRD | Resume a session whose entries carry the old effect detail | Loads and projects; unknown detail shape falls back to the generic kind | — |
| 17 | Model with no context window | Provider reports no threshold | `contextWindow: 0`, notification still emitted | — |
| 18 | Callback kind the client cannot answer | Server would raise `connector_auth` to a client that did not declare it | Refused before emission | "This client cannot answer a {kind} callback" |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Typing `EffectDetail` breaks the projection's tool-call correlation, which currently keys off `detail.toolCallId` | High | High | US-087 carries an explicit criterion for correlation; move the call id into the reducer state rather than the published detail, and keep the existing projection tests green |
| 2 | Migrating `config/read` off the `{snapshot}` envelope breaks the TUI settings screen across 11 call sites | High | Medium | US-091 carries a criterion enumerating the sites; `config/fields/read` already publishes per-layer values and is conformant, so the migration is a read-path change with no new capture |
| 3 | Sessions persisted before this PRD carry entry shapes the new readers reject | Medium | High | Edge case 16 plus an NFR requiring a pre-PRD fixture session to load; the read path falls back to the generic kind rather than failing |
| 4 | The corpus captures reference-authored prose and violates `NOTICE` | Medium | High | FR-12 forbids it, and an automated check fails on any captured string over 200 characters that is not a name, alias, pointer or enum value; the configuration corpus already sets this precedent |
| 5 | Emitting eight new notifications per turn measurably slows streaming | Medium | Medium | NFR bounds the added median latency at 1 ms per event and names the existing benchmark that measures it; notifications are already batched through the dispatch batch |
| 6 | `clientTool/*` requires a server-to-client request path that only `callback/call` exercises today | Medium | Medium | US-094 sizes the transport work into the first client-tool story rather than assuming it; the pending-request map already exists on `ServerConnection` |
| 7 | The reference protocol modules cannot be imported without starting a server, invalidating the capture approach | Low | High | Already disproven during the audit: 250 models were enumerated from a plain import of `vibe.app_server.protocol` |
| 8 | 20 stories across 6 epics overruns before the parity document is re-scored | Medium | Medium | EP-024 alone raises the score by closing the handshake and the inventory, and each epic after it is independently landable; US-098 carries the re-score as a criterion so it cannot be forgotten |

## Non-Goals

- **Behavioral equivalence beyond the wire shape.** This PRD proves that `bash` publishes a conformant effect detail, not that `bash` produces the same output as upstream. An output oracle is a different instrument and is explicitly out of scope, as `docs/parity.md` already records for the tool surface.
- **Reference-authored description text.** Field descriptions, tool descriptions and prompt text stay original prose in Rust. `NOTICE` forbids anything else, and the corpus records presence and length only.
- **Retiring the three local extension methods.** `config/batchWrite`, `connectors/toggle` and `mcp/auth/complete` have live callers in `vibe-cli` and `vibe-acp`. They leave the advertised inventory in US-082 and stay routable; removing them is a separate decision with its own migration.
- **The `{snapshot}` config envelope as a published method.** It is retired as a response shape, not re-published under a local name. `config/fields/read` covers what read it.
- **Automatic compaction, OTel, the skills registry and browser sign-in.** They appear in `docs/parity.md` with their own scores and depend on parts other than the protocol.
- **Re-pinning the reference.** Every measurement here is taken at `68ff32e6a92e80a874c8153312f0aa8ae4955477`. A re-pin regenerates all three corpora and is its own change.

## Files NOT to Modify

- `NOTICE` — the licensing boundary this whole PRD is shaped by. Changing it changes what may be committed.
- `crates/vibe-core/tests/config-surface/corpus.json` and `crates/vibe-app-server/tests/tool-surface/baseline.json` — corpora owned by delivered PRDs. A change here means their oracle drifted, which is a separate finding.
- `Cargo.toml` `[workspace.metadata.vibe] dependency-layers` — the layering this work must respect, not relax.
- `crates/vibe-protocol/src/lib.rs` `Envelope` and its four variant structs, beyond adding fields to `ClientCapabilities` — `deny_unknown_fields` on each is what lets the untagged enum discriminate, and relaxing it makes declaration order silently load-bearing.
- `scripts/parity/oracle.py`, `scripts/parity/tool_surface.py`, `scripts/parity/config_surface.py` — read them as the pattern; the new script is a sibling, not an edit.

## Technical Considerations

- **Corpus granularity:** should the census record a model's fields flat, or preserve nesting so a divergence reports a full JSON pointer? Recommended: flat per model plus a reachability graph, which yields pointers on demand without duplicating nested models. Engineering to confirm the graph is cheap to walk in the replay.
- **Where the typed unions live:** `EffectDetail` and `NoticeDetail` are consumed by `vibe-core::events` and produced by the engine, so they belong in `vibe-core`. Alternative: `vibe-protocol`, which would put wire models next to envelopes but pull tool presentation into the protocol crate. Trade-off: crate cohesion against a second wire-model home. Recommended: `vibe-core`, matching where `PublicHistoryEntry` already lives.
- **Effect display source:** `EffectCallDisplay` and `EffectResultDisplay` are presentation, currently computed in the TUI. Should the app-server compute them, or should the engine emit them with the tool result? Recommended: the engine, since the reference emits them from the tool layer and the ACP bridge needs them too.
- **Notification batching:** should the eight new notifications flow through the existing `DispatchBatch`, or through a dedicated event queue with its own watermark lock? Recommended: `DispatchBatch`, which already orders outbound frames per dispatch. Engineering to confirm it can carry a notification raised outside a request.
- **Client-tool transport:** `callback/call` is the only server-to-client request today and its pending map is keyed by callback id. Should `clientTool/*` reuse that map with a request-id key, or get its own? Recommended: generalize the existing map to request id, which `ServerConnection.pending_server_requests` already stores.
- **`projectLinks` state ownership:** the saved links store is session-less. Should it live in `vibe-core::storage` next to session metadata, or in a dedicated module? Recommended: dedicated, since its lifecycle is tied to repository roots rather than sessions.
- **Migration:** entries persisted with the untyped effect detail need a read path. Is a fallback to the generic kind sufficient, or does a one-shot rewrite belong here? Recommended: fallback only. Rewriting persisted transcripts is irreversible and the generic kind renders correctly.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Reference contract points reproduced | 87 of 114 (76%) | 114 of 114 | Month-1 | `cargo test -p vibe-app-server --all-features app_server_surface_parity_tests -- --nocapture` conforming count |
| Methods in `SERVER_METHODS` matching the reference | 79 of 91, plus 3 invented | 91 of 91, 0 invented | Month-1 | Corpus replay, inventory family |
| Notifications emitted under reference names | 7 of 15, plus 4 invented | 15 of 15, 0 invented | Month-1 | Corpus replay, notification family |
| Responses validating under `extra="forbid"` | Not measured; 6 known divergent on inspection | 91 of 91 | Month-1 | Corpus replay, envelope family |
| `EffectDetail` kinds published | 1 of 12 | 12 of 12 | Month-1 | Corpus replay, union family |
| `NoticeDetail` kinds published | 2 of 8 | 8 of 8 | Month-1 | Corpus replay, union family |
| Methods answering with a shell payload | 3 (`runtime/read`, `stats/read`, `account/read`) | 0 | Month-1 | US-090 criteria plus corpus replay |
| Handshake rejections on a conforming client | 100% when `disabledNotifications` is sent | 0% | Month-1 | US-081 criterion, replayed as a corpus scenario |
| `docs/parity.md` app-server score | 78, from a name count | 100, from a corpus run | Month-1 | US-098 criterion |
| Wire models added without a corpus entry | Not detectable | 0 | Month-6 | CI conformance step fails on an unmeasured model |

## Open Questions

- Does any client outside this repository depend on the four invented notification names (`mcp/updated`, `workspace/trust/updated`, `connectors/updated`, `shell/updated`)? Arthur to confirm before US-086 lands; if yes, they become local extensions rather than removals.
- Should `identity/read` cache the identity for the session lifetime or re-fetch per call? The reference re-fetches with an internal guard; engineering to decide before US-096, since a per-call fetch adds network latency to a method a TUI may poll.
- Is the `projectLinks` store expected to interoperate with the file the Python client writes, or may it use its own format? Arthur to decide before US-097; interoperation is the stated goal of this PRD but the store is not part of the protocol surface the corpus measures.
- Should the corpus record request parameter shapes for the 7 `clientTool/*` methods as server-issued requests, given the replay has no client to answer them? Engineering to decide during US-079; recording the shape without replaying the round trip is the cheaper option and still catches a field rename.
[/PRD]
