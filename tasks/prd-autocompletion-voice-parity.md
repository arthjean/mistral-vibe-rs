[PRD]
# PRD: Autocompletion Indexer and Voice Configuration Parity

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-10 | Arthur Jean | Initial PRD from the measured audit of `vibe/cli/autocompletion/` and the voice subtree against the Python reference at commit `b78b451`. Seven configuration keys are declared, published to every client and read by nothing: `file_watcher_for_autocomplete` and the six transcribe and TTS keys. The file index has no watcher and no incremental path, so the whole `file_indexer` package has no counterpart. The realtime transcription endpoint, model, sample rate, encoding and streaming delay are hard-coded in `realtime.rs` rather than resolved from the configuration the port already publishes. There is no speech transport at all, so `narrator_enabled` drives a state machine that can never speak. Neither area has an oracle, which is why `docs/parity.md` scores them 70 and 65 from module presence alone |

## Problem Statement

1. **Seven configuration keys are published and read by nothing.** `file_watcher_for_autocomplete` is declared at `crates/vibe-core/src/config/registry.rs:817` and published as `fileWatcherForAutocomplete` at `crates/vibe-core/src/config/view.rs:36`. The six audio keys `active_transcribe_model`, `transcribe_providers`, `transcribe_models`, `active_tts_model`, `tts_providers` and `tts_models` are declared at `registry.rs:636-661` and published through the `transcription` and `speech` objects at `view.rs:47-60`. A workspace-wide grep finds no production reader for any of the seven. `docs/parity.md:78` states the rule this breaks in the configuration row's own words: declaring a key is not implementing its feature. An operator reading `config/fields/read` is told this build watches the filesystem for autocompletion and lets them choose a transcription model and a TTS voice. It does neither.

2. **The file index has no watcher, so it goes stale the moment a file is created.** The reference runs `WatchController` ([watcher.py](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/file_indexer/watcher.py)) on a daemon thread with `step=200`, `yield_on_timeout=True`, a 0.5 second readiness wait and a 1 second join, gated on the getter the app wires at [app.py:810](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py) and reads at [app.py:1033](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py). This port has no watcher, no watch dependency in `Cargo.toml`, and rebuilds only when the workspace root itself changes (`crates/vibe-cli/src/tui/completion/path.rs:56-62`). A file written during a session is invisible to `@` completion until the process restarts, which is the single most common way an agent session creates a file the operator then wants to mention.

3. **There is no incremental update path, so the only repair is a full rescan.** `FileIndexStore.apply_changes` ([store.py:85-127](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/file_indexer/store.py)) applies additions, deletions and modifications entry by entry, removes a deleted directory by relative-path prefix, walks a newly created directory recursively, invalidates the cached order, and falls back to a full rebuild past `mass_change_threshold=200` changes. None of it exists here. Without it, a watcher would have nothing to feed.

4. **The realtime transcription transport ignores every value the configuration carries.** `VoiceConfig::from_api_base` hard-codes the model `voxtral-mini-transcribe-realtime-2602`, the sample rate 16000 and the target streaming delay 500 at `crates/vibe-cli/src/tui/voice/realtime.rs:19-21`, hard-codes the path at `:45`, and hard-codes the encoding `pcm_s16le` in the `session.update` frame at `:203`. The endpoint host comes from `arguments.api_base`, the LLM provider URL, at `crates/vibe-cli/src/tui/mod.rs:1320`, not from `transcribe_providers[].api_base`, whose default is `wss://api.mistral.ai`. The reference builds its client from `config.transcription.model` and `config.transcription.provider` ([lazy_audio_managers.py:212-244](/home/arthur/dev/mistral-vibe/vibe/cli/lazy_audio_managers.py)) and passes all four values to the wire ([mistral_transcribe_client.py:36-42](/home/arthur/dev/mistral-vibe/vibe/cli/transcribe/mistral_transcribe_client.py)). Changing `active_transcribe_model` in this port changes nothing observable.

5. **The provider's credential variable is never resolved.** The reference calls `resolve_api_key(provider.api_key_env_var)` ([api_keys.py:8-11](/home/arthur/dev/mistral-vibe/vibe/utils/api_keys.py)), reading the process environment first and the keyring second, under the name the provider entry declares. This port hands the realtime session one credential taken from the LLM path (`mod.rs:1320`). An operator whose audio provider declares a different `api_key_env_var` has no way to make it take effect.

6. **There is no speech transport, so the narrator can never speak.** `crates/vibe-cli/src/tui/narrator.rs` reproduces the reference `NarratorManager` state machine faithfully, and every `NarratorEffect::Speak` it produces is answered by `report_speech_unavailable` at `crates/vibe-cli/src/tui/mod.rs:902`, which posts one diagnostic per session. The reference posts the summary to the speech endpoint with the configured `voice` and `response_format` ([mistral_tts_client.py:44-56](/home/arthur/dev/mistral-vibe/vibe/cli/tts/mistral_tts_client.py)), base64-decodes the answer and plays it through the default output device ([audio_player.py](/home/arthur/dev/mistral-vibe/vibe/cli/audio_player/audio_player.py), 144 lines, with `decode_wav` in [utils.py](/home/arthur/dev/mistral-vibe/vibe/cli/audio_player/utils.py)). `narrator_enabled` is therefore a toggle whose only effect is a diagnostic.

7. **Two configuration validations the reference performs are missing, and one is stricter here.** `_default_alias_to_name` ([models.py:408-412](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py)) fills `alias` from `name` before validation for all four model families, so a `[[transcribe_models]]` entry without an alias is accepted upstream. This port marks `alias` required at `crates/vibe-core/src/config/registry.rs:329` and `:360`, so the same document is rejected here. `_unique_by("alias")` ([vibe_schema.py:192-202](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py)) rejects two entries sharing an alias; nothing here does.

8. **The audio model choice has no surface.** The reference config screen offers `active_transcribe_model` and `active_tts_model` as choice lists fed from `ConfigView.transcribe_models` and `tts_models` ([config_screen.py:110-111](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/screens/config/config_screen.py)). The voice overlay here offers exactly two booleans (`crates/vibe-cli/src/tui/pickers.rs:423-424`). The two alias lists are published and never rendered.

9. **The four audio telemetry events do not exist.** The reference emits `vibe.audio.transcription.start`, `.cancel_recording`, `.done` and `.error` with a recording id, a transcript length and two durations ([voice_manager.py:202-251](/home/arthur/dev/mistral-vibe/vibe/cli/voice_manager/voice_manager.py)). Nothing here emits an audio event, even though `telemetry/record` already accepts client events and keeps them locally.

10. **Nothing measures any of it.** Ten differential oracles live in `scripts/parity/`. None covers autocompletion or voice. `docs/parity.md:71` scores autocompletion 70 and `:72` scores voice 65, both by module presence, which `## Method` names as the most uncertain measurement class. The reference carries 89 test functions across `tests/autocompletion/` and `tests/core/autocompletion/` and 127 across the seven voice test directories, and `cargo test --workspace --all-features` passes green against code that shares none of that behavior.

**Why now:** rank 13 closed on 2026-08-10 and rank 14 is next in the execution order, with `projectLinks/*` already delivered by EP-029, so the two remaining halves are exactly this PRD. The cost of deferral is concentrated in defect 1 and it compounds the same way the browser-auth keys did: every configuration file written while these seven keys are published without a reader is a file whose author believes a value takes effect. Two of them, `transcribe_providers[].api_base` and `api_key_env_var`, address a network endpoint and a credential, so a self-hosted operator can point this build at their own audio gateway today and be silently sent to the public one.

## Overview

This initiative makes the autocompletion file index and the voice configuration behaviorally equivalent to the reference at every boundary an operator or a stored file can observe: which entries the index holds after a sequence of filesystem changes, which of them a query returns and in what order, which model and endpoint a transcription session addresses, which credential it presents, and what a narrator summary is turned into. Equivalence is defined mechanically: for a given fixture tree, ignore rules and change sequence, this port must hold the same entry set and answer the same ranked candidates; and for a given configuration document, it must resolve the same model, provider, endpoint, credential variable and wire frame.

Sequencing puts the instrument first, following this repository's own record: every part measured by an oracle scores 92 or above, and every part measured by module presence sits between 25 and 85. The first and third epics build two capture scripts and their corpora. The reference makes both affordable. `FileIndexStore` takes its ignore rules and its stats as constructor arguments and `apply_changes` takes a plain list of `(Change, Path)` pairs, so a capture drives the whole store over a scratch tree with no watcher thread running, and `IgnoreRules` answers `should_ignore` as a pure function of three strings. On the voice side, `make_transcribe_client` and `make_tts_client` take a provider view and a model view and nothing else, and both clients store every resolved value as an attribute before any connection, so a capture reads the resolved wire parameters without a socket, exactly as the setup oracle drives `BrowserSignInService` with a stub gateway.

The second epic ports the watcher and the incremental store, which is where the observable surface is densest: an entry set after a change sequence is a set comparison, not a judgement. The fourth resolves the six audio keys into the transport that already exists, which is a small diff with a large observable consequence. The fifth adds the speech client and the audio output the narrator needs, the only genuinely new subsystem in this PRD. The sixth publishes the audio model choice, emits the four telemetry events, records what cannot be ported and remeasures the scorecard.

Four boundaries are decided in advance rather than discovered during implementation. The reference returns its index in `scandir` order after a rebuild and in relative-path order only after an incremental update, so its order is not a contract anything can conform to, and the same normalization `grep` already uses applies on both sides. The reference's background rebuild executor, its per-root cancellation tasks and its `_target_root` bookkeeping are structure with no observable consequence for any current caller, so they are recorded rather than ported, following the per-layer async state machine precedent. `WALK_SKIP_DIR_NAMES` is exported by the reference and imported by nothing at the pinned commit, so it ships absent here on the dormant-registry precedent. Audio output depends on a device this port cannot assume, so an absent output backend degrades to a reported capability rather than a failure, following the null `ptyBackend` precedent.

