# Compatibility report

- Baseline: `mistral-vibe@2.23.1`
- Rust build: `mistral-vibe-rs@2.23.1`
- Release: `1`

| Matrix row | Scenario | Verdict | First difference |
|---|---|---|---|
| config.bootstrap | contract-config-bootstrap | Pass |  |
| corpus.recording | contract-corpus-recording | Pass |  |
| differential.reports | contract-differential-reports | Pass |  |
| foundation.baseline | contract-foundation-baseline | Pass |  |
| foundation.workspace | contract-foundation-workspace | Pass |  |
| harness.hermetic-primitives | contract-hermetic-primitives | Pass |  |
| oracle.canonicalization | canonical-volatile-fields | Pass |  |
| protocol.app-server | protocol-error | Pass |  |
| protocol.app-server | protocol-initialize-camel-case | Pass |  |
| protocol.app-server | protocol-invalid-boolean-id | Pass |  |
| protocol.app-server | protocol-invalid-missing-payload | Pass |  |
| protocol.app-server | protocol-invalid-non-object-result | Pass |  |
| protocol.app-server | protocol-invalid-result-and-error | Pass |  |
| protocol.app-server | protocol-notification | Pass |  |
| protocol.app-server | protocol-request | Pass |  |
| protocol.app-server | protocol-success-integer-id | Pass |  |
| protocol.app-server | protocol-success-string-id | Pass |  |
| surface.acp-minimal | contract-acp-minimal | Pass |  |
| surface.app-server-lifecycle | contract-appserver-transport | Pass |  |
| surface.cli-flags | process-conflicting-resume | IntentionalDivergence | stderr differs |
| surface.cli-flags | process-help | IntentionalDivergence | stdout differs |
| surface.cli-flags | process-invalid-max-turns | IntentionalDivergence | stderr differs |
| surface.cli-flags | process-version-long | IntentionalDivergence | stdout differs |
| surface.cli-flags | process-version-short | IntentionalDivergence | stdout differs |
| surface.engine-loop | contract-engine-loop | Pass |  |
| surface.persistence-formats | persistence-jsonl-empty-invalid | Pass |  |
| surface.persistence-formats | persistence-jsonl-empty-valid | Pass |  |
| surface.persistence-formats | persistence-jsonl-valid | Pass |  |
| surface.provider-dialects | contract-provider-dialects | Pass |  |
| surface.provider-mistral | contract-provider-mistral | Pass |  |
| surface.turn-lifecycle | contract-turn-lifecycle | Pass |  |
| surface.wire-variants | contract-event-families | Pass |  |
