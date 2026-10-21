"""The scenarios ``unified_harness.py`` runs against an app server, the
reference's or this port's.

A scenario names the flags its server starts with, the configuration, files
and tables its home holds, what the chat-completions stand-in answers on the
model and utility queues, how the client answers callbacks, and the steps it
sends. Every value here is authored for the corpus.
"""

from __future__ import annotations

import json
from typing import Any

import loop_capture

call = loop_capture.call
send = loop_capture.send
START = loop_capture.START
PERSISTED = loop_capture.PERSISTED


def config_read() -> dict[str, Any]:
    return send("config/read", pick=["startupIssue", "harnessSelectionSource"])


def runtime_read() -> dict[str, Any]:
    return send("runtime/read", sessionId="$S1")


#: A one-pixel PNG, inline and as the bytes `WRITE_PNG` writes to `shot.png`.
PNG = ("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAA"
       "AABJRU5ErkJggg==")
WRITE_PNG = f"import base64; open('shot.png', 'wb').write(base64.b64decode('{PNG}'))"
#: The default Mistral model, blind, beside a local one.
BLIND_MODELS = (
    '\n[[models]]\nname = "mistral-vibe-cli-latest"\nprovider = "mistral"\n'
    'alias = "mistral-medium-3.5"\ntemperature = 1.0\nthinking = "high"\n'
    'supports_images = false\n'
)
#: The default model, compacting once its context passes 20 tokens.
AUTO_COMPACT_MODEL = (
    '\n[[models]]\nname = "mistral-vibe-cli-latest"\nprovider = "mistral"\n'
    'alias = "mistral-medium-3.5"\ntemperature = 1.0\nthinking = "high"\n'
    'auto_compact_threshold = 50000\n'
)
#: A model of the same provider that reads images.
VISION_MODEL = (
    '\n[[models]]\nname = "pixtral-large-latest"\nprovider = "mistral"\n'
    'alias = "pixtral"\nsupports_images = true\n'
)


def inline_image() -> dict[str, Any]:
    return {"type": "image", "attachment": {"source": {"kind": "inline", "data": PNG},
                                            "alias": "pasted.png", "mimeType": "image/png"}}


def file_image(path: str) -> dict[str, Any]:
    return {"type": "image", "attachment": {"source": {"kind": "file", "path": path},
                                            "alias": "shot.png", "mimeType": "image/png"}}


#: Two turns of file writes: the first writes `a.txt`, the second rewrites it
#: and creates `b.txt`, so a rewind to either has files to restore.
REWIND_WRITES = [
    {"toolCalls": [call("write_file", {"path": "a.txt", "content": "one\n"}, "call_w1")]},
    {"text": "Wrote one."},
    {"toolCalls": [call("write_file", {"path": "a.txt", "content": "two\n"}, "call_w2")]},
    {"toolCalls": [call("write_file", {"path": "b.txt", "content": "new\n"}, "call_w3")]},
    {"text": "Wrote two."},
    {"text": "Again."},
]
#: The two turns `REWIND_WRITES` answers, each naming its own message, which
#: is the identifier the harness core rewinds to.
REWIND_TURNS = [
    {"turn": "Write one", "params": {"clientUserMessageId": "msg-one"}},
    {"turn": "Write two", "params": {"clientUserMessageId": "msg-two"}},
]
#: Lets a session run under the smart-approve profile.
SMART_APPROVE = "smart_approve_available = true\n"


def verdict(safe: bool, reason: str, risk: str, authorization: str) -> dict[str, Any]:
    """A classifier answer, as the smart-approve gate reads one."""
    return {"text": json.dumps({"safe": safe, "reason": reason, "risk_level": risk,
                                "user_authorization": authorization})}


def user_entry(text: str) -> dict[str, Any]:
    """A queued user entry holding `text`."""
    return {"role": "user", "content": [{"type": "text", "text": text}]}


def session_start(agent: str = "auto-approve") -> dict[str, Any]:
    """A `session/start` whose answer, the state the session opened with, is
    recorded."""

    return send("session/start", agentConfig={"cwd": "$WS", "agent": agent})


ECHO = [{"toolCalls": [call("bash", {"command": "echo hi"}, "call_b1")]}, {"text": "Finished."}]
#: A command no allowlist covers, so a session that asks before running one
#: raises an approval for it.
TOUCH = [{"toolCalls": [call("bash", {"command": "touch made.txt"}, "call_b2")]},
         {"text": "Finished."}]
#: The same command under another call: the reference derives an action's
#: identifier from its call's, so a later turn reusing a call identifier is a
#: redelivery it refuses, which no model produces.
TOUCH_AGAIN = [{"toolCalls": [call("bash", {"command": "touch made.txt"}, "call_b3")]},
               {"text": "Finished."}]
#: One question in the shape the unified catalog declares: its keys are the
#: camel-cased ones `ui.ask_user_question` takes.
QUESTION = {"questions": [{
    "question": "Which color?",
    "header": "Color",
    "options": [{"label": "Red", "description": "Warm"},
                {"label": "Blue", "description": "Cold"}],
}]}


#: A program that drives every process operation once. Process IDs and
#: output paths name the session, so the program reports neither.
#: A stdio MCP server small enough to read at a glance: three tools, one
#: whose name needs normalizing and one that reports an error.
MCP_SERVER = """\
import json
import sys

OBJECT = {"type": "object", "properties": {}, "additionalProperties": False}
TOOLS = [
    {"name": "echo", "description": "Send a message back.",
     "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}},
                     "required": ["message"], "additionalProperties": False}},
    {"name": "create-issue", "description": "Open an issue with a title.",
     "inputSchema": {"type": "object", "properties": {"title": {"type": "string"}},
                     "required": ["title"], "additionalProperties": False}},
    {"name": "fail", "description": "Report an error every time.", "inputSchema": OBJECT},
]


def answer(name, arguments):
    if name == "echo":
        text = arguments.get("message", "")
        return {"content": [{"type": "text", "text": "echo " + text}],
                "structuredContent": {"echo": text}}
    if name == "create-issue":
        return {"content": [{"type": "text", "text": "opened " + arguments.get("title", "")}]}
    return {"content": [{"type": "text", "text": "it failed"}], "isError": True}


for line in sys.stdin:
    message = json.loads(line)
    if "id" not in message:
        continue
    method = message.get("method")
    if method == "initialize":
        result = {"protocolVersion": message["params"]["protocolVersion"],
                  "capabilities": {"tools": {}},
                  "serverInfo": {"name": "oracle-mcp", "version": "1.0.0"}}
    elif method == "tools/list":
        result = {"tools": TOOLS}
    elif method == "tools/call":
        params = message["params"]
        result = answer(params["name"], params.get("arguments") or {})
    elif method == "ping":
        result = {}
    else:
        reply = {"jsonrpc": "2.0", "id": message["id"],
                 "error": {"code": -32601, "message": "unknown method"}}
        print(json.dumps(reply), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}), flush=True)
"""

#: The server above, under a name the group normalizes, with a hint.
MCP_TABLES = """
[[mcp_servers]]
name = "data-api"
transport = "stdio"
command = "python3"
args = ["$WS/mcp_server.py"]
prompt = "Reach the issue tracker through these tools."
"""

