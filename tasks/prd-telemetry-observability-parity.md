[PRD]
# PRD: Telemetry and Observability Parity

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-11 | Arthur Jean | Initial draft. Closes rank 15 of `docs/parity.md`. |

## Problem Statement

`docs/parity.md` scores the telemetry and observability part 60/100, the lowest
score of any part this repository still intends to close, and rank 15 of the
execution order is the only rank below 16 still marked `PARTIAL`. The score is
also the least trustworthy number in that document, because it is declarative:
every part scored 100 is backed by a differential oracle, and this one is backed
by reading source.

Measured against the pinned reference `b78b451`, the gap is the following.

1. **The datalake envelope diverges, and that divergence cascades.** The
   reference posts `{"event": name, "properties": base_metadata | payload,
   "correlation_id"?}` to `POST {server}/v1/datalake/events`
   (`vibe/core/telemetry/send.py:38-39,153-196`). This port posts a closed
   vocabulary under `{"schemaVersion":1,"event":...,"properties":{"metadata":
   {...},"attributes":{...}},"correlationId":...}` with an invented `product`
   field (`crates/vibe-core/src/telemetry.rs:214-252`). Because there is no
   open-properties envelope, three downstream surfaces have nowhere to ship:
   `telemetry/record` keeps its event in a memory ring buffer instead of
   sending it (`crates/vibe-app-server/src/server.rs:3860-3888`), the four
   audio events are kept locally (`crates/vibe-cli/src/tui/mod.rs:1042-1057`),
   and the ACP `telemetry/send` notification is absent entirely.
2. **The base metadata carries 6 of 15 fields.** The reference builds
   `TelemetryBaseMetadata` (12 fields) and `TelemetryRequestMetadata` (3 more)
   through `build_base_metadata`, `build_request_metadata` and
   `build_launch_context` (`vibe/core/telemetry/types.py:48-69`,
   `vibe/core/telemetry/build_metadata.py:19-95`). Absent here: the `os` and
   `os_version` split, `terminal_emulator`, `session_id`,
   `parent_session_id`, `experiments`, `user_plan`, `call_type`,
   `call_source`, `message_id`. `LaunchContext` has no counterpart.
3. **5 of 26 events are emitted, and their payloads are thinner than their
   names suggest.** The reference emits 19 `vibe.*` events plus 7 audio events.
   This port ships 5. Seven event names are declared and never constructed
   outside tests (`crates/vibe-core/src/telemetry.rs:21-54`). Where a name does
   match, the payload does not: `vibe.tool_call_finished` carries 11 fields
   upstream (`vibe/core/telemetry/send.py:243-276`) and 2 here, one of which
   (`duration_ms`) is not a reference field at all while `tool_name` is never
   set; `vibe.request_sent` carries 8 fields upstream and 1 here.
4. **OTel is absent in totality, 465 lines.** No `opentelemetry` dependency
   exists in the workspace. `setup_tracing`, the exporter-config resolution,
   the two redaction modes, the four span families, the baggage propagation of
   the conversation id, the `gen_ai.*` attribute set, the provider-name
   normalization and its 7 aliases, and the three post-hoc attribute setters
   all have no counterpart (`vibe/core/tracing.py:39-465`). The three
   configuration keys `enable_otel`, `otel_endpoint` and `otel_redaction` are
   declared and read by nothing
   (`crates/vibe-core/src/config/registry.rs:775-790`), which is precisely the
   failure `docs/parity.md` names in its own configuration row: declaring a key
   is not implementing its feature.
5. **Observability is absent, 307 lines.** No rotating file log at
   `$VIBE_HOME/logs/vibe.log`, no `timestamp ppid pid level message` line
   format, no backslash and newline encoding round-trip, no `LOG_LEVEL` /
   `DEBUG_MODE` / `LOG_MAX_BYTES` handling (`vibe/observability/logging.py`),
   and no `LogReader` (`vibe/core/log_reader.py`). `diagnostics/logs/read`
   answers from a process-local ring buffer, so `ppid` and `pid` are hardcoded
   to 0, entry identity is a synthetic `log-{index}`, and nothing written by
   another process is readable (`crates/vibe-app-server/src/resources.rs:515-548`).
6. **Telemetry is opt-in here and opt-out upstream.** The reference reads
   `enable_telemetry`, which defaults to `True`
   (`vibe/core/config/vibe_schema.py:441`, `vibe/cli/cli.py:409-413`). This
   port ignores the key on the CLI path and gates on a clap flag `--telemetry`
   defaulting to false, locked in by a test
   (`crates/vibe-cli/src/lib.rs:92,636-660`). A configuration file that sets
   `enable_telemetry = true` ships nothing. `--telemetry` is also a flag the
   reference does not publish.

**Why now:** rank 14 closed on 2026-08-11 and nothing of it remains open, so
rank 15 is the first rank of the execution order still carrying work. It has no
downstream consumer, which is exactly why it has been deferred five times; it is
also the last part whose score is unbacked by an instrument, so leaving it there
means the weighted total in `docs/parity.md` keeps a hand-maintained judgement
over a number nothing can reproduce. Deferring further costs nothing in
migration and everything in measurement credibility.

## Overview

This PRD closes rank 15 by building the instrument first, then the surface, and
by deciding two divergences rather than porting them.

The instrument is a differential oracle in the shape every closed rank already
uses: a capture script that drives the reference's own telemetry client,
tracing module, log formatter and log reader over inputs the script authors, and
a Rust replay that reads the committed corpus unconditionally while only the
recapture probe skips when the checkout is absent or off-pin. The capture runs
with no network and no credentials: every environment variable a document names
is set to a sentinel first, and a socket guard fails the run on any connection
attempt, the same isolation `scripts/parity/setup_auth.py` already asserts. No
reference-authored prose enters the corpus; message text is recorded as a length
plus a SHA-256, as `NOTICE` requires.

The surface work is decided by one strategic call: the closed envelope is
replaced by the reference's open-properties envelope. Keeping the closed one
would permanently block event shipping, `telemetry/record`, the audio events and
the ACP `telemetry/send`, capping this part around 75 whatever else is built.
Adopting the reference envelope means client-authored properties reach the
Mistral datalake, which is why it arrives together with the opt-out gate: the
existing label validators are kept as an internal invariant on events this port
authors itself, and are never applied to properties a client explicitly recorded
through `telemetry/record`. `enable_telemetry`, read from the merged
configuration and defaulting to true, becomes the single decision point, and the
`--telemetry` flag is retired.

Two things are decided rather than ported. Sentry ships dormant upstream:
`_CLI_SENTRY_DSN` and `_ACP_SENTRY_DSN` are both `None` at the pinned commit
(`vibe/observability/sentry.py:15-16`), so `sentry_sdk.init(dsn=None)` never
initializes and `init_sentry` always returns `False`. That is the same shape as
the remote skills registry this document already records as dormant, and it is
closed by a divergence row plus a test that asserts the dormancy, not by
`sentry-rust`. The `experiments` metadata field is carried on the wire and stays
empty until rank 16 ships GrowthBook, recorded as its own row rather than left
as a silent hole.

## Goals

| Goal | At EP-003 completion | At PRD completion |
|------|---------------------|-------------------|
| `docs/parity.md` telemetry row, measured by oracle | 80, restated from printed counts | 100, restated from printed counts |
| Reference event names emitted with reference payload keys | 26/26 names, 26/26 payload key sets | 26/26 held by the replay |
| Base and request metadata fields produced | 15/15 | 15/15 held by the replay |
| Span families reproduced with their attribute sets | 0/4 | 4/4 |
| Oracle comparisons printed per run | >= 250 across >= 6 families | >= 400 across >= 10 families |
| Divergences outside the ledger | 0 | 0 |

## Target Users

### Parity maintainer

- **Role:** the engineer who restates a score in `docs/parity.md` and has to
  defend the number.
- **Behaviors:** runs `cargo test -p <crate> --all-features <area>_parity_tests
  -- --nocapture`, quotes the printed per-family counts into the document, and
  reads the ledger to know what is still open.
- **Pain points:** the telemetry row is the only one that cannot be reproduced.
  Its 60 comes from reading source, so a rerun cannot disagree with it, which
  makes the weighted total partly unfalsifiable.
- **Current workaround:** reading `vibe/core/telemetry/send.py` beside
  `crates/vibe-core/src/telemetry.rs` by hand, once per audit.
- **Success looks like:** one command prints a ledger, a per-family conforming
  count and a closing total, and the document quotes those lines verbatim.

### Vibe operator

- **Role:** the person running the `vibe` binary who wants product telemetry off,
  or wants to see what the agent logged when a turn misbehaved.
- **Behaviors:** sets `enable_telemetry = false` in the configuration, opens the
  debug console, reads `$VIBE_HOME/logs/vibe.log` after a crash.
- **Pain points:** `enable_telemetry` does nothing on the CLI path, so the only
  way to stop event shipping is to not pass an undocumented flag that is already
  off; and there is no log file, so a failure that happened before the app
  server attached leaves no trace on disk.
- **Current workaround:** none for the log file. For telemetry, the accidental
  default of the flag.
- **Success looks like:** the configuration key decides, in both directions,
  and a rotating log file exists at the reference path with the reference line
  format.

### Observability consumer

- **Role:** a team pointing an OTLP collector at the agent to see turns, tool
  calls and model calls as spans.
- **Behaviors:** sets `enable_otel = true` and `otel_endpoint`, expects
  `gen_ai.*` spans in their collector, expects `otel_redaction` to remove prompt
  and completion content before export.
- **Pain points:** the three keys exist in the published schema and change
  nothing, which is worse than their absence: the schema advertises a capability
  the binary does not have.
- **Current workaround:** none.
- **Success looks like:** the same four span families the reference emits, with
  the same attribute keys and the same conversation-id propagation, and a
  redaction mode that actually strips content.

## Research Findings

### Competitive Context

The only comparable here is the reference implementation itself; there is no
market to survey. What the ecosystem survey did decide is the dependency set.

- **Reference Python stack:** `opentelemetry` API and SDK 1.39.1,
  `opentelemetry-exporter-otlp-proto-http` 1.39.1,
  `opentelemetry-semantic-conventions` 0.60b1, plus
  `mistralai.extra.observability` for the redaction policy.
- **Rust equivalents, resolved from crates.io on 2026-08-11:** `opentelemetry`
  0.32.0, `opentelemetry_sdk` 0.32.1, `opentelemetry-otlp` 0.32.0 with the
  features `trace` + `http-proto` + `reqwest-client`, and
  `opentelemetry-semantic-conventions` 0.32.1.
- **Gap:** `mistralai.extra.observability` is a Python package with no Rust
  counterpart, so `default_redaction_policy` and `AttributeRedactionPolicy` are
  behavior to reproduce, not prose to avoid. `NOTICE` does not block it.

### Best Practices Applied

