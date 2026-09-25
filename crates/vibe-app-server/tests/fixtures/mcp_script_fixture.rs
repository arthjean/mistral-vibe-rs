//! A scripted MCP server both implementations are measured against.
//!
//! `scripts/parity/mcp_transport.py` drives the reference's MCP client against
//! this binary and `tests/mcp_transport_parity_tests.rs` drives this port's, so
//! the two clients meet the same server and differ only in what they send and
//! how they read the answers. The server answers each request from a script
//! and appends everything it received to a log, which is what both sides
//! compare.
//!
//! Usage: `vibe-mcp-script-fixture stdio` or `vibe-mcp-script-fixture http`.
//! The environment carries the rest, so the command line an MCP entry declares
//! stays the same across runs:
//!
//! - `VIBE_MCP_SCRIPT`: the script, a JSON object whose `methods` maps a method
//!   name (or `tools/call:<tool>`) to the reactions its successive requests get;
//!   the last reaction repeats once the list is spent.
//! - `VIBE_MCP_LOG`: the JSON-lines log every received message is appended to.
//! - `VIBE_MCP_STATE`: the counters, shared by every process of one scenario so
//!   a respawned server continues the script where the last one stopped.
//! - `VIBE_MCP_PORT_FILE`: HTTP only, where the bound address is written.
//!
//! A reaction is an object: `result` or `error` answers; `exit` ends a stdio
//! process (or drops an HTTP connection) without answering; `delayMs` waits
//! first; `notify` sends notifications before the answer; `serverRequest` asks
//! the client something and waits for its reply before answering; over HTTP,
//! `status` answers with that status and no JSON-RPC body, `headers` adds
//! response headers, and `sse` streams the answer as server-sent events.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Map, Value, json};

type Failure = Box<dyn std::error::Error + Send + Sync>;

/// Headers a transport adds on its own, which say nothing about the client.
const TRANSPORT_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "accept-encoding",
    "transfer-encoding",
];

fn main() -> Result<(), Failure> {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let fixture = Fixture::from_environment()?;
    match mode.as_str() {
        "stdio" => fixture.serve_stdio(),
        "http" => fixture.serve_http(),
        other => Err(format!("unknown mode `{other}`").into()),
    }
}

#[derive(Clone)]
struct Fixture {
    script: Arc<Value>,
    log: PathBuf,
    state: PathBuf,
    process: u64,
}

impl Fixture {
    fn from_environment() -> Result<Self, Failure> {
        let variable = |name: &str| -> Result<PathBuf, Failure> {
            std::env::var_os(name)
                .map(PathBuf::from)
                .ok_or_else(|| format!("{name} is not set").into())
        };
        let script: Value = serde_json::from_slice(&fs::read(variable("VIBE_MCP_SCRIPT")?)?)?;
        let log = variable("VIBE_MCP_LOG")?;
        let state = variable("VIBE_MCP_STATE")?;
        let mut fixture = Self {
            script: Arc::new(script),
            log,
            state,
            process: 0,
        };
        fixture.process = fixture.update_state(|state| {
            let spawns = state.get("spawns").and_then(Value::as_u64).unwrap_or(0) + 1;
            state.insert("spawns".to_owned(), json!(spawns));
            spawns
        })?;
        Ok(fixture)
    }

