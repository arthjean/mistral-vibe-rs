# Parity Scorecard

Structural audit of Mistral Vibe RS against the upstream Python implementation,
with the execution order derived from it.

| Field | Value |
|---|---|
| First audit | 2026-08-04, Rust `5617d0c` |
| Last remeasure | 2026-08-08, from the compaction oracle. Only the compaction row was restated, from 55 to 100 against the counts `compaction_parity_tests` prints, and the configuration and execution-order rows amended for the five keys it now consumes; every other part carries the number its own remeasure produced, so the weighted total below is a hand-maintained judgement over rows measured at different dates. The `b78b451` re-pin earlier the same day restated the app-server row and recaptured the chat-input trace corpus, which had sat at `99a6efa9` (2.23.2), and `chat_input_parity` now fails when a corpus and the pin disagree |
| Python reference | `b78b451`, reference package version 2.24.0, against this port's `[workspace.package] version` 2.23.1 |
| Pin sources | `vibe_core::parity::REFERENCE_COMMIT` and `EXPECTED_COMMIT` in `scripts/parity/pin.py`, held equal by `crates/vibe-core/src/parity/parity_tests.rs` |
| Restore an off-pin checkout | `git -C /home/arthur/dev/mistral-vibe checkout b78b451c39eab9213393ad2f45908e8562a5c5e7` |
| Weighted score | 77/100 (was 76 at the last remeasure, 65 at first audit) |

## Method

Scores measure **reproduced surface**, not behavioral conformance. They come from
diffing inventories, not from running both implementations side by side:

- app-server JSON-RPC method names (91 upstream `SERVER_METHODS` plus 7 `clientTool/*`, counted from the pinned checkout by `scripts/parity/app_server_surface.py`);
- user-facing configuration keys (`vibe/core/config/vibe_schema.py` against
  `LayeredConfig::schema()`);
- published agent tool names and schemas;
- slash commands and CLI flags;
- module presence and depth, measured in lines of production code.

A part scores 100 when its externally observable contract is fully reproduced.
Anything measured only by module presence carries more uncertainty than anything
measured by name-level diff, and the table notes which is which.

Where a differential oracle exists, the score comes from running it rather than
from reading source. The tool surface was the first part in that category, the
configuration surface the second, through
`crates/vibe-core/tests/config-surface/corpus.json` and the two modules that
replay it, and the app-server protocol the third, through
`crates/vibe-app-server/tests/app-server-surface/corpus.json`.

### Measured volumes

| Tree | Lines |
|---|---|
| `vibe/` (Python, tests live in `tests/`) | 89 279 |
| `crates/` total | 117 066 |

## Parity by part

