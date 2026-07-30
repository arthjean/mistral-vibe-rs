# Python Custom Tools Product Boundary

Mistral Vibe 2.23.1 imports custom Python tool modules into the application process. The Rust port does not embed an interpreter and does not copy upstream Python implementation code into native artifacts.

`surface.python-custom-tools` is the single excluded capability row. This is an approved intentional product-boundary divergence, not implemented or blocked native behavior. Its upstream fixture records the Python module contract. Its Rust fixture records that the product exposes no Python runtime and directs integrations to MCP stdio.

The feasibility prototype starts a persistent external process without a shell and uses bounded JSON Lines frames. It validates process lifecycle, deadlines, cancellation, output bounds, typed values, state, imports, and errors. The result is unsuitable as a product path: importing a module executes top-level Python before Rust can inspect it, module-declared permissions are not server-owned, and the prototype's bespoke `TOOLS` list does not implement the upstream `BaseTool` contract. A host protocol is a lifecycle boundary, not an operating-system sandbox.

The prototype is evidence only. It is not compiled, configured, registered, or shipped by any Rust product target. MCP stdio is the sole supported external executable extension seam.

## Migration to MCP stdio

Wrap the integration as an MCP server that writes newline-delimited JSON-RPC only to stdout, exposes tools through `tools/list`, accepts invocations through `tools/call`, and uses stderr for logs. Configure it for a trusted workspace:

```toml
[[mcp_servers]]
name = "custom"
transport = "stdio"
command = "/absolute/path/to/custom-mcp-server"
args = []
startup_timeout_sec = 10
tool_timeout_sec = 60
disabled_tools = []
```

The Rust session launches this process without a shell, discovers tools into the session-owned registry, exposes that same filtered registry to the model, and applies native trust and permission policy at invocation. Closing the session cancels active calls, closes the transport, and reaps the owned process group.

Measured feasibility results and operational limits are recorded in `docs/python-extension-host-spike.md`.
