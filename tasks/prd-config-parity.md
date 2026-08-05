[PRD]
# PRD: Configuration Parity

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-04 | Arthur Jean | Initial PRD from the measured configuration audit against the Python reference: 64 reference schema fields, 10 reproduced, 4 merge strategies collapsed into 1, 8 protocol methods with 2 absent and 1 invented |

## Problem Statement

1. The Rust port publishes 15 configuration keys where the Python reference declares 64. Measured by walking the `VibeConfigSchema` class body at [vibe/core/config/vibe_schema.py:219](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py) against `LayeredConfig::schema()` (`crates/vibe-core/src/config.rs:786`), only 10 of the 15 have an upstream counterpart. 54 upstream keys are absent, including every model, provider, transcribe and TTS entry, `tools`, all six `*_paths` and `enabled/disabled_*` list fields, `default_agent`, `auto_compact_threshold`, `system_prompt_id` and the whole OTel triple. The remaining 5 (`thinking`, `notifications`, `proxy`, `tls_ca_path`, `dotenv_path`) exist only in this port and match no reference field.
2. The defaults layer is empty in production. `Release3Service::default()` and `Release3Service::build` receive `Table::new()` as the defaults document (`crates/vibe-app-server/src/release3.rs:120`, `:134`), so `ConfigLayerKind::Defaults` composes nothing. `DEFAULT_PROVIDERS`, `DEFAULT_MODELS`, `DEFAULT_TRANSCRIBE_PROVIDERS`, `DEFAULT_TRANSCRIBE_MODELS`, `DEFAULT_TTS_PROVIDERS`, `DEFAULT_TTS_MODELS` and the 9 scalar constants of [vibe/core/config/_defaults.py](/home/arthur/dev/mistral-vibe/vibe/core/config/_defaults.py) have no Rust counterpart. Every behavior that upstream derives from a default currently diverges by omission.
3. Four merge strategies collapse into one. The reference annotates each field with a strategy consumed by `ConfigBuilder._merge_fields` ([vibe/core/config/builder.py:84](/home/arthur/dev/mistral-vibe/vibe/core/config/builder.py)); the field census yields 45 `REPLACE`, 10 `CONCAT`, 7 `UNION`, 2 `DEEP_MERGE`. `merge_tables` (`crates/vibe-core/src/config/integrations.rs:254`) deep-merges tables, replaces arrays, and special-cases `mcp_servers` and `connectors` into a union by name. The 10 `CONCAT` fields are therefore wrong in a way that loses data: a higher layer's `disabled_tools` overwrites a lower layer's denylist instead of extending it, and the same applies to `tool_paths`, `agent_paths`, `skill_paths`, `enabled_agents`, `disabled_agents`, `installed_agents`, `enabled_skills`, `disabled_skills` and `applied_migrations`. The 5 remaining `UNION` fields (`providers`, `transcribe_providers`, `transcribe_models`, `tts_providers`, `tts_models`) are replaced wholesale, and `models` and `tools` never deep-merge because they are not tables in the persisted form.
4. Two protocol methods are missing and one is invented. Upstream routes 8 config methods ([vibe/app_server/protocol.py:86](/home/arthur/dev/mistral-vibe/vibe/app_server/protocol.py)); the Rust dispatcher (`crates/vibe-app-server/src/release3.rs:402`) implements 6 of them, omits `config/patch` and `config/fields/read`, and adds `config/batchWrite`, which no reference client calls. A conforming app-server client therefore cannot write configuration at all, and the settings screen has no source for field kinds, descriptions, per-layer values or the 13-entry `POPULAR_SETTINGS` set ([vibe/app_server/_config_introspect.py:23](/home/arthur/dev/mistral-vibe/vibe/app_server/_config_introspect.py)).
5. Project configuration discovery stops at the working directory. `ConfigPaths::project_config()` (`crates/vibe-core/src/config.rs:98`) resolves exactly `{cwd}/.vibe/config.toml`, where `_discover_config_file` ([vibe/core/config/layers/project.py:105](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/project.py)) walks every parent up to the directory above `VIBE_HOME`. Opening any subdirectory of a repository silently loses its project configuration.
6. No configuration migration runs. The 4 migrations of [vibe/core/config/_migration.py](/home/arthur/dev/mistral-vibe/vibe/core/config/_migration.py) (bash allowlist plus `find` plus trailing-wildcard strip, the one-shot read-only command sync, the `devstral-2` to `mistral-medium-3.5` rename, and the `read` to `read_file` / `search_replace` to `edit` tool rename with option transfer) have no Rust counterpart, and `applied_migrations` is never written. A configuration file written by the Python client and read by this binary keeps tool names this port no longer publishes.
7. `~/.vibe/.env` is never loaded. `load_dotenv_values` ([vibe/core/config/vibe_schema.py:75](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py)) reads `GLOBAL_ENV_FILE` at startup with process values winning and FIFO paths supported. In Rust the file is written by `ProxyEnvironmentStore` (`crates/vibe-core/src/config/proxy.rs:10`) and read by nothing, while `dotenv_path` is advertised in the published schema and consumed nowhere.
8. No test in the repository asserts what the effective configuration should be for a given layer stack. `cargo test --workspace --all-features` passes at full green with an empty defaults layer, four wrong merge strategies, and 54 absent keys.

**Why now:** `docs/parity.md` ranks the configuration mechanism second in execution order, ahead of missing protocol notifications, precisely because every later part reads through it. Automatic compaction needs `auto_compact_threshold`, `compaction_model`, `compaction_prompt_id`, `context_warnings` and `raise_on_compaction_failure`. Skills need `skill_paths` and `enabled/disabled_skills`. Telemetry needs the OTel triple. Voice needs the transcribe and TTS provider and model entries. Each of those parts will otherwise invent its own key handling, and the `CONCAT` defect will be replicated into each new list field. The differential-oracle infrastructure that makes the fix verifiable already exists (`scripts/parity/oracle.py`, `scripts/parity/tool_surface.py`, the pinned checkout, the conditional live probe) and is directly reusable.

## Overview

This initiative makes the Rust configuration module contract-equivalent to the Python reference. Equivalence is defined narrowly and mechanically: for the same set of layer documents, the effective configuration document produced by `LayeredConfig::load()` is semantically identical to the one `ConfigBuilder.build()` produces from the same inputs, and the app-server publishes the same config method set with the same wire shapes. Field descriptions cover the same directives in original prose, for the licensing reason stated below.

The work is sequenced so that correctness becomes structural rather than per-key. The first epic replaces the ad hoc merge with a declarative field registry: one static table carrying, for each field, its name, editor kind, default, description, merge strategy and popular flag. That table becomes the single source for four consumers that currently drift independently, the Defaults layer, `config/schema`, `config/fields/read` and environment coercion, and it is stood up together with a differential oracle that replays layer stacks against the reference builder. Every later story is verified by that oracle instead of by hand-written assertions.

The second epic fills the table with the 64 reference fields and their defaults, and restores the model-map semantics that the reference relies on: the alias-keyed internal map, list and map input forms, sparse default completion, the global `auto_compact_threshold` propagation and the unknown `active_model` fallback. The third adds the patch surface: a JSON Pointer core supporting the two wire ops, the `config/patch` method with its merged-config preflight and per-layer routing, `config/fields/read`, and the change event bus that lets the runtime react to a write without a full reload. The fourth restores discovery: parent walk-up, the harness file manager that decides which file is writable, dotenv loading, and the four migrations. The fifth completes the MCP configuration surface. The sixth adds the remaining layer, locks the corpus into CI and re-scores `docs/parity.md`.

The reference is a read-only checkout pinned for this PRD at commit `68ff32e6a92e80a874c8153312f0aa8ae4955477` (v2.23.3, 2026-08-03), which every measurement in this document was taken from. Its location is machine-dependent: `C:\dev\mistral-vibe` on Windows and `/home/arthur/dev/mistral-vibe` on Linux. Every reference link below is written in the Linux form as the canonical spelling and resolves against whichever checkout is local; the parity scripts read `VIBE_REFERENCE` as an override of that default, and `--reference` wins over both. The configuration module is [vibe/core/config](/home/arthur/dev/mistral-vibe/vibe/core/config), 4 346 lines across 28 files, and splits into six parts every story navigates back to: [vibe_schema.py](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py) declares the 64 fields, their defaults, their merge annotations and 6 post-validation rules; [models.py](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py) types every nested value including the MCP transport and auth unions; [schema.py](/home/arthur/dev/mistral-vibe/vibe/core/config/schema.py) and [vibe/core/utils/merge.py](/home/arthur/dev/mistral-vibe/vibe/core/utils/merge.py) define the strategy vocabulary and its semantics; [layer.py](/home/arthur/dev/mistral-vibe/vibe/core/config/layer.py), [builder.py](/home/arthur/dev/mistral-vibe/vibe/core/config/builder.py) and [orchestrator.py](/home/arthur/dev/mistral-vibe/vibe/core/config/orchestrator.py) own the layer lifecycle, the merge and the patch routing; [layers/](/home/arthur/dev/mistral-vibe/vibe/core/config/layers) holds the eight concrete layers; [harness_files/](/home/arthur/dev/mistral-vibe/vibe/core/config/harness_files) resolves which files are readable and writable. Two contracts reach outside the module and are still in scope: [vibe/app_server/protocol.py:398](/home/arthur/dev/mistral-vibe/vibe/app_server/protocol.py) defines every config wire model, and [vibe/app_server/_config_introspect.py](/home/arthur/dev/mistral-vibe/vibe/app_server/_config_introspect.py) turns a validated config into the settings-screen field list.

