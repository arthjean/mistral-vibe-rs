# Parity Scorecard

Structural audit of Mistral Vibe RS against the upstream Python implementation,
with the execution order derived from it.

| Field | Value |
|---|---|
| Audit date | 2026-08-04 |
| Rust reference | `5617d0c`, workspace version 2.23.1 |
| Python reference | `68ff32e`, package version 2.23.3 |

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

Tool-surface numbers come from `tasks/prd-tool-surface-parity.md`, which
introspects `BaseTool.get_name()` over the reference package rather than
counting source files. That measurement supersedes any file-count estimate.

### Measured volumes

| Tree | Lines |
|---|---|
| `vibe/` (Python, tests live in `tests/`) | 89 279 |
| `crates/` total | 107 358 |
| `crates/` excluding test code | ~70 900 |

## Parity by part

| Part | Score | State and gaps |
|---|---|---|
| Distribution, updates, installers | 95 | `install.sh`/`install.ps1`, archives, checksums, rollback, shell completions, `action.yml`. Exceeds upstream PyInstaller packaging |
| Slash commands | 95 | 26 of 27 aliases identical. Missing `/retry` (and its `turn/retrying` notification) |
| Worktree (`--worktree`) | 90 | `startup/worktree.rs`, full create/reuse/cleanup/branch lifecycle |
| ACP (`vibe-acp`) | 85 | Agent, sessions, updates, teleport, mcp, proxy. Comparable volume (4 769 against ~4 200) |
| Rewind | 85 | `session/rewind`, `session/rewind/read`, TUI mode |
| LLM backends | 85 | 6 styles (mistral, openai, reasoning, openai-responses, anthropic, vertex-anthropic), SSE streaming, retry. Image, cache and tool-id adaptation details unverified |
| Trusted folders and permissions | 85 | `policy.rs`: modes, rules, leases, trust roots, approvals |
| Programmatic mode (`-p`) | 85 | text / json / streaming all implemented |
| CLI surface (flags, modes) | 85 | Every upstream flag present. Missing the `vibe mcp ...` subcommand; `--enabled-tools`/`--disabled-tools` match names by exact set membership where upstream matches globs, `re:` prefixes and case-insensitively |
| TUI (composer, transcript, pickers) | 80 | Broad coverage, backed by a dedicated observable-parity harness (JSON traces plus Python oracles). Missing vim navigation, word selection, `load_more`, braille rendering |
| Review and turn diff | 80 | `review/{state,baseline,hunks,approve,revert,turnDiff}` all present |
| Sessions, resume, fork, history | 80 | `storage.rs`: metadata, pagination, migration, file locks, handoff journal |
| MCP | 75 | stdio, streamable-http, OAuth, registry, toggle. Tools published as `mcp_{alias}_{tool}` where upstream publishes `{alias}_{tool}`. Missing `mcp/authUrl` and the `vibe mcp` CLI |
| app-server protocol | 75 | 86 of 113 methods. Absent: `clientTool/*` (7), `projectLinks/*` (9), `config/patch`, `config/fields/read`, `session/{updated,snapshot,statsUpdated,contextCleared}`, `runtime/updated`, `telemetry/record` |
| Hooks | 75 | 1:1 on event types (PreTool, PostTool, PostAgent) with matcher, timeout, retries, strict |
| System prompt and project context | 75 | `AGENTS.md` walk-up, prompt resolution, skill and subagent summaries. Missing `include_*`, `system_prompt_id`, `project_context` |
| Managed shell and terminals | 70 | `TerminalManager` plus a rich shell policy analyzer (flavors, indirection, path operands). Missing dedicated `git_bash`, `windows_shell`, `experimental_bash` surfaces |
| Agents, subagents, delegation | 70 | `AgentProfile`, `AgentRegistry`, `SubagentManager`, `agents/{list,install,uninstall}` |
| Connectors | 70 | Registry, auth, refresh, toggle. Catalog scope unverified |
| Teleport and Vibe Code Web | 70 | `vibeCode/teleport/*` and `vibeCode/projects/*` present. The whole session-less `projectLinks/*` API is absent |
| Autocompletion | 70 | Slash, path and fuzzy completion. Missing the file indexer with watcher (`file_watcher_for_autocomplete`) |
| Voice (STT, TTS, narrator) | 65 | `voice/{realtime,recorder,session,state}` plus `narrator.rs`, cpal wired. Missing transcribe and TTS provider/model configuration |
| Vibe Code Project | 65 | Workflow and picker present, the `projectLinks` layer is not |
| Compaction | 55 | `Compactor` trait plus manual compaction. **Automatic compaction absent** (`auto_compact_threshold`, `context_warnings`, `compaction_model`, `compaction_prompt_id`, `raise_on_compaction_failure`) |
| Telemetry and observability | 55 | `telemetry.rs` with an intentionally divergent envelope. **OTel absent** (`enable_otel`, `otel_endpoint`, `otel_redaction`), no `telemetry/record`, no log reader |
| Tool infrastructure (registry, schemas, filtering) | 55 | Registry, policy-guarded tools and bounded streaming are solid, but `object_schema` unconditionally injects `additionalProperties: false` and an always-present `required`, and `validate_value` understands neither `$ref`, `anyOf`, `items`, array-form `type`, nor defaults |
| Checkpoints | 50 | Baseline, hunks and revert exist on the review side. No dedicated file checkpointer (store, recorder, history) |
| Skills | 45 | `SKILL.md` discovery, injection, `skills/list`. Missing the remote registry (install, manifest, store), the builtin skills, and `enabled/disabled_skills`, `skill_paths` |
| Configuration | 40 | Layers present (Defaults, SelectedToml, Experiments, Environment, Runtime, Agent) but the published schema exposes 13 keys plus `mcp_servers` and `connectors` against ~65 upstream. Missing `config/patch`, `config/fields/read`, the discovered and growthbook layers |
| Setup, onboarding, authentication | 35 | Linear 6-step flow plus keyring. **No multi-screen TUI onboarding, no browser sign-in** |
| Experiments and rollouts | 25 | `ConfigLayerKind::Experiments` and an experiments table only. No GrowthBook client, no rollout or experiment-session handling |
| Built-in tools | 15 | 5 tool names published against 26 upstream, none schema-conformant. Present: `read`, `search`, `edit`, `ask_user_question`, `exit_plan_mode`. `read` and `search` are locally invented names shadowing `read_file` and `grep`. Absent: the entire `bash`/`git_bash`/`powershell` surface, `write_file`, `todo`, `task`, `skill`, `web_fetch`, `web_search` |
| VS Code extension promo | 10 | Not ported |

