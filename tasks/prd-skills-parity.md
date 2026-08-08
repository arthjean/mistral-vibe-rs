[PRD]
# PRD: Skills Parity

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-08 | Arthur Jean | Initial PRD from the measured skills audit against the Python reference at commit `b78b451`: the 2 124-line surface formed by `vibe/core/skills/` plus its five consumers has a frontmatter reader that is not YAML, a model missing six fields, five search roots reduced to one invented path, three configuration keys declared and read by nobody, no builtin skills at all, an invoked-skill path that writes a different history than the reference, and no oracle measuring any of it |

## Problem Statement

1. **The frontmatter reader is not a YAML parser, and it is wrong on real files.** `parse_skill` reads line by line and splits each on the first colon (`crates/vibe-core/src/extensions.rs:653-699`). The reference delegates to `yaml.safe_load` after a three-way boundary split ([vibe/core/skills/parser.py:18](/home/arthur/dev/mistral-vibe/vibe/core/skills/parser.py)). Measured against the four real skills the reference ships in its own repository, the port reads `user-invocable: true` into a key named `user-invocable`, then reads `user_invocable` and finds nothing, so it defaults to `true` (`extensions.rs:687`). A skill published as `user-invocable: false` is invocable here. The nested `metadata:` block is flattened into the same map, so `display-name` and `default-prompt` land in the root namespace. Any legitimate YAML line without a colon, a sequence item or a folded scalar, fails the whole parse and drops the skill (`extensions.rs:669`).

2. **The model carries 6 of the 12 declared fields.** `SkillMetadata` declares `name`, `description`, `license`, `compatibility`, `metadata`, `allowed_tools` and `user_invocable`, the last two under the hyphenated validation aliases `allowed-tools` and `user-invocable`, with `allowed_tools` coerced from a space-delimited string and `metadata` normalized to string values ([models.py:37-93](/home/arthur/dev/mistral-vibe/vibe/core/skills/models.py)). `SkillInfo` adds `source`, `scope` and `registry`. `SkillDefinition` declares `name`, `description`, `user_invocable`, `body`, `source` and `path` (`crates/vibe-core/src/extensions.rs:310-317`). Six fields have no counterpart.

3. **Name validation is looser than the reference, so this port accepts skills upstream rejects.** The reference enforces `^[a-z0-9]+(-[a-z0-9]+)*$` with a length of 1 to 64, and a `description` of 1 to 1024 that is mandatory ([models.py:40-52](/home/arthur/dev/mistral-vibe/vibe/core/skills/models.py)). `valid_extension_name` allows 128 characters, uppercase and underscores (`crates/vibe-core/src/extensions.rs:1600`), and the description falls back to an empty string (`extensions.rs:686`). `SkillDoThing`, `skill_do_thing` and a description-less skill all load here and none of them loads upstream.

4. **Four of the five search roots do not exist, and the fifth is at an invented path.** `_compute_search_paths` walks `config.skill_paths`, then every project root's `.vibe/skills` and `.agents/skills`, then `~/.vibe/skills` and `~/.agents/skills`, resolving and deduplicating ([manager.py:73-90](/home/arthur/dev/mistral-vibe/vibe/core/skills/manager.py), [_harness_manager.py:119,146](/home/arthur/dev/mistral-vibe/vibe/core/config/harness_files/_harness_manager.py), [_local_config_files.py:29-33](/home/arthur/dev/mistral-vibe/vibe/core/paths/_local_config_files.py)). This port passes `configured: Vec::new()` at both construction sites (`crates/vibe-app-server/src/release3.rs:270`, `crates/vibe-core/src/tools/builtins.rs:302`), reads project skills only from `.vibe/skills`, and reads user skills from `~/.vibe/extensions/skills`, a prefix that appears nowhere in the reference. A user who followed the documented layout and wrote `~/.vibe/skills/my-skill/SKILL.md` gets nothing. `trustable_files` already knows `.agents/skills` exists (`crates/vibe-app-server/src/startup.rs:306`), so the port asks for trust over a directory it then never reads.

5. **Three configuration keys are declared and consumed by nobody.** `skill_paths`, `enabled_skills` and `disabled_skills` are declared, published and merged (`crates/vibe-core/src/config/registry.rs:733-747`), and a workspace-wide grep finds no other mention of any of them. `experimental_enable_registry_skills` (`registry.rs:749`) is in the same state on both sides. `docs/parity.md` states the rule this violates in its own configuration row: declaring a key is not implementing its feature.

6. **There are no builtin skills.** All three callers pass an empty map for `builtin_skills` (`crates/vibe-app-server/src/release3.rs:1805`, `crates/vibe-core/src/tools/builtins.rs:539`, `crates/vibe-app-server/src/client.rs:3478`). The reference seeds `vibe` and `skill-creator` before any disk walk and reserves their names against override ([builtins/__init__.py:7](/home/arthur/dev/mistral-vibe/vibe/core/skills/builtins/__init__.py), [manager.py:93,119](/home/arthur/dev/mistral-vibe/vibe/core/skills/manager.py)). Three consequences follow: the `source: "builtin"` literal the app-server census declares is never emitted, the name reservation branch protects an empty set, and `BannerMetrics.skills_count` (`crates/vibe-cli/src/tui/mod.rs:208`) counts every skill where the reference counts custom skills only ([manager.py:169](/home/arthur/dev/mistral-vibe/vibe/core/skills/manager.py)).

7. **Invoking a skill writes a different conversation than the reference does.** `_inject_invoked_skill` appends an assistant message carrying a synthetic `skill` tool call plus the matching tool message, then yields both events ([_loop.py:1694-1766](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py)). The dedup marker `<skill_content name="...">` is searched in the tool messages of the stored history ([_loop.py:1687](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py)). This port sends the body as a `resource` block addressed `skill://name` from the CLI (`crates/vibe-cli/src/tui/prompt.rs:105-119`) and accepts `injectInvokedSkill` on `turn/steer` and `context/inject` while doing nothing with it (`crates/vibe-app-server/src/server.rs:4529,4546`). The persisted transcript differs, the transcript shows no tool call, and the skill is never marked loaded, so a later model call to `skill` re-delivers the whole body where the reference answers that it is already loaded.

8. **A skill that fails to load is silent to the operator.** The reference records a `SkillConfigIssue` per failure and projects every one of them into `diagnostics/list` ([manager.py:142](/home/arthur/dev/mistral-vibe/vibe/core/skills/manager.py), [_projection.py:390](/home/arthur/dev/mistral-vibe/vibe/app_server/_projection.py)). This port builds the `DiscoveryIssue` (`crates/vibe-core/src/extensions.rs:513`) but the `skill` tool handler discards the catalog's issue list entirely (`crates/vibe-core/src/tools/builtins.rs:539`), so a malformed skill invoked from a tool call is indistinguishable from one that was never written.

9. **The whole remote registry subtree is absent.** 661 lines across four files implement a paginated catalog client capped at 50 pages, an atomically staged version store with traversal and reserved-entrypoint rejection, owner-only executable bits, pruning, local export, and two manifest scopes with a `latest` alias pin. Nothing in `crates/` corresponds. The mitigating fact, established by grep over the entire reference tree, is that **no module in `vibe/` imports any of it** at this pin: it is reachable only from `tests/skills/registry/`, and `experimental_enable_registry_skills` has no reader either. It publishes no wire method, no CLI command and no response field.

10. **Nothing measures any of it.** Six differential oracles exist here, for the tool surface, the configuration surface, the app-server wire surface, tool execution, checkpoints and compaction. None covers skills. `docs/parity.md` scores the part 55 by reading module presence. The reference carries 128 test functions over this surface (40 in `tests/skills/test_manager.py`, 19 in `test_models.py`, 8 in `test_parser.py`, 7 in `test_builtin_sync.py`, and 54 across `tests/skills/registry/`), and `cargo test --workspace --all-features` passes green against a parser that mis-reads their fixtures.

**Why now:** `docs/parity.md` places complete skills at rank 11 and both dependencies its own table names are satisfied: the `skill` tool shipped with rank 1, and the three configuration keys shipped declared with rank 2. The cost of deferral is concentrated in defect 1 and it compounds in the wrong direction. Skill files are **user-authored artifacts already on disk**. Every week the parser stays wrong, more `SKILL.md` files are written against what this port happens to accept rather than against the schema the reference publishes, and a later fix turns them from working into rejected. The same argument put tool names at rank 1, applied to files the user owns instead of to identifiers the port emits.

## Overview

This initiative makes the skills subsystem behaviorally equivalent to the reference at every boundary a user or a client can observe: which files are read, which of them parse, which fields survive, which skills are published, what the wire says about them, and what the conversation contains after one is invoked. Equivalence is defined mechanically: for a given directory tree, configuration document and prompt, this port must publish the same skill set with the same field values, reject the same files with an issue in the same place, and write the same history entries.

The sequencing puts the instrument first, following this repository's own record: every part measured by an oracle scores 95 or above and every part measured by module presence sits between 55 and 80. The first epic builds `scripts/parity/skills.py` and its corpus. The reference makes this cheap: `SkillManager` takes a `config_getter` callable and an optional `HarnessFilesManager` ([manager.py:29](/home/arthur/dev/mistral-vibe/vibe/core/skills/manager.py)), so a capture script drives it over temporary directory trees with no network, no key and no backend, exactly as the compaction oracle drives `CompactionManager` over a stubbed completion function.

