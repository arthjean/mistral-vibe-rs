# Changelog

## Unreleased

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