No reference prose is involved anywhere in this PRD. Neither subtree carries authored user-facing text beyond error sentences already covered by the general licensing rule, so unlike the compaction envelope and the builtin skills, no digest-only prose family is needed in either corpus.

The reference is a read-only checkout pinned for this PRD at commit `b78b451c39eab9213393ad2f45908e8562a5c5e7` (v2.24.0), which every measurement in this document was taken from. This PRD does **not** re-pin: `vibe_core::parity::REFERENCE_COMMIT` and `EXPECTED_COMMIT` in `scripts/parity/pin.py` already name it. Its location is machine-dependent, `C:\dev\mistral-vibe` on Windows and `/home/arthur/dev/mistral-vibe` on Linux; reference links below use the Linux form and resolve against whichever checkout is local, through `VIBE_REFERENCE` or `--reference`, and Rust tests reach it through `vibe_core::parity::reference_root`.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Consume the declared keys | 7 of 7 keys read by a real code path, each proven by a test that changes the value and observes the change | 0 declared-only keys in either area |
| Keep the index fresh | A file created, modified or deleted during a session is reflected in the next `@` query in 100% of watched cases | 0 maintained |
| Reproduce the incremental store | 1 of 1 mass-change threshold, prefix deletion and recursive directory addition replayed with the reference entry set, 0 divergent | 0 maintained |
| Address the configured transcription | 4 of 4 wire values (model, sample rate, encoding, streaming delay) and the endpoint resolved from configuration, 0 hard-coded | 0 maintained |
| Resolve the audio credential | `api_key_env_var` read from the environment then the keyring, 0 sessions presenting a credential the configuration did not name | 0 maintained |
| Make the narrator speak | 1 of 1 speech request issued with the configured voice and format, playback attempted on every summary when a device exists | 0 maintained |
| Reproduce the configuration validations | `alias` defaulted from `name` in 4 of 4 model families, duplicate aliases rejected, replayed by the configuration corpus | 0 maintained |
| Make conformance mechanically enforced | Two corpora replay at least 240 scenarios across 9 families and fail on any divergence outside a named ledger | Ledger holds only the four recorded form divergences |
| Raise the measured scores | `docs/parity.md` autocompletion from 70 to 100 and voice from 65 to 100, measured by the two new oracles | Weighted total restated with the rows this work touches |

## Target Users

### Developer mentioning a file the session just created

- **Role:** Engineer running a long interactive session in which the agent writes new files.
- **Behaviors:** Types `@` and a fragment of a path, expects the popup to offer what exists on disk right now.
- **Pain points:** A file created after the first completion query is invisible for the rest of the process lifetime, because the index is built once per workspace root and never refreshed. The operator sees an empty popup, assumes they mistyped, and retypes the path by hand.
- **Current workaround:** Restart the binary, or type the full path with no completion.
- **Success looks like:** The file appears in the popup within a second of being written, without any command.

### Operator on a self-hosted or regional audio gateway

- **Role:** Engineer whose organization serves transcription and speech from its own endpoint, or from a region other than the default.
- **Behaviors:** Writes `[[transcribe_providers]]` and `[[tts_providers]]` entries with a custom `api_base` and `api_key_env_var`, and an `active_transcribe_model` naming their deployed model.
- **Pain points:** All six keys are accepted, published back through `config/read`, and ignored. Every recording is sent to the host the LLM `--api-base` names, with the default model name, under a credential the configuration never mentioned.
- **Current workaround:** None inside the product. Voice mode is unusable on a non-default deployment.
- **Success looks like:** The endpoint, model, sample rate, encoding, streaming delay and credential variable all come from the configuration, and changing any one of them changes the session.

### Operator who turned the narrator on

- **Role:** Engineer who enabled `narrator_enabled` to hear turn summaries while working in another window.
- **Behaviors:** Expects each completed turn to be summarized and spoken.
- **Pain points:** The toggle is accepted and the state machine runs, but the only output is one diagnostic saying playback is unavailable. `active_tts_model`, the voice and the response format are configurable and inert.
- **Current workaround:** None.
- **Success looks like:** Turn summaries are spoken with the configured voice, and a host with no output device says so once instead of failing.

### Parity reviewer certifying the scorecard

- **Role:** Reviewer deciding whether a row in `docs/parity.md` may be restated.
- **Behaviors:** Runs the named oracle, reads the conforming counts, checks the ledger for stale entries.
- **Pain points:** Two rows in the table carry numbers derived from counting lines in modules. There is no command to run and no count to read, so the numbers cannot be defended or refuted.
- **Success looks like:** Two reproducible commands print per-family counts, and any divergence outside a named ledger fails the suite.

## Research Findings

Web research was not conducted for this PRD and is recorded as deliberately skipped rather than omitted. The domain is behavioral conformance to a pinned local checkout: the specification is the reference source, not an external solution space, and no third-party guidance can adjudicate what `apply_changes` does with a deleted directory. The research phase was therefore a direct reading of the reference subtrees and of this port's counterparts, measured file by file.

### Measured volumes

| Tree | Lines | Counterpart | Lines |
|---|---|---|---|
| `vibe/cli/autocompletion/file_indexer/` | 630 | none | 0 |
| `vibe/cli/autocompletion/` (rest) | 856 | `crates/vibe-cli/src/tui/completion.rs` and `completion/` | 1 756 |
| `vibe/cli/transcribe/` and `vibe/cli/tts/` | 326 | `crates/vibe-cli/src/tui/voice/realtime.rs` | 257 |
| `vibe/cli/voice_manager/` | 365 | `crates/vibe-cli/src/tui/voice.rs` and `voice/` | 1 285 |
| `vibe/cli/audio_recorder/` | 382 | `crates/vibe-cli/src/tui/voice/recorder.rs` | 223 |
| `vibe/cli/audio_player/` | 229 | none | 0 |
| `vibe/cli/narrator_manager/` and `turn_summary/` | 586 | `crates/vibe-cli/src/tui/narrator.rs` | 374 |
| `vibe/cli/lazy_audio_managers.py` | 261 | none | 0 |

### Reference test surface

89 test functions across `tests/autocompletion/` and `tests/core/autocompletion/`: 12 on the indexer, 14 on the fuzzy matcher, 23 on path-completer fuzzy behavior, 6 on recursive completion, 15 on the completion controller, 15 on the slash controller and 4 on the watcher. 127 across the voice directories: 32 on the voice manager, 29 on the recorder, 15 on the player, 12 each on the transcribe and TTS configuration, 8 on the narrator, 6 on the lazy managers, 5 each on the two clients, and 3 on the audio configuration boundary.

### What is already conformant here

The 36 default ignore patterns are reproduced in the reference order at `crates/vibe-cli/src/tui/completion/path.rs:270`, with the full `.gitignore` grammar (comments, `!` negation, `/` anchoring, directory-only suffix, name-only detection, last-match-wins) and a hand-written glob including character classes. The ten `MatchRank` components are in the reference order with the reference semantics. Both caps, 32 000 processed entries and 100 target matches, are exact. The fuzzy matcher is an integer-exact port of all four strategies, their multipliers and their scoring terms, with the `/help` and `/config` boosts at `completion.rs:681`. The six audio keys carry the reference defaults byte for byte, verified against `crates/vibe-core/tests/config-surface/corpus.json:132-166`, and `ConfigView.transcription` and `speech` publish the reference field shapes at `crates/vibe-core/src/config/view.rs:198-239`. None of that needs to change.

## Assumptions & Constraints

### Assumptions (to validate)

- `notify` reports create, modify and delete events on Linux, macOS and Windows with enough fidelity to feed `apply_changes` in the same three categories `watchfiles` reports. This is the assumption US-202 validates first, because the whole watcher story rests on it.
- The reference's `mass_change_threshold=200` is a count of changes in one delivered batch, not a rate over time. Read from `store.py:89`, which compares `len(changes)` for a single call.
- A speech response is a WAV container in the default `response_format`, so one decoder covers the shipped configuration. Other formats are accepted by the schema and are out of scope for playback.
- `cpal` output on a host with no device fails at stream construction rather than at process start, so an absent device can be reported instead of crashing. Validated by US-211's unhappy-path criterion.

### Hard Constraints

- `NOTICE` forbids copying reference source. Both subtrees are reproduced from observed behavior, and no reference file is translated.
- The layering in `[workspace.metadata.vibe] dependency-layers` holds: the index and the audio transport are `vibe-cli` concerns, and the configuration keys they read are already resolved in `vibe-core`. Nothing in this PRD adds a `vibe-protocol` or `vibe-app-server` method.
- Both corpora replay unconditionally; only the probes that recapture from the pinned checkout may skip. A missing reference checkout must never fail `cargo test`.
- One new production dependency is introduced (`notify`) and it needs the same approval the `cpal` and `tokio-tungstenite` additions received in `tasks/decision-ep005-voice-boundary.md`.
- `crates/vibe-core/src/parity/ledger_tests.rs` reads the accepted-divergences table, so every row this PRD adds must name an artifact that resolves.

## Quality Gates

These commands must pass for every user story, run from the workspace root:

