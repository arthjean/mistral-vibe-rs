use std::collections::BTreeSet;

/// Every identifier a submitted line can parse into.
///
/// The variants and [`COMMANDS`] are one closed set: a variant absent from the
/// table can never be produced by [`parse_command_in`], so its handlers would be
/// unreachable. [`CommandId::ALL`] and `command_ids_and_definitions_agree` keep
/// the two in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CommandId {
    Help,
    Config,
    Model,
    Skills,
    Thinking,
    Reload,
    Clear,
    Copy,
    PasteImage,
    Log,
    LogLevel,
    Debug,
    Compact,
    Exit,
    Status,
    Whoami,
    Teleport,
    RemoteProject,
    ProxySetup,
    Resume,
    Rename,
    Mcp,
    Plugins,
    ReloadPlugins,
    Todo,
    Voice,
    InstallLean,
    UninstallLean,
    Rewind,
    Branch,
    Retry,
    Loop,
    DataRetention,
    Theme,
}

impl CommandId {
    /// Every variant, in declaration order. Adding a variant without extending
    /// this list fails `command_ids_and_definitions_agree`.
    pub const ALL: &'static [Self] = &[
        Self::Help,
        Self::Config,
        Self::Model,
        Self::Skills,
        Self::Thinking,
        Self::Reload,
        Self::Clear,
        Self::Copy,
        Self::PasteImage,
        Self::Log,
        Self::LogLevel,
        Self::Debug,
        Self::Compact,
        Self::Exit,
        Self::Status,
        Self::Whoami,
        Self::Teleport,
        Self::RemoteProject,
        Self::ProxySetup,
        Self::Resume,
        Self::Rename,
        Self::Mcp,
        Self::Plugins,
        Self::ReloadPlugins,
        Self::Todo,
        Self::Voice,
        Self::InstallLean,
        Self::UninstallLean,
        Self::Rewind,
        Self::Branch,
        Self::Retry,
        Self::Loop,
        Self::DataRetention,
        Self::Theme,
    ];
}

/// What decides whether a command is offered, one variant per predicate the
/// reference registry declares (`vibe/cli/commands.py`, `is_available`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandGate {
    /// No predicate: offered unless excluded.
    Always,
    /// `/paste-image`, offered on the one platform whose clipboard the
    /// reference reads images from.
    ClipboardImage,
    /// `/skills`, offered while `experimental_enable_registry_skills` is set.
    RegistrySkills,
    /// `/plugins`, `/reload-plugins` and `/todo`, offered only when the session
    /// runs on the Unified harness backend.
    ExperimentalHarness,
}

/// Reference `CommandContext` (`vibe/cli/commands.py:10-13`), plus the two
/// inputs the reference reads from elsewhere: the host platform and the
/// excluded keys.
///
/// `experimental_harness` is the reference's backend selection. This port runs
/// one backend, the equivalent of the reference's default legacy one, so every
/// production context answers `false` here, which is what a reference session
/// launched without `--experimental-harness` answers too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandContext {
    pub registry_skills_enabled: bool,
    pub experimental_harness: bool,
    pub clipboard_image_supported: bool,
    excluded: BTreeSet<String>,
}

impl Default for CommandContext {
    fn default() -> Self {
        Self::new(false, false)
    }
}

const fn clipboard_image_supported_by_platform() -> bool {
    cfg!(target_os = "macos")
}

