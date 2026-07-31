# Security model

Mistral Vibe RS enforces workspace trust and typed permission decisions at the
engine boundary. MCP executables are operator-trusted code after activation,
not sandboxed plugins. Project-local executable configuration remains inactive
before workspace trust. The runtime still owns discovery bounds, invocation
policy, output limits, cancellation, and child-process cleanup.

Credentials remain secret references until the owning network boundary.
Diagnostics, configuration projections, telemetry payloads, errors, and
compatibility fixtures must not contain resolved credentials, credentialed
proxy URLs, prompts, file contents, full paths, exception strings, or tool
output.

Telemetry is opt-in through `--telemetry`. It is also inactive when the
selected provider is not Mistral or no eligible Mistral credential exists,
without creating a request or persistent queue. Enabled telemetry uses a
schema-versioned envelope, a closed set of bounded scalar attributes, and the
HTTPS event endpoint derived from the selected Mistral API base. This
intentionally differs from the upstream open property dictionary:
[telemetry envelope divergence](../compat/divergences/telemetry-envelope.md).

The release gate rejects unknown licenses, yanked crates, unresolved critical
advisories, stale lockfile digests, missing signatures or attestations,
unhealthy signing credentials, incomplete native evidence, and secret-safety
corpus failures.