- `cargo fmt --all -- --check` - formatting
- `cargo check --workspace --all-targets --all-features` - compilation across every target
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` - lint set as errors
- `cargo test --workspace --all-features` - the whole suite, never filtered to the edited module

For stories that touch a corpus, additionally:

- `cargo test -p vibe-cli --all-features autocompletion_parity_tests -- --nocapture` - prints the per-family conforming counts
- `cargo test -p vibe-cli --all-features voice_parity_tests -- --nocapture` - prints the per-family conforming counts

## Reference Map

Every file an implementer opens before writing Rust, at the pinned commit `b78b451`. Paths use the Linux canonical spelling `/home/arthur/dev/mistral-vibe/` and resolve against whichever checkout is local, through `VIBE_REFERENCE` or `--reference`; Rust tests reach the same root through `vibe_core::parity::reference_root`. Each story below names its own anchor; this is the whole surface in one place. Reading these is required by `AGENTS.md`, and grepping them does not replace opening the declaration they point at.

The two subtrees this PRD reproduces are [/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion) and the seven audio directories under [/home/arthur/dev/mistral-vibe/vibe/cli/](/home/arthur/dev/mistral-vibe/vibe/cli). Open the directory before the individual file: the reference splits the index into a store, a rules compiler, a watcher and an orchestrator, and splits audio into a port, a client and a factory per direction, and a change read in isolation from those splits reads as arbitrary.

### The file index (5 files, 630 lines)

- [vibe/cli/autocompletion/file_indexer/indexer.py](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/file_indexer/indexer.py), 187 lines: `_RebuildTask` (20), `FileIndexer` (25), `__init__` with `mass_change_threshold=200` and `should_enable_watcher` (26), `stats` (52), `get_index` (55) for the root-change, rebuild and watcher-gating sequence, `refresh` (90), `shutdown` (102), `__del__` (109), `_start_background_rebuild` (116), `_rebuild_worker` (136), `_wait_for_rebuild` (166), `_handle_watch_changes` (172) for the three accepted change categories and the stale-root guard.
- [vibe/cli/autocompletion/file_indexer/store.py](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/file_indexer/store.py), 188 lines: `ASCII_CODEPOINT_LIMIT` (11), `FileIndexStats` (15), `IndexEntry` (21) with its six fields, `build_ascii_mask` (30), `FileIndexStore` (40), `clear` (58), `rebuild` (63), `snapshot` (74) and its lazy sort, `apply_changes` (85) with the threshold at (89), `_create_entry` (129), `_walk_directory` (144) with `follow_symlinks=False` at (157), `_remove_entry` (177) with the prefix deletion at (183).
- [vibe/cli/autocompletion/file_indexer/watcher.py](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/file_indexer/watcher.py), 75 lines: `WatchController` (10), `start` (20) with the same-root short circuit and the 0.5 second readiness wait (42), `is_watching` (45), `stop` (48) with the 1 second join (58), `_watch_loop` (60) with `step=200` and `yield_on_timeout=True` (62). The dependency is `watchfiles==1.2.0` ([pyproject.toml:123](/home/arthur/dev/mistral-vibe/pyproject.toml)), itself built on the same `notify` crate this PRD adds.
- [vibe/cli/autocompletion/file_indexer/ignore_rules.py](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/file_indexer/ignore_rules.py), 170 lines: `DEFAULT_IGNORE_PATTERNS` (9) for the 36 patterns in order, `CompiledPattern` (50), `IgnoreRules` (59), `_compile_default_patterns` (65), `get_walk_skip_dir_names` (84), `ensure_for_root` (91), `should_ignore` (97) for last-match-wins, `reset` (107), `_build_patterns` (111) for the `.gitignore` grammar, `_matches` (154), `WALK_SKIP_DIR_NAMES` (170) which nothing imports at this pin.

### What drives the index

- [vibe/cli/autocompletion/completers.py](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/completers.py), 373 lines: `DEFAULT_MAX_ENTRIES_TO_PROCESS` (14), `DEFAULT_TARGET_MATCHES` (15), `CommandCompleter` (33) with `_PROMOTED_BOOSTS` (47), `PathCompleter` (94), `MatchRank` (95) for the ten ranking components in order, the indexer construction (113), `_SearchContext` (117), `_extract_partial` (124), `_build_search_context` (136), `_build_query_ascii_mask` (169), `_is_immediate_child_of_prefix` (174), `_matches_prefix` (188), `_is_visible` (208), `_can_possibly_fuzzy_match` (211), `_format_label` (220), `_build_match_rank` (224), `_score_matches` (273) with the double sort (309), `_collect_matches` (313) and its root assumption (323).
- [vibe/cli/autocompletion/fuzzy.py](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/fuzzy.py), 189 lines: the three multipliers (5-7), `fuzzy_match` (17), `_find_best_match` (26), `_try_word_boundary_match` (60), `_try_consecutive_match` (94), `_try_subsequence_match` (124), `_calculate_score` (146). Already ported integer-exact; read it only to confirm the mask filter of US-205 cannot change a verdict.
- [vibe/cli/autocompletion/path_completion.py](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/path_completion.py), 177 lines: `MAX_SUGGESTIONS_COUNT` (12), `PathCompletionController` (15), `can_handle` (27), `on_text_changed` (58), `_update_suggestions` (107). [base.py](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/base.py): `CompletionResult` (7), `CompletionView` (13). [slash_command.py](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/slash_command.py): `SlashCommandController` (9).
- [vibe/cli/textual_ui/app.py](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py): the getter passed to the container (810), `_is_file_watcher_enabled` (1033). [widgets/chat_input/container.py](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/chat_input/container.py): the parameter (46), the field (56), the completer construction (65).
- [vibe/core/config/vibe_schema.py](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py): `file_watcher_for_autocomplete` (432). [vibe/core/autocompletion/path_prompt.py](/home/arthur/dev/mistral-vibe/vibe/core/autocompletion/path_prompt.py), 154 lines: the provider-neutral half already covered here.

### The voice subtree (7 directories, 2 157 lines)

- [vibe/cli/voice_manager/voice_manager.py](/home/arthur/dev/mistral-vibe/vibe/cli/voice_manager/voice_manager.py), 262 lines: `TRANSCRIPTION_DRAIN_TIMEOUT` (36), `VoiceManager` (39), `is_enabled` reading the config getter (57), `apply_enabled` (68), `start_recording` (78) with the sample rate taken from the model (88), `stop_recording` (103), `cancel_recording` (128), `_run_transcription` (160) for the four event cases, and the four telemetry emitters (202, 210, 221, 240). [voice_manager_port.py](/home/arthur/dev/mistral-vibe/vibe/cli/voice_manager/voice_manager_port.py): `TranscribeState` (9), `RecordingStartError` (15), `VoiceManagerListener` (19). [telemetry.py](/home/arthur/dev/mistral-vibe/vibe/cli/voice_manager/telemetry.py): `TranscriptionTrackingState` (8) for the four recorded quantities.
- [vibe/cli/transcribe/mistral_transcribe_client.py](/home/arthur/dev/mistral-vibe/vibe/cli/transcribe/mistral_transcribe_client.py), 95 lines: the five values resolved before any connection (36-42), `transcribe` (59) for the wire call and the four event mappings. [transcribe_client_port.py](/home/arthur/dev/mistral-vibe/vibe/cli/transcribe/transcribe_client_port.py): the four event types (9, 14, 19, 24). [factory.py](/home/arthur/dev/mistral-vibe/vibe/cli/transcribe/factory.py): `make_transcribe_client` (11), the whole injection seam the oracle drives.
- [vibe/cli/tts/mistral_tts_client.py](/home/arthur/dev/mistral-vibe/vibe/cli/tts/mistral_tts_client.py), 68 lines: the four values resolved in `__init__` (23-27), `speak` (44) for the request shape and the base64 decode (55). [tts_client_port.py](/home/arthur/dev/mistral-vibe/vibe/cli/tts/tts_client_port.py): `TTSResult` (8). [factory.py](/home/arthur/dev/mistral-vibe/vibe/cli/tts/factory.py): `make_tts_client` (11).
- [vibe/cli/audio_player/audio_player.py](/home/arthur/dev/mistral-vibe/vibe/cli/audio_player/audio_player.py), 144 lines: `DEFAULT_BLOCKSIZE` (26), `DTYPE` (27), `DEFAULT_SAMPLE_WIDTH` (28), `check_audio_available` (31), `play` (61), `stop` (102), `_audio_callback` (108), `_on_stream_finished` (127), `_guard_audio_output` (138). [audio_player_port.py](/home/arthur/dev/mistral-vibe/vibe/cli/audio_player/audio_player_port.py): `AudioFormat` (8) and the four error types (12, 16, 20, 24). [utils.py](/home/arthur/dev/mistral-vibe/vibe/cli/audio_player/utils.py): `decode_wav` (7).
- [vibe/cli/audio_recorder/audio_recorder.py](/home/arthur/dev/mistral-vibe/vibe/cli/audio_recorder/audio_recorder.py), 287 lines: the seven constants (31-38), `start` (75), `stop` (139), `cancel` (169), `audio_stream` (183), `_guard_audio_input` (236). [audio_recorder_port.py](/home/arthur/dev/mistral-vibe/vibe/cli/audio_recorder/audio_recorder_port.py): `AudioRecording` (11) and the four error types (18, 22, 26, 30). Already ported; read it for the error taxonomy US-212 mirrors on the output side.
- [vibe/cli/narrator_manager/narrator_manager.py](/home/arthur/dev/mistral-vibe/vibe/cli/narrator_manager/narrator_manager.py), 272 lines: `on_turn_end` (107) with the three-way gate including `tts_client is not None` (112), `cancel` (119), `sync` (131), `_make_turn_summary` (138), `_make_tts_client` (146) for the resolution and its `KeyError` fallback, `_speak_summary` (200), `_on_playback_finished` (221), the four read-aloud telemetry hooks (227, 238, 250). [turn_summary/tracker.py](/home/arthur/dev/mistral-vibe/vibe/cli/turn_summary/tracker.py): `TurnSummaryTracker` (15). [turn_summary/port.py](/home/arthur/dev/mistral-vibe/vibe/cli/turn_summary/port.py): `TurnSummaryData` (10).
- [vibe/cli/lazy_audio_managers.py](/home/arthur/dev/mistral-vibe/vibe/cli/lazy_audio_managers.py), 261 lines: `check_audio_available` (25), `LazyVoiceManager` (31) with its materialize-on-enabled constructor (41), `LazyNarratorManager` (104) with the same (114) and `sync` (157), `create_default_voice_manager` (190), `_create_real_voice_manager` (212) for the resolution this PRD reproduces, `_create_real_narrator_manager` (247).
- [vibe/utils/audio.py](/home/arthur/dev/mistral-vibe/vibe/utils/audio.py): `RecordingMode` (4). [vibe/utils/api_keys.py](/home/arthur/dev/mistral-vibe/vibe/utils/api_keys.py): `resolve_api_key` (8), three lines that decide which credential an audio session presents.

### The audio configuration

- [vibe/core/config/models.py](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py): `TranscribeClient` (128), `TranscribeProviderConfig` (132), `_default_alias_to_name` (408), `TranscribeModelConfig` (565) with `sample_rate=16000` (569), `encoding` (570), `language` (571), `target_streaming_delay_ms` (572), `TTSClient` (577), `TTSProviderConfig` (581), `TTSModelConfig` (588) with `voice` (592) and `response_format` (593). The three `_default_alias_to_name` bindings are at (428) for `ModelConfig`, (574) for `TranscribeModelConfig` and (595) for `TTSModelConfig`; `compaction_model` inherits the first.
- [vibe/core/config/vibe_schema.py](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py): `_unique_by` (192), the audio defaults (146-174), the six key declarations (266-287), `voice_mode_enabled` (436), and the four resolvers `get_active_transcribe_model` (527), `get_transcribe_provider_for_model` (541), `get_active_tts_model` (552), `get_tts_provider_for_model` (561).

### What publishes both on the wire

- [vibe/app_server/config.py](/home/arthur/dev/mistral-vibe/vibe/app_server/config.py): `TranscribeModelConfigView` (20), `AudioProviderView` (28), `TranscriptionConfigView` (33), `TTSModelConfigView` (39), `SpeechConfigView` (44), and in `ConfigView` the three fields this PRD touches, `file_watcher_for_autocomplete` (64), `transcribe_models` (73) and `tts_models` (74). [_projection.py](/home/arthur/dev/mistral-vibe/vibe/app_server/_projection.py): the two active-model resolutions (100-101), the watcher field (115), the two alias lists (124-125).
- [vibe/cli/textual_ui/screens/config/config_screen.py](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/screens/config/config_screen.py): the two audio choice lists (110-111), which is the surface US-214 reproduces.

### The behavioral inventory

The reference's own tests are the checklist. For the index, all under [/home/arthur/dev/mistral-vibe/tests/autocompletion/](/home/arthur/dev/mistral-vibe/tests/autocompletion): 12 functions in [test_file_indexer.py](/home/arthur/dev/mistral-vibe/tests/autocompletion/test_file_indexer.py), 14 in [test_fuzzy.py](/home/arthur/dev/mistral-vibe/tests/autocompletion/test_fuzzy.py), 23 in [test_path_completer_fuzzy.py](/home/arthur/dev/mistral-vibe/tests/autocompletion/test_path_completer_fuzzy.py), 6 in [test_path_completer_recursive.py](/home/arthur/dev/mistral-vibe/tests/autocompletion/test_path_completer_recursive.py), 15 in [test_path_completion_controller.py](/home/arthur/dev/mistral-vibe/tests/autocompletion/test_path_completion_controller.py) and 15 in [test_slash_command_controller.py](/home/arthur/dev/mistral-vibe/tests/autocompletion/test_slash_command_controller.py), plus 4 in [tests/core/autocompletion/test_watcher.py](/home/arthur/dev/mistral-vibe/tests/core/autocompletion/test_watcher.py). For audio: 32 in [tests/voice_manager/test_voice_manager.py](/home/arthur/dev/mistral-vibe/tests/voice_manager/test_voice_manager.py), 29 in [tests/audio_recorder/test_audio_recorder.py](/home/arthur/dev/mistral-vibe/tests/audio_recorder/test_audio_recorder.py), 15 in [tests/audio_player/test_audio_player.py](/home/arthur/dev/mistral-vibe/tests/audio_player/test_audio_player.py), 12 each in [tests/core/config/test_transcribe_config.py](/home/arthur/dev/mistral-vibe/tests/core/config/test_transcribe_config.py) and [test_tts_config.py](/home/arthur/dev/mistral-vibe/tests/core/config/test_tts_config.py), 8 in [tests/narrator_manager/test_narrator_manager.py](/home/arthur/dev/mistral-vibe/tests/narrator_manager/test_narrator_manager.py), 6 in [tests/cli/test_lazy_audio_managers.py](/home/arthur/dev/mistral-vibe/tests/cli/test_lazy_audio_managers.py), 5 each in [tests/cli/transcribe/test_transcribe_client.py](/home/arthur/dev/mistral-vibe/tests/cli/transcribe/test_transcribe_client.py) and [tests/cli/tts/test_tts_client.py](/home/arthur/dev/mistral-vibe/tests/cli/tts/test_tts_client.py), and 3 in [tests/cli/test_audio_config_boundary.py](/home/arthur/dev/mistral-vibe/tests/cli/test_audio_config_boundary.py). The stubs under [tests/stubs/](/home/arthur/dev/mistral-vibe/tests/stubs) name the seams the oracles drive: `fake_audio_player.py`, `fake_audio_recorder.py`, `fake_transcribe_client.py`, `fake_tts_client.py` and `fake_voice_manager.py`. Read all of these for the cases, never for the code.

## Epics & User Stories

### EP-058: The Autocompletion Oracle and Its Corpus

Build the instrument before the code it measures. A capture script drives the reference index over scratch trees and change sequences, and a Rust module replays it against this build.

**Definition of Done:** `scripts/parity/autocompletion.py` captures five families with no network, the committed corpus carries fixture-supplied names and no reference prose, and `autocompletion_parity_tests` replays it unconditionally and prints per-family counts.

#### US-200: Capture the reference index, its rules and its ranking
**Description:** As a parity reviewer, I want a capture script that records what the reference file index answers for a fixture tree, an ignore ruleset, a change sequence and a query, so that this port's answers can be compared instead of assumed.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None

**Reference:** [vibe/cli/autocompletion/file_indexer/store.py:40-188](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/file_indexer/store.py) for the store the capture drives directly, [ignore_rules.py:59-167](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/file_indexer/ignore_rules.py) for `should_ignore` as a pure function, [completers.py:94-344](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/completers.py) for the ranking family, and [tests/autocompletion/](/home/arthur/dev/mistral-vibe/tests/autocompletion) for the 89 cases the families must cover. The local pattern to follow is `scripts/parity/config_surface.py` for the interpreter re-exec and the `git archive` read, and `scripts/parity/setup_auth.py` for the socket guard

**Acceptance Criteria:**
- [ ] Given the pinned checkout, when `scripts/parity/autocompletion.py --corpus` runs, then it writes `.parity/autocompletion-corpus.json` and `crates/vibe-cli/tests/autocompletion/corpus.json` with five families: `ignoreRules`, `walk`, `changes`, `ranking` and `constants`.
- [ ] Given a fixture tree with a `.gitignore` exercising comments, negation, anchoring, directory-only patterns and character classes, when the capture runs, then `ignoreRules` records the reference `should_ignore` verdict for at least 40 `(rel, name, is_dir)` triples.
- [ ] Given a change sequence, when the capture runs, then `changes` records the entry set `FileIndexStore` holds after each `apply_changes` call, plus the `FileIndexStats` counters, for at least 12 sequences including one that crosses `mass_change_threshold`.
- [ ] Given a query against a walked tree, when the capture runs, then `ranking` records the ordered candidate list and the `MatchRank` tuple per candidate for at least 30 queries.
- [ ] Given the reference's non-deterministic post-rebuild order, when the capture records a `walk` or `ranking` family, then it sorts entries by relative path before recording, and the corpus documents that normalization in its `note` field.
- [ ] Given the script runs, when any code path attempts a socket connection or a DNS resolution, then a socket guard fails the run before a corpus is written.
- [ ] Given the checkout sits at another commit, when the script runs, then it reads the pinned commit through `git archive` and never moves `HEAD`, creates a branch or adds a worktree.
- [ ] Given the reference checkout is absent, when the script runs, then it exits with a message naming `VIBE_REFERENCE` and writes no partial corpus.

#### US-201: Replay the corpus against this build
**Description:** As a parity reviewer, I want a Rust module that replays the committed corpus against this build and reports per-family conformance, so that a divergence fails CI rather than surviving to a scorecard claim.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-200

**Reference:** no Python counterpart; the shape is a local one. Follow `crates/vibe-core/src/skills/skills_parity_tests.rs` for the family layout, the `DIVERGENCES` ledger and its stale check, and `crates/vibe-core/src/config/surface_parity_tests.rs` for the unconditional replay with a skippable recapture probe

**Acceptance Criteria:**
- [ ] Given the committed corpus, when `cargo test -p vibe-cli --all-features autocompletion_parity_tests -- --nocapture` runs, then it prints a conforming count per family and the total.
- [ ] Given a family entry this build answers differently, when the replay runs, then it fails naming the family, the scenario id and the divergent field.
- [ ] Given a named ledger entry whose divergence has been fixed, when the replay runs, then it fails as a stale entry rather than passing silently.
- [ ] Given the reference checkout is absent or off-pin, when the suite runs, then the replay still runs and only the recapture probe is skipped.
- [ ] Given the corpus declares a family this build has no reader for, when the replay runs, then it fails naming the family rather than skipping it.

---

### EP-059: The File Index Watcher and the Incremental Store

Give `file_watcher_for_autocomplete` a reader, and give the watcher an incremental store to feed.

**Definition of Done:** The key gates a real watcher, a file created during a session appears in the next query, `apply_changes` reproduces the reference entry set for every captured sequence, and `path_candidates` no longer rebuilds per call.

#### US-202: Watch the workspace under the configured key
**Description:** As a developer mentioning a file the session just created, I want the index to follow filesystem changes when the key is on, so that the completion popup reflects what is on disk.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-201

**Reference:** [vibe/cli/autocompletion/file_indexer/watcher.py:10-75](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/file_indexer/watcher.py) for the whole controller including `step=200`, the 0.5 second readiness wait and the 1 second join, [indexer.py:55-88](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/file_indexer/indexer.py) for the root-change, start and stop sequence in `get_index`, [indexer.py:172-187](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/file_indexer/indexer.py) for the three accepted change categories and the stale-root guard, and [app.py:810,1033](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/app.py) with [container.py:46,65](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/widgets/chat_input/container.py) for the gating getter

**Acceptance Criteria:**
- [ ] Given `file_watcher_for_autocomplete` is true and a workspace root, when a completion query runs, then a watcher is started for that root and reports create, modify and delete events into the index.
- [ ] Given the key is false, when a completion query runs, then no watcher thread exists and the index behaves exactly as it does today.
- [ ] Given the key changes from true to false during a session, when the next query runs, then the watcher is stopped before the query is answered.
- [ ] Given the workspace root changes, when the next query runs, then the watcher for the previous root is stopped before the watcher for the new root is started.
- [ ] Given the watcher is already running for the same root, when another query runs, then it is not restarted.
- [ ] Given the platform backend cannot be created, when the watcher starts, then the failure is reported once as a diagnostic and completion continues against the last built index rather than failing the query.
- [ ] Given the process exits with a watcher running, when the runtime drops, then the watcher thread is joined within 1 second and the process does not hang.

#### US-203: Apply changes incrementally
**Description:** As a developer, I want a filesystem change to update the index in place, so that a large workspace does not pay a full rescan for every saved file.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-202

**Reference:** [vibe/cli/autocompletion/file_indexer/store.py:85-127](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/file_indexer/store.py) for `apply_changes`, the threshold comparison at (89), the relative-path guard at (96), the deletion branch at (103), the existence check at (108) and the recursive directory addition at (116), plus `_remove_entry` (177) with its prefix sweep at (183) and `_create_entry` (129) for the ignore-rule rejection

**Acceptance Criteria:**
- [ ] Given a batch of at most 200 changes, when they are applied, then only the affected entries change and the entry set equals the reference's for every captured sequence.
- [ ] Given a batch of more than 200 changes, when it is applied, then a full rebuild replaces the incremental path, matching `mass_change_threshold`.
- [ ] Given a deleted directory, when the deletion is applied, then the directory entry and every entry whose relative path starts with its path plus a separator are removed.
- [ ] Given a newly created directory, when the addition is applied, then the directory entry and every non-ignored descendant are added.
- [ ] Given a change whose path is outside the index root, when it is applied, then it is skipped without error and the entry set is unchanged.
- [ ] Given a change whose path no longer exists and is not a deletion, when it is applied, then it is skipped without error.
- [ ] Given a change whose entry the ignore rules reject, when it is applied, then no entry is created.

#### US-204: Track index statistics and stop rebuilding per query
**Description:** As a parity reviewer, I want the index to expose the reference's two counters and to be reused across queries on every path, so that rebuild behavior is observable and a keystroke does not cost a full rescan.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-203

**Reference:** [vibe/cli/autocompletion/file_indexer/store.py:15-17](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/file_indexer/store.py) for `FileIndexStats` and its two counters, incremented at (72) and (127), [indexer.py:52](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/file_indexer/indexer.py) for how they are exposed, `refresh` (90) and `shutdown` (102) for the reset semantics, and `_wait_for_rebuild` (166) for the caller that blocks on the first build

**Acceptance Criteria:**
- [ ] Given a sequence of rebuilds and incremental updates, when the counters are read, then they equal the reference `FileIndexStats.rebuilds` and `incremental_updates` for every captured sequence.
- [ ] Given two consecutive queries against the same unchanged root, when they are answered, then exactly one walk of the tree occurred, on every code path that answers a mention query.
- [ ] Given the index is reset, when the next query runs, then a rebuild occurs and the compiled ignore rules are recompiled for the root.
- [ ] Given a query arrives while no index has been built, when it is answered, then the caller waits for the first build rather than receiving an empty candidate list.

#### US-205: Prefilter fuzzy candidates by ASCII mask
**Description:** As a developer working in a large repository, I want the index to skip entries that cannot possibly match before scoring them, so that completion latency stays bounded as the tree grows.

**Priority:** P2
**Size:** S (2 pts)
**Dependencies:** Blocked by US-203

**Reference:** [vibe/cli/autocompletion/file_indexer/store.py:11,30-37](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/file_indexer/store.py) for `ASCII_CODEPOINT_LIMIT` and `build_ascii_mask`, the mask stored per entry at (141), and [completers.py:169-172,211-218,297](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/completers.py) for `_build_query_ascii_mask`, `_can_possibly_fuzzy_match` and the call site that precedes the matcher

**Acceptance Criteria:**
- [ ] Given an entry whose lowercase relative path is stored, when it is indexed, then a 128-bit ASCII mask is computed from it, matching `build_ascii_mask`.
- [ ] Given a query containing only ASCII characters, when candidates are scored, then an entry whose mask lacks a required bit is skipped before the fuzzy matcher runs.
- [ ] Given a query containing a codepoint at or above 128, when candidates are scored, then no mask filtering is applied and every entry reaches the matcher.
- [ ] Given any query and any tree in the corpus, when the mask filter is enabled and disabled, then the returned candidate list is identical, proving the filter never removes a match.

---

### EP-060: The Voice Oracle and Its Corpus

Measure what a configuration document resolves into before changing what reads it.

**Definition of Done:** `scripts/parity/voice.py` captures four families with no network and no audio device, and `voice_parity_tests` replays them unconditionally.

#### US-206: Capture the reference audio resolution and wire parameters
**Description:** As a parity reviewer, I want a capture script that records what model, provider, endpoint, credential variable and wire values the reference resolves from a configuration document, so that this port's resolution can be compared field by field.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None

**Reference:** [vibe/cli/lazy_audio_managers.py:212-260](/home/arthur/dev/mistral-vibe/vibe/cli/lazy_audio_managers.py) for the two resolutions the capture reproduces, [vibe/cli/transcribe/factory.py:11](/home/arthur/dev/mistral-vibe/vibe/cli/transcribe/factory.py) and [vibe/cli/tts/factory.py:11](/home/arthur/dev/mistral-vibe/vibe/cli/tts/factory.py) for the injection seams, [mistral_transcribe_client.py:36-42](/home/arthur/dev/mistral-vibe/vibe/cli/transcribe/mistral_transcribe_client.py) and [mistral_tts_client.py:23-27](/home/arthur/dev/mistral-vibe/vibe/cli/tts/mistral_tts_client.py) for the values resolved before any connection, [vibe_schema.py:527-568](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py) for the four resolvers and their `ValueError` messages, and [app_server/config.py:20-47](/home/arthur/dev/mistral-vibe/vibe/app_server/config.py) for the view shapes

**Acceptance Criteria:**
- [ ] Given the pinned checkout, when `scripts/parity/voice.py --corpus` runs, then it writes `.parity/voice-corpus.json` and `crates/vibe-cli/tests/voice/corpus.json` with four families: `transcriptionResolution`, `speechResolution`, `wireFrames` and `constants`.
- [ ] Given at least 20 configuration documents including missing entries, a mistyped active alias, a duplicate alias and an entry with no alias, when the capture runs, then each family records the resolved values or the reference exception type.
- [ ] Given a resolved transcription configuration, when the capture runs, then `wireFrames` records the reference `AudioFormat`, the model name, the target streaming delay and the resolved server URL, read as attributes without any connection.
- [ ] Given a resolved speech configuration, when the capture runs, then `wireFrames` records the model, voice and response format the reference would post.
- [ ] Given the script runs, when any code path attempts a socket connection, a DNS resolution or an audio device query, then a guard fails the run before a corpus is written.
- [ ] Given a captured value is a credential, when the corpus is written, then the value is replaced by the variable name and never the secret.

#### US-207: Replay the voice corpus against this build
**Description:** As a parity reviewer, I want a Rust module that replays the voice corpus and reports per-family conformance, so that a resolution divergence fails CI.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-206

**Reference:** no Python counterpart; follow the same local shape as US-201, plus `crates/vibe-core/src/setup_auth_parity_tests.rs` for the convention of comparing an error case by cause rather than by message text, which `NOTICE` requires

**Acceptance Criteria:**
- [ ] Given the committed corpus, when `cargo test -p vibe-cli --all-features voice_parity_tests -- --nocapture` runs, then it prints a conforming count per family and the total.
- [ ] Given a scenario this build resolves differently, when the replay runs, then it fails naming the scenario and the divergent field.
- [ ] Given a scenario the reference answers with an exception, when the replay runs, then this build's error case is compared by cause rather than by message text.
- [ ] Given the reference checkout is absent, when the suite runs, then the replay still runs and only the recapture probe is skipped.

---

### EP-061: The Transcription Configuration

Make the six audio keys take effect on the transport that already exists.

**Definition of Done:** No transcription wire value is hard-coded, the endpoint and credential come from the configured provider, and the two configuration validations match the reference.

#### US-208: Resolve the transcription session from configuration
**Description:** As an operator on a self-hosted audio gateway, I want the transcription session to address the model and endpoint my configuration names, so that voice mode works on my deployment.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-207

**Reference:** [vibe/cli/transcribe/mistral_transcribe_client.py:36-42,59-73](/home/arthur/dev/mistral-vibe/vibe/cli/transcribe/mistral_transcribe_client.py) for the five resolved values and how they reach the wire, [lazy_audio_managers.py:219-231](/home/arthur/dev/mistral-vibe/vibe/cli/lazy_audio_managers.py) for reading `config.transcription.model` and `.provider` and swallowing a `KeyError` into a null client, [vibe_schema.py:527-550](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py) for the two resolvers, and [models.py:132-136,565-575](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py) for the provider and model field defaults

**Acceptance Criteria:**
- [ ] Given a configuration whose active transcribe model names a model, when a session starts, then the endpoint query carries that model name rather than a constant.
- [ ] Given the model declares a sample rate, an encoding and a target streaming delay, when the `session.update` frame is sent, then all three come from the model entry.
- [ ] Given the active model's provider declares an `api_base`, when the endpoint is built, then its host and scheme come from that entry rather than from `--api-base`.
- [ ] Given `active_transcribe_model` names an alias no entry declares, when a session starts, then the first declared entry is used, matching the reference view's fallback, and no panic occurs.
- [ ] Given `transcribe_models` is empty, when a session starts, then the start resolves to an error naming the missing configuration and voice mode reports it once rather than connecting to a default.
- [ ] Given the configured `api_base` is not a valid URL, when a session starts, then the failure is reported to the operator and no connection is attempted.

#### US-209: Present the credential the provider names
**Description:** As an operator whose audio gateway uses a different key variable, I want the session to read the variable my provider entry declares, so that the right credential is presented.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-208

**Reference:** [vibe/utils/api_keys.py:8-11](/home/arthur/dev/mistral-vibe/vibe/utils/api_keys.py) for `resolve_api_key`, its empty-name guard and its environment-then-keyring order, [mistral_transcribe_client.py:36](/home/arthur/dev/mistral-vibe/vibe/cli/transcribe/mistral_transcribe_client.py) for the call site, and [models.py:135](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py) for `api_key_env_var` and its empty default

**Acceptance Criteria:**
- [ ] Given the active provider declares an `api_key_env_var`, when a session starts, then the credential is read from that environment variable first and from the credential store second, matching `resolve_api_key`.
- [ ] Given `api_key_env_var` is empty, when a session starts, then no credential lookup is attempted under an empty name and the session falls back to the runtime credential.
- [ ] Given the named variable is unset and the store holds no entry for it, when a session starts, then the start fails with a message naming the variable and no request is issued.
- [ ] Given a credential is resolved, when any diagnostic or log line is written, then the secret value never appears in it.

#### US-210: Validate audio model entries the way the reference does
**Description:** As an operator writing a configuration file, I want an entry without an alias to be accepted and a duplicate alias to be rejected, so that a document the reference accepts is accepted here.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-207

**Reference:** [vibe/core/config/models.py:408-412](/home/arthur/dev/mistral-vibe/vibe/core/config/models.py) for `_default_alias_to_name` and its three bindings at (428), (574) and (595), [vibe_schema.py:192-202](/home/arthur/dev/mistral-vibe/vibe/core/config/vibe_schema.py) for `_unique_by` and its two audio applications at (275) and (286), and [lazy_audio_managers.py:157-162](/home/arthur/dev/mistral-vibe/vibe/cli/lazy_audio_managers.py) with [narrator_manager.py:131-135](/home/arthur/dev/mistral-vibe/vibe/cli/narrator_manager/narrator_manager.py) for the re-read on toggle

**Acceptance Criteria:**
- [ ] Given a `[[transcribe_models]]` or `[[tts_models]]` entry with a `name` and no `alias`, when the configuration loads, then the alias defaults to the name and the entry is accepted.
- [ ] Given the same defaulting rule, when it is applied, then it covers the three model classes the reference binds it to, `ModelConfig`, `TranscribeModelConfig` and `TTSModelConfig`, which is what also covers `compaction_model`.
- [ ] Given two entries in the same list declaring the same alias, when the configuration loads, then the load reports a validation warning naming the duplicated alias and the document is not silently accepted with one entry shadowed.
- [ ] Given `voice_mode_enabled` or `narrator_enabled` changes while a session is running, when the next turn starts, then the audio configuration is re-read rather than kept from process start.
- [ ] Given the configuration corpus, when `config::surface_parity_tests` runs, then the new alias scenarios replay against the reference verdicts with no divergence.

---

### EP-062: The Speech Transport

Give the narrator something to speak with.

**Definition of Done:** A turn summary is posted to the configured speech endpoint with the configured voice and format, the decoded audio is played through the default output device, and a host with no device says so once.

#### US-211: Post a summary to the speech endpoint
**Description:** As an operator who turned the narrator on, I want the turn summary sent to the configured speech model, so that there is audio to play.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-207

**Reference:** [vibe/cli/tts/mistral_tts_client.py:23-27,44-56](/home/arthur/dev/mistral-vibe/vibe/cli/tts/mistral_tts_client.py) for the four resolved values, the request shape and the base64 decode at (55), [narrator_manager.py:146-165](/home/arthur/dev/mistral-vibe/vibe/cli/narrator_manager/narrator_manager.py) for `_make_tts_client` and its `KeyError` fallback to a null client, and [narrator_manager.py:200-225](/home/arthur/dev/mistral-vibe/vibe/cli/narrator_manager/narrator_manager.py) for `_speak_summary` and the playback callback

**Acceptance Criteria:**
- [ ] Given a resolved speech configuration, when a summary is spoken, then a request is issued to the provider's `api_base` carrying the model name, the input text, the configured voice and the configured response format.
- [ ] Given a successful response, when it is read, then the audio payload is base64-decoded into bytes before playback.
- [ ] Given the request fails or times out, when the failure is handled, then the narrator returns to idle, the generation is settled, and the operator is told once rather than once per turn.
- [ ] Given `narrator_enabled` is false, when a turn completes, then no speech request is issued at all.
- [ ] Given `tts_models` is empty or the active alias resolves to nothing, when a summary is produced, then no request is issued and the narrator reports the missing configuration once.

#### US-212: Play decoded audio through the default device
**Description:** As an operator who turned the narrator on, I want the returned audio played, so that I can hear the summary without looking at the terminal.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-211

**Reference:** [vibe/cli/audio_player/utils.py:7-12](/home/arthur/dev/mistral-vibe/vibe/cli/audio_player/utils.py) for `decode_wav` returning sample rate, channels and PCM, [audio_player.py:26-28,31-42,61-144](/home/arthur/dev/mistral-vibe/vibe/cli/audio_player/audio_player.py) for the constants, `check_audio_available`, `play`, `stop`, the stream callback and `_guard_audio_output`, and [audio_player_port.py:8-25](/home/arthur/dev/mistral-vibe/vibe/cli/audio_player/audio_player_port.py) for `AudioFormat` and the four error types the recorder already mirrors on the input side

**Acceptance Criteria:**
- [ ] Given WAV bytes, when they are decoded, then the sample rate, channel count and PCM payload are read from the container rather than assumed.
- [ ] Given decoded audio and an available output device, when playback starts, then the audio is written to the default output stream and a completion signal is raised when the buffer is exhausted.
- [ ] Given playback is in progress, when the narrator is cancelled or a new turn starts, then playback stops before the state machine returns to idle.
- [ ] Given no output device or no backend exists, when playback is requested, then the capability is reported as absent once per session and the narrator settles to idle rather than failing the turn.
- [ ] Given a payload that is not a supported container, when decoding is attempted, then the failure is reported by cause and no audio device is opened.
- [ ] Given playback is already running, when a second playback is requested, then the request is rejected without corrupting the running stream.

#### US-213: Wire the narrator to the transport
**Description:** As an operator, I want the narrator state machine to drive the real speech path, so that the toggle has an effect instead of a diagnostic.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-212

**Reference:** [vibe/cli/narrator_manager/narrator_manager.py:107-117](/home/arthur/dev/mistral-vibe/vibe/cli/narrator_manager/narrator_manager.py) for the three-way gate on turn end including `tts_client is not None` (112), (119-130) for `cancel` stopping playback before idling, (131-137) for `sync` rebuilding both the tracker and the client from the current configuration, and [lazy_audio_managers.py:104-115,157-162](/home/arthur/dev/mistral-vibe/vibe/cli/lazy_audio_managers.py) for the materialize-on-enabled behavior this port reproduces without the Lazy wrapper

**Acceptance Criteria:**
- [ ] Given a completed turn with the narrator enabled and a resolved speech configuration, when the summary arrives, then `NarratorEffect::Speak` reaches the speech client instead of `report_speech_unavailable`.
- [ ] Given the speech configuration cannot be resolved, when the narrator starts a turn, then `speech_available` is false and the state machine never enters the speaking state, matching the reference's `tts_client is not None` gate.
- [ ] Given `narrator_enabled` is toggled during a session, when the next turn starts, then the speech client is rebuilt from the current configuration, matching `NarratorManager.sync`.
- [ ] Given a late speech result whose generation has been superseded, when it arrives, then it is discarded and nothing is played.

---

### EP-063: The Audio Surface, the Divergences and the Scorecard

Publish the choice the configuration already offers, emit the events the reference emits, record what will not be ported, and restate the two rows.

**Definition of Done:** The voice overlay offers both model choices, the four audio telemetry events are emitted, four divergence rows are in `docs/parity.md` with resolvable evidence, and both scores are restated from oracle output.

#### US-214: Offer the audio model choice
**Description:** As an operator, I want to pick my transcription and speech model from the voice settings, so that the two alias lists the configuration publishes are usable.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-210

**Reference:** [vibe/cli/textual_ui/screens/config/config_screen.py:110-111](/home/arthur/dev/mistral-vibe/vibe/cli/textual_ui/screens/config/config_screen.py) for the two choice lists and where their options come from, [app_server/_projection.py:124-125](/home/arthur/dev/mistral-vibe/vibe/app_server/_projection.py) for the alias lists that feed them, and [app_server/config.py:73-74](/home/arthur/dev/mistral-vibe/vibe/app_server/config.py) for the two published fields

**Acceptance Criteria:**
- [ ] Given a configuration declaring more than one transcribe model, when the voice overlay opens, then `active_transcribe_model` is offered as a choice list built from the published aliases.
- [ ] Given the same for TTS models, when the overlay opens, then `active_tts_model` is offered the same way.
- [ ] Given a choice is made, when it is confirmed, then the value is persisted through the existing configuration write path and the next session resolves the new model.
- [ ] Given only one model is declared in a family, when the overlay opens, then the entry is still shown with its single value rather than hidden.
- [ ] Given the write fails, when the failure is handled, then the previous value stays selected and the operator is told, rather than the overlay showing a value that was not persisted.

#### US-215: Emit the four audio telemetry events
**Description:** As a parity reviewer, I want the reference's audio events recorded locally, so that the audio lifecycle is observable under the telemetry divergence already accepted.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-209

**Reference:** [vibe/cli/voice_manager/voice_manager.py:202-251](/home/arthur/dev/mistral-vibe/vibe/cli/voice_manager/voice_manager.py) for the four emitters, their event names and their exact property sets, and [vibe/cli/voice_manager/telemetry.py:8-30](/home/arthur/dev/mistral-vibe/vibe/cli/voice_manager/telemetry.py) for `TranscriptionTrackingState`, which supplies the recording id, the accumulated transcript length, the elapsed milliseconds and the last recording duration. The local gate is `AppServer::telemetry_record` in `crates/vibe-app-server/src/server.rs`

**Acceptance Criteria:**
- [ ] Given a transcription session is created, when the session-created event arrives, then a `vibe.audio.transcription.start` event carrying the recording id is recorded.
- [ ] Given a recording is cancelled, when cancellation completes, then a `vibe.audio.transcription.cancel_recording` event carrying the recording id and the elapsed milliseconds is recorded.
- [ ] Given a transcription completes, when it settles, then a `vibe.audio.transcription.done` event carrying the recording id, the accumulated transcript length and both durations is recorded.
- [ ] Given a transcription fails, when the failure settles, then a `vibe.audio.transcription.error` event carrying the recording id, the error message and both durations is recorded.
- [ ] Given `enable_telemetry` is false, when any of the four events fires, then nothing is recorded, matching the existing `telemetry/record` behavior.
- [ ] Given an event is recorded, when it is read back through `diagnostics/logs/read`, then it is present with the reference property names.

#### US-216: Record the accepted divergences
**Description:** As a parity reviewer, I want the four decisions that will not be ported written into the scorecard with enforceable evidence, so that they never resurface in a later review.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-205, US-213

**Reference:** [vibe/cli/autocompletion/file_indexer/store.py:63-83](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/file_indexer/store.py) for the order divergence, where `rebuild` keeps walk order and `snapshot` sorts only after an incremental update invalidated the cache, [indexer.py:116-170](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/file_indexer/indexer.py) for the background executor and its per-root cancellation, [ignore_rules.py:84-89,170](/home/arthur/dev/mistral-vibe/vibe/cli/autocompletion/file_indexer/ignore_rules.py) for `WALK_SKIP_DIR_NAMES` and its absent importers, and [audio_player.py:31-42](/home/arthur/dev/mistral-vibe/vibe/cli/audio_player/audio_player.py) for the backend probe whose absent branch this port reports. The local precedents are the `grep` order row and the per-layer async state machine row of `docs/parity.md`

**Acceptance Criteria:**
- [ ] Given the index order divergence, when `docs/parity.md` is read, then a row states that the reference's post-rebuild order is `scandir` order and this port sorts, names the normalization applied on both sides of the oracle, and points at the sorting call and the capture's normalization.
- [ ] Given the background rebuild executor, when the table is read, then a row states that the per-root cancellation tasks and `_target_root` bookkeeping are structure with no observable consequence for any current caller, and points at the module that rebuilds synchronously.
- [ ] Given `WALK_SKIP_DIR_NAMES`, when the table is read, then a row states that the constant is exported by the reference and imported by nothing at the pinned commit, and names the reference symbol.
- [ ] Given a host with no audio output backend, when the table is read, then a row states that playback degrades to a reported absent capability, and points at the reporting path.
- [ ] Given each new row, when `cargo test -p vibe-core --all-features ledger_tests` runs, then every pointer resolves to an artifact this repository holds.
- [ ] Given a divergence recorded here is later closed in code, when the corresponding replay runs, then the stale ledger entry fails the suite.

#### US-217: Remeasure and restate the scorecard
**Description:** As a parity reviewer, I want both rows restated from the two oracles' printed counts, so that the scores are defensible.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-216

**Reference:** no Python counterpart; the target is local. `docs/parity.md:71` and `:72` are the two rows, `:107` is the rank-14 execution-order row, `## Verification` is the section that names each oracle, and `crates/vibe-core/src/parity/ledger_tests.rs` is what will read the table afterward