    /// Reads, changes and writes the shared counters under a lock file, so two
    /// processes of one scenario never lose each other's increments.
    fn update_state<T>(
        &self,
        change: impl FnOnce(&mut Map<String, Value>) -> T,
    ) -> Result<T, Failure> {
        let lock = self.state.with_extension("lock");
        let mut attempts = 0_u32;
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&lock) {
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists && attempts < 2_000 => {
                    attempts += 1;
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => return Err(error.into()),
            }
        }
        let result = (|| -> Result<T, Failure> {
            let mut state = match fs::read(&self.state) {
                Ok(bytes) if !bytes.is_empty() => {
                    serde_json::from_slice::<Map<String, Value>>(&bytes)?
                }
                _ => Map::new(),
            };
            let value = change(&mut state);
            fs::write(&self.state, serde_json::to_vec(&state)?)?;
            Ok(value)
        })();
        let _ = fs::remove_file(&lock);
        result
    }

    fn record(&self, mut entry: Map<String, Value>) -> Result<(), Failure> {
        entry.insert("process".to_owned(), json!(self.process));
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)?;
        let mut line = serde_json::to_vec(&Value::Object(entry))?;
        line.push(b'\n');
        file.write_all(&line)?;
        Ok(())
    }

    /// The reaction the next request of `method` gets, advancing its counter.
    fn reaction(&self, method: &str, request: &Value) -> Result<Value, Failure> {
        let methods = self.script.get("methods").and_then(Value::as_object);
        let tool_key = request
            .pointer("/params/name")
            .and_then(Value::as_str)
            .map(|tool| format!("{method}:{tool}"));
        let key = tool_key
            .filter(|key| methods.is_some_and(|methods| methods.contains_key(key)))
            .unwrap_or_else(|| method.to_owned());
        let Some(reactions) = methods
            .and_then(|methods| methods.get(&key))
            .and_then(Value::as_array)
            .filter(|reactions| !reactions.is_empty())
        else {
            return Ok(default_reaction(method));
        };
        let index = self.update_state(|state| {
            let counters = state
                .entry("counters")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .map(|counters| {
                    let next = counters.get(&key).and_then(Value::as_u64).unwrap_or(0);
                    counters.insert(key.clone(), json!(next + 1));
                    next
                });
            counters.unwrap_or(0)
        })?;
        let index = usize::try_from(index)
            .unwrap_or(usize::MAX)
            .min(reactions.len() - 1);
        Ok(reactions[index].clone())
    }

    fn answer(&self, request: &Value, reaction: &Value) -> Value {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        if let Some(error) = reaction.get("error") {
            return json!({"jsonrpc": "2.0", "id": id, "error": error});
        }
        let mut result = reaction.get("result").cloned().unwrap_or_else(|| json!({}));
        if request.get("method").and_then(Value::as_str) == Some("initialize")
            && result.get("protocolVersion").and_then(Value::as_str) == Some("$requested")
            && let Some(requested) = request.pointer("/params/protocolVersion").cloned()
            && let Some(object) = result.as_object_mut()
        {
            object.insert("protocolVersion".to_owned(), requested);
        }
        json!({"jsonrpc": "2.0", "id": id, "result": result})
    }

    // ----------------------------------------------------------------------
    // stdio
    // ----------------------------------------------------------------------

    fn serve_stdio(&self) -> Result<(), Failure> {
        // Which variables the client let through is part of how it launches a
        // server, so the names are logged; the values would leak the
        // environment of whoever ran the capture.
        let mut names = std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .collect::<Vec<_>>();
        names.sort();
        let cwd = std::env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        self.record(entry([
            ("kind", json!("spawn")),
            ("envNames", json!(names)),
            ("cwd", json!(cwd)),
        ]))?;
        let stdin = io::stdin();
        let mut lines = stdin.lock().lines();
        let mut stdout = io::stdout().lock();
        let mut next_server_request = 0_u64;
        while let Some(line) = lines.next() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let message: Value = serde_json::from_str(&line)?;
            self.record(entry([
                ("kind", json!("message")),
                ("body", message.clone()),
            ]))?;
            let (Some(method), Some(_)) = (
                message.get("method").and_then(Value::as_str),
                message.get("id"),
            ) else {
                continue;
            };
            let reaction = self.reaction(method, &message)?;
            if reaction.get("exit").and_then(Value::as_bool) == Some(true) {
                self.record(entry([("kind", json!("exit"))]))?;
                return Ok(());
            }
            delay(&reaction);
            for notification in notifications(&reaction) {
                write_line(&mut stdout, &notification)?;
            }
            if let Some(server_request) = reaction.get("serverRequest") {
                next_server_request += 1;
                let id = format!("server-{next_server_request}");
                let mut outbound = server_request.clone();
                if let Some(object) = outbound.as_object_mut() {
                    object.insert("jsonrpc".to_owned(), json!("2.0"));
                    object.insert("id".to_owned(), json!(id));
                }
                write_line(&mut stdout, &outbound)?;
                // The client's answer arrives on stdin like any other message.
                for reply in lines.by_ref() {
                    let reply: Value = serde_json::from_str(&reply?)?;
                    self.record(entry([("kind", json!("message")), ("body", reply.clone())]))?;
                    if reply.get("id").and_then(Value::as_str) == Some(id.as_str())
                        && reply.get("method").is_none()
                    {
                        break;
                    }
                }
            }
            write_line(&mut stdout, &self.answer(&message, &reaction))?;
        }
        self.record(entry([("kind", json!("eof"))]))?;
        Ok(())
    }

    // ----------------------------------------------------------------------
    // HTTP
    // ----------------------------------------------------------------------

    fn serve_http(&self) -> Result<(), Failure> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let port_file = std::env::var_os("VIBE_MCP_PORT_FILE")
            .map(PathBuf::from)
            .ok_or("VIBE_MCP_PORT_FILE is not set")?;
        let temporary = port_file.with_extension("tmp");
        fs::write(&temporary, address.port().to_string())?;
        fs::rename(&temporary, &port_file)?;
        let shared = Arc::new(Shared::default());
        for stream in listener.incoming() {
            let stream = stream?;
            let fixture = self.clone();
            let shared = shared.clone();
            thread::spawn(move || {
                let _ = fixture.serve_connection(stream, &shared);
            });
        }
        Ok(())
    }

    fn serve_connection(&self, stream: TcpStream, shared: &Shared) -> Result<(), Failure> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut writer = stream;
        loop {
            let Some(request) = read_request(&mut reader)? else {
                return Ok(());
            };
            let body: Value = if request.body.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&request.body)
                    .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&request.body)}))
            };
            self.record(entry([
                ("kind", json!("http")),
                ("method", json!(request.method)),
                ("path", json!(request.path)),
                ("headers", json!(request.headers)),
                ("body", body.clone()),
            ]))?;
            match request.method.as_str() {
                "GET" => {
                    // A standalone stream the client may open for server
                    // messages. It stays open and silent until the client
                    // hangs up, which keeps the count of these deterministic.
                    write_head(&mut writer, 200, "text/event-stream", &[], None)?;
                    let mut sink = [0_u8; 256];
                    while reader.read(&mut sink).is_ok_and(|read| read > 0) {}
                    return Ok(());
                }
                "DELETE" => {
                    write_head(&mut writer, 200, "application/json", &[], Some(b""))?;
                    continue;
                }
                "POST" => {}
                _ => {
                    write_head(&mut writer, 405, "text/plain", &[], Some(b""))?;
                    continue;
                }
            }
            let method = body
                .get("method")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let is_request = body.get("id").is_some() && method.is_some();
            if !is_request {
                // A notification, or a reply to a server request.
                if let Some(id) = body.get("id").and_then(Value::as_str)
                    && body.get("method").is_none()
                {
                    shared.deliver(id, body.clone());
                }
                let reaction = match &method {
                    Some(method) => self.reaction(method, &body)?,
                    None => json!({}),
                };
                if let Some(status) = reaction.get("status").and_then(Value::as_u64) {
                    write_status(&mut writer, status, &reaction)?;
                } else {
                    write_head(&mut writer, 202, "application/json", &[], Some(b""))?;
                }
                continue;
            }
            let method = method.unwrap_or_default();
            let reaction = self.reaction(&method, &body)?;
            if reaction.get("exit").and_then(Value::as_bool) == Some(true) {
                return Ok(());
            }
            delay(&reaction);
            if let Some(status) = reaction.get("status").and_then(Value::as_u64) {
                write_status(&mut writer, status, &reaction)?;
                continue;
            }
            let mut extra = response_headers(&reaction);
            if method == "initialize" {
                let session = self.update_state(|state| {
                    let next = state.get("sessions").and_then(Value::as_u64).unwrap_or(0) + 1;
                    state.insert("sessions".to_owned(), json!(next));
                    next
                })?;
                extra.push(("mcp-session-id".to_owned(), format!("session-{session}")));
            }
            let answer = self.answer(&body, &reaction);
            let streamed = reaction.get("sse").and_then(Value::as_bool) == Some(true)
                || reaction.get("notify").is_some()
                || reaction.get("serverRequest").is_some();
            if !streamed {
                let payload = serde_json::to_vec(&answer)?;
                write_head(&mut writer, 200, "application/json", &extra, Some(&payload))?;
                continue;
            }
            write_head(&mut writer, 200, "text/event-stream", &extra, None)?;
            let mut chunks = Vec::new();
            for notification in notifications(&reaction) {
                chunks.push(notification);
            }
            for chunk in chunks {
                write_event(&mut writer, &chunk)?;
            }
            if let Some(server_request) = reaction.get("serverRequest") {
                let id = shared.next_request_id();
                let mut outbound = server_request.clone();
                if let Some(object) = outbound.as_object_mut() {
                    object.insert("jsonrpc".to_owned(), json!("2.0"));
                    object.insert("id".to_owned(), json!(id));
                }
                let reply = shared.expect(&id);
                write_event(&mut writer, &outbound)?;
                let _ = reply.recv_timeout(Duration::from_secs(30));
            }
            write_event(&mut writer, &answer)?;
            write_chunk(&mut writer, b"")?;
        }
    }
}

