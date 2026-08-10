//! The onboarding screen graph as a pure state machine.
//!
//! Reference `vibe/setup/onboarding/`: seven screens for a browser-capable
//! provider and three for any other, navigated by replacement with no stack,
//! terminating with one of five values. The machine consumes key presses,
//! timer expirations and sign-in service events, and answers with effects the
//! driver executes: starting or cancelling a sign-in worker, scheduling the
//! URL help and the success exit, copying the sign-in URL, and exiting. The
//! parity replay drives this machine directly, the way the reference corpus
//! was captured by driving the real `OnboardingApp` through Textual's
//! headless pilot.

use std::fmt::Write as _;

use toml::{Table, Value};
use vibe_core::auth::{
    DEFAULT_BROWSER_AUTH_API_BASE_URL, DEFAULT_BROWSER_AUTH_BASE_URL, PersistOutcome,
    SignInErrorCode, SignInStatus,
};

use super::context::{self, DomainFeedback, OnboardingContext};
use crate::tui::themes::sorted_theme_names;

/// Reference `SUCCESS_EXIT_DELAY_SECONDS`: how long the success state holds
/// before the flow terminates with the completion value.
pub const SUCCESS_EXIT_DELAY_SECONDS: f64 = 2.0;

/// Reference `SIGN_IN_URL_HELP_DELAY_SECONDS`: how long an attempt runs
/// before the URL help is revealed.
pub const SIGN_IN_URL_HELP_DELAY_SECONDS: f64 = 4.0;

/// Reference `BrowserSignInStep` variant names, in step order.
pub const SIGN_IN_STEP_NAMES: [&str; 3] = ["OPEN", "CONFIRM", "FINISH"];

/// Reference `VISIBLE_NEIGHBORS`: themes shown on each side of the selection.
pub const THEME_VISIBLE_NEIGHBORS: usize = 3;

/// Reference `FADE_CLASSES`: the fade level per distance from the selection.
pub const THEME_FADE_CLASSES: [&str; 3] = ["fade-1", "fade-2", "fade-3"];

/// Reference `GRADIENT_COLORS`: the welcome highlight cycles through this
/// table by character index, never by interpolation.
pub const GRADIENT_COLORS: [&str; 10] = [
    "#ff6b00", "#ff7b00", "#ff8c00", "#ff9d00", "#ffae00", "#ffbf00", "#ffae00", "#ff9d00",
    "#ff8c00", "#ff7b00",
];

/// The seven screens, named as the reference installs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScreenId {
    ApiKey,
    AuthMethod,
    BrowserSignIn,
    CustomDomain,
    SignInTarget,
    ThemeSelection,
    Welcome,
}

impl ScreenId {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::AuthMethod => "auth_method",
            Self::BrowserSignIn => "browser_sign_in",
            Self::CustomDomain => "custom_domain",
            Self::SignInTarget => "sign_in_target",
            Self::ThemeSelection => "theme_selection",
            Self::Welcome => "welcome",
        }
    }
}

/// The five values the flow can terminate with. Reference `OnboardingApp`
/// exits with `None` on a cancellation and with a string otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnboardingOutcome {
    Cancelled,
    Completed,
    EnvVarError { detail: String },
    SaveError { detail: String },
    ProviderConfigError { detail: String },
}

impl OnboardingOutcome {
    /// The reference's terminating value: `None` for a cancellation, the
    /// error code and its detail joined by a colon otherwise.
    #[must_use]
    pub fn as_reference_value(&self) -> Option<String> {
        match self {
            Self::Cancelled => None,
            Self::Completed => Some("completed".to_owned()),
            Self::EnvVarError { detail } => Some(format!("env_var_error:{detail}")),
            Self::SaveError { detail } => Some(format!("save_error:{detail}")),
            Self::ProviderConfigError { detail } => Some(format!("provider_config_error:{detail}")),
        }
    }
}

