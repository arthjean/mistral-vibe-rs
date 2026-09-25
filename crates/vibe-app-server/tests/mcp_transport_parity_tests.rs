//! Replays the committed MCP transport corpus against this port's MCP client.
//!
//! `scripts/parity/mcp_transport.py` captured the corpus from the pinned
//! reference's MCP client in the composition its app server builds for a
//! session: an authentication service bound to the configured servers, the
//! references the catalog resolves from it, and a registry with a descriptor
//! cache. Every server either side meets is `vibe-mcp-script-fixture`, which
//! logs every message it receives, so a scenario compares what each client sent
//! (`wire`), what it published (`published`), what the model reads for each call
//! (`calls`), what it required of the host (`authRequired`), what it asked the
//! model to complete (`sampling`), what it cached on disk (`cache`) and the
//! identity each server's fingerprint hashes (`fingerprints`).
//!
//! The replay drives this port's `vibe_core::mcp` in the same composition:
//! `McpAuthenticationService`, `McpRegistry` configured with the references and
//! a `McpDescriptorCache`, and the tools it publishes, called through the tool
//! registry. It needs no reference checkout; only the live probe at the end,
//! which recaptures the reference, does.
//!
//! A failure sentence is authored prose on both sides: the corpus holds the
//! reference's as a length and a SHA-256, and this port writes its own
//! (`NOTICE`), so the replay compares that a call or a discovery failed and
//! requires the two sentences to differ. Every other difference has to fall
//! under a `LEDGER` entry, and every entry has to still reproduce.

#![cfg(feature = "test-fixtures")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use url::Url;
use vibe_core::auth::{McpOAuthStore, MemoryKeyringBackend};
use vibe_core::mcp::authorization::{python_json, server_fingerprint, server_identity};
use vibe_core::mcp::{
    DefaultMcpPeerFactory, McpAuthConfig, McpAuthenticationService, McpAuthorizationRequired,
    McpDeclared, McpDeclaredCommand, McpDescriptorCache, McpFuture, McpOAuthConfig, McpRegistry,
    McpServerConfig, McpStaticAuth, McpTransportConfig, SamplingHandler, SamplingRequest,
    SamplingResponse, SamplingRole,
};
use vibe_core::parity::{RESTORE_COMMAND, off_pin_reason, reference_root};
use vibe_core::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PermissionStore,
};
use vibe_core::tools::{ToolInvocation, ToolRegistry};

/// The corpus, compiled in so a moved file fails the build rather than the run.
const CORPUS: &str = include_str!("mcp-transport/corpus.json");
const FIXTURE: &str = env!("CARGO_BIN_EXE_vibe-mcp-script-fixture");
const CAPTURE_SCRIPT: &str = "scripts/parity/mcp_transport.py";

/// A difference the replay accepts, and why.
struct Divergence {
    /// Scenarios it applies to: an exact name or a `*/suffix` over both
    /// transports.
    scenario: &'static str,
    /// The JSON pointer, inside the scenario's observation, where the two
    /// sides differ.
    pointer: &'static str,
    why: &'static str,
}

const LEDGER: &[Divergence] = &[Divergence {
    scenario: "*/spaced-name",
    pointer: "/steps/0/published/0/name",
    why: "a remote tool name a provider would refuse is sanitized into the published \
              name; the reference publishes it as the server spelled it",
}];

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels under the workspace")
        .to_path_buf()
}

fn hexadecimal(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn digest(text: &str) -> Value {
    json!({
        "length": text.chars().count(),
        "sha256": hexadecimal(&Sha256::digest(text.as_bytes())),
    })
}

// --------------------------------------------------------------------------
// The host pieces the reference capture supplies.
// --------------------------------------------------------------------------

struct Approve;

impl ApprovalAgent for Approve {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::ApproveOnce) })
    }
}

/// The capture's `SamplingBackend`: a fixed answer, every request recorded.
#[derive(Default)]
struct RecordingSampler {
    calls: Mutex<Vec<Value>>,
}