/// What the HTTP connections share: the replies server requests wait on.
#[derive(Default)]
struct Shared {
    waiting: Mutex<BTreeMap<String, Sender<Value>>>,
    requests: Mutex<u64>,
}

impl Shared {
    fn next_request_id(&self) -> String {
        let mut requests = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *requests += 1;
        format!("server-{requests}")
    }

    fn expect(&self, id: &str) -> Receiver<Value> {
        let (sender, receiver) = channel();
        self.waiting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_owned(), sender);
        receiver
    }

    fn deliver(&self, id: &str, reply: Value) {
        if let Some(sender) = self
            .waiting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
        {
            let _ = sender.send(reply);
        }
    }
}

struct Request {
    method: String,
    path: String,
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
    let path = parts.next().unwrap_or_default().to_owned();
    let mut headers = BTreeMap::new();
    let mut length = 0_usize;
    let mut chunked = false;
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
        if name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked") {
            chunked = true;
        }
        if !TRANSPORT_HEADERS.contains(&name.as_str()) {
            headers.insert(name, value);
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size)?;
            let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
            if size == 0 {
                let mut trailer = String::new();
                reader.read_line(&mut trailer)?;
                break;
            }
            let mut chunk = vec![0_u8; size];
            reader.read_exact(&mut chunk)?;
            body.extend_from_slice(&chunk);
            let mut crlf = [0_u8; 2];
            reader.read_exact(&mut crlf)?;
        }
    } else {
        body.resize(length, 0);
        reader.read_exact(&mut body)?;
    }
    Ok(Some(Request {
        method,
        path,
        headers,
        body,
    }))
}

