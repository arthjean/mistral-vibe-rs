# Migrating Python custom tools to MCP

The native product does not load Python `BaseTool` modules or expose their
in-process Python API. This is an explicit product-boundary divergence.
External executable tools use MCP.

Move each custom tool behind an MCP server that publishes a typed JSON schema
and returns bounded structured results. Configure a local executable through a
typed `[[mcp_servers]]` TOML entry. Keep the executable and arguments in
user-controlled trusted configuration, or approve the workspace before a
project-local entry activates.

The live session registry performs discovery, exposes tool definitions to the
model, applies Vibe permission policy to every call, streams bounded output,
propagates cancellation, and kills then waits for the owned server process
during cleanup.

Do not translate a Python module into an in-process Rust dynamic library. The
1.0 extension boundary has no Python host, dynamic Rust ABI, Vibe-specific
JSONL protocol, or WASM runtime.
