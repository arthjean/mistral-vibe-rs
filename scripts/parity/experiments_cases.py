"""The inputs ``experiments.py`` drives the pinned reference's experiments
surface over.

The eval host and key, the experiment keys, the model definitions, the gate and
precedence documents, and the eval responses the cache and startup cases store
and seed. Every value here is authored for the corpus; none is read from the
reference.
"""

from __future__ import annotations

import json
from typing import Any

# --------------------------------------------------------------------------
# Inputs the scenarios are driven over
# --------------------------------------------------------------------------

#: The eval host and key every request scenario is built from. Neither is a
#: credential: the GrowthBook client key is publishable by the vendor's own
#: security documentation, and these are authored values rather than the
#: reference's own.
ORACLE_API_HOST = "https://experiments.example.test"
ORACLE_CLIENT_KEY = "sdk-oracle-client-key"

#: The surface every attribute set names. The reference requires it with no
#: default (``vibe/core/experiments/models.py:27``); ``legacy`` is the surface of
#: the reference's own ``AgentLoop`` and the rollout's default variant
#: (``vibe/core/experiments/active.py:38``), and the one this port's single
#: backend corresponds to.
ORACLE_HARNESS = "legacy"

#: The three original reference experiment keys, spelled here so a rename on either side
#: fails the replay rather than moving the question and the answer together.
SYSTEM_PROMPT = "vibe_cli_system_prompt"
MANAGED_SHELL = "vibe_cli_managed_shell_tools"
MODEL_ROUTING = "vibe_cli_default_routing_model"

#: The five keys the reference added since, spelled for the same reason.
SMART_APPROVE = "vibe_cli_smart_approve"
SMART_APPROVE_DEFAULT = "vibe_cli_smart_approve_default"
EXTRA_MODELS = "vibe_cli_extra_models"
REGISTRY_SKILLS = "vibe_cli_registry_skills"
UNIFIED_HARNESS = "vibe_cli_unified_harness_rollout"

#: A model definition the routing variant carries, authored so the layer has
#: something valid to re-encode and the orchestrator something to merge.
ROUTED_ALIAS = "oracle-routed-alias"
ROUTED_MODEL_CONFIG: dict[str, Any] = {
    "name": "oracle-routed-model",
    "provider": "mistral",
    "alias": ROUTED_ALIAS,
    "input_price": "0.0",
    "output_price": "0.0",
    "supports_images": False,
}

#: Two model definitions the extra-models variant carries, authored like the
#: routed one so the layer has valid entries to key by alias.
EXTRA_MODEL: dict[str, Any] = {
    "name": "oracle-extra-model",
    "provider": "mistral",
    "alias": "oracle-extra-alias",
    "input_price": 0.5,
    "output_price": 1.5,
}
SECOND_EXTRA_MODEL: dict[str, Any] = {
    "name": "oracle-second-extra-model",
    "provider": "mistral",
    "alias": "oracle-second-extra-alias",
    "thinking": "high",
}

#: The user document every orchestrator scenario starts from, so a precedence
#: case differs from its neighbour by one line rather than by a whole file. A
#: case's own lines are prepended, because everything below is a table and a
#: bare key written after one would land inside it.
BASE_USER_DOCUMENT = """
[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
"""

#: Provider stacks the session gates are measured over, authored as the
#: documents both sides load.
GATE_CONFIGURATIONS: list[dict[str, str]] = [
    {
        "id": "mistral-active",
        "toml": """
enable_telemetry = true
active_model = "oracle"

[experiments]
enable = true

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
""",
    },
    {
        "id": "telemetry-disabled",
        "toml": """
enable_telemetry = false
active_model = "oracle"

[experiments]
enable = true

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
""",
    },
    {
        "id": "experiments-disabled",
        "toml": """
enable_telemetry = true
active_model = "oracle"

[experiments]
enable = false

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
""",
    },
    {
        "id": "both-gates-off",
        "toml": """
enable_telemetry = false
active_model = "oracle"

[experiments]
enable = false

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
""",
    },
    {
        "id": "third-party-only",
        "toml": """
enable_telemetry = true
active_model = "oracle"

[experiments]
enable = true

[[providers]]
name = "third-party"
api_base = "https://third-party.example.test/v1"
api_key_env_var = "ORACLE_THIRD_PARTY_KEY"

[[models]]
name = "oracle-model"
provider = "third-party"
alias = "oracle"
""",
    },
    {
        "id": "mistral-without-a-resolvable-key",
        "toml": """
enable_telemetry = true
active_model = "oracle"

[experiments]
enable = true

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_ABSENT_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
""",
    },
    {
        "id": "custom-system-prompt",
        "toml": """
enable_telemetry = true
active_model = "oracle"
system_prompt_id = "lean"

[experiments]
enable = true

[[providers]]
name = "mistral"
api_base = "https://api.mistral.ai/v1"
api_key_env_var = "ORACLE_MISTRAL_KEY"
backend = "mistral"

[[models]]
name = "oracle-model"
provider = "mistral"
alias = "oracle"
""",
    },
]


