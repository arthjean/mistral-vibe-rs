# MCP live revocation

Mistral Vibe 2.23.1 updates the configured MCP server state when a server is
disabled, but it does not cancel a tool call that is already awaiting the
server. That call can remain alive and later publish a result from a server the
user has revoked.

The Rust implementation binds every call to the server connection epoch.
Disable, reconnect, and close advance that epoch, cancel the in-flight call,
and prevent a late result from entering the tool lifecycle. Calls made while
the server remains enabled retain the upstream discovery, invocation, refresh,
and cleanup behavior.

The semantic fixture starts a hung call before disabling the server and records
the bounded security correction.
