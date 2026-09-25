//! A scripted HTTP server playing both an MCP resource and its OAuth
//! authorization server.
//!
//! `scripts/parity/mcp_oauth.py` drives the reference's MCP OAuth client
//! against this binary and `tests/mcp_oauth_parity_tests.rs` drives this
//! port's, so both clients meet the same server. Every request is answered
//! from a script and appended to a log, which is what both sides compare.
//!
//! The environment carries the inputs:
//!
//! - `VIBE_OAUTH_SCRIPT`: a JSON object whose `routes` maps `METHOD /path`
//!   (the path without its query) to the reactions its successive requests
//!   get; the last reaction repeats once the list is spent, and a route the
//!   script does not name answers `404`.
//! - `VIBE_OAUTH_LOG`: the JSON-lines log every request is appended to.
//! - `VIBE_OAUTH_PORT_FILE`: where the bound address is written.
//!
//! A reaction is an object with a `status`, optional `headers`, and a body:
//! `json` (sent as `application/json`) or `text` (sent as `text/plain`).
//! Every string of a reaction has `{base}` replaced by `http://<address>`.
//!
//! The script may also carry an `mcp` object, which makes one path a working
//! MCP endpoint: `path` names it, `tools` is what `tools/list` answers, and
//! `token`, when present, is the bearer credential a request must carry. A
//! request without it gets the route's scripted reactions, or a `401` whose
//! challenge points at the protected-resource metadata; one with it is
//! answered by JSON-RPC method, which is what `scripts/parity/mcp_catalog.py`
//! needs to take a server from `needs_auth` to connected.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Map, Value, json};

type Failure = Box<dyn std::error::Error + Send + Sync>;

/// Headers a client's HTTP stack adds on its own, which say nothing about the
/// flow.
const STACK_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "accept-encoding",
    "transfer-encoding",
];

struct Fixture {
    script: Value,
    log: PathBuf,
    base: String,
    counters: Mutex<BTreeMap<String, usize>>,
    journal: Mutex<()>,
}

fn main() -> Result<(), Failure> {
    let variable = |name: &str| -> Result<PathBuf, Failure> {
        std::env::var_os(name)
            .map(PathBuf::from)
            .ok_or_else(|| format!("{name} is not set").into())
    };
    let script: Value = serde_json::from_slice(&fs::read(variable("VIBE_OAUTH_SCRIPT")?)?)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let fixture = Arc::new(Fixture {
        script,
        log: variable("VIBE_OAUTH_LOG")?,
        base: format!("http://{address}"),
        counters: Mutex::new(BTreeMap::new()),
        journal: Mutex::new(()),
    });
    let port_file = variable("VIBE_OAUTH_PORT_FILE")?;
    let pending = port_file.with_extension("pending");
    fs::write(&pending, address.to_string())?;
    fs::rename(&pending, &port_file)?;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let fixture = fixture.clone();
        thread::spawn(move || {
            let _ = fixture.serve(stream);
        });
    }
    Ok(())
}

