# Changelog

## Unreleased

- Let an operator rewrite what a tool tells the model. A `<tools-dir>/prompts/<name>.md`
  file now replaces the description the matching tool publishes, read from the
  `tool_paths` entries in configuration order, then from `.vibe/tools` in every
  open project directory, then from `~/.vibe/tools`, with the later directory
  winning and a directory named twice read once at its first position. A blank
  file, a file that cannot be read, and a file named after no tool all leave the
  surface as it was, and a session naming an unresolvable entry still opens. The
  match is on the published name, so a tool served by an MCP server or by a
  connector is redescribed the same way, and the files are re-read at every
  publication, so a description written while a session is open reaches the next
  turn.

- Render a proxied tool call the way the reference renders it. A tool published
  by an MCP server or by a connector no longer borrows the generic fallback that
  spells its arguments into the header: the call now names the published tool
  alone, the loading indicator names the remote tool the server knows, and the
  settled header reports how the call itself settled rather than what a server
  answered about its own subject, so a remote call that failed no longer renders
  as a success and a successful one is no longer reported as a failure. Every
  published call display now carries a `settledMessage`, filled from the
  presentation that names its own subject or from the summary when none does,
  and the generic fallback names its first three arguments in the order the call
  carried them, spelling a boolean, a null, and a nested object the way the
  reference interpreter prints them.

- Say what the built-in tools answer differently, tool by tool. The parity
  scorecard's single line about warning wording is now five rows that name
  every tool whose authored result or error text is this port's own prose and
  the field it lands in, plus the capped catalog an unknown skill name is
  answered with, the HTML page stripped to prose, and the request envelope the
  HTTP client adds. Every row is bound to what holds it, in both directions, so
  a divergence that closes fails the suite until its scorecard row goes with it,
  and the built-in tools row is remeasured from the widened execution oracle:
  103 cases over 11 tools, none diverging outside the ledger.

- Answer `web_fetch`'s contract edges the way the reference answers them. A
  `timeout` above `max_timeout` is now refused by name instead of being lowered
  to the cap, so a call that cannot run as asked is reported rather than quietly
  running as something else, and a non-positive `timeout` and an empty `url` are
  refused as tool errors rather than schema violations. A page past
  `max_content_bytes` is cut at that bound alone, no longer at whichever is
  smaller between the bound and what the turn's output buffer has left, and a
  body landing exactly on the bound is no longer flagged as truncated.

- Fetch pages the way the reference fetches them. The request now carries the
  `Accept` and `Accept-Language` values the reference sets, so a host that
  varies its answer on them serves this port the same document; a 403 carrying
  `cf-mitigated: challenge` is retried exactly once under a user agent that
  names itself, and any other 403 is reported on the first answer. The redirect
  budget rises to the reference's own, so a long chain the reference follows is
  no longer refused, and a `Content-Type` of `text/html` with a charset is now
  read as HTML while a type that merely contains the word is not.

- Publish `task` behind the permission policy, like every other built-in tool.
  `tools.task.denylist` refuses a subagent outright and `tools.task.allowlist`
  grants one without a prompt, both matching the name as a glob rather than by
  equality, and the denylist is read first, so a name in both lists is refused.
  A subagent in neither list now asks, and declining starts no child and hands
  the model the refusal. The allowlist still defaults to `explore`, so the
  built-in subagent delegates without a prompt.

- Delegation stops one level deep, at the call rather than by hiding the tool.
  A subagent that may act now sees `task` in its own tool list and reads an
  error when it delegates again, instead of finding the tool missing; a
  top-level call is unaffected.

- Publish the second document the UI reads. `grep` now sends the client one
  parsed entry per match, each carrying the matched file's path anchored on the
  searched root and the line number it was found at, and `edit` now sends one
  entry per replacement with the line it started on and the whole lines it
  replaced. The transcript draws an applied edit from those occurrences instead
  of diffing the two raw strings, so a `replace_all` shows one hunk per
  replacement at its own line.

- Return the reference's own result fields from the last five built-in tools
  instead of the bare answer. `web_fetch` now renders `url`, `content`,
  `content_type` and `was_truncated`, `web_search` renders `query`, `answer`
  and `sources` with an empty search rendering `[]`, `task` renders `response`,
  `turns_used` and `completed`, `ask_user_question` renders its answers and
  `cancelled`, and `exit_plan_mode` renders `switched` and `message`. The model
  reads one labeled line per field the way the reference writes it, and
  `web_fetch` publishes `content_type` and `was_truncated` where it used to
  publish `contentType` and `wasTruncated`.

- Answer the edges of a slash line the way the reference answers them. Exactly
  one leading marker is stripped, so `//` and `///` now show nothing instead of
  the whole command list, and the caret bounds both the query and the range a
  candidate replaces: with the caret inside `mcp` on `/mcp add x`, the popup
  ranks `mc` and accepting replaces only up to the caret, leaving the rest of
  the line in place behind a separating space.

- Echo a submitted slash command into the transcript before it runs, under the
  `/` prompt the reference paints it with and with no separator under it. The
  line keeps its arguments and its case and loses one leading slash, and a bare
  alias is echoed as the registry key it resolved to, so `:q` reads as `exit`.
  A command the runtime refuses echoes nothing.

- Report the registry key a command answers under, once, and only when the
  command actually runs. `/connectors` now reports `mcp`, `/new` reports
  `clear`, and a command refused because a job is running or the queue is paused
  reports nothing. `/exit` no longer bypasses dispatch, so it is echoed and
  reported like every other command.

- Give the two refusals their two reasons: a running job asks the operator to
  let it finish, a paused queue asks them to clear it or remove the input. A
  teleport line carries the same two reasons, and either refusal returns the
  submitted text to the composer and queues nothing.

- Answer `/help` with a Markdown message in the transcript instead of a modal
  overlay, the way the reference mounts it. The document carries the reference's
  three sections in its order: the key bindings, the input prefixes, and every
  available command sorted by registry key with all of its aliases as code
  spans, canonical name first. It scrolls, selects and copies like any other
  entry, and it survives a session reload. The modal help overlay is gone, and
  the shortcut section is now the key handler's own binding table, so a line can
  never advertise a chord this binary ignores: it names the `Ctrl+D` quit this
  port binds and the empty-prompt condition its double-escape rewind applies.

- Measure the slash-command registry against the pinned reference instead of
  against a hand-diffed list of names. `scripts/parity/commands.py` drives the
  reference's own `CommandRegistry` and records its key and alias inventory, the
  keys each availability context leaves standing, what fifty-seven submitted
  lines resolve to, and the structure of the `/help` document.
  `crates/vibe-cli/src/tui/commands_parity_tests.rs` replays that corpus as 395
  comparisons behind a divergence ledger that fails both on a divergence no
  entry names and on an entry whose divergence stopped reproducing, and
  recaptures it live when the checkout sits on the pin. Every reference-authored
  help line is recorded as a length and a SHA-256 and never as text.

- Resolve a command alias the way the reference resolves it. Alias matching
  folded ASCII case only, so `/THINKING` spelled with the Kelvin sign U+212A
  resolved to `thinking` upstream and did not parse here. The head word is now
  lowercased the way the reference lowercases it, which the parse family of the
  new corpus measures.

- Offer the flags the binary actually accepts in every shell completion. The
  four committed completion files were written by hand and had drifted: none
  offered `--yolo`, the visible alias of `--auto-approve`, the zsh and fish
  files omitted `--enabled-tools`, `--disabled-tools` and `--worktree`, and the
  fish file omitted `--help` while still offering `--telemetry`, which the CLI
  stopped accepting when telemetry moved into the configuration file. They are
  now held to the clap definition by a test that names each flag present in one
  and absent from the other, so a new flag shipped without a completion update
  fails the suite naming the flag and every file that omits it.

- Exercise `scripts/install.ps1` on a Windows runner. The PowerShell installer
  is the one delivery path no Linux job can execute, so CI now packages a real
  `windows-x86_64` archive and drives the installer through a clean install,
  the refusal of an interrupted upgrade, the refusal of an archive whose digest
  the manifest does not name, and removal.

- Keep the recorded update state across the pre-TOML layout and across the rest
  of `cache.toml`. An `update_cache.json` written by an older layout was ignored,
  so an upgrade re-announced release notes that had already been read and
  re-offered a version that had already been dismissed. That file is now read
  once and migrated into the `[update_cache]` section, its null keys omitted.
  Writing the section merges into what the file already holds instead of
  replacing it, so a key this port does not model, an optional key the write
  omits, and any unrelated table all survive. A file past one megabyte reads as
  absent rather than being parsed into memory, and a failed write reports the
  cache-write error while leaving the previous file intact.

- Check for updates on the releases of the repository the binary was installed
  from. The update check read the PyPI project the Python reference publishes,
  so an installed binary compared itself against a version it could never
  install and offered an upgrade to a different product. It now reads the
  GitHub releases of `[package] repository`, honoring `GITHUB_TOKEN` when one is
  exported and still honoring `VIBE_UPDATE_BASE_URL`.

