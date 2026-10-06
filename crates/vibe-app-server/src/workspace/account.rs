//! `account/read`: the plan behind the Mistral key the session runs with.
//!
//! Reference `AccountController.read` (`vibe/app_server/_account.py`) and its
//! gateway (`vibe/setup/auth/whoami.py`): the status is decided locally until a
//! key resolves, and from the console's `/api/vibe/whoami` answer after that.

use std::time::Duration;

use serde_json::{Value, json};
use toml::Value as TomlValue;
use vibe_core::config::DotenvValues;
use vibe_core::whoami::{
    HttpWhoAmIGateway, WhoAmIFailure, WhoAmIGateway, WhoAmIResult, reconcile_tenant_domains,
    store_cached_whoami, whoami_cache_path,
};

use super::WorkspaceService;
use crate::vocabulary::{AccountActionKind, AccountPlanKind, AccountStatus};

const DEFAULT_CONSOLE_BASE_URL: &str = "https://console.mistral.ai";
/// Chat plan names the reference sells as Pro.
const PAID_CHAT_PLANS: [&str; 3] = ["INDIVIDUAL", "EDU", "TEAM"];
/// The reference's HTTP client keeps httpx's five-second default.
const WHOAMI_TIMEOUT: Duration = Duration::from_secs(5);

/// What one account read learned that the session's telemetry reconciles
/// with. Reference `AccountController.read` calls the agent loop back with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountLookup {
    /// No Mistral provider is configured: the plan is the sentinel.
    NoMistralProvider,
    /// The console answered a plan for this key.
    Plan {
        console: String,
        key: String,
        result: WhoAmIResult,
    },
    /// The console refused this key.
    Unauthorized { key: String },
    /// Nothing to reconcile: a missing key, an unreachable console, or an
    /// unreadable configuration.
    Nothing,
}

impl WorkspaceService {
    /// The account as `AccountView` declares it.
    pub async fn read_account(&self) -> Value {
        self.read_account_lookup().await.0
    }

    /// The account, and what the read learned for the session's telemetry.
    pub async fn read_account_lookup(&self) -> (Value, AccountLookup) {
        let upgrade = self.account_action(AccountActionKind::UpgradeToPro);
        let unavailable = view(AccountStatus::Unavailable, &upgrade);
        let Ok(snapshot) = self.config.load() else {
            return (unavailable, AccountLookup::Nothing);
        };
        // Reference `get_mistral_provider_and_api_key`: the active provider
        // when it is Mistral, else the first Mistral provider configured. A
        // provider that declares no backend is a generic one.
        let Some(provider) = vibe_core::telemetry::mistral_provider(&snapshot.effective) else {
            return (unavailable, AccountLookup::NoMistralProvider);
        };
        let variable = provider
            .get("api_key_env_var")
            .and_then(TomlValue::as_str)
            .unwrap_or_default();
        let Some(key) = (!variable.is_empty())
            .then(|| self.resolve_credential(variable))
            .flatten()
        else {
            // A missing key is the account's state only while the session
            // runs on Mistral; otherwise there is no account to show.
            let status = if vibe_core::telemetry::is_active_model_mistral(&snapshot.effective) {
                AccountStatus::MissingKey
            } else {
                AccountStatus::Unavailable
            };
            return (view(status, &upgrade), AccountLookup::Nothing);
        };
        let console = snapshot
            .effective
            .get("console_base_url")
            .and_then(TomlValue::as_str)
            .unwrap_or(DEFAULT_CONSOLE_BASE_URL)
            .to_owned();
        let Some(gateway) = HttpWhoAmIGateway::production() else {
            return (unavailable, AccountLookup::Nothing);
        };
        // The account is shown only while the session runs on Mistral; the
        // plan is still read for telemetry and the tenant heal otherwise.
        let shows_account = vibe_core::telemetry::is_active_model_mistral(&snapshot.effective);
        let provider_name = provider
            .get("name")
            .and_then(TomlValue::as_str)
            .unwrap_or_default()
            .to_owned();
        match gateway.read(&console, &key, Some(WHOAMI_TIMEOUT)).await {
            Err(WhoAmIFailure::Unauthorized) if !shows_account => {
                (unavailable, AccountLookup::Unauthorized { key })
            }
            Err(WhoAmIFailure::Unauthorized) => {
                let mut account = view(AccountStatus::Unauthorized, &upgrade);
                account["planOffer"] = upgrade.clone();
                account["rateLimitAction"] = upgrade;
                (account, AccountLookup::Unauthorized { key })
            }
            Err(WhoAmIFailure::Unavailable) => (unavailable, AccountLookup::Nothing),
            Ok(whoami) => {
                // Reference warms the cross-session cache with every live
                // answer, so the next session starts from it.
                store_cached_whoami(&whoami_cache_path(self.vibe_home()), &key, &whoami);
                let account = if shows_account {
                    self.plan_view(&whoami).unwrap_or(unavailable)
                } else {
                    unavailable
                };
                // Reference `AccountController.read` heals the configuration
                // with the tenant hosts after building the view, so the view
                // still names the chat base it was read with.
                reconcile_tenant_domains(&self.config, &whoami, &provider_name);
                (
                    account,
                    AccountLookup::Plan {
                        console,
                        key,
                        result: whoami,
                    },
                )
            }
        }
    }