| Part | Score | State and gaps |
|---|---|---|
| Distribution, updates, installers | 95 | `install.sh`/`install.ps1`, archives, checksums, rollback, shell completions, `action.yml`. Exceeds upstream PyInstaller packaging |
| Slash commands | 95 | 26 of 27 aliases identical. Missing `/retry` (and its `turn/retrying` notification) |
| Built-in tools | 98 | **Measured by two differential oracles**. Surface: 12/12 names and 12/12 schemas on the base surface, 16/16 under the managed shell rollout, 10/10 on the Windows families, 38/38 against the committed digest, and 92/92 argument fixtures now returning the reference verdict. Execution: 35/41 cases match the reference over the committed fixture tree, and the 6 others diverge only in the wording of a warning or an applied-edit message, which `NOTICE` keeps out of reach. `grep` runs on the ripgrep library crates with smart case, the ignore files and the 23 configured exclusion globs; `read_file`, `write_file`, `edit` and `todo` publish the reference result fields and the field-per-line rendering the agent loop sends. Descriptions and message text are compared for presence only, never text, as `NOTICE` requires |
| Tool infrastructure (registry, schemas, filtering) | 95 | `object_schema` removed, `apply_defaults` applies schema defaults, `validate_arguments` understands `$ref`, `anyOf`, `items` and array-form `type`. `coerce_and_validate` reproduces the reference Pydantic lax coercion before dispatch, so a handler reads the coerced value, proven by 92/92 replayed fixtures with 0 wrongly accepted and 0 stricter. **Per-tool configuration is measured by a third oracle**: 26/26 tool classes, 22/22 keys and 146/146 `(tool, key)` pairs declared and read, replayed against `tests/tool-config/defaults.json`. `matching.rs` matches globs, `re:` prefixes and is case-insensitive |
| Worktree (`--worktree`) | 90 | `startup/worktree.rs`, full create/reuse/cleanup/branch lifecycle |
| Managed shell and terminals | 92 | `TerminalManager` plus the reference shell policy, and the `bash_*`, `git_bash` and `powershell` families now publish conformant schemas. **The policy is measured by a differential oracle**: 28/28 grammar extractions, 45/45 path-inspecting commands with 0 missing and 0 invented, 23/23 escaping-operand cases and 60/63 resolutions match the reference, the 3 others being ledgered places this port asks where the reference grants. Reproduce with `cargo test -p vibe-core --all-features shell_parity_tests -- --nocapture`. A managed session now runs under a real PTY on POSIX, so a program that probes for a terminal finds one, a control key written through `<family>_stdin` reaches the foreground process group, and a hard timeout terminates the whole group including a grandchild that outlived its parent. Each session writes a manifest beside its log, and one left behind by a previous process is listed, read and inspected as `orphaned`. Residual: the session behavior is asserted by named tests rather than by a differential oracle, the Windows families still execute on pipes, and a host that provides no PTY backend falls back to pipes with the reduced capability reported as a null `ptyBackend` |
| CLI surface (flags, modes) | 90 | Every upstream flag present, and tool filtering now matches globs and `re:` prefixes. Missing the `vibe mcp ...` subcommand |
| ACP (`vibe-acp`) | 85 | Agent, sessions, updates, teleport, mcp, proxy. Comparable volume |
| Rewind | 95 | Both methods take `entryId` and answer the declared shapes, validated against the app-server census. The restore plan comes from the checkpoint log rather than from a parallel snapshot store, so a partial write reports one error per path and still keeps what landed, and a message-list reset that is not a rewind clears the log and reopens a running turn. Residual: the TUI confirms in one step where the reference confirms in two, recorded in `crates/vibe-cli/tests/runtime-parity/session-management-ep008.json` |
| LLM backends | 85 | 6 styles (mistral, openai, reasoning, openai-responses, anthropic, vertex-anthropic), SSE streaming, retry. Image, cache and tool-id adaptation details unverified |
| MCP | 87 | stdio, streamable-http, OAuth, registry, toggle. Tools now published as `{alias}_{tool}` matching upstream. Sampling is served on both transports: an entry that enables it advertises the capability at `initialize` and its `sampling/createMessage` requests are answered by the provider the turn itself runs on, with the system prompt prepended, an unknown role read as assistant, non-text blocks skipped, and a backend failure returned as a structured error rather than as a partial completion; an entry that disables it advertises nothing and refuses the request with the capability-absent error. Missing `mcp/authUrl` and the `vibe mcp` CLI |
| Trusted folders and permissions | 92 | **Partly measured by differential oracle**: the 4 reference permission scopes are spoken with 0 missing and 0 invented, the requirement carries exactly `scope`, `invocationPattern`, `sessionPattern` and `label`, the 138-entry arity table matches entry for entry, and 20 session-pattern and 19 wildcard cases replay the reference verdict. Reproduce with `cargo test -p vibe-core --all-features permission_parity_tests -- --nocapture`. `policy.rs` keeps this port's own trust roots, leases and atomic revocation on top, which the reference expresses as project roots. The shell policy now resolves from the four reference lists and is measured by its own oracle, recorded under managed shell |
| Programmatic mode (`-p`) | 85 | text / json / streaming all implemented |
| TUI (composer, transcript, pickers) | 80 | Broad coverage, backed by a dedicated observable-parity harness (JSON traces plus Python oracles). Missing vim navigation, word selection, `load_more`, braille rendering |
| Review and turn diff | 95 | **Measured by the checkpoint oracle plus the app-server census**: the six methods answer from the session's checkpoint log rather than from a map production never wrote to, and 73/73 replayed scenarios conform on regions, scopes, projections and anchors. All seven decision targets resolve, a revert is written to disk and rolled back with the log when the write does not land, and a hand edit keeps its own owner slot. Reproduce with `cargo test -p vibe-core --all-features checkpoint_parity_tests -- --nocapture`. Residual: the four read methods are census-validated from a bare probe and from a written session, and the two mutations answer an empty object the census cannot discriminate |
| Sessions, resume, fork, history | 90 | `storage.rs`: metadata, pagination, migration, file locks, handoff journal. The two rewind methods now address a stable entry identity and both validate against the app-server census from a written session, with `SessionRewindResponse` carrying all five declared fields. Residual: `history/list` still answers a flat stored-message list where the reference answers a `PublicHistoryPage`, which is why a bare probe cannot reach it |
| app-server protocol | 93 | **Measured by differential oracle**, restated at the `b78b451` re-pin: 91 of 91 reference methods declared and 89 of 91 routed with 0 invented names inside the inventory, 15 of 15 notifications emitted with 0 invented, 7 of 7 `clientTool/*` issued, 12 of 12 error codes spoken, 19 of 22 enum vocabularies compared and 373 models in the census. 21 of the 23 methods a bare probe session can reach validate against the census with 0 missing required and 0 surplus aliases. Reproduce with `cargo test -p vibe-app-server --all-features app_server_surface_parity_tests -- --nocapture`. The nine `projectLinks/*` answers are validated against the same census from a repository fixture, since a bare probe reaches only their ineligible form. The two `session/rewind*` answers are validated the same way, from a session a test writes and drives a turn against. Residual: v2.24.0 added `identity/read` and `workspace/worktrees/list`, declared here and unrouted, gave `ConfigView` the three unpinned-model fields `config/read` and `runtime/read` therefore no longer carry, and made `PublicRetryCategory` reachable; `TurnStartParams.injected` is carried on the wire and defaults false, with the first path that sets it arriving with the compaction envelope; `TerminalEmulator` stays unmodeled, and 58 responses need a backend or a written session neither harness stands up, so they are declared and routed but not census-validated |
| Hooks | 75 | 1:1 on event types (PreTool, PostTool, PostAgent) with matcher, timeout, retries, strict |
| System prompt and project context | 75 | `AGENTS.md` walk-up, prompt resolution, skill and subagent summaries. Missing `include_*`, `system_prompt_id`, `project_context` |
| Agents, subagents, delegation | 75 | `AgentProfile`, `AgentRegistry`, `SubagentManager`, `agents/{list,install,uninstall}`, and `task` now published conformantly |
| Connectors | 70 | Registry, auth, refresh, toggle. Catalog scope unverified |
| Teleport and Vibe Code Web | 85 | `vibeCode/teleport/*`, `vibeCode/projects/*` and the session-less `projectLinks/*` all present, the last two sharing one saved-link store as upstream does. Teleport workflow states and the push flow are unmeasured against the reference |
| Autocompletion | 70 | Slash, path and fuzzy completion. Missing the file indexer with watcher (`file_watcher_for_autocomplete`) |
| Voice (STT, TTS, narrator) | 65 | `voice/{realtime,recorder,session,state}` plus `narrator.rs`, cpal wired. Missing transcribe and TTS provider/model configuration |
| Vibe Code Project | 85 | Workflow, picker and the `projectLinks` layer present, with the reference candidate ranking and the four root reject reasons. The project API client is measured by fixture rather than against the live service |
| Compaction | 100 | **Measured by a differential oracle**: 172 scenarios across 8 families replay the reference's own answers with 0 divergent outside a four-entry ledger, at 10/10 token counts, 11/11 truncations, 10/10 summary extractions, 8/8 round drops, 8/8 rendered envelopes, 11/11 parsed envelopes, 16/16 message selections and 98/98 manager scenarios covering the call sequence, the retry ladder, the fallback decision and the two failure reasons. Reproduce with `cargo test -p vibe-core --all-features compaction_parity_tests -- --nocapture`. `crates/vibe-core/src/middleware.rs` holds the policy pipeline the reference resolves ordering with, and `crates/vibe-core/src/compaction/` the pure calculation and the provider-neutral manager. Automatic compaction fires on `auto_compact_threshold` before the request rather than after a provider refusal, a reactive recovery happens at most once per turn and never under `raise_on_compaction_failure`, the envelope preserves the last 20 000 tokens of the operator's own turns, `compaction_model` addresses the summarization, `compaction_prompt_id` resolves the request through the project and user override chain, `context_warnings` injects one warning per session at half the window, the event pair projects as one checkpoint entry carrying all five `CompactionDetails` fields, and a compacted session keeps its stable identity suffix. Residual: the four ledger entries below, three of them licensing and one an unmodeled telemetry call type |
| Telemetry and observability | 60 | `telemetry.rs` with an intentionally divergent envelope. `telemetry/record` accepts the reference parameters and honors `enable_telemetry`, keeping the event locally rather than shipping it, which is a recorded divergence below. **OTel absent** (`enable_otel`, `otel_endpoint`, `otel_redaction`), no log reader |
| Skills | 55 | `SKILL.md` discovery, injection, `skills/list`, and the `skill` tool now published conformantly. Missing the remote registry (install, manifest, store), the builtin skills, and `enabled/disabled_skills`, `skill_paths` |
| Checkpoints | 100 | **Measured by a differential oracle**: 32/32 line fixtures, 36/36 opcode fixtures and 73 engine scenarios replayed against the reference with 0 divergent across all seven families (steps, regions, scopes, projections, anchors, restore plans, log shape) and an empty ledger. `crates/vibe-core/src/checkpoints/` holds the append-only log, the pure read model, the recorder and the filesystem port, with region identity, dependency edges, decision closure, reconstruction and hunk anchors all reproduced. Reproduce with `cargo test -p vibe-core --all-features checkpoint_parity_tests -- --nocapture`. The retention ceiling below is the one recorded divergence |
| Configuration | 92 | **Measured by differential oracle**: 64/68 reference fields declared, published and merged by the strategy the reference declares, 30/30 merge scenarios, 8/8 model-validation scenarios and 22/22 MCP entry scenarios replayed from the committed corpus, plus 8/8 reference `config/*` methods dispatched. Seven layers compose (Defaults, Discovered, SelectedToml, Experiments, Environment, Runtime, Agent). Residual: the GrowthBook layer and the per-layer async state machine are recorded divergences below, the fingerprint token has its own format, 5 keys this port declares have no upstream counterpart, and the `b78b451` re-pin left 4 v2.24.0 fields undeclared here with `active_model` still pinned, both recorded below. Declaring a key is not implementing its feature: each arrives with the feature that reads it, and the five compaction keys arrived with rank 10, each with a test that changes the key and observes the behavior change |
| Setup, onboarding, authentication | 35 | Linear 6-step flow plus keyring. **No multi-screen TUI onboarding, no browser sign-in** |
| Experiments and rollouts | 25 | `ConfigLayerKind::Experiments` and an experiments table only. No GrowthBook client, no rollout or experiment-session handling |
| VS Code extension promo | 10 | Not ported |

