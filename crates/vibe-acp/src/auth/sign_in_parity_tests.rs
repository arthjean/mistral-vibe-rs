//! Differential replay of the editor-protocol sign-in against the reference.
//!
//! `scripts/parity/setup_auth.py` drives the reference's `AcpAuthController`
//! (`vibe/acp/auth.py`) through its constructor ports and records, per case,
//! the provider the sign-in service was built for, the key save, the tenant
//! lookup and the batched credentials write, and the response meta or the
//! JSON-RPC error code. This module replays every case through
//! [`AuthController`] over an environment scripted the same way: the
//! `/whoami` answer feeds the shared tenant adoption, and the persisters
//! answer what the case scripts. The live recapture belongs to the setup-auth
//! replay in `vibe-core`, which owns the corpus.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Deserialize;
use serde_json::{Map, Value, json};
use toml::Table;
use vibe_core::auth::{
    AuthState, PersistOutcome, ProviderCredentialsRequest, ProviderCredentialsResult, RemoveError,
    SignInAttempt, SignInError, SignInErrorCode, allows_origin_rewrite, default_mistral_provider,
    effective_browser_auth_url,
};
use vibe_core::parity::REFERENCE_COMMIT;
use vibe_core::whoami::{WhoAmIResult, adopt_tenant_domains};

use super::{
    AcpAuthEnvironment, AuthAttemptFuture, AuthController, AuthKeyFuture, ConfiguredBases,
    TenantDomainsFuture,
};

const CORPUS_RELATIVE: &str = "crates/vibe-core/tests/setup-auth/corpus.json";

/// Fewer cases than this means the family stopped being captured.
const MINIMUM_CASES: usize = 31;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Corpus {
    reference: Reference,
    acp_sign_in: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Reference {
    commit: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    case: String,
    provider: Value,
    console_base_url: String,
    vibe_base_url: String,
    arguments: Value,
    whoami: Option<Value>,
    api_key_result: String,
    provider_ok: bool,
    console_ok: bool,
    vibe_ok: bool,
    outcome: Value,
    service_providers: Vec<Value>,
    api_key_persists: Vec<Value>,
    tenant_lookups: Vec<Value>,
    persist_requests: Vec<Value>,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

fn provider_fields(provider: &Table) -> Value {
    json!({
        "name": provider.get("name").and_then(toml::Value::as_str),
        "apiBase": provider.get("api_base").and_then(toml::Value::as_str),
        "browserAuthBaseUrl": effective_browser_auth_url(provider, "browser_auth_base_url"),
        "browserAuthApiBaseUrl": effective_browser_auth_url(provider, "browser_auth_api_base_url"),
        "allowOriginRewrite": allows_origin_rewrite(provider),
    })
}

/// The configured provider: the shipped Mistral entry with the case's
/// overrides, or a generic provider that cannot browser sign-in.
fn configured_provider(spec: &Value) -> Table {
    if spec == "generic" {
        let mut table = Table::new();
        for (key, value) in [
            ("name", "generic"),
            ("api_base", "https://llm.example/v1"),
            ("api_key_env_var", "GENERIC_API_KEY"),
        ] {
            table.insert(key.to_owned(), toml::Value::String(value.to_owned()));
        }
        return table;
    }
    let mut table = default_mistral_provider();
    for (key, value) in spec.as_object().into_iter().flatten() {
        let value = match value {
            Value::Bool(flag) => toml::Value::Boolean(*flag),
            Value::String(text) => toml::Value::String(text.clone()),
            other => panic!("unexpected provider override {other}"),
        };
        table.insert(key.clone(), value);
    }
    table
}

#[derive(Default)]
struct Observed {
    service_providers: Vec<Value>,
    api_key_persists: Vec<Value>,
    tenant_lookups: Vec<Value>,
    persist_requests: Vec<Value>,
}

struct ScriptedEnvironment {
    case: Case,
    provider: Table,
    observed: Mutex<Observed>,
}

impl ScriptedEnvironment {
    fn observed(&self) -> std::sync::MutexGuard<'_, Observed> {
        self.observed.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl AcpAuthEnvironment for ScriptedEnvironment {
    fn load_provider(&self) -> Table {
        self.provider.clone()
    }

    fn assess(&self, _env_key: &str) -> std::io::Result<AuthState> {
        Err(std::io::Error::other("no case assesses"))
    }

    fn persist_api_key(
        &self,
        env_key: &str,
        backend_is_mistral: bool,
        api_key: &str,
        custom_domain: bool,
    ) -> PersistOutcome {
        self.observed().api_key_persists.push(json!({
            "envKey": env_key,
            "backend": if backend_is_mistral { "mistral" } else { "generic" },
            "apiKey": api_key,
            "customDomain": custom_domain,
        }));
        match self.case.api_key_result.split_once(':') {
            Some(("save_error", detail)) => PersistOutcome::SaveError {
                detail: detail.to_owned(),
            },
            Some(("env_var_error", detail)) => PersistOutcome::EnvVarError {
                detail: detail.to_owned(),
            },
            _ => PersistOutcome::Completed,
        }
    }

    fn remove_api_key(&self, _env_key: &str) -> Result<(), RemoveError> {
        Ok(())
    }

    fn load_bases(&self) -> ConfiguredBases {
        ConfiguredBases {
            console_base_url: self.case.console_base_url.clone(),
            vibe_base_url: self.case.vibe_base_url.clone(),
        }
    }

    fn persist_provider_credentials(
        &self,
        request: &ProviderCredentialsRequest,
    ) -> ProviderCredentialsResult {
        self.observed().persist_requests.push(json!({
            "provider": provider_fields(&request.provider),
            "consoleBaseUrl": request.console_base_url,
            "vibeBaseUrl": request.vibe_base_url,
        }));
        ProviderCredentialsResult {
            provider: self.case.provider_ok,
            console_base_url: request
                .console_base_url
                .as_ref()
                .map(|_| self.case.console_ok),
            vibe_base_url: request.vibe_base_url.as_ref().map(|_| self.case.vibe_ok),
        }
    }

    fn resolve_tenant_domains<'a>(
        &'a self,
        provider: Table,
        console_base_url: &'a str,
        api_key: &'a str,
        vibe_base_url: &'a str,
    ) -> TenantDomainsFuture<'a> {
        self.observed().tenant_lookups.push(json!({
            "consoleBaseUrl": console_base_url,
            "apiKey": api_key,
            "vibeBaseUrl": vibe_base_url,
        }));
        let answer = self.case.whoami.clone().map(|mut answer| {
            answer["plan_type"] = json!("api");
            answer["plan_name"] = json!("oracle");
            serde_json::from_value::<WhoAmIResult>(answer)
                .expect("the scripted answer is a valid account")
        });
        let adopted = adopt_tenant_domains(answer.as_ref(), provider, vibe_base_url);
        Box::pin(async move { adopted })
    }

    fn browser_authenticate<'a>(&'a self, provider: &'a Table) -> AuthKeyFuture<'a> {
        self.observed()
            .service_providers
            .push(provider_fields(provider));
        Box::pin(async { Ok("oracle-key".to_owned()) })
    }

    fn start_attempt<'a>(&'a self, _provider: &'a Table) -> AuthAttemptFuture<'a> {
        Box::pin(async {
            Err(SignInError::with_message(
                SignInErrorCode::StartFailed,
                "no case starts a delegated attempt".to_owned(),
            ))
        })
    }

    fn complete_attempt<'a>(
        &'a self,
        _provider: &'a Table,
        _attempt: &'a SignInAttempt,
    ) -> AuthKeyFuture<'a> {
        Box::pin(async {
            Err(SignInError::with_message(
                SignInErrorCode::PollFailed,
                "no case completes a delegated attempt".to_owned(),
            ))
        })
    }
}

