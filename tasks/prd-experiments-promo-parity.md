[PRD]
# PRD: Experiments, GrowthBook and the VS Code Promo

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-12 | Arthur Jean | Initial draft. Closes rank 16 of `docs/parity.md`, the last open rank of the execution order. |

## Problem Statement

`docs/parity.md` scores two parts below every other row it still intends to
close: experiments and rollouts at 25 and the VS Code extension promo at 10.
Together they are rank 16, the only rank of the execution order still marked
`TODO` now that rank 15 closed on 2026-08-12. Both rows are declarative: no
oracle produces either number.

Measured against the pinned reference `b78b451`, the gap is the following.

1. **The experiments engine is absent in totality, 598 lines.** The reference
   ships `vibe/core/experiments/` (7 files, 497 lines) and
   `vibe/core/config/layers/growthbook.py` (101 lines). Nothing in `crates/`
   resolves a variant, addresses the eval endpoint, filters a feature set,
   distinguishes a confirmed exposure from a forced rule, or maps a variant onto
   a configuration field. What exists is the socket the engine would plug into:
   `ConfigLayerKind::Experiments` in the layer list
   (`crates/vibe-core/src/config.rs:86,658`), the `experiments` table with the
   reference's own schema and defaults
   (`crates/vibe-core/src/config/registry.rs:401-408,525-530,966-969`), and a
   `with_experiments` setter whose only caller is a test
   (`crates/vibe-core/src/config.rs:542`, called at `:3121`).
2. **The layer sits at the wrong precedence, which inverts the contract.** The
   reference composes `Default, GrowthBook, user TOML, project TOML,
   environment, overrides, admin`
   (`vibe/core/config/default_orchestrator.py:65-76`), so a value a user wrote
   beats an experiment assignment. This port composes `Defaults, Discovered,
   SelectedToml, Experiments, Environment, Runtime, Agent` and merges in order
   (`crates/vibe-core/src/config.rs:644-677`), so the experiment layer overrides
   the user's own file. Three reference tests assert the opposite behavior
   (`test_selected_toml_wins_over_growthbook_layer`,
   `test_selected_toml_disables_growthbook_managed_shell`,
   `test_forced_growthbook_variant_without_tracks_loses_to_selected_toml` in
   `tests/core/config/test_growthbook_layer.py`). The bug is latent only because
   nothing fills the layer.
3. **Three of the four fields the layer writes are undeclared here.** The
   mapping targets `system_prompt_id`, `routed_default_model`,
   `routed_model_config` and `managed_shell_tools_enabled`
   (`vibe/core/config/layers/growthbook.py:48-62`). The last three are recorded
   as undeclared in `UNDECLARED_FIELDS`
   (`crates/vibe-core/src/config/surface_parity_tests.rs:60-73`) and
   `docs/parity.md:145`. Worse, `active_model` is pinned here where the reference
   ships an empty sentinel resolved on read
   (`UNPINNED_ACTIVE_MODEL` at `surface_parity_tests.rs:75-85`,
   `docs/parity.md:146`), and `routed_default_model` only ever takes effect for
   an unpinned user (`vibe/core/config/vibe_schema.py:473-479`). Porting the
   layer without that resolution would ship a layer whose values change nothing.
4. **The session never carries an assignment, though the field is already on
   disk.** `SessionMetadata` serializes `experiment_state` under the reference's
   `experiments` key, inherits it on fork and migrates it from the legacy shape
   (`crates/vibe-core/src/storage.rs:48-49,667,1279`). Nothing writes it, nothing
   reads it, and no resume path hydrates from it, where the reference persists on
   initialize (`vibe/core/session/session_logger.py:488-497`), hydrates on resume
   (`vibe/core/experiments/session.py:68-80`) and hands the exported state to a
   forked runtime (`vibe/app_server/_runtime.py:420`).
5. **Every telemetry event ships without its experiment exposure.** The metadata
   census carries `experiments` and correctly omits an empty map
   (`crates/vibe-core/src/telemetry.rs:329-338,423-425`), but no producer ever
   fills `TelemetryContext.experiments` (`telemetry.rs:400-405`): both entry
   points construct the context with the field defaulted
   (`crates/vibe-cli/src/lib.rs:754-763`, `crates/vibe-acp/src/main.rs:368`).
   `docs/parity.md` already records this as its own divergence row, deferring to
   this rank. Rank 15 therefore closed at 100 with one field permanently empty.
6. **The VS Code promo has no counterpart at all, and no state.** The reference
   ships `vibe/cli/vscode_extension_promo/` (4 files) with a three-condition
   predicate, a `[vscode_extension_promo]` section of `cache.toml` and two
   display paths (`vibe/cli/textual_ui/app.py:4071-4105,4130-4139`). This port
   already owns the surrounding machinery: `UpdateCacheStore` reads and writes a
   named section of the same `cache.toml` while preserving its siblings
   (`crates/vibe-core/src/updates.rs:295-375`), `announce_release_notes` is the
   reference's own display point (`crates/vibe-cli/src/tui/mod.rs:991-1008`), and
   `detect_terminal_emulator` already splits `vscode`, `vscode_insiders` and
   `cursor` (`crates/vibe-core/src/telemetry.rs:540-575`). Only the feature is
   missing.

**Why now:** rank 15 closed on 2026-08-12 and rank 16 is the only rank left, so
this is the last work the execution order tracks. It is also the rank whose
stated blocker turns out to be wrong. `docs/parity.md:154` justifies the
divergence with "credentials this repository does not hold", but the GrowthBook
client key is a publishable credential by the vendor's own security
documentation, and the exact key the reference ships is already committed here at
`registry.rs:527`. Leaving the row at 25 for a reason the repository itself
falsifies is worse than leaving it open: it makes the scorecard's own
justifications unreliable, which is the property the last three remeasures were
spent buying back.

## Overview

This PRD closes rank 16 by building the instrument first, removing the
configuration blockers second, and porting the two features third.

The instrument is a differential oracle in the shape every closed rank uses: a
capture script that drives the reference's own `RemoteEvalClient`,
`ExperimentManager`, `GrowthbookLayer`, session helpers and promo repository over
inputs the script authors, and a Rust replay that reads the committed corpus
unconditionally while only the recapture probe skips when the checkout is absent
or off-pin. The capture never reaches the network: the eval request is
intercepted one call before the connection, exactly as `scripts/parity/voice.py`
already does, and a socket guard fails the run on any connection attempt.

The measurement is sound without any GrowthBook account, and this is the
strategic finding that unblocks the rank. In remote evaluation mode the proxy
performs the bucketing and rewrites every feature as a pre-resolved `force` rule
carrying the exposure metadata in `tracks`; the client performs no hashing at
all. What a client implementation can get wrong is therefore entirely local:
which URL it builds, what payload it posts, how it fails open, how it resolves a
value, which features it keeps, and which variants it lets reach configuration
versus telemetry. All of that is capturable from the reference by feeding both
sides the same synthetic eval response. The live round trip to
`experiments.mistral.services` stays unmeasured and is recorded as a residual,
the same treatment the OTLP wire already carries.

Two blockers are removed rather than worked around. The experiment layer is
reseated below the selected TOML layer, which is where the reference puts it and
where its own tests assert it belongs. And the three fields the mapping writes
are declared together with the resolution that gives them meaning: an unpinned
`active_model` resolved on read from `routed_default_model`, and a
`managed_shell_tools_enabled` that actually selects the managed shell family.
Declaring the keys without that resolution is the failure mode `docs/parity.md`
names in its own configuration row, so the resolution is in scope and
`show_greeting`, which no experiment targets, is not.

The promo is ported whole: the predicate, the ceiling, the start instant, the
cache section and both display paths. Its two sentences are reference-authored
prose that `NOTICE` forbids, so they are recorded as digests and answered with
original prose, the treatment the eleven sign-in sentences already receive. What
the message should say, given that the advertised extension drives the Python
binary rather than this one, is the one product decision this PRD escalates
rather than settles.

## Goals

| Goal | At EP-005 completion | At PRD completion |
|------|---------------------|-------------------|
| `docs/parity.md` experiments row, measured by oracle | 85, restated from printed counts | 100, restated from printed counts |
| `docs/parity.md` VS Code promo row | 10, unchanged | 100, restated from printed counts |
| Reference experiment names resolved with reference defaults | 3/3 | 3/3 held by the replay |
| Configuration fields the layer writes, each with a consumer | 4/4 | 4/4 proven by a change-and-observe test |
| Telemetry events carrying confirmed exposures | all, when assigned | all, held by the replay |
| Oracle comparisons printed per run | >= 200 across >= 8 families | >= 300 across >= 12 families |
| Divergences outside the ledger | 0 | 0 |
| Ranks of the execution order still open | 1 (rank 16) | 0 |

## Target Users

### Parity maintainer

- **Role:** the engineer who restates a score in `docs/parity.md` and has to
  defend the number.
