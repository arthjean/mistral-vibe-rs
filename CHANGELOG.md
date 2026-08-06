# Changelog

## Unreleased

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
- Publish the Windows shell families. On Windows, under the
  `VIBE_MANAGED_SHELL_TOOLS` rollout, `git_bash`, `git_bash_output`,
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
- Add the managed shell session family behind the `VIBE_MANAGED_SHELL_TOOLS`
  rollout, standing in for the reference experiment variant: `bash` gains
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