- Make `Update now` install. The prompt offered an update and then reported that
  no update was available, because installing was not implemented. It now reruns
  the installer this repository publishes and reports the reference's four
  outcomes with its exit codes: continue starts the session, an installed
  upgrade names both versions and exits 0, a failed upgrade names the manual
  path and exits 1, and quitting exits 0. Ctrl+C during an upgrade terminates
  the running command, kills it if it has not exited after two seconds, and
  closes the prompt as a quit rather than starting a session on a release it
  never installed.

- Fetch releases from the repository that actually publishes them. Both
  installers defaulted to a `github.com` owner no remote of this project uses,
  so every install that did not override the base URL resolved a 404 rather
  than an archive. The default is now bound to `[workspace.package] repository`
  and a test fails when either script drifts from it.

- Publish one `SHA256SUMS` covering every packaged target. Each packaging run
  overwrote the aggregate manifest with its own single line, so whichever
  matrix leg finished last decided which target a release could verify and the
  other four were unverifiable. A run now merges its line into the manifest,
  replacing only its own and sorting by archive name, which makes the result
  independent of the order the targets are packaged in.

- Release from a tag. Nothing built or uploaded the five published archives, so
  a version bump produced no downloadable release at all. A tag push now
  packages every target, refuses to continue when the tag disagrees with
  `[workspace.package] version`, publishes only when every leg succeeded, and
  then installs the result with the committed `install.sh` and no override. No
  tag has been pushed: the workflow is dormant until the port's parity is proven
  across the scorecard, so its shape is asserted by tests rather than by a run.

- State the version once. The version string is hand-written in five files
  besides the workspace manifest, and nothing detected the drift between them;
  a test now reads the manifest and fails both when a copy disagrees and when a
  file stops carrying the version at all.

- Name the envelope and the offending field when an inbound frame is rejected.
  Every rejection reported only that no envelope variant matched, so a missing
  `message`, an unknown error code, a stray field and a non-object `result`
  were indistinguishable. Since a rejected frame carries no answerable `id` and
  the connection is closed instead, that message was the only record of the
  cause, and it named none of them.

- Keep the `data` key on an error frame that carries no detail. The reference
  dumps its error payload without a null filter, so `"data": null` is on the
  wire whether or not a detail exists. This port dropped the key, which made
  every detail-free error frame one key shorter than the reference's.

- Refuse a request or a notification that omits `params`. The reference
  declares the field on both inbound shapes without a default, so a frame that
  leaves it out fails validation there. This port read an absent `params` as an
  empty one, which let a client through that upstream turns away, and left the
  two implementations disagreeing on which frames are well formed.

- Report an orphaned shell session under the status its own manifest recorded.
  `<family>_output` forced `orphaned` on every session a previous process left
  behind, so a build that finished cleanly before the client exited was
  reported as orphaned by that tool while `<family>_sessions` listed it as
  completed. Both now answer from the same value, as the reference does.

- Honor a hook that never reads its stdin. The invocation is written to the
  hook's standard input, and a hook that exits without consuming it closes the
  read end, so the write returns a broken pipe. That was reported as a hook
  failure, which made such a hook succeed or fail depending on how loaded the
  machine was: its rewrite of the tool's arguments was silently discarded on a
  busy host. The reference swallows a broken pipe there and reads the hook's
  answer off its stdout and exit status, and so does this port now.

- Mark the active theme as current in the `/theme` picker when no theme has
  been persisted. The picker and the cancel path read the preference through
  two copies of the same accessor that disagreed on the default, so the
  catalog's automatic entry was annotated on one path and on neither the other.

- Keep the plan-mode directive and the agent profile's prompt on every cycle of
  a persisted session. The transcript was hydrated after the preamble had been
  composed and replaced it wholesale, so the model was told the workspace was
  read-only on the session's first cycle and on no other. Both prompts now
  survive a resume.

- Report a malformed optional parameter instead of substituting a default for
  it. `session/continue` with a non-string `cwd` silently continued the
  server's own directory, `session/fork` with a non-string `newSessionId`
  silently generated one, and a non-string `systemPrompt` silently became
  empty. All three now answer `invalid_params`, as the other dispatchers
  already did.

- Refuse a `session/list` or `history/list` page outside the range the session
  store accepts, as `invalid_params` naming the parameter, rather than letting
  it surface as a storage conflict.

- Publish an editor session's command catalog after the response that
  announced the session, not before it. The reference sends it from a task the
  session spawns, so a client learns a session exists before it is told what
  that session can run; this port queued it during dispatch, and the writer
  being ordered meant the editor saw it first. `session/new`, `session/load`
  and `session/fork` are all affected.

- Report the session's real context window on a usage update rather than a
  fixed 200,000. The size an editor renders now comes from what the session
  publishes, as the reference reads it, so a model that declares another
  window no longer shows a context bar measured against the wrong total.

- Tell the two session conflicts apart. A session that already has a prompt or
  a command running and a session identity a second lifecycle operation is
  claiming were one error, and the first rendered as a sentence nested inside
  another one. A load whose reservation is taken over by a concurrent shutdown
  now reports that conflict instead of claiming the adapter was never
  initialized.

- Describe the ACP client tools the agent can call with the arguments those
  methods actually take: `terminal/create` declares its arguments, environment,
  working directory and output limit, and `fs/read_text_file` its line and
  limit. Every tool previously advertised its parameters as a flat set of
  required strings.

- Stop the throwaway service a `session/load`, `session/fork` or
  `session/list` opens even when the work fails. A refused load left its
  service running for the life of the process.

- Resolve a session's rollout once, off the startup path. With
  `enable_telemetry` and `experiments.enable` both on, a Mistral provider
  present and its variable resolving, a detached lookup posts nine attributes
  to the configured eval host and applies what comes back: the variants land in
  the configuration layer that sits below every file a human wrote, and the
  confirmed exposures travel on every telemetry event the session sends
  afterward. The credential never leaves as itself, only as the truncated
  SHA-256 the bucketing key is, and the organization a rollout can be scoped to
  is read from the provider's own `/users/me` under a four second budget.
  Nothing waits on any of it: either gate turned off issues no request at all,
  and a failed lookup, an unreadable identity and a third-party provider each
  leave the session on its declared variants. What a session resolved is
  written to its metadata, so a resumed session and a fork reuse it instead of
  asking again, and a session that quits mid-lookup cancels the request rather
  than waiting for it.

- Leave `active_model` unpinned in the document a fresh installation writes, as
  the reference does, and resolve the alias when it is read: a pinned value
  still wins, an empty one selects `routed_default_model` when it names a
  configured model, then the shipped default alias, then the first model there
  is. The model new turns run on is unchanged, and only the stored document
  differs from what earlier builds wrote.

- Declare `routed_default_model` and `routed_model_config`, the two fields a
  routing rollout writes. The definition arrives as the JSON text of one model
  and is read through the reference's own field types, so a quoted price is a
  number and a key the model does not declare is dropped; a definition that
  does not read is ignored with a validation warning rather than failing the
  load. A definition whose alias matches the routed one and that no
  configuration declares is merged into the model map under that alias.

- Select the managed shell session family with `managed_shell_tools_enabled`
  rather than with the `VIBE_MANAGED_SHELL_TOOLS` environment variable, which
  is withdrawn. The field is read from the session's merged configuration at
  every registration, so a value written after startup reaches the next one.
  The three fields are filled by a runtime rather than by an operator and are
  withheld from the settings screen, as the reference withholds them.

- Ship the event a client records. `telemetry/record` now hands its name and its
  properties to the same telemetry client the turn's own events travel through,
  under the same envelope and behind the same `enable_telemetry` key, in
  addition to leaving the entry on the debug console. What a client recorded is
  carried unmodified: the identity census travels underneath its properties and
  the client's own keys win.

- Accept the `telemetry/send` notification on the editor protocol, so an editor
  reports what it observes through the same client the terminal uses.
  `vibe.at_mention_inserted` is recorded with the properties the editor sent,
  and `vibe.user_rating_feedback` with the rating it supplied, defaulting to
  zero, the alias of the model the session runs on, and a correlation with the
  last request. Any other event name is ignored with a warning naming it, a
  notification for a session that is not open is dropped, and neither an
  unsupported name nor an invalid payload is answered on the wire, since a
  notification carries no identifier to answer.

- Write a log file at `$VIBE_HOME/logs/vibe.log`, so a failure that happens
  before the app server attaches leaves a trace on disk. A record is one line,
  `<timestamp> <ppid> <pid> <LEVEL> <message>`, with backslashes and newlines
  encoded so a message carrying either stays on its line and decodes back
  exactly. `DEBUG_MODE=true` sets the level to `DEBUG`, `LOG_LEVEL` names one
  otherwise and an unknown name falls back to `WARNING`, and `LOG_MAX_BYTES`
  moves the 10 MiB ceiling the file rotates at, keeping no backup. Opening the
  file twice attaches nothing new, and a directory that cannot be created or
  written is reported once and starts the binary anyway.

