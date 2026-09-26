#!/usr/bin/env python3
"""Capture how the pinned reference validates app-server request parameters.

The reference validates every request's parameters with pydantic before it
does anything else, and a client reads the outcome: an ``invalid_params``
error whose ``data`` lists every issue with the path to the offending value.
Reproducing that answer means reproducing the validation, so this script
records what pydantic-core validates each parameter model with: the core
schema, reduced to the structure a validator walks.

Every node keeps only its kind, the aliases and field names it reads, its
numeric and literal constraints, enum values, discriminators and the name of
any custom validator it runs. Descriptions, titles and every other string the
reference authored are dropped, so the file carries names, numbers and
literal values only, as the app-server census does, which is what lets it be
committed under ``NOTICE``. Unlike the census it is read by production code:
``crates/vibe-app-server/src/wire_validation.rs`` walks it to answer each
request the way pydantic-core would.

Model definitions are keyed by class name, so a recursive or shared model is
recorded once and referenced; the recursive ``JsonValue`` alias, which accepts
anything a JSON parser produces, is recorded as its own node kind.

Usage::

    scripts/parity/app_server_params.py --reference /path/to/reference
    scripts/parity/app_server_params.py --check

The checkout must sit on the pinned commit; the script re-executes itself
under the reference interpreter when the current one cannot import ``vibe``.
"""

from __future__ import annotations

import argparse
import enum
import json
import os
from pathlib import Path
import subprocess
import sys
from typing import Any

from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-app-server/src/wire_validation/schema.json"
DEFAULT_CASES = REPOSITORY / "crates/vibe-app-server/tests/wire-validation/cases.json"
#: The values every field is tried with, alone, on top of a valid input.
PROBE_VALUES: list[Any] = [None, 0, 1, -1, 1.5, True, "", "x", " 5 ", "true", [], ["x"], [1], {}, {"a": 1}]
#: The values a nested field is tried with.
NESTED_VALUES: list[Any] = [None, 1, "x", True, [], {}]
CENSUS = REPOSITORY / "crates/vibe-app-server/tests/app-server-surface/corpus.json"
SCHEMA_VERSION = 1
#: Longest string the file may carry: names, aliases, patterns and literals
#: are all shorter, and anything longer would be prose.
MAX_CAPTURED_STRING = 120


class OracleError(RuntimeError):
    pass


def resolve_reference(reference: Path, expected: str) -> str:
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=reference, capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        raise OracleError(f"git rev-parse failed in {reference}: {result.stderr.strip()}")
    commit = result.stdout.strip()
    if commit != expected:
        raise OracleError(f"reference checkout is at {commit}, not the pinned {expected}")
    return commit


def reexecute(reference: Path) -> None:
    if str(reference) not in sys.path:
        sys.path.insert(0, str(reference))
    try:
        import vibe.app_server.protocol  # noqa: F401

        return
    except ImportError:
        pass
    for candidate in (reference / ".venv/bin/python", reference / ".venv/Scripts/python.exe"):
        if candidate.is_file():
            completed = subprocess.run(
                [str(candidate), __file__, *sys.argv[1:]],
                env={**os.environ, "PYTHONPATH": str(Path(__file__).resolve().parent)},
                check=False,
            )
            raise SystemExit(completed.returncode)
    raise OracleError(f"no interpreter can import vibe from {reference}")