The second epic replaces the frontmatter reader. This is the one place in the PRD where a dependency enters the workspace, and the decision is argued rather than assumed: the boundary rules stay hand-written because they are parity contract, and only the YAML block is delegated. The third epic gives discovery its five roots, its configuration input and its filter. The fourth seeds the builtin catalog, which is where `NOTICE` binds hardest: the reference ships 40 589 bytes of builtin prose, so the bodies are written originally and the divergence is recorded rather than hidden. The fifth rewrites the invoked-skill path so the conversation this port persists is the conversation the reference persists. The sixth ports the dormant registry behind its experiment key, without inventing the surface the reference does not publish, and remeasures the scorecard.

The reference is a read-only checkout pinned for this PRD at commit `b78b451c39eab9213393ad2f45908e8562a5c5e7` (v2.24.0), which every measurement in this document was taken from. This PRD does **not** re-pin: `vibe_core::parity::REFERENCE_COMMIT` and `EXPECTED_COMMIT` in `scripts/parity/pin.py` already name it. Its location is machine-dependent, `C:\dev\mistral-vibe` on Windows and `/home/arthur/dev/mistral-vibe` on Linux; reference links below use the Linux form and resolve against whichever checkout is local, through `VIBE_REFERENCE` or `--reference`, and Rust tests reach it through `vibe_core::parity::reference_root`.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Parse what the reference parses | 0 divergent parses over the captured frontmatter corpus, including nested mappings, block scalars, sequences and the YAML 1.1 boolean vocabulary | 0 maintained |
| Reject what the reference rejects | 0 wrongly accepted and 0 wrongly rejected over the captured validation cases | 0 maintained |
| Publish the whole model | 12 of 12 metadata and info fields carried, with both hyphenated aliases accepted | 0 dropped fields |
| Read every search root | 5 of 5 roots resolved in reference order with resolve-and-dedup, 0 invented paths | 0 maintained |
| Consume the declared configuration | 4 of 4 skill keys read by a real code path, each proven by a test that changes the key and observes the behavior change | 0 declared-only skill keys |
| Seed the builtin catalog | 2 of 2 builtins published with `source: "builtin"`, names reserved, custom count excluding them | 0 maintained |
| Write the reference's conversation | The invoked-skill path appends the same two history entries, and the already-loaded answer is returned on the second invocation | 0 divergent history shapes |
| Port the registry faithfully and dormant | 4 of 4 registry modules ported behind the experiment key, 0 wire methods invented | 0 maintained |
| Make conformance mechanically enforced | Corpus replays at least 120 scenarios across 8 families and fails on any divergence outside a named ledger | Ledger holds only `NOTICE` entries |
| Raise the measured score | `docs/parity.md` Skills from 55 to 100, measured by the new oracle | Configuration row updated with the four keys this work consumes |

## Target Users

### Operator who wrote a skill by the documentation

- **Role:** Developer who read the skill layout, created `~/.vibe/skills/run-migrations/SKILL.md`, and expects `/run-migrations` to work.
- **Behaviors:** Copies the frontmatter shape from an existing skill, uses a nested `metadata:` block for display names, marks internal skills `user-invocable: false`.
- **Pain points:** The skill never appears, because the port reads `~/.vibe/extensions/skills`. When it is moved to the path the port does read, the `user-invocable: false` marker is ignored and the skill shows up in the slash menu anyway. A folded description breaks the whole file and nothing says why.
- **Current workaround:** Trial and error against undocumented behavior, converging on a frontmatter subset that happens to work here and nowhere else.
- **Success looks like:** The documented path is read, every documented field is honored, and a file that cannot load says so in `diagnostics/list`.

### Model consuming the skill catalog mid-turn

- **Role:** The agent itself, reading the `# Available Skills` section and calling the `skill` tool.
- **Behaviors:** Routes on the description, loads the body once, relies on the base directory to resolve the skill's support files.
- **Pain points:** Descriptions are truncated or empty when the frontmatter used a folded scalar, so routing degrades. A skill invoked by the user through `/name` is not recorded as loaded, so calling `skill` again re-delivers the entire body and burns the context the load was meant to save.
- **Current workaround:** None available to the model.
- **Success looks like:** Every advertised skill has its full description, and a second load is answered with the already-loaded sentence.

### Editor integration author rendering skills

- **Role:** Author of an IDE extension speaking JSON-RPC to the app-server against the reference protocol.
- **Behaviors:** Lists skills from `skills/list`, groups them by `source`, hides the ones that are not user invocable, and renders the invocation as a tool call in the transcript.
- **Pain points:** `source` never takes the `builtin` value the census declares, so the grouping has one empty bucket forever. A skill invoked by slash command produces no tool call to render, because the body traveled as an opaque resource block instead.
- **Current workaround:** Special-case this port against the documented protocol.
- **Success looks like:** All three `source` values are reachable and an invoked skill renders as the tool call the reference emits.

### Parity maintainer certifying the port

- **Role:** Maintainer of `docs/parity.md`, required by `AGENTS.md` to state a parity claim only from a measurement.
- **Behaviors:** Runs the oracles, reads the printed counts, updates the scorecard row from them.
- **Pain points:** Skills is one of the largest unmeasured parts left. Its score of 55 comes from reading module presence, which cannot distinguish a parser that works from one that silently mis-reads every nested mapping.
- **Current workaround:** Score by inspection and mark the number as uncertain in the method section.
- **Success looks like:** `cargo test -p vibe-core --all-features skills_parity_tests -- --nocapture` prints per-family counts and the scorecard row cites them.

## Research Findings

Key findings that informed this PRD. The research is a first-party measurement against the pinned oracle, plus one external question that changes an implementation decision.

### The surface, measured

- `vibe/core/skills/` is 2 124 lines across 11 files: the core is `manager.py` at 195, `models.py` at 153 and `parser.py` at 40; the builtins are `vibe.py` at 928 and `skill_creator.py` at 125; the registry is `_store.py` at 241, `models.py` at 187, `_client.py` at 135 and `_manifest.py` at 98.
- Reference test coverage of this surface is 128 test functions: 40 in `tests/skills/test_manager.py`, 19 in `test_models.py`, 8 in `test_parser.py`, 7 in `test_builtin_sync.py`, 18 in `registry/test_models.py`, 14 in `registry/test_client.py`, 14 in `registry/test_store.py` and 8 in `registry/test_manifest.py`. That density is why this PRD carries a corpus rather than named tests alone.
- Builtin prose totals 40 589 bytes across two files, of which one block is 39 752 bytes. This is three times the tool description prose that `docs/parity.md` already records as permanently out of reach.

### The registry is dormant upstream, and that is load-bearing for the plan

A grep over the whole reference tree for `RegistrySkillsClient`, `SkillManifest`, `store_root`, `materialize`, `prune`, `export_local`, `RegistryRef`, `SkillScope` and `REGISTRY_LATEST_ALIAS` returns hits only inside `vibe/core/skills/registry/`, `vibe/core/skills/models.py` and `tests/skills/registry/`. `experimental_enable_registry_skills` has exactly one occurrence in the whole tree, its own declaration at [vibe_schema.py:399](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py). The subtree therefore publishes no observable contract at this pin. Two consequences shape the plan: porting it cannot raise a wire score, and inventing a `skills/install` method to make it reachable would add an invented name that `app_server_surface_parity_tests` fails on. It ships dormant, exactly as upstream, and its conformance is measured against the module contract rather than against a wire surface.

### The oracle needs no backend

`SkillManager.__init__` takes `config_getter: Callable[[], VibeConfigSchema]` and an optional `harness_files` ([manager.py:29-36](/home/arthur/dev/mistral-vibe/vibe/core/skills/manager.py)). A capture script builds a temporary tree, constructs a config with the desired `skill_paths`, `enabled_skills` and `disabled_skills`, and reads `available_skills`, `config_issues`, `custom_skills_count` and `parse_skill_command`. `parse_skill_markdown` and `SkillMetadata.model_validate` are pure. `_store` and `_manifest` are filesystem-only and drive from a temporary directory. Only `RegistrySkillsClient` needs transport, and its four HTTP behaviors (the field mask, the pagination cap, the status-to-error mapping, the version sort) are observable from the request parameters and the raised reason without a live service.

### The YAML dependency question, answered externally

The workspace has no YAML crate anywhere, confirmed by grepping every `Cargo.toml`. The external state of the ecosystem as of August 2026: `serde_yaml` was archived on 2024-03-25 at `0.9.34+deprecated` and receives no fixes; `serde_yml` carries RUSTSEC-2025-0068 for versions at or below 0.0.12 and is now a compatibility shim; the live options are `serde_yaml_ng 0.10.0` and `serde_norway 0.9.42`, both continuations of the dtolnay lineage over a libyaml-derived backend, and `serde-saphyr 1.0.1`, an independent pure-Rust implementation on `saphyr-parser` that reached 1.0 and states panic-free handling of malformed input as a design goal. `gray_matter 0.3.2` handles frontmatter extraction and typed deserialization in one dependency, over `yaml-rust2`.

The choice is `serde-saphyr` for the YAML block only, with the boundary reproduced locally. Two reasons: the boundary rules are parity contract, not convenience (`^-{3,}\s*$` as a multiline pattern, a three-way split, rejection when the text before the first boundary is non-empty, and a leading BOM stripped, [parser.py:15-24](/home/arthur/dev/mistral-vibe/vibe/core/skills/parser.py)), and a crate that owns them would have to be bent back into shape. Pure Rust also matters in a workspace that forbids `unsafe_code`, even though the lint binds local crates only.

### The scalar resolution trap

