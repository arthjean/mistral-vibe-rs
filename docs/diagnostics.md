# Configuration and diagnostics

Configuration composes defaults, selected TOML, experiments, environment,
runtime mutation, and agent overlay in that order. Project executable settings
remain inactive until trust is established. Public configuration and
diagnostics expose credential references and redacted proxy state, never
resolved secret values.

For local diagnosis:

```console
vibe --version
vibe --help
vibe-acp --help
cargo run -p vibe-compat -- validate \
  --corpus compat/corpus/upstream-2.23.1
```

Actionable startup failures identify malformed configuration, missing
credentials, unsupported providers, invalid TLS material, unavailable native
credential stores, or untrusted executable settings without creating a
runtime first.

Release diagnostics live in `compat/reports`. A blocked certification report
is expected until external native, signing, notarization, and performance
evidence is present. Do not replace a failed report with a hand-edited passing
artifact.
