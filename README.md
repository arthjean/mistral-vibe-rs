# Mistral Vibe RS

**An independent, from-scratch Rust implementation of [Mistral Vibe](https://github.com/mistralai/mistral-vibe).**

> **Compatibility target 2.23.1**
>
> The native implementation is complete through Release 4. Release 5 native
> host, signing, notarization, supply-chain, and 1.0 certification evidence is
> still required before publication.

## Mission

Mistral Vibe RS rebuilds Mistral Vibe as a native Rust application.

The first objective is **full functional parity with the upstream Python implementation**. Before adding new product capabilities, the Rust version must reproduce the complete user-facing contract of Mistral Vibe across its CLI, interactive interface, agent runtime, tools, extensions, configuration, persistence, and supported platforms.

The codebase starts independently in Rust. It contains neither a Python runtime nor code forked from the upstream implementation.

Once parity is reached, Mistral Vibe RS will explore capabilities that are still missing from current coding agents, with the quality bar set by products such as Codex CLI and Claude Code.

## Why Rust

A terminal coding agent is a long-running, concurrent systems application. It coordinates model streams, subprocesses, filesystem access, permissions, terminal rendering, persistent sessions, protocol clients, and cancellation.

Rust gives this implementation the foundations to pursue:

- a native binary with no language runtime to install;
- fast startup and predictable resource usage;
- explicit concurrency, cancellation, and ownership boundaries;
- strongly typed agent, tool, configuration, and protocol state;
- robust failure handling at security-sensitive boundaries;
- a portable core shared by interactive, programmatic, and ACP surfaces.

These are engineering targets, not assumed advantages. They will be validated through compatibility tests, profiling, and benchmarks as the implementation matures.

## Full parity

Parity means reproducing every externally observable capability of Mistral Vibe, including:

- the `vibe` and `vibe-acp` command-line surfaces;
- interactive and programmatic execution;
- text, JSON, and streaming output modes;
- streaming agent turns, tool calls, approvals, interruption, and usage limits;
- built-in tools, managed shell sessions, project context, and trusted folders;
- built-in agents, custom agents, subagents, task delegation, and interactive questions;
- the terminal interface, command and path completion, file and image mentions, themes, history, shortcuts, and voice mode;
- slash commands, Agent Skills, hooks, MCP servers, and editor integrations;
- layered configuration, setup, authentication, TLS configuration, notifications, and updates;
- local sessions, resume behavior, telemetry controls, and custom Vibe directories;
- supported behavior on Linux, macOS, and Windows.

Parity is measured at public boundaries: commands, flags, configuration, workflows, protocols, persisted behavior, tool semantics, and user-visible output. The internal architecture is free to differ when Rust offers a stronger design.

A versioned parity matrix and automated compatibility suite will track the implementation against a pinned upstream release. Any intentional incompatibility must be explicit and documented.

## Current certification state

The terminal, programmatic CLI, ACP adapter, engine, tools, sessions, MCP,
configuration, and cloud-client contracts are implemented. The 1.0 release
gate remains fail-closed until all five native targets and every non-functional
threshold have current evidence from the same source revision.

Use the release surfaces through these documents:

- [Installation and updates](docs/installation.md)
- [Configuration and diagnostics](docs/diagnostics.md)
- [Security model and intentional differences](docs/security.md)
- [Supported platforms and certification](docs/platforms.md)
- [Python custom-tool to MCP migration](docs/mcp-migration.md)
- [Compatibility reports and rebaseline policy](docs/compatibility.md)
- [Release and rollback process](docs/release-process.md)

Product differentiation begins after the parity baseline is complete.

## Compatibility foundation

The Rust workspace separates protocol, core, app-server, CLI, ACP, and
compatibility-harness ownership. `compat/capability-matrix.toml` inventories
the pinned 2.23.1 surface, while the checked-in corpus records deterministic,
redacted black-box outcomes from the clean checkout identified by
`compat/baseline.toml`.

```console
cargo run -p vibe-compat -- provision --source ../mistral-vibe --sync
cargo run -p vibe-compat -- record
cargo run -p vibe-compat -- validate --corpus compat/corpus/upstream-2.23.1
```

The mutable sibling checkout is navigation-only. Provisioning creates an
ignored detached checkout under `target/compat`; only that checkout may be
executed as the upstream oracle. See `PROVENANCE.md` and `compat/README.md`.

## Relationship to Mistral Vibe

[Mistral Vibe](https://github.com/mistralai/mistral-vibe) is created and maintained by Mistral AI. It is the behavioral reference for the initial parity work.

Mistral Vibe RS is a separate implementation and is not currently an official Mistral AI project.