class Reducer:
    """Walks pydantic-core schemas into the reduced node format."""

    def __init__(self) -> None:
        self.defs: dict[str, Any] = {}
        self.refs: dict[str, dict[str, Any]] = {}
        self.functions: set[str] = set()

    def reduce_model(self, model: type) -> str:
        model.model_rebuild()
        schema = model.__pydantic_core_schema__
        if schema.get("type") == "definitions":
            for definition in schema["definitions"]:
                self.refs[definition["ref"]] = definition
            schema = schema["schema"]
        node = self.node(schema)
        if node.get("type") == "ref":
            return node["name"]
        name = model.__name__
        self.defs.setdefault(name, node)
        return name

    def node(self, schema: dict[str, Any]) -> dict[str, Any]:
        kind = schema["type"]
        if "ref" in schema and kind != "definition-ref":
            self.refs.setdefault(schema["ref"], schema)
        if kind == "definition-ref":
            target = self.refs.get(schema["schema_ref"])
            if target is None:
                raise OracleError(f"unresolved reference {schema['schema_ref']}")
            return self.node({k: v for k, v in target.items() if k != "ref"} | {"ref": target.get("ref")})
        if kind == "json-or-python":
            if schema.get("python_schema", {}).get("custom_error_type") == "invalid-json-value":
                return {"type": "json-value"}
            return self.node(schema["python_schema"])
        if kind == "model":
            return self.model(schema)
        if kind in {"function-after", "function-before", "function-wrap", "function-plain"}:
            function = schema["function"]
            callable_ = function.get("function") if isinstance(function, dict) else function
            name = getattr(callable_, "__qualname__", None)
            if not isinstance(name, str):
                raise OracleError(f"unnamed validator in {kind}")
            self.functions.add(name)
            reduced: dict[str, Any] = {"type": kind, "function": name}
            if "schema" in schema:
                reduced["schema"] = self.node(schema["schema"])
            inner = reduced.get("schema", {})
            # A model validator wraps the model's own schema; it belongs to
            # the model wherever the model is referenced.
            owner = self.model_class(schema.get("schema", {}))
            owners = {base.__qualname__ for base in owner.__mro__} if owner else set()
            if inner.get("type") == "ref" and name.rpartition(".")[0] in owners:
                model = self.defs[inner["name"]]
                if model is not None:
                    validators = model.setdefault("validators", [])
                    if not any(entry["function"] == name for entry in validators):
                        validators.append({"mode": kind, "function": name})
                return inner
            return reduced
        if kind == "default":
            return {"type": "default", "schema": self.node(schema["schema"])}
        if kind == "nullable":
            return {"type": "nullable", "schema": self.node(schema["schema"])}
        if kind == "list":
            reduced = {"type": "list", "items": self.node(schema.get("items_schema", {"type": "any"}))}
            return reduced | constraints(schema, ("min_length", "max_length", "strict"))
        if kind == "dict":
            return {
                "type": "dict",
                "keys": self.node(schema.get("keys_schema", {"type": "any"})),
                "values": self.node(schema.get("values_schema", {"type": "any"})),
            } | constraints(schema, ("min_length", "max_length", "strict"))
        if kind == "tagged-union":
            discriminator = schema["discriminator"]
            if callable(discriminator):
                discriminator = {"function": discriminator.__qualname__}
            return {
                "type": "tagged-union",
                "discriminator": discriminator,
                "choices": [[str(tag), self.node(choice)] for tag, choice in schema["choices"].items()],
            } | constraints(schema, ("strict", "from_attributes", "custom_error_type"))
        if kind == "literal":
            return {"type": "literal", "expected": [plain(value) for value in schema["expected"]]}
        if kind == "enum":
            return {
                "type": "enum",
                "name": schema["cls"].__name__,
                "values": [plain(member.value) for member in schema["members"]],
            } | constraints(schema, ("sub_type", "strict"))
        if kind in {"str", "int", "float", "bool", "none", "any"}:
            return {"type": kind} | constraints(
                schema,
                ("strict", "ge", "gt", "le", "lt", "min_length", "max_length", "pattern",
                 "allow_inf_nan", "multiple_of", "strip_whitespace"),
            )
        raise OracleError(f"no reduction for schema kind {kind}")

    def model_class(self, schema: dict[str, Any]) -> type | None:
        """The model class a schema validates, through references."""
        if schema.get("type") == "definition-ref":
            schema = self.refs.get(schema["schema_ref"], {})
        return schema["cls"] if schema.get("type") == "model" else None

    def model(self, schema: dict[str, Any]) -> dict[str, Any]:
        name = schema["cls"].__name__
        ref = {"type": "ref", "name": name}
        if name in self.defs:
            return ref
        self.defs[name] = None  # a placeholder that stops recursion
        inner = schema["schema"]
        post = []
        while inner["type"].startswith("function-"):
            post.append(inner)
            inner = inner["schema"]
        if inner["type"] != "model-fields":
            raise OracleError(f"model {name} validates {inner['type']}")
        config = schema.get("config", {})
        fields = []
        for field_name, field in inner["fields"].items():
            alias = field.get("validation_alias", field_name)
            if not isinstance(alias, str):
                raise OracleError(f"{name}.{field_name} has a path alias")
            fields.append({"name": field_name, "alias": alias, "schema": self.node(field["schema"])})
        reduced: dict[str, Any] = {
            "type": "model",
            "name": name,
            "fields": fields,
            "extra": config.get("extra_fields_behavior", "ignore"),
            "byName": bool(config.get("validate_by_name", False)),
        }
        if config.get("allow_inf_nan") is False:
            reduced["allowInfNan"] = False
        for wrapper in reversed(post):
            function = wrapper["function"]
            callable_ = function.get("function") if isinstance(function, dict) else function
            qualified = callable_.__qualname__
            self.functions.add(qualified)
            reduced.setdefault("validators", []).append({"mode": wrapper["type"], "function": qualified})
        self.defs[name] = reduced
        return ref