**Acceptance Criteria:**
- [ ] Given both oracles pass, when `docs/parity.md` is updated, then the autocompletion row moves from 70 to its measured value citing the printed counts, and the voice row moves from 65 to its measured value the same way.
- [ ] Given the rank-14 execution-order row, when the table is updated, then its status reflects the two halves this PRD closes and names what remains, if anything.
- [ ] Given the `## Verification` section, when it is updated, then both new oracles are described with their reproduce commands.
- [ ] Given the `## Related` section, when it is updated, then this PRD is listed with the ranks it delivered.
- [ ] Given a count printed by an oracle disagrees with the number written in the table, when a reviewer reruns the command, then the disagreement is visible, because the table quotes the counts rather than summarizing them.
- [ ] Given user-visible changes shipped, when `CHANGELOG.md` is updated, then the watcher, the audio configuration resolution and the narrator speech are recorded under `## Unreleased`.

## Functional Requirements

- FR-01: The system must start a filesystem watcher for the active workspace root when `file_watcher_for_autocomplete` is true, and must not start one when it is false.
- FR-02: The system must apply create, modify and delete events to the index incrementally, and must fall back to a full rebuild when one batch carries more than 200 changes.
- FR-03: When a directory entry is deleted, the system must remove every entry whose relative path begins with that directory's path followed by a separator.
- FR-04: When a directory entry is created, the system must add that entry and every non-ignored descendant.
- FR-05: The system must expose a rebuild counter and an incremental-update counter matching the reference's.
- FR-06: The system must build at most one index per workspace root per process, reused across every completion query.
- FR-07: The system must resolve the transcription model name, sample rate, encoding and target streaming delay from the active `transcribe_models` entry.
- FR-08: The system must resolve the transcription endpoint host and scheme from the active model's entry in `transcribe_providers`.
- FR-09: The system must read the audio credential from the environment variable named by the active provider's `api_key_env_var`, then from the credential store, and must not attempt a lookup under an empty name.
- FR-10: The system must resolve the speech model name, voice and response format from the active `tts_models` entry, and its endpoint from `tts_providers`.
- FR-11: The system must post a narrator summary to the resolved speech endpoint and decode the returned payload before playback.
- FR-12: The system must play decoded audio through the default output device and stop playback when the narrator is cancelled.
- FR-13: The system must default a model entry's `alias` to its `name` when the alias is absent, across all four model families.
- FR-14: The system must report a duplicate alias within one model list as a validation warning.
- FR-15: The system must offer `active_transcribe_model` and `active_tts_model` as choices in the voice settings surface.
- FR-16: The system must record the four audio telemetry events with the reference names and properties, subject to `enable_telemetry`.
- FR-17: The system must NOT connect to a transcription endpoint derived from the LLM `--api-base` when an audio provider entry declares one.
- FR-18: The system must NOT fail a completion query when the watcher backend is unavailable; it must answer from the last built index.
- FR-19: The system must NOT write a secret credential value into any diagnostic, log line or corpus.

