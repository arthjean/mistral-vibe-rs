[PRD]
# PRD: Compaction Parity

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-08 | Arthur Jean | Initial PRD from the measured compaction audit against the Python reference at commit `b78b451`: the 708-line surface formed by `vibe/core/compaction/`, `vibe/core/middleware.py` and the compaction slice of `vibe/core/agent_loop/_loop.py` has no middleware counterpart at all, no threshold-driven trigger, and a summarizer whose prompt, envelope, failure taxonomy, fallback and retry ladder are all absent |
| 1.1 | 2026-08-08 | Arthur Jean | Retargeted from the superseded pin `68ff32e` (v2.23.3) to `b78b451` (v2.24.0), the head of `upstream/main`. Every line anchor remeasured against it. The compaction surface itself is byte-identical across the two releases, `vibe/core/compaction/` and `vibe/core/middleware.py` unchanged and no line of the `_loop.py` compaction slice touched, so only the anchors moved and no requirement changed. The re-pin itself enters scope as US-142, which `AGENTS.md` requires to move both pin sources and regenerate every committed corpus in one change |

## Problem Statement

1. **There is no middleware layer anywhere in this port.** The string `middleware` does not appear once in `crates/`. The reference expresses six conversation policies as a pipeline with an observable contract: `MiddlewarePipeline.run_before_turn` returns on the first `STOP` or `COMPACT` and otherwise joins every `INJECT_MESSAGE` with two newlines ([vibe/core/middleware.py:238](/home/arthur/dev/mistral-vibe/vibe/core/middleware.py)). Order is load-bearing, not cosmetic: `TurnLimitMiddleware` is registered before `AutoCompactMiddleware` ([_loop.py:1393,1353](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py)), so a conversation that reaches its turn limit and its compaction threshold in the same cycle stops and does not compact. No structure in this port can express that resolution.

2. **Compaction never fires on a threshold.** `AutoCompactMiddleware` triggers when `threshold > 0 and context_tokens >= threshold` **before** the request is built ([middleware.py:100](/home/arthur/dev/mistral-vibe/vibe/core/middleware.py)). This port compacts only from `StreamOutcome::Completed(Err(ProviderError::ContextOverflow))` (`crates/vibe-core/src/engine.rs:676`), which means it has already paid for a request the backend refused. An operator on a 200 000-token window sees a hard provider error where the reference sees a silent compaction.

3. **The summarizer is a stub with a hard-coded prompt.** `ProviderSessionCompactor::compact_with_instructions` (`crates/vibe-app-server/src/client.rs:2919`) builds one literal English sentence at `client.rs:2926`, sends it with `tools: Vec::new()` and `max_tokens: 4096`, trims the response, and fails only when the text is empty. The reference resolves its request through `compaction_prompt`, which reads `compaction_prompt_id` and accepts a project or user `.md` override ([vibe_schema.py:596](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py), [prompts/__init__.py:75](/home/arthur/dev/mistral-vibe/vibe/core/prompts/__init__.py)), sends it **with** the live tool list and tool choice so the request rides the conversation's own token prefix, and classifies failure into exactly two named reasons.

4. **The failure taxonomy, the fallback and the retry ladder do not exist.** `CompactionFailureReason` is `"tool_call" | "empty_summary"` ([manager.py:28](/home/arthur/dev/mistral-vibe/vibe/core/compaction/manager.py)). A model that answers with a tool call instead of a summary is a distinct, reported failure. On any failure and outside strict mode, `_fallback` makes a second call with a dedicated system prompt, `thinking` forced off, no tools, and the conversation re-rendered as a transcript that keeps tool calls and drops reasoning ([manager.py:157,210](/home/arthur/dev/mistral-vibe/vibe/core/compaction/manager.py)). Either call retries up to `_COMPACTION_PTL_RETRIES = 3` times on overflow, dropping the oldest round each time. This port has none of it: one call, no fallback, no retry, and a failure surfaces as `TurnErrorCode::CompactionFailed` with a provider string.

5. **The compaction envelope is one line where the reference builds a structured document.** `render_compaction_context` emits a continuation preamble, a `<previous_user_messages>` block holding each preserved turn in `<previous_user_message>` tags with the reserved tags HTML-escaped inside the content, then a `<compaction_summary>` block ([context.py:62](/home/arthur/dev/mistral-vibe/vibe/core/compaction/context.py)). This port emits `[Conversation summary]\n{summary}` (`client.rs:2978`). The envelope is also a parser: `parse_previous_user_messages` reads it back, so a second compaction merges the previously preserved turns with the newer ones instead of losing them.

6. **No user message survives a compaction.** `collect_prior_user_messages` walks the transcript under a 20 000-token budget, newest first, skipping injected messages and prior summaries, re-parsing earlier envelopes, and middle-truncating the message that spills over ([context.py:141](/home/arthur/dev/mistral-vibe/vibe/core/compaction/context.py)). This port keeps the first system message and the summary and discards every user turn verbatim. The two helpers this rests on, `approx_token_count` and `truncate_middle_to_tokens` ([vibe/core/utils/tokens.py](/home/arthur/dev/mistral-vibe/vibe/core/utils/tokens.py)), have no Rust counterpart.

7. **The wire surface publishes one event where the reference publishes two, and a model that nothing produces.** The reference emits `CompactStartEvent{tool_call_id, current_context_tokens, threshold}` then `CompactEndEvent{tool_call_id, summary_length, old_session_id, new_session_id}` ([types.py:576,586](/home/arthur/dev/mistral-vibe/vibe/core/types.py)), projected as one checkpoint entry created `IN_PROGRESS` with `message: "Compacting context"` and patched to `"Context compacted"` ([_projector.py:638,635](/home/arthur/dev/mistral-vibe/vibe/app_server/_projector.py)). This port emits a single `EngineEvent::Compaction { summary }` and pushes an already-`Completed` checkpoint with `details: Value::Null` (`crates/vibe-core/src/events.rs:961`). `CompactionDetails` and its five fields are already recorded in `crates/vibe-app-server/tests/app-server-surface/corpus.json:2451` and are produced by nothing in this repository.

8. **Reactive compaction has no loop guard.** The reference allows exactly one reactive recovery per user turn (`_reactive_recovery_used`, reset at the top of every run, consulted by `_should_self_heal`, which also refuses to heal in strict mode) ([_loop.py:1490,1574,1596](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py)). This port's overflow branch does `continue` with no counter (`engine.rs:684`), so a conversation whose compacted transcript still overflows compacts again on the next cycle, without bound.

9. **Five configuration keys are declared and read by nobody.** `auto_compact_threshold`, `compaction_model`, `compaction_prompt_id`, `context_warnings` and `raise_on_compaction_failure` are all declared in `crates/vibe-core/src/config/registry.rs:624,629,834,863,880`, published in the schema and merged by the declared strategy. `auto_compact_threshold` has exactly one consumer, `Release3Service::context_window` (`crates/vibe-app-server/src/release3.rs:362`), which forwards it to the client as a display number. The other four have none. `docs/parity.md` already states the rule this violates: declaring a key is not implementing its feature.

10. **The context warning never reaches the model.** `ContextWarningMiddleware(0.5)` injects a `<vibe_warning>` message into the conversation once per session when half the window is consumed, and clears its latch on `ResetReason.COMPACT` ([middleware.py:112](/home/arthur/dev/mistral-vibe/vibe/core/middleware.py)). This port renders a percentage in the TUI status bar (`crates/vibe-cli/src/tui/render.rs:617`), which a model never sees.

11. **Nothing measures any of it.** Five differential oracles exist here, for the tool surface, the configuration surface, the app-server wire surface, tool execution and checkpoints. None covers compaction. `docs/parity.md` scores the part 55 from reading module presence, and its own text calls automatic compaction absent. `cargo test --workspace --all-features` passes green against a summarizer that cannot classify a failure.

**Why now:** `docs/parity.md` places automatic compaction at rank 10, and the two dependencies its own table names are satisfied. The configuration mechanism shipped at rank 2, with all five keys already declared, published and merged, so the feature that reads them arrives on a surface that is already conformant. Engine token accounting exists: `TurnLedger::record_completion` sets `context_tokens` from real usage on every completion (`engine.rs:1134`) and `SessionStats` carries it across turns. The cost of further deferral is concentrated in one place and grows: the compaction envelope is a **persisted** artifact. Every conversation compacted before the envelope is correct writes a transcript whose preserved user turns are gone for good, and no later change recovers them. This is the same argument that put tool names at rank 1, applied to message content instead of to identifiers.

## Overview

This initiative gives the port the conversation policy layer it never had, then makes compaction behaviorally equivalent to the reference on top of it. Equivalence is defined mechanically: for a given transcript, threshold, budget and scripted model response, this port must select the same user messages, render the same envelope byte for byte, classify the same failure reason, take the same retry and fallback decisions, and emit the same event pair with the same details.

The sequencing puts the seam first and the instrument second. The first epic builds `vibe-core/src/middleware.rs`: the four actions, the reset reasons, the conversation context, the result, the trait and the pipeline with its short-circuit and its two-newline aggregation, then wires it into `run_turn_controlled` at the top of every cycle where `exhausted_budget` and `apply_controls` already run. The three limit middlewares are ported and made the engine's only budget authority so that ordering against `AutoCompact` is real rather than notional, while the `TurnStopReason` values the app-server corpus already validates are preserved exactly.