#: A program that calls each tool of the server, catching the failure.
MCP_PROGRAM = (
    "async function main() {\n"
    "  const echoed = await tools.mcp_data_api.echo({message: 'hi'});\n"
    "  const opened = await tools.mcp_data_api.create_issue({title: 'Bug'});\n"
    "  let failure = null;\n"
    "  try { await tools.mcp_data_api.fail({}); } catch (error) { failure = error; }\n"
    "  return {echoed, opened, failure};\n}\n"
)

#: A project plugin that declares the same server, launched from its own
#: checkout.
MCP_PLUGIN = {
    ".vibe/plugins/tracker/plugin.json": json.dumps({
        "$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
        "name": "tracker", "version": "1.0.0"}) + "\n",
    ".vibe/plugins/tracker/mcp.json": json.dumps({
        "$schema": "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
        "mcpServers": {"issues": {"type": "stdio", "command": "./server.py"}}}) + "\n",
    ".vibe/plugins/tracker/server.py": "#!/usr/bin/env python3\n" + MCP_SERVER,
}

#: Discovery runs once the session is open; a turn waits for it.
MCP_START = [START, {"sleep": 2.0}]

PROCESS_PROGRAM = """
const strip = (value) => {
  if (!value || typeof value !== "object") return value;
  const { processId, outputPath, ...rest } = value;
  if (outputPath !== undefined) rest.hasOutputPath = outputPath !== null;
  return rest;
};
const attempt = async (run) => {
  try { return strip(await run()); } catch (error) {
    const { processId, ...details } = (error.cause && error.cause.details) || {};
    return { name: error.name, message: error.message, details };
  }
};
async function main() {
  const a = await tools.process.start({ command: "printf 'one\\ntwo\\n'" });
  await tools.self.sleep({ seconds: 0.8 });
  const out = await tools.process.output({ processId: a.processId });
  const tail = await tools.process.output({ processId: a.processId, from: "end", maxBytes: 4 });
  const page = await tools.process.output({ processId: a.processId, cursor: 2, maxBytes: 3 });
  const b = await tools.process.start({ command: "sleep 30", env: { MARK: "x" } });
  const listed = await tools.process.list({});
  const stopped = await tools.process.stop({ processId: b.processId });
  const again = await attempt(() => tools.process.stop({ processId: b.processId }));
  const written = await attempt(() => tools.process.write({ processId: b.processId, text: "x" }));
  const missing = await attempt(() => tools.process.output({ processId: "process-missing" }));
  const badCwd = await attempt(() => tools.process.start({ command: "true", cwd: "/nonexistent/dir" }));
  const failed = await tools.process.start({ command: "exit 3" });
  await tools.self.sleep({ seconds: 0.8 });
  const failedOut = await tools.process.output({ processId: failed.processId });
  return {
    a: strip(a), out: strip(out), tail: strip(tail), page: strip(page), b: strip(b),
    listed: listed.processes.map(strip), stopped: strip(stopped), again, written, missing,
    badCwd, failedOut: strip(failedOut),
  };
}
"""

#: A program that talks to a process through its terminal.
INTERACTIVE_PROGRAM = """
async function main() {
  const cat = await tools.process.start({ command: "cat" });
  const sent = await tools.process.write({ processId: cat.processId, text: "hello\\n" });
  await tools.self.sleep({ seconds: 0.5 });
  const echoed = await tools.process.output({ processId: cat.processId, waitMs: 2000 });
  const control = await tools.process.write({ processId: cat.processId, control: ["ctrl_d"] });
  await tools.self.sleep({ seconds: 0.5 });
  const done = await tools.process.output({ processId: cat.processId });
  const bytes = await tools.process.start({ command: "head -c 3" });
  const raw = await tools.process.write({ processId: bytes.processId, bytesBase64: "YWJj" });
  await tools.self.sleep({ seconds: 0.5 });
  const rawOut = await tools.process.output({ processId: bytes.processId });
  const drop = ({ processId, ...rest }) => rest;
  return { sent: drop(sent), echoed: drop(echoed), control: drop(control), done: drop(done),
           raw: drop(raw), rawOut: drop(rawOut) };
}
"""


#: A program that drives every subagent operation once, waiting on each
#: turn so the scripted answers reach the child in order.
SUBAGENT_PROGRAM = """
const attempt = async (run) => {
  try { return await run(); } catch (error) { return { name: error.name, message: error.message }; }
};
async function main() {
  const spawned = await tools.subagent.spawn({ agentName: "helper", message: "Do the first part" });
  const first = await tools.subagent.wait({ agentName: "helper", timeoutMs: 20000 });
  const listed = await tools.subagent.list({});
  const again = await tools.subagent.spawn({ agentName: "helper", message: "Again" });
  const sent = await tools.subagent.sendMessage({ agentName: "helper", message: "Do the second part" });
  const second = await tools.subagent.wait({ agentName: "helper", timeoutMs: 20000 });
  const interrupted = await tools.subagent.interrupt({ agentName: "helper" });
  const closed = await tools.subagent.close({ agentName: "helper" });
  const after = await tools.subagent.list({});
  const missing = await tools.subagent.wait({ agentName: "nobody", timeoutMs: 10 });
  const badType = await attempt(() => tools.subagent.spawn({ agentName: "other", message: "x", agentType: "nope" }));
  return { spawned, first, listed, again, sent, second, interrupted, closed, after, missing, badType };
}
"""


#: A child interrupted inside a long command, then waited on and closed.
SUBAGENT_INTERRUPT_PROGRAM = """
async function main() {
  const spawned = await tools.subagent.spawn({ agentName: "worker", message: "Work slowly" });
  await tools.self.sleep({ seconds: 1 });
  const listed = await tools.subagent.list({});
  const interrupted = await tools.subagent.interrupt({ agentName: "worker" });
  const waited = await tools.subagent.wait({ agentName: "worker", timeoutMs: 20000 });
  const sent = await tools.subagent.sendMessage({ agentName: "worker", message: "Resume" });
  const resumed = await tools.subagent.wait({ agentName: "worker", timeoutMs: 20000 });
  const closed = await tools.subagent.close({ agentName: "worker" });
  const again = await tools.subagent.close({ agentName: "worker" });
  const message = await tools.subagent.sendMessage({ agentName: "worker", message: "x" });
  const late = await tools.subagent.wait({ agentName: "worker", timeoutMs: 10 });
  return { spawned, listed, interrupted, waited, sent, resumed, closed, again, message, late };
}
"""

#: A child whose command asks the user, in a session that asks.
SUBAGENT_APPROVAL_PROGRAM = """
async function main() {
  await tools.subagent.spawn({ agentName: "asker", message: "Touch the file" });
  return await tools.subagent.wait({ agentName: "asker", timeoutMs: 20000 });
}
"""

SUBAGENT_KEEP_PROGRAM = """
async function main() {
  await tools.subagent.spawn({ agentName: "keeper", message: "Remember this" });
  return await tools.subagent.wait({ agentName: "keeper", timeoutMs: 20000 });
}
"""