fn reason(status: u64) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

/// Writes a response head, and the body when there is one. A missing body
/// means the answer streams: chunked, until [`write_chunk`] ends it.
fn write_head(
    writer: &mut TcpStream,
    status: u64,
    content_type: &str,
    extra: &[(String, String)],
    body: Option<&[u8]>,
) -> Result<(), Failure> {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: {content_type}\r\n",
        reason(status)
    );
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    match body {
        Some(body) => head.push_str(&format!("content-length: {}\r\n\r\n", body.len())),
        None => head.push_str("cache-control: no-cache\r\ntransfer-encoding: chunked\r\n\r\n"),
    }
    writer.write_all(head.as_bytes())?;
    if let Some(body) = body {
        writer.write_all(body)?;
    }
    writer.flush()?;
    Ok(())
}

fn write_status(writer: &mut TcpStream, status: u64, reaction: &Value) -> Result<(), Failure> {
    let body = reaction
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .as_bytes()
        .to_vec();
    write_head(
        writer,
        status,
        "text/plain",
        &response_headers(reaction),
        Some(&body),
    )
}

fn write_chunk(writer: &mut TcpStream, bytes: &[u8]) -> Result<(), Failure> {
    writer.write_all(format!("{:x}\r\n", bytes.len()).as_bytes())?;
    writer.write_all(bytes)?;
    writer.write_all(b"\r\n")?;
    writer.flush()?;
    Ok(())
}

fn write_event(writer: &mut TcpStream, message: &Value) -> Result<(), Failure> {
    let event = format!(
        "event: message\ndata: {}\n\n",
        serde_json::to_string(message)?
    );
    write_chunk(writer, event.as_bytes())
}

fn write_line(stdout: &mut impl Write, message: &Value) -> Result<(), Failure> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    stdout.write_all(&line)?;
    stdout.flush()?;
    Ok(())
}

fn response_headers(reaction: &Value) -> Vec<(String, String)> {
    reaction
        .get("headers")
        .and_then(Value::as_object)
        .map(|headers| {
            headers
                .iter()
                .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

fn notifications(reaction: &Value) -> Vec<Value> {
    reaction
        .get("notify")
        .and_then(Value::as_array)
        .map(|notifications| {
            notifications
                .iter()
                .map(|notification| {
                    let mut notification = notification.clone();
                    if let Some(object) = notification.as_object_mut() {
                        object.insert("jsonrpc".to_owned(), json!("2.0"));
                    }
                    notification
                })
                .collect()
        })
        .unwrap_or_default()
}

fn delay(reaction: &Value) {
    if let Some(milliseconds) = reaction.get("delayMs").and_then(Value::as_u64) {
        thread::sleep(Duration::from_millis(milliseconds));
    }
}

fn default_reaction(method: &str) -> Value {
    match method {
        "initialize" => json!({"result": {
            "protocolVersion": "$requested",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "script", "version": "1.0.0"}
        }}),
        "tools/list" => json!({"result": {"tools": []}}),
        "ping" => json!({"result": {}}),
        _ => json!({"error": {"code": -32601, "message": "Method not found"}}),
    }
}

fn entry<const N: usize>(fields: [(&str, Value); N]) -> Map<String, Value> {
    fields
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}