The second epic builds the pure calculation and its oracle, in that order. `approx_token_count`, `truncate_middle_to_tokens`, `extract_summary`, `drop_oldest_round`, `render_compaction_context`, `parse_previous_user_messages` and `collect_prior_user_messages` are all total functions over strings and message lists, with no provider and no filesystem, which is why the oracle for them needs no backend: `scripts/parity/compaction.py` drives the reference functions directly over scripted inputs and records what they return. The same script drives `CompactionManager` with a stubbed `CompletionFn`, which is a `Protocol` the reference already injects ([manager.py:39](/home/arthur/dev/mistral-vibe/vibe/core/compaction/manager.py)), so the call sequence, the retry ladder, the fallback decision and the failure reason are captured with no network and no key.

The third epic turns compaction on: `CompactionSettings` reads the five configuration keys, `AutoCompactMiddleware` fires at the threshold, the reactive path gains its one-recovery-per-turn guard and its strict-mode refusal, and the two events and their two telemetry records are emitted. The fourth rebuilds the summarizer itself in `vibe-core/src/compaction/manager.rs`: the primary call with tools, the typed failures, the dedicated fallback with its rendered transcript, the overflow retry ladder, accounted usage, the `context_tokens` reset, and the app-server projection that finally produces `CompactionDetails`. The fifth closes the periphery: the injected context warning, the session identity format, and the scorecard remeasure.

The reference is a read-only checkout pinned for this PRD at commit `b78b451c39eab9213393ad2f45908e8562a5c5e7` (v2.24.0), which every measurement in this document was taken from. Its location is machine-dependent: `C:\dev\mistral-vibe` on Windows and `/home/arthur/dev/mistral-vibe` on Linux. Reference links below use the Linux form as the canonical spelling and resolve against whichever checkout is local; the parity scripts read `VIBE_REFERENCE` as an override and `--reference` wins over both, and Rust tests reach it through `vibe_core::parity::reference_root`.

Two constraints shaped the plan. `NOTICE` forbids shipping upstream prose, and compaction is the second place in this repository where prose is functional rather than decorative: the three prompt files total 1 582 bytes and one of them, `compact_summary_prefix.md` at 186 bytes, is read as a **filter marker** rather than as an instruction ([manager.py:89](/home/arthur/dev/mistral-vibe/vibe/core/compaction/manager.py)). Original prose is written for all three and the marker divergence is recorded rather than hidden. Second, the compaction envelope changes what gets persisted, so the envelope story lands before the trigger story: turning compaction on ahead of a correct envelope would write lossy transcripts that no later change repairs.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Reproduce the pipeline contract | 100 % of ordering scenarios resolve to the reference action, including simultaneous stop-and-compact | 0 maintained |
| Reproduce message selection | 0 divergent selections over the captured budget scenarios, including the middle-truncated spill | 0 maintained |
| Reproduce the envelope | Byte-for-byte equality on render, and round-trip equality on parse over every captured case | 0 divergent bytes |
| Fire compaction on the threshold | Automatic compaction triggers at `auto_compact_threshold` with 0 provider overflow errors reached first in the threshold scenarios | 0 maintained |
| Reproduce the failure taxonomy | Both reasons classified, fallback attempted outside strict mode, 3-step retry ladder observed | 0 divergent decisions |
| Publish the event pair | 2 of 2 events emitted, `CompactionDetails` produced with all 5 fields, validated against the app-server census | 0 synthetic values on the compaction surface |
| Read the declared configuration | 5 of 5 keys consumed by a real code path, proven by a test that changes each key and observes the behavior change | 0 declared-only compaction keys |
| Make conformance mechanically enforced | Corpus replays at least 60 scenarios across 7 families and fails on any divergence outside a named ledger | Ledger holds only `NOTICE` entries |
| Raise the measured score | `docs/parity.md` Compaction from 55 to 100, measured by the new oracle | Telemetry and Configuration rows updated with the keys this work consumes |

## Target Users

### Operator running a long conversation

- **Role:** Developer running `vibe` interactively for hours on one task, well past a single context window.
- **Behaviors:** Keeps working without watching the token counter, expects the tool to manage its own context, occasionally reruns a long command whose output floods the transcript.
- **Pain points:** The conversation runs to a provider refusal rather than compacting ahead of it, because nothing reads the threshold. When compaction does happen reactively, every user message is discarded, so the agent forgets constraints stated ten turns ago and has to be told again. There is no warning before the wall.
- **Current workaround:** Watch the status bar percentage and run `/compact` by hand, which is the manual version of the feature the configuration file already advertises.
- **Success looks like:** The session compacts silently at the threshold, the last 20 000 tokens of the operator's own words survive it, and a warning arrives at half the window.

### Editor integration author rendering compaction

- **Role:** Author of an IDE extension speaking JSON-RPC to the app-server, written against the reference protocol documentation.
- **Behaviors:** Renders a checkpoint entry while compaction runs, showing the current context size against the threshold, then updates it in place when the summary lands.
- **Pain points:** No entry appears until compaction is over, because only one event is emitted and it is already `Completed`. `CompactionDetails` is in the protocol census but arrives as `null`, so neither the progress numbers nor the session handoff identifiers can be rendered. The entry's message carries the entire summary text where the reference carries a two-word label.
- **Current workaround:** Poll `session/read` and diff the state, which shows the handoff after the fact and never shows the progress.
- **Success looks like:** An `IN_PROGRESS` entry with `currentContextTokens` and `threshold` appears when compaction starts and is patched to `Context compacted` with `summaryLength`, `oldSessionId` and `newSessionId` when it ends.

### Operator who configured compaction and got nothing

- **Role:** Developer who set `auto_compact_threshold`, `compaction_model` and `raise_on_compaction_failure` in `vibe.toml` after reading the published schema.
- **Behaviors:** Tunes the threshold down on an expensive model, points compaction at a cheap model, and turns on strict mode in CI so a failed compaction fails the run instead of silently degrading.
- **Pain points:** All three keys validate, merge and appear in `config/fields/read`, and none of them changes any behavior. Strict mode in particular is a silent no-op, which is worse than an error: CI reports success on a run whose context was quietly replaced by a placeholder.
- **Current workaround:** None. The keys are indistinguishable from working ones from the outside.
- **Success looks like:** Each key has an observable effect, and strict mode fails loudly.

### Parity maintainer certifying the port

- **Role:** Maintainer running the parity suite before proposing a commit, reading `docs/parity.md` as the record of what is proven.
- **Behaviors:** Runs the CI sequence, reads the per-family conformance counts each oracle prints, and updates the scorecard only from a measurement.
- **Pain points:** The compaction score of 55 comes from reading module presence. The part is now the largest unmeasured behavior left whose oracle needs no backend, which makes it the cheapest remaining score to make falsifiable.
- **Current workaround:** None.
- **Success looks like:** One command prints conformance counts across seven families, a divergence names the scenario and the field, and the score is a number that command produced.

## Research Findings

Key findings that informed this PRD. The research is a first-party measurement against the pinned oracle rather than a market survey: this is a parity port, so the only authority is the reference checkout, and every number below was taken from it at `b78b451` during the audit that produced this document.

### The surface, measured

- `vibe/core/compaction/` is 456 lines across three files: `manager.py` at 237, `context.py` at 188, `__init__.py` at 31.
- `vibe/core/middleware.py` is 252 lines and declares 6 middlewares plus the pipeline.
- `vibe/core/agent_loop/_loop.py` is 2 912 lines, of which the compaction slice is `_setup_middleware` (1388), `_handle_middleware_result` (1420), `_run_compaction` (1444), `_should_self_heal` (1490), the reactive branch (1641), `_reset_session` (2665) and `compact` (2713).
- Reference test coverage of this surface is 90 test functions: 33 in `tests/test_middleware.py`, 29 in `tests/core/compaction/test_compaction.py`, 24 in `tests/agent_loop/test_agent_auto_compact.py`, 3 in `tests/agent_loop/e2e/test_e2e_compaction.py`, 1 in `tests/cli/test_compact_message.py`. That density is the reason this PRD carries a corpus rather than named tests alone.
- The three prompt files total 1 582 bytes: `compact.md` 891, `compact_system.md` 505, `compact_summary_prefix.md` 186.

### The oracle needs no backend

`CompletionFn` is a `Protocol` injected into `CompactionManager` ([manager.py:39,69](/home/arthur/dev/mistral-vibe/vibe/core/compaction/manager.py)), and every model call the manager makes goes through it. A capture script can therefore supply a stub that returns scripted `LLMChunk` values and raises `ContextTooLongError` on demand, and observe the manager's full decision tree with no network, no key and no nondeterminism. This is a stronger position than the tool-execution oracle, which needed a fixture tree on disk, and it is why this PRD can promise scenario counts rather than sampled ones.

### Two arithmetic traps

- `approx_token_count` is `ceil(len(text) / 4)` where `len` on a Python `str` counts **code points**. The Rust counterpart must use `chars().count()`, never `len()`, which counts UTF-8 bytes. A transcript containing any non-ASCII character diverges silently otherwise, and the divergence is a budget error, so it changes which messages survive.
- `truncate_middle_to_tokens` slices with `text[:head]` and `text[-tail:]`, again on code points, and splits an odd budget as `head = available // 2` with the remainder going to the tail ([tokens.py:13](/home/arthur/dev/mistral-vibe/vibe/core/utils/tokens.py)). Rust must slice on `char_indices` boundaries and reproduce the same asymmetry.

