//! `account/read`: the plan behind the Mistral key the session runs with.
//!
//! Reference `AccountController.read` (`vibe/app_server/_account.py`) and its
//! gateway (`vibe/setup/auth/whoami.py`): the status is decided locally until a
//! key resolves, and from the console's `/api/vibe/whoami` answer after that.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use toml::Value as TomlValue;
use vibe_core::config::DotenvValues;

use super::{MISTRAL_KEY, WorkspaceService};
use crate::vocabulary::{AccountActionKind, AccountPlanKind, AccountStatus};

const WHOAMI_PATH: &str = "/api/vibe/whoami";
const DEFAULT_CONSOLE_BASE_URL: &str = "https://console.mistral.ai";
/// Chat plan names the reference sells as Pro.
const PAID_CHAT_PLANS: [&str; 3] = ["INDIVIDUAL", "EDU", "TEAM"];
/// The reference's HTTP client keeps httpx's five-second default.
const WHOAMI_TIMEOUT: Duration = Duration::from_secs(5);

/// Reference `WhoAmIResult`, reduced to the fields the view reads.
#[derive(Debug, Deserialize)]
struct WhoAmI {
    plan_type: String,
    plan_name: String,
    #[serde(default)]
    prompt_switching_to_pro_plan: bool,
}

enum Gateway {
    Plan(WhoAmI),
    Unauthorized,
    Unavailable,
}

impl WorkspaceService {
    /// The account as `AccountView` declares it.
    pub async fn read_account(&self) -> Value {
        let upgrade = self.account_action(AccountActionKind::UpgradeToPro);
        let unavailable = view(AccountStatus::Unavailable, &upgrade);
        let Ok(snapshot) = self.config.load() else {
            return unavailable;
        };
        let Some(provider) = snapshot.active_provider() else {
            return unavailable;
        };
        let mistral = provider
            .get("backend")
            .and_then(TomlValue::as_str)
            .unwrap_or("mistral")
            == "mistral";
        if !mistral {
            return unavailable;
        }
        let variable = provider
            .get("api_key_env_var")
            .and_then(TomlValue::as_str)
            .unwrap_or(MISTRAL_KEY);
        // The credential resolves as every other reader resolves one: the
        // process environment with the vibe home's dotenv filling in what it
        // does not set, then the OS keyring.
        let environ = DotenvValues::global(&self.paths.vibe_home).environment();
        let store = vibe_core::auth::KeyringStore::native();
        let Some(key) = vibe_core::auth::resolve_api_key(variable, &environ, &store)
            .filter(|key| !key.is_empty())
        else {
            return view(AccountStatus::MissingKey, &upgrade);
        };
        let console = snapshot
            .effective
            .get("console_base_url")
            .and_then(TomlValue::as_str)
            .unwrap_or(DEFAULT_CONSOLE_BASE_URL)
            .to_owned();
        match fetch_whoami(&console, &key).await {
            Gateway::Unauthorized => {
                let mut account = view(AccountStatus::Unauthorized, &upgrade);
                account["planOffer"] = upgrade.clone();
                account["rateLimitAction"] = upgrade;
                account
            }
            Gateway::Unavailable => unavailable,
            Gateway::Plan(whoami) => self.plan_view(&whoami).unwrap_or(unavailable),
        }
    }

    /// Reference `_Plan` read into the `ready` view, or `None` for a plan
    /// kind the reference's model rejects.
    fn plan_view(&self, whoami: &WhoAmI) -> Option<Value> {
        let kind = match whoami.plan_type.trim().to_lowercase().as_str() {
            "api" => AccountPlanKind::Api,
            "chat" => AccountPlanKind::Chat,
            "mistral_code" => AccountPlanKind::MistralCode,
            _ => return None,
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

/// Reference `HttpAccountGateway.read`: a rejected key is `Unauthorized`, and
/// anything else short of a well-formed plan is `Unavailable`.
async fn fetch_whoami(console: &str, key: &str) -> Gateway {
    let Ok(client) = reqwest::Client::builder().timeout(WHOAMI_TIMEOUT).build() else {
        return Gateway::Unavailable;
    };
    let url = format!("{}{WHOAMI_PATH}", console.trim_end_matches('/'));
    let Ok(response) = client.get(url).bearer_auth(key).send().await else {
        return Gateway::Unavailable;
    };
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Gateway::Unauthorized;
    }
    if !status.is_success() {
        return Gateway::Unavailable;
    }
    response
        .json::<WhoAmI>()
        .await
        .map_or(Gateway::Unavailable, Gateway::Plan)
}
