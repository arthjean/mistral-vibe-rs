---
name: create-plugin
description: Author a plugin for Mistral Vibe, an Agent Plugins 1.0 package that can bundle skills, MCP servers and, through the Vibe extension, hooks, knowledge, agents, libraries and connectors. Load it whenever the user wants to write, scaffold or fix a Vibe plugin.
---

# Writing a Vibe plugin

A plugin is one directory holding a `plugin.json` manifest in the Agent
Plugins 1.0 format and whatever component files it needs. Vibe resolves
plugins when a session runs on the unified harness.

## Where plugins live

| Scope | Directory | Read when |
|---|---|---|
| Project | `<project>/.vibe/plugins/<name>/` | the folder is trusted |
| User | `~/.vibe/plugins/<name>/` (under `VIBE_HOME`) | always |
| Built in | shipped with Vibe | always |

Every subdirectory of a plugins directory is one candidate plugin. When two
scopes hold a plugin of the same name, the project one wins over the user one,
which wins over the built-in one. Two plugins of the same name in the same
scope are both dropped, and so are plugins that end up sharing a namespace.

## Layout

```
my-plugin/
  plugin.json             required manifest
  skills/<skill>/SKILL.md skills, one directory each
  mcp.json                MCP servers
  libraries.json          Node and Python libraries (Vibe extension)
  connectors.json         managed connectors (Vibe extension)
  ai.mistral.vibe/        Vibe extension components
    hooks.toml
    knowledge/<folder>/KNOWLEDGE.md
    agents/<agent>.toml
```

Files that need the Vibe extension are ignored unless the manifest declares
it (see below).

## The manifest

```json
{
  "$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
  "name": "my-plugin",
  "version": "0.1.0",
  "description": "One sentence on what the plugin adds.",
  "author": {"name": "Your Name", "email": "you@example.com", "url": "https://example.com"},
  "homepage": "https://example.com/my-plugin",
  "repository": "https://github.com/you/my-plugin",
  "license": "Apache-2.0",
  "keywords": ["example"],
  "extensions": {}
}
```

- `$schema` and `name` are required and `$schema` must be exactly the URL
  above; a different Agent Plugins schema version is rejected as unsupported.
- `name` is 1 to 64 characters of lowercase letters, digits, `.` and `-`,
  starting and ending with a letter or digit, never containing `--` or `..`.
- Every other field is optional, and every field has to have the declared type:
  nothing is coerced, and an unknown key fails the whole manifest.
- Without a `description`, Vibe describes the plugin as
  `Capabilities provided by <name>.`

## The Vibe extension

```json
"extensions": {
  "ai.mistral.vibe": {
    "schemaVersion": 1,
    "toolNamespace": "my_plugin",
    "toolOverrides": {
      "server-id/tool_name": {"name": "friendly_name", "exposure": "direct"}
    }
  }
}
```

- `schemaVersion` must be `1`.
- `toolNamespace` is an identifier (`[A-Za-z_$][A-Za-z0-9_$]*`). Without it the
  namespace is the plugin name with every non-identifier character turned into
  `_`. `file_system`, `self`, `process`, `agent` and `vibe` are reserved.
- `toolOverrides` renames a source's tool or sets its `exposure` to
  `programmatic` (the default), `direct` or `direct_and_programmatic`. A key
  that matches no tool once its source answered is reported as unused.

## Skills

Put each skill in `skills/<skill-name>/SKILL.md`. The frontmatter `name` must
equal the directory name, and the skill is published as `<namespace>:<name>`,
so `my_plugin:summarize`. Frontmatter, `allowed-tools`,
`disable-model-invocation`, `user-invocable` and an `agents/openai.yaml` policy
beside the file work as they do for any Vibe skill; the policy file has to stay
inside the plugin.

## MCP servers

```json
{
  "$schema": "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
  "mcpServers": {
    "search": {
      "type": "stdio",
      "command": "./bin/search-server",
      "args": ["--data", "${PLUGIN_DATA}"],
      "env": {"LOG_LEVEL": "info"},
      "cwd": "${PLUGIN_ROOT}"
    },
    "docs": {
      "type": "streamable-http",
      "url": "https://docs.example.com/mcp",
      "headers": {"X-Client": "vibe"}
    }
  }
}
```

- `stdio` servers name either a bare executable found on `PATH` or a path
  starting with `./` that stays inside the plugin. `${PLUGIN_ROOT}` and
  `${PLUGIN_DATA}` expand in `args`, `env` and `cwd`, and both are also set in
  the server's environment, so `env` may not define them. `cwd` must start with
  `./`, `${PLUGIN_ROOT}` or `${PLUGIN_DATA}` and stay under that root.
