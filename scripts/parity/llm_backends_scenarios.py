"""The cases ``llm_backends.py`` drives through the pinned reference.

A probe case and a completion case drive the utility model's availability probe
and one background completion against the scripted HTTP stand-in. Every
provider, model, message, response and key here is authored for the corpus;
none is read from the reference.
"""

from __future__ import annotations

from typing import Any

KEY_VARIABLE = "ORACLE_API_KEY"
#: The variable the Mistral client reads a key from when it is handed none.
CLIENT_KEY_VARIABLE = "MISTRAL_API_KEY"
KEY = "oracle-key"
SESSION_ID = "session-oracle"


# --------------------------------------------------------------------------
# Scenario vocabulary
# --------------------------------------------------------------------------


# --------------------------------------------------------------------------
# Response builders, one per dialect
# --------------------------------------------------------------------------


USAGE = {"prompt_tokens": 12, "completion_tokens": 5, "total_tokens": 17}


def chat_json(content: Any = "Hi there.", finish: str = "stop", usage: dict[str, Any] | None = None,
              calls: list[dict[str, Any]] | None = None, extra: dict[str, Any] | None = None) -> dict[str, Any]:
    message: dict[str, Any] = {"role": "assistant", "content": content}
    if calls:
        message["tool_calls"] = calls
    if extra:
        message.update(extra)
    return {
        "status": 200,
        "json": {
            "id": "cmpl-oracle",
            "object": "chat.completion",
            "created": 1_700_000_000,
            "model": "oracle-model",
            "choices": [{"index": 0, "message": message, "finish_reason": finish}],
            "usage": usage or USAGE,
        },
    }


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------


#: The two names the fast utility model is asked for under, in preference order
#: (reference ``FAST_MODEL_CANDIDATES``).
FAST_CANDIDATES = ("mistral-vibe-cli-fast", "mistral-small-latest")
#: The wall clock the availability cache reads, frozen so ages are exact.
PROBE_NOW = 1_800_000_000
#: The probe budget every probe case runs with unless it names another.
PROBE_BUDGET = 1.0


def provider_entry(name: str, api_base: str, key_variable: str, backend: str = "mistral") -> dict[str, Any]:
    entry: dict[str, Any] = {"name": name, "api_base": api_base, "api_key_env_var": key_variable,
                             "backend": backend}
    if backend == "generic":
        entry["api_style"] = "openai"
    return entry


OPENAI = provider_entry("openai", "https://api.openai.com/v1", "OPENAI_KEY", "generic")
PUBLIC = provider_entry("mistral", "https://api.mistral.ai/v1", KEY_VARIABLE)
LOCAL = provider_entry("mistral", "http://127.0.0.1:1/v1", "")
BIG = {"name": "big", "provider": "openai", "alias": "big"}
MEDIUM = {"name": "mistral-medium-latest", "provider": "mistral", "alias": "medium"}


def probe_case(name: str, *, rounds: list[dict[str, Any]], responses: list[dict[str, Any]] | None = None,
               providers: list[dict[str, Any]] | None = None, active: Any = BIG,
               allowed: list[str] | None = None, env: dict[str, str] | None = None,
               seed: list[dict[str, Any]] | None = None, raw_file: str | None = None,
               models: list[dict[str, Any]] | None = None,
               utility: dict[str, str] | None = None) -> dict[str, Any]:
    """One availability case: a cache seeded before the first round, then rounds.

    A seed entry is ``{"model", "available", "age"}``, stored under the key the
    case's Mistral provider and credential give that model, or ``{"raw",
    "value"}``, stored under a key of its own. A round may ``advance`` both
    clocks first, ``seed`` the file again (another process writing it), turn
    probing off with ``disabled``, pick its ``features`` and its ``budget``.
    """

    return {
        "name": name,
        "providers": providers if providers is not None
        else [OPENAI, provider_entry("mistral", "$BASE/v1", KEY_VARIABLE)],
        "active": active,
        "allowedModels": allowed or [],
        "models": models or [],
        "utilityModels": utility or {},
        "env": env if env is not None else {KEY_VARIABLE: KEY},
        "seed": seed or [],
        "rawFile": raw_file,
        "responses": responses or [],
        "rounds": [{"advance": 0, "features": ["title"], "disabled": False,
                    "budget": PROBE_BUDGET, "seed": [], **r} for r in rounds],
    }


