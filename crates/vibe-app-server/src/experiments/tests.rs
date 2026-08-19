//! What a session's enrollment changes: the configuration the next load
//! composes, the census the next event carries, and the state the next session
//! reads back.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use tempfile::TempDir;
use vibe_core::identity::{IdentityFuture, IdentityResolver, IdentityResult};
use vibe_core::telemetry::ExperimentExposures;

use crate::workspace::{WorkspacePaths, WorkspaceService};

use super::{Credentials, SessionExperiments};

const ORACLE_VARIABLE: &str = "ORACLE_MISTRAL_KEY";
const ORACLE_KEY: &str = "oracle-mistral-sentinel";

/// A rollout that forces the managed shell on and reports a confirmed exposure
/// for it, which is the pair that proves configuration and telemetry read the
/// same response differently.
const MANAGED_SHELL_ROLLOUT: &str = r#"{"features": {"vibe_cli_managed_shell_tools": {
    "defaultValue": "legacy",
    "rules": [{"force": "managed", "tracks": [{
        "experiment": {"key": "vibe_cli_managed_shell_tools"},
        "result": {"key": "1", "variationId": 1, "value": "managed", "inExperiment": true}}]}]}}}"#;

/// The document a session composes over: an eval host this test never reaches,
/// a Mistral provider whose variable resolves, and telemetry on.
fn document(port: u16) -> String {
    format!(
        r#"
enable_telemetry = true
active_model = "oracle"

[experiments]
enable = true
api_host = "http://127.0.0.1:{port}"
client_key = "sdk-oracle"

[[providers]]
name = "mistral"
api_base = "http://127.0.0.1:{port}"
api_key_env_var = "{ORACLE_VARIABLE}"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
"#
    )
}

fn credentials() -> Credentials {
    Arc::new(|name: &str| (name == ORACLE_VARIABLE).then(|| ORACLE_KEY.to_owned()))
}

/// A resolver that answers no organization and counts what it was asked.
struct CountingIdentity(std::sync::atomic::AtomicUsize);

impl CountingIdentity {
    fn count(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl IdentityResolver for CountingIdentity {
    fn resolve<'a>(
        &'a self,
        _base_url: &'a str,
        _api_key: &'a str,
        _timeout: Option<std::time::Duration>,
    ) -> IdentityFuture<'a, Option<IdentityResult>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Box::pin(std::future::ready(None))
    }
}

/// A service over a scratch home carrying `document`.
fn service(root: &Path, document: &str) -> WorkspaceService {
    let home = root.join("home/.vibe");
    let working = root.join("project");
    std::fs::create_dir_all(&home).expect("the home directory");
    std::fs::create_dir_all(&working).expect("the project directory");
    std::fs::write(home.join("config.toml"), document).expect("the document writes");
    WorkspaceService::new(
        WorkspacePaths {
            session_root: home.join("sessions"),
            vibe_home: home,
            working_directory: working,
        },
        true,
    )
    .expect("the service builds")
}

/// An eval endpoint that answers one authored body and counts its requests.
struct EvalStub {
    port: u16,
    requests: Arc<std::sync::atomic::AtomicUsize>,
    handle: tokio::task::JoinHandle<()>,
}