### Best practices applied, from this repository's own record

- **Instrument before implementation.** All five existing oracles were built before the work they measure, and `docs/parity.md` records that as the reason the measured parts score 95 and above while the unmeasured ones sit at 55 to 80. The pure calculation and its oracle are epic 2, before the trigger in epic 3 and the summarizer in epic 4.
- **Capture through `git archive`, never by moving HEAD.** `scripts/parity/tool_execution.py` reads the pinned commit without creating a branch or a worktree. The new script follows it.
- **Commit observations, digest everything else.** Envelope bytes, selected message contents and token counts are values the scenario supplied, so they commit verbatim; anything that would carry reference-authored prose commits as a SHA-256 digest.
- **Ledger the divergences, and fail when the ledger goes stale.** `STRICTER_THAN_THE_REFERENCE` in the shell oracle and the `LICENSING` entries in the tool-execution oracle both fail the suite when a listed divergence disappears.

### Pure core, impure shell

The reference splits compaction the same way it split checkpoints: `context.py` is total functions over strings and message lists, while `manager.py` orchestrates injected callables and never touches disk itself. The port keeps that split, which is what lets epic 2 ship a corpus-verified module before any provider work exists.

*Full research sources available in project documentation.*

## Assumptions & Constraints

### Assumptions (to validate)

- **The reference's pure functions are deterministic over the captured inputs.** No randomness, no clock, no hash-order dependence: `collect_prior_user_messages` iterates a list and `render_compaction_context` concatenates. US-147 validates this by capturing twice and comparing. If a capture is unstable, the affected family is normalized on both sides the way `grep` match order already is.
- **`TurnLedger.context_tokens` is the correct threshold input.** It is set from `usage.input_tokens + usage.output_tokens` after each completion (`engine.rs:1134`), which is the same arithmetic the reference applies to `stats.context_tokens` ([_loop.py:2608](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py)). US-149 asserts the two agree on a scripted usage sequence before the threshold is read from it.
- **No persisted transcript in this port carries a compaction envelope today.** Based on the current envelope being `[Conversation summary]\n{summary}` with no tags, so nothing on disk can be mistaken for the new format. If a stored session is found carrying one, US-145 gains a compatibility criterion for reading it.
- **`compact_summary_prefix.md` has no producer at the pinned commit.** `COMPACT_SUMMARY_PREFIX` is read at exactly one site in the whole reference tree, the filter in `collect_prior_user_messages`. It is therefore a defensive filter against transcripts written by an older version, which is what makes an original-prose marker safe here. US-146 records the reasoning as an accepted divergence rather than leaving it implicit.
- **Making the pipeline the engine's budget authority preserves every `TurnStopReason`.** Based on the three limit middlewares testing the same quantities `exhausted_budget` already tests. US-140 asserts the existing stop-reason tests still pass unchanged, which is the falsifier.

### Hard constraints

- `NOTICE` forbids copying, translating, vendoring, linking or shipping upstream implementation source, prompt files or tool description text. The three compaction prompts are written originally and held to directive coverage, never to text. The corpus commits structural observations and scenario-supplied values only.
- The reference pin lives in exactly two places, `vibe_core::parity::REFERENCE_COMMIT` and `EXPECTED_COMMIT` in `scripts/parity/pin.py`, held equal by `crates/vibe-core/src/parity/parity_tests.rs`. This PRD **does** re-pin, from `68ff32e` (v2.23.3) to `b78b451` (v2.24.0), because measuring compaction against a superseded release would certify parity with a version nobody runs. `AGENTS.md` requires a re-pin to move both sources and regenerate every committed corpus in the same change, which is why US-142 is a single indivisible story and blocks every measurement in this PRD.
- A missing or off-pin reference checkout must never fail `cargo test`. The committed corpus replays unconditionally; only the live recapture probe skips.
- The layering in `[workspace.metadata.vibe] dependency-layers` holds: the middleware and the compaction engine belong in `vibe-core`, their projection in `vibe-app-server`, and `vibe-cli` and `vibe-acp` are adapters.
- `unsafe_code` is forbidden workspace-wide; `panic`, `unimplemented` and `dbg_macro` are denied outside tests.
- Every `EngineEvent` variant is serialized into persisted transcripts, so an existing variant is never removed or renamed; a new field arrives with `#[serde(default)]`, following the precedent set by `SessionHandoffCause`.
- `[workspace.package] version` is not bumped by this work.

## Quality Gates

These commands must pass for every user story:

- `cargo fmt --all -- --check` - formatting
- `cargo check --workspace --all-targets --all-features` - compilation of every target including the fixture binaries
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` - lint set with warnings denied
- `cargo test --workspace --all-features` - the full suite, never a filtered subset, because parity fixtures are read from more than one module

Stories that touch a parity corpus additionally report their conformance counts:

- `cargo test -p vibe-core --all-features compaction_parity_tests -- --nocapture` - compaction conformance counts across the seven families
- `cargo test -p vibe-app-server --all-features app_server_surface_parity_tests -- --nocapture` - wire census, which `CompactionDetails` and the checkpoint entry are validated against

## Reference Map

Every file an implementer opens before writing Rust, at the pinned commit `b78b451`. Paths use the Linux canonical spelling and resolve against whichever checkout is local, through `VIBE_REFERENCE` or `--reference`. Each story below names its own anchor; this is the whole surface in one place. Reading these is required by `AGENTS.md`, and grepping them does not replace opening the declaration they point at.

### The compaction module (3 files, 456 lines)

- [vibe/core/compaction/context.py](/home/arthur/dev/mistral-vibe/vibe/core/compaction/context.py), 188 lines, the pure calculation: `COMPACT_USER_MESSAGE_MAX_TOKENS = 20_000`, the six reserved tag constants, `extract_summary`, `drop_oldest_round`, `render_compaction_context`, `_escape_reserved_previous_user_message_tags`, `render_teleport_summary_request`, `parse_previous_user_messages`, `_is_compaction_context_message`, `collect_prior_user_messages`.
- [vibe/core/compaction/manager.py](/home/arthur/dev/mistral-vibe/vibe/core/compaction/manager.py), 237 lines, the orchestration: `_COMPACTION_PTL_RETRIES = 3`, `CompactionFailureReason`, `CompactionFailedError`, the `CompletionFn` protocol, and `CompactionManager` with `compact`, `_summarize`, `_primary`, `_fallback`, `_summarize_call`, `_render_transcript`, `_send_compaction_failed`.
- [vibe/core/compaction/__init__.py](/home/arthur/dev/mistral-vibe/vibe/core/compaction/__init__.py), 31 lines, the public facade.

### The policy layer (1 file, 252 lines)

- [vibe/core/middleware.py](/home/arthur/dev/mistral-vibe/vibe/core/middleware.py): `MiddlewareAction`, `ResetReason`, `ConversationContext`, `MiddlewareResult`, `ConversationMiddleware`, `TurnLimitMiddleware`, `PriceLimitMiddleware`, `TokenLimitMiddleware`, `AutoCompactMiddleware`, `ContextWarningMiddleware`, `ReadOnlyAgentMiddleware`, `MiddlewarePipeline`.

### What drives them

- [vibe/core/agent_loop/_loop.py](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py): `_setup_middleware` (1388), `_handle_middleware_result` (1420), `_run_compaction` (1444), `_should_self_heal` (1490), the run loop and its reactive branch (1620-1652), `_complete` (2403), the usage write (2608), `_clean_message_history` (2612), `_reset_session` (2665), `clear_history` (2685), `compact` (2713).
- [vibe/core/utils/tokens.py](/home/arthur/dev/mistral-vibe/vibe/core/utils/tokens.py): `_APPROX_BYTES_PER_TOKEN = 4`, `_TRUNCATION_MARKER`, `approx_token_count`, `truncate_middle_to_tokens`.
- [vibe/core/session/session_id.py](/home/arthur/dev/mistral-vibe/vibe/core/session/session_id.py): `generate_session_id(suffix)` and `extract_suffix`, the UUID-shaped identity with a stable trailing segment that a compaction preserves.
- [vibe/core/config/vibe_schema.py](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py): `compaction_model` (262), `auto_compact_threshold` (263), `context_warnings` (435), `raise_on_compaction_failure` (440), `compaction_prompt_id` (444), `get_compaction_model` (501), the `compaction_prompt` property (596), `_apply_global_auto_compact_threshold` (620), `_check_compaction_model_provider` (654).
- [vibe/core/prompts/__init__.py](/home/arthur/dev/mistral-vibe/vibe/core/prompts/__init__.py): `UtilityPrompt`, `MissingPromptFileError`, `load_prompt` (75).

### What publishes it on the wire

- [vibe/core/types.py](/home/arthur/dev/mistral-vibe/vibe/core/types.py): `CompactStartEvent` (576), `CompactEndEvent` (586), `LLMMessage.injected` (312), `MessageList.reset` (645), `AgentStats.reset_context_state` (140).
- [vibe/app_server/_projector.py](/home/arthur/dev/mistral-vibe/vibe/app_server/_projector.py): `_project_compaction_started` (638), `_project_compaction_completed` (654).
- [vibe/app_server/_turns.py](/home/arthur/dev/mistral-vibe/vibe/app_server/_turns.py): the handoff branch that raises `session/compacted` (701).
- [vibe/app_server/models.py](/home/arthur/dev/mistral-vibe/vibe/app_server/models.py): `CompactionDetails` (912).
- [vibe/app_server/protocol.py](/home/arthur/dev/mistral-vibe/vibe/app_server/protocol.py): `SessionCompactParams` (1188), `SessionCompactResponse` (1193), `SessionCompactedParams` (1226).
- [vibe/core/telemetry/send.py](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py): `send_auto_compact_triggered` (286), `send_compaction_failed` (305).

### The behavioral inventory

The reference's own tests are the checklist: 33 functions in `tests/test_middleware.py`, 29 in `tests/core/compaction/test_compaction.py`, 24 in `tests/agent_loop/test_agent_auto_compact.py`, 3 in `tests/agent_loop/e2e/test_e2e_compaction.py`, 1 in `tests/cli/test_compact_message.py`. Read them for the cases, never for the code.

## Epics & User Stories

### EP-041: The Conversation Policy Pipeline

Build the middleware layer this port has never had, with the ordering and aggregation semantics that make every later policy decision reproducible, and make it the engine's single budget authority so that ordering against compaction is real.

**Definition of Done:** `vibe-core` declares the four actions, the two reset reasons, the context, the result, the trait and the pipeline; `run_turn_controlled` consults the pipeline at the top of every cycle; the three limit middlewares produce the existing `TurnStopReason` values unchanged; and a simultaneous stop-and-compact resolves to stop.

#### US-139: Declare the middleware vocabulary and its pipeline
**Description:** As a parity maintainer, I want the reference's policy vocabulary declared in `vibe-core` so that every later conversation policy has one place to live and one contract to satisfy.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None
**Reference:** [vibe/core/middleware.py:16](/home/arthur/dev/mistral-vibe/vibe/core/middleware.py) for the action enum, 23 for the reset reasons, 29 for the context, 36 for the result, 43 for the protocol, 223 for the pipeline and 238 for `run_before_turn`, which is where the short-circuit and the two-newline aggregation live

**Acceptance Criteria:**
- [ ] Given `crates/vibe-core/src/middleware.rs`, when it is read, then it declares an action enum with exactly `Continue`, `Stop`, `Compact` and `InjectMessage`, a reset-reason enum with exactly `Stop` and `Compact`, a conversation context carrying the message list, the stats and the compaction settings, and a result carrying an action with an optional message and an optional reason
- [ ] Given a pipeline holding three middlewares where the second returns `Stop`, when `before_turn` runs, then the third is never polled and the result is the second's, including its reason string
- [ ] Given a pipeline where the second returns `Compact`, when `before_turn` runs, then the third is never polled and the result is `Compact`
- [ ] Given a pipeline where the first and third both return `InjectMessage` and the second returns `Continue`, when `before_turn` runs, then the result is one `InjectMessage` whose message is the two joined by exactly two newline characters, in registration order
- [ ] Given a pipeline where no middleware returns anything but `Continue`, when `before_turn` runs, then the result is `Continue` with no message
- [ ] Given a pipeline whose middlewares hold latched state, when `reset` is called with a reason, then every middleware receives that reason
- [ ] Given the module, when clippy runs with `-D warnings`, then no `panic`, `unwrap` or `expect` appears outside `#[cfg(test)]`