- Answer the debug console from that file rather than from a buffer this process
  kept, so a line another process wrote is readable and every entry carries the
  identifiers of the process that wrote it instead of zeros. A page is newest
  first, `limit` and `offset` walk backward from the end, a page that filled its
  limit says where the next one starts, no log file at all is an empty page
  rather than an error, and a line that does not parse is skipped rather than
  failing the read.

- Export OpenTelemetry spans, so the three configuration keys that advertised
  tracing finally do something. With `enable_telemetry` and `enable_otel` both
  on, the binary installs an OTLP HTTP exporter pointed at `otel_endpoint` when
  one is set and at the Mistral provider's own collector otherwise, and a turn
  is reported as the four upstream span families: one `invoke_agent` span per
  turn, a `chat` span per model call carrying the request model, the API style,
  the temperature, the token counts and the HTTP status it was answered with, an
  `execute_tool` span per tool call carrying its arguments and result, and a
  `hook` span per hook run carrying the tool it guards. The conversation
  identifier is published once by the turn and read back by every descendant, so
  a collector shows one tree per turn. `otel_redaction` decides what leaves the
  process: `strict` replaces every content-bearing attribute outright, the
  default scans values for credentials and personal data, and `none` exports
  what the span carried. Tracing never changes an outcome: with no exporter
  installed, or with one that fails, the turn, the tool call and the hook answer
  exactly what they answered before. When tracing is on and the credential
  variable resolves to nothing, the binary names the variable and starts anyway.

- Ship telemetry under the upstream envelope, and let the configuration decide
  it. An event now travels as `{"event", "properties"}` plus a correlation
  identifier when there is one, with the properties being the 15-field identity,
  session and launch census merged with the event's own payload, so a datalake
  consumer written against the reference reads what this binary sends without a
  translation layer. The endpoint, the credential and the user agent are all
  resolved from the active Mistral provider in the merged configuration, the
  credential being read under the variable that provider declares from the
  environment first and the OS credential store second: a third-party provider
  never supplies the key, a provider whose key variable resolves to nothing
  sends nothing, and a delivery that fails, times out or is rejected is
  swallowed without touching the turn.

- Decide telemetry with `enable_telemetry`, in both directions. The key is read
  from the merged configuration on every send, so setting it to `false` stops
  every event from the CLI, the TUI and the app server, editing it mid-session
  decides the next one, and a configuration that cannot be read at all silences
  telemetry rather than failing the run. It defaults to on, matching the
  documented default. **Breaking:** the `--telemetry` flag no longer exists;
  passing it is an unknown argument.

- Pick the transcription and speech model from the voice settings. `/voice` now
  offers `active_transcribe_model` and `active_tts_model` beside the two
  toggles, each as a choice list built from the aliases the configuration
  publishes, including the entry a family declares on its own. A confirmed
  choice is written through the same configuration path every other setting is,
  and both audio surfaces are resolved again from it, so the next recording and
  the next narrated turn address the model that was chosen. A write that cannot
  land leaves the previous value selected and says why. A `[[tts_models]]` entry
  now publishes its output container as a closed vocabulary rather than as a
  free string.

- Report the whole upstream event vocabulary. A session now reports when it
  opens and closes, how long it took to become ready and how long the terminal
  took to draw its first frame; every model call reports the model, the context
  and prompt sizes, the call type and the attachments it carries; every answered
  tool call reports the tool, how it ended, the operator's decision, the agent
  profile, the model, the files it created or modified, the extension it touched
  and whether a shell command ran in the background. A slash command, a copied
  selection, an interrupted agent, a refused approval, a cancelled question, an
  inserted `@` mention, the voice toggle and an API key added during onboarding
  each report themselves too, and so does a teleport run: its completion or its
  failure, attributed to the stage it reached, with the project picker's own
  payload merged in, and a link the service refuses is reported as cleared
  because the failure now carries the HTTP status that refused it. `vibe.admin_config_applied` is the one upstream event this
  binary does not raise, because it reports on an org-managed configuration
  layer this binary does not compose.

- Record the audio lifecycle. A transcription session that opens, a recording
  that is cancelled, a transcription that completes and one that fails each
  report an event carrying the recording id the endpoint named, the accumulated
  transcript length and the durations involved, and a narrated turn reports its
  request, its playback and how it ended. The events are sent under the same
  envelope and the same `enable_telemetry` gate as every other event rather than
  being kept locally.

- Watch the workspace while `file_watcher_for_autocomplete` is on. A file
  created, modified or deleted during a session is reflected in the next `@`
  completion instead of at the next process start: changes are applied
  incrementally, a batch carrying more than 200 of them falls back to a full
  rebuild, a deleted directory drops every entry under it and a created one adds
  every non-ignored descendant. The index is built once per workspace root and
  reused across every query rather than rebuilt per query, and the preference is
  read on every query, so turning it off stops the watcher.

- Speak the turn summary the narrator produces. With `narrator_enabled` on, a
  completed turn is posted to the `[[tts_models]]` entry `active_tts_model`
  names, at its provider's `api_base`, carrying that entry's model, voice and
  response format, and the audio the response answers with is decoded and
  played through the default output device. Cancelling a turn or starting the
  next one stops playback before the narrator returns to idle, a summary whose
  turn was superseded plays nothing, and a second playback is refused rather
  than layered over the running one. A configuration that resolves to no
  speech model, a payload that is not a supported container and a host with no
  output device each leave the turn successful and tell the operator once per
  session instead of once per turn.

- Address the transcription session the configuration names. The endpoint, the
  model, the sample rate, the encoding and the target streaming delay now come
  from `active_transcribe_model`, its `[[transcribe_models]]` entry and that
  entry's provider instead of from constants and the LLM `--api-base`, so voice
  mode works on a self-hosted or regional audio gateway, including one served
  below a path prefix. The credential is read under the `api_key_env_var` the
  provider declares, from the environment first and the credential store
  second, and a provider naming no variable keeps the credential the session
  started with. A configuration that resolves to no session no longer connects
  to a default: the start reports it, naming the missing configuration or the
  unset credential variable and never the secret. The audio surface is read
  again on every configuration change, so an edited model or provider takes
  effect on the next recording rather than at the next process start.

- Give `[compaction_model]` the alias rule and the field defaults every
  `[[models]]` entry already had. A table declaring a name and a provider is
  published with an alias borrowed from its name, the `ModelConfig` defaults and
  the global compaction threshold, as the reference's `_default_alias_to_name`
  binding gives it.

- Publish an audio entry with the per-entry defaults the reference fills in. A
  `[[transcribe_models]]`, `[[tts_models]]`, `[[transcribe_providers]]` or
  `[[tts_providers]]` entry that declares only what identifies it is now
  published with the sample rate, encoding, language, streaming delay, voice,
  response format and endpoint the reference defaults them to, instead of with
  a zero or an empty string a client cannot use.

- Give `file_watcher_for_autocomplete` its first reader. With the key on, the
  workspace is watched and a file created, modified or deleted during a session
  is offered by the next `@` query instead of staying invisible until the
  process restarts. A batch of changes updates the index in place, and only a
  batch larger than 200 falls back to a full rescan. A host that cannot watch
  the filesystem is told once and completion keeps answering from the last
  built index. The index is also now built once per workspace root for the life
  of the process rather than once per keystroke on the synchronous path.

- Rank an `@.` mention query the way the reference does. The query's stem was
  read as `.` rather than as the empty stem the reference's path handling
  answers, so every candidate whose own stem does not start with a dot lost the
  stem-prefix rank component and sorted below one that did. The new
  autocompletion oracle measured the divergence.

- Reword four onboarding screen lines that read identically to the upstream
  ones: the welcome hint, the theme heading, the custom-domain heading and the
  browser step's completion detail. Each says the same thing in this port's own
  words, which `NOTICE` requires, and a new guard digests every sentence the
  onboarding screens and the ACP authentication methods carry and fails if one
  ever matches the reference's.
- Publish the reference's ACP authentication surface. `vibe-acp` now
  advertises `browser-auth` when the active provider supports browser
  sign-in, adds `browser-auth-delegated` when the client declares that
  capability, adds a `vibe-setup` terminal method under `terminal-auth` that
  runs the `vibe` binary's setup flow, and
  advertises nothing to a JetBrains client whose provider is already usable;
  the invented `environment` method is gone. `authenticate` drives the full
  browser flow or the delegated start/complete lifecycle, honoring
  `signInTarget` with a validated custom domain, and the new `auth/status`
  and `auth/signOut` extension methods report credential provenance and
  offer the product's only sign-out, refused exactly where the reference
  refuses it.
