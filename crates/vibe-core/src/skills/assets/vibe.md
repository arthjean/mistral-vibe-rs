# Vibe

This skill is the source of truth about the CLI agent you are running inside:
Mistral Vibe, version __VERSION__. Consult it before answering any question
about the tool itself, its files, its configuration, or your own behavior, and
prefer what it says over guesses drawn from other products.

Full sources and release notes for this exact build:
__REPOSITORY__/blob/v__VERSION__/README.md

## The Vibe home and its override

State lives under one directory, the Vibe home. It defaults to `~/.vibe` and
the `VIBE_HOME` environment variable relocates it wholesale; every path below
written as `~/.vibe/...` follows the override.

Layout of the home:

- `config.toml`: the user-level configuration document.
- `.env`: API keys, one `NAME=value` per line. Never read or edit this file
  yourself; when a key is missing, tell the user to add it there.
- `trusted_folders.toml`: the workspace trust decisions.
- `skills/`: user-global skills, one directory per skill holding a `SKILL.md`.
- `extensions/`: user-global agent profiles (`extensions/agents/*.toml`),
  prompts (`extensions/prompts/*.md`), commands (`extensions/commands/*.md`)
  and hooks (`extensions/hooks.toml`).
- `sessions/`: persisted session transcripts, which `/resume` browses.

A project contributes its own `.vibe/` directory with the same shape
(`.vibe/config.toml`, `.vibe/skills/`, `.vibe/agents/`, ...), read only while
the folder is trusted.

## Configuration files and precedence

Configuration is composed from, in increasing precedence:

1. Built-in defaults.
2. One selected TOML document: the project's `.vibe/config.toml` while the
   workspace is trusted, otherwise the user's `~/.vibe/config.toml`.
3. `VIBE_*` environment variables, where `VIBE_ACTIVE_MODEL=...` sets
   `active_model` and a double underscore descends into a table
   (`VIBE_SECTION__KEY`).

Edits to a TOML file are applied by `/reload` without restarting; many keys
are also re-read per use. To change configuration for the user, read the
target file first, edit it with the ordinary file tools, then suggest
`/reload`.

## Models and providers

`active_model` names the model in use; `models` declares the available entries
and `providers` the endpoints they resolve against, including a custom
`api_base` and the environment variable holding the credential. `/model`
switches the active model interactively, `/thinking` selects the thinking
level, and `compaction_model` picks the model summarization runs on.

## Agents and subagents

An agent profile is a TOML file declaring a name, a kind (`agent` or
`subagent`) and overrides. Built-in agents cover the permission styles
(default, plan, accept-edits, auto-approve) plus the opt-in `lean` agent
installed by `/leanstall`; the built-in `explore` subagent runs bounded
read-only investigations through the `task` tool. Custom profiles live under
the extension roots above. The keys `agent_paths`, `enabled_agents`,
`disabled_agents`, `installed_agents` and `default_agent` govern discovery,
filtering and selection; `--agent <name>` selects one for a run.

## Skills

A skill is a directory holding a `SKILL.md`: YAML frontmatter (`name`,
`description`, optional `user-invocable`, `allowed-tools`, `license`,
`compatibility`, `metadata`) followed by Markdown instructions. Discovery
walks, in order: `skill_paths` entries from configuration, each trusted
project root's `.vibe/skills` and `.agents/skills`, then `~/.vibe/skills` and
`~/.agents/skills`; the first directory publishing a name wins, and the
built-in skill names (`vibe`, `skill-creator`) are reserved. `enabled_skills`
and `disabled_skills` narrow the published set, the allowlist deciding alone
when present. The model loads a skill with the `skill` tool; a user-invocable
one is also reachable as `/skill-name`. A skill that fails to parse is
reported in diagnostics rather than silently dropped.

## Tools and their permission model

