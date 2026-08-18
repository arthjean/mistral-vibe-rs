//! The payloads exchanged during `initialize`.

use serde::{Deserialize, Serialize};

/// How the client process was launched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientEntrypoint {
    /// Not reported by the client.
    #[default]
    Unknown,
    /// Interactive terminal client.
    Cli,
    /// Agent Client Protocol bridge.
    Acp,
    /// Embedded or scripted client.
    Programmatic,
}

/// Terminal the client is attached to, when it can be identified.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalEmulator {
    /// Not reported, or not recognized.
    #[default]
    Unknown,
    /// Visual Studio Code integrated terminal.
    Vscode,
    /// Visual Studio Code Insiders integrated terminal.
    VscodeInsiders,
    /// Cursor integrated terminal.
    Cursor,
    /// JetBrains IDE integrated terminal.
    Jetbrains,
    /// macOS Terminal.app.
    AppleTerminal,
    /// iTerm2.
    Iterm2,
    /// WezTerm.
    Wezterm,
    /// Ghostty.
    Ghostty,
    /// Alacritty.
    Alacritty,
    /// Kitty.
    Kitty,
    /// Hyper.
    Hyper,
    /// Windows Terminal.
    WindowsTerminal,
}

/// Identity the client declares during `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ClientInfo {
    /// Program name.
    pub name: String,
    /// Program version.
    pub version: String,
    /// Display name, when it differs from `name`.
    #[serde(default)]
    pub title: Option<String>,
    /// How the client was launched.
    #[serde(default)]
    pub entrypoint: ClientEntrypoint,
    /// Terminal the client runs in.
    #[serde(default)]
    pub terminal_emulator: TerminalEmulator,
}

/// Kinds of server-initiated callback a client can answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallbackKind {
    /// Approve or deny a tool effect.
    Approval,
    /// Supply free-form input to the running turn.
    UserInput,
    /// Complete a connector authorization flow.
    ConnectorAuth,
}

/// Tools the server may delegate to the client process.
///
/// This is the single spelling of the capability, on the wire and in the engine
/// that gates on it: the serialized name is the reference method prefix, so a
/// port that reads one reads the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ClientToolCapability {
    /// Read files through the client.
    #[serde(rename = "filesystem/read")]
    FilesystemRead,
    /// Write files through the client.
    #[serde(rename = "filesystem/write")]
    FilesystemWrite,
    /// Run terminal commands through the client.
    #[serde(rename = "terminal")]
    Terminal,
}

/// What the client can handle, declared during `initialize`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ClientCapabilities {
    /// Callback kinds the client answers. The server refuses to raise a
    /// callback the client did not declare.
    #[serde(default)]
    pub callback_kinds: Vec<CallbackKind>,
    /// Tools the client exposes back to the server.
    #[serde(default)]
    pub client_tools: Vec<ClientToolCapability>,
    /// Notification names the client does not want delivered.
    ///
    /// The server honors the list for every notification except a sequenced
    /// event: muting one of those would open a gap in the per-session event
    /// stream that the client's own projection treats as a fault.
    #[serde(default)]
    pub disabled_notifications: Vec<String>,
}

/// Parameters of the `initialize` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InitializeParams {
    /// Who is connecting.
    pub client_info: ClientInfo,
    /// What the client can handle.
    #[serde(default)]
    pub capabilities: ClientCapabilities,
}

/// Identity the server reports during `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ServerInfo {
    /// Program name.
    pub name: String,
    /// Program version.
    pub version: String,
}

/// How a connection is carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    /// Client and server share a process.
    InProcess,
    /// Newline-delimited frames over stdin and stdout.
    Stdio,
}

/// What the server offers, reported during `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ServerCapabilities {
    /// Methods this build actually routes, a subset of
    /// [`SERVER_METHODS`](crate::SERVER_METHODS).
    #[serde(default)]
    pub methods: Vec<String>,
    /// Callback kinds this build can raise.
    #[serde(default)]
    pub callback_kinds: Vec<CallbackKind>,
    /// Transport carrying this connection.
    #[serde(default)]
    pub transports: Vec<TransportKind>,
}

/// The only contract version this crate describes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProtocolVersion {
    /// Serializes as `"1"`.
    #[default]
    #[serde(rename = "1")]
    V1,
}

/// Result of the `initialize` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InitializeResponse {
    /// Who answered.
    pub server_info: ServerInfo,
    /// Contract version spoken by the server.
    pub protocol_version: ProtocolVersion,
    /// What the server offers.
    pub capabilities: ServerCapabilities,
}

#[cfg(test)]
mod handshake_tests;
