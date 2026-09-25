//! The access token a Vertex AI request carries.
//!
//! Reference `VertexCredentials` (`vibe/core/llm/backend/vertex.py`), which
//! asks `google.auth.default` for Application Default Credentials scoped to
//! the cloud platform once per process, and refreshes them whenever they are
//! no longer valid. The lookup order is the one that library documents: the
//! file `GOOGLE_APPLICATION_CREDENTIALS` names, then the file `gcloud auth
//! application-default login` writes, then the metadata server of a Google
//! Compute Engine host. A credentials file may hold a user's refresh token, a
//! service account key, or an impersonation of a service account by either;
//! workload identity federation (`external_account`) is refused.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::{Value, json};

use super::error::{LocalFailure, LocalKind};
use super::{TokenFuture, VertexAccess};

/// The scope every Vertex token is minted for.
pub const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const IAM_SCOPE: &str = "https://www.googleapis.com/auth/iam";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const JWT_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
/// How long a minted assertion and an impersonated token live.
const TOKEN_LIFETIME: Duration = Duration::from_secs(3_600);
/// A token this close to its expiry is refreshed, as `google.auth` does.
const REFRESH_THRESHOLD: Duration = Duration::from_secs(225);
const METADATA_TIMEOUT: Duration = Duration::from_secs(3);

/// The regional address a Vertex request goes to. Reference
/// `build_vertex_base_url`.
#[must_use]
pub fn base_url(region: &str) -> String {
    if region == "global" {
        "https://aiplatform.googleapis.com".to_owned()
    } else {
        format!("https://{region}-aiplatform.googleapis.com")
    }
}

/// The path of one model call. Reference `build_vertex_endpoint`.
#[must_use]
pub fn endpoint(region: &str, project_id: &str, model: &str, streaming: bool) -> String {
    let action = if streaming {
        "streamRawPredict"
    } else {
        "rawPredict"
    };
    format!(
        "/v1/projects/{project_id}/locations/{region}/publishers/anthropic/models/{model}:{action}"
    )
}

/// The process-wide credentials every Vertex backend shares, so a refresh
/// happens once per process rather than once per call.
#[must_use]
pub fn shared_credentials() -> Arc<dyn VertexAccess> {
    static SHARED: OnceLock<Arc<ApplicationDefault>> = OnceLock::new();
    SHARED
        .get_or_init(|| Arc::new(ApplicationDefault::default()))
        .clone()
}

/// Application Default Credentials, found on first use.
#[derive(Default)]
pub struct ApplicationDefault {
    state: tokio::sync::Mutex<Option<Minted>>,
}

struct Minted {
    source: Source,
    token: Option<Token>,
}

struct Token {
    value: String,
    expiry: Option<SystemTime>,
}

impl Token {
    fn valid(&self) -> bool {
        self.expiry
            .is_none_or(|expiry| SystemTime::now() + REFRESH_THRESHOLD < expiry)
    }
}

impl VertexAccess for ApplicationDefault {
    fn access_token(&self) -> TokenFuture<'_> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            if state.is_none() {
                *state = Some(Minted {
                    source: discover().await?,
                    token: None,
                });
            }
            let Some(minted) = state.as_mut() else {
                return Err(failure("Vertex AI credentials could not be loaded"));
            };
            if let Some(token) = minted.token.as_ref().filter(|token| token.valid()) {
                return Ok(token.value.clone());
            }
            let client = http_client()?;
            let token = minted.source.refresh(&client).await?;
            let value = token.value.clone();
            minted.token = Some(token);
            Ok(value)
        })
    }
}

enum Source {
    User {
        client_id: String,
        client_secret: String,
        refresh_token: String,
        token_uri: String,
    },
    ServiceAccount(ServiceAccount),
    Impersonated {
        source: Box<Source>,
        url: String,
        delegates: Vec<Value>,
    },
    Metadata {
        host: String,
    },
}

struct ServiceAccount {
    client_email: String,
    private_key: String,
    private_key_id: Option<String>,
    token_uri: String,
    scope: &'static str,
}