- **Behaviors:** runs `cargo test -p <crate> --all-features <area>_parity_tests
  -- --nocapture`, quotes the printed per-family counts into the document, and
  reads the ledger to know what is still open.
- **Pain points:** the two rank-16 rows are the last declarative scores in the
  document, and their stated blocker is a credential claim the repository's own
  `registry.rs:527` contradicts. Defending 25 currently means defending a reason
  that is false.
- **Current workaround:** reading `vibe/core/experiments/` beside an empty
  `crates/` subtree and scoring the absence by eye.
- **Success looks like:** one command per crate prints a ledger, a per-family
  conforming count and a closing total, and both rows quote those lines.

### Vibe operator

- **Role:** the person running the `vibe` binary, who may be enrolled in a
  rollout without ever asking to be.
- **Behaviors:** sets `enable_telemetry = false` to stay out of product
  measurement, pins `active_model` and `system_prompt_id` in a TOML file, and
  expects a value they wrote to win.
- **Pain points:** none today, because nothing is enrolled. After this PRD lands
  the risk is real, which is why the gate and the precedence are part of the same
  work: a rollout that overrode a user's own configuration file would be a
  regression this port introduced, not one it inherited.
- **Current workaround:** none needed yet.
- **Success looks like:** `enable_telemetry = false` or `experiments.enable =
  false` means zero eval requests, and any value in the user or project TOML
  beats any assignment.

### Rollout owner

- **Role:** whoever operates the GrowthBook workspace behind
  `experiments.mistral.services` and needs the Rust client to answer the same way
  the Python client does.
- **Behaviors:** defines a feature keyed `vibe_cli_system_prompt`,
  `vibe_cli_managed_shell_tools` or `vibe_cli_default_routing_model`, targets it
  on the `userId` hash attribute, and reads exposures back from the datalake.
- **Pain points:** a Rust client that resolved differently, or reported an
  exposure for a forced rule, would corrupt the experiment's own analysis, and
  nothing today would detect it.
- **Current workaround:** the Rust binary is invisible to every rollout.
- **Success looks like:** the two clients produce the same variant for the same
  eval response, and only a confirmed exposure reaches telemetry.

## Research Findings

### The remote evaluation protocol (primary sources)