#### US-140: Port the three limit middlewares without changing any stop reason
**Description:** As a parity maintainer, I want the turn, price and token limits expressed as middlewares so that their precedence against compaction is the reference's, while every stop reason the app-server corpus already validates stays identical.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-139
**Reference:** [vibe/core/middleware.py:49](/home/arthur/dev/mistral-vibe/vibe/core/middleware.py) for the turn limit, 65 for the price limit, 81 for the token limit, and [vibe/core/agent_loop/_loop.py:1393](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py) for the registration order that makes them precede auto-compaction

**Acceptance Criteria:**
- [ ] Given a conversation at its step limit, when the pipeline runs, then the turn-limit middleware returns `Stop` and the engine maps it to `TurnStopReason::MaxSteps`
- [ ] Given a conversation past its token limit, when the pipeline runs, then the token-limit middleware returns `Stop` and the engine maps it to `TurnStopReason::TokenLimit`
- [ ] Given a conversation past its price limit, when the pipeline runs, then the price-limit middleware returns `Stop` and the engine maps it to `TurnStopReason::PriceLimit`
- [ ] Given the existing engine tests that assert stop reasons, when the suite runs after this change, then every one passes unmodified
- [ ] Given a cancelled token, when the pipeline runs, then cancellation is still checked before any middleware and yields `TurnStopReason::Cancelled`
- [ ] Given a limit set to zero or absent, when the pipeline runs, then the matching middleware is not registered rather than registered with a zero threshold

#### US-141: Consult the pipeline at every turn boundary
**Description:** As an operator, I want the engine to ask its policy layer before building each request so that a policy can stop, compact or inject before the tokens are spent rather than after.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-140
**Reference:** [vibe/core/agent_loop/_loop.py:1625](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py) for the call site at the top of the loop and 1420 for `_handle_middleware_result`, which is how each action is applied

**Acceptance Criteria:**
- [ ] Given `run_turn_controlled`, when a cycle begins, then the pipeline runs after `apply_controls` and before `stream_completion`, and its result decides whether the request is built at all
- [ ] Given a result of `InjectMessage`, when the cycle continues, then the message is appended as a user message marked injected and the request that follows carries it
- [ ] Given a result of `Stop`, when the cycle ends, then the turn finalizes with the mapped stop reason and no provider request is made
- [ ] Given a middleware that returns `Compact`, when the cycle runs, then the compaction path is entered before any request is built
- [ ] Given a turn that ends for any reason, when the next user turn starts, then the pipeline has been reset with reason `Stop`
- [ ] Given a transcript persisted before this change, when it is replayed, then no event variant fails to deserialize

---

### EP-042: The Pure Calculation and Its Oracle

Move the pin to the reference's current release, then port the seven total functions compaction rests on and capture the reference's answers for them and for the manager's decision tree into a committed corpus that replays with no backend.

**Definition of Done:** both pin sources name `b78b451` with every committed corpus regenerated from it in the same change; `vibe-core/src/compaction/context.rs` reproduces every captured case; `scripts/parity/compaction.py` captures seven families from the pinned checkout through `git archive`; `crates/vibe-core/tests/compaction/corpus.json` is committed; and `compaction_parity_tests` replays it unconditionally, printing per-family counts and failing on any divergence outside a named ledger.

#### US-142: Re-pin the reference to v2.24.0 and regenerate every corpus
**Description:** As a parity maintainer, I want the pin moved to the reference's current release with every committed corpus recaptured in the same change so that this port is measured against the version people actually run, and so that no corpus asserts a commit the constants contradict.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None
**Reference:** No reference counterpart. The targets are local: `crates/vibe-core/src/parity.rs` for `REFERENCE_COMMIT`, `REFERENCE_VERSION` and `RESTORE_COMMAND`, `scripts/parity/pin.py` for `EXPECTED_COMMIT` and `EXPECTED_VERSION`, and the ten capture scripts under `scripts/parity/`. The reference side is read at [b78b451](/home/arthur/dev/mistral-vibe), which declares version 2.24.0 in `pyproject.toml` and is the head of `upstream/main`

**Acceptance Criteria:**
- [ ] Given the two pin sources, when they are read, then both name `b78b451c39eab9213393ad2f45908e8562a5c5e7` and version `2.24.0`, the restore command quotes the same commit, and `parity_tests` still asserts the commit appears in exactly two places
- [ ] Given every committed corpus, when the capture scripts are rerun, then each is regenerated from the new pin in this same change and no corpus retains an observation captured from `68ff32e`
- [ ] Given the app-server surface replay, when it runs, then `identity/read` and `workspace/worktrees/list` appear in the reference inventory and are reported as declared-but-unrouted rather than silently absent
- [ ] Given `docs/parity.md`, when it is updated, then the accepted divergence stating that those two names exist nowhere in the reference tree is removed, because it is false at this pin, and the app-server score is restated from the new counts
- [ ] Given the configuration surface replay, when it runs, then any field the new reference declares and this port does not fails the replay naming the field, and the resulting gaps are recorded rather than silently absorbed
- [ ] Given a corpus whose replay now fails because the reference changed behavior, when the failure is triaged, then it is either fixed or added to that family's ledger with a named reason, and never suppressed by loosening the assertion
- [ ] Given the full suite after the re-pin, when `cargo test --workspace --all-features` runs, then it passes, and any test that skipped because the checkout was off-pin now executes
- [ ] Given a machine with no reference checkout, when the suite runs, then every committed corpus still replays and only the live probes skip

#### US-143: Reproduce the token approximation and the middle truncation
**Description:** As a parity maintainer, I want the reference's token arithmetic reproduced exactly so that every budget decision downstream selects the same messages.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None
**Reference:** [vibe/core/utils/tokens.py:5](/home/arthur/dev/mistral-vibe/vibe/core/utils/tokens.py) for the bytes-per-token constant, 9 for `approx_token_count` and 13 for `truncate_middle_to_tokens` with its marker and its head-tail split

