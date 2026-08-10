use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_SETUP_INPUT_BYTES: usize = 16 * 1024;
const MAX_SETUP_NAME_BYTES: usize = 256;
const MAX_PROXY_BYTES: usize = 2 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    Light,
    Dark,
    #[default]
    System,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationPreference {
    Off,
    #[default]
    WhenUnfocused,
    Always,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preferences {
    pub theme: Theme,
    pub notifications: NotificationPreference,
    pub update_checks: bool,
    pub voice_device: Option<String>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            notifications: NotificationPreference::WhenUnfocused,
            update_checks: true,
            voice_device: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PreferenceStore {
    path: PathBuf,
}

impl PreferenceStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> Result<Preferences, SetupError> {
        match fs::read_to_string(&self.path) {
            Ok(text) => toml::from_str(&text).map_err(SetupError::PreferenceDecode),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(Preferences::default())
            }
            Err(error) => Err(SetupError::PreferenceIo(error)),
        }
    }

    pub fn save(&self, preferences: &Preferences) -> Result<(), SetupError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(SetupError::PreferenceIo)?;
        }
        let encoded = toml::to_string_pretty(preferences).map_err(SetupError::PreferenceEncode)?;
        let temporary = sibling_temporary_path(&self.path);
        fs::write(&temporary, encoded).map_err(SetupError::PreferenceIo)?;
        if let Err(error) = fs::rename(&temporary, &self.path) {
            let _ = fs::remove_file(&temporary);
            return Err(SetupError::PreferenceIo(error));
        }
        Ok(())
    }
}

fn sibling_temporary_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("preferences.toml");
    path.with_file_name(format!(".{name}.tmp-{}", std::process::id()))
}

pub trait CredentialStore: Send + Sync {
    fn set(&self, account: &str, secret: &str) -> Result<(), CredentialError>;
    fn get(&self, account: &str) -> Result<Option<String>, CredentialError>;
}

/// The production store: `vibe_core::auth` persistence over the OS keyring
/// under the shared service names, with the global dotenv fallback that keeps
/// the key when the keyring cannot take it.
///
/// The reference exports the persisted key into `os.environ`; this port keeps
/// that overlay here, so the completion check sees the key whichever storage
/// path accepted it.
pub struct PersistedCredentialStore {
    store: vibe_core::auth::KeyringStore,
    env_file: PathBuf,
    backend_is_mistral: bool,
    process_env: Mutex<std::collections::BTreeMap<String, String>>,
}

impl PersistedCredentialStore {
    #[must_use]
    pub fn new(env_file: PathBuf, backend_is_mistral: bool) -> Self {
        Self::with_store(
            env_file,
            backend_is_mistral,
            vibe_core::auth::KeyringStore::native(),
        )
    }

