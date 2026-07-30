[PRD]
# PRD: Mistral Vibe RS Rust-Native Behavioral Parity

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-07-29 | Arthur Jean | Initial research-informed product and delivery specification |
| 1.1 | 2026-07-29 | Arthur Jean | Replace Python custom-tool parity with a Rust-native, MCP-first extension boundary and require production MCP end-to-end proof |
| 1.2 | 2026-07-29 | Arthur Jean | Assign support classification to US-031, separate excluded Python custom tools from the required MCP stdio extension surface, and close both paths explicitly during final certification |

## Problem Statement

1. Developers who depend on Mistral Vibe have no native Rust implementation that preserves its required observable contract across terminal, headless, editor, tool, configuration, persistence, and platform boundaries.
2. The target repository defines native behavioral parity as its mission. Implementing modules without an explicit supported-surface boundary would make omissions, accidental behavior changes, and misleading compatibility claims likely.
3. The upstream 2.23.1 behavior is broader than its documented UX and contains timing-sensitive concurrency, lossy public projections, platform-specific shell behavior, dynamic Python extensions, persistence migrations, and ADR/code contradictions. Source-shape imitation cannot prove compatibility.
4. Existing coding-agent users expect one resumable engine across terminal, automation, and editors, with deterministic structured output, granular permissions, MCP, bounded cancellation cleanup, and native platform behavior. The product must distinguish required native parity from explicitly unsupported implementation-specific extension surfaces.

**Why now:** Mistral Vibe RS is still at the project-definition stage, so its compatibility contract and ownership boundaries can be fixed before implementation debt accumulates. The upstream reference has been audited and can be pinned to version 2.23.1, while current agent products increasingly converge on shared terminal, headless, ACP, and MCP surfaces.

## Overview

Mistral Vibe RS will be an independent, from-scratch Rust implementation of the required externally observable behavior of Mistral Vibe 2.23.1. Compatibility will be measured at process, protocol, state, filesystem, configuration, tool, and terminal boundaries by a versioned capability matrix and a black-box differential harness. The upstream source tree remains reference-only; Rust internals need not reproduce Python module structure.

The product will use one provider-neutral engine behind serialized app-server boundaries and thin programmatic CLI, TUI, and ACP adapters. Typed events, explicit task and subprocess ownership, a private durable transcript, a lossy public projection, policy-controlled tools, and deterministic configuration composition form the core. Tokio, Serde, and Clap are planned foundations. Terminal, ACP/MCP SDK, keyring, and PTY choices remain gated by validation stories.

Observed 2.23.1 behavior is the default compatibility oracle for required native surfaces, including non-dangerous quirks. Secret disclosure, credential exposure, data loss, orphan-process behavior, and arbitrary Python custom-tool loading will not be copied. Each correction or excluded surface must be registered as an intentional, versioned divergence with evidence proving both the upstream behavior and the Rust boundary. Delivery is split into six gated releases so that 48 session-sized stories remain tractable.

### Native compatibility boundary

- Every capability-matrix row declares `support = "required-native"` or `support = "excluded"` independently from implementation status. The 1.0 compatibility claim covers every `required-native` row.
- Upstream Python `BaseTool` modules and their in-process Python API are inventoried but explicitly excluded. They are an approved product-boundary divergence, not a hidden omission and not a release blocker.
- The completed external-Python-host spike is retained only as non-shipping decision evidence. It is not a product capability, extension path, or separately certifiable matrix surface.
- Rust-authored and language-neutral external tools use MCP. Local executables use MCP stdio and are configured through `[[mcp_servers]]` TOML entries.
- TOML is configuration, not an invocation protocol. The product will not add a Vibe-specific JSONL tool protocol, a dynamic Rust library ABI, or a WASM runtime without a separate concrete requirement.
- A configured MCP executable is operator-trusted at process launch. Project-local executable configuration must not activate before workspace trust. Vibe still owns tool discovery limits, invocation policy, output bounds, cancellation, process cleanup, and public effects, but does not claim to sandbox arbitrary server internals.
- Documentation must provide a migration path from Python custom tools to MCP servers and state the unsupported surface before users install the native binary.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Versioned capability-matrix coverage | At least 20% of inventoried contracts have an owner, fixture, classification, and verdict | 100% of required native 2.23.1 contracts have an owner, fixture, and passing verdict; excluded rows have approved divergence evidence |
| Differential conformance | At least 95% pass rate for implemented EP-001 and EP-002 scenarios | 100% pass rate for required scenarios, with zero undocumented divergences |
| Native platform coverage | Linux x86_64 artifact passes its native smoke suite | Linux x86_64/aarch64, macOS x86_64/aarch64, and Windows x86_64 all pass native suites |
| Secret-safety corpus | Zero disclosures across at least 500 seeded error/config/transcript cases | Zero disclosures across at least 10,000 seeded cases |
| Startup performance | Cold `vibe --help` p95 at or below 150 ms on the reference Linux runner | Cold `vibe --help` p95 at or below 100 ms on every supported target |
| Cancellation reliability | 99% of owned fake subprocesses terminate within 2 seconds in 1,000 trials | 100% terminate within 5 seconds and 99% within 500 ms in 10,000 trials per OS |

## Target Users

### Terminal-native Mistral Vibe users

- **Role:** Developers using an agent interactively for repository exploration, editing, shell execution, review, and long-running implementation.
- **Behaviors:** Work in Git repositories, resume sessions, invoke slash commands and tools, approve side effects, use custom instructions, and switch between terminal and editor.
- **Pain points:** A partial replacement would lose sessions, alter permission decisions, change scripts, or omit familiar TUI interactions.
- **Current workaround:** Continue using upstream Python Mistral Vibe and accept its runtime, startup, resource, and failure characteristics.
- **Success looks like:** Existing workflows and saved projects behave equivalently through a native binary, with every intentional difference visible in the compatibility report.

### Automation and editor integrators

- **Role:** Developers and CI maintainers invoking headless JSON/NDJSON output or connecting editors through ACP.
- **Behaviors:** Parse stdout, depend on exit codes, stream events, host filesystem or terminal operations, reconnect sessions, and automate setup.
- **Pain points:** Undocumented output variants, event-order drift, callbacks in headless mode, and incomplete ACP capabilities break integrations.
- **Current workaround:** Pin a Python package version and maintain integration-specific wrappers.
- **Success looks like:** Versioned schemas, deterministic output, exact lifecycle semantics, and one engine shared by CLI and ACP.

### Extension and platform maintainers

- **Role:** Authors of agents, skills, hooks, MCP servers, connectors, installers, and OS-specific integrations.
- **Behaviors:** Add project or user configuration, run external processes, authenticate services, package releases, and diagnose platform failures.
- **Pain points:** Discovery precedence, migration from Python custom tools, shell quoting, credentials, PTY behavior, and cleanup differ by environment.
- **Current workaround:** Test manually against upstream on a subset of platforms.
- **Success looks like:** One documented MCP-first external tool contract, native platform suites, a Python-to-MCP migration path, and actionable diagnostics.

## Research Findings

Key findings that informed this PRD:

### Competitive Context