impl Source {
    fn refresh<'a>(
        &'a self,
        client: &'a reqwest::Client,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Token, LocalFailure>> + Send + 'a>>
    {
        Box::pin(async move {
            match self {
                Self::User {
                    client_id,
                    client_secret,
                    refresh_token,
                    token_uri,
                } => {
                    let form = [
                        ("grant_type", "refresh_token"),
                        ("client_id", client_id.as_str()),
                        ("client_secret", client_secret.as_str()),
                        ("refresh_token", refresh_token.as_str()),
                    ];
                    oauth_token(client.post(token_uri).form(&form)).await
                }
                Self::ServiceAccount(account) => {
                    let assertion = account.assertion()?;
                    let form = [("grant_type", JWT_GRANT), ("assertion", assertion.as_str())];
                    oauth_token(client.post(&account.token_uri).form(&form)).await
                }
                Self::Impersonated {
                    source,
                    url,
                    delegates,
                } => {
                    let started = SystemTime::now();
                    let source_token = source.refresh(client).await?;
                    let body = json!({
                        "delegates": delegates,
                        "scope": [CLOUD_PLATFORM_SCOPE],
                        "lifetime": format!("{}s", TOKEN_LIFETIME.as_secs()),
                    });
                    let answer = read_json(
                        client
                            .post(url)
                            .bearer_auth(&source_token.value)
                            .json(&body),
                    )
                    .await?;
                    let value = answer
                        .get("accessToken")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            failure("the impersonation answer carries no access token")
                        })?;
                    Ok(Token {
                        value: value.to_owned(),
                        expiry: Some(started + TOKEN_LIFETIME),
                    })
                }
                Self::Metadata { host } => {
                    let url = format!(
                        "http://{host}/computeMetadata/v1/instance/service-accounts/default/token?scopes={CLOUD_PLATFORM_SCOPE}"
                    );
                    oauth_token(client.get(url).header("Metadata-Flavor", "Google")).await
                }
            }
        })
    }
}

impl ServiceAccount {
    fn from_info(info: &Value, scope: &'static str) -> Result<Self, LocalFailure> {
        Ok(Self {
            client_email: field(info, "client_email")?,
            private_key: field(info, "private_key")?,
            private_key_id: info
                .get("private_key_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            token_uri: info
                .get("token_uri")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_TOKEN_URI)
                .to_owned(),
            scope,
        })
    }

    /// The signed JWT the token endpoint exchanges for an access token.
    fn assertion(&self) -> Result<String, LocalFailure> {
        use aws_lc_rs::{rand, signature};

        let issued = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let mut header = json!({"alg": "RS256", "typ": "JWT"});
        if let Some(id) = &self.private_key_id {
            header["kid"] = Value::String(id.clone());
        }
        let claims = json!({
            "iss": self.client_email,
            "aud": self.token_uri,
            "scope": self.scope,
            "iat": issued,
            "exp": issued + TOKEN_LIFETIME.as_secs(),
        });
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let key = signature::RsaKeyPair::from_pkcs8(&pem_body(&self.private_key)?)
            .map_err(|_| failure("the service account private key is not a PKCS #8 RSA key"))?;
        let mut signed = vec![0; key.public_modulus_len()];
        key.sign(
            &signature::RSA_PKCS1_SHA256,
            &rand::SystemRandom::new(),
            signing_input.as_bytes(),
            &mut signed,
        )
        .map_err(|_| failure("the service account assertion could not be signed"))?;
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signed)
        ))
    }
}

fn pem_body(pem: &str) -> Result<Vec<u8>, LocalFailure> {
    let body: String = pem
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("-----"))
        .collect();
    STANDARD
        .decode(body)
        .map_err(|_| failure("the service account private key is not valid PEM"))
}

async fn discover() -> Result<Source, LocalFailure> {
    if let Some(path) = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS") {
        let path = PathBuf::from(path);
        if !path.is_file() {
            return Err(failure(format!(
                "GOOGLE_APPLICATION_CREDENTIALS points at {}, which is not a file",
                path.display()
            )));
        }
        return from_file(&path);
    }
    if let Some(path) = gcloud_file().filter(|path| path.is_file()) {
        return from_file(&path);
    }
    if on_compute_engine().await {
        return Ok(Source::Metadata {
            host: std::env::var("GCE_METADATA_HOST")
                .unwrap_or_else(|_| "metadata.google.internal".to_owned()),
        });
    }
    Err(failure(
        "no Application Default Credentials were found: set GOOGLE_APPLICATION_CREDENTIALS or run `gcloud auth application-default login`",
    ))
}

fn gcloud_file() -> Option<PathBuf> {
    let directory = match std::env::var_os("CLOUDSDK_CONFIG") {
        Some(directory) => PathBuf::from(directory),
        None if cfg!(windows) => PathBuf::from(std::env::var_os("APPDATA")?).join("gcloud"),
        None => std::env::home_dir()?.join(".config").join("gcloud"),
    };
    Some(directory.join("application_default_credentials.json"))
}

fn from_file(path: &std::path::Path) -> Result<Source, LocalFailure> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        failure(format!(
            "reading credentials {} failed: {error}",
            path.display()
        ))
    })?;
    let info: Value = serde_json::from_str(&text).map_err(|error| {
        failure(format!(
            "credentials {} are not JSON: {error}",
            path.display()
        ))
    })?;
    from_info(&info, CLOUD_PLATFORM_SCOPE)
}