## Ordering principle

Sorting by ascending score is the wrong order. The primary criterion is **cost of
deferral**: a part goes first when postponing it forces migrating code already
written, data already persisted, or parity traces already recorded. Then comes
the number of downstream consumers, then user value, then cost.

## Execution order

| Rank | Part | Status | Why here |
|---|---|---|---|
| 1 | Tool names and schemas | DONE | Names were already written into persisted sessions, hook matchers and parity traces, so every deferred week multiplied the migration cost |
| 2 | Configuration mechanism | DONE | `config/patch`, `config/fields/read`, the registry-generated schema and the discovered layer all shipped through `tasks/prd-config-parity.md`, backed by a committed corpus. The 47 keys still without a consumer are declared, defaulted, published and merged; each feature still arrives on its own, and the five compaction keys left that set with rank 10 |
| 3 | Missing protocol notifications | DONE | 15 of 15 reference notifications are emitted and the four invented names are retired, replayed by `cargo test -p vibe-app-server --all-features app_server_surface_parity_tests -- --nocapture`. Accepting a plan with clearing now rotates the session inside the turn, which is what raises `session/contextCleared` |
| 4 | `write_file`, `grep`, the `bash` surface, `todo` | DONE | Shipped with rank 1 |
| 5 | `task`, `skill` | DONE | Shipped with rank 1 |
| 6 | `web_search`, `web_fetch` | DONE | Shipped with rank 1 |
| 7 | Tool name matching (globs, `re:`, case-insensitive) | DONE | `crates/vibe-core/src/matching.rs` |
| 8 | `clientTool/*` | DONE | All 7 server-to-client methods are issued, gated on the capabilities the client declared at `initialize`, replayed by the app-server surface corpus |
| 9 | Checkpoints | DONE | Delivered by `tasks/prd-checkpoints-parity.md`: the engine, its oracle, the review surface on top of it and rewind by entry identity |
| 10 | Automatic compaction | DONE | Delivered by `tasks/prd-compaction-parity.md`: the policy pipeline, the pure calculation and its oracle, the threshold trigger, the faithful summarizer and its wire surface, and the periphery |
| 11 | Skills, complete (remote registry, builtins) | TODO | Depends on the `skill` tool, now available, and on `skill_paths` keys |
| 12 | Specialized shells (`git_bash`, `powershell`) | DONE | Shipped with rank 1 |
| 13 | Browser sign-in and onboarding | TODO | Blocks adoption, blocks nothing technically downstream |
| 14 | `projectLinks/*`, autocompletion indexer, voice configuration | PARTIAL | `projectLinks/*` shipped with EP-029: all 9 methods routed and validating. The autocompletion indexer and the voice configuration are independent periphery, parallelizable |
| 15 | Telemetry and OTel | PARTIAL | `telemetry/record` shipped with EP-029. OTel depends on configuration and has no downstream consumer |
| 16 | Experiments and GrowthBook, VS Code promo | TODO | See accepted divergences |