PyYAML implements the YAML 1.1 boolean vocabulary minus its single-letter forms: `yes`, `no`, `on`, `off` and their capitalizations resolve to booleans, while bare `y` and `n` stay strings, which the captured `frontmatter` family records case by case and the implementation follows over this sentence. YAML 1.2 core schema, which every modern Rust crate implements, resolves the whole set as strings. A skill written `user-invocable: no` is `False` upstream and the string `"no"` here, which a naive `bool` deserializer rejects and a lenient one reads as truthy. The inversion is silent and it publishes a model-only skill in the slash menu. The two boolean-bearing fields deserialize through an explicit resolver reproducing the 1.1 vocabulary, and the corpus carries the ambiguous scalars as their own family. The same class of trap exists for unquoted `1.0` and for sexagesimal integers, but neither reaches a typed field in this schema, so both are recorded and left alone.

### Best practices applied, from this repository's own record

- **Instrument before implementation.** All six existing oracles were built before the work they measure. The corpus is epic 1, before any parser change.
- **Capture through `git archive`, never by moving HEAD**, following `scripts/parity/tool_execution.py`.
- **Commit observations, digest reference-authored prose.** Field names, pointers, counts and scenario-supplied values commit verbatim; the builtin bodies commit as length plus SHA-256, following `Digested` in `crates/vibe-core/src/compaction/compaction_parity_tests.rs:239`.
- **Ledger the divergences and fail when the ledger goes stale**, following `DIVERGENCES` and `settle()` in `compaction_parity_tests.rs:61,358`.

## Assumptions & Constraints

### Assumptions (to validate)

- **`serde-saphyr` accepts every frontmatter construction PyYAML accepts, for the shapes this schema reads.** Based on both implementing the YAML core schema for mappings, scalars and sequences. US-162 validates it by replaying the parse family; any construction that diverges is normalized on both sides or recorded, as `grep` match order already is.
- **No `SKILL.md` on a user's disk depends on the current flattening behavior.** Based on the flattening producing keys the port then ignores, so no value it produces is read by anything. If a counterexample is found, US-163 gains a compatibility criterion.
- **Moving the user skills root from `~/.vibe/extensions/skills` to `~/.vibe/skills` strands no existing installation.** Based on `~/.vibe/extensions` appearing in no released documentation and in no installer. US-165 keeps the old path as an additional root behind a deprecation note rather than deleting it outright, and the criterion states which of the two wins on a name collision.
- **`ModelMessage::Assistant` with a `tool_calls` vector and no content round-trips through every provider adapter.** Based on the same shape being produced by real completions today (`crates/vibe-core/src/engine.rs:844`). US-172 asserts it against a persisted transcript replay.
- **The registry client's four HTTP behaviors are observable without a live service.** Based on the reference's own tests driving it through a mock transport. US-178 captures request parameters and error reasons, not response bodies from the real API.

### Hard constraints

- `NOTICE` forbids copying, translating, vendoring, linking or shipping upstream implementation source, prompt files or tool description text. The two builtin bodies are written originally and held to directive coverage, never to text. The corpus commits structural observations and scenario-supplied values only.
- The reference pin lives in exactly two places, `vibe_core::parity::REFERENCE_COMMIT` and `EXPECTED_COMMIT` in `scripts/parity/pin.py`, held equal by `crates/vibe-core/src/parity/parity_tests.rs`. This PRD does not move them.
- A missing or off-pin reference checkout must never fail `cargo test`. The committed corpus replays unconditionally; only the live recapture probe skips, through `off_pin_reason` (`crates/vibe-core/src/parity.rs:64`).
- The layering in `[workspace.metadata.vibe] dependency-layers` holds: the parser, the model, discovery, the builtin catalog and the registry belong in `vibe-core`; their projection belongs in `vibe-app-server`; `vibe-cli` and `vibe-acp` are adapters.
- `unsafe_code` is forbidden workspace-wide; `panic`, `unimplemented` and `dbg_macro` are denied outside tests.
- Exactly one new workspace dependency is introduced by this PRD. Any second one is a scope change requiring an amendment.
- Every `EngineEvent` and `ModelMessage` variant is serialized into persisted transcripts, so an existing variant is never removed or renamed; a new field arrives with `#[serde(default)]`.
- `[workspace.package] version` is not bumped by this work.

## Quality Gates

These commands must pass for every user story:

