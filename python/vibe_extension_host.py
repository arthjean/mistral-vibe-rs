#!/usr/bin/env python3
"""Minimal JSON Lines host for explicitly configured Python tool modules."""

from __future__ import annotations

import asyncio
import contextlib
import importlib.util
import inspect
import io
import json
import sys
import traceback
from pathlib import Path
from types import ModuleType
from typing import Any


MAX_ERROR_CHARS = 16_384
_modules: dict[str, ModuleType] = {}
_tools: dict[str, tuple[dict[str, Any], Any]] = {}


def _frame(request_id: int, kind: str, payload: Any = None, message: str = "") -> None:
    frame = {"id": request_id, "kind": kind, "payload": payload}
    if message:
        frame["message"] = message[:MAX_ERROR_CHARS]
    sys.stdout.write(json.dumps(frame, ensure_ascii=False, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def _load_module(path_text: str) -> ModuleType:
    path = Path(path_text).resolve(strict=True)
    key = str(path)
    if key in _modules:
        return _modules[key]
    module_name = f"vibe_extension_{abs(hash(key))}"
    spec = importlib.util.spec_from_file_location(module_name, path)
    if spec is None or spec.loader is None:
        raise ValueError(f"cannot load Python tool module: {path.name}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    _modules[key] = module
    return module


def _discover(paths: list[str]) -> list[dict[str, Any]]:
    discovered: list[dict[str, Any]] = []
    _tools.clear()
    for path in paths:
        module = _load_module(path)
        definitions = getattr(module, "TOOLS", None)
        if not isinstance(definitions, list):
            raise ValueError(f"{Path(path).name} must export a TOOLS list")
        for definition in definitions:
            if not isinstance(definition, dict):
                raise ValueError("tool definitions must be objects")
            public = {
                "name": definition["name"],
                "description": definition["description"],
                "inputSchema": definition["input_schema"],
                "outputSchema": definition.get("output_schema"),
                "permissions": definition.get("permissions", []),
            }
            callable_name = definition["callable"]
            function = getattr(module, callable_name)
            if public["name"] in _tools:
                raise ValueError(f"duplicate Python tool name: {public['name']}")
            _tools[public["name"]] = (public, function)
            discovered.append(public)
    discovered.sort(key=lambda tool: tool["name"])
    return discovered


async def _invoke(request_id: int, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    if name not in _tools:
        raise ValueError(f"unknown Python tool: {name}")
    _, function = _tools[name]
    captured = io.StringIO()
    with contextlib.redirect_stdout(captured):
        result = function(**arguments)
        if inspect.isawaitable(result):
            result = await result
    noise = captured.getvalue()
    if noise:
        sys.stderr.write(noise)
        sys.stderr.flush()
    if isinstance(result, dict) and "typed_result" in result:
        chunks = result.pop("chunks", [])
        for chunk in chunks:
            _frame(request_id, "chunk", str(chunk))
        return {
            "typedResult": result["typed_result"],
            "modelText": result.get("model_text", json.dumps(result["typed_result"])),
            "display": result.get("display"),
            "chunks": [],
        }
    return {
        "typedResult": result,
        "modelText": result if isinstance(result, str) else json.dumps(result),
        "display": None,
        "chunks": [],
    }


async def _dispatch(request: dict[str, Any]) -> bool:
    request_id = int(request["id"])
    method = request["method"]
    params = request.get("params", {})
    if method == "discover":
        _frame(request_id, "result", {"tools": _discover(params["modules"])})
        return True
    if method == "invoke":
        result = await _invoke(request_id, params["name"], params.get("arguments", {}))
        _frame(request_id, "result", result)
        return True
    if method == "shutdown":
        _frame(request_id, "result", {})
        return False
    raise ValueError(f"unknown method: {method}")


async def _main() -> None:
    running = True
    while running:
        line = await asyncio.to_thread(sys.stdin.buffer.readline)
        if not line:
            return
        request_id = 0
        try:
            request = json.loads(line)
            request_id = int(request.get("id", 0))
            running = await _dispatch(request)
        except Exception as error:
            detail = "".join(traceback.format_exception_only(type(error), error)).strip()
            _frame(request_id, "error", None, detail)


if __name__ == "__main__":
    asyncio.run(_main())