impl SamplingHandler for RecordingSampler {
    fn complete<'a>(&'a self, request: SamplingRequest) -> McpFuture<'a, SamplingResponse> {
        Box::pin(async move {
            let messages = request
                .messages
                .iter()
                .map(|message| {
                    let role = match message.role {
                        SamplingRole::System => "system",
                        SamplingRole::User => "user",
                        SamplingRole::Assistant => "assistant",
                    };
                    json!({"role": role, "content": message.content})
                })
                .collect::<Vec<_>>();
            self.calls
                .lock()
                .unwrap()
                .push(json!({"messages": messages, "maxTokens": request.max_tokens}));
            Ok(SamplingResponse {
                text: "sampled answer".to_owned(),
                model: "oracle-model".to_owned(),
            })
        })
    }
}

// --------------------------------------------------------------------------
// One scenario.
// --------------------------------------------------------------------------

struct ServerFiles {
    script: PathBuf,
    log: PathBuf,
    state: PathBuf,
    port: PathBuf,
}

struct Scenario {
    spec: Value,
    root: tempfile::TempDir,
    files: BTreeMap<String, ServerFiles>,
    ports: BTreeMap<String, String>,
    processes: Vec<Child>,
}

impl Drop for Scenario {
    fn drop(&mut self) {
        for process in &mut self.processes {
            let _ = process.kill();
            let _ = process.wait();
        }
    }
}

impl Scenario {
    fn new(spec: &Value) -> Self {
        let root = tempfile::Builder::new()
            .prefix("mcp-transport-")
            .tempdir()
            .expect("scenario root");
        let mut files = BTreeMap::new();
        for server in spec["servers"].as_array().expect("servers") {
            let name = server["name"].as_str().expect("server name").to_owned();
            let directory = root.path().join(&name);
            std::fs::create_dir_all(&directory).expect("server directory");
            let script = directory.join("script.json");
            std::fs::write(&script, spec["scripts"][&name].to_string()).expect("script");
            files.insert(
                name,
                ServerFiles {
                    script,
                    log: directory.join("log.jsonl"),
                    state: directory.join("state.json"),
                    port: directory.join("port"),
                },
            );
        }
        Self {
            spec: spec.clone(),
            root,
            files,
            ports: BTreeMap::new(),
            processes: Vec::new(),
        }
    }

    fn environment(&self, name: &str) -> BTreeMap<String, String> {
        let files = &self.files[name];
        BTreeMap::from([
            (
                "VIBE_MCP_SCRIPT".to_owned(),
                files.script.display().to_string(),
            ),
            ("VIBE_MCP_LOG".to_owned(), files.log.display().to_string()),
            (
                "VIBE_MCP_STATE".to_owned(),
                files.state.display().to_string(),
            ),
        ])
    }

