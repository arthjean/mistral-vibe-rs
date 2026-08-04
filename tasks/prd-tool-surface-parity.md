[PRD]
# PRD: Tool Surface Parity

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-04 | Arthur Jean | Initial PRD from the measured tool-surface audit against the Python reference: 26 reference tool names, 3 present in Rust, 0 schema-conformant, plus 5 structural blockers |

## Problem Statement

1. The Rust port publishes 5 agent-facing builtin tools where the Python reference publishes 26. Measured by introspecting the reference package directly (`BaseTool.get_name()` over `vibe/core/tools/builtins`), 23 names are absent: the whole shell surface (`bash`, `bash_output`, `bash_stdin`, `bash_sessions`, `bash_log_file` and their `git_bash_*` and `powershell_*` counterparts), plus `read_file`, `grep`, `write_file`, `skill`, `task`, `todo`, `web_fetch`, `web_search`.
2. Of the 3 names that do exist, none carries a conformant schema. `edit` uses camelCase keys (`path`, `oldText`, `newText`, `replaceAll`) against the reference snake_case (`file_path`, `old_string`, `new_string`, `replace_all`). `exit_plan_mode` adds an `additionalProperties: false` the reference does not emit. `ask_user_question` inlines what the reference publishes as `$defs`/`$ref`, drops every property `description` and every `default`, and adds `minLength` constraints that do not exist upstream.
3. Two Rust tool names have no counterpart at all. `read` and `search` (`crates/vibe-core/src/workspace.rs:817`, `:838`) semantically shadow `read_file` and `grep` but are invented locally, so a model prompted for reference behavior calls tools that do not exist and never calls tools that do.
4. Five structural mechanisms make the gap self-reproducing. `object_schema` (`crates/vibe-core/src/tools.rs:666`) unconditionally injects `additionalProperties: false` and an always-present `required`, so every schema built through it is non-conformant before any field is written. Tool descriptions are one-line strings where the reference ships 13.8 KB of operational prompt text resolved by `available_tool_specs` (`/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:614`). `validate_value` (`tools.rs:588`) understands neither `$ref`, `anyOf`, `items`, array-form `type`, nor default application. MCP tools are published as `mcp_{alias}_{tool}` (`remote_tools.rs:52`) where the reference publishes `{alias}_{tool}` (`/home/arthur/dev/mistral-vibe/vibe/core/tools/mcp/tools.py:214`). Tool filtering compares names by exact `BTreeSet` membership (`tools.rs:287`) where the reference matches globs, `re:` prefixes, and is case-insensitive (`/home/arthur/dev/mistral-vibe/vibe/core/utils/matching.py:16`).
5. No test in the repository asserts anything about the published tool names or schemas. `cargo test` passes at full green with a surface that is 88% absent.

**Why now:** every subsequent behavioral parity effort is downstream of the tool surface. A model cannot exercise a runtime path it has no tool to reach, so the completed chat-input and TUI-runtime parity work (`tasks/prd-chat-input-observable-parity.md`, `tasks/prd-tui-runtime-observable-parity.md`) currently validates a client driving an agent that can barely act. The differential-oracle infrastructure those efforts built (`scripts/parity/oracle.py`, pinned reference checkout, conditional probe) is already in place and directly reusable for tool definitions.

## Overview

This initiative makes the Rust tool surface interface-equivalent to the Python reference. Equivalence is defined precisely and narrowly: for the same platform and configuration, the set of published tool names is identical, and for each name the `parameters` object sent in `tools[].function.parameters` is semantically identical to the reference `model_json_schema()` output after key canonicalization. Descriptions must cover the same operational directives but are original text, for the licensing reason stated below. Execution behavior must be correct and useful, but is not held to observable-trace parity in this PRD.

The work is sequenced so that conformance is structural rather than per-tool. The first epic replaces the schema emission path, extends argument validation to the reference vocabulary including default application, and stands up a differential oracle that captures the reference corpus and diffs it against `ToolRegistry::list()`. Every later story is then verified by that oracle rather than by hand-checked assertions. The second epic converts the five existing tools and the two remote naming rules. The third adds the universal tools that need no shell runtime. The fourth and fifth add the shell families, reusing `TerminalManager` (`crates/vibe-core/src/process.rs:101`), whose run/write/close_stdin/read/wait/list/interrupt/release surface already matches most managed-session semantics. The last epic restores conditional availability, rollout-based variant selection, glob filtering, and locks the result in CI.

The reference is the checkout at `/home/arthur/dev/mistral-vibe`, pinned for this PRD at commit `68ff32e6a92e80a874c8153312f0aa8ae4955477` (v2.23.3, 2026-08-03), which is the tree every measurement in this document was taken from. The tool surface lives in [vibe/core/tools](/home/arthur/dev/mistral-vibe/vibe/core/tools) and splits into four parts that every story navigates back to: [base.py](/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py) defines naming, schema emission, and argument validation; [manager.py](/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py) resolves availability, variant selection, filtering, and the model-facing definition list; [builtins/](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins) holds the 26 published tools and their sibling [builtins/prompts/](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/prompts) descriptions; [mcp/](/home/arthur/dev/mistral-vibe/vibe/core/tools/mcp) and [connectors/](/home/arthur/dev/mistral-vibe/vibe/core/tools/connectors) own remote naming. Three argument models sit outside that directory and are still part of the contract: [vibe/questions.py:15](/home/arthur/dev/mistral-vibe/vibe/questions.py:15) supplies `ask_user_question` with both its `extra="forbid"` and its camelCase alias generator, and [vibe/core/subagents.py:16](/home/arthur/dev/mistral-vibe/vibe/core/subagents.py:16) supplies `task`. Every epic and every story below carries a source-navigation line into these files so an implementing agent reads the reference before writing Rust.

One constraint shaped the whole plan. `NOTICE` declares that no upstream implementation source is copied, translated, vendored, linked, or shipped. The 17 reference `prompts/*.md` description files therefore cannot be embedded, and description parity is specified as directive coverage rather than textual identity. The reference corpus is still captured by the oracle, but only as a locally generated, gitignored test artifact, never as shipped content.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Publish the reference tool name set | 16 of 16 Linux-visible reference names published, 0 invented names remaining | 26 of 26 names published across Linux and Windows with correct per-platform gating |
| Make every published schema conformant | 100% of published schemas byte-identical to the canonicalized reference schema for the same name | 100% maintained with zero accepted divergence entries |
| Make conformance mechanically enforced | Differential oracle runs locally and fails on any name or schema divergence | Oracle gate wired into CI, no tool may be added without a corpus entry |
| Preserve argument-handling semantics | `anyOf`, `$ref`, `items`, array-form `type`, and default application supported by the validator | 0 arguments accepted by Rust that Pydantic rejects, measured across the corpus fixture set |
| Keep the surface honest | 0 registered tools whose handler cannot execute | 0 maintained, enforced by a registration-time smoke assertion |

## Target Users

### Vibe operator switching between clients

- **Role:** Developer running the Python client and the Rust client interchangeably on the same repository, with the same configuration and the same prompts.
- **Behaviors:** Asks the agent to read files, grep, edit, write, run shell commands, manage a todo list, fetch pages, search the web, and delegate to subagents. Reuses saved prompts, agent profiles, and `disabled_tools` configuration across both clients.
- **Pain points:** The same prompt produces a different plan because the Rust agent has no shell, no write tool, and no web access. Configuration entries naming `read_file` or `bash` silently match nothing. Agent profiles that disable `exit_plan_mode` work, profiles that disable `write_file` do not.
- **Current workaround:** Return to the Python client for any task that needs more than a bounded read, a search, and a single-span edit.
- **Success looks like:** The same prompt in either client yields the same available actions, and configuration written for one client applies unchanged to the other.