One constraint shaped the plan. `NOTICE` declares that no upstream implementation source is copied, translated, vendored, linked, or shipped. Field descriptions are reference-authored prose and are therefore written originally in Rust; the committed corpus records description presence and length, never text. Default values, field names, merge strategies, JSON pointers and merged-document shapes are observations rather than authored prose and are committed, exactly as `crates/vibe-app-server/tests/tool-surface/baseline.json` already does for the tool surface.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Reproduce the reference key surface | 64 of 64 fields declared in the registry and published by `config/schema` | 64 of 64 maintained, 0 invented keys without a recorded divergence entry |
| Make merging strategy-correct | 4 of 4 reachable strategies implemented, 0 fields merged by the wrong strategy | 0 maintained, enforced by the corpus gate |
| Ship the defaults layer | Defaults layer non-empty at every construction site, 100% of registry defaults present | 0 construction sites passing an empty defaults document |
| Reproduce the config protocol surface | 8 of 8 reference config methods dispatched, `config/patch` and `config/fields/read` wire-conformant | 8 of 8 maintained, `config/batchWrite` retained only as a documented local alias |
| Make conformance mechanically enforced | Differential oracle replays at least 24 layer-stack scenarios and fails on any divergence | Oracle wired into CI, no field added without a corpus entry |
| Preserve user configuration across clients | 4 of 4 migrations applied, `applied_migrations` written | 0 configuration files readable by one client and stale for the other |

## Target Users

### Vibe operator sharing one configuration between clients

- **Role:** Developer running the Python client and the Rust binary on the same machine, against the same `~/.vibe/config.toml` and the same project `.vibe/config.toml`.
- **Behaviors:** Sets an active model, pins a theme, disables tools in the user file and adds more to the denylist in the project file, declares MCP servers, keeps API keys in `~/.vibe/.env`, opens the agent from a subdirectory of a monorepo.
- **Pain points:** The project denylist replaces the user denylist instead of extending it, so tools the user disabled globally come back. Opening a subdirectory drops the project file entirely. Keys in `~/.vibe/.env` are invisible to the Rust binary. A model alias renamed by the Python client's migration is never renamed here.
- **Current workaround:** Maintain one configuration per client and never open the agent below the repository root.
- **Success looks like:** One configuration file, read the same way by both binaries, with the same effective result for the same layer stack.

### Rust port maintainer adding a configuration key

- **Role:** Engineer landing a feature that needs a new key, for example `auto_compact_threshold` for automatic compaction.
- **Behaviors:** Adds the key where the feature reads it, hopes the merge does the right thing, updates the JSON Schema literal by hand.
- **Pain points:** Four places must be edited in lockstep and nothing detects drift between them. The merge strategy is implicit and untestable. No fixture states what the effective document should be.
- **Current workaround:** Read the Python source for every key, which is exactly the work this PRD automates.
- **Success looks like:** One registry entry declares the key everywhere at once, and a failing oracle names the divergent field, its JSON pointer and the expected value.

### App-server client author

- **Role:** Author of an editor integration or of the bundled TUI settings screen, driving the app-server over JSON-RPC.
- **Behaviors:** Calls `config/fields/read` to render a settings list, `config/patch` to write one field, `config/reload` to apply.
- **Pain points:** Neither read nor write method exists here; `config/batchWrite` requires knowledge of the on-disk target and its fingerprint, which the reference protocol never exposes to clients.
- **Current workaround:** Special-case the Rust binary or edit the TOML file directly.
- **Success looks like:** The same three calls work against either implementation.

## Research Findings

Key findings that informed this PRD:

### Reference Contract

- The strategy census over `VibeConfigSchema` returns 64 fields: 45 `WithReplaceMerge`, 10 `WithConcatMerge`, 7 `WithUnionMerge`, 2 `WithDeepMerge`. `WithShallowMerge` and `WithConflictMerge` are declared in [schema.py:152](/home/arthur/dev/mistral-vibe/vibe/core/config/schema.py) and used by no field, so they are unreachable from the current schema and produce no observable behavior.
- `config/patch` is not RFC 6902 on the wire. `ConfigPatchOpWire` ([protocol.py:560](/home/arthur/dev/mistral-vibe/vibe/app_server/protocol.py)) carries `op: "set" | "remove"`, a JSON Pointer `path`, an optional `value` and an optional `target_layer`; `set` is mapped to a JSON Patch `add` inside the server ([_resources.py:515](/home/arthur/dev/mistral-vibe/vibe/app_server/_resources.py)). Only the internal orchestrator sees RFC 6902 verbs.
- `ConfigOrchestrator.apply_patch` is explicitly non-atomic across layers: a merged-config preflight rejects the whole request, then per-layer writes proceed independently and failures are returned rather than raised ([orchestrator.py:163](/home/arthur/dev/mistral-vibe/vibe/core/config/orchestrator.py)). `ConfigPatchResponse` distinguishes `rejected` from `failures` accordingly.
- `ConfigBuilder._merge_fields` populates an `origins` map that is never written to, so `ConfigSchema.origin_of` always returns `None` upstream. The Rust TUI already derives origin from layer values, which is strictly more informative and must not be regressed to match.
- `_merge_fields` skips any key absent from `schema.model_fields`, so unknown keys never reach the merged document even though `RawConfig` preserves them per layer.

### Technical Patterns Applied