# --------------------------------------------------------------------------
# Authored eval responses
# --------------------------------------------------------------------------


def feature(
    default: Any = None, rules: list[dict[str, Any]] | None = None
) -> dict[str, Any]:
    return {"defaultValue": default, "rules": rules or []}


def track(
    *,
    key: str = SYSTEM_PROMPT,
    result_key: str | None = "1",
    variation: int | None = 1,
    value: Any = None,
    in_experiment: bool | None = True,
) -> dict[str, Any]:
    result: dict[str, Any] = {}
    if result_key is not None:
        result["key"] = result_key
    if variation is not None:
        result["variationId"] = variation
    if value is not None:
        result["value"] = value
    if in_experiment is not None:
        result["inExperiment"] = in_experiment
    return {"experiment": {"key": key}, "result": result}


#: One precedence case: the documents around the orchestrator, the variants the
#: layer is fed, and the field the effective configuration is read on.
PRECEDENCE_CASES: list[dict[str, Any]] = [
    {
        "id": "variant-alone",
        "user": "",
        "project": None,
        "environment": {},
        "overrides": {},
        "variants": {SYSTEM_PROMPT: "tests"},
        "fields": ["system_prompt_id"],
    },
    {
        "id": "user-toml-wins-over-a-variant",
        "user": 'system_prompt_id = "lean"\n',
        "project": None,
        "environment": {},
        "overrides": {},
        "variants": {SYSTEM_PROMPT: "tests"},
        "fields": ["system_prompt_id"],
    },
    {
        "id": "project-toml-wins-over-a-variant",
        "user": "",
        "project": 'system_prompt_id = "minimal"\n',
        "environment": {},
        "overrides": {},
        "variants": {SYSTEM_PROMPT: "tests"},
        "fields": ["system_prompt_id"],
    },
    {
        "id": "project-toml-disables-the-managed-shell",
        "user": "",
        "project": "managed_shell_tools_enabled = false\n",
        "environment": {},
        "overrides": {},
        "variants": {MANAGED_SHELL: "managed"},
        "fields": ["managed_shell_tools_enabled"],
    },
    {
        "id": "user-toml-disables-the-managed-shell",
        "user": "managed_shell_tools_enabled = false\n",
        "project": None,
        "environment": {},
        "overrides": {},
        "variants": {MANAGED_SHELL: "managed"},
        "fields": ["managed_shell_tools_enabled"],
    },
    {
        "id": "managed-shell-variant-alone",
        "user": "",
        "project": None,
        "environment": {},
        "overrides": {},
        "variants": {MANAGED_SHELL: "managed"},
        "fields": ["managed_shell_tools_enabled"],
    },
    {
        "id": "environment-wins-over-a-variant",
        "user": "",
        "project": None,
        "environment": {"VIBE_SYSTEM_PROMPT_ID": "minimal"},
        "overrides": {},
        "variants": {SYSTEM_PROMPT: "tests"},
        "fields": ["system_prompt_id"],
    },
    {
        "id": "runtime-override-wins-over-a-variant",
        "user": "",
        "project": None,
        "environment": {},
        "overrides": {"system_prompt_id": "lean"},
        "variants": {SYSTEM_PROMPT: "tests"},
        "fields": ["system_prompt_id"],
    },
    {
        "id": "forced-variant-without-tracks-loses-to-the-user-toml",
        "user": 'system_prompt_id = "lean"\n',
        "project": None,
        "environment": {},
        "overrides": {},
        "variants": {SYSTEM_PROMPT: "tests"},
        "fields": ["system_prompt_id"],
    },
    {
        "id": "unknown-variant-leaves-the-default",
        "user": "",
        "project": None,
        "environment": {},
        "overrides": {},
        "variants": {SYSTEM_PROMPT: "removed_after_graduation"},
        "fields": ["system_prompt_id"],
    },
    {
        "id": "routing-variant-for-an-unpinned-user",
        "user": "",
        "project": None,
        "environment": {},
        "overrides": {},
        "variants": {
            MODEL_ROUTING: json.dumps(
                {"active_model": ROUTED_ALIAS, "model_config": ROUTED_MODEL_CONFIG}
            )
        },
        "fields": [
            "active_model",
            "routed_default_model",
            "routed_model_config",
            "resolvedDefaultAlias",
            "activeModelAlias",
        ],
    },
    {
        "id": "routing-variant-loses-to-a-pinned-model",
        "user": 'active_model = "oracle"\n',
        "project": None,
        "environment": {},
        "overrides": {},
        "variants": {
            MODEL_ROUTING: json.dumps(
                {"active_model": ROUTED_ALIAS, "model_config": ROUTED_MODEL_CONFIG}
            )
        },
        "fields": [
            "active_model",
            "routed_default_model",
            "resolvedDefaultAlias",
            "activeModelAlias",
        ],
    },
]