fn from_info(info: &Value, scope: &'static str) -> Result<Source, LocalFailure> {
    match info.get("type").and_then(Value::as_str).unwrap_or_default() {
        "authorized_user" => Ok(Source::User {
            client_id: field(info, "client_id")?,
            client_secret: field(info, "client_secret")?,
            refresh_token: field(info, "refresh_token")?,
            token_uri: info
                .get("token_uri")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_TOKEN_URI)
                .to_owned(),
        }),
        "service_account" => ServiceAccount::from_info(info, scope).map(Source::ServiceAccount),
        "impersonated_service_account" => {
            let source = info
                .get("source_credentials")
                .ok_or_else(|| failure("impersonated credentials name no source credentials"))?;
            Ok(Source::Impersonated {
                source: Box::new(from_info(source, IAM_SCOPE)?),
                url: field(info, "service_account_impersonation_url")?,
                delegates: info
                    .get("delegates")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            })
        }
        other => Err(failure(format!(
            "credentials of type {other:?} are not supported for Vertex AI"
        ))),
    }
}

async fn on_compute_engine() -> bool {
    if std::env::var("NO_GCE_CHECK").is_ok_and(|value| value.eq_ignore_ascii_case("true")) {
        return false;
    }
    let ip = std::env::var("GCE_METADATA_IP").unwrap_or_else(|_| "169.254.169.254".to_owned());
    let Ok(client) = reqwest::Client::builder()
        .timeout(METADATA_TIMEOUT)
        .no_proxy()
        .build()
    else {
        return false;
    };
    let answered = client
        .get(format!("http://{ip}"))
        .header("Metadata-Flavor", "Google")
        .send()
        .await
        .is_ok_and(|response| {
            response
                .headers()
                .get("Metadata-Flavor")
                .is_some_and(|flavor| flavor == "Google")
        });
    answered
        || (cfg!(target_os = "linux")
            && std::fs::read_to_string("/sys/class/dmi/id/product_name")
                .is_ok_and(|name| name.starts_with("Google")))
}

fn http_client() -> Result<reqwest::Client, LocalFailure> {
    crate::http_trust::trust_certificate_environment(
        reqwest::Client::builder().timeout(Duration::from_secs(120)),
    )
    .build()
    .map_err(|error| failure(format!("the credentials client could not start: {error}")))
}

async fn read_json(request: reqwest::RequestBuilder) -> Result<Value, LocalFailure> {
    let response = request
        .send()
        .await
        .map_err(|error| failure(format!("refreshing Vertex AI credentials failed: {error}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| failure(format!("refreshing Vertex AI credentials failed: {error}")))?;
    if !status.is_success() {
        return Err(failure(format!(
            "refreshing Vertex AI credentials failed with HTTP {}: {text}",
            status.as_u16()
        )));
    }
    serde_json::from_str(&text)
        .map_err(|_| failure("the credentials endpoint answered with something other than JSON"))
}

async fn oauth_token(request: reqwest::RequestBuilder) -> Result<Token, LocalFailure> {
    let started = SystemTime::now();
    let answer = read_json(request).await?;
    let value = answer
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| failure("Vertex AI credential refresh did not produce a token"))?;
    let expiry = answer
        .get("expires_in")
        .and_then(|seconds| {
            seconds
                .as_u64()
                .or_else(|| seconds.as_str().and_then(|text| text.parse().ok()))
        })
        .map(|seconds| started + Duration::from_secs(seconds));
    Ok(Token {
        value: value.to_owned(),
        expiry,
    })
}

fn field(info: &Value, name: &str) -> Result<String, LocalFailure> {
    info.get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| failure(format!("the credentials carry no {name}")))
}

fn failure(message: impl Into<String>) -> LocalFailure {
    LocalFailure::new(LocalKind::Credentials, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credentials_file_is_read_by_its_type() {
        let user = json!({
            "type": "authorized_user",
            "client_id": "id",
            "client_secret": "secret",
            "refresh_token": "refresh",
        });
        assert!(matches!(
            from_info(&user, CLOUD_PLATFORM_SCOPE),
            Ok(Source::User { token_uri, .. }) if token_uri == DEFAULT_TOKEN_URI
        ));
        let federated = json!({"type": "external_account"});
        assert!(from_info(&federated, CLOUD_PLATFORM_SCOPE).is_err());
        let impersonated = json!({
            "type": "impersonated_service_account",
            "service_account_impersonation_url": "https://iam.example/sa:generateAccessToken",
            "source_credentials": user,
        });
        assert!(matches!(
            from_info(&impersonated, CLOUD_PLATFORM_SCOPE),
            Ok(Source::Impersonated { .. })
        ));
    }

    #[test]
    fn a_token_near_its_expiry_is_refreshed() {
        let fresh = Token {
            value: String::new(),
            expiry: Some(SystemTime::now() + Duration::from_secs(3_000)),
        };
        let stale = Token {
            value: String::new(),
            expiry: Some(SystemTime::now() + Duration::from_secs(60)),
        };
        assert!(fresh.valid());
        assert!(!stale.valid());
    }
}