def constraints(schema: dict[str, Any], keys: tuple[str, ...]) -> dict[str, Any]:
    return {key: plain(schema[key]) for key in keys if key in schema and schema[key] is not None}


def plain(value: Any) -> Any:
    if isinstance(value, enum.Enum):
        value = value.value
    if isinstance(value, str) and len(value) > MAX_CAPTURED_STRING:
        raise OracleError(f"refusing to capture a {len(value)}-character string")
    if isinstance(value, (str, int, float, bool)) or value is None:
        return value
    if hasattr(value, "pattern"):
        return plain(value.pattern)
    raise OracleError(f"cannot record {type(value).__name__} values")


def capture() -> dict[str, Any]:
    from vibe.app_server import protocol

    census = json.loads(CENSUS.read_text(encoding="utf-8"))
    reducer = Reducer()
    methods: dict[str, str | None] = {}
    for entry in census["methods"]:
        name = entry["params"]
        methods[entry["name"]] = reducer.reduce_model(getattr(protocol, name)) if name else None
    lifecycle = {"initialize": reducer.reduce_model(protocol.InitializeParams)}
    # Models a handler validates past its parameters: `callback/result` reads
    # its output as `CallbackOutput`, the field `CallbackRespondParams` holds
    # (vibe/app_server/_handler.py, `_callback_result`).
    nested = {"callback/result": reducer.reduce_model(protocol.CallbackRespondParams)}
    return {
        "schemaVersion": SCHEMA_VERSION,
        "note": (
            "Captured by scripts/parity/app_server_params.py from the pinned reference: the "
            "pydantic-core schema of every request parameter model, reduced to names, aliases, "
            "constraints, literal and enum values and validator names. No reference prose."
        ),
        "methods": dict(sorted(methods.items())),
        "lifecycle": lifecycle,
        "nested": nested,
        "functions": sorted(reducer.functions),
        "defs": dict(sorted(reducer.defs.items())),
    }


class Cases:
    """Inputs for every parameter model, and what pydantic answered each.

    An input is a valid one built from the reduced schema with one value
    replaced, at the top level or one model deep, so every constraint, every
    lax coercion and every nested path is exercised. What is recorded is the
    error type and location of each issue, in order, or the value pydantic
    produced: names and values only, never a message.
    """

    def __init__(self, defs: dict[str, Any]) -> None:
        self.defs = defs

    def minimal(self, node: dict[str, Any], depth: int = 0) -> Any:
        kind = node["type"]
        if depth > 6:
            return None
        if kind == "ref":
            return self.minimal(self.defs[node["name"]], depth + 1)
        if kind == "model":
            value: dict[str, Any] = {}
            for field in node["fields"]:
                if field["schema"]["type"] != "default":
                    value[field["alias"]] = self.minimal(field["schema"], depth + 1)
            return value
        if kind in {"default", "function-after", "function-before"}:
            return self.minimal(node["schema"], depth + 1)
        if kind == "nullable":
            return None
        if kind == "str":
            return "x" * max(1, int(node.get("min_length") or 1))
        if kind in {"int", "float"}:
            low = node.get("ge", node.get("gt"))
            if low is None:
                return 0 if node.get("le", node.get("lt", 1)) >= 0 else node.get("le")
            return int(low) + (1 if "gt" in node else 0)
        if kind == "bool":
            return False
        if kind == "literal":
            return node["expected"][0]
        if kind == "enum":
            return node["values"][0]
        if kind == "list":
            count = int(node.get("min_length") or 0)
            return [self.minimal(node["items"], depth + 1) for _ in range(count)]
        if kind == "dict":
            return {}
        if kind == "tagged-union":
            tag, choice = node["choices"][0]
            value = self.minimal(choice, depth + 1)
            if isinstance(value, dict) and isinstance(node["discriminator"], str):
                value[node["discriminator"]] = tag
            return value
        return None

    def nested_models(self, node: dict[str, Any]) -> list[tuple[Any, dict[str, Any]]]:
        """The model a field holds directly, through a list or a union arm,
        with the value that places a valid one there."""
        kind = node["type"]
        if kind in {"default", "nullable", "function-after", "function-before"}:
            return self.nested_models(node["schema"])
        if kind == "ref":
            model = self.defs[node["name"]]
            return [(self.minimal(model), model)]
        if kind == "list":
            return [([value], model) for value, model in self.nested_models(node["items"])]
        if kind == "tagged-union":
            found = []
            for tag, choice in node["choices"]:
                for value, model in self.nested_models(choice):
                    if isinstance(value, dict) and isinstance(node["discriminator"], str):
                        value = {**value, node["discriminator"]: tag}
                    found.append((value, model))
            return found
        return []

    def inputs(self, model: dict[str, Any]) -> list[Any]:
        base = self.minimal(model)
        found: list[Any] = [{}, base, {**base, "bogus": 1}, [], "x", None]
        for field in model["fields"]:
            if field["name"] != field["alias"]:
                found.append({**base, field["name"]: self.minimal(field["schema"])})
            for value in PROBE_VALUES:
                found.append({**base, field["alias"]: value})
            for placed, nested in self.nested_models(field["schema"])[:4]:
                found.append({**base, field["alias"]: placed})
                target = placed
                while isinstance(target, list):
                    target = target[0] if target else {}
                if not isinstance(target, dict):
                    continue
                for inner in nested["fields"]:
                    for value in NESTED_VALUES:
                        changed = json.loads(json.dumps(placed))
                        holder = changed
                        while isinstance(holder, list):
                            holder = holder[0]
                        holder[inner["alias"]] = value
                        found.append({**base, field["alias"]: changed})
                changed = json.loads(json.dumps(placed))
                holder = changed
                while isinstance(holder, list):
                    holder = holder[0]
                holder["bogus"] = 1
                found.append({**base, field["alias"]: changed})
        unique: list[Any] = []
        seen: set[str] = set()
        for value in found:
            key = json.dumps(value, sort_keys=False)
            if key not in seen:
                seen.add(key)
                unique.append(value)
        return unique