fn replay(case: &Case) -> Vec<String> {
    let environment = Arc::new(ScriptedEnvironment {
        provider: configured_provider(&case.provider),
        case: case.clone(),
        observed: Mutex::new(Observed::default()),
    });
    let controller = AuthController::new(environment.clone());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime builds");
    let result = runtime.block_on(controller.authenticate("browser-auth", &case.arguments));
    let outcome = match result {
        Ok(response) => {
            let meta = response
                .get("_meta")
                .and_then(|meta| meta.get("browser-auth"))
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            json!({ "meta": meta })
        }
        Err(error) => json!({ "errorCode": error.json_rpc_code() }),
    };
    let observed = environment.observed();
    let mut mismatches = Vec::new();
    for (label, expected, actual) in [
        ("outcome", &case.outcome, &outcome),
        (
            "serviceProviders",
            &json!(case.service_providers),
            &json!(observed.service_providers),
        ),
        (
            "apiKeyPersists",
            &json!(case.api_key_persists),
            &json!(observed.api_key_persists),
        ),
        (
            "tenantLookups",
            &json!(case.tenant_lookups),
            &json!(observed.tenant_lookups),
        ),
        (
            "persistRequests",
            &json!(case.persist_requests),
            &json!(observed.persist_requests),
        ),
    ] {
        if expected != actual {
            mismatches.push(format!(
                "{}: {label}\n  reference {expected}\n  port      {actual}",
                case.case
            ));
        }
    }
    mismatches
}

#[test]
fn the_editor_sign_in_resolves_and_writes_what_the_reference_does() {
    let path = repo_root().join(CORPUS_RELATIVE);
    let raw = std::fs::read_to_string(&path).expect("the setup-auth corpus is readable");
    let corpus: Corpus = serde_json::from_str(&raw).expect("the setup-auth corpus parses");
    assert_eq!(
        corpus.reference.commit, REFERENCE_COMMIT,
        "the corpus was captured from an unpinned reference"
    );
    assert!(
        corpus.acp_sign_in.len() >= MINIMUM_CASES,
        "the acpSignIn family shrank to {} cases",
        corpus.acp_sign_in.len()
    );
    let mismatches: Vec<String> = corpus.acp_sign_in.iter().flat_map(replay).collect();
    println!(
        "acpSignIn: {}/{} cases conform",
        corpus.acp_sign_in.len() - mismatches.len().min(corpus.acp_sign_in.len()),
        corpus.acp_sign_in.len()
    );
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}