### Rust port maintainer

- **Role:** Engineer adding or reviewing tools in `vibe-core` and their registration through `SessionToolFactory`.
- **Behaviors:** Adds a tool spec, a handler, a policy requirement, and a presentation mapping; runs `cargo test`.
- **Pain points:** Nothing in the test suite states what the surface should be, so a non-conformant schema ships green. The compatibility tables in `extensions.rs:272` and `transcript.rs:36` encode two contradictory naming conventions at once, and neither is authoritative.
- **Current workaround:** Manually diff against the Python checkout, which is exactly the work this PRD automates.
- **Success looks like:** A failing oracle names the first divergent tool, the divergent JSON pointer, and the expected value.

## Research Findings

Key findings that informed this PRD:

### Competitive Context

- **Mistral function calling API:** the documentation specifies only `type: object`, `properties`, `required`, `description`, and `enum` for `tools[].function.parameters`. No JSON Schema subset is published, and no strict mode is documented for tool parameters (a `strict` flag exists only for `response_format: json_schema`). Inference, to be validated: schemas reach the model as best-effort context, so reference-shaped constructs carry no rejection risk.
- **OpenAI strict function calling** (comparison point): supports `$defs`/`$ref` and array-form `type`, forbids `allOf`, `not`, `if/then/else`, and ignores `minLength`, `minItems`, `default`. The reference schemas stay inside this envelope, so conformance does not paint the port into a provider corner.
- **Market gap:** no published tooling canonicalizes and diffs tool definitions between two implementations of the same agent. The oracle built here is repository-specific by necessity.

### Best Practices Applied

- Schema quality measurably drives tool-selection accuracy. The Composio benchmark reported in the Quotient AI literature review moves from roughly 33% to 74% accuracy across schema optimizations covering descriptions, types, and enums. This is why description directive coverage is a hard acceptance criterion and not a nicety.
- Key order is guaranteed by neither Pydantic nor `serde_json`. Comparison must canonicalize both sides (sorted keys) rather than rely on insertion order.
- `schemars` 1.x `draft2020_12()` matches Pydantic on `$defs` and `anyOf`-null but adds `title`, root `$schema`, and numeric `format`. Its `Transform` trait can strip them, but for 2-to-8-field argument schemas the transform code exceeds the literal it replaces.
- The repository already owns the correct verification pattern: a pinned reference checkout, a Python capture script, a versioned corpus, and a Rust differential runner with a conditional probe when the checkout is absent.

*Full research sources available in project documentation.*

## Assumptions & Constraints

### Assumptions (to validate)

- The externally reported 35/100 score weighs names and schemas in an unpublished way. This PRD targets a defined internal metric (name-set identity plus canonicalized schema identity) and assumes the two move together. HIGH risk if the external metric also weighs description text, which the licensing constraint caps.
- ~~The Mistral API accepts reference-shaped schemas containing `$defs`, `$ref`, `anyOf`, and `default` without rejection.~~ Validated 2026-08-04 against the live endpoint by the US-042 probe: HTTP 200. See the answered open question below.
- `TerminalManager` can back a model-facing `bash` tool without a PTY. The reference legacy `Bash` tool runs a single non-interactive command, which matches the existing pipe-based implementation. The managed family may need more.
- Reference-conformant naming does not break the ACP surface. The 7 `acp_*` tools have no Python counterpart and are additive by design.

### Hard Constraints

- `NOTICE` forbids copying, translating, vendoring, linking, or shipping upstream implementation source. Tool descriptions must be original text; the reference corpus is a local, gitignored test artifact only.
- Workspace dependency layers are enforced (`Cargo.toml:63`): `vibe-protocol`/`vibe-core`, then `vibe-app-server`, then `vibe-cli`/`vibe-acp`. Builtin tools belong in `vibe-core` and may not reach upward.
- `unsafe_code = "forbid"`, `panic`/`unimplemented`/`dbg_macro` denied at workspace level. Handlers must return `ToolError`, never panic.
- The reference checkout at `/home/arthur/dev/mistral-vibe` is read-only and pinned at `68ff32e6a92e80a874c8153312f0aa8ae4955477`. No file in it is created, modified, or deleted by any story. Every source-navigation line in this PRD was validated against that commit; all 74 references resolve to the declaration they name.
- The runtime probe is pinned to the same commit (`crates/vibe-cli/src/tui/runtime_parity_tests.rs:39`). It previously pinned `99a6efa9ca1fb48671adebe0f6f5d931945bd8c9` (v2.23.2) and skipped silently because the local checkout had moved to v2.23.3; it was re-pinned to `68ff32e6` so the tool-surface oracle can reuse that constant and both oracles can never read different reference trees. `crates/vibe-cli/src/tui/runtime_parity_tests*.rs` assert `corpus.reference.commit == REFERENCE_COMMIT`, so every runtime corpus was relabelled to `68ff32e6`/`2.23.3` in the same change. EP-009, EP-010 and EP-011 carry a live Python probe that re-executes against the new tree and passes. EP-006, EP-007 and EP-008 were replay-only and had no capture script, so their relabel was re-measured before it could stand: `crates/vibe-cli/tests/runtime-parity/ep006-python-oracle.py`, `ep007-python-oracle.py` and `ep008-python-oracle.py` now drive the reference at `68ff32e6` and report what it does. EP-006 (10 traces) and EP-007 (6 traces) match their corpora exactly, as do both EP-008 session-deletion traces. One trace did move: `vibe/cli/textual_ui/widgets/rewind_app.py` replaced the one-step `RewindWithRestore`/`RewindWithoutRestore` flow with an action-then-persistence flow emitting `RewindConfirmed(restore_files, inplace)`, so Enter now advances a step instead of dispatching. That trace left `session-management-ep008.json` for its `unavailable` entries, and US-029 is reopened in `tasks/prd-tui-runtime-observable-parity-status.json`.
- Tools must remain bounded by the existing output-size contract (`DEFAULT_MAX_TOOL_OUTPUT_BYTES`, `ToolOutputSink`). A shell tool may not bypass it.

## Quality Gates

These commands must pass for every user story:

- `cargo +stable fmt --all -- --check` - formatting matches the CI gate
- `cargo +stable check --workspace --all-targets --all-features` - the workspace compiles including tests and benches
- `cargo +stable clippy --workspace --all-targets --all-features -- -D warnings` - no lint regression under the workspace lint policy
- `cargo +stable test --workspace --all-features` - full suite including the tool-definition oracle when the pinned checkout is present

## Epics & User Stories

### EP-012: Schema Conformance Foundation

Replace the schema emission and validation path so that conformance is a property of the mechanism rather than of each tool, and stand up the differential oracle every later story is verified against.

**Definition of Done:** A tool spec can be declared in a reference-conformant shape, its arguments validate with reference semantics including defaults, and a single command reports every name and schema divergence against the pinned reference.

**Mistral Vibe source navigation:** [base.py:395 get_parameters](/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py:395), [base.py:418 get_name](/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py:418), [base.py:248 validate_arguments](/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py:248), [manager.py:614 available_tool_specs](/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:614)

#### US-040: Emit reference-conformant argument schemas
**Description:** As a Rust port maintainer, I want a schema construction path that emits exactly the shape Pydantic emits so that no tool can be non-conformant by construction.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None

**Mistral Vibe source navigation:** [base.py:395 get_parameters](/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py:395), [base.py:110 ToolInfo](/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py:110), [questions.py:15 QuestionModel extra=forbid](/home/arthur/dev/mistral-vibe/vibe/questions.py:15), [edit.py:34 EditArgs permissive model](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/edit.py:34)
**Probable Rust delivery surfaces:** `crates/vibe-core/src/tools.rs`, `crates/vibe-core/src/schema.rs`

