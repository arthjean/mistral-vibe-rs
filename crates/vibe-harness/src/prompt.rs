//! The system prompt a unified session sends.
//!
//! The reference composes it in two halves (`harness/core/src/core/prompt/`):
//! the core's own sections, which depend on what the session can do, then
//! the host's instructions (`vibe/app_server/_runtime.py`
//! `_build_unified_system_instructions`: the Vibe Code instructions with the
//! date, the project context and the `AGENTS.md` documents), then the
//! searchable groups, the skill catalog and the capability catalogs
//! (`capabilities.rs`: knowledge folders, agent types, plugins). This module
//! keeps that order and every data-bearing part; the prose of each section
//! is this port's own, written to cover the same directives (`NOTICE`), which
//! the parity ledger records.

/// One skill the catalog lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub path: String,
}

/// One knowledge folder: a plugin's, or the session's own when `plugin` is
/// `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeEntry {
    pub name: String,
    pub description: String,
    pub path: String,
    pub access: String,
    pub plugin: Option<String>,
}

/// One preconfigured subagent type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTypeEntry {
    pub name: String,
    pub description: String,
    pub path: String,
}

/// One mounted plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginEntry {
    pub name: String,
    pub description: String,
    pub path: String,
    /// The tool namespaces it exposes to programs.
    pub namespaces: Vec<String>,
}

/// What the prompt is built from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptContext {
    /// The host's instructions, already composed: the configured prompt
    /// with its date, then the project context and the `AGENTS.md`
    /// documents when project context is on.
    pub instructions: String,
    /// Whether programs can start background processes.
    pub background_processes: bool,
    /// Whether the session can start subagents: a subagent cannot.
    pub subagents: bool,
    /// The groups `search_tool_functions` searches.
    pub groups: Vec<String>,
    pub skills: Vec<SkillEntry>,
    pub knowledge: Vec<KnowledgeEntry>,
    pub agent_types: Vec<AgentTypeEntry>,
    pub plugins: Vec<PluginEntry>,
}

const INTRO: &str = "You get users' tasks done end to end, acting on your own initiative. \
Skills hold task-specific instructions and load with the `skill` tool; further tools are found \
with `search_tool_functions` and called from `run_typescript`.";

const AUTONOMY: &str = "## Working on your own

- A request authorizes the ordinary work inside its scope. Begin without asking whether to go ahead.
- Loading skills, discovering tools, reading files and searching context are reversible: do them without asking.
- When an action needs approval, call the tool anyway; the runtime asks the user. Do not ask in the conversation first.
- Settle ambiguity from the context and the tools before asking. Ask only when the answer would change the outcome materially, cannot be found, or grants authority you lack.
- Choose sensibly, keep going, and stop only when the outcome is reached or a genuine blocker remains.";

const PROGRAMS: &str = "## Calling tool functions from run_typescript

`run_typescript` runs a TypeScript program in a sandbox. The program reaches outside services through functions under `tools.<group>.<function>`, which users may call tools, integrations, connectors or services. The same tool also suits pure data work: counting, filtering, grouping, sorting and computing over results.

Find functions with the top-level `search_tool_functions` tool. Before using a function you have not used yet:

1. Search with `best_match` and a short description of the capability; it answers ranked exact names.
2. Ask for `details` on each name you picked, to learn its signature.
3. Call it at its documented `tools.<group>.<function>` path inside `run_typescript`.";

const BACKGROUND_PROCESSES: &str = "## Background processes

When a command has to keep running while you do other work, use the `tools.process` group from `run_typescript`:
- `tools.process.start` takes the same command syntax as `tools.file_system.bash` and answers at once with a process ID that stays valid.
- `tools.process.output` reads output from a byte cursor, optionally waiting for more output or for the process to end; `from: \"end\"` reads the newest page instead. Continue reading at `nextCursor`; `hasMore` says retained output remains unread and `truncatedBefore` says older output was dropped.
- `tools.process.write` sends text, control keys or raw bytes to an interactive process.
- `tools.process.list` shows the processes of this session.
- `tools.process.stop` ends a process together with its children.

