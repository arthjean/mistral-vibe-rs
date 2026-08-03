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
    Thinking,
    Reload,
    Clear,
    Copy,
    PasteImage,
    Log,
    Debug,
    Compact,
    Exit,
    Status,
    Teleport,
    RemoteProject,
    ProxySetup,
    Resume,
    Rename,
    Mcp,
    Voice,
    InstallLean,
    UninstallLean,
    Rewind,
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
        Self::Thinking,
        Self::Reload,
        Self::Clear,
        Self::Copy,
        Self::PasteImage,
        Self::Log,
        Self::Debug,
        Self::Compact,
        Self::Exit,
        Self::Status,
        Self::Teleport,
        Self::RemoteProject,
        Self::ProxySetup,
        Self::Resume,
        Self::Rename,
        Self::Mcp,
        Self::Voice,
        Self::InstallLean,
        Self::UninstallLean,
        Self::Rewind,
        Self::Loop,
        Self::DataRetention,
        Self::Theme,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandContext {
    pub vibe_code_enabled: bool,
    pub clipboard_image_supported: bool,
    excluded: BTreeSet<String>,
}

impl Default for CommandContext {
    fn default() -> Self {
        Self {
            vibe_code_enabled: false,
            clipboard_image_supported: clipboard_image_supported_by_platform(),
            excluded: BTreeSet::new(),
        }
    }
}

const fn clipboard_image_supported_by_platform() -> bool {
    cfg!(target_os = "macos")
}

impl CommandContext {
    #[must_use]
    pub fn new(vibe_code_enabled: bool) -> Self {
        Self {
            vibe_code_enabled,
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
        COMMANDS
            .iter()
            .find(|command| command.id == id)
            .is_some_and(|command| command_available_in(command, self))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandDefinition {
    pub id: CommandId,
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedCommand<'a> {
    pub id: CommandId,
    pub alias: &'a str,
    pub arguments: &'a str,
}

/// Commands exposed by the pinned Python reference, and the sole source of
/// parseable aliases. Every [`CommandId`] appears here exactly once.
pub const COMMANDS: &[CommandDefinition] = &[
    command(CommandId::Help, "help", &["/help"], "Show help message"),
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
        "Clear conversation history",
    ),
    command(
        CommandId::Copy,
        "copy",
        &["/copy"],
        "Copy the last agent message to the clipboard",
    ),
    command(
        CommandId::PasteImage,
        "paste-image",
        &["/paste-image"],
        "Paste an image from the OS clipboard into the prompt",
    ),
    command(
        CommandId::Log,
        "log",
        &["/log"],
        "Show path to current interaction log file",
    ),
    command(
        CommandId::Debug,
        "debug",
        &["/debug"],
        "Toggle debug console",
    ),
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
    ),
    command(
        CommandId::Status,
        "status",
        &["/status"],
        "Display agent statistics",
    ),
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
    ),
    command(
        CommandId::Mcp,
        "mcp",
        &["/mcp", "/connectors"],
        "Display available MCP servers and connectors. Pass a name to list tools; subcommands: add <url> [--transport http|streamable-http], status, login <alias>, logout <alias>",
    ),
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
    ),
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
    }
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
    COMMANDS
        .iter()
        .filter(|command| command_available_in(command, context))
        .find_map(|command| {
            command
                .aliases
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(alias))
                .then_some(ParsedCommand {
                    id: command.id,
                    alias,
                    arguments,
                })
        })
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
    match command.id {
        CommandId::PasteImage => context.clipboard_image_supported,
        CommandId::Teleport | CommandId::RemoteProject => context.vibe_code_enabled,
        _ => true,
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
