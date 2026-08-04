# Parity Scorecard

Structural audit of Mistral Vibe RS against the upstream Python implementation,
with the execution order derived from it.

| Field | Value |
|---|---|
| First audit | 2026-08-04, Rust `5617d0c` |
| Last remeasure | 2026-08-04, Rust `14bb137` plus uncommitted worktree changes |
| Python reference | `68ff32e`, package version 2.23.3 |
| Weighted score | 74/100 (was 65 at first audit) |

## Method

Scores measure **reproduced surface**, not behavioral conformance. They come from
diffing inventories, not from running both implementations side by side:

- app-server JSON-RPC method names (113 upstream methods);
- user-facing configuration keys (`vibe/core/config/vibe_schema.py` against
  `LayeredConfig::schema()`);
- published agent tool names and schemas;
- slash commands and CLI flags;
- module presence and depth, measured in lines of production code.

A part scores 100 when its externally observable contract is fully reproduced.
Anything measured only by module presence carries more uncertainty than anything
measured by name-level diff, and the table notes which is which.

Where a differential oracle exists, the score comes from running it rather than
from reading source. The tool surface is the first part in that category.

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
| app-server protocol | 75 | 86 of 113 methods. Absent: `clientTool/*` (7), `projectLinks/*` (9), `config/patch`, `config/fields/read`, `session/{updated,snapshot,statsUpdated,contextCleared}`, `runtime/updated`, `telemetry/record` |
| Hooks | 75 | 1:1 on event types (PreTool, PostTool, PostAgent) with matcher, timeout, retries, strict |
| System prompt and project context | 75 | `AGENTS.md` walk-up, prompt resolution, skill and subagent summaries. Missing `include_*`, `system_prompt_id`, `project_context` |
| Agents, subagents, delegation | 75 | `AgentProfile`, `AgentRegistry`, `SubagentManager`, `agents/{list,install,uninstall}`, and `task` now published conformantly |
| Connectors | 70 | Registry, auth, refresh, toggle. Catalog scope unverified |
| Teleport and Vibe Code Web | 70 | `vibeCode/teleport/*` and `vibeCode/projects/*` present. The whole session-less `projectLinks/*` API is absent |
| Autocompletion | 70 | Slash, path and fuzzy completion. Missing the file indexer with watcher (`file_watcher_for_autocomplete`) |
| Voice (STT, TTS, narrator) | 65 | `voice/{realtime,recorder,session,state}` plus `narrator.rs`, cpal wired. Missing transcribe and TTS provider/model configuration |
| Vibe Code Project | 65 | Workflow and picker present, the `projectLinks` layer is not |
| Compaction | 55 | `Compactor` trait plus manual compaction. **Automatic compaction absent** (`auto_compact_threshold`, `context_warnings`, `compaction_model`, `compaction_prompt_id`, `raise_on_compaction_failure`) |
| Telemetry and observability | 55 | `telemetry.rs` with an intentionally divergent envelope. **OTel absent** (`enable_otel`, `otel_endpoint`, `otel_redaction`), no `telemetry/record`, no log reader |
| Skills | 55 | `SKILL.md` discovery, injection, `skills/list`, and the `skill` tool now published conformantly. Missing the remote registry (install, manifest, store), the builtin skills, and `enabled/disabled_skills`, `skill_paths` |
| Checkpoints | 50 | Baseline, hunks and revert exist on the review side. No dedicated file checkpointer (store, recorder, history) |
| Configuration | 40 | Layers present (Defaults, SelectedToml, Experiments, Environment, Runtime, Agent) but the published schema exposes 13 keys plus `mcp_servers` and `connectors` against ~65 upstream. Missing `config/patch`, `config/fields/read`, the discovered and growthbook layers |
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
| 2 | Configuration mechanism | NEXT | `config/patch`, `config/fields/read`, extensible schema, discovered layer. The 52 missing keys do not land here: each arrives with the feature that consumes it. Keep this phase short, the trap is turning it into a rewrite |
| 3 | Missing protocol notifications | TODO | `session/updated`, `session/snapshot`, `session/statsUpdated`, `session/contextCleared`, `runtime/updated`, `turn/retrying`. Everything written afterwards emits or consumes these. `/retry` falls out of `turn/retrying` for free |
| 4 | `write_file`, `grep`, the `bash` surface, `todo` | DONE | Shipped with rank 1 |
| 5 | `task`, `skill` | DONE | Shipped with rank 1 |
| 6 | `web_search`, `web_fetch` | DONE | Shipped with rank 1 |
| 7 | Tool name matching (globs, `re:`, case-insensitive) | DONE | `crates/vibe-core/src/matching.rs` |
| 8 | `clientTool/*` | TODO | Routes file and terminal tools to the client, unblocks IDE and ACP embedding |
| 9 | Checkpoints | TODO | Depends on `write_file` and `edit` to capture mutations, now available |
| 10 | Automatic compaction | TODO | Depends on the configuration mechanism and engine token accounting |
| 11 | Skills, complete (remote registry, builtins) | TODO | Depends on the `skill` tool, now available, and on `skill_paths` keys |
| 12 | Specialized shells (`git_bash`, `powershell`) | DONE | Shipped with rank 1 |
| 13 | Browser sign-in and onboarding | TODO | Blocks adoption, blocks nothing technically downstream |
| 14 | `projectLinks/*`, autocompletion indexer, voice configuration | TODO | Independent periphery, parallelizable |
| 15 | Telemetry and OTel | TODO | Depends on configuration, has no downstream consumer |
| 16 | Experiments and GrowthBook, VS Code promo | TODO | See accepted divergences |

## Accepted divergences

Three parts cannot reach 100 through code alone. For these, 100 means a decided
and documented divergence, not a port. Recording the decision early keeps it out
of every subsequent parity review.

| Part | Reason |
|---|---|
| Telemetry | The envelope already diverges intentionally from the upstream open-properties format (see `CHANGELOG.md`) |
| Experiments and GrowthBook | Requires access to a third-party Mistral service with credentials this repository does not hold |
| VS Code extension promo | Advertises an extension that does not target this binary |

## Verification

Scores are declarative until an oracle backs them. The tool surface is the first
part where that is no longer true: `crates/vibe-app-server/src/tool_surface_parity_tests.rs`
captures the reference surface from the pinned checkout, diffs it against what a
real session registers, and reports missing names, invented names and per-name
schema divergence as JSON pointers. `crates/vibe-app-server/tests/tool-surface/baseline.json`
holds the conformance target and currently records zero divergence on Linux.

Extend the harness before writing each phase, not after.

The next ceiling is the harness itself, not a rank in the list. It compares
contracts, not behavior: it proves `bash` publishes the right schema, not that
`bash` produces the same output. Two blind spots follow from that. Tool bodies
are unproven, and descriptions are compared for presence only, so the 13.8 KB of
upstream directive text has no measured counterpart. Closing those needs an
output oracle, which is a different instrument from the surface diff.

## Related

- `tasks/prd-tool-surface-parity.md` (DONE) delivered ranks 1, 4, 5, 6, 7 and 12.
- `tasks/prd-chat-input-observable-parity.md` and
  `tasks/prd-tui-runtime-observable-parity.md` established the harness this
  document relies on.
