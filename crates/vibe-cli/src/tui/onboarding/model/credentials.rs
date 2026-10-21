//! Persisting what the flow gathered: the API key, then the provider entry and
//! every base URL the flow moved.

use toml::Value;
use vibe_core::auth::{
    DEFAULT_BROWSER_AUTH_BASE_URL, DEFAULT_CONSOLE_BASE_URL, PersistOutcome,
    ProviderCredentialsRequest, same_provider,
};

use super::super::context;
use super::{OnboardingModel, OnboardingOutcome, OnboardingPorts};

impl OnboardingModel {
    /// Reference `OnboardingApp.persist_credentials`: the key goes to the
    /// resolved provider first. When the flow moved the provider or either
    /// base URL, a custom console is asked for its tenant's hosts, then the
    /// provider entry and every moved base URL are written in one batch, the
    /// first failed field naming the error.
    pub(super) fn persist_credentials(
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
            PersistOutcome::Completed => self.persist_provider_credentials(api_key, ports),
            PersistOutcome::EnvVarError { detail } => OnboardingOutcome::EnvVarError { detail },
            PersistOutcome::SaveError { detail } => OnboardingOutcome::SaveError { detail },
        }
    }

    fn persist_provider_credentials(
        &mut self,
        api_key: &str,
        ports: &mut dyn OnboardingPorts,
    ) -> OnboardingOutcome {
        if same_provider(&self.provider, &self.context.provider)
            && self.console_base_url == self.context.console_base_url
            && self.vibe_base_url == self.context.vibe_base_url
        {
            return OnboardingOutcome::Completed;
        }
        // The public console has no tenant to discover.
        if self.console_base_url != DEFAULT_CONSOLE_BASE_URL {
            let (provider, vibe_base_url) = ports.resolve_tenant_domains(
                self.provider.clone(),
                &self.console_base_url,
                api_key,
                &self.vibe_base_url,
            );
            self.provider = provider;
            self.vibe_base_url = vibe_base_url;
        }
        let moved =
            |working: &str, configured: &str| (working != configured).then(|| working.to_owned());
        let request = ProviderCredentialsRequest {
            provider: self.provider.clone(),
            console_base_url: moved(&self.console_base_url, &self.context.console_base_url),
            vibe_base_url: moved(&self.vibe_base_url, &self.context.vibe_base_url),
        };
        match ports.persist_provider_credentials(&request).first_failure() {
            Some(field) => OnboardingOutcome::ProviderConfigError {
                detail: format!("failed to persist {field}"),
            },
            None => OnboardingOutcome::Completed,
        }
    }
}