- The endpoint is `POST {api_host}/api/eval/{client_key}`, with a body carrying
  `attributes`, `forcedVariations`, `forcedFeatures` and `url`, all optional and
  defaulting to `{}`, `{}`, `[]` and `""`
  ([Remote Evaluation](https://docs.growthbook.io/self-host/remote-evaluation)).
  The reference posts exactly this shape with the last three left empty
  (`vibe/core/experiments/client.py:46-51`).
- **Bucketing happens on the server.** The proxy runs the evaluation and rewrites
  each feature as `{defaultValue: result.value, rules: [{force: result.value,
  tracks: [...]}]}`, scrubbing conditions and unused variations
  ([growthbook-proxy eval lib](https://github.com/growthbook/growthbook-proxy/tree/main/packages/lib/eval),
  framed in the [JS SDK docs](https://docs.growthbook.io/lib/js) as targeting
  data never being seen by the client). This is why the reference's
  `resolved_value` reads only `force` and `defaultValue` and implements no
  hashing (`vibe/core/experiments/models.py:70-74`), and it is what makes this
  part measurable with synthetic responses.
- `tracks[].result.inExperiment` means the user was actually hashed into a
  variation, as opposed to receiving a forced value, a coverage exclusion or a
  default ([Build Your Own](https://docs.growthbook.io/lib/build-your-own)).
  This is the semantic difference the reference encodes as `assignments()`
  against `config_variants()`.
- **The client key is publishable.** GrowthBook's
  [security documentation](https://docs.growthbook.io/using/security) states the
  client key is not a secret and is safe to expose client-side, with the caveat
  that it reveals the feature list, which is what encrypted payloads and remote
  evaluation exist to harden. The key the reference ships is already committed
  here (`crates/vibe-core/src/config/registry.rs:527`).
- The eval response also carries scrubbed `experiments` metadata alongside
  `features`. The reference models only `features` and ignores everything else
  through `extra="ignore"` (`models.py:77-80`), so the Rust models must ignore
  unknown fields rather than deny them, which is the opposite of the
  `deny_unknown_fields` rule `vibe-protocol` envelopes follow.
- No vendor guidance prescribes fail-open behavior for an unreachable eval
  endpoint; the SDK contract is a caller-supplied fallback
  ([feature basics](https://docs.growthbook.io/features/basics)). The reference
  chose fail-open with client-side defaults, and that choice is what parity
  reproduces.

### Best practices applied

- Measure the client, not the service: since assignment is server-side, the
  oracle feeds both implementations the same response and compares what they do
  with it. Nothing in the corpus depends on a live rollout, so a captured
  scenario stays valid after the rollout changes.
- Fail-open with a bounded timeout, never blocking the product path. The
  reference uses a 5.0 second eval timeout and a 4.0 second identity timeout, and
  runs both inside a detached task.
- Anonymous bucketing keys: the hash attribute is a SHA-256 of the API key
  truncated to 32 hex characters, so the bucketing is stable per user without the
  credential leaving the process (`vibe/core/experiments/manager.py:17-19`).

## Assumptions & Constraints

### Assumptions (to validate)

- **The eval response only ever carries pre-resolved `force` rules.** Confirmed
  by the proxy source and the vendor's framing, not by a captured production
  payload. The Rust models mirror the reference's tolerant shape, so a rule
  carrying `condition` or `coverage` instead would be read as a rule without a
  force and fall through to `defaultValue`, which is exactly what the reference
  does. Risk is therefore bounded to both implementations being wrong together,
  which is parity.
- **The client key stays publishable and stays the reference's own.** If the
  reference ever moves the key out of its committed defaults, this port follows
  the reference rather than inventing a key.
- **`experiments.enable` and `enable_telemetry` are the only gates.** Both are
  read from the merged configuration on every decision, as the reference reads
  them (`vibe/core/experiments/session.py:33,74`).
- **`identity/read` stays unrouted.** The organization identity is fetched by a
  `vibe-core` gateway for the `organizationId` attribute; the app-server method
  of the same name remains declared and unrouted, as `docs/parity.md:142`
  records. This PRD does not close that row.

### Hard Constraints

- `NOTICE` forbids copying reference source, prompts or message text. The two
  promo sentences and every reference log line enter the corpus as a length plus
  a SHA-256, and every counterpart here is written originally.
- The pin lives in exactly two places, `vibe_core::parity::REFERENCE_COMMIT` and
  `EXPECTED_COMMIT` in `scripts/parity/pin.py`. A new parity test calls
  `vibe_core::parity::reference_root` rather than spelling a path.
- A missing or off-pin reference checkout must never fail `cargo test`: both
  replays run unconditionally against their committed corpus and only the
  recapture probe skips.
- The dependency layering in `[workspace.metadata.vibe] dependency-layers`
  holds: the experiments engine, its models, the configuration layer and the
  identity gateway belong to `vibe-core`; the session lifecycle belongs to
  `vibe-app-server`; the promo belongs to `vibe-cli` and stays an adapter.
- `unsafe_code` is forbidden workspace-wide; `panic`, `unimplemented` and
  `dbg_macro` are denied in non-test code.
- `crates/vibe-core/src/parity/ledger_tests.rs` reads the accepted-divergences
  table of `docs/parity.md` and fails when a row names an artifact the repository
  no longer holds, so every divergence row this PRD adds or removes must name a
  real symbol.
- No credential value may appear in a corpus, a log line, a span or an eval
  payload. The only derivative of the API key that leaves the process is the
  truncated SHA-256.

## Quality Gates

These commands must pass for every user story, run from the workspace root:

- `cargo fmt --all -- --check` - formatting
- `cargo check --workspace --all-targets --all-features` - compilation
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` - lints
- `cargo test --workspace --all-features` - the full suite, never filtered to the
  module under edit, because the configuration surface corpus is read by tests in
  more than one crate and this PRD changes what that replay expects

For stories that touch a parity corpus, additionally:

- `cargo test -p vibe-core --all-features experiments_parity_tests -- --nocapture` -
  prints the ledger, one conforming count per family and the closing total that
  `docs/parity.md` quotes
- `cargo test -p vibe-cli --all-features promo_parity_tests -- --nocapture` -
  the same, for the promo families

## Reference Map

Every file an implementer opens before writing Rust, at the pinned commit
`b78b451`. Paths use the Linux canonical spelling
`/home/arthur/dev/mistral-vibe/` and resolve against whichever checkout is local,
through `VIBE_REFERENCE` or `--reference`; Rust tests reach the same root through
`vibe_core::parity::reference_root`. Each story names its own anchor; this is the
whole surface in one place. Reading these is required by `AGENTS.md`, and
grepping them does not replace opening the declaration they point at.

The reference splits this rank across three subtrees that are never read in
isolation: [vibe/core/experiments/](/home/arthur/dev/mistral-vibe/vibe/core/experiments)
holds the engine, [vibe/core/config/layers/growthbook.py](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/growthbook.py)
holds the only place a variant becomes a configuration value, and
[vibe/cli/vscode_extension_promo/](/home/arthur/dev/mistral-vibe/vibe/cli/vscode_extension_promo)
holds the promo. 751 lines of production code, plus the 153 lines of identity the
attributes depend on. Open the engine before the layer: a mapping read without
`config_variants` reads as arbitrary, since the layer is fed by a method that
deliberately admits more than telemetry does.

### The experiments engine (7 files, 497 lines)

- [vibe/core/experiments/active.py](/home/arthur/dev/mistral-vibe/vibe/core/experiments/active.py),
  21 lines: `ExperimentName` (7) for the three names, `DEFAULT_VARIANTS` (13) for
  the three client-side defaults including the `"{}"` string, and the assertion
  (19) that ties them together.
- [vibe/core/experiments/_constants.py](/home/arthur/dev/mistral-vibe/vibe/core/experiments/_constants.py),
  15 lines: `GROWTHBOOK_EVAL_PATH_TEMPLATE` (5),
  `EVAL_REQUEST_TIMEOUT_SECONDS` (7), `build_eval_url` (10) for the strip, the
  trailing-slash removal and the empty-input null.
- [vibe/core/experiments/models.py](/home/arthur/dev/mistral-vibe/vibe/core/experiments/models.py),
  80 lines: `ExperimentAttributes` (10) for the nine attributes and the docstring
  explaining the hash attribute, `TrackedExperiment` (32),
  `TrackedExperimentResult` (38) for the seven result fields,
  `TrackData` (50), `FeatureRule` (57), `FeatureDefinition` (64) with
  `resolved_value` (70), `EvalResponse` (77). Every model carries
  `extra="ignore"`.
- [vibe/core/experiments/client.py](/home/arthur/dev/mistral-vibe/vibe/core/experiments/client.py),
  81 lines: `RemoteEvalClient` (17) with the fail-open contract in its docstring,
  `from_settings` (30), the lazy `_client` (34) with the timeout and the SSL
  context, `evaluate` (43) for the payload and the four failure branches,
  `aclose` (78).
- [vibe/core/experiments/manager.py](/home/arthur/dev/mistral-vibe/vibe/core/experiments/manager.py),
  134 lines: `hash_api_key` (17), `ExperimentManager` (22), `initialize` (27),
  `hydrate` (34), `export_state` (38), `_filter_to_known_experiments` (41),
  `_log_resolved_variants` (48), `get_variant_or_none` (57) with its JSON
  serialization, `get_variant` (74), `config_variants` (79),
  `assignments` (95), `_forced_variant_or_none` (110), `_variant_label` (119) for
  the four-level fallback, `aclose` (133).
- [vibe/core/experiments/session.py](/home/arthur/dev/mistral-vibe/vibe/core/experiments/session.py),
  108 lines: `EXPERIMENT_IDENTITY_TIMEOUT_S` (20), `initialize_experiments` (25)
  for the two gates, the provider resolution, the identity fetch and the return
  value that decides a refresh, `hydrate_experiments_from_session` (68),
  `_build_attributes` (83) including the `custom_system_prompt` comparison
  against the schema default.
- [vibe/core/experiments/__init__.py](/home/arthur/dev/mistral-vibe/vibe/core/experiments/__init__.py),
  58 lines: the lazy export surface, which names the public API.

### The configuration layer (1 file, 101 lines) and its seat

- [vibe/core/config/layers/growthbook.py](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/growthbook.py):
  `_map_system_prompt_variant` (16) for the load-or-drop validation,
  `_map_default_routing_model` (24), `_map_routed_model_config` (35) for the
  re-encode, `GROWTHBOOK_CONFIG_MAPPINGS` (48) for the three experiments and four
  fields, `GrowthbookLayer` (65), `set_variants` (72) with the comment naming the
  config/telemetry split, `_check_trust` (76), `_build_config_snapshot` (79) for
  the empty-snapshot branches and the fingerprint, `_save_to_store` (100) for the
  read-only refusal.
- [vibe/core/config/default_orchestrator.py:25-79](/home/arthur/dev/mistral-vibe/vibe/core/config/default_orchestrator.py):
  the docstring (33-41) states the precedence in words and the list (65-76)
  states it in code. This is the anchor for the reseating.
- [vibe/core/config/models.py:56-61](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py):
  `ExperimentsConfig`, already reproduced at
  `crates/vibe-core/src/config/registry.rs:401-408,525-530`.
- [vibe/core/config/vibe_schema.py:240-249](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py)
  for `routed_default_model` and `routed_model_config` with their comments naming
  the layer, `:469-471` for the `experiments` field, `:473-479` for
  `resolve_default_model_alias`, which is the resolution that gives
  `routed_default_model` an effect.

### The session lifecycle (5 call sites)

- [vibe/core/agent_loop/_loop.py:474-481](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py)
  for the manager construction and the fork hydration, `:552` for the telemetry
  getter, `:679-683` for `_sync_growthbook_layer_variants`, `:889-918` for the
  detached task and the two refresh paths, `:946-956` for the cancel-and-close
  order, `:2681` for the re-initialization on a new session.
- [vibe/app_server/_handler.py:460](/home/arthur/dev/mistral-vibe/vibe/app_server/_handler.py)
  and [vibe/app_server/server.py:375](/home/arthur/dev/mistral-vibe/vibe/app_server/server.py)
  for the two places initialization starts,
  [vibe/app_server/_runtime.py:138,160,295,420](/home/arthur/dev/mistral-vibe/vibe/app_server/_runtime.py)
  for the state passed into a replacement runtime and the hydration on resume.
- [vibe/core/session/session_logger.py:488-497](/home/arthur/dev/mistral-vibe/vibe/core/session/session_logger.py):
  `persist_experiments`, the only writer of the metadata field.
- [vibe/core/identity.py](/home/arthur/dev/mistral-vibe/vibe/core/identity.py),
  102 lines: `_IDENTITY_PATH` (11), the two strict models (14-30), the two error
  types (32-37), `HttpIdentityGateway.read` (45) for the status mapping, and
  `fetch_identity` (73).
  [vibe/core/identity_cache.py](/home/arthur/dev/mistral-vibe/vibe/core/identity_cache.py),
  51 lines: `IdentityCache` (8) with the single-flight `resolve` (24) and the
  non-fetching `peek` (49).
- [vibe/core/telemetry/send.py:80-87,141-151](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py)
  for the getter and its use, [types.py:61](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/types.py)
  and [build_metadata.py:24](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/build_metadata.py)
  for the field, all three already reproduced here.

### The VS Code promo (4 files, 90 lines) and its display

- [vibe/cli/vscode_extension_promo/__init__.py](/home/arthur/dev/mistral-vibe/vibe/cli/vscode_extension_promo/__init__.py),
  44 lines: `MAX_SHOWN_COUNT` (25), `PROMO_START` (26), `should_show_promo` (29)
  for the three-branch predicate, `VscodeExtensionPromo` (40).
- [vibe/cli/vscode_extension_promo/_port.py](/home/arthur/dev/mistral-vibe/vibe/cli/vscode_extension_promo/_port.py),
  14 lines: `VscodeExtensionPromoState` (7), the repository protocol (12).
- [vibe/cli/vscode_extension_promo/adapters/filesystem_repository.py](/home/arthur/dev/mistral-vibe/vibe/cli/vscode_extension_promo/adapters/filesystem_repository.py),
  42 lines: `_CACHE_SECTION` (13), `get` (22) with the non-integer rejection,
  `set` (31), `_read_section` (38).
- [vibe/cli/textual_ui/app.py:234](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py)
  for the terminal family set, `:290-291` for the predicate, `:576-581` for the
  three-condition gate computed once at construction, `:4054-4069` for the
  counter increment and its swallowed failure, `:4071-4105` for the whats-new
  path and its suffix, `:4130-4139` for the standalone path and its mount
  position, `:4403-4406` for the construction and the eager state read.
- [vibe/cli/textual_ui/widgets/messages.py:426-431](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/messages.py)
  for the URI, the link label and the two sentences,`:455-462` for the widget.
  The URI and the label are identifiers and are reproduced; both sentences are
  prose and are not.
- [vibe/utils/cache_store.py](/home/arthur/dev/mistral-vibe/vibe/utils/cache_store.py),
  67 lines: the section protocol this port already implements as
  `UpdateCacheStore`.

### Reference tests, as the scenario inventory (1 703 lines)

[tests/core/experiments/](/home/arthur/dev/mistral-vibe/tests/core/experiments)
holds 1 161 lines across 6 files and
[tests/core/config/test_growthbook_layer.py](/home/arthur/dev/mistral-vibe/tests/core/config/test_growthbook_layer.py)
542 more. They are read as a case list for the capture, never copied: 38 manager
cases, 13 layer-mapping cases, 12 precedence cases, 10 session-gate cases, 6
client cases, 5 resume cases and 3 telemetry-integration cases.

## Epics & User Stories

### EP-001: The experiments oracle and its corpus

Build the differential instrument before any surface change, so every later story
is measured rather than asserted. The oracle drives the reference's own eval
client, manager, configuration layer, session helpers and promo repository over
inputs the script authors, with no network and no credentials.

**Definition of Done:** `scripts/parity/experiments.py` captures every family
below into `crates/vibe-core/tests/experiments/corpus.json` and
`crates/vibe-cli/tests/promo/corpus.json`, both Rust replays read their corpus
unconditionally, print a per-family conforming count and a closing total, fail on
a divergence outside their ledger and on a stale ledger entry, and the recapture
probe is the only part that skips when the checkout is absent or off-pin.

#### US-001: Capture the eval client and the manager
**Description:** As a parity maintainer, I want the reference's URL construction,
eval payload, failure handling and variant resolution captured into a committed
corpus so that the engine is compared against the reference's own answers rather
than against a reading of its source.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None

**Reference:** [vibe/core/experiments/_constants.py:5-15](/home/arthur/dev/mistral-vibe/vibe/core/experiments/_constants.py) for the URL rules, [client.py:43-81](/home/arthur/dev/mistral-vibe/vibe/core/experiments/client.py) for the payload and the four failure branches, [manager.py:17-134](/home/arthur/dev/mistral-vibe/vibe/core/experiments/manager.py) for every resolution method, [models.py:64-80](/home/arthur/dev/mistral-vibe/vibe/core/experiments/models.py) for `resolved_value` and the tolerant models, and [active.py:7-21](/home/arthur/dev/mistral-vibe/vibe/core/experiments/active.py) for the names and defaults. The 44 cases to cover are in [tests/core/experiments/test_client.py](/home/arthur/dev/mistral-vibe/tests/core/experiments/test_client.py) and [test_manager.py](/home/arthur/dev/mistral-vibe/tests/core/experiments/test_manager.py). The local pattern to follow is `scripts/parity/voice.py` for intercepting one call before the connection, `scripts/parity/setup_auth.py` for the socket guard and the credential sentinels, and `scripts/parity/config_surface.py` for the interpreter re-exec

**Acceptance Criteria:**
- [ ] Given the pinned checkout, when `scripts/parity/experiments.py` runs, then
      it re-executes itself with the reference interpreter, accepts
      `--reference`, and reads `VIBE_REFERENCE` when the flag is absent
- [ ] Given api_host and client_key inputs that vary in trailing slashes,
      surrounding whitespace and emptiness, when the capture runs, then the
      `evalUrl` family records the resolved URL or the null for each
- [ ] Given a manager driven over an intercepted transport, when `evaluate` runs,
      then the `evalRequest` family records the method, the resolved URL, the
      header names, the four payload keys and the configured timeout, with no
      credential value present
- [ ] Given each of a connection error, a 4xx, a 5xx, a non-JSON body and a body
      that fails validation, when the capture runs, then the `evalFailures`
      family records that the call returned no state and that the manager was
      left unchanged
- [ ] Given eval responses that vary `defaultValue`, a forced rule, several
      rules and an unknown feature key, when the capture runs, then the
      `featureResolution` and `variantResolution` families record the resolved
      value, its JSON serialization for object and array arms, and the filtered
      feature set
- [ ] Given responses carrying tracks with `inExperiment` true, false and
      absent, when the capture runs, then the `configVariants` family records
      `config_variants` and `assignments` side by side for the same input, and
      the `variantLabels` family records the four-level fallback including the
      empty-string terminal
- [ ] Given any attempt to open a socket during the capture, when the guard
      fires, then the run fails rather than recording a network-dependent answer

#### US-002: Capture the layer, the session and the promo
**Description:** As a parity maintainer, I want the reference's variant-to-field
mapping, layer precedence, session gating and promo predicate captured so that
every surface this PRD ports is measured against observable answers.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-001

**Reference:** [vibe/core/config/layers/growthbook.py:16-101](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/growthbook.py) for the three mappers and the snapshot branches, [default_orchestrator.py:25-79](/home/arthur/dev/mistral-vibe/vibe/core/config/default_orchestrator.py) for the precedence the capture drives a real orchestrator over, [experiments/session.py:25-108](/home/arthur/dev/mistral-vibe/vibe/core/experiments/session.py) for the two gates and the attribute construction, and [vscode_extension_promo/__init__.py:25-44](/home/arthur/dev/mistral-vibe/vibe/cli/vscode_extension_promo/__init__.py) with [adapters/filesystem_repository.py:13-42](/home/arthur/dev/mistral-vibe/vibe/cli/vscode_extension_promo/adapters/filesystem_repository.py) for the promo. The 40 cases are in [tests/core/config/test_growthbook_layer.py](/home/arthur/dev/mistral-vibe/tests/core/config/test_growthbook_layer.py) and [tests/core/experiments/test_session_helpers.py](/home/arthur/dev/mistral-vibe/tests/core/experiments/test_session_helpers.py)

**Acceptance Criteria:**
- [ ] Given variants for each of the three experiments, including an unknown
      prompt id, a routing payload without `active_model` and a payload whose
      `model_config` is absent, when the capture runs, then the `configMapping`
      family records the field set the layer produces for each
- [ ] Given a real reference orchestrator with a user TOML, a project TOML,
      environment variables and runtime overrides, when a variant is set, then
      the `layerPrecedence` family records the effective value for each pairing,
      including the three cases where the TOML wins
- [ ] Given configurations that vary `enable_telemetry`, `experiments.enable`,
      the presence of a Mistral provider and the reachability of identity, when
      the capture runs, then the `sessionGates` family records the boolean each
      helper returns and whether an eval request was attempted
- [ ] Given launch contexts that vary entrypoint, client, terminal emulator and
      `system_prompt_id`, when the capture runs, then the `attributes` family
      records all nine attribute keys with their values, with `userId` recorded
      as a digest and never as a key
- [ ] Given promo states of absent, zero, nine, ten and a non-integer, crossed
      with instants before and after the start, when the capture runs, then the
      `promoPredicate` family records the decision for each and the `promoState`
      family records what the repository reads back
- [ ] Given the two reference promo sentences, when the capture runs, then the
      `promoProse` family records only a byte length and a SHA-256 for each
- [ ] Given a reference checkout that is absent or off-pin, when the capture is
      invoked, then it reports why and exits without writing a partial corpus

#### US-003: Replay both corpora from Rust with a ledger
**Description:** As a parity maintainer, I want one command per crate that
replays the committed corpus and prints per-family counts so that the two
`docs/parity.md` rows quote a reproducible number instead of a reading.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001, US-002

**Reference:** the local pattern is `crates/vibe-core/src/telemetry/telemetry_parity_tests.rs` for the ledger, the per-family counts and the closing total, and `crates/vibe-cli/src/tui/completion/path/autocompletion_parity_tests.rs` for a ledger whose entries fail once they go stale

**Acceptance Criteria:**
- [ ] Given the committed corpus, when `cargo test -p vibe-core --all-features
      experiments_parity_tests -- --nocapture` runs, then it replays every family
      unconditionally and prints one conforming count per family plus a closing
      total
- [ ] Given the committed promo corpus, when `cargo test -p vibe-cli
      --all-features promo_parity_tests -- --nocapture` runs, then it does the
      same for the promo families
- [ ] Given a divergence outside the ledger, when either replay runs, then it
      fails and names the family, the case and the two values
- [ ] Given a ledger entry whose case now conforms, when the replay runs, then it
      fails as stale rather than passing quietly
- [ ] Given a prose digest, when the replay compares it, then it asserts
      permanent inequality against this port's own sentence and fails if one ever
      matches
- [ ] Given an absent or off-pin reference checkout, when the suite runs, then
      both replays still run against the committed corpora and only the recapture
      probe skips, with the skip reason printed

---

### EP-002: The configuration fields the layer writes

Remove the two blockers that would otherwise make the GrowthBook layer a
schema-only feature: three undeclared fields and a pinned `active_model` that
makes routed defaults unreachable. Each field arrives with the resolution that
gives it an effect, which is the standard `docs/parity.md` states for its own
configuration row.

**Definition of Done:** `UNDECLARED_FIELDS` loses its three experiment-target
entries, `UNPINNED_ACTIVE_MODEL` describes the shipped document, and each of the
four fields has a test that changes the value and observes the change in a real
execution path.

#### US-004: Declare the routed model fields and resolve them on read
**Description:** As a Vibe operator with no pinned model, I want the routed
default to select my model so that a routing rollout has an observable effect
instead of writing a key nothing reads.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-003

**Reference:** [vibe/core/config/vibe_schema.py:240-249](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py) for the two fields and their merge strategies, `:473-479` for `resolve_default_model_alias` and its three-step fallback, and `_coerce_routed_model_config` for the string-to-model coercion. Locally, `crates/vibe-core/src/config/registry.rs:881` shows the `FieldSpec` shape and `surface_parity_tests.rs:50-85` holds the two exception lists this story shortens

**Acceptance Criteria:**
- [ ] Given the registry, when the schema is published, then
      `routed_default_model` and `routed_model_config` are declared with the
      reference's merge strategies and defaults, and both leave
      `UNDECLARED_FIELDS`
- [ ] Given an unpinned `active_model` and a `routed_default_model` naming a
      configured model, when the configuration loads, then the active model is
      the routed one
- [ ] Given an unpinned `active_model` and a `routed_default_model` naming a
      model that is not configured, when the configuration loads, then the
      default alias is selected rather than an error being raised
- [ ] Given a `routed_model_config` carrying a full model definition, when the
      configuration loads, then the definition is merged into the model map under
      the routed alias
- [ ] Given a `routed_model_config` that is not a valid model definition, when
      the configuration loads, then the value is dropped and the load still
      succeeds with a recorded warning
- [ ] Given the config-surface replay, when it runs, then the two fields are
      compared against the reference declaration with 0 divergence

#### US-005: Unpin `active_model` and gate the managed shell family
**Description:** As a rollout owner, I want an unpinned active model and a
managed-shell toggle so that the two experiments that target them can change what
the binary does.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-004

**Reference:** [vibe/core/config/vibe_schema.py:240](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py) for the `UNPINNED_ACTIVE_MODEL` sentinel, and the `managed_shell_tools_enabled` field with its consumer in the tool manager. Locally, `crates/vibe-core/src/tools/shell.rs` holds the managed families this port already publishes, and `crates/vibe-core/src/config/surface_parity_tests.rs:75-85` holds the sentinel constants

**Acceptance Criteria:**
- [ ] Given a fresh installation, when the configuration document is written,
      then `active_model` carries the reference's empty sentinel and the
      effective model is unchanged from today's pinned alias
- [ ] Given a user who pinned `active_model`, when a routed default is present,
      then the pinned value wins
- [ ] Given `managed_shell_tools_enabled = true`, when the tool surface is
      registered, then the managed shell family is published in place of the
      one-shot `bash`, and the reverse holds for false
- [ ] Given `managed_shell_tools_enabled` absent, when the tool surface is
      registered, then the legacy family is published, matching the reference
      default variant
- [ ] Given the config-surface replay, when it runs, then `UNDECLARED_FIELDS`
      contains only `show_greeting` and the sentinel assertion describes the
      shipped document

---

### EP-003: The experiments engine

Port the engine itself: the models the eval response deserializes into, the
client that fetches it, and the manager that decides what a variant means. All
three are provider-neutral contracts and belong to `vibe-core`.

**Definition of Done:** the `evalUrl`, `evalRequest`, `evalFailures`,
`featureResolution`, `variantResolution`, `configVariants` and `variantLabels`
families all replay conforming, and no eval request is issued when either gate is
off.

#### US-006: The eval payload models and their resolution
**Description:** As a rollout owner, I want the eval response deserialized and
resolved exactly as the reference resolves it so that both clients read the same
variant from the same payload.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-003

**Reference:** [vibe/core/experiments/models.py:32-80](/home/arthur/dev/mistral-vibe/vibe/core/experiments/models.py) for the six models, their `extra="ignore"` configuration and `resolved_value`, and [active.py:7-21](/home/arthur/dev/mistral-vibe/vibe/core/experiments/active.py) for the names and defaults

**Acceptance Criteria:**
- [ ] Given a response carrying fields the models do not declare, including the
      proxy's `experiments` block, when it is deserialized, then the unknown
      fields are ignored rather than rejected
- [ ] Given a feature whose first rule carries a force, when it is resolved, then
      the forced value wins over `defaultValue`
- [ ] Given a feature whose rules carry no force, when it is resolved, then
      `defaultValue` is returned, including when it is null
- [ ] Given a malformed response, when it is deserialized, then the failure is
      returned as an error rather than a panic, and the caller keeps its previous
      state
- [ ] Given the three experiment names, when defaults are read, then they carry
      the reference's `cli`, `legacy` and `{}` values and a compile-time or test
      assertion fails if a name is added without one

#### US-007: The remote eval client
**Description:** As a Vibe operator, I want a rollout lookup that never blocks or
breaks my session so that an unreachable experiment service costs nothing.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-006

**Reference:** [vibe/core/experiments/client.py:17-81](/home/arthur/dev/mistral-vibe/vibe/core/experiments/client.py) for the fail-open contract, the lazy client, the payload and the four branches, and [_constants.py:5-15](/home/arthur/dev/mistral-vibe/vibe/core/experiments/_constants.py) for the URL and the timeout. Locally, `crates/vibe-core/src/telemetry.rs:1070-1085` and `crates/vibe-core/src/auth/sign_in_http.rs:229` show the `reqwest` client construction this port already uses

**Acceptance Criteria:**
- [ ] Given an api_host and a client_key, when the client is built, then the URL
      is `{api_host}/api/eval/{client_key}` after trimming whitespace and one
      trailing slash
- [ ] Given an empty api_host or an empty client_key, when `evaluate` is called,
      then no request is issued and no state is returned
- [ ] Given a configured client, when `evaluate` is called, then the posted body
      carries the attributes plus the three empty defaults, and the request uses
      a 5.0 second timeout
- [ ] Given a connection error, a status at or above 400, a non-JSON body or a
      body that fails deserialization, when `evaluate` is called, then no state
      is returned, a warning is logged naming the cause, and nothing propagates
      to the caller
- [ ] Given a client that was never used, when it is closed, then closing is a
      no-op and closing twice does not fail

#### US-008: The experiment manager
**Description:** As a rollout owner, I want confirmed exposures separated from
forced assignments so that telemetry reports enrollment and configuration honors
a force, exactly as the reference splits them.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-006

**Reference:** [vibe/core/experiments/manager.py:17-134](/home/arthur/dev/mistral-vibe/vibe/core/experiments/manager.py) in full: the hash, the filter, the two variant readers, the two exporters and the label fallback. The 38 cases are in [tests/core/experiments/test_manager.py](/home/arthur/dev/mistral-vibe/tests/core/experiments/test_manager.py)

**Acceptance Criteria:**
- [ ] Given an API key, when the bucketing key is derived, then it is the
      SHA-256 hex digest truncated to 32 characters, is stable across calls, and
      differs for a different key
- [ ] Given a response carrying features this build does not know, when it is
      taken in through either initialization or hydration, then the unknown keys
      are dropped
- [ ] Given no response at all, when a variant is read, then the client-side
      default is returned
- [ ] Given a feature resolving to an object or an array, when a variant is read,
      then its JSON serialization is returned rather than the default
- [ ] Given a track whose result is not in the experiment, or whose flag is
      absent, when exposures are read, then the feature is excluded from
      telemetry assignments but a forced rule still reaches configuration
      variants
- [ ] Given a confirmed track, when its label is computed, then the four-level
      fallback is applied in order and an exhausted fallback yields an empty
      label that is not reported
- [ ] Given a second initialization, when it succeeds, then it replaces the
      previous state rather than merging into it

---

### EP-004: The GrowthBook configuration layer

Turn variants into configuration values at the precedence the reference gives
them, which is below every file a human wrote.

**Definition of Done:** the `configMapping` and `layerPrecedence` families replay
conforming, and a value written in a user or project TOML beats an assignment for
all four fields.

#### US-009: Map variants onto configuration fields
**Description:** As a rollout owner, I want a variant to become a configuration
value only when it is valid so that a malformed or unknown variant leaves the
configuration untouched.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-005, US-008

**Reference:** [vibe/core/config/layers/growthbook.py:16-101](/home/arthur/dev/mistral-vibe/vibe/core/config/layers/growthbook.py) for the three mappers, the mapping table, the two empty-snapshot branches, the fingerprint and the read-only refusal. Locally, `crates/vibe-core/src/prompt.rs:251` is the `load_system_prompt` counterpart the prompt mapper validates against

**Acceptance Criteria:**
- [ ] Given a system-prompt variant naming a resolvable prompt, when the layer
      builds its snapshot, then `system_prompt_id` carries it
- [ ] Given a system-prompt variant naming a prompt that does not resolve, when
      the layer builds its snapshot, then no field is written and the load still
      succeeds
- [ ] Given a routing variant carrying a non-empty `active_model`, when the layer
      builds its snapshot, then `routed_default_model` carries it, and
      `routed_model_config` carries the re-encoded `model_config` when present
- [ ] Given a routing variant that is not JSON, is not an object, or carries an
      empty `active_model`, when the layer builds its snapshot, then neither
      routed field is written
- [ ] Given a managed-shell variant equal to the managed arm, when the layer
      builds its snapshot, then `managed_shell_tools_enabled` is true; given any
      other value, nothing is written
- [ ] Given no variants at all, or variants that map to nothing, when the layer
      builds its snapshot, then the snapshot is empty and carries no fingerprint
- [ ] Given an attempt to persist through the layer, when it is made, then it is
      refused as read-only

#### US-010: Seat the layer below the selected TOML
**Description:** As a Vibe operator, I want any value I wrote in my own
configuration file to beat any experiment assignment so that enrollment never
silently overrides my choice.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-009

**Reference:** [vibe/core/config/default_orchestrator.py:33-76](/home/arthur/dev/mistral-vibe/vibe/core/config/default_orchestrator.py), whose docstring states the precedence and whose list implements it. Locally, `crates/vibe-core/src/config.rs:644-677` composes the layers in order and `crates/vibe-core/src/config/discovery_tests.rs:64-73` asserts the order that this story changes

**Acceptance Criteria:**
- [ ] Given the layer list, when the configuration composes, then `Experiments`
      is merged before `SelectedToml` and after `Discovered`
- [ ] Given a user TOML and an assignment that target the same field, when the
      configuration loads, then the TOML value is effective
- [ ] Given a project TOML that disables the managed shell and an assignment that
      enables it, when the configuration loads, then the shell stays legacy
- [ ] Given an environment variable and a runtime override on the same field,
      when the configuration loads, then both still beat the assignment
- [ ] Given only an assignment, when the configuration loads, then the assigned
      value is effective and its provenance reports the experiments layer
- [ ] Given the layer order test, when it runs, then it asserts the new order and
      fails on any future reordering

---

### EP-005: Session lifecycle and telemetry exposure

Wire the engine into a real session: fetch the attributes, run the lookup off the
startup path, persist what it resolved, hydrate it on resume and fork, and let
telemetry report enrollment.

**Definition of Done:** the `sessionGates` and `attributes` families replay
conforming, a resumed session resolves the same variants without a second
request, and a telemetry event carries the confirmed exposures.

#### US-011: Resolve the caller's organization identity
**Description:** As a rollout owner, I want assignments to be targetable by
organization so that a rollout can be scoped to a workspace rather than only to a
hashed user.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-003

**Reference:** [vibe/core/identity.py:11-102](/home/arthur/dev/mistral-vibe/vibe/core/identity.py) for the path, the two strict models, the status mapping and the two error types, and [vibe/core/identity_cache.py:8-51](/home/arthur/dev/mistral-vibe/vibe/core/identity_cache.py) for the single-flight cache keyed by base URL and key, whose failures are not cached

**Acceptance Criteria:**
- [ ] Given a base URL and a credential, when identity is fetched, then the
      request is `GET {base}/users/me` with a bearer header and the configured
      timeout
- [ ] Given a 401 or a 403, when identity is fetched, then the unauthorized
      outcome is distinguished from an unavailable one
- [ ] Given a transport failure, a non-success status or a body that fails
      validation, when identity is fetched, then no identity is returned and the
      caller proceeds without an organization
- [ ] Given two concurrent callers with the same base URL and key, when both
      resolve, then exactly one request is issued
- [ ] Given a failed fetch, when a later caller resolves, then a new request is
      issued because failures are not cached

#### US-012: Initialize, persist and hydrate a session's experiments
**Description:** As a Vibe operator, I want enrollment resolved once per session
and reused on resume so that a session's behavior does not change under me and
startup is never delayed.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-008, US-010, US-011

**Reference:** [vibe/core/experiments/session.py:25-108](/home/arthur/dev/mistral-vibe/vibe/core/experiments/session.py) for both helpers and the attribute builder, [vibe/core/agent_loop/_loop.py:889-918](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py) for the detached task and the refresh pair, `:946-956` for the cancel-then-close order, [vibe/app_server/_runtime.py:295,420](/home/arthur/dev/mistral-vibe/vibe/app_server/_runtime.py) for resume and fork, and [session_logger.py:488-497](/home/arthur/dev/mistral-vibe/vibe/core/session/session_logger.py) for the persistence. Locally, `crates/vibe-core/src/storage.rs:48-49,667` already holds the metadata field and its fork inheritance

**Acceptance Criteria:**
- [ ] Given `enable_telemetry = false` or `experiments.enable = false`, when a
      session starts, then no eval request and no identity request are issued
- [ ] Given no Mistral provider or no resolvable credential, when a session
      starts, then no request is issued and the session proceeds on defaults
- [ ] Given a successful lookup, when it completes, then the state is persisted
      into the session metadata under the reference's key and the configuration
      and system prompt are refreshed once
- [ ] Given a failed lookup, when it completes, then nothing is persisted, no
      refresh occurs, and the session keeps its default variants
- [ ] Given a session resumed from metadata carrying a state, when it starts,
      then the manager is hydrated from disk and no eval request is issued
- [ ] Given a forked session, when it starts, then it inherits the parent's
      resolved state rather than issuing its own lookup
- [ ] Given a session that closes while a lookup is in flight, when it closes,
      then the task is cancelled before the client is closed and shutdown does
      not block beyond the configured timeout

#### US-013: Publish confirmed exposures on every telemetry event
**Description:** As a rollout owner, I want every product event to carry the
experiments the user was actually enrolled in so that a rollout can be analyzed
against real behavior.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-012

**Reference:** [vibe/core/agent_loop/_loop.py:552](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py) for the getter the client is built with, and [vibe/core/telemetry/send.py:141-151](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py) for where it is read. Locally, `crates/vibe-core/src/telemetry.rs:400-425` already carries the field and its empty-map omission, and `crates/vibe-cli/src/lib.rs:754-763` and `crates/vibe-acp/src/main.rs:368` are the two producers that leave it defaulted

**Acceptance Criteria:**
- [ ] Given a session with confirmed exposures, when any event is sent, then its
      metadata carries the experiment map with the resolved labels
- [ ] Given a session with no confirmed exposure, when any event is sent, then
      the field is absent rather than an empty object
- [ ] Given a forced assignment with no confirmed exposure, when any event is
      sent, then the field is absent, because a force is not an enrollment
- [ ] Given a request-metadata event, when it is sent, then the field stays
      unset, matching the reference builder
- [ ] Given the telemetry replay, when it runs, then the divergence row recording
      the permanently empty field is removed and its ledger entry fails as stale
      if left behind

---

### EP-006: The VS Code extension promo

Port the promo whole: its ceiling, its start instant, its terminal gate, its
persisted counter and both display paths, with original prose in place of the two
reference sentences.

**Definition of Done:** the `promoConstants`, `promoPredicate`, `promoState` and
`promoProse` families replay conforming, and the promo is shown at most ten times
per installation and never outside a VS Code family terminal.

#### US-014: The promo state and its predicate
**Description:** As a Vibe operator, I want the promo to stop after a fixed
number of appearances so that a suggestion never becomes a recurring
interruption.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-003

**Reference:** [vibe/cli/vscode_extension_promo/__init__.py:25-37](/home/arthur/dev/mistral-vibe/vibe/cli/vscode_extension_promo/__init__.py) for the ceiling, the start instant and the three-branch predicate, [_port.py:7-14](/home/arthur/dev/mistral-vibe/vibe/cli/vscode_extension_promo/_port.py) for the state and the port, and [adapters/filesystem_repository.py:13-42](/home/arthur/dev/mistral-vibe/vibe/cli/vscode_extension_promo/adapters/filesystem_repository.py) for the cache section and the non-integer rejection. Locally, `crates/vibe-core/src/updates.rs:295-375` already reads and writes a named section of the same `cache.toml` while preserving its siblings

**Acceptance Criteria:**
- [ ] Given an instant before the start, when the predicate runs, then the promo
      is withheld regardless of state
- [ ] Given no stored state and an instant after the start, when the predicate
      runs, then the promo is allowed
- [ ] Given a stored count below the ceiling, when the predicate runs, then the
      promo is allowed; at or above the ceiling it is withheld
- [ ] Given a cache file whose promo section holds a non-integer count, when the
      state is read, then it reads as absent rather than failing the read
- [ ] Given an unreadable or malformed cache file, when the state is read, then
      it reads as absent and the binary starts normally
- [ ] Given a write of the promo section, when it completes, then the update
      cache section of the same file is preserved unchanged

#### US-015: Show the promo and record the exposure
**Description:** As a VS Code user running the CLI, I want to learn once that a
richer editor surface exists so that I can choose it, without the message
following me into other terminals.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-014

**Reference:** [vibe/cli/textual_ui/app.py:576-581](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py) for the three-condition gate evaluated once, `:4071-4105` for the whats-new suffix path and the deferred counter write, `:4130-4139` for the standalone path, `:4054-4069` for the increment whose failure is logged and swallowed, and [widgets/messages.py:426-431](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/messages.py) for the URI and the label, whose sentences are not reproduced. Locally, `crates/vibe-cli/src/tui/mod.rs:991-1008` is the counterpart display point and `crates/vibe-core/src/telemetry.rs:540-575` already classifies the three VS Code family terminals

**Acceptance Criteria:**
- [ ] Given a terminal outside the VS Code family, when the session starts, then
      no promo is shown and no counter is written
- [ ] Given a VS Code, Insiders or Cursor terminal with release notes to show,
      when the session starts, then the promo is appended to the notes and the
      counter is incremented once
- [ ] Given the same terminal with no release notes to show, when the session
      starts, then the standalone promo is shown and the counter is incremented
      once
- [ ] Given a counter write that fails, when the promo is shown, then the failure
      is reported as a diagnostic and the session continues
- [ ] Given the promo text this port ships, when the prose replay runs, then it
      differs from both reference digests and is not empty
- [ ] Given a session where the promo is withheld, when it starts, then the
      counter is unchanged

---

### EP-007: Score restatement and the divergence ledger

Close the rank in the document the same way it is closed in code: restate both
rows from printed counts, retire the two divergence rows this PRD falsifies, and
record what stays out of reach.

**Definition of Done:** `docs/parity.md` carries both rows restated from the
replays' own output, the execution-order table has no open rank, and every
divergence row names a symbol `ledger_tests.rs` can find.

#### US-016: Restate the rows and record the residual divergences
**Description:** As a parity maintainer, I want the scorecard to quote the two
replays and to stop citing a credential blocker the repository falsifies so that
every remaining justification is one I can defend.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-013, US-015

**Reference:** `docs/parity.md:80-81` for the two rows, `:109` for the rank-16 line, `:154-155` for the two divergence rows this story replaces, and `crates/vibe-core/src/parity/ledger_tests.rs` for the constraint every row must satisfy

**Acceptance Criteria:**
- [ ] Given both replays' printed output, when the rows are restated, then each
      quotes its per-family conforming counts and its closing total
- [ ] Given the retired credential justification, when the divergence table is
      edited, then the experiments row is removed and replaced by a row naming
      the live eval round trip as the unmeasured residual
- [ ] Given the promo, when the divergence table is edited, then the row claiming
      no promo surface exists is replaced by a row recording the original prose
      and its permanent-inequality guard
- [ ] Given the execution-order table, when it is edited, then rank 16 is marked
      `DONE` and names the epics that closed it
- [ ] Given a divergence row naming a symbol that does not exist, when
      `ledger_tests.rs` runs, then it fails
- [ ] Given `CHANGELOG.md`, when the change lands, then the user-visible effects
      are recorded under `## Unreleased`: enrollment, the routed default model,
      the managed shell toggle and the promo

## Functional Requirements

- FR-01: The system must resolve experiment variants from a remote evaluation
  response and must never perform bucketing locally.
- FR-02: The system must not issue an eval request when `enable_telemetry` is
  false, when `experiments.enable` is false, when no Mistral provider is
  configured, or when no credential resolves.
- FR-03: The system must fail open: any transport, status, decoding or validation
  failure leaves the client on its declared defaults and propagates nothing.
- FR-04: The system must send as its bucketing key a SHA-256 of the API key
  truncated to 32 hex characters, and must never send the key itself.
- FR-05: The system must drop features whose keys are not among the three names
  this build knows, on both initialization and hydration.
- FR-06: The system must admit a forced rule into configuration variants while
  reporting only confirmed exposures to telemetry.
- FR-07: The system must let any value written in a user or project TOML, an
  environment variable or a runtime override beat an experiment assignment.
- FR-08: The system must write a variant into configuration only when it maps to
  a valid value, and must leave the field unwritten otherwise.
- FR-09: The system must persist the resolved state into the session metadata,
  hydrate from it on resume, and inherit it on fork without a second request.
- FR-10: The system must carry confirmed exposures on client event metadata and
  must omit the field entirely when there are none.
- FR-11: The system must show the VS Code promo only in a VS Code family
  terminal, only after the start instant, and at most ten times per installation.
- FR-12: The system must NOT ship any reference-authored sentence, and must
  record every reference sentence it measures as a length plus a digest.
- FR-13: The system must NOT block session startup on either the eval request or
  the identity request.

## Non-Functional Requirements

- **Performance:** the eval request uses a 5.0 second total timeout and the
  identity request 4.0 seconds, matching the reference. Both run in a detached
  task: measured time to first prompt with a black-holed experiments host differs
  from the disabled baseline by less than 5 ms at P95.
- **Performance:** a resumed or forked session issues exactly 0 eval requests and
  0 identity requests.
- **Performance:** at most 1 identity request per (base URL, credential) pair per
  session, enforced by the single-flight cache.
- **Reliability:** experiment failures are swallowed at 100 percent: 0 transport
  failures, 0 decoding failures and 0 validation failures reach a caller, and 0
  of them change the resolved variant away from its default.
- **Reliability:** a session that closes with a lookup in flight completes
  shutdown within the 5.0 second timeout with 0 leaked tasks.
- **Security:** 0 credential values appear in an eval payload, a log line or the
  corpus. The only key derivative that leaves the process is the 32-character
  digest.
- **Security:** 0 sockets may be opened during a capture run, enforced by a guard
  that fails the run.
- **Privacy:** with `enable_telemetry = false`, 0 network requests are issued by
  this feature, asserted by a test that fails on any attempted connection.
- **Correctness:** the two replays print at least 300 comparisons across at least
  12 families with 0 divergences outside their ledgers and 0 stale ledger
  entries.
- **Correctness:** 4 of 4 configuration fields the layer writes have a test that
  changes the value and observes the change in a real execution path.
- **Portability:** the full suite passes with the reference checkout absent, with
  0 test failures and only the recapture probes skipped.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Experiments host unreachable | Network down or DNS failure | Lookup returns nothing; defaults stand; no refresh occurs | none |
| 2 | Host answers 4xx or 5xx | Misconfigured client key, service outage | Same as above, with a warning naming the status | none |
| 3 | Host answers non-JSON or an invalid payload | Proxy misconfiguration, captive portal | Same as above, with a warning naming the decode failure | none |
| 4 | Empty `api_host` or `client_key` | Operator cleared the key in a TOML | No request is issued at all | none |
| 5 | Response carries an unknown feature key | Rollout defined for a newer build | Unknown key dropped; known keys still applied | none |
| 6 | Response carries a rule with no force | Proxy returns an unresolved rule | `defaultValue` is used, as the reference does | none |
| 7 | Variant names a prompt that does not exist | Rollout typo | Field unwritten; configuration loads unchanged | none |
| 8 | Routing payload is not an object, or lacks `active_model` | Rollout misconfiguration | Neither routed field is written | none |
| 9 | Variant collides with a user's own TOML value | Operator pinned the same field | The TOML wins; enrollment still reported to telemetry | none |
| 10 | Identity fetch times out | Slow console | Attributes are built without an organization; lookup proceeds | none |
| 11 | Credential revoked mid-session | Key rotated externally | Identity fails, is not cached, and a later call retries | none |
| 12 | Session closes during a lookup | Fast quit | Task cancelled before the client closes; no leaked task | none |
| 13 | Session metadata carries an experiments field this build cannot parse | Session written by a newer client | Hydration is skipped; defaults stand; the session still opens | none |
| 14 | Session metadata predates the field | Session written before this PRD | Read as absent; a fresh lookup runs | none |
| 15 | Cache file holds a non-integer promo count | Hand edit | Read as absent; the promo may show again | none |
| 16 | Cache file is read-only or the disk is full | Locked `$VIBE_HOME` | Promo still shows; the counter write fails and is reported once | Diagnostic naming the failed write |
| 17 | Promo ceiling reached | Tenth appearance recorded | Promo withheld permanently; nothing is written | none |
| 18 | Terminal is not in the VS Code family | Any other terminal | Promo never shown; counter never written | none |
| 19 | Reference checkout absent | Fresh clone with no oracle checkout | Replays run against the committed corpora; only recapture probes skip | Skip reason printed |
| 20 | Reference checkout off-pin | Local reference moved to another commit | Recapture refuses and names the restore command | Restore command printed |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | This PRD gives the binary a startup network call it did not have, to a third-party-operated host, for users who never asked to be enrolled | High | High | The call is gated on `enable_telemetry`, which the operator already controls and which rank 15 made a real gate, plus a second `experiments.enable` switch; it is detached, bounded at 5.0 seconds and fail-open; an NFR asserts 0 requests when the gate is off; the privacy posture is identical to the reference's, which is what parity means here |
| 2 | An assignment overriding a user's own configuration file would be a regression this port introduced | Medium | High | US-010 reseats the layer below the selected TOML before US-012 ever fills it, and the `layerPrecedence` family measures all four fields against the reference orchestrator |
| 3 | Unpinning `active_model` changes the configuration document every installation ships and is visible to `config/read` | Medium | Medium | US-005 keeps the effective model identical and asserts it; the change is confined to the stored document, which `docs/parity.md:146` already describes as the intended end state |
| 4 | The oracle feeds synthetic eval responses, so it measures the client's application of a response and never the service's answer | High | Low | Recorded as the replacement divergence row in US-016; justified by the vendor's own documentation that assignment is server-side, so the client has no assignment logic left to get wrong |
| 5 | The reference response shape could carry rules the models do not anticipate, silently changing resolution | Low | Medium | The Rust models mirror the reference's tolerant configuration exactly, so both implementations degrade identically; the `featureResolution` family covers the no-force case explicitly |
| 6 | Advertising a VS Code extension that drives the Python binary may mislead a user of this port | Medium | Medium | The mechanism is ported and the message content is escalated as an open question rather than decided here; the prose is original in every case, so nothing forces the reference's claim |
| 7 | The promo counter shares `cache.toml` with the update cache, so a careless write could drop the sibling section | Low | Medium | US-014 asserts sibling preservation, which the existing store already implements by reading the document before inserting |
| 8 | EP-002 reaches outside the rank into configuration parity and could grow | Medium | Medium | Scope is fixed to the three fields the mapping targets plus the resolution that gives them effect; `show_greeting` and the three `ConfigView` fields are Non-Goals, so the growth boundary is written down rather than negotiated per story |

## Non-Goals

- **Client-side experiment evaluation.** No hashing, no condition matching, no
  coverage, no namespaces, no sticky bucketing. The proxy resolves and the client
  applies, which is what the reference does and what the vendor documents.
- **Encrypted feature payloads.** The reference does not request them; adding
  them would create a capability the reference does not have.
- **A disk cache of eval responses.** The reference caches nothing between
  sessions; the resolved state lives in the session metadata and nowhere else.
- **`show_greeting` and the three `ConfigView` fields.** `activeModelPinned`,
  `defaultModelAlias` and `showGreeting` belong to app-server parity and no
  experiment targets them. `docs/parity.md:143` stays open, narrowed to the
  greeting alone.
- **Routing `identity/read`.** The identity gateway lands in `vibe-core` because
  the attributes need it; the app-server method stays declared and unrouted, and
  `docs/parity.md:142` is unchanged.
- **Publishing or maintaining a VS Code extension.** This PRD ports a message and
  a counter, nothing more.
- **Shipping any reference-authored prose.** `NOTICE` forbids it. Both promo
  sentences enter the corpus as a length plus a SHA-256, and this port writes its
  own.
- **Measuring the live eval round trip.** No CI job may reach
  `experiments.mistral.services`. The unmeasured wire is recorded as a residual,
  the same treatment the OTLP wire carries.
- **A second gate or an opt-in prompt for experiments.** Two switches exist
  upstream and two exist here; adding a third would be a divergence, not parity.

## Files NOT to Modify

- `crates/vibe-core/src/parity.rs`: holds `REFERENCE_COMMIT`; re-pinning is a
  separate change that regenerates every corpus at once.
- `scripts/parity/pin.py`: the second and last pin source, held equal to the
  first by `parity_tests.rs`.
- `NOTICE`: the licensing boundary this PRD operates inside.
- `crates/vibe-core/tests/config-surface/corpus.json`: it records the reference's
  own answers and does not change when this port declares a field. EP-002 shortens
  the exception lists in `surface_parity_tests.rs` instead; a change that seems to
  need the corpus edited is a signal the change is wrong.
- `crates/vibe-core/tests/telemetry/corpus.json`,
  `crates/vibe-core/tests/setup-auth/corpus.json` and the other committed corpora
  are owned by their own oracles. US-013 removes a ledger entry from the telemetry
  replay, never a case from its corpus.
- `crates/vibe-protocol/src/lib.rs` `SERVER_METHODS`: this PRD adds no wire
  method, and `identity/read` stays declared and unrouted.

## Technical Considerations

Framed as questions for engineering input, not mandates.

- **Architecture:** where does the engine live? Recommended:
  `crates/vibe-core/src/experiments.rs` with a `experiments/` submodule
  directory, mirroring the reference's single subtree and matching how
  `compaction`, `checkpoints` and `skills` are laid out here. The configuration
  layer stays inside `config.rs` where the layer list already lives. Does the
  manager need interior mutability, given that the configuration layer reads it
  after a task completes?
- **Architecture:** who owns the lifecycle? Recommended: `vibe-app-server`, since
  it owns session lifecycle and already holds the resume and fork paths that must
  hydrate rather than refetch. `vibe-cli` and `vibe-acp` stay adapters that
  supply the launch context. Engineering to confirm the detached task has a home
  in the current runtime without a new executor.
- **Data Model:** the eval response is untyped JSON with tolerant models
  upstream. Recommended: `serde` structs with `#[serde(default)]` on every field
  and no `deny_unknown_fields`, which reproduces `extra="ignore"`. This is the
  opposite of the `vibe-protocol` envelope rule, so the boundary deserves a
  comment naming why.
- **Data Model:** `routed_model_config` is a full model definition carried as a
  JSON string in the variant and as a typed value in the schema. Recommended:
  reuse the existing model-entry deserialization and drop the value on failure,
  which is what the reference's `BeforeValidator` does. Alternative: store the
  raw string and parse at read time. Trade-off: parsing early fails loudly in one
  place; parsing late keeps the layer purely textual.
- **API Design:** no new wire method. `config/read` will start reporting an empty
  `active_model` after US-005. Does any existing client of this port read that
  field expecting an alias? Confirm before US-005 lands.
- **Dependencies:** none new. `reqwest`, `serde`, `sha2` and `toml` are already
  workspace dependencies, and the promo needs only what `updates.rs` already
  uses. Engineering to confirm the eval client can share the telemetry client's
  TLS and proxy configuration rather than constructing its own.
- **Migration:** sessions written before this PRD carry no experiments field, and
  sessions written after carry one this build understands. Backward
  compatibility: absent reads as absent, which `storage.rs:48` already does.
  Rollback: disabling `experiments.enable` returns the binary to today's
  behavior with the stored field ignored.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| `docs/parity.md` experiments and rollouts score | 25, declarative | 100, oracle-backed | PRD completion | The row quotes the replay's printed per-family counts |
| `docs/parity.md` VS Code extension promo score | 10, declarative | 100, oracle-backed | PRD completion | The row quotes the promo replay's counts |
| Reference experiment names resolved | 0 of 3 | 3 of 3 | EP-003 completion | `variantResolution` family conforming count |
| Configuration fields the layer writes, each with a consumer | 1 of 4 declared, 0 wired | 4 of 4 | EP-004 completion | A test per field that changes the value and observes the change |
| Undeclared reference configuration fields | 4 | 1 (`show_greeting`) | EP-002 completion | `UNDECLARED_FIELDS` length |
| Telemetry metadata fields never filled | 1 (`experiments`) | 0 | EP-005 completion | The telemetry ledger entry fails as stale once filled |
| Eval requests issued by a resumed or forked session | Not applicable | 0 | EP-005 completion | A test asserting the transport was never called |
| Oracle comparisons per run | 0 | >= 300 across >= 12 families | PRD completion | The closing line each replay prints |
| Divergences outside the ledger | Not measurable | 0 | PRD completion | Either replay fails on any |
| Ranks of the execution order still open | 1 (rank 16) | 0 | PRD completion | `docs/parity.md` execution-order table |

## Open Questions

- What should the promo say in this port, given that the advertised extension
  drives the Python binary? Arthur to decide before US-015 writes the sentence.
  Recommended: ship the mechanism and phrase the message as naming the editor
  extension without claiming it drives this binary, and record the wording choice
  in the divergence row. The alternative is to keep the mechanism dormant behind
  the predicate and score the row on the mechanism alone, which the ledger would
  have to state explicitly.
- Does the ACP entry point initialize experiments, or only the CLI and the app
  server? The reference builds the manager inside the agent loop, which both
  entry points share, so the answer is believed to be both. Engineering to
  confirm against `vibe/acp/` before US-012, since it decides whether an editor
  session issues an eval request.
- Should the eval client reuse the telemetry client's `reqwest` instance,
  including its proxy and TLS configuration? Engineering to answer inside US-007;
  sharing removes a second connection pool, separating keeps the timeout
  independent.
- Does anything in this repository or its CI read `active_model` expecting a
  non-empty alias? Answer before US-005 unpins it.
- The reference logs resolved variants at info level on both initialization and
  hydration. Is that line wanted here, given it names enrollment in a user's log
  file? Parity maintainer to decide in US-012; the prose is written originally in
  either case.
[/PRD]