- Replace the chat-transcript setup with the reference's onboarding screens.
  `vibe --setup` and an interactive launch with no resolvable credential now
  walk the reference's screen graph: a welcome screen whose advance action
  arms only after the text finishes typing, a wrapping theme picker with a
  live preview, and, for a provider that supports browser sign-in, the
  authentication method, sign-in target, custom domain and browser sign-in
  screens, with the API key screen as the manual path. A custom console
  domain typed once is validated live, warned about when it looks like a
  Mistral private-cloud host, derived into the browser and API base URLs the
  sign-in uses, and persisted to the provider entry. The flow terminates with
  the reference's five values and their exit paths: a cancellation leaves
  nothing behind and exits cleanly, an unusable key variable exits with a
  failure, and every other path persists the chosen theme once after the
  screens close. The previous setup's network, model and workspace-trust
  questions are gone; proxy and certificate settings stay reachable from
  `/proxy-setup`, the model from `/model`, and workspace trust is decided by
  the pre-session dialog, which `--setup` no longer suppresses.

- Speak the browser sign-in protocol. `vibe_core::auth` now carries the PKCE
  `S256` flow the reference speaks: a sign-in process is created against the
  configured console API, polled every 3 seconds while tolerating two
  consecutive transient failures and never sleeping past the server's expiry,
  and exchanged for the API key, with the reference's four statuses and eleven
  error codes reproduced exactly. Every server-supplied URL is validated
  against the configured console origin and path prefix before any request is
  issued to it or any browser opened at it, on every use; the 33 captured
  reference verdicts replay identically. The two configuration keys
  `browser_auth_base_url` and `browser_auth_api_base_url` are now consumed by
  a real code path, resolved from the provider entry under the reference's
  availability gate and its mistral-only defaults, so a custom console domain
  steers every sign-in request while a third-party provider gets no browser
  sign-in even when it carries both keys. The system browser is launched detached with its output discarded,
  a host where no launcher spawns reports the failure and keeps the sign-in
  URL retrievable for manual use, and no log, error or debug output ever
  carries a credential, an exchange token, a code verifier or a full
  server-supplied URL.

- Store the API key under the keyring service both implementations read.
  Credentials now live under `ai.mistral.vibe` with the reference's read
  fallback to the legacy `vibe` service, plus a read of `mistral-vibe-rs`, the
  service every earlier build of this port wrote under; a key found under a
  non-current name is rewritten under the current one and the old entry
  removed, so an upgrade never asks for the key again and a key saved here is
  visible to the reference on the same machine.

- Stop losing the API key when the keyring is unavailable. A keyring that
  refuses the write, including a headless host with no Secret Service, now
  degrades to writing `$VIBE_HOME/.env` with owner-only permissions instead of
  failing setup, and a later successful keyring save removes the stale
  plaintext copy. That file is edited the way the reference edits it, staged
  beside itself and moved into place: an interrupted write leaves the other
  variables it holds whole, and a symlinked `.env` is replaced rather than
  written through, so the key never lands at the link's target. Credential
  provenance is classified into the reference's six
  auth states with its five-level precedence, replacing the binary
  found-or-not decision at startup, and `account/read` now resolves the key
  through the keyring as the reference does, so a key stored only there reads
  as `ready` instead of `missing_key`.

- Persist the provider entry the setup flow authenticated against, upserted
  into `providers` keyed by name with only non-default fields, writing nothing
  when the entry is unchanged and preserving fields this port does not model.

- Port the remote skill registry, dormant. The catalog client with its 50-page
  cap and error taxonomy, the atomically staged version store with its
  traversal and entrypoint guards and owner-only execute bits, and the two
  manifest scopes with the `latest` alias pin are all implemented and measured
  against the reference, and none of them is reachable from a session: the
  reference itself never calls this subtree at the pinned commit, so no
  install command or wire method is invented for it.
  `experimental_enable_registry_skills` is now read from the merged
  configuration and gates the subtree whole; disabled, which is the default,
  runs no registry code, creates no cache directory and constructs no
  transport, and enabled changes nothing until upstream publishes a load
  lifecycle to reproduce.

- Record a slash-invoked skill in the conversation the way the reference
  does. Submitting `/name` now appends a synthetic `skill` tool call and its
  result to the session history, so the transcript shows the load settling,
  the persisted conversation reads the same whether you or the model loaded
  the skill, and invoking one a second time answers that it is already loaded
  instead of paying for the body again. The flag `injectInvokedSkill` on
  `turn/steer` and `session/context/inject` now decides whether that
  injection happens, and the terminal client stops shipping the skill body as
  a `skill://` resource block: the server is the one place that resolves an
  invocation.

- Ship the two builtin skills. `vibe` and `skill-creator` are seeded into
  every catalog ahead of the disk walk, exactly as the reference seeds them:
  `skill-creator` is user-invocable from `/skill-creator` and guides creating,
  editing and deleting skills; `vibe` is model-only, so the model can load the
  CLI's own reference while `/vibe` stays an ordinary prompt. Both publish
  `source: "builtin"` on `skills/list` with no path, their names are reserved
  so a disk skill cannot shadow them, and the banner keeps counting only the
  skills you added. Their bodies are this port's own prose: `NOTICE` forbids
  shipping the reference's, and the parity replay fails if either ever matches
  the upstream text.

- Read skills from the directories the documentation names. Discovery walked
  one project directory and one path this port invented; it now walks the five
  the reference does, in its order: every directory `skill_paths` names, then
  each trusted project root's `.vibe/skills` and `.agents/skills`, then
  `~/.vibe/skills` and `~/.agents/skills`. Roots are resolved before they are
  compared, so two spellings of one directory are walked once, and the first
  root holding a name wins. `~/.vibe/extensions/skills` is deprecated: it is
  still read, ranked last, so nothing you already installed stops loading, and
  a skill of the same name in either documented user directory now wins over
  it. An untrusted workspace still contributes no directory at all.

- Make the three skill configuration keys do something. `skill_paths`,
  `enabled_skills` and `disabled_skills` were declared, published and read by
  nobody. Each entry of `skill_paths` now adds a directory, ahead of every
  other root, with `~` expanded and a relative entry anchored on the working
  directory; an entry that names nothing walkable is skipped and the remaining
  directories are still read. `enabled_skills` publishes only what it matches
  and `disabled_skills` is not consulted while it holds an entry; both take the
  glob and `re:` patterns the tool filters already take, and a pattern that
  does not compile matches nothing rather than failing the catalog. The filter
  runs where the catalog is built, so the `skill` tool and `skills/list` cannot
  disagree about what exists.

- Say why a skill did not load. A `SKILL.md` that will not parse used to
  disappear silently. It is now an issue naming the file and the reason on
  `diagnostics/list`, three malformed files are three issues rather than one
  aborted walk, and asking the `skill` tool for a name that failed to load is
  answered with the file and the reason instead of a bare "not found". A
  directory that ships no `SKILL.md` is still not an error.

- Read skill frontmatter as YAML. The line-by-line reader that split on the
  first colon is replaced by a real YAML parse behind the reference's own
  boundary rules, so a nested `metadata:` block, a folded description, a
  block sequence and a fence of more than three hyphens all load exactly as
  they load upstream, and `user-invocable: no` finally means no. Validation
  now enforces the reference's schema too: names are lowercase hyphenated
  words of at most 64 characters, a description of 1 to 1024 characters is
  mandatory, both hyphenated aliases are accepted, and `allowed-tools`
  coerces from a space-delimited string. A skill accepted here is a skill
  accepted upstream, and vice versa, measured over the captured corpus.

- Carry the whole skill model. A skill's `license`, `compatibility`,
  `metadata` mapping and `allowed-tools` list now survive loading instead of
  being dropped, the recorded path is the resolved absolute one, and the
  catalog's `source` vocabulary covers all three published values so a
  `skills/list` client can group by origin the way the protocol documents.

- Warn the agent before the window closes. With `context_warnings` enabled, a
  session that has consumed half of `auto_compact_threshold` now tells the model
  so, once, naming the share used, the current token count and the window. The
  agent can wrap up or record what has to survive instead of discovering the
  wall, and the warning arms again after a compaction replaces the context it
  measured. Leaving the key off registers nothing, so nothing is injected.

- Keep a compacted session recognizable. A compaction now mints the identifier
  the reference does: UUID-shaped, with a fresh head and the previous
  identifier's trailing segment preserved, so every session a conversation
  leaves behind reads as the same conversation. A compacted session records the
  one it summarized as its parent; clearing the context records none, because
  what it would point at was discarded. Nothing on disk is renamed and every
  identifier a session ever wore still resolves.