#: The instant every cache scenario runs at, in seconds since the epoch. The
#: fraction proves the reference truncates the clock before comparing it.
CACHE_NOW = 1_800_000_000
CACHE_CLOCK = CACHE_NOW + 0.75

#: The response the cache scenarios store and seed, with a confirmed exposure so
#: the dump carries every track field.
CACHED_RESPONSE: dict[str, Any] = {
    "features": {
        SYSTEM_PROMPT: feature("lean", [{"force": "tests", "tracks": [track()]}]),
        SMART_APPROVE: feature(False, [{"force": True}]),
    }
}


#: The fields a startup scenario reads off the configuration: every field a
#: rollout reaches except the routed model, which `layerPrecedence` covers.
STARTUP_FIELDS = (
    "system_prompt_id",
    "managed_shell_tools_enabled",
    "smart_approve_available",
    "smart_approve_default",
    "experimental_enable_registry_skills",
)

#: What a previous session left in the cache: a prompt off its baseline and a
#: confirmed smart-approve exposure.
STARTUP_CACHED: dict[str, Any] = {
    "features": {
        SYSTEM_PROMPT: feature("lean"),
        SMART_APPROVE: feature(False, [{"force": True, "tracks": [track(key=SMART_APPROVE)]}]),
    }
}

#: A cached response whose every value sits at its baseline, so it maps to no
#: configuration variant at all.
STARTUP_INERT: dict[str, Any] = {
    "features": {SYSTEM_PROMPT: feature("cli"), SMART_APPROVE: feature(False)}
}

#: What the live lookup answers, deliberately different from the cache. It
#: leaves the registry-skills flag alone: turning it on starts a registry sync,
#: which would leave the process.
STARTUP_LIVE: dict[str, Any] = {
    "features": {
        SYSTEM_PROMPT: feature("cli", [{"force": "tests", "tracks": [track()]}]),
        SMART_APPROVE_DEFAULT: feature(False, [{"force": True}]),
    }
}

#: The seeds a startup scenario finds under the vibe home: nothing, a recent
#: entry, one past the bound, one that maps to nothing, and one for another
#: credential. Ages are relative to the scenario's clock so a replay can seed
#: the same entry against its own.
STARTUP_SEEDS: list[tuple[str, dict[str, Any] | None]] = [
    ("nothing-cached", None),
    ("cached", {"owner": "oracle", "ageSeconds": 60, "payload": STARTUP_CACHED}),
    (
        "cached-past-the-bound",
        {"owner": "oracle", "ageSeconds": 7 * 24 * 60 * 60, "payload": STARTUP_CACHED},
    ),
    ("cached-at-the-baseline", {"owner": "oracle", "ageSeconds": 60, "payload": STARTUP_INERT}),
    ("cached-for-another-key", {"owner": "other", "ageSeconds": 60, "payload": STARTUP_CACHED}),
]
