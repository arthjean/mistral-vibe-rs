//! Reference `AcpAuthController`, over the ambient environment: the advertised
//! browser methods, the two sign-in flows, status, and the product's only
//! credential removal path.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value, json};
use toml::Table;
use vibe_core::auth::{
    AuthState, DEFAULT_BROWSER_AUTH_API_BASE_URL, DEFAULT_BROWSER_AUTH_BASE_URL,
    DEFAULT_CONSOLE_BASE_URL, ProviderCredentialsRequest, SignInAttempt, SignInErrorCode,
    allows_origin_rewrite, apply_browser_auth_urls, browser_auth_account_base,
    configured_custom_domain, effective_browser_auth_url, is_valid_custom_domain,
    resolve_api_key_provider, resolve_browser_auth_urls, same_provider, supports_browser_sign_in,
};

use crate::auth::{
    AcpAuthEnvironment, SIGN_IN_TARGET_CUSTOM, SIGN_IN_TARGET_MISTRAL, browser_method,
};
use crate::protocol::AcpError;

/// A delegated attempt waiting for its completion call, with the provider it
/// started against.
#[derive(Clone)]
struct PendingSignIn {
    attempt: SignInAttempt,
    provider: Table,
}

/// Reference `_account_base_of`: the console account calls go to, or `None`
/// for the public one. A custom console answers its own origin, or the API
/// origin when a split-horizon deployment keeps the browser console out of
/// reach, and so does a split-horizon API base on the public console.
fn account_base_of(provider: &Table) -> Option<String> {
    let custom = configured_custom_domain(provider).map(str::to_owned);
    if custom.is_none() && !allows_origin_rewrite(provider) {
        return None;
    }
    let browser_base = effective_browser_auth_url(provider, "browser_auth_base_url");
    let api_base = effective_browser_auth_url(provider, "browser_auth_api_base_url");
    match (browser_base, api_base) {
        (Some(browser), Some(api)) if !browser.is_empty() && !api.is_empty() => {
            Some(browser_auth_account_base(&browser, Some(&api)))
        }
        (browser, _) => custom.or(browser),
    }
}

/// Reference `AcpAuthController`, over the environment above.
pub(crate) struct AuthController {
    environment: Arc<dyn AcpAuthEnvironment>,
    pending: Mutex<BTreeMap<String, PendingSignIn>>,
}

impl AuthController {
    pub(crate) fn new(environment: Arc<dyn AcpAuthEnvironment>) -> Self {
        Self {
            environment,
            pending: Mutex::new(BTreeMap::new()),
        }
    }

    /// The browser methods the provider predicate admits. Reference
    /// `browser_methods`: none at all when the provider cannot browser
    /// sign-in, and the delegated variant only when the client advertised it.
    pub(crate) fn browser_methods(&self, delegated: bool) -> Vec<Value> {
        if !supports_browser_sign_in(&self.environment.load_provider()) {
            return Vec::new();
        }
        let mut methods = vec![browser_method("browser-auth")];
        if delegated {
            methods.push(browser_method("browser-auth-delegated"));
        }
        methods
    }

    pub(crate) async fn authenticate(
        &self,
        method_id: &str,
        arguments: &Value,
    ) -> Result<Value, AcpError> {
        match method_id {
            "browser-auth" => self.authenticate_browser(arguments).await,
            "browser-auth-delegated" => self.authenticate_delegated(arguments).await,
            _ => Err(AcpError::UnsupportedAuthentication(method_id.to_owned())),
        }
    }

    /// Reference `status`: reassess provenance from a fresh dotenv read.
    ///
    /// # Errors
    ///
    /// The dotenv read failure, surfaced as the internal failure the
    /// reference's raised `OSError` becomes.
    pub(crate) fn status(&self) -> Result<AuthState, AcpError> {
        let provider = self.environment.load_provider();
        let env_key = provider
            .get("api_key_env_var")
            .and_then(toml::Value::as_str)
            .unwrap_or("");
        self.environment.assess(env_key).map_err(|error| {
            AcpError::AuthFailure(format!("auth state assessment failed: {error}"))
        })
    }

    /// The `auth/status` payload, with the reference's four field names.
    pub(crate) fn status_payload(&self) -> Result<Value, AcpError> {
        let state = self.status()?;
        Ok(json!({
            "authenticated": state.can_use_active_provider,
            "authState": state.kind.as_str(),
            "signOutAvailable": state.sign_out_available,
            "customDomain": self.custom_domain(),
        }))
    }

    /// The configured console domain when it differs from the shipped
    /// default. Reference `custom_domain`.
    pub(crate) fn custom_domain(&self) -> Option<String> {
        configured_custom_domain(&self.environment.load_provider()).map(str::to_owned)
    }