**Acceptance Criteria:**
- [ ] Given any string, when its approximate token count is computed, then it equals the ceiling of the code-point count divided by four, using `chars().count()` and never the UTF-8 byte length
- [ ] Given a string of non-ASCII characters, when its count is computed, then it matches the reference for the same input, proving the code-point choice
- [ ] Given a budget of zero or less, when truncation runs, then the result is the empty string
- [ ] Given a string that already fits the budget, when truncation runs, then the string is returned unchanged
- [ ] Given a string that does not fit and a budget whose character allowance exceeds the marker length, when truncation runs, then the result is the head, the marker and the tail, with the head taking the floor of half the remaining allowance and the tail the rest
- [ ] Given a budget whose character allowance is at most the marker length, when truncation runs, then the result is the input truncated to the allowance with no marker
- [ ] Given a multi-byte character sitting on a computed boundary, when truncation runs, then the slice falls on a character boundary and never panics

#### US-144: Reproduce summary extraction and oldest-round dropping
**Description:** As a parity maintainer, I want the summary parser and the transcript trimmer reproduced so that the manager's failure classification and its retry ladder rest on the same primitives.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None
**Reference:** [vibe/core/compaction/context.py:36](/home/arthur/dev/mistral-vibe/vibe/core/compaction/context.py) for `extract_summary` and its regex, 43 for `drop_oldest_round` and the round definition its docstring gives

**Acceptance Criteria:**
- [ ] Given text containing a summary element, when extraction runs, then the inner text is returned trimmed, with the match spanning newlines
- [ ] Given text whose summary element is empty or whitespace only, when extraction runs, then nothing is returned, which is the empty-summary failure signal
- [ ] Given text with no summary element, when extraction runs, then nothing is returned
- [ ] Given text with more than one summary element, when extraction runs, then the first is returned
- [ ] Given a message list whose head is a system message followed by one or more user messages and everything they triggered, when the oldest round is dropped, then the result is the system message followed by everything from the next user message onward
- [ ] Given a message list holding only the system message and the most recent round, when the oldest round is dropped, then nothing is returned, which is the retry ladder's exhaustion signal
- [ ] Given a message list whose second element is not a user message, when the oldest round is dropped, then the leading assistant and tool messages are dropped with it

#### US-145: Render and parse the compaction envelope
**Description:** As an operator, I want the compaction envelope to carry my previous messages in the reference's structure so that a second compaction can read them back instead of losing them.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-144
**Reference:** [vibe/core/compaction/context.py:62](/home/arthur/dev/mistral-vibe/vibe/core/compaction/context.py) for `render_compaction_context`, 11 to 27 for the reserved tag constants, 90 for the escaping, 115 for `parse_previous_user_messages` and 129 for the envelope classification

**Acceptance Criteria:**
- [ ] Given a list of preserved messages and a summary, when the envelope is rendered, then it matches the reference byte for byte, including the continuation preamble, the blank lines, the previous-messages block, one element per preserved message and the summary block
- [ ] Given a preserved message whose content contains any of the four reserved tags, when the envelope is rendered, then each occurrence is HTML-escaped without escaping quotes, so the block cannot be reopened by its own content
- [ ] Given an empty list of preserved messages, when the envelope is rendered, then the block is present and empty rather than omitted
- [ ] Given a rendered envelope, when it is parsed, then the preserved messages come back in order and with their content unchanged
- [ ] Given text with no previous-messages block, or with an opening tag and no closing tag, when it is parsed, then the result is empty rather than an error
- [ ] Given a user message that is injected and carries all four block markers, when it is classified, then it is recognized as a compaction envelope; given a message missing any one of them, then it is not

#### US-146: Select the user messages that survive under budget
**Description:** As an operator, I want the last 20 000 tokens of my own words preserved through a compaction so that the agent keeps the constraints I stated before it.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-143, US-145
**Reference:** [vibe/core/compaction/context.py:10](/home/arthur/dev/mistral-vibe/vibe/core/compaction/context.py) for the 20 000-token budget and 141 for `collect_prior_user_messages`

**Acceptance Criteria:**
- [ ] Given a transcript of user, assistant and tool messages, when selection runs, then only user messages with non-empty content are considered, in transcript order
- [ ] Given a prior compaction envelope in the transcript, when selection runs, then the messages it holds are parsed out and treated as candidates in its place
- [ ] Given an injected user message that is not an envelope, when selection runs, then it is skipped, including the case where it starts with the summary marker
- [ ] Given candidates whose total is under budget, when selection runs, then all are returned in transcript order, each marked injected
- [ ] Given candidates whose total exceeds the budget, when selection runs, then the walk proceeds newest first and stops when the budget reaches zero
- [ ] Given a candidate that does not fit the remaining budget, when selection runs, then it is middle-truncated to exactly the remaining budget, it is the last one included, and the budget is then zero
- [ ] Given a budget of zero, when selection runs, then nothing is selected
- [ ] Given a transcript compacted twice, when the second envelope is rendered, then it holds the surviving messages of the first merged with the newer real turns

#### US-147: Capture the compaction oracle and replay it
**Description:** As a parity maintainer, I want a committed corpus of the reference's own answers so that the compaction score is a number a command produced rather than a reading of module presence.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-142, US-146
**Reference:** [vibe/core/compaction/manager.py:39](/home/arthur/dev/mistral-vibe/vibe/core/compaction/manager.py) for the `CompletionFn` protocol the capture stubs. The reference test files name the cases and are read for cases only, never for code: `tests/core/compaction/test_compaction.py` (29 functions) and `tests/test_middleware.py` (33). The script's shape follows the local `scripts/parity/tool_execution.py`

**Acceptance Criteria:**
- [ ] Given `scripts/parity/compaction.py`, when it runs, then it reads the pinned commit through `git archive`, re-executes itself with the reference interpreter, accepts `--reference`, and never moves HEAD, creates a branch or adds a worktree
- [ ] Given the script, when it runs, then it records seven families: token counts, truncations, summary extractions, round drops, rendered envelopes, parsed envelopes and message selections
- [ ] Given the script, when it runs, then it also drives `CompactionManager` with a stubbed completion callable and records, per scenario, the sequence of calls with their model, whether tools were passed, the retry count, whether the fallback ran, and the resulting summary or failure reason
- [ ] Given the capture, when it is committed to `crates/vibe-core/tests/compaction/corpus.json`, then it holds scenario-supplied values, counts, tag names and pointers, and stores as a SHA-256 digest anything that would carry reference-authored prose
- [ ] Given the corpus, when `compaction_parity_tests` runs on a machine with no reference checkout, then the replay still executes and only the recapture probe skips, with a message naming the pin and the restore command
- [ ] Given the replay, when it runs, then it prints per-family conformance counts and the total scenario count, which is at least 60
- [ ] Given a divergence not named in the ledger, when the replay runs, then it fails naming the family, the scenario and the field
- [ ] Given a ledger entry whose divergence no longer reproduces, when the replay runs, then it fails as a stale entry

---

### EP-043: Automatic Compaction in the Loop

Read the declared configuration, fire compaction on the threshold, bound the reactive path, and publish the event pair and its telemetry.

**Definition of Done:** All five compaction keys have an observable effect; compaction fires before the request when the threshold is reached; a reactive recovery happens at most once per turn and never in strict mode; and both events and both telemetry records are emitted.

#### US-148: Read the five declared compaction keys
**Description:** As an operator who configured compaction, I want each key I set to change what the tool does so that the published schema stops advertising features that do not exist.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-141
**Reference:** [vibe/core/config/vibe_schema.py:262](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py) for `compaction_model`, 263 for `auto_compact_threshold`, 435 for `context_warnings`, 440 for `raise_on_compaction_failure`, 444 for `compaction_prompt_id`, 501 for `get_compaction_model`, 620 for the global threshold propagation and 654 for the provider check

**Acceptance Criteria:**
- [ ] Given a compaction settings type in `vibe-core`, when it is built from a configuration snapshot, then it carries the threshold, the compaction model, the prompt identifier, the warning flag and the strict flag
- [ ] Given a configuration where the active model declares no threshold, when settings are built, then the global value is used, which is the behavior `propagate_auto_compact_threshold` already implements
- [ ] Given a configuration with no compaction model, when the model is resolved, then the active model is used
- [ ] Given a compaction model whose provider does not exist, when the configuration is validated, then validation fails naming the model alias and the missing provider
- [ ] Given each of the five keys in turn, when its value changes, then a test observes a different behavior, and the test names the key
- [ ] Given the app-server, when a session opens, then the settings are read once and carried on the session alongside the existing context window

#### US-149: Fire compaction when the threshold is reached
**Description:** As an operator running a long conversation, I want the session to compact before the request that would overflow so that I never see a provider refusal my configuration was supposed to prevent.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-148
**Reference:** [vibe/core/middleware.py:100](/home/arthur/dev/mistral-vibe/vibe/core/middleware.py) for `AutoCompactMiddleware` and [vibe/core/agent_loop/_loop.py:2608](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py) for the arithmetic that writes `context_tokens`, which is the value the threshold is compared against