- `serde_json` implements RFC 6901 natively (`Value::pointer`, `Value::pointer_mut`), including `~1` then `~0` unescape ordering. Pointer parsing needs no new dependency ([RFC 6901](https://www.rfc-editor.org/rfc/rfc6901), [serde-rs/json#41](https://github.com/serde-rs/json/pull/41/files)).
- The `json-patch` crate implements RFC 6902 and RFC 7396 over `serde_json::Value` ([crates.io](https://crates.io/crates/json-patch), [idubrov/json-patch](https://github.com/idubrov/json-patch)). It is not adopted here: the store document is `toml::Table`, only two operations are on the wire, and the repository budget forbids a dependency without a current requirement.
- The tool-surface effort established the reusable shape for this work: a capture script re-executing itself under the reference interpreter, a committed corpus of normalized observations, unconditional replay in `cargo test`, and a live probe skipped when the checkout is absent or off-pin (`crates/vibe-cli/src/tui/runtime_parity_tests.rs:46`).

*Full research sources available in project documentation.*

## Assumptions & Constraints

### Assumptions (to validate)

- The reference `ConfigBuilder` can be driven headlessly from a capture script with synthetic in-memory layers, without initialising the harness-files singleton or reaching the network. Validated by US-061; if it fails, the oracle falls back to capturing `create_default_config()` plus hand-authored layer-stack fixtures and the scenario count drops.
- No consumer outside this repository depends on `config/batchWrite`. It is called by `crates/vibe-cli` and by tests only, per `grep -rn 'config/batchWrite' crates`.
- Rendering the settings screen from `config/fields/read` requires no runtime state beyond the loaded layers, so the Rust implementation can answer it from `ConfigSnapshot` without an agent loop.
- Reference default values are behavioral observations rather than authored prose and may be committed. Field descriptions are prose and may not.

### Hard Constraints

- `NOTICE`: no upstream source copied, translated, vendored, linked or shipped. Descriptions are original prose; corpora carrying reference-authored text stay gitignored under `.parity/`.
- The reference checkout is read-only and pinned at `68ff32e6a92e80a874c8153312f0aa8ae4955477`. Re-pinning requires regenerating every corpus and updating all four `REFERENCE_COMMIT` constants in the same change.
- Layering declared by `[workspace.metadata.vibe] dependency-layers`: the registry, merge, patch core, discovery, dotenv and migrations belong to `vibe-core`; only method dispatch and wire models belong to `vibe-app-server`.
- No new workspace dependency unless a story states the requirement that forces it.
- A missing reference checkout must never fail `cargo test`; only the live probe may skip.
- `unsafe_code` is forbidden; `panic`, `unimplemented` and `dbg_macro` are denied in non-test code.
- Existing on-disk configuration must keep loading. Any key already persisted by this port stays readable after the change.

## Quality Gates

These commands must pass for every user story, from the workspace root:

- `cargo fmt --all -- --check` - formatting
- `cargo check --workspace --all-targets --all-features` - compilation across every target
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` - lint set with warnings denied
- `cargo test --workspace --all-features` - full suite, including the replayed configuration corpus

`--all-features` is load-bearing: `vibe-app-server`'s `test-fixtures` feature gates fixture binaries that several integration tests drive.

## Epics & User Stories

### EP-018: Declarative Merge and the Field Registry

Replace the implicit merge rule with a strategy declared per field, and stand up the differential oracle that every later story is verified by. This epic changes no key surface; it changes how any key surface is composed and how composition is proven.

Source navigation: [schema.py](/home/arthur/dev/mistral-vibe/vibe/core/config/schema.py) for the strategy vocabulary, [vibe/core/utils/merge.py](/home/arthur/dev/mistral-vibe/vibe/core/utils/merge.py) for each strategy's exact semantics including the empty-mapping-as-absent rule, [builder.py:84](/home/arthur/dev/mistral-vibe/vibe/core/config/builder.py) for how strategies are applied and unknown keys dropped.

**Definition of Done:** `merge_tables` consults a per-field strategy for every top-level key, the four reachable strategies match the reference on every corpus scenario, and a test asserts that no reference field declares a strategy this port does not implement.

#### US-059: Declare every configuration field in one registry
**Description:** As a Rust port maintainer, I want one static table declaring each field's name, editor kind, default, description, merge strategy and popular flag so that the defaults layer, the published schema, the settings introspection and environment coercion stop drifting apart.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None
**Reference:** [vibe_schema.py:219](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py:219) declares the 64 fields with their defaults and merge annotations, [schema.py:92](/home/arthur/dev/mistral-vibe/vibe/core/config/schema.py:92) defines `MergeFieldMetadata` and its concrete markers, [_config_introspect.py:45](/home/arthur/dev/mistral-vibe/vibe/app_server/_config_introspect.py:45) maps an annotation onto an editor kind and enum choices, and [_config_introspect.py:22](/home/arthur/dev/mistral-vibe/vibe/app_server/_config_introspect.py:22) is the 13-entry popular set.

**Acceptance Criteria:**
- [ ] Given the registry, when a field entry is declared, then it carries name, kind (`bool`, `enum`, `int`, `float`, `str`, `list`, `complex`), optional enum choices, default value, original description, merge strategy and popular flag
- [ ] Given the registry, when `LayeredConfig::schema()` is called, then the emitted JSON Schema is generated from the registry rather than from a hand-written literal
- [ ] Given a field absent from the registry, when it appears in a layer document, then it is preserved in the effective document and reported as unregistered by a dedicated accessor, never silently dropped
- [ ] Given two registry entries with the same name, when the registry is built, then a unit test fails naming the duplicate
- [ ] Given the registry, when a `union` strategy entry omits its merge key, then compilation fails or a unit test rejects it, so an unusable declaration cannot ship

#### US-060: Merge each field by its declared strategy
**Description:** As a Vibe operator, I want a project-level denylist to extend the user-level denylist rather than replace it so that tools I disabled globally stay disabled.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-059
**Reference:** [merge.py:31](/home/arthur/dev/mistral-vibe/vibe/core/utils/merge.py:31) defines the strategy vocabulary and [merge.py:39](/home/arthur/dev/mistral-vibe/vibe/core/utils/merge.py:39) its dispatch, with [merge.py:70](/home/arthur/dev/mistral-vibe/vibe/core/utils/merge.py:70) for the empty-mapping-as-absent rule and [merge.py:97](/home/arthur/dev/mistral-vibe/vibe/core/utils/merge.py:97) for the union merge-key error. [builder.py:84](/home/arthur/dev/mistral-vibe/vibe/core/config/builder.py:84) applies one strategy per field and skips keys absent from the schema.

**Acceptance Criteria:**
- [ ] Given two layers both setting a `concat` field, when they merge, then the result is the lower layer's entries followed by the higher layer's, in layer order, with duplicates preserved
- [ ] Given two layers both setting a `union` field, when they merge, then entries are keyed by the declared merge key and the higher layer wins per key, preserving first-seen order
- [ ] Given two layers both setting a `deep_merge` field, when they merge, then nested tables merge recursively and absent keys are preserved
- [ ] Given two layers both setting a `replace` field, when they merge, then the higher layer wins outright
- [ ] Given a layer providing an empty table where a `concat` or `union` field is expected, when it merges, then the empty table is treated as absent instead of failing, matching `_empty_mapping_as_absent`
- [ ] Given a `union` entry missing its merge key, when it merges, then the load fails with an error naming the field and the merge key, and the error text contains no value from the entry
- [ ] Given the reference declares a strategy no registry entry uses, when the corpus test runs, then it asserts the unused strategies are exactly `shallow` and `conflict` and fails if a field ever adopts one

#### US-061: Replay merged configurations against the reference
**Description:** As a Rust port maintainer, I want a capture script and a committed corpus that state the effective document for a set of layer stacks so that any merge divergence fails `cargo test` with the field and pointer named.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-059
**Reference:** [builder.py:60](/home/arthur/dev/mistral-vibe/vibe/core/config/builder.py:60) is the function under replay. [layer.py:263](/home/arthur/dev/mistral-vibe/vibe/core/config/layer.py:263) shows the load and cache path the capture must bypass, and [types.py:40](/home/arthur/dev/mistral-vibe/vibe/core/config/types.py:40) the empty-layer snapshot. Local model to follow: `scripts/parity/tool_surface.py` and `crates/vibe-app-server/src/tool_surface_parity_tests.rs:47`.

**Acceptance Criteria:**
- [ ] Given the pinned checkout, when `scripts/parity/config_surface.py --reference <path>` runs, then it re-executes itself under the reference interpreter and emits, for each scenario, the merged document produced by `ConfigBuilder.build()`
- [ ] Given a scenario set, when the corpus is generated, then it covers at least 24 stacks exercising every strategy, single-layer, two-layer and four-layer stacks, empty layers, and a stack where only the defaults layer is present
- [ ] Given the corpus, when `cargo test --workspace --all-features` runs without a reference checkout, then the replay still executes and only the live probe is skipped
- [ ] Given a divergence, when the replay fails, then the message names the scenario, the JSON pointer and both values, and contains no value from a key the redaction rules treat as sensitive
- [ ] Given the corpus, when it is committed, then it contains field names, pointers, strategies and values but no reference-authored description text
- [ ] Given a `REFERENCE_COMMIT` constant, when this story lands, then it matches the four existing constants, verified by `grep -rn 'REFERENCE_COMMIT: &str' crates`

#### US-062: Coerce environment overrides through the registry
**Description:** As a Vibe operator, I want `VIBE_*` variables to be typed by the field they target so that a value that would be rejected upstream is rejected here instead of silently changing type.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-059
**Reference:** [environment.py:23](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/environment.py:23) builds a settings class from the schema fields, and [environment.py:13](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/environment.py:13) carries the prefix, the `__` nesting delimiter, case-insensitivity and `env_ignore_empty`.

**Acceptance Criteria:**
- [ ] Given `VIBE_ENABLE_TELEMETRY=false`, when the environment layer is built, then the value is a boolean
- [ ] Given `VIBE_THEME=1` on a field the registry types as a string, when the environment layer is built, then the value is the string `1`, not an integer
- [ ] Given `VIBE_API_TIMEOUT=abc` on a field typed as a float, when the environment layer is built, then the load fails with an error naming the variable, and the error text does not include the value
- [ ] Given an empty `VIBE_*` value, when the environment layer is built, then the variable is ignored, matching `env_ignore_empty`
- [ ] Given `VIBE_NESTED__WINNER`, when the environment layer is built, then `__` still maps to nesting and the existing precedence test keeps passing

---

### EP-019: Defaults and the Complete Key Surface

Fill the registry with the 64 reference fields, ship their defaults, and restore the model-map semantics the reference depends on. This is where the published surface goes from 10 reproduced keys to 64.

Source navigation: [vibe_schema.py:219](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py) for the field list, defaults and the 6 model validators, [_defaults.py](/home/arthur/dev/mistral-vibe/vibe/core/config/_defaults.py) for the 9 constants, [models.py:415](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py) for `ModelConfig` and the normalization helpers, [layers/default.py](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/default.py) for how defaults enter the stack.

**Definition of Done:** the defaults layer is non-empty at every construction site, `config/schema` publishes 64 fields, and the corpus proves the default document matches `create_default_config()` field for field.

#### US-063: Ship the default configuration document
**Description:** As a Vibe operator, I want the binary to start with the same defaults as the reference so that behavior I never configured matches between clients.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-059, US-061
**Reference:** [vibe_schema.py:93](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py:93) `DEFAULT_PROVIDERS`, [vibe_schema.py:121](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py:121) `DEFAULT_MODELS`, [vibe_schema.py:140](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py:140) and [vibe_schema.py:148](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py:148) the transcribe pair, [vibe_schema.py:156](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py:156) and [vibe_schema.py:164](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py:164) the TTS pair, [_defaults.py:9](/home/arthur/dev/mistral-vibe/vibe/core/config/_defaults.py:9) the 9 scalar constants. [default.py:10](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/default.py:10) shows how defaults enter the stack, [models.py:64](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py:64) the path expansion rule, and [vibe_schema.py:642](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py:642) `create_default_config` is the oracle target.

**Acceptance Criteria:**
- [ ] Given the defaults function, when it is called, then it returns the two default providers, the three default models, the default transcribe provider and model, the default TTS provider and model, and every scalar default from the registry
- [ ] Given `Release3Service::default()`, when the service is built, then the defaults layer is the shipped document and not an empty table
- [ ] Given every other construction site of `LayeredConfig`, when this story lands, then none passes an empty defaults document except in tests that assert empty-layer behavior explicitly
- [ ] Given the corpus, when the default document is replayed, then it matches `create_default_config()` on every field except `tools`, whose contents come from tool discovery and are compared for shape only
- [ ] Given a user file overriding one default scalar, when the configuration loads, then the override wins and every untouched default remains present
- [ ] Given a default whose value is a path, when the configuration loads, then it is expanded and absolutized as `SessionLoggingConfig.expand_save_dir` does, and a path that cannot be expanded fails the load with the field named

#### US-064: Publish all 64 fields through the config schema
**Description:** As an app-server client author, I want `config/schema` to describe every field so that a settings UI can render the full surface without hard-coding it.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-063
**Reference:** [protocol.py:409](/home/arthur/dev/mistral-vibe/vibe/app_server/protocol.py:409) and [protocol.py:413](/home/arthur/dev/mistral-vibe/vibe/app_server/protocol.py:413) define the request and the response, including the schema version field and the `schema` alias. [vibe_schema.py:171](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py:171) shows how the persisted document is read back.

**Acceptance Criteria:**
- [ ] Given `config/schema`, when it is dispatched, then the response carries both a schema version string and the schema object, matching `ConfigSchemaReadResponse`
- [ ] Given the published schema, when it is compared to the registry, then every registry field appears with its type, default, description and enum choices
- [ ] Given the 5 keys this port publishes with no reference counterpart, when this story lands, then each is either mapped onto its reference equivalent or recorded in a divergence table in the PRD's Non-Goals with a one-line reason
- [ ] Given a client that persisted one of those 5 keys, when the configuration loads after the change, then the value still loads and, where a mapping exists, is read from the mapped field
- [ ] Given the schema, when it is emitted twice in the same process, then the output is byte-identical, so a client can cache it

#### US-065: Normalize the model map and apply the schema-level model rules
**Description:** As a Vibe operator, I want per-model settings to merge instead of replacing the whole model list so that overriding one model's temperature does not erase the others.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-060, US-063
**Reference:** [models.py:415](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py:415) `ModelConfig`, [models.py:431](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py:431) `normalize_model_configs`, [models.py:449](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py:449) the write-back form, [models.py:468](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py:468) sparse default completion. [vibe_schema.py:569](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py:569) propagates the global threshold, [vibe_schema.py:584](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py:584) falls back on an unknown active model, [vibe_schema.py:199](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py:199) rejects an empty model set. [_base.py:93](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/_base.py:93) and [_base.py:84](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/_base.py:84) are the read and write shapes.

**Acceptance Criteria:**
- [ ] Given a `[[models]]` array, when the layer is read, then it is normalized to a map keyed by alias, and an entry with neither alias nor name fails the load with the field named
- [ ] Given an alias-keyed model table, when the layer is read, then it is accepted unchanged, and a key that disagrees with the entry's own alias fails the load naming both
- [ ] Given the internal map, when it is written back to TOML, then it is serialized as a `[[models]]` array with null-valued fields dropped
- [ ] Given a sparse override of a known default model that omits `name` or `provider`, when layers merge, then the missing identity fields are completed from the default entry
- [ ] Given a global `auto_compact_threshold` and a model that does not set its own, when the configuration is validated, then the model inherits the global value, and a model that sets its own keeps it
- [ ] Given an `active_model` naming no configured model, when the configuration is validated, then the first configured model is selected, a warning is recorded in a readable validation-warnings list, and the load succeeds
- [ ] Given zero configured models after merging, when the configuration is validated, then the load fails with a message telling the operator to define at least one model

---

### EP-020: Patch Surface and Settings Introspection

Reproduce the two missing protocol methods and the change notification they depend on, on top of a JSON Pointer patch core in `vibe-core`.

Source navigation: [patch.py](/home/arthur/dev/mistral-vibe/vibe/core/config/patch.py) for the operations, upsert resolution and parent auto-vivification, [orchestrator.py:163](/home/arthur/dev/mistral-vibe/vibe/core/config/orchestrator.py) for preflight, routing and failure semantics, [event_bus.py](/home/arthur/dev/mistral-vibe/vibe/core/config/event_bus.py) for key matching, [protocol.py:560](/home/arthur/dev/mistral-vibe/vibe/app_server/protocol.py) for the wire models, [_config_introspect.py](/home/arthur/dev/mistral-vibe/vibe/app_server/_config_introspect.py) for field classification.

**Definition of Done:** `config/patch` and `config/fields/read` are dispatched with reference-conformant wire shapes, a rejected patch leaves every file byte-identical, and subscribers receive the changed key set.

#### US-066: Address configuration values by JSON Pointer
**Description:** As a Rust port maintainer, I want a patch core that resolves RFC 6901 pointers over the configuration document so that writes are expressed the way the protocol expresses them.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-059
**Reference:** [patch.py:61](/home/arthur/dev/mistral-vibe/vibe/core/config/patch.py:61) and [patch.py:89](/home/arthur/dev/mistral-vibe/vibe/core/config/patch.py:89) are the two operations that reach the store, [patch.py:104](/home/arthur/dev/mistral-vibe/vibe/core/config/patch.py:104) the token escaping, [patch.py:109](/home/arthur/dev/mistral-vibe/vibe/core/config/patch.py:109) the upsert resolution, [patch.py:137](/home/arthur/dev/mistral-vibe/vibe/core/config/patch.py:137) the parent auto-vivification and its skip rules for index and append tokens.

**Acceptance Criteria:**
- [ ] Given a pointer with `~0` and `~1` escapes, when it is resolved, then `~1` is decoded before `~0`, matching RFC 6901
- [ ] Given a `set` operation on `/tools/bash/allowlist` where `[tools.bash]` does not exist, when it is applied, then the intermediate tables are created and the leaf is set
- [ ] Given a `set` operation whose parent path traverses an existing non-table value, when it is applied, then the operation fails without mutating the document
- [ ] Given a pointer ending in `-` on an array, when a `set` is applied, then the value is appended
- [ ] Given a pointer with a numeric token past the end of an array, when it is applied, then the operation fails with the pointer named
- [ ] Given an upsert against a list field with a key field, when an entry with the same key exists, then it is replaced in place, otherwise the value is appended, and an absent or non-list target creates the section
- [ ] Given a `remove` on an absent path, when it is applied, then the operation fails and the document is unchanged

#### US-067: Write configuration through `config/patch`
**Description:** As an app-server client author, I want to write one field by pointer without knowing which file backs it so that the same call works against either implementation.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-066
**Reference:** [_resources.py:510](/home/arthur/dev/mistral-vibe/vibe/app_server/_resources.py:510) maps the wire ops onto patch operations, [protocol.py:560](/home/arthur/dev/mistral-vibe/vibe/app_server/protocol.py:560) defines the op, [protocol.py:567](/home/arthur/dev/mistral-vibe/vibe/app_server/protocol.py:567) the params and [protocol.py:574](/home/arthur/dev/mistral-vibe/vibe/app_server/protocol.py:574) the rejected and failures split. [orchestrator.py:163](/home/arthur/dev/mistral-vibe/vibe/core/config/orchestrator.py:163) is the preflight and per-layer routing, [orchestrator.py:250](/home/arthur/dev/mistral-vibe/vibe/core/config/orchestrator.py:250) the default-layer resolution.

**Acceptance Criteria:**
- [ ] Given a request with `ops`, `reason` and `reload_runtime`, when it is dispatched, then each op carries `op` of `set` or `remove`, a `path`, an optional `value` and an optional `target_layer`
- [ ] Given ops without `target_layer`, when they are routed, then they are applied to the writable layer the current selection resolves to
- [ ] Given a patch that would produce an invalid configuration, when the preflight runs, then the response sets `rejected` and no file on disk changes
- [ ] Given a patch whose preflight passes but whose write fails, when it is applied, then the response carries a non-empty `failures` list and `rejected` stays false
- [ ] Given a patch targeting the project layer while workspace trust is revoked, when it is applied, then it fails with the existing untrusted-project error and no file changes
- [ ] Given a concurrent external edit between load and write, when the patch is applied, then it fails with the existing concurrent-edit error rather than overwriting
- [ ] Given `config/batchWrite`, when this story lands, then it still dispatches and routes through the same core, and its retention is recorded as a local divergence

#### US-068: Describe every field through `config/fields/read`
**Description:** As an app-server client author, I want each field's kind, description, current value, per-layer values and popular flag so that I can render a settings screen without hard-coding the surface.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-064
**Reference:** [_resources.py:611](/home/arthur/dev/mistral-vibe/vibe/app_server/_resources.py:611) assembles the response and [_resources.py:628](/home/arthur/dev/mistral-vibe/vibe/app_server/_resources.py:628) the writable target list. [_config_introspect.py:73](/home/arthur/dev/mistral-vibe/vibe/app_server/_config_introspect.py:73) collects per-layer values highest priority first, [_config_introspect.py:90](/home/arthur/dev/mistral-vibe/vibe/app_server/_config_introspect.py:90) builds one wire per field, and [protocol.py:536](/home/arthur/dev/mistral-vibe/vibe/app_server/protocol.py:536) is the wire shape.

**Acceptance Criteria:**
- [ ] Given the method, when it is dispatched, then the response carries a field list and a writable-target list
- [ ] Given a field, when it is described, then it carries name, kind, description, current value, JSON Pointer path, popular flag, enum choices and per-layer values ordered highest priority first
- [ ] Given a field set by no layer, when it is described, then the layer values end with an entry naming the default layer carrying the registry default
- [ ] Given the popular set, when fields are described, then exactly the 13 reference entries are marked popular
- [ ] Given the `tools` field, when fields are described, then it is excluded from the list, matching the reference
- [ ] Given a field whose name the redaction rules treat as sensitive, when it is described, then its value and every per-layer value are redacted

#### US-069: Publish configuration changes to subscribers
**Description:** As a Rust port maintainer, I want components to subscribe to configuration keys so that a write applies without reloading the whole runtime.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-067
**Reference:** [event_bus.py:18](/home/arthur/dev/mistral-vibe/vibe/core/config/event_bus.py:18) is the bus, [event_bus.py:42](/home/arthur/dev/mistral-vibe/vibe/core/config/event_bus.py:42) the delivery filter and [event_bus.py:52](/home/arthur/dev/mistral-vibe/vibe/core/config/event_bus.py:52) the ancestor and descendant key match. [types.py:46](/home/arthur/dev/mistral-vibe/vibe/core/config/types.py:46) is the event payload, [orchestrator.py:258](/home/arthur/dev/mistral-vibe/vibe/core/config/orchestrator.py:258) the subscription entry point and [orchestrator.py:276](/home/arthur/dev/mistral-vibe/vibe/core/config/orchestrator.py:276) the changed-key diff.

**Acceptance Criteria:**
- [ ] Given a successful patch that changes the effective document, when it completes, then subscribers receive an event carrying the changed key set, the before and after documents and the reason
- [ ] Given a patch that succeeds but changes nothing, when it completes, then no event is published
- [ ] Given a subscription filtered on `models`, when `models/active` changes, then the subscriber is notified, and when `model` changes it is not, matching the ancestor and descendant rule
- [ ] Given a subscription with no key filter, when any change occurs, then the subscriber is notified
- [ ] Given a subscriber that unsubscribes, when a later change occurs, then it is not notified and no error is raised
- [ ] Given a subscriber callback that panics or errors, when an event is published, then remaining subscribers are still notified

---

### EP-021: Discovery, Harness Files, Dotenv and Migrations

Restore the file-resolution rules that decide which configuration is read, which is writable, and how an older file is brought forward.

Source navigation: [layers/project.py:105](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/project.py) for the walk-up, [harness_files/_harness_manager.py](/home/arthur/dev/mistral-vibe/vibe/core/config/harness_files/_harness_manager.py) for source selection and roots, [default_orchestrator.py:88](/home/arthur/dev/mistral-vibe/vibe/core/config/default_orchestrator.py) for persistence-layer selection, [vibe_schema.py:75](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py) for dotenv precedence, [_migration.py](/home/arthur/dev/mistral-vibe/vibe/core/config/_migration.py) for the four migrations.

**Definition of Done:** opening a subdirectory finds the repository's project configuration, `~/.vibe/.env` populates the process environment with process values winning, and the four migrations run once and record themselves.

#### US-070: Discover the project configuration by walking up
**Description:** As a Vibe operator, I want the project configuration found from any subdirectory of my repository so that opening the agent below the root does not silently drop it.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None
**Reference:** [project.py:108](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/project.py:108) is the walk and its stop condition, [project.py:63](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/project.py:63) resolves trust against the discovered directory, and [project.py:48](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/project.py:48) exposes whether a file was found at all.

**Acceptance Criteria:**
- [ ] Given a `.vibe/config.toml` at the repository root and a working directory three levels below, when the configuration loads, then the root file is the project layer
- [ ] Given files at two levels, when the configuration loads, then the nearest one to the working directory wins
- [ ] Given no file between the working directory and the home directory, when the configuration loads, then the project layer is empty and the user layer is selected
- [ ] Given the walk, when it reaches the directory containing the vibe home, then it stops without inspecting it or anything above it
- [ ] Given a discovered file whose directory is not trusted, when the configuration loads, then the project layer is empty and the selected target is the user file
- [ ] Given a symlinked working directory, when the walk runs, then it terminates and does not loop

#### US-071: Resolve readable and writable configuration sources
**Description:** As a Vibe operator, I want the binary to know which sources are enabled and which file is writable so that a write lands where the reference would put it.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-070
**Reference:** [_harness_manager.py:70](/home/arthur/dev/mistral-vibe/vibe/core/config/harness_files/_harness_manager.py:70) selects the configuration file, [_harness_manager.py:81](/home/arthur/dev/mistral-vibe/vibe/core/config/harness_files/_harness_manager.py:81) the project roots, [_harness_manager.py:99](/home/arthur/dev/mistral-vibe/vibe/core/config/harness_files/_harness_manager.py:99) the hook files, [_harness_manager.py:108](/home/arthur/dev/mistral-vibe/vibe/core/config/harness_files/_harness_manager.py:108) the persistence rule, [_harness_manager.py:154](/home/arthur/dev/mistral-vibe/vibe/core/config/harness_files/_harness_manager.py:154) the user file. [default_orchestrator.py:88](/home/arthur/dev/mistral-vibe/vibe/core/config/default_orchestrator.py:88) is the persistence-layer selection, with [user.py:9](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/user.py:9) and [overrides.py:11](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/overrides.py:11) as its two fallbacks, and [_paths.py:5](/home/arthur/dev/mistral-vibe/vibe/core/config/harness_files/_paths.py:5) the global directory set.

**Acceptance Criteria:**
- [ ] Given enabled sources of user and project, when the configuration file is resolved, then a trusted project file wins and the user file is the fallback
- [ ] Given only the project source enabled, when persistence is attempted, then it is refused, matching `persist_allowed`
- [ ] Given neither a trusted project file nor a user source, when the persistence layer is resolved, then an ephemeral in-memory layer is used and writes do not touch disk
- [ ] Given additional working directories, when project roots are resolved, then they are absolutized and deduplicated, and a root equal to the working directory is dropped
- [ ] Given an additional directory containing the working directory, when project roots are resolved, then both survive
- [ ] Given the resolved roots, when hook files are listed, then each root's `.vibe/hooks.toml` is listed followed by the user-level file when the user source is enabled

#### US-072: Load the global dotenv file
**Description:** As a Vibe operator, I want `~/.vibe/.env` read at startup so that API keys stored there are visible to this binary.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None
**Reference:** [vibe_schema.py:75](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py:75) is the loader, its precedence rule and its FIFO allowance, and [_vibe_home.py:7](/home/arthur/dev/mistral-vibe/vibe/core/paths/_vibe_home.py:7) resolves the file path from the vibe home.

**Acceptance Criteria:**
- [ ] Given a `~/.vibe/.env` with a variable absent from the process environment, when startup completes, then the variable is set
- [ ] Given a variable already set to a non-empty value in the process environment, when the file also sets it, then the process value wins
- [ ] Given an empty value in the file, when it is read, then the variable is not set
- [ ] Given the path is a FIFO rather than a regular file, when startup runs, then it is still read
- [ ] Given no such file, when startup runs, then it proceeds without error
- [ ] Given a malformed line, when the file is parsed, then the line is skipped, startup continues, and no value from the file appears in any log or error message

#### US-073: Migrate older configuration files once
**Description:** As a Vibe operator, I want a configuration written by an older client brought forward so that renamed tools and models keep working.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-066, US-071
**Reference:** [_migration.py:33](/home/arthur/dev/mistral-vibe/vibe/core/config/_migration.py:33) drives the layers and [_migration.py:57](/home/arthur/dev/mistral-vibe/vibe/core/config/_migration.py:57) orders the four migrations: [_migration.py:69](/home/arthur/dev/mistral-vibe/vibe/core/config/_migration.py:69) the allowlist, [_migration.py:90](/home/arthur/dev/mistral-vibe/vibe/core/config/_migration.py:90) the one-shot read-only sync keyed by [_migration.py:18](/home/arthur/dev/mistral-vibe/vibe/core/config/_migration.py:18), [_migration.py:108](/home/arthur/dev/mistral-vibe/vibe/core/config/_migration.py:108) the model rename, [_migration.py:160](/home/arthur/dev/mistral-vibe/vibe/core/config/_migration.py:160) the tool rename driven by [_migration.py:22](/home/arthur/dev/mistral-vibe/vibe/core/config/_migration.py:22) and [_migration.py:25](/home/arthur/dev/mistral-vibe/vibe/core/config/_migration.py:25).

**Acceptance Criteria:**
- [ ] Given a bash allowlist without `find`, when migrations run, then `find` is added and the list is sorted
- [ ] Given allowlist entries ending in a trailing wildcard, when migrations run, then the suffix is stripped and duplicates are collapsed
- [ ] Given an allowlist and no record of the read-only migration, when migrations run, then the default read-only commands are unioned in and the migration id is appended to `applied_migrations`
- [ ] Given the read-only migration already recorded, when migrations run again, then the allowlist is unchanged
- [ ] Given a model named `mistral-vibe-cli-latest` aliased `devstral-2`, when migrations run, then the alias, temperature, prices and thinking level are updated, the map key is renamed, and an `active_model` pointing at the old alias is repointed
- [ ] Given tool settings under the old names, when migrations run, then they move to the new names without clobbering an existing new key, the dropped options are removed, and the names are rewritten inside the enable and disable lists
- [ ] Given a read-only or untrusted configuration source, when migrations would write, then the write is skipped and the load still succeeds
- [ ] Given a migration writes, when the write fails, then the failure is reported and the original file is left intact

---

### EP-022: MCP Configuration Completeness

Close the gap between the 30-line preflight and the 404-line reference module that decides how an MCP server is named, deduplicated, authenticated and removed.

Source navigation: [mcp_servers.py](/home/arthur/dev/mistral-vibe/vibe/core/config/mcp_servers.py) for URL normalization, name suggestion and add/remove, [models.py:146](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py) for the transport and auth unions and the name normalization rule.

**Definition of Done:** the three reference transports are accepted, both auth kinds round-trip, and adding a server produces the same name and the same rejection as the reference for the same input.

#### US-074: Normalize and deduplicate MCP server URLs
**Description:** As a Vibe operator, I want the same server rejected as a duplicate whichever spelling of its URL I use so that I do not end up with two entries for one endpoint.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-063
**Reference:** [mcp_servers.py:216](/home/arthur/dev/mistral-vibe/vibe/core/config/mcp_servers.py:216) is the public normalization, [mcp_servers.py:260](/home/arthur/dev/mistral-vibe/vibe/core/config/mcp_servers.py:260) the parse and every rejection rule, [mcp_servers.py:334](/home/arthur/dev/mistral-vibe/vibe/core/config/mcp_servers.py:334) the comparison key, [mcp_servers.py:338](/home/arthur/dev/mistral-vibe/vibe/core/config/mcp_servers.py:338) the host and port canonicalization, [mcp_servers.py:353](/home/arthur/dev/mistral-vibe/vibe/core/config/mcp_servers.py:353) the loopback test.

**Acceptance Criteria:**
- [ ] Given a URL, when it is normalized, then the scheme and host are lowercased, a default port for the scheme is dropped, an IPv6 host is bracketed, and the fragment is removed
- [ ] Given two URLs differing only by trailing slash or default port, when they are compared, then they are the same server
- [ ] Given an `http` URL whose host is not loopback, when a server is added, then it is rejected
- [ ] Given an `http` URL on localhost or a loopback address, when a server is added, then it is accepted
- [ ] Given a URL with credentials, a fragment, no scheme, no host, or a scheme other than http and https, when a server is added, then it is rejected with a message naming the defect and not echoing the URL's userinfo
- [ ] Given an existing server with an equivalent URL, when another is added, then the rejection names the existing server

#### US-075: Derive and deduplicate MCP server names
**Description:** As a Vibe operator, I want a usable alias derived from the URL when I do not supply one so that adding a server needs one argument.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-074
**Reference:** [models.py:139](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py:139) is the name normalization rule, [mcp_servers.py:362](/home/arthur/dev/mistral-vibe/vibe/core/config/mcp_servers.py:362) the suggestion from the host, [mcp_servers.py:376](/home/arthur/dev/mistral-vibe/vibe/core/config/mcp_servers.py:376) the path fallback, [mcp_servers.py:384](/home/arthur/dev/mistral-vibe/vibe/core/config/mcp_servers.py:384) the numeric dedupe and [mcp_servers.py:248](/home/arthur/dev/mistral-vibe/vibe/core/config/mcp_servers.py:248) the explicit-name collision rule.

**Acceptance Criteria:**
- [ ] Given a name, when it is normalized, then characters outside letters, digits, underscore and hyphen become underscores, leading and trailing underscores and hyphens are stripped, and the result is truncated to 256 characters
- [ ] Given a name that normalizes to empty, when it is used, then the operation is rejected with a message saying the name must contain letters or numbers
- [ ] Given no name and a host of three or more labels beginning with a droppable prefix, when a name is suggested, then the prefix label is dropped
- [ ] Given a first label that is generic, when a name is suggested, then the first non-generic path segment is used, and `mcp` is the final fallback
- [ ] Given a suggested name that collides, when it is deduplicated, then a numeric suffix starting at two is appended until it is free
- [ ] Given an explicitly requested name that collides, when a server is added, then it is rejected rather than renamed

#### US-076: Accept every MCP transport and auth form
**Description:** As a Vibe operator, I want the same MCP block accepted by both clients so that a configuration written by one does not fail to load in the other.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-075
**Reference:** [models.py:146](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py:146) carries the fields shared by every transport, [models.py:194](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py:194) static auth and its validators, [models.py:296](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py:296) OAuth, [models.py:329](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py:329) the legacy promotion, [models.py:345](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py:345) the HTTP fields, [models.py:367](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py:367) stdio and its argv rule, [models.py:388](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py:388) the discriminated union. [mcp_servers.py:151](/home/arthur/dev/mistral-vibe/vibe/core/config/mcp_servers.py:151) is the removal path.

**Acceptance Criteria:**
- [ ] Given a transport of `http`, `streamable-http` or `stdio`, when the entry is decoded, then it is accepted, and any other transport is rejected without echoing the entry's command or URL
- [ ] Given a static auth block, when it is decoded, then headers are validated as HTTP header names, duplicates differing only by case are rejected, the env var name is validated, and the token format string is rejected unless it references exactly the token placeholder
- [ ] Given legacy top-level auth keys, when the entry is decoded, then they are promoted into a static auth block, and mixing them with an explicit auth block is rejected
- [ ] Given an OAuth auth block, when it is decoded, then scopes are required, a client id and a client metadata URL are mutually exclusive, and the redirect port is bounded to the non-privileged range
- [ ] Given a static auth block with an env var set, when headers are computed, then the token header is added unless an explicit header of the same name already exists, compared case-insensitively
- [ ] Given a stdio entry whose command is a string, when the argument vector is built, then it is shell-split and the declared arguments are appended
- [ ] Given a persisted server, when it is removed by name, then the entry disappears from the writable layer, and removing an unknown name reports not-removed rather than failing
- [ ] Given `prompt`, `sampling_enabled`, `disabled` and `disabled_tools`, when an entry is decoded, then each is honored with the reference default

---

### EP-023: Layer Lifecycle and Certification

Add the last missing layer, lock the corpus into CI, and re-score the audit so the claim is measured rather than declared.

Source navigation: [layers/discovered.py](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/discovered.py) for the runtime-discovery layer, [layer.py:263](/home/arthur/dev/mistral-vibe/vibe/core/config/layer.py) for load, cache and trust transitions.

**Definition of Done:** discovered tool defaults enter the stack at the documented priority, CI fails on any corpus divergence, and `docs/parity.md` carries a remeasured configuration score with the evidence behind it.

#### US-077: Compose runtime-discovered defaults as their own layer
**Description:** As a Rust port maintainer, I want tool defaults discovered at runtime to enter the stack as a layer so that they are overridable by every file the operator controls.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-060, US-063
**Reference:** [discovered.py:11](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/discovered.py:11) is the layer, and [default_orchestrator.py:45](/home/arthur/dev/mistral-vibe/vibe/core/config/default_orchestrator.py:45) shows its position in the stack relative to the defaults and the selected file.

**Acceptance Criteria:**
- [ ] Given discovered tool defaults, when the configuration composes, then they sit above the schema defaults and below the selected file
- [ ] Given a user file setting a key the discovery layer also sets, when the configuration composes, then the user value wins
- [ ] Given no discovery result, when the configuration composes, then the layer is empty and the effective document is unchanged
- [ ] Given the layer list published by `config/read`, when this story lands, then the discovered layer appears in the layer values with its own name
- [ ] Given a discovery pass that fails, when the configuration composes, then the load succeeds with an empty discovery layer and the failure is reported once

#### US-078: Gate the configuration surface in CI and remeasure the audit
**Description:** As a Rust port maintainer, I want the configuration corpus enforced in CI so that no later change silently regresses the surface.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-061, US-064, US-068, US-073, US-076
**Reference:** [fingerprint.py:30](/home/arthur/dev/mistral-vibe/vibe/core/config/fingerprint.py:30) is the reference fingerprint format this port diverges from, and [fingerprint.py:17](/home/arthur/dev/mistral-vibe/vibe/core/config/fingerprint.py:17) its concurrency guard. Local targets: `docs/parity.md`, `CHANGELOG.md` and the CI conformance count already reported for the tool surface.

**Acceptance Criteria:**
- [ ] Given the CI workflow, when it runs, then it reports the number of conforming scenarios and fields, as the tool-surface conformance count already does
- [ ] Given a registry field added without a corpus entry, when CI runs, then it fails naming the field
- [ ] Given the completed epics, when `docs/parity.md` is updated, then the configuration row carries a remeasured score, the measurement method, and any accepted divergence
- [ ] Given the accepted divergences, when they are recorded, then each names the reason and the evidence, and the fingerprint format is among them
- [ ] Given `CHANGELOG.md`, when this story lands, then the user-visible configuration changes are recorded under the unreleased heading

## Functional Requirements

- FR-01: The system must declare every configuration field once, with its kind, default, description, merge strategy and popular flag, and derive the published schema, the defaults layer, the settings field list and environment coercion from that declaration.
- FR-02: The system must merge each field by its declared strategy, supporting replace, concat, union by key and deep merge.
- FR-03: The system must treat an empty table supplied where a list field is expected as absent rather than failing.
- FR-04: The system must preserve keys absent from the registry in the effective document rather than dropping them.
- FR-05: The system must compose a non-empty defaults layer at every construction site outside tests that assert empty-layer behavior.
- FR-06: The system must normalize models into an alias-keyed map on read and serialize them back as an array on write.
- FR-07: The system must dispatch `config/patch` and `config/fields/read` with the reference wire shapes.
- FR-08: When a patch fails its merged-configuration preflight, the system must reject the whole request and leave every file byte-identical.
- FR-09: When a patch passes preflight but a layer write fails, the system must report the failure per layer rather than rejecting the request.
- FR-10: The system must publish a change event carrying the changed key set after any patch that alters the effective document, and must not publish when nothing changed.
- FR-11: The system must discover the project configuration by walking parent directories, stopping at the directory containing the vibe home.
- FR-12: The system must refuse to persist to a project file whose directory is not trusted.
- FR-13: The system must load the global dotenv file at startup, with non-empty process values taking precedence, and must accept a FIFO at that path.
- FR-14: The system must apply each configuration migration at most once and record one-shot migrations in `applied_migrations`.
- FR-15: The system must accept the `http`, `streamable-http` and `stdio` MCP transports and both the static and OAuth auth forms, including promotion of legacy top-level auth keys.
- FR-16: The system must reject an MCP server whose URL is equivalent to a configured one, and must reject a plaintext HTTP URL unless its host is loopback.
- FR-17: The system must NOT include any value from a sensitive-named key in an error message, a log line or a corpus entry.
- FR-18: The system must NOT copy reference-authored description text into the repository or into a committed corpus.
- FR-19: The system must NOT fail `cargo test` when the reference checkout is absent or off-pin; only the live probe may skip.

## Non-Functional Requirements

- **Performance:** `LayeredConfig::load()` completes in under 25 ms at P95 for a stack of six layers whose files total 64 KiB, measured by a criterion-free timing assertion in the corpus test on the CI runner. `config/fields/read` answers in under 50 ms at P95 for the 64-field surface.
- **Security:** No error, log line, event payload or corpus entry contains a value from a key matching the sensitive-key rule (`crates/vibe-core/src/config.rs:1358`), asserted by a test that seeds a secret into every layer and greps every produced string. Configuration files and their transaction sidecars keep mode 0600 and their parent directory 0700. Proxy-bearing URLs carrying credentials remain rejected at load.
- **Reliability:** A rejected or failed patch leaves every target file byte-identical, asserted by SHA-256 comparison before and after. An interrupted write is recovered on the next load, with 0 partially written configuration files across 200 simulated interruption points in the existing journal test.
- **Compatibility:** 100% of configuration files that load today still load after the change. 0 of the 64 reference fields fail to round-trip through write then read.
- **Scalability:** A configuration file up to the existing 4 MiB limit loads without additional allocation beyond twice its size, and a `mcp_servers` array of 256 entries merges in under 10 ms.
- **Observability:** Every load failure names the field or JSON pointer at fault. 100% of validation warnings, such as the `active_model` fallback, are readable through a public accessor rather than only logged.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Empty stack | No user file, no project file, no environment | Defaults layer alone produces a complete, valid configuration | — |
| 2 | Unknown key | A key not in the registry appears in a layer | Preserved in the effective document, reported as unregistered, never dropped | — |
| 3 | Wrong type from environment | `VIBE_API_TIMEOUT=abc` on a float field | Load fails naming the variable, value not echoed | "Environment variable VIBE_API_TIMEOUT is not a valid value for api_timeout" |
| 4 | Union entry without its key | An entry in `providers` missing `name` | Load fails naming the field and merge key, entry contents not echoed | "providers entries require a name" |
| 5 | Concurrent external edit | The file changes between load and write | Write fails with the existing concurrent-edit error, no overwrite | "Configuration changed on disk, reload and retry" |
| 6 | Patch preflight rejection | A patch that would leave zero configured models | Whole request rejected, every file byte-identical | "This change would leave no configured model" |
| 7 | Partial patch failure | Preflight passes, one of two layer writes fails | Per-layer failure reported, the succeeding write stands | "1 of 2 configuration writes failed" |
| 8 | Trust revoked mid-session | Project trust withdrawn between two loads | Project layer empties, selection falls back to the user file, no project write is possible | "Project configuration is unavailable until the workspace is trusted" |
| 9 | Walk-up boundary | Working directory outside the home tree | Walk stops at the vibe home's parent, project layer stays empty | — |
| 10 | Dotenv conflict | Variable set in both the process and the file | Process value wins, file value ignored | — |
| 11 | Malformed dotenv line | A line without a separator | Line skipped, startup continues, no file value logged | — |
| 12 | Migration on a read-only file | Migration needed, file not writable | Migration skipped, load succeeds, in-memory result unmigrated | "Configuration migration skipped: file is not writable" |
| 13 | Duplicate MCP URL | Two entries resolving to the same normalized URL | Add rejected naming the existing server | "This URL is already configured as `docs`" |
| 14 | Plaintext MCP URL | `http://` host that is not loopback | Add rejected | "MCP server URL must use https unless it points to localhost" |
| 15 | Mixed MCP auth | Legacy top-level keys alongside an explicit auth block | Entry rejected naming both forms | "Move the legacy auth keys into the [auth] block" |
| 16 | Oversize file | Configuration file above 4 MiB | Load fails with the existing size error before parsing | "Configuration file exceeds the 4 MiB limit" |
| 17 | Subscriber failure | A change subscriber errors | Remaining subscribers still notified, the patch result unaffected | — |
| 18 | Absent reference checkout | Corpus test runs on a machine without the reference | Replay executes, live probe skips, suite passes | — |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Shipping defaults changes behavior for existing users whose files relied on a key being absent | High | High | Land defaults behind the corpus in US-063 with an explicit before-and-after diff of the effective document for a fixture user file, and record the behavior change in `CHANGELOG.md` |
| 2 | Turning the epic into a rewrite of the whole layer lifecycle, which `docs/parity.md` explicitly warns against | Medium | High | The layer state machine of `layer.py` is deliberately out of scope except for the discovered layer; the Rust stateless reload stays, recorded in Non-Goals |
| 3 | The reference builder cannot be driven headlessly, weakening the oracle | Medium | High | US-061 states the fallback: capture `create_default_config()` plus hand-authored stack fixtures, and record the reduced scenario count in `docs/parity.md` |
| 4 | The 5 locally invented keys have persisted values that a mapping would silently reinterpret | Medium | Medium | US-064 requires either an explicit mapping with a round-trip test or a recorded divergence, never a silent drop |
| 5 | Correcting `disabled_tools` to concat exposes tools a user believed disabled, or hides tools they expected | Medium | High | US-060 ships with a fixture covering both directions, and the change is called out in `CHANGELOG.md` as a behavior fix |
| 6 | Scope creep from the 54 absent keys into implementing the features that consume them | High | Medium | The registry declares every key, but only keys with an existing consumer are read; `docs/parity.md` already states the 52-key rule that each key arrives with its feature |
| 7 | Fingerprint divergence breaks a mixed-client workflow if a Python client ever consumes a Rust fingerprint | Low | Medium | Recorded as an accepted divergence in US-078 with the reason; the token is opaque and never compared across implementations today |
| 8 | Twenty stories exceed a comfortable single-PRD size | Medium | Medium | Epics are independently shippable and ordered; EP-018 through EP-020 are the mechanism and can ship as phase one, EP-021 through EP-023 as phase two |

## Non-Goals

- Porting the per-layer asynchronous state machine of `layer.py`, including cached loads, forced reloads, cache invalidation and the grant and revoke trust transitions. Rust reloads statelessly from disk on every `load()`, which is observably equivalent for every current caller and simpler. Revisit if a caller needs to hold a layer across trust changes.
- Porting the GrowthBook layer and its experiment-to-field mapping ([layers/growthbook.py:28](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/growthbook.py:28), whose only current mapping targets `system_prompt_id`). `docs/parity.md` already records experiments as an accepted divergence requiring third-party credentials this repository does not hold. The `experiments` table stays as the injection point.
- Reshaping the agent-profile overlay. The reference holds it in a dedicated in-memory layer ([layers/agent_profile.py:11](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/agent_profile.py:11)); this port already composes an equivalent `Agent` table at the top of the stack, and the two are observably identical for every current caller. Only its merge strategy changes, through US-060.
- Implementing the `shallow` and `conflict` merge strategies. No field in `VibeConfigSchema` declares either, so neither is reachable and neither produces observable behavior. US-060 adds a test that fails if a field ever adopts one.
- Implementing the features behind the newly declared keys. Declaring `auto_compact_threshold` does not implement automatic compaction; declaring `otel_endpoint` does not implement OTel. Each key is declared, defaulted, published and merged, and is read when its feature lands.
- Exposing `tools` in the settings field list. The reference excludes it explicitly, and per-tool editing has no UI on either side.
- Aligning the fingerprint token on the reference's stat-based format. Recorded as an accepted divergence with the reason in US-078.
- Removing `config/batchWrite`. It is retained as a local alias over the same core so existing callers keep working; its removal is a separate decision once no caller remains.
- Mapping the 5 locally invented keys onto reference fields. Decided in US-064 and recorded in the divergence table below; each stays declared, published and merged under its own name, so a value already on disk keeps loading and nothing is silently reinterpreted.

### Recorded divergences from the reference key surface

Each entry is published by `config/schema` and declared in the registry with `local: true`, which keeps it out of the shipped default document so the defaults stay comparable to `create_default_config()` field for field. `crates/vibe-core/src/config/surface_parity_tests.rs` fails if this set changes.

| Key | Reason no mapping exists |
|---|---|
| `thinking` | Reasoning effort is per model upstream (`ModelConfig.thinking`); a top-level value would have to pick one model to write to, and reading it back would not round-trip |
| `notifications` | Tri-state (`off`, `unfocused`, `always`) where upstream `enable_notifications` is a boolean, so `unfocused` has no lossless target |
| `proxy` | Upstream carries no proxy configuration at all; this port persists one and rejects credential-bearing URLs at load |
| `tls_ca_path` | Same: no upstream field, and the value feeds this port's own TLS setup |
| `dotenv_path` | Upstream always reads `~/.vibe/.env` and exposes no field; the configurable path is local. US-072 loads the global file and keeps this key as the override |

Two further divergences are recorded where the port publishes more than the reference rather than something else:

| Divergence | Reason |
|---|---|
| `theme` is an `enum` here and `str` upstream | The port ships a theme catalog, so the settings screen offers a picker instead of a free-text field. The accepted values are a superset of the reference catalog |
| An unregistered key survives the merge | FR-04. The reference merge drops a key its schema does not declare; keeping it lets a file written by a newer client round-trip through this one, and `ConfigSnapshot::unregistered_keys` reports the set |

## Files NOT to Modify

- `crates/vibe-app-server/tests/tool-surface/baseline.json` — the committed tool-surface conformance target; a configuration change must never require editing it
- `crates/vibe-app-server/src/tool_surface_parity_tests.rs` — the existing oracle harness; extend by adding a sibling module, do not repurpose
- `crates/vibe-cli/tests/runtime-parity/` — recorded runtime traces; a configuration change that alters them signals a regression, not a fixture update
- `NOTICE` — the licensing boundary this PRD is written under
- `/home/arthur/dev/mistral-vibe/**` on Linux and `C:\dev\mistral-vibe\**` on Windows — the reference checkout is read-only

## Technical Considerations

Framed as questions for engineering input, not mandates:

- **Registry shape:** a static slice of const structs versus a lazily built map. Recommended: a const slice with a lazily built index, so the declaration is greppable and the lookup is not linear. Engineering to confirm that const construction covers every default, including the nested provider and model documents, or whether those need a builder function.
- **Where the registry lives:** `vibe-core` is the only layer both `vibe-app-server` and the CLI can reach. Recommended: a new `config/registry.rs` module beside `config.rs`, keeping `config.rs` as the store. Should the registry own the schema emission too, or should that stay in `config.rs`?
- **Patch representation:** recommended is a hand-rolled `set` and `remove` over `toml::Table` using `serde_json`'s RFC 6901 token rules, adding no dependency. The alternative is the `json-patch` crate, which speaks `serde_json::Value` and would force a document conversion on every write. Trade-off: roughly 150 lines of owned code against a conversion cost and a dependency whose last visible activity is 2025.
- **Model map storage:** the reference keys models by alias internally and serializes them as an array. Recommended: mirror that exactly, since the deep-merge strategy depends on the map shape. Should the internal shape be exposed through `config/read`, or normalized back to the array form for the wire? The reference exposes the map.
- **Dotenv parsing:** recommended is a minimal parser covering `KEY=value`, single and double quotes, and an optional `export` prefix, matching what `ProxyEnvironmentStore` already writes. Is a fuller dotenv grammar needed, and if so does it justify a dependency?
- **Migration timing:** the reference migrates when the orchestrator is built, before the first merge. Recommended: migrate on the first load per process, guarded by the existing file lock. Should a migration write be attempted when the process holds no write intent, or deferred to the first real write?
- **Oracle transport:** recommended is a new `scripts/parity/config_surface.py` following the self-re-execution pattern of `scripts/parity/tool_surface.py`, with the corpus committed under `crates/vibe-core/tests/config-surface/`. Should the corpus live in `vibe-core` beside the merge it proves, or in `vibe-app-server` beside the existing baseline?

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Reference fields declared and published | 10 of 64 | 64 of 64 | Month-1 | Registry length asserted against the corpus field list |
| Fields merged by the correct strategy | 45 of 64 (replace only, by coincidence) | 64 of 64 | Month-1 | Per-strategy scenarios in the replayed corpus |
| Reference config methods dispatched | 6 of 8 | 8 of 8 | Month-1 | Method inventory diff against `vibe/app_server/protocol.py` |
| Corpus scenarios replayed in `cargo test` | 0 | 24 or more | Month-1 | Test output count, reported by CI as the tool-surface count already is |
| Construction sites with an empty defaults layer | 3 | 0 | Month-1 | `grep -rn 'Table::new()' crates` filtered to `LayeredConfig` call sites |
| Configuration migrations applied | 0 of 4 | 4 of 4 | Month-1 | Fixture files migrated and asserted in unit tests |
| `docs/parity.md` configuration score | 40, declarative | 95 or above, oracle-backed | Month-6 | Remeasured in US-078 with the method stated |
| Secret values appearing in errors, logs or corpora | Unmeasured | 0 | Month-1 | Seeded-secret test grepping every produced string |

## Open Questions

- ~~Should the 5 locally invented keys (`thinking`, `notifications`, `proxy`, `tls_ca_path`, `dotenv_path`) be mapped onto reference fields or kept as documented divergences?~~ Resolved 2026-08-05 by Arthur Jean: all five stay as recorded divergences, written out in the divergence table under Non-Goals. No mapping is lossless, and a mapping would reinterpret values already on disk.
- Does any external consumer call `config/batchWrite`? If not, it can be deprecated in a follow-up rather than carried indefinitely. Owner: Arthur Jean, before US-067 lands, affects the divergence table only.
- ~~Should the corpus live under `vibe-core` or beside the existing tool-surface baseline in `vibe-app-server`?~~ Resolved in US-061: `crates/vibe-core/tests/config-surface/corpus.json`, beside the merge it proves.
- ~~Should the internal model map be exposed through `config/read`, or normalized back to the array form for the wire?~~ Resolved in US-065: the map is exposed, as the reference exposes it. The persisted form stays the `[[models]]` list, and the two CLI readers of the array form were redirected to the map in the same change.
- Is a criterion-style benchmark warranted for the load path, or is the timing assertion in the corpus test sufficient? Owner: engineering, before the NFR is certified in US-078.
[/PRD]
