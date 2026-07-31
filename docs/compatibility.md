# Compatibility and evidence

The compatibility claim is pinned to Mistral Vibe 2.23.1. The capability
matrix separates required native surfaces from excluded product boundaries.
Required rows need current passing native verdicts or an approved intentional
safety divergence. Excluded rows need upstream and Rust-boundary fixtures,
user-visible documentation, and migration guidance, and are excluded from the
native conformance denominator.

Reports compare process bytes, protocol schemas, semantic events, filesystem
effects, persistence, and PTY transcripts. Canonicalization is limited to
scenario-declared volatile fields. Missing, blocked, flaky, or unowned evidence
fails the release gate.

The current reports are under [`compat/reports`](../compat/reports). Release 5
adds native-host, telemetry, distribution, supply-chain, security, and NFR
evidence. A generated report is certification evidence only when its `ready`
field is true and every artifact points to the same source revision.

See [rebaseline policy](rebaseline.md) before changing the upstream version or
recorded fixtures.