**Acceptance Criteria:**
- [ ] Given a threshold greater than zero and a context at or above it, when the pipeline runs, then the auto-compaction middleware returns `Compact`
- [ ] Given a threshold of zero or less, when the pipeline runs, then the middleware returns `Continue` regardless of the context size
- [ ] Given a context below the threshold, when the pipeline runs, then the middleware returns `Continue`
- [ ] Given a turn-limit middleware and the auto-compaction middleware both triggering in the same cycle, when the pipeline runs, then the result is `Stop`, because the limit is registered first
- [ ] Given a scripted sequence of completions with known usage, when the turn runs, then the value the middleware reads equals the reference's `context_tokens` for the same sequence
- [ ] Given a compaction triggered by the threshold, when it completes, then the pipeline is reset with reason `Compact` and the next cycle builds its request from the compacted transcript

#### US-150: Bound the reactive recovery and honor strict mode
**Description:** As an operator, I want an overflow recovery to be attempted once and then reported so that a transcript that still overflows after compaction fails loudly instead of compacting forever.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-149
**Reference:** [vibe/core/agent_loop/_loop.py:1490](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py) for `_should_self_heal`, 1622 for the per-turn reset of the flag, 1641 for the overflow branch and 1644 for where the flag is set

**Acceptance Criteria:**
- [ ] Given a turn whose first request overflows, when the engine recovers, then it compacts once and retries without spending a step
- [ ] Given a turn whose request overflows a second time after a recovery, when the engine handles it, then it does not compact again and the overflow is reported
- [ ] Given a new user turn after a recovery, when its first request overflows, then a recovery is allowed again, because the guard resets per turn
- [ ] Given strict mode enabled, when a request overflows, then no recovery is attempted and the overflow is reported directly
- [ ] Given a cancellation while a reactive compaction is running, when the turn finalizes, then it finalizes once with the cancelled stop reason and the existing cancellation test still passes

#### US-151: Publish the compaction event pair and its telemetry
**Description:** As an editor integration author, I want a progress entry when compaction starts and a patch when it ends so that the panel shows the operation instead of its aftermath.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-149
**Reference:** [vibe/core/types.py:576](/home/arthur/dev/mistral-vibe/vibe/core/types.py) for `CompactStartEvent` and 586 for `CompactEndEvent`, [vibe/core/agent_loop/_loop.py:1444](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py) for `_run_compaction` and its status handling, and [vibe/core/telemetry/send.py:286](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py) and 305 for the two payloads

**Acceptance Criteria:**
- [ ] Given a compaction about to run, when it starts, then an event carrying a correlation identifier, the current context tokens and the threshold is emitted before any model call
- [ ] Given a compaction that finished, when it ends, then an event carrying the same correlation identifier, the summary length in characters, the old session identifier and the new one is emitted
- [ ] Given a transcript written before this change, when it is replayed, then the pre-existing compaction variant still deserializes and projects as it did
- [ ] Given a compaction that succeeded, failed or was cancelled, when it finalizes, then one telemetry record named for the reference's auto-compaction event is written with the tokens before, the threshold and the matching status
- [ ] Given a compaction whose summarization failed with a classified reason, when it finalizes, then one telemetry record named for the reference's failure event is written carrying that reason
- [ ] Given a compaction that failed or was cancelled, when the turn continues, then no end event is emitted and no session handoff is recorded
- [ ] Given telemetry disabled, when a compaction runs, then neither record is written

---

### EP-044: The Faithful Summarizer and Its Wire Surface

Rebuild the summarizer in `vibe-core` with the reference's call shape, failure taxonomy, fallback and retry ladder, account for its usage, and project the result onto the wire models the census already declares.

**Definition of Done:** The summarizer resolves its prompt from configuration, calls with tools, classifies both failures, falls back outside strict mode, retries on overflow, credits the ledger, resets the context tokens, and the app-server produces `CompactionDetails` with all five fields.

#### US-152: Resolve the compaction request and make the primary call
**Description:** As an operator, I want the compaction request to come from the configured prompt and to ride my conversation's own token prefix so that compaction is both customizable and cache-friendly.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-147, US-148
**Reference:** [vibe/core/compaction/manager.py:88](/home/arthur/dev/mistral-vibe/vibe/core/compaction/manager.py) for `compact`, 112 for `_summarize` and 137 for `_primary`, plus [vibe/core/config/vibe_schema.py:596](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py) for the `compaction_prompt` property and [vibe/core/prompts/__init__.py:75](/home/arthur/dev/mistral-vibe/vibe/core/prompts/__init__.py) for the override chain. The three prompt files are read to enumerate directives, never to copy prose

**Acceptance Criteria:**
- [ ] Given a prompt identifier, when the request is resolved, then a matching `.md` file in the project prompt directories wins, then one in the user prompt directories, then the built-in
- [ ] Given an identifier that matches nothing, when the request is resolved, then resolution fails with a message naming the setting, the value, the available built-ins and the directories searched
- [ ] Given extra instructions, when the request is built, then they are appended under their own heading after the base request
- [ ] Given a compaction, when the primary call is made, then it is made on a copy of the live transcript with the request appended as a user message, with the available tools and the tool choice passed, and the live transcript is not mutated
- [ ] Given a primary response carrying tool calls, when it is classified, then the failure reason is the tool-call reason and no summary is taken from the text
- [ ] Given a primary response whose text has no usable summary element, when it is classified, then the failure reason is the empty-summary reason
- [ ] Given a primary response with a usable summary, when it is classified, then the summary is returned and no fallback runs
- [ ] Given the three built-in prompts, when they are read, then they are original prose covering the reference's directives and no byte of reference prose appears in this repository

#### US-153: Fall back to a dedicated summarizer call
**Description:** As an operator, I want a failed summarization retried with a dedicated prompt so that a compaction degrades to a second attempt instead of straight to a placeholder.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-152
**Reference:** [vibe/core/compaction/manager.py:157](/home/arthur/dev/mistral-vibe/vibe/core/compaction/manager.py) for `_fallback` and 210 for `_render_transcript`

**Acceptance Criteria:**
- [ ] Given a primary failure and strict mode disabled, when the manager continues, then exactly one fallback call is made
- [ ] Given a primary failure and strict mode enabled, when the manager continues, then no fallback is made and the classified failure is raised
- [ ] Given a fallback call, when it is built, then it carries a dedicated system message, the compaction model with thinking forced off, no tools and no tool choice
- [ ] Given a fallback call, when its user message is built, then it carries the request followed by the conversation rendered as a transcript
- [ ] Given a transcript rendering, when it runs, then system messages are skipped, each remaining message is a heading naming its role followed by its text, each tool call is rendered as its name and arguments, reasoning is dropped, and a message with neither text nor tool calls is omitted
- [ ] Given a fallback that also produces no usable summary, when the manager finishes, then the failure reported is the primary's reason, not the fallback's
- [ ] Given a failure with strict mode disabled and no usable summary from either call, when compaction completes, then a placeholder summary is used and the conversation is still compacted

#### US-154: Retry the summarization on overflow
**Description:** As an operator whose transcript is too large to summarize in one call, I want the summarizer to shed the oldest rounds and retry so that compaction succeeds instead of failing on the very condition it exists to solve.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-153
**Reference:** [vibe/core/compaction/manager.py:27](/home/arthur/dev/mistral-vibe/vibe/core/compaction/manager.py) for the retry constant and 181 for `_summarize_call`, which holds the ladder

**Acceptance Criteria:**
- [ ] Given a summarization call that overflows, when it is retried, then the oldest round is dropped and the call is remade with the trimmed history
- [ ] Given repeated overflows, when the ladder runs, then at most three retries occur and the fourth overflow is propagated
- [ ] Given an overflow when only the system message and the most recent round remain, when the ladder runs, then the overflow is propagated immediately
- [ ] Given a successful retry, when the manager continues, then the trimmed history is what a subsequent fallback renders, not the original
- [ ] Given a non-overflow provider error, when it occurs, then it is propagated without any retry

#### US-155: Account the compaction and reset the context
**Description:** As an operator watching my spend, I want the compaction's own model call counted so that the token and price ceilings I set cover every request the tool makes.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-152
**Reference:** [vibe/core/compaction/manager.py:104](/home/arthur/dev/mistral-vibe/vibe/core/compaction/manager.py) for the transcript replacement and 108 for the context-token reset, plus [vibe/core/agent_loop/_loop.py:2403](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py) for `_complete`, the accounted call every compaction request goes through

**Acceptance Criteria:**
- [ ] Given a compaction that made one or more model calls, when the turn continues, then the usage of every call is credited to the turn ledger
- [ ] Given a compaction that made calls, when the price ceiling is evaluated afterward, then those calls are included in the total
- [ ] Given a compaction that completed, when the transcript is replaced, then the context token count is reset to zero and the next completion recomputes it from real usage
- [ ] Given a compaction that failed, when the turn continues, then the transcript is unchanged and no partial replacement is visible
- [ ] Given a compaction, when it completes, then the transcript is exactly the original system message followed by the envelope, and the envelope message is marked injected

#### US-156: Project compaction onto the wire
**Description:** As an editor integration author, I want the compaction entry and its details to match the protocol census so that I can render progress from declared fields instead of guessing.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-151
**Reference:** [vibe/app_server/_projector.py:638](/home/arthur/dev/mistral-vibe/vibe/app_server/_projector.py) for the in-progress entry and 654 for the patch, and [vibe/app_server/models.py:912](/home/arthur/dev/mistral-vibe/vibe/app_server/models.py) for `CompactionDetails`