impl CommandContext {
    #[must_use]
    pub fn new(registry_skills_enabled: bool, experimental_harness: bool) -> Self {
        Self {
            registry_skills_enabled,
            experimental_harness,
            clipboard_image_supported: clipboard_image_supported_by_platform(),
            excluded: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn with_excluded<'a>(mut self, excluded: impl IntoIterator<Item = &'a str>) -> Self {
        self.excluded = excluded.into_iter().map(str::to_owned).collect();
        self
    }

    #[must_use]
    pub fn with_clipboard_image_supported(mut self, supported: bool) -> Self {
        self.clipboard_image_supported = supported;
        self
    }

    #[must_use]
    pub fn is_available(&self, id: CommandId) -> bool {
        definition(id).is_some_and(|command| command_available_in(command, self))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandDefinition {
    pub id: CommandId,
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    /// Reference `Command.side_channel`: the command runs at once while a turn
    /// or a paused queue holds the composer, instead of being refused.
    pub side_channel: bool,
    /// Reference `Command.exits`: running the command ends the session.
    pub exits: bool,
    pub gate: CommandGate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedCommand<'a> {
    pub id: CommandId,
    pub alias: &'a str,
    pub arguments: &'a str,
}

/// Commands exposed by the pinned Python reference, in the reference's
/// declaration order, and the sole source of parseable aliases. Every
/// [`CommandId`] appears here exactly once.
pub const COMMANDS: &[CommandDefinition] = &[
    command(CommandId::Help, "help", &["/help"], "Show help message").side_channel(),
    command(
        CommandId::Config,
        "config",
        &["/config"],
        "Edit config settings",
    ),
    command(
        CommandId::Model,
        "model",
        &["/model"],
        "Select active model",
    ),
    command(
        CommandId::Skills,
        "skills",
        &["/skills"],
        "Browse, import, and manage skills",
    )
    .gated(CommandGate::RegistrySkills),
    command(
        CommandId::Thinking,
        "thinking",
        &["/thinking"],
        "Select thinking level",
    ),
    command(
        CommandId::Reload,
        "reload",
        &["/reload"],
        "Reload configuration, agent instructions, and skills from disk",
    ),
    command(
        CommandId::Clear,
        "clear",
        &["/clear", "/new"],
        "Start a new conversation. Optionally pass a prompt to seed it.",
    ),
    command(
        CommandId::Copy,
        "copy",
        &["/copy"],
        "Copy the last agent message to the clipboard",
    )
    .side_channel(),
    command(
        CommandId::PasteImage,
        "paste-image",
        &["/paste-image"],
        "Paste an image from the OS clipboard into the prompt",
    )
    .side_channel()
    .gated(CommandGate::ClipboardImage),
    command(
        CommandId::Log,
        "log",
        &["/log"],
        "Show path to current interaction log file",
    )
    .side_channel(),
    command(
        CommandId::LogLevel,
        "log-level",
        &["/log-level"],
        "Change the log level for this session or persist it to config.toml.",
    ),
    command(
        CommandId::Debug,
        "debug",
        &["/debug"],
        "Toggle debug console",
    )
    .side_channel(),
    command(
        CommandId::Compact,
        "compact",
        &["/compact"],
        "Compact conversation history by summarizing. Optionally pass instructions to guide the summary",
    ),
    command(
        CommandId::Exit,
        "exit",
        &["/exit", "exit", "quit", ":q", ":quit"],
        "Exit the application",
    )
    .side_channel()
    .exits(),
    command(
        CommandId::Status,
        "status",
        &["/status"],
        "Display agent statistics",
    )
    .side_channel(),
    command(
        CommandId::Whoami,
        "whoami",
        &["/whoami"],
        "Display the Mistral signed-in user, workspace, and plan",
    )
    .side_channel(),
    command(
        CommandId::Teleport,
        "teleport",
        &["/teleport"],
        "Teleport session to Vibe Code Web",
    ),
    command(
        CommandId::RemoteProject,
        "remote-project",
        &["/remote-project"],
        "Select the Vibe Code Web project for this repository",
    ),
    command(
        CommandId::ProxySetup,
        "proxy-setup",
        &["/proxy-setup"],
        "Configure proxy and SSL certificate settings",
    ),
    command(
        CommandId::Resume,
        "resume",
        &["/resume", "/continue"],
        "Browse, resume, or delete saved sessions",
    ),
    command(
        CommandId::Rename,
        "rename",
        &["/rename"],
        "Rename the current session",
    )
    .side_channel(),
    command(
        CommandId::Mcp,
        "mcp",
        &["/mcp", "/connectors"],
        "Display available MCP servers and connectors. Pass a name to list tools; subcommands: add <url> [--transport http|streamable-http], status, login <alias>, logout <alias>",
    ),
    command(
        CommandId::Plugins,
        "plugins",
        &["/plugins"],
        "Display the plugins this session is running",
    )
    .gated(CommandGate::ExperimentalHarness),
    command(
        CommandId::ReloadPlugins,
        "reload-plugins",
        &["/reload-plugins"],
        "Re-pin this session's plugins and report what changed",
    )
    .gated(CommandGate::ExperimentalHarness),
    command(
        CommandId::Todo,
        "todo",
        &["/todo"],
        "Show the current todo list",
    )
    .gated(CommandGate::ExperimentalHarness),
    command(
        CommandId::Voice,
        "voice",
        &["/voice"],
        "Configure voice settings",
    ),
    command(
        CommandId::InstallLean,
        "leanstall",
        &["/leanstall"],
        "Install the Lean 4 agent (leanstral)",
    ),
    command(
        CommandId::UninstallLean,
        "unleanstall",
        &["/unleanstall"],
        "Uninstall the Lean 4 agent",
    ),
    command(
        CommandId::Rewind,
        "rewind",
        &["/rewind"],
        "Rewind to a previous message (or press Esc twice)",
    ),
    command(
        CommandId::Branch,
        "branch",
        &["/branch"],
        "Fork the current conversation into a new resumable session, leaving this session unchanged. Resume the copy with `vibe --resume <id>`.",
    ),
    command(
        CommandId::Retry,
        "retry",
        &["/retry"],
        "Continue an interrupted model response; optionally pass additional instructions",
    ),
    command(
        CommandId::Loop,
        "loop",
        &["/loop"],
        "Schedule a recurring prompt. Use `/loop <interval> <prompt>`, `/loop list`, or `/loop cancel <id|all>`",
    ),
    command(
        CommandId::DataRetention,
        "data-retention",
        &["/data-retention"],
        "Show data retention information",
    )
    .side_channel(),
    command(CommandId::Theme, "theme", &["/theme"], "Select theme"),
];

const fn command(
    id: CommandId,
    name: &'static str,
    aliases: &'static [&'static str],
    description: &'static str,
) -> CommandDefinition {
    CommandDefinition {
        id,
        name,
        aliases,
        description,
        side_channel: false,
        exits: false,
        gate: CommandGate::Always,
    }
}

impl CommandDefinition {
    const fn side_channel(mut self) -> Self {
        self.side_channel = true;
        self
    }

    const fn exits(mut self) -> Self {
        self.exits = true;
        self
    }

    const fn gated(mut self, gate: CommandGate) -> Self {
        self.gate = gate;
        self
    }
}

/// The table row for `id`. [`COMMANDS`] carries every identifier, which
/// `command_ids_and_definitions_agree` holds, so `None` is unreachable.
#[must_use]
pub fn definition(id: CommandId) -> Option<&'static CommandDefinition> {
    COMMANDS.iter().find(|command| command.id == id)
}

#[must_use]
pub fn parse_command(input: &str) -> Option<ParsedCommand<'_>> {
    parse_command_in(input, &CommandContext::default())
}

