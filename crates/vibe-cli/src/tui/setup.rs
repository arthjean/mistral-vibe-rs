//! The credential store the interactive CLI resolves and persists keys
//! through, and the terminal theme resolution.
//!
//! The first-run flow itself lives in [`super::onboarding`]; this module
//! keeps what outlives it: where a key is read from and written to, and how
//! a theme preference resolves against the terminal.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    Light,
    Dark,
    #[default]
    System,
}

/// The production store: `vibe_core::auth` persistence over the OS keyring
/// under the shared service names, with the global dotenv fallback that keeps
/// the key when the keyring cannot take it.
///
/// The reference exports the persisted key into `os.environ`; this port keeps
/// that overlay here, so a resolution after a save sees the key whichever
/// storage path accepted it.
pub struct PersistedCredentialStore {
    store: vibe_core::auth::KeyringStore,
    env_file: PathBuf,
    process_env: Mutex<std::collections::BTreeMap<String, String>>,
}

impl PersistedCredentialStore {
    #[must_use]
    pub fn new(env_file: PathBuf) -> Self {
        Self::with_store(env_file, vibe_core::auth::KeyringStore::native())
    }

    /// The same bridge over a caller-supplied keyring store, which is how the
    /// tests keep the OS keyring out of the loop.
    #[must_use]
    pub fn with_store(env_file: PathBuf, store: vibe_core::auth::KeyringStore) -> Self {
        Self {
            store,
            env_file,
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

    /// Persists `secret` for `env_key` with the reference's outcomes: the
    /// process-environment overlay, then the keyring, then the global dotenv
    /// when the keyring refuses, so a storage failure never discards the key.
    ///
    /// The mistral-backend flag drives only the report's local telemetry
    /// record; the reference sends an onboarding event there, and this port
    /// keeps the event local on the same terms as `telemetry/record`.
    pub fn persist_report(
        &self,
        env_key: &str,
        backend_is_mistral: bool,
        secret: &str,
        custom_domain: bool,
    ) -> vibe_core::auth::PersistReport {
        let Ok(mut overlay) = self.process_env.lock() else {
            return vibe_core::auth::PersistReport {
                outcome: vibe_core::auth::PersistOutcome::SaveError {
                    detail: "the in-process credential overlay is unavailable".to_owned(),
                },
                telemetry: Vec::new(),
            };
        };
        vibe_core::auth::persist_api_key(
            env_key,
            backend_is_mistral,
            secret,
            custom_domain,
            &mut overlay,
            &self.env_file,
            &self.store,
        )
    }
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn a_persisted_store_with_no_keyring_falls_back_to_the_dotenv_file() {
        let temporary = tempfile::tempdir().expect("temporary vibe home");
        let env_file = temporary.path().join(".env");
        // A disabled store answers as if no OS keyring existed, so the save
        // must degrade to the dotenv fallback rather than fail.
        let store = PersistedCredentialStore::with_store(
            env_file.clone(),
            vibe_core::auth::KeyringStore::disabled(Box::new(
                vibe_core::auth::NativeKeyringBackend::new(),
            )),
        );
        let report = store.persist_report("MISTRAL_API_KEY", true, "typed-key", false);
        assert_eq!(report.outcome, vibe_core::auth::PersistOutcome::Completed);
        assert_eq!(
            store.resolve("MISTRAL_API_KEY").as_deref(),
            Some("typed-key")
        );
        let contents = fs::read_to_string(&env_file).expect("the fallback file exists");
        assert!(contents.contains("typed-key"));
    }

    #[test]
    fn an_unusable_variable_name_writes_nothing_and_names_the_variable() {
        let temporary = tempfile::tempdir().expect("temporary vibe home");
        let env_file = temporary.path().join(".env");
        let store = PersistedCredentialStore::with_store(
            env_file.clone(),
            vibe_core::auth::KeyringStore::disabled(Box::new(
                vibe_core::auth::NativeKeyringBackend::new(),
            )),
        );
        let report = store.persist_report("BAD=KEY", true, "secret", false);
        assert_eq!(
            report.outcome,
            vibe_core::auth::PersistOutcome::EnvVarError {
                detail: "BAD=KEY".to_owned()
            }
        );
        assert!(!env_file.exists());
        // The failure never projects the secret anywhere it could be logged.
        assert!(!format!("{report:?}").contains("secret"));
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
    fn theme_resolution_is_deterministic() {
        assert_eq!(
            resolve_theme(Theme::System, DetectedTheme::Light, true),
            ResolvedTheme {
                theme: Theme::Light,
                colors_enabled: false,
            }
        );
        assert_eq!(
            resolve_theme(Theme::Dark, DetectedTheme::Light, false),
            ResolvedTheme {
                theme: Theme::Dark,
                colors_enabled: true,
            }
        );
    }
}
