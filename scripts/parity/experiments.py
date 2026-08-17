#!/usr/bin/env python3
"""Capture what the pinned Python reference's experiments engine and VS Code promo answer.

This rank splits across three reference subtrees that are never read in
isolation: the engine under ``vibe/core/experiments/``, the only place a variant
becomes a configuration value in ``vibe/core/config/layers/growthbook.py``, and
the promo under ``vibe/cli/vscode_extension_promo/``. This capture drives every
one of them directly, over inputs this script authors, and writes two corpora
because the two replays live in two crates.

The engine corpus records thirteen families the Rust replay in
``crates/vibe-core/src/experiments_parity_tests.rs`` compares this build
against::

    constants           the three names, their defaults, the paths and timeouts
    bucketingKey        the anonymous hash attribute derived from an API key
    evalUrl             the URL one api_host and client_key pair resolves to
    evalRequest         the request the client would have issued, one call early
    evalFailures        what each of the five failure branches leaves behind
    featureResolution   which value a feature definition resolves to
    variantResolution   what the manager answers per experiment name
    configVariants      config_variants beside assignments for the same input
    variantLabels       the four-level label fallback and its empty terminal
    configMapping       the field set the GrowthBook layer writes per variant
    layerPrecedence     the effective value when a layer and a variant collide
    sessionGates        which gate stops an initialization and what it attempted
    attributes          the nine attributes a launch context produces

The promo corpus records four more, replayed from
``crates/vibe-cli/src/tui/promo_parity_tests.rs``::

    promoConstants      the ceiling, the start instant, the section and the URI
    promoPredicate      the decision per stored state and instant
    promoState          what the repository reads back out of ``cache.toml``
    promoProse          a length and a SHA-256 for each reference sentence

Three artifacts come out of a run::

    .parity/experiments-corpus.json                 the full capture, gitignored
    crates/vibe-core/tests/experiments/corpus.json  the committed engine corpus
    crates/vibe-cli/tests/promo/corpus.json         the committed promo corpus

The capture asserts its own isolation. Every environment variable a scenario
names is set to a sentinel before anything runs, a resolved credential is
recorded as the variable it came from and never as a value, and a socket guard
fails the run on any connection or DNS attempt, so a scenario that accidentally
reached ``experiments.mistral.services`` could not write a corpus. The eval
request is intercepted one call before the connection, which is where
``scripts/parity/voice.py`` already intercepts the reference. Reference-authored
sentences, which here are the two promo sentences, are recorded as a byte length
and a SHA-256: a digest still fails the replay on any change while no reference
sentence ships.

Usage::

    scripts/parity/experiments.py --reference /path/to/reference --corpus

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it. The wrapper re-executes itself with
the reference interpreter, reading the pinned commit through ``git archive`` so
the checkout is never moved.
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import hashlib
import json
import logging
import os
from pathlib import Path
import platform
import shutil
import socket
import subprocess
import sys
import tarfile
import tempfile
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT, RESTORE_COMMAND

SCHEMA_VERSION = 1
DEFAULT_OUTPUT = Path(".parity/experiments-corpus.json")
DEFAULT_CORPUS = Path("crates/vibe-core/tests/experiments/corpus.json")
DEFAULT_PROMO_CORPUS = Path("crates/vibe-cli/tests/promo/corpus.json")
DEFAULT_CACHE = Path(".parity")

#: Set on the re-executed process so it does not extract and re-exec forever.
_REEXEC_MARKER = "VIBE_PARITY_PINNED_TREE"

#: Stands in for the platform identifier, which differs per workstation. The
#: replay substitutes what its own host answers before comparing.
PLATFORM_ID_PLACEHOLDER = "{platformId}"
#: Stands in for the reference package version, so a release does not rewrite
#: the corpus.
VERSION_PLACEHOLDER = "{version}"

#: Every environment variable this capture names, with the sentinel it is set to
#: first. A value here never reaches a corpus: what is recorded is the variable
#: a credential came from.
SENTINELS: dict[str, str] = {
    "MISTRAL_API_KEY": "oracle-default-sentinel",
    "ORACLE_MISTRAL_KEY": "oracle-mistral-sentinel",
    "ORACLE_SECOND_KEY": "oracle-second-sentinel",
    "ORACLE_THIRD_PARTY_KEY": "oracle-third-party-sentinel",
}

#: Named but deliberately left unset, so the "no credential" branch is reached
#: through a variable the corpus can name.
UNSET_VARIABLES = ("ORACLE_ABSENT_KEY",)


class OracleError(RuntimeError):
    """Raised when a corpus cannot be produced from an authoritative state."""


# --------------------------------------------------------------------------
# Reference pinning
# --------------------------------------------------------------------------


def _git(reference: Path, *arguments: str) -> str:
    result = subprocess.run(
        ["git", *arguments],
        cwd=reference,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise OracleError(
            f"git {' '.join(arguments)} failed in {reference}: {result.stderr.strip()}"
        )
    return result.stdout.strip()


def resolve_reference(reference: Path, expected: str) -> dict[str, str]:
    """The pinned commit, read out of the checkout without depending on its HEAD.

    Everything downstream reads the pinned tree through ``git archive``, so a
    checkout parked on another revision is still an oracle as long as it holds
    the commit. A checkout that is absent, unreadable or missing the pin is not,
    and the refusal happens here, before a byte of any corpus is written.
    """

    if not reference.is_dir():
        raise OracleError(
            f"no reference checkout at {reference}; set VIBE_REFERENCE or pass --reference"
        )
    try:
        _git(reference, "cat-file", "-e", f"{expected}^{{commit}}")
    except OracleError as error:
        raise OracleError(
            f"{reference} does not contain the pinned commit {expected}: {error}. "
            f"Restore it with `{RESTORE_COMMAND}`"
        ) from error
    return {"commit": expected, "path": str(reference)}


def extract_pinned_tree(reference: Path, commit: str, cache: Path) -> Path:
    """The pinned source tree, materialized out of tree and reused across runs.

    Two Rust probes drive this script, one per crate, and `cargo test` may run
    them at the same time. The extraction therefore lands in a per-process
    directory and is moved into place at the end, so a concurrent run reads a
    complete tree or builds its own rather than importing a half-written one.
    """

    tree = (cache / f"reference-{commit[:12]}").resolve()
    marker = Path("vibe") / "__init__.py"
    if (tree / marker).is_file():
        return tree
    staged = tree.with_name(f"{tree.name}.{os.getpid()}.partial")
    staged.mkdir(parents=True, exist_ok=True)
    archive = staged.with_suffix(".tar")
    _git(reference, "archive", "--format=tar", "-o", str(archive), commit)
    with tarfile.open(archive) as bundle:
        bundle.extractall(staged, filter="data")
    archive.unlink(missing_ok=True)
    if not (staged / marker).is_file():
        raise OracleError(f"the extracted tree at {staged} carries no `vibe` package")
    try:
        staged.rename(tree)
    except OSError:
        # Another run finished first, which is the only way the destination
        # exists by now. Its tree is the same commit, so theirs wins.
        shutil.rmtree(staged, ignore_errors=True)
    if not (tree / marker).is_file():
        raise OracleError(f"the extracted tree at {tree} carries no `vibe` package")
    return tree


def reexecute_with_reference_interpreter(
    reference: Path, override: Path | None, tree: Path
) -> None:
    """Re-runs this script under an interpreter importing the *pinned* tree."""

    if os.environ.get(_REEXEC_MARKER) == str(tree):
        if not _imports_pinned_vibe(tree):
            raise OracleError(
                f"the reference interpreter did not import `vibe` from {tree}"
            )
        return
    candidates = [override] if override else []
    candidates += [
        reference / ".venv/bin/python",
        reference / ".venv/Scripts/python.exe",
    ]
    interpreter = next((c for c in candidates if c and c.is_file()), None)
    if interpreter is None:
        raise OracleError(
            f"no interpreter can import `vibe`; looked for a virtual environment in {reference}"
        )
    environment = dict(os.environ)
    environment[_REEXEC_MARKER] = str(tree)
    environment["PYTHONPATH"] = os.pathsep.join(
        [
            str(tree),
            *([environment["PYTHONPATH"]] if environment.get("PYTHONPATH") else []),
        ]
    )
    os.execve(str(interpreter), [str(interpreter), *sys.argv], environment)


def _imports_pinned_vibe(tree: Path) -> bool:
    try:
        import vibe
    except Exception:
        return False
    return Path(vibe.__file__).resolve().is_relative_to(tree.resolve())


# --------------------------------------------------------------------------
# Isolation
# --------------------------------------------------------------------------


class _SocketGuard:
    """Fails the capture the moment anything tries to reach a network.

    ``connect``, ``connect_ex``, ``create_connection`` and ``getaddrinfo`` are
    replaced with raisers; ``socketpair`` stays, because asyncio's self-pipe uses
    it without connecting anywhere.
    """

    def __init__(self) -> None:
        self.attempts: list[str] = []

    def install(self) -> None:
        guard = self

        def refuse(name: str) -> Any:
            def raiser(*arguments: Any, **keywords: Any) -> Any:
                guard.attempts.append(name)
                raise OracleError(
                    f"the capture attempted network access through {name}"
                )

            return raiser

        socket.socket.connect = refuse("socket.connect")  # type: ignore[method-assign]
        socket.socket.connect_ex = refuse("socket.connect_ex")  # type: ignore[method-assign]
        socket.create_connection = refuse("socket.create_connection")  # type: ignore[assignment]
        socket.getaddrinfo = refuse("socket.getaddrinfo")  # type: ignore[assignment]


GUARD = _SocketGuard()


def install_sentinels() -> None:
    """Every named variable set to a sentinel, before any scenario runs."""

    for name, value in SENTINELS.items():
        os.environ[name] = value
    for name in UNSET_VARIABLES:
        os.environ.pop(name, None)


@contextlib.contextmanager
def patched_environ(values: dict[str, str | None]):
    originals = {name: os.environ.get(name) for name in values}
    for name, value in values.items():
        if value is None:
            os.environ.pop(name, None)
        else:
            os.environ[name] = value
    try:
        yield
    finally:
        for name, original in originals.items():
            if original is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = original


@contextlib.contextmanager
def captured_logs():
    """Every record the reference's logger emits inside the block.

    What is recorded is the level and the count, never the message: the log
    lines are reference-authored prose that `NOTICE` forbids reproducing, and
    what parity needs is that a failure warns rather than what it says.
    """

    from vibe.observability.logging import logger

    records: list[logging.LogRecord] = []

    class _Collector(logging.Handler):
        def emit(self, record: logging.LogRecord) -> None:
            records.append(record)

    handler = _Collector(level=logging.DEBUG)
    previous_level = logger.level
    previous_propagate = logger.propagate
    logger.addHandler(handler)
    logger.setLevel(logging.DEBUG)
    logger.propagate = False
    try:
        yield records
    finally:
        logger.removeHandler(handler)
        logger.setLevel(previous_level)
        logger.propagate = previous_propagate


def log_summary(records: list[logging.LogRecord]) -> dict[str, Any]:
    return {
        "count": len(records),
        "levels": sorted({record.levelname for record in records}),
    }


def digest(value: str) -> dict[str, Any]:
    """A string recorded by its length and its SHA-256, never by its content."""

    return {
        "length": len(value),
        "digest": hashlib.sha256(value.encode("utf-8")).hexdigest(),
    }


def scrub(value: Any) -> Any:
    """Machine-dependent and release-dependent values replaced by placeholders."""

    from vibe import __version__
    from vibe.core.utils import get_platform_id

    replacements = [
        (__version__, VERSION_PLACEHOLDER),
        (get_platform_id(), PLATFORM_ID_PLACEHOLDER),
    ]
    if isinstance(value, str):
        for original, placeholder in replacements:
            if original:
                value = value.replace(original, placeholder)
        return value
    if isinstance(value, dict):
        return {key: scrub(item) for key, item in value.items()}
    if isinstance(value, list):
        return [scrub(item) for item in value]
    return value


def variable_for(value: str | None) -> str | None:
    """The environment variable a captured credential came from.

    A credential is recorded by the variable that supplied it, so no value ever
    reaches a corpus even when a scenario resolves one. A value no sentinel
    matches is recorded as ``unrecognized`` rather than as itself.
    """

    if value is None:
        return None
    for name, sentinel in SENTINELS.items():
        if sentinel == value:
            return name
    return "unrecognized"


def sentinel_variable(value: str | None) -> str | None:
    """The variable a value is the sentinel of, or [`None`] when it is not one.

    Used where the answer must be that nothing carries a credential: an eval
    payload names a bucketing digest, and a variable appearing here would mean
    the key itself had leaked into the request.
    """

    if value is None:
        return None
    for name, sentinel in SENTINELS.items():
        if sentinel == value:
            return name
    return None


# --------------------------------------------------------------------------
# Inputs the scenarios are driven over
# --------------------------------------------------------------------------

#: The eval host and key every request scenario is built from. Neither is a
#: credential: the GrowthBook client key is publishable by the vendor's own
#: security documentation, and these are authored values rather than the
#: reference's own.
ORACLE_API_HOST = "https://experiments.example.test"
ORACLE_CLIENT_KEY = "sdk-oracle-client-key"

#: The three reference experiment keys, spelled here so a rename on either side
#: fails the replay rather than moving the question and the answer together.
SYSTEM_PROMPT = "vibe_cli_system_prompt"
MANAGED_SHELL = "vibe_cli_managed_shell_tools"
MODEL_ROUTING = "vibe_cli_default_routing_model"

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


def build_config(document: str) -> Any:
    """The reference configuration one authored document validates to."""

    import tomllib

    from vibe.core.config.vibe_schema import VibeConfigSchema

    return VibeConfigSchema.model_validate(
        tomllib.loads(document), context={"require_api_key": False}
    )


# --------------------------------------------------------------------------
# Transports, standing one call before the connection
# --------------------------------------------------------------------------


class _Response:
    """The shape ``RemoteEvalClient.evaluate`` reads off an httpx response."""

    def __init__(self, status: int, body: Any, *, raw: str | None = None) -> None:
        self.status_code = status
        self._body = body
        self.text = raw if raw is not None else json.dumps(body)

    def json(self) -> Any:
        if self._body is _INVALID_JSON:
            raise ValueError("the oracle body is not JSON")
        return self._body


#: Sentinel for a body whose decoding raises, which is the reference's third
#: failure branch.
_INVALID_JSON = object()


class RecordingEvalTransport:
    """Stands where the reference's HTTP client stands, one call before the
    connection: it records the request the client would have issued, answers
    with an authored response, and never opens a socket."""

    def __init__(
        self,
        *,
        status: int = 200,
        body: Any = None,
        error: Exception | None = None,
        raw: str | None = None,
    ) -> None:
        self.requests: list[dict[str, Any]] = []
        self.closes = 0
        self._status = status
        self._body = {"features": {}} if body is None else body
        self._error = error
        self._raw = raw

    async def post(
        self,
        url: str,
        *,
        json: dict[str, Any] | None = None,
        headers: dict[str, str] | None = None,
        **_: Any,
    ) -> _Response:
        self.requests.append(
            {
                "method": "POST",
                "url": url,
                "payload": json,
                "headerNames": sorted(headers or {}),
            }
        )
        if self._error is not None:
            raise self._error
        return _Response(self._status, self._body, raw=self._raw)

    async def aclose(self) -> None:
        self.closes += 1


def eval_client(transport: RecordingEvalTransport | None, *, url: str | None) -> Any:
    """A reference eval client whose lazy transport is already filled in."""

    from vibe.core.experiments.client import RemoteEvalClient

    client = RemoteEvalClient(url=url)
    if transport is not None:
        client._http = transport  # type: ignore[assignment]
    return client


def manager_with(response: dict[str, Any] | None) -> Any:
    """A reference manager hydrated from one authored eval response."""

    from vibe.core.experiments.manager import ExperimentManager
    from vibe.core.experiments.models import EvalResponse

    manager = ExperimentManager(client=eval_client(None, url=None))
    if response is not None:
        with captured_logs():
            manager.hydrate(EvalResponse.model_validate(response))
    return manager


def oracle_attributes(**overrides: Any) -> Any:
    """The attributes every request scenario posts, before any override."""

    from vibe.core.experiments.models import ExperimentAttributes

    values: dict[str, Any] = {
        "userId": "0123456789abcdef0123456789abcdef",
        "entrypoint": "cli",
        "agent_version": "9.9.9",
        "client_name": "oracle-client",
        "client_version": "1.2.3",
        "os": "linux",
        "terminal_emulator": "vscode",
        "custom_system_prompt": False,
        "organizationId": "oracle-organization",
    }
    values.update(overrides)
    return ExperimentAttributes.model_validate(values)


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


# --------------------------------------------------------------------------
# constants
# --------------------------------------------------------------------------


def capture_constants() -> list[dict[str, Any]]:
    from vibe.core.config.layers.growthbook import (
        GROWTHBOOK_CONFIG_MAPPINGS,
        GrowthbookLayer,
    )
    from vibe.core.config.vibe_schema import VibeConfigSchema
    from vibe.core.experiments._constants import (
        EVAL_REQUEST_TIMEOUT_SECONDS,
        GROWTHBOOK_EVAL_PATH_TEMPLATE,
    )
    from vibe.core.experiments.active import DEFAULT_VARIANTS, ExperimentName
    from vibe.core.experiments.session import EXPERIMENT_IDENTITY_TIMEOUT_S
    from vibe.core.identity import _IDENTITY_PATH

    experiments_default = VibeConfigSchema.model_fields["experiments"].default_factory
    defaults = experiments_default()  # type: ignore[misc]
    return [
        {
            "id": "experimentNames",
            "value": [name.value for name in ExperimentName],
        },
        {
            "id": "defaultVariants",
            "value": {name.value: value for name, value in DEFAULT_VARIANTS.items()},
        },
        {
            "id": "everyNameHasADefault",
            "value": all(name in DEFAULT_VARIANTS for name in ExperimentName),
        },
        {"id": "evalPathTemplate", "value": GROWTHBOOK_EVAL_PATH_TEMPLATE},
        {"id": "evalTimeoutSeconds", "value": EVAL_REQUEST_TIMEOUT_SECONDS},
        {"id": "identityTimeoutSeconds", "value": EXPERIMENT_IDENTITY_TIMEOUT_S},
        {"id": "identityPath", "value": _IDENTITY_PATH},
        {"id": "bucketingKeyLength", "value": 32},
        {"id": "layerName", "value": GrowthbookLayer.NAME},
        {
            "id": "configuredFields",
            "value": {
                name.value: [field for field, _ in mappers]
                for name, mappers in GROWTHBOOK_CONFIG_MAPPINGS.items()
            },
        },
        {
            "id": "experimentsConfigDefaults",
            "value": {
                "enable": defaults.enable,
                "api_host": defaults.api_host,
                "client_key": defaults.client_key,
            },
        },
        {
            "id": "payloadKeys",
            "value": ["attributes", "forcedFeatures", "forcedVariations", "url"],
        },
    ]


# --------------------------------------------------------------------------
# bucketingKey
# --------------------------------------------------------------------------


def capture_bucketing_key() -> list[dict[str, Any]]:
    from vibe.core.experiments.manager import hash_api_key

    cases: list[dict[str, Any]] = []
    for variable in ("ORACLE_MISTRAL_KEY", "ORACLE_SECOND_KEY", "MISTRAL_API_KEY"):
        key = SENTINELS[variable]
        first = hash_api_key(key)
        cases.append(
            {
                "id": variable,
                "variable": variable,
                "bucketingKey": first,
                "length": len(first),
                "stable": first == hash_api_key(key),
                "hexadecimal": all(
                    character in "0123456789abcdef" for character in first
                ),
            }
        )
    distinct = {case["bucketingKey"] for case in cases}
    cases.append(
        {
            "id": "distinctPerKey",
            "variable": None,
            "bucketingKey": None,
            "length": None,
            "stable": len(distinct) == len(cases),
            "hexadecimal": None,
        }
    )
    return cases


# --------------------------------------------------------------------------
# evalUrl
# --------------------------------------------------------------------------


def capture_eval_url() -> list[dict[str, Any]]:
    from vibe.core.experiments._constants import build_eval_url

    inputs: list[tuple[str, str, str]] = [
        ("plain", "https://experiments.example.test", "sdk-key"),
        ("one-trailing-slash", "https://experiments.example.test/", "sdk-key"),
        ("three-trailing-slashes", "https://experiments.example.test///", "sdk-key"),
        ("host-with-a-path", "https://gateway.example.test/growthbook", "sdk-key"),
        ("host-with-a-path-and-a-slash", "https://gateway.example.test/gb/", "sdk-key"),
        ("surrounding-whitespace", "  https://experiments.example.test  ", " sdk-key "),
        ("empty-host", "", "sdk-key"),
        ("blank-host", "   ", "sdk-key"),
        ("host-of-only-slashes", "///", "sdk-key"),
        ("empty-key", "https://experiments.example.test", ""),
        ("blank-key", "https://experiments.example.test", "  "),
        ("both-empty", "", ""),
    ]
    return [
        {
            "id": identifier,
            "apiHost": api_host,
            "clientKey": client_key,
            "url": build_eval_url(api_host, client_key),
        }
        for identifier, api_host, client_key in inputs
    ]


# --------------------------------------------------------------------------
# evalRequest
# --------------------------------------------------------------------------


def capture_eval_request() -> list[dict[str, Any]]:
    import httpx

    from vibe.core.experiments._constants import build_eval_url

    url = build_eval_url(ORACLE_API_HOST, ORACLE_CLIENT_KEY)
    cases: list[dict[str, Any]] = []

    scenarios: list[tuple[str, Any]] = [
        ("every-attribute", oracle_attributes()),
        (
            "optional-attributes-absent",
            oracle_attributes(
                client_name=None,
                client_version=None,
                terminal_emulator=None,
                organizationId=None,
            ),
        ),
        (
            "custom-system-prompt",
            oracle_attributes(custom_system_prompt=True, entrypoint="acp"),
        ),
    ]
    for identifier, attributes in scenarios:
        transport = RecordingEvalTransport()
        client = eval_client(transport, url=url)
        with captured_logs():
            response = asyncio.run(client.evaluate(attributes))
        request = transport.requests[0]
        payload = request["payload"] or {}
        cases.append(
            {
                "id": identifier,
                "method": request["method"],
                "url": request["url"],
                "headerNames": request["headerNames"],
                "payloadKeys": sorted(payload),
                "attributeKeys": sorted(payload.get("attributes", {})),
                "attributes": scrub(payload.get("attributes", {})),
                "forcedVariations": payload.get("forcedVariations"),
                "forcedFeatures": payload.get("forcedFeatures"),
                "urlField": payload.get("url"),
                "credentialVariable": sentinel_variable(
                    payload.get("attributes", {}).get("userId")
                ),
                "returnedState": None if response is None else "features",
                "requests": len(transport.requests),
            }
        )

    # The timeout is a property of the client the reference builds lazily, so it
    # is read off a real one. Constructing an httpx client opens no socket, and
    # the guard would fail the run if it did.
    lazy = eval_client(None, url=url)
    timeout = lazy._client.timeout  # noqa: SLF001 - the lazy build is the subject
    cases.append(
        {
            "id": "lazyClientTimeout",
            "method": None,
            "url": url,
            "headerNames": None,
            "payloadKeys": None,
            "attributeKeys": None,
            "attributes": {
                "connect": timeout.connect,
                "read": timeout.read,
                "write": timeout.write,
                "pool": timeout.pool,
            },
            "forcedVariations": None,
            "forcedFeatures": None,
            "urlField": None,
            "credentialVariable": None,
            "returnedState": None,
            "requests": 0,
        }
    )
    asyncio.run(lazy.aclose())
    assert isinstance(timeout, httpx.Timeout)

    # A client that was never used closes without touching a transport, and
    # closing twice is a no-op.
    unused = eval_client(None, url=url)
    asyncio.run(unused.aclose())
    asyncio.run(unused.aclose())
    used = RecordingEvalTransport()
    client = eval_client(used, url=url)
    asyncio.run(client.aclose())
    asyncio.run(client.aclose())
    cases.append(
        {
            "id": "closeIsIdempotent",
            "method": None,
            "url": None,
            "headerNames": None,
            "payloadKeys": None,
            "attributeKeys": None,
            "attributes": {"closes": used.closes},
            "forcedVariations": None,
            "forcedFeatures": None,
            "urlField": None,
            "credentialVariable": None,
            "returnedState": None,
            "requests": 0,
        }
    )
    return cases


# --------------------------------------------------------------------------
# evalFailures
# --------------------------------------------------------------------------


def capture_eval_failures() -> list[dict[str, Any]]:
    import httpx

    from vibe.core.experiments._constants import build_eval_url
    from vibe.core.experiments.active import DEFAULT_VARIANTS, ExperimentName

    url = build_eval_url(ORACLE_API_HOST, ORACLE_CLIENT_KEY)
    resolved = {
        name.value: DEFAULT_VARIANTS[name] for name in ExperimentName
    }
    scenarios: list[tuple[str, str | None, dict[str, Any]]] = [
        (
            "connection-error",
            url,
            {"error": httpx.ConnectError("the oracle refuses to connect")},
        ),
        ("timeout", url, {"error": httpx.ReadTimeout("the oracle timed out")}),
        ("status-400", url, {"status": 400, "body": {"features": {}}}),
        ("status-404", url, {"status": 404, "body": {"features": {}}}),
        ("status-500", url, {"status": 500, "body": {"features": {}}}),
        ("status-503", url, {"status": 503, "body": {"features": {}}}),
        ("non-json-body", url, {"body": _INVALID_JSON, "raw": "<html>nope</html>"}),
        ("body-fails-validation", url, {"body": {"features": 17}}),
        ("url-unset", None, {}),
    ]

    cases: list[dict[str, Any]] = []
    for identifier, case_url, keywords in scenarios:
        transport = None if case_url is None else RecordingEvalTransport(**keywords)
        manager = manager_with(None)
        manager._client = eval_client(transport, url=case_url)  # noqa: SLF001
        with captured_logs() as records:
            asyncio.run(manager.initialize(oracle_attributes()))
        cases.append(
            {
                "id": identifier,
                "state": manager.export_state(),
                "requests": 0 if transport is None else len(transport.requests),
                "variants": {
                    name.value: manager.get_variant(name) for name in ExperimentName
                },
                "variantsAreDefaults": {
                    name.value: manager.get_variant(name) for name in ExperimentName
                }
                == resolved,
                "assignments": manager.assignments(),
                "configVariants": manager.config_variants(),
                "logs": log_summary(records),
            }
        )
    return cases


# --------------------------------------------------------------------------
# featureResolution
# --------------------------------------------------------------------------


def capture_feature_resolution() -> list[dict[str, Any]]:
    from vibe.core.experiments.models import FeatureDefinition

    definitions: list[tuple[str, dict[str, Any]]] = [
        ("default-only", feature("cli")),
        ("default-null", feature(None)),
        ("no-rules-at-all", {"defaultValue": "cli"}),
        ("forced-string", feature("cli", [{"force": "tests"}])),
        ("forced-empty-string", feature("cli", [{"force": ""}])),
        ("forced-false", feature("cli", [{"force": False}])),
        ("forced-zero", feature("cli", [{"force": 0}])),
        ("forced-object", feature("cli", [{"force": {"active_model": ROUTED_ALIAS}}])),
        ("forced-array", feature("cli", [{"force": ["a", "b"]}])),
        ("first-rule-without-a-force", feature("cli", [{}, {"force": "tests"}])),
        ("no-rule-carries-a-force", feature("cli", [{}, {"tracks": []}])),
        ("rule-forcing-null", feature("cli", [{"force": None}])),
        (
            "unknown-rule-keys",
            feature(
                "cli",
                [
                    {
                        "condition": {"country": "FR"},
                        "coverage": 0.5,
                        "variations": ["a", "b"],
                        "force": "tests",
                    }
                ],
            ),
        ),
        (
            "unknown-feature-keys",
            {
                "defaultValue": "cli",
                "rules": [],
                "scrubbed": True,
                "experiments": [{"key": "unused"}],
            },
        ),
    ]
    return [
        {
            "id": identifier,
            "definition": definition,
            "resolved": FeatureDefinition.model_validate(definition).resolved_value(),
        }
        for identifier, definition in definitions
    ]


# --------------------------------------------------------------------------
# variantResolution
# --------------------------------------------------------------------------


def capture_variant_resolution() -> list[dict[str, Any]]:
    from vibe.core.experiments.active import ExperimentName

    routing_payload = {"active_model": ROUTED_ALIAS, "model_config": ROUTED_MODEL_CONFIG}
    responses: list[tuple[str, dict[str, Any] | None]] = [
        ("uninitialized", None),
        ("empty-features", {"features": {}}),
        (
            "string-value",
            {"features": {SYSTEM_PROMPT: feature("cli", [{"force": "tests"}])}},
        ),
        (
            "object-value",
            {"features": {MODEL_ROUTING: feature(None, [{"force": routing_payload}])}},
        ),
        (
            "array-value",
            {"features": {SYSTEM_PROMPT: feature(None, [{"force": ["cli", "lean"]}])}},
        ),
        (
            "numeric-value",
            {"features": {SYSTEM_PROMPT: feature(None, [{"force": 7}])}},
        ),
        (
            "boolean-value",
            {"features": {MANAGED_SHELL: feature(None, [{"force": True}])}},
        ),
        (
            "resolved-null",
            {"features": {SYSTEM_PROMPT: feature(None, [{"tracks": []}])}},
        ),
        (
            "default-value-without-a-force",
            {"features": {SYSTEM_PROMPT: feature("lean")}},
        ),
        (
            "unknown-feature-key",
            {
                "features": {
                    "vibe_cli_unknown_rollout": feature("x", [{"force": "y"}]),
                    SYSTEM_PROMPT: feature("cli", [{"force": "tests"}]),
                }
            },
        ),
        (
            "every-known-feature",
            {
                "features": {
                    SYSTEM_PROMPT: feature("cli", [{"force": "lean"}]),
                    MANAGED_SHELL: feature("legacy", [{"force": "managed"}]),
                    MODEL_ROUTING: feature("{}", [{"force": routing_payload}]),
                }
            },
        ),
    ]

    cases: list[dict[str, Any]] = []
    for identifier, response in responses:
        manager = manager_with(response)
        state = manager.export_state()
        cases.append(
            {
                "id": identifier,
                "response": response,
                "knownFeatures": sorted(state.features) if state is not None else None,
                "variants": {
                    name.value: manager.get_variant(name) for name in ExperimentName
                },
                "variantsOrNone": {
                    name.value: manager.get_variant_or_none(name)
                    for name in ExperimentName
                },
            }
        )
    return cases


# --------------------------------------------------------------------------
# configVariants
# --------------------------------------------------------------------------


def capture_config_variants() -> list[dict[str, Any]]:
    routing_payload = {"active_model": ROUTED_ALIAS}
    responses: list[tuple[str, dict[str, Any] | None]] = [
        ("uninitialized", None),
        ("empty-features", {"features": {}}),
        (
            "confirmed-exposure",
            {
                "features": {
                    SYSTEM_PROMPT: feature("cli", [{"force": "tests", "tracks": [track()]}])
                }
            },
        ),
        (
            "forced-without-tracks",
            {"features": {SYSTEM_PROMPT: feature("cli", [{"force": "tests"}])}},
        ),
        (
            "forced-not-in-experiment",
            {
                "features": {
                    SYSTEM_PROMPT: feature(
                        "cli",
                        [{"force": "tests", "tracks": [track(in_experiment=False)]}],
                    )
                }
            },
        ),
        (
            "forced-with-in-experiment-absent",
            {
                "features": {
                    SYSTEM_PROMPT: feature(
                        "cli",
                        [{"force": "tests", "tracks": [track(in_experiment=None)]}],
                    )
                }
            },
        ),
        (
            "default-value-without-a-force",
            {"features": {SYSTEM_PROMPT: feature("cli", [{"tracks": [track()]}])}},
        ),
        (
            "forced-object-without-tracks",
            {
                "features": {
                    MODEL_ROUTING: feature("{}", [{"force": routing_payload}])
                }
            },
        ),
        (
            "experiment-key-differs-from-the-feature-key",
            {
                "features": {
                    MANAGED_SHELL: feature(
                        "legacy",
                        [
                            {
                                "force": "managed",
                                "tracks": [track(key="some_other_experiment")],
                            }
                        ],
                    )
                }
            },
        ),
        (
            "mixed-confirmed-and-forced",
            {
                "features": {
                    SYSTEM_PROMPT: feature(
                        "cli", [{"force": "tests", "tracks": [track()]}]
                    ),
                    MANAGED_SHELL: feature("legacy", [{"force": "managed"}]),
                    MODEL_ROUTING: feature("{}"),
                }
            },
        ),
    ]
    cases: list[dict[str, Any]] = []
    for identifier, response in responses:
        manager = manager_with(response)
        cases.append(
            {
                "id": identifier,
                "response": response,
                "assignments": manager.assignments(),
                "configVariants": manager.config_variants(),
            }
        )
    return cases


# --------------------------------------------------------------------------
# variantLabels
# --------------------------------------------------------------------------


def capture_variant_labels() -> list[dict[str, Any]]:
    definitions: list[tuple[str, dict[str, Any]]] = [
        (
            "track-value-string",
            feature("cli", [{"force": "tests", "tracks": [track(value="from-track")]}]),
        ),
        (
            "track-value-object",
            feature(
                "cli",
                [{"force": "tests", "tracks": [track(value={"variant": "managed"})]}],
            ),
        ),
        (
            "falls-back-to-the-resolved-value",
            feature("cli", [{"force": "tests", "tracks": [track()]}]),
        ),
        (
            "falls-back-to-the-default-value",
            feature("cli", [{"tracks": [track()]}]),
        ),
        (
            "falls-back-to-the-result-key",
            feature(None, [{"tracks": [track(result_key="control")]}]),
        ),
        (
            "falls-back-to-the-variation-id",
            feature(None, [{"tracks": [track(result_key=None, variation=3)]}]),
        ),
        (
            "variation-id-zero",
            feature(None, [{"tracks": [track(result_key=None, variation=0)]}]),
        ),
        (
            "exhausted-fallback",
            feature(None, [{"tracks": [track(result_key=None, variation=None)]}]),
        ),
        (
            "last-confirmed-track-wins",
            feature(
                None,
                [
                    {"tracks": [track(value="first"), track(value="second")]},
                ],
            ),
        ),
    ]
    cases: list[dict[str, Any]] = []
    for identifier, definition in definitions:
        manager = manager_with({"features": {SYSTEM_PROMPT: definition}})
        cases.append(
            {
                "id": identifier,
                "definition": definition,
                "assignments": manager.assignments(),
                "reported": SYSTEM_PROMPT in manager.assignments(),
            }
        )
    return cases


# --------------------------------------------------------------------------
# configMapping
# --------------------------------------------------------------------------


def capture_config_mapping() -> list[dict[str, Any]]:
    from vibe.core.config.layers.growthbook import GrowthbookLayer
    from vibe.core.config.types import MISSING_BACKING_STORE_DATA_FINGERPRINT

    routing = json.dumps({"active_model": ROUTED_ALIAS})
    routing_with_config = json.dumps(
        {"active_model": ROUTED_ALIAS, "model_config": ROUTED_MODEL_CONFIG}
    )
    variants: list[tuple[str, dict[str, str]]] = [
        ("no-variants", {}),
        ("known-system-prompt", {SYSTEM_PROMPT: "tests"}),
        ("another-known-system-prompt", {SYSTEM_PROMPT: "lean"}),
        ("unknown-system-prompt", {SYSTEM_PROMPT: "removed_after_graduation"}),
        ("empty-system-prompt", {SYSTEM_PROMPT: ""}),
        ("managed-shell-managed", {MANAGED_SHELL: "managed"}),
        ("managed-shell-legacy", {MANAGED_SHELL: "legacy"}),
        ("managed-shell-unknown", {MANAGED_SHELL: "something-else"}),
        ("routing-active-model", {MODEL_ROUTING: routing}),
        ("routing-with-model-config", {MODEL_ROUTING: routing_with_config}),
        ("routing-empty-object", {MODEL_ROUTING: "{}"}),
        ("routing-without-active-model", {MODEL_ROUTING: json.dumps({"a": 1})}),
        (
            "routing-empty-active-model",
            {MODEL_ROUTING: json.dumps({"active_model": ""})},
        ),
        (
            "routing-active-model-not-a-string",
            {MODEL_ROUTING: json.dumps({"active_model": 7})},
        ),
        (
            "routing-model-config-not-an-object",
            {MODEL_ROUTING: json.dumps({"active_model": ROUTED_ALIAS, "model_config": []})},
        ),
        ("routing-not-json", {MODEL_ROUTING: "not json at all"}),
        ("routing-json-array", {MODEL_ROUTING: "[1, 2]"}),
        ("routing-json-string", {MODEL_ROUTING: '"a string"'}),
        (
            "every-experiment",
            {
                SYSTEM_PROMPT: "tests",
                MANAGED_SHELL: "managed",
                MODEL_ROUTING: routing_with_config,
            },
        ),
        (
            "variants-that-map-to-nothing",
            {SYSTEM_PROMPT: "removed_after_graduation", MANAGED_SHELL: "legacy"},
        ),
        ("unknown-experiment-name", {"vibe_cli_unknown_rollout": "on"}),
    ]

    cases: list[dict[str, Any]] = []
    for identifier, variant_map in variants:
        layer = GrowthbookLayer()
        layer.set_variants(variant_map)
        data = asyncio.run(layer.load())
        cases.append(
            {
                "id": identifier,
                "variants": variant_map,
                "data": data.model_dump(),
                "hasFingerprint": layer.fingerprint
                not in (None, MISSING_BACKING_STORE_DATA_FINGERPRINT),
            }
        )

    layer = GrowthbookLayer()
    layer.set_variants({SYSTEM_PROMPT: "tests"})
    asyncio.run(layer.load())
    try:
        asyncio.run(layer._save_to_store({}))  # noqa: SLF001 - the refusal is the subject
    except NotImplementedError:
        refusal = "NotImplementedError"
    else:
        refusal = None
    cases.append(
        {
            "id": "persisting-is-refused",
            "variants": {SYSTEM_PROMPT: "tests"},
            "data": {"refusal": refusal},
            "hasFingerprint": None,
        }
    )
    return cases


# --------------------------------------------------------------------------
# layerPrecedence
# --------------------------------------------------------------------------

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


def capture_layer_precedence(scratch: Path) -> list[dict[str, Any]]:
    from vibe.core.config.harness_files import HarnessFilesManager
    from vibe.core.config.layers.growthbook import GrowthbookLayer
    from vibe.core.trusted_folders import TrustedFoldersManager

    cases: list[dict[str, Any]] = []
    for index, case in enumerate(PRECEDENCE_CASES):
        home = scratch / f"precedence-{index}"
        project = home / "project"
        (home / ".vibe").mkdir(parents=True, exist_ok=True)
        project.mkdir(parents=True, exist_ok=True)
        (home / ".vibe" / "config.toml").write_text(
            case["user"] + BASE_USER_DOCUMENT, encoding="utf-8"
        )
        if case["project"] is not None:
            (project / ".vibe").mkdir(parents=True, exist_ok=True)
            (project / ".vibe" / "config.toml").write_text(
                case["project"], encoding="utf-8"
            )
        environment: dict[str, str | None] = {
            "VIBE_HOME": str(home / ".vibe"),
            "VIBE_SYSTEM_PROMPT_ID": None,
        }
        environment.update(case["environment"])
        with patched_environ(environment):
            trust = TrustedFoldersManager()
            trust.trust_for_session(project)
            manager = HarnessFilesManager(
                sources=("user", "project"), cwd=project, trust_store=trust
            )
            observed = asyncio.run(
                _precedence_answer(case, manager, GrowthbookLayer.NAME)
            )
        cases.append({"id": case["id"], **observed})
    return cases


async def _precedence_answer(
    case: dict[str, Any], manager: Any, layer_name: str
) -> dict[str, Any]:
    from vibe.core.config import build_default_orchestrator

    orchestrator = await build_default_orchestrator(
        case["overrides"] or None, harness_files=manager, require_api_key=False
    )
    layer = orchestrator.get_layer(layer_name)
    layer.set_variants(case["variants"])
    await orchestrator.reload()
    config = orchestrator.config
    values: dict[str, Any] = {}
    for field in case["fields"]:
        if field == "resolvedDefaultAlias":
            values[field] = config.resolve_default_model_alias()
        elif field == "activeModelAlias":
            values[field] = config.get_active_model().alias
        else:
            values[field] = _json_value(getattr(config, field, None))
    return {
        "user": case["user"],
        "project": case["project"],
        "environment": case["environment"],
        "overrides": case["overrides"],
        "variants": case["variants"],
        "effective": values,
    }


def _json_value(value: Any) -> Any:
    """One effective configuration value, as JSON rather than as a repr.

    Reading a routed model config back off the schema hands out a validated
    model, and recording its ``repr`` would compare Python formatting instead of
    the value the layer wrote.
    """

    if value is None or isinstance(value, (str, int, float, bool)):
        return value
    if hasattr(value, "model_dump"):
        return value.model_dump(mode="json")
    if isinstance(value, list):
        return [_json_value(item) for item in value]
    if isinstance(value, dict):
        return {key: _json_value(item) for key, item in value.items()}
    return str(value)


# --------------------------------------------------------------------------
# sessionGates
# --------------------------------------------------------------------------


class _RecordingSessionLogger:
    """Stands where the reference's session logger stands: it records what would
    have been persisted and answers the metadata a hydration reads."""

    def __init__(self, metadata: Any = None) -> None:
        self.session_metadata = metadata
        self.persisted: list[Any] = []

    async def persist_experiments(self, response: Any) -> None:
        self.persisted.append(response)


def capture_session_gates(scratch: Path) -> list[dict[str, Any]]:
    from vibe.core.experiments.models import EvalResponse
    from vibe.core.experiments.session import (
        hydrate_experiments_from_session,
        initialize_experiments,
    )

    forced = {
        "features": {SYSTEM_PROMPT: feature("cli", [{"force": "tests", "tracks": [track()]}])}
    }

    cases: list[dict[str, Any]] = []
    for configuration in GATE_CONFIGURATIONS:
        for identity_present in (True, False):
            identity_calls: list[dict[str, Any]] = []

            async def resolve_identity(
                *, base_url: str, api_key: str, timeout: float | None = None
            ) -> Any:
                identity_calls.append(
                    {
                        "baseUrl": base_url,
                        "credentialVariable": variable_for(api_key),
                        "timeout": timeout,
                    }
                )
                if not identity_present:
                    return None
                from vibe.core.identity import IdentityResult

                return IdentityResult.model_validate(
                    {
                        "id": "oracle-user",
                        "organization": {
                            "id": "oracle-organization",
                            "name": "Oracle",
                        },
                    }
                )

            transport = RecordingEvalTransport(body=forced)
            manager = manager_with(None)
            manager._client = eval_client(  # noqa: SLF001
                transport, url=f"{ORACLE_API_HOST}/api/eval/{ORACLE_CLIENT_KEY}"
            )
            logger_stub = _RecordingSessionLogger()
            with patched_environ({"VIBE_HOME": str(scratch / "gates")}):
                with captured_logs():
                    refreshed = asyncio.run(
                        initialize_experiments(
                            config=build_config(configuration["toml"]),
                            manager=manager,
                            session_logger=logger_stub,
                            launch_context=None,
                            resolve_identity=resolve_identity,
                        )
                    )
            cases.append(
                {
                    "id": f"initialize/{configuration['id']}/"
                    + ("identity" if identity_present else "no-identity"),
                    "helper": "initialize",
                    "configuration": configuration["id"],
                    "returned": refreshed,
                    "evalRequests": len(transport.requests),
                    "identityRequests": len(identity_calls),
                    "identityTimeout": identity_calls[0]["timeout"]
                    if identity_calls
                    else None,
                    "persisted": len(logger_stub.persisted),
                    "organizationId": (
                        (transport.requests[0]["payload"] or {})
                        .get("attributes", {})
                        .get("organizationId")
                        if transport.requests
                        else None
                    ),
                }
            )

    # A failed lookup persists nothing and reports no refresh.
    for identifier, keywords in (
        ("eval-fails", {"status": 500, "body": {"features": {}}}),
        ("eval-returns-nothing", {"body": {"features": {}}}),
    ):
        transport = RecordingEvalTransport(**keywords)
        manager = manager_with(None)
        manager._client = eval_client(  # noqa: SLF001
            transport, url=f"{ORACLE_API_HOST}/api/eval/{ORACLE_CLIENT_KEY}"
        )
        logger_stub = _RecordingSessionLogger()
        identity_calls = []

        async def no_identity(
            *, base_url: str, api_key: str, timeout: float | None = None
        ) -> Any:
            identity_calls.append({"timeout": timeout})
            return None

        with patched_environ({"VIBE_HOME": str(scratch / "gates")}):
            with captured_logs():
                refreshed = asyncio.run(
                    initialize_experiments(
                        config=build_config(GATE_CONFIGURATIONS[0]["toml"]),
                        manager=manager,
                        session_logger=logger_stub,
                        launch_context=None,
                        resolve_identity=no_identity,
                    )
                )
        cases.append(
            {
                "id": f"initialize/{identifier}",
                "helper": "initialize",
                "configuration": GATE_CONFIGURATIONS[0]["id"],
                "returned": refreshed,
                "evalRequests": len(transport.requests),
                "identityRequests": len(identity_calls),
                "identityTimeout": identity_calls[0]["timeout"]
                if identity_calls
                else None,
                "persisted": len(logger_stub.persisted),
                "organizationId": None,
            }
        )

    # Hydration reads the same two gates and never issues a request.
    metadata_states: list[tuple[str, Any]] = [
        ("metadata-absent", None),
        ("metadata-without-experiments", _Metadata(None)),
        ("metadata-with-experiments", _Metadata(EvalResponse.model_validate(forced))),
    ]
    for configuration in GATE_CONFIGURATIONS[:4]:
        for identifier, metadata in metadata_states:
            transport = RecordingEvalTransport(body=forced)
            manager = manager_with(None)
            manager._client = eval_client(  # noqa: SLF001
                transport, url=f"{ORACLE_API_HOST}/api/eval/{ORACLE_CLIENT_KEY}"
            )
            logger_stub = _RecordingSessionLogger(metadata)
            with patched_environ({"VIBE_HOME": str(scratch / "gates")}):
                with captured_logs():
                    hydrated = asyncio.run(
                        hydrate_experiments_from_session(
                            config=build_config(configuration["toml"]),
                            manager=manager,
                            session_logger=logger_stub,
                        )
                    )
            cases.append(
                {
                    "id": f"hydrate/{configuration['id']}/{identifier}",
                    "helper": "hydrate",
                    "configuration": configuration["id"],
                    "returned": hydrated,
                    "evalRequests": len(transport.requests),
                    "identityRequests": 0,
                    "identityTimeout": None,
                    "persisted": len(logger_stub.persisted),
                    "organizationId": None,
                }
            )
    return cases


class _Metadata:
    """The one metadata field the hydration helper reads."""

    def __init__(self, experiments: Any) -> None:
        self.experiments = experiments


# --------------------------------------------------------------------------
# attributes
# --------------------------------------------------------------------------


def capture_attributes(scratch: Path) -> list[dict[str, Any]]:
    from vibe.core.experiments.session import _build_attributes
    from vibe.core.telemetry.types import LaunchContext

    contexts: list[tuple[str, Any]] = [
        ("no-launch-context", None),
        (
            "cli",
            LaunchContext(
                agent_entrypoint="cli",
                agent_version="9.9.9",
                client_name="vibe",
                client_version="9.9.9",
                terminal_emulator=None,
            ),
        ),
        (
            "cli-in-vscode",
            LaunchContext(
                agent_entrypoint="cli",
                agent_version="9.9.9",
                client_name="vibe",
                client_version="9.9.9",
                terminal_emulator="vscode",
            ),
        ),
        (
            "acp-in-cursor",
            LaunchContext(
                agent_entrypoint="acp",
                agent_version="9.9.9",
                client_name="zed",
                client_version="0.1.0",
                terminal_emulator="cursor",
            ),
        ),
        (
            "programmatic",
            LaunchContext(
                agent_entrypoint="programmatic",
                agent_version="9.9.9",
                client_name="harness",
                client_version="2.0.0",
                terminal_emulator="unknown",
            ),
        ),
    ]
    documents = [
        ("default-prompt", GATE_CONFIGURATIONS[0]["toml"]),
        ("custom-prompt", GATE_CONFIGURATIONS[6]["toml"]),
    ]

    cases: list[dict[str, Any]] = []
    with patched_environ({"VIBE_HOME": str(scratch / "attributes")}):
        for document_id, document in documents:
            config = build_config(document)
            for identifier, context in contexts:
                attributes = _build_attributes(
                    config,
                    SENTINELS["ORACLE_MISTRAL_KEY"],
                    context,
                    organization_id=(
                        "oracle-organization" if identifier != "no-launch-context" else None
                    ),
                )
                payload = attributes.model_dump(exclude_none=True)
                cases.append(
                    {
                        "id": f"{document_id}/{identifier}",
                        "document": document_id,
                        "context": identifier,
                        "attributes": scrub(attributes.model_dump()),
                        "payloadKeys": sorted(payload),
                        "credentialVariable": sentinel_variable(payload["userId"]),
                    }
                )
    return cases


# --------------------------------------------------------------------------
# The promo
# --------------------------------------------------------------------------


def capture_promo_constants() -> list[dict[str, Any]]:
    from vibe.cli.textual_ui.widgets.messages import (
        VSCODE_EXTENSION_LINK_LABEL,
        VSCODE_EXTENSION_URI,
    )
    from vibe.cli.vscode_extension_promo import MAX_SHOWN_COUNT, PROMO_START
    from vibe.cli.vscode_extension_promo.adapters.filesystem_repository import (
        _CACHE_SECTION,
        FileSystemVscodeExtensionPromoRepository,
    )

    # The reference names the file inline rather than exporting a constant, so
    # it is read off a repository built on a base path. Constructing one joins a
    # path and touches no disk.
    cache_file = FileSystemVscodeExtensionPromoRepository(
        Path("oracle-home")
    )._cache_file  # noqa: SLF001 - the file name is the subject

    return [
        {"id": "maxShownCount", "value": MAX_SHOWN_COUNT},
        {"id": "promoStart", "value": PROMO_START.isoformat()},
        {"id": "promoStartEpochSeconds", "value": int(PROMO_START.timestamp())},
        {"id": "cacheSection", "value": _CACHE_SECTION},
        {"id": "cacheFile", "value": cache_file.name},
        {"id": "extensionUri", "value": VSCODE_EXTENSION_URI},
        {"id": "linkLabel", "value": VSCODE_EXTENSION_LINK_LABEL},
    ]


def capture_promo_predicate() -> list[dict[str, Any]]:
    from datetime import UTC, datetime, timedelta

    from vibe.cli.vscode_extension_promo import PROMO_START, should_show_promo
    from vibe.cli.vscode_extension_promo._port import VscodeExtensionPromoState

    instants: list[tuple[str, Any]] = [
        ("a-day-before-the-start", PROMO_START - timedelta(days=1)),
        ("a-second-before-the-start", PROMO_START - timedelta(seconds=1)),
        ("exactly-at-the-start", PROMO_START),
        ("a-second-after-the-start", PROMO_START + timedelta(seconds=1)),
        ("long-after-the-start", datetime(2027, 1, 1, tzinfo=UTC)),
    ]
    states: list[tuple[str, Any]] = [
        ("absent", None),
        ("zero", VscodeExtensionPromoState(shown_count=0)),
        ("one-below-the-ceiling", VscodeExtensionPromoState(shown_count=9)),
        ("at-the-ceiling", VscodeExtensionPromoState(shown_count=10)),
        ("above-the-ceiling", VscodeExtensionPromoState(shown_count=11)),
    ]
    return [
        {
            "id": f"{state_id}/{instant_id}",
            "shownCount": None if state is None else state.shown_count,
            "instant": instant.isoformat(),
            "show": should_show_promo(state, instant),
        }
        for state_id, state in states
        for instant_id, instant in instants
    ]


def capture_promo_state(scratch: Path) -> list[dict[str, Any]]:
    from vibe.cli.vscode_extension_promo._port import VscodeExtensionPromoState
    from vibe.cli.vscode_extension_promo.adapters.filesystem_repository import (
        FileSystemVscodeExtensionPromoRepository,
    )

    documents: list[tuple[str, str | None]] = [
        ("no-cache-file", None),
        ("empty-cache-file", ""),
        ("section-absent", '[update_cache]\nlatest_version = "1.0.0"\n'),
        ("empty-section", "[vscode_extension_promo]\n"),
        ("valid-count", "[vscode_extension_promo]\nshown_count = 4\n"),
        ("zero-count", "[vscode_extension_promo]\nshown_count = 0\n"),
        ("count-is-a-string", '[vscode_extension_promo]\nshown_count = "4"\n'),
        ("count-is-a-float", "[vscode_extension_promo]\nshown_count = 4.0\n"),
        ("count-is-a-boolean", "[vscode_extension_promo]\nshown_count = true\n"),
        ("count-is-negative", "[vscode_extension_promo]\nshown_count = -1\n"),
        ("malformed-toml", "[vscode_extension_promo\nshown_count = 4\n"),
        (
            "other-keys-in-the-section",
            "[vscode_extension_promo]\nunrelated = 1\nshown_count = 6\n",
        ),
    ]

    cases: list[dict[str, Any]] = []
    for index, (identifier, document) in enumerate(documents):
        home = scratch / f"promo-{index}"
        home.mkdir(parents=True, exist_ok=True)
        if document is not None:
            (home / "cache.toml").write_text(document, encoding="utf-8")
        repository = FileSystemVscodeExtensionPromoRepository(home)
        state = asyncio.run(repository.get())
        cases.append(
            {
                "id": identifier,
                "document": document,
                "shownCount": None if state is None else state.shown_count,
                "documentAfterWrite": None,
            }
        )

    # Writing the promo section preserves the update cache beside it.
    home = scratch / "promo-write"
    home.mkdir(parents=True, exist_ok=True)
    (home / "cache.toml").write_text(
        '[update_cache]\nlatest_version = "1.2.3"\nstored_at_timestamp = 42\n',
        encoding="utf-8",
    )
    repository = FileSystemVscodeExtensionPromoRepository(home)
    asyncio.run(repository.set(VscodeExtensionPromoState(shown_count=3)))
    written = asyncio.run(repository.get())
    cases.append(
        {
            "id": "write-preserves-siblings",
            "document": '[update_cache]\nlatest_version = "1.2.3"\nstored_at_timestamp = 42\n',
            "shownCount": None if written is None else written.shown_count,
            "documentAfterWrite": _read_toml(home / "cache.toml"),
        }
    )

    # Writing into a home with no cache file at all creates one.
    home = scratch / "promo-create"
    home.mkdir(parents=True, exist_ok=True)
    repository = FileSystemVscodeExtensionPromoRepository(home)
    asyncio.run(repository.set(VscodeExtensionPromoState(shown_count=1)))
    asyncio.run(repository.set(VscodeExtensionPromoState(shown_count=2)))
    written = asyncio.run(repository.get())
    cases.append(
        {
            "id": "write-creates-the-file",
            "document": None,
            "shownCount": None if written is None else written.shown_count,
            "documentAfterWrite": _read_toml(home / "cache.toml"),
        }
    )
    return cases


def _read_toml(path: Path) -> Any:
    import tomllib

    try:
        with path.open("rb") as handle:
            return tomllib.load(handle)
    except (OSError, tomllib.TOMLDecodeError):
        return None


def capture_promo_prose() -> list[dict[str, Any]]:
    from vibe.cli.textual_ui.widgets.messages import (
        VSCODE_EXTENSION_PROMO_STANDALONE,
        VSCODE_EXTENSION_PROMO_WHATS_NEW_SUFFIX,
    )

    return [
        {"id": "standalone", **digest(VSCODE_EXTENSION_PROMO_STANDALONE)},
        {"id": "whatsNewSuffix", **digest(VSCODE_EXTENSION_PROMO_WHATS_NEW_SUFFIX)},
    ]


# --------------------------------------------------------------------------
# Assembly
# --------------------------------------------------------------------------


def build_engine_families(scratch: Path) -> dict[str, Any]:
    return {
        "constants": capture_constants(),
        "bucketingKey": capture_bucketing_key(),
        "evalUrl": capture_eval_url(),
        "evalRequest": capture_eval_request(),
        "evalFailures": capture_eval_failures(),
        "featureResolution": capture_feature_resolution(),
        "variantResolution": capture_variant_resolution(),
        "configVariants": capture_config_variants(),
        "variantLabels": capture_variant_labels(),
        "configMapping": capture_config_mapping(),
        "layerPrecedence": capture_layer_precedence(scratch),
        "sessionGates": capture_session_gates(scratch),
        "attributes": capture_attributes(scratch),
    }


def build_promo_families(scratch: Path) -> dict[str, Any]:
    return {
        "promoConstants": capture_promo_constants(),
        "promoPredicate": capture_promo_predicate(),
        "promoState": capture_promo_state(scratch),
        "promoProse": capture_promo_prose(),
    }


ENGINE_NOTE = (
    "Experiments corpus: what the pinned reference's remote eval client, experiment manager, "
    "GrowthBook configuration layer and session helpers answer for each scripted input. The eval "
    "request is intercepted one call before the connection and a socket guard fails the run on any "
    "connection attempt, so no scenario depends on a live rollout: in remote evaluation mode the "
    "proxy resolves the bucketing and the client only applies the answer. A credential is recorded "
    "as the environment variable it came from and never as a value, and the only key derivative "
    "committed is the 32-character bucketing digest. Regenerate with scripts/parity/experiments.py "
    "--corpus when the pinned reference moves."
)

PROMO_NOTE = (
    "VS Code promo corpus: the ceiling, the start instant, the predicate, and what the pinned "
    "reference's filesystem repository reads back out of each cache.toml document. The two "
    "reference sentences are committed as a byte length and a SHA-256 because NOTICE forbids "
    "shipping reference-authored prose; the replay asserts this port's own sentences never match "
    "either digest. Regenerate with scripts/parity/experiments.py --corpus when the pinned "
    "reference moves."
)


def build_corpus(commit: str, families: dict[str, Any], note: str) -> dict[str, Any]:
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": {"commit": commit},
        "note": note,
        **families,
    }


def assert_no_credential_leaked(corpus: dict[str, Any]) -> None:
    encoded = json.dumps(corpus)
    for name, sentinel in SENTINELS.items():
        if sentinel in encoded:
            raise OracleError(
                f"the corpus carries the value of {name}; record the variable instead"
            )


def count_comparisons(families: dict[str, Any]) -> dict[str, int]:
    return {
        family: len(value) if isinstance(value, list) else 1
        for family, value in families.items()
    }


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--python", type=Path, default=None)
    parser.add_argument(
        "--corpus",
        type=Path,
        nargs="?",
        const=DEFAULT_CORPUS,
        default=None,
        help=(
            "also write the committed engine corpus, which the Rust replay reads "
            f"unconditionally (default {DEFAULT_CORPUS})"
        ),
    )
    parser.add_argument(
        "--promo-corpus",
        type=Path,
        default=None,
        help=(
            "where the committed promo corpus goes; defaults beside --corpus at "
            f"{DEFAULT_PROMO_CORPUS}"
        ),
    )
    parser.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def write_json(path: Path, document: dict[str, Any]) -> None:
    """Write one corpus, keeping every key in the order it was authored in.

    Key order is load-bearing here rather than cosmetic. An object-valued
    variant is answered as ``json.dumps`` of the value the eval response
    carried, which is the wire order, so a writer that sorted the recorded
    response would hand a replay an input that cannot produce the recorded
    answer. Every dict this capture builds is built deterministically, so
    insertion order is as stable as a sort would be.
    """

    path.parent.mkdir(parents=True, exist_ok=True)
    staged = path.with_name(f"{path.name}.{os.getpid()}.tmp")
    staged.write_text(
        json.dumps(document, indent=2, sort_keys=False, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    os.replace(staged, path)


def main() -> int:
    arguments = parse_arguments()
    try:
        reference = resolve_reference(arguments.reference, arguments.expected_commit)
        pinned = extract_pinned_tree(
            arguments.reference, reference["commit"], arguments.cache
        )
        reexecute_with_reference_interpreter(
            arguments.reference, arguments.python, pinned
        )
        install_sentinels()
        GUARD.install()
        with tempfile.TemporaryDirectory(prefix="experiments-oracle-") as scratch_name:
            scratch = Path(scratch_name)
            with patched_environ({"VIBE_HOME": str(scratch / "home")}):
                from vibe.core.config.harness_files import init_harness_files_manager
                from vibe.utils import api_keys

                # The prompt validators reach the harness-files singleton, and a
                # keyring lookup would leave the process, so the credential
                # sources are the sentinels and nothing else.
                init_harness_files_manager("user")
                api_keys.get_api_key_from_keyring = lambda _: None
                engine = build_engine_families(scratch)
                promo = build_promo_families(scratch)
    except OracleError as error:
        print(f"experiments capture failed: {error}", file=sys.stderr)
        return 1

    if GUARD.attempts:
        print(
            f"experiments capture failed: network attempts {GUARD.attempts}",
            file=sys.stderr,
        )
        return 1

    engine_corpus = build_corpus(reference["commit"], engine, ENGINE_NOTE)
    promo_corpus = build_corpus(reference["commit"], promo, PROMO_NOTE)
    try:
        assert_no_credential_leaked(engine_corpus)
        assert_no_credential_leaked(promo_corpus)
    except OracleError as error:
        print(f"experiments capture failed: {error}", file=sys.stderr)
        return 1

    write_json(
        arguments.output,
        {
            "engine": engine_corpus,
            "promo": promo_corpus,
            "reference": reference,
            "platform": platform.system().lower(),
            "python": platform.python_version(),
        },
    )
    if arguments.corpus is not None:
        write_json(arguments.corpus, engine_corpus)
        promo_path = arguments.promo_corpus or DEFAULT_PROMO_CORPUS
        write_json(promo_path, promo_corpus)

    counted = {**count_comparisons(engine), **count_comparisons(promo)}
    total = sum(counted.values())
    print(
        f"captured {total} records across {len(counted)} families "
        f"from {reference['commit'][:12]} into {arguments.output}"
    )
    for family, count in counted.items():
        print(f"  {family}: {count}")
    if arguments.corpus is not None:
        print(f"wrote the committed corpora to {arguments.corpus} and {promo_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