#[must_use]
pub fn parse_command_in<'a>(input: &'a str, context: &CommandContext) -> Option<ParsedCommand<'a>> {
    let trimmed = input.trim();
    let split = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let alias = &trimmed[..split];
    let arguments = trimmed[split..].trim();
    if !alias.starts_with('/') && !arguments.is_empty() {
        return None;
    }
    // The reference resolves an alias through `user_input.lower()`, which is
    // Unicode-aware: `/THIN\u{212A}ING`, spelled with the Kelvin sign, folds
    // onto `/thinking` there. An ASCII-only fold left that input unparsed here,
    // which the `parse` family of `crates/vibe-cli/tests/commands/corpus.json`
    // measures. Every declared alias is lowercase, which the capture asserts, so
    // lowercasing the head word once is the whole comparison.
    let lowered = alias.to_lowercase();
    COMMANDS
        .iter()
        .filter(|command| command_available_in(command, context))
        .find_map(|command| {
            command
                .aliases
                .contains(&lowered.as_str())
                .then_some(ParsedCommand {
                    id: command.id,
                    alias,
                    arguments,
                })
        })
}

/// The registry key a parsed command answers under.
///
/// Reference `parse_command` answers with the key rather than with the alias
/// typed, which is what `_handle_command` reports to telemetry and what it
/// displays for a bare alias: `/connectors` is `mcp`, `/new` is `clear` and
/// `:q` is `exit`. `command_ids_and_definitions_agree` keeps every identifier
/// in [`COMMANDS`], so the fallback is unreachable rather than a silent name.
#[must_use]
pub fn command_name(id: CommandId) -> &'static str {
    definition(id).map_or("", |command| command.name)
}

/// What reference `_handle_command` shows above a command's own output.
///
/// A slash line keeps its arguments and its case and loses exactly one leading
/// slash, so `/HELP` reads as `HELP`; a bare alias is replaced by the key it
/// resolved to, so `:q` reads as `exit`.
#[must_use]
pub fn command_echo(input: &str, id: CommandId) -> String {
    let trimmed = input.trim();
    trimmed
        .strip_prefix('/')
        .map_or_else(|| command_name(id).to_owned(), str::to_owned)
}

pub fn command_aliases() -> impl Iterator<Item = &'static str> {
    command_aliases_in(&CommandContext::default())
}

pub fn command_aliases_in(context: &CommandContext) -> impl Iterator<Item = &'static str> + use<> {
    let mut aliases = COMMANDS
        .iter()
        .filter(|command| command_available_in(command, context))
        .flat_map(|command| command.aliases.iter().copied())
        .filter(|alias| alias.starts_with('/'))
        .collect::<Vec<_>>();
    aliases.sort_unstable();
    aliases.into_iter()
}

#[must_use]
pub fn command_description(alias: &str) -> &'static str {
    COMMANDS
        .iter()
        .find(|command| command.aliases.contains(&alias))
        .map_or("", |command| command.description)
}

#[must_use]
pub fn command_available_in(command: &CommandDefinition, context: &CommandContext) -> bool {
    if context.excluded.contains(command.name) {
        return false;
    }
    match command.gate {
        CommandGate::Always => true,
        CommandGate::ClipboardImage => context.clipboard_image_supported,
        CommandGate::RegistrySkills => context.registry_skills_enabled,
        CommandGate::ExperimentalHarness => context.experimental_harness,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A [`CommandId`] absent from [`COMMANDS`] can never be parsed, so every
    /// handler behind it would be dead code.
    #[test]
    fn command_ids_and_definitions_agree() {
        for id in CommandId::ALL {
            assert!(
                COMMANDS.iter().any(|command| command.id == *id),
                "{id:?} has no alias, so its handlers are unreachable"
            );
        }
        for command in COMMANDS {
            assert!(
                CommandId::ALL.contains(&command.id),
                "{:?} is missing from CommandId::ALL",
                command.id
            );
        }
        assert_eq!(CommandId::ALL.len(), COMMANDS.len());
    }
}