impl EvalStub {
    /// A listener bound to a loopback port, answering `body` to every request.
    ///
    /// The stub is what makes "no request was issued" observable: a scenario
    /// that never connects leaves the counter at zero, and one that does leaves
    /// it at one, without either depending on a rollout service.
    async fn start(body: &'static str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback listener binds");
        let port = listener.local_addr().expect("the bound address").port();
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                let mut scratch = [0_u8; 4096];
                drop(stream.read(&mut scratch).await);
                drop(stream.write_all(response.as_bytes()).await);
                drop(stream.shutdown().await);
            }
        });
        Self {
            port,
            requests,
            handle,
        }
    }

    fn count(&self) -> usize {
        self.requests.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for EvalStub {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// One session, resolved end to end over the stub.
async fn resolve_over(
    root: &TempDir,
    document: &str,
    identity: Arc<dyn IdentityResolver>,
) -> (WorkspaceService, ExperimentExposures, String) {
    let service = service(root.path(), document);
    let exposures = ExperimentExposures::default();
    let session = service
        .session_store()
        .create("oracle-session", "/tmp", None, 1)
        .expect("the session is created");
    let experiments = Arc::new(
        SessionExperiments::new(&service, credentials(), None, exposures.clone())
            .resolving_identity_through(identity),
    );
    experiments.resolve(&session.id).await;
    experiments.close().await;
    (service, exposures, session.id)
}

#[tokio::test]
async fn a_resolved_rollout_reaches_the_configuration_the_census_and_the_session() {
    let stub = EvalStub::start(MANAGED_SHELL_ROLLOUT).await;
    let root = tempfile::tempdir().expect("a scratch directory");
    let identity = Arc::new(CountingIdentity(std::sync::atomic::AtomicUsize::new(0)));
    let (service, exposures, session_id) =
        resolve_over(&root, &document(stub.port), Arc::clone(&identity) as _).await;

    assert_eq!(stub.count(), 1, "the lookup issued exactly one request");
    assert_eq!(
        identity.count(),
        1,
        "the organization attribute was asked for"
    );

    // The configuration: the variant is composed by the next load, and the
    // resolver that follows a load publishes the family it selects.
    let snapshot = service.layered_config().load().expect("the load composes");
    assert_eq!(
        snapshot
            .effective
            .get("managed_shell_tools_enabled")
            .and_then(toml::Value::as_bool),
        Some(true),
        "the assignment reaches the merged document"
    );
    assert!(
        service.tool_config().managed_shell_tools_enabled(),
        "the tool surface reads the rollout the next registration publishes"
    );

    // The census: only a confirmed exposure, under the feature's own key.
    assert_eq!(
        exposures.resolved().get("vibe_cli_managed_shell_tools"),
        Some(&"managed".to_owned())
    );

    // The session: what it resolved is on disk under the reference's key.
    let metadata = service
        .session_store()
        .metadata(&session_id)
        .expect("the metadata reads back");
    assert_eq!(
        metadata
            .experiment_state
            .pointer("/features/vibe_cli_managed_shell_tools/rules/0/force")
            .and_then(serde_json::Value::as_str),
        Some("managed")
    );
}

/// A force is not an enrollment. The same response reaches configuration and
/// telemetry, and only configuration acts on it, which is what keeps a pinned
/// build out of the rollout's own analysis.
#[tokio::test]
async fn a_forced_variant_configures_without_reporting_an_exposure() {
    const FORCED: &str = r#"{"features": {"vibe_cli_managed_shell_tools": {
        "defaultValue": "legacy", "rules": [{"force": "managed"}]}}}"#;
    let stub = EvalStub::start(FORCED).await;
    let root = tempfile::tempdir().expect("a scratch directory");
    let identity = Arc::new(CountingIdentity(std::sync::atomic::AtomicUsize::new(0)));
    let (service, exposures, _) =
        resolve_over(&root, &document(stub.port), Arc::clone(&identity) as _).await;
    assert_eq!(
        service
            .layered_config()
            .load()
            .expect("the load composes")
            .effective
            .get("managed_shell_tools_enabled")
            .and_then(toml::Value::as_bool),
        Some(true),
        "configuration honors the force"
    );
    assert!(
        exposures.resolved().is_empty(),
        "telemetry reports no enrollment for a force"
    );
}

#[tokio::test]
async fn a_session_that_already_resolved_hydrates_without_a_second_request() {
    let stub = EvalStub::start(MANAGED_SHELL_ROLLOUT).await;
    let root = tempfile::tempdir().expect("a scratch directory");
    let document = document(stub.port);
    let identity = Arc::new(CountingIdentity(std::sync::atomic::AtomicUsize::new(0)));
    let (service, _, session_id) = resolve_over(&root, &document, Arc::clone(&identity) as _).await;
    assert_eq!(stub.count(), 1);

    // A second runtime over the same session, which is what a resume opens and
    // what a fork inherits, since the fork copies the metadata field.
    let exposures = ExperimentExposures::default();
    let resumed = Arc::new(
        SessionExperiments::new(&service, credentials(), None, exposures.clone())
            .resolving_identity_through(Arc::clone(&identity) as _),
    );
    resumed.resolve(&session_id).await;
    resumed.close().await;

    assert_eq!(stub.count(), 1, "a resumed session issues no eval request");
    assert_eq!(identity.count(), 1, "and no identity request either");
    assert_eq!(
        exposures.resolved().get("vibe_cli_managed_shell_tools"),
        Some(&"managed".to_owned()),
        "it resolves the same variants it wrote"
    );
}