#: A subagent type a user wrote, with its own instructions and no shell.
REVIEWER = {"write": {"$ROOT/vibe-home/agents/reviewer.toml": (
    'agent_type = "subagent"\n'
    'description = "Reviews changes without running commands"\n'
    'instructions = "You review. Never run commands."\n'
    'disabled_tools = ["bash"]\n')}}

SUBAGENT_TYPED_PROGRAM = """
const attempt = async (run) => {
  try { return await run(); } catch (error) { return { name: error.name, message: error.message }; }
};
async function main() {
  const unknown = await attempt(() => tools.subagent.spawn({ agentName: "ghost", agentType: "nobody", message: "Hi" }));
  await tools.subagent.spawn({ agentName: "critic", agentType: "reviewer", message: "Review the change" });
  const answer = await tools.subagent.wait({ agentName: "critic", timeoutMs: 20000 });
  const listed = await tools.subagent.list({});
  return { unknown, answer, listed };
}
"""

#: A session an older Vibe saved, which the unified host lists, previews and
#: imports on resume.
LEGACY_ID = "5eed0000-0000-4000-8000-00000000abcd"
LEGACY_DIRECTORY = "$ROOT/vibe-home/logs/session/session_20260102_030405_5eed0000"
LEGACY = {"write": {
    f"{LEGACY_DIRECTORY}/meta.json": json.dumps({
        "session_id": LEGACY_ID, "parent_session_id": None,
        "start_time": "2026-01-02T03:04:05.000000+00:00",
        "end_time": "2026-01-02T03:05:06.000000+00:00",
        "git_commit": None, "git_branch": None,
        "environment": {"working_directory": "$WS"}, "origin_directory": "$WS",
        "username": "oracle", "child_sessions": [], "loops": [],
        "title": "Seeded session", "title_source": "manual",
        "bumped_at": "2026-01-02T03:04:05.500000+00:00", "pinned_at": None,
        "experiments": None, "config": {"active_model": "mistral-medium-3.5"},
        "import_provenance": None, "created_worktree": None, "stats": {},
        "total_messages": 4, "last_message_fingerprint": None, "tools_available": [],
        "agent_profile": {"name": "default", "overrides": {}}, "system_prompt": None,
    }, indent=2),
    f"{LEGACY_DIRECTORY}/messages.jsonl": "".join(json.dumps(message) + "\n" for message in [
        {"role": "user", "content": "Seeded question", "message_id": "5eed0000-0000-4000-8000-000000000001"},
        {"role": "assistant", "content": "Seeded answer", "message_id": "5eed0000-0000-4000-8000-000000000002"},
        {"role": "user", "content": "Seeded follow-up", "message_id": "5eed0000-0000-4000-8000-000000000003"},
        {"role": "assistant", "content": "Seeded closing", "message_id": "5eed0000-0000-4000-8000-000000000004"},
    ]),
}}


