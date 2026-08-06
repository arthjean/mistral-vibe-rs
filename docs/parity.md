# Parity Scorecard

Structural audit of Mistral Vibe RS against the upstream Python implementation,
with the execution order derived from it.

| Field | Value |
|---|---|
| First audit | 2026-08-04, Rust `5617d0c` |
| Last remeasure | 2026-08-06, Rust `d4c6dcb` plus the EP-029 changes. Only the four parts EP-029 touched were remeasured, so the weighted total below predates them |
| Python reference | `68ff32e`, package version 2.23.3 |
| Weighted score | 76/100 (was 74 at the last remeasure, 65 at first audit) |

## Method

Scores measure **reproduced surface**, not behavioral conformance. They come from
diffing inventories, not from running both implementations side by side:

- app-server JSON-RPC method names (89 upstream `SERVER_METHODS` plus 7 `clientTool/*`, counted from the pinned checkout by `scripts/parity/app_server_surface.py`);
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
| Built-in tools | 95 | **Measured by differential oracle**: 12/12 names and 12/12 schemas on the base surface, 16/16 under the managed shell rollout, 10/10 on the Windows families, 38/38 against the committed digest. Residual: 2 argument fixtures of 92 rejected here and accepted upstream (`edit/replace_all`, `grep/use_default_ignore` boolean coercion). Descriptions are compared for presence only, never text, as `NOTICE` requires |
| Tool infrastructure (registry, schemas, filtering) | 95 | `object_schema` removed, `apply_defaults` applies schema defaults, `validate_arguments` understands `$ref`, `anyOf`, `items` and array-form `type`, proven by 92 replayed fixtures. `matching.rs` matches globs, `re:` prefixes and is case-insensitive |
| Worktree (`--worktree`) | 90 | `startup/worktree.rs`, full create/reuse/cleanup/branch lifecycle |
| Managed shell and terminals | 90 | `TerminalManager` plus a rich shell policy analyzer, and the `bash_*`, `git_bash` and `powershell` families now publish conformant schemas. Execution equivalence is not yet proven by an oracle |
| CLI surface (flags, modes) | 90 | Every upstream flag present, and tool filtering now matches globs and `re:` prefixes. Missing the `vibe mcp ...` subcommand |
| ACP (`vibe-acp`) | 85 | Agent, sessions, updates, teleport, mcp, proxy. Comparable volume |
| Rewind | 85 | `session/rewind`, `session/rewind/read`, TUI mode |
| LLM backends | 85 | 6 styles (mistral, openai, reasoning, openai-responses, anthropic, vertex-anthropic), SSE streaming, retry. Image, cache and tool-id adaptation details unverified |
| MCP | 85 | stdio, streamable-http, OAuth, registry, toggle. Tools now published as `{alias}_{tool}` matching upstream. Missing `mcp/authUrl` and the `vibe mcp` CLI |
| Trusted folders and permissions | 85 | `policy.rs`: modes, rules, leases, trust roots, approvals |
| Programmatic mode (`-p`) | 85 | text / json / streaming all implemented |
| TUI (composer, transcript, pickers) | 80 | Broad coverage, backed by a dedicated observable-parity harness (JSON traces plus Python oracles). Missing vim navigation, word selection, `load_more`, braille rendering |
| Review and turn diff | 80 | `review/{state,baseline,hunks,approve,revert,turnDiff}` all present |
| Sessions, resume, fork, history | 80 | `storage.rs`: metadata, pagination, migration, file locks, handoff journal |
| app-server protocol | 95 | **Measured by differential oracle**: 89 of 89 reference methods declared and routed with 0 invented names inside the inventory, 15 of 15 notifications emitted with 0 invented, 7 of 7 `clientTool/*` issued, 12 of 12 error codes spoken, 18 of 20 enum vocabularies compared and 364 models in the census. The 20 methods a bare probe session can reach all validate against the census with 0 missing required and 0 surplus aliases. Reproduce with `cargo test -p vibe-app-server --all-features app_server_surface_parity_tests -- --nocapture`. The nine `projectLinks/*` answers are validated against the same census from a repository fixture, since a bare probe reaches only their ineligible form. Residual: `TerminalEmulator` is unmodeled, and 63 of the 89 responses need a backend or a written session neither harness stands up, so they are declared and routed but not census-validated |
| Hooks | 75 | 1:1 on event types (PreTool, PostTool, PostAgent) with matcher, timeout, retries, strict |
| System prompt and project context | 75 | `AGENTS.md` walk-up, prompt resolution, skill and subagent summaries. Missing `include_*`, `system_prompt_id`, `project_context` |
| Agents, subagents, delegation | 75 | `AgentProfile`, `AgentRegistry`, `SubagentManager`, `agents/{list,install,uninstall}`, and `task` now published conformantly |
| Connectors | 70 | Registry, auth, refresh, toggle. Catalog scope unverified |
| Teleport and Vibe Code Web | 85 | `vibeCode/teleport/*`, `vibeCode/projects/*` and the session-less `projectLinks/*` all present, the last two sharing one saved-link store as upstream does. Teleport workflow states and the push flow are unmeasured against the reference |
| Autocompletion | 70 | Slash, path and fuzzy completion. Missing the file indexer with watcher (`file_watcher_for_autocomplete`) |
| Voice (STT, TTS, narrator) | 65 | `voice/{realtime,recorder,session,state}` plus `narrator.rs`, cpal wired. Missing transcribe and TTS provider/model configuration |
| Vibe Code Project | 85 | Workflow, picker and the `projectLinks` layer present, with the reference candidate ranking and the four root reject reasons. The project API client is measured by fixture rather than against the live service |
| Compaction | 55 | `Compactor` trait plus manual compaction. **Automatic compaction absent** (`auto_compact_threshold`, `context_warnings`, `compaction_model`, `compaction_prompt_id`, `raise_on_compaction_failure`) |
| Telemetry and observability | 60 | `telemetry.rs` with an intentionally divergent envelope. `telemetry/record` accepts the reference parameters and honors `enable_telemetry`, keeping the event locally rather than shipping it, which is a recorded divergence below. **OTel absent** (`enable_otel`, `otel_endpoint`, `otel_redaction`), no log reader |
| Skills | 55 | `SKILL.md` discovery, injection, `skills/list`, and the `skill` tool now published conformantly. Missing the remote registry (install, manifest, store), the builtin skills, and `enabled/disabled_skills`, `skill_paths` |
| Checkpoints | 50 | Baseline, hunks and revert exist on the review side. No dedicated file checkpointer (store, recorder, history) |
| Configuration | 95 | **Measured by differential oracle**: 64/64 reference fields declared, published and merged by the strategy the reference declares, 30/30 merge scenarios, 8/8 model-validation scenarios and 22/22 MCP entry scenarios replayed from the committed corpus, plus 8/8 reference `config/*` methods dispatched. Seven layers compose (Defaults, Discovered, SelectedToml, Experiments, Environment, Runtime, Agent). Residual: the GrowthBook layer and the per-layer async state machine are recorded divergences below, the fingerprint token has its own format, and 5 keys this port declares have no upstream counterpart. Declaring a key is not implementing its feature: each arrives with the feature that reads it |
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
| 2 | Configuration mechanism | DONE | `config/patch`, `config/fields/read`, the registry-generated schema and the discovered layer all shipped through `tasks/prd-config-parity.md`, backed by a committed corpus. The 52 keys with no consumer are declared, defaulted, published and merged; each feature still arrives on its own |
| 3 | Missing protocol notifications | DONE | 15 of 15 reference notifications are emitted and the four invented names are retired, replayed by `cargo test -p vibe-app-server --all-features app_server_surface_parity_tests -- --nocapture`. Accepting a plan with clearing now rotates the session inside the turn, which is what raises `session/contextCleared` |
| 4 | `write_file`, `grep`, the `bash` surface, `todo` | DONE | Shipped with rank 1 |
| 5 | `task`, `skill` | DONE | Shipped with rank 1 |
| 6 | `web_search`, `web_fetch` | DONE | Shipped with rank 1 |
| 7 | Tool name matching (globs, `re:`, case-insensitive) | DONE | `crates/vibe-core/src/matching.rs` |
| 8 | `clientTool/*` | DONE | All 7 server-to-client methods are issued, gated on the capabilities the client declared at `initialize`, replayed by the app-server surface corpus |
| 9 | Checkpoints | TODO | Depends on `write_file` and `edit` to capture mutations, now available |
| 10 | Automatic compaction | TODO | Depends on the configuration mechanism and engine token accounting |
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
| Telemetry | The envelope already diverges intentionally from the upstream open-properties format | `CHANGELOG.md`, telemetry entry |
| `telemetry/record` keeps the event locally | The reference hands a client-authored name and free-form properties to the agent loop's telemetry client, which ships them under the open-properties envelope this port does not publish. The method accepts and validates exactly the reference parameters and honors `enable_telemetry`; the event is kept on `diagnostics/logs/read` instead of being shipped, and shipping it needs the envelope divergence above resolved first | `AppServer::telemetry_record` in `crates/vibe-app-server/src/server.rs`, asserted by `a_recorded_client_event_is_kept_only_while_telemetry_is_enabled` |
| `identity/read` and `workspace/worktrees/list` are absent | Neither name exists anywhere in the reference tree at the pinned commit `68ff32e`; both were added upstream afterwards. Routing them would mean inventing a contract or re-pinning, and a re-pin regenerates all three corpora as its own change | `crates/vibe-app-server/tests/app-server-surface/corpus.json`, whose 89 methods are the whole pinned inventory |
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

Extend the harness before writing each phase, not after.

The next ceiling is the harness itself, not a rank in the list. It compares
contracts, not behavior: it proves `bash` publishes the right schema, not that
`bash` produces the same output. Two blind spots follow from that. Tool bodies
are unproven, and descriptions are compared for presence only, so the 13.8 KB of
upstream directive text has no measured counterpart. Closing those needs an
output oracle, which is a different instrument from the surface diff.

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