def probe_cases() -> list[dict[str, Any]]:
    first, second = FAST_CANDIDATES
    served = chat_json("ok")
    once = [{}]

    def refusal(status: int) -> dict[str, Any]:
        return {"status": status, "json": {"message": f"status {status}"}}

    keyless = [OPENAI, provider_entry("mistral", "$BASE/v1", "")]
    return [
        probe_case("served-first", rounds=once, responses=[served]),
        probe_case("refused-first-served-second", rounds=once, responses=[refusal(404), served]),
        probe_case("refused-both", rounds=once, responses=[refusal(400), refusal(403)]),
        probe_case("unauthorized", rounds=once, responses=[refusal(401), refusal(401)]),
        probe_case("unprocessable", rounds=once, responses=[refusal(422), served]),
        probe_case("request-timeout-status", rounds=once, responses=[refusal(408), served]),
        probe_case("rate-limited", rounds=once, responses=[refusal(429), served]),
        probe_case("server-errors", rounds=[{}, {}, {"advance": 601}],
                   responses=[refusal(500), refusal(503), served]),
        probe_case("slow-first", rounds=[{}, {"advance": 599}, {"advance": 2}],
                   responses=[{**served, "delay": 3.0}, served]),
        probe_case("connection-refused", rounds=once,
                   providers=[OPENAI, provider_entry("mistral", "$CLOSED/v1", KEY_VARIABLE)]),
        probe_case("keyed-without-key", rounds=once, env={}),
        probe_case("keyless", rounds=once, providers=keyless, env={}, responses=[served]),
        probe_case("keyless-client-environment-key", rounds=once, providers=keyless,
                   env={CLIENT_KEY_VARIABLE: "oracle-client-key"}, responses=[served]),
        probe_case("keyless-refused", rounds=once, providers=keyless, env={},
                   responses=[refusal(404), refusal(404)]),
        probe_case("no-features", rounds=[{"features": []}]),
        probe_case("disabled", rounds=[{"disabled": True}, {}], responses=[served]),
        probe_case("no-mistral-provider", rounds=once, providers=[OPENAI]),
        probe_case("allowlist-second-only", rounds=once, allowed=[second], responses=[served]),
        probe_case("allowlist-excludes-both", rounds=once, allowed=["big"]),
        probe_case("allowlist-alias-only", rounds=once, allowed=["mistral-small"]),
        probe_case("cached-available", rounds=once,
                   seed=[{"model": first, "available": True, "age": 10}]),
        probe_case("cached-unavailable", rounds=once, responses=[served],
                   seed=[{"model": first, "available": False, "age": 10}]),
        probe_case("cached-both-unavailable", rounds=once,
                   seed=[{"model": first, "available": False, "age": 10},
                         {"model": second, "available": False, "age": 3_599}]),
        probe_case("stale-unavailable", rounds=once, responses=[served],
                   seed=[{"model": first, "available": False, "age": 3_600}]),
        probe_case("stale-available", rounds=once, responses=[refusal(404), served],
                   seed=[{"model": first, "available": True, "age": 604_800}]),
        probe_case("fresh-available-boundary", rounds=once,
                   seed=[{"model": first, "available": True, "age": 604_799}]),
        probe_case("second-cached-available", rounds=once, responses=[served],
                   seed=[{"model": second, "available": True, "age": 10}]),
        probe_case("prune-on-write", rounds=once, responses=[served], seed=[
            {"raw": "foreign-fresh", "value": {"available": True, "stored_at_timestamp": PROBE_NOW - 5}},
            {"raw": "foreign-stale", "value": {"available": False, "stored_at_timestamp": PROBE_NOW - 4_000}},
            {"raw": "foreign-extra", "value": {"available": True, "stored_at_timestamp": PROBE_NOW,
                                               "note": "kept as written"}},
            {"raw": "foreign-flag", "value": {"available": False, "stored_at_timestamp": True}},
            {"raw": "foreign-float", "value": {"available": True, "stored_at_timestamp": 1.5e9}},
            {"raw": "foreign-text", "value": "not an entry"},
            {"raw": "fördern", "value": {"available": True, "stored_at_timestamp": PROBE_NOW}},
        ]),
        probe_case("malformed-file", rounds=once, responses=[served], raw_file="{not json"),
        probe_case("array-file", rounds=once, responses=[served], raw_file="[1, 2]"),
        probe_case("malformed-entry", rounds=once, responses=[served], seed=[
            {"raw": "$FIRST", "value": {"available": "yes", "stored_at_timestamp": PROBE_NOW}},
        ]),
        probe_case("remembered-in-process", rounds=[{}, {}], responses=[served]),
        probe_case("external-write-within-a-minute", rounds=[
            {"features": []},
            {"seed": [{"model": first, "available": True, "age": 0}]},
        ], responses=[refusal(404), refusal(404)]),
        probe_case("external-write-after-a-minute", rounds=[
            {"features": []},
            {"advance": 61, "seed": [{"model": first, "available": True, "age": 0}]},
        ]),
        probe_case("unknown-active-model", rounds=once, active=None, responses=[served]),
        probe_case("active-on-mistral", rounds=once, active=MEDIUM, responses=[served]),
        probe_case("title-overridden", rounds=once, utility={"title": "big"}),
        probe_case("title-overridden-to-missing", rounds=once, utility={"title": "nope"},
                   responses=[served]),
        probe_case("smart-approve-overridden", rounds=once, utility={"smart_approve": "big"},
                   responses=[served]),
        probe_case("both-features-overridden", rounds=[{"features": ["title", "smart_approve"]}],
                   utility={"title": "big", "smart_approve": "big"}),
        probe_case("one-of-two-features-overridden", rounds=[{"features": ["smart_approve", "title"]}],
                   utility={"smart_approve": "big"}, responses=[served]),
        probe_case("override-cannot-resolve", rounds=once, active=None, utility={"title": "active"}),
    ]