- Summarize a compaction the way the reference does. The summarizer that made
  one blind call and gave up on an empty answer is replaced: the request comes
  from `compaction_prompt_id`, resolved from a `.md` file in your project's
  `.vibe/prompts`, then in `{vibe_home}/prompts`, then from the shipped prompt,
  and it rides your conversation's own token prefix with the live tools
  attached. A model that answers with a tool call instead of a summary is now a
  named failure rather than a generic one, and either failure gets one dedicated
  second attempt with its own prompt, no tools and the conversation rendered as
  a transcript. A summarization that is itself too large to send sheds its
  oldest round and retries, up to three times, rather than failing on the very
  condition compaction exists to solve. Outside strict mode a conversation whose
  summarization failed twice is still compacted, under a placeholder, and the
  reason it degraded from is reported; with `raise_on_compaction_failure` it
  fails instead, and no attempt is silently swallowed. Every call a compaction
  makes now counts against the token and price ceilings you set, so a ceiling
  covers every request the tool makes rather than only the ones you asked for.
  A compaction entry finally carries its declared details on the wire:
  `currentContextTokens` and `threshold` while it runs, then `summaryLength`,
  `oldSessionId` and `newSessionId` when it lands, so an editor can render the
  progress instead of only the aftermath.

- Compact before the wall instead of after it. A conversation whose context
  reaches `auto_compact_threshold` is now compacted before the request that
  would have overflowed, so a long session no longer runs into a provider
  refusal the configuration was meant to prevent. The compaction keys the schema
  advertises are read for the first time and carried on the session: the
  threshold fires the compaction, `compaction_model` sends the summarization to
  the model you chose, and `raise_on_compaction_failure` makes an overflow fail
  the turn loudly instead of being repaired silently. An overflow is now recovered from at most
  once per turn rather than compacting without bound. A compaction also reports
  itself while it runs: an entry appears when it starts and is patched in place
  when it ends, carrying the session handoff, where a client used to see nothing
  until it was over. The context window a client renders against is fixed at the
  same time: it was published as zero for every real configuration, because it
  looked the active model up in a shape a merged configuration never carries.

- Pick up a model response that stopped in the middle. `/retry` submits a
  continuation that asks the model to resume where the stream broke off without
  repeating what it already wrote, and takes optional instructions to steer the
  rest. `/whoami` reports the account this build can see and says plainly that
  the signed-in name, workspace and organization are not available yet, rather
  than showing nothing.
- Track the reference at its current release. The parity pin moves from 2.23.3
  to 2.24.0 and every committed corpus was recaptured from it in the same
  change, including the chat-input traces that had been sitting two releases
  behind. Recapturing them is now mechanically enforced: a corpus captured from
  any other revision fails the suite instead of quietly certifying old behavior.
- Preserve your own words through a compaction. The pure half of compaction
  lands: the token arithmetic, the middle truncation, the summary parser, the
  oldest-round trimmer and the envelope that carries the last 20 000 tokens of
  your messages across a compaction and reads them back on the next one. It is
  measured against the reference by a committed corpus of 74 scenarios, and the
  envelope reproduces its structure exactly while wording its own prose.

- Rewind to a point in the conversation rather than to a position in a list.
  `session/rewind` and `session/rewind/read` now take the identity of the
  history entry you picked, so a rewind after a compaction lands where you
  pointed instead of on whichever turn happens to sit at that index now. Asking
  about a point answers whether it would change files and which ones, straight
  from the session's checkpoint log, and the rewind itself reports the paths it
  restored and one entry per path it could not write, keeping the rest. A
  history clear or a compaction empties the checkpoint log with the message list
  it is numbered against, and reopens a turn that was still running so the tools
  it has left keep being recorded. The `/rewind` picker lists the saved
  conversation and no longer takes a message count.
- Stop dropping file history behind your back. A session used to keep its last
  64 checkpoints and silently discard the ones before, which retired changes a
  review panel was still showing. Nothing is discarded now: the log keeps every
  turn it recorded, and if a session ever accumulates more than 512 MiB of
  tracked file content it stops capturing new changes and says so on
  `diagnostics/list` rather than quietly forgetting old ones. The files
  themselves are written either way.
- Review what the agent actually changed. The six `review/*` methods now answer
  from the session's checkpoint engine instead of from a stub production never
  wrote to, so `review/state` lists the real changed files with a hunk per
  change, each carrying the turn or the hand edit that produced it, the earlier
  hunks it was built on and the decision in force. All seven decision
  granularities are honored, from one hunk to every file, `review/turnDiff`
  answers the owner it is asked about, and `review/hunks` locates each pending
  change in the rendered diff so an inline control can be pinned to it. An
  approval leaves disk alone, a revert is written back immediately, and a write
  that does not land rolls the decision back rather than recording a decision
  against a file that did not change.
- Run a managed shell session under a real terminal. A program that checks
  whether it is attached to one now finds it is, a control key sent through
  `<family>_stdin` reaches the foreground program instead of landing in a pipe,
  and a command stopped by its hard timeout takes its whole process group with
  it, including a child that outlived its parent. A session reports which
  terminal backend it got, and a host that provides none runs it on pipes and
  says so rather than refusing to start it.
- Keep a shell session findable after the client restarts. Every session writes
  a manifest beside its log, and one left running by a previous process is
  listed, read and inspected as `orphaned` instead of disappearing with the
  process that started it. A reset clears the logs and manifests together only
  when it is asked to, a manifest that cannot be read is skipped without hiding
  the sessions beside it, and an output window that lands inside a multi-byte
  character is adjusted rather than answered with a replacement character.
- Answer the completions MCP servers ask for. A server entry with sampling
  enabled now advertises the capability when the session initializes, and its
  `sampling/createMessage` requests are answered by the model the turn itself
  runs on, over stdio and streamable HTTP alike. An entry with sampling disabled
  advertises nothing and refuses the request, and a backend failure comes back as
  a structured error rather than as half an answer.

- Return the matches the Python client returns. `grep` now runs on the libraries
  ripgrep is built from instead of walking the tree with a plain regex: a
  lowercase pattern matches case-insensitively and one carrying a capital does
  not, `.gitignore` and `.ignore` are honored inside a repository unless the call
  turns them off, the 23 configured exclusion globs and every `.vibeignore` line
  apply either way, and a binary file is walked past rather than matched. The
  answer reports the match list, the count that survived the cap and whether the
  cap or the byte budget cut it short, with the matches ordered by path and line
  so the same query gives the same answer twice.
- Answer a tool call the way the Python client answers one. `read_file`,
  `write_file`, `edit`, `grep` and `todo` now publish the reference result fields
  and reach the model through the same field-per-line rendering, so a prompt
  tuned against one client reads the same result from the other. `read_file`
  numbers its lines the same way, tells an empty file from an offset past the
  last line, and reports the requested offset and limit it was given.
- Inject a subdirectory's `AGENTS.md` when a file under it is read. The
  instructions of every directory between the file and the workspace root reach
  the model once per directory per session, and a progress line names the files
  as they are discovered.
- Keep a file's encoding and line endings through an edit. A CRLF file written in
  a single-byte codec is written back as it was found rather than rewritten as
  UTF-8 with LF, and two edits of one file now serialize instead of one silently
  overwriting the other. An edit whose `old_string` is empty, equals
  `new_string`, is absent from the file, or matches more than once without
  `replace_all` each fails naming its own cause, and `write_file` refuses an
  existing file both before and during the write.
- Acknowledge a skill already loaded instead of sending it twice. A second
  request for the same skill in one conversation answers with a short
  acknowledgment carrying the skill directory rather than the whole body, and the
  file list a first load advertises now walks the skill directory recursively.
- Send `web_search` the request metadata and the user agent the Python client
  sends, and fail a response carrying no text instead of returning an empty
  answer.

- Guard shell commands the way the Python client guards them. Commands are now
  extracted with the same bash grammar, so `python3 <<'EOF'` with a body is
  recognized as a heredoc instead of being refused as a bare interpreter, and
  each segment of a chain is judged on its own. The policy comes from the four
  configurable lists rather than from a hardcoded table: nothing is refused
  outright unless a denylist entry matches it, so `rm -rf build/`, `dd`, `mkfs`,
  `shutdown`, `eval` and `git reset --hard` now reach an approval prompt instead
  of a flat refusal, while `vim`, `nano`, `emacs`, `tmux`, `screen`, `gdb`,
  `passwd` and a bare `bash`, `sh`, `python3` or `su` are refused as they are
  upstream. Emptying the allowlist asks once per segment rather than making the
  tool unusable.
- Ask before a shell command reads or writes outside the workspace. The path
  operands of every command the reference inspects are resolved against the
  working directory, with `~` expanded and `..` folded first, so `grep secret
  /etc/passwd`, `cat ~/.ssh/id_rsa` and `cat ../elsewhere/secret.txt` raise an
  outside-directory approval naming the directory they reach, while the session
  scratchpad, an ascent that lands back inside and a file the call is about to
  create inside the workspace raise nothing. Two operands in one directory are
  one approval. A `find` asked to run a program is approved under the whole
  segment, whatever the allowlist says about `find`.