**Acceptance Criteria:**
- [ ] Given a tool with no arguments, when its schema is built, then the output is `{"type": "object", "properties": {}}` with no `required` key and no `additionalProperties` key
- [ ] Given a tool whose reference model does not set `extra="forbid"`, when its schema is built, then no `additionalProperties` key is present
- [ ] Given a tool whose reference model sets `extra="forbid"`, when its schema is built, then `additionalProperties: false` is present at that object level and at every nested object level the reference marks
- [ ] Given a property with a default value, when its schema is built, then a `default` key carries that value and the property is absent from `required`
- [ ] Given a nullable property, when its schema is built, then it emits `anyOf` with the concrete type and `{"type": "null"}`, not an array-form `type`
- [ ] Given a tool with nested object arguments, when its schema is built, then nested definitions appear under `$defs` and are referenced by `$ref`, matching the reference structure
- [ ] Given the existing `object_schema` helper, when this story completes, then it is either removed or confined to `acp_*` tools with a doc comment stating it is not reference-conformant
- [ ] Given a schema built through the new path, when `ToolSpec::validate` runs, then it accepts `$defs`, `$ref`, and `anyOf` without error
- [ ] Given a spec whose `$ref` points at a `$defs` entry that does not exist, when `ToolSpec::validate` runs, then registration fails naming the unresolved pointer rather than publishing a broken schema
- [ ] Given a spec declaring a property in `required` that is absent from `properties`, when `ToolSpec::validate` runs, then registration fails naming the property

#### US-041: Validate arguments with reference semantics
**Description:** As a Vibe operator, I want the Rust agent to accept and reject exactly the arguments the reference accepts and rejects so that a model-produced call behaves identically in both clients.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-040

**Mistral Vibe source navigation:** [base.py:232 invoke](/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py:232), [base.py:248 validate_arguments](/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py:248), [read_file.py:53 ReadFileArgs defaults](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/read_file.py:53), [grep.py:87 GrepArgs nullable](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/grep.py:87)
**Probable Rust delivery surfaces:** `crates/vibe-core/src/tools.rs`, `crates/vibe-core/src/schema.rs`

**Acceptance Criteria:**
- [ ] Given a schema containing `$ref` to a `$defs` entry, when arguments are validated, then the referenced subschema is resolved and applied
- [ ] Given a schema property declared as `anyOf` of a type and null, when a null is supplied, then validation succeeds
- [ ] Given a schema property declared as `anyOf` of a type and null, when a value of a third type is supplied, then validation fails with the JSON pointer of the offending property
- [ ] Given an array property with an `items` schema, when an element violates it, then validation fails naming the element index
- [ ] Given a property absent from the arguments and carrying a `default`, when the handler is invoked, then it receives the default value rather than an absent key
- [ ] Given a schema without `additionalProperties: false`, when an unknown key is supplied, then validation succeeds and the key is passed through, matching Pydantic's default extra-ignore behavior
- [ ] Given a `$ref` cycle, when validation runs, then it terminates with a bounded-depth error instead of recursing indefinitely
- [ ] Given an arguments payload that Pydantic rejects, when the same payload is validated in Rust, then it is also rejected, verified across a fixture set covering every published tool

#### US-042: Build the tool-definition differential oracle
**Description:** As a Rust port maintainer, I want one command that reports every divergence between the published Rust tool definitions and the pinned reference so that conformance is measured rather than asserted.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-040

**Mistral Vibe source navigation:** [manager.py:614 available_tool_specs](/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:614), [manager.py:303 available_tools](/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:303), [base.py:223 get_full_description](/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py:223), [base.py:418 get_name](/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py:418)
**Probable Rust delivery surfaces:** `scripts/parity/tool_surface_oracle.py`, `crates/vibe-core/src/tools.rs`

**Acceptance Criteria:**
- [ ] Given the pinned reference checkout, when the capture script runs, then it writes a corpus of `{name, parameters}` entries for every available tool on the current platform, canonicalized with sorted keys
- [ ] Given the corpus, when the Rust differential test runs, then it reports missing names, extra names, and per-name schema diffs as a JSON pointer plus expected and actual value
- [ ] Given the pinned checkout is absent or at an unexpected commit, when the test runs, then it skips with an explicit message naming the expected commit rather than failing or silently passing
- [ ] Given the corpus, when it is written, then it lands in a gitignored path and no reference description text is committed to the repository
- [ ] Given the corpus records a platform, when the test runs on a different platform, then it compares only the entries valid for the running platform
- [ ] Given a live API key is available, when the schema probe runs, then it confirms the Mistral endpoint accepts a reference-shaped schema containing `$defs`, `$ref`, `anyOf`, and `default`, and records the result in the PRD's open questions
- [ ] Given no divergence exists, when the test runs, then it prints the conformance count in the form `N/N names, N/N schemas`

---

### EP-013: Align the Existing Surface

Convert the five tools already published and the two remote naming rules to reference names and shapes, unwinding the reverse-compatibility tables that currently encode a second convention.

**Definition of Done:** No invented tool name remains, the five converted tools pass the oracle with zero divergence, and MCP and connector tools publish under reference names.

**Mistral Vibe source navigation:** [read_file.py:95 ReadFile](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/read_file.py:95), [grep.py:168 Grep](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/grep.py:168), [edit.py:76 Edit](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/edit.py:76), [ask_user_question.py:27 AskUserQuestion](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/ask_user_question.py:27), [tools.py:214 published_name](/home/arthur/dev/mistral-vibe/vibe/core/tools/mcp/tools.py:214)

#### US-043: Rename the file tools to reference names
**Description:** As a Vibe operator, I want `read_file` and `grep` instead of `read` and `search` so that prompts and configuration written for the reference apply to the Rust client.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-040, US-042

**Mistral Vibe source navigation:** [read_file.py:95 ReadFile](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/read_file.py:95), [read_file.py:53 ReadFileArgs](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/read_file.py:53), [grep.py:168 Grep](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/grep.py:168), [grep.py:87 GrepArgs](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/grep.py:87)
**Probable Rust delivery surfaces:** `crates/vibe-core/src/workspace.rs`, `crates/vibe-core/src/extensions.rs`, `crates/vibe-cli/src/tui/transcript.rs`

**Acceptance Criteria:**
- [ ] Given a session, when the tool list is published, then `read_file` and `grep` appear and `read` and `search` do not
- [ ] Given `read_file`, when its schema is compared to the reference, then it exposes `file_path` required, `offset` nullable integer with minimum 1 and default null, and `limit` integer with default 2000 and exclusive minimum 0, with zero oracle divergence
- [ ] Given `grep`, when its schema is compared to the reference, then it exposes `pattern` required, `path` defaulting to `.`, `max_matches` nullable integer defaulting to null, and `use_default_ignore` boolean defaulting to true, with zero oracle divergence
- [ ] Given `read_file` called with `offset` beyond the file length, when it executes, then it returns an explicit out-of-range result rather than an empty success
- [ ] Given `grep` called with an invalid regex, when it executes, then it returns a `ToolError` naming the pattern error
- [ ] Given `canonical_tool_name` in `crates/vibe-core/src/extensions.rs:272`, when this story completes, then it no longer maps `read_file` to `read` or `grep` to `search`, and imported permission profiles resolve against the new names
- [ ] Given `profile_permission_scope` in `crates/vibe-core/src/extensions.rs:257`, when it receives the new names, then it produces the same permission scope strings it produced for the old names
- [ ] Given `EffectKind::from_tool_name` in `crates/vibe-cli/src/tui/transcript.rs:36`, when it receives the new names, then transcript rendering is unchanged

#### US-044: Align the edit tool schema
**Description:** As a Vibe operator, I want `edit` to take the reference argument keys so that a model-generated edit call succeeds in the Rust client.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-040, US-042

