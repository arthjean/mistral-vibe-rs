//! The closed value sets the app-server publishes.
//!
//! Each one is a wire vocabulary the reference declares as an enumeration: a
//! client matches on the exact strings, so spelling them as Rust types rather
//! than as literals at the point of use is what keeps a new value from being
//! invented by a typo. The corpus replay compares every set here against the
//! reference vocabulary of the same name.

use serde::Serialize;

/// How an account stands with the service serving its model.
///
/// `Ready` and `MissingKey` are decided from the configuration; `Unauthorized`
/// is a verdict only the console can return, and this port has no client for
/// that endpoint yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    Ready,
    MissingKey,
    Unauthorized,
    Unavailable,
}

impl AccountStatus {
    pub const ALL: [Self; 4] = [
        Self::Ready,
        Self::MissingKey,
        Self::Unauthorized,
        Self::Unavailable,
    ];
}

/// The kind of plan an account runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountPlanKind {
    Api,
    Chat,
    MistralCode,
}

impl AccountPlanKind {
    pub const ALL: [Self; 3] = [Self::Api, Self::Chat, Self::MistralCode];
}

/// What an account view offers a client to open in a browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountActionKind {
    SwitchApiKey,
    UpgradeToPro,
}

impl AccountActionKind {
    pub const ALL: [Self; 2] = [Self::SwitchApiKey, Self::UpgradeToPro];
}

/// Where a published MCP source comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpSourceKind {
    Server,
    Connector,
}

impl McpSourceKind {
    pub const ALL: [Self; 2] = [Self::Server, Self::Connector];
}

/// How a published MCP source stands.
///
/// `Enabled` is a source the configuration keeps but whose transport has not
/// reported yet, which is what distinguishes it from `Connected`; `Disabled` is
/// a deliberate switch and never a failure, which is what keeps a source the
/// operator turned off from being rendered as broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpSourceStatus {
    Disabled,
    Connected,
    Enabled,
    NeedsAuth,
    NeedsSetup,
    Unavailable,
}

impl McpSourceStatus {
    pub const ALL: [Self; 6] = [
        Self::Disabled,
        Self::Connected,
        Self::Enabled,
        Self::NeedsAuth,
        Self::NeedsSetup,
        Self::Unavailable,
    ];
}

/// How much an agent profile is trusted to do without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSafety {
    Safe,
    Neutral,
    Destructive,
    Yolo,
}

impl AgentSafety {
    pub const ALL: [Self; 4] = [Self::Safe, Self::Neutral, Self::Destructive, Self::Yolo];

    /// The safety a profile declares, or `Neutral` when it declares a word this
    /// vocabulary does not carry.
    ///
    /// A profile is a file an operator writes, so an unknown word is a typo
    /// rather than a new value: publishing the cautious middle keeps the wire
    /// vocabulary closed without dropping the profile.
    #[must_use]
    pub fn parse(declared: &str) -> Self {
        match declared {
            "safe" => Self::Safe,
            "destructive" => Self::Destructive,
            "yolo" => Self::Yolo,
            _ => Self::Neutral,
        }
    }
}
