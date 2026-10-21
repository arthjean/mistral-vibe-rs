"""What a black-box capture of subagents and callbacks drives the app server with.

It builds on :mod:`app_server_client`: a server whose callbacks a scenario
answers from a script (:class:`Server`); a session that also learns the child
sessions a parent linked (:class:`Session`); the conversation a request sent
(:func:`transcript`); and a normalizer that also maps the identifiers a hook
invocation carries and a subagent's log directory (:class:`Normalizer`).
Nothing here imports the reference.
"""

from __future__ import annotations

from pathlib import Path
import re
from typing import Any

import app_server_client


# --------------------------------------------------------------------------
# One server process
# --------------------------------------------------------------------------


class Server(app_server_client.Server):
    """A server whose callbacks the scenario answers from a script.

    A ``callback/call`` is acknowledged at once and answered with the next
    scripted output, the last one repeating, so an approval or a question the
    session raises settles without a step of its own.
    """

    def __init__(self, *args: Any, callbacks: list[dict[str, Any]], **kwargs: Any) -> None:
        self.callbacks = callbacks
        self.answered_callbacks = 0
        self.callback_log: list[dict[str, Any]] = []
        super().__init__(*args, **kwargs)

    def _answer(self, request: dict[str, Any]) -> None:
        if request.get("method") != "callback/call":
            super()._answer(request)
            return
        params = request.get("params") or {}
        self.callback_log.append(params)
        callback_id = (params.get("callback") or {}).get("callbackId")
        self.send({"jsonrpc": "2.0", "id": request["id"],
                   "result": {"callbackId": callback_id, "accepted": True}})
        if not self.callbacks:
            return
        script = self.callbacks[min(self.answered_callbacks, len(self.callbacks) - 1)]
        self.answered_callbacks += 1
        self.send({"jsonrpc": "2.0", "id": f"callback-{self.answered_callbacks}",
                   "method": "callback/result",
                   "params": {
                       "sessionId": (params.get("callback") or {}).get("sessionId"),
                       "result": {"callbackId": callback_id, **callback_output(script, params)},
                   }})


def callback_output(script: dict[str, Any], params: dict[str, Any]) -> dict[str, Any]:
    """The answer a scripted client gives one callback.

    ``approve`` and ``deny`` settle an approval; ``choose`` answers every
    question of a user input callback with the option carrying that label.
    """

    if "approve" in script:
        decision = "approve" if script["approve"] else "deny"
        return {"output": {"type": "approval", "decision": {"type": decision}}}
    if "choose" in script:
        detail = (params.get("callback") or {}).get("detail") or {}
        questions = (detail.get("request") or {}).get("questions") or []
        return {"output": {"type": "user_input", "result": {
            "answers": [
                {"question": question.get("question", ""), "answer": script["choose"],
                 "isOther": False}
                for question in questions
            ],
            "cancelled": False,
        }}}
    return script


def response_to(observed: list[dict[str, Any]], identifier: int) -> dict[str, Any] | None:
    return next(
        (m for m in observed if "method" not in m and m.get("id") == identifier), None
    )


# --------------------------------------------------------------------------
# What a scenario learned
# --------------------------------------------------------------------------


class Session(app_server_client.Session):
    """What a scenario learned, plus the child sessions a parent linked."""

    def __init__(self, root: Path) -> None:
        super().__init__(root)
        self.children: list[str] = []

    def learn(self, message: dict[str, Any]) -> None:
        super().learn(message)
        for operation in (message.get("params") or {}).get("patch") or []:
            if (
                isinstance(operation, dict)
                and operation.get("path") == "/detail/childSessionId"
                and isinstance(operation.get("value"), str)
                and operation["value"] not in self.children
            ):
                self.children.append(operation["value"])

    def substitute(self, value: Any) -> Any:
        value = super().substitute(value)
        if isinstance(value, str):
            for index in range(len(self.children), 0, -1):
                value = value.replace(f"$K{index}", self.children[index - 1])
        elif isinstance(value, dict):
            return {key: self.substitute(item) for key, item in value.items()}
        elif isinstance(value, list):
            return [self.substitute(item) for item in value]
        return value


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


def transcript(messages: list[Any]) -> list[dict[str, Any]]:
    """The conversation a request sends, the system prompt aside."""

    turns: list[dict[str, Any]] = []
    for message in messages:
        if not isinstance(message, dict) or message.get("role") == "system":
            continue
        content = message.get("content")
        if isinstance(content, list):
            content = "".join(
                part.get("text", "") for part in content if isinstance(part, dict)
            )
        entry: dict[str, Any] = {"role": message.get("role"), "content": content or ""}
        if message.get("tool_calls"):
            # A request nests the call under `function`; the port's own
            # `messages.jsonl` does not, a layout row 16 answers for.
            entry["toolCalls"] = [
                {
                    "id": call.get("id"),
                    "name": call.get("function", call).get("name"),
                    "arguments": call.get("function", call).get("arguments"),
                }
                for call in message["tool_calls"]
            ]
        if message.get("tool_call_id") or message.get("call_id"):
            entry["toolCallId"] = message.get("tool_call_id") or message.get("call_id")
        turns.append(entry)
    return turns


#: A subagent's log directory under its parent's, ending in eight hex digits
#: of its identifier.
CHILD_DIRECTORY = re.compile(r"(/agents/[A-Za-z0-9_-]+_<time>_)[0-9a-f]{8}")


class Normalizer(app_server_client.Normalizer):
    def text(self, value: str) -> str:
        return CHILD_DIRECTORY.sub(r"\1<short>", super().text(value))

    def value(self, value: Any, key: str | None = None) -> Any:
        if key == "duration_ms" and isinstance(value, int | float):
            return "<duration>"
        if key in HOOK_ID_KEYS and isinstance(value, str) and value:
            if value in self.authored:
                return value
            return self.identity(value)
        if isinstance(value, str) and value.startswith(TOML_FAILURE):
            # The TOML reader's own words are the library's, not the hook
            # contract's (the ledger row on library error text).
            return TOML_FAILURE + "<toml-error>"
        return super().value(value, key)

    def collect_ids(self, value: Any, key: str | None = None) -> None:
        if key in HOOK_ID_KEYS and isinstance(value, str) and value:
            if value not in self.authored:
                self.identity(value)
            return
        if isinstance(value, dict):
            for name, item in sorted(value.items()):
                self.collect_ids(item, name)
            return
        super().collect_ids(value, key)


#: The identifiers a hook invocation carries under its own snake_case keys.
HOOK_ID_KEYS = {"tool_call_id", "session_id", "parent_session_id"}

#: How a hook file that is not TOML is reported, before the reader's message.
TOML_FAILURE = "Failed to parse: "
