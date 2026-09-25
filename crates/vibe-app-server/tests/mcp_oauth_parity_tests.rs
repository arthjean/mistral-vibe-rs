//! Replays the committed MCP OAuth corpus against this port's OAuth client.
//!
//! `scripts/parity/mcp_oauth.py` captured the corpus from the pinned
//! reference's `MCPAuthenticationService`, which signs in through
//! `perform_oauth_login` and refreshes through the MCP SDK's
//! `OAuthClientProvider`. Both sides meet `vibe-oauth-script-fixture`, a
//! scripted server playing the MCP resource and its authorization server that
//! logs every request, over a keyring held in memory and seeded per scenario,
//! with a simulated browser answering each authorization URL through the
//! loopback callback. A scenario compares every step's outcome, the revisions
//! it left, the authorization URLs it published, what the browser got back and
//! the keyring afterwards, plus every request the fixture received.
//!
//! The replay drives `vibe_core::mcp::McpAuthenticationService` over a
//! `McpOAuthStore` backed by `MemoryKeyringBackend`. The callback pages are the
//! only reference-authored text in the corpus; this port serves its own, so
//! the replay requires their digests to differ. Every other difference has to
//! fall under a `LEDGER` entry, and every entry has to still reproduce.

#![cfg(feature = "test-fixtures")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;
use vibe_core::auth::mcp_oauth::fingerprint_json;
use vibe_core::auth::{
    AuthUrlSink, KeyringBackend, KeyringFailure, McpOAuthStore, MemoryKeyringBackend,
};
use vibe_core::mcp::{
    McpAuthConfig, McpAuthenticationError, McpAuthenticationService, McpAuthorization, McpDeclared,
    McpOAuthConfig, McpServerConfig, McpStaticAuth, McpTransportConfig,
};
use vibe_core::parity::{RESTORE_COMMAND, off_pin_reason, reference_root};

const CORPUS: &str = include_str!("mcp-oauth/corpus.json");
const FIXTURE: &str = env!("CARGO_BIN_EXE_vibe-oauth-script-fixture");
const CAPTURE_SCRIPT: &str = "scripts/parity/mcp_oauth.py";
const SERVICE: &str = "ai.mistral.vibe";
const STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// A difference the replay accepts, and why.
struct Divergence {
    /// The scenario it applies to, or `*` for every one.
    scenario: &'static str,
    /// A JSON pointer inside the scenario's observation, where a `*` segment
    /// matches any one segment.
    pointer: &'static str,
    /// Whether the reference's value and this port's are the difference the
    /// entry describes.
    accepts: fn(Option<&Value>, Option<&Value>) -> bool,
    why: &'static str,
}

const LEDGER: &[Divergence] = &[
    Divergence {
        scenario: "*",
        pointer: "/wire/*/headers/accept",
        accepts: |reference, port| reference.is_none() && port == Some(&json!("*/*")),
        why: "reqwest gives every request `Accept: */*` and offers no way to take it off, where \
          the SDK's flow builds its metadata, registration and token requests as bare \
          `httpx.Request`s that carry no `Accept` at all",
    },
    Divergence {
        scenario: "*",
        pointer: "/wire/*/headers/user-agent",
        accepts: |reference, port| {
            port.is_none()
                && reference
                    .and_then(Value::as_str)
                    .is_some_and(|agent| agent.starts_with("python-httpx/"))
        },
        why: "the requests httpx's client builds itself (a login's `initialize`, a refresh's \
          `GET`) name the Python HTTP library that sent them; this port's name none",
    },
];

fn pointer_matches(pattern: &str, pointer: &str) -> bool {
    let pattern = pattern.split('/').collect::<Vec<_>>();
    let pointer = pointer.split('/').collect::<Vec<_>>();
    pattern.len() == pointer.len()
        && pattern
            .iter()
            .zip(&pointer)
            .all(|(pattern, segment)| *pattern == "*" || pattern == segment)
}

fn repository() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

// --------------------------------------------------------------------------
// One scenario.
// --------------------------------------------------------------------------

