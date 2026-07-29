# Provenance policy

Mistral Vibe RS is independently authored Rust code. Mistral Vibe 2.23.1 is
an Apache-2.0-licensed behavioral oracle, not an implementation dependency.

Allowed evidence is limited to externally observable commands, wire frames,
filesystem effects, persisted formats, terminal transcripts, public schemas,
documented contracts, symbol names, and root-relative source or test anchors.
Fixtures must be produced by the clean checkout pinned in
`compat/baseline.toml`, redacted before their first disk write, and carry their
scenario and baseline identifiers.

Do not copy, translate, vendor, link, embed, or ship upstream Python source.
Do not execute the mutable navigation checkout. Do not record credentials,
absolute home paths, proxy credentials, tokens, or undeclared host data.
Intentional incompatibilities require a matrix entry, rationale, fixtures for
both outcomes, and user-visible documentation.

Changes based on upstream behavior must identify the relevant matrix row and
fixture. Apache-2.0 attribution is retained in `NOTICE`; the Rust
implementation remains separately copyrighted and licensed by `LICENSE`.