/// The two persistence primitives the flow reaches outside itself, which is
/// exactly what the reference corpus records: each key save with its
/// resolved variable, and each provider write.
pub trait OnboardingPorts {
    /// Persists the API key for the resolved provider. Reference
    /// `persist_api_key`; the outcome vocabulary is the reference's.
    fn persist_api_key(
        &mut self,
        env_key: &str,
        provider: &Table,
        api_key: &str,
        custom_domain: bool,
    ) -> PersistOutcome;

    /// Upserts the provider entry into the configuration. Reference
    /// `persist_provider_to_config`: answers whether the write landed.
    fn persist_provider(&mut self, provider: &Table) -> bool;
}

/// A key press the driver already normalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPress {
    Enter,
    Escape,
    CtrlC,
    Up,
    Down,
    Backspace,
    Char(char),
}

/// Everything the machine consumes.
#[derive(Debug, Clone)]
pub enum ModelEvent {
    Key(KeyPress),
    /// The welcome text finished typing; the advance action arms.
    WelcomeTypingFinished,
    /// The sign-in worker announced its attempt and the URL to open.
    SignInStarted {
        attempt: u64,
        sign_in_url: String,
    },
    /// The sign-in worker advanced a status.
    SignInStatus {
        attempt: u64,
        status: SignInStatus,
    },
    /// The sign-in worker stopped with an error.
    SignInFailed {
        attempt: u64,
        code: Option<SignInErrorCode>,
        message: String,
    },
    /// The sign-in worker returned the credential.
    SignInCompleted {
        attempt: u64,
        api_key: String,
    },
    /// The URL help delay elapsed for an attempt.
    UrlHelpElapsed {
        attempt: u64,
    },
    /// The success hold elapsed; the flow terminates with completion.
    SuccessDelayElapsed,
}

/// Everything the machine asks the driver to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelEffect {
    /// Build a sign-in service from the working provider and run it.
    StartSignIn {
        attempt: u64,
    },
    /// Cancel the running worker; the worker closes its service on the way
    /// out, so a cancelled attempt never leaks its connection.
    CancelSignIn,
    CopyUrl {
        url: String,
    },
    ScheduleUrlHelp {
        attempt: u64,
    },
    ScheduleSuccessExit,
    Exit(OnboardingOutcome),
}

/// Reference `BrowserSignInViewState.variant`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInVariant {
    Pending,
    Error,
    Success,
}

impl SignInVariant {
    /// The class name the corpus records for this variant.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Error => "error",
            Self::Success => "success",
        }
    }
}

/// Reference `BrowserSignInViewState`, minus the rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInViewState {
    /// 0 opens the browser, 1 confirms the sign-in, 2 finishes setup.
    pub step: usize,
    pub message: String,
    pub variant: SignInVariant,
    pub running: bool,
    pub sign_in_url: Option<String>,
    pub show_url_help: bool,
    pub reveal_url: bool,
}

impl SignInViewState {
    fn initial() -> Self {
        Self {
            step: 0,
            message: "Preparing the sign-in...".to_owned(),
            variant: SignInVariant::Pending,
            running: false,
            sign_in_url: None,
            show_url_help: false,
            reveal_url: false,
        }
    }
}

/// The sign-in target options, in display order.
const TARGET_MISTRAL: usize = 0;
const TARGET_OTHER: usize = 1;

/// The first authentication method option: the browser sign-in; the second
/// is the manual key entry.
const METHOD_BROWSER: usize = 0;

/// The machine. One instance is one run of the flow.
pub struct OnboardingModel {
    context: OnboardingContext,
    provider: Table,
    supports_browser: bool,
    screen: ScreenId,
    outcome: Option<OnboardingOutcome>,
    typing_done: bool,
    themes: Vec<&'static str>,
    theme_index: usize,
    method_index: usize,
    target_index: usize,
    override_armed: bool,
    domain_value: String,
    domain_feedback_visible: bool,
    key_value: String,
    key_feedback: Option<bool>,
    attempt_number: u64,
    active_attempt: Option<u64>,
    sign_in: SignInViewState,
}