def scenarios() -> list[dict[str, Any]]:
    return [
        # -- which backend a launch selects ----------------------------------
        {
            "name": "selection/default",
            "steps": [config_read(), session_start(), runtime_read()],
        },
        {
            "name": "selection/experimental-flag",
            "flags": ["--experimental-harness"],
            "steps": [config_read(), session_start(), runtime_read()],
        },
        {
            "name": "selection/legacy-flag",
            "flags": ["--legacy-harness"],
            "steps": [config_read(), session_start(), runtime_read()],
        },
        # -- what a turn sends the model and raises --------------------------
        {
            "name": "turn/text",
            "backend": [{"text": "Hello there."}],
            "steps": [START, {"turn": "Say hello"}, PERSISTED],
        },
        {
            "name": "turn/reasoning",
            "backend": [{"reasoning": "Thinking it over.", "text": "Answer."}],
            "steps": [START, {"turn": "Think first"}],
        },
        {
            "name": "turn/two-turns",
            "backend": [{"text": "First."}, {"text": "Second."}],
            "steps": [START, {"turn": "One"}, {"turn": "Two"}, PERSISTED],
        },
        {
            "name": "tools/bash",
            "backend": ECHO,
            "steps": [START, {"turn": "Run echo"}, PERSISTED],
        },
        {
            "name": "tools/files",
            "files": {"notes.txt": "alpha\nbeta\n"},
            "backend": [
                {"toolCalls": [call("read_file", {"path": "notes.txt"}, "call_r1")]},
                {"toolCalls": [call("write_file", {"path": "made.txt", "content": "new\n"},
                                    "call_w1")]},
                {"toolCalls": [call("edit", {"file_path": "notes.txt", "old_string": "beta",
                                             "new_string": "gamma"}, "call_e1")]},
                {"text": "Edited."},
            ],
            "steps": [START, {"turn": "Work on the files"}, {"file": "$WS/notes.txt"},
                      {"file": "$WS/made.txt"}],
        },
        {
            "name": "tools/parallel",
            "files": {"a.txt": "one\n", "b.txt": "two\n"},
            "backend": [
                {"toolCalls": [call("read_file", {"path": "a.txt"}, "call_p1"),
                               call("read_file", {"path": "b.txt"}, "call_p2")]},
                {"text": "Both read."},
            ],
            "steps": [START, {"turn": "Read both"}],
        },
        {
            "name": "tools/failures",
            "backend": [
                {"toolCalls": [call("read_file", {"path": "missing.txt"}, "call_f1")]},
                {"toolCalls": [call("no_such_tool", {}, "call_f2")]},
                {"toolCalls": [{"id": "call_f3", "name": "bash", "raw": "{not json"}]},
                {"toolCalls": [call("edit", {"path": "x.txt"}, "call_f4")]},
                {"text": "Gave up."},
            ],
            "steps": [START, {"turn": "Try things"}],
        },
        {
            "name": "tools/todo",
            "backend": [
                {"toolCalls": [call("todo", {"action": "write", "todos": [
                    {"id": "1", "content": "First task", "status": "in_progress"},
                    {"id": "2", "content": "Second task", "priority": "high"},
                ]}, "call_t1")]},
                {"toolCalls": [call("todo", {"action": "read"}, "call_t2")]},
                {"text": "Planned."},
            ],
            "steps": [START, {"turn": "Plan the work"}],
        },
        {
            "name": "tools/scratchpad",
            "backend": [
                {"toolCalls": [call("unified_harness_scratchpad",
                                    {"action": "write", "path": "notes.md", "content": "kept\n"},
                                    "call_s1")]},
                {"toolCalls": [call("unified_harness_scratchpad", {"action": "list"}, "call_s2")]},
                {"toolCalls": [call("unified_harness_scratchpad",
                                    {"action": "read", "path": "notes.md"}, "call_s3")]},
                {"text": "Noted."},
            ],
            "steps": [START, {"turn": "Take notes"}],
        },
        # -- approvals and questions ----------------------------------------
        {
            "name": "approval/approve",
            "backend": TOUCH,
            "callbacks": [{"approve": True}],
            "steps": [session_start("ask"), {"turn": "Run echo"}, PERSISTED],
        },
        {
            "name": "approval/deny",
            "backend": TOUCH,
            "callbacks": [{"approve": False}],
            "steps": [session_start("ask"), {"turn": "Run echo"}, PERSISTED],
        },
        {
            "name": "approval/for-session",
            "backend": [*TOUCH, *TOUCH_AGAIN],
            "callbacks": [{"output": {"type": "approval",
                                      "decision": {"type": "approve_for_session"}}}],
            "steps": [session_start("ask"), {"turn": "Run touch"}, {"turn": "Again"},
                      PERSISTED],
        },
        {
            "name": "approval/accept-edits",
            "backend": [
                {"toolCalls": [call("write_file", {"path": "made.txt", "content": "x\n"},
                                    "call_a1")]},
                {"toolCalls": [call("bash", {"command": "touch other.txt"}, "call_a2")]},
                {"text": "Done."},
            ],
            "callbacks": [{"approve": True}],
            "steps": [session_start("accept-edits"), {"turn": "Write then run"}],
        },
        {
            "name": "approval/interrupted",
            "backend": TOUCH,
            "callbacks": [{"ignore": True}],
            "steps": [session_start("ask"),
                      {"turn": "Run echo", "background": True, "settle": 1.5},
                      {"interrupt": True}, PERSISTED],
        },
        # -- rewind ----------------------------------------------------------
        {
            "name": "rewind/inplace",
            "backend": REWIND_WRITES,
            "steps": [START, *REWIND_TURNS,
                      send("session/rewind/read", sessionId="$S1", entryId="$U2"),
                      send("session/rewind", sessionId="$S1", entryId="$U2", inplace=True,
                           restoreFiles=True),
                      {"file": "$WS/a.txt"}, {"file": "$WS/b.txt"},
                      send("session/rewind/read", sessionId="$S1", entryId="$U1"),
                      {"turn": "Again"}, send("session/read", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "rewind/keep-files",
            "backend": REWIND_WRITES,
            "steps": [START, *REWIND_TURNS,
                      send("session/rewind", sessionId="$S1", entryId="$U1", inplace=True,
                           restoreFiles=False),
                      {"file": "$WS/a.txt"}, send("session/turns/list", sessionId="$S1"),
                      PERSISTED],
        },
        {
            "name": "rewind/fork",
            "backend": REWIND_WRITES,
            "steps": [START, *REWIND_TURNS,
                      send("session/rewind", sessionId="$S1", entryId="$U2", inplace=False,
                           restoreFiles=True),
                      {"file": "$WS/a.txt"}, {"file": "$WS/b.txt"},
                      send("session/read", sessionId="$S2"),
                      send("session/rewind/read", sessionId="$S2", entryId="$U1"),
                      {"turn": "Again", "session": "$S2"}, send("session/list"), PERSISTED],
        },
        {
            "name": "rewind/projected-ids",
            "backend": REWIND_WRITES,
            "steps": [START, {"turn": "Write one"}, {"turn": "Write two"},
                      send("session/rewind/read", sessionId="$S1", entryId="$U2"),
                      send("session/rewind", sessionId="$S1", entryId="$U2", inplace=True,
                           restoreFiles=True),
                      send("session/rewind", sessionId="$S1", entryId="$U2", inplace=False,
                           restoreFiles=True),
                      {"file": "$WS/a.txt"}, PERSISTED],
        },
        {
            "name": "rewind/unknown",
            "backend": [{"text": "Hi."}],
            "steps": [START, {"turn": "Hello"},
                      send("session/rewind", sessionId="$S1", entryId="user-missing", inplace=True,
                           restoreFiles=True),
                      send("session/rewind/read", sessionId="$S1", entryId="$A1"),
                      send("session/rewind", sessionId="$S1", entryId="$U1", inplace=True,
                           restoreFiles=True),
                      send("session/read", sessionId="$S1")],
        },
        # -- background titles -----------------------------------------------
        {
            **loop_capture.TITLES,
            "name": "titles/first-answer",
            "backend": [{"text": "Answer one."}],
            "utility": [{"text": "Parser plan"}],
            "steps": [START, {"turn": "Plan the parser", "linger": 1.5}, PERSISTED],
        },
        {
            **loop_capture.TITLES,
            "name": "titles/tool-heavy-turn",
            "backend": [{**step, "delay": 0.6} if index in (2, 9) else step
                        for index, step in enumerate(loop_capture.echo_steps(8))],
            "utility": [{"text": "Echo marathon"}, {"text": "Echo marathon two"}],
            "steps": [START, {"turn": "Echo many times", "linger": 1.5}, PERSISTED],
        },
        {
            **loop_capture.TITLES,
            "name": "titles/after-compaction",
            "backend": [{"text": "Answer one."}, {"text": "A summary of the session."},
                        {"text": "Answer two."}],
            "utility": [{"text": "First title"}, {"text": "Second title"}],
            "steps": [START, {"turn": "First question", "linger": 1.5},
                      {**send("session/compact", sessionId="$S1"), "linger": 1.0},
                      {"turn": "Second question", "linger": 1.5}, PERSISTED],
        },
        {
            **loop_capture.TITLES,
            "name": "titles/manual-title-wins",
            "backend": [{"text": "Answer one."}],
            "steps": [START, send("session/rename", sessionId="$S1", title="Mine"),
                      {"turn": "First question", "linger": 1.5}, PERSISTED],
        },
        {
            **loop_capture.TITLES,
            "name": "titles/generic-answer",
            "backend": [{"text": "Answer one."}, {"text": "Answer two."}],
            "utility": [{"text": "\"New session\""}, {"text": "  Second   try\nignored"}],
            "steps": [START, {"turn": "First question", "linger": 1.5},
                      {"turn": "Second question", "linger": 1.5}, PERSISTED],
        },
        # -- smart approve ---------------------------------------------------
        {
            "name": "smart/allow",
            "config": SMART_APPROVE,
            "backend": TOUCH,
            "utility": [verdict(True, "Creates a file in the workspace", "low", "high")],
            "steps": [session_start("smart-approve"), {"turn": "Run touch"}, PERSISTED],
        },
        {
            "name": "smart/deny-continue",
            "config": SMART_APPROVE,
            "backend": TOUCH,
            "utility": [verdict(False, "Touches files nobody asked for", "medium", "unknown")],
            "steps": [session_start("smart-approve"), {"turn": "Run touch"}, PERSISTED],
        },
        {
            "name": "smart/escalate",
            "config": SMART_APPROVE,
            "backend": TOUCH,
            "utility": [verdict(False, "Writes into the workspace", "high", "high")],
            "callbacks": [{"approve": True}],
            "steps": [session_start("smart-approve"), {"turn": "Run touch"}, PERSISTED],
        },
        {
            "name": "smart/unparsable",
            "config": SMART_APPROVE,
            "backend": TOUCH,
            "utility": [{"text": "not a verdict"}],
            "callbacks": [{"approve": False}],
            "steps": [session_start("smart-approve"), {"turn": "Run touch"}, PERSISTED],
        },
        {
            "name": "smart/repeat",
            "config": SMART_APPROVE,
            "backend": [TOUCH[0], TOUCH_AGAIN[0], {"text": "Finished."}],
            "utility": [verdict(False, "Touches files nobody asked for", "medium", "unknown"),
                        verdict(False, "Touches files nobody asked for", "medium", "unknown")],
            "callbacks": [{"approve": False}],
            "steps": [session_start("smart-approve"), {"turn": "Run touch"}, PERSISTED],
        },
        {
            "name": "smart/session-grant",
            "config": SMART_APPROVE,
            "backend": [*TOUCH, *TOUCH_AGAIN],
            "utility": [verdict(False, "Writes into the workspace", "high", "high")],
            "callbacks": [{"output": {"type": "approval",
                                      "decision": {"type": "approve_for_session"}}}],
            "steps": [session_start("smart-approve"), {"turn": "Run touch"}, {"turn": "Again"},
                      PERSISTED],
        },
        {
            "name": "smart/precleared",
            "config": SMART_APPROVE,
            "backend": ECHO,
            "steps": [session_start("smart-approve"), {"turn": "Run echo"}, PERSISTED],
        },
        {
            "name": "question/answer",
            "backend": [{"toolCalls": [call("ask_user_question", QUESTION, "call_q1")]},
                        {"text": "Thanks."}],
            "callbacks": [{"choose": "Blue"}],
            "steps": [START, {"turn": "Ask me"}],
        },
        {
            "name": "tools/skill",
            "backend": [{"toolCalls": [call("skill", {"name": "vibe"}, "call_k1")]},
                        {"toolCalls": [call("skill", {"name": "vibe:worktree"}, "call_k2")]},
                        {"text": "Loaded."}],
            "steps": [START, {"turn": "Load the skill"}],
        },
        {
            "name": "approval/cancel-turn",
            "backend": TOUCH,
            "callbacks": [{"output": {"type": "approval", "decision": {"type": "cancel_turn"}}}],
            "steps": [session_start("ask"), {"turn": "Run touch"}, PERSISTED],
        },
        # -- steering a turn -------------------------------------------------
        {
            "name": "turn/interrupt-stream",
            "backend": [{"text": "Slow.", "delay": 3}],
            "steps": [START, {"turn": "Wait", "background": True, "settle": 1.0},
                      {"interrupt": True}, PERSISTED],
        },
        {
            "name": "turn/steer",
            "backend": [{"toolCalls": [call("bash", {"command": "sleep 1"}, "call_s1")]},
                        {"text": "Steered."}],
            "steps": [START, {"turn": "Work", "background": True, "settle": 0.5},
                      {"steer": "Also this"}, {"wait": True}, PERSISTED],
        },
        {
            "name": "turn/steer-applied",
            "backend": [{"toolCalls": [call("bash", {"command": "sleep 3"}, "call_s1")]},
                        {"text": "Steered."}],
            "steps": [START, {"turn": "Work", "background": True, "settle": 0.5},
                      send("turn/steer", sessionId="$S1", expectedTurnId="turn-other",
                           message=[{"type": "text", "text": "Wrong turn"}]),
                      send("turn/steer", sessionId="$S1", expectedTurnId="$R1",
                           message=[{"type": "text", "text": "Also this"}]),
                      {"wait": True},
                      send("turn/steer", sessionId="$S1", expectedTurnId="$R1",
                           message=[{"type": "text", "text": "Too late"}]),
                      PERSISTED],
        },
        # -- preparing a turn's input ----------------------------------------
        {
            "name": "prepare/skills",
            "backend": [{"text": "Reviewed."}, {"text": "Again."}, {"text": "Plain."}],
            "steps": [{"write": {
                          "$ROOT/vibe-home/skills/review/SKILL.md":
                              "---\nname: review\ndescription: Review the change\n---\n\nRead the diff.\n",
                          "$ROOT/vibe-home/skills/hidden/SKILL.md":
                              "---\nname: hidden\ndescription: Not for users\nuser-invocable: false\n---\n\nSecret.\n",
                      }},
                      START, {"turn": "/review the diff, then /hidden and `/review`"},
                      {"turn": "Please /review again."},
                      {"turn": "/unknown skill"},
                      send("session/read", sessionId="$S1"),
                      send("session/list"),
                      PERSISTED],
        },
        {
            "name": "prepare/mentions",
            "backend": [{"text": "Read it."}],
            "steps": [{"write": {"notes.txt": "first line\nsecond line\n",
                                 "fence.md": "````\ncode\n````\n"}},
                      START, {"turn": "Look at @notes.txt and @fence.md and @missing.txt"},
                      PERSISTED],
        },
        {
            "name": "prepare/scratchpad",
            "backend": [{"toolCalls": [call("unified_harness_scratchpad",
                                            {"action": "write", "path": "plan.md",
                                             "content": "Step one"}, "call_p1")]},
                        {"text": "Saved."}, {"text": "Next."}, {"text": "Same."},
                        {"toolCalls": [call("unified_harness_scratchpad",
                                            {"action": "write", "path": "plan.md",
                                             "content": ""}, "call_p2")]},
                        {"text": "Removed."}, {"text": "Empty."}],
            "steps": [START, {"turn": "Save a note"}, {"turn": "Continue"}, {"turn": "Again"},
                      {"turn": "Empty it"}, {"turn": "Now"}, PERSISTED],
        },
        # -- images ----------------------------------------------------------
        {
            "name": "images/native",
            "backend": [{"text": "Inline seen."}, {"text": "File seen."}],
            "steps": [{"run": ["python3", "-c", WRITE_PNG]},
                      START,
                      {"turn": "What is this?", "params": {"message": [
                          {"type": "text", "text": "What is this?"}, inline_image()]}},
                      {"turn": "And this file?", "params": {"message": [
                          {"type": "text", "text": "And this file?"}, file_image("$WS/shot.png")]}},
                      send("session/read", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "images/described",
            "tables": BLIND_MODELS + VISION_MODEL,
            "utility": [{"text": "A single red pixel."},
                        {"status": 500, "body": {"message": "vision down"}}],
            "backend": [{"text": "Described."}, {"text": "Unreadable."}],
            "steps": [{"run": ["python3", "-c", WRITE_PNG]},
                      START,
                      {"turn": "What is this?", "params": {"message": [
                          {"type": "text", "text": "What is this?"}, inline_image()]}},
                      {"turn": "And this file?", "params": {"message": [
                          {"type": "text", "text": "And this file?"}, file_image("$WS/shot.png")]}},
                      PERSISTED],
        },
        {
            "name": "images/blind",
            "tables": BLIND_MODELS,
            "backend": [{"text": "Linked."}, {"text": "Inline."}],
            "steps": [{"run": ["python3", "-c", WRITE_PNG]},
                      START,
                      {"turn": "And this file?", "params": {"message": [
                          {"type": "text", "text": "And this file?"}, file_image("$WS/shot.png")]}},
                      {"turn": "What is this?", "params": {"message": [
                          {"type": "text", "text": "What is this?"}, inline_image()]}},
                      PERSISTED],
        },
        {
            "name": "errors/bad-request",
            "backend": [{"status": 400, "body": {"message": "bad request"}}],
            "steps": [START, {"turn": "Fail"}, PERSISTED],
        },
        {
            "name": "errors/unauthorized",
            "backend": [{"status": 401, "body": {"message": "no key"}}],
            "steps": [START, {"turn": "Fail"}],
        },
        {
            "name": "errors/retried",
            "backend": [{"status": 503, "body": {"message": "busy"}}, {"text": "Recovered."}],
            "steps": [START, {"turn": "Retry"}],
        },
        {
            "name": "session/reads",
            "backend": [{"text": "Hi."}],
            "steps": [START, {"turn": "Hello"},
                      send("session/turns/list", sessionId="$S1"),
                      send("session/history/list", sessionId="$S1"),
                      send("session/history/get", sessionId="$S1"),
                      send("session/title/update", sessionId="$S1", title="Titled")],
        },
        # -- code mode -------------------------------------------------------
        {
            "name": "code/inventory",
            "backend": [{"toolCalls": [call("search_tool_functions",
                                            {"mode": "all_connector_capabilities"}, "call_s1")]},
                        {"toolCalls": [call("search_tool_functions",
                                            {"query": "run a background process"}, "call_s2")]},
                        {"toolCalls": [call("search_tool_functions",
                                            {"functions": ["process.start", "subagent.spawn"]}, "call_s3")]},
                        {"text": "Listed."}],
            "steps": [START, {"turn": "List"}],
        },
        {
            "name": "code/details",
            "backend": [{"toolCalls": [call("search_tool_functions", {"functions": [
                "file_system.bash", "file_system.read_file", "file_system.write_file",
                "file_system.edit", "self.sleep", "process.start", "process.output",
                "process.write", "process.list", "process.stop"]}, "call_d1")]},
                        {"toolCalls": [call("search_tool_functions", {"functions": [
                "subagent.list", "subagent.spawn", "subagent.wait", "subagent.sendMessage",
                "subagent.interrupt", "subagent.close", "vibe.todo", "tools.vibe.todo",
                "nowhere.missing"]}, "call_d2")]},
                        {"toolCalls": [call("search_tool_functions", {"mode": "details"}, "call_d3")]},
                        {"toolCalls": [call("search_tool_functions",
                                            {"mode": "all_connector_capabilities",
                                             "connectors": ["process", "nowhere"]}, "call_d4")]},
                        {"toolCalls": [call("search_tool_functions", {"mode": "best_match"}, "call_d5")]},
                        {"toolCalls": [call("search_tool_functions", {"query": "zzzz"}, "call_d6")]},
                        {"toolCalls": [call("search_tool_functions", {"query": "stop",
                                                                      "connectors": ["nowhere"]}, "call_d7")]},
                        {"toolCalls": [call("search_tool_functions", {"functions": [""]}, "call_d8")]},
                        {"text": "Detailed."}],
            "steps": [START, {"turn": "Details"}],
        },
        {
            "name": "code/basic",
            "backend": [{"toolCalls": [call("run_typescript", {"code": (
                "async function main() {\n  console.log('hi', {a: 1});\n"
                "  console.error('warn');\n  return {value: 42, list: [1, 'two']};\n}\n")}, "call_t1")]},
                        {"toolCalls": [call("run_typescript", {"code": (
                "async function main() {\n  throw new Error('boom');\n}\n")}, "call_t2")]},
                        {"toolCalls": [call("run_typescript", {"code": "const x: = 1;"}, "call_t3")]},
                        {"toolCalls": [call("run_typescript", {"code": "const y = 1;"}, "call_t4")]},
                        {"text": "Ran."}],
            "steps": [START, {"turn": "Run"}],
        },
        {
            "name": "code/tools",
            "backend": [{"toolCalls": [call("run_typescript", {"code": (
                "async function main() {\n"
                "  const read = await tools.file_system.read_file({path: 'notes.txt'});\n"
                "  return read;\n}\n")}, "call_t5")]},
                        {"text": "Read."}],
            "steps": [START, {"write": {"notes.txt": "alpha\nbeta\n"}}, {"turn": "Read"}],
        },
        # -- an MCP server's tools ------------------------------------------
        {
            "name": "mcp/calls",
            "files": {"mcp_server.py": MCP_SERVER},
            "tables": MCP_TABLES,
            "backend": [
                {"toolCalls": [call("search_tool_functions",
                                    {"mode": "all_connector_capabilities",
                                     "connectors": ["mcp_data_api"]}, "call_m1")]},
                {"toolCalls": [call("search_tool_functions",
                                    {"functions": ["mcp_data_api.echo",
                                                   "mcp_data_api.create_issue"]}, "call_m2")]},
                {"toolCalls": [call("run_typescript", {"code": MCP_PROGRAM}, "call_m3")]},
                {"text": "Called."},
            ],
            "steps": [*MCP_START, {"turn": "Use the tracker"}, PERSISTED],
        },
        {
            "name": "mcp/approve",
            "files": {"mcp_server.py": MCP_SERVER},
            "tables": MCP_TABLES,
            "backend": [{"toolCalls": [call("run_typescript", {"code": (
                "async function main() {\n"
                "  return await tools.mcp_data_api.echo({message: 'asked'});\n}\n")},
                "call_m4")]}, {"text": "Approved."}],
            "callbacks": [{"approve": True}],
            "steps": [session_start("ask"), {"sleep": 2.0}, {"turn": "Echo"}, PERSISTED],
        },
        {
            "name": "mcp/deny",
            "files": {"mcp_server.py": MCP_SERVER},
            "tables": MCP_TABLES,
            "backend": [{"toolCalls": [call("run_typescript", {"code": (
                "async function main() {\n"
                "  return await tools.mcp_data_api.echo({message: 'refused'});\n}\n")},
                "call_m5")]}, {"text": "Denied."}],
            "callbacks": [{"approve": False}],
            "steps": [session_start("ask"), {"sleep": 2.0}, {"turn": "Echo"}],
        },
        {
            "name": "mcp/never",
            "files": {"mcp_server.py": MCP_SERVER},
            "tables": MCP_TABLES + '\n[tools.data-api_echo]\npermission = "never"\n',
            "backend": [{"toolCalls": [call("run_typescript", {"code": (
                "async function main() {\n"
                "  return await tools.mcp_data_api.echo({message: 'never'});\n}\n")},
                "call_m6")]}, {"text": "Refused."}],
            "steps": [session_start("ask"), {"sleep": 2.0}, {"turn": "Echo"}],
        },
        {
            "name": "mcp/runtime",
            "files": {"mcp_server.py": MCP_SERVER, **MCP_PLUGIN},
            "tables": MCP_TABLES,
            "steps": [*MCP_START, runtime_read()],
        },
        {
            "name": "mcp/plugin",
            "files": MCP_PLUGIN,
            "backend": [
                {"toolCalls": [call("search_tool_functions",
                                    {"mode": "all_connector_capabilities",
                                     "connectors": ["mcp_issues"]}, "call_m7")]},
                {"toolCalls": [call("run_typescript", {"code": (
                    "async function main() {\n"
                    "  return await tools.mcp_issues.create_issue({title: 'From a plugin'});\n}\n")},
                    "call_m8")]},
                {"text": "Opened."},
            ],
            "callbacks": [{"approve": True}],
            "steps": [session_start("ask"), {"sleep": 2.0}, {"turn": "Open one"}, PERSISTED],
        },
        # -- background processes -------------------------------------------
        {
            "name": "process/lifecycle",
            "backend": [{"toolCalls": [call("run_typescript", {"code": PROCESS_PROGRAM}, "call_p1")]},
                        {"text": "Processes."}],
            "steps": [START, {"turn": "Processes"},
                      send("session/backgroundProcess/output", sessionId="$S1",
                           processId="process-missing"),
                      send("session/backgroundProcess/output", sessionId="$S1",
                           processId="process-missing", fromEnd=False, cursor=0, waitMs=0,
                           maxBytes=100),
                      send("session/backgroundProcess/stop", sessionId="$S1",
                           processId="process-missing"),
                      PERSISTED],
        },
        {
            "name": "process/interactive",
            "backend": [{"toolCalls": [call("run_typescript", {"code": INTERACTIVE_PROGRAM}, "call_p2")]},
                        {"text": "Interacted."}],
            "steps": [START, {"turn": "Interact"}],
        },
        {
            "name": "process/idle-exit",
            "backend": [{"toolCalls": [call("run_typescript", {"code": (
                "async function main() {\n"
                "  const late = await tools.process.start({command: 'sleep 1; echo late'});\n"
                "  return late.status;\n}\n")}, "call_p3")]},
                        {"text": "Started."}, {"text": "Again."}],
            "steps": [START, {"turn": "Start"}, {"sleep": 2.0},
                      send("session/resume", sessionId="$S1"), {"turn": "Again"}],
        },
        # -- subagents -------------------------------------------------------
        {
            "name": "subagent/lifecycle",
            "backend": [{"toolCalls": [call("run_typescript", {"code": SUBAGENT_PROGRAM}, "call_a1")]},
                        {"text": "Child answer."},
                        {"text": "Child second answer."},
                        {"text": "Delegated."}],
            "steps": [START, {"turn": "Delegate"},
                      send("session/list"), PERSISTED],
        },
        {
            "name": "subagent/interrupt",
            "backend": [{"toolCalls": [call("run_typescript", {"code": SUBAGENT_INTERRUPT_PROGRAM}, "call_i1")]},
                        {"toolCalls": [call("bash", {"command": "sleep 30"}, "call_i2")]},
                        {"text": "Resumed."},
                        {"text": "Interrupted."}],
            "steps": [START, {"turn": "Delegate"}],
        },
        {
            "name": "subagent/approval",
            "backend": [{"toolCalls": [call("run_typescript", {"code": SUBAGENT_APPROVAL_PROGRAM}, "call_q1")]},
                        {"toolCalls": [call("bash", {"command": "touch made.txt"}, "call_q2")]},
                        {"text": "Touched."},
                        {"text": "Approved."}],
            "callbacks": [{"approve": True}],
            "steps": [session_start("ask"), {"turn": "Delegate"}, PERSISTED],
        },
        {
            "name": "subagent/workspace-type",
            "backend": [{"toolCalls": [call("run_typescript", {"code": SUBAGENT_TYPED_PROGRAM}, "call_r1")]},
                        {"toolCalls": [call("bash", {"command": "echo hi"}, "call_r2")]},
                        {"text": "Reviewed."},
                        {"text": "Done."}],
            "steps": [REVIEWER, START, {"turn": "Review"}, PERSISTED],
        },
        {
            "name": "subagent/workspace-type-auto",
            "backend": [{"toolCalls": [call("run_typescript", {"code": SUBAGENT_TYPED_PROGRAM}, "call_r1")]},
                        {"toolCalls": [call("read_file", {"path": "notes.txt"}, "call_r2")]},
                        {"text": "Reviewed."},
                        {"text": "Done."}],
            "files": {"notes.txt": "note\n"},
            "steps": [REVIEWER, session_start("auto-approve"), {"turn": "Review"}],
        },
        {
            "name": "subagent/workspace-type-write",
            "backend": [{"toolCalls": [call("run_typescript", {"code": SUBAGENT_TYPED_PROGRAM}, "call_r1")]},
                        {"toolCalls": [call("write_file", {"path": "review.md", "content": "ok\n"}, "call_r2")]},
                        {"text": "Reviewed."},
                        {"text": "Done."}],
            "callbacks": [{"approve": True}],
            "steps": [REVIEWER, session_start("auto-approve"), {"turn": "Review"}, {"file": "review.md"}],
        },
        {
            "name": "subagent/resume-children",
            "backend": [{"toolCalls": [call("run_typescript", {"code": SUBAGENT_KEEP_PROGRAM}, "call_k1")]},
                        {"text": "Child answer."},
                        {"text": "Kept."},
                        {"text": "After."}],
            "steps": [START, {"turn": "Delegate"},
                      send("session/read", sessionId="$K1"),
                      send("session/history/list", sessionId="$K1"),
                      {"restart": True},
                      send("session/resume", sessionId="$S1"),
                      send("session/read", sessionId="$K1"),
                      {"turn": "Again"}, PERSISTED],
        },
        # -- sessions an older Vibe saved ------------------------------------------
        {
            "name": "legacy/list-read",
            "steps": [LEGACY, send("session/list"),
                      send("session/read", sessionId=LEGACY_ID),
                      send("session/read", sessionId=LEGACY_ID, history={"limit": 1})],
        },
        {
            "name": "legacy/resume",
            "backend": [{"text": "Continued."}],
            "steps": [LEGACY, send("session/resume", sessionId=LEGACY_ID),
                      {"turn": "Go on"}, send("session/list"), PERSISTED],
        },
        # -- scheduled loops ----------------------------------------------------
        {
            "name": "loops/methods",
            "steps": [START,
                      send("loops/list", sessionId="$S1"),
                      send("loops/create", sessionId="$S1", interval="5m", prompt="  Check the build  "),
                      send("loops/create", sessionId="$S1", interval="5x", prompt="Bad"),
                      send("loops/create", sessionId="$S1", interval="10s", prompt="Too fast"),
                      send("loops/create", sessionId="$S1", interval="1h", prompt="/compact"),
                      send("loops/create", sessionId="$S1", interval="1h", prompt="   "),
                      send("loops/list", sessionId="$S1"),
                      send("loops/delete", sessionId="$S1", loopId="nope"),
                      send("loops/delete", sessionId="$S1", loopId="$L1"),
                      send("loops/create", sessionId="$S1", interval="2d", prompt="Daily"),
                      send("loops/clear", sessionId="$S1"),
                      send("loops/clear", sessionId="$S1")],
        },
        {
            "name": "loops/cron-tool",
            "backend": [{"toolCalls": [
                            call("cron", {"action": "schedule", "interval_seconds": 120,
                                          "prompt": "Check the build"}, "call_c1"),
                            call("cron", {"action": "schedule_cron", "cron": "0 9 * * 1",
                                          "prompt": "Weekly report"}, "call_c2"),
                            call("cron", {"action": "schedule", "interval_seconds": 5,
                                          "prompt": "Too fast"}, "call_c3"),
                            call("cron", {"action": "schedule_cron", "cron": "bad",
                                          "prompt": "Broken"}, "call_c4"),
                            call("cron", {"action": "cancel", "id": "nope"}, "call_c5")]},
                        {"toolCalls": [call("cron", {"action": "list"}, "call_c6")]},
                        {"toolCalls": [call("cron", {"action": "clear"}, "call_c7")]},
                        {"text": "Scheduled."}],
            "steps": [START, {"turn": "Schedule"},
                      send("loops/list", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "loops/fire",
            "backend": [{"text": "Hello."}, {"text": "Looped."}],
            "steps": [START, {"turn": "Hi"},
                      send("loops/create", sessionId="$S1", interval="30s", prompt="Check the build"),
                      {"wait": True},
                      send("loops/list", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "loops/slash-loop",
            "backend": [{"toolCalls": [call("cron", {"action": "schedule", "interval_seconds": 60,
                                                     "prompt": "Check the build"}, "call_l1")]},
                        {"text": "Scheduled."}],
            "steps": [START, {"turn": "/loop every minute check the build"},
                      send("session/read", sessionId="$S1"),
                      send("session/list")],
        },
        # -- session operations ---------------------------------------------
        {
            "name": "session/compact",
            "backend": [{"text": "First."}],
            "utility": [{"text": "A summary of the work."}],
            "steps": [START, {"turn": "Hello"},
                      send("session/compact", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "compact/auto-midturn",
            "tables": AUTO_COMPACT_MODEL,
            "backend": [{"toolCalls": [call("bash", {"command": "echo hi"}, "call_c1")],
                         "promptTokens": 60000},
                        {"text": "Done."}],
            "utility": [{"text": "A summary of the work."}],
            "steps": [START, {"turn": "Work"},
                      send("session/read", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "compact/auto-next-turn",
            "tables": AUTO_COMPACT_MODEL,
            "backend": [{"text": "First.", "promptTokens": 60000}, {"text": "Second."}],
            "utility": [{"text": "A summary of the work."}],
            "steps": [START, {"turn": "One"}, {"turn": "Two"},
                      send("session/read", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "compact/auto-empty",
            "tables": AUTO_COMPACT_MODEL,
            "backend": [{"text": "First.", "promptTokens": 60000},
                        {"text": " "}, {"text": " "}, {"text": " "}, {"text": "Second."}],
            "steps": [START, {"turn": "One"}, {"turn": "Two"},
                      send("session/read", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "compact/auto-failed",
            "tables": AUTO_COMPACT_MODEL,
            "backend": [{"text": "First.", "promptTokens": 60000},
                        {"toolCalls": [call("bash", {"command": "echo a"}, "call_f1")]},
                        {"toolCalls": [call("bash", {"command": "echo b"}, "call_f2")]},
                        {"toolCalls": [call("bash", {"command": "echo c"}, "call_f3")]},
                        {"text": "Second."}],
            "steps": [START, {"turn": "One"}, {"turn": "Two"},
                      send("session/read", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "session/inject",
            "backend": [{"text": "Seen."}],
            "steps": [START,
                      send("session/context/inject", sessionId="$S1",
                           input=[{"type": "text", "text": "Background fact."}]),
                      {"turn": "Hello"}],
        },
        {
            "name": "session/clear",
            "backend": [{"text": "First."}],
            "steps": [START, {"turn": "Hello"},
                      send("session/history/clear", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "session/fork-delete",
            "backend": [{"text": "First."}],
            "steps": [START, {"turn": "Hello"},
                      send("session/fork", sourceSessionId="$S1"),
                      send("session/list"),
                      send("session/delete", sessionId="$S1"),
                      send("session/list")],
        },
        {
            "name": "session/settings",
            "backend": [{"text": "First."}],
            "steps": [START,
                      send("session/settings/update", sessionId="$S1",
                           settings={"reasoningEffort": "low"}),
                      send("session/agent/update", sessionId="$S1", agent="plan"),
                      {"turn": "Hello"}],
        },
        {
            "name": "queue/enqueue",
            "backend": [{"toolCalls": [call("bash", {"command": "sleep 8"}, "call_q0")]},
                        {"text": "First answer."}, {"text": "Second answer."},
                        {"text": "Third answer."}],
            "steps": [START, {"turn": "First", "background": True, "settle": 0.5},
                      send("session/turn/enqueue", sessionId="$S1", entries=[]),
                      send("session/turn/enqueue", sessionId="$S1",
                           entries=[user_entry("Second")]),
                      send("session/turn/enqueue", sessionId="$S1",
                           entries=[{"role": "context",
                                     "content": [{"type": "text", "text": "Context note"}]},
                                    user_entry("Third")]),
                      send("session/turn/queue/read", sessionId="$S1"),
                      send("session/turn/queue/replace", sessionId="$S1", queueItemId="$Q1",
                           entries=[user_entry("Second, edited")]),
                      {"wait": True}, {"sleep": 3},
                      send("session/turn/queue/read", sessionId="$S1"), PERSISTED],
        },
        {
            "name": "queue/pause",
            "backend": [{"text": "Slow.", "delay": 3}, {"text": "Resumed."}],
            "steps": [START, {"turn": "First", "background": True, "settle": 0.5},
                      send("session/turn/enqueue", sessionId="$S1", entries=[user_entry("Later")]),
                      {"interrupt": True}, {"sleep": 1},
                      send("session/turn/queue/read", sessionId="$S1"),
                      send("session/turn/queue/resume", sessionId="$S1"),
                      {"sleep": 2}, PERSISTED],
        },
        {
            "name": "queue/steer",
            "backend": [{"toolCalls": [call("bash", {"command": "sleep 7"}, "call_q1")]},
                        {"text": "Steered."}, {"text": "Idle answer."}],
            "steps": [START, {"turn": "Work", "background": True, "settle": 0.5},
                      send("session/turn/enqueue", sessionId="$S1",
                           entries=[user_entry("Steer me")]),
                      send("session/turn/enqueue", sessionId="$S1",
                           entries=[user_entry("Remove me")]),
                      send("session/turn/queue/steer", sessionId="$S1", queueItemId="$Q1",
                           expectedTurnId="$R1"),
                      send("session/turn/queue/remove", sessionId="$S1", queueItemId="$Q2"),
                      send("session/turn/queue/steer", sessionId="$S1", queueItemId="$Q2",
                           expectedTurnId="$R1"),
                      {"wait": True},
                      send("session/turn/enqueue", sessionId="$S1", entries=[user_entry("Idle")]),
                      {"sleep": 2}, PERSISTED],
        },
        {
            "name": "session/shell",
            "steps": [START,
                      send("session/shellCommand", sessionId="$S1", command="echo shell"),
                      {"sleep": 1}],
        },
        # -- session marks ---------------------------------------------------
        {
            "name": "session/marks",
            "backend": [{"text": "Hi."}],
            "steps": [START, {"turn": "Hello"},
                      send("session/rename", sessionId="$S1", title="Renamed"),
                      send("session/pin", sessionId="$S1", pinned=True),
                      send("session/markAsSeen", sessionId="$S1"),
                      send("session/archive", sessionId="$S1", archived=True),
                      send("session/list"),
                      send("session/archive", sessionId="$S1", archived=False),
                      send("session/pin", sessionId="$S1", pinned=False),
                      PERSISTED],
        },
        # -- the store -------------------------------------------------------
        {
            "name": "store/restart-and-resume",
            "backend": [{"text": "Before."}, {"text": "After."}],
            "steps": [START, {"turn": "Before the restart"}, {"restart": True},
                      send("session/list"),
                      send("session/resume", sessionId="$S1"),
                      {"turn": "After the restart"}, PERSISTED],
        },
    ]