- `cargo fmt --all -- --check` - formatting
- `cargo check --workspace --all-targets --all-features` - compilation of every target including the fixture binaries
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` - lint set with warnings denied
- `cargo test --workspace --all-features` - the full suite, never a filtered subset, because skill fixtures are read from more than one module

Stories that touch a parity corpus additionally report their conformance counts:

- `cargo test -p vibe-core --all-features skills_parity_tests -- --nocapture` - skills conformance counts across the eight families
- `cargo test -p vibe-app-server --all-features app_server_surface_parity_tests -- --nocapture` - wire census, which `SkillSummary` and its three `source` literals are validated against
- `cargo test -p vibe-core --all-features config_surface_parity_tests -- --nocapture` - configuration census, for the four skill keys this work makes readable

## Reference Map

Every file an implementer opens before writing Rust, at the pinned commit `b78b451`. Paths use the Linux canonical spelling and resolve against whichever checkout is local. Each story below names its own anchor; this is the whole surface in one place. Reading these is required by `AGENTS.md`, and grepping them does not replace opening the declaration they point at.

The subtree this PRD reproduces is [/home/arthur/dev/mistral-vibe/vibe/core/skills/](/home/arthur/dev/mistral-vibe/vibe/core/skills), and its behavioral specification is [/home/arthur/dev/mistral-vibe/tests/skills/](/home/arthur/dev/mistral-vibe/tests/skills). Open the directory before the individual file: the reference splits this surface into a core, a builtin set and a registry, and a change read in isolation from that split reads as arbitrary.

### The skills module (11 files, 2 124 lines)

- [vibe/core/skills/manager.py](/home/arthur/dev/mistral-vibe/vibe/core/skills/manager.py), 195 lines: `SkillManager` (28), `config_issues` (55), `_apply_filters` (58), `_compute_search_paths` (73), `_discover_skills` (92), `_discover_skills_in_dir` (109), `_try_load_skill` (137), `_parse_skill_file` (148), `custom_skills_count` (169), `get_skill` (172), `parse_skill_command` (175).
- [vibe/core/skills/models.py](/home/arthur/dev/mistral-vibe/vibe/core/skills/models.py), 153 lines: `SkillSource` (10), `SkillScope` (16), `REGISTRY_LATEST_ALIAS` (25), `RegistryRef` (28), `SkillMetadata` (37), `parse_allowed_tools` (79), `normalize_metadata` (88), `SkillInfo` (96), `skill_dir` (112), `from_metadata` (118), `SkillConfigIssue` (145), `ParsedSkillCommand` (150).
- [vibe/core/skills/parser.py](/home/arthur/dev/mistral-vibe/vibe/core/skills/parser.py), 40 lines: `SkillParseError` (9), `FM_BOUNDARY` (15), `parse_skill_markdown` (18).
- [vibe/core/skills/builtins/__init__.py](/home/arthur/dev/mistral-vibe/vibe/core/skills/builtins/__init__.py): `BUILTIN_SKILLS` (7). [builtins/vibe.py](/home/arthur/dev/mistral-vibe/vibe/core/skills/builtins/vibe.py): `SKILL` (913), `user_invocable=False`, no path. [builtins/skill_creator.py](/home/arthur/dev/mistral-vibe/vibe/core/skills/builtins/skill_creator.py): `SKILL` (114), `user_invocable=True`.
- [vibe/core/skills/registry/_client.py](/home/arthur/dev/mistral-vibe/vibe/core/skills/registry/_client.py): `_MAX_PAGES` (17), `RegistrySkillsError` (20), `_parse` (26), `_CATALOG_FIELDS` (39), `list_catalog` (70), `list_versions` (73), `get_skill` (80), `_list` (93), `_get_json` (117).
- [vibe/core/skills/registry/_store.py](/home/arthur/dev/mistral-vibe/vibe/core/skills/registry/_store.py): `_RESERVED_ENTRYPOINTS` (18), `store_root` (22), `_skill_root` (26), `skill_dir` (38), `latest_materialized` (50), `_materialize` (82), `_build_skill_markdown` (124), `_strip_frontmatter` (142), `_write_assets` (152), `_safe_dest` (173), `_export_local` (197), `_prune` (221).
- [vibe/core/skills/registry/_manifest.py](/home/arthur/dev/mistral-vibe/vibe/core/skills/registry/_manifest.py): `ManifestEntry` (16), `alias` (26), `SkillManifest` (31), `upsert` (36), `remove` (40), `global_manifest_path` (47), `_project_manifest_paths` (63), `_load` (78), `_save` (95).
- [vibe/core/skills/registry/models.py](/home/arthur/dev/mistral-vibe/vibe/core/skills/registry/models.py): `sanitize_skill_name` (14), `RegistryAssetContent.to_bytes` (33), `RegistrySkillPayload` (44), `RegistrySkillItem` (106), `resolved_name` (125), `resolved_description` (141), `ListSkillsResponse` (150), `SkillVersionInfo` (159), `RegistryVersionRow.to_info` (178).

### What drives them

- [vibe/core/agent_loop/_loop.py](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py): construction (498), banner count (924), the two injection call sites (1129, 1581), `_skill_already_loaded` (1687), `_inject_invoked_skill` (1694), reload (2846).
- [vibe/core/tools/builtins/skill.py](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/skill.py): `_MAX_LISTED_FILES` (21), `skill_content_marker` (24), `render_skill_result` (40), `already_loaded_result` (85), `select_skill_result` (97), `run` (140).
- [vibe/core/system_prompt.py](/home/arthur/dev/mistral-vibe/vibe/core/system_prompt.py): `_get_available_skills_section` (262).
- [vibe/core/config/vibe_schema.py](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py): `skill_paths` (375), `enabled_skills` (384), `disabled_skills` (392), `experimental_enable_registry_skills` (399).
- [vibe/core/config/harness_files/_paths.py](/home/arthur/dev/mistral-vibe/vibe/core/config/harness_files/_paths.py): `GLOBAL_SKILLS_DIR` (6), `GLOBAL_REGISTRY_SKILLS_CACHE_DIR` (7), `GLOBAL_AGENTS_SKILLS_DIR` (12). [_harness_manager.py](/home/arthur/dev/mistral-vibe/vibe/core/config/harness_files/_harness_manager.py): `user_skills_dirs` (119), `project_skills_dirs` (146). [_local_config_files.py](/home/arthur/dev/mistral-vibe/vibe/core/paths/_local_config_files.py): the four subdirectory constants (29), `find_local_config_dirs` (54). [_agents_home.py](/home/arthur/dev/mistral-vibe/vibe/core/paths/_agents_home.py): `_DEFAULT_AGENTS_HOME` (7).
- [vibe/core/utils/matching.py](/home/arthur/dev/mistral-vibe/vibe/core/utils/matching.py): `name_matches` (16).

### What publishes it on the wire

- [vibe/app_server/models.py](/home/arthur/dev/mistral-vibe/vibe/app_server/models.py): `SkillSummary` (478). [protocol.py](/home/arthur/dev/mistral-vibe/vibe/app_server/protocol.py): `skills/list` (153). [_resources.py](/home/arthur/dev/mistral-vibe/vibe/app_server/_resources.py): dispatch (364), `_skills_list` (816). [_projection.py](/home/arthur/dev/mistral-vibe/vibe/app_server/_projection.py): `project_skills` (231), the config-issue projection (390). [_turns.py](/home/arthur/dev/mistral-vibe/vibe/app_server/_turns.py): `inject_invoked_skill` on steer (303) and inject (339). [client_state.py](/home/arthur/dev/mistral-vibe/vibe/app_server/client_state.py): `custom_skills_count` (44).

### The behavioral inventory

The reference's own tests are the checklist, all under [/home/arthur/dev/mistral-vibe/tests/skills/](/home/arthur/dev/mistral-vibe/tests/skills): 40 functions in [test_manager.py](/home/arthur/dev/mistral-vibe/tests/skills/test_manager.py), 19 in [test_models.py](/home/arthur/dev/mistral-vibe/tests/skills/test_models.py), 8 in [test_parser.py](/home/arthur/dev/mistral-vibe/tests/skills/test_parser.py), 7 in [test_builtin_sync.py](/home/arthur/dev/mistral-vibe/tests/skills/test_builtin_sync.py), and 54 across [tests/skills/registry/](/home/arthur/dev/mistral-vibe/tests/skills/registry), split 18 in `test_models.py`, 14 in `test_client.py`, 14 in `test_store.py` and 8 in `test_manifest.py`. The four skills the reference ships in its own repository, under [/home/arthur/dev/mistral-vibe/.vibe/skills/](/home/arthur/dev/mistral-vibe/.vibe/skills), are the real-world frontmatter fixtures: each one carries a nested `metadata:` block and one carries `user-invocable`, which is what the current reader mis-parses. Read all of these for the cases, never for the code.

## Epics & User Stories

### EP-046: The Skills Oracle and Its Corpus

Capture the reference's answers for the parser, the schema, discovery, filtering, command parsing, the store and the manifests into a committed corpus that replays with no backend, before a single line of the implementation changes.

**Definition of Done:** `scripts/parity/skills.py` captures eight families from the pinned checkout through `git archive`; `crates/vibe-core/tests/skills/corpus.json` is committed; `skills_parity_tests` replays it unconditionally, prints per-family counts, and fails on any divergence outside a named ledger and on any ledger entry that has gone stale.

#### US-160: Capture the reference skill surface into a committed corpus
**Description:** As a parity maintainer, I want the reference's own answers recorded for every skill behavior so that conformance is measured rather than asserted, and so that the implementation stories that follow have a target that cannot drift.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None
**Reference:** [vibe/core/skills/parser.py:18](/home/arthur/dev/mistral-vibe/vibe/core/skills/parser.py) for the boundary rules, [models.py:37](/home/arthur/dev/mistral-vibe/vibe/core/skills/models.py) for the schema, [manager.py:73,92,58,175](/home/arthur/dev/mistral-vibe/vibe/core/skills/manager.py) for search paths, discovery, filtering and command parsing, [registry/_store.py:82](/home/arthur/dev/mistral-vibe/vibe/core/skills/registry/_store.py) and [registry/_manifest.py:78](/home/arthur/dev/mistral-vibe/vibe/core/skills/registry/_manifest.py) for the filesystem families. The local pattern to follow is `scripts/parity/config_surface.py:85` for the interpreter re-exec and `:1146` for the entry point

**Acceptance Criteria:**
- [ ] Given `scripts/parity/skills.py`, when it runs with no arguments, then it resolves the reference from `pin.DEFAULT_REFERENCE`, accepts `--reference` and `VIBE_REFERENCE` as overrides, and re-executes itself with the reference interpreter when `import vibe` fails, following `config_surface.py:85-128`
- [ ] Given the script, when it reads the reference, then it does so through `git archive` at the pinned commit and never moves HEAD, creates a branch or adds a worktree
- [ ] Given the capture, when it completes, then the corpus carries `schemaVersion`, `reference` and eight families: `frontmatter`, `metadata`, `discovery`, `filtering`, `command`, `projection`, `store` and `manifest`
- [ ] Given the `frontmatter` family, when it is inspected, then it holds at least 20 cases covering a valid document, a missing boundary, an unclosed boundary, invalid YAML, a non-mapping document, an empty document, a body-less document, a leading BOM, a boundary of more than three hyphens, a nested mapping, a block sequence, a folded scalar, a literal scalar, and the YAML 1.1 boolean vocabulary on `user-invocable`
- [ ] Given the `metadata` family, when it is inspected, then it holds at least 25 cases recording accepted or rejected per case, covering both hyphenated aliases, both underscore spellings, `allowed_tools` as a space-delimited string, as a list and as null, `metadata` values coerced from non-strings, an uppercase name, an invalid character, consecutive hyphens, leading and trailing hyphens, a 64-character name, a 65-character name, an empty description and a 1025-character description
- [ ] Given the `discovery` family, when it is inspected, then it holds at least 20 scenarios over synthetic trees covering all five roots, precedence between them, duplicate names within one root, a directory without `SKILL.md`, a file where a directory is expected, a nonexistent configured path, and a name that collides with a builtin
- [ ] Given the reference's builtin bodies, when they are captured, then only their length and SHA-256 are recorded and no byte of their text enters the corpus
- [ ] Given a capture run twice on an unchanged checkout, when the two outputs are compared, then they are identical, and any family that is not is normalized on both sides with the normalization recorded

#### US-161: Replay the corpus unconditionally with a named divergence ledger
**Description:** As a parity maintainer, I want the corpus replayed on every `cargo test` so that a regression fails the build on a machine that has never seen the reference checkout.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-160
**Reference:** No reference counterpart. The local pattern is `crates/vibe-core/src/compaction/compaction_parity_tests.rs:61` for the ledger keyed `family/case`, `:358` for `settle()`, `:239` for the digested form and `:906` for the live-probe skip

**Acceptance Criteria:**
- [ ] Given `crates/vibe-core/src/skills/skills_parity_tests.rs`, when `cargo test` runs on a machine with no reference checkout, then every family replays from the committed corpus and no test is skipped
- [ ] Given the replay, when it runs with `--nocapture`, then it prints one conforming-count line per family in the form `family: N/M`
- [ ] Given a divergence that appears in a family and is not in the ledger, when the replay runs, then it fails naming the family, the case and the observed and expected values
- [ ] Given a ledger entry whose divergence has been fixed, when the replay runs, then it fails naming the stale entry, so the ledger can never silently outlive its reason
- [ ] Given a corpus whose `reference` field disagrees with `vibe_core::parity::REFERENCE_COMMIT`, when the replay runs, then it fails before comparing anything
- [ ] Given the live recapture probe, when the checkout is absent or off-pin, then `off_pin_reason` returns early with the message quoting `RESTORE_COMMAND` and the test passes

---

### EP-047: The Frontmatter Parser and the Skill Schema

Replace the line-by-line reader with a real YAML parse behind the reference's own boundary rules, and validate what it produces against the reference's schema.

**Definition of Done:** exactly one YAML dependency is in `Cargo.toml`; the boundary rules are reproduced locally; every field of `SkillMetadata` is read with both spellings accepted; the `frontmatter` and `metadata` families replay with zero divergence; and the whole model reaches the wire.

#### US-162: Land a YAML parser and reproduce the frontmatter boundary
**Description:** As an operator, I want my skill's frontmatter read as YAML so that a nested mapping, a folded description or a sequence loads here exactly as it loads upstream.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-161
**Reference:** [vibe/core/skills/parser.py:15-40](/home/arthur/dev/mistral-vibe/vibe/core/skills/parser.py) for `FM_BOUNDARY`, the three-way split, the non-empty-prefix rejection, the BOM strip, the YAML error wrapping, the `None` document becoming an empty mapping and the non-mapping rejection

**Acceptance Criteria:**
- [ ] Given the root `Cargo.toml`, when this story lands, then exactly one YAML dependency is added, it is `serde-saphyr` at its 1.x release, and no second YAML crate appears anywhere in the workspace
- [ ] Given a document whose frontmatter is delimited by exactly three hyphens, by more than three, or by hyphens followed by trailing whitespace, when it is parsed, then all three are accepted, matching `^-{3,}\s*$` applied per line
- [ ] Given a document with any non-whitespace text before the first boundary, when it is parsed, then it is rejected with a boundary error
- [ ] Given a document with an opening boundary and no closing one, when it is parsed, then it is rejected with a boundary error
- [ ] Given a document whose frontmatter is empty, when it is parsed, then the metadata is an empty mapping and the body is returned, rather than the parse failing
- [ ] Given a document whose frontmatter is a sequence or a scalar rather than a mapping, when it is parsed, then it is rejected with a mapping error
- [ ] Given a document starting with a UTF-8 byte order mark, when it is parsed, then the mark is stripped before the boundary is sought
- [ ] Given a document whose YAML is malformed, when it is parsed, then the error is wrapped as a skill parse error carrying the underlying reason, and no panic reaches the caller
- [ ] Given the `frontmatter` family of the corpus, when the replay runs, then every case conforms or is a named ledger entry

#### US-163: Validate the skill metadata schema the reference way
**Description:** As a parity maintainer, I want the schema to accept exactly the documents the reference accepts so that a skill that loads upstream loads here and one that is rejected upstream is rejected here.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-162
**Reference:** [vibe/core/skills/models.py:37-93](/home/arthur/dev/mistral-vibe/vibe/core/skills/models.py) for the field constraints, the two validation aliases, `parse_allowed_tools` and `normalize_metadata`

**Acceptance Criteria:**
- [ ] Given a name that is not `^[a-z0-9]+(-[a-z0-9]+)*$`, when it is validated, then it is rejected, covering uppercase, underscores, consecutive hyphens and leading or trailing hyphens
- [ ] Given a name of 64 characters and a name of 65, when both are validated, then the first is accepted and the second rejected
- [ ] Given a document with no `description`, an empty one, or one of 1025 characters, when it is validated, then each is rejected
- [ ] Given `user-invocable` and `user_invocable`, when either appears, then both are read into the same field, and when both appear the document is still parsed without panic
- [ ] Given `user-invocable: no`, `off`, `No` or `OFF`, when it is validated, then the field is false, reproducing the YAML 1.1 boolean vocabulary PyYAML resolves
- [ ] Given `allowed-tools` as a space-delimited string, as a list of strings, and as null, when each is validated, then the field is the split list, the list itself and an empty list respectively
- [ ] Given a `metadata` mapping whose values are integers, booleans or nulls, when it is validated, then every value is normalized to its string form and no key is dropped
- [ ] Given `compatibility` longer than 500 characters, when it is validated, then it is rejected
- [ ] Given the `metadata` family of the corpus, when the replay runs, then accepted and rejected match the reference verdict on every case with 0 wrongly accepted and 0 wrongly rejected

#### US-164: Carry the whole skill model and publish it on the wire
**Description:** As an editor integration author, I want every declared field carried through to the catalog so that what the protocol documents is what the response contains.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-163
**Reference:** [vibe/core/skills/models.py:96-142](/home/arthur/dev/mistral-vibe/vibe/core/skills/models.py) for `SkillInfo`, `skill_dir` and `from_metadata`, [vibe/app_server/models.py:478](/home/arthur/dev/mistral-vibe/vibe/app_server/models.py) for `SkillSummary` and [vibe/app_server/_projection.py:231](/home/arthur/dev/mistral-vibe/vibe/app_server/_projection.py) for the projection

**Acceptance Criteria:**
- [ ] Given `SkillDefinition`, when it is read, then it carries `license`, `compatibility`, `metadata`, `allowed_tools`, `source` and `scope` in addition to the six fields it already has
- [ ] Given a skill loaded from disk, when its path is read, then it is the resolved absolute path and its directory is the resolved parent, matching `skill_dir`
- [ ] Given a skill whose frontmatter name differs from its directory name, when it is loaded, then the frontmatter name wins and the mismatch is recorded as a warning rather than a rejection
- [ ] Given `skills/list`, when it responds, then every entry validates against the `SkillSummary` census with 0 missing required fields and 0 surplus aliases
- [ ] Given the `source` field, when a builtin, a disk skill and a registry skill are each projected, then the three literals `builtin`, `local` and `registry` are all reachable
- [ ] Given a skill with no file on disk, when it is projected, then the response omits the path rather than emitting an empty string

---

### EP-048: Discovery, Precedence and Filtering

Give discovery the five roots the reference walks, the configuration that feeds it, the filter that trims it, and the diagnostic that explains what it dropped.

**Definition of Done:** all five roots resolve in reference order with resolve-and-dedup; `skill_paths`, `enabled_skills` and `disabled_skills` are each read by a real code path proven by a behavior-change test; and a skill that fails to parse reaches `diagnostics/list`.

#### US-165: Resolve the five reference search roots
**Description:** As an operator, I want the documented skill directories read so that a skill placed where the documentation says it goes is found.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-161
**Reference:** [vibe/core/skills/manager.py:73-90](/home/arthur/dev/mistral-vibe/vibe/core/skills/manager.py) for the order and the dedup, [_harness_manager.py:119,146](/home/arthur/dev/mistral-vibe/vibe/core/config/harness_files/_harness_manager.py) for the user and project sets, [_local_config_files.py:29](/home/arthur/dev/mistral-vibe/vibe/core/paths/_local_config_files.py) for `.vibe/skills` and `.agents/skills`, [_agents_home.py:7](/home/arthur/dev/mistral-vibe/vibe/core/paths/_agents_home.py) for `~/.agents`

**Acceptance Criteria:**
- [ ] Given a project root holding `.agents/skills/probe/SKILL.md`, when the catalog is built with the project trusted, then the skill is published
- [ ] Given `~/.vibe/skills/probe/SKILL.md`, when the catalog is built, then the skill is published without the `extensions` path segment being involved
- [ ] Given `~/.agents/skills/probe/SKILL.md`, when the catalog is built, then the skill is published
- [ ] Given the same skill name in a configured path, a project root and a user root, when the catalog is built, then the configured one wins, then the project one, then the user one, matching the reference order
- [ ] Given two roots that resolve to the same directory through a symlink or a relative spelling, when the catalog is built, then it is walked once
- [ ] Given a legacy `~/.vibe/extensions/skills` directory, when the catalog is built, then its skills are still published, ranked after both documented user roots, and the deprecation is recorded in `CHANGELOG.md`
- [ ] Given an untrusted project, when the catalog is built, then no project root is walked, preserving the existing trust gate
- [ ] Given the `discovery` family of the corpus, when the replay runs, then every scenario conforms on the published set and its ordering

#### US-166: Read `skill_paths` from the merged configuration
**Description:** As an operator, I want `skill_paths` in my configuration to add skill directories so that the key the schema publishes changes what the agent can load.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-165
**Reference:** [vibe/core/skills/manager.py:76](/home/arthur/dev/mistral-vibe/vibe/core/skills/manager.py) for the read and the `is_dir` filter, [vibe_schema.py:375](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py) for the declaration, the concat merge and the path expansion

**Acceptance Criteria:**
- [ ] Given `skill_paths = ["/tmp/extra"]` in the merged document, when the catalog is built, then `/tmp/extra` is the first root walked
- [ ] Given a `skill_paths` entry that is not a directory or does not exist, when the catalog is built, then it is skipped silently and the remaining roots are still walked
- [ ] Given a relative `skill_paths` entry, when the catalog is built, then it resolves against the current working directory
- [ ] Given an entry containing `~`, when the catalog is built, then it expands to the home directory, matching the reference's before-validator
- [ ] Given the same key set at the user layer and the project layer, when the document merges, then both entries survive, matching the declared concat strategy
- [ ] Given the key changed between two catalog builds, when the second runs, then the published set changes accordingly, which is the behavior-change proof this story owes

#### US-167: Apply `enabled_skills` and `disabled_skills` with the reference precedence
**Description:** As an operator, I want to publish or withhold skills by pattern so that a workspace can narrow what the agent sees without deleting files.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-166
**Reference:** [vibe/core/skills/manager.py:58-71](/home/arthur/dev/mistral-vibe/vibe/core/skills/manager.py) for the precedence and [vibe/core/utils/matching.py:16](/home/arthur/dev/mistral-vibe/vibe/core/utils/matching.py) for the pattern vocabulary, which `crates/vibe-core/src/matching.rs:26` already reproduces as `NameFilter`

**Acceptance Criteria:**
- [ ] Given `enabled_skills` set, when the filter applies, then only matching skills are published and `disabled_skills` is ignored entirely, even when it names a skill that `enabled_skills` matched
- [ ] Given only `disabled_skills` set, when the filter applies, then matching skills are withheld and every other skill is published
- [ ] Given a glob pattern such as `search-*`, when the filter applies, then it matches case-insensitively
- [ ] Given a pattern prefixed `re:`, when the filter applies, then it is an anchored case-insensitive full match
- [ ] Given a `re:` pattern that does not compile, when the filter applies, then it matches nothing and the catalog still builds, following `NameFilter::invalid`
- [ ] Given a filter that withholds a skill, when `get_skill` is called for it by the `skill` tool, then it is not found, so filtering and lookup cannot disagree
- [ ] Given a builtin skill and a `disabled_skills` pattern matching it, when the filter applies, then it is withheld, matching the reference applying filters after seeding
- [ ] Given the `filtering` family of the corpus, when the replay runs, then every scenario conforms

#### US-168: Publish an unloadable skill as a diagnostic
**Description:** As an operator, I want a skill that fails to parse to tell me so that a typo in frontmatter is a message rather than a disappearance.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-163
**Reference:** [vibe/core/skills/manager.py:137-146](/home/arthur/dev/mistral-vibe/vibe/core/skills/manager.py) for the accumulation and [vibe/app_server/_projection.py:390](/home/arthur/dev/mistral-vibe/vibe/app_server/_projection.py) for the projection

**Acceptance Criteria:**
- [ ] Given a `SKILL.md` with invalid frontmatter, when the catalog is built, then the skill is absent and one issue naming its path and a reason is present
- [ ] Given three malformed skills, when the catalog is built, then three issues are accumulated rather than the first aborting the walk
- [ ] Given `diagnostics/list`, when it responds after a malformed skill was discovered, then the issue appears in the `issues` array
- [ ] Given the `skill` tool handler, when it builds its own catalog, then it does not discard the issue list, so an issue raised there is reachable
- [ ] Given a directory with no `SKILL.md`, when the catalog is built, then it is skipped with no issue, because absence is not an error
- [ ] Given a `SKILL.md` that cannot be read for an I/O reason, when the catalog is built, then an issue carries the I/O reason rather than a parse reason

---

### EP-049: The Builtin Skills

Seed the catalog the way the reference does, with names that cannot be overridden and bodies this repository is allowed to ship.

**Definition of Done:** a builtin catalog lives in `vibe-core` and reaches both the wire and the `skill` tool; both builtins are published with `source: "builtin"`; a disk skill cannot take a builtin name; the custom count excludes them; and the prose divergence is recorded with a test that fails if the wording ever conforms.

#### US-169: Seed the catalog with a builtin skill set and reserve its names
**Description:** As an editor integration author, I want builtin skills present in the catalog so that the `builtin` source the protocol declares is reachable and the reserved-name rule is real.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-164
**Reference:** [vibe/core/skills/builtins/__init__.py:7](/home/arthur/dev/mistral-vibe/vibe/core/skills/builtins/__init__.py) for the map and [vibe/core/skills/manager.py:93,119](/home/arthur/dev/mistral-vibe/vibe/core/skills/manager.py) for the seeding and the reservation

**Acceptance Criteria:**
- [ ] Given the builtin catalog, when it is declared, then it lives in `vibe-core` so that the `skill` tool handler and the app-server catalog read the same set
- [ ] Given a catalog built with no skills on disk, when it is read, then both builtins are present
- [ ] Given a disk skill whose frontmatter name is `vibe`, when the catalog is built, then the builtin body is published and the disk one is skipped
- [ ] Given the `vibe` builtin, when it is projected, then `userInvocable` is false and no path is emitted
- [ ] Given the `skill-creator` builtin, when it is projected, then `userInvocable` is true
- [ ] Given the `skill` tool invoked with a builtin name, when it runs, then the body is returned and no base directory line is rendered, because a builtin has no directory on disk
- [ ] Given `/vibe` typed at the composer, when it is submitted, then it is not treated as a skill invocation, because the skill is not user invocable

#### US-170: Write both builtin bodies as original prose and ledger the divergence
**Description:** As a parity maintainer, I want the builtin bodies written in this repository's own words so that the agent gains the same self-knowledge without a byte of upstream prose entering the tree.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-169
**Reference:** [vibe/core/skills/builtins/vibe.py](/home/arthur/dev/mistral-vibe/vibe/core/skills/builtins/vibe.py) and [builtins/skill_creator.py](/home/arthur/dev/mistral-vibe/vibe/core/skills/builtins/skill_creator.py), read for their **directive coverage only**. `NOTICE` forbids reproducing any of their 40 589 bytes

**Acceptance Criteria:**
- [ ] Given the `vibe` builtin body, when it is read, then it covers the same directives as the reference: the home directory and its override, the directory layout, configuration file locations and precedence, models and providers, agents and subagents, skills, tools and their permission model, slash commands and CLI flags, hooks, MCP servers, connectors, trusted folders, file mentions, logs, themes and voice
- [ ] Given the `vibe` builtin body, when it names a documentation URL, then the URL is pinned to this port's own `[workspace.package] version` rather than to a fixed release, so it never advertises a version that is not running
- [ ] Given the `skill-creator` builtin body, when it is read, then it covers requirement gathering one question at a time, the `SKILL.md` shape, the five discovery locations with their precedence, the builtin name reservation, support files, and the permission flow for writing under a skills directory
- [ ] Given both bodies, when they are compared against the reference digests recorded in the corpus, then neither length nor SHA-256 matches, and the ledger entry `builtinProse` records why
- [ ] Given the ledger entry, when a future change makes either body conform to the reference digest, then the replay fails, so the divergence can never be closed by copying
- [ ] Given `docs/parity.md`, when this story lands, then the accepted-divergences table carries a row for builtin skill prose naming the constant that holds it in place

#### US-171: Count custom skills only
**Description:** As an operator, I want the banner to report the skills I added so that seeding two builtins does not inflate a number that used to mean something.

**Priority:** P2
**Size:** XS (1 pt)
**Dependencies:** Blocked by US-169
**Reference:** [vibe/core/skills/manager.py:169](/home/arthur/dev/mistral-vibe/vibe/core/skills/manager.py) for the definition and [vibe/app_server/client_state.py:44](/home/arthur/dev/mistral-vibe/vibe/app_server/client_state.py) for the read the banner performs

**Acceptance Criteria:**
- [ ] Given a catalog with two builtins and no disk skills, when the banner renders, then the skill count is zero
- [ ] Given a catalog with two builtins and three disk skills, when the banner renders, then the count is three
- [ ] Given a disk skill withheld by `disabled_skills`, when the banner renders, then it is not counted, because the count reads the filtered catalog

---

### EP-050: The Invoked Skill in the Conversation

Make a slash-invoked skill produce the conversation the reference produces, so the transcript, the persisted history and the already-loaded answer all agree with upstream.

**Definition of Done:** invoking `/name` appends a synthetic assistant tool call plus its tool result to history; the dedup marker is honored on the second invocation; `injectInvokedSkill` decides whether it happens; and the CLI no longer sends a `skill://` resource block.