- `streamable-http` servers take a `url` and optional `headers` with valid,
  distinct names. `sse` is recognized and refused.
- A server whose id matches an MCP server already in the user's configuration
  is disabled with a warning, because the two would share stored credentials.
  Rename one of them.
- Plugin servers appear in `/mcp` under their id, or under a digest-suffixed id
  when two plugins declare the same one, and cannot be toggled or removed
  there; change the plugin instead.

## Hooks

`ai.mistral.vibe/hooks.toml` uses the same `[[hooks]]` entries as a user
`hooks.toml`, read strictly: no unknown keys, at most 64 KiB, at most 128 hooks,
every name distinct. Each hook is published as `<plugin>:<name>`, runs in the
plugin directory, and sees `PLUGIN_ROOT` and `PLUGIN_DATA`.

## Knowledge

Each folder under `ai.mistral.vibe/knowledge/` is one knowledge source with a
`KNOWLEDGE.md` entrypoint:

```markdown
---
name: accounting-policy
description: Revenue recognition rules and reporting calendar.
display_name: Accounting policy
icon: 📒
---
```

`name` is kebab case and must equal the folder name; `description` holds 5 to
300 characters, `display_name` up to 100, `icon` up to 16. A plugin may publish
100 folders, the entrypoint may weigh 256 KiB, and a folder may hold only plain
files and directories, no symbolic links. Folders are copied into the plugin's
data directory and published read-only as `<namespace>:<name>`.

## Agents

Each `ai.mistral.vibe/agents/<agent-name>.toml` declares one subagent type,
published as `<namespace>:<agent-name>`; the file name is kebab case.

```toml
schema_version = 1
agent_type = "subagent"
display_name = "Researcher"
description = "Digs through the knowledge base and reports back."
safety = "safe"
instructions = "Cite the file every claim comes from."
enabled_tools = ["read_file", "grep"]

[tools.bash]
permission = "ask"
allowlist = ["git log*"]
```

`schema_version` and `agent_type` are required and fixed, `description` holds 1
to 300 characters, `safety` is `safe`, `neutral` (the default), `destructive` or
`yolo`, `instructions` holds up to 65 536 characters, and the tool lists hold up
to 128 distinct names. Without `display_name` the file name is title-cased. A
file may weigh 64 KiB and a plugin may declare 128 agents.

## Libraries

```json
{
  "schemaVersion": 1,
  "node": {"@acme/helpers": "./lib/node/helpers"},
  "python": {"acme_tools": "./lib/python/acme_tools"}
}
```

Paths start with `./`, stay inside the plugin, and contain no symbolic links.
Node aliases are npm package names; Python aliases are identifiers, and may
point at a package directory or a single `.py` file. Each language allows 128
libraries. Libraries are copied into the plugin's data directory and put on
`NODE_PATH` or `PYTHONPATH` for the commands a session runs; Node libraries need
Node.js installed. Two plugins claiming the same alias both lose it.

## Connectors

```json
{"schemaVersion": 1, "connectors": [{"id": "linear", "tools": ["search_issues"]}]}
```

Each entry names a managed connector and the tools it needs from it, 1 to 256
distinct names. A connector the account cannot use, or a tool it does not offer,
is reported and left out.

## Reading the diagnostics

A broken plugin never stops a session: it is dropped, and the reason is listed
by `/plugins` and in the session's configuration issues. The codes say where to
look: `plugin.manifest.*` and `plugin.schema.*` for the manifest,
`plugin.compatibility.*` for format detection, `plugin.namespace.reserved`,
`plugin.skill.*`, `plugin.hooks.*`, `plugin.knowledge.*`, `plugin.agent.*`,
`plugin.libraries.*` and `plugin.library.*`, `plugin.connectors.*` and
`plugin.connector.*`, `plugin.mcp.*` for servers (including
`plugin.mcp.server_shadowed`), `plugin.tool.name_collision` when two tools land
on one name, and `plugin.tool_override.unused`.

## Other plugin formats

Vibe also reads Claude Code plugins (`.claude-plugin/plugin.json`), Codex
plugins (`.codex-plugin/plugin.json`), Kimi Code plugins (`kimi.plugin.json` or
`.kimi-plugin/plugin.json`) and the declarative parts of OpenCode packages,
translating what it can and listing the rest as unsupported. Code that an
OpenCode plugin would run is never loaded. Built-in plugins must be native.

## Workflow

1. Pick the scope, then create `<plugins dir>/<name>/plugin.json`.
2. Add components one at a time, starting with skills.
3. Run `/reload-plugins` and read what changed, then `/plugins` to inspect the
   plugin and any dropped files.
4. Fix each reported diagnostic before adding the next component.