Built-in tools cover file reading and editing, shell execution, search, todo
tracking, web fetch and web search (the latter published only when a Mistral
key resolves), and skill loading. `enabled_tools` and `disabled_tools` filter
the surface with globs or `re:` patterns, `tool_paths` adds external tool
directories, and `[tools.<name>]` tables carry per-tool settings.

Mutating tool calls prompt for approval unless the session runs with
`--auto-approve` (alias `--yolo`), the agent profile pre-approves them, or
`bypass_tool_permissions` is set. File edits under a trusted project are
resolved against that trust decision; reads are broadly permitted, writes
prompt.

## Slash commands

`/help`, `/config`, `/model`, `/thinking`, `/reload`, `/clear` (alias
`/new`), `/copy`, `/paste-image`, `/log`, `/debug`, `/compact`, `/exit`,
`/status`, `/whoami`, `/teleport`, `/remote-project`, `/proxy-setup`,
`/resume` (alias `/continue`), `/rename`, `/mcp` (alias `/connectors`),
`/voice`, `/leanstall`, `/unleanstall`, `/rewind`, `/retry`, `/loop`,
`/data-retention`, `/theme`. A `/word` that matches no command is looked up as
a user-invocable skill, then as a custom command file.

## CLI flags

`vibe [prompt]` starts interactive mode; `-p/--prompt` runs programmatically
with `--output text|json|streaming`. Session control: `--resume [id]`,
`-c/--continue`, `--workdir <path>`, `--add-dir <path>`, `--worktree <name>`.
Trust and permissions: `--trust`, `--auto-approve`/`--yolo`,
`--enabled-tools`, `--disabled-tools`. Budgets: `--max-turns`, `--max-tokens`,
`--max-price`. Others: `--agent <name>`, `--setup`, `--check-upgrade`,
`--telemetry`, `-v/--version`.

## Hooks

`hooks.toml` under an extension root declares external programs run at
`pre_tool`, `post_tool` or `post_agent`. A pre-tool hook can observe or block
a call before it executes; hook output is reported into the session. Hooks
from every open root are collected and ordered deterministically.

## MCP servers

Remote tool servers are declared as `[[mcp_servers]]` tables in the
configuration document, each with a `name`, a `transport`
(`streamable-http` among others) and a `url`. In a session, `/mcp` lists
servers and their tools, `/mcp add <url>` registers one, `/mcp login <alias>`
and `/mcp logout <alias>` manage authentication, and `/mcp status` reports
health. `vibe mcp remove <name>` edits the user file from outside a session.

## Connectors

Connectors are Mistral-hosted integrations toggled by `enable_connectors`;
`/connectors` (the `/mcp` alias) shows them alongside MCP servers, with login
handled through the browser sign-in flow.

## Trusted folders

Opening a workspace asks for a trust decision, persisted in
`~/.vibe/trusted_folders.toml`; `--trust` grants it for the run. An untrusted
workspace contributes no project configuration, skills, agents, prompts,
commands or hooks: only user-level state applies there.

## File mentions

`@path` in the composer attaches the named file to the prompt, with
autocompletion over the workspace; `file_watcher_for_autocomplete` keeps that
index fresh.

## Logs

`/log` prints the path of the current interaction log. `session_logging`
controls what sessions persist, and `/debug` toggles the in-TUI debug
console. Session transcripts live under the home's `sessions/` directory.

## Themes

`/theme` selects the color theme interactively and the `theme` key persists
it. `disable_welcome_banner_animation` and `displayed_workdir` tune the
banner; `show_thinking_nodes` toggles thinking output in the transcript.

## Voice

`/voice` configures voice interaction. `voice_mode_enabled` turns voice input
on, `active_transcribe_model` names the transcription model,
`active_tts_model` the synthesis model, and `narrator_enabled` reads
responses aloud.

## Sensitive files

Never read, print or edit `~/.vibe/.env` or any file the user designates as a
credential store. Direct the user to edit those themselves.