Prefer plain `tools.file_system.bash` when you need a command's whole result right away. Keep every process ID and cursor you are given. When a process ends, the runtime adds a short notification to the conversation; read the rest of its output with `tools.process.output` before concluding anything, and stop processes you no longer need.

To pause while background work progresses, call `tools.self.sleep({ seconds })` from `run_typescript`, then look at the output or the status.";

const SUBAGENTS: &str = "## Subagents

Subagents keep their own conversation and are driven through the `tools.subagent` group:
- `tools.subagent.spawn` starts a named subagent and returns without waiting. Every new subagent needs a name this session has never used.
- `tools.subagent.sendMessage` steers the running turn, or starts the next one.
- `tools.subagent.list` shows the subagents of this session.
- `tools.subagent.wait` waits for the current turn of one subagent.
- `tools.subagent.interrupt` stops the running turn and keeps the subagent.
- `tools.subagent.close` ends the subagent for good; its name stays taken.

These operations have effects outside the sandbox: call them from `run_typescript`. Use an existing name only to keep working with the same live subagent, and pick a fresh name for any new one. Wait for every result you need before answering.";

const SKILL_RULES: &str = "## Available Skills

A skill carries the instructions for one kind of task. When a request fits a skill's description, call `skill` with its exact name before starting, then follow what it says; if its full instructions are already in the conversation, follow them without loading it again. Load every skill that clearly applies.

Only names listed in `<available-skills>` are skills. Connector names, and names a skill mentions but the list does not, are not.

After loading a skill, do not read its root `SKILL.md` with `read_file`; use `read_file` only for the supporting files the skill points to.";

const CURRENT_TIME: &str = "## The current time

When a request depends on the present moment, read `new Date().toISOString()` from `run_typescript` before any search, calendar, scheduling or service call that depends on it, and treat that value as now. Present times in the user's time zone when the context gives one, otherwise in UTC; ask for the zone only when local time changes the answer.";

const KNOWLEDGE_INTRO: &str = "## Knowledge folders

The descriptions below tell which knowledge folders matter for a request. Before answering from one, read its `KNOWLEDGE.md`, and keep to its access mode and any rules it states.";

const AGENT_TYPES_INTRO: &str = "## Subagent types

To start one of these preconfigured subagents, pass its exact name as `agentType` to `tools.subagent.spawn`.";

const PLUGINS_INTRO: &str = "## Plugins

Plugins are packages of capabilities mounted into this session. Their descriptions only help route a request: load one of a plugin's listed skills for how to act. When the user names a plugin, work through its skills, its knowledge folders or the functions in its tool namespace, and do not assume it can do anything those do not expose.";

/// The prompt for `context`.
#[must_use]
pub fn system_prompt(context: &PromptContext) -> String {
    let mut core = vec![INTRO.to_owned(), AUTONOMY.to_owned(), PROGRAMS.to_owned()];
    if context.background_processes {
        core.push(BACKGROUND_PROCESSES.to_owned());
    }
    if context.subagents {
        core.push(SUBAGENTS.to_owned());
    }
    if !context.skills.is_empty() {
        core.push(SKILL_RULES.to_owned());
    }
    core.push(CURRENT_TIME.to_owned());
    let mut sections = vec![core.join("\n\n")];
    if !context.instructions.trim().is_empty() {
        sections.push(context.instructions.trim().to_owned());
    }
    sections.push(groups_section(&context.groups));
    if let Some(skills) = skills_section(&context.skills) {
        sections.push(skills);
    }
    if let Some(knowledge) = knowledge_section(&context.knowledge) {
        sections.push(knowledge);
    }
    if context.subagents
        && let Some(agent_types) = agent_types_section(&context.agent_types)
    {
        sections.push(agent_types);
    }
    if let Some(plugins) = plugins_section(&context.plugins) {
        sections.push(plugins);
    }
    sections.join("\n\n")
}