def outcome(model: type, value: Any) -> dict[str, Any]:
    from pydantic import ValidationError

    from vibe.app_server._model import validate_wire

    try:
        validated = validate_wire(model, value)
    except ValidationError as error:
        return {"errors": [[list(issue["loc"]), issue["type"]] for issue in error.errors()]}
    except (TypeError, ValueError) as error:
        return {"raised": type(error).__name__}
    return {"value": validated.model_dump(mode="json", by_alias=True, exclude_unset=True)}


def capture_cases(payload: dict[str, Any]) -> dict[str, Any]:
    from vibe.app_server import protocol

    cases = Cases(payload["defs"])
    models = sorted(
        {name for name in payload["methods"].values() if name}
        | set(payload["lifecycle"].values())
        | set(payload["nested"].values())
    )
    recorded = []
    for name in models:
        model = getattr(protocol, name)
        for value in cases.inputs(payload["defs"][name]):
            recorded.append({"model": name, "input": value, **outcome(model, value)})
    return {
        "schemaVersion": SCHEMA_VERSION,
        "note": (
            "Captured by scripts/parity/app_server_params.py: inputs for every request "
            "parameter model and the error types and locations pydantic answered, or the "
            "value it produced. No messages."
        ),
        "cases": recorded,
    }


def rendered(payload: dict[str, Any]) -> str:
    return json.dumps(payload, indent=1, sort_keys=False, ensure_ascii=False) + "\n"


def compact(payload: dict[str, Any]) -> str:
    """One case per line, which keeps a large corpus diffable."""
    lines = [json.dumps(case, ensure_ascii=False, sort_keys=False) for case in payload["cases"]]
    head = {key: value for key, value in payload.items() if key != "cases"}
    body = ",\n".join(lines)
    return json.dumps(head, ensure_ascii=False)[:-1] + ', "cases": [\n' + body + "\n]}\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=Path(os.environ.get("VIBE_REFERENCE", DEFAULT_REFERENCE)))
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--cases", type=Path, default=DEFAULT_CASES)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    arguments = parser.parse_args()
    try:
        commit = resolve_reference(arguments.reference, arguments.expected_commit)
        reexecute(arguments.reference)
        payload = capture() | {"reference": {"commit": commit}}
        cases = capture_cases(payload) | {"reference": {"commit": commit}}
        outputs = [(arguments.output, rendered(payload)), (arguments.cases, compact(cases))]
        if arguments.check:
            for path, text in outputs:
                if path.read_text(encoding="utf-8") != text:
                    raise OracleError(f"{path} differs from a fresh capture")
            print("the schema and the cases match the pinned reference")
            return 0
        for path, text in outputs:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
        print(
            f"wrote {len(payload['defs'])} models for {len(payload['methods'])} methods and "
            f"{len(cases['cases'])} cases"
        )
        return 0
    except OracleError as error:
        print(f"params capture: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