impl OnboardingModel {
    #[must_use]
    pub fn new(context: OnboardingContext) -> Self {
        let themes = sorted_theme_names();
        let theme_index = themes
            .iter()
            .position(|name| *name == context.theme)
            .unwrap_or(0);
        Self {
            provider: context.provider.clone(),
            supports_browser: context.supports_browser_sign_in(),
            context,
            screen: ScreenId::Welcome,
            outcome: None,
            typing_done: false,
            themes,
            theme_index,
            method_index: METHOD_BROWSER,
            target_index: TARGET_MISTRAL,
            override_armed: false,
            domain_value: String::new(),
            domain_feedback_visible: false,
            key_value: String::new(),
            key_feedback: None,
            attempt_number: 0,
            active_attempt: None,
            sign_in: SignInViewState::initial(),
        }
    }

    // ----------------------------------------------------------------------
    // Observations
    // ----------------------------------------------------------------------

    /// The screens this run installed, sorted by name as the corpus lists
    /// them: seven for a browser-capable provider, three otherwise.
    #[must_use]
    pub fn installed_screens(&self) -> Vec<&'static str> {
        if self.supports_browser {
            vec![
                ScreenId::ApiKey,
                ScreenId::AuthMethod,
                ScreenId::BrowserSignIn,
                ScreenId::CustomDomain,
                ScreenId::SignInTarget,
                ScreenId::ThemeSelection,
                ScreenId::Welcome,
            ]
        } else {
            vec![
                ScreenId::ApiKey,
                ScreenId::ThemeSelection,
                ScreenId::Welcome,
            ]
        }
        .into_iter()
        .map(ScreenId::name)
        .collect()
    }

    #[must_use]
    pub const fn current_screen(&self) -> ScreenId {
        self.screen
    }

    #[must_use]
    pub const fn outcome(&self) -> Option<&OnboardingOutcome> {
        self.outcome.as_ref()
    }

    #[must_use]
    pub fn selected_theme(&self) -> &'static str {
        self.themes[self.theme_index % self.themes.len()]
    }

    #[must_use]
    pub fn themes(&self) -> &[&'static str] {
        &self.themes
    }

    /// The theme at `offset` from the selection, wrapping at both ends.
    #[must_use]
    pub fn theme_at_offset(&self, offset: isize) -> &'static str {
        let length = self.themes.len() as isize;
        let index = (self.theme_index as isize + offset).rem_euclid(length);
        self.themes[index as usize]
    }

    /// The focused input of the current screen, as widget id and kind; the
    /// option and progress screens focus the screen itself.
    #[must_use]
    pub const fn focus(&self) -> Option<(&'static str, &'static str)> {
        match self.screen {
            ScreenId::CustomDomain => Some(("domain", "Input")),
            ScreenId::ApiKey => Some(("key", "Input")),
            _ => None,
        }
    }

    /// The key input is always masked; it never echoes.
    #[must_use]
    pub const fn key_masked(&self) -> bool {
        true
    }

    #[must_use]
    pub fn key_value_len(&self) -> usize {
        self.key_value.chars().count()
    }

    #[must_use]
    pub const fn key_feedback(&self) -> Option<bool> {
        self.key_feedback
    }

    /// The working provider entry, carrying any applied domain.
    #[must_use]
    pub const fn provider(&self) -> &Table {
        &self.provider
    }

    /// The provider entry the flow started from.
    #[must_use]
    pub const fn initial_provider(&self) -> &Table {
        &self.context.provider
    }

    #[must_use]
    pub fn vibe_base_url(&self) -> &str {
        &self.context.vibe_base_url
    }

    #[must_use]
    pub const fn supports_browser_sign_in(&self) -> bool {
        self.supports_browser
    }

    #[must_use]
    pub const fn typing_done(&self) -> bool {
        self.typing_done
    }

    #[must_use]
    pub const fn method_selection(&self) -> usize {
        self.method_index
    }

    #[must_use]
    pub const fn target_selection(&self) -> usize {
        self.target_index
    }

    #[must_use]
    pub const fn override_armed(&self) -> bool {
        self.override_armed
    }

    #[must_use]
    pub fn domain_value(&self) -> &str {
        &self.domain_value
    }

    /// The validation classes the domain input currently shows, `None` while
    /// feedback is suppressed after a reset.
    #[must_use]
    pub fn domain_feedback(&self) -> Option<DomainFeedback> {
        self.domain_feedback_visible
            .then(|| context::domain_feedback(&self.domain_value))
    }

    #[must_use]
    pub const fn sign_in_view(&self) -> &SignInViewState {
        &self.sign_in
    }

    /// The applied browser-auth URLs, empty strings reading as absent, which
    /// is how the corpus records a provider that cannot browser sign-in.
    #[must_use]
    pub fn provider_browser_auth(&self) -> (Option<&str>, Option<&str>) {
        let field = |key: &str| {
            self.provider
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        };
        (
            field("browser_auth_base_url"),
            field("browser_auth_api_base_url"),
        )
    }

    // ----------------------------------------------------------------------
    // The event loop
    // ----------------------------------------------------------------------

    pub fn handle(
        &mut self,
        event: ModelEvent,
        ports: &mut dyn OnboardingPorts,
    ) -> Vec<ModelEffect> {
        if self.outcome.is_some() {
            return Vec::new();
        }
        match event {
            ModelEvent::Key(key) => self.handle_key(key, ports),
            ModelEvent::WelcomeTypingFinished => {
                self.typing_done = true;
                Vec::new()
            }
            ModelEvent::SignInStarted {
                attempt,
                sign_in_url,
            } => {
                if !self.attempt_active(attempt) {
                    return Vec::new();
                }
                self.sign_in.sign_in_url = Some(sign_in_url);
                self.sign_in.show_url_help = false;
                self.sign_in.reveal_url = false;
                vec![ModelEffect::ScheduleUrlHelp { attempt }]
            }
            ModelEvent::SignInStatus { attempt, status } => {
                if self.attempt_active(attempt) {
                    self.apply_sign_in_status(status);
                }
                Vec::new()
            }
            ModelEvent::SignInFailed {
                attempt,
                code,
                message,
            } => {
                if !self.attempt_active(attempt) {
                    return Vec::new();
                }
                self.active_attempt = None;
                self.sign_in.variant = SignInVariant::Error;
                self.sign_in.running = false;
                self.sign_in.message = if code == Some(SignInErrorCode::PollFailed) {
                    "The sign-in could not be confirmed. Retry when you are ready.".to_owned()
                } else {
                    message
                };
                self.sign_in.show_url_help = self.sign_in.sign_in_url.is_some();
                self.sign_in.reveal_url = false;
                Vec::new()
            }
            ModelEvent::SignInCompleted { attempt, api_key } => {
                if !self.attempt_active(attempt) {
                    return Vec::new();
                }
                let outcome = self.persist_credentials(&api_key, ports);
                if outcome != OnboardingOutcome::Completed {
                    self.active_attempt = None;
                    return self.exit_with(outcome);
                }
                self.sign_in.step = 2;
                self.sign_in.message = "Signed in.".to_owned();
                self.sign_in.variant = SignInVariant::Success;
                self.sign_in.running = true;
                self.sign_in.show_url_help = false;
                self.sign_in.reveal_url = false;
                vec![ModelEffect::ScheduleSuccessExit]
            }
            ModelEvent::UrlHelpElapsed { attempt } => {
                if self.attempt_active(attempt) && self.sign_in.sign_in_url.is_some() {
                    self.sign_in.show_url_help = true;
                }
                Vec::new()
            }
            ModelEvent::SuccessDelayElapsed => {
                if self.screen == ScreenId::BrowserSignIn
                    && self.sign_in.variant == SignInVariant::Success
                {
                    self.active_attempt = None;
                    self.exit_with(OnboardingOutcome::Completed)
                } else {
                    Vec::new()
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyPress, ports: &mut dyn OnboardingPorts) -> Vec<ModelEffect> {
        match self.screen {
            ScreenId::Welcome => match key {
                KeyPress::Enter if self.typing_done => self.switch(ScreenId::ThemeSelection),
                KeyPress::Escape | KeyPress::CtrlC => self.cancel(),
                _ => Vec::new(),
            },
            ScreenId::ThemeSelection => match key {
                KeyPress::Up => {
                    self.theme_index =
                        (self.theme_index + self.themes.len() - 1) % self.themes.len();
                    Vec::new()
                }
                KeyPress::Down => {
                    self.theme_index = (self.theme_index + 1) % self.themes.len();
                    Vec::new()
                }
                KeyPress::Enter => {
                    let next = if self.supports_browser {
                        ScreenId::AuthMethod
                    } else {
                        ScreenId::ApiKey
                    };
                    self.switch(next)
                }
                KeyPress::Escape | KeyPress::CtrlC => self.cancel(),
                _ => Vec::new(),
            },
            ScreenId::AuthMethod => match key {
                KeyPress::Up | KeyPress::Down => {
                    self.method_index = (self.method_index + 1) % 2;
                    Vec::new()
                }
                KeyPress::Enter => {
                    if self.method_index == METHOD_BROWSER {
                        self.switch(ScreenId::SignInTarget)
                    } else {
                        self.switch(ScreenId::ApiKey)
                    }
                }
                KeyPress::Escape | KeyPress::CtrlC => self.cancel(),
                _ => Vec::new(),
            },
            ScreenId::SignInTarget => match key {
                KeyPress::Up | KeyPress::Down => {
                    self.override_armed = false;
                    self.target_index = (self.target_index + 1) % 2;
                    Vec::new()
                }
                KeyPress::Enter => self.select_sign_in_target(),
                KeyPress::Escape => self.switch(ScreenId::AuthMethod),
                KeyPress::CtrlC => self.cancel(),
                _ => Vec::new(),
            },
            ScreenId::CustomDomain => match key {
                KeyPress::Char(character) => {
                    self.domain_value.push(character);
                    self.domain_feedback_visible = true;
                    Vec::new()
                }
                KeyPress::Backspace => {
                    self.domain_value.pop();
                    self.domain_feedback_visible = true;
                    Vec::new()
                }
                KeyPress::Enter => self.submit_domain(),
                KeyPress::Escape => self.switch(ScreenId::SignInTarget),
                KeyPress::CtrlC => self.cancel(),
                _ => Vec::new(),
            },
            ScreenId::BrowserSignIn => match key {
                KeyPress::Char('r') => {
                    if self.sign_in.running {
                        Vec::new()
                    } else {
                        self.start_attempt()
                    }
                }
                KeyPress::Char('m') => {
                    if self.sign_in.variant == SignInVariant::Success {
                        return Vec::new();
                    }
                    let mut effects = self.cancel_attempt();
                    effects.extend(self.switch(ScreenId::ApiKey));
                    effects
                }
                KeyPress::Char('c') => {
                    if self.sign_in.variant == SignInVariant::Success {
                        return Vec::new();
                    }
                    let Some(url) = self.sign_in.sign_in_url.clone() else {
                        return Vec::new();
                    };
                    self.sign_in.show_url_help = true;
                    self.sign_in.reveal_url = true;
                    vec![ModelEffect::CopyUrl { url }]
                }
                KeyPress::Escape | KeyPress::CtrlC => {
                    if self.sign_in.variant == SignInVariant::Success {
                        return Vec::new();
                    }
                    let mut effects = self.cancel_attempt();
                    effects.extend(self.cancel());
                    effects
                }
                _ => Vec::new(),
            },
            ScreenId::ApiKey => match key {
                KeyPress::Char(character) => {
                    self.key_value.push(character);
                    self.key_feedback = Some(!self.key_value.is_empty());
                    Vec::new()
                }
                KeyPress::Backspace => {
                    self.key_value.pop();
                    self.key_feedback = Some(!self.key_value.is_empty());
                    Vec::new()
                }
                KeyPress::Enter => {
                    if self.key_value.is_empty() {
                        self.key_feedback = Some(false);
                        return Vec::new();
                    }
                    let key = self.key_value.clone();
                    let outcome = self.persist_credentials(&key, ports);
                    self.exit_with(outcome)
                }
                KeyPress::Escape | KeyPress::CtrlC => self.cancel(),
                _ => Vec::new(),
            },
        }
    }

    // ----------------------------------------------------------------------
    // Screen actions
    // ----------------------------------------------------------------------

    /// Reference `SignInTargetScreen.action_select`: the other option opens
    /// the domain screen, and the default target overwrites a configured
    /// custom domain only after a second confirmation.
    fn select_sign_in_target(&mut self) -> Vec<ModelEffect> {
        if self.target_index == TARGET_OTHER {
            self.override_armed = false;
            return self.switch(ScreenId::CustomDomain);
        }
        if context::configured_custom_domain(&self.context.provider).is_some()
            && !self.override_armed
        {
            self.override_armed = true;
            return Vec::new();
        }
        self.apply_mistral_default_domain();
        self.switch(ScreenId::BrowserSignIn)
    }

    /// Reference `CustomDomainScreen.on_input_submitted`: an invalid value
    /// keeps the screen and shows the failure; a valid one derives the URLs
    /// and moves on to the sign-in.
    fn submit_domain(&mut self) -> Vec<ModelEffect> {
        let domain = self.domain_value.trim().to_owned();
        if !context::is_valid_custom_domain(&domain) {
            self.domain_feedback_visible = true;
            return Vec::new();
        }
        self.apply_custom_domain(&domain);
        self.switch(ScreenId::BrowserSignIn)
    }

    fn apply_custom_domain(&mut self, domain: &str) {
        let (base, api) = context::resolve_browser_auth_urls(domain);
        self.provider
            .insert("browser_auth_base_url".to_owned(), Value::String(base));
        self.provider
            .insert("browser_auth_api_base_url".to_owned(), Value::String(api));
    }

    fn apply_mistral_default_domain(&mut self) {
        self.provider.insert(
            "browser_auth_base_url".to_owned(),
            Value::String(DEFAULT_BROWSER_AUTH_BASE_URL.to_owned()),
        );
        self.provider.insert(
            "browser_auth_api_base_url".to_owned(),
            Value::String(DEFAULT_BROWSER_AUTH_API_BASE_URL.to_owned()),
        );
    }

    fn apply_sign_in_status(&mut self, status: SignInStatus) {
        let (step, message) = match status {
            SignInStatus::OpeningBrowser => (0, "Launching your browser..."),
            SignInStatus::WaitingForBrowserSignIn => (1, "Waiting while you sign in..."),
            SignInStatus::Exchanging | SignInStatus::Completed => (2, "Wrapping up..."),
        };
        self.sign_in.step = step;
        self.sign_in.message = message.to_owned();
        self.sign_in.variant = SignInVariant::Pending;
        self.sign_in.running = true;
    }

    /// Reference `OnboardingApp.persist_credentials`: the key goes to the
    /// resolved provider, and a modified provider entry follows it into the
    /// configuration only after the key landed.
    fn persist_credentials(
        &mut self,
        api_key: &str,
        ports: &mut dyn OnboardingPorts,
    ) -> OnboardingOutcome {
        let resolved = context::resolve_api_key_provider(&self.provider);
        let env_key = resolved
            .get("api_key_env_var")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let custom_domain = self
            .provider
            .get("browser_auth_base_url")
            .and_then(Value::as_str)
            .is_some_and(|base| !base.is_empty() && base != DEFAULT_BROWSER_AUTH_BASE_URL);
        match ports.persist_api_key(&env_key, &resolved, api_key, custom_domain) {
            PersistOutcome::Completed => {
                if self.provider != self.context.provider && !ports.persist_provider(&self.provider)
                {
                    return OnboardingOutcome::ProviderConfigError {
                        detail: "the provider entry could not be written".to_owned(),
                    };
                }
                OnboardingOutcome::Completed
            }
            PersistOutcome::EnvVarError { detail } => OnboardingOutcome::EnvVarError { detail },
            PersistOutcome::SaveError { detail } => OnboardingOutcome::SaveError { detail },
        }
    }

    // ----------------------------------------------------------------------
    // Plumbing
    // ----------------------------------------------------------------------

    /// Replaces the current screen, running the entered screen's reset
    /// behavior; there is no stack, so a back action names its target.
    fn switch(&mut self, to: ScreenId) -> Vec<ModelEffect> {
        self.screen = to;
        match to {
            ScreenId::SignInTarget => {
                self.override_armed = false;
                Vec::new()
            }
            ScreenId::CustomDomain => {
                let seed = context::configured_custom_domain(&self.context.provider)
                    .unwrap_or_default()
                    .to_owned();
                self.domain_value = seed;
                self.domain_feedback_visible = false;
                Vec::new()
            }
            ScreenId::BrowserSignIn => self.start_attempt(),
            _ => Vec::new(),
        }
    }

    fn start_attempt(&mut self) -> Vec<ModelEffect> {
        self.attempt_number += 1;
        self.active_attempt = Some(self.attempt_number);
        self.sign_in = SignInViewState {
            running: true,
            ..SignInViewState::initial()
        };
        vec![ModelEffect::StartSignIn {
            attempt: self.attempt_number,
        }]
    }

    /// Reference `_cancel_current_attempt`: the attempt stops being active
    /// before the worker unwinds, so its late events are ignored.
    fn cancel_attempt(&mut self) -> Vec<ModelEffect> {
        self.active_attempt = None;
        self.sign_in.running = false;
        vec![ModelEffect::CancelSignIn]
    }

    fn cancel(&mut self) -> Vec<ModelEffect> {
        self.exit_with(OnboardingOutcome::Cancelled)
    }

    fn exit_with(&mut self, outcome: OnboardingOutcome) -> Vec<ModelEffect> {
        self.outcome = Some(outcome.clone());
        vec![ModelEffect::Exit(outcome)]
    }

    fn attempt_active(&self, attempt: u64) -> bool {
        self.active_attempt == Some(attempt) && self.sign_in.running
    }
}

/// The gradient color for one character of the welcome highlight, cycling
/// the ten-entry table by index plus animation offset.
#[must_use]
pub fn gradient_color(character_index: usize, offset: usize) -> &'static str {
    GRADIENT_COLORS[(character_index + offset) % GRADIENT_COLORS.len()]
}

/// The fade class for a theme row at `offset` from the selection, `None` on
/// the selection itself. Reference `ThemeSelectionScreen._update_display`.
#[must_use]
pub fn theme_fade_class(offset: isize) -> Option<&'static str> {
    if offset == 0 {
        return None;
    }
    let distance = (offset.unsigned_abs() - 1).min(THEME_FADE_CLASSES.len() - 1);
    Some(THEME_FADE_CLASSES[distance])
}

/// The preview pane height for a terminal `height`, clamped to the
/// reference's floor of 7 rows under a 17-row header.
#[must_use]
pub fn theme_preview_height(height: u16) -> u16 {
    height.saturating_sub(17).max(7)
}

/// A masked rendering of a secret input: one bullet per character, so the
/// value itself never reaches a frame or a trace.
#[must_use]
pub fn masked(length: usize) -> String {
    let mut masked = String::new();
    for _ in 0..length {
        let _ = write!(masked, "\u{2022}");
    }
    masked
}