fn groups_section(groups: &[String]) -> String {
    let lines: Vec<String> = groups.iter().map(|group| format!("- {group}")).collect();
    format!("### Searchable connector groups\n\n{}", lines.join("\n"))
}

fn skills_section(skills: &[SkillEntry]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let entries: Vec<String> = skills
        .iter()
        .map(|skill| {
            format!(
                "  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    <path>{}</path>\n  </skill>",
                escape(&skill.name),
                escape(&skill.description),
                escape(&skill.path)
            )
        })
        .collect();
    Some(format!(
        "<available-skills>\n{}\n</available-skills>",
        entries.join("\n")
    ))
}

fn knowledge_section(folders: &[KnowledgeEntry]) -> Option<String> {
    if folders.is_empty() {
        return None;
    }
    let mut folders: Vec<&KnowledgeEntry> = folders.iter().collect();
    folders.sort_by(|left, right| (&left.name, &left.path).cmp(&(&right.name, &right.path)));
    let entries: Vec<String> = folders
        .into_iter()
        .map(|folder| {
            let source = match &folder.plugin {
                Some(plugin) => format!(
                    "    <source>\n      <type>plugin</type>\n      <plugin-name>{}</plugin-name>\n    </source>",
                    escape(plugin)
                ),
                None => "    <source>\n      <type>standalone</type>\n    </source>".to_owned(),
            };
            format!(
                "  <knowledge-folder>\n    <name>{}</name>\n    <description>{}</description>\n    <path>{}</path>\n    <access>{}</access>\n{source}\n  </knowledge-folder>",
                escape(&folder.name),
                escape(&folder.description),
                escape(&folder.path),
                folder.access
            )
        })
        .collect();
    Some(format!(
        "{KNOWLEDGE_INTRO}\n\n<available-knowledge-folders>\n{}\n</available-knowledge-folders>",
        entries.join("\n")
    ))
}

fn agent_types_section(agent_types: &[AgentTypeEntry]) -> Option<String> {
    if agent_types.is_empty() {
        return None;
    }
    let mut agent_types: Vec<&AgentTypeEntry> = agent_types.iter().collect();
    agent_types.sort_by(|left, right| left.name.cmp(&right.name));
    let entries: Vec<String> = agent_types
        .into_iter()
        .map(|agent| {
            format!(
                "  <agent>\n    <name>{}</name>\n    <description>{}</description>\n    <path>{}</path>\n  </agent>",
                escape(&agent.name),
                escape(&agent.description),
                escape(&agent.path)
            )
        })
        .collect();
    Some(format!(
        "{AGENT_TYPES_INTRO}\n\n<available-agent-types>\n{}\n</available-agent-types>",
        entries.join("\n")
    ))
}

fn plugins_section(plugins: &[PluginEntry]) -> Option<String> {
    if plugins.is_empty() {
        return None;
    }
    let mut plugins: Vec<&PluginEntry> = plugins.iter().collect();
    plugins.sort_by(|left, right| left.name.cmp(&right.name));
    let entries: Vec<String> = plugins
        .into_iter()
        .map(|plugin| {
            let namespaces: String = plugin
                .namespaces
                .iter()
                .map(|namespace| format!("\n    <tool-namespace>{}</tool-namespace>", escape(namespace)))
                .collect();
            format!(
                "  <plugin>\n    <name>{}</name>\n    <description>{}</description>\n    <path>{}</path>{namespaces}\n  </plugin>",
                escape(&plugin.name),
                escape(&plugin.description),
                escape(&plugin.path)
            )
        })
        .collect();
    Some(format!(
        "{PLUGINS_INTRO}\n\n<available-plugins>\n{}\n</available-plugins>",
        entries.join("\n")
    ))
}

/// XML text escaping, apostrophes included.
#[must_use]
pub fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod prompt_tests;
