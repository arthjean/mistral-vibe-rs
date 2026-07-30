# External Python Extension Host Spike

Status: completed decision evidence for the excluded `surface.python-custom-tools` boundary.

## Contract

The spike is not a product capability. The native Rust crates expose no Python host module, configuration, registry adapter, or runtime activation path. The scripts under `python/` are retained only as reproducible evidence and are not included by a Rust target.

Rust owns process lifecycle, deadlines, cancellation, byte limits, and protocol validation. It starts the interpreter without a shell, using an explicit argument vector, `-I`, an empty environment, piped standard streams, and kill-on-drop. A cancelled, timed-out, oversized, malformed, noisy, or unexpectedly exited host is killed and waited before it can be replaced.

The host speaks newline-delimited UTF-8 JSON. Requests are `discover`, `invoke`, and `shutdown`. Responses are typed `result`, `chunk`, or `error` frames correlated by request ID. Tool stdout is redirected to bounded stderr so it cannot impersonate protocol frames.

Python modules are operator-trusted executable code. Import executes top-level module code before tool metadata can be inspected, so a module can perform filesystem or network effects before Rust can authorize an invocation. Module-declared permissions are not a server-owned security boundary. The prototype also uses a bespoke `TOOLS` list instead of the upstream `BaseTool` subclass and re-export contract.

These findings establish the approved exclusion in US-031. They do not block EP-004 because excluded rows are outside the native certification denominator while still requiring fixtures and user-visible documentation. US-032 supplies the supported migration path through the production MCP stdio extension seam.

## Representative coverage

`python/fixtures/representative_tools.py` defines ten tools covering:

- typed arguments and scalar or object results
- incremental output chunks
- process-retained state
- standard-library imports and re-exported callables
- asynchronous execution and cancellation
- declared filesystem permission metadata, which the spike proved is not authoritative
- bounded exceptions and host restart

The Rust integration tests discover all ten and exercise the protocol behaviors above. They do not claim compatibility with upstream custom-tool modules or prove safe authorization.

## Measurement

Measured on the development host on 2026-07-29 with `/usr/bin/python3`, isolated mode, one discovery request, one addition request, and clean shutdown. Ten independent samples each completed in 0.04 seconds at `/usr/bin/time` resolution. Maximum resident set size ranged from 20,916 to 21,500 KiB, with a median of 21,064 KiB.

This is a feasibility measurement, not a production latency guarantee. The persistent process amortizes startup across subsequent tool calls. Module import cost, tool dependencies, machine load, and an external sandbox will change both latency and memory.