struct Scenario {
    _root: tempfile::TempDir,
    process: Child,
    address: String,
    redirect: u16,
    log: PathBuf,
}

impl Scenario {
    fn start(spec: &Value) -> Self {
        let root = tempfile::tempdir().unwrap();
        let redirect = free_port();
        let script = spec["script"]
            .to_string()
            .replace("{redirect}", &redirect.to_string());
        let script_path = root.path().join("script.json");
        std::fs::write(&script_path, script).unwrap();
        let log = root.path().join("log.jsonl");
        let port_file = root.path().join("port");
        let process = Command::new(FIXTURE)
            .env("VIBE_OAUTH_SCRIPT", &script_path)
            .env("VIBE_OAUTH_LOG", &log)
            .env("VIBE_OAUTH_PORT_FILE", &port_file)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !port_file.exists() {
            assert!(
                Instant::now() < deadline,
                "the OAuth fixture never bound a port"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let address = std::fs::read_to_string(&port_file)
            .unwrap()
            .trim()
            .to_owned();
        Self {
            _root: root,
            process,
            address,
            redirect,
            log,
        }
    }

    fn base(&self) -> String {
        format!("http://{}", self.address)
    }

    fn server(&self, spec: &Value) -> McpServerConfig {
        let url = format!("{}/mcp", self.base());
        let auth = &spec["server"]["auth"];
        let (auth, headers) = if auth["type"] == "oauth" {
            let strings = |value: &Value| {
                value
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            };
            (
                McpAuthConfig::Oauth(McpOAuthConfig {
                    scopes: strings(&auth["scopes"]),
                    client_id: auth["client_id"].as_str().map(str::to_owned),
                    client_metadata_url: auth["client_metadata_url"]
                        .as_str()
                        .map(|url| Url::parse(url).unwrap()),
                    redirect_port: self.redirect,
                }),
                BTreeMap::new(),
            )
        } else {
            let headers = auth["headers"]
                .as_object()
                .map(|headers| {
                    headers
                        .iter()
                        .map(|(name, value)| (name.clone(), value.as_str().unwrap().to_owned()))
                        .collect()
                })
                .unwrap_or_default();
            (McpAuthConfig::Static(McpStaticAuth::default()), headers)
        };
        McpServerConfig {
            alias: "demo".to_owned(),
            transport: McpTransportConfig::StreamableHttp {
                url: Url::parse(&url).unwrap(),
                headers,
            },
            enabled: true,
            disabled_tools: Default::default(),
            startup_timeout_ms: 10_000,
            tool_timeout_ms: 60_000,
            auth,
            prompt: None,
            sampling_enabled: true,
            declared: Some(McpDeclared {
                url: Some(url),
                ..McpDeclared::default()
            }),
        }
    }

    fn seed(&self, backend: &dyn KeyringBackend, spec: &Value, server: &McpServerConfig) {
        let Some(entries) = spec["keyring"].as_object() else {
            return;
        };
        for (kind, value) in entries {
            if value.is_null() {
                continue;
            }
            let text = match kind.as_str() {
                "tokens" => {
                    let mut value = value.clone();
                    if let Some(offset) = value["expires_at"].as_str() {
                        value["expires_at"] = json!(now() + offset.parse::<f64>().unwrap());
                    }
                    value.to_string()
                }
                "fingerprint" if value == "stale" => json!({
                    "url": format!("{}/elsewhere", self.base()),
                    "scopes_sorted": [],
                    "client_marker": "<dcr>",
                })
                .to_string(),
                "fingerprint" => fingerprint_json(server).unwrap(),
                _ => value
                    .to_string()
                    .replace("{redirect}", &self.redirect.to_string()),
            };
            backend
                .set(SERVICE, &format!("mcp-oauth:demo:{kind}"), &text)
                .unwrap();
        }
    }

    fn normalize(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(
                text.replace(&self.base(), "http://__BASE__")
                    .replace(&self.address, "__BASE__")
                    .replace(
                        &format!("127.0.0.1:{}", self.redirect),
                        "127.0.0.1:__REDIRECT__",
                    ),
            ),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.normalize(item)).collect())
            }
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(name, field)| {
                        let Value::String(name) = self.normalize(&json!(name)) else {
                            unreachable!("a string normalizes to a string")
                        };
                        (name, self.normalize(field))
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    fn raw_wire(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn wire(&self) -> Value {
        let entries = self
            .raw_wire()
            .into_iter()
            .map(|mut entry| {
                entry["query"] = mask_pairs(&entry["query"]);
                if entry["body"].is_array() {
                    entry["body"] = mask_pairs(&entry["body"]);
                } else if entry["body"]["params"]["clientInfo"]["version"].is_string() {
                    entry["body"]["params"]["clientInfo"]["version"] = json!("__VERSION__");
                }
                entry
            })
            .collect::<Vec<_>>();
        self.normalize(&Value::Array(entries))
    }

    fn keyring_state(&self, backend: Option<&MemoryKeyringBackend>) -> Value {
        let Some(backend) = backend else {
            return json!("headless");
        };
        let mut state = serde_json::Map::new();
        for ((service, account), text) in backend.entries() {
            let mut value = serde_json::from_str::<Value>(&text).unwrap_or(json!(text));
            if value["expires_at"].is_number() {
                value["expires_at"] = json!("__TIME__");
            }
            state.insert(format!("{service}|{account}"), self.normalize(&value));
        }
        Value::Object(state)
    }
}

impl Drop for Scenario {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn mask(name: &str, value: &str) -> String {
    match name {
        "state" => "__STATE__".to_owned(),
        "code_challenge" => "__CHALLENGE__".to_owned(),
        "code_verifier" => "__VERIFIER__".to_owned(),
        _ => value.to_owned(),
    }
}

fn mask_pairs(pairs: &Value) -> Value {
    Value::Array(
        pairs
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|pair| {
                let name = pair[0].as_str().unwrap_or_default();
                json!([name, mask(name, pair[1].as_str().unwrap_or_default())])
            })
            .collect(),
    )
}

fn digest(text: &str) -> Value {
    json!({"length": text.chars().count(), "sha256": hex(&Sha256::digest(text.as_bytes()))})
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A host without any credential store.
struct HeadlessKeyring;

impl KeyringBackend for HeadlessKeyring {
    fn get(&self, _: &str, _: &str) -> Result<Option<String>, KeyringFailure> {
        Err(KeyringFailure::NoBackend)
    }
    fn set(&self, _: &str, _: &str, _: &str) -> Result<(), KeyringFailure> {
        Err(KeyringFailure::NoBackend)
    }
    fn delete(&self, _: &str, _: &str) -> Result<(), KeyringFailure> {
        Err(KeyringFailure::NoBackend)
    }
}

// --------------------------------------------------------------------------
// The browser.
// --------------------------------------------------------------------------

#[derive(Clone)]
struct Browser {
    action: String,
    port: u16,
    visits: Arc<Mutex<Vec<Value>>>,
    threads: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
}

impl Browser {
    fn visit(&self, url: &str) {
        if matches!(self.action.as_str(), "none" | "busy") {
            return;
        }
        let this = self.clone();
        let url = url.to_owned();
        let handle = std::thread::spawn(move || this.request(&url));
        self.threads.lock().unwrap().push(handle);
    }

    fn request(&self, url: &str) {
        let parsed = Url::parse(url).unwrap();
        let state = parsed
            .query_pairs()
            .find(|(name, _)| name == "state")
            .map(|(_, value)| value.into_owned())
            .unwrap_or_default();
        let target = match self.action.as_str() {
            "code" => Some(format!("/callback?code=code-1&state={state}")),
            "no-code" => Some(format!("/callback?error=access_denied&state={state}")),
            "wrong-state" => Some("/callback?code=code-1&state=forged".to_owned()),
            _ => None,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut connection = loop {
            match TcpStream::connect(("127.0.0.1", self.port)) {
                Ok(connection) => break connection,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(_) => {
                    self.visits.lock().unwrap().push(json!({"status": null}));
                    return;
                }
            }
        };
        match target {
            Some(target) => connection
                .write_all(
                    format!(
                        "GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .unwrap(),
            None => connection.shutdown(std::net::Shutdown::Write).unwrap(),
        }
        let mut received = Vec::new();
        let _ = connection.read_to_end(&mut received);
        let text = String::from_utf8_lossy(&received).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|line| line.split(' ').nth(1))
            .and_then(|status| status.parse::<u16>().ok());
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            .collect::<BTreeMap<_, _>>();
        self.visits.lock().unwrap().push(json!({
            "status": status,
            "headers": {
                "content-type": headers.get("content-type"),
                "cache-control": headers.get("cache-control"),
                "connection": headers.get("connection"),
            },
            "page": digest(body),
        }));
    }

    fn settle(&self) -> Vec<Value> {
        let threads = std::mem::take(&mut *self.threads.lock().unwrap());
        for thread in threads {
            let _ = thread.join();
        }
        std::mem::take(&mut *self.visits.lock().unwrap())
    }
}

// --------------------------------------------------------------------------
// Running one scenario against this port.
// --------------------------------------------------------------------------

fn generation(revision: &str) -> Value {
    json!(revision.rsplit(':').next().unwrap().parse::<u64>().unwrap())
}

fn describe(result: &McpAuthorization) -> Value {
    match result {
        McpAuthorization::Snapshot(snapshot) => json!({"snapshot": {
            "headers": snapshot.headers,
            "connection": generation(&snapshot.connection_revision),
            "descriptor": generation(&snapshot.descriptor_revision),
        }}),
        McpAuthorization::Required(required) => json!({"required": {
            "reason": required.reason.as_str(),
            "descriptor": generation(&required.descriptor_revision),
            "observed": required.observed_connection_revision.as_deref().map(generation),
        }}),
    }
}

fn error_kind(error: &McpAuthenticationError) -> Value {
    json!({"error": match error {
        McpAuthenticationError::NotOauth(_) => "not_oauth",
        McpAuthenticationError::OAuth(error) => error.kind(),
    }})
}

fn authorization_url(url: &str) -> Value {
    let parsed = Url::parse(url).unwrap();
    json!({
        "endpoint": format!("{}://{}{}", parsed.scheme(), parsed.host_str().unwrap_or_default()
            .to_owned() + &parsed.port().map(|port| format!(":{port}")).unwrap_or_default(), parsed.path()),
        "parameters": parsed
            .query_pairs()
            .map(|(name, value)| json!([name, mask(&name, &value)]))
            .collect::<Vec<_>>(),
    })
}

fn pkce(urls: &[String], raw: &[Value]) -> Value {
    let challenges = urls.iter().map(|url| {
        Url::parse(url)
            .unwrap()
            .query_pairs()
            .find(|(name, _)| name == "code_challenge")
            .map(|(_, value)| value.into_owned())
    });
    let verifiers = raw.iter().filter_map(|entry| {
        let pairs = entry["body"].as_array()?;
        let field = |name: &str| {
            pairs
                .iter()
                .find(|pair| pair[0] == name)
                .and_then(|pair| pair[1].as_str())
                .map(str::to_owned)
        };
        (field("grant_type").as_deref() == Some("authorization_code"))
            .then(|| field("code_verifier"))?
    });
    Value::Array(
        challenges
            .zip(verifiers)
            .map(|(challenge, verifier)| {
                let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
                json!(if Some(computed) == challenge && verifier.len() == 128 {
                    "valid"
                } else {
                    "invalid"
                })
            })
            .collect(),
    )
}

async fn run_scenario(spec: &Value) -> Value {
    let scenario = Scenario::start(spec);
    let server = scenario.server(spec);
    let headless = spec["keyring"] == "headless";
    let memory = Arc::new(MemoryKeyringBackend::new());
    let backend: Arc<dyn KeyringBackend> = if headless {
        Arc::new(HeadlessKeyring)
    } else {
        memory.clone()
    };
    if !headless {
        scenario.seed(memory.as_ref(), spec, &server);
    }
    let service = McpAuthenticationService::new(Some(Arc::new(McpOAuthStore::new(backend, false))));
    service
        .bind_catalog(std::slice::from_ref(&server), None)
        .await;
    let browser = Browser {
        action: spec["browser"].as_str().unwrap().to_owned(),
        port: scenario.redirect,
        visits: Arc::default(),
        threads: Arc::default(),
    };
    let _busy = (spec["browser"] == "busy")
        .then(|| TcpListener::bind(("127.0.0.1", scenario.redirect)).unwrap());
    let urls = Arc::new(Mutex::new(Vec::<String>::new()));
    let all_urls = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink: AuthUrlSink = {
        let urls = urls.clone();
        let all_urls = all_urls.clone();
        let browser = browser.clone();
        Arc::new(move |url: String| {
            urls.lock().unwrap().push(url.clone());
            all_urls.lock().unwrap().push(url.clone());
            browser.visit(&url);
            Box::pin(async {})
        })
    };
    let mut steps = Vec::new();
    let mut last_snapshot = None;
    for step in spec["steps"].as_array().unwrap() {
        let action = step["do"].as_str().unwrap();
        let mut observed = serde_json::Map::new();
        observed.insert("do".to_owned(), json!(action));
        match action {
            "login" => {
                match tokio::time::timeout(STEP_TIMEOUT, service.login("demo", sink.clone(), &None))
                    .await
                {
                    Ok(Ok(revision)) => {
                        observed.insert("outcome".to_owned(), json!("ok"));
                        observed.insert("descriptor".to_owned(), generation(&revision));
                    }
                    Ok(Err(error)) => {
                        observed.insert("outcome".to_owned(), error_kind(&error));
                    }
                    Err(_) => {
                        observed.insert("outcome".to_owned(), json!({"error": "timeout"}));
                    }
                }
            }
            "logout" => match service.logout("demo", &None).await {
                Ok(revision) => {
                    observed.insert("outcome".to_owned(), json!("ok"));
                    observed.insert("descriptor".to_owned(), generation(&revision));
                }
                Err(error) => {
                    observed.insert("outcome".to_owned(), error_kind(&error));
                }
            },
            "resolve" => {
                let reference = service.reference_for(&server).await;
                match tokio::time::timeout(STEP_TIMEOUT, service.resolve(&reference)).await {
                    Ok(result) => {
                        if let McpAuthorization::Snapshot(snapshot) = &result {
                            last_snapshot = Some(snapshot.connection_revision.clone());
                        }
                        observed.insert("result".to_owned(), describe(&result));
                    }
                    Err(_) => {
                        observed.insert("outcome".to_owned(), json!({"error": "timeout"}));
                    }
                }
            }
            "reject" => {
                let reference = service.reference_for(&server).await;
                let observed_revision = last_snapshot.clone().unwrap_or_else(|| "none".to_owned());
                let result = service.reject(&reference, &observed_revision).await;
                observed.insert("result".to_owned(), describe(&result));
            }
            other => unreachable!("unknown step `{other}`"),
        }
        let published = std::mem::take(&mut *urls.lock().unwrap());
        observed.insert(
            "urls".to_owned(),
            Value::Array(published.iter().map(|url| authorization_url(url)).collect()),
        );
        observed.insert("browser".to_owned(), Value::Array(browser.settle()));
        observed.insert(
            "keyring".to_owned(),
            scenario.keyring_state((!headless).then_some(memory.as_ref())),
        );
        steps.push(scenario.normalize(&Value::Object(observed)));
    }
    let raw = scenario.raw_wire();
    json!({
        "steps": steps,
        "pkce": pkce(&all_urls.lock().unwrap(), &raw),
        "wire": scenario.wire(),
    })
}

// --------------------------------------------------------------------------
// Comparing.
// --------------------------------------------------------------------------

/// Every pointer at which two observations differ.
fn differences(expected: &Value, actual: &Value, pointer: &str, found: &mut Vec<String>) {
    match (expected, actual) {
        (Value::Object(left), Value::Object(right)) => {
            let keys = left
                .keys()
                .chain(right.keys())
                .collect::<std::collections::BTreeSet<_>>();
            for key in keys {
                differences(
                    left.get(key).unwrap_or(&Value::Null),
                    right.get(key).unwrap_or(&Value::Null),
                    &format!("{pointer}/{key}"),
                    found,
                );
            }
        }
        (Value::Array(left), Value::Array(right)) if left.len() == right.len() => {
            for (index, (left, right)) in left.iter().zip(right).enumerate() {
                differences(left, right, &format!("{pointer}/{index}"), found);
            }
        }
        _ if expected == actual => {}
        _ => found.push(pointer.to_owned()),
    }
}

fn is_page(pointer: &str) -> bool {
    pointer.contains("/browser/") && pointer.contains("/page/")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_scenario_matches_the_reference_capture() {
    let corpus: Value = serde_json::from_str(CORPUS).unwrap();
    let only = std::env::var("VIBE_OAUTH_PARITY_ONLY").ok();
    let mut failures = Vec::new();
    let mut used = vec![false; LEDGER.len()];
    let mut replayed = 0;
    for entry in corpus["scenarios"].as_array().unwrap() {
        let name = entry["name"].as_str().unwrap();
        if only.as_deref().is_some_and(|only| !name.starts_with(only)) {
            continue;
        }
        replayed += 1;
        let observed = run_scenario(&entry["spec"]).await;
        if std::env::var_os("VIBE_OAUTH_PARITY_DUMP").is_some() {
            eprintln!(
                "{name}\nexpected {}\nobserved {}",
                entry["observed"], observed
            );
        }
        let mut found = Vec::new();
        differences(&entry["observed"], &observed, "", &mut found);
        // The pages are the reference's own prose: they must differ, and only
        // they may differ by right.
        let pages = found
            .iter()
            .filter(|pointer| is_page(pointer) && pointer.ends_with("/sha256"))
            .count();
        let expected_pages = entry["observed"]["steps"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|step| step["browser"].as_array().unwrap().iter())
            .filter(|visit| visit.get("page").is_some())
            .count();
        if pages < expected_pages {
            failures.push(format!(
                "{name}: a callback page matches the reference's digest"
            ));
        }
        for pointer in found.iter().filter(|pointer| !is_page(pointer)) {
            match LEDGER.iter().position(|divergence| {
                (divergence.scenario == "*" || divergence.scenario == name)
                    && pointer_matches(divergence.pointer, pointer)
                    && (divergence.accepts)(
                        entry["observed"].pointer(pointer),
                        observed.pointer(pointer),
                    )
            }) {
                Some(index) => used[index] = true,
                None => failures.push(format!(
                    "{name} {pointer}: expected {} observed {}",
                    entry["observed"].pointer(pointer).unwrap_or(&Value::Null),
                    observed.pointer(pointer).unwrap_or(&Value::Null)
                )),
            }
        }
    }
    if only.is_none() {
        for (divergence, used) in LEDGER.iter().zip(used) {
            if !used {
                failures.push(format!(
                    "the ledger entry {} {} no longer reproduces ({})",
                    divergence.scenario, divergence.pointer, divergence.why
                ));
            }
        }
    }
    assert!(replayed > 0, "no scenario was replayed");
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn the_live_reference_still_answers_what_the_corpus_records() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "MCP OAuth") {
        eprintln!("{reason}");
        eprintln!("the committed corpus replayed regardless; restore with `{RESTORE_COMMAND}`");
        return;
    }
    let output = Command::new("python3")
        .arg(repository().join(CAPTURE_SCRIPT))
        .args(["--check", "--fixture", FIXTURE, "--reference"])
        .arg(&root)
        .current_dir(repository())
        .output()
        .expect("the capture script runs");
    assert!(
        output.status.success(),
        "the pinned reference no longer answers what the corpus records; regenerate it with \
         `{CAPTURE_SCRIPT} --corpus`: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