    fn start_http(&mut self, name: &str) -> String {
        let port_file = self.files[name].port.clone();
        let child = Command::new(FIXTURE)
            .arg("http")
            .envs(self.environment(name))
            .env("VIBE_MCP_PORT_FILE", &port_file)
            .stdin(Stdio::null())
            .spawn()
            .expect("the HTTP fixture starts");
        self.processes.push(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !port_file.exists() {
            assert!(
                Instant::now() < deadline,
                "the HTTP fixture never bound a port"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let port = std::fs::read_to_string(&port_file)
            .expect("port file")
            .trim()
            .to_owned();
        self.ports.insert(name.to_owned(), port.clone());
        port
    }

    /// The capture's `server_models`, as this port's configuration.
    fn configs(&mut self) -> Vec<McpServerConfig> {
        let servers = self.spec["servers"].as_array().expect("servers").clone();
        servers
            .iter()
            .map(|server| {
                let name = server["name"].as_str().expect("name").to_owned();
                let seconds = |key: &str, default: u64| {
                    server
                        .get(key)
                        .and_then(Value::as_f64)
                        .map_or(default, |seconds| (seconds * 1_000.0).round() as u64)
                };
                let (transport, auth, declared) = if server["transport"] == "stdio" {
                    let args = server
                        .get("args")
                        .and_then(Value::as_array)
                        .map(|args| {
                            args.iter()
                                .map(|arg| arg.as_str().expect("arg").to_owned())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let mut environment = server
                        .get("env")
                        .and_then(Value::as_object)
                        .map(|env| {
                            env.iter()
                                .map(|(key, value)| {
                                    (key.clone(), value.as_str().expect("env").to_owned())
                                })
                                .collect::<BTreeMap<_, _>>()
                        })
                        .unwrap_or_default();
                    environment.extend(self.environment(&name));
                    let command = if server.get("command_form") == Some(&json!("text")) {
                        McpDeclaredCommand::Text(format!("{FIXTURE} stdio"))
                    } else {
                        McpDeclaredCommand::Argv(vec![FIXTURE.to_owned(), "stdio".to_owned()])
                    };
                    let mut arguments = vec!["stdio".to_owned()];
                    arguments.extend(args.iter().cloned());
                    (
                        McpTransportConfig::Stdio {
                            command: FIXTURE.to_owned(),
                            arguments,
                            environment,
                            working_directory: None,
                        },
                        McpAuthConfig::default(),
                        McpDeclared {
                            command: Some(command),
                            args,
                            cwd: None,
                            url: None,
                        },
                    )
                } else {
                    let port = self.start_http(&name);
                    let url = format!("http://127.0.0.1:{port}/mcp");
                    let auth = server.get("auth").cloned().unwrap_or(Value::Null);
                    let headers = auth
                        .get("headers")
                        .and_then(Value::as_object)
                        .map(|headers| {
                            headers
                                .iter()
                                .map(|(key, value)| {
                                    (key.clone(), value.as_str().expect("header").to_owned())
                                })
                                .collect::<BTreeMap<_, _>>()
                        })
                        .unwrap_or_default();
                    let auth = if auth["type"] == "oauth" {
                        McpAuthConfig::Oauth(McpOAuthConfig {
                            scopes: auth["scopes"]
                                .as_array()
                                .map(|scopes| {
                                    scopes
                                        .iter()
                                        .map(|scope| scope.as_str().expect("scope").to_owned())
                                        .collect()
                                })
                                .unwrap_or_default(),
                            ..McpOAuthConfig::default()
                        })
                    } else {
                        let mut statics = McpStaticAuth::default();
                        if let Some(variable) = auth.get("api_key_env").and_then(Value::as_str) {
                            variable.clone_into(&mut statics.api_key_env);
                        }
                        if let Some(header) = auth.get("api_key_header").and_then(Value::as_str) {
                            header.clone_into(&mut statics.api_key_header);
                        }
                        if let Some(format) = auth.get("api_key_format").and_then(Value::as_str) {
                            format.clone_into(&mut statics.api_key_format);
                        }
                        McpAuthConfig::Static(statics)
                    };
                    let parsed = Url::parse(&url).expect("fixture URL");
                    let transport = if server["transport"] == "http" {
                        McpTransportConfig::Http {
                            url: parsed,
                            headers,
                        }
                    } else {
                        McpTransportConfig::StreamableHttp {
                            url: parsed,
                            headers,
                        }
                    };
                    (
                        transport,
                        auth,
                        McpDeclared {
                            url: Some(url),
                            ..McpDeclared::default()
                        },
                    )
                };
                McpServerConfig {
                    alias: name,
                    transport,
                    enabled: server.get("disabled") != Some(&json!(true)),
                    disabled_tools: BTreeSet::new(),
                    startup_timeout_ms: seconds("startup_timeout_sec", 10_000),
                    tool_timeout_ms: seconds("tool_timeout_sec", 60_000),
                    auth,
                    prompt: server
                        .get("prompt")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    sampling_enabled: server.get("sampling_enabled") != Some(&json!(false)),
                    declared: Some(declared),
                }
            })
            .collect()
    }

    fn normalize(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => {
                let mut text = text.replace(FIXTURE, "__FIXTURE__");
                for port in self.ports.values() {
                    text = text.replace(&format!("127.0.0.1:{port}"), "127.0.0.1:__PORT__");
                }
                text = text.replace(&self.root.path().display().to_string(), "__SCENARIO__");
                text = text.replace(
                    &format!("MistralAI-VibeCLI/{}", vibe_core::telemetry::version()),
                    "MistralAI-VibeCLI/__VERSION__",
                );
                Value::String(text)
            }
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.normalize(item)).collect())
            }
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| {
                        let key = match self.normalize(&Value::String(key.clone())) {
                            Value::String(key) => key,
                            _ => key.clone(),
                        };
                        (key, self.normalize(value))
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    fn wire(&self, name: &str, inherited: &BTreeSet<String>) -> Value {
        let Ok(log) = std::fs::read_to_string(&self.files[name].log) else {
            return json!([]);
        };
        let working_directory = std::env::current_dir()
            .expect("working directory")
            .display()
            .to_string();
        Value::Array(
            log.lines()
                .map(|line| {
                    let mut entry: Value = serde_json::from_str(line).expect("log entry");
                    if entry["kind"] == "spawn" {
                        let names = entry["envNames"]
                            .as_array()
                            .expect("envNames")
                            .iter()
                            .filter_map(Value::as_str)
                            .filter(|name| !inherited.contains(*name))
                            .map(str::to_owned)
                            .collect::<BTreeSet<_>>();
                        entry["envNames"] = json!(names);
                        if entry["cwd"].as_str() == Some(working_directory.as_str()) {
                            entry["cwd"] = json!("__CWD__");
                        }
                    }
                    self.normalize(&entry)
                })
                .collect(),
        )
    }

    /// The capture's `settle`: every stdio server spawned has logged its end.
    async fn settle(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let settled = self.files.values().all(|files| {
                let Ok(log) = std::fs::read_to_string(&files.log) else {
                    return true;
                };
                let mut spawned = BTreeSet::new();
                let mut ended = BTreeSet::new();
                for line in log.lines() {
                    let entry: Value = serde_json::from_str(line).expect("log entry");
                    match entry["kind"].as_str() {
                        Some("spawn") => {
                            spawned.insert(entry["process"].clone().to_string());
                        }
                        Some("eof" | "exit") => {
                            ended.insert(entry["process"].clone().to_string());
                        }
                        _ => {}
                    }
                }
                spawned.is_subset(&ended)
            });
            if settled {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// One process's worth of MCP state, the capture's `Session`.
struct Session {
    registry: McpRegistry,
    tools: ToolRegistry,
    factory: Arc<DefaultMcpPeerFactory>,
    required: Arc<Mutex<Vec<Value>>>,
    sampler: Arc<RecordingSampler>,
}

impl Session {
    async fn start(configs: &[McpServerConfig], root: &Path, token: &(String, String)) -> Self {
        let (variable, value) = token.clone();
        let service = Arc::new(
            // The capture's host held no credential for any OAuth scenario.
            McpAuthenticationService::new(Some(Arc::new(McpOAuthStore::new(
                Arc::new(MemoryKeyringBackend::new()),
                false,
            ))))
            .with_environment(Arc::new(move |name| {
                if name == variable {
                    Some(value.clone())
                } else {
                    std::env::var(name).ok()
                }
            })),
        );
        service.bind_catalog(configs, None).await;
        let mut references = BTreeMap::new();
        for config in configs {
            references.insert(config.alias.clone(), service.reference_for(config).await);
        }
        let required = Arc::new(Mutex::new(Vec::new()));
        let sink_required = required.clone();
        let registry = McpRegistry::default();
        registry
            .configure_descriptor_cache(Some(Arc::new(McpDescriptorCache::new(
                root.join("descriptors"),
                86_400.0,
            ))))
            .await;
        registry
            .configure_authorization(
                service,
                references,
                Some(Arc::new(
                    move |name: &str, requirement: &McpAuthorizationRequired| {
                        sink_required.lock().unwrap().push(json!({
                            "name": name,
                            "reason": requirement.reason.as_str(),
                            "descriptorRevision": requirement.descriptor_revision,
                            "observedConnectionRevision": requirement.observed_connection_revision,
                        }));
                    },
                )),
            )
            .await;
        let sampler = Arc::new(RecordingSampler::default());
        Self {
            registry,
            tools: ToolRegistry::default(),
            factory: Arc::new(DefaultMcpPeerFactory::with_sampling(sampler.clone())),
            required,
            sampler,
        }
    }

    async fn discover(&self, scenario: &Scenario, configs: &[McpServerConfig]) -> Value {
        self.registry
            .discover_all(
                configs.to_vec(),
                self.factory.clone(),
                &self.tools,
                PermissionStore::default(),
                Arc::new(Approve),
            )
            .await;
        let specs = self
            .tools
            .list()
            .expect("tool list")
            .into_iter()
            .map(|spec| (spec.name.clone(), spec))
            .collect::<BTreeMap<_, _>>();
        let published = self
            .registry
            .published()
            .await
            .into_iter()
            .map(|name| {
                let spec = &specs[&name];
                json!({
                    "name": name,
                    "description": digest(
                        scenario
                            .normalize(&Value::String(spec.description.clone()))
                            .as_str()
                            .expect("description"),
                    ),
                    "parameters": spec.input_schema,
                })
            })
            .collect::<Vec<_>>();
        let mut revisions = Map::new();
        for config in configs {
            revisions.insert(
                config.alias.clone(),
                json!(self.registry.descriptor_revision(&config.alias).await),
            );
        }
        let failed = self
            .registry
            .pop_failed()
            .await
            .into_iter()
            .map(|(name, message)| (name, digest(&message)))
            .collect::<Map<_, _>>();
        let status = self
            .registry
            .auth_status()
            .await
            .into_iter()
            .map(|(name, status)| (name, json!(status.as_str())))
            .collect::<Map<_, _>>();
        json!({
            "published": published,
            "state": {
                "status": status,
                "needsAuth": self.registry.needs_auth().await,
                "descriptorRevisions": revisions,
                "failed": failed,
            },
        })
    }

    async fn call(&self, tool: &str, arguments: &Value) -> Value {
        if !self
            .registry
            .published()
            .await
            .iter()
            .any(|name| name == tool)
        {
            return json!({"unpublished": true});
        }
        match self
            .tools
            .invoke(
                tool,
                ToolInvocation {
                    call_id: "oracle-call".to_owned(),
                    arguments: arguments.clone(),
                },
            )
            .await
        {
            Ok(output) => json!({"text": output.model_text}),
            Err(error) => json!({"error": {"kind": "tool", "message": digest(&error.to_string())}}),
        }
    }

    async fn close(&self) {
        self.registry.close().await;
    }
}

fn age_cache(root: &Path, seconds: f64) {
    let Ok(entries) = std::fs::read_dir(root.join("descriptors")) else {
        return;
    };
    let moved = vibe_core::auth::sign_in::UtcTimestamp::now().plus_seconds(-seconds);
    let stamp = moved.to_iso8601().replace("+00:00", "Z");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let mut record: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("record")).expect("record JSON");
        record["discoveredAt"] = json!(stamp);
        record["lastUsedAt"] = json!(stamp);
        std::fs::write(&path, record.to_string()).expect("aged record");
    }
}

fn replace_fingerprints(value: &Value, known: &BTreeMap<String, String>) -> Value {
    match value {
        Value::String(text) => {
            let mut text = text.clone();
            for (fingerprint, name) in known {
                text = text.replace(fingerprint, &format!("__FINGERPRINT__{name}"));
                text = text.replace(&fingerprint[..16], &format!("__FINGERPRINT__{name}"));
            }
            Value::String(text)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| replace_fingerprints(item, known))
                .collect(),
        ),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), replace_fingerprints(value, known)))
                .collect(),
        ),
        other => other.clone(),
    }
}

