use std::collections::BTreeMap;
use std::sync::Arc;

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

const SUPPORTED_PROVIDERS: [&str; 4] = ["anthropic", "mistral", "openai", "vertex"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CredentialRef {
    Environment { name: String },
    Keyring { service: String, account: String },
}

pub trait CredentialResolver {
    fn resolve(&self, reference: &CredentialRef) -> Result<Option<SecretString>, String>;
}

pub trait TlsMaterialReader {
    fn read(&self, path: &str) -> Result<Option<String>, String>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectBootstrapSettings {
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub executables: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapInput {
    pub provider: String,
    pub model: String,
    pub credential: CredentialRef,
    pub vibe_home: String,
    pub proxy: Option<String>,
    pub tls_ca_path: Option<String>,
    pub project: ProjectBootstrapSettings,
    pub workspace_trusted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapSnapshot {
    pub provider: String,
    pub model: String,
    pub credential: CredentialRef,
    pub vibe_home: String,
    pub proxy: Option<String>,
    pub tls_ca_path: Option<String>,
    pub active_extensions: Vec<String>,
    pub active_executables: Vec<String>,
}

pub struct BootstrapRuntime {
    snapshot: Arc<BootstrapSnapshot>,
    pub(crate) credential: SecretString,
}

impl BootstrapRuntime {
    #[must_use]
    pub fn snapshot(&self) -> Arc<BootstrapSnapshot> {
        Arc::clone(&self.snapshot)
    }

    #[must_use]
    pub fn credential_is_empty(&self) -> bool {
        self.credential.expose_secret().is_empty()
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BootstrapError {
    #[error("unsupported provider `{provider}`")]
    UnsupportedProvider { provider: String },
    #[error("model must not be empty")]
    MissingModel,
    #[error("credential environment reference `{name}` is invalid")]
    InvalidEnvironmentReference { name: String },
    #[error("credential `{reference}` is unavailable")]
    MissingCredential { reference: String },
    #[error("credential lookup failed: {0}")]
    CredentialLookup(String),
    #[error("proxy URL is invalid: {0}")]
    InvalidProxy(String),
    #[error("proxy URL must not contain credentials")]
    ProxyContainsCredentials,
    #[error("TLS material `{path}` is unavailable")]
    MissingTlsMaterial { path: String },
    #[error("TLS material `{path}` is not a PEM certificate")]
    InvalidTlsMaterial { path: String },
    #[error("TLS material lookup failed: {0}")]
    TlsMaterialLookup(String),
}

pub fn load_bootstrap(
    input: BootstrapInput,
    credentials: &impl CredentialResolver,
    tls: &impl TlsMaterialReader,
) -> Result<BootstrapRuntime, BootstrapError> {
    if !SUPPORTED_PROVIDERS.contains(&input.provider.as_str()) {
        return Err(BootstrapError::UnsupportedProvider {
            provider: input.provider,
        });
    }
    if input.model.trim().is_empty() {
        return Err(BootstrapError::MissingModel);
    }
    validate_credential_reference(&input.credential)?;
    let credential = credentials
        .resolve(&input.credential)
        .map_err(BootstrapError::CredentialLookup)?
        .filter(|secret| !secret.expose_secret().is_empty())
        .ok_or_else(|| BootstrapError::MissingCredential {
            reference: credential_label(&input.credential),
        })?;
    let proxy = validate_proxy(input.proxy)?;
    validate_tls(input.tls_ca_path.as_deref(), tls)?;
    let (active_extensions, active_executables) = if input.workspace_trusted {
        (input.project.extensions, input.project.executables)
    } else {
        (Vec::new(), Vec::new())
    };
    let snapshot = BootstrapSnapshot {
        provider: input.provider,
        model: input.model,
        credential: input.credential,
        vibe_home: input.vibe_home,
        proxy,
        tls_ca_path: input.tls_ca_path,
        active_extensions,
        active_executables,
    };
    Ok(BootstrapRuntime {
        snapshot: Arc::new(snapshot),
        credential,
    })
}

fn validate_credential_reference(reference: &CredentialRef) -> Result<(), BootstrapError> {
    if let CredentialRef::Environment { name } = reference {
        let mut chars = name.chars();
        let starts_valid = chars
            .next()
            .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic());
        if !starts_valid || !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
            return Err(BootstrapError::InvalidEnvironmentReference { name: name.clone() });
        }
    }
    Ok(())
}

fn credential_label(reference: &CredentialRef) -> String {
    match reference {
        CredentialRef::Environment { name } => format!("environment:{name}"),
        CredentialRef::Keyring { service, account } => format!("keyring:{service}/{account}"),
    }
}

fn validate_proxy(proxy: Option<String>) -> Result<Option<String>, BootstrapError> {
    let Some(proxy) = proxy else {
        return Ok(None);
    };
    let parsed =
        Url::parse(&proxy).map_err(|error| BootstrapError::InvalidProxy(error.to_string()))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(BootstrapError::ProxyContainsCredentials);
    }
    if !matches!(parsed.scheme(), "http" | "https" | "socks5") {
        return Err(BootstrapError::InvalidProxy(format!(
            "unsupported scheme `{}`",
            parsed.scheme()
        )));
    }
    Ok(Some(proxy))
}

fn validate_tls(
    tls_ca_path: Option<&str>,
    reader: &impl TlsMaterialReader,
) -> Result<(), BootstrapError> {
    let Some(path) = tls_ca_path else {
        return Ok(());
    };
    let material = reader
        .read(path)
        .map_err(BootstrapError::TlsMaterialLookup)?
        .ok_or_else(|| BootstrapError::MissingTlsMaterial {
            path: path.to_owned(),
        })?;
    if !material.contains("-----BEGIN CERTIFICATE-----")
        || !material.contains("-----END CERTIFICATE-----")
    {
        return Err(BootstrapError::InvalidTlsMaterial {
            path: path.to_owned(),
        });
    }
    Ok(())
}

#[derive(Default)]
pub struct InMemoryCredentialResolver {
    entries: BTreeMap<String, SecretString>,
}

impl InMemoryCredentialResolver {
    pub fn insert(&mut self, reference: &CredentialRef, secret: impl Into<String>) {
        self.entries.insert(
            credential_label(reference),
            SecretString::from(secret.into()),
        );
    }
}

impl CredentialResolver for InMemoryCredentialResolver {
    fn resolve(&self, reference: &CredentialRef) -> Result<Option<SecretString>, String> {
        Ok(self.entries.get(&credential_label(reference)).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serves exactly one certificate path, and nothing else.
    #[derive(Default)]
    struct FakeTlsReader {
        certificate_path: Option<String>,
    }

    impl FakeTlsReader {
        fn with_certificate(path: &str) -> Self {
            Self {
                certificate_path: Some(path.to_owned()),
            }
        }
    }

    impl TlsMaterialReader for FakeTlsReader {
        fn read(&self, path: &str) -> Result<Option<String>, String> {
            if self.certificate_path.as_deref() == Some(path) {
                return Ok(Some(
                    "-----BEGIN CERTIFICATE-----\nmaterial\n-----END CERTIFICATE-----".to_owned(),
                ));
            }
            Ok(None)
        }
    }

    fn input(reference: CredentialRef) -> BootstrapInput {
        BootstrapInput {
            provider: "mistral".to_owned(),
            model: "codestral".to_owned(),
            credential: reference,
            vibe_home: "/vibe-home".to_owned(),
            proxy: Some("https://proxy.example".to_owned()),
            tls_ca_path: Some("/certs/ca.pem".to_owned()),
            project: ProjectBootstrapSettings {
                extensions: vec!["project.py".to_owned()],
                executables: vec!["hook.sh".to_owned()],
            },
            workspace_trusted: false,
        }
    }

    #[test]
    fn snapshot_never_projects_secret_or_untrusted_executables() {
        let reference = CredentialRef::Environment {
            name: "MISTRAL_API_KEY".to_owned(),
        };
        let mut credentials = InMemoryCredentialResolver::default();
        credentials.insert(&reference, "super-secret");
        let tls = FakeTlsReader::with_certificate("/certs/ca.pem");

        let runtime =
            load_bootstrap(input(reference), &credentials, &tls).expect("valid bootstrap fixture");
        let encoded = serde_json::to_string(&*runtime.snapshot()).expect("snapshot serializes");

        assert!(!encoded.contains("super-secret"));
        assert!(!encoded.contains("project.py"));
        assert!(!encoded.contains("hook.sh"));
        assert!(!runtime.credential_is_empty());
    }

    #[test]
    fn invalid_inputs_fail_before_runtime_creation() {
        let reference = CredentialRef::Environment {
            name: "MISSING KEY".to_owned(),
        };
        let credentials = InMemoryCredentialResolver::default();
        let tls = FakeTlsReader::default();
        assert!(matches!(
            load_bootstrap(input(reference), &credentials, &tls),
            Err(BootstrapError::InvalidEnvironmentReference { .. })
        ));
    }

    #[test]
    fn missing_credentials_and_invalid_transport_settings_are_typed() {
        let reference = CredentialRef::Environment {
            name: "MISTRAL_API_KEY".to_owned(),
        };
        let credentials = InMemoryCredentialResolver::default();
        let tls = FakeTlsReader::with_certificate("/certs/ca.pem");
        assert!(matches!(
            load_bootstrap(input(reference.clone()), &credentials, &tls),
            Err(BootstrapError::MissingCredential { .. })
        ));

        let mut credentials = InMemoryCredentialResolver::default();
        credentials.insert(&reference, "secret");
        let mut invalid_proxy = input(reference.clone());
        invalid_proxy.proxy = Some("https://user:password@proxy.example".to_owned());
        assert_eq!(
            load_bootstrap(invalid_proxy, &credentials, &tls)
                .err()
                .expect("proxy credentials are rejected"),
            BootstrapError::ProxyContainsCredentials
        );

        assert!(matches!(
            load_bootstrap(input(reference), &credentials, &FakeTlsReader::default()),
            Err(BootstrapError::MissingTlsMaterial { .. })
        ));
    }
}
