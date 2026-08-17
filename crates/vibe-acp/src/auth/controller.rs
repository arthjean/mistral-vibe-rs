//! Reference `AcpAuthController`, over the ambient environment: the advertised
//! browser methods, the two sign-in flows, status, and the product's only
//! credential removal path.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value, json};
use toml::Table;
use vibe_core::auth::{
    AuthState, DEFAULT_BROWSER_AUTH_API_BASE_URL, DEFAULT_BROWSER_AUTH_BASE_URL, SignInAttempt,
    SignInErrorCode, configured_custom_domain, is_valid_custom_domain, resolve_api_key_provider,
    resolve_browser_auth_urls, supports_browser_sign_in,
};

use crate::auth::{
    AcpAuthEnvironment, SIGN_IN_TARGET_CUSTOM, SIGN_IN_TARGET_MISTRAL, browser_method,
};
use crate::protocol::AcpError;

/// The provider a sign-in runs against, and whether resolving it rewrote the
/// browser-auth URLs the configured entry carries.
///
/// Reference `_persist_credentials` persists the provider entry only when the
/// flow modified it. The flag records that at the one place that knows, rather
/// than re-reading the configuration afterward to compare two tables.
#[derive(Clone)]
struct SignInProvider {
    provider: Table,
    modified: bool,
}

/// A delegated attempt waiting for its completion call.
#[derive(Clone)]
struct PendingSignIn {
    attempt: SignInAttempt,
    provider: SignInProvider,
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
            .browser_authenticate(&provider.provider)
            .await
            .map_err(|error| AcpError::AuthFailure(error.message().to_owned()))?;
        let mut meta = self.persist_credentials(&provider, &api_key);
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
            .start_attempt(&provider.provider)
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
            .complete_attempt(&pending.provider.provider, &pending.attempt)
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
        let mut meta = self.persist_credentials(&pending.provider, &api_key);
        meta.insert("status".to_owned(), json!("completed"));
        let mut delegated = Map::new();
        delegated.insert("attemptId".to_owned(), json!(attempt_id));
        for (key, value) in meta {
            delegated.insert(key, value);
        }
        Ok(json!({"_meta": {"browser-auth-delegated": delegated}}))
    }

    /// Reference `_persist_credentials`: the key first, then the provider
    /// entry when the flow modified it, without rolling the key back when the
    /// provider write fails.
    fn persist_credentials(&self, provider: &SignInProvider, api_key: &str) -> Map<String, Value> {
        let resolved = resolve_api_key_provider(&provider.provider);
        let env_key = resolved
            .get("api_key_env_var")
            .and_then(toml::Value::as_str)
            .unwrap_or("");
        let backend_is_mistral =
            resolved.get("backend").and_then(toml::Value::as_str) == Some("mistral");
        let custom_domain = configured_custom_domain(&provider.provider).is_some();
        let outcome =
            self.environment
                .persist_api_key(env_key, backend_is_mistral, api_key, custom_domain);
        let mut meta = Map::new();
        meta.insert(
            "persistResult".to_owned(),
            json!(outcome.as_reference_string()),
        );
        if provider.modified {
            let persisted = self.environment.persist_provider(&provider.provider);
            meta.insert(
                "persistProviderResult".to_owned(),
                json!(if persisted { "completed" } else { "failed" }),
            );
        }
        meta
    }

    /// Reference `_resolve_sign_in_provider`: the configured provider as-is,
    /// or with its browser-auth URLs replaced by the shipped defaults or by a
    /// validated custom domain.
    fn resolve_sign_in_provider(&self, arguments: &Value) -> Result<SignInProvider, AcpError> {
        let provider = self.enabled_provider()?;
        let target = match arguments.get("signInTarget") {
            None | Some(Value::Null) => {
                return Ok(SignInProvider {
                    provider,
                    modified: false,
                });
            }
            Some(target) => target,
        };
        let (base_url, api_base_url) = if target == SIGN_IN_TARGET_MISTRAL {
            (
                DEFAULT_BROWSER_AUTH_BASE_URL.to_owned(),
                DEFAULT_BROWSER_AUTH_API_BASE_URL.to_owned(),
            )
        } else if target == SIGN_IN_TARGET_CUSTOM {
            let domain = arguments
                .get("domain")
                .and_then(Value::as_str)
                .filter(|domain| is_valid_custom_domain(domain))
                .ok_or_else(|| {
                    AcpError::InvalidParams("the custom sign-in domain is not valid".to_owned())
                })?;
            resolve_browser_auth_urls(domain)
        } else {
            return Err(AcpError::InvalidParams(format!(
                "sign-in target `{target}` is not supported"
            )));
        };
        let rewritten = with_browser_auth_urls(provider.clone(), &base_url, &api_base_url);
        Ok(SignInProvider {
            modified: rewritten != provider,
            provider: rewritten,
        })
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

fn with_browser_auth_urls(mut provider: Table, base_url: &str, api_base_url: &str) -> Table {
    provider.insert(
        "browser_auth_base_url".to_owned(),
        toml::Value::String(base_url.to_owned()),
    );
    provider.insert(
        "browser_auth_api_base_url".to_owned(),
        toml::Value::String(api_base_url.to_owned()),
    );
    provider
}