#### US-172: Append the synthetic skill call pair to history
**Description:** As a model, I want an operator's `/skill-name` to arrive as a `skill` tool call and result so that the conversation reads the same whether the user or I loaded it.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-164
**Reference:** [vibe/core/agent_loop/_loop.py:1694-1766](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py) for the pair, the call id, the argument encoding and the field-per-line result text, [_loop.py:1687](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py) for the marker search, and [vibe/core/tools/builtins/skill.py:97](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/skill.py) for `select_skill_result`

**Acceptance Criteria:**
- [ ] Given a user message of exactly `/name` where `name` is a user-invocable skill, when the turn starts, then history gains an assistant message whose only content is one `skill` tool call, followed by a tool message carrying the rendered result
- [ ] Given the tool call, when it is inspected, then its arguments are the JSON object `{"name": "<skill>"}` and its id is minted locally in the UUID shape `crates/vibe-core/src/session_id.rs:27` already produces
- [ ] Given `/name extra instructions here`, when the turn starts, then the skill is still resolved from the first word and the remaining text stays the user's message
- [ ] Given `/NAME`, when the turn starts, then the lookup is case-insensitive, matching `parse_skill_command`
- [ ] Given a slash word naming no skill, or naming one that is not user invocable, when the turn starts, then no pair is appended and the message is an ordinary prompt
- [ ] Given the same skill invoked twice in one conversation, when the second invocation runs, then the tool message carries the already-loaded sentence and not the body, decided by searching the stored tool messages for `<skill_content name="...">`
- [ ] Given the tool message, when it is rendered for the model, then it is the result fields one per line as `key: value`, matching the reference's `model_dump` join
- [ ] Given a transcript persisted before this change, when it is replayed, then it still deserializes, and the new pair round-trips through the store unchanged