**Mistral Vibe source navigation:** [edit.py:76 Edit](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/edit.py:76), [edit.py:34 EditArgs](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/edit.py:34)
**Probable Rust delivery surfaces:** `crates/vibe-core/src/workspace.rs`, `crates/vibe-core/src/extensions.rs`

**Acceptance Criteria:**
- [ ] Given `edit`, when its schema is compared to the reference, then it exposes `file_path`, `old_string`, `new_string` as required and `replace_all` boolean defaulting to false, with zero oracle divergence
- [ ] Given an `edit` call using the previous camelCase keys, when it is validated, then it fails with a missing-required-property error naming `file_path`
- [ ] Given `old_string` matching multiple locations and `replace_all` absent, when the tool executes, then it fails with the ambiguity error already defined at `crates/vibe-core/src/workspace.rs:801`
- [ ] Given `old_string` not present in the file, when the tool executes, then it fails with the stale-edit error already defined at `crates/vibe-core/src/workspace.rs:799`
- [ ] Given the policy requirement extraction at `crates/vibe-core/src/workspace.rs:757`, when the key is renamed, then the write permission is still derived from the correct argument
- [ ] Given `auto_approves_edits` in `crates/vibe-core/src/extensions.rs:288`, when it inspects tool overrides, then it resolves against the reference names

#### US-045: Align the interactive tool schemas
**Description:** As a Vibe operator, I want `ask_user_question` and `exit_plan_mode` to publish the reference schema shape so that the model receives the same option and default semantics in both clients.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-040, US-042

**Mistral Vibe source navigation:** [ask_user_question.py:27 AskUserQuestion](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/ask_user_question.py:27), [questions.py:47 UserQuestionRequest](/home/arthur/dev/mistral-vibe/vibe/questions.py:47), [questions.py:28 UserQuestion](/home/arthur/dev/mistral-vibe/vibe/questions.py:28), [questions.py:21 QuestionChoice](/home/arthur/dev/mistral-vibe/vibe/questions.py:21), [exit_plan_mode.py:43 ExitPlanMode](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/exit_plan_mode.py:43), [exit_plan_mode.py:30 ExitPlanModeArgs](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/exit_plan_mode.py:30)
**Probable Rust delivery surfaces:** `crates/vibe-app-server/src/client.rs`

**Acceptance Criteria:**
- [ ] Given `ask_user_question`, when its schema is compared to the reference, then question and choice objects are published under `$defs` and referenced by `$ref`, with zero oracle divergence
- [ ] Given `ask_user_question`, when its schema is inspected, then every property carries the reference description text equivalent and the reference defaults (`header: ""`, `multiSelect: false`, `hideOther: false`, choice `description: ""`, `footerNote: null`)
- [ ] Given `ask_user_question`, when its schema is inspected, then the `minLength` constraints currently at `crates/vibe-app-server/src/client.rs:2641` and `:2649` are absent, and `footerNote` uses `anyOf` rather than array-form `type`
- [ ] Given the reference `QuestionModel` configures `alias_generator=to_camel` alongside `extra="forbid"`, when the schema is emitted, then property names are camelCase (`multiSelect`, `hideOther`, `footerNote`) while every other reference tool stays snake_case, and a test asserts both conventions coexist
- [ ] Given `exit_plan_mode`, when its schema is compared to the reference, then it is exactly `{"type": "object", "properties": {}}` with no `additionalProperties` key, with zero oracle divergence
- [ ] Given `ask_user_question` invoked with fewer than two options in a question, when it is validated, then it fails naming the offending question index
- [ ] Given both tools, when their descriptions are reviewed, then each operational directive present in the reference prompt has an equivalent in the original Rust description, recorded in a directive coverage table

#### US-046: Publish remote tools under reference names
**Description:** As a Vibe operator, I want MCP and connector tools named exactly as the reference names them so that per-tool disable entries and prompts transfer between clients.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-042

**Mistral Vibe source navigation:** [tools.py:214 http published_name](/home/arthur/dev/mistral-vibe/vibe/core/tools/mcp/tools.py:214), [tools.py:415 stdio published_name](/home/arthur/dev/mistral-vibe/vibe/core/tools/mcp/tools.py:415), [connector_registry.py:260 connector published_name](/home/arthur/dev/mistral-vibe/vibe/core/tools/connectors/connector_registry.py:260), [connector_registry.py:67 _normalize_name](/home/arthur/dev/mistral-vibe/vibe/core/tools/connectors/connector_registry.py:67)
**Probable Rust delivery surfaces:** `crates/vibe-core/src/remote_tools.rs`, `crates/vibe-core/src/mcp/registry.rs`, `crates/vibe-core/src/integrations/shared.rs`

**Acceptance Criteria:**
- [ ] Given an MCP server aliased `serena` exposing a tool `find_symbol`, when it is registered, then the published name is `serena_find_symbol` and not `mcp_serena_find_symbol`
- [ ] Given an MCP alias containing a character outside the allowed set, when it is registered, then it is sanitized by the same rule the reference applies and the resulting name passes `validate_tool_name`
- [ ] Given a connector aliased with mixed case and hyphens, when it is registered, then the alias normalization matches the reference rule at `/home/arthur/dev/mistral-vibe/vibe/core/tools/connectors/connector_registry.py:67`, preserving case and hyphens rather than lowercasing
- [ ] Given a connector remote tool name, when it is registered, then it is used verbatim, matching the reference, rather than normalized
- [ ] Given two remote tools that normalize to the same published name, when registration runs, then the duplicate is rejected with a diagnostic naming both sources
- [ ] Given persisted per-tool disable preferences using the previous `mcp_`-prefixed names, when the session loads, then the migration is either performed or the breakage is reported in a startup diagnostic naming the affected entries

---

### EP-014: Universal Tools

Add the reference tools that require no shell runtime, each with a working handler, so the agent can write, plan, browse, and delegate.

**Definition of Done:** `write_file`, `todo`, `web_fetch`, `web_search`, `skill`, and `task` are published with conformant schemas and handlers that execute, and none is a registered stub.

**Mistral Vibe source navigation:** [write_file.py:51 WriteFile](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/write_file.py:51), [todo.py:79 Todo](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/todo.py:79), [web_fetch.py:81 WebFetch](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/web_fetch.py:81), [web_search.py:60 WebSearch](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/web_search.py:60), [skill.py:107 Skill](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/skill.py:107), [task.py:29 Task](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/task.py:29)

#### US-047: Implement write_file
**Description:** As a Vibe operator, I want the agent to create files so that it can complete tasks that require new content rather than only span edits.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-043

**Mistral Vibe source navigation:** [write_file.py:51 WriteFile](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/write_file.py:51), [write_file.py:28 WriteFileArgs](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/write_file.py:28)
**Probable Rust delivery surfaces:** `crates/vibe-core/src/workspace.rs`

**Acceptance Criteria:**
- [ ] Given `write_file`, when its schema is compared to the reference, then it exposes `file_path` and `content` as required with no other properties, with zero oracle divergence
- [ ] Given a path outside the workspace root, when the tool executes, then it is refused by the existing path policy rather than writing
- [ ] Given a write exceeding the configured byte limit, when the tool executes, then it fails with the existing `WriteLimit` error at `crates/vibe-core/src/workspace.rs:793`
- [ ] Given an existing file, when the tool executes, then the previous content is captured for rewind before the write
- [ ] Given a path whose parent directory does not exist, when the tool executes, then the behavior matches the reference and is stated explicitly in the tool description

#### US-048: Implement the todo tool
**Description:** As a Vibe operator, I want the agent to maintain a task list so that long multi-step turns are visible and resumable as they are in the reference client.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-040