## Non-Functional Requirements

- **Performance:** a completion query against a 32 000 entry index returns in under 50 ms at the 95th percentile on the fixture tree, measured by the ranking family's timing assertion. A watcher event is reflected in the index within 1 second of delivery, bounded by the reference's 200 ms poll step plus application time.
- **Performance:** at most one full tree walk occurs per workspace root per process while the root is unchanged, asserted by the rebuild counter.
- **Reliability:** a watcher thread is joined within 1 second of shutdown, matching the reference's join timeout, and a hung backend never blocks process exit beyond that.
- **Reliability:** an absent audio output backend is reported exactly once per session and never more, asserted by a named test.
- **Reliability:** a speech request failure returns the narrator to idle in 100% of cases, with no generation left outstanding.
- **Security:** no credential value appears in any corpus, diagnostic or log line; corpora record variable names only, asserted by the capture guard.
- **Security:** both capture scripts fail the run on any socket connection or DNS resolution attempt, so no corpus can be written from a live backend.
- **Scalability:** the incremental path handles a batch of 200 changes without a full rebuild; beyond that, one rebuild replaces the batch.
- **Memory:** the ASCII mask adds 16 bytes per index entry, bounding the addition at 512 KB for a 32 000 entry index.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Watcher backend unavailable | Container without inotify, or file-descriptor exhaustion | Report once, keep answering from the last built index | "Filesystem watching is unavailable; completion may not reflect new files" |
| 2 | Mass change | Branch switch rewriting thousands of files | One full rebuild replaces the incremental batch past 200 changes | none |
| 3 | Change outside the root | Watcher reports a path the index does not cover | Skip the change, entry set unchanged | none |
| 4 | Deleted directory | `rm -rf` on an indexed subtree | Directory entry and every descendant removed by prefix | none |
| 5 | Empty index | First query before the first build completes | Wait for the build rather than returning an empty list | none |
| 6 | Non-ASCII query | Operator types an accented path fragment | Mask filtering disabled, every entry reaches the matcher | none |
| 7 | Mistyped active alias | `active_transcribe_model` names an entry that does not exist | Fall back to the first declared entry, matching the view | none |
| 8 | Empty model list | `transcribe_models = []` in a layer that replaces | Refuse to start the session, name the missing configuration | "No transcription model is configured" |
| 9 | Invalid provider URL | `api_base` is not parseable | Refuse before connecting, name the key | "The transcription endpoint URL is invalid" |
| 10 | Missing credential variable | `api_key_env_var` names an unset variable with no store entry | Refuse before connecting, name the variable | "No credential found in {variable}" |
| 11 | No audio output device | Headless host, or no PortAudio equivalent | Report once, narrator settles to idle | "Audio playback is unavailable on this host" |
| 12 | Unsupported speech format | `response_format` set to a container the decoder does not handle | Report by cause, do not open a device | "The speech response format is not supported for playback" |
| 13 | Speech request timeout | Gateway unreachable | Narrator settles, generation released, told once | "Speech synthesis failed" |
| 14 | Duplicate alias | Two `[[tts_models]]` entries share an alias | Validation warning naming the alias, surfaced through the existing warnings channel | "Duplicate alias {alias} in tts_models" |
| 15 | Alias omitted | `[[transcribe_models]]` entry with only `name` and `provider` | Alias defaults to name, entry accepted | none |
| 16 | Configuration toggled mid-session | `narrator_enabled` switched on during a turn | Client rebuilt from current configuration at the next turn | none |
| 17 | Late speech result | Turn cancelled while synthesis is in flight | Result discarded, nothing played | none |
| 18 | Reference checkout absent | Contributor without the oracle checkout | Replays still run from the committed corpora, only recapture probes skip | none |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | `notify` reports event categories differently per platform, so an incremental sequence diverges on macOS or Windows | High | Medium | US-202 validates the mapping first. The store's fallback to a full rebuild past the threshold bounds the damage of a misclassified batch, and the corpus compares entry sets rather than event streams, so a platform that coalesces events still conforms as long as the set converges |
| 2 | A watcher thread outlives the process or holds a file handle on Windows | Medium | High | The 1 second join is an explicit NFR with a named test, and the watcher is stopped on root change, on key change and on drop. The three stop paths are separate acceptance criteria rather than one |
| 3 | Audio playback cannot be tested in CI, so US-212 ships unmeasured by any oracle | High | Medium | Playback is asserted by named tests over a decoded-buffer boundary, with the device layer behind a trait, exactly as the PTY work separated the backend from the semantics. The absent-backend path is the only one CI exercises, and the scorecard records that residual rather than claiming coverage |
| 4 | Resolving the endpoint from `transcribe_providers` breaks an operator relying today on `--api-base` reaching the audio gateway | Medium | Medium | The shipped default provider entry already names `wss://api.mistral.ai`, which is the same host the current hard-coded path reaches for a default deployment, so the resolved value equals the current one unless the operator configured otherwise. The change is recorded in `CHANGELOG.md` |
| 5 | The ASCII mask changes ranking through a subtle asymmetry between mask computation and matcher folding | Low | High | US-205's last criterion asserts identical candidate lists with the filter enabled and disabled across every corpus tree, which fails on any asymmetry. The story is P2 and can be dropped without blocking the epic |
| 6 | Two oracles plus a new dependency plus a new subsystem is a large surface for one PRD | Medium | Medium | Eighteen stories across six epics, with the two oracle epics independent of each other, so EP-058 and EP-060 can proceed in parallel and the two halves never block one another. EP-062 is the only genuinely new subsystem and it sits last |
| 7 | Alias defaulting changes how existing on-disk documents merge, silently altering a resolved model | Low | High | The defaulting runs before validation, exactly as the reference does, and the configuration corpus replays the reference verdicts. A document that resolves differently after this change fails the existing configuration oracle before it reaches an operator |

