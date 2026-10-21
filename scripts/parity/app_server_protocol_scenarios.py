"""The scenarios ``app_server_protocol.py`` drives an app server with, the
reference's or this port's.

A scenario is a list of steps: the frames the client writes, the requests and
notifications it sends, the turns it starts, and what the chat-completions
stand-in answers them with. The per-method probes stay in the driver because
they are derived from the committed surface census rather than authored. Every
value here is authored for the corpus.
"""

from __future__ import annotations

from typing import Any


INIT = {"init": True}
START = {"start": {}}


def send(method: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
    return {"send": {"method": method, "params": params if params is not None else {}}}


def exact(step: dict[str, Any]) -> dict[str, Any]:
    """A step whose result is compared by value, not only by shape."""
    return {**step, "exact": True}


def frames() -> list[dict[str, Any]]:
    client = {"clientInfo": {"name": "protocol-oracle", "version": "0"}}
    return [
        {"name": "frames/request-before-initialize",
         "steps": [send("session/list"), {"init": True, "record": True}, send("session/list", {"cwd": "/probe-empty"})]},
        {"name": "frames/initialize-twice",
         "steps": [{"init": True, "record": True}, {"send": {"method": "initialize", "params": client}}]},
        {"name": "frames/initialize-invalid",
         "steps": [
             {"send": {"method": "initialize", "params": {}}},
             {"send": {"method": "initialize", "params": {"clientInfo": {"name": "x"}, "extra": 1}}},
             {"send": {"method": "initialize", "params": {**client, "capabilities": {"callbackKinds": ["nope"]}}}},
             {"init": True, "record": True},
         ]},
        {"name": "frames/initialize-with-capabilities",
         "steps": [
             {"init": True, "record": True, "params": {
                 "clientInfo": {"name": "protocol-oracle", "version": "0", "title": "Oracle",
                                "entrypoint": "desktop", "terminalEmulator": "kitty"},
                 "capabilities": {"callbackKinds": ["approval", "user_input"],
                                  "clientTools": ["filesystem/read"],
                                  "disabledNotifications": ["warning"]}}},
             send("session/list", {"cwd": "/probe-empty"}),
         ]},
        {"name": "frames/request-before-initialized",
         "steps": [
             {"send": {"method": "initialize", "params": client}},
             send("session/list", {"cwd": "/probe-empty"}),
         ]},
        {"name": "frames/initialized-twice",
         "steps": [INIT, {"notify": {"method": "initialized", "params": {}}}]},
        {"name": "frames/initialized-before-initialize",
         "steps": [{"notify": {"method": "initialized", "params": {}}}]},
        {"name": "frames/unknown-notification",
         "steps": [INIT, {"notify": {"method": "probe/unknown", "params": {}}}]},
        {"name": "frames/exit-notification",
         "steps": [INIT, {"notify": {"method": "exit", "params": {}}}]},
        {"name": "frames/shutdown-request",
         "steps": [INIT, send("shutdown")]},
        {"name": "frames/unknown-method",
         "steps": [INIT, send("probe/unknown"), send("internal/session/list")]},
        {"name": "frames/unknown-method-before-initialize",
         "steps": [send("probe/unknown")]},
        {"name": "frames/string-id",
         "steps": [INIT, {"frame": {"jsonrpc": "2.0", "id": "text-id", "method": "session/list",
                                    "params": {"cwd": "/probe-empty"}}}]},
        {"name": "frames/missing-params",
         "steps": [INIT, {"frame": {"jsonrpc": "2.0", "id": 90, "method": "session/list"}}]},
        {"name": "frames/extra-envelope-field",
         "steps": [INIT, {"frame": {"jsonrpc": "2.0", "id": 91, "method": "session/list",
                                    "params": {}, "extra": 1}}]},
        {"name": "frames/wrong-version",
         "steps": [INIT, {"frame": {"jsonrpc": "1.0", "id": 92, "method": "session/list",
                                    "params": {}}}]},
        {"name": "frames/response-to-nothing",
         "steps": [INIT, {"frame": {"jsonrpc": "2.0", "id": 93, "result": {}}}]},
        {"name": "frames/error-to-nothing",
         "steps": [INIT, {"frame": {"jsonrpc": "2.0", "id": 94, "error": {
             "code": "internal_error", "message": "x", "data": None}}}]},
        {"name": "frames/malformed-json",
         "steps": [INIT, {"raw": "{not json\n"}]},
        {"name": "frames/non-object",
         "steps": [INIT, {"raw": "[1, 2]\n"}]},
        {"name": "frames/blank-line",
         "steps": [INIT, {"raw": "\n"}]},
    ]


def flows() -> list[dict[str, Any]]:
    return [
        {"name": "flows/session-start",
         "steps": [INIT, {"start": {}, "record": True}, send("session/start", start_params_placeholder())]},
        {"name": "flows/session-start-refusals",
         "steps": [
             INIT,
             send("session/start", {"cwd": "$WS"}),
             send("session/start", {"agentConfig": {"cwd": "$WS", "bogus": 1}}),
             send("session/start", {"agentConfig": {"cwd": "$WS"}, "historyLimit": 0}),
             send("session/start", {"agentConfig": {"cwd": "$WS"}, "kind": "nope"}),
             {"start": {"agentConfig": {"cwd": "$WS", "agent": "auto-approve"}, "kind": "ephemeral"},
              "record": True},
         ]},
        {"name": "flows/events-read",
         "steps": [
             INIT,
             exact(send("events/read")),
             exact(send("events/read", {"afterEventId": 3, "batchSize": 10,
                                        "filters": {"sessionIds": ["x"], "eventTypes": ["turn/started"]}})),
             send("events/read", {"batchSize": 0}),
             send("events/read", {"bogus": True}),
             START,
             exact(send("events/read", {"afterEventId": 0})),
         ]},
    ]


def user_entry(text: str) -> dict[str, Any]:
    return {"role": "user", "content": [{"type": "text", "text": text}]}


def context_entry(text: str) -> dict[str, Any]:
    return {"role": "context", "content": [{"type": "text", "text": text}]}


def queue_flows() -> list[dict[str, Any]]:
    """The turn queue: what a queued turn does to an idle session and to a
    busy one, how an interrupt pauses the queue, and what it refuses."""
    read = exact(send("session/turn/queue/read", {"sessionId": "$S1"}))
    return [
        {"name": "flows/turn-lifecycle",
         "backend": [{"text": "Plain answer."}, {"text": "Slow answer.", "delay": 3.0}],
         "steps": [
             INIT, START,
             send("turn/start", {"sessionId": "$S1", "message": [{"type": "text", "text": "Plain question"}]}),
             send("turn/interrupt", {"sessionId": "$S1", "expectedTurnId": "$T1"}),
             {"turn": "Slow question", "background": True, "settle": 0.5},
             send("turn/start", {"sessionId": "$S1", "message": [{"type": "text", "text": "Refused"}]}),
             send("turn/interrupt", {"sessionId": "$S1", "expectedTurnId": "$T1"}),
             exact(send("turn/interrupt", {"sessionId": "$S1", "expectedTurnId": "$T2"})),
             {"await": True},
             exact(send("session/turn/queue/read", {"sessionId": "$S1"})),
         ]},
        {"name": "flows/turn-tools",
         "files": {"notes.txt": "one\ntwo\n"},
         "backend": [
             {"text": "Reading.", "toolCalls": [{"id": "call_read", "name": "read_file",
                                                  "arguments": {"file_path": "notes.txt"}}]},
             {"text": "Read it."},
         ],
         "steps": [
             INIT,
             {"start": {"agentConfig": {"cwd": "$WS", "agent": "auto-approve"}}},
             send("turn/start", {"sessionId": "$S1", "message": [{"type": "text", "text": "Read notes"}]}),
             {"await": True},
         ]},
        {"name": "flows/turn-queue-idle",
         "backend": [{"text": "Queued answer."}],
         "steps": [
             INIT, START,
             exact(send("session/turn/enqueue", {"sessionId": "$S1", "entries": [user_entry("Queued question")]})),
             {"await": True},
             read,
         ]},
        {"name": "flows/turn-queue-busy",
         "backend": [
             {"text": "Slow answer.", "delay": 5.0},
             {"text": "First queued answer."},
             {"text": "Second queued answer."},
         ],
         "steps": [
             INIT, START,
             {"turn": "Slow question", "background": True, "settle": 0.5},
             exact(send("session/turn/enqueue", {"sessionId": "$S1", "entries": [user_entry("First queued")]})),
             exact(send("session/turn/enqueue", {"sessionId": "$S1", "entries": [
                 context_entry("Some context"), user_entry("Second queued")]})),
             read,
             exact(send("session/turn/queue/replace", {"sessionId": "$S1", "queueItemId": "$Q2",
                                                       "entries": [user_entry("Second, replaced")]})),
             exact(send("session/turn/queue/remove", {"sessionId": "$S1", "queueItemId": "$Q1"})),
             read,
             send("turn/start", {"sessionId": "$S1", "message": [{"type": "text", "text": "Refused"}]}),
             {"await": True},
             read,
         ]},
        {"name": "flows/turn-queue-interrupt",
         "backend": [
             {"text": "Slow answer.", "delay": 3.0},
             {"text": "Resumed answer."},
         ],
         "steps": [
             INIT, START,
             {"turn": "Slow question", "background": True, "settle": 0.5},
             exact(send("session/turn/enqueue", {"sessionId": "$S1", "entries": [user_entry("Waiting")]})),
             exact(send("turn/interrupt", {"sessionId": "$S1", "expectedTurnId": "$T1"})),
             {"sleep": 1.0},
             read,
             send("turn/start", {"sessionId": "$S1", "message": [{"type": "text", "text": "Refused"}]}),
             exact(send("session/turn/queue/resume", {"sessionId": "$S1"})),
             {"await": True},
             read,
             exact(send("session/turn/queue/resume", {"sessionId": "$S1"})),
         ]},
        {"name": "flows/turn-queue-refusals",
         "backend": [{"text": "Keyed answer."}, {"text": "Context answer."}],
         "steps": [
             INIT, START,
             exact(send("session/turn/enqueue", {"sessionId": "$S1", "idempotencyKey": "k1",
                                                 "entries": [user_entry("Keyed")]})),
             {"await": True},
             exact(send("session/turn/enqueue", {"sessionId": "$S1", "idempotencyKey": "k1",
                                                 "entries": [user_entry("Keyed")]})),
             exact(send("session/turn/enqueue", {"sessionId": "$S1", "idempotencyKey": "k1",
                                                 "entries": [user_entry("Different")]})),
             exact(send("session/turn/queue/replace", {"sessionId": "$S1", "queueItemId": "no-such-item",
                                                       "entries": [user_entry("Nothing")]})),
             exact(send("session/turn/queue/remove", {"sessionId": "$S1", "queueItemId": "no-such-item"})),
             send("session/turn/enqueue", {"sessionId": "$S1", "entries": [
                 user_entry("One"), user_entry("Two")]}),
             send("session/turn/enqueue", {"sessionId": "$S1", "entries": [
                 user_entry("First"), context_entry("After")]}),
             send("session/turn/enqueue", {"sessionId": "$S1", "entries": []}),
             exact(send("session/turn/enqueue", {"sessionId": "$S1", "entries": [context_entry("Only context")]})),
             {"sleep": 1.0},
             read,
             send("session/turn/queue/steer", {"sessionId": "$S1", "queueItemId": "$Q1", "expectedTurnId": "x"}),
         ]},
    ]


def session_flows() -> list[dict[str, Any]]:
    """The session methods a probe only reaches the validation of, driven
    with parameters that pass it."""
    def on(method: str, **params: Any) -> dict[str, Any]:
        return send(method, {"sessionId": "$S1", **params})

    return [
        {"name": "flows/session-mutations",
         "steps": [
             INIT, START,
             on("session/rename", title="Renamed"),
             on("session/settings/update", maxTurns=5),
             on("session/settings/update", maxTokens=1000, maxTurns=None),
             on("session/agent/update", agentName="plan"),
             on("session/agent/update", agentName="nope"),
             on("agents/install", agentName="nope"),
             on("agents/install", agentName="lean"),
             on("agents/uninstall", agentName="lean"),
             on("feedback/record", action="asked"),
             on("feedback/record", action="snoozed"),
             on("telemetry/record", name="oracle.event", properties={"k": 1}),
         ]},
        {"name": "flows/workspace-prompt",
         "files": {"notes.txt": "one\n", "shot.png": "not an encoded image\n"},
         # The image is snapshot into the session's directory, which exists
         # once a turn has been written.
         "backend": [{"text": "Noted."}],
         "steps": [
             INIT, START,
             on("workspace/prompt/prepare", message="hello there"),
             on("workspace/prompt/prepare", message="read @notes.txt please"),
             {"turn": "hi"},
             on("workspace/prompt/prepare", message="see @shot.png and @shot.png"),
             on("workspace/prompt/prepare", message="",
                titleContent=[{"type": "text", "text": "Title text"}]),
         ]},
        {"name": "flows/context-inject",
         "steps": [
             INIT, START,
             on("session/context/inject", input=[{"type": "text", "text": "ctx"}]),
             on("session/context/inject", input=[{"type": "text", "text": "msg"}], asMessage=True),
             on("session/context/inject", input=[{"type": "text", "text": "again"}], asMessage=True,
                clientUserMessageId="client-1"),
             on("session/history/get"),
         ]},
        {"name": "flows/loops",
         "steps": [
             INIT, START,
             on("loops/create", interval="5m", prompt="ping"),
             on("loops/create", interval="soon", prompt="ping"),
             on("loops/list"),
             on("loops/delete", loopId="nope"),
         ]},
        {"name": "flows/config-writes",
         "steps": [
             INIT, START,
             on("config/proxy/write", changes={"HTTP_PROXY": "http://proxy.invalid:8080"}),
             on("config/proxy/read"),
             on("config/proxy/write", changes={"HTTP_PROXY": None}),
             on("config/write", ops=[{"op": "set", "path": "/theme", "value": "dracula"}]),
             on("config/write", ops=[{"op": "set", "path": "/theme", "value": 3}]),
             on("config/write", ops=[{"op": "remove", "path": "/theme"}], reason="oracle"),
             on("config/model/write", reasoningEffort="low"),
             on("config/model/write", modelAlias="nope"),
             on("config/reload"),
         ]},
        {"name": "flows/history-get",
         "backend": [{"text": "Answer."}],
         "steps": [
             INIT, START,
             on("session/history/get"),
             {"turn": "Question"},
             on("session/history/get"),
             on("session/history/get", historyLimit=1),
         ]},
        {"name": "flows/callback-result",
         "steps": [
             INIT, START,
             on("callback/result", result={"callbackId": "nope", "output": {"x": 1}}),
             on("callback/result", result={"callbackId": "nope",
                                           "output": {"type": "approval", "approved": True}}),
             on("callback/result", result={"callbackId": "nope",
                                           "error": {"message": "refused"}}),
         ]},
        {"name": "flows/callback-approval",
         "backend": [
             {"text": "Writing.", "toolCalls": [{"id": "call_write", "name": "write_file",
                                                 "arguments": {"file_path": "out.txt", "content": "x\n"}}]},
             {"text": "Written."},
         ],
         "steps": [
             {"init": True, "params": {
                 "clientInfo": {"name": "protocol-oracle", "version": "0"},
                 "capabilities": {"callbackKinds": ["approval", "user_input"]}}},
             {"start": {"agentConfig": {"cwd": "$WS", "agent": "ask"}}},
             send("turn/start", {"sessionId": "$S1", "message": [{"type": "text", "text": "Write"}]}),
             {"sleep": 1.0},
             {"ack": {"result": {"callbackId": "$C1", "accepted": True}}},
             on("callback/result", result={"callbackId": "$C1",
                                           "output": {"type": "user_input", "result": {"answers": []}}}),
             on("callback/result", result={"callbackId": "$C1",
                                           "output": {"type": "approval", "decision": {"type": "approve"}}}),
             {"await": True},
             on("callback/result", result={"callbackId": "$C1",
                                           "output": {"type": "approval", "decision": {"type": "approve"}}}),
             on("callback/result", result={"callbackId": "$C1",
                                           "output": {"type": "approval", "decision": {"type": "deny"}}}),
         ]},
        {"name": "flows/callback-reject",
         "backend": [
             {"text": "Writing.", "toolCalls": [{"id": "call_write", "name": "write_file",
                                                 "arguments": {"file_path": "out.txt", "content": "x\n"}}]},
             {"text": "Refused."},
         ],
         "steps": [
             {"init": True, "params": {
                 "clientInfo": {"name": "protocol-oracle", "version": "0"},
                 "capabilities": {"callbackKinds": ["approval", "user_input"]}}},
             {"start": {"agentConfig": {"cwd": "$WS", "agent": "ask"}}},
             send("turn/start", {"sessionId": "$S1", "message": [{"type": "text", "text": "Write"}]}),
             {"sleep": 1.0},
             {"ack": {"result": {"callbackId": "$C1", "accepted": True}}},
             on("callback/result", result={"callbackId": "$C1", "error": {"message": "refused"}}),
             {"await": True},
             on("callback/result", result={"callbackId": "$C1", "error": {"message": "refused"}}),
         ]},
        {"name": "flows/callback-refused-delivery",
         "backend": [
             {"text": "Writing.", "toolCalls": [{"id": "call_write", "name": "write_file",
                                                 "arguments": {"file_path": "out.txt", "content": "x\n"}}]},
             {"text": "Refused."},
         ],
         "steps": [
             {"init": True, "params": {
                 "clientInfo": {"name": "protocol-oracle", "version": "0"},
                 "capabilities": {"callbackKinds": ["approval", "user_input"]}}},
             {"start": {"agentConfig": {"cwd": "$WS", "agent": "ask"}}},
             send("turn/start", {"sessionId": "$S1", "message": [{"type": "text", "text": "Write"}]}),
             {"sleep": 1.0},
             {"ack": {"result": {"callbackId": "$C1", "accepted": False}}},
             {"await": True},
         ]},
        {"name": "flows/narration",
         "backend": [{"text": "Narrated summary."}],
         "steps": [
             INIT, START,
             on("narration/summarize", userMessage="hi", assistantText="hello"),
         ]},
        {"name": "flows/shell-command",
         "steps": [
             INIT, START,
             on("session/shellCommand", command="echo hi"),
             on("session/shellCommand", command="echo out; echo err >&2; exit 3"),
             on("session/shellCommand", command="sleep 3", timeoutSeconds=0.5),
             on("session/shellCommand", action="interrupt", operationId="nope"),
             on("session/shellCommand", action="interrupt"),
             on("session/shellCommand"),
             on("session/shellCommand", command="   "),
             on("session/shellCommand", command="pwd", timeoutSeconds=0),
             on("session/shellCommand", command="pwd", cwd="$WS/missing"),
             on("session/shellCommand", command="pwd", cwd="/"),
             on("session/shellCommand", command="pwd", cwd="$WS"),
             {"post": {"method": "session/shellCommand",
                       "params": {"sessionId": "$S1", "command": "sleep 5", "operationId": "op-1"}}},
             on("session/shellCommand", command="echo again", operationId="op-1"),
             on("session/shellCommand", action="interrupt", operationId="op-1"),
             {"sleep": 1.0},
             on("session/history/get"),
         ]},
        {"name": "flows/connector-catalog",
         "config": '[[connectors]]\nname = "Drive"\ndisabled_tools = ["list"]\n\n[[connectors]]\nname = "Ghost"\ndisabled = true\n\n[[connectors]]\nname = "Mail_box"\n',
         "gets": {"/v1/connectors/bootstrap": {"connectors": [
             {"id": "c-drive", "name": "Drive", "display_name": "Google Drive", "protocol": "mcp",
              "status": {"is_ready": True},
              "tools": [{"name": "search", "description": "Search files", "inputSchema": {"type": "object"}},
                        {"name": "list"}]},
             {"id": "c-mail", "name": "Mail box", "status": {"is_ready": False},
              "auth_action": {"type": "oauth"}, "bootstrap_errors": ["token_expired: gone"]},
             {"id": "c-bad", "tools": "nope"},
         ]}},
         "steps": [
             INIT,
             send("connector_catalog/read"),
             send("connector_catalog/refresh"),
             send("connector_catalog/read"),
             START,
             on("connector_catalog/read"),
             on("connector_catalog/refresh"),
             on("connector_catalog/read"),
             on("connectors/read"),
             send("connector_catalog/toggle", {"alias": "Drive", "disabled": False, "toolName": "list"}),
             on("connector_catalog/toggle", alias="Drive", disabled=False, toolName="list"),
             on("connector_catalog/toggle", alias="Nope", disabled=True),
             on("connector_catalog/toggle", alias="Bad name", disabled=True),
             on("connector_catalog/toggle", alias="Drive", disabled=True, toolName=" "),
             send("connector_catalog/toggle", {"alias": "Other", "disabled": True}),
             on("connector_catalog/auth/request", alias="Drive"),
             on("connector_catalog/auth/request", alias="Nope"),
             on("connector_catalog/auth/request", alias="Mail_box"),
             on("connectors/auth/read", name="Mail_box"),
             on("connectors/refresh", name="Drive"),
             on("connectors/refresh", name="Nope"),
             on("connector_catalog/read"),
         ]},
        {"name": "flows/skills-registry",
         "gets": {
             "/v1/skills/sk-1": {
                 "skillId": "sk-1", "version": 2,
                 "skill": {"skillName": "Demo Skill", "skillDescription": "Runs the demo",
                           "skillBody": "Run the demo.\n"},
                 "metadata": {"name": "demo", "latestVersion": 3, "sharingScope": "org",
                              "createdAt": "2026-01-01T00:00:00Z", "createdBy": "someone",
                              "lastModifiedAt": "2026-01-02T00:00:00Z"},
                 "versionMetadata": {"createdAt": "2026-01-01T00:00:00Z"},
                 "versionAttributes": {"aliases": ["stable"], "notes": "first"}},
             "/v1/skills/sk-1/versions": {"items": [
                 {"version": 1}, {"version": 2, "versionAttributes": {"aliases": ["stable"]}}]},
             "/v1/skills/sk-empty": {"skillId": "sk-empty", "version": 1,
                                     "metadata": {"name": "hollow"}},
         },
         "steps": [
             INIT, START,
             on("skills/versions", skillId="sk-1"),
             on("skills/versions", skillId="sk-missing"),
             on("skills/detail", skillId="sk-1"),
             on("skills/detail", skillId="sk-missing", version=4),
             on("skills/import", skillId="sk-empty"),
             on("skills/import", skillId="sk-missing"),
             on("skills/import", skillId="sk-1", scope="project"),
             on("skills/import", skillId="sk-1"),
             on("skills/installed"),
             on("skills/setAlias", name="demo", alias="stable"),
             on("skills/setVersion", name="demo", version=2),
             on("skills/setLatest", name="demo"),
             on("skills/setLatest", name="ghost"),
             on("skills/setEnabled", name="ghost", enabled=False),
             on("skills/remove", name="ghost"),
             on("skills/convertLocal", name="ghost"),
             on("skills/convertLocal", name="demo"),
             on("skills/installed"),
             on("skills/setEnabled", name="demo", enabled=False),
             on("skills/setEnabled", name="demo", enabled=True),
             on("skills/remove", name="demo"),
             on("skills/import", skillId="sk-1", alias="stable"),
             on("skills/remove", name="demo"),
         ]},
        {"name": "flows/git-checkouts",
         "git": True,
         "remote": "git@gitlab.example.com:group/sub/demo.git",
         "files": {"nested/keep.txt": "x\n"},
         "steps": [
             INIT,
             send("workspace/git/checkouts", {"repoLocalPaths": ["$WS", "$ROOT/missing"]}),
             send("workspace/git/checkouts",
                  {"repoLocalPaths": ["$WS", "$ROOT"], "sessionCwd": "$WS/nested"}),
             send("workspace/git/checkouts", {"repoLocalPaths": []}),
             send("workspace/git/checkouts", {"repoLocalPaths": "$WS"}),
         ]},
        {"name": "flows/setup",
         "config": '\n[[providers]]\nname = "other"\napi_base = "https://other.example.invalid/v1"\n'
                   'api_key_env_var = "OTHER_ORACLE_KEY"\n',
         "steps": [
             INIT,
             exact(send("setup/status")),
             exact(send("setup/status", {"provider": "other"})),
             send("setup/status", {"provider": "nope"}),
             send("setup/status", {"provider": "other", "bogus": 1}),
             exact(send("setup/store-credential", {"provider": "other", "apiKey": "oracle-stored"})),
             exact(send("setup/status", {"provider": "other"})),
             send("setup/store-credential", {"provider": "nope", "apiKey": "x"}),
             send("setup/store-credential", {"provider": "other"}),
             exact(send("setup/submit-choices")),
             exact(send("setup/submit-choices", {"theme": "dracula"})),
             exact(send("setup/status")),
             exact(send("setup/submit-choices", {
                 "provider": {"name": "other", "apiBase": "https://moved.example.invalid/v1",
                              "apiKeyEnvVar": "OTHER_ORACLE_KEY"},
                 "consoleBaseUrl": "https://console.example.invalid"})),
             exact(send("setup/status", {"provider": "other"})),
             exact(send("setup/submit-choices", {"provider": {"name": "ghost", "apiBase": "https://g.invalid"}})),
             send("setup/submit-choices", {"provider": {"name": "other"}}),
         ]},
        {"name": "flows/worktree-reap",
         "git": True,
         "steps": [
             INIT,
             {"start": {"agentConfig": {"cwd": "$WS", "agent": "auto-approve",
                                        "worktree": {"kind": "create", "branch": "oracle-reap",
                                                     "name": "oracle-reap"}}}},
             exact(send("workspace/git/worktrees/list", {"cwd": "$WS"})),
             exact(send("workspace/git/worktrees/remove", {"cwd": "$W1", "inspect": True})),
             send("workspace/git/worktrees/reap", {"cwd": "$W1", "requesterId": "r1"}),
             exact(send("workspace/git/worktrees/reap/cancel", {"cwd": "$W1", "requestId": "q1"})),
             exact(send("workspace/git/worktrees/reap",
                        {"cwd": "$W1", "requesterId": "r1", "requestId": "q1"})),
             exact(send("workspace/git/worktrees/reap/cancel",
                        {"cwd": "$W1", "requesterId": "r1", "requestId": "q1"})),
             exact(send("workspace/git/worktrees/reap",
                        {"cwd": "$W1", "requesterId": "r2", "requestId": "q2"})),
             exact(send("workspace/git/worktrees/list", {"cwd": "$WS"})),
             exact(send("session/stop", {"sessionId": "$S1"})),
             {"restart": True},
             INIT,
             exact(send("workspace/git/worktrees/list", {"cwd": "$WS"})),
             exact(send("workspace/git/worktrees/reap", {"cwd": "$WS/missing"})),
         ]},
        {"name": "flows/worktree-reap-idle",
         "git": True,
         "steps": [
             INIT,
             {"start": {"agentConfig": {"cwd": "$WS", "agent": "auto-approve",
                                        "worktree": {"kind": "create", "branch": "oracle-idle",
                                                     "name": "oracle-idle"}}}},
             exact(send("workspace/git/worktrees/list", {"cwd": "$WS"})),
             exact(send("session/stop", {"sessionId": "$S1"})),
             {"restart": True},
             INIT,
             exact(send("workspace/git/worktrees/remove", {"cwd": "$W1", "inspect": True})),
             exact(send("workspace/git/worktrees/reap/cancel", {"cwd": "$W1"})),
             exact(send("workspace/git/worktrees/reap", {"cwd": "$W1"})),
             exact(send("workspace/git/worktrees/list", {"cwd": "$WS"})),
         ]},
        {"name": "flows/worktree-reap-dirty",
         "git": True,
         "steps": [
             INIT,
             {"start": {"agentConfig": {"cwd": "$WS", "agent": "auto-approve",
                                        "worktree": {"kind": "create", "branch": "oracle-dirty",
                                                     "name": "oracle-dirty"}}}},
             exact(send("workspace/git/worktrees/list", {"cwd": "$WS"})),
             {"write": {"draft.txt": "unsaved\n"}, "into": "$W1"},
             exact(send("workspace/git/worktrees/remove", {"cwd": "$W1", "inspect": True})),
             exact(send("workspace/git/worktrees/reap",
                        {"cwd": "$W1", "requesterId": "r1", "requestId": "q1"})),
             exact(send("session/stop", {"sessionId": "$S1"})),
             {"restart": True},
             INIT,
             exact(send("workspace/git/worktrees/list", {"cwd": "$WS"})),
             exact(send("workspace/git/worktrees/remove", {"cwd": "$W1", "inspect": True})),
             exact(send("workspace/git/worktrees/reap/cancel", {"cwd": "$W1", "requesterId": "r1"})),
             exact(send("workspace/git/worktrees/reap", {"cwd": "$W1"})),
             exact(send("workspace/git/worktrees/list", {"cwd": "$WS"})),
             exact(send("workspace/git/worktrees/reap", {"cwd": "$W1"})),
         ]},
        {"name": "flows/session-stop",
         "steps": [
             INIT, START,
             on("session/stop", reason="done"),
             on("session/read"),
         ]},
        {"name": "flows/session-stop-unknown",
         "steps": [
             INIT, START,
             send("session/stop", {"sessionId": "nope"}),
             on("session/read"),
         ]},
        {"name": "flows/session-stop-unattached",
         "steps": [
             INIT,
             send("session/stop", {"sessionId": "nope"}),
             send("session/list", {"cwd": "/probe-empty"}),
         ]},
    ]


def start_params_placeholder() -> dict[str, Any]:
    return {"agentConfig": {"cwd": "$WS", "agent": "auto-approve"}}