**Weighted score, using upstream line counts as functional weight: ~65/100.**

## Ordering principle

Sorting by ascending score is the wrong order. The primary criterion is **cost of
deferral**: a part goes first when postponing it forces migrating code already
written, data already persisted, or parity traces already recorded. Then comes
the number of downstream consumers, then user value, then cost.

Under that criterion the entry point is not the tool bodies despite their score
of 15. It is the contracts those bodies will be written against.

## Execution order

| Rank | Part | Why here |
|---|---|---|
| 1 | Tool names and schemas | `read` → `read_file`, `search` → `grep`, snake_case argument keys, remove the unconditional `additionalProperties`/`required` injection, teach `validate_value` about `$ref`/`anyOf`/`items`/defaults. These names are already written into persisted sessions (`tools_available`), hook matchers and parity traces. Every tool added before this rename is a tool to migrate after |
| 2 | Configuration mechanism | `config/patch`, `config/fields/read`, extensible schema, discovered layer. The 52 missing keys do not land here: each arrives with the feature that consumes it. Keep this phase short, the trap is turning it into a rewrite |
| 3 | Missing protocol notifications | `session/updated`, `session/snapshot`, `session/statsUpdated`, `session/contextCleared`, `runtime/updated`, `turn/retrying`. Everything written afterwards emits or consumes these. `/retry` falls out of `turn/retrying` for free |
| 4 | `write_file`, `grep`, the `bash` surface, `todo` | The floor below which the agent is not substitutable for upstream |
| 5 | `task`, `skill` | Wire up `SubagentManager` and `SkillInjector`, both already written |
| 6 | `web_search`, `web_fetch` | Independent, high value, low cost |
| 7 | Tool name matching (globs, `re:`, case-insensitive) | Makes `--enabled-tools 'bash*'` behave, only meaningful once the surface exists |
| 8 | `clientTool/*` | Routes file and terminal tools to the client, unblocks IDE and ACP embedding |
| 9 | Checkpoints | Depends on `write_file` and `edit` to capture mutations |
| 10 | Automatic compaction | Depends on the configuration mechanism and engine token accounting |
| 11 | Skills, complete (remote registry, builtins) | Depends on the `skill` tool and on `skill_paths` keys |
| 12 | Specialized shells (`git_bash`, `windows_shell`, `experimental_bash`) | Large upstream volume, low value per line |
| 13 | Browser sign-in and onboarding | Blocks adoption, blocks nothing technically downstream |
| 14 | `projectLinks/*`, autocompletion indexer, voice configuration | Independent periphery, parallelizable |
| 15 | Telemetry and OTel | Depends on configuration, has no downstream consumer |
| 16 | Experiments and GrowthBook, VS Code promo | See accepted divergences |

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

These scores are declarative until an oracle backs them. The differential harness
(`scripts/parity/oracle.py`, the pinned reference checkout, and
`crates/vibe-cli/tests/runtime-parity/`) is the asset that turns them into
measurements.

Extend the harness before writing each phase, not after. On tools especially, an
oracle capturing every upstream tool's inputs and outputs is worth more than the
lines being reimplemented: it turns "`bash` is ported" into "`bash` is
byte-identical across N recorded scenarios".

## Related

- `tasks/prd-tool-surface-parity.md` covers ranks 1 and 4 through 7.
- `tasks/prd-chat-input-observable-parity.md` and
  `tasks/prd-tui-runtime-observable-parity.md` established the harness this
  document relies on.