## Non-Goals

- **Porting the reference's background rebuild executor.** The per-root `_RebuildTask`, the cancellation events and the `_target_root` bookkeeping have no observable consequence for any current caller, since this port already rebuilds inside a dedicated worker. Recorded as an accepted divergence instead.
- **Reproducing `WALK_SKIP_DIR_NAMES`.** Exported by the reference and imported by nothing at the pinned commit. Shipping a derived constant with no consumer is code this repository would then have to maintain and justify.
- **Speech output formats beyond the shipped default.** The schema accepts other values, and playback covers the WAV container the default names. A different container reports an unsupported-format cause rather than shipping a codec matrix.
- **A batch (non-realtime) transcription fallback.** Rejected in `tasks/decision-ep005-voice-boundary.md` because it cannot reproduce live deltas or flushing semantics, and nothing in this PRD changes that reasoning.
- **Shipping audio telemetry off the machine.** The telemetry envelope is already an accepted divergence; the four events are recorded locally through the existing `telemetry/record` path and are not transmitted.
- **The `vibe mcp` subcommand, OTel, and experiments.** Adjacent gaps in `docs/parity.md` that belong to ranks 15 and 16.
- **Vim navigation, word selection, `load_more` and braille rendering.** The other TUI gaps the scorecard records, unrelated to the index.

