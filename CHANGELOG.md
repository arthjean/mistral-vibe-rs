# Changelog

## Unreleased

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