#### US-173: Honor `injectInvokedSkill` on the two wire methods
**Description:** As an editor integration author, I want the flag I send to decide whether the skill is injected so that a client that resolves skills itself is not double-served.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-172
**Reference:** [vibe/app_server/_turns.py:303,339](/home/arthur/dev/mistral-vibe/vibe/app_server/_turns.py) for both call sites and [vibe/app_server/session.py:347](/home/arthur/dev/mistral-vibe/vibe/app_server/session.py) for the default the client sends

**Acceptance Criteria:**
- [ ] Given `turn/steer` with `injectInvokedSkill` true and an input naming a user-invocable skill, when it is handled, then the pair is appended
- [ ] Given the same call with the flag false, when it is handled, then no pair is appended and the input is carried unchanged
- [ ] Given `context/inject` with the flag true, when it is handled, then the pair is appended
- [ ] Given either method, when the flag is omitted, then the declared default applies and the census still validates the params model
- [ ] Given the flag set on a message that names no skill, when it is handled, then nothing is injected and no error is raised
- [ ] Given `crates/vibe-app-server/src/server.rs:4529,4546`, when this story lands, then neither field carries `#[allow(dead_code)]` any longer

#### US-174: Retire the CLI resource block
**Description:** As an operator, I want the terminal client to stop sending its own skill payload so that one path produces the conversation and the transcript shows what actually happened.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-173
**Reference:** No reference counterpart. The reference client sends the prompt and lets the server inject, which is what [vibe/app_server/session.py:347](/home/arthur/dev/mistral-vibe/vibe/app_server/session.py) shows. The local target is `crates/vibe-cli/src/tui/prompt.rs:105-119`