- Speak the permission vocabulary the Python client speaks. An approval request
  now names one of the four reference scopes, `command_pattern`,
  `outside_directory`, `file_pattern` and `url_pattern`, and carries the
  invocation pattern it applies to, the pattern a session-wide approval would
  cover and a label, instead of the six unrelated kinds this client used to
  invent. An approval granted in one client therefore means the same thing in
  the other. An approval stored under the retired vocabulary is dropped rather
  than reinterpreted, so a resumed session asks again instead of granting or
  refusing something it no longer understands.
- Stop asking once per argument list. An approval is stored under the pattern
  the reference arity table derives, so approving `git status` covers a later
  `git status --short`, approving `npm run build` covers `npm run build --watch`,
  and a chain whose segments reduce to the same pattern is asked about once
  rather than per command. A command the table does not know falls back to its
  first word. The table is replayed against a capture of the reference's own,
  entry by entry.
- Keep a permanent approval past the session. Choosing "always allow" now writes
  the granted patterns into `tools.<name>.allowlist`, which is where the Python
  client writes them and where both read them back, so the next session starts
  already covered. A session approval still dies with the session, and a
  configuration file that refuses the write leaves the approval in place for the
  session and reports why.
- Resolve a file tool's permission the way the reference resolves it. The
  session scratchpad is reached without consulting any list, the denylist is
  read before the allowlist, a name matching `sensitive_patterns` raises a
  requirement even where the tool is configured to always, and a path leaving
  the workspace names the directory it would touch. A path that resolves nowhere
  is treated as outside the workspace rather than inside it, so the guard asks
  rather than assuming.
- Read the per-tool configuration an operator writes. `[tools.grep]
  default_max_matches = 500`, `[tools.read_file] max_read_bytes`, `[tools.todo]
  max_todos`, the `web_fetch` and `web_search` timeouts, the shell budgets and
  the four permission lists every tool carries are now resolved from the layered
  configuration at each call rather than compiled in: the 26 tool classes the
  reference declares, the 22 keys they draw from and all 146 `(tool, key)` pairs
  have a live reader, replayed against a committed corpus captured from the
  pinned reference. A settings screen reads the same table, since the discovered
  configuration layer now publishes every declared tool with the reference key
  names instead of four tools with invented ones.
- Obey a configuration change between two turns. The tool families are published
  against a resolver rather than a snapshot, so raising a budget or moving a
  permission applies to the next call without the session being restarted,
  whether a client patched the file or an operator edited it by hand.
- Refuse a mistyped setting without refusing the session. A value of the wrong
  type, or a limit set to zero or below, falls back to the shipped default and
  leaves a diagnostic naming the tool, the key and the replacement, in one line
  a narrow terminal renders whole.
- Guard what the Python client guards. A tool's configured `permission`,
  `allowlist`, `denylist` and `sensitive_patterns` now decide alongside the
  trust roots: reading a `.env` file asks even though `read_file` is configured
  to always, `vim`, `nano`, `tmux`, `screen`, `gdb`, `passwd` and a bare
  `python`, `bash`, `sh` or `su` are refused as the reference refuses them, and
  a session approval carrying no pattern grants the tool for the session instead
  of asking again on the next call. An allowlisted reader pointed outside the
  workspace still reaches the operator: every allowlisted command has its path
  operands inspected first.
- Ask about `sudo` instead of refusing it. The reference carries `sudo` as a
  shell sensitive pattern rather than a denylist entry, so a command starting
  with it now reaches an approval prompt where this client used to refuse it
  outright. It is still never granted automatically, whatever the allowlist
  says. The other outright denials this client adds on its own, `rm`, `dd`,
  `mkfs`, `shutdown` and `eval`, are unchanged for now.
- Accept the argument spellings the reference accepts. A model that sends
  `"replace_all": "yes"` or `"max_matches": "50"` used to have the call refused
  here and honored by the Python client, because the reference builds its
  arguments through Pydantic in lax mode and this port validated the raw JSON.
  Scalars are now coerced before validation exactly as the reference coerces
  them: a boolean accepts `yes`, `no`, `on`, `off`, `true`, `false`, `t`, `f`,
  `y`, `n`, `1` and `0` in any case, plus the numbers `0` and `1`; an integer
  accepts an integral string, a whole float and a boolean; a string accepts
  neither a number nor a boolean, because the reference coerces neither. The
  handler reads the coerced value, so a prompt tuned against one client now
  works against the other. The 92 committed argument fixtures all return the
  reference verdict, where two of them used to diverge.
- Measure what the tools do, not only what they declare. A new execution oracle
  drives `read_file`, `grep`, `write_file`, `edit` and `todo` over a fixture
  tree checked into the repository, records what the reference returns for each
  of 41 cases and replays it against this build on every `cargo test`, with no
  reference checkout needed. 12 cases match today; each of the other 29 is held
  in a ledger naming the story that closes it, and both an unlisted divergence
  and a ledger entry that has gone stale fail the suite. The instrument
  immediately found that an `edit` whose `old_string` equals its `new_string` is
  refused upstream and silently rewrites the file here.
- Link a repository to a Vibe Code project without opening a session. The nine
  `projectLinks/*` calls now answer: a path resolves to its repository root with
  the repository's name and its current and default branches, or is reported
  ineligible with the reason it failed, whether that is not being a repository,
  publishing no usable remote, having no commit yet or being unresolvable.
  Inspecting a root reports the link saved against it and drops one that names
  another repository, saying so rather than staying silent. The picker pages
  candidates and recommends the same project the terminal would: the currently
  linked one first, then single-repository matches, then multi-repository ones.
  Creating, linking and saving persist to the same store `vibeCode/projects/*`
  already writes, so a link made through either is the one the other sees;
  saving refuses when the remote moved since the caller last read it. Unlinking
  succeeds on a root that carries no link, and still removes the link of a
  checkout that has moved or been deleted. A missing credential is answered as
  an authorization failure and a reachable backend that fails as an internal
  one, so a client knows whether to prompt for sign-in or to retry.
- Accept `telemetry/record`. A client can record its own event against a
  session, and the call is honored only while `enable_telemetry` is on; the
  event is kept where `diagnostics/logs/read` reports it rather than shipped,
  because this port's telemetry envelope deliberately differs from the
  protocol's. A field the protocol does not declare is refused with the pointer
  to it.
- Let a client host the agent's file access and its terminals. A client that
  declares `filesystem/read`, `filesystem/write` or `terminal` during the
  handshake is now asked to answer for them: `read_file` and `edit` read the
  buffer the editor holds rather than the file last saved, `write_file` and
  `edit` write back into it, and a command runs on the client's terminal where
  the user can watch it. A client that declares nothing keeps every tool on this
  host, unchanged. Hosting the filesystem is not a way around the workspace
  boundary: a path that escapes the root is refused before it is delegated, and
  what travels is the absolute path the client can resolve. A delegation the
  client leaves unanswered fails the tool naming the call rather than holding
  the turn open, a malformed answer names the field it is missing, and a
  terminal is always released, whether its command finished, its turn was
  interrupted or its session closed.
- Report what a session is actually running. `runtime/read` used to answer with
  an empty configuration, no agents, no skills, zeroed statistics and a context
  window of zero whatever the session was doing; it now carries the same agent
  and skill catalogs `agents/list` and `skills/list` publish, the agent the
  session runs, its live token accounting, the active model's threshold, the
  number of registered hooks and its real logging state. `stats/read` answers
  with the same snapshot, and `account/read` classifies the configured
  credential instead of always reporting a missing key.
- Answer the configuration calls in the shapes the protocol declares.
  `config/read` carries the published configuration view and the base it was
  composed from, `config/reload` and `config/thinking/write` carry the runtime
  the write produced, and `config/patch` carries what it rejected, what failed
  to land and the runtime afterward. The port's own `{snapshot}` envelope,
  which carried every layer and every effective key, leaves the wire; the
  settings screen reads the configuration in-process and every other caller
  reads the published field surface. `config/batchWrite` now takes the
  fingerprint it compares against inside its own transaction, so a caller no
  longer reads one a call earlier to send it back.
- Publish every source a session can call a tool through in one list. MCP
  servers and connectors now share `mcp/read`, separated by `kind`, each with
  the six-value status the protocol declares: a source the operator switched off
  reads as disabled rather than broken, and one that would not start carries its
  reason under its own name in `discoveryErrors`. `connectors/read` answers with
  the counts alone, `connectors/refresh` with the runtime and the tool count it
  produced, and `mcp/add` with whether it created the source, its name, its URL
  and the runtime.
- Trim the remaining answers to what the protocol declares. `agents/list` names
  the agent the addressed session runs alongside the catalog, `skills/list`
  publishes the summaries without the discovery issues that now travel on the
  runtime, `tools/list` publishes tool names, and `session/list` publishes the
  page. A published session names its model and its agent profile rather than
  reporting both as null.