**Mistral Vibe source navigation:** [todo.py:79 Todo](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/todo.py:79), [todo.py:49 TodoArgs](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/todo.py:49), [todo.py:34 TodoItem](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/todo.py:34), [todo.py:21 TodoStatus](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/todo.py:21), [todo.py:28 TodoPriority](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/todo.py:28)
**Probable Rust delivery surfaces:** `crates/vibe-core/src/tools/`, `crates/vibe-cli/src/tui/transcript.rs`

**Acceptance Criteria:**
- [ ] Given `todo`, when its schema is compared to the reference, then `action` is required, `todos` is an array of items defined under `$defs` with `id` and `content` required, `status` defaulting to `pending`, and `priority` defaulting to `medium`, with zero oracle divergence
- [ ] Given the status and priority enums, when the schema is inspected, then they list exactly `pending`, `in_progress`, `completed`, `cancelled` and `low`, `medium`, `high` in the reference order
- [ ] Given `action: "read"` with no list ever written, when the tool executes, then it returns an empty list rather than an error
- [ ] Given `action: "write"` with duplicate item ids, when the tool executes, then it fails naming the duplicated id
- [ ] Given a written list, when the transcript renders it, then `EffectKind::Todo` at `crates/vibe-cli/src/tui/transcript.rs:41` receives a payload it can render

#### US-049: Implement web_fetch and web_search
**Description:** As a Vibe operator, I want the agent to read pages and search the web so that it can resolve current information instead of guessing.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-040

**Mistral Vibe source navigation:** [web_fetch.py:81 WebFetch](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/web_fetch.py:81), [web_fetch.py:49 WebFetchArgs](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/web_fetch.py:49), [web_search.py:60 WebSearch](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/web_search.py:60), [web_search.py:41 WebSearchArgs](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/web_search.py:41), [web_search.py:67 is_available](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/web_search.py:67)
**Probable Rust delivery surfaces:** `crates/vibe-core/src/tools/`, `crates/vibe-core/src/provider.rs`

**Acceptance Criteria:**
- [ ] Given `web_fetch`, when its schema is compared to the reference, then it exposes `url` required and `timeout` nullable integer defaulting to null, with zero oracle divergence
- [ ] Given `web_search`, when its schema is compared to the reference, then it exposes `query` as the single required property, with zero oracle divergence
- [ ] Given no Mistral API key resolves, when the tool list is published, then `web_search` is absent, matching the reference availability rule at `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/web_search.py:67`
- [ ] Given a fetch target returning more than the output limit, when the tool executes, then the response is truncated within the existing `ToolOutputSink` contract rather than exceeding it
- [ ] Given a fetch target that does not respond within the timeout, when the tool executes, then it fails with a timeout error naming the URL host and not the full URL with query parameters
- [ ] Given a URL with a non-http scheme, when the tool is invoked, then it is refused before any network call
- [ ] Given a redirect chain, when it exceeds a bounded hop count, then the fetch fails rather than following indefinitely

#### US-050: Implement skill and task
**Description:** As a Vibe operator, I want the agent to load skills and delegate to subagents so that reference workflows depending on either do not stall in the Rust client.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-040

**Mistral Vibe source navigation:** [skill.py:107 Skill](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/skill.py:107), [skill.py:28 SkillArgs](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/skill.py:28), [task.py:29 Task](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/task.py:29), [subagents.py:16 TaskArgs extra=forbid](/home/arthur/dev/mistral-vibe/vibe/core/subagents.py:16)
**Probable Rust delivery surfaces:** `crates/vibe-core/src/tools/`, `crates/vibe-app-server/src/builtin_agents.rs`

**Acceptance Criteria:**
- [ ] Given `skill`, when its schema is compared to the reference, then it exposes `name` as the single required property, with zero oracle divergence
- [ ] Given `task`, when its schema is compared to the reference, then it exposes `task` required and `agent` defaulting to `explore`, and the object carries `additionalProperties: false`, with zero oracle divergence
- [ ] Given a skill name absent from the discovered set, when `skill` executes, then it fails listing the available names rather than returning empty content
- [ ] Given an agent name absent from `crates/vibe-app-server/src/builtin_agents.rs`, when `task` executes, then it fails naming the available agents
- [ ] Given a subagent runner is unavailable in the current session, when the tool list is published, then `task` is absent rather than registered and failing at call time
- [ ] Given a subagent turn is running, when the parent turn is cancelled, then the subagent is cancelled with it and no orphaned turn remains

---

### EP-015: POSIX Shell Surface

Add the shell family that represents the largest single block of missing capability, reusing the existing terminal infrastructure rather than introducing a second process abstraction.

**Definition of Done:** `bash` executes commands under policy, the managed session family is published under the reference rollout condition, and all five POSIX shell names pass the oracle on Linux.

**Mistral Vibe source navigation:** [bash.py:322 Bash](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/bash.py:322), [experimental_bash.py:1611 ExperimentalBash](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1611), [experimental_bash.py:1813 BashOutput](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1813), [manager.py:357 _is_enabled_for_shell_rollout](/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:357)

#### US-051: Implement the bash tool
**Description:** As a Vibe operator, I want the agent to run shell commands so that it can build, test, and inspect the system instead of only reading files.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-041

**Mistral Vibe source navigation:** [bash.py:322 Bash](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/bash.py:322), [bash.py:308 BashArgs](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/bash.py:308), [manager.py:357 legacy rollout gate](/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:357)
**Probable Rust delivery surfaces:** `crates/vibe-core/src/tools/`, `crates/vibe-core/src/process.rs`, `crates/vibe-core/src/shell.rs`, `crates/vibe-core/src/policy.rs`

**Acceptance Criteria:**
- [ ] Given `bash`, when its schema is compared to the reference legacy variant, then it exposes `command` required and `timeout` nullable integer defaulting to null, with zero oracle divergence
- [ ] Given a command, when it executes, then it runs through `TerminalManager` at `crates/vibe-core/src/process.rs:101` rather than a new process abstraction
- [ ] Given a command whose analysis at `crates/vibe-core/src/shell.rs:81` does not resolve to an always-permitted mode, when it is invoked, then approval is requested before execution
- [ ] Given a command producing more output than the tool output limit, when it executes, then output is bounded by the existing sink contract and the truncation is reported to the model
- [ ] Given a command that exceeds its timeout, when the timeout fires, then the process group is terminated and no orphaned child survives the turn
- [ ] Given the turn is cancelled mid-command, when cancellation propagates, then the process group is terminated and the terminal is released
- [ ] Given a non-zero exit status, when the tool returns, then the model receives the status code and the captured output rather than an opaque failure
- [ ] Given `canonical_tool_name` at `crates/vibe-core/src/extensions.rs:272`, when this story completes, then it no longer maps `bash` to `shell` in a way that breaks imported permission profiles

#### US-052: Implement bash_output and bash_stdin
**Description:** As a Vibe operator, I want the agent to poll and feed a running command so that long-lived and interactive processes behave as they do in the reference client.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-051

**Mistral Vibe source navigation:** [experimental_bash.py:1813 BashOutput](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1813), [experimental_bash.py:1267 BashOutputArgs](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1267), [experimental_bash.py:1906 BashStdin](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1906), [experimental_bash.py:1291 BashStdinArgs](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1291)
**Probable Rust delivery surfaces:** `crates/vibe-core/src/tools/`, `crates/vibe-core/src/process.rs`