#: The launch an attributed utility completion reports.
LAUNCH = {"agent_entrypoint": "cli", "agent_version": "9.9.9", "client_name": "vibe_cli",
          "client_version": "9.9.9", "terminal_emulator": "kitty"}
#: Authored here, so they may be recorded verbatim; the user content measures
#: characters, not bytes.
UTILITY_SYSTEM = "Answer with a short label."
UTILITY_USER = "Résumé of the café session 🚀"


def completion_case(name: str, *, feature: str | None = None, call_type: str | None = None,
                    launch: dict[str, Any] | None = None, session: str | None = None,
                    telemetry: bool = True, skip_if_no_key: bool = False,
                    env: dict[str, str] | None = None, providers: list[dict[str, Any]] | None = None,
                    models: list[dict[str, Any]] | None = None,
                    utility: dict[str, str] | None = None,
                    responses: list[dict[str, Any]] | None = None) -> dict[str, Any]:
    """One ``run_utility_completion`` call, its arguments and the stand-in's answers."""

    return {
        "name": name,
        "providers": providers if providers is not None
        else [OPENAI, provider_entry("mistral", "$BASE/v1", KEY_VARIABLE)],
        "active": {"name": "mistral-large-latest", "provider": "mistral", "alias": "large"},
        "allowedModels": [],
        "models": models or [],
        "utilityModels": utility or {},
        "env": env if env is not None else {KEY_VARIABLE: KEY},
        "feature": feature,
        "callType": call_type,
        "launch": launch,
        "session": session,
        "telemetry": telemetry,
        "skipIfNoKey": skip_if_no_key,
        "responses": responses if responses is not None else [chat_json("Cafe notes")],
    }


def completion_cases() -> list[dict[str, Any]]:
    keyless = [OPENAI, provider_entry("mistral", "$BASE/v1", "")]
    plain = {**LAUNCH, "terminal_emulator": None}
    return [
        completion_case("title-attributed", feature="title", launch=LAUNCH,
                        session=SESSION_ID),
        completion_case("title-unattributed", feature="title", telemetry=False),
        completion_case("title-without-terminal", feature="title", launch=plain,
                        session=SESSION_ID),
        completion_case("smart-approve", feature="smart_approve", session=SESSION_ID),
        completion_case("worktree-title", call_type="worktree_title", launch=LAUNCH,
                        skip_if_no_key=True, telemetry=False),
        completion_case("worktree-title-reported", call_type="worktree_title",
                        launch=LAUNCH, skip_if_no_key=True),
        completion_case("unlabeled", session=SESSION_ID),
        completion_case("skipped-without-key", call_type="worktree_title",
                        skip_if_no_key=True, env={}, responses=[]),
        completion_case("keyless-not-skipped", call_type="worktree_title",
                        skip_if_no_key=True, env={}, providers=keyless),
        completion_case("title-override", feature="title", session=SESSION_ID,
                        models=[MEDIUM], utility={"title": "medium"}),
        completion_case("empty-answer", feature="title", session=SESSION_ID,
                        responses=[chat_json("")]),
    ]
