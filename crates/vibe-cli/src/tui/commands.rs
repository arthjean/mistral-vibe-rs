#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    Continue,
    Rename,
    Mcp,
    Voice,
    InstallLean,
    UninstallLean,
    Rewind,
    Loop,
    DataRetention,
    Theme,
    Approve,
    Deny,
    Fork,
    History,
    Setup,
    Settings,
    Trust,
    Update,
}

impl CommandId {
    #[must_use]
    pub const fn changes_session_projection(self) -> bool {
        matches!(
            self,
            Self::Clear | Self::Compact | Self::Continue | Self::Fork | Self::Resume | Self::Rewind
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandDefinition {
    pub id: CommandId,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedCommand<'a> {
    pub id: CommandId,
    pub alias: &'a str,
    pub arguments: &'a str,
}

pub const COMMANDS: &[CommandDefinition] = &[
    command(CommandId::Help, &["/help"], "Show help message"),
    command(CommandId::Config, &["/config"], "Edit config settings"),
    command(CommandId::Model, &["/model"], "Select active model"),
    command(CommandId::Thinking, &["/thinking"], "Select thinking level"),
    command(
        CommandId::Reload,
        &["/reload"],
        "Reload configuration, agents, and skills",
    ),
    command(
        CommandId::Clear,
        &["/clear", "/new"],
        "Clear conversation history",
    ),
    command(CommandId::Copy, &["/copy"], "Copy the last agent message"),
    command(
        CommandId::PasteImage,
        &["/paste-image"],
        "Paste an image from the clipboard",
    ),
    command(CommandId::Log, &["/log"], "Show the interaction log path"),
    command(CommandId::Debug, &["/debug"], "Toggle the debug console"),
    command(
        CommandId::Compact,
        &["/compact"],
        "Compact conversation history",
    ),
    command(
        CommandId::Exit,
        &["/exit", "/close", "/quit", "exit", "quit", ":q", ":quit"],
        "Exit Vibe",
    ),
    command(CommandId::Status, &["/status"], "Display agent statistics"),
    command(
        CommandId::Teleport,
        &["/teleport"],
        "Teleport session to Vibe Code Web",
    ),
    command(
        CommandId::RemoteProject,
        &["/remote-project"],
        "Select the Vibe Code Web project",
    ),
    command(
        CommandId::ProxySetup,
        &["/proxy-setup"],
        "Configure proxy and TLS settings",
    ),
    command(
        CommandId::Resume,
        &["/resume"],
        "Browse, resume, or delete saved sessions",
    ),
    command(
        CommandId::Continue,
        &["/continue"],
        "Continue the latest session",
    ),
    command(
        CommandId::Rename,
        &["/rename", "/title"],
        "Rename the current session",
    ),
    command(
        CommandId::Mcp,
        &["/mcp", "/connectors"],
        "Manage MCP servers and connectors",
    ),
    command(CommandId::Voice, &["/voice"], "Configure voice settings"),
    command(
        CommandId::InstallLean,
        &["/leanstall"],
        "Install the Lean 4 agent",
    ),
    command(
        CommandId::UninstallLean,
        &["/unleanstall"],
        "Uninstall the Lean 4 agent",
    ),
    command(
        CommandId::Rewind,
        &["/rewind"],
        "Rewind to a previous message",
    ),
    command(CommandId::Loop, &["/loop"], "Manage scheduled prompts"),
    command(
        CommandId::DataRetention,
        &["/data-retention"],
        "Show data retention information",
    ),
    command(CommandId::Theme, &["/theme"], "Select theme"),
    command(
        CommandId::Approve,
        &["/approve"],
        "Approve the pending action",
    ),
    command(CommandId::Deny, &["/deny"], "Deny the pending action"),
    command(CommandId::Fork, &["/fork"], "Fork the current session"),
    command(CommandId::History, &["/history"], "Browse saved history"),
    command(
        CommandId::Setup,
        &["/setup"],
        "Configure credentials and preferences",
    ),
    command(
        CommandId::Settings,
        &["/settings"],
        "Update session settings",
    ),
    command(CommandId::Trust, &["/trust"], "Trust the current workspace"),
    command(CommandId::Update, &["/update"], "Check for updates"),
];

const fn command(
    id: CommandId,
    aliases: &'static [&'static str],
    description: &'static str,
) -> CommandDefinition {
    CommandDefinition {
        id,
        aliases,
        description,
    }
}

pub fn parse_command(input: &str) -> Option<ParsedCommand<'_>> {
    let trimmed = input.trim();
    let split = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let alias = &trimmed[..split];
    let arguments = trimmed[split..].trim();
    if !alias.starts_with('/') && !arguments.is_empty() {
        return None;
    }
    COMMANDS
        .iter()
        .filter(|command| command_available(command.id))
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
    COMMANDS
        .iter()
        .filter(|command| command_available(command.id))
        .flat_map(|command| command.aliases.iter().copied())
        .filter(|alias| alias.starts_with('/'))
}

#[must_use]
pub fn command_description(alias: &str) -> &'static str {
    COMMANDS
        .iter()
        .find(|command| command.aliases.contains(&alias))
        .map_or("", |command| command.description)
}

#[must_use]
pub const fn command_available(id: CommandId) -> bool {
    !matches!(id, CommandId::PasteImage) || cfg!(target_os = "macos")
}
