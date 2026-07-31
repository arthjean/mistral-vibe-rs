# Native distribution boundary

The 2.23.1 upstream release is a Python and uv product. Its installer and
GitHub Action install that package and do not expose the native artifact
integrity and rollback contract required by Mistral Vibe RS.

The Rust distribution publishes one archive per declared native target. The
installer downloads the archive and `SHA256SUMS`, verifies the checksum before
touching an installed binary, stages both executables, preserves the previous
version during activation, and restores it if activation fails. Archives also
contain shell completions, license, NOTICE, and provenance policy.

The composite GitHub Action installs the same signed native archive used by
terminal users. It accepts prompt, Mistral credential, approval policy, and
optional Python setup as string inputs and returns deterministic programmatic
JSON. Python is optional tool support, not a runtime dependency of the native
binary.

This divergence is limited to distribution mechanics. CLI, ACP, workspace,
permission, and session behavior remains owned by their compatibility rows.
