# Changelog

## Unreleased

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

- Record the audio lifecycle locally. A transcription session that opens, a
  recording that is cancelled, a transcription that completes and one that fails
  each record an event carrying the recording id the endpoint named, the
  accumulated transcript length and the durations involved. The events are kept
  on `diagnostics/logs/read` rather than shipped, and `enable_telemetry` decides
  whether they are kept at all.

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
