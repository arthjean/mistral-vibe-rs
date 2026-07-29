use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use secrecy::SecretString;
use thiserror::Error;
use tokio::sync::Notify;

use crate::bootstrap::{CredentialRef, CredentialResolver, TlsMaterialReader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalDependency {
    Provider,
    Keyring,
    UserConfig,
    GitIdentity,
    Network,
    HostFile,
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("hermetic harness denied external dependency: {0:?}")]
pub struct HermeticViolation(pub ExternalDependency);

#[derive(Debug, Default)]
pub struct HermeticGuard;

impl HermeticGuard {
    pub fn deny<T>(&self, dependency: ExternalDependency) -> Result<T, HermeticViolation> {
        Err(HermeticViolation(dependency))
    }
}

#[derive(Debug, Default)]
pub struct FakeClock {
    millis: Mutex<u64>,
}

impl FakeClock {
    pub fn now_millis(&self) -> u64 {
        self.millis.lock().map_or(0, |value| *value)
    }

    pub fn advance(&self, duration: Duration) {
        if let Ok(mut value) = self.millis.lock() {
            *value = value.saturating_add(duration.as_millis() as u64);
        }
    }
}

#[derive(Debug, Default)]
pub struct FakeIdSource {
    values: Mutex<VecDeque<String>>,
}

impl FakeIdSource {
    pub fn new(values: impl IntoIterator<Item = String>) -> Self {
        Self {
            values: Mutex::new(values.into_iter().collect()),
        }
    }

    pub fn next(&self) -> Option<String> {
        self.values.lock().ok()?.pop_front()
    }
}

#[derive(Debug, Clone, Default)]
pub struct FakeEnvironment {
    pub home: String,
    pub cwd: String,
    pub terminal_size: (u16, u16),
    pub values: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct FakeProvider {
    chunks: Arc<Mutex<VecDeque<Result<String, String>>>>,
}

impl FakeProvider {
    pub fn scripted(chunks: impl IntoIterator<Item = Result<String, String>>) -> Self {
        Self {
            chunks: Arc::new(Mutex::new(chunks.into_iter().collect())),
        }
    }

    pub fn next(&self) -> Option<Result<String, String>> {
        self.chunks.lock().ok()?.pop_front()
    }
}

#[derive(Debug, Clone, Default)]
pub struct FakeMcpPeer {
    responses: Arc<Mutex<VecDeque<Result<String, String>>>>,
}

impl FakeMcpPeer {
    pub fn scripted(responses: impl IntoIterator<Item = Result<String, String>>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into_iter().collect())),
        }
    }

    pub fn respond(&self) -> Option<Result<String, String>> {
        self.responses.lock().ok()?.pop_front()
    }
}

#[derive(Debug, Clone, Default)]
pub struct FakeFileSystem {
    files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
}

impl FakeFileSystem {
    pub fn write(&self, path: impl Into<String>, bytes: impl Into<Vec<u8>>) {
        if let Ok(mut files) = self.files.lock() {
            files.insert(path.into(), bytes.into());
        }
    }

    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        self.files.lock().ok()?.get(path).cloned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeProcessOutcome {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Default)]
pub struct FakeProcessRunner {
    outcomes: Arc<Mutex<VecDeque<FakeProcessOutcome>>>,
}

impl FakeProcessRunner {
    pub fn scripted(outcomes: impl IntoIterator<Item = FakeProcessOutcome>) -> Self {
        Self {
            outcomes: Arc::new(Mutex::new(outcomes.into_iter().collect())),
        }
    }

    pub fn run(&self) -> Option<FakeProcessOutcome> {
        self.outcomes.lock().ok()?.pop_front()
    }
}

#[derive(Debug, Clone, Default)]
pub struct FakeKeyring {
    values: Arc<Mutex<BTreeMap<String, SecretString>>>,
}

impl FakeKeyring {
    pub fn insert(&self, reference: &CredentialRef, secret: impl Into<String>) {
        if let Ok(mut values) = self.values.lock() {
            values.insert(reference_key(reference), SecretString::from(secret.into()));
        }
    }
}

impl CredentialResolver for FakeKeyring {
    fn resolve(&self, reference: &CredentialRef) -> Result<Option<SecretString>, String> {
        Ok(self
            .values
            .lock()
            .map_err(|_| "fake keyring lock poisoned".to_owned())?
            .get(&reference_key(reference))
            .cloned())
    }
}

fn reference_key(reference: &CredentialRef) -> String {
    match reference {
        CredentialRef::Environment { name } => format!("env:{name}"),
        CredentialRef::Keyring { service, account } => format!("keyring:{service}:{account}"),
    }
}

#[derive(Debug, Clone, Default)]
pub struct FakeTlsReader {
    materials: Arc<Mutex<BTreeMap<String, String>>>,
}

impl FakeTlsReader {
    pub fn with_certificate(path: &str) -> Self {
        let reader = Self::default();
        if let Ok(mut materials) = reader.materials.lock() {
            materials.insert(
                path.to_owned(),
                "-----BEGIN CERTIFICATE-----\nFAKE\n-----END CERTIFICATE-----".to_owned(),
            );
        }
        reader
    }
}

impl TlsMaterialReader for FakeTlsReader {
    fn read(&self, path: &str) -> Result<Option<String>, String> {
        Ok(self
            .materials
            .lock()
            .map_err(|_| "fake TLS reader lock poisoned".to_owned())?
            .get(path)
            .cloned())
    }
}

#[derive(Debug, Clone, Default)]
pub struct FakeScheduler {
    gates: Arc<Mutex<BTreeMap<String, Arc<Notify>>>>,
}

impl FakeScheduler {
    pub async fn wait(&self, gate: &str) {
        self.gate(gate).notified().await;
    }

    pub fn open(&self, gate: &str) {
        self.gate(gate).notify_one();
    }

    fn gate(&self, name: &str) -> Arc<Notify> {
        let Ok(mut gates) = self.gates.lock() else {
            return Arc::new(Notify::new());
        };
        Arc::clone(
            gates
                .entry(name.to_owned())
                .or_insert_with(|| Arc::new(Notify::new())),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scheduler_reproduces_order_without_sleeping() {
        let scheduler = FakeScheduler::default();
        let waiting = scheduler.clone();
        let task = tokio::spawn(async move {
            waiting.wait("response-written").await;
            "model-started"
        });
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        scheduler.open("response-written");
        assert_eq!(task.await.expect("fake task completes"), "model-started");
    }

    #[test]
    fn all_external_boundaries_fail_closed() {
        let guard = HermeticGuard;
        for dependency in [
            ExternalDependency::Provider,
            ExternalDependency::Keyring,
            ExternalDependency::UserConfig,
            ExternalDependency::GitIdentity,
            ExternalDependency::Network,
            ExternalDependency::HostFile,
        ] {
            assert_eq!(
                guard.deny::<()>(dependency),
                Err(HermeticViolation(dependency))
            );
        }
    }
}
