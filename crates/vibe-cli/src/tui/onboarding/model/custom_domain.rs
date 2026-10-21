//! The custom-domain screen: its two inputs, their validation feedback, and
//! the browser-auth URLs a submitted domain derives (reference
//! `vibe/setup/onboarding/screens/custom_domain.py`).

use toml::Value;
use vibe_core::auth::{
    DEFAULT_BROWSER_AUTH_API_BASE_URL, DEFAULT_BROWSER_AUTH_BASE_URL, DEFAULT_CONSOLE_BASE_URL,
    DEFAULT_MISTRAL_API_BASE, DEFAULT_VIBE_BASE_URL, apply_browser_auth_urls,
    browser_auth_account_base,
};

use super::super::context::{self, FeedbackLine};
use super::{DomainInput, ModelEffect, OnboardingModel, ScreenId};

impl OnboardingModel {
    /// Reference `CustomDomainScreen.on_input_submitted`, from either input:
    /// an invalid domain, or a non-blank invalid API base, keeps the screen
    /// and shows the failure; otherwise the URLs are derived and the sign-in
    /// starts.
    pub(super) fn submit_domain(&mut self) -> Vec<ModelEffect> {
        let domain = self.domain_value.trim().to_owned();
        if !context::is_valid_custom_domain(&domain) {
            self.render_domain_feedback(&domain);
            return Vec::new();
        }
        let api_base = self.api_base_value.trim().to_owned();
        if !api_base.is_empty() && !context::is_valid_custom_domain(&api_base) {
            self.render_api_base_feedback(&api_base);
            return Vec::new();
        }
        self.apply_custom_domain(&domain, (!api_base.is_empty()).then_some(api_base.as_str()));
        self.switch(ScreenId::BrowserSignIn)
    }

    /// Reference `OnboardingApp.apply_custom_domain`: both browser-auth URLs,
    /// the origin rewrite a split between them needs, and the console account
    /// calls go to, which on a split-horizon deployment is the API origin.
    fn apply_custom_domain(&mut self, domain: &str, api_base_url: Option<&str>) {
        let (base, api) = context::resolve_browser_auth_urls(domain, api_base_url);
        apply_browser_auth_urls(&mut self.provider, &base, &api);
        self.console_base_url = browser_auth_account_base(&base, Some(&api));
    }

    /// Reference `OnboardingApp.apply_mistral_default_domain`: the shipped
    /// browser-auth URLs and API base, no rewrite, and the public console and
    /// chat bases.
    pub(super) fn apply_mistral_default_domain(&mut self) {
        let field = |value: &str| Value::String(value.to_owned());
        self.provider.insert(
            "browser_auth_base_url".to_owned(),
            field(DEFAULT_BROWSER_AUTH_BASE_URL),
        );
        self.provider.insert(
            "browser_auth_api_base_url".to_owned(),
            field(DEFAULT_BROWSER_AUTH_API_BASE_URL),
        );
        self.provider.insert(
            "browser_auth_allow_origin_rewrite".to_owned(),
            Value::Boolean(false),
        );
        self.provider
            .insert("api_base".to_owned(), field(DEFAULT_MISTRAL_API_BASE));
        DEFAULT_CONSOLE_BASE_URL.clone_into(&mut self.console_base_url);
        DEFAULT_VIBE_BASE_URL.clone_into(&mut self.vibe_base_url);
    }

    /// Replaces a custom-domain input's whole value at once, as assigning a
    /// widget's value does: one change when the value differs, none when it
    /// does not. Typing raises one change per keystroke instead.
    pub fn set_input_value(&mut self, input: DomainInput, value: &str) {
        if self.screen != ScreenId::CustomDomain || self.input_value_mut(input) == value {
            return;
        }
        value.clone_into(self.input_value_mut(input));
        self.input_changed(input);
    }

    pub(super) fn input_value_mut(&mut self, input: DomainInput) -> &mut String {
        match input {
            DomainInput::Domain => &mut self.domain_value,
            DomainInput::ApiBase => &mut self.api_base_value,
        }
    }

    /// Reference `CustomDomainScreen.on_input_changed`: a change the screen's
    /// reset caused renders nothing; any other renders the changed input.
    pub(super) fn input_changed(&mut self, input: DomainInput) {
        match input {
            DomainInput::Domain => {
                if std::mem::take(&mut self.suppress_domain_feedback) {
                    return;
                }
                let value = self.domain_value.clone();
                self.render_domain_feedback(&value);
            }
            DomainInput::ApiBase => {
                if std::mem::take(&mut self.suppress_api_base_feedback) {
                    return;
                }
                let value = self.api_base_value.clone();
                self.render_api_base_feedback(&value);
            }
        }
    }

    /// Reference `_render_domain_feedback`: the shared line and the domain
    /// box speak for the domain; the API base box keeps its classes.
    fn render_domain_feedback(&mut self, value: &str) {
        let feedback = context::domain_feedback(value);
        self.domain_box = Some(feedback);
        self.feedback_line = Some(FeedbackLine::Domain(feedback));
    }

    /// Reference `_render_api_base_feedback`: a blank value clears the shared
    /// line and the API base box.
    fn render_api_base_feedback(&mut self, value: &str) {
        let feedback = context::api_base_feedback(value);
        self.api_base_box = feedback;
        self.feedback_line = feedback.map(FeedbackLine::ApiBase);
    }

    /// Reference `CustomDomainScreen._reset_input`, run on every entry: both
    /// inputs take the configured values, the domain input takes the focus,
    /// and every class is cleared. Setting an input to a different value
    /// raises a change the screen swallows; setting it to the value it
    /// already holds raises none, so the swallow stays armed for the next
    /// keystroke.
    pub(super) fn reset_custom_domain(&mut self) {
        let domain_seed = context::configured_custom_domain(&self.context.provider)
            .unwrap_or_default()
            .to_owned();
        let api_seed =
            context::configured_custom_api_base(&self.context.provider).unwrap_or_default();
        self.suppress_domain_feedback = !domain_seed.is_empty() && domain_seed == self.domain_value;
        self.suppress_api_base_feedback = !api_seed.is_empty() && api_seed == self.api_base_value;
        self.domain_value = domain_seed;
        self.api_base_value = api_seed;
        self.domain_focus = DomainInput::Domain;
        self.domain_box = None;
        self.api_base_box = None;
        self.feedback_line = None;
    }
}