impl Fixture {
    fn serve(&self, stream: TcpStream) -> Result<(), Failure> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut writer = stream;
        while let Some(request) = read_request(&mut reader)? {
            let (path, query) = request
                .target
                .split_once('?')
                .map_or((request.target.as_str(), ""), |(path, query)| (path, query));
            self.record(&request, path, query)?;
            let route = format!("{} {path}", request.method);
            let reaction = self
                .mcp(&request, path)
                .unwrap_or_else(|| self.reaction(&route));
            respond(&mut writer, &reaction)?;
        }
        Ok(())
    }

    fn reaction(&self, route: &str) -> Value {
        let reactions = self
            .script
            .get("routes")
            .and_then(|routes| routes.get(route))
            .and_then(Value::as_array);
        let Some(reactions) = reactions.filter(|reactions| !reactions.is_empty()) else {
            return json!({"status": 404, "text": "not found"});
        };
        let index = {
            let mut counters = self
                .counters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let counter = counters.entry(route.to_owned()).or_insert(0);
            let index = (*counter).min(reactions.len() - 1);
            *counter += 1;
            index
        };
        self.expand(&reactions[index])
    }

    /// The answer of the scripted MCP endpoint, or `None` when the request is
    /// not for it or lacks the credential and the routes decide.
    fn mcp(&self, request: &Request, path: &str) -> Option<Value> {
        let mcp = self.script.get("mcp")?;
        if mcp.get("path").and_then(Value::as_str) != Some(path) {
            return None;
        }
        if let Some(token) = mcp.get("token").and_then(Value::as_str) {
            let presented = request
                .headers
                .get("authorization")
                .and_then(|value| value.split_once(' '))
                .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
                .map(|(_, credential)| credential.to_owned());
            if presented.as_deref() != Some(token) {
                let route = format!("{} {path}", request.method);
                let scripted = self
                    .script
                    .get("routes")
                    .and_then(|routes| routes.get(&route))
                    .is_some();
                return (!scripted).then(|| {
                    self.expand(&json!({
                        "status": 401,
                        "headers": {"www-authenticate": format!(
                            "Bearer resource_metadata=\"{{base}}/.well-known/oauth-protected-resource{path}\""
                        )},
                        "text": "unauthorized",
                    }))
                });
            }
        }
        if request.method != "POST" {
            return Some(json!({"status": 405, "text": "method not allowed"}));
        }
        let Ok(message) = serde_json::from_slice::<Value>(&request.body) else {
            return Some(json!({"status": 400, "text": "not JSON"}));
        };
        let Some(id) = message.get("id").cloned() else {
            return Some(json!({"status": 202, "text": ""}));
        };
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let answer = match method {
            "initialize" => json!({"result": {
                "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-11-25")),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fixture", "version": "0"},
            }}),
            "tools/list" => {
                json!({"result": {"tools": mcp.get("tools").cloned().unwrap_or(json!([]))}})
            }
            "tools/call" => json!({"result": {"content": [{
                "type": "text",
                "text": format!("called {}", params.get("name").and_then(Value::as_str).unwrap_or("")),
            }]}}),
            "ping" => json!({"result": {}}),
            _ => json!({"error": {"code": -32601, "message": "method not found"}}),
        };
        let mut envelope = json!({"jsonrpc": "2.0", "id": id});
        if let (Some(envelope), Some(answer)) = (envelope.as_object_mut(), answer.as_object()) {
            envelope.extend(answer.clone());
        }
        Some(json!({"status": 200, "json": envelope}))
    }

    fn expand(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(text.replace("{base}", &self.base)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.expand(item)).collect())
            }
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(name, field)| (name.clone(), self.expand(field)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    fn record(&self, request: &Request, path: &str, query: &str) -> Result<(), Failure> {
        let content_type = request
            .headers
            .get("content-type")
            .map(String::as_str)
            .unwrap_or_default();
        let body = if request.body.is_empty() {
            Value::Null
        } else if content_type.starts_with("application/json") {
            serde_json::from_slice(&request.body)
                .unwrap_or_else(|_| json!(String::from_utf8_lossy(&request.body)))
        } else if content_type.starts_with("application/x-www-form-urlencoded") {
            json!(pairs(&String::from_utf8_lossy(&request.body)))
        } else {
            json!(String::from_utf8_lossy(&request.body))
        };
        let mut entry = Map::new();
        entry.insert("method".to_owned(), json!(request.method));
        entry.insert("path".to_owned(), json!(path));
        entry.insert("query".to_owned(), json!(pairs(query)));
        entry.insert("headers".to_owned(), json!(request.headers));
        entry.insert("body".to_owned(), body);
        let _guard = self
            .journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)?;
        writeln!(log, "{}", Value::Object(entry))?;
        Ok(())
    }
}

/// The pairs of a query or a form body, decoded, in order.
fn pairs(encoded: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(encoded.as_bytes())
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect()
}

struct Request {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn read_request(reader: &mut BufReader<TcpStream>) -> Result<Option<Request>, Failure> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let mut headers = BTreeMap::new();
    let mut length = 0_usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            return Ok(None);
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_owned();
        if name == "content-length" {
            length = value.parse().unwrap_or(0);
        }
        if !STACK_HEADERS.contains(&name.as_str()) {
            headers.insert(name, value);
        }
    }
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body)?;
    Ok(Some(Request {
        method,
        target,
        headers,
        body,
    }))
}

fn respond(writer: &mut TcpStream, reaction: &Value) -> Result<(), Failure> {
    let status = reaction
        .get("status")
        .and_then(Value::as_u64)
        .unwrap_or(200);
    let (content_type, body) = match (reaction.get("json"), reaction.get("text")) {
        (Some(value), _) => ("application/json", value.to_string()),
        (None, Some(Value::String(text))) => ("text/plain", text.clone()),
        _ => ("text/plain", String::new()),
    };
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n",
        reason(status),
        body.len()
    );
    if let Some(headers) = reaction.get("headers").and_then(Value::as_object) {
        for (name, value) in headers {
            if let Some(value) = value.as_str() {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
        }
    }
    head.push_str("\r\n");
    writer.write_all(head.as_bytes())?;
    writer.write_all(body.as_bytes())?;
    writer.flush()?;
    Ok(())
}

fn reason(status: u64) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}
