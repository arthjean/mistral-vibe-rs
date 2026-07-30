use std::fs;
use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args();
    let _program = arguments.next();
    if arguments.next().as_deref() == Some("--linger") {
        let marker = arguments.next().ok_or("missing descendant marker")?;
        std::thread::sleep(std::time::Duration::from_millis(1_500));
        fs::write(marker, b"leaked")?;
        return Ok(());
    }
    if let Ok(marker) = std::env::var("VIBE_MCP_DESCENDANT_FILE") {
        let executable = std::env::current_exe()?;
        std::process::Command::new(executable)
            .arg("--linger")
            .arg(marker)
            .spawn()?;
    }
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let request: Value = serde_json::from_str(&line?)?;
        let Some(method) = request.get("method").and_then(Value::as_str) else {
            continue;
        };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "vibe-test-fixture", "version": "1.0.0"}
                }
            }),
            "tools/list" => {
                if request.pointer("/params/cursor").and_then(Value::as_str) == Some("page-2") {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [{
                                "name": "status",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {},
                                    "additionalProperties": false
                                }
                            }]
                        }
                    })
                } else {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [{
                                "name": "echo",
                                "description": "Echo a bounded message",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {"message": {"type": "string"}},
                                    "required": ["message"],
                                    "additionalProperties": false
                                },
                                "outputSchema": {
                                    "type": "object",
                                    "properties": {"echo": {"type": "string"}},
                                    "required": ["echo"],
                                    "additionalProperties": false
                                },
                                "annotations": {"readOnlyHint": true}
                            }],
                            "nextCursor": "page-2"
                        }
                    })
                }
            }
            "tools/call" => {
                let message = request
                    .pointer("/params/arguments/message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{"type": "text", "text": format!("hello {message}")}],
                        "structuredContent": {"echo": message},
                        "isError": false
                    }
                })
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": "Method not found"}
            }),
        };
        serde_json::to_writer(&mut stdout, &response)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    if let Ok(path) = std::env::var("VIBE_MCP_EXIT_FILE") {
        fs::write(path, b"closed")?;
    }
    Ok(())
}
