# Compatibility report

- Baseline: `mistral-vibe@2.23.1`
- Rust build: `mistral-vibe-rs@2.23.1`
- Source revision: `8b7894f92dcc26a0f0a9f7b24f0a91dd45cac80b`
- Dirty source: `true`
- Release: `5`
- Native certification: `42/43` rows
- Excluded boundaries documented: `1/1` rows

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
| surface.acp | contract-acp-full | Pass |  |
| surface.acp-minimal | contract-acp-minimal | Pass |  |
| surface.app-server-lifecycle | contract-appserver-transport | Pass |  |
| surface.cli-flags | process-conflicting-resume | IntentionalDivergence | stderr differs |
| surface.cli-flags | process-help | IntentionalDivergence | stdout differs |
| surface.cli-flags | process-invalid-max-turns | IntentionalDivergence | stderr differs |
| surface.cli-flags | process-version-long | IntentionalDivergence | stdout differs |
| surface.cli-flags | process-version-short | IntentionalDivergence | stdout differs |
| surface.cloud-workflows | contract-cloud-workflows | Pass |  |
| surface.config-layers | contract-config-layers | Pass |  |
| surface.distribution | contract-distribution | IntentionalDivergence | first semantic difference at /jsonFrames/0/checks/checksumFailurePreservesBinary |
| surface.engine-loop | contract-engine-loop | Pass |  |
| surface.extension-discovery | contract-extension-discovery | Pass |  |
| surface.managed-processes | contract-managed-processes | Pass |  |
| surface.mcp | contract-mcp-lifecycle | IntentionalDivergence | first semantic difference at /0/checks/liveRevocation |
| surface.mcp-stdio-extension | contract-mcp-stdio-extension | Pass |  |
| surface.native-targets | contract-native-targets | Blocked | capability matrix row is explicitly blocked |
| surface.operational-resources | contract-operational-resources | Pass |  |
| surface.persistence-formats | persistence-jsonl-empty-invalid | Pass |  |
| surface.persistence-formats | persistence-jsonl-empty-valid | Pass |  |
| surface.persistence-formats | persistence-jsonl-valid | Pass |  |
| surface.prompt-composition | contract-prompt-composition | Pass |  |
| surface.provider-dialects | contract-provider-dialects | Pass |  |
| surface.provider-mistral | contract-provider-mistral | Pass |  |
| surface.python-custom-tools | contract-python-custom-tools | IntentionalDivergence | first semantic difference at /jsonFrames/0/boundary |
| surface.review-tools | contract-review-tools | Pass |  |
| surface.session-continuity | contract-session-continuity | Pass |  |
| surface.session-lifecycle | contract-session-lifecycle | Pass |  |
| surface.shell-policy | contract-shell-policy | IntentionalDivergence | first semantic difference at /0/checks/destructive |
| surface.subagents | contract-subagents | Pass |  |
| surface.telemetry | contract-telemetry | IntentionalDivergence | first semantic difference at /jsonFrames/0/checks/compatibilityEnvelope |
| surface.tool-policy | contract-tool-policy | Pass |  |
| surface.tools | contract-tool-abi | Pass |  |
| surface.tui-controls | contract-tui-controls | Pass |  |
| surface.tui-input | contract-tui-input | Pass |  |
| surface.tui-rendering | contract-tui-rendering | Pass |  |
| surface.tui-setup | contract-tui-setup | Pass |  |
| surface.tui-shell | contract-tui-shell | Pass |  |
| surface.tui-workflows | contract-tui-terminal-stack | Pass |  |
| surface.tui-workflows | pty-help | Pass |  |
| surface.tui-workflows | pty-version | Pass |  |
| surface.turn-lifecycle | contract-turn-lifecycle | Pass |  |
| surface.wire-variants | contract-event-families | Pass |  |
| surface.workspace-tools | contract-workspace-tools | Pass |  |

## Certification failures

- surface.native-targets is not certified required-native behavior
