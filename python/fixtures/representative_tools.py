"""Representative tools used to verify the external Python host contract."""

from __future__ import annotations

import asyncio
import math
from pathlib import Path
from typing import Any


_counter = 0
_upper = str.upper

OBJECT = {"type": "object", "properties": {}}
NUMBER = {"type": "number"}


def echo(value: Any) -> Any:
    return value


def add(left: int, right: int) -> int:
    return left + right


def state() -> int:
    global _counter
    _counter += 1
    return _counter


def stream(count: int) -> dict[str, Any]:
    return {
        "typed_result": count,
        "model_text": f"emitted {count} chunks",
        "chunks": [f"chunk-{index}" for index in range(count)],
    }


def read_path(path: str) -> dict[str, Any]:
    candidate = Path(path)
    return {"name": candidate.name, "exists": candidate.exists()}


def sqrt(value: float) -> float:
    return math.sqrt(value)


def reexported_upper(value: str) -> str:
    return _upper(value)


def typed_object(name: str, enabled: bool) -> dict[str, Any]:
    return {"name": name, "enabled": enabled}


def explode() -> None:
    raise RuntimeError("representative failure")


async def sleep(seconds: float) -> str:
    await asyncio.sleep(seconds)
    return "awake"


def tool(
    name: str,
    description: str,
    callable_name: str,
    properties: dict[str, Any],
    required: list[str],
    output_schema: dict[str, Any] | None = None,
    permissions: list[dict[str, str]] | None = None,
) -> dict[str, Any]:
    return {
        "name": name,
        "description": description,
        "callable": callable_name,
        "input_schema": {
            "type": "object",
            "properties": properties,
            "required": required,
        },
        "output_schema": output_schema,
        "permissions": permissions or [],
    }


TOOLS = [
    tool("python_echo", "Return a typed value.", "echo", {"value": {}}, ["value"]),
    tool(
        "python_add",
        "Add two integers.",
        "add",
        {"left": {"type": "integer"}, "right": {"type": "integer"}},
        ["left", "right"],
        NUMBER,
    ),
    tool("python_state", "Increment retained host state.", "state", {}, [], NUMBER),
    tool(
        "python_stream",
        "Emit bounded progress chunks.",
        "stream",
        {"count": {"type": "integer"}},
        ["count"],
        NUMBER,
    ),
    tool(
        "python_read_path",
        "Inspect a path after Rust permission approval.",
        "read_path",
        {"path": {"type": "string"}},
        ["path"],
        OBJECT,
        [{"kind": "read_argument", "argument": "path"}],
    ),
    tool(
        "python_sqrt",
        "Use a standard-library import.",
        "sqrt",
        {"value": {"type": "number"}},
        ["value"],
        NUMBER,
    ),
    tool(
        "python_reexport",
        "Invoke a re-exported callable.",
        "reexported_upper",
        {"value": {"type": "string"}},
        ["value"],
        {"type": "string"},
    ),
    tool(
        "python_typed",
        "Return a typed object.",
        "typed_object",
        {"name": {"type": "string"}, "enabled": {"type": "boolean"}},
        ["name", "enabled"],
        OBJECT,
    ),
    tool("python_explode", "Raise a bounded exception.", "explode", {}, []),
    tool(
        "python_sleep",
        "Exercise cancellation.",
        "sleep",
        {"seconds": {"type": "number"}},
        ["seconds"],
        {"type": "string"},
    ),
]