## Accepted divergences

Some parts cannot reach 100 through code alone. For these, 100 means a decided
and documented divergence, not a port. Recording the decision early keeps it out
of every subsequent parity review. Each row names why the divergence stands and
what in the repository holds it in place.

| Part | Reason | Evidence |
|---|---|---|
| Tool description text | `NOTICE` forbids shipping upstream prose, so the 13.8 KB of reference directive text has no byte-identical counterpart here and never will. Descriptions are held to directive coverage and compared for presence only. **This does not cap the score in this document**, measured 2026-08-06 and settled two ways. By stated method: the scoring unit above is names and schemas, and description text is not among the inventories it diffs. By construction: the instrument that produces the tool scores replaces every description with `<described>` in both the capture and the replay, so prose carries exactly zero weight and no amount of it can move the number. The residue is therefore a licensing fact with no scoring consequence here | `DESCRIBED` in `scripts/parity/tool_surface.py` and `crates/vibe-app-server/src/tool_surface_parity_tests.rs`, which substitute every description before diffing, plus the `## Method` section above |
| A third-party score weighing description text | Not determinable from this repository, and recorded as such rather than left open. `tasks/prd-tool-surface-parity.md` records the externally reported 35/100 score as weighing names and schemas "in an unpublished way", so no method exists here to confirm or refute against. The exposure is bounded instead of guessed: measured at the pinned commit, the reference publishes 13 776 bytes of tool prompt prose and 1 735 bytes of parameter descriptions against 3 789 bytes of description-free schema and 94 bytes of names. A byte-weighing external metric would therefore hold roughly four fifths of the tool contract permanently out of reach, which would make that metric a measure of licensing posture rather than of engineering parity. Chasing it is refused on that basis | `vibe/core/tools/builtins/prompts/*.md` at `b78b451`, byte-counted out of tree; no prose is stored here |
| Three shell commands ask where the reference grants | The reference resolves `cat $(which ls)`, `cat file.txt > out.txt` and `git diff --no-index /etc/passwd /dev/null` to `always`, because each one is composed only of allowlisted words. What runs is not what the words say: a substitution runs a program the text never names, a redirect writes to a target the segment never carries, and `--no-index` reads a path the index never held. This port withholds the automatic grant in those three places and asks instead. Nothing is refused that the reference allows, so the divergence costs an approval prompt and never a capability | `STRICTER_THAN_THE_REFERENCE` in `crates/vibe-core/src/shell/shell_parity_tests.rs`, which fails both when the set grows and when a listed case stops diverging |
| `grep` returns its matches sorted | `rg` walks in parallel and prints whichever file finished first, so its match order is not a contract anything can conform to: one capture of the pinned reference recorded two different orders for the same query in a single run. This port sorts by path and then by line, and the capture script sorts the reference's output the same way before recording it, so the match *set* is still compared byte for byte while the order is normalized out on both sides | `stabilize` in `scripts/parity/tool_execution.py` and the sort in `crates/vibe-core/src/workspace/search.rs`, replayed by `tool_execution_parity_tests` |
| Warning and applied-edit message wording | `read_file` answers an empty file or an out-of-range offset with a sentence, and `edit` reports what it changed with one. Both are authored prose, which `NOTICE` forbids shipping and the PRD lists as a non-goal: a message is held to naming the same cause, value and limit, not to the reference's words. The divergence is bounded to those two fields and every other field of the same result still matches | `LICENSING` entries in `crates/vibe-app-server/src/tool_execution_parity_tests.rs`, which fail if the gap widens past those pointers or narrows to none |
| The checkpoint log holds at most 512 MiB | The reference constructs its log unbounded (`vibe/core/checkpoints/checkpointer.py:56`) and holds it for one agent loop's lifetime. This port holds one log per attached session inside a server process that outlives any single conversation, so an unbounded log is an unbounded process. The ceiling refuses further capture rather than trimming: a region identity a client already holds must never stop resolving because the log made room behind it. Reaching it publishes one warning on `diagnostics/list` and leaves the file writes themselves untouched | `RETAINED_BYTES_LIMIT` in `crates/vibe-core/src/checkpoints/checkpointer.rs`, asserted by `the_log_is_never_shortened_behind_the_caller` and `a_full_log_refuses_the_capture_and_publishes_it` |
| A rewindable entry identity is derived, not stored | The reference reads a message's own `message_id` and falls back to `history:{index}:{role}` when it has none (`vibe/app_server/_projection.py:607`). A message this port persists carries no identifier, so every rewindable entry resolves through that fallback. The identity is therefore stable for as long as the stored list is, which is exactly what a rewind cuts, and it moves when a compaction rewrites the list, at which point the checkpoint log is cleared with it | `history_entry_id` in `crates/vibe-app-server/src/release3.rs`, asserted by `rewind_resolves_an_entry_identity_and_forks_before_the_selected_message` |
| `edit` decodes a narrower codec set | The reference tries the byte-order mark, UTF-8, the locale codec and then `charset-normalizer`, a statistical detector. This port recognizes the mark, UTF-8, and falls back to Latin-1, which decodes every byte, so no file is refused and every one of the three round-trips byte for byte. A file in a multi-byte codec with no mark decodes as Latin-1 and is written back unchanged unless the edit touches it | `crates/vibe-core/src/workspace/text_file.rs` and its round-trip tests |
| `grep` is confined to the workspace root | The reference resolves the search path against its working directory and lets the permission chain answer for a path outside it. This port refuses it at the boundary instead, which is the confinement every other file tool here already applies. Nothing outside the workspace becomes readable that was not readable before | `Workspace::confined`, reached by `workspace::search` |
| Telemetry | The envelope already diverges intentionally from the upstream open-properties format | `CHANGELOG.md`, telemetry entry |
| `telemetry/record` keeps the event locally | The reference hands a client-authored name and free-form properties to the agent loop's telemetry client, which ships them under the open-properties envelope this port does not publish. The method accepts and validates exactly the reference parameters and honors `enable_telemetry`; the event is kept on `diagnostics/logs/read` instead of being shipped, and shipping it needs the envelope divergence above resolved first | `AppServer::telemetry_record` in `crates/vibe-app-server/src/server.rs`, asserted by `a_recorded_client_event_is_kept_only_while_telemetry_is_enabled` |
| The compaction envelope's two prose runs | `NOTICE` forbids shipping upstream prose, and the envelope carries two runs of it: the sentences that open it and the one that introduces the summary. This port writes its own covering the same three directives, so the envelope reads the same and no reference sentence ships. Everything its readers depend on is held to the reference byte for byte: the four reserved tags, the line layout, one element per preserved message, the escaping and the summary block. The corpus records each prose run's length and SHA-256 so a change on either side is still detected | `envelopeProse` in `crates/vibe-core/src/compaction/compaction_parity_tests.rs`, whose two ledger entries fail the replay if either run ever conforms, against `envelopeRenders` 8/8 conforming on structure |
| The placeholder a failed summarization falls back to | Outside strict mode the reference still compacts when neither summarization call produced a usable summary, and the summary it writes into the envelope is an authored sentence `NOTICE` forbids shipping. This port writes its own, so the conversation degrades the same way and no reference sentence ships; the decision tree around it, both failure reasons, the fallback and the retry ladder, is held to the reference exactly | `placeholderSummary` in `crates/vibe-core/src/compaction/compaction_parity_tests.rs`, whose ledger entry fails the replay if the wording ever conforms |
| The compaction request carries no telemetry call type | The reference labels its summarization request with a call type its telemetry client reads. Nothing here models that vocabulary, and the telemetry envelope is already a recorded divergence two rows above, so the request is marked through the provider metadata instead. The request itself, its transcript copy, its tools and its tool choice are the reference's | `managerCallType` in `crates/vibe-core/src/compaction/compaction_parity_tests.rs`, whose ledger entry fails the replay when a call type starts conforming |
| The context warning's message is original prose | `NOTICE` forbids shipping the reference's warning sentence, so this port writes its own naming the same three quantities: the share of the window consumed, the current token count and the window. Only the `vibe_warning` tag is reproduced verbatim, because it is an identifier a client matches on rather than prose | `ContextWarningMiddleware` in `crates/vibe-core/src/middleware.rs`, asserted by `the_context_warning_fires_once_at_half_the_window` |
| The context-warning latch clears on the compaction reset alone | The reference clears the latch on either reset reason, and resets its pipeline only when the conversation is compacted or cleared. This port also resets at the top of every turn, so clearing on both reasons would warn once per turn instead of once per session. Clearing on the compaction reason alone is what reproduces the reference's observable cadence under this port's own | `ContextWarningMiddleware::reset` in `crates/vibe-core/src/middleware.rs`, asserted by `the_context_warning_is_silent_without_a_window_and_relatches_on_a_compaction` and by `the_context_warning_reaches_the_model_once_per_session` in `crates/vibe-app-server/src/client.rs` |
| `identity/read` and `workspace/worktrees/list` are declared and unrouted | Both names arrived upstream at v2.24.0 and the `b78b451` re-pin brought them into the inventory, so the earlier statement that they exist nowhere in the reference tree is false at this pin and was removed. They are declared in `SERVER_METHODS`, so a client reading the inventory sees the same names, and routing them is app-server parity work rather than compaction work | `UNROUTED_METHODS` in `crates/vibe-app-server/src/app_server_surface_parity_tests.rs`, which fails the replay when either becomes routed |
| `ConfigView` omits the three unpinned-model fields | v2.24.0 added `activeModelPinned`, `defaultModelAlias` and `showGreeting`, all three produced from an `active_model` that may be left unpinned and resolved on read. Producing them means porting that feature, which is configuration parity work; until then `config/read` and `runtime/read` are recorded as diverging from the census rather than validated against a shape this port cannot fill | `DIVERGENT_RESPONSES` in `crates/vibe-app-server/src/app_server_surface_parity_tests.rs`, which fails the replay when either response starts validating |
| `PublicRetryCategory` is unmodeled | The vocabulary was recorded as absent until v2.24.0 reached it from `turn/error`. Nothing in this port classifies a retry, so the name is recorded rather than answered with a vocabulary no code produces | `UNMODELED_ENUMS` in `crates/vibe-app-server/src/app_server_surface_parity_tests.rs` |
| 4 configuration fields the v2.24.0 reference declares are undeclared here | `routed_default_model`, `routed_model_config`, `show_greeting` and `managed_shell_tools_enabled` all belong to features this port has not ported. Declaring the keys without the resolution they feed would publish a schema whose values change nothing, which is the failure this document names two rows below | `UNDECLARED_FIELDS` in `crates/vibe-core/src/config/surface_parity_tests.rs`, which fails the replay when any of them becomes declared |
| `active_model` is pinned where the reference leaves it unpinned | v2.24.0 ships an empty `active_model` as a sentinel meaning "not pinned" and resolves it on read from `routed_default_model`, then from the default alias. This port still ships the alias itself, which is the value that resolution produces when no routed default is configured, so the effective model is the same and only the stored document differs | `UNPINNED_ACTIVE_MODEL` in `crates/vibe-core/src/config/surface_parity_tests.rs`, which asserts the port pins exactly the alias the reference resolves to |
| Experiments and GrowthBook | Requires access to a third-party Mistral service with credentials this repository does not hold. The `experiments` table stays as the injection point | `tasks/prd-config-parity.md`, Non-Goals |
| VS Code extension promo | Advertises an extension that does not target this binary | No promo surface exists in `crates/`, and the parity table scores the part 10 for that reason |
| Configuration fingerprint format | Upstream builds the token from `st_dev:st_ino:st_mtime_ns:st_size` (`vibe/core/config/fingerprint.py:30`); this port digests the file contents. The token is opaque, is only ever compared against one produced by the same implementation, and a content digest detects an edit that restores the size and timestamp | `crates/vibe-core/src/config.rs`, `fingerprint_optional` and the concurrent-edit tests |
| Per-layer async state machine | Upstream caches each layer, forces reloads and transitions trust per layer (`vibe/core/config/layer.py:263`); this port recomposes from disk on every `load()`, which is observably equivalent for every current caller | `tasks/prd-config-parity.md`, Non-Goals |
| 5 configuration keys with no upstream counterpart | `thinking`, `notifications`, `proxy`, `tls_ca_path` and `dotenv_path` have no lossless reference target, and mapping them would reinterpret values already on disk | `crates/vibe-core/src/config/surface_parity_tests.rs`, which fails if the set changes |
| An unregistered key survives the merge | The reference merge drops a key its schema does not declare; keeping it lets a file written by a newer client round-trip through this one | `ConfigSnapshot::unregistered_keys`, asserted per corpus scenario |
| `config/batchWrite` | Writes several configuration targets in one request, which the reference client does with one `config/patch` per target. `vibe-cli` settings screens already depend on the atomic form; retiring it is its own migration | `LOCAL_EXTENSION_METHODS` in `crates/vibe-protocol/src/lib.rs`, kept out of `SERVER_METHODS` and out of `ServerCapabilities.methods`, asserted by `app_server_surface_parity_tests` |
| `connectors/toggle` | Enables or disables one connector without the full `config/patch` round trip the reference uses. Live callers in `vibe-cli` | Same as above |
| `mcp/auth/complete` | Completes the local MCP OAuth callback this port serves itself, which the reference delegates to its own browser flow | Same as above |
| `session/overrides/write` | Holds a model, a mode, a thinking level, a reasoning effort and an approval stance for one session's lifetime. Upstream none of the five is session-scoped: the model and the thinking level are configuration writes and the mode and the approval stance come from an agent profile. They used to ride on `session/settings/update`, which made that method accept five fields its reference model forbids; moving them here left it exactly `sessionId`, `maxTurns` and `maxTokens` | Same as above, plus `settings_update_is_strict_and_applies_to_the_next_turn_while_active`, which asserts each of the five is answered with `invalid_params` on the reference method |