**Acceptance Criteria:**
- [ ] Given `/name` submitted in the TUI, when the turn starts, then the request carries no `skill://` resource block and the server performs the injection
- [ ] Given the same submission, when the transcript renders, then it shows a `skill` tool call settling to the loaded state, using the existing `ToolEffectKind::Skill` rendering
- [ ] Given the slash completion menu, when it opens, then it still lists only user-invocable skills, so this change does not alter what is offered
- [ ] Given a submission classified as `Submission::Skill`, when it is queued rather than started, then the classification survives the queue and the injection happens when it runs
- [ ] Given the existing chat-input and TUI parity traces, when the suite runs, then every one still passes, or the trace is recaptured in the same change with the reason recorded

---

### EP-051: The Remote Registry and the Scorecard

Port the four dormant registry modules faithfully, keep them as unreachable as they are upstream, and close the PRD by remeasuring the scorecard from the oracle.

**Definition of Done:** the payload models, the store, the manifests and the client are ported with their safety rules intact; nothing outside the experiment key can reach them; no wire method or CLI command is invented; and `docs/parity.md` carries the Skills row, the rank 11 status and the new divergences, all cited from printed counts.

#### US-175: Port the registry payload models and the name sanitizer
**Description:** As a parity maintainer, I want the registry's wire vocabulary modeled so that a catalog response is read into the same values the reference reads.

**Priority:** P2
**Size:** M (3 pts)
**Dependencies:** Blocked by US-161
**Reference:** [vibe/core/skills/registry/models.py:14,33,106,125,141,178](/home/arthur/dev/mistral-vibe/vibe/core/skills/registry/models.py) for the sanitizer, the asset decode, the item, both resolvers and the version row

**Acceptance Criteria:**
- [ ] Given a payload in camel case and the same payload in snake case, when each is read, then both populate the same fields, and an unknown field is ignored rather than rejected
- [ ] Given a raw name with spaces, punctuation or mixed case, when it is sanitized, then non-alphanumeric runs collapse to single hyphens, the result is lowercased, trimmed of hyphens, capped at 64 characters and re-trimmed, and an empty result is `None`
- [ ] Given an item with no metadata name, when its name resolves, then it falls back to the payload name, then the attribute title, then a sanitized `skill-<id>` built from the full id with hyphens removed
- [ ] Given an item with no identifier of any kind, when its name resolves, then the result is `None` and the item is skipped rather than published under a generated placeholder
- [ ] Given an asset carrying `textContent`, one carrying base64 `rawContent`, one carrying invalid base64 and one carrying neither, when each decodes, then the results are the UTF-8 bytes, the decoded bytes, `None` and `None`
- [ ] Given a versions response, when it is read, then rows carry their author aliases and the list sorts by version descending
- [ ] Given the `registry` family of the corpus, when the replay runs, then every model case conforms

#### US-176: Port the version store with its atomic write and its asset safety
**Description:** As an operator, I want a cached registry skill written safely so that a failed download never leaves a half-written skill and a hostile asset path cannot escape its directory.

**Priority:** P2
**Size:** L (5 pts)
**Dependencies:** Blocked by US-175
**Reference:** [vibe/core/skills/registry/_store.py:26,82,124,142,152,173,197,221](/home/arthur/dev/mistral-vibe/vibe/core/skills/registry/_store.py) for the id guard, the staged write, the generated frontmatter, the embedded-frontmatter strip, the asset write, the destination guard, the local export and the prune

**Acceptance Criteria:**
- [ ] Given a skill id that is empty, `.`, `..` or contains a path separator, when a store path is resolved, then it is rejected before any filesystem call
- [ ] Given a materialize that fails partway, when the store is inspected, then the previous version is intact and no staging directory remains
- [ ] Given a skill whose body is empty, when it is materialized, then nothing is written, any prior cache for that version is removed, and the result reports that nothing was stored
- [ ] Given a body that already carries frontmatter, when it is materialized, then the embedded frontmatter is stripped and the generated one carries the name, the resolved description and the source, id and version metadata
- [ ] Given an asset path containing `..`, an absolute path, or one resolving to the skill directory itself, when it is written, then it is skipped
- [ ] Given an asset that normalizes to `SKILL.md` or `skills.md` in the skill root, in any case, when it is written, then it is skipped so the generated entrypoint cannot be overwritten
- [ ] Given an asset marked executable, when it is written, then the owner execute bit is set and neither the group nor the world bit is
- [ ] Given a re-materialize of the same version, when it completes, then assets from the previous materialization that are absent from the new payload are gone
- [ ] Given an active set of id and version pairs, when the store is pruned, then every version outside it is removed and an id directory left empty is removed too
- [ ] Given a materialized version exported locally, when the export is read, then the registry metadata is gone and the remaining frontmatter carries only the name and the description

#### US-177: Port the two manifest scopes and the alias pin
**Description:** As an operator, I want installed registry skills recorded per scope so that a project pins what it needs without changing my global set.

**Priority:** P2
**Size:** M (3 pts)
**Dependencies:** Blocked by US-175
**Reference:** [vibe/core/skills/registry/_manifest.py:16,26,36,40,47,63,78,95](/home/arthur/dev/mistral-vibe/vibe/core/skills/registry/_manifest.py) for the entry, the alias property, the upsert, the removal, the two path resolvers and the load and save

**Acceptance Criteria:**
- [ ] Given an entry whose version is the string `latest`, when its alias is read, then it is `latest`, and given an integer version, then the alias is absent
- [ ] Given an entry upserted twice under the same name, when the manifest is read, then it holds one entry and the second write won
- [ ] Given a manifest saved and loaded, when the two are compared, then the round trip is lossless, including an alias pin and an empty description
- [ ] Given a manifest path whose parent does not exist, when it is saved, then the parent is created
- [ ] Given a missing manifest file, when it is loaded, then an empty manifest is returned rather than an error
- [ ] Given a malformed manifest file, when it is loaded, then an empty manifest is returned and a warning is logged, so a corrupt file never blocks startup
- [ ] Given a project root whose manifest path resolves to the global manifest, when project paths are collected, then it is dropped, so project scope is never an alias for global
- [ ] Given two project roots resolving to the same path, when project paths are collected, then it appears once

#### US-178: Port the paginated catalog client and its error taxonomy
**Description:** As a parity maintainer, I want the catalog client to make the same requests and fail the same way so that a future feature built on it inherits reference behavior rather than a local approximation.

**Priority:** P2
**Size:** L (5 pts)
**Dependencies:** Blocked by US-175
**Reference:** [vibe/core/skills/registry/_client.py:17,39,70,73,80,93,117](/home/arthur/dev/mistral-vibe/vibe/core/skills/registry/_client.py) for the page cap, the field mask, the three operations, the pagination loop and the status mapping

**Acceptance Criteria:**
- [ ] Given a catalog listing, when the request is built, then it carries the reference field mask verbatim and the requested page size
- [ ] Given a response carrying a next page token, when the listing continues, then the token is sent as `pageToken` and items accumulate across pages
- [ ] Given a response chain that never terminates, when 50 pages have been read, then the listing fails with a page-cap error rather than returning a silently truncated catalog
- [ ] Given a request for one skill with a version, with an alias, and with neither, when each is built, then the parameters are `version`, `alias` and nothing respectively, never both at once
- [ ] Given a 401 or a 403, when the response is handled, then the error names it unauthorized with the status; given a 404, then it names not found; given any other unsuccessful status, then it names the unexpected status
- [ ] Given a transport failure, when the request is made, then it surfaces as a registry error carrying the underlying reason and never as a panic
- [ ] Given a 200 whose body is not valid JSON, or whose JSON does not match the model, when it is parsed, then it surfaces as a registry error naming which payload failed
- [ ] Given the client, when it is used outside an established session, then it errors rather than constructing a transport implicitly

#### US-179: Gate the subtree on its experiment key and remeasure the scorecard
**Description:** As a parity maintainer, I want the registry to stay as unreachable as it is upstream and the scorecard to state the measured number so that no claim in `docs/parity.md` outruns its oracle.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-176, US-177, US-178, US-170, US-174, US-168
**Reference:** [vibe/core/config/vibe_schema.py:399](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py) for the key, whose single occurrence in the whole reference tree is its own declaration

**Acceptance Criteria:**
- [ ] Given `experimental_enable_registry_skills` false, which is the default, when a session starts, then no registry code runs, no cache directory is created and no network call is made
- [ ] Given the key true and no registry configured, when a session starts, then the catalog is unchanged and no error is surfaced
- [ ] Given the app-server surface oracle, when it runs after this epic, then it reports 0 invented method names, so no `skills/install` or equivalent has been added
- [ ] Given `docs/parity.md`, when it is read after this story, then the Skills row cites the counts `skills_parity_tests` prints, the execution-order row for rank 11 is updated, and the accepted-divergences table carries the builtin prose row and the dormant-registry row
- [ ] Given the `Last remeasure` field of the scorecard, when it is read, then it names this oracle and the date, following the format the compaction remeasure established
- [ ] Given `CHANGELOG.md`, when it is read, then the `## Unreleased` section records the user-visible changes: the documented skill directories now read, the frontmatter now parsed as YAML, the two builtins, and the invoked-skill transcript entry

## Functional Requirements