## Files NOT to Modify

- `crates/vibe-core/src/parity.rs` and `scripts/parity/pin.py`: the two pin sources. This PRD measures against the existing pin and re-pinning would invalidate every committed corpus in the repository.
- `crates/vibe-core/tests/config-surface/corpus.json`: regenerate through `scripts/parity/config_surface.py` for US-210's scenarios; never hand-edit, since the replay compares it against the reference.
- `crates/vibe-cli/src/tui/completion/fuzzy.rs`: an integer-exact port already verified against the reference scoring. US-205 filters before it and must not change it.
- `crates/vibe-cli/src/tui/narrator.rs`: the state machine is already conformant. EP-062 wires effects to a transport and must not alter the transitions.
- `crates/vibe-protocol/src/lib.rs`: nothing in this PRD adds or renames a wire method; a new name would fail the app-server surface oracle on an invented name.

## Technical Considerations

- **Architecture:** should the watcher live beside `WorkspaceIndex` in `crates/vibe-cli/src/tui/completion/`, or in its own module under `completion/index/`? Recommended: a `completion/index/` module holding the store, the rules and the watcher, since the current `path.rs` already mixes indexing, ranking and ignore rules in 596 lines and the incremental store will push it past a reasonable single file. Engineering to confirm the split does not disturb the existing test module paths.
- **Dependencies:** `notify` is the only new production dependency, and it is what `watchfiles` itself is built on, so the event semantics match by construction. Which version and feature set? Engineering to confirm whether the default backend or a debounced wrapper better matches the reference's 200 ms step, and whether a debouncer is worth a second crate.
- **Data model:** should `IndexedPath` gain the ASCII mask unconditionally, or only when the mask filter is enabled? Recommended: unconditionally, since the field is 16 bytes and a conditional field would fork the walk. Trade-off: 512 KB at the 32 000 entry cap.
- **Architecture:** where should the speech client live, `vibe-cli` beside the realtime transcriber, or `vibe-core` beside the other HTTP clients? Recommended: `vibe-cli`, matching both the reference layout (`vibe/cli/tts/`) and `tasks/decision-ep005-voice-boundary.md`, which placed the realtime transcriber there for the same reason. Engineering to confirm `reqwest` can be added to `vibe-cli` without pulling a second TLS stack.
- **Architecture:** should audio output sit behind a trait so CI can exercise the decode boundary without a device? Recommended: yes, mirroring the `VoiceSessionFactory` trait the recording path already uses. Engineering to confirm the trait boundary lands below decoding so the decoder stays testable.
- **Migration:** resolving the transcription endpoint from `transcribe_providers` changes the effective host for anyone who set `--api-base` to a non-default value while relying on it reaching the audio gateway. Backward compatibility: the shipped default resolves to the same host, so only explicitly reconfigured setups change. Rollback: revert the resolution and the hard-coded constants return.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Declared-only configuration keys in these two areas | 7 | 0 | Month-1 | Grep for each key, one production reader each, plus the per-key behavior test named in its story |
| Autocompletion parity score | 70, from module presence | 100, from oracle output | Month-1 | `cargo test -p vibe-cli --all-features autocompletion_parity_tests -- --nocapture` |
| Voice parity score | 65, from module presence | 100, from oracle output | Month-1 | `cargo test -p vibe-cli --all-features voice_parity_tests -- --nocapture` |
| Corpus scenarios replayed | 0 | at least 240 across 9 families | Month-1 | Printed totals of the two replays |
| Divergences outside a named ledger | not measurable | 0 | Month-1 | Both replays fail on any unledgered divergence |
| Hard-coded transcription wire values | 4 | 0 | Month-1 | `wireFrames` family conforming, plus a test that changes each value and observes the frame change |
| Full tree walks per root per process | 1 per query on the synchronous path | 1 per root | Month-1 | The rebuild counter asserted across consecutive queries |
| Index staleness after a file write | unbounded (process lifetime) | under 1 second when watching | Month-1 | Named test writing a file and querying |
| Stale ledger entries | not measurable | 0 | Month-6 | `ledger_tests` plus each replay's stale check |
| Weighted total in `docs/parity.md` | 80 | restated with these two rows | Month-1 | Hand-maintained judgement over the remeasured rows, as the table's own note describes |

## Open Questions

- Does `notify` on macOS coalesce a directory rename into a create-plus-delete pair, or into a single rename event the reference never sees from `watchfiles`? US-202 answers this during implementation; if a rename category exists, it maps onto the delete-then-add pair the store already handles, and the mapping is recorded in the corpus `note`.
- Should the duplicate-alias case be a validation warning, as this PRD specifies, or a hard load failure as the reference's `ValueError` implies? The reference raises during model validation, but this port's load path collects `validation_warnings` and repairs rather than refusing, which is an already-recorded architectural difference. Decided as a warning for consistency with the existing load; a reviewer may overturn it in US-210 if the corpus shows the reference's failure is observable to a client.
- Is the ASCII mask worth its 512 KB and its extra field, given the fuzzy matcher already short-circuits on length? US-205 is P2 precisely because the answer may be no; its final criterion makes the filter provably neutral, so dropping the story costs only latency.
- Does the reference ever run the index against a root other than `Path(".")`? `completers.py:323` carries a TODO questioning that assumption. This port already resolves the workspace root explicitly, which is a superset; if a reviewer finds a reference path that passes something else, the multi-root behavior in US-202 already covers it.
[/PRD]