## Verification

Scores are declarative until an oracle backs them. The tool surface is the first
part where that is no longer true: `crates/vibe-app-server/src/tool_surface_parity_tests.rs`
captures the reference surface from the pinned checkout, diffs it against what a
real session registers, and reports missing names, invented names and per-name
schema divergence as JSON pointers. `crates/vibe-app-server/tests/tool-surface/baseline.json`
holds the conformance target and currently records zero divergence on Linux.

The configuration surface is the second. `scripts/parity/config_surface.py`
drives the reference `ConfigBuilder` over synthetic layer stacks and records the
document it merges, the field census with each field's strategy, merge key and
editor kind, the default document, the model-validation outcomes and the MCP
entry decisions. `crates/vibe-core/tests/config-surface/corpus.json` is that
capture, committed because it carries names, pointers and values authored for it
and no reference-authored prose. `config::surface_parity_tests` and
`config::mcp_parity_tests` replay it unconditionally; only the probe that
recaptures from the pinned checkout skips when it is absent. A registry field
arriving without a corpus entry fails the replay naming the field, which is what
the `Configuration-surface conformance` CI step reports alongside the conforming
counts.

The app-server wire surface is the third. `scripts/parity/app_server_surface.py`
imports the reference `protocol` and `_connection_protocol` modules and records
the method inventory, the client-tool and notification vocabularies, the error
codes, the enum value sets, the discriminated unions and a per-model field
census with aliases and required flags.
`crates/vibe-app-server/tests/app-server-surface/corpus.json` is that capture,
committed on the same terms as the configuration one: names, aliases, pointers,
enum values and counts, and no reference-authored prose.
`app_server_surface_parity_tests` replays it unconditionally against what this
build declares, routes and answers, and only the probe that recaptures from the
pinned checkout skips. Each family carries a ledger of what is still divergent,
and a divergence that appears outside it fails the replay, as does a ledger
entry that has gone stale. Run it with `cargo test -p vibe-app-server
--all-features app_server_surface_parity_tests -- --nocapture`, which is where
the app-server score above comes from.