**Acceptance Criteria:**
- [ ] Given `bash_output`, when its schema is compared to the reference, then it exposes `session_id` required plus `cursor`, `wait_seconds`, and `max_bytes`, with zero oracle divergence
- [ ] Given `bash_stdin`, when its schema is compared to the reference, then it exposes `session_id` required plus `text`, `control`, and `bytes_base64`, with zero oracle divergence
- [ ] Given a cursor from a previous read, when `bash_output` is called again, then only bytes after that cursor are returned and the new cursor is reported
- [ ] Given a session that has exited, when `bash_output` is called, then the final output and the exit status are returned rather than an error
- [ ] Given an unknown session id, when either tool is called, then it fails listing the active session ids
- [ ] Given `bytes_base64` that is not valid base64, when `bash_stdin` is called, then it fails before writing anything to the process
- [ ] Given both `text` and `bytes_base64` are supplied, when `bash_stdin` is called, then the precedence matches the reference and is stated in the description
- [ ] Given a session whose output buffer overflows, when `bash_output` is called, then the drop is reported through the existing `backpressure_dropped` signal rather than silently losing bytes

#### US-053: Implement bash_sessions, bash_log_file, and rollout selection
**Description:** As a Vibe operator, I want session listing, log access, and the reference variant-selection rule so that the managed shell surface appears under the same conditions in both clients.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-052

**Mistral Vibe source navigation:** [experimental_bash.py:1990 BashSessions](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1990), [experimental_bash.py:1326 BashSessionsArgs](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1326), [experimental_bash.py:2134 BashLogFile](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:2134), [experimental_bash.py:1365 BashLogFileArgs](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1365), [experimental_bash.py:1226 ExperimentalBashArgs](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/experimental_bash.py:1226), [manager.py:357 managed rollout gate](/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:357)
**Probable Rust delivery surfaces:** `crates/vibe-core/src/tools/`, `crates/vibe-core/src/process.rs`, `crates/vibe-core/src/config.rs`

**Acceptance Criteria:**
- [ ] Given `bash_sessions`, when its schema is compared to the reference, then it exposes `action`, `session_id`, `clear_logs`, and `max_bytes` with no required property, with zero oracle divergence
- [ ] Given `bash_log_file`, when its schema is compared to the reference, then it exposes `action` required plus `session_id`, `relative_path`, `offset`, `max_bytes`, and `content`, with zero oracle divergence
- [ ] Given the managed rollout is disabled, when the tool list is published, then `bash` carries the legacy two-property schema and the four managed tools are absent
- [ ] Given the managed rollout is enabled on a non-Windows host, when the tool list is published, then `bash` carries the eight-property managed schema and the four managed tools are present
- [ ] Given both variants of `bash` are registered, when selection runs, then the managed variant wins by `selection_priority`, matching the reference priority of 10
- [ ] Given `bash_log_file` with a `relative_path` escaping the session log directory, when it executes, then it is refused before any filesystem access
- [ ] Given `bash_sessions` with a kill action on a session owned by another turn, when it executes, then the behavior matches the reference and is covered by an explicit test

---

### EP-016: Windows Shell Surface

Publish the ten Windows-only reference names so that platform-gated parity is complete rather than assumed.

**Definition of Done:** On Windows, `git_bash_*` and `powershell_*` publish with conformant schemas and executing handlers; on Linux they are absent, and the oracle enforces both.

**Mistral Vibe source navigation:** [git_bash.py:193 GitBash](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/git_bash.py:193), [windows_shell.py:849 WindowsShell](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/windows_shell.py:849)

#### US-054: Implement the git_bash family
**Description:** As a Vibe operator on Windows, I want the Git Bash tool family so that POSIX-shaped commands run in the Rust client as they do in the reference.

**Priority:** P2
**Size:** L (5 pts)
**Dependencies:** Blocked by US-053

**Mistral Vibe source navigation:** [git_bash.py:193 GitBash](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/git_bash.py:193), [git_bash.py:141 GitBashArgs](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/git_bash.py:141), [git_bash.py:206 is_available](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/git_bash.py:206), [git_bash.py:329 ExperimentalGitBash](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/git_bash.py:329), [git_bash.py:376 GitBashOutput](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/git_bash.py:376), [git_bash.py:428 GitBashLogFile](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/git_bash.py:428)
**Probable Rust delivery surfaces:** `crates/vibe-core/src/tools/`, `crates/vibe-core/src/platform.rs`, `crates/vibe-core/src/shell.rs`

**Acceptance Criteria:**
- [ ] Given a Windows host with Git Bash present, when the tool list is published, then `git_bash`, `git_bash_output`, `git_bash_stdin`, `git_bash_sessions`, and `git_bash_log_file` appear with zero oracle divergence
- [ ] Given a Linux host, when the tool list is published, then none of the five names appear
- [ ] Given a Windows host without Git Bash installed, when the tool list is published, then none of the five names appear, matching the reference availability rule at `/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/git_bash.py:206`
- [ ] Given a `git_bash` session, when it is created, then it uses a session prefix distinct from the `bash` family so the two never collide
- [ ] Given a Windows path argument, when it crosses into the Git Bash shell, then the path translation is applied and covered by a test

#### US-055: Implement the powershell family
**Description:** As a Vibe operator on Windows, I want the PowerShell tool family so that native Windows commands are available in the Rust client.

**Priority:** P2
**Size:** L (5 pts)
**Dependencies:** Blocked by US-053

**Mistral Vibe source navigation:** [windows_shell.py:849 WindowsShell](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/windows_shell.py:849), [windows_shell.py:706 WindowsShellArgs](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/windows_shell.py:706), [windows_shell.py:868 is_available](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/windows_shell.py:868), [windows_shell.py:986 ExperimentalWindowsShell](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/windows_shell.py:986), [windows_shell.py:1023 WindowsShellOutput](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/windows_shell.py:1023), [windows_shell.py:1083 WindowsShellLogFile](/home/arthur/dev/mistral-vibe/vibe/core/tools/builtins/windows_shell.py:1083)
**Probable Rust delivery surfaces:** `crates/vibe-core/src/tools/`, `crates/vibe-core/src/platform.rs`, `crates/vibe-core/src/shell.rs`

**Acceptance Criteria:**
- [ ] Given a Windows host, when the tool list is published, then `powershell`, `powershell_output`, `powershell_stdin`, `powershell_sessions`, and `powershell_log_file` appear with zero oracle divergence
- [ ] Given a Linux host, when the tool list is published, then none of the five names appear
- [ ] Given the PowerShell executable is absent, when the tool list is published, then none of the five names appear
- [ ] Given a command producing UTF-16 output, when it is captured, then it is decoded correctly rather than yielding interleaved null bytes
- [ ] Given a cancelled turn, when a PowerShell process group is running, then it is terminated and no orphaned process survives

---

### EP-017: Selection, Filtering, and Enforcement

Restore the reference availability and filtering semantics and make conformance impossible to regress silently.

**Definition of Done:** Configuration-driven enabling and disabling behaves as it does in the reference, availability is conditional per tool, and CI fails on any surface divergence.

**Mistral Vibe source navigation:** [manager.py:303 available_tools](/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:303), [manager.py:377 _select_available_variant](/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:377), [matching.py:16 name_matches](/home/arthur/dev/mistral-vibe/vibe/core/utils/matching.py:16), [base.py:424 is_available](/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py:424)

#### US-056: Match reference tool filtering semantics
**Description:** As a Vibe operator, I want `enabled_tools` and `disabled_tools` to match the reference matching rules so that a shared configuration file has the same effect in both clients.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-042

**Mistral Vibe source navigation:** [matching.py:16 name_matches](/home/arthur/dev/mistral-vibe/vibe/core/utils/matching.py:16), [manager.py:324 denylist precedence](/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:324), [manager.py:416 _apply_per_source_filtering](/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:416)
**Probable Rust delivery surfaces:** `crates/vibe-core/src/tools.rs`, `crates/vibe-core/src/config.rs`, `crates/vibe-app-server/src/client.rs`