    /// Reference `sign_out`: refused unless the assessed state marks it
    /// available, and a storage failure surfaces after the removal cleared
    /// what it could.
    pub(crate) fn sign_out(&self) -> Result<(), AcpError> {
        let provider = self.environment.load_provider();
        let state = self.status()?;
        if !state.sign_out_available {
            return Err(AcpError::InvalidParams(format!(
                "sign-out is not available in auth state `{}`",
                state.kind.as_str()
            )));
        }
        let env_key = provider
            .get("api_key_env_var")
            .and_then(toml::Value::as_str)
            .unwrap_or("");
        self.environment
            .remove_api_key(env_key)
            .map_err(|error| AcpError::AuthFailure(format!("sign-out did not complete: {error}")))
    }

    async fn authenticate_browser(&self, arguments: &Value) -> Result<Value, AcpError> {
        match arguments.get("action") {
            None | Some(Value::Null) => {}
            Some(Value::String(action)) if action == "start" => {}
            Some(action) => {
                return Err(AcpError::InvalidParams(format!(
                    "browser auth action `{action}` is not supported"
                )));
            }
        }
        let provider = self.resolve_sign_in_provider(arguments)?;
        let api_key = self
            .environment
            .browser_authenticate(&provider)
            .await
            .map_err(|error| AcpError::AuthFailure(error.message().to_owned()))?;
        let mut meta = self.persist_credentials(provider, &api_key).await;
        meta.insert("status".to_owned(), json!("completed"));
        Ok(json!({"_meta": {"browser-auth": meta}}))
    }

    async fn authenticate_delegated(&self, arguments: &Value) -> Result<Value, AcpError> {
        let action = match arguments.get("action") {
            None => "start",
            Some(Value::String(action)) => action.as_str(),
            Some(action) => {
                return Err(AcpError::InvalidParams(format!(
                    "delegated browser auth action `{action}` is not supported"
                )));
            }
        };
        match action {
            "start" => self.start_delegated(arguments).await,
            "complete" => self.complete_delegated(arguments).await,
            action => Err(AcpError::InvalidParams(format!(
                "delegated browser auth action `{action}` is not supported"
            ))),
        }
    }

    async fn start_delegated(&self, arguments: &Value) -> Result<Value, AcpError> {
        let provider = self.resolve_sign_in_provider(arguments)?;
        let attempt = self
            .environment
            .start_attempt(&provider)
            .await
            .map_err(|error| AcpError::AuthFailure(error.message().to_owned()))?;
        let response = json!({
            "_meta": {
                "browser-auth-delegated": {
                    "attemptId": attempt.process_id,
                    "expiresAt": attempt.expires_at.to_iso8601().replace("+00:00", "Z"),
                    "signInUrl": attempt.sign_in_url,
                }
            }
        });
        self.pending().insert(
            attempt.process_id.clone(),
            PendingSignIn { attempt, provider },
        );
        Ok(response)
    }

    async fn complete_delegated(&self, arguments: &Value) -> Result<Value, AcpError> {
        let attempt_id = arguments
            .get("attemptId")
            .or_else(|| arguments.get("attempt_id"))
            .and_then(Value::as_str)
            .filter(|attempt_id| !attempt_id.is_empty())
            .ok_or_else(|| {
                AcpError::InvalidParams("a browser sign-in attempt ID is required".to_owned())
            })?
            .to_owned();
        let pending = self.pending().get(&attempt_id).cloned().ok_or_else(|| {
            AcpError::InvalidParams(format!(
                "no browser sign-in attempt is pending under `{attempt_id}`"
            ))
        })?;
        let completed = self
            .environment
            .complete_attempt(&pending.provider, &pending.attempt)
            .await;
        let api_key = match completed {
            Ok(api_key) => api_key,
            Err(error) => {
                // A poll or exchange hiccup leaves the attempt completable;
                // every other failure discards it, as the reference does.
                if !matches!(
                    error.code,
                    SignInErrorCode::ExchangeFailed | SignInErrorCode::PollFailed
                ) {
                    self.pending().remove(&attempt_id);
                }
                return Err(AcpError::InvalidParams(error.message().to_owned()));
            }
        };
        self.pending().remove(&attempt_id);
        let mut meta = self.persist_credentials(pending.provider, &api_key).await;
        meta.insert("status".to_owned(), json!("completed"));
        let mut delegated = Map::new();
        delegated.insert("attemptId".to_owned(), json!(attempt_id));
        for (key, value) in meta {
            delegated.insert(key, value);
        }
        Ok(json!({"_meta": {"browser-auth-delegated": delegated}}))
    }