- Keep `session/settings/update` to the two turn budgets it declares. Changing a
  session's model, mode, thinking level, reasoning effort or approval stance now
  goes through `session/overrides/write`, which none of them was ever part of
  the protocol as: upstream a model and a thinking level are configuration
  writes and a mode and an approval stance come from an agent profile. The
  reference method answers a call carrying any of the five with `invalid_params`
  like any other field it does not declare, and the new name is never advertised
  to a client that did not already ask for it.

- Publish a tool call as the effect it is rather than as an untyped blob. Every
  history entry now carries one of the twelve declared effect kinds with the
  input its kind describes and the presentation a client renders it with, so a
  shell command, a file edit and a subagent run reach every client as three
  different things instead of one generic tool call. The port's own
  `toolCallId` and raw `arguments` leave the wire; a settled effect carries the
  result header it finished with, and only a cancellation that produced nothing
  carries none. A transcript written by either implementation now renders the
  same way in both.
- Publish a notice under one of the eight declared kinds rather than an
  invented one. A finished hook names itself and what it reported, a title
  change names the title, a plan review names the file it opened, and a cleared
  context names the plan whose acceptance cleared it. A failed turn no longer
  appends a notice under a kind the protocol does not declare, since the
  failure already reaches a client through the completed turn and the session
  status.
- Answer a callback in the two forms the protocol declares. An approval carries
  the typed effect it is gating, its required permissions and the decisions on
  offer; a question carries the request a client renders. An answer whose type
  is not the open callback's is refused with `invalid_params` rather than
  recorded. A plan review is published as its own notice entry carrying the plan
  file, which is where a client reads it from now that the callback carries only
  the fields the protocol declares.
- Push session status instead of making a client poll for it. Every server-side
  transition now publishes `session/updated` with a JSON patch replacing
  `/status` and `/updatedAt`: a running turn names its turn id, a blocked one
  names the callback and the kind of answer it wants, and a failed one carries
  the message it failed with. A client attaching to a session is handed the
  whole state as `session/snapshot`, whose embedded watermark equals the
  notification's own, followed by any callback still open on that session, so it
  can answer a question raised before it arrived. The per-session `eventId`
  sequence is contiguous throughout, which is what a reference client's
  projection requires of it.
- Publish token accounting as it accumulates. `session/statsUpdated` carries the
  whole seventeen-field snapshot the protocol declares, including the cached
  token counts this port never reported, plus the active model's compaction
  threshold as `contextWindow`. A published session now reports its real
  `tokenUsage` rather than null.
- Clear the planning context inside the turn that accepted the plan. Choosing
  "Yes, clear context and auto approve edits" now drops the transcript, rotates
  the session onto a fresh identifier and continues from the approval message
  alone, publishing `session/contextCleared` with both identifiers, the new
  state and the plan file the acceptance came from. The clearing used to run
  between two turns as a plain history reset, which changed the transcript
  without telling any client that it had. Each clearing writes a new session
  file, as a compaction already did.
- Name why a turn failed in the vocabulary the protocol declares. A failing
  turn now carries one of the nine reference codes, classified from the
  failure's type rather than from the text it rendered to: a 429 is
  `rate_limit`, an overflowing context `context_too_long`, a refused answer
  `refusal`, a failed compaction `compaction_failed`. The port used to answer
  `turn_failed` or `provider_refusal`, neither of which a client written
  against the protocol could branch on.
- Report a retried provider request. `turn/retrying` is emitted while the
  backend is still waiting, naming the status or connection failure that caused
  the wait, so a stalling turn can be explained rather than merely looking slow.
- Retire four notification names this port had invented. `mcp/updated`,
  `connectors/updated`, `shell/updated` and `workspace/trust/updated` are gone;
  a mutation that moves runtime state now publishes `runtime/updated` with the
  full runtime snapshot, a recoverable problem publishes `warning`, an MCP
  source waiting on authorization publishes `mcp/authUrl`, and a failure that
  ends a connection publishes `error` before it closes. A client written against
  the reference protocol no longer has to learn a name only this port spoke.

- Complete the handshake with a client that mutes notifications. `initialize`
  accepts `capabilities.disabledNotifications`, which the reference client
  library declares and which this port used to answer with `invalid_params`,
  leaving the connection dead on its first frame. The server honors the list
  for every notification except a sequenced event: those carry the per-session
  `eventId` a client counts, so silencing one would open a gap it reads as a
  fault. Muting a name consumes no event id, so the sequence stays contiguous
  either way. A capability the protocol does not declare is still rejected.
- Name the offending value when a request is rejected. Every `invalid_params`
  answer now carries `data` with `errorCount` and an `issues` array. An issue
  raised while deserializing carries the `path` to the value that failed, as
  field names and array indices rather than a flattened string; one raised by a
  dispatcher's own check reports at the parameter object. A rejection under any
  other code leaves `data` off the wire.
- Advertise the reference method inventory. `initialize` reports the methods
  this build routes from the reference contract and no longer offers
  `config/batchWrite`, `connectors/toggle` or `mcp/auth/complete`, the three
  names only this implementation answers. All three stay routable for the
  clients already calling them and are recorded in the accepted divergences of
  `docs/parity.md`.
- Publish the settings a tool declares as a configuration layer. `web_fetch`'s
  content and redirect limits, `web_search`'s and `web_fetch`'s timeouts and
  `todo`'s cap now appear under `tools` in `config/read`, in a `discovered`
  layer sitting above the shipped defaults and below every file you own, so
  writing `[tools.web_fetch] maxRedirects = 1` wins over the declaration in the
  effective document and leaves the rest of the entry standing. The tools
  themselves still run on the limits they declare; the layer is what makes those
  limits visible and addressable, and each tool reads its entry when the feature
  behind that option lands. A tool that does not register declares nothing, and a
  discovery pass that cannot run leaves the layer empty with the reason readable
  in the configuration's validation warnings rather than failing the load.
- Accept every MCP entry the reference accepts. `transport = "http"` loads and
  round-trips beside `streamable-http`, `/mcp add --transport http` is no longer
  refused, and a stdio `command` may be a list or a quoted string that is split
  the way a shell splits it, so `command = "npx -y @scope/server"` launches the
  program it names instead of looking for a file with spaces in its name. An
  entry's `prompt` now reaches the model as a hint on every tool the server
  publishes, and `sampling_enabled`, `disabled` and `disabled_tools` carry the
  reference defaults.
- Authenticate an MCP server through an `[auth]` block. A static block declares
  headers, the environment variable holding the token, the header it rides in
  and its format; the token is read when the request is made, never persisted,
  and an explicit header of the same name wins. An OAuth block declares the
  scopes to request, a pre-registered client id or a client-metadata document
  URL, and the loopback port the callback binds, and the login uses them. The
  legacy top-level `headers`, `api_key_env`, `api_key_header` and
  `api_key_format` keys keep working: they are promoted into a static block, and
  mixing them with an explicit block is refused.
- Reject an MCP server URL the way the reference rejects it, and add one under
  the same name. Credentials, a fragment, a missing scheme or host, a scheme
  other than HTTP or HTTPS, and plaintext HTTP to anything but this machine are
  all refused without echoing the URL. Two spellings of one endpoint, differing
  only by case, a default port or a trailing slash, are recognized as the same
  server and the rejection names the entry that already holds it. An add with no
  name derives one from the host, dropping a leading `mcp` or `www` label and
  falling back to the first usable path segment, then numbering it until it is
  free; a name you asked for is never renamed behind your back.
- Remove a configured MCP server with `vibe mcp remove <name>`. The entry
  disappears from the file writes land in, and a name nothing carries is
  reported as such rather than failing.
- Find the project configuration from anywhere inside a repository. The walk
  starts at the working directory and climbs one parent at a time, taking the
  nearest `.vibe/config.toml` and stopping before the directory that holds the
  vibe home, so opening the agent three levels down no longer silently drops the
  repository's configuration. A discovered file in an untrusted directory is
  still ignored, and a write with nothing discovered still lands in the working
  directory's own file.
- Resolve which configuration sources a session reads and writes. Both the user
  file and the project file are open by default; a session that opens only the
  project source refuses a user write, and one that resolves to neither keeps
  its selection in memory, where writes succeed without reaching disk.
  Directories opened alongside the working directory are absolutized and
  deduplicated, and each one is a project root of its own: its `.vibe` extension
  directories, commands and `hooks.toml` are discovered, ahead of the
  user-level file.
- Read `~/.vibe/.env` at startup. An API key kept there is now visible to this
  binary and to the `VIBE_*` configuration layer, with an exported value winning
  over the file, an empty value ignored, a FIFO accepted in place of a regular
  file, and a malformed line skipped without echoing anything it carried. Unlike
  the reference the process environment itself is not mutated, which Rust
  forbids here; every reader resolves through the file instead.