**Acceptance Criteria:**
- [ ] Given a disable entry `serena_*`, when the tool list is published, then every tool whose name matches the glob is absent
- [ ] Given a disable entry prefixed `re:`, when the tool list is published, then it is applied as a regular expression, matching the reference rule at `/home/arthur/dev/mistral-vibe/vibe/core/utils/matching.py:16`
- [ ] Given a disable entry differing only in case from a tool name, when the tool list is published, then the tool is absent, matching the reference case-insensitive comparison
- [ ] Given both an enable list and a disable list matching the same tool, when the tool list is published, then the disable wins, matching the reference precedence at `/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:324`
- [ ] Given an invalid regular expression in a `re:` entry, when the configuration loads, then a diagnostic names the entry and the tool list is published without applying it
- [ ] Given per-source disable entries on an MCP server, when the tool list is published, then only that server's named tools are absent

#### US-057: Match reference availability and variant selection
**Description:** As a Rust port maintainer, I want per-tool conditional availability so that a tool whose runtime prerequisite is missing is never published.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-049, US-053

**Mistral Vibe source navigation:** [manager.py:377 _select_available_variant](/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:377), [manager.py:332 _is_tool_available](/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:332), [base.py:424 is_available](/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py:424), [base.py:160 selection_priority](/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py:160), [base.py:164 shell_rollout](/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py:164)
**Probable Rust delivery surfaces:** `crates/vibe-core/src/tools.rs`, `crates/vibe-app-server/src/server.rs`

**Acceptance Criteria:**
- [ ] Given a tool declaring an availability condition, when the condition is false, then the tool is absent from the published list rather than registered as unavailable
- [ ] Given two tools registered under the same name, when selection runs, then the higher `selection_priority` wins and, on a tie, the later registration wins, matching the existing rule at `crates/vibe-core/src/tools.rs:258`
- [ ] Given a tool that is registered but whose handler cannot execute, when registration runs, then it is rejected with a diagnostic naming the tool
- [ ] Given the full availability matrix, when the oracle runs on Linux, then the published name count equals the reference count on the same platform and configuration
- [ ] Given a configuration change that flips an availability condition mid-session, when the next turn starts, then the published list reflects the new state

#### US-058: Lock tool-surface conformance in CI
**Description:** As a Rust port maintainer, I want conformance enforced by the pipeline so that a non-conformant tool cannot merge.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-042

**Mistral Vibe source navigation:** [manager.py:614 available_tool_specs](/home/arthur/dev/mistral-vibe/vibe/core/tools/manager.py:614), [base.py:395 get_parameters](/home/arthur/dev/mistral-vibe/vibe/core/tools/base.py:395)
**Probable Rust delivery surfaces:** `.github/workflows/ci.yml`, `scripts/parity/tool_surface_oracle.py`

**Acceptance Criteria:**
- [ ] Given a pull request adding a tool with no corpus entry, when CI runs, then the tool-surface test fails naming the unregistered tool
- [ ] Given a pull request changing a published schema, when CI runs, then the test fails with the JSON pointer of the change
- [ ] Given CI has no pinned reference checkout, when the test runs, then it compares against the committed canonical corpus digest rather than skipping entirely
- [ ] Given the corpus digest is regenerated, when it is committed, then the commit contains only names and schema structure and no reference description text
- [ ] Given the test passes, when CI reports, then the conformance count is printed in the job log

## Functional Requirements

- FR-01: The system must publish exactly the reference tool name set for the running platform and configuration, with no invented names and no missing names.
- FR-02: For every published name, the system must emit a `parameters` object that is identical to the reference `model_json_schema()` output for that tool after sorted-key canonicalization.
- FR-03: The system must apply schema defaults to absent properties before invoking a tool handler.
- FR-04: The system must accept unknown properties on schemas that do not declare `additionalProperties: false`, and reject them on schemas that do.
- FR-05: The system must resolve `$ref` against `$defs` and evaluate `anyOf` and `items` during argument validation.
- FR-06: The system must publish MCP tools as `{alias}_{tool}` and connector tools as `connector_{alias}_{tool}` using the reference normalization rules.
- FR-07: The system must match tool enable and disable entries by glob, by `re:` prefixed regular expression, and case-insensitively.
- FR-08: The system must NOT register a tool whose handler cannot execute in the current session.
- FR-09: The system must NOT ship, embed, or commit any text copied from the reference implementation, including tool description prompts.
- FR-10: The system must bound every tool's output within the existing output-size contract, including shell tools.
- FR-11: When a turn is cancelled, the system must terminate every process group started by a shell tool during that turn.

## Non-Functional Requirements

- **Performance:** Publishing the full tool list adds under 5 ms to session start, measured as the delta between an empty registry and the full registry across 100 runs. Argument validation of the largest reference schema completes in under 200 microseconds at P95.
- **Security:** Shell tools inherit the existing policy path without a bypass; 0 commands execute at a permission mode below the one `analyze_shell` returns. `web_fetch` refuses non-http schemes and bounds redirects at 5 hops. `bash_log_file` refuses any `relative_path` resolving outside the session log directory. No tool error message includes a full URL with query parameters or an environment variable value.
- **Accessibility:** Not applicable; no new user-facing surface is introduced by this PRD.
- **Scalability:** The registry handles 500 registered tools, the realistic ceiling with several MCP servers attached, with list publication remaining under 5 ms.
- **Reliability:** 0 orphaned child processes across 100 consecutive cancelled shell turns, verified by process-table inspection. The oracle produces identical results across 10 consecutive runs on unchanged inputs.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Empty tool list | Every tool disabled by configuration | Session starts, model receives an empty tools array, no crash | "No tools are enabled for this session." |
| 2 | Reference checkout absent | Oracle run on a machine without the pinned Python tree | Test skips with the expected commit named | "skipping the tool-surface oracle: pinned checkout not found at {path}" |
| 3 | Schema drift | A tool schema edited without regenerating the corpus | Test fails with the JSON pointer, expected, and actual | "tool `edit` diverges at /properties/file_path: expected {...}, got {...}" |
| 4 | Duplicate published name | Two MCP servers exposing the same tool under aliases that normalize identically | Second registration rejected, both sources named | "tool `serena_find` is published by two sources: {a}, {b}" |
| 5 | Missing runtime prerequisite | `task` invoked with no subagent runner available | Tool absent from the list rather than failing at call time | — |
| 6 | Argument type mismatch | Model sends a string where an integer is required | Validation fails naming the JSON pointer | "schema validation failed at $.limit: expected integer" |
| 7 | Unknown extra property | Model sends a hallucinated property on a permissive schema | Accepted and ignored, matching Pydantic default | — |
| 8 | Unknown extra property, strict schema | Same on `ask_user_question` or `task` | Rejected naming the property | "schema validation failed at $.foo: additional property is not allowed" |
| 9 | Shell timeout | Command exceeds `timeout` | Process group killed, partial output plus timeout status returned | "command timed out after {n}s; process group terminated" |
| 10 | Turn cancelled mid-shell | Operator interrupts during a long command | Process group terminated, terminal released, no orphan | — |
| 11 | Output overflow | Command produces more than the output limit | Output truncated within the sink contract, truncation reported | "output truncated at {n} bytes" |
| 12 | Stale session id | `bash_output` called after session release | Active session ids listed | "unknown session `{id}`; active sessions: {list}" |
| 13 | Missing API key | `web_search` with no resolvable Mistral key | Tool absent from the published list | — |
| 14 | Log path escape | `bash_log_file` with `relative_path` containing `..` | Refused before filesystem access | "log path escapes the session log directory" |
| 15 | Legacy config names | Configuration disabling `read` or `search` after the rename | Startup diagnostic naming the stale entries | "configuration disables unknown tools: read, search" |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | The external 35/100 score weighs description text, which `NOTICE` prevents matching literally, so 100/100 stays unreachable without a licensing decision | High | High | Raised as the first open question. Directive coverage tables make the residual gap explicit and measurable. A NOTICE amendment permitting Apache-2.0 attributed reuse is a one-line decision that unblocks it if the score demands it. |
| 2 | The managed shell family needs persistent-session semantics `TerminalManager` may not fully cover, particularly PTY-dependent interactive programs | High | Medium | US-051 lands the legacy variant on the existing infrastructure first, which is the only variant visible without the rollout. US-052 and US-053 are P1 and may be deferred without blocking the P0 surface. |
| 3 | Renaming `read`, `search`, and the `edit` keys breaks saved configurations, imported permission profiles, and agent definitions | Medium | Medium | US-043 and US-044 carry explicit criteria for `canonical_tool_name`, `profile_permission_scope`, and `auto_approves_edits`. Edge case 15 requires a startup diagnostic naming stale entries. |
| 4 | Mistral rejects reference-shaped schemas containing `$defs` or `anyOf`, invalidating the conformance target | Low | High | US-042 probes the live endpoint before the corpus is frozen. If rejection occurs, the fallback is `$ref` inlining, which changes the conformance definition and must be re-decided. |
| 5 | Registering 21 new tools degrades tool-selection accuracy by crowding the context | Medium | Medium | Reference parity is the goal, and the reference ships the same count. The NFR bounds list-publication cost; selection quality is out of scope and is not a reason to under-publish. |
| 6 | The oracle corpus drifts from the pinned commit as the reference evolves | Medium | Low | The corpus records the reference commit and the test refuses to compare against a different one, matching the existing probe pattern at `crates/vibe-cli/src/tui/runtime_parity_tests.rs:46`. |
| 7 | Shell tools introduce a command-execution path that bypasses the policy layer | Low | High | US-051 requires execution through the existing `PolicyGuardedTool` wrapper and `analyze_shell`. The security NFR sets the measurable bar at zero bypasses. |