**Acceptance Criteria:**
- [ ] Given a compaction start event, when it is projected, then a checkpoint entry of kind compaction is added in progress with a short label and details carrying the current context tokens and the threshold
- [ ] Given a compaction end event, when it is projected, then the same entry is patched with a completed label and details carrying the summary length, the old session identifier and the new one
- [ ] Given an end event whose start was never seen, when it is projected, then the entry is created first and then patched, so a late subscriber still sees a coherent entry
- [ ] Given the produced details, when they are validated against the app-server census, then all five declared field names and aliases match with no surplus and no missing required field
- [ ] Given a manual compaction through the existing method, when it completes, then its response and its notification are unchanged and their existing tests still pass
- [ ] Given the app-server surface replay, when it runs after this change, then the compaction models are no longer declared-only and the ledger entry that recorded them as unproduced is removed

---

### EP-045: Periphery, Identity and the Scorecard

Deliver the injected context warning, align the session identity a compaction mints, and remeasure the scorecard from the new oracle.

**Definition of Done:** A warning reaches the model once per session at half the window, a compacted session keeps its stable identity suffix, and `docs/parity.md` records the compaction score as a number the new oracle produced.

#### US-157: Inject the context warning once per session
**Description:** As an operator, I want the agent told when half my context is gone so that it starts summarizing its own work instead of discovering the wall.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-148
**Reference:** [vibe/core/middleware.py:112](/home/arthur/dev/mistral-vibe/vibe/core/middleware.py) for `ContextWarningMiddleware`, its latch and its reset

**Acceptance Criteria:**
- [ ] Given the warning flag disabled, when the pipeline is built, then the warning middleware is not registered
- [ ] Given the flag enabled and a context at or above half the window, when the pipeline runs, then an inject result is returned whose message names the percentage used, the current tokens and the window, wrapped in the reference's warning tag
- [ ] Given the warning already emitted, when the pipeline runs again, then nothing further is injected
- [ ] Given a window of zero or less, when the pipeline runs, then nothing is injected
- [ ] Given a compaction, when the pipeline is reset with the compaction reason, then the latch clears and the warning can fire again
- [ ] Given a warning and another injecting middleware in the same cycle, when the pipeline runs, then both messages arrive as one injection joined by two newlines

#### US-158: Mint the compacted session identity the reference way
**Description:** As an operator resuming work, I want a compacted session to keep the stable part of its identifier so that the sessions on disk stay recognizable across compactions.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-155
**Reference:** [vibe/core/session/session_id.py:8](/home/arthur/dev/mistral-vibe/vibe/core/session/session_id.py) for `generate_session_id` and 22 for `extract_suffix`, plus [vibe/core/agent_loop/_loop.py:2665](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py) for `_reset_session` and its `keep_parent` distinction

**Acceptance Criteria:**
- [ ] Given a session identifier, when a compaction mints the next one, then it is UUID-shaped with a freshly random head and the previous identifier's trailing segment preserved
- [ ] Given a compaction, when the handoff is recorded, then the previous identifier is retained as the parent
- [ ] Given a message-list clearing that is not a compaction, when the next identifier is minted, then no parent is retained, matching the reference's distinction
- [ ] Given sessions written before this change, when they are listed, read and resumed, then their identifiers still resolve and no stored session is renamed
- [ ] Given two compactions of the same session, when both identifiers are compared, then the trailing segment is the same and the heads differ

#### US-159: Remeasure the scorecard from the new oracle
**Description:** As a parity maintainer, I want the scorecard updated from a measurement so that the compaction score is falsifiable and the parts that depended on it are corrected in the same change.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-147, US-156, US-157, US-158
**Reference:** No reference counterpart. The targets are the local `docs/parity.md` rows and `CHANGELOG.md`, updated from the counts the new replay prints

**Acceptance Criteria:**
- [ ] Given the compaction replay, when its counts are read, then `docs/parity.md` records the compaction score, the family counts and the reproduce command, in the form the checkpoints row already uses
- [ ] Given the configuration row, when it is reread, then the five compaction keys are no longer counted among declared-only keys and the row's wording is corrected
- [ ] Given the execution order table, when it is reread, then rank 10 is marked done and names this PRD
- [ ] Given every divergence this work chose to keep, when the scorecard is read, then each has a row under accepted divergences naming the reason and the repository artifact that holds it in place
- [ ] Given the weighted total, when it is restated, then the document says which parts were remeasured and which predate the remeasure, as the current header already does
- [ ] Given `CHANGELOG.md`, when it is read, then the user-visible changes are recorded under `## Unreleased`

## Functional Requirements

- FR-01: The system must declare a conversation policy pipeline whose `before_turn` returns on the first stop or compact result and otherwise returns every injection joined by exactly two newlines, in registration order.
- FR-02: The system must consult that pipeline at the top of every engine cycle, before any provider request is built.
- FR-03: The system must register the turn, price and token limits before the auto-compaction policy, so that a cycle reaching both resolves to stop.
- FR-04: The system must compact when the context token count is at or above a strictly positive threshold read from the active model's configuration.
- FR-05: The system must approximate a token count as the ceiling of a string's code-point count divided by four.
- FR-06: The system must preserve user messages through a compaction under a 20 000-token budget, walking newest first, skipping injected messages, re-parsing prior envelopes, and middle-truncating the message that spills over.
- FR-07: The system must render the compaction envelope with the reference's preamble, previous-messages block and summary block, escaping the reserved tags inside preserved content.
- FR-08: The system must parse a rendered envelope back into its preserved messages, and must return an empty result rather than an error for malformed input.
- FR-09: The system must resolve its compaction request from the configured prompt identifier, preferring a project override, then a user override, then the built-in.
- FR-10: The system must make the primary summarization call on a copy of the live transcript with the tools and the tool choice attached, and must not mutate the live transcript before success.
- FR-11: The system must classify a summarization failure as either a tool call or an empty summary, and must report the primary call's reason when both calls fail.
- FR-12: The system must make one fallback call with a dedicated system prompt, thinking off, no tools and a rendered transcript, unless strict mode is enabled.
- FR-13: The system must retry a summarization call at most three times on context overflow, dropping the oldest round before each retry, and must propagate the overflow when nothing older remains.
- FR-14: The system must replace the transcript with the original system message followed by the envelope, reset the context token count to zero, and persist before returning.
- FR-15: The system must NOT compact more than once per user turn in response to a provider overflow, and must NOT attempt any reactive recovery when strict mode is enabled.
- FR-16: The system must emit a compaction start event carrying the current context tokens and the threshold before any model call, and a compaction end event carrying the summary length and both session identifiers after the handoff.
- FR-17: The system must project those events as one checkpoint entry created in progress and then patched, with details matching the declared compaction detail model.
- FR-18: The system must record one telemetry event for a triggered compaction with its status, and one for a classified failure with its reason, both honoring the telemetry toggle.
- FR-19: The system must inject a context warning once per session when the context reaches half the window, and must clear that latch when the pipeline is reset for a compaction.
- FR-20: The system must mint a compacted session identifier that preserves the previous identifier's trailing segment and retains the previous identifier as the parent.
- FR-21: The system must NOT remove or rename an existing engine event variant, so that transcripts written before this work still deserialize.

## Non-Functional Requirements