    /// Reference `_Plan` read into the `ready` view, or `None` for a plan
    /// kind the reference's model rejects.
    fn plan_view(&self, whoami: &WhoAmIResult) -> Option<Value> {
        let kind = match whoami.plan_type {
            vibe_core::whoami::AccountPlanKind::Api => AccountPlanKind::Api,
            vibe_core::whoami::AccountPlanKind::Chat => AccountPlanKind::Chat,
            vibe_core::whoami::AccountPlanKind::MistralCode => AccountPlanKind::MistralCode,
        };
        let name = whoami.plan_name.trim();
        let normalized = name.to_uppercase();
        let title = match kind {
            AccountPlanKind::Chat if normalized == "FREE" => Some("Free"),
            AccountPlanKind::Chat => PAID_CHAT_PLANS
                .contains(&normalized.as_str())
                .then_some("[Subscription] Pro"),
            AccountPlanKind::Api if normalized.contains("FREE") => Some("Free"),
            AccountPlanKind::Api => Some("[API] Scale plan"),
            AccountPlanKind::MistralCode => match normalized.as_str() {
                "F" => Some("Mistral Code Free"),
                "E" => Some("Mistral Code Enterprise"),
                _ => None,
            },
        };
        let code_free = kind == AccountPlanKind::MistralCode && normalized == "F";
        let offers_upgrade = kind == AccountPlanKind::Api
            || (kind == AccountPlanKind::Chat && normalized == "FREE")
            || code_free;
        let rate_limit_upgrade = kind == AccountPlanKind::Api || code_free;
        let upgrade = self.account_action(AccountActionKind::UpgradeToPro);
        let switch_key = self.account_action(AccountActionKind::SwitchApiKey);
        let plan_offer = if whoami.prompt_switching_to_pro_plan {
            switch_key.clone()
        } else if offers_upgrade {
            upgrade.clone()
        } else {
            Value::Null
        };
        let teleport_eligible = kind != AccountPlanKind::MistralCode;
        Some(json!({
            "status": AccountStatus::Ready,
            "plan": {"kind": kind, "name": name, "title": title},
            "planOffer": plan_offer,
            "rateLimitAction": if rate_limit_upgrade { upgrade } else { Value::Null },
            "teleportEligible": teleport_eligible,
            "teleportAction": if teleport_eligible { Value::Null } else { switch_key },
        }))
    }

    fn account_action(&self, kind: AccountActionKind) -> Value {
        json!({
            "kind": kind,
            "url": format!("{}/code/extensions?focus=key", self.vibe_base_url().trim_end_matches('/')),
        })
    }
}

fn view(status: AccountStatus, teleport_action: &Value) -> Value {
    json!({
        "status": status,
        "plan": null,
        "planOffer": null,
        "rateLimitAction": null,
        "teleportEligible": false,
        "teleportAction": teleport_action,
    })
}