## Non-Goals

- Byte-identical tool descriptions. `NOTICE` forbids shipping upstream text; parity is specified as directive coverage. Revisit only if the licensing posture changes.
- Observable execution-trace parity for tool handlers. This PRD makes the surface identical and the handlers correct; it does not assert that `grep` output is byte-identical to the reference. That is a separate effort, comparable in size to the completed runtime PRD.
- Custom tool discovery from `.vibe/tools/*.py`. The reference loads user-authored Python tools at runtime; the Rust port has no equivalent extension mechanism and inventing one is out of scope.
- Description overrides from `<tools-dir>/prompts/*.md`. Deferred with custom tool discovery, since both rely on the same search-path mechanism.
- MCP sampling, OAuth login flows, and connector authentication changes. Naming is in scope, lifecycle is not.
- The `experimental_bash` 78 KB implementation depth. Only the published schema and a working handler are in scope, not internal parity with the reference implementation.

## Files NOT to Modify

- `/home/arthur/dev/mistral-vibe/**` — the reference checkout is a read-only behavioral oracle. No story creates, modifies, or deletes any file under it.
- `NOTICE` — the licensing posture is a deliberate project decision. Changing it is an explicit operator call, not an implementation side effect.
- `crates/vibe-protocol/src/lib.rs` — the wire protocol is pinned by tests in `crates/vibe-app-server`. Tool surface changes must not alter the protocol shape.
- `scripts/parity/harness.py`, `scripts/parity/oracle.py`, `scripts/parity/scenarios.py` — the chat-input oracle is a completed, passing contract. The tool-surface oracle is a sibling script, not an edit to these.
- `crates/vibe-cli/src/tui/runtime_parity_tests*.rs` — the runtime parity corpus is certified DONE. Reuse its probe pattern by reference, do not modify it.

## Technical Considerations

- **Architecture:** Where do the 21 new tools live? Recommended: a `crates/vibe-core/src/tools/builtins/` submodule tree, one file per tool family, registered through a single `BuiltinToolFactory` implementing `SessionToolFactory` (`crates/vibe-app-server/src/server.rs:158`) and chained via `ChainedSessionToolFactory`. This keeps `vibe-core` as the owner, respecting the dependency layers at `Cargo.toml:63`. Engineering to confirm whether the factory trait, currently declared in `vibe-app-server`, should move down to `vibe-core` or whether registration stays in the upper layer.
- **Data Model:** Schema literals versus derived schemas. Recommended: handwritten `json!` literals paired with `Deserialize` argument structs, with the oracle as the anti-drift mechanism. Alternative: `schemars` with a Pydantic-compatibility `Transform`. Trade-off: derivation removes struct-schema drift but adds `title`, `$schema`, and `format` cleanup, and still cannot guarantee key order, which the canonicalizer handles anyway.
- **API Design:** Should `ToolSpec` carry an availability predicate rather than a static `ToolAvailability`? US-057 needs conditional publication. Recommended: a closure evaluated at list time, which also expresses the shell rollout. Engineering to confirm the cost against the 5 ms publication budget.
- **Dependencies:** No new runtime crate is required. `reqwest` is already available in `vibe-core` for `web_fetch` and `web_search`, `TerminalManager` covers process execution, and `regex` covers `grep` and `re:` filtering. `insta` is a candidate for corpus snapshots but the oracle's JSON-pointer diff is more useful than a text snapshot; recommended: skip it.
- **Migration:** Renaming `read` and `search` is a breaking configuration change. Recommended: no alias period, with a startup diagnostic naming stale entries (edge case 15). Rollback plan: the rename is confined to spec construction and three compatibility tables, so reverting is a single-commit operation.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Reference names published (Linux) | 3 of 16 | 16 of 16 | Month-1 | Tool-surface oracle name-set diff |
| Reference names published (all platforms) | 3 of 26 | 26 of 26 | Month-6 | Oracle run on Linux and Windows |
| Schemas conformant | 0 of 26 | 26 of 26 | Month-6 | Oracle canonicalized schema diff |
| Invented names published | 2 (`read`, `search`) | 0 | Month-1 | Oracle extra-name report |
| Tool-surface tests in the suite | 0 | 1 oracle plus per-tool validation fixtures | Month-1 | `cargo test --workspace` output |
| Orphaned processes after cancelled shell turns | Not applicable, no shell tool exists | 0 across 100 runs | Month-6 | Process-table inspection in an integration test |
| Registered tools with non-executing handlers | 0 | 0 | Month-6 | Registration-time smoke assertion |

## Open Questions

- What exactly does the external 35/100 score measure? Owner: Arthur. Needed before US-042 freezes the conformance definition. If it weighs description text, the `NOTICE` posture caps the achievable score and a licensing decision is required first. Everything in EP-012 is unaffected either way.
- ~~Does the Mistral API accept `$defs`, `$ref`, `anyOf`, and `default` in `tools[].function.parameters`?~~ **Answered 2026-08-04 by the US-042 live probe.** `scripts/parity/tool_surface.py --probe-endpoint` published the reference `ask_user_question` schema, which carries all four constructs, to `https://api.mistral.ai/v1/chat/completions` on `mistral-medium-3.5`: `{"accepted": true, "ran": true, "status": 200, "tool": "ask_user_question"}`. Conformance stays defined as reference-shaped schemas; `$ref` inlining is not needed and risk 4 is closed.
- Should `SessionToolFactory` move from `vibe-app-server` down to `vibe-core`? Owner: Arthur, at EP-012 start. Affects where 21 tool registrations live but not their content.
- Is the managed shell rollout condition reachable in the Rust port at all? The reference gates it on a GrowthBook flag with no Rust equivalent. Owner: Arthur, before US-053. If unreachable, the managed family is permanently absent on Linux and US-052 and US-053 become Windows-only prerequisites.
- Do saved sessions and persisted preferences reference `mcp_`-prefixed tool names on disk today? Owner: US-046 investigation. Determines whether a migration is required or a diagnostic suffices.
[/PRD]