Tool *execution* is the fourth, and the first oracle in this repository that
compares output rather than declarations. `scripts/parity/tool_execution.py`
drives the reference's `read_file`, `grep`, `write_file`, `edit` and `todo` over
the fixture tree at `crates/vibe-app-server/tests/tool-execution/tree` and
records, per case, the typed result, the text the agent loop would send to the
model, and whether the call returned or raised.
`crates/vibe-app-server/tests/tool-execution/corpus.json` is that capture,
committed on stricter terms than the others: a captured string survives verbatim
only when it is a value the case supplied, a normalized path or an
identifier-shaped token, and everything else, including every error message, is
committed as a SHA-256 digest. `tool_execution_parity_tests` replays it
unconditionally against this build and reports **35 of 41 cases matching**. The
6 others carry a divergence in exactly one field each, and the ledger names what
keeps it open rather than the story that closes it: `NOTICE`. Three `read_file`
cases answer an empty file or an out-of-range offset with a warning sentence and
three `edit` cases report the applied change with one, and reaching those digests
would mean writing the reference's own sentences into this repository. Every
other field of those six cases still compares byte for byte, which is why the
entries are scoped to the field rather than to the tool. A divergence outside the
ledger fails the suite, and so does a ledger entry whose divergence has been
fixed. Reproduce with `cargo test -p vibe-app-server --all-features
tool_execution_parity_tests -- --nocapture`.

The capture reads the pinned commit with `git archive` instead of requiring the
checkout to sit on it, so it never moves HEAD, creates a branch or adds a
worktree. A workstation whose checkout has moved on can still capture.

Extend the harness before writing each phase, not after.

One blind spot remains, and it is a licensing one rather than an engineering one:
descriptions are compared for presence only, so the 13.8 KB of upstream directive
text has no measured counterpart, and error message text is compared by presence
for the same reason. It does not cap the score above, which weighs names and
schemas and never reads a description; whether it caps a third-party score whose
method is unpublished cannot be determined from here. Both halves are settled
under accepted divergences.

## Related

- `tasks/prd-tool-surface-parity.md` (DONE) delivered ranks 1, 4, 5, 6, 7 and 12.
- `tasks/prd-config-parity.md` delivered rank 2 and the configuration corpus this
  document's configuration score is measured from.
- `tasks/prd-app-server-parity.md` delivered ranks 3 and 8, the `projectLinks/*`
  part of rank 14 and the `telemetry/record` part of rank 15, plus the
  app-server surface corpus this document's app-server score is measured from.
- `tasks/prd-chat-input-observable-parity.md` and
  `tasks/prd-tui-runtime-observable-parity.md` established the harness this
  document relies on.
