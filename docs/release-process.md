# Release and rollback process

Release promotion is a manual, protected workflow. It consumes a release
candidate manifest and performs no promotion unless all of these agree on one
40-character source revision:

- Release 5 compatibility report
- five native-host evidence manifests
- checksummed and signed archives
- artifact attestations, CycloneDX SBOMs, license inventories, Cargo.lock
  digest, build metadata, and NOTICE
- two-build reproducibility evidence
- startup, TUI-ready, streaming, memory, cancellation, handoff, persistence,
  secret-safety, and determinism metrics
- installation, security, platform, MCP migration, diagnostics, compatibility,
  rebaseline, changelog, and rollback documentation

Any failed signing, checksum mismatch, missing target, unresolved dependency
policy finding, incomplete metadata, or threshold miss blocks promotion.

Rollback never mutates an existing versioned artifact. Remove the affected
version from recommendation, restore the previous signed archive through the
installer’s `.previous` recovery path, preserve the failed evidence, and open a
new candidate from a new source revision.

The checked-in workflow currently uploads a certified immutable bundle. It
does not publish a GitHub release or tag without a separate explicit delivery
action.