- **Instrument before surface.** Every part `docs/parity.md` scores 100 was
  measured by an oracle before the code that satisfies it was written, and the
  document says so explicitly ("Extend the harness before writing each phase,
  not after"). EP-001 comes first for that reason.
- **Keys from the semantic conventions crate, values declared locally.** The 15
  attribute keys the reference uses (`GEN_AI_OPERATION_NAME`,
  `GEN_AI_PROVIDER_NAME`, `GEN_AI_AGENT_NAME`, `GEN_AI_CONVERSATION_ID`,
  `GEN_AI_REQUEST_MODEL`, `GEN_AI_REQUEST_TEMPERATURE`,
  `GEN_AI_REQUEST_MAX_TOKENS`, `GEN_AI_TOOL_NAME`, `GEN_AI_TOOL_CALL_ID`,
  `GEN_AI_TOOL_CALL_ARGUMENTS`, `GEN_AI_TOOL_CALL_RESULT`, `GEN_AI_TOOL_TYPE`,
  `GEN_AI_USAGE_INPUT_TOKENS`, `GEN_AI_USAGE_OUTPUT_TOKENS`,
  `GEN_AI_RESPONSE_FINISH_REASONS`, `GEN_AI_RESPONSE_MODEL`,
  `GEN_AI_RESPONSE_ID`, `HTTP_REQUEST_METHOD`, `HTTP_URL`,
  `HTTP_RESPONSE_STATUS_CODE`) are all present in
  `opentelemetry-semantic-conventions` 0.32.1 behind the `semconv_experimental`
  feature, verified by grepping the downloaded crate source. The **value**
  vocabularies (`GenAiProviderNameValues`, `GenAiOperationNameValues`) are not:
  the Rust crate publishes keys only, zero value-enum occurrences. The provider
  alias table and the operation-name values therefore come from the oracle.
- **Wrap the exporter, do not fork it.** `opentelemetry_sdk::trace::SpanExporter`
  is an implementable trait taking `Vec<SpanData>`, which is the supported way
  to reproduce `RedactingSpanExporter`.
- **Tracing must never raise.** The reference wraps every span in `_safe_span`,
  which logs and yields `INVALID_SPAN` rather than propagating
  (`vibe/core/tracing.py:153-193`). Rust span creation is infallible and export
  errors surface only through `force_flush`/`shutdown`, so the same policy is
  reachable without `panic`, which this workspace denies anyway.

*Dependency versions resolved with `cargo info` against crates.io on
2026-08-11; semconv constant presence verified against the downloaded crate
source.*

## Assumptions & Constraints

### Assumptions (to validate)

- **The baggage surface is equivalent.** The reference propagates the
  conversation id with `context.attach(baggage.set_baggage(...))` and detaches a
  token. The Rust equivalent is believed to be `opentelemetry::baggage::BaggageExt`
  with `Context::with_baggage` and an RAII `ContextGuard`, but this was **not
  verified** by documentation retrieval. US-013 proves it with a test before any
  span family is written; if it is absent, the conversation id is threaded
  explicitly and the divergence is recorded.
- **The `gen_ai.*` constant family is stable across 0.32.x.** Present today, but
  the `mcp.*` constants in the same crate carry `#[deprecated]` notes saying
  they moved to the GenAI semantic conventions repository, so this family
  carries churn risk. Mitigated by pinning the crate minor version and letting
  the oracle fail on a key that changes.
- **`experiments` stays empty.** The reference fills it from its experiments
  manager. Rank 16 is `TODO`, so the field is carried and always empty here.
  Recorded as a divergence rather than assumed away.
- **`user_plan` is reachable for three of four statuses.** It comes from
  `account/read`, which `docs/parity.md` already records as unable to answer
  `unauthorized`. The field is produced from the three statuses this port
  classifies.

### Hard Constraints

- `NOTICE` forbids copying reference source, prompts or tool description text.
  Every reference-authored sentence enters the corpus as a length plus a
  SHA-256, and every counterpart here is written originally.
- The pin lives in exactly two places, `vibe_core::parity::REFERENCE_COMMIT` and
  `EXPECTED_COMMIT` in `scripts/parity/pin.py`. A new parity test calls
  `vibe_core::parity::reference_root` rather than spelling a path.
- A missing or off-pin reference checkout must never fail `cargo test`: the
  replay runs unconditionally against the committed corpus and only the
  recapture probe skips.
- The dependency layering in `[workspace.metadata.vibe] dependency-layers`
  holds: telemetry and tracing contracts belong to `vibe-core`, session
  dispatch to `vibe-app-server`, and `vibe-cli` and `vibe-acp` stay adapters.
- `unsafe_code` is forbidden workspace-wide; `panic`, `unimplemented` and
  `dbg_macro` are denied in non-test code.
- `crates/vibe-core/src/parity/ledger_tests.rs` reads the accepted-divergences
  table of `docs/parity.md` and fails when a row names an artifact the
  repository no longer holds, so every new divergence row must name a real
  symbol.

## Quality Gates

These commands must pass for every user story, run from the workspace root:

- `cargo fmt --all -- --check` - formatting
- `cargo check --workspace --all-targets --all-features` - compilation
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` - lints
- `cargo test --workspace --all-features` - the full suite, never filtered to
  the module under edit, because parity fixtures are read from more than one
  crate

For stories that touch a parity corpus, additionally:

- `cargo test -p <crate> --all-features telemetry_parity_tests -- --nocapture` -
  prints the ledger, one conforming count per family and the closing total that
  `docs/parity.md` quotes

## Reference Map

Every file an implementer opens before writing Rust, at the pinned commit
`b78b451`. Paths use the Linux canonical spelling `/home/arthur/dev/mistral-vibe/`
and resolve against whichever checkout is local, through `VIBE_REFERENCE` or
`--reference`; Rust tests reach the same root through
`vibe_core::parity::reference_root`. Each story below names its own anchor; this
is the whole surface in one place. Reading these is required by `AGENTS.md`, and
grepping them does not replace opening the declaration they point at.

The reference splits this part across four subtrees that are never read in
isolation: [/home/arthur/dev/mistral-vibe/vibe/core/telemetry/](/home/arthur/dev/mistral-vibe/vibe/core/telemetry)
holds the client and the metadata census,
[/home/arthur/dev/mistral-vibe/vibe/core/tracing.py](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py)
holds all of OTel in one module,
[/home/arthur/dev/mistral-vibe/vibe/observability/](/home/arthur/dev/mistral-vibe/vibe/observability)
holds the log formatter and the dormant crash reporter, and five satellite
`telemetry.py` files hold the per-feature tracking state. 1 950 lines of
production code in total. Open the directory before the individual file: the
client publishes 19 named senders whose payloads are decided in the modules that
call them, and a sender read without its call site reads as arbitrary.

### The telemetry client (3 files, 682 lines)

- [vibe/core/telemetry/send.py](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py),
  473 lines: `_DEFAULT_TELEMETRY_BASE_URL` (38) and `_DATALAKE_EVENTS_PATH` (39)
  for the endpoint this PRD reproduces, `get_mistral_provider_and_api_key` (42)
  for the rule that no third-party credential reaches a Mistral endpoint,
  `_extract_file_extension` (66), `TelemetryClient` (73), `__init__` (74) for the
  six getters the client is built from, `_get_telemetry_url` (93),
  `_is_enabled` (104) with its exception fallback, `is_active` (110), the
  `client` property (114) for the 5.0 second timeout and the 5/10 connection
  limits, `build_client_event_metadata` (141), `send_telemetry_event` (153) for
  the envelope, the header set and the fire-and-forget task,  `aclose` (198) for
  the flush, `_calculate_file_metrics` (205), `_extract_bash_background` (232),
  and the 15 named senders at 243, 278, 282, 286, 305, 318, 324, 345, 348, 362,
  368, 395, 399, 415, 422, 441 and 466.
- [vibe/core/telemetry/types.py](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/types.py),
  114 lines: `AttachmentKind` (12), `ClientMetadata` (16), `LaunchContext` (21)
  with `telemetry_fields` (28) and `sentry_tags` (41), `TelemetryCallType` (45),
  `TelemetryBaseMetadata` (48) for the 12 fields and `use_enum_values`,
  `TelemetryRequestMetadata` (65) for the 3 more, `TeleportFailureStage` (71)
  for the 7 stages, `TeleportContextSummaryStatus` (80),
  `ProjectSelectionSource` (81), `RemoteProjectOutcome` (84),
  `TeleportFailureDetails` (87), `ProjectPickerTelemetryPayload` (92),
  `TeleportCompletedPayload` (101), `TeleportFailedPayload` (108),
  `RemoteProjectConfiguredPayload` (113).
- [vibe/core/telemetry/build_metadata.py](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/build_metadata.py),
  95 lines: `build_base_metadata` (19) with its `exclude_none` at (41),
  `build_request_metadata` (45), `build_attachment_counts` (70) for the
  `supports_images` gate, `build_launch_context` (81).
- [vibe/utils/__init__.py:7](/home/arthur/dev/mistral-vibe/vibe/utils/__init__.py):
  `AgentEntrypoint`, four values.
  [vibe/utils/terminal.py:6-19](/home/arthur/dev/mistral-vibe/vibe/utils/terminal.py):
  `TerminalEmulator`, 13 values, already modeled here at
  `crates/vibe-protocol/src/lib.rs:386`.

### Tracing (1 file, 465 lines)

- [vibe/core/tracing.py](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py):
  the seven constants (30-36) including the three `vibe.*` attribute keys the
  semantic conventions do not publish, `build_otel_span_exporter_config` (39)
  for the explicit-endpoint branch (50), the Mistral-derived branch (55-65) and
  the missing-key warning (67), `setup_tracing` (76) for the two-key gate (77),
  the resource (92), the redaction wrap (97-109) and the `atexit` shutdown
  (113), `_get_tracer` (116), `_backend_error_from` (122) for the cause and
  context walk, `_exception_status` (135) for the three-way status decision,
  `_safe_span` (154) for the never-raise policy and the `INVALID_SPAN` fallback
  (167), `agent_span` (197) with the baggage attach at (215-218),
  `tool_span` (228), `_provider_attribute_value` (248) for the 7 aliases and the
  `unknown` fallback (270), `model_call_span` (274) with the conversation-id
  resolution at (310-314), `set_model_call_http_status` (327),
  `set_model_call_usage` (339), `_string_attribute` (356),
  `_response_finish_reasons` (362), `_response_metadata_sources` (387),
  `set_model_call_response_metadata` (395), `hook_span` (434),
  `set_tool_result` (459).
- The redaction policy itself lives outside the tree, in
  `mistralai.extra.observability` (`AttributeRedactionPolicy`,
  `RedactingSpanExporter`, `default_redaction_policy`), imported at
  [tracing.py:98-102](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py). It is
  a dependency, not reference source: reproduce the attribute sets it removes,
  measured by the `redaction` corpus family, never its code.

### Observability and the log reader (4 files, 543 lines)

- [vibe/observability/logging.py](/home/arthur/dev/mistral-vibe/vibe/observability/logging.py),
  80 lines: `logger` (10), `StructuredLogFormatter` (13) with `format` (14) for
  the exact field order, `_VibeFileHandler` (28), `init_file_logging` (32) with
  the duplicate-handler guard (36), the 10 MiB default (44), the `DEBUG_MODE`
  and `LOG_LEVEL` resolution (46-51) and `backupCount=0` (54),
  `encode_log_message` (62), `decode_log_message` (66).
- [vibe/core/log_reader.py](/home/arthur/dev/mistral-vibe/vibe/core/log_reader.py),
  227 lines: `LogEntry` (16) for the seven fields, `PaginatedLogs` (27),
  `DEFAULT_LOG_PATTERN` (37) for the five capture groups, `LOG_POLL_INTERVAL`
  (44), `LogReader` (47), `get_logs` (73) with the `has_more` and `cursor`
  decision (83-87), `_read_lines_backward` (89) for the reverse chunked read,
  the `_new_lines_count` skew correction (97) and the 8 KiB chunk (98),
  `set_consumer` (131), `start_watching` (134), `stop_watching` (149) with the
  1 second join, `shutdown` (159), `_poll_log_loop` (162),
  `_process_new_content` (172) with the shrink reset (185), `_parse_line` (205).
- [vibe/core/paths/_vibe_home.py:12](/home/arthur/dev/mistral-vibe/vibe/core/paths/_vibe_home.py):
  `LOG_FILE`, the path US-017 writes to.
- [vibe/observability/sentry.py](/home/arthur/dev/mistral-vibe/vibe/observability/sentry.py),
  233 lines: read it only to confirm the divergence US-020 records.
  `_CLI_SENTRY_DSN` (15) and `_ACP_SENTRY_DSN` (16) are both `None` at this pin,
  which is the whole argument; `SentryTarget` (19), the filter sets (42, 48, 55,
  78, 88), `scrub_paths` (104) with `_PATH_RE` (92) and `_HOME_RE` (97),
  `_is_benign_exception` (126), `_before_send` (162), `init_sentry` (177) with
  the `is_initialized` check (199) that makes the null DSN observable,
  `capture_sentry_exception` (212).

### The satellite emitters (5 files, 263 lines)

- [vibe/core/teleport/telemetry.py](/home/arthur/dev/mistral-vibe/vibe/core/teleport/telemetry.py),
  116 lines: `send_teleport_early_failure_telemetry` (24),
  `TeleportTelemetryTracker` (41) with `record_event` (53) for the six-event
  stage machine, `record_service_error` (69) with the 403/404 saved-link
  clearing (72-77), `record_context_summary_generated` (79),
  `record_context_summary_failed` (83), `record_cancelled` (88),
  `record_unexpected_error` (92), `send_success` (95),
  `send_failure_if_needed` (104) with its success short circuit.
- [vibe/core/vibe_code_project/telemetry.py](/home/arthur/dev/mistral-vibe/vibe/core/vibe_code_project/telemetry.py),
  70 lines: `build_project_picker_telemetry` (19),
  `build_headless_project_telemetry` (38),
  `build_project_resolution_failed_telemetry` (51),
  `count_multi_repo_matches` (62) for the more-than-one-repository rule.
- [vibe/cli/voice_manager/telemetry.py](/home/arthur/dev/mistral-vibe/vibe/cli/voice_manager/telemetry.py),
  30 lines: `TranscriptionTrackingState` (8) and its five methods. Already
  ported at `crates/vibe-cli/src/tui/voice/telemetry.rs`; read it to confirm
  US-012 changes only where the events go, never what they carry.
- [vibe/cli/narrator_manager/telemetry.py](/home/arthur/dev/mistral-vibe/vibe/cli/narrator_manager/telemetry.py),
  30 lines: `ReadAloudTrackingState` (9), `reset` (14) for the UUID session id,
  `mark_play_started` (19), `time_to_first_read_s` (22) and
  `elapsed_since_play_s` (27), both returning 0.0 when playback never started.
  No counterpart here.
- [vibe/cli/telemetry.py](/home/arthur/dev/mistral-vibe/vibe/cli/telemetry.py),
  17 lines: `ClientTelemetry` (6), the two-method protocol the CLI depends on.

### Where the events are raised

- [vibe/core/agent_loop/_loop.py](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py):
  the telemetry imports (90-102), the pending flags (445-446), the
  `TelemetryClient` construction and its six getters (547), the tracker handoff
  (597), the ready emission (662), and the OTel gate passed to the backend
  (1073-1080), which is the only place `enable_otel` changes a request.
- [vibe/core/agent_loop_hooks.py](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop_hooks.py):
  the span threaded through the hook path (57, 81, 112, 263, 348, 370), which is
  what US-016 wires.
- [vibe/cli/textual_ui/app.py](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py):
  `_send_startup_telemetry_once` (971) for the three durations and the
  once-per-process latch, `vibe.voice_mode_toggled` (1325),
  `_send_skill_telemetry` (1681) for the slash-command payload, and the two
  crash-capture call sites (2087, 4320).
- [vibe/cli/voice_manager/voice_manager.py:202-251](/home/arthur/dev/mistral-vibe/vibe/cli/voice_manager/voice_manager.py):
  the four transcription emitters.
  [vibe/cli/narrator_manager/narrator_manager.py:226-262](/home/arthur/dev/mistral-vibe/vibe/cli/narrator_manager/narrator_manager.py):
  the three read-aloud emitters, `_on_read_aloud_requested` (226),
  `_on_read_aloud_play_started` (238) and `_on_read_aloud_ended` (250).
- [vibe/cli/cli.py:409-413](/home/arthur/dev/mistral-vibe/vibe/cli/cli.py): the
  one place `enable_telemetry` is read on the CLI path, which US-007 reproduces.
  [vibe/cli/entrypoint.py:278-280](/home/arthur/dev/mistral-vibe/vibe/cli/entrypoint.py)
  and [vibe/app_server/stdio.py:36](/home/arthur/dev/mistral-vibe/vibe/app_server/stdio.py):
  where file logging initializes.
- [vibe/acp/agent.py](/home/arthur/dev/mistral-vibe/vibe/acp/agent.py):
  `TelemetryNotification` (215) for the three-field model with its `sessionId`
  alias, `ext_notification` (1002-1031) for the method guard, the two routed
  event names and the warning branch, plus the three `resources.telemetry.record`
  call sites (652, 669, 1201).
  [vibe/acp/entrypoint.py:99-100](/home/arthur/dev/mistral-vibe/vibe/acp/entrypoint.py):
  the ACP-side `enable_telemetry` read.
  [vibe/acp/commands/controller.py:91](/home/arthur/dev/mistral-vibe/vibe/acp/commands/controller.py):
  the slash-command record.

### The configuration and the wire

- [vibe/core/config/vibe_schema.py](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py):
  `enable_otel` (414), `otel_endpoint` (415), `otel_redaction` (416) and
  `enable_telemetry` (441) with its default of `True`, which is the whole of
  US-007's argument.
  [vibe/core/config/models.py:598-608](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py):
  `OtelRedactionMode` for the three values and `OtelSpanExporterConfig` for the
  endpoint and header pair.
- [vibe/app_server/protocol.py](/home/arthur/dev/mistral-vibe/vibe/app_server/protocol.py):
  `diagnostics/logs/read` (100) and `telemetry/record` (155) in the inventory,
  `DiagnosticsLogsReadParams` (653) for the 1 to 500 limit and the non-negative
  offset, `DiagnosticsLogsReadResponse` (659), `TelemetryRecordParams` (1078)
  for the four fields this port already models.
  [vibe/app_server/models.py:348-361](/home/arthur/dev/mistral-vibe/vibe/app_server/models.py):
  `DebugLogEntry` for the seven fields including the `datetime` timestamp, and
  `DebugLogPage`.
- [vibe/app_server/_resources.py](/home/arthur/dev/mistral-vibe/vibe/app_server/_resources.py):
  the `LogReader` construction (161), the `diagnostics/logs/read` dispatch
  (376), `_dispatch_telemetry` (487-499) for the `correlate_last_request`
  behavior, `_diagnostics_logs_read` (843).
  [vibe/app_server/_service_resources.py:57](/home/arthur/dev/mistral-vibe/vibe/app_server/_service_resources.py):
  `TelemetryResource` and its `_record` (106).
  [vibe/app_server/_runtime_resources.py:405](/home/arthur/dev/mistral-vibe/vibe/app_server/_runtime_resources.py):
  `read_logs`.

### The behavioral inventory

The reference's own tests are the checklist for what the corpus families must
cover, 206 test functions across 14 files. Read them for the cases, never for
the code.

- Telemetry: 59 in
  [tests/core/telemetry/test_telemetry_send.py](/home/arthur/dev/mistral-vibe/tests/core/telemetry/test_telemetry_send.py)
  (1 341 lines, the single densest file in this part) and 4 in
  [tests/core/telemetry/test_build_attachment_counts.py](/home/arthur/dev/mistral-vibe/tests/core/telemetry/test_build_attachment_counts.py).
- Tracing: 35 in
  [tests/test_tracing.py](/home/arthur/dev/mistral-vibe/tests/test_tracing.py)
  (1 037 lines) and 9 in
  [tests/core/config/test_config_otel.py](/home/arthur/dev/mistral-vibe/tests/core/config/test_config_otel.py).
- Observability: 23 in
  [tests/observability/test_logging.py](/home/arthur/dev/mistral-vibe/tests/observability/test_logging.py),
  33 in
  [tests/core/test_log_reader.py](/home/arthur/dev/mistral-vibe/tests/core/test_log_reader.py),
  1 in
  [tests/cli/test_log_command.py](/home/arthur/dev/mistral-vibe/tests/cli/test_log_command.py),
  6 in
  [tests/observability/test_sentry.py](/home/arthur/dev/mistral-vibe/tests/observability/test_sentry.py)
  and 2 in
  [tests/observability/test_sentry_pii.py](/home/arthur/dev/mistral-vibe/tests/observability/test_sentry_pii.py).
- Emission sites: 12 in
  [tests/core/teleport/test_teleport_telemetry.py](/home/arthur/dev/mistral-vibe/tests/core/teleport/test_teleport_telemetry.py),
  9 in
  [tests/narrator_manager/test_telemetry.py](/home/arthur/dev/mistral-vibe/tests/narrator_manager/test_telemetry.py),
  6 in
  [tests/voice_manager/test_telemetry.py](/home/arthur/dev/mistral-vibe/tests/voice_manager/test_telemetry.py),
  4 in
  [tests/cli/textual_ui/test_startup_telemetry.py](/home/arthur/dev/mistral-vibe/tests/cli/textual_ui/test_startup_telemetry.py)
  and 3 in
  [tests/core/experiments/test_telemetry_integration.py](/home/arthur/dev/mistral-vibe/tests/core/experiments/test_telemetry_integration.py),
  the last of which is the only place `experiments` is observably filled and is
  therefore the evidence for the divergence US-020 records.

## Epics & User Stories

### EP-001: The telemetry oracle and its corpus

Build the differential instrument before any surface changes, so every later
story is measured rather than asserted. The oracle drives the reference's own
telemetry client, tracing module, log formatter and log reader over inputs the
script authors, with no network and no credentials.

**Definition of Done:** `scripts/parity/telemetry.py` captures every family
below into `crates/vibe-core/tests/telemetry/corpus.json`, the Rust replay reads
it unconditionally, prints a per-family conforming count and a closing total,
fails on a divergence outside its ledger and on a ledger entry that has gone
stale, and the recapture probe is the only part that skips when the checkout is
absent or off-pin.

#### US-001: Capture the envelope, metadata and event vocabulary
**Description:** As a parity maintainer, I want the reference's telemetry
envelope, metadata census and event vocabulary captured into a committed corpus
so that the event surface is compared against the reference's own answers rather
than against a reading of its source.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None

**Reference:** [vibe/core/telemetry/send.py:73-203](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py:73-203) for the client the capture drives directly, [types.py:16-114](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/types.py:16-114) for `LaunchContext`, the two metadata models and the four payload TypedDicts, [build_metadata.py:19-95](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/build_metadata.py:19-95) for the four builders, and [send.py:243-473](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py:243-473) for the 15 named senders whose payload keys become the `eventPayloads` family. The 63 cases to cover are in [tests/core/telemetry/](/home/arthur/dev/mistral-vibe/tests/core/telemetry/). The local pattern to follow is `scripts/parity/setup_auth.py` for the socket guard and the credential sentinels, `scripts/parity/voice.py` for intercepting one call before the connection, and `scripts/parity/config_surface.py` for the interpreter re-exec and the `git archive` read

**Acceptance Criteria:**
- [ ] Given the pinned checkout, when `scripts/parity/telemetry.py` runs, then it
      re-executes itself with the reference interpreter, accepts `--reference`,
      and reads `VIBE_REFERENCE` when the flag is absent
- [ ] Given a `TelemetryClient` driven over scripted configurations, when an
      event is sent, then the request is intercepted one call before the
      connection and the captured record carries the resolved URL, the header
      names, the event name and the full property key set with value types
- [ ] Given the reference models, when the capture runs, then the `baseMetadata`
      family records all 12 `TelemetryBaseMetadata` fields and the 3 additional
      `TelemetryRequestMetadata` fields with their defaults and their
      `exclude_none` behavior
- [ ] Given every `send_*` method on `TelemetryClient` plus the emitters in
      `vibe/cli/`, `vibe/acp/` and `vibe/core/teleport/`, when the capture runs,
      then the `eventVocabulary` family records all 26 event names and the
      `eventPayloads` family records each one's property key set
- [ ] Given a document that names an environment variable, when the capture
      runs, then the variable is set to a sentinel first and any resolved
      credential is recorded as the variable it came from, never as a value
- [ ] Given any attempt to open a socket during the capture, when the guard
      fires, then the run fails rather than recording a network-dependent answer
- [ ] Given a reference-authored message, when it is captured, then only its
      byte length and SHA-256 are recorded

#### US-002: Capture the tracing families
**Description:** As a parity maintainer, I want the reference's exporter-config
resolution, span families and redaction behavior captured so that the OTel port
is measured against observable answers rather than against a reimplementation
that merely looks similar.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-001

**Reference:** [vibe/core/tracing.py:39-73](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py:39-73) for the exporter resolution and its two branches, [tracing.py:76-113](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py:76-113) for `setup_tracing`, [tracing.py:154-193](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py:154-193) for the never-raise policy and the status decision, [tracing.py:197-245](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py:197-245) for the agent and tool spans and the baggage attach, [tracing.py:248-270](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py:248-270) for the 7 provider aliases, [tracing.py:274-354](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py:274-354) for the model call span and its post-hoc setters, [tracing.py:434-465](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py:434-465) for the hook span, and [config/models.py:598-608](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py:598-608) for the three redaction modes. The 35 cases are in [tests/test_tracing.py](/home/arthur/dev/mistral-vibe/tests/test_tracing.py)

**Acceptance Criteria:**
- [ ] Given configurations that vary `enable_telemetry`, `enable_otel`,
      `otel_endpoint` and the Mistral provider, when the capture runs, then the
      `exporterConfig` family records the resolved endpoint, the header names
      and whether `setup_tracing` returned without installing a provider
- [ ] Given an in-memory span exporter installed in place of the OTLP one, when
      each of `agent_span`, `tool_span`, `model_call_span` and `hook_span` runs,
      then the `spans` family records the span name, the attribute key set and
      every non-sensitive attribute value
- [ ] Given a `model_call_span` nested inside an `agent_span`, when the capture
      runs, then the recorded child carries the conversation id the parent set
      in baggage
- [ ] Given the reference's provider names and its 7 aliases, when the capture
      runs, then the `providerNames` family records the normalized value for
      each input, including the empty and unknown inputs
- [ ] Given each of the three `OtelRedactionMode` values, when a span with
      content attributes is exported, then the `redaction` family records which
      attribute keys survive
- [ ] Given a span whose body raises a `BackendError` and one that raises a
      plain exception, when the capture runs, then the recorded status
      description and the recorded `record_exception` decision differ as
      `_exception_status` decides, with the message recorded as a digest
- [ ] Given a reference checkout that is absent or off-pin, when the capture is
      invoked, then it reports why and exits without writing a partial corpus

#### US-003: Capture the observability families
**Description:** As a parity maintainer, I want the reference's log line format,
its parse verdicts and its backward-paginated reader captured so that the log
surface is compared entry for entry.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001

**Reference:** [vibe/observability/logging.py:13-71](/home/arthur/dev/mistral-vibe/vibe/observability/logging.py:13-71) for the formatter, the handler resolution and the encode and decode pair, and [vibe/core/log_reader.py:37-226](/home/arthur/dev/mistral-vibe/vibe/core/log_reader.py:37-226) for the pattern, `get_logs` and `_parse_line`. The 57 cases are in [tests/observability/test_logging.py](/home/arthur/dev/mistral-vibe/tests/observability/test_logging.py) and [tests/core/test_log_reader.py](/home/arthur/dev/mistral-vibe/tests/core/test_log_reader.py)

**Acceptance Criteria:**
- [ ] Given records at each of the five levels, with and without exception info,
      when `StructuredLogFormatter` formats them, then the `logFormat` family
      records the field order, the separator and the encoded message, with the
      message body supplied by the script rather than by the reference
- [ ] Given messages containing backslashes and newlines, when they round-trip
      through `encode_log_message` and `decode_log_message`, then the
      `logEncoding` family records both directions
- [ ] Given lines that match and lines that do not, including a malformed
      timestamp, an unknown level and an empty line, when `_parse_line` runs,
      then the `logParse` family records the verdict and the parsed fields
- [ ] Given a log file the script authors, when `get_logs` is called across
      several `limit` and `offset` combinations including offsets past the end,
      then the `logPagination` family records the entry order, `has_more` and
      `cursor` for each
- [ ] Given a file whose size shrinks between calls, when the reader is polled,
      then the recorded position reset is captured rather than silently ignored
- [ ] Given `LOG_LEVEL`, `DEBUG_MODE` and `LOG_MAX_BYTES` set to valid and
      invalid values, when `init_file_logging` runs, then the `logConfig` family
      records the resolved level and rotation size for each

#### US-004: Replay the corpus from Rust with a ledger
**Description:** As a parity maintainer, I want a Rust replay that compares this
build against the committed corpus so that a rerun can disagree with the score
in `docs/parity.md`.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-001, US-002, US-003

**Reference:** no Python counterpart; the shape is a local one. Follow `crates/vibe-cli/src/tui/voice/voice_parity_tests.rs` for the family layout, the `DIVERGENCES` ledger and its stale check, `crates/vibe-core/src/config/surface_parity_tests.rs` for the unconditional replay with a skippable recapture probe, and `crates/vibe-core/src/parity/ledger_tests.rs` for the row-to-artifact guard

**Acceptance Criteria:**
- [ ] Given the committed corpus, when `telemetry_parity_tests` runs, then it
      replays every family unconditionally and prints one conforming count per
      family plus a closing `N comparisons across M families` line
- [ ] Given a divergence that is not in the `DIVERGENCES` ledger, when the
      replay runs, then it fails naming the family and the pointer
- [ ] Given a ledger entry whose divergence has been fixed, when the replay
      runs, then it fails as a stale entry
- [ ] Given a reference checkout that is absent or sitting at another commit,
      when the suite runs, then only the recapture probe skips and every replay
      still executes
- [ ] Given a new event name added to this build without a corpus entry, when
      the replay runs, then it fails naming the event rather than passing
      quietly
- [ ] Given the accepted-divergences table of `docs/parity.md`, when
      `ledger_tests` runs, then every row this PRD adds resolves to a symbol the
      repository holds

---

### EP-002: The reference envelope and its gate

Replace the closed envelope with the reference's open-properties envelope, build
the full metadata census behind it, and move the activation decision from a CLI
flag to the configuration key the reference reads.

**Definition of Done:** an event this port sends is byte-comparable with the
reference's for its envelope shape, its metadata field set and its property
keys; `enable_telemetry` decides in both directions from the merged
configuration; the `--telemetry` flag no longer exists.

#### US-005: Publish the reference datalake envelope
**Description:** As an observability consumer, I want the events this port sends
to carry the reference envelope so that a datalake consumer written against the
reference reads them without a translation layer.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-004

**Reference:** [vibe/core/telemetry/send.py:38-39](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py:38-39) for the base URL and the datalake path, [send.py:42-63](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py:42-63) for the rule that only a Mistral provider supplies a key, [send.py:93-121](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py:93-121) for the URL join and the 5.0 second timeout with its 5/10 limits, [send.py:153-196](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py:153-196) for the envelope, the three headers and the silent exception swallow at (191), and [send.py:198-203](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py:198-203) for `aclose`

**Acceptance Criteria:**
- [ ] Given an event, when it is serialized, then the body is exactly
      `{"event": name, "properties": {...}}` plus `"correlation_id"` only when
      one is present, with no `schemaVersion`, no `product` and no nesting under
      `metadata` or `attributes`
- [ ] Given a provider `api_base`, when the endpoint is resolved, then it is the
      server URL derived from that base joined with `/v1/datalake/events`, and
      the default base is used when the derivation yields nothing
- [ ] Given a request, when it is issued, then it carries
      `Content-Type: application/json`, `Authorization: Bearer <key>` and the
      provider-derived `User-Agent`
- [ ] Given a non-Mistral active provider with a Mistral provider configured
      elsewhere, when the key is resolved, then the Mistral provider's key is
      used and no third-party credential is sent
- [ ] Given no Mistral provider at all, when an event is recorded, then nothing
      is sent and no error surfaces to the caller
- [ ] Given a delivery that fails, times out or is rejected, when the send
      completes, then the failure is swallowed, the turn is unaffected, and a
      pending send is still awaited by the flush
- [ ] Given events this port authors itself, when they are built, then the label
      validators still refuse a path, a secret-shaped token and a control
      character, and given properties a client recorded through
      `telemetry/record`, then they pass through unmodified

#### US-006: Produce the full metadata census
**Description:** As a parity maintainer, I want the 15 metadata fields the
reference produces so that every event carries the same identity, session and
launch context as upstream.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-005

**Reference:** [vibe/core/telemetry/types.py:16-69](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/types.py:16-69) for `LaunchContext` and the two metadata models, [build_metadata.py:19-95](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/build_metadata.py:19-95) for the builders and the `exclude_none` at (41), [utils/__init__.py:7](/home/arthur/dev/mistral-vibe/vibe/utils/__init__.py:7) for the four entrypoint values, and [utils/terminal.py:6-19](/home/arthur/dev/mistral-vibe/vibe/utils/terminal.py:6-19) for the 13 terminal values, already modeled here at `crates/vibe-protocol/src/lib.rs:386`. The attachment rule is [build_metadata.py:70-78](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/build_metadata.py:70-78) with its 4 cases in [tests/core/telemetry/test_build_attachment_counts.py](/home/arthur/dev/mistral-vibe/tests/core/telemetry/test_build_attachment_counts.py)

**Acceptance Criteria:**
- [ ] Given a launch, when base metadata is built, then it carries
      `agent_entrypoint`, `agent_version`, `client_name`, `client_version`,
      `os`, `os_version`, `version`, `terminal_emulator`, `session_id`,
      `parent_session_id`, `experiments` and `user_plan`
- [ ] Given a request-scoped event, when its metadata is built, then it adds
      `call_type`, `call_source` defaulting to `vibe_code`, and `message_id`
- [ ] Given a field with no value, when metadata is serialized, then the key is
      omitted rather than sent as null, matching the reference's `exclude_none`
- [ ] Given the terminal the process is attached to, when it is identified, then
      the value comes from the existing `TerminalEmulator` vocabulary in
      `crates/vibe-protocol/src/lib.rs` and is `unknown` when it cannot be
      identified
- [ ] Given a message carrying images and a provider that supports them, when
      attachment counts are built, then `image` is reported with its count, and
      given a provider that does not, then the key is absent rather than zero
- [ ] Given rank 16 is unshipped, when `experiments` is produced, then it is
      absent rather than fabricated, and the absence is recorded as a divergence
      row naming the symbol that produces it

#### US-007: Gate on `enable_telemetry` and retire the flag
**Description:** As a Vibe operator, I want `enable_telemetry` to decide whether
anything is sent so that the documented configuration key is the control, in
both directions.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-005

**Reference:** [vibe/core/config/vibe_schema.py:441](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py:441) for the default of `True`, [send.py:104-111](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py:104-111) for `_is_enabled` and its exception fallback, [cli/cli.py:409-413](/home/arthur/dev/mistral-vibe/vibe/cli/cli.py:409-413) and [acp/entrypoint.py:99-100](/home/arthur/dev/mistral-vibe/vibe/acp/entrypoint.py:99-100) for the only two places the key is read. What this story retires is local: `crates/vibe-cli/src/lib.rs:92,636-660` and the test `telemetry_is_explicitly_opt_in`

**Acceptance Criteria:**
- [ ] Given `enable_telemetry` unset, when the CLI starts, then telemetry is
      active, matching the reference default of true
- [ ] Given `enable_telemetry = false`, when any event would be sent, then
      nothing is sent from the CLI, the TUI, the app server or the ACP binary
- [ ] Given the key is changed on disk and the configuration is reloaded, when
      the next event is recorded, then the new value decides
- [ ] Given the `--telemetry` flag, when it is passed, then the binary reports
      an unknown argument, and the flag appears in no help output
- [ ] Given the configuration cannot be read at all, when telemetry eligibility
      is evaluated, then it resolves to disabled rather than propagating an
      error, matching the reference's `_is_enabled` fallback

---

### EP-003: The event vocabulary and its payloads

Emit all 26 reference events with the reference property keys, from the same
places the reference emits them.

**Definition of Done:** the `eventVocabulary` and `eventPayloads` families
replay at 26/26 with an empty ledger, and no event name is declared without a
producer.

#### US-008: Session lifecycle events
**Description:** As an observability consumer, I want `vibe.new_session`,
`vibe.session_closed`, `vibe.ready` and `vibe.startup` so that session volume
and startup latency are measurable.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-006

**Reference:** [vibe/core/telemetry/send.py:324-346](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py:324-346) for `send_new_session` and `send_session_closed` including the `unknown` entrypoint fallback, [send.py:395-397](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py:395-397) for `send_ready`, [app.py:971-992](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:971-992) for the three startup durations and the once-per-process latch, and [_loop.py:445-446](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py:445-446) with (662) for the pending flags that decide when ready fires. The 4 cases are in [tests/cli/textual_ui/test_startup_telemetry.py](/home/arthur/dev/mistral-vibe/tests/cli/textual_ui/test_startup_telemetry.py)

**Acceptance Criteria:**
- [ ] Given a session opens, when `vibe.new_session` is sent, then it carries
      `has_agents_md`, `nb_skills`, `nb_mcp_servers`, `nb_models`, `entrypoint`,
      `version`, `client_name`, `client_version` and `terminal_emulator`
- [ ] Given no launch context, when `vibe.new_session` is built, then
      `entrypoint` is `unknown` and the client fields are absent
- [ ] Given a session closes, when `vibe.session_closed` is sent, then its
      properties are the base metadata alone
- [ ] Given the agent becomes ready, when `vibe.ready` is sent, then it carries
      `init_duration_ms`
- [ ] Given the TUI draws its first frame, when `vibe.startup` is sent, then it
      carries `first_frame_duration_ms`, `agent_ready_duration_ms` and
      `session_init_duration_ms`, each null when its measurement is unavailable
- [ ] Given the startup event has already been sent in this process, when the
      trigger fires again, then nothing further is sent

#### US-009: Turn events with their full payloads
**Description:** As an observability consumer, I want `vibe.request_sent` and
`vibe.tool_call_finished` with all their reference fields so that model usage
and tool outcomes are analyzable.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-006

**Reference:** [vibe/core/telemetry/send.py:368-393](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py:368-393) for `send_request_sent` and its empty-attachment rule, [send.py:243-276](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py:243-276) for `send_tool_call_finished` and its 11 fields, [send.py:205-230](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py:205-230) for `_calculate_file_metrics` and its three tool branches, and [send.py:232-241](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py:232-241) for `_extract_bash_background` and its result-over-request precedence. The cases are in [tests/core/telemetry/test_telemetry_send.py](/home/arthur/dev/mistral-vibe/tests/core/telemetry/test_telemetry_send.py)

**Acceptance Criteria:**
- [ ] Given a backend request, when `vibe.request_sent` is sent, then it carries
      `model`, `nb_context_chars`, `nb_context_messages`, `nb_prompt_chars`,
      `call_source`, `call_type`, `message_id` and `attachment_counts`
- [ ] Given attachment counts that are all zero, when the payload is built, then
      `attachment_counts` is an empty object rather than carrying zero entries
- [ ] Given a tool call finishes, when `vibe.tool_call_finished` is sent, then
      it carries `tool_name`, `status`, `decision`, `approval_type`,
      `agent_profile_name`, `model`, `nb_files_created`, `nb_files_modified`,
      `file_extension` and `message_id`
- [ ] Given a `write_file` success, then `nb_files_created` is 1 and
      `file_extension` is the lowercased suffix; given an `edit` success, then
      `nb_files_modified` is 1; given a failure or a skip, then both counts are
      0 and the extension is absent
- [ ] Given a bash-family call, when the result reports the actual mode, then
      `bash_background` carries it, and when no result exists, then it falls
      back to the requested mode and is omitted when neither is a boolean
- [ ] Given a tool call the operator declined, when the event is sent, then
      `status` is `skipped` and not `cancelled`
- [ ] Given no decision was recorded, when the event is sent, then `decision`
      and `approval_type` are null rather than absent

#### US-010: Interaction and configuration events
**Description:** As an observability consumer, I want the interaction events so
that feature usage inside a session is measurable.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-006

**Reference:** [vibe/core/telemetry/send.py:278-284](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py:278-284) for the copied-text length and the cancelled action, [send.py:318-322](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py:318-322) for the slash-command strip, [send.py:348-366](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py:348-366) for the admin-config and onboarding payloads, [send.py:399-420](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py:399-420) for the mentions and the correlated rating, [app.py:1325](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py:1325) for the voice toggle and (1681-1688) for the skill command type, and [acp/commands/controller.py:91](/home/arthur/dev/mistral-vibe/vibe/acp/commands/controller.py:91) for the editor-side slash record

**Acceptance Criteria:**
- [ ] Given a slash command runs, when `vibe.slash_command_used` is sent, then
      `command` has its leading slash stripped and `command_type` is `builtin`
      or `skill`
- [ ] Given text is copied, when `vibe.user_copied_text` is sent, then it
      carries `text_length` and never the text
- [ ] Given an action is cancelled, when `vibe.user_cancelled_action` is sent,
      then it carries `action`
- [ ] Given mentions are inserted, when `vibe.at_mention_inserted` is sent, then
      it carries `nb_mentions`, `context_types`, `file_extensions` and
      `message_id`, with `file_extensions` null when none apply
- [ ] Given voice mode is toggled, when `vibe.voice_mode_toggled` is sent, then
      it carries `enabled`
- [ ] Given a rating is submitted, when `vibe.user_rating_feedback` is sent,
      then it carries `rating`, `version` and `model` and correlates with the
      last request
- [ ] Given admin configuration is applied, when `vibe.admin_config_applied` is
      sent, then it carries `outcome`, plus `nb_enforced_fields` only when keys
      were enforced and `has_error` only when an error occurred
- [ ] Given a key is added during onboarding, when
      `vibe.onboarding_api_key_added` is sent, then it carries `version` and
      `custom_domain`

#### US-011: Teleport and project-picker events
**Description:** As an observability consumer, I want the teleport tracker and
the project-picker payloads so that teleport failures are attributable to a
stage.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-006

**Reference:** [vibe/core/teleport/telemetry.py:24-117](/home/arthur/dev/mistral-vibe/vibe/core/teleport/telemetry.py:24-117) for the whole tracker including the stage machine at (53) and the 403/404 saved-link clearing at (72-77), [vibe_code_project/telemetry.py:19-70](/home/arthur/dev/mistral-vibe/vibe/core/vibe_code_project/telemetry.py:19-70) for the three builders and the multi-repo count, [types.py:71-114](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/types.py:71-114) for the stage, source and outcome vocabularies, and [send.py:422-473](/home/arthur/dev/mistral-vibe/vibe/core/telemetry/send.py:422-473) for the three senders. The 12 cases are in [tests/core/teleport/test_teleport_telemetry.py](/home/arthur/dev/mistral-vibe/tests/core/teleport/test_teleport_telemetry.py)

**Acceptance Criteria:**
- [ ] Given a teleport run, when its events are recorded, then the tracker moves
      through the reference's 7 failure stages as each yield event arrives
- [ ] Given a completed teleport, when `vibe.teleport_completed` is sent, then
      it carries `push_required`, `nb_session_messages`, `context_summary` and
      `context_summary_chars`, merged with the project-picker payload when one
      exists
- [ ] Given a failed teleport, when `vibe.teleport_failed` is sent, then it adds
      `stage`, `error_class` and the failure details
- [ ] Given a saved-link selection whose service error carries HTTP 403 or 404,
      when the failure is recorded, then `saved_project_link_cleared` becomes
      true
- [ ] Given a teleport that succeeded, when the failure path runs, then no
      failure event is sent
- [ ] Given a cancellation, when the run ends, then `stage` is `cancelled` and
      `error_class` is `CancelledError`
- [ ] Given a project picker, when its payload is built, then it carries
      `project_picker_shown`, `project_selection_source`,
      `project_candidate_count_loaded`, `project_multi_repo_match_count`,
      `saved_project_link_cleared` and `project_repo_remote_changed`, and the
      multi-repo count counts only projects with more than one repository linked
      to the current remote

#### US-012: Ship the audio events and add the narrator's
**Description:** As an observability consumer, I want the seven audio events
sent rather than kept locally so that voice usage is measurable on the same
terms as the rest.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-005

**Reference:** [vibe/cli/voice_manager/voice_manager.py:202-251](/home/arthur/dev/mistral-vibe/vibe/cli/voice_manager/voice_manager.py:202-251) for the four transcription emitters, already ported here at `crates/vibe-cli/src/tui/voice/telemetry.rs`, and [narrator_manager/telemetry.py:9-30](/home/arthur/dev/mistral-vibe/vibe/cli/narrator_manager/telemetry.py:9-30) with [narrator_manager.py:226-262](/home/arthur/dev/mistral-vibe/vibe/cli/narrator_manager/narrator_manager.py:226-262) for the three read-aloud events and the two elapsed measures that return 0.0 when playback never started. The 15 cases are in [tests/voice_manager/test_telemetry.py](/home/arthur/dev/mistral-vibe/tests/voice_manager/test_telemetry.py) and [tests/narrator_manager/test_telemetry.py](/home/arthur/dev/mistral-vibe/tests/narrator_manager/test_telemetry.py)

**Acceptance Criteria:**
- [ ] Given the envelope now exists, when a transcription event is produced,
      then the four `vibe.audio.transcription.*` events are sent rather than
      only written to `diagnostics/logs/read`
- [ ] Given a read-aloud request, when `vibe.read_aloud.requested` is sent, then
      it carries `read_aloud_session_id` and `trigger`
- [ ] Given playback starts, when `vibe.read_aloud.play_started` is sent, then
      it carries `read_aloud_session_id`, `time_to_first_read_s` and
      `speed_selection`
- [ ] Given playback ends, when `vibe.read_aloud.ended` is sent, then it carries
      `read_aloud_session_id`, `status`, `error_type`, `speed_selection` and
      `elapsed_seconds`
- [ ] Given playback never started, when the elapsed times are computed, then
      they are 0.0 rather than negative or absent
- [ ] Given telemetry is disabled, when an audio event is produced, then nothing
      is sent and the operator sees no diagnostic in the transcript

---

### EP-004: OpenTelemetry tracing

Reproduce the four span families, their attribute sets, their exporter
resolution and their redaction, so the three declared configuration keys change
behavior.

**Definition of Done:** `exporterConfig`, `spans`, `providerNames` and
`redaction` replay against the corpus with an empty ledger, and a turn run with
`enable_otel = true` against a local collector produces the reference span tree.

#### US-013: Install the tracer provider and resolve the exporter
**Description:** As an observability consumer, I want `enable_otel`,
`otel_endpoint` and `otel_redaction` to install a real exporter so that the
published schema stops advertising a capability the binary lacks.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-004

**Reference:** [vibe/core/tracing.py:30-113](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py:30-113) for the constants, the exporter resolution, its two branches, the missing-key warning at (67), the two-key gate at (77), the resource at (92) and the `atexit` shutdown at (113), [config/models.py:598-608](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py:598-608) for the mode enum and the exporter config pair, [vibe_schema.py:414-416](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py:414-416) for the three keys, and [_loop.py:1073-1080](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py:1073-1080) for the only place the gate reaches a backend. The 9 configuration cases are in [tests/core/config/test_config_otel.py](/home/arthur/dev/mistral-vibe/tests/core/config/test_config_otel.py)

**Acceptance Criteria:**
- [ ] Given the workspace, when the dependencies are added, then they are
      `opentelemetry` 0.32, `opentelemetry_sdk` 0.32, `opentelemetry-otlp` 0.32
      with `trace`, `http-proto` and `reqwest-client`, and
      `opentelemetry-semantic-conventions` 0.32 with `semconv_experimental`
- [ ] Given `enable_telemetry` false or `enable_otel` false, when setup runs,
      then no provider is installed and no exporter is constructed
- [ ] Given `otel_endpoint` set, when the exporter is built, then the endpoint
      is that value joined with the default traces export path and no
      authorization header is added
- [ ] Given `otel_endpoint` empty and a Mistral provider configured, when the
      exporter is built, then the endpoint is the provider's server URL joined
      with `/telemetry` and the traces export path, and the header carries the
      resolved key
- [ ] Given `otel_endpoint` empty and no resolvable key, when setup runs, then a
      warning naming the environment variable is logged, no provider is
      installed, and startup continues
- [ ] Given the process exits, when shutdown runs, then the provider is shut
      down and pending spans are flushed
- [ ] Given the baggage assumption, when this story completes, then a test
      proves the attach and detach equivalent exists and names the API used; if
      it does not, the conversation id is threaded explicitly and the divergence
      is recorded in `docs/parity.md`

#### US-014: The four span families
**Description:** As an observability consumer, I want the agent, tool, model
call and hook spans with the reference attributes so that a collector shows the
same tree as upstream.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-013

**Reference:** [vibe/core/tracing.py:154-193](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py:154-193) for `_safe_span`, its `INVALID_SPAN` fallback at (167) and the status decision at (135-150), [tracing.py:197-245](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py:197-245) for the agent span, its baggage attach at (215-218) and the tool span, [tracing.py:274-324](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py:274-324) for the model call span and the conversation-id resolution at (310-314), [tracing.py:327-354](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py:327-354) for the HTTP status and usage setters, [tracing.py:362-430](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py:362-430) for the response metadata search order, and [tracing.py:434-465](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py:434-465) for the hook span and `set_tool_result`

**Acceptance Criteria:**
- [ ] Given a turn, when the agent span opens, then it is named
      `invoke_agent mistral-vibe` and carries the operation name, provider name
      and agent name attributes, plus the request model and conversation id when
      known
- [ ] Given a session id, when the agent span opens, then the conversation id is
      set in baggage and every descendant span carries it
- [ ] Given a tool call, when its span opens, then it is named
      `execute_tool <name>` and carries the tool name, call id, arguments and
      the tool type `function`
- [ ] Given a model call, when its span opens, then it is named `chat <model>`
      and carries the operation name, the normalized provider name, the request
      model, the API style and the streaming flag, plus temperature, max tokens
      and the HTTP method and URL when present
- [ ] Given a hook runs, when its span opens, then it is named
      `hook <type> <name>` and carries the hook name and type, plus the tool
      name and call id when the hook is tool-scoped
- [ ] Given a response, when its metadata is recorded, then the response model,
      response id and finish reasons are set from the first source that supplies
      each, searching the body then its `message` and `response` members
- [ ] Given a failing model call, when the span closes, then the status carries
      the backend provider and status without the raw exception, and the
      exception is not recorded; given any other failing span, then the message
      is recorded
- [ ] Given tracing is not installed or a span cannot be created, when the body
      runs, then it still executes and no error reaches the caller

#### US-015: Redaction and provider normalization
**Description:** As an observability consumer, I want `otel_redaction` to strip
content before export so that enabling tracing does not exfiltrate prompts.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-014

**Reference:** [vibe/core/tracing.py:97-109](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py:97-109) for the mode-to-policy mapping and the exporter wrap, and [tracing.py:248-270](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py:248-270) for the normalization, its 7 aliases and the `unknown` fallback. The policy itself is a third-party dependency (`mistralai.extra.observability`), not reference source: reproduce the attribute sets it removes as the `redaction` corpus family records them, never its code. The value vocabularies are absent from `opentelemetry-semantic-conventions` 0.32.1, verified by grep, so they come from the corpus

**Acceptance Criteria:**
- [ ] Given `otel_redaction = none`, when a span is exported, then every
      attribute survives
- [ ] Given `otel_redaction = default`, when a span is exported, then exactly
      the attribute set the reference's default policy removes is removed
- [ ] Given `otel_redaction = strict`, when a span is exported, then every
      content-bearing attribute is removed, including tool call arguments and
      results
- [ ] Given the redacting exporter, when the inner exporter fails, then the
      failure is not propagated to the span-producing code
- [ ] Given a provider name, when it is normalized, then the 7 reference aliases
      map to their canonical values, a known value passes through, and an empty
      or unrecognized name resolves to `unknown` after lowercasing and hyphen
      folding
- [ ] Given the value vocabulary is absent from the semantic conventions crate,
      when the values are declared here, then they are held to the corpus and a
      change on either side fails the replay

#### US-016: Wire tracing into the turn, tools and hooks
**Description:** As an observability consumer, I want the spans opened from the
real execution paths so that tracing reflects what the agent actually did.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-014

**Reference:** [vibe/core/agent_loop/_loop.py:134-137](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop/_loop.py:134-137) for the tracing imports, (217) for the deferred `trace` import and (1073-1080) for the backend gate, [agent_loop_hooks.py:57](/home/arthur/dev/mistral-vibe/vibe/core/agent_loop_hooks.py:57) with (81, 112, 263, 348, 370) for the span threaded through every hook decision, and [tracing.py:327-354](/home/arthur/dev/mistral-vibe/vibe/core/tracing.py:327-354) for what a returning model call attaches

**Acceptance Criteria:**
- [ ] Given a turn runs with tracing installed, when it completes, then exactly
      one agent span exists and every model call and tool call is a descendant
      of it
- [ ] Given a model call returns, when its span closes, then the HTTP status and
      the input and output token counts are attached
- [ ] Given a subagent turn, when its spans are produced, then they carry the
      parent conversation id
- [ ] Given a cancelled turn, when the spans close, then the agent span carries
      an error status and no span is left unended
- [ ] Given tracing is disabled, when a turn runs, then no tracing code path
      allocates an exporter and the turn's measured duration is unchanged

---

### EP-005: File logging and the log reader

Give the product a log file at the reference path with the reference line
format, and answer `diagnostics/logs/read` from it.

**Definition of Done:** `logFormat`, `logEncoding`, `logParse`, `logPagination`
and `logConfig` replay against the corpus with an empty ledger, and a log line
written by one process is readable by another through the wire method.

#### US-017: Structured rotating file logging
**Description:** As a Vibe operator, I want a log file at the reference path so
that a failure before the app server attached still leaves a trace on disk.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-004

**Reference:** [vibe/observability/logging.py:13-71](/home/arthur/dev/mistral-vibe/vibe/observability/logging.py:13-71) for the formatter field order, the duplicate-handler guard at (36), the 10 MiB default at (44), the level resolution at (46-51), `backupCount=0` at (54) and the encode and decode pair at (62-71), [paths/_vibe_home.py:12](/home/arthur/dev/mistral-vibe/vibe/core/paths/_vibe_home.py:12) for `LOG_FILE`, and the three initialization sites [cli/entrypoint.py:278-280](/home/arthur/dev/mistral-vibe/vibe/cli/entrypoint.py:278-280), [app_server/stdio.py:36](/home/arthur/dev/mistral-vibe/vibe/app_server/stdio.py:36) and [acp/entrypoint.py:69](/home/arthur/dev/mistral-vibe/vibe/acp/entrypoint.py:69). The 23 cases are in [tests/observability/test_logging.py](/home/arthur/dev/mistral-vibe/tests/observability/test_logging.py)

**Acceptance Criteria:**
- [ ] Given the binary starts, when logging initializes, then it writes to
      `$VIBE_HOME/logs/vibe.log`, creating the directory when absent
- [ ] Given a record, when it is formatted, then the line is
      `<ISO-8601 UTC timestamp> <ppid> <pid> <LEVEL> <message>` with exception
      text appended on the same line when present
- [ ] Given a message containing backslashes or newlines, when it is written,
      then it is encoded so the line stays single, and decoding restores the
      original exactly
- [ ] Given `DEBUG_MODE=true`, then the level is `DEBUG`; given `LOG_LEVEL` set
      to a valid name, then that level applies; given an invalid name, then it
      falls back to `WARNING`
- [ ] Given `LOG_MAX_BYTES` unset, when the file grows, then it rotates at
      10 MiB keeping no backup; given it set, then that size applies
- [ ] Given initialization runs twice against the same resolved path, when the
      second call runs, then no second handler is attached
- [ ] Given the log directory cannot be created or written, when initialization
      runs, then the binary starts anyway and reports the degradation once

#### US-018: Answer `diagnostics/logs/read` from the file
**Description:** As a Vibe operator, I want the debug console to read the real
log file so that entries written by another process are visible.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-017

**Reference:** [vibe/core/log_reader.py:47-226](/home/arthur/dev/mistral-vibe/vibe/core/log_reader.py:47-226) for the whole reader, `get_logs` at (73) with the `has_more` and `cursor` decision at (83-87), `_read_lines_backward` at (89) with the skew correction at (97) and the 8 KiB chunk at (98), the shrink reset at (185) and `_parse_line` at (205), [_resources.py:843-848](/home/arthur/dev/mistral-vibe/vibe/app_server/_resources.py:843-848) with the construction at (161) and the dispatch at (376), [models.py:348-361](/home/arthur/dev/mistral-vibe/vibe/app_server/models.py:348-361) for the seven entry fields and the page shape, and [protocol.py:653-660](/home/arthur/dev/mistral-vibe/vibe/app_server/protocol.py:653-660) for the 1 to 500 limit. The 34 cases are in [tests/core/test_log_reader.py](/home/arthur/dev/mistral-vibe/tests/core/test_log_reader.py) and [tests/cli/test_log_command.py](/home/arthur/dev/mistral-vibe/tests/cli/test_log_command.py)

**Acceptance Criteria:**
- [ ] Given a log file, when `diagnostics/logs/read` is called, then entries are
      returned newest first, parsed into `id`, `timestamp`, `ppid`, `pid`,
      `level`, `message` and `rawLine`, with the real process identifiers rather
      than zeros
- [ ] Given `limit` and `offset`, when a page is returned, then `hasMore` and
      `cursor` follow the reference's semantics, and `cursor` is absent when the
      page is the last
- [ ] Given `limit` outside 1 to 500 or a negative `offset`, when the request is
      dispatched, then it is refused with invalid params
- [ ] Given a line the pattern does not match, when the page is built, then the
      line is skipped rather than failing the request
- [ ] Given no log file exists, when the method is called, then it answers an
      empty page rather than an error
- [ ] Given a line written by a different process after the page was requested,
      when the same offset is requested again, then the skew is corrected so the
      caller does not see a duplicate or a hole
- [ ] Given a file that shrank since the last read, when the next read runs,
      then the position resets rather than reading past the end

---

### EP-006: Residual surfaces and recorded divergences

Close the two remaining wire surfaces, record the two decisions that are
divergences rather than ports, and restate the score.

**Definition of Done:** `docs/parity.md` scores the telemetry row from the
oracle's printed counts, every new divergence row names a symbol
`ledger_tests` resolves, and `CHANGELOG.md` records the user-visible changes.

#### US-019: Serve `telemetry/send` on ACP and ship `telemetry/record`
**Description:** As an editor integrating over ACP, I want the telemetry
extension notification accepted so that editor-side events reach the same place
as terminal-side ones.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-010

**Reference:** [vibe/acp/agent.py:215-220](/home/arthur/dev/mistral-vibe/vibe/acp/agent.py:215-220) for `TelemetryNotification` and its `sessionId` alias, [agent.py:1002-1031](/home/arthur/dev/mistral-vibe/vibe/acp/agent.py:1002-1031) for the method guard, the validation error, the unknown-session drop, the two routed names and the warning branch, and [_resources.py:487-499](/home/arthur/dev/mistral-vibe/vibe/app_server/_resources.py:487-499) with [_service_resources.py:57-125](/home/arthur/dev/mistral-vibe/vibe/app_server/_service_resources.py:57-125) for what `telemetry/record` does with `correlate_last_request`

**Acceptance Criteria:**
- [ ] Given an extension notification named `telemetry/send`, when it arrives
      with a valid payload, then it is accepted; given any other method name,
      then it is ignored without error
- [ ] Given `vibe.at_mention_inserted`, when it arrives, then its properties are
      recorded unchanged
- [ ] Given `vibe.user_rating_feedback`, when it arrives, then it is recorded
      with the supplied rating defaulting to 0 and the active model alias, and
      it correlates with the last request
- [ ] Given any other event name, when it arrives, then it is ignored and a
      warning naming the event is logged
- [ ] Given a payload that fails validation, when it arrives, then an invalid
      request error is returned
- [ ] Given an unknown session id, when the notification arrives, then it is
      dropped without error
- [ ] Given `telemetry/record` on the app server, when telemetry is enabled,
      then the event is now sent under the reference envelope in addition to
      being readable on `diagnostics/logs/read`

#### US-020: Record the divergences and restate the score
**Description:** As a parity maintainer, I want every remaining gap recorded as
a decided divergence and the score restated from measurement so that the row
stops being defensible only by reading source.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-016, US-018, US-019

**Reference:** [vibe/observability/sentry.py:15-16](/home/arthur/dev/mistral-vibe/vibe/observability/sentry.py:15-16) for the two null DSNs and (177-209) for the `is_initialized` check that makes the dormancy observable, and [tests/core/experiments/test_telemetry_integration.py](/home/arthur/dev/mistral-vibe/tests/core/experiments/test_telemetry_integration.py) for the only place `experiments` is observably filled. The rest is local: `docs/parity.md` accepted divergences, `crates/vibe-core/src/parity/ledger_tests.rs` for the row-to-artifact guard, and the `published_skills` dormant-registry row as the precedent this story reuses

**Acceptance Criteria:**
- [ ] Given `_CLI_SENTRY_DSN` and `_ACP_SENTRY_DSN` are both `None` at the
      pinned commit, when the divergence is recorded, then the row states that
      the reference ships crash reporting dormant, cites the same reasoning as
      the dormant-registry row, and names a test that fails if the reference
      ever sets a DSN
- [ ] Given `experiments` cannot be filled until rank 16, when the divergence is
      recorded, then the row names the symbol that omits it and the rank that
      closes it
- [ ] Given `user_plan` depends on `account/read`, when the divergence is
      recorded, then the row states which three statuses it is produced from and
      references the existing `account/read` row
- [ ] Given every new row, when `ledger_tests` runs, then each names an artifact
      the repository holds
- [ ] Given the replay's printed counts, when `docs/parity.md` is updated, then
      the telemetry row quotes those lines rather than summarizing them, the
      rank-15 execution-order row moves to `DONE`, and the "OTel absent" and
      "no log reader" statements are removed
- [ ] Given a gap that is not closed and not recorded, when the review runs,
      then the score is not restated

---

## Functional Requirements

- FR-01: The system must send telemetry events under the reference envelope
  `{"event", "properties", "correlation_id"?}` to
  `{server}/v1/datalake/events`.
- FR-02: The system must resolve the credential from a Mistral provider only,
  preferring the active provider when it is Mistral and otherwise the first
  Mistral provider configured.
- FR-03: The system must NOT send any telemetry when `enable_telemetry` is
  false, and must treat an unreadable configuration as false.
- FR-04: The system must default `enable_telemetry` to true, matching the
  reference.
- FR-05: The system must NOT expose a `--telemetry` command-line flag.
- FR-06: The system must emit all 26 reference event names with the reference
  property key set for each.
- FR-07: The system must carry the 12 base metadata fields and the 3 additional
  request metadata fields, omitting any field with no value.
- FR-08: When `enable_telemetry` and `enable_otel` are both true, the system
  must install an OTLP HTTP span exporter resolved from `otel_endpoint` or from
  the Mistral provider.
- FR-09: The system must emit the four reference span families with the
  reference attribute keys, propagating the conversation id to descendants.
- FR-10: The system must apply the redaction policy selected by
  `otel_redaction` before export.
- FR-11: The system must NOT allow a tracing failure to alter the outcome of a
  turn, a tool call or a hook.
- FR-12: The system must write a rotating structured log to
  `$VIBE_HOME/logs/vibe.log`.
- FR-13: The system must answer `diagnostics/logs/read` from that file, newest
  first, with real process identifiers.
- FR-14: The system must accept the ACP `telemetry/send` extension notification
  for the two event names the reference routes and ignore the rest.
- FR-15: The system must NOT initialize a crash reporter, and must record that
  absence as a divergence citing the reference's null DSNs.

## Non-Functional Requirements

- **Performance:** an event send uses a 5.0 second total timeout with at most 5
  keep-alive connections and 10 total connections, matching the reference's
  `httpx.Timeout(5.0)` and `httpx.Limits(5, 10)`. A send never blocks the turn:
  measured turn duration with telemetry enabled and a black-holed endpoint
  differs from the disabled baseline by less than 10 ms at P95.
- **Performance:** with `enable_otel = false`, no OTel allocation occurs on the
  turn path, asserted by a test that runs a turn and observes zero exporter
  construction.
- **Reliability:** telemetry and tracing failures are swallowed at 100 percent:
  0 delivery failures, 0 export failures and 0 span-creation failures propagate
  to a caller. A pending send is awaited by flush with a bounded wait of 5.0
  seconds at shutdown.
- **Reliability:** the log file rotates at 10 MiB with 0 backups by default and
  never grows unbounded.
- **Security:** no credential value is ever recorded in the corpus, in a log
  line or in a span attribute; the capture asserts this by setting every named
  environment variable to a sentinel and recording the variable name instead.
  0 sockets may be opened during a capture run, enforced by a guard that fails
  the run.
- **Security:** with `otel_redaction = strict`, 0 content-bearing attributes
  survive export; with `default`, exactly the reference's removed set is
  removed.
- **Correctness:** the oracle prints at least 400 comparisons across at least 10
  families with 0 divergences outside its ledger and 0 stale ledger entries.
- **Portability:** the full suite passes with the reference checkout absent,
  with 0 test failures and only the recapture probes skipped.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | No Mistral provider | Only a third-party provider configured | Telemetry resolves inactive; nothing is sent; no third-party key leaves the process | none |
| 2 | Telemetry endpoint unreachable | Network down or DNS failure | Send fails silently; turn unaffected; flush still completes | none |
| 3 | Endpoint rejects the event | HTTP 4xx or 5xx | Failure swallowed; no retry storm; no diagnostic in the transcript | none |
| 4 | Configuration unreadable | Malformed TOML on the telemetry path | Telemetry resolves disabled rather than raising | Existing configuration diagnostic |
| 5 | OTel enabled, no key resolvable | `enable_otel = true`, `otel_endpoint` empty, key env unset | No provider installed; startup continues | Warning naming the environment variable |
| 6 | Span creation fails | Provider not installed, or tracer unavailable | Body still executes; an invalid span is used; nothing propagates | none |
| 7 | Collector unreachable | `otel_endpoint` points nowhere | Batch export fails in the background; turn unaffected | none |
| 8 | Log directory not writable | Read-only `$VIBE_HOME` or disk full | Binary starts; file logging degrades to absent | Degradation reported once per session |
| 9 | Log file grows past the ceiling | 10 MiB reached | File rotates with 0 backups; oldest content is discarded | none |
| 10 | Log page requested past the end | `offset` beyond the entry count | Empty entry list, `hasMore` false, `cursor` absent | none |
| 11 | Log file shrinks between reads | External rotation or truncation | Reader position resets to 0 rather than reading past the end | none |
| 12 | Malformed log line | Hand-edited file, or a foreign writer | Line skipped; the page still returns | none |
| 13 | ACP notification for an unknown event | Editor sends an unsupported name | Ignored; warning logged naming the event | none |
| 14 | ACP notification for an unknown session | Session already closed | Dropped without error | none |
| 15 | Telemetry toggled mid-session | `enable_telemetry` changed on disk and reloaded | The next event follows the new value | none |
| 16 | Rating submitted with no prior request | Feedback before any backend call | Event sent with no correlation id rather than a fabricated one | none |
| 17 | Reference checkout absent | Fresh clone with no oracle checkout | Replays run against the committed corpus; only recapture probes skip | Skip reason printed |
| 18 | Reference checkout off-pin | Local reference moved to another commit | Recapture refuses and names the restore command | Restore command printed |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Adopting the open-properties envelope lets client-authored properties reach the datalake, weakening the closed-vocabulary guarantee this port chose deliberately | High | High | Ship the envelope and the opt-out gate in the same epic (US-005 with US-007); keep the label validators as an invariant on events this port authors; apply them never to properties a client explicitly recorded; document the boundary in the divergence table |
| 2 | The `gen_ai.*` semconv constants move or are deprecated, as the `mcp.*` family in the same crate already was | Medium | Medium | Pin the crate to 0.32; let the `spans` corpus family fail on any key change; the keys are one `const` block, so a move is a localized edit |
| 3 | The baggage API is not equivalent to Python's attach/detach token | Medium | Medium | US-013 proves the surface with a test before US-014 writes a span; the fallback is threading the conversation id explicitly, recorded as a divergence |
| 4 | The redaction policy has no Rust counterpart and must be reimplemented from observation | High | Medium | The `redaction` corpus family records which attribute keys survive per mode, so the policy is measured rather than guessed; only the attribute sets matter, never the policy's prose |
| 5 | OTel export cannot be captured without a collector, so the oracle measures configuration and attributes rather than the wire | High | Low | Capture the exporter config and the span attributes through an in-memory exporter on both sides; record the unmeasured wire as a residual in the parity row rather than claiming coverage |
| 6 | 20 stories is at the documented ceiling and the epics are sequentially dependent | Medium | Medium | EP-001 is the only hard prerequisite; EP-004 and EP-005 depend on it but not on EP-002 or EP-003, so three tracks run in parallel after the oracle lands. If the effort overruns, EP-004 splits into its own PRD and the row is restated at the intermediate score |
| 7 | `experiments` stays empty until rank 16, so a field is on the wire that never carries a value | Low | Low | Recorded as a divergence row naming the omitting symbol and the rank that closes it, so it is not mistaken for a bug |
| 8 | Moving `diagnostics/logs/read` from the ring buffer to the file changes what existing callers see | Medium | Low | The wire shape is unchanged; the debug console tests are updated in US-018 and the ring buffer stays as the source for events the server itself records before the file exists |

## Non-Goals

- **Shipping any reference-authored prose.** `NOTICE` forbids it. Log message
  text, warning sentences and any reference message enter the corpus as a
  length plus a SHA-256, and every counterpart here is written originally.
- **Porting a crash reporter.** The reference ships Sentry dormant at the pinned
  commit, with both DSNs null. Adding `sentry-rust` would create a capability
  the reference does not have. Recorded as a divergence in US-020; revisit only
  if the reference ever sets a DSN.
- **GrowthBook and experiment sessions.** Rank 16, and blocked on credentials
  this repository does not hold. `experiments` is carried on the wire and stays
  empty.
- **A metrics or logs OTel signal.** The reference exports traces only. Metrics
  and log signals are out of scope even though the exporter crate supports them.
- **Reproducing the reference's `scrub_paths` regexes as a general facility.**
  They exist to sanitize crash reports, which this port does not send. If
  redaction needs path scrubbing, it is written for the redaction policy alone.
- **Making the telemetry envelope configurable.** One envelope, the reference's.
  A compatibility switch would double the surface the oracle has to measure.
- **Retro-shipping events for sessions recorded before this PRD.** Telemetry is
  live-only on both sides.

## Files NOT to Modify

- `crates/vibe-core/src/parity.rs`: holds `REFERENCE_COMMIT`; re-pinning is a
  separate change that regenerates every corpus at once.
- `scripts/parity/pin.py`: the second and last pin source, held equal to the
  first by `parity_tests.rs`.
- `NOTICE`: the licensing boundary this PRD operates inside.
- `crates/vibe-app-server/tests/app-server-surface/corpus.json`,
  `crates/vibe-core/tests/config-surface/corpus.json`,
  `crates/vibe-core/tests/setup-auth/corpus.json`,
  `crates/vibe-cli/tests/voice/corpus.json` and the other committed corpora are
  owned by their own oracles; a telemetry change that needs one of them changed
  is a signal the change is wrong.
- `crates/vibe-protocol/src/lib.rs` `SERVER_METHODS`: the method inventory is
  measured by the app-server oracle; this PRD adds no wire method, and
  `telemetry/record` and `diagnostics/logs/read` already exist there.

## Technical Considerations

Framed as questions for engineering input, not mandates.

- **Architecture:** where does the telemetry client live? Recommended:
  `vibe-core` owns the envelope, the metadata census and the transport trait, as
  it does today; `vibe-app-server` owns the session-scoped decision;
  `vibe-cli` and `vibe-acp` stay adapters that supply a launch context. Does the
  metadata census need a session handle that `vibe-core` does not currently
  hold?
- **Architecture:** should tracing live in `vibe-core` beside telemetry, or in
  its own module? Recommended: `crates/vibe-core/src/tracing.rs` mirroring the
  reference's single module, with the OTel dependency optional behind a feature
  so a build that does not want protobuf can omit it. Engineering to confirm
  whether an optional feature complicates `--all-features` in CI.
- **Data Model:** the metadata census is a flat map upstream. Recommended: a
  typed struct that serializes with `skip_serializing_if = "Option::is_none"`,
  which reproduces `exclude_none` exactly. Alternative: an ordered map built by
  hand. Trade-off: the typed struct fails the compile when a field is forgotten;
  the map is easier to extend for `telemetry/record` passthrough.
- **API Design:** `diagnostics/logs/read` currently answers a `u64` timestamp
  where the reference declares a `datetime`. Should the port emit an ISO-8601
  string to match, or is the census indifferent to types? Engineering to confirm
  against the app-server corpus before US-018 changes the field.
- **Dependencies:** `opentelemetry` 0.32.0, `opentelemetry_sdk` 0.32.1,
  `opentelemetry-otlp` 0.32.0 (`trace`, `http-proto`, `reqwest-client`),
  `opentelemetry-semantic-conventions` 0.32.1 (`semconv_experimental`).
  Alternatives considered: `tracing-opentelemetry` on top of the `tracing`
  crate, rejected because this port has no `tracing` instrumentation to bridge
  and the bridge would add a second span model. Does adding protobuf to the
  dependency tree materially affect binary size or cold-start time?
- **Migration:** `diagnostics/logs/read` moves from a process-local ring buffer
  to a file. Backward compatibility: the wire shape is unchanged, so no client
  migrates. Rollback: the ring buffer stays as the source for entries the server
  records before the log file exists, so the two can coexist during the change.
- **Migration:** removing `--telemetry` is a breaking CLI change. Since the flag
  is not in the reference and defaults to off, does anything in CI or the
  installers pass it? Confirm before US-007 lands.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| `docs/parity.md` telemetry and observability score | 60, declarative | 100, oracle-backed | PRD completion | The row quotes the replay's printed per-family counts |
| Reference event names emitted | 5 of 26 | 26 of 26 | EP-003 completion | `eventVocabulary` family conforming count |
| Event payload key sets matching the reference | 0 of 26 | 26 of 26 | EP-003 completion | `eventPayloads` family conforming count |
| Metadata fields produced | 6 of 15 | 15 of 15 | EP-002 completion | `baseMetadata` family conforming count |
| Span families reproduced | 0 of 4 | 4 of 4 | EP-004 completion | `spans` family conforming count |
| Declared configuration keys with no consumer | 3 (`enable_otel`, `otel_endpoint`, `otel_redaction`) | 0 | EP-004 completion | A test per key that changes the value and observes the change |
| Oracle comparisons per run | 0 | >= 400 across >= 10 families | PRD completion | The closing line the replay prints |
| Divergences outside the ledger | Not measurable | 0 | PRD completion | The replay fails on any |
| Ranks of the execution order still open below 16 | 1 (rank 15) | 0 | PRD completion | `docs/parity.md` execution-order table |

## Open Questions

- Does the `opentelemetry::baggage` surface provide an attach and detach
  equivalent to Python's context token? Engineering to answer inside US-013,
  before US-014 starts; the answer decides whether the conversation id
  propagates implicitly or is threaded explicitly and recorded as a divergence.
- Should `diagnostics/logs/read` emit an ISO-8601 timestamp string to match the
  reference's `datetime`, or does the app-server census compare names and
  aliases only? Parity maintainer to answer against the existing corpus before
  US-018; it decides whether US-018 is a shape change or a source change.
- Should the OTel dependency be optional behind a Cargo feature? Engineering to
  answer before US-013; `--all-features` is load-bearing in CI, so an optional
  feature must not create a configuration CI never builds.
- Does anything in `.github/workflows/`, `scripts/install.sh`,
  `scripts/install.ps1` or `action.yml` pass `--telemetry`? Answer before US-007
  removes the flag.
- The reference sends `vibe.new_session` with `nb_models`; does that count
  configured models or reachable ones? Capture decides in US-001; if the capture
  cannot disambiguate, it is recorded as a residual rather than guessed.
[/PRD]