impl WorkspaceService {
    /// Reference `IdentityController.read` (`vibe/app_server/_identity.py`):
    /// who the Mistral key the session runs with belongs to, read from the
    /// provider's `/users/me`, or nothing when the model is not Mistral's, no
    /// key resolves, or the endpoint does not answer.
    ///
    /// Like the reference, a fresh read is not written to the cache: only the
    /// identity another reader already stored is reused.
    pub async fn read_identity(&self) -> Value {
        let Ok(snapshot) = self.config.load() else {
            return Value::Null;
        };
        let Some(provider) = snapshot.active_provider().filter(|provider| {
            provider.get("backend").and_then(TomlValue::as_str) == Some("mistral")
        }) else {
            return Value::Null;
        };
        let text = |key: &str| {
            provider
                .get(key)
                .and_then(TomlValue::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let base_url = text("api_base");
        let environ = DotenvValues::global(&self.paths.vibe_home).environment();
        let store = vibe_core::auth::KeyringStore::native();
        let Some(key) =
            vibe_core::auth::resolve_api_key(&text("api_key_env_var"), &environ, &store)
                .filter(|key| !key.is_empty())
        else {
            return Value::Null;
        };
        let identity = match self.identity_cache.peek(&base_url, &key).await {
            Some(cached) => Some(cached),
            None => match vibe_core::identity::HttpIdentityGateway::production() {
                Some(gateway) => {
                    vibe_core::identity::fetch_identity(&gateway, &base_url, &key, None).await
                }
                None => None,
            },
        };
        identity.map_or(Value::Null, |identity| identity_view(&identity))
    }
}

/// Reference `IdentityView`, with every field it declares present.
fn identity_view(identity: &vibe_core::identity::IdentityResult) -> Value {
    let entity = |entity: &Option<vibe_core::identity::IdentityEntity>| {
        entity.as_ref().map_or(
            Value::Null,
            |entity| json!({"id": entity.id, "name": entity.name}),
        )
    };
    json!({
        "id": identity.id,
        "email": identity.email,
        "firstName": identity.first_name,
        "lastName": identity.last_name,
        "workspace": entity(&identity.workspace),
        "organization": entity(&identity.organization),
    })
}

impl WorkspaceService {
    /// Records a connector toggle in the file writes land in.
    pub(crate) fn persist_connector_toggle(
        &self,
        name: &str,
        disabled: bool,
        tool_name: Option<&str>,
    ) -> Result<(), vibe_core::config::ConfigError> {
        self.config
            .persist_connector_toggle(name, disabled, tool_name)
            .map(|_| ())
    }

    /// The merged configuration as it stands now.
    pub(crate) fn config_snapshot(&self) -> Option<vibe_core::config::ConfigSnapshot> {
        self.config.load().ok()
    }

    /// The credential `variable` resolves to: the process environment with
    /// the Vibe home's dotenv filling in, then the OS keyring.
    /// What a model call made on the workspace's behalf reads its key from:
    /// the environment, the global `.env` file, then the keyring.
    pub(crate) fn credentials(&self) -> std::sync::Arc<dyn vibe_core::llm::Credentials> {
        std::sync::Arc::new(vibe_core::llm::AmbientCredentials::new(
            DotenvValues::global(&self.paths.vibe_home),
            vibe_core::auth::KeyringStore::native(),
        ))
    }

    pub(crate) fn resolve_credential(&self, variable: &str) -> Option<String> {
        let environ = DotenvValues::global(&self.paths.vibe_home).environment();
        let store = vibe_core::auth::KeyringStore::native();
        vibe_core::auth::resolve_api_key(variable, &environ, &store).filter(|key| !key.is_empty())
    }

    /// Reference `_manage_connectors_url`: the console page managing the
    /// caller's connectors, scoped to the organization and workspace the
    /// Mistral provider's identity names, or `None` when that identity does
    /// not resolve.
    pub(crate) async fn connector_manage_url(
        &self,
        api_base: &str,
        api_key: &str,
    ) -> Option<String> {
        let gateway = vibe_core::identity::HttpIdentityGateway::production()?;
        let identity = self
            .identity_cache
            .resolve(&gateway, api_base, api_key, None)
            .await?;
        let organization = identity.organization.as_ref()?;
        let workspace = identity.workspace.as_ref()?;
        let share_context = vibe_core::mcp::authorization::python_json(&serde_json::json!({
            "organizationId": organization.id,
            "workspaceId": workspace.id,
        }));
        let console = self
            .config
            .load()
            .ok()
            .and_then(|snapshot| {
                snapshot
                    .effective
                    .get("console_base_url")
                    .and_then(TomlValue::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| DEFAULT_CONSOLE_BASE_URL.to_owned());
        let encoded = share_context
            .bytes()
            .map(|byte| {
                if byte.is_ascii_alphanumeric() || b"_.-~".contains(&byte) {
                    char::from(byte).to_string()
                } else {
                    format!("%{byte:02X}")
                }
            })
            .collect::<String>();
        Some(format!(
            "{}/build/connectors?shareContext={encoded}",
            console.trim_end_matches('/')
        ))
    }
}

#[cfg(test)]
mod account_tests;