    /// The same bridge over a caller-supplied keyring store, which is how the
    /// tests keep the OS keyring out of the loop.
    #[must_use]
    pub fn with_store(
        env_file: PathBuf,
        backend_is_mistral: bool,
        store: vibe_core::auth::KeyringStore,
    ) -> Self {
        Self {
            store,
            env_file,
            backend_is_mistral,
            process_env: Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// The stored credential for `account`, the overlay of keys persisted this
    /// run winning over the keyring.
    #[must_use]
    pub fn resolve(&self, account: &str) -> Option<String> {
        if let Ok(overlay) = self.process_env.lock()
            && let Some(secret) = overlay.get(account)
        {
            return Some(secret.clone());
        }
        self.store.get_api_key(account)
    }
}

impl CredentialStore for PersistedCredentialStore {
    fn set(&self, account: &str, secret: &str) -> Result<(), CredentialError> {
        if account.trim().is_empty() || secret.is_empty() {
            return Err(CredentialError::InvalidInput);
        }
        let mut overlay = self
            .process_env
            .lock()
            .map_err(|_| CredentialError::Unavailable)?;
        let report = vibe_core::auth::persist_api_key(
            account,
            self.backend_is_mistral,
            secret,
            false,
            &mut overlay,
            &self.env_file,
            &self.store,
        );
        // The reference sends an onboarding telemetry event here; the
        // telemetry envelope is a recorded divergence, so the event stays
        // local and unsent, on the same terms as `telemetry/record`.
        match report.outcome {
            vibe_core::auth::PersistOutcome::Completed => Ok(()),
            vibe_core::auth::PersistOutcome::EnvVarError { .. } => {
                Err(CredentialError::InvalidInput)
            }
            vibe_core::auth::PersistOutcome::SaveError { .. } => Err(CredentialError::Unavailable),
        }
    }

    fn get(&self, account: &str) -> Result<Option<String>, CredentialError> {
        if account.trim().is_empty() {
            return Err(CredentialError::InvalidInput);
        }
        Ok(self.resolve(account))
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum CredentialError {
    #[error("credential input is invalid")]
    InvalidInput,
    #[error(
        "the API key could not be saved: the OS keyring refused it and the fallback file under \
         the vibe home could not be written"
    )]
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupStep {
    Provider,
    Authentication,
    WorkspaceTrust,
    Network,
    Model,
    Preferences,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupResources {
    pub provider: String,
    pub credential_handle: String,
    pub workspace_trusted: bool,
    pub proxy: Option<String>,
    pub certificate_path: Option<PathBuf>,
    pub model: String,
    pub thinking: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupState {
    pub step: SetupStep,
    pub provider: String,
    pub credential_account: String,
    pub workspace_trusted: bool,
    pub proxy: Option<String>,
    pub certificate_path: Option<PathBuf>,
    pub model: String,
    pub thinking: String,
    pub preferences: Preferences,
}

impl Default for SetupState {
    fn default() -> Self {
        Self {
            step: SetupStep::Provider,
            provider: "mistral".to_owned(),
            credential_account: "MISTRAL_API_KEY".to_owned(),
            workspace_trusted: false,
            proxy: None,
            certificate_path: None,
            model: "mistral-medium-3.5".to_owned(),
            thinking: "off".to_owned(),
            preferences: Preferences::default(),
        }
    }
}

impl SetupState {
    pub fn select_provider(&mut self, provider: impl Into<String>) -> Result<(), SetupError> {
        let provider = provider.into();
        if provider.trim().is_empty() || provider.len() > MAX_SETUP_NAME_BYTES {
            return Err(SetupError::InvalidProvider);
        }
        self.provider = provider;
        self.step = SetupStep::Authentication;
        Ok(())
    }

    pub fn authenticate(
        &mut self,
        store: &dyn CredentialStore,
        account: impl Into<String>,
        secret: &str,
    ) -> Result<(), SetupError> {
        let account = account.into();
        store.set(&account, secret)?;
        self.credential_account = account;
        self.step = SetupStep::WorkspaceTrust;
        Ok(())
    }

    pub fn set_network(
        &mut self,
        proxy: Option<String>,
        certificate_path: Option<PathBuf>,
    ) -> Result<(), SetupError> {
        if proxy.as_deref().is_some_and(|value| {
            value.len() > MAX_PROXY_BYTES
                || !(value.starts_with("http://") || value.starts_with("https://"))
        }) {
            return Err(SetupError::InvalidProxy);
        }
        if let Some(path) = &certificate_path
            && !path.is_file()
        {
            return Err(SetupError::InvalidCertificate);
        }
        self.proxy = proxy;
        self.certificate_path = certificate_path;
        self.step = SetupStep::Model;
        Ok(())
    }

    pub fn set_workspace_trust(&mut self, trusted: bool) {
        self.workspace_trusted = trusted;
        self.step = SetupStep::Network;
    }

    pub fn set_model(
        &mut self,
        model: impl Into<String>,
        thinking: impl Into<String>,
    ) -> Result<(), SetupError> {
        let model = model.into();
        let thinking = thinking.into();
        if model.trim().is_empty() || model.len() > MAX_SETUP_NAME_BYTES {
            return Err(SetupError::InvalidModel);
        }
        if !matches!(thinking.as_str(), "off" | "low" | "medium" | "high" | "max") {
            return Err(SetupError::InvalidThinking);
        }
        self.model = model;
        self.thinking = thinking;
        self.step = SetupStep::Preferences;
        Ok(())
    }

    pub fn set_preferences(&mut self, preferences: Preferences) {
        self.preferences = preferences;
    }

    pub fn complete(&mut self, store: &dyn CredentialStore) -> Result<SetupResources, SetupError> {
        if store.get(&self.credential_account)?.is_none() {
            return Err(SetupError::AuthenticationRequired);
        }
        self.step = SetupStep::Complete;
        Ok(SetupResources {
            provider: self.provider.clone(),
            credential_handle: self.credential_account.clone(),
            workspace_trusted: self.workspace_trusted,
            proxy: self.proxy.clone(),
            certificate_path: self.certificate_path.clone(),
            model: self.model.clone(),
            thinking: self.thinking.clone(),
        })
    }
}

pub struct SetupFlow {
    pub state: SetupState,
}

pub struct SetupCompletion {
    pub resources: SetupResources,
    pub preferences: Preferences,
}

pub enum SetupProgress {
    Continue { prompt: String, secret_input: bool },
    Complete(SetupCompletion),
}

impl SetupFlow {
    #[must_use]
    pub fn new(
        provider: impl Into<String>,
        credential_account: impl Into<String>,
        workspace_trusted: bool,
        model: impl Into<String>,
    ) -> Self {
        Self {
            state: SetupState {
                provider: provider.into(),
                credential_account: credential_account.into(),
                workspace_trusted,
                model: model.into(),
                ..SetupState::default()
            },
        }
    }

    #[must_use]
    pub fn prompt(&self) -> String {
        match self.state.step {
            SetupStep::Provider => format!(
                "Provider [{}]. Enter a provider name or `default`.",
                self.state.provider
            ),
            SetupStep::Authentication => format!(
                "Enter the API key for `{}`. Input is masked.",
                self.state.credential_account
            ),
            SetupStep::WorkspaceTrust => {
                "Trust this workspace? Enter `yes` or `no`.".to_owned()
            }
            SetupStep::Network => {
                "Network: enter `off` or `proxy=<url> ca=<certificate-path>`.".to_owned()
            }
            SetupStep::Model => format!(
                "Model [{}]: enter `<model> <off|low|medium|high|max>`.",
                self.state.model
            ),
            SetupStep::Preferences => {
                "Preferences: enter `<system|light|dark> <off|unfocused|always> <updates-on|updates-off>`."
                    .to_owned()
            }
            SetupStep::Complete => "Setup is complete.".to_owned(),
        }
    }

    pub fn submit(
        &mut self,
        input: &str,
        store: &dyn CredentialStore,
    ) -> Result<SetupProgress, SetupError> {
        if input.len() > MAX_SETUP_INPUT_BYTES {
            return Err(SetupError::InputTooLarge);
        }
        match self.state.step {
            SetupStep::Provider => {
                let provider = match input.trim() {
                    "default" => self.state.provider.clone(),
                    provider => provider.to_owned(),
                };
                self.state.select_provider(provider)?;
            }
            SetupStep::Authentication => {
                self.state
                    .authenticate(store, self.state.credential_account.clone(), input)?;
            }
            SetupStep::WorkspaceTrust => {
                let trusted = match input.trim().to_ascii_lowercase().as_str() {
                    "yes" | "y" | "true" => true,
                    "no" | "n" | "false" => false,
                    _ => {
                        return Err(SetupError::InvalidStepInput(
                            "workspace trust must be yes or no".to_owned(),
                        ));
                    }
                };
                self.state.set_workspace_trust(trusted);
            }
            SetupStep::Network => {
                let (proxy, certificate) = parse_network(input)?;
                self.state.set_network(proxy, certificate)?;
            }
            SetupStep::Model => {
                let mut values = input.split_whitespace();
                let model = values.next().ok_or_else(|| {
                    SetupError::InvalidStepInput("model name is required".to_owned())
                })?;
                let model = if model == "default" {
                    self.state.model.clone()
                } else {
                    model.to_owned()
                };
                let thinking = values.next().unwrap_or("off");
                if values.next().is_some() {
                    return Err(SetupError::InvalidStepInput(
                        "model step accepts exactly a model and thinking level".to_owned(),
                    ));
                }
                self.state.set_model(model, thinking)?;
            }
            SetupStep::Preferences => {
                self.state.set_preferences(parse_preferences(input.trim())?);
                let resources = self.state.complete(store)?;
                return Ok(SetupProgress::Complete(SetupCompletion {
                    resources,
                    preferences: self.state.preferences.clone(),
                }));
            }
            SetupStep::Complete => {
                return Err(SetupError::InvalidStepInput(
                    "setup is already complete".to_owned(),
                ));
            }
        }
        Ok(SetupProgress::Continue {
            prompt: self.prompt(),
            secret_input: self.state.step == SetupStep::Authentication,
        })
    }
}

fn parse_network(input: &str) -> Result<(Option<String>, Option<PathBuf>), SetupError> {
    if matches!(input.trim(), "off" | "none") {
        return Ok((None, None));
    }
    let mut proxy = None;
    let mut certificate = None;
    for value in input.split_whitespace() {
        if let Some(value) = value.strip_prefix("proxy=") {
            proxy = Some(value.to_owned());
        } else if let Some(value) = value.strip_prefix("ca=") {
            certificate = Some(PathBuf::from(value));
        } else {
            return Err(SetupError::InvalidStepInput(
                "network values must use proxy=<url> or ca=<path>".to_owned(),
            ));
        }
    }
    Ok((proxy, certificate))
}

fn parse_preferences(input: &str) -> Result<Preferences, SetupError> {
    let values = input.split_whitespace().collect::<Vec<_>>();
    let [theme, notifications, updates] = values.as_slice() else {
        return Err(SetupError::InvalidStepInput(
            "preferences require theme, notifications, and update-check values".to_owned(),
        ));
    };
    let theme = match *theme {
        "system" => Theme::System,
        "light" => Theme::Light,
        "dark" => Theme::Dark,
        _ => {
            return Err(SetupError::InvalidStepInput(
                "theme must be system, light, or dark".to_owned(),
            ));
        }
    };
    let notifications = match *notifications {
        "off" => NotificationPreference::Off,
        "unfocused" => NotificationPreference::WhenUnfocused,
        "always" => NotificationPreference::Always,
        _ => {
            return Err(SetupError::InvalidStepInput(
                "notifications must be off, unfocused, or always".to_owned(),
            ));
        }
    };
    let update_checks = match *updates {
        "updates-on" => true,
        "updates-off" => false,
        _ => {
            return Err(SetupError::InvalidStepInput(
                "updates must be updates-on or updates-off".to_owned(),
            ));
        }
    };
    Ok(Preferences {
        theme,
        notifications,
        update_checks,
        voice_device: None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedTheme {
    Light,
    Dark,
    Unknown,
}

pub trait TerminalThemeDetector {
    fn detect(&self) -> DetectedTheme;
}

pub struct EnvironmentThemeDetector;

impl TerminalThemeDetector for EnvironmentThemeDetector {
    fn detect(&self) -> DetectedTheme {
        detect_terminal_theme(
            std::env::var("VIBE_TERMINAL_THEME").ok().as_deref(),
            std::env::var("COLORFGBG").ok().as_deref(),
        )
    }
}

#[must_use]
pub fn detect_terminal_theme(
    explicit: Option<&str>,
    color_foreground_background: Option<&str>,
) -> DetectedTheme {
    match explicit
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("light") => return DetectedTheme::Light,
        Some("dark") => return DetectedTheme::Dark,
        _ => {}
    }
    let background = color_foreground_background
        .and_then(|value| value.rsplit(';').next())
        .and_then(|value| value.parse::<u8>().ok());
    match background {
        Some(0..=6 | 8) => DetectedTheme::Dark,
        Some(7 | 9..=15) => DetectedTheme::Light,
        _ => DetectedTheme::Unknown,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedTheme {
    pub theme: Theme,
    pub colors_enabled: bool,
}

#[must_use]
pub fn resolve_theme(preference: Theme, detected: DetectedTheme, no_color: bool) -> ResolvedTheme {
    let theme = match (preference, detected) {
        (Theme::System, DetectedTheme::Light) => Theme::Light,
        (Theme::System, DetectedTheme::Dark | DetectedTheme::Unknown) => Theme::Dark,
        (explicit, _) => explicit,
    };
    ResolvedTheme {
        theme,
        colors_enabled: !no_color,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateState {
    Idle,
    Checking,
    Available { version: String },
    Current,
    Failed { message: String },
}

impl UpdateState {
    pub fn fail(&mut self) {
        *self = Self::Failed {
            message: "Update check failed; the installed version remains usable".to_owned(),
        };
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceState {
    Idle,
    Recording { device: String, elapsed_ms: u64 },
    Transcribing,
    Playing,
    Cancelled,
    Failed { message: String },
}

impl VoiceState {
    pub fn start(
        &mut self,
        device: Option<&str>,
        available_devices: &[String],
    ) -> Result<(), SetupError> {
        let selected = device
            .filter(|device| {
                available_devices
                    .iter()
                    .any(|available| available == device)
            })
            .or_else(|| available_devices.first().map(String::as_str))
            .ok_or(SetupError::AudioUnavailable)?;
        *self = Self::Recording {
            device: selected.to_owned(),
            elapsed_ms: 0,
        };
        Ok(())
    }

    pub fn tick(&mut self, elapsed_ms: u64, limit_ms: u64) -> Result<(), SetupError> {
        let Self::Recording {
            elapsed_ms: current,
            ..
        } = self
        else {
            return Err(SetupError::VoiceNotRecording);
        };
        *current = current.saturating_add(elapsed_ms);
        if *current > limit_ms {
            *self = Self::Failed {
                message: "Voice recording reached its configured limit".to_owned(),
            };
            return Err(SetupError::VoiceLimit);
        }
        Ok(())
    }

    pub fn transcribe(&mut self) -> Result<(), SetupError> {
        if !matches!(self, Self::Recording { .. }) {
            return Err(SetupError::VoiceNotRecording);
        }
        *self = Self::Transcribing;
        Ok(())
    }

    pub fn cancel(&mut self) {
        *self = Self::Cancelled;
    }
}

#[derive(Debug, Error)]
pub enum SetupError {
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error("provider authentication is required")]
    AuthenticationRequired,
    #[error("setup input exceeds the safety limit")]
    InputTooLarge,
    #[error("provider name is required")]
    InvalidProvider,
    #[error("model name is required")]
    InvalidModel,
    #[error("thinking must be off, low, medium, high, or max")]
    InvalidThinking,
    #[error("{0}")]
    InvalidStepInput(String),
    #[error("proxy URL must use HTTP or HTTPS")]
    InvalidProxy,
    #[error("TLS certificate path is unavailable")]
    InvalidCertificate,
    #[error("no audio input device is available")]
    AudioUnavailable,
    #[error("voice recording is not active")]
    VoiceNotRecording,
    #[error("voice recording exceeded its configured limit")]
    VoiceLimit,
    #[error("preference file could not be read or written: {0}")]
    PreferenceIo(#[source] std::io::Error),
    #[error("preference file is invalid: {0}")]
    PreferenceDecode(#[source] toml::de::Error),
    #[error("preferences could not be encoded: {0}")]
    PreferenceEncode(#[source] toml::ser::Error),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct MemoryCredentials {
        values: Mutex<BTreeMap<String, String>>,
        unavailable: bool,
    }

    impl CredentialStore for MemoryCredentials {
        fn set(&self, account: &str, secret: &str) -> Result<(), CredentialError> {
            if self.unavailable {
                return Err(CredentialError::Unavailable);
            }
            self.values
                .lock()
                .expect("credential state")
                .insert(account.to_owned(), secret.to_owned());
            Ok(())
        }

        fn get(&self, account: &str) -> Result<Option<String>, CredentialError> {
            if self.unavailable {
                return Err(CredentialError::Unavailable);
            }
            Ok(self
                .values
                .lock()
                .expect("credential state")
                .get(account)
                .cloned())
        }
    }

    #[test]
    fn setup_persists_a_handle_without_projecting_the_secret() {
        let credentials = MemoryCredentials::default();
        let mut state = SetupState::default();
        state
            .authenticate(&credentials, "account", "never-project-this")
            .expect("credential stores");
        state.workspace_trusted = true;
        let resources = state.complete(&credentials).expect("setup completes");
        let public = serde_json::to_string(&resources).expect("resources encode");
        assert!(public.contains("account"));
        assert!(!public.contains("never-project-this"));
    }

    #[test]
    fn unavailable_keyring_has_a_non_secret_recovery_path() {
        let credentials = MemoryCredentials {
            unavailable: true,
            ..MemoryCredentials::default()
        };
        let mut state = SetupState::default();
        let error = state
            .authenticate(&credentials, "account", "secret")
            .expect_err("keyring unavailable");
        assert_eq!(
            error.to_string(),
            "the API key could not be saved: the OS keyring refused it and the fallback file \
             under the vibe home could not be written"
        );
        assert!(!error.to_string().contains("secret"));
    }

    #[test]
    fn a_persisted_store_with_no_keyring_falls_back_to_the_dotenv_file() {
        let temporary = tempfile::tempdir().expect("temporary vibe home");
        let env_file = temporary.path().join(".env");
        // A disabled store answers as if no OS keyring existed, so the save
        // must degrade to the dotenv fallback rather than fail.
        let store = PersistedCredentialStore::with_store(
            env_file.clone(),
            true,
            vibe_core::auth::KeyringStore::disabled(Box::new(
                vibe_core::auth::NativeKeyringBackend::new(),
            )),
        );
        store
            .set("MISTRAL_API_KEY", "typed-key")
            .expect("the fallback save completes");
        assert_eq!(
            store.resolve("MISTRAL_API_KEY").as_deref(),
            Some("typed-key")
        );
        let contents = fs::read_to_string(&env_file).expect("the fallback file exists");
        assert!(contents.contains("typed-key"));
    }

    #[test]
    fn theme_no_color_and_preferences_are_deterministic() {
        let temporary = tempfile::tempdir().expect("temporary preferences");
        let store = PreferenceStore::new(temporary.path().join("preferences.toml"));
        let preferences = Preferences {
            theme: Theme::System,
            notifications: NotificationPreference::Always,
            update_checks: false,
            voice_device: Some("microphone".to_owned()),
        };
        store.save(&preferences).expect("preferences save");
        assert_eq!(store.load().expect("preferences load"), preferences);
        assert_eq!(
            resolve_theme(Theme::System, DetectedTheme::Light, true),
            ResolvedTheme {
                theme: Theme::Light,
                colors_enabled: false,
            }
        );
    }

    #[test]
    fn terminal_theme_detection_is_deterministic_and_keeps_an_unknown_fallback() {
        assert_eq!(
            detect_terminal_theme(Some("light"), Some("15;0")),
            DetectedTheme::Light
        );
        assert_eq!(
            detect_terminal_theme(None, Some("15;0")),
            DetectedTheme::Dark
        );
        assert_eq!(
            detect_terminal_theme(None, Some("0;15")),
            DetectedTheme::Light
        );
        assert_eq!(detect_terminal_theme(None, None), DetectedTheme::Unknown);
        assert!(!resolve_theme(Theme::System, DetectedTheme::Light, true).colors_enabled);
    }

    #[test]
    fn setup_state_advances_through_every_bounded_production_step() {
        let credentials = MemoryCredentials::default();
        let mut state = SetupState::default();
        state.select_provider("mistral").expect("provider");
        assert_eq!(state.step, SetupStep::Authentication);
        state
            .authenticate(&credentials, "account", "secret")
            .expect("authentication");
        state.set_workspace_trust(true);
        assert_eq!(state.step, SetupStep::Network);
        state.set_network(None, None).expect("network");
        state.set_model("configured-model", "high").expect("model");
        state.set_preferences(Preferences {
            theme: Theme::Light,
            notifications: NotificationPreference::Always,
            update_checks: false,
            voice_device: None,
        });
        let resources = state.complete(&credentials).expect("complete");
        assert_eq!(state.step, SetupStep::Complete);
        assert_eq!(resources.model, "configured-model");
        assert_eq!(resources.thinking, "high");
    }

    #[test]
    fn setup_flow_requires_every_production_step_before_completion() {
        let temporary = tempfile::tempdir().expect("temporary certificate");
        let certificate = temporary.path().join("ca.pem");
        fs::write(&certificate, "certificate").expect("certificate fixture");
        let credentials = MemoryCredentials::default();
        let mut flow = SetupFlow::new("mistral", "account", false, "default-model");

        let progress = flow.submit("default", &credentials).expect("provider");
        assert!(matches!(
            progress,
            SetupProgress::Continue {
                secret_input: true,
                ..
            }
        ));
        flow.submit("secret", &credentials).expect("authentication");
        flow.submit("yes", &credentials).expect("workspace trust");
        flow.submit(
            &format!("proxy=https://proxy.example ca={}", certificate.display()),
            &credentials,
        )
        .expect("network");
        flow.submit("configured-model high", &credentials)
            .expect("model");
        let completion = flow
            .submit("light always updates-off", &credentials)
            .expect("preferences");
        let SetupProgress::Complete(completion) = completion else {
            panic!("preferences must complete setup");
        };
        assert_eq!(completion.resources.model, "configured-model");
        assert_eq!(completion.resources.thinking, "high");
        assert_eq!(completion.preferences.theme, Theme::Light);
        assert!(!completion.preferences.update_checks);
    }

    #[test]
    fn oversized_setup_input_is_rejected_without_advancing_state() {
        let credentials = MemoryCredentials::default();
        let mut flow = SetupFlow::new("mistral", "account", false, "model");
        let result = flow.submit(&"x".repeat(MAX_SETUP_INPUT_BYTES + 1), &credentials);
        assert!(matches!(result, Err(SetupError::InputTooLarge)));
        assert_eq!(flow.state.step, SetupStep::Provider);
    }

    #[test]
    fn voice_limits_and_missing_devices_keep_prompt_recoverable() {
        let mut state = VoiceState::Idle;
        assert!(matches!(
            state.start(None, &[]),
            Err(SetupError::AudioUnavailable)
        ));
        state
            .start(Some("mic"), &["mic".to_owned()])
            .expect("voice starts");
        assert!(matches!(
            state.tick(1_001, 1_000),
            Err(SetupError::VoiceLimit)
        ));
        state.cancel();
        assert_eq!(state, VoiceState::Cancelled);
    }
}
