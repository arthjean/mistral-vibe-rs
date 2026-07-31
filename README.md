# Mistral Vibe RS

**An independent, from-scratch Rust implementation of [Mistral Vibe](https://github.com/mistralai/mistral-vibe).**

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

These are engineering targets, not assumed advantages. They will be validated through profiling, benchmarks, and direct comparison with the upstream implementation as the project matures.

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

Alignment is reviewed iteratively, axis by axis, against the official upstream
repository. This repository does not claim automated compatibility
certification.

## Relationship to Mistral Vibe

[Mistral Vibe](https://github.com/mistralai/mistral-vibe) is created and maintained by Mistral AI. It is the behavioral reference for the initial parity work.

Mistral Vibe RS is a separate implementation and is not currently an official Mistral AI project.