- [Mistral Vibe CLI](https://docs.mistral.ai/vibe/code/cli/work-with-cli) and its [shared CLI/editor/cloud sessions](https://docs.mistral.ai/vibe/code/choose-cli-vscode-web-sessions) establish the direct compatibility surface: interactive and headless flows, files, commands, permissions, sessions, MCP, ACP, and cloud handoff.
- [Claude Code CLI](https://code.claude.com/docs/en/cli-usage) establishes user expectations for resumable and headless sessions, machine-readable output, limits, permission modes, MCP, and background execution.
- [OpenCode agents](https://opencode.ai/docs/agents) and [OpenCode ACP](https://dev.opencode.ai/docs/acp/) reinforce provider portability, granular command permissions, one engine across surfaces, and editor interoperability.
- **Market gap:** Comparable products provide overlapping capabilities, but none proves behavioral compatibility with Mistral Vibe through a public, versioned differential matrix implemented as a native Rust product.

### Best Practices Applied

- Define parity as a versioned observable contract and compare black-box outcomes instead of copying source architecture.
- Use byte equality for deterministic process and wire outputs, semantic equality with narrowly declared normalizers for volatile fields, and golden PTY transcripts for terminal workflows.
- Keep ACP editor transport separate from MCP tool/context transport, and enforce policy at the engine rather than trusting MCP roots.
- Use MCP stdio as the sole external executable tool seam. Keep TOML as typed configuration and avoid a second Vibe-specific invocation protocol.
- Follow [MCP security guidance](https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/security_best_practices): default-deny sensitive operations, prevent token passthrough, bind tokens to intended resources, redact logs, and treat local project configuration as untrusted before folder trust.
- Own asynchronous work explicitly. Tokio `JoinSet` supports tracked shutdown, but task abort is cooperative and cannot stop already-running blocking work; subprocesses require explicit kill-and-wait ownership.
- Build and test each target natively. [cross-rs](https://github.com/cross-rs/cross) can aid builds but does not replace native terminal, shell, credential-store, and process-lifecycle validation.

*Primary research sources are linked above. Upstream behavioral evidence and ADR references are maintained by the compatibility corpus.*

## Upstream Reference Map

The navigation reference is `/home/arthur/dev/mistral-vibe`, with all paths below relative to that root. The audit behind this PRD inspected version 2.23.1 at commit `8b4bcd0b8e1d4c59d3fdc2d2282dbf00a8615583`. This mutable working tree is for source navigation only. It must not be executed as the compatibility oracle or used to record approved fixtures.

US-002 must provision a separate clean checkout or installed artifact, verify its immutable digest, and record that location in the baseline manifest. Matrix evidence must use root-relative paths plus symbol or test names so it remains relocatable. Each implementation story should narrow these anchors to the smallest relevant source and test set rather than rescan the entire upstream repository.

| Epic | Normative and source anchors | Contract tests and artifacts |
|------|-------------------------------|------------------------------|
| EP-001 | `pyproject.toml`; `docs/adr/0001-architecture-principles.md`; `docs/adr/0002-core-engine-and-delivery-surfaces.md`; `docs/adr/0009-app-server-boundary.md`; `vibe/app_server/protocol.py::SERVER_METHODS`; `vibe/app_server/_connection_protocol.py`; `vibe/app_server/_model.py::ProtocolModel`; `vibe/app_server/transport.py`; `vibe/app_server/server.py` | `tests/conftest.py`; `tests/app_server/test_protocol.py`; `tests/app_server/test_transport.py`; `tests/app_server/test_events.py`; `tests/app_server/test_harness_contracts.py` |
| EP-002 | `docs/adr/0003-event-driven-agent-loop.md`; `docs/adr/0006-local-sessions.md`; `vibe/core/agent_loop/_loop.py::AgentLoop`; `vibe/core/types.py`; `vibe/core/llm/backend/factory.py`; `vibe/core/llm/backend/mistral.py`; `vibe/core/llm/backend/generic.py`; `vibe/core/llm/backend/anthropic.py`; `vibe/core/llm/backend/openai_responses.py`; `vibe/core/llm/backend/vertex.py`; `vibe/app_server/_handler.py`; `vibe/app_server/_turns.py`; `vibe/app_server/_projector.py`; `vibe/app_server/_runtime.py`; `vibe/app_server/events.py`; `vibe/app_server/models.py`; `vibe/app_server/session.py`; `vibe/cli/programmatic.py`; `vibe/acp/agent.py` | `tests/app_server/test_session.py`; `tests/app_server/test_projection.py`; `tests/app_server/test_session_persistence.py`; `tests/backend/test_backend.py`; `tests/backend/test_anthropic_adapter.py`; `tests/backend/test_openai_responses_adapter.py`; `tests/backend/test_reasoning_adapter.py`; `tests/backend/test_vertex_anthropic_adapter.py`; `tests/agent_loop/e2e/test_e2e_agent_loop.py` |
| EP-003 | `docs/adr/0004-typed-permissioned-tools.md`; `docs/adr/0007-extension-mechanisms.md`; `vibe/core/tools/base.py`; `vibe/core/tools/manager.py`; `vibe/core/tools/models.py`; `vibe/core/tools/permissions.py`; `vibe/core/tools/io_port.py`; `vibe/core/tools/terminal_runtime.py`; `vibe/core/tools/builtins/`; `vibe/core/tools/mcp/`; `vibe/core/tools/connectors/`; `vibe/core/agent_loop_hooks.py`; `vibe/app_server/_resources.py`; `vibe/app_server/_review.py`; `vibe/app_server/_shell.py`; `vibe/app_server/_tool_io.py`; `vibe/app_server/_tool_projection.py` | `tests/tools/`; `tests/app_server/test_client_tools.py`; `tests/app_server/test_mcp.py`; `tests/app_server/test_review.py`; `tests/app_server/test_shell.py`; `tests/app_server/test_tool_projection.py`; `tests/app_server/test_tool_resume.py`; `tests/agent_loop/e2e/test_e2e_bash.py`; `tests/agent_loop/e2e/test_e2e_connectors.py`; `tests/agent_loop/e2e/test_e2e_tools.py` |
| EP-004 | `docs/adr/0005-layered-configuration.md`; `docs/adr/0006-local-sessions.md`; `docs/adr/0007-extension-mechanisms.md`; `vibe/core/config/default_orchestrator.py`; `vibe/core/config/orchestrator.py`; `vibe/core/config/vibe_schema.py`; `vibe/core/config/layers/`; `vibe/core/config/harness_files/_harness_manager.py`; `vibe/core/system_prompt.py`; `vibe/core/prompts/`; `vibe/core/session/session_logger.py`; `vibe/core/session/session_loader.py`; `vibe/core/session/session_migration.py`; `vibe/core/session/last_session_pointer.py`; `vibe/core/agents/`; `vibe/core/skills/`; `vibe/core/hooks/`; `vibe/app_server/_root_session.py`; `vibe/app_server/_sessions.py` | `tests/core/config/`; `tests/session/`; `tests/app_server/test_fork_config_isolation.py`; `tests/app_server/test_session.py`; `tests/app_server/test_subagents.py`; `tests/core/hooks/test_hooks.py`; `tests/skills/`; `tests/e2e/agent_loop_characterization/` |
| EP-005 | `docs/adr/0002-core-engine-and-delivery-surfaces.md`; `docs/adr/0009-app-server-boundary.md`; `docs/adr/0010-textual-content-rendering.md`; `vibe/cli/entrypoint.py`; `vibe/cli/cli.py`; `vibe/cli/commands.py`; `vibe/cli/history_manager.py`; `vibe/cli/clipboard.py`; `vibe/cli/autocompletion/`; `vibe/cli/textual_ui/`; `vibe/acp/agent.py`; `vibe/acp/session.py`; `vibe/acp/session_updates.py`; `vibe/acp/tool_io.py`; `vibe/app_server/_vibe_code.py`; `vibe/app_server/_project_links.py`; `vibe/app_server/_service_resources.py`; `vibe/core/vibe_code_project/` | `tests/snapshots/`; `tests/cli/`; `tests/acp/`; `tests/app_server/test_loops.py`; `tests/app_server/test_project_links.py`; `tests/app_server/test_vibe_code.py`; `tests/agent_loop/e2e/test_e2e_teleport.py` |
| EP-006 | `docs/adr/0008-feature-instrumentation.md`; `vibe/core/telemetry/send.py`; `vibe/core/telemetry/types.py`; `vibe/core/proxy_setup.py`; `action.yml`; `.github/workflows/ci.yml`; `.github/workflows/build-and-upload.yml`; `.github/workflows/release.yml`; `scripts/install.sh`; `scripts/ci/`; `pyproject.toml` project scripts and platform metadata | `tests/core/telemetry/test_telemetry_send.py`; `tests/core/test_proxy_setup.py`; `tests/cli/test_startup_update_prompt.py`; native artifact smoke suites defined by US-041 through US-045 |

## Assumptions & Constraints

### Assumptions (to validate)

- Mistral Vibe 2.23.1 can be installed and executed hermetically as a black-box oracle in development and CI.
- Nondeterministic values can be normalized through an explicit allowlist without hiding semantic regressions.
- A Rust terminal stack can reproduce required interaction and restoration behavior; US-033 validates Ratatui and alternatives before adoption.
- A production MCP stdio implementation can expose Rust-authored tools through the same session-owned registry used by built-ins; US-023 and US-032 must prove this without injected test peers.
- Native or provider-hosted CI is available for five release targets before parity certification.
- Upstream Apache-2.0 terms permit independent implementation and behavioral fixtures when attribution, notices, and provenance are maintained.

### Hard Constraints

- Pin the initial compatibility baseline to Mistral Vibe 2.23.1 and record the exact source/package digest.
- Treat `/home/arthur/dev/mistral-vibe` as a read-only navigation checkout, never as the executable oracle; record fixtures only from the clean baseline provisioned by US-002.
- Do not fork, vendor, translate, or ship upstream Python implementation code.
- Do not embed a Python runtime in the primary binary.
- Do not ship a Python-specific extension host or configuration surface.
- Do not add a Vibe-specific executable tool protocol, dynamic Rust plugin ABI, or WASM runtime without a separately approved requirement.
- Preserve the repository mission: complete required native behavioral parity before product differentiation.
- Use one engine and serialized app-server contract for interactive, programmatic, and ACP surfaces.
- Support Linux x86_64/aarch64, macOS x86_64/aarch64, and Windows x86_64 before the 1.0 parity claim.
- Register every intentional incompatibility with rationale, scope, fixtures, and user-visible documentation.
- Never reproduce upstream behavior that discloses secrets, risks durable data loss, or leaves owned processes running.
- P0 stories block the first parity candidate. P1 stories block 1.0 certification. No P2 capability is in this PRD.
- This PRD is explicitly phased into six releases because honest session-sized decomposition exceeds 20 stories.

## Quality Gates

These commands must pass for every user story:

- `cargo fmt --all -- --check` - verify deterministic Rust formatting.
- `cargo check --workspace --all-targets --all-features` - type-check every crate, target, and feature.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` - enforce workspace lint and panic policy.
- `cargo test --workspace --all-features` - run unit, integration, protocol, fixture, and snapshot tests.

For TUI stories, deterministic terminal-buffer snapshots and focused PTY transcripts must also be inspected. Browser verification is not applicable.

## Epics & User Stories

### EP-001: Release 0 - Compatibility Foundation

Establish the legal, architectural, protocol, fixture, and differential-testing substrate that makes every later parity claim machine-verifiable.

**Definition of Done:** The Rust workspace passes all quality gates; upstream 2.23.1 is pinned; every known surface can be represented in the capability matrix; protocol schemas round-trip; deterministic fakes run without real network or credentials; and the differential runner produces machine-readable verdicts.

#### US-001: Bootstrap the Rust workspace and provenance policy

**Description:** As a maintainer, I want a policy-compliant Rust workspace and provenance boundary so that all implementation starts from a reproducible, legally explicit foundation.

**Priority:** P0  
**Size:** M (3 pts)  
**Dependencies:** None

**Acceptance Criteria:**

- [ ] Given a fresh checkout, when the workspace is inspected, then it contains separately owned protocol, core, app-server, CLI, ACP, and compatibility-harness crates with no cyclic dependency.
- [ ] Given production Rust targets, when lint policy is evaluated, then panic-like escapes are denied and unwrap/expect usage follows the repository Rust policy.
- [ ] Given upstream Apache-2.0 reference use, when provenance files are inspected, then license, NOTICE, attribution, and no-source-copy rules are explicit.
- [ ] Given a crate that violates the dependency direction or panic policy, when validation runs, then it fails with the owning crate and rule identified.

#### US-002: Pin upstream and define the capability matrix

**Description:** As a parity maintainer, I want a versioned inventory of observable upstream contracts so that completion cannot be claimed through an incomplete checklist.

**Priority:** P0  
**Size:** M (3 pts)  
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**

- [ ] Given the baseline manifest, when loaded, then it identifies Mistral Vibe 2.23.1, an immutable source/package digest, Python version, platform, and fixture schema version.
- [ ] Given the audited surface, when the matrix is validated, then it represents CLI flags, 79 app-server methods, wire variants, config fields, persistence formats, tools, extensions, TUI workflows, ACP, telemetry, distribution, and five native targets.
- [ ] Given a matrix row, when inspected, then it has an owner, priority, root-relative source/test paths, symbol or test names, fixture class, Rust status, divergence status, and dependencies; every referenced path resolves in the clean pinned checkout.
- [ ] Given a discovered public behavior with no matrix row, when matrix validation runs, then the release gate fails instead of silently treating it as out of scope.

#### US-003: Define strict protocol models and schema digests

**Description:** As a surface implementer, I want strict Rust protocol types so that serialized app-server behavior remains compatible across all clients.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-001, US-002

**Acceptance Criteria:**

- [ ] Given protocol version 1, when schemas are generated, then all method names, camelCase fields, tagged unions, string error codes, request-ID domains, and dictionary result constraints match fixtures.
- [ ] Given valid fixture frames, when deserialized and reserialized, then the selected canonical wire representation is stable.
- [ ] Given snake_case input, extra fields, unknown variants, invalid IDs, or non-object results, when validated, then the exact compatible error or connection outcome is produced.
- [ ] Given an accidental public schema change, when digest validation runs, then it fails and requires a baseline-version decision.

#### US-004: Provide hermetic fakes and cross-platform primitives

**Description:** As a test author, I want deterministic clocks, IDs, providers, MCP peers, keyrings, filesystems, processes, and platform abstractions so that concurrency and OS behavior are reproducible.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**

- [ ] Given a test scenario, when configured, then time, UUIDs, provider streams, MCP responses, credentials, subprocess exits, terminal size, home, cwd, and environment can be controlled independently.
- [ ] Given Windows, POSIX, and Git Bash path fixtures on Linux, when parsed, then platform-independent policy tests run without reading the host filesystem.
- [ ] Given concurrent tasks, when the fake scheduler gates are used, then response/notification, cancellation, and handoff races can be reproduced without sleeps.
- [ ] Given a test that reaches a real provider, keyring, user config, Git identity, or network endpoint, when the fake harness is active, then it fails immediately.

#### US-005: Validate the upstream parity oracle and canonicalization

**Description:** As a parity engineer, I want evidence that upstream can serve as a stable black-box oracle so that differential tests distinguish volatility from behavior.

**Priority:** P0  
**Size:** M (3 pts)  
**Dependencies:** Blocked by US-002, US-004

**Acceptance Criteria:**

- [ ] Given at least 20 representative process, protocol, persistence, and PTY scenarios, when each runs twice against the pinned upstream, then deterministic and volatile fields are enumerated with evidence.
- [ ] Given timestamps, UUIDs, paths, ports, and provider tokens, when canonicalized, then only fields declared by the fixture schema may change.
- [ ] Given a semantic change in event order, error code, output channel, permission decision, or persisted state, when compared, then canonicalization does not hide it.
- [ ] Given an upstream scenario that cannot run hermetically, when the spike completes, then the row is marked blocked with a reproducible reason and cannot count as passing.

#### US-006: Record a redacted compatibility corpus

**Description:** As a parity engineer, I want reproducible upstream fixtures so that Rust behavior can be developed without live providers or mutable upstream state.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-004, US-005

**Acceptance Criteria:**

- [ ] Given a scenario, when recorded, then argv, stdin, stdout, stderr, exit status, JSON frames, public events, filesystem delta, persisted state, and terminal transcript are captured when applicable.
- [ ] Given credentials, home paths, tokens, or proxy URLs in oracle output, when a fixture is saved, then deterministic placeholders replace sensitive values before disk write.
- [ ] Given a recorded upstream timeout, crash, malformed response, or cancellation, when replayed, then the failure remains an expected observable outcome rather than being dropped.
- [ ] Given an undeclared external network call or host-file read during recording, when it occurs, then recording fails and identifies the attempted dependency.

#### US-007: Implement the differential runner and compatibility reports

**Description:** As a maintainer, I want one differential runner for byte, schema, semantic, filesystem, and PTY comparisons so that every implementation slice emits an auditable verdict.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-003, US-006

**Acceptance Criteria:**

- [ ] Given upstream and Rust outcomes, when compared, then the runner selects the fixture-declared comparison mode and emits pass, fail, blocked, or intentional-divergence.
- [ ] Given a failure, when the report is generated, then it names the matrix row, first semantic difference, relevant artifacts, upstream baseline, and Rust build.
- [ ] Given a report, when serialized, then both human-readable Markdown and versioned machine-readable JSON are produced deterministically.
- [ ] Given a required matrix row without a current fixture or verdict, when a release report is requested, then the report fails closed and lists the missing evidence.

#### US-008: Implement minimal configuration and authentication contracts

**Description:** As an engine implementer, I want validated bootstrap configuration and credential references so that provider work never bypasses trust or secret boundaries.

**Priority:** P0  
**Size:** M (3 pts)  
**Dependencies:** Blocked by US-001, US-002, US-004

**Acceptance Criteria:**

- [ ] Given defaults, selected model/provider, environment-variable names, TLS, proxy, and VIBE_HOME inputs, when bootstrap config is loaded, then a typed immutable snapshot is produced.
- [ ] Given API-key environment references or keyring handles, when public config or diagnostics are projected, then resolved secret values are absent.
- [ ] Given missing credentials, malformed provider config, unsupported backend, or invalid TLS material, when bootstrap runs, then it returns a typed actionable error without creating a runtime.
- [ ] Given untrusted project configuration, when bootstrap runs before trust, then project-controlled executable or extension settings are not activated.

### EP-002: Release 1 - Headless Engine and Provider Parity

Deliver the first usable vertical product slice: one durable engine, compatible app-server lifecycle, provider adapters, limits, text/JSON/NDJSON CLI, and a minimal ACP proof over the same boundary.

**Definition of Done:** A prompt can run through fake and opt-in live providers, stream typed events, execute turn lifecycle and compaction, persist and resume, and expose compatible programmatic CLI plus ACP smoke flows without either surface importing engine internals.

#### US-009: Model typed events, transcripts, and public projections

**Description:** As a surface implementer, I want one typed event algebra and projection reducer so that every client observes consistent immutable history and lifecycle state.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-003, US-004

**Acceptance Criteria:**

- [ ] Given model text, reasoning, user messages, tool calls, tool streams, callbacks, hooks, compaction, titles, and lifecycle changes, when emitted, then each has a typed Rust variant and stable wire projection.
- [ ] Given private LLM messages and public state, when persisted or projected, then model context remains private and the six public history-entry families remain lossily compatible.
- [ ] Given event IDs, when reduced, then positive monotonic sequencing, duplicate suppression, gap detection, snapshots, and session handoffs follow the protocol fixtures.
- [ ] Given an illegal transition, foreign session event, unknown public variant, or mismatched watermark, when reduced, then state is unchanged and a typed error is returned.

#### US-010: Implement app-server transports and lifecycle

**Description:** As a client author, I want memory and stdio app-server transports with compatible concurrency so that local and external clients share one serialized contract.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-003, US-009

**Acceptance Criteria:**

- [ ] Given memory transport, when a frame crosses the boundary, then it is serialized and parsed as JSON rather than passed as a Rust object.
- [ ] Given initialize, initialized, attachment, detachment, shutdown, and close flows, when exercised, then protocol states, buffering, cleanup, and response ordering match fixtures or a registered safety divergence.
- [ ] Given concurrent non-initialize requests, when dispatched, then independent tasks may progress concurrently while attachment and runtime ownership invariants remain serialized.
- [ ] Given malformed JSON, unsolicited responses, unknown IDs, duplicate initialization, or transport EOF, when received, then the compatible error/close path occurs without leaked tasks.

#### US-011: Implement root-session, turn, context, and callback lifecycle

**Description:** As an agent user, I want exact turn and callback semantics so that steering, interruption, approvals, and interactive questions behave consistently across surfaces.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-010

**Acceptance Criteria:**

- [ ] Given `turn/start`, when accepted, then the turn ID and execution reservation exist before the response while model work starts only after the response is written.
- [ ] Given steering, context injection, interrupt, callback request, duplicate callback answer, and callback rejection, when exercised, then compatible status and event transitions are emitted.
- [ ] Given the audited upstream response/notification-order contradictions, when implemented, then each method follows its matrix verdict and any corrected behavior is registered explicitly.
- [ ] Given stale turn IDs, wrong session IDs, conflicting callbacks, unsupported callback kinds, or concurrent turns, when requested, then no runtime mutation occurs and the typed compatible error is returned.

#### US-012: Implement the Mistral provider backend

**Description:** As a Mistral user, I want authenticated streaming and non-streaming completions so that the Rust engine supports the reference provider without surface-specific logic.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-008, US-009

**Acceptance Criteria:**

- [ ] Given model, messages, images, tools, thinking, headers, limits, and metadata, when a request is built, then its fixture-normalized wire form matches upstream.
- [ ] Given streaming text, reasoning, tool calls, usage, refusal, correlation ID, and stop state, when decoded, then typed chunks aggregate into the compatible final message.
- [ ] Given configured retryable responses, when backoff runs, then retries are bounded by elapsed-time policy and never duplicate a completed tool side effect.
- [ ] Given missing final usage, malformed chunks, authentication failure, TLS failure, or refusal, when received, then the engine emits the compatible typed failure and closes owned HTTP resources.

#### US-013: Implement generic provider wire dialects

**Description:** As a provider-portability user, I want all upstream generic API styles so that existing OpenAI-compatible, reasoning, Anthropic, Vertex, and Responses configurations remain usable.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-008, US-009

**Acceptance Criteria:**

- [ ] Given `openai`, `reasoning`, `openai-responses`, `anthropic`, and `vertex-anthropic` styles, when fixture requests are built, then roles, images, tool schemas, reasoning effort, signatures, and IDs match upstream.
- [ ] Given streaming responses for every style, when decoded, then text, reasoning, tools, usage, refusals, and stop reasons aggregate compatibly.
- [ ] Given provider-specific tool-ID sanitization and reasoning-signature round trips, when messages are replayed, then no information required by the provider is lost.
- [ ] Given an unknown style, malformed event sequence, unsupported content block, or missing credential, when handled, then a typed configuration/provider error is returned without fallback to another dialect.

#### US-014: Establish durable session storage and resume

**Description:** As a returning user, I want crash-resistant session creation and resume so that later compaction, forks, and surfaces share one durable substrate.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-009, US-010

**Acceptance Criteria:**

- [ ] Given a session, when messages and metadata are saved, then append-friendly JSONL, atomic metadata replacement, directory naming, fingerprints, and current format identifiers match the compatibility contract.
- [ ] Given resume, when a session is hydrated, then non-system messages, statistics, experiment state, and parent linkage are restored while current configuration and system prompt are applied as upstream does.
- [ ] Given continue, when a terminal pointer is valid, missing, or stale, then pointer-first and latest-valid-session resolution follows fixtures.
- [ ] Given corrupt metadata, truncated JSONL, empty invalid logs, I/O failure, or ambiguous short-ID matches, when loaded, then the matrix-defined error or intentional safety divergence occurs without overwriting evidence.

#### US-015: Implement the conversation loop, limits, compaction, and cancellation

**Description:** As an agent user, I want a bounded streaming state machine so that long tasks, tool cycles, usage limits, and interruptions remain predictable.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-011, US-012, US-013, US-014

**Acceptance Criteria:**

- [ ] Given a user turn, when model and tool cycles run, then checkpoints, step counts, stop reasons, title events, and final transcript ordering match fixtures.
- [ ] Given a context overflow, when reactive compaction succeeds, then the same model turn retries without consuming the turn budget and the session handoff is emitted.
- [ ] Given max-turn, token, price, refusal, or response-length limits, when reached, then compatible terminal status, usage, and public error are produced.
- [ ] Given cancellation during provider streaming, tool execution, compaction, or persistence, when propagated, then the turn finalizes once, missing tool results are repaired when required, and owned tasks are drained.

#### US-016: Expose programmatic CLI and minimal ACP smoke paths

**Description:** As an automation or editor integrator, I want two thin adapters over the same engine so that the architecture is proven before the TUI is built.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-010, US-015

**Acceptance Criteria:**

- [ ] Given compatible programmatic flags, when invoked, then prompt/session intent, workdir, add-dir, trust, agent, tool filters, limits, and auto-approval reach the app-server unchanged.
- [ ] Given text, JSON, and NDJSON modes, when a turn completes, then stdout, stderr, final assistant selection, full public-history serialization, Teleport event handling, and exit codes match fixtures.
- [ ] Given an ACP client, when initialize, new session, prompt, updates, and close run, then a minimal complete exchange uses only app-server public contracts.
- [ ] Given invalid argv, denied callbacks, broken stdout, missing session, unauthorized provider, or ACP disconnect, when encountered, then the surface returns the exact typed/exit behavior and closes its session.

### EP-003: Release 2 - Tools, Policy, and External Integrations

Implement typed side effects, granular policy, filesystem and shell semantics, managed processes, MCP, connectors, and operational app-server resources.

**Definition of Done:** Built-in side effects produce compatible public effects and model-visible results; permission decisions are default-deny and race-safe; subprocesses are owned; production MCP transports discover tools into the session registry; the model receives those definitions and invokes the same registry; MCP/connectors clean up; and every resource mutation returns canonical state. Fake-peer or registry-only tests cannot satisfy this gate.

#### US-017: Define the tool ABI, registry, and effect lifecycle

**Description:** As a tool author, I want typed args, results, config, state, and semantic effects so that arbitrary tools integrate without surface-specific switches.

**Priority:** P0  
**Size:** M (3 pts)  
**Dependencies:** Blocked by US-009, US-015

**Acceptance Criteria:**

- [ ] Given a built-in or external tool, when registered, then its name, typed input/output schema, config, state, availability, presentation kind, and lifecycle are queryable.
- [ ] Given parallel tool calls, when executed, then outputs stream and completions emit in arrival order while transcript repair preserves model protocol requirements.
- [ ] Given a result, when projected, then one public effect carries start, output, approval state, typed result, duration, cancellation, and terminal status.
- [ ] Given duplicate names, unavailable tools, schema-invalid arguments/results, oversized output, or execution panic, when handled, then deterministic precedence, bounded errors, and cleanup follow the matrix.

#### US-018: Implement permission, trust, and approval policy

**Description:** As a workspace owner, I want server-enforced granular permissions so that reads, writes, shell, network, MCP, and destructive operations cannot be authorized by rendering metadata.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-008, US-017

**Acceptance Criteria:**

- [ ] Given ALWAYS, ASK, and NEVER policy plus required path/network permissions, when resolved, then the most specific compatible decision and rationale are produced.
- [ ] Given concurrent approval requests, when callbacks wait, then permission-store mutation and user decisions are serialized without blocking unrelated safe tool output.
- [ ] Given trusted, session-trusted, untrusted, ancestor, and add-dir roots, when evaluated, then closest-decision and explicit opt-in semantics match fixtures.
- [ ] Given trust revoked mid-session, symlink escape, forged public metadata, stale approval, or uncovered external path, when a tool runs, then execution is denied before side effects.

#### US-019: Implement discovery, read, search, and project-context tools

**Description:** As a developer, I want bounded repository discovery and reads so that the agent can understand projects without escaping workspace policy.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-017, US-018

**Acceptance Criteria:**

- [ ] Given repository files, ignores, Git state, AGENTS.md hierarchy, images, and text resources, when discovered, then compatible content, ordering, metadata, and truncation are returned.
- [ ] Given file reads, listing, glob/search, Git inspection, and mentions, when invoked, then typed results and public presentation match fixtures.
- [ ] Given large, binary, invalid-encoding, missing, or changed files, when read, then bounded compatible errors or partial results are emitted without corrupting model context.
- [ ] Given traversal, symlink escape, home path, or external-root access without approval, when requested, then policy denies it and identifies the required permission.

#### US-020: Implement mutation, diff, checkpoint, and review tools

**Description:** As a developer, I want controlled file mutation and reversible review so that edits are inspectable and recoverable.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-014, US-017, US-018

**Acceptance Criteria:**

- [ ] Given write, edit, patch, todo/plan, and related mutation tools, when successful, then typed results, diffs, file counts, presentations, and transcript text match fixtures.
- [ ] Given a turn boundary, when mutations occur, then checkpoints, baseline, hunks, turn diff, approve, revert, and rewind resources remain internally consistent.
- [ ] Given post-tool hooks, when they replace model-visible text, then persisted/model text and already-public typed result follow the registered compatibility/security verdict.
- [ ] Given stale file content, overlapping edits, invalid patch, deleted target, permission denial, or failed rollback, when handled, then no unreported partial success occurs.

#### US-021: Implement cross-platform shell permission semantics

**Description:** As a developer, I want shell-native parsing and path policy so that command approval is predictable on POSIX, cmd.exe, PowerShell, and Git Bash.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-004, US-018

**Acceptance Criteria:**

- [ ] Given POSIX shell syntax, when analyzed, then nested command ASTs, deny patterns, reader allowlists, path commands, find execution predicates, and workspace boundaries match fixtures.
- [ ] Given MSYS, Windows drive, UNC, cmd.exe, and PowerShell forms, when normalized, then compatible shell-specific ASK/NEVER/ALWAYS behavior is produced.
- [ ] Given shell configuration by platform and user override, when selected, then executable, arguments, quoting, environment, and displayed guidance remain consistent.
- [ ] Given unparseable syntax, indirection, `eval`, nested shell strings, unknown commands, or ambiguous paths, when analyzed, then policy falls back to ASK or NEVER as declared rather than auto-allowing.

#### US-022: Own subprocesses, managed terminals, and client ToolIO

**Description:** As an agent user, I want foreground, background, managed, and client-hosted terminal operations to terminate and stream reliably.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-010, US-017, US-018, US-021

**Acceptance Criteria:**

- [ ] Given foreground and background processes, when spawned, then stdin/stdout/stderr, bounded queues, process groups, terminal IDs, state, and exit status are owned by the session.
- [ ] Given managed shell read, write, run, list, and interrupt operations, when used, then compatible chunking, polling, completion, and cleanup events are emitted.
- [ ] Given advertised client ToolIO capabilities, when filesystem or terminal work is delegated, then typed server-to-client requests are validated before runtime mutation.
- [ ] Given task abort, transport loss, output backpressure, unkillable child, client timeout, or session close, when cleanup runs, then kill/wait is attempted, deadlines are reported, and no handle silently detaches.

#### US-023: Implement MCP transports, lifecycle, and OAuth security

**Description:** As an integration user, I want compatible MCP servers and authentication so that tools and resources work without weakening engine policy.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-017, US-018, US-022

**Acceptance Criteria:**

- [ ] Given stdio, HTTP, streamable HTTP, static headers, and OAuth configurations, when connected, then discovery, toggle, refresh, login, logout, and cleanup match fixtures.
- [ ] Given a production build with no injected test double, when a fixture MCP server is configured over stdio, then Vibe launches the process, discovers its tools, exposes their definitions to the model, executes model-selected calls through the session `ToolRegistry`, and returns results through the normal effect lifecycle.
- [ ] Given partial MCP failures, when the runtime starts, then healthy servers remain usable and canonical diagnostics identify failed servers.
- [ ] Given OAuth, when tokens are acquired or refreshed, then resource binding, audience validation, secure storage, and no-token-passthrough rules are enforced.
- [ ] Given malformed tool schemas/results, untrusted project configuration, server timeout, process crash, auth conflict, credential-store failure, or root claim outside policy, when handled, then activation fails closed, the server cannot grant permission, and every owned process is killed and waited.

#### US-024: Implement connectors and operational resources

**Description:** As a client surface, I want typed connector and operational resources so that account, diagnostics, feedback, narration, stats, and tool state do not require engine access.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-008, US-010, US-018, US-023

**Acceptance Criteria:**

- [ ] Given connector discovery, auth state, refresh, credential setup, and disconnect, when invoked, then canonical resource views and notifications match fixtures.
- [ ] Given account, diagnostics, logs, feedback, narration, stats, runtime, ready, and tools-list requests, when invoked, then all 2.23.1 methods return typed public data.
- [ ] Given a resource mutation, when accepted, then the response and subsequent canonical-state notification follow the method-specific ordering verdict.
- [ ] Given unavailable backends, corrupt logs, connector auth failure, unsupported narration, or sensitive exception text, when handled, then the response is actionable and redacted.

### EP-004: Release 3 - Configuration, Sessions, and Extensions

Complete layered configuration, prompt composition, durable lifecycle, handoffs, child sessions, agents, skills, hooks, and the Rust-native MCP extension path.

**Definition of Done:** Configuration and prompts reproduce precedence; every saved-session operation is durable; handoff/reconnect fault tests lose no data; extension discovery is deterministic; child sessions power subagents; Python custom tools are recorded as an intentional unsupported divergence; and a Rust MCP stdio server works end-to-end through typed TOML configuration, session discovery, model exposure, policy, invocation, cancellation, and cleanup.

#### US-025: Complete layered configuration and public config resources

**Description:** As a user, I want deterministic configuration composition and mutation so that project, user, experiment, environment, runtime, and agent values have compatible precedence.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-008, US-010, US-018

**Acceptance Criteria:**

- [ ] Given defaults, one selected trusted project-or-user TOML, experiments, VIBE environment values, runtime overrides, and agent overlay, when composed, then the effective snapshot matches fixtures.
- [ ] Given config read, schema, batch write, reload, thinking, proxy, TLS, dotenv, and VIBE_HOME behaviors, when invoked, then public views and persistence targets are compatible.
- [ ] Given unknown forward-compatible fields, when config is loaded and rewritten, then they are preserved unless the matrix explicitly rejects them.
- [ ] Given corrupt TOML, validation failure, concurrent edits, partial multi-layer write, secret-bearing proxy value, or trust revocation, when handled, then state and redacted errors follow the registered verdict.

#### US-026: Reproduce prompt, instruction, and attachment composition

**Description:** As an agent user, I want exact system and user context composition so that model-visible behavior does not drift between implementations.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-019, US-025

**Acceptance Criteria:**

- [ ] Given base prompt, headless mode, commit policy, model info, OS/tool guidance, skills, subagents, scratchpad, project context, add-dir roots, user AGENTS.md, and project AGENTS.md, when composed, then section order and separators match fixtures.
- [ ] Given project, user, and built-in custom prompts, when resolved, then exact discovery and override precedence is preserved.
- [ ] Given text, image, file, directory, and other user resources, when prepared, then model content and user-display content remain separately typed and compatible.
- [ ] Given missing prompt, invalid prompt ID, unreadable instruction file, stale Git context, unsupported image, or out-of-policy attachment, when handled, then the matrix-defined error or intentional correction is explicit.

#### US-027: Complete saved-session lifecycle and migrations

**Description:** As a returning user, I want list, read, continue, resume, fork, rename, and delete behavior so that the Rust binary can manage its full durable history.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-014, US-025

**Acceptance Criteria:**

- [ ] Given session list, history list, log read, read, continue, resume, title update, fork, delete, and close, when invoked, then filtering, pagination, metadata, pointers, and canonical state match fixtures.
- [ ] Given legacy single-file and current split formats, when migration runs, then success, partial failure, retry, and concurrent invocation are crash-safe and versioned.
- [ ] Given fork or resume, when the runtime is created, then parent linkage, independent config orchestrator, current prompt, transcript, stats, and experiment hydration follow the compatibility contract.
- [ ] Given corrupted entries, duplicate short IDs, stale pointers, missing messages, read-only storage, or interrupted delete, when handled, then valid sessions remain discoverable and no unrelated session is selected.

#### US-028: Make handoff, rewind, and reconnect lossless

**Description:** As a long-running-session user, I want compaction, clear, fork, rewind, and reconnect transitions to preserve identity and history.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-011, US-015, US-027

**Acceptance Criteria:**

- [ ] Given compaction or context clear, when the root session ID changes, then old/new IDs, parent linkage, callback routes, child ownership, resources, watermark, and public snapshot transition atomically.
- [ ] Given event duplicates or gaps, when clients reduce notifications, then duplicates are ignored and gaps trigger a snapshot resync without losing completed entries.
- [ ] Given rewind or history clear, when accepted, then transcript, checkpoints, files, usage baseline, public state, and persistence agree after restart.
- [ ] Given disconnect during handoff, lost handoff notification, reconnect during active turn, stale old-ID interrupt, or crash during persistence, when fault-injected, then no session snaps back, duplicates execution, or loses durable data.

#### US-029: Implement agents, child sessions, subagents, and delegation

**Description:** As an advanced user, I want built-in and custom agents plus delegated child sessions so that task specialization remains compatible without exposing engine objects to clients.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-017, US-027, US-028

**Acceptance Criteria:**

- [ ] Given built-in, project, user, and configured agent paths, when discovered, then profile parsing, migration, override precedence, install, list, uninstall, and active-agent updates match fixtures.
- [ ] Given a task delegation, when a subagent starts, then it receives an independent session ID, copied config, bounded depth, child logging policy, and parent-linked public effect.
- [ ] Given child tool and ToolIO activity, when projected, then root/child ownership and public session IDs remain compatible across completion and handoff.
- [ ] Given missing agent, recursive-depth breach, child crash, cancellation, config migration failure, or parent close, when handled, then children are finalized and the parent receives one bounded result.

#### US-030: Implement skills, hooks, prompts, and command discovery

**Description:** As an extension author, I want deterministic skills, hooks, prompts, and slash-command discovery so that project customization is predictable.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-017, US-025, US-026

**Acceptance Criteria:**

- [ ] Given built-in, configured, project, and user sources, when agents, skills, hooks, prompts, and commands are discovered, then each mechanism follows its audited ordering and duplicate rule.
- [ ] Given pre-tool, post-tool, and post-agent hooks, when executed, then typed input/output, timeout, retries, text replacement, notices, reload, and failure isolation match fixtures.
- [ ] Given skill invocation and lazy subdirectory instructions, when content is injected, then scope, ordering, and repeated-injection behavior are compatible.
- [ ] Given duplicate names, malformed TOML or Markdown metadata, timeout, hook crash, missing file, or untrusted project source, when discovered, then safe mechanisms continue and canonical issues are reported.

#### US-031: Codify the Rust-native extension boundary

**Description:** As a product owner, I want the Python custom-tool incompatibility and the MCP-first replacement recorded explicitly so that the native compatibility claim is honest and the implementation has one external tool seam.

**Priority:** P0

**Size:** L (5 pts)

**Dependencies:** Blocked by US-017, US-023, US-030

**Acceptance Criteria:**

- [ ] Given the upstream `BaseTool` contract and the completed external-host spike, when the boundary is reviewed, then typed args/results/config/state, imports, re-exports, streaming, `InvokeContext`, permissions, startup cost, and trust limitations are documented as compatibility evidence.
- [ ] Given capability-matrix schema version 2, when validated, then every row declares `support = "required-native"` or `support = "excluded"`; the `known_rows` inventory contains every audited surface; and a missing row, duplicate row, missing classification, or unknown classification fails closed.
- [ ] Given the Python custom-tool surface, when represented in the capability matrix, then exactly one `surface.python-custom-tools` row owned by US-031 is classified `excluded`, carries an approved intentional product-boundary divergence with upstream and Rust-boundary fixtures plus user-visible documentation, and cannot appear as implemented, blocked, passing native behavior, or an MCP substitute.
- [ ] Given the completed external-host spike and native product artifacts, when inspected, then the spike remains evidence only and no compiled module, shipped script, configuration schema, runtime path, capability row, embedded interpreter, Python tool path, or upstream implementation code exposes a Python-specific host.
- [ ] Given compatibility reports, when native conformance is calculated, then only `required-native` rows contribute to the native pass denominator while every `excluded` row must have current approved evidence and documentation or certification fails.
- [ ] Given extension guidance, when reviewed, then MCP stdio is the only external executable tool seam, TOML is described only as configuration, and migration guidance explains how to replace a Python custom tool with an MCP server.

#### US-032: Complete the Rust-native MCP stdio extension path

**Description:** As an extension author, I want a Rust-native MCP stdio path integrated with configuration and the live agent runtime so that external tools work without Python or a proprietary plugin protocol.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-023, US-025, US-030, US-031

**Acceptance Criteria:**

- [ ] Given the capability matrix, when Release 3 is validated, then one `surface.mcp-stdio-extension` row owned by US-032 is classified `required-native`, depends on the production MCP, configuration, extension-discovery, and tool-registry surfaces, and has no dependency on the excluded Python custom-tool row.
- [ ] Given a typed `[[mcp_servers]]` TOML entry with stdio command, arguments, environment, working directory, and timeouts, when a trusted session starts, then the configured server is launched and discovered without a Python-specific setting or runtime dependency.
- [ ] Given discovered MCP tools plus built-ins, when a turn starts, then enabled and disabled filters select one session-owned `ToolRegistry`, the provider receives exactly those definitions, and model-selected calls execute through that same registry.
- [ ] Given an MCP invocation, when approval, streaming, typed result, display projection, transcript persistence, refresh, disable, reconnect, or session close occurs, then it follows the same bounded tool/effect lifecycle as native tools.
- [ ] Given untrusted project configuration, invalid schema, stdout protocol noise, timeout, cancellation, crash, duplicate name, or attempted permission bypass, when handled, then activation or invocation fails closed, diagnostics are canonical, and the child process is killed and waited.

### EP-005: Release 4 - TUI, ACP, and Cloud Workflows

Expose the completed engine through compatible interactive terminal and editor experiences, then implement Vibe Code project, Teleport, and scheduled-loop workflows.

**Definition of Done:** TUI and ACP consume only public app-server contracts; all audited keyboard, rendering, callback, completion, session, setup, voice, and editor workflows have fixtures; and cloud operations degrade safely without contaminating local state.

#### US-033: Validate the terminal UI stack and restoration model

**Description:** As a TUI maintainer, I want a measured terminal-stack decision so that rendering and restoration are proven before the full interface is built.

**Priority:** P0  
**Size:** M (3 pts)  
**Dependencies:** Blocked by US-004, US-009, US-010

**Acceptance Criteria:**

- [ ] Given Ratatui and at least one viable alternative, when prototyped, then synchronous rendering, immutable state snapshots, input events, TestBackend-style assertions, resize, Unicode, mouse, and clipboard constraints are compared.
- [ ] Given normal exit, panic, cancellation, SIGINT, terminal loss, and nested error paths, when the prototype ends, then terminal modes and cursor state restore in every measurable case.
- [ ] Given Linux, macOS, and Windows backend requirements, when evaluated, then unsupported features and native-test needs are recorded before dependency lock.
- [ ] Given no candidate meeting restoration and testability requirements, when the spike completes, then downstream TUI stories remain BLOCKED rather than adopting an unverified stack.

#### US-034: Build the TUI shell, event loop, and transcript recovery

**Description:** As an interactive user, I want a terminal shell over app-server state with bounded event-to-render latency so that long sessions survive resize, paging, event gaps, and reconnects.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-016, US-028, US-033

**Acceptance Criteria:**

- [ ] Given startup, session attach, ready wait, event consumption, and shutdown, when the TUI runs, then it imports no engine internals and restores the terminal through one ownership guard.
- [ ] Given long history, when rendered and paged, then cursor windows, lazy history loading, immutable completed entries, and scroll behavior match fixtures.
- [ ] Given event gaps or transport reconnect, when resync occurs, then one canonical snapshot replaces local projection without duplicate visible entries.
- [ ] Given resize storms, slow rendering, EOF, panic, server failure, or close failure, when handled, then input remains bounded, diagnostics are preserved, and the terminal restores.

#### US-035: Render messages, reasoning, effects, diffs, and rich content

**Description:** As an interactive user, I want semantically compatible transcript rendering so that model reasoning, tools, plans, files, and errors remain inspectable.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-017, US-020, US-034

**Acceptance Criteria:**

- [ ] Given public messages, reasoning, effects, callbacks, checkpoints, notices, Markdown, code, tables, links, images, and plans, when rendered, then bounded semantic layouts match snapshots.
- [ ] Given streamed updates, when entries transition, then content grows monotonically and completed entries become immutable.
- [ ] Given file diffs and tool-specific presentation kinds, when displayed, then added/removed/context lines, truncation, status, duration, and errors remain distinguishable without parsing tool names.
- [ ] Given untrusted markup, control characters, huge lines, invalid Unicode, unsupported image protocol, or narrow terminal, when rendered, then content is escaped, bounded, and never interpreted as terminal control.

#### US-036: Implement prompt editing, history, completion, and mentions

**Description:** As an interactive user, I want upstream-compatible prompt composition so that commands, files, images, resources, and external editing preserve their workflows.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-019, US-026, US-034

**Acceptance Criteria:**

- [ ] Given multiline editing, cursor movement, deletion, history, shortcuts, paste, and external editor, when used, then prompt text and selection transitions match PTY fixtures.
- [ ] Given slash-command, path, agent, skill, and `@` completion, when invoked, then filtering, ordering, insertion, and cancellation are compatible.
- [ ] Given file and image mentions, when submitted, then display content, model resources, mention metrics, and workspace permission checks remain correctly separated.
- [ ] Given missing editor, invalid path, binary file, unsupported image, enormous paste, completion race, or clipboard failure, when handled, then input remains recoverable with an actionable message.

#### US-037: Implement approvals, questions, plans, and session controls

**Description:** As an interactive user, I want all blocking and reversible workflows so that I can supervise agent behavior without losing the active turn.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-011, US-018, US-020, US-028, US-034

**Acceptance Criteria:**

- [ ] Given tool approval and interactive-question callbacks, when displayed and answered, then options, free text, always/once decisions, retry-safe answers, and status transitions match fixtures.
- [ ] Given plan review, turn interrupt, rewind, clear, compact, fork, resume, continue, title, close, and history controls, when invoked, then canonical server state drives the UI.
- [ ] Given shortcuts and notifications, when an operation blocks or completes, then focus, waiting status, transcript entry, and terminal notification are compatible.
- [ ] Given stale callback, conflicting answer, interruption during approval, failed rewind, compaction failure, or close with pending child work, when handled, then no input is applied to the wrong turn.

#### US-038: Implement setup, auth, config, trust, theme, update, and voice UI

**Description:** As an interactive user, I want complete bootstrap and preference workflows so that the native product is usable without editing files manually.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-024, US-025, US-034

**Acceptance Criteria:**

- [ ] Given first run, setup, provider auth, keyring, trust, proxy/TLS, agent/model/thinking selection, and configuration changes, when completed, then canonical resources persist the expected values.
- [ ] Given theme detection, explicit theme, NO_COLOR, notifications, update prompt, and version display, when rendered, then output and preference persistence match fixtures.
- [ ] Given voice recording, transcription, playback, device selection, and cancellation, when supported, then typed lifecycle, limits, and prompt insertion are compatible.
- [ ] Given headless terminal, denied browser sign-in, unavailable keyring/audio device, invalid certificate, update failure, or revoked trust, when handled, then the workflow offers a non-secret-leaking recovery path.

#### US-039: Complete ACP lifecycle and editor capabilities

**Description:** As an editor integrator, I want full ACP session and client-tool parity so that editors can host the same durable engine as the CLI.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-016, US-023, US-028, US-029

**Acceptance Criteria:**

- [ ] Given ACP initialize, authenticate, new/load/list/fork/close session, modes, config options, commands, and complete-history pagination, when invoked, then capabilities and results match fixtures.
- [ ] Given prompts with text, images, resources, client filesystem, and client terminal, when run, then ACP updates map from the same public events and usage state.
- [ ] Given approvals, when routed, then only supported callback kinds reach the ACP client and unsupported user-input flows receive the compatible denial.
- [ ] Given editor disconnect, invalid session, client-tool timeout, malformed ACP frame, callback race, or multiple simultaneous ACP sessions, when handled, then each harness remains isolated and closes owned work.

#### US-040: Implement Vibe Code projects, Teleport, and scheduled loops

**Description:** As a Vibe Code user, I want project, cloud handoff, and scheduled-loop workflows so that the remaining upstream surfaces are represented without reimplementing Mistral cloud services.

**Priority:** P1  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-024, US-028, US-034

**Acceptance Criteria:**

- [ ] Given project create, open, recover, select, load-more, unlink, and cancel, when invoked, then typed operation state and client notifications match fixtures.
- [ ] Given Teleport start, summarization, Git checks, push approval, push response, workflow start, cancel, complete, and failure, when exercised, then URL/output behavior is compatible.
- [ ] Given scheduled-loop create, list, fire, clear, and delete, when exercised, then session ownership, notices, and persistence match fixtures.
- [ ] Given no Git, dirty/unpushed state, denied push, cloud outage, auth expiry, duplicate response, or interrupted loop, when handled, then local session state remains valid and the failure is actionable.

### EP-006: Release 5 - Native Platforms and 1.0 Certification

Prove native shell, terminal, credential, packaging, telemetry, supply-chain, performance, and compatibility behavior, then publish only when the parity matrix is complete.

**Definition of Done:** Five signed native artifacts pass platform suites; telemetry and secrets meet policy; installers and GitHub Action are verified; the compatibility report has 100% required verdicts and zero undocumented divergences; and release metrics meet the PRD targets.

#### US-041: Certify Linux x86_64 and aarch64

**Description:** As a Linux user, I want native artifacts with verified process and terminal behavior so that architecture differences do not hide runtime failures.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-016, US-022, US-034, US-039

**Acceptance Criteria:**

- [ ] Given Linux x86_64 and aarch64 release artifacts, when run natively, then CLI, TUI, ACP, filesystem, POSIX shell, managed terminal, signals, keyring fallback, proxy/TLS, and persistence suites pass.
- [ ] Given glibc and declared minimum-runtime environments, when binaries start, then dependency and CPU requirements match published metadata.
- [ ] Given SIGINT, SIGTERM, terminal loss, child process tree, and package uninstall, when exercised, then terminal and process cleanup meet reliability NFRs.
- [ ] Given an artifact tested only through cross-compilation or emulation, when certification is requested, then certification fails and the target remains unverified.

#### US-042: Certify macOS x86_64 and arm64

**Description:** As a macOS user, I want native Intel and Apple Silicon artifacts so that terminal, shell, keychain, signing, and architecture behavior are trustworthy.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-016, US-022, US-034, US-039

**Acceptance Criteria:**

- [ ] Given macOS x86_64 and arm64 artifacts, when run natively, then CLI, TUI, ACP, shell, PTY, Keychain, proxy/TLS, filesystem, and persistence suites pass.
- [ ] Given universal terminal encodings, Homebrew paths, login shells, and application-notification behavior, when exercised, then matrix verdicts are recorded per architecture.
- [ ] Given signed and notarized artifacts, when downloaded on a clean supported macOS host, then Gatekeeper accepts them and checksums match.
- [ ] Given missing entitlement, denied Keychain, failed notarization, unsupported OS, or orphaned process, when encountered, then release certification fails with evidence.

#### US-043: Certify Windows x86_64

**Description:** As a Windows user, I want native cmd.exe, PowerShell, Git Bash, console, path, and credential behavior so that Unix assumptions cannot ship unnoticed.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-016, US-022, US-034, US-039

**Acceptance Criteria:**

- [ ] Given a Windows x86_64 artifact, when run natively, then CLI, TUI, ACP, cmd.exe, PowerShell, Git Bash, ConPTY, credential store, proxy/TLS, filesystem, and persistence suites pass.
- [ ] Given drive-relative, absolute, UNC, long, non-UTF-8-compatible, MSYS, space-containing, and quoted paths, when used, then compatible validation and display behavior is recorded.
- [ ] Given CTRL_C, CTRL_BREAK, process trees, locked files, antivirus delay, and console resize, when exercised, then cleanup and recovery meet NFRs.
- [ ] Given a path or shell scenario proven only by Linux simulation, when certification is requested, then certification fails and Windows remains unverified.

#### US-044: Implement privacy-safe telemetry parity

**Description:** As a user and product maintainer, I want controllable, schema-versioned telemetry so that required observability never exposes workspace or credential data.

**Priority:** P1  
**Size:** M (3 pts)  
**Dependencies:** Blocked by US-009, US-024, US-025

**Acceptance Criteria:**

- [ ] Given telemetry enabled, when supported lifecycle, tool, mention, compaction, Teleport, feedback, and session events occur, then event names, correlation IDs, metadata, and endpoint selection follow registered fixtures.
- [ ] Given telemetry disabled or no eligible Mistral credential, when events occur, then no telemetry network request or persistent queue entry is created.
- [ ] Given the upstream ADR/envelope contradiction, when payloads are emitted, then the selected compatibility or intentional-divergence contract is versioned and documented.
- [ ] Given prompts, file contents, full paths, secrets, proxy credentials, exception strings, or tool outputs, when telemetry is serialized or logged, then the secret-safety corpus observes zero disclosure.

#### US-045: Package installers, updates, completions, and GitHub Action

**Description:** As an adopter, I want verified installation and automation surfaces so that the native binaries are usable outside a development checkout.

**Priority:** P1  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-041, US-042, US-043, US-044

**Acceptance Criteria:**

- [ ] Given each supported artifact, when installed by the documented script or package path, then `vibe`, `vibe-acp`, shell completions, version, setup, upgrade check, and uninstall behave as documented.
- [ ] Given updater states, when current, outdated, offline, unsupported, or partially upgraded, then selection, messaging, rollback guidance, and exit behavior match fixtures or documented native differences.
- [ ] Given the GitHub Action, when run with string inputs, prompt, credential, approvals, and optional Python setup, then conditions, outputs, exit status, and workspace behavior are tested in GitHub-hosted runners.
- [ ] Given checksum failure, unsupported architecture, read-only install path, offline registry, update interruption, or denied tool callback, when encountered, then installation or automation fails closed without replacing a working binary.

#### US-046: Secure and attest the release supply chain

**Description:** As a release consumer, I want verifiable artifacts and dependency provenance so that I can audit what executes on my machine.

**Priority:** P1  
**Size:** M (3 pts)  
**Dependencies:** Blocked by US-045

**Acceptance Criteria:**

- [ ] Given a release, when artifacts are produced, then each target has a cryptographic checksum, signature/attestation, SBOM, source revision, dependency lock, license inventory, and build metadata.
- [ ] Given two builds in the documented environment, when compared, then reproducibility status and any unavoidable byte differences are published.
- [ ] Given dependencies and licenses, when policy is evaluated, then unknown licenses, yanked crates, and known critical advisories block release pending an explicit decision.
- [ ] Given failed signing, mismatched checksum, missing NOTICE, compromised credential, or incomplete target metadata, when publishing is attempted, then no artifact is promoted.

#### US-047: Close the compatibility and security matrices

**Description:** As a product owner, I want an evidence-backed final audit so that “native behavioral parity” has a precise, inspectable meaning.

**Priority:** P0  
**Size:** L (5 pts)  
**Dependencies:** Blocked by US-032, US-035, US-036, US-037, US-038, US-039, US-040, US-041, US-042, US-043, US-044, US-045, US-046

**Acceptance Criteria:**

- [ ] Given the complete 2.23.1 matrix, when certification runs, then every `known_rows` entry has exactly one support classification and every `required-native` row has a passing native verdict or an approved intentional safety divergence.
- [ ] Given an `excluded` row, when certification runs, then it has an approved intentional product-boundary divergence, current upstream and Rust-boundary fixtures, user-visible documentation, and migration guidance where applicable; it is reported separately from native conformance and any missing evidence blocks release.
- [ ] Given process, wire, persistence, config, permission, provider, tool, TUI, ACP, extension, telemetry, and packaging fixtures, when rerun, then reports contain zero undocumented differences.
- [ ] Given threat modeling and secret, path, protocol, OAuth, subprocess, extension, and supply-chain tests, when audited, then all high-impact findings are resolved or block release.
- [ ] Given a missing platform, incomplete production MCP path, flaky required fixture, unowned method, or unresolved data-loss race, when certification runs, then the native parity claim remains blocked.

#### US-048: Publish the 1.0 parity release and rebaseline process

**Description:** As a user, I want an honest, measured 1.0 release and upstream-update policy so that compatibility remains maintainable after the first baseline.

**Priority:** P0  
**Size:** M (3 pts)  
**Dependencies:** Blocked by US-047

**Acceptance Criteria:**

- [ ] Given a release candidate, when benchmarked, then startup, event latency, memory, cancellation, handoff, persistence, and five-target goals meet all NFR thresholds.
- [ ] Given user documentation, when reviewed, then supported capabilities, installation, config, security differences, unsupported Python custom tools, MCP migration guidance, platform limits, diagnostics, and compatibility report are linked from the README.
- [ ] Given the approved candidate, when published, then versioned artifacts, reports, schemas, notices, changelog, and rollback instructions point to the same immutable source revision.
- [ ] Given a newer upstream release, post-release regression, failed artifact, or newly discovered undocumented divergence, when triaged, then 1.0 artifacts remain immutable and a new baseline/change report is opened rather than silently replacing evidence.

## Functional Requirements

- FR-01: The system must pin every compatibility baseline to an immutable upstream version and digest.
- FR-02: The system must maintain a machine-readable capability matrix covering all observable 2.23.1 surfaces and platforms, with a complete `known_rows` inventory and an explicit `support = "required-native" | "excluded"` classification on every row that is independent from implementation status.
- FR-03: The system must compare Rust and upstream outcomes through versioned byte, schema, semantic, filesystem, and PTY fixtures.
- FR-04: The app-server must serialize JSON even for in-process memory transport and enforce strict protocol version 1 models.
- FR-05: Request concurrency, attachment buffering, event sequencing, duplicate suppression, gap recovery, and callback lifecycles must follow method-specific compatibility verdicts.
- FR-06: One provider-neutral event-driven engine must serve programmatic CLI, TUI, and ACP clients.
- FR-07: Mistral and all five generic provider dialects must preserve request, stream, reasoning, tool, usage, refusal, and stop semantics.
- FR-08: The conversation loop must support steering, context injection, interruption, limits, compaction, handoff, and transcript repair.
- FR-09: Private model transcripts and lossy public session projections must remain distinct.
- FR-10: Session storage must support create, save, list, read, continue, resume, fork, title, history, rewind, clear, delete, close, migration, and crash recovery.
- FR-11: Tool execution must use typed args/results/config/state and one semantic public effect lifecycle.
- FR-12: Tool permission and workspace trust must be server-enforced, default-deny, path-aware, and race-safe.
- FR-13: Filesystem, mutation, shell, managed-terminal, and client ToolIO behavior must be compatible and bounded.
- FR-14: Production MCP stdio, HTTP, and streamable HTTP transports, OAuth, connectors, proxy, TLS, account, diagnostics, feedback, narration, stats, and runtime resources must be exposed through typed app-server methods and the live session tool registry.
- FR-15: Configuration must reproduce defaults, selected TOML, experiment, environment, runtime, and agent-overlay precedence.
- FR-16: Prompt construction must preserve system, platform, tool, skill, subagent, project, AGENTS.md, scratchpad, attachment, and display-content semantics.
- FR-17: Built-in and custom agents, child sessions, subagents, task delegation, skills, hooks, prompts, commands, and discovery precedence must be represented.
- FR-18: External executable tools must use MCP; local MCP servers must be configured through typed TOML, activated only from trusted configuration, exposed to the model and invoked through one session-owned registry, and cleaned up by the Rust runtime. Python `BaseTool` compatibility is explicitly out of scope.
- FR-19: Programmatic mode must support compatible flags, text, JSON, NDJSON, stdout/stderr, callback, Teleport, and exit behavior.
- FR-20: The TUI must support compatible transcript rendering, prompt editing, history, completion, mentions, approvals, questions, plans, session controls, setup, auth, themes, notifications, updates, and voice.
- FR-21: ACP must support full advertised lifecycle, history, capabilities, filesystem/terminal client tools, permissions, and multi-session isolation.
- FR-22: Vibe Code project, Teleport, and scheduled-loop clients must preserve local state on cloud failure.
- FR-23: Telemetry must be user-controllable, schema-versioned, redacted, and absent when disabled.
- FR-24: Native artifacts, installers, updates, completions, and GitHub Action behavior must be tested on supported hosts.
- FR-25: A native-behavioral-parity claim must be impossible while any `required-native` row is missing, blocked, flaky, or undocumented; while any `excluded` row lacks approved evidence and a user-visible divergence; or while an excluded row is counted in the native conformance denominator.

## Non-Functional Requirements

- **Compatibility:** 100% of required native 2.23.1 matrix rows must have a passing verdict or approved intentional safety divergence; every excluded row must have approved evidence and migration guidance where applicable; undocumented divergence count must equal 0.
- **Startup:** Cold `vibe --help` p95 must be at most 100 ms and cold local TUI-ready p95, excluding provider/network work, at most 300 ms on each supported release target.
- **Streaming latency:** With the deterministic fake provider, p95 time from backend-chunk receipt to client-visible event must be at most 20 ms and p99 at most 50 ms over 100,000 chunks.
- **Concurrency:** A session must process 32 concurrent fake tool calls without deadlock, event loss, or more than 100 ms scheduler-induced p99 delay.
- **Memory:** Idle headless RSS must remain at most 80 MiB; a 10,000-entry public history plus transcript fixture must remain at most 300 MiB RSS.
- **Cancellation:** Across 10,000 trials per OS, 100% of owned tasks and child processes must terminate within 5 seconds and at least 99% within 500 ms after accepted cancellation.
- **Persistence:** Across 10,000 injected crash points, acknowledged metadata writes must remain atomic, append logs must expose corruption explicitly, and unrelated sessions must have 0 lost or misattributed records.
- **Handoff reliability:** Across 10,000 compaction/disconnect/reconnect schedules, wrong-session attachment, duplicate turn execution, and durable-entry loss must each equal 0.
- **Security:** Across at least 10,000 seeded config, proxy, MCP, error, log, telemetry, transcript, and crash cases, public secret disclosures must equal 0.
- **Policy:** 100% of Vibe-owned mutation, external-path, shell, network, connector, and client-hosted side effects, plus 100% of MCP tool invocations, must pass through a typed server-side permission decision. Configured MCP executables are trusted code after activation and are not represented as sandboxed.
- **Terminal safety:** Across normal exit, error, panic, SIGINT/CTRL_C, transport loss, and forced cancellation suites, terminal restoration failures must equal 0 on all five targets.
- **Cross-platform:** Native suites must pass on Linux x86_64/aarch64, macOS x86_64/aarch64, and Windows x86_64; emulated-only evidence cannot count.
- **Output determinism:** Repeating any deterministic compatibility fixture 100 times must produce identical canonical output and report digests.
- **Supply chain:** Every release artifact must have a checksum, signature/attestation, SBOM, license inventory, source revision, and native smoke verdict before publication.
- **Accessibility:** Every interactive action must be keyboard-reachable; status must never rely on color alone; NO_COLOR output must pass snapshots on all supported terminals.

## Edge Cases & Error States

Systematic coverage of unhappy paths:

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Empty state | No config, credential, project, or saved session | Start only safe bootstrap resources; do not create a runtime before required config/auth | "Setup is required before starting a session." |
| 2 | Loading state | Provider, MCP, session hydration, compaction, or cloud operation in progress | Emit typed readiness/progress state while input and cancellation remain bounded | Context-specific progress, no fabricated completion |
| 3 | Validation or runtime error | Malformed argv, JSON, config, model response, tool result, or persisted data | Reject at the owning boundary, preserve prior valid state, redact internal details | Typed actionable error with stable code |
| 4 | Network degradation | Provider, MCP, OAuth, connector, telemetry, update, or Teleport is slow/offline | Apply bounded retry only where safe; allow cancellation; preserve local session | "Service unavailable; local session remains available." |
| 5 | Permission change | Trust revoked, callback expires, token revoked, or policy changes mid-session | Re-evaluate before side effect and deny stale authorization | "Permission changed; the operation was not executed." |
| 6 | Concurrent modification | Two requests edit config/session/title/history or race attachment/handoff | Serialize ownership-sensitive transitions, surface conflicts, resync canonical state | "State changed; refreshed current state." |
| 7 | Boundary value | Zero/max limits, huge output/history, event-ID gap, context overflow, long path | Enforce numeric bounds, truncate only declared content, compact/resync explicitly | Stable limit-specific message |
| 8 | Undo or reversal | Rewind, revert, clear, delete, failed patch, interrupted migration | Restore the last acknowledged checkpoint or leave evidence for recovery | "The operation could not be completed; no unrelated state changed." |
| 9 | Interrupted flow | SIGINT, CTRL_C, terminal close, EOF, client disconnect, crash during handoff | Finalize once, kill/wait owned processes, restore terminal, preserve durable state | "Interrupted; recoverable session state was preserved." |
| 10 | External dependency outage | Keyring, browser auth, MCP subprocess, Git, shell, audio, CI runner, or cloud unavailable | Degrade only the dependent capability and mark parity evidence blocked where required | Dependency-specific recovery instruction |
| 11 | Capability classification drift | A known row loses its support class, changes class without approval, or an excluded row enters native conformance | Reject the matrix or report before certification and identify the exact row and invalid transition | "Compatibility classification is incomplete or inconsistent." |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Native parity scope exceeds a single release and drifts into horizontal rewrites | High | High | Six gated epics, 48 bounded stories, matrix ownership, vertical executable slices, no differentiation before certification |
| 2 | MCP appears complete through registry and fake-peer tests while no production server reaches the live model loop | High | High | US-023 and US-032 require a real stdio subprocess, typed config, model-visible definitions, invocation through the session registry, policy, and cleanup |
| 3 | Prompt/provider nondeterminism makes differential tests flaky or masks regressions | High | High | Pinned fixtures, fake providers, narrow canonicalization allowlist, repeated oracle runs, flaky rows cannot pass |
| 4 | Windows/macOS shell, PTY, keyring, path, and signal semantics diverge late | Medium | High | Cross-platform abstractions in EP-001/003, Linux-simulated path tests, mandatory native EP-006 suites |
| 5 | Async cancellation detaches tasks or leaves child processes | Medium | High | Explicit task sets, child ownership, cooperative cancellation plus kill/wait, 10,000-trial fault tests |
| 6 | Upstream defects conflict with security and reliability requirements | Medium | High | Observed-behavior default with approved safety-divergence registry for secrets, data loss, and orphan processes |
| 7 | Upstream releases invalidate the baseline during implementation | High | Medium | Keep 2.23.1 immutable until 1.0; evaluate newer versions only through a separate rebaseline report |
| 8 | TUI library cannot reproduce restoration, rendering, or target support | Medium | Medium | US-033 compares Ratatui and alternatives before dependency lock; downstream TUI remains blocked on failure |
| 9 | License, attribution, or copied-source provenance becomes ambiguous | Low | High | Apache-2.0 notice, source-copy prohibition, provenance review, fixture redaction, release license inventory |
| 10 | A large compatibility matrix creates false completion through missing or misclassified rows | Medium | High | Complete `known_rows` inventory, independent support classification, class-specific report gates, and unowned or unevidenced rows fail certification closed |

## Non-Goals

- New agent capabilities, product differentiation, or UX redesign before the 2.23.1 parity baseline is certified.
- Source-level, module-level, plugin-ABI, or Python internal API compatibility with upstream implementation code.
- Direct loading or execution of upstream Python custom tools.
- A Vibe-specific executable tool protocol, dynamic Rust library plugin ABI, or WASM plugin runtime in 1.0.
- Reimplementation of Mistral-hosted cloud services; only compatible clients and local state transitions are in scope.
- Embedding or silently downloading a Python interpreter in primary artifacts.
- Supporting additional CPU/OS targets before the five declared targets pass.
- Reproducing upstream secret leakage, credential exposure, durable data loss, or orphan-process defects.
- Byte-identical terminal rendering where terminal capabilities differ; the requirement is fixture-defined semantic and interaction parity.

## Files NOT to Modify

- `/home/arthur/dev/mistral-vibe/**` - upstream is a read-only behavioral reference and fixture oracle.
- `/home/arthur/dev/mistral-vibe-rs/.codex/**` - app-managed local automation state, outside product source.
- `/home/arthur/dev/mistral-vibe-rs/README.md` - preserve the mission until a dedicated documentation/release story intentionally updates it.
- Recorded upstream fixtures after approval - changes require a baseline-version or canonicalization decision, never an incidental test rewrite.

## Technical Considerations

- **Architecture:** Should the workspace use the proposed protocol/core/app-server/surface/compatibility crate boundaries? Recommended: yes, with dependency-direction tests. Engineering must confirm crate granularity during US-001 without adding layers that lack a current invariant.
- **Async ownership:** Should session tasks use Tokio `JoinSet` plus explicit child-process owners? Recommended: yes. Engineering must prove shutdown behavior for blocking work and OS process trees.
- **Data Model:** Should private transcripts, public projections, metadata, and fixture schemas be separate versioned types? Recommended: yes. Engineering must choose migration encoding while preserving JSON compatibility.
- **API Design:** Should the internal memory path serialize the same JSON protocol as stdio? Recommended: yes, because serialization is observable and prevents surface/core coupling.
- **Terminal:** Can Ratatui meet restoration, buffer-testing, Unicode, input, and five-target requirements? Recommended: validate in US-033 before locking it; compare a lower-level crossterm renderer if it fails.
- **Provider stack:** Should one HTTP stack serve Mistral and generic adapters? Recommended: yes if it supports streaming backpressure, custom TLS/proxy, cancellation, and fixtureable transport without provider leakage into core.
- **ACP/MCP:** Is an official Rust SDK mature enough for pinned ACP 0.11-compatible and MCP behavior? Recommended: evaluate exact protocol coverage before adoption; handwritten strict wire types remain acceptable when an SDK obscures required behavior.
- **Credentials:** Which native credential abstraction covers Keychain, Windows Credential Manager, and Linux keyring/fallback? Recommended: choose only after native failure-mode tests; no plaintext fallback without explicit opt-in.
- **Compatibility model:** Should product support scope reuse mutable Rust implementation status? Recommended: no. Keep `support = "required-native" | "excluded"` as an independent stable field, use `known_rows` only for audited-inventory completeness, and let class-specific report rules own certification.
- **Executable extensions:** Should Vibe add a TOML manifest protocol, dynamic Rust ABI, or WASM runtime beside MCP? Recommended: no. Use typed TOML to configure MCP stdio and add another mechanism only when a concrete requirement cannot be expressed through MCP.
- **Migration:** How should future upstream baselines evolve fixtures? Recommended: immutable baseline directories and explicit migration/rebaseline reports, never in-place expected-output replacement.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| `required-native` capability rows with passing or approved safety-divergence verdict | Support classification not yet modeled | 100% | Month 6 | Machine-readable compatibility report |
| `excluded` capability rows with approved evidence and user-visible divergence | Support classification not yet modeled | 100% | Before US-047 exit | Machine-readable compatibility report and documentation audit |
| Undocumented observable divergences | N/A, no implementation | 0 | Every release gate | Differential suite and audit |
| Native certified targets | 0 of 5 | 5 of 5 | Month 6 | Signed native CI reports |
| Secret disclosures in seeded corpus | No corpus | 0 across at least 10,000 cases | Month 6 | Property/fuzz corpus report |
| Handoff/reconnect data-loss or wrong-session failures | Unmeasured upstream risk | 0 across 10,000 schedules | Before EP-004 exit | Deterministic scheduler fault suite |
| Orphaned owned processes after 5 seconds | Unmeasured | 0 across 10,000 cancellations per OS | Before EP-006 exit | Native process-tree tests |
| Cold `vibe --help` p95 | N/A | At most 100 ms per target | Month 6 | Release benchmark runner |
| Fake-provider chunk-to-client p95 latency | N/A | At most 20 ms over 100,000 chunks | Before EP-002 exit | Instrumented deterministic benchmark |
| Deterministic fixture repeatability | No fixtures | 100 identical digests in 100 repetitions | Every epic exit | Compatibility runner |
| Known matrix rows without owner, support classification, or current evidence | Support classification not yet modeled | 0 | Before US-031 exit and thereafter | Matrix schema validation and compatibility report |

## Open Questions

- Which production MCP Rust implementation provides stdio, HTTP, streamable HTTP, cancellation, OAuth, and process ownership without obscuring the required wire contract? Owner: US-023 and US-032; answer required before EP-003 and EP-004 exit.
- Which terminal stack passes restoration and native-backend requirements? Owner: US-033; answer required before US-034.
- Which ACP/MCP Rust implementation exposes enough low-level wire control? Owner: US-003, US-023, and US-039; decide before adding either SDK.
- Which credential-store abstraction passes all three OS failure modes? Owner: US-008 and US-038; decide before EP-005 exit.
- Which native CI/notarization providers cover all five targets within project budget? Owner: US-041 through US-043; decide before EP-006 execution.
- Which audited upstream defects qualify for safety divergence beyond secrets, data loss, and orphan processes? Owner: US-007 and US-047; each decision requires a fixture and rationale before certification.
[/PRD]