- Bring an older configuration file forward at startup. The bash allowlist gains
  `find` and loses trailing wildcards, the default read-only commands are
  unioned in once and recorded in `applied_migrations`, a `devstral-2` model is
  renamed to `mistral-medium-3.5` with its pricing and thinking level and any
  `active_model` pointing at it is repointed, and settings under the old `read`
  and `search_replace` tool names move to `read_file` and `edit` without
  clobbering a key you already configured. An untrusted or unwritable file is
  left alone and the load still succeeds, with the reason readable in the
  configuration's validation warnings.
- Write configuration through `config/patch`. A client addresses a field by JSON
  Pointer, with `set` and `remove`, and the server decides which file backs it:
  an operation naming no `targetLayer` lands in the file the current selection
  resolves to. A change that would leave an invalid configuration is rejected
  whole and leaves every file byte-identical, while a target whose write fails
  after that is reported on its own so the target that succeeded stands. Beyond
  the reference response, the answer carries the `changedKeys` the write moved.
  `config/batchWrite` still dispatches and now routes through the same core.
- Describe the settings surface through `config/fields/read`. Every published
  field arrives with its editor kind, description, effective value, JSON Pointer,
  popular flag, choices and per-layer values ordered from the highest-priority
  layer down to the shipped defaults, together with the configuration files a
  write can be routed to. Per-tool settings stay out, as they do upstream, and a
  field whose name is sensitive is redacted in its value and in every layer.
- Publish configuration changes to in-process subscribers, filtered by key. A
  subscription on `models` hears about `models/active` and the reverse, a write
  that changes nothing publishes nothing, and one failing subscriber never
  silences the others. Published documents are redacted.
- Start with the reference default configuration instead of an empty one. Every
  key the reference declares now has its upstream default at every construction
  site, so behavior you never configured matches between the two clients: the
  two providers, the three models, the transcription and speech pairs, and every
  scalar. Four defaults changed as a result. `theme` defaults to `auto` rather
  than `system`, which resolves identically. `show_thinking_nodes` defaults to
  off, so reasoning regions are hidden until you turn them on.
  `autocopy_to_clipboard` defaults to on. `active_model` defaults to
  `mistral-medium-3.5` instead of being unset. A value you already wrote still
  wins over all of them.
- Publish all 64 reference configuration keys through `config/schema`, up from
  15, so the settings screen renders the full surface with its types, defaults,
  choices and descriptions. The response now also carries a
  `configSchemaVersion` token, so a client can cache the surface. The five keys
  this port declares without an upstream counterpart (`thinking`,
  `notifications`, `proxy`, `tls_ca_path`, `dotenv_path`) keep working under
  their own names; they are recorded as divergences rather than mapped onto a
  reference field, so nothing already on disk is reinterpreted.
- Read models the way the reference does. A persisted `[[models]]` list is read
  back keyed by alias, so overriding one model's temperature no longer erases
  the others, and an entry that names neither an alias nor a name fails the load
  naming the field. An entry that omits `name` or `provider` is completed from
  the default model it overrides, a model that sets no compaction threshold
  inherits the global `auto_compact_threshold`, and an `active_model` naming
  nothing configured selects the first configured model and records a readable
  warning instead of failing. A configuration that ends up with no model at all
  fails the load. Writes still persist the `[[models]]` list form.
- Merge each configuration key by the strategy the reference declares for it
  instead of replacing every list. A denylist now extends the one a lower layer
  set rather than erasing it, which affects `disabled_tools`, `tool_paths`,
  `agent_paths`, `skill_paths`, `enabled_agents`, `disabled_agents`,
  `installed_agents`, `enabled_skills`, `disabled_skills` and
  `applied_migrations`. A tool a lower layer disabled therefore stays disabled
  when a higher layer disables another one; if you relied on a higher layer
  replacing the list, move the entries you want to keep into that layer.
  `enabled_tools` still replaces, as it does upstream. The provider, connector,
  MCP server, transcribe-model and TTS-model lists merge entry by entry, keyed by
  `name` or `alias`, so a higher layer redefining one entry no longer drops the
  others, and when two layers do combine such a list, an entry missing that key
  fails the load naming the field and the key rather than being silently kept.
- Type `VIBE_*` environment overrides by the field they target. A boolean field
  accepts the usual true and false spellings and rejects anything else, a
  numeric field rejects text, a string field keeps its value verbatim rather
  than parsing it as a TOML literal, and a list field is read as JSON. A
  rejected value fails the load naming the variable and the field without
  echoing the value. Empty values are still ignored and `__` still maps to
  nesting.
- Read `enabled_tools` and `disabled_tools` from `config.toml` and match both
  lists the way the reference does: entries are shell globs (`serena_*`), or
  regular expressions when prefixed with `re:`, and both forms ignore case. An
  allowlist narrows the surface and the denylist is applied last, so a name both
  lists match is withheld. `--enabled-tools` replaces the configured allowlist
  and `--disabled-tools` adds to the configured denylist. An entry that is not a
  valid regular expression is reported on `diagnostics/list` and ignored instead
  of failing the session.
- Withhold a tool whose runtime prerequisite is missing rather than publishing
  it and failing at call time, re-checking the prerequisite at every
  publication: a Windows shell family whose interpreter is uninstalled while a
  session runs leaves the surface at the next turn. Withheld tools are named on
  `diagnostics/list`.
- Publish the Windows shell families. On Windows, under
  `managed_shell_tools_enabled`, `git_bash`, `git_bash_output`,
  `git_bash_stdin`, `git_bash_sessions` and `git_bash_log_file` appear when a
  Git Bash is installed, and the matching `powershell_*` names appear when
  PowerShell is installed and Git Bash is not. Each family drives its own
  shell, mints its session ids under its own prefix, forces the reference
  interactivity and pager variables into the child, and decodes UTF-16 console
  output as text. A POSIX host publishes none of them, and a Windows host with
  neither shell publishes no shell tool at all.
- Add `bash`, which runs a shell command in the working directory under the
  existing shell policy: a command the analysis permits outright runs, anything
  else waits for approval, and a destructive one is refused. Output is bounded
  and reports its own truncation, a non-zero exit carries the status and both
  streams, and a command that times out or whose turn is cancelled has its
  process group terminated.
- Add the managed shell session family behind `managed_shell_tools_enabled`,
  the field the reference experiment writes: `bash` gains
  `background`, `cwd`, `env`, `shell` and the two timeout controls, and
  `bash_output`, `bash_stdin`, `bash_sessions` and `bash_log_file` poll, feed,
  list and read the sessions it leaves running. A call that overrides the
  working directory, the shell or the environment waits for approval whatever
  the command is, and the request names the override. Sessions stop when the
  Vibe session closes.
- Agent profiles naming `bash` now resolve against the published `bash` tool
  rather than the manual shell surface.
- Take the reference argument shape on `task`: `task` is required, `agent`
  defaults to `explore` instead of enumerating the discovered agents, and an
  unknown agent is refused with the names that do exist.
- Publish the reference tools that need no shell in every session: `todo`
  keeps the session task list the transcript renders, `skill` loads a
  discovered skill and names the available ones when it cannot, `web_fetch`
  retrieves a page over http while refusing other schemes, bounding redirects
  at five hops and truncating a long body, and `web_search` answers from live
  results whenever a Mistral API key resolves.
- Add `write_file`, which creates a file and the parent directories it needs
  and refuses to overwrite an existing one, naming `edit` instead.
- Rename the file tools to their reference names: `read_file` replaces `read`
  and `grep` replaces `search`. `read_file` takes `file_path`, `offset` and
  `limit`; `grep` always treats its `pattern` as a regular expression and takes
  `path`, `max_matches` and `use_default_ignore`. Configuration and agent
  profiles naming the old tools no longer match anything.
- Take reference argument keys on `edit`: `file_path`, `old_string`,
  `new_string` and `replace_all` replace the previous camelCase keys.
- Publish `ask_user_question` and `exit_plan_mode` in the reference schema
  shape, with the question and choice models under `$defs` and no invented
  constraints.
- Publish MCP tools as `{alias}_{tool}` instead of `mcp_{alias}_{tool}`, and
  keep connector aliases in the case and hyphenation the reference keeps.
  Per-tool disable preferences written under the previous MCP names are
  migrated on load, and connector preferences persisted under the previously
  lowercased alias still apply. A second source claiming an already published
  name is now rejected naming both sources instead of shadowing the first.
- Publish tool argument schemas in the shape the reference emits: `required`
  and `additionalProperties` only where the tool declares them, defaults on
  optional properties, `anyOf` for nullable ones, and nested models under
  `$defs`.
- Accept and reject tool arguments the way the reference does, resolving
  `$ref`, evaluating `anyOf` and `items`, enforcing declared bounds, and
  applying defaults before a tool runs.
- Add privacy-safe, schema-versioned telemetry with an intentional divergence
  from the upstream open properties envelope.
- Add native archives, atomic checksum-verifying installers, shell
  completions, update and rollback contracts, and a composite GitHub Action.