- **Correctness:** 0 divergent cases over the committed corpus across all seven families, with at least 60 scenarios replayed. Any divergence outside the named ledger fails the suite, as does a stale ledger entry.
- **Performance:** message selection over a 2 000-message transcript completes in under 50 ms on the reference workstation, since it runs before every turn once the pipeline is consulted. The pipeline itself adds under 1 ms per cycle with 5 middlewares registered.
- **Memory:** compaction operates on a bounded copy of the transcript; peak additional allocation stays under twice the transcript size, and the rendered transcript used by the fallback is dropped as soon as the call returns.
- **Reliability:** a summarization failure never leaves a partially trimmed conversation; the transcript is replaced atomically on success only, and a failure persists the untouched transcript before propagating.
- **Compatibility:** 100 % of transcripts written before this work deserialize and project unchanged; 0 stored sessions are renamed.
- **Security:** no prompt content, transcript content or summary text is written to telemetry; the two telemetry records carry counts, a status and a reason only.
- **Licensing:** 0 bytes of reference prose in this repository. The three prompts are original, and the corpus stores as a digest anything that would carry reference-authored text.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Threshold disabled | `auto_compact_threshold` at zero or below | Auto-compaction never fires; reactive recovery still works | none |
| 2 | Stop and compact in one cycle | Turn limit reached at the same time as the threshold | Stop wins; no compaction runs | Existing turn-limit stop reason |
| 3 | Model answers with a tool call | Summarizer decides to call a tool instead of summarizing | Classified as the tool-call failure; fallback runs outside strict mode | none |
| 4 | Summary element empty | Model returns the element with only whitespace | Classified as the empty-summary failure | none |
| 5 | Both calls fail, strict off | Primary and fallback both unusable | Placeholder summary; conversation still compacts; failure telemetry carries the primary reason | none |
| 6 | Both calls fail, strict on | Same, with strict mode enabled | No fallback; compaction fails; turn reports the compaction-failed error code with the reason | Error naming the reason |
| 7 | Overflow during summarization | Transcript too large for the summarizer itself | Oldest round dropped, retried, up to three times | none |
| 8 | Overflow with nothing to drop | Only the system message and the newest round remain | Overflow propagated immediately | Existing context-too-long error |
| 9 | Second overflow in one turn | Compacted transcript still overflows | No second recovery; overflow reported | Existing context-too-long error |
| 10 | Cancellation mid-compaction | Operator cancels while summarizing | Turn finalizes once as cancelled; transcript untouched; telemetry status cancelled | none |
| 11 | Reserved tags in user content | A user message literally contains the envelope tags | Tags escaped inside the preserved content; the block cannot be reopened | none |
| 12 | Budget spill | The next candidate does not fit the remaining budget | Middle-truncated to the remaining budget, marker inserted, selection stops | none |
| 13 | Multi-byte character on a slice boundary | Truncation lands inside a character | Slice moves to a character boundary; never panics | none |
| 14 | Second compaction | A transcript already holding an envelope is compacted again | Previously preserved messages parsed out and merged with newer turns | none |
| 15 | Unknown prompt identifier | `compaction_prompt_id` names no built-in and no file | Resolution fails naming the setting, the value, the built-ins and the directories searched | Error listing valid values |
| 16 | Compaction model with a missing provider | `compaction_model` points at an unconfigured provider | Configuration validation fails naming the alias and the provider | Validation error |
| 17 | Empty transcript | Compaction requested on a session with only a system message | Compaction is a no-op that still succeeds and leaves the system message in place | none |
| 18 | Telemetry disabled | `enable_telemetry` off | Neither compaction record is written; behavior otherwise identical | none |
| 19 | Old transcript replayed | A session persisted before this work is resumed | The pre-existing compaction event deserializes and projects as before | none |
| 20 | Reference checkout absent or off-pin | CI machine or a workstation that moved on | The corpus replays; only the recapture probe skips, naming the pin and the restore command | Skip message with the restore command |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Making the pipeline the budget authority changes an observable stop reason | Med | High | US-140 requires every existing stop-reason test to pass unmodified; the middlewares test the same quantities and only the call site moves |
| 2 | The token approximation diverges on non-ASCII because of a byte-versus-code-point mistake | Med | High | US-143 carries an explicit non-ASCII criterion and the corpus captures non-ASCII cases; the trap is named in the research section |
| 3 | Turning compaction on before the envelope is correct writes lossy transcripts that cannot be repaired | Low | High | The dependency order forces it: US-145 and US-146 precede US-149, and the trigger story is blocked by the settings story which is blocked by the pipeline |
| 4 | Original prompt prose produces materially worse summaries than the reference's | Med | Med | The prompts are held to directive coverage, and the fallback path plus the retry ladder bound the damage of a weak summary; the corpus measures the decision tree, not the summary quality, which is stated as a non-goal |
| 5 | Adding a second engine event breaks transcript deserialization | Low | High | FR-21 forbids removing or renaming a variant; new fields default; US-141 and US-151 both carry a replay criterion |
| 6 | Crediting compaction usage to the turn ledger pushes turns over their token ceiling sooner | Med | Low | This is the reference's behavior and the point of the story; the ceiling now covers every request, which is the correct accounting |
| 7 | Changing the minted session identifier orphans sessions on disk | Low | High | US-158 forbids renaming any stored session and requires listing, reading and resuming to still resolve |
| 8 | The corpus grows large enough to slow the suite | Low | Low | The families are pure calculations over small inputs; the tool-execution corpus at 21 KB is the precedent, and the app-server one at 343 KB replays without issue |
| 9 | Scope creep into plan mode through the read-only middleware | Med | Med | The read-only agent middleware is an explicit non-goal; the pipeline reserves its slot without implementing it |

## Non-Goals

Explicit boundaries. What this version does NOT include:

- **The read-only agent middleware and plan mode.** The pipeline is built to hold it and the reference registers it there, but plan mode is a separate part of the scorecard with its own dependencies. This work reserves the slot and implements none of it.
- **Summary quality benchmarking.** The corpus measures the decision tree, the selection, the envelope and the classification. Whether the summary text is good is not a parity question and has no reference oracle that `NOTICE` permits.
- **The teleport summary request.** `render_teleport_summary_request` lives in the same module and belongs to the teleport workflow, which the scorecard tracks separately at 85. It is ported when teleport is measured, not here.
- **Compaction of subagent transcripts.** The reference runs subagents through the same loop, but subagent parity has its own row and its own dependencies.
- **Re-pinning the reference.** A re-pin regenerates all corpora and is its own change.
- **Shipping the telemetry envelope.** The two compaction records are kept locally like every other event here, under the divergence `docs/parity.md` already records.
- **Replacing the manual compaction method.** `session/compact/start` is already conformant and its response shape is unchanged; only what happens underneath it improves.

## Files NOT to Modify

Outside US-142, which re-pins and regenerates every corpus as one indivisible change, these are never touched:

- `crates/vibe-core/src/parity.rs` and `scripts/parity/pin.py` - the two pin sources. US-142 moves them together; no other story reads or writes them, and a second edit means an accidental second re-pin
- `crates/vibe-app-server/tests/app-server-surface/corpus.json` - regenerated once by US-142 and then read-only. The compaction models are recorded there and this work produces them rather than editing the record
- `crates/vibe-core/tests/config-surface/corpus.json` - regenerated once by US-142 and then read-only. The five compaction keys are captured there and this work reads them, it does not redeclare them
- `crates/vibe-app-server/tests/tool-surface/*`, `crates/vibe-app-server/tests/tool-execution/corpus.json` and the checkpoint, shell, permission and tool-config corpora - regenerated once by US-142 and then out of scope; they cover unrelated surfaces
- `NOTICE` - the licensing boundary this PRD is written under

## Technical Considerations

Framed as questions for engineering input, not mandates:

- **Architecture:** where does the pipeline live? Recommended: `crates/vibe-core/src/middleware.rs` with the compaction engine at `crates/vibe-core/src/compaction/{context.rs, manager.rs}`, mirroring the reference's pure-core split and keeping `engine.rs` from growing past its current 2 258 lines. The app-server keeps only the provider-bound `Compactor` implementation, which becomes a thin adapter over the core manager.
- **The budget authority question:** should `exhausted_budget` become three middlewares, or stay and be consulted alongside the pipeline? Recommended: migrate, because ordering against auto-compaction is observable and two authorities cannot express it. The falsifier is the existing stop-reason suite, which must pass unmodified.
- **Usage accounting:** the current `Compactor` trait returns no usage, so the engine cannot credit it. Recommended: extend the compaction result with a usage field and have the engine credit it, rather than giving the compactor a handle on the ledger, which would invert the dependency.
- **The completion port:** the reference injects a `CompletionFn` protocol. Recommended: mirror it as a trait object in `vibe-core` so the manager stays provider-neutral and the oracle can drive it with a scripted stub, which is what makes a backend-free corpus possible.
- **Event compatibility:** adding two variants next to the existing one leaves three ways to express a compaction in a transcript. Recommended: keep the old variant as read-only compatibility with a doc comment saying so, following the `SessionHandoffCause` precedent, and never emit it again.
- **Prompt storage:** where do the three original prompts live? Recommended: alongside the existing system prompt assets in `vibe-core`, resolved through the same override chain, so a custom `.md` works for compaction exactly as it does for the system prompt.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Compaction parity score | 55 | 100 | Month-1 | `docs/parity.md`, from the counts printed by `cargo test -p vibe-core --all-features compaction_parity_tests -- --nocapture` |
| Corpus scenarios replayed | 0 | at least 60 across 7 families | Month-1 | The replay's own printed counts |
| Divergent cases | not measurable | 0 outside the ledger | Month-1 | The replay fails on any divergence outside the ledger |
| Compaction keys with a consumer | 1 of 5 (`auto_compact_threshold`, display only) | 5 of 5 with a behavior test each | Month-1 | A named test per key, listed in US-148 |
| Compaction models produced on the wire | 0 of 1 (`CompactionDetails` declared, unproduced) | 1 of 1 with all 5 fields | Month-1 | `app_server_surface_parity_tests` census validation |
| Provider overflow errors reached before compaction | 1 per threshold scenario | 0 | Month-1 | The threshold scenarios in the engine test suite |
| User messages surviving a compaction | 0 | up to 20 000 tokens, matching the reference selection | Month-1 | The selection family of the corpus |
| Middleware ordering scenarios resolved as the reference does | not expressible | 100 % | Month-1 | The pipeline tests in US-139 and US-149 |

## Open Questions

- Should the three limit middlewares carry the reference's formatted reason strings, or keep this port's `TurnStopReason` values as the only stop vocabulary? The corpus validates the stop reasons and nothing validates the strings; engineering to confirm during US-140 that no client reads a reason string today.
- Does any current client render the compaction checkpoint entry's message as the summary text? If one does, US-156 changes what it displays from the full summary to a short label, which is the reference's behavior but a visible change; to be checked against `vibe-cli` and `vibe-acp` before US-156 lands.
- Should the compaction prompts be overridable per agent profile as well as per project and user? The reference resolves them through the harness files manager only; deferred unless a profile-scoped need appears.
[/PRD]