async fn run_scenario(
    spec: &Value,
    token: &(String, String),
    inherited: &BTreeSet<String>,
) -> Value {
    let mut scenario = Scenario::new(spec);
    let configs = scenario.configs();
    let fingerprints = configs
        .iter()
        .map(|config| (server_fingerprint(config), config.alias.clone()))
        .collect::<BTreeMap<_, _>>();
    let root = scenario.root.path().to_path_buf();
    let mut sessions = vec![Session::start(&configs, &root, token).await];
    let mut steps = Vec::new();
    for step in spec["steps"].as_array().expect("steps") {
        let session = sessions.last().expect("a session");
        match step["do"].as_str().expect("step") {
            "discover" => {
                let mut observed = session.discover(&scenario, &configs).await;
                observed["do"] = json!("discover");
                steps.push(observed);
            }
            "call" => {
                let tool = step["tool"].as_str().expect("tool");
                let mut observed = session.call(tool, &step["arguments"]).await;
                observed["do"] = json!("call");
                observed["tool"] = json!(tool);
                steps.push(observed);
            }
            "restart" => {
                session.close().await;
                sessions.push(Session::start(&configs, &root, token).await);
                steps.push(json!({"do": "restart"}));
            }
            "ageCache" => {
                age_cache(&root, step["seconds"].as_f64().expect("seconds"));
                steps.push(json!({"do": "ageCache"}));
            }
            other => unreachable!("unknown step {other}"),
        }
    }
    sessions.last().expect("a session").close().await;
    scenario.settle().await;
    let mut cache = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root.join("descriptors")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let mut record: Value =
                serde_json::from_slice(&std::fs::read(&path).expect("record")).expect("JSON");
            let key = record["key"].as_str().expect("key").to_owned();
            assert_eq!(
                path.file_name().and_then(|name| name.to_str()),
                Some(format!("{}.json", hexadecimal(&Sha256::digest(key.as_bytes()))).as_str()),
                "a descriptor cache file is not named by its key"
            );
            let mut parsed: Value = serde_json::from_str(&key).expect("key JSON");
            if let Some(name) = parsed["serverFingerprint"]
                .as_str()
                .and_then(|fingerprint| fingerprints.get(fingerprint))
            {
                parsed["serverFingerprint"] = json!(name);
            }
            record["key"] = parsed;
            record["discoveredAt"] = json!("__TIME__");
            record["lastUsedAt"] = json!("__TIME__");
            cache.push(record);
        }
    }
    cache.sort_by_key(|record| {
        (
            record["sourceName"].as_str().unwrap_or_default().to_owned(),
            python_json(&record["key"]),
        )
    });
    let required = sessions
        .iter()
        .flat_map(|session| session.required.lock().unwrap().clone())
        .collect::<Vec<_>>();
    let sampling = sessions
        .iter()
        .flat_map(|session| session.sampler.calls.lock().unwrap().clone())
        .collect::<Vec<_>>();
    let wire = scenario
        .files
        .keys()
        .map(|name| (name.clone(), scenario.wire(name, inherited)))
        .collect::<Map<_, _>>();
    let identities = configs
        .iter()
        .map(|config| {
            (
                config.alias.clone(),
                json!({"canonical": python_json(&server_identity(config))}),
            )
        })
        .collect::<Map<_, _>>();
    let observed = json!({
        "steps": steps,
        "authRequired": required,
        "sampling": sampling,
        "wire": wire,
        "cache": cache,
        "fingerprints": identities,
    });
    replace_fingerprints(&scenario.normalize(&observed), &fingerprints)
}