1. The system must parse `SKILL.md` frontmatter as YAML behind the reference's boundary rules, and reject exactly the documents the reference rejects.
2. The system must validate skill metadata against the reference's constraints, accepting both the hyphenated and the underscore spelling of `user-invocable` and `allowed-tools`.
3. The system must resolve the YAML 1.1 boolean vocabulary for boolean-bearing fields.
4. The system must carry all twelve declared fields from frontmatter to catalog to wire.
5. The system must walk the five reference search roots in reference order, resolving and deduplicating, and must not walk an invented path except the recorded legacy root.
6. The system must read `skill_paths`, `enabled_skills`, `disabled_skills` and `experimental_enable_registry_skills` from the merged configuration document.
7. The system must apply `enabled_skills` with precedence over `disabled_skills`, using the existing `NameFilter` vocabulary.
8. The system must seed the catalog with builtin skills whose names cannot be taken by a disk skill.
9. The system must publish `source` as `builtin`, `local` or `registry`, and must count only custom skills in the banner.
10. The system must append a synthetic `skill` tool call and its result to history when a user invokes a skill by slash command, honoring `injectInvokedSkill`.
11. The system must answer a second invocation of an already-loaded skill with the already-loaded result rather than the body.
12. The system must surface a skill that fails to load as a diagnostic naming its path and reason.
13. The system must implement the registry client, store and manifests with their reference safety rules, reachable only when the experiment key is enabled.
14. The system must replay a committed corpus on every `cargo test` and fail on any divergence outside a named ledger.

## Non-Functional Requirements

1. Catalog construction over a tree of 100 skills completes in under 50 ms on the reference workstation, measured by a criterion-free timing assertion in the discovery tests.
2. A malformed `SKILL.md` of any size, including one crafted to exhaust a parser, must not panic and must not exceed the existing `MAX_EXTENSION_FILE_BYTES` read bound.
3. The corpus replays at least 120 scenarios across 8 families and adds no more than 400 ms to `cargo test --workspace --all-features`.
4. Exactly one new workspace dependency is added, and it carries no `unsafe` in its own source.
5. Registry assets are written with the owner execute bit only; no group or world execute bit is ever set.
6. No registry network call is made when the experiment key is false, asserted by a test that fails if any transport is constructed.

## Edge Cases & Error States

| Case | Expected behavior |
|---|---|
| Frontmatter delimited by more than three hyphens | Accepted, matching `^-{3,}\s*$` |
| Text before the opening boundary | Rejected with a boundary error |
| Opening boundary with no closing one | Rejected with a boundary error |
| Empty frontmatter document | Empty mapping, body returned, no error |
| Frontmatter that is a sequence or scalar | Rejected with a mapping error |
| Leading UTF-8 byte order mark | Stripped before the boundary is sought |
| `user-invocable: no` | False, through the YAML 1.1 resolver |
| Both `user-invocable` and `user_invocable` present | Parsed without panic, one value wins deterministically |
| `allowed-tools` as a null | Empty list |
| `metadata` values that are not strings | Coerced to their string form, no key dropped |
| Name 65 characters long, or with an underscore | Rejected |
| Frontmatter name differs from directory name | Frontmatter name wins, warning recorded |
| Two skills with the same name in one root | First by sorted directory name wins |
| Same name in two roots | Earlier root wins |
| Disk skill named after a builtin | Builtin wins, disk skill skipped silently |
| `skill_paths` entry that does not exist | Skipped, remaining roots still walked |
| Symlinked root resolving to an already-walked directory | Walked once |
| Untrusted project | No project root walked |
| `re:` pattern that does not compile | Matches nothing, catalog still builds |
| Skill withheld by filter then requested by the `skill` tool | Not found |
| `/name` where name is not user invocable | Ordinary prompt, no injection |
| Same skill invoked twice | Second answers already-loaded |
| Registry skill id containing a separator | Rejected before any filesystem call |
| Registry asset path escaping the skill directory | Skipped |
| Registry asset normalizing to the entrypoint name | Skipped |
| Registry response chain exceeding 50 pages | Page-cap error, never a truncated catalog |
| Registry 200 with a malformed body | Registry error naming the payload |
| Malformed skills manifest on disk | Empty manifest, warning logged, startup proceeds |
| Reference checkout absent or off pin | Corpus still replays; only the live probe skips |

## Risks & Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| The chosen YAML crate diverges from PyYAML on a construction a user already wrote | Medium | High | The `frontmatter` family carries the constructions explicitly, including the 1.1 boolean vocabulary; any residual divergence is normalized on both sides or ledgered, never left silent |
| Moving the user skills root strands an existing installation | Low | Medium | US-165 keeps the legacy root as a lower-ranked additional root and records the deprecation, so nothing stops loading on the day of the change |
| Tightening name validation rejects skills that load here today | Medium | Medium | The rejection is the parity contract, so it stands, but US-168 makes every rejection a diagnostic naming the file and the reason instead of a silent disappearance |
| Builtin prose drifts toward the reference wording over time | Low | High | The ledger entry fails the replay if either body's digest ever matches, which makes conformance a test failure rather than a review question |
| Changing the invoked-skill path breaks existing TUI parity traces | Medium | Medium | US-174 requires either every trace to pass or the trace to be recaptured in the same change with the reason recorded |
| The registry ports 661 lines that nothing calls, and rots | Medium | Low | Its conformance is asserted by the corpus rather than by a live caller, so a regression is a test failure, not a silent decay; and its dormancy is the reference's own state, recorded as such |
| The corpus grows large enough to slow the suite | Low | Low | Reference-authored prose is stored as digests, and the non-functional budget caps the added time at 400 ms |

## Non-Goals

- Re-pinning the reference. The pin already names `b78b451` and this PRD does not move it.
- Inventing a wire surface for the registry. No `skills/install`, `skills/uninstall` or `skills/registry/*` method is added, because the reference publishes none and the app-server oracle fails on invented names.
- Implementing `allowed_tools` as a permission filter. The field is declared and unread upstream, so parity is carrying it, not acting on it.
- Implementing `SkillScope` as a behavior. It is declared and never populated upstream; it is carried as a field.
- Porting the reference's logging text. Log messages are held to naming the same cause, following the settled treatment of warning prose.
- A skills marketplace, a skill installer UI, or any feature the reference does not have.
- Changing the `skill` tool's rendering, its file sampling limit or its permission grant, all of which already conform.

## Files NOT to Modify

- `crates/vibe-protocol/src/lib.rs` beyond what a census-validated field requires. The routed inventory and `LOCAL_EXTENSION_METHODS` are settled surface.
- `crates/vibe-core/src/parity.rs` and `scripts/parity/pin.py`. The pin does not move in this PRD.
- `crates/vibe-app-server/tests/app-server-surface/corpus.json` and the other committed corpora, except the new skills corpus. A corpus is regenerated only by its own capture script.
- `crates/vibe-core/src/matching.rs`. `NameFilter` already reproduces the reference vocabulary and is consumed as is.
- `NOTICE`. The licensing boundary is a constraint on this work, not a target of it.
- Any file under the reference checkout. It is read-only.

## Technical Considerations

- **Layering.** The parser, the schema, discovery, the builtin catalog and the registry belong in `vibe-core`. Only the projection and the wire flag belong in `vibe-app-server`. The CLI change in US-174 is a deletion, which is what an adapter change should look like.
- **Where the builtins live.** `builtin_agents.rs` sits in `vibe-app-server` because only the app-server registers agents. Skills cannot follow it: `crates/vibe-core/src/tools/builtins.rs:539` builds its own catalog for the `skill` tool, so a set seeded only in the app-server would make the tool and `skills/list` disagree about what exists.
- **The tool call id.** The reference mints `uuid4()`. This workspace has no uuid crate and does not need one: `crates/vibe-core/src/session_id.rs:27` already produces a UUID-shaped identifier from `getrandom` with a clock fallback. US-172 reuses it rather than adding a dependency.
- **Configuration reads.** There is no typed accessor: `ConfigSnapshot.effective` is a `toml::Table` read through `get(key)` and `as_bool`, `as_str` or `as_array`, with `string_array` as the private list helper (`crates/vibe-core/src/config.rs:255`). The four skill keys follow `enabled_tools` at `config.rs:242`.
- **Discovery is per session.** The catalog is rebuilt per session because a project may ship its own skills (`crates/vibe-core/src/tools/builtins.rs:180`). The five roots multiply that work, which is what the 50 ms budget in the non-functional requirements bounds.
- **The corpus is the contract.** Following `compaction_parity_tests.rs`, the ledger is keyed `family/case`, `settle()` fails both unrecorded divergences and stale entries, and reference-authored prose is committed as length plus SHA-256 only.

## Success Metrics

| Metric | Baseline | Target | Timeframe |
|---|---|---|---|
| `docs/parity.md` Skills score | 55, from module presence | 100, from the oracle | End of EP-051 |
| Conforming corpus scenarios | 0 measured | at least 120 across 8 families, 0 divergent outside the ledger | End of EP-051 |
| Metadata verdicts matching the reference | Unmeasured | 0 wrongly accepted, 0 wrongly rejected | End of EP-047 |
| Search roots resolved | 1 of 5, at an invented path | 5 of 5, 0 invented | End of EP-048 |
| Skill configuration keys with a consumer | 0 of 4 | 4 of 4, each with a behavior-change test | End of EP-048 |
| Reachable `source` literals | 1 of 3 | 3 of 3 | End of EP-051 |
| Invoked-skill history entries matching the reference shape | 0 of 2 | 2 of 2 | End of EP-050 |
| New workspace dependencies | 0 | exactly 1 | End of EP-047 |

## Open Questions

1. Should the legacy `~/.vibe/extensions/skills` root be removed in a later release, and if so, on what version boundary? US-165 keeps it and records the deprecation; the removal is deliberately left out of this PRD.
2. Does any provider adapter reorder or drop an assistant message whose content is empty and whose only payload is a tool call? US-172's persistence criterion is the falsifier; if one does, the content becomes a single space rather than the shape changing.
3. When `experimental_enable_registry_skills` gains a consumer upstream, does it read the catalog at startup or lazily on first use? Unanswerable at this pin, and recorded so the port does not guess a lifecycle the reference has not published.
[/PRD]