    /// Reference `_persist_credentials`: the key first. Then, unless the
    /// provider is the configured one and the console account calls go to is
    /// already the configured one, a custom console is asked for its tenant's
    /// hosts and the provider entry and every moved base URL are written in
    /// one batch, each write reported in the meta. A failed key save does not
    /// stop the rest, as in the reference.
    async fn persist_credentials(&self, provider: Table, api_key: &str) -> Map<String, Value> {
        let resolved = resolve_api_key_provider(&provider);
        let env_key = resolved
            .get("api_key_env_var")
            .and_then(toml::Value::as_str)
            .unwrap_or("");
        let backend_is_mistral =
            resolved.get("backend").and_then(toml::Value::as_str) == Some("mistral");
        let custom_domain = configured_custom_domain(&provider).is_some();
        let outcome =
            self.environment
                .persist_api_key(env_key, backend_is_mistral, api_key, custom_domain);
        let mut meta = Map::new();
        meta.insert(
            "persistResult".to_owned(),
            json!(outcome.as_reference_string()),
        );
        let bases = self.environment.load_bases();
        let account_base = account_base_of(&provider);
        let desired_console = account_base
            .clone()
            .filter(|base| !base.is_empty())
            .unwrap_or_else(|| DEFAULT_CONSOLE_BASE_URL.to_owned());
        if same_provider(&provider, &self.environment.load_provider())
            && desired_console == bases.console_base_url
        {
            return meta;
        }
        let (provider, desired_vibe) = if account_base.is_some() {
            self.environment
                .resolve_tenant_domains(provider, &desired_console, api_key, &bases.vibe_base_url)
                .await
        } else {
            (provider, bases.vibe_base_url.clone())
        };
        let moved =
            |desired: &str, configured: &str| (desired != configured).then(|| desired.to_owned());
        let request = ProviderCredentialsRequest {
            provider,
            console_base_url: moved(&desired_console, &bases.console_base_url),
            vibe_base_url: moved(&desired_vibe, &bases.vibe_base_url),
        };
        let result = self.environment.persist_provider_credentials(&request);
        let status = |landed: bool| json!(if landed { "completed" } else { "failed" });
        meta.insert("persistProviderResult".to_owned(), status(result.provider));
        if let Some(landed) = result.console_base_url {
            meta.insert("persistConsoleBaseUrlResult".to_owned(), status(landed));
        }
        if let Some(landed) = result.vibe_base_url {
            meta.insert("persistVibeBaseUrlResult".to_owned(), status(landed));
        }
        meta
    }

    /// Reference `_resolve_sign_in_provider`: the configured provider as-is,
    /// or with its browser-auth URLs replaced by the shipped defaults, or by a
    /// validated custom domain and, for a split-horizon deployment, the
    /// separately reachable sign-in API base, the origin rewrite following
    /// whether the two differ.
    fn resolve_sign_in_provider(&self, arguments: &Value) -> Result<Table, AcpError> {
        let mut provider = self.enabled_provider()?;
        let target = match arguments.get("signInTarget") {
            None | Some(Value::Null) => return Ok(provider),
            Some(target) => target,
        };
        if target == SIGN_IN_TARGET_MISTRAL {
            provider.insert(
                "browser_auth_base_url".to_owned(),
                toml::Value::String(DEFAULT_BROWSER_AUTH_BASE_URL.to_owned()),
            );
            provider.insert(
                "browser_auth_api_base_url".to_owned(),
                toml::Value::String(DEFAULT_BROWSER_AUTH_API_BASE_URL.to_owned()),
            );
            provider.insert(
                "browser_auth_allow_origin_rewrite".to_owned(),
                toml::Value::Boolean(false),
            );
            return Ok(provider);
        }
        if target != SIGN_IN_TARGET_CUSTOM {
            return Err(AcpError::InvalidParams(format!(
                "sign-in target `{target}` is not supported"
            )));
        }
        let domain = arguments
            .get("domain")
            .and_then(Value::as_str)
            .filter(|domain| is_valid_custom_domain(domain))
            .ok_or_else(|| {
                AcpError::InvalidParams("the custom sign-in domain is not valid".to_owned())
            })?;
        // The browser-auth sign-in base of a split-horizon deployment, not the
        // `/v1` model API base; absent or blank, it derives as `domain/api`.
        let api_base = match arguments.get("apiBaseUrl") {
            None | Some(Value::Null) => None,
            Some(Value::String(api_base)) => Some(api_base.trim()),
            Some(other) => {
                return Err(AcpError::InvalidParams(format!(
                    "the custom sign-in API base URL {other} is not a string"
                )));
            }
        };
        if let Some(api_base) = api_base
            && !api_base.is_empty()
            && !is_valid_custom_domain(api_base)
        {
            return Err(AcpError::InvalidParams(
                "the custom sign-in API base URL is not valid".to_owned(),
            ));
        }
        let (base_url, api_base_url) =
            resolve_browser_auth_urls(domain, api_base.filter(|api| !api.is_empty()));
        apply_browser_auth_urls(&mut provider, &base_url, &api_base_url);
        Ok(provider)
    }

    fn enabled_provider(&self) -> Result<Table, AcpError> {
        let provider = self.environment.load_provider();
        if !supports_browser_sign_in(&provider) {
            return Err(AcpError::InvalidParams(
                "the configured provider does not support browser sign-in".to_owned(),
            ));
        }
        Ok(provider)
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, PendingSignIn>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