/// A fork inherits what its parent resolved, because the fork copies the
/// metadata field the parent wrote. The child therefore hydrates and issues no
/// lookup of its own.
#[tokio::test]
async fn a_forked_session_inherits_its_parent_state_without_a_request() {
    let stub = EvalStub::start(MANAGED_SHELL_ROLLOUT).await;
    let root = tempfile::tempdir().expect("a scratch directory");
    let identity = Arc::new(CountingIdentity(std::sync::atomic::AtomicUsize::new(0)));
    let (service, _, parent_id) =
        resolve_over(&root, &document(stub.port), Arc::clone(&identity) as _).await;
    assert_eq!(stub.count(), 1);

    let child = service
        .session_store()
        .fork(&parent_id, "forked-session", "prompt", BTreeMap::new(), 2)
        .expect("the fork succeeds");
    assert_eq!(
        child
            .metadata
            .experiment_state
            .pointer("/features/vibe_cli_managed_shell_tools/rules/0/force")
            .and_then(serde_json::Value::as_str),
        Some("managed"),
        "the fork carried the parent's resolved state"
    );

    let exposures = ExperimentExposures::default();
    let forked = Arc::new(
        SessionExperiments::new(&service, credentials(), None, exposures.clone())
            .resolving_identity_through(Arc::clone(&identity) as _),
    );
    forked.resolve(&child.metadata.id).await;
    forked.close().await;

    assert_eq!(stub.count(), 1, "a forked session issues no eval request");
    assert_eq!(identity.count(), 1, "and no identity request either");
    assert_eq!(
        exposures.resolved().get("vibe_cli_managed_shell_tools"),
        Some(&"managed".to_owned()),
        "it resolves the variants its parent did"
    );
}

#[tokio::test]
async fn a_disabled_gate_issues_nothing_and_reports_nothing() {
    for gate in ["enable_telemetry = false", "experiments_off"] {
        let stub = EvalStub::start(MANAGED_SHELL_ROLLOUT).await;
        let root = tempfile::tempdir().expect("a scratch directory");
        let document = match gate {
            "experiments_off" => document(stub.port).replace("enable = true", "enable = false"),
            other => document(stub.port).replace("enable_telemetry = true", other),
        };
        let identity = Arc::new(CountingIdentity(std::sync::atomic::AtomicUsize::new(0)));
        let (service, exposures, session_id) =
            resolve_over(&root, &document, Arc::clone(&identity) as _).await;

        assert_eq!(stub.count(), 0, "{gate} issues no eval request");
        assert_eq!(identity.count(), 0, "{gate} issues no identity request");
        assert!(
            exposures.resolved().is_empty(),
            "{gate} reports no exposure"
        );
        assert_eq!(
            service
                .layered_config()
                .load()
                .expect("the load composes")
                .effective
                .get("managed_shell_tools_enabled")
                .and_then(toml::Value::as_bool),
            Some(false),
            "{gate} leaves the declared default"
        );
        assert!(
            service
                .session_store()
                .metadata(&session_id)
                .expect("the metadata reads back")
                .experiment_state
                .is_null(),
            "{gate} persists nothing"
        );
    }
}

/// A session that closes while a lookup is still going cancels it rather than
/// waiting for it. The stub accepts the connection and never answers, so a
/// close that waited would hang until the eval budget expired.
#[tokio::test]
async fn closing_cancels_a_lookup_in_flight() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback listener binds");
    let port = listener.local_addr().expect("the bound address").port();
    let silent = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    let root = tempfile::tempdir().expect("a scratch directory");
    let service = service(root.path(), &document(port));
    let session = service
        .session_store()
        .create("oracle-session", "/tmp", None, 1)
        .expect("the session is created");
    let experiments = Arc::new(SessionExperiments::new(
        &service,
        credentials(),
        None,
        ExperimentExposures::default(),
    ));
    experiments.start(&session.id);
    let closed = tokio::time::timeout(std::time::Duration::from_secs(2), experiments.close()).await;
    assert!(
        closed.is_ok(),
        "the close cancelled the lookup instead of waiting on it"
    );
    silent.abort();
}