// --------------------------------------------------------------------------
// Comparison.
// --------------------------------------------------------------------------

/// Moves every failure sentence out of an observation, leaving that a failure
/// happened, and returns the sentences' digests by pointer.
fn extract_prose(observed: &mut Value) -> BTreeMap<String, Value> {
    let mut prose = BTreeMap::new();
    if let Some(steps) = observed["steps"].as_array_mut() {
        for (index, step) in steps.iter_mut().enumerate() {
            if let Some(message) = step.pointer_mut("/error/message") {
                prose.insert(format!("/steps/{index}/error/message"), message.take());
                *message = json!("<prose>");
            }
            if let Some(failed) = step
                .pointer_mut("/state/failed")
                .and_then(Value::as_object_mut)
            {
                for (name, message) in failed.iter_mut() {
                    prose.insert(
                        format!("/steps/{index}/state/failed/{name}"),
                        message.take(),
                    );
                    *message = json!("<prose>");
                }
            }
        }
    }
    prose
}

fn differences(expected: &Value, actual: &Value, pointer: &str, found: &mut Vec<String>) {
    match (expected, actual) {
        (Value::Object(left), Value::Object(right)) => {
            for key in left.keys().chain(right.keys()).collect::<BTreeSet<_>>() {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                let next = format!("{pointer}/{escaped}");
                match (left.get(key), right.get(key)) {
                    (Some(left), Some(right)) => differences(left, right, &next, found),
                    _ => found.push(next),
                }
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

fn ledgered(scenario: &str, pointer: &str) -> Option<&'static Divergence> {
    LEDGER.iter().find(|entry| {
        let applies = entry
            .scenario
            .strip_prefix('*')
            .map_or(entry.scenario == scenario, |suffix| {
                scenario.ends_with(suffix)
            });
        applies && entry.pointer == pointer
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_scenario_matches_the_reference_mcp_client() {
    let corpus: Value = serde_json::from_str(CORPUS).expect("corpus JSON");
    let token = (
        corpus["tokenVariable"]
            .as_str()
            .expect("token variable")
            .to_owned(),
        corpus["token"].as_str().expect("token").to_owned(),
    );
    let inherited = corpus["inheritedNames"]
        .as_array()
        .expect("inherited names")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let only = std::env::var("VIBE_MCP_PARITY_ONLY").ok();
    let mut failures = Vec::new();
    let mut used = BTreeSet::new();
    let mut conformant = 0_usize;
    let mut total = 0_usize;
    for scenario in corpus["scenarios"].as_array().expect("scenarios") {
        let name = scenario["name"].as_str().expect("scenario name");
        if only.as_deref().is_some_and(|only| !name.starts_with(only)) {
            continue;
        }
        total += 1;
        let mut expected = scenario["observed"].clone();
        let mut actual = run_scenario(&scenario["spec"], &token, &inherited).await;
        let reference_prose = extract_prose(&mut expected);
        let port_prose = extract_prose(&mut actual);
        let mut found = Vec::new();
        differences(&expected, &actual, "", &mut found);
        for (pointer, reference) in &reference_prose {
            if port_prose.get(pointer) == Some(reference) {
                found.push(format!(
                    "{pointer} (a failure sentence matches the reference digest)"
                ));
            }
        }
        let mut unexplained = Vec::new();
        for pointer in found {
            match ledgered(name, &pointer) {
                Some(entry) => {
                    used.insert((entry.scenario, entry.pointer));
                }
                None => unexplained.push(pointer),
            }
        }
        if unexplained.is_empty() {
            conformant += 1;
        } else {
            let detail = unexplained
                .iter()
                .map(|pointer| {
                    format!(
                        "    {pointer}\n      reference: {}\n      this port: {}",
                        expected
                            .pointer(pointer)
                            .map_or("<absent>".to_owned(), |value| {
                                value.to_string().chars().take(600).collect()
                            }),
                        actual
                            .pointer(pointer)
                            .map_or("<absent>".to_owned(), |value| {
                                value.to_string().chars().take(600).collect()
                            }),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            if std::env::var_os("VIBE_MCP_PARITY_DUMP").is_some() {
                eprintln!("{name} reference: {expected}\n{name} this port: {actual}");
            }
            failures.push(format!("  {name}:\n{detail}"));
        }
    }
    println!(
        "MCP transport replay: {conformant}/{total} scenarios conformant, {} ledgered divergences",
        used.len()
    );
    assert!(
        failures.is_empty(),
        "{} scenario(s) differ from the reference:\n{}",
        failures.len(),
        failures.join("\n")
    );
    if only.is_none() {
        let stale = LEDGER
            .iter()
            .filter(|entry| !used.contains(&(entry.scenario, entry.pointer)))
            .map(|entry| format!("  {} {}: {}", entry.scenario, entry.pointer, entry.why))
            .collect::<Vec<_>>();
        assert!(
            stale.is_empty(),
            "ledger entries no longer reproduce:\n{}",
            stale.join("\n")
        );
    }
}

#[test]
fn the_live_reference_still_answers_what_the_corpus_records() {
    let root = reference_root();
    if let Some(reason) = off_pin_reason(&root, "MCP transport") {
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
