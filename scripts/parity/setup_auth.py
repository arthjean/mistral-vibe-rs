#!/usr/bin/env python3
"""Capture what the pinned Python reference's authentication logic answers.

The reference's setup authentication splits into injectable seams, and this
capture drives every one of them with no network, no browser and no OS
credential store. ``assess_auth_state`` takes its environment, its dotenv path
and its pre-load flag as arguments and reads the keyring through one function
whose primitives are patched to scripted answers. ``persist_api_key`` and
``remove_api_key`` run over a scratch ``VIBE_HOME`` with the same patched
primitives. ``BrowserSignInService`` takes its gateway, its browser opener, its
sleep function and its clock as constructor arguments, so a stub gateway and a
fake clock observe the whole state machine. ``HttpBrowserSignInGateway``
accepts its HTTP client, so a recording stub captures every request it would
have issued and feeds it scripted ``httpx.Response`` objects.

Eleven families come out, and are what the Rust replay compares:

``authState``            the six provenance states over the five-source matrix
``persistence``          save, fallback, removal and the keyring read-and-migrate
``signInProtocol``       the service state machine and the gateway wire behavior
``urlValidation``        the origin and path-prefix verdict per server URL
``urlRewrite``           the URL a split-horizon gateway re-homes each one onto
``tenantDomains``        what ``/whoami`` tenant discovery adopts and refuses
``providerCredentials``  the configuration a sign-in's batched write leaves
``acpSignIn``            what the editor-protocol sign-in resolves and writes
``tenantReconcile``      what an account read heals in the configuration
``errorTaxonomy``        the eleven error codes and their message digests
``acpAuthProse``         the editor-protocol method labels, as length and digest

Two artifacts come out of a run::

    .parity/setup-auth-corpus.json               the full capture, gitignored
    crates/vibe-core/tests/setup-auth/corpus.json  the committed corpus

The committed corpus carries scenario-supplied values, vocabulary, counts,
call orders and verdicts. Anything reference-authored, which is the error
message sentences and the ACP method labels, is committed as a length plus a
SHA-256: a digest still fails the replay on any change while no reference
sentence ships.

The capture asserts its own isolation: a socket guard fails the run if any
code path attempts a connection or a DNS resolution, so a scenario that
accidentally reaches a real backend cannot write a corpus.

Usage::

    scripts/parity/setup_auth.py --reference /path/to/reference --corpus

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it. The wrapper re-executes itself with
the reference interpreter, reading the pinned commit through ``git archive`` so
the checkout is never moved.
"""

from __future__ import annotations

import argparse
import ast
import asyncio
import contextlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import stat
import sys
import tempfile
from datetime import UTC, datetime, timedelta
from types import SimpleNamespace
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT
from reference import DEFAULT_CACHE, OracleError, reexecute_pinned
from inprocess import SocketGuard, digest, patched, patched_environ
from setup_auth_cases import (
    API_BASE,
    BROWSER_BASE,
    TENANT_ANSWERS,
    acp_sign_in_cases,
    auth_state_cases,
    gateway_cases,
    provider_credentials_cases,
    service_cases,
    tenant_reconcile_cases,
    tenant_sanitize_cases,
    url_rewrite_cases,
    url_validation_cases,
)

SCHEMA_VERSION = 4
DEFAULT_OUTPUT = Path(".parity/setup-auth-corpus.json")
DEFAULT_CORPUS = Path("crates/vibe-core/tests/setup-auth/corpus.json")
#: Stable stand-ins for machine-dependent paths in committed records.
VIBE_HOME_PLACEHOLDER = "__VIBE_HOME__"

#: The fixed instant every sign-in scenario starts from, so expiries and the
#: recorded timeline are identical across runs and machines.
CLOCK_START = datetime(2026, 1, 1, 0, 0, 0, tzinfo=UTC)

#: The scripted PKCE verifier, chosen by this script so the derived challenge
#: is a committable value rather than a random one.
SCRIPTED_VERIFIER = "oracle-verifier-0123456789abcdefghijklmnopqrstuvwxyz-ABCDEFGH_~.-tail"


# --------------------------------------------------------------------------
# Isolation
# --------------------------------------------------------------------------


GUARD = SocketGuard()


@contextlib.contextmanager
def scratch_vibe_home():
    """A throwaway ``VIBE_HOME`` the persistence scenarios write into."""

    with tempfile.TemporaryDirectory(prefix="setup-auth-oracle-") as scratch:
        home = Path(scratch) / "vibe-home"
        home.mkdir()
        with patched_environ({"VIBE_HOME": str(home)}):
            yield home


# --------------------------------------------------------------------------
# authState: the provenance matrix
# --------------------------------------------------------------------------

CUSTOM_ENV_KEY = "ORACLE_CUSTOM_KEY"


def _refuse_keyring_write(*arguments: Any, **keywords: Any) -> Any:
    """Stands in for a keyring primitive no auth-state scenario may reach."""

    raise OracleError("the auth-state capture attempted a real keyring write")


def capture_auth_state() -> list[dict[str, Any]]:
    from vibe.core.config import DEFAULT_MISTRAL_API_ENV_KEY, DEFAULT_PROVIDERS
    from vibe.setup.auth.auth_state import assess_auth_state
    from vibe.utils import keyring as keyring_module

    mistral = next(p for p in DEFAULT_PROVIDERS if p.name == "mistral")
    records: list[dict[str, Any]] = []
    for case in auth_state_cases():
        env_key = {
            "default": DEFAULT_MISTRAL_API_ENV_KEY,
            "custom": CUSTOM_ENV_KEY,
            "empty": "",
        }[case["envKeyKind"]]
        provider = mistral.model_copy(update={"api_key_env_var": env_key})

        keyring_value = case["keyring"]

        def scripted_get(
            service: str, username: str, _value: str | None = keyring_value
        ) -> str | None:
            if service == "ai.mistral.vibe":
                return _value
            return None

        with tempfile.TemporaryDirectory(prefix="setup-auth-state-") as scratch:
            root = Path(scratch)
            env_path = root / "global.env"
            if case["dotenv"] == "value":
                env_path.write_text(f"{env_key}=oracle-dotenv-value\n", newline="")
            elif case["dotenv"] == "empty":
                env_path.write_text(f"{env_key}=\n", newline="")
            elif case["dotenv"] == "directory":
                env_path.mkdir()
            elif case["dotenv"] == "unreadable":
                env_path.write_text(f"{env_key}=oracle-dotenv-value\n", newline="")
                env_path.chmod(0)

            environ: dict[str, str] = {}
            if case["processEnv"] is not None:
                environ[env_key] = case["processEnv"]

            keyring_module.clear_api_key_keyring_cache()
            # `_get_password` alone is the only primitive these scenarios
            # reach, because none of them seeds a legacy service and so none
            # triggers the read-and-migrate write. Refusing the write and the
            # delete outright makes "no OS credential store" a property of the
            # harness rather than of the scenario table.
            with (
                patched(keyring_module, "_get_password", scripted_get),
                patched(keyring_module, "_set_password", _refuse_keyring_write),
                patched(keyring_module, "_delete_password", _refuse_keyring_write),
            ):
                try:
                    state = assess_auth_state(
                        provider,
                        env_path=env_path,
                        environ=environ,
                        process_env_had_value_before_dotenv_load=case[
                            "hadValueBeforeDotenvLoad"
                        ],
                    )
                    outcome: dict[str, Any] = {
                        "kind": state.kind.value,
                        "canUseActiveProvider": state.can_use_active_provider,
                        "signOutAvailable": state.sign_out_available,
                        "reportedEnvKey": state.env_key,
                    }
                except Exception as error:  # noqa: BLE001 - the verdict is the record
                    outcome = {"raised": type(error).__name__}
            keyring_module.clear_api_key_keyring_cache()
            if case["dotenv"] == "unreadable":
                env_path.chmod(stat.S_IRUSR | stat.S_IWUSR)
        records.append({**case, **outcome})
    return records


# --------------------------------------------------------------------------
# persistence: save, fallback, removal, read-and-migrate
# --------------------------------------------------------------------------


class _KeyringScript:
    """Scripted keyring primitives that record every call in order."""

    def __init__(
        self,
        *,
        stored: dict[str, str] | None = None,
        set_error: str | None = None,
        get_error: str | None = None,
        delete_errors: dict[str, str] | None = None,
    ) -> None:
        self.stored = dict(stored or {})
        self.set_error = set_error
        self.get_error = get_error
        self.delete_errors = dict(delete_errors or {})
        self.calls: list[str] = []

    def install(self):
        from keyring.errors import KeyringError, NoKeyringError, PasswordDeleteError
        from vibe.utils import keyring as keyring_module

        script = self

        errors = {
            "keyring": KeyringError,
            "no-keyring": NoKeyringError,
            "delete-missing": PasswordDeleteError,
        }

        def get_password(service: str, username: str) -> str | None:
            script.calls.append(f"get:{service}")
            if script.get_error is not None:
                raise errors[script.get_error]("scripted get failure")
            return script.stored.get(service)

        def set_password(service: str, username: str, password: str) -> None:
            script.calls.append(f"set:{service}")
            if script.set_error is not None:
                raise errors[script.set_error]("scripted set failure")
            script.stored[service] = password

        def delete_password(service: str, username: str) -> None:
            script.calls.append(f"delete:{service}")
            if service in script.delete_errors:
                raise errors[script.delete_errors[service]](
                    "scripted delete failure"
                )
            if service not in script.stored:
                raise PasswordDeleteError("nothing stored")
            del script.stored[service]

        stack = contextlib.ExitStack()
        stack.enter_context(patched(keyring_module, "_get_password", get_password))
        stack.enter_context(patched(keyring_module, "_set_password", set_password))
        stack.enter_context(
            patched(keyring_module, "_delete_password", delete_password)
        )
        return stack


class _TelemetryRecorder:
    instances: list[_TelemetryRecorder] = []

    def __init__(self, **_: Any) -> None:
        self.events: list[dict[str, Any]] = []
        _TelemetryRecorder.instances.append(self)

    def send_onboarding_api_key_added(self, *, custom_domain: bool) -> None:
        self.events.append({"event": "apiKeyAdded", "customDomain": custom_domain})


def _read_env_file(home: Path, env_key: str) -> str | None:
    from dotenv import dotenv_values

    env_file = home / ".env"
    if not env_file.is_file():
        return None
    value = dotenv_values(env_file).get(env_key)
    return value if isinstance(value, str) else None


def capture_persistence() -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    records.extend(_capture_persist_cases())
    records.extend(_capture_remove_cases())
    records.extend(_capture_keyring_read_cases())
    return records


def _capture_persist_cases() -> list[dict[str, Any]]:
    from vibe.core.config import DEFAULT_MISTRAL_API_ENV_KEY, DEFAULT_PROVIDERS
    from vibe.setup.auth import api_key_persistence
    from vibe.utils import keyring as keyring_module

    mistral = next(p for p in DEFAULT_PROVIDERS if p.name == "mistral")
    env_key = DEFAULT_MISTRAL_API_ENV_KEY

    async def fake_orchestrator() -> Any:
        return SimpleNamespace(config=None)

    cases: list[dict[str, Any]] = [
        {
            "case": "persist-keyring-success-removes-stale-dotenv",
            "script": _KeyringScript(),
            "seedDotenv": True,
            "customDomain": False,
        },
        {
            "case": "persist-keyring-success-custom-domain-telemetry",
            "script": _KeyringScript(),
            "seedDotenv": False,
            "customDomain": True,
        },
        {
            "case": "persist-keyring-failure-falls-back-to-dotenv",
            "script": _KeyringScript(set_error="keyring"),
            "seedDotenv": False,
            "customDomain": False,
        },
        {
            "case": "persist-no-backend-falls-back-to-dotenv",
            "script": _KeyringScript(set_error="no-keyring"),
            "seedDotenv": False,
            "customDomain": False,
        },
        {
            "case": "persist-empty-env-var",
            "script": _KeyringScript(),
            "seedDotenv": False,
            "customDomain": False,
            "envKey": "",
        },
        {
            "case": "persist-keyring-and-dotenv-both-fail",
            "script": _KeyringScript(set_error="keyring"),
            "seedDotenv": False,
            "customDomain": False,
            "breakDotenv": True,
        },
        {
            "case": "persist-stale-dotenv-removal-failure-still-completes",
            "script": _KeyringScript(),
            "seedDotenv": True,
            "customDomain": False,
            "breakUnset": True,
        },
    ]

    records = []
    for case in cases:
        script: _KeyringScript = case["script"]
        case_env_key = case.get("envKey", env_key)
        provider = mistral.model_copy(update={"api_key_env_var": case_env_key})
        _TelemetryRecorder.instances = []
        keyring_module.clear_api_key_keyring_cache()
        with contextlib.ExitStack() as stack:
            home = stack.enter_context(scratch_vibe_home())
            stack.enter_context(script.install())
            stack.enter_context(patched_environ({case_env_key or "ORACLE_UNSET": None}))
            stack.enter_context(
                patched(
                    api_key_persistence,
                    "build_default_orchestrator",
                    fake_orchestrator,
                )
            )
            stack.enter_context(
                patched(api_key_persistence, "TelemetryClient", _TelemetryRecorder)
            )
            if case.get("breakUnset"):

                def failing_unset(path: Any, key: str) -> None:
                    raise OSError("scripted unset failure")

                stack.enter_context(
                    patched(api_key_persistence, "unset_key", failing_unset)
                )
            if case.get("seedDotenv"):
                (home / ".env").write_text(f"{case_env_key}=stale-dotenv-copy\n", newline="")
            if case.get("breakDotenv"):
                # The env-file parent is a *file*, so the fallback mkdir fails.
                broken = home / "not-a-directory"
                broken.write_text("occupied", newline="")
                stack.enter_context(
                    patched_environ({"VIBE_HOME": str(broken / "nested")})
                )
                home = broken / "nested"

            outcome = api_key_persistence.persist_api_key(
                provider,
                "oracle-api-key",
                custom_domain=case["customDomain"],
            )

            process_value = os.environ.get(case_env_key) if case_env_key else None
            dotenv_value = (
                _read_env_file(home, case_env_key) if case_env_key else None
            )
            telemetry = [
                event
                for recorder in _TelemetryRecorder.instances
                for event in recorder.events
            ]
        keyring_module.clear_api_key_keyring_cache()
        outcome_kind, _, outcome_detail = outcome.partition(":")
        records.append(
            {
                "case": case["case"],
                "op": "persist",
                "envKey": case_env_key,
                "customDomain": case["customDomain"],
                # The save_error detail is an OS-authored sentence and is not
                # deterministic across machines, so only its kind commits.
                "outcome": outcome
                if outcome_kind != "save_error"
                else "save_error",
                "outcomeDetailPresent": bool(outcome_detail),
                "processEnvValue": process_value,
                "dotenvValue": dotenv_value,
                "keyringStored": dict(sorted(script.stored.items())),
                "keyringCalls": script.calls,
                "telemetry": telemetry,
            }
        )
    return records


def _capture_remove_cases() -> list[dict[str, Any]]:
    from vibe.core.config import DEFAULT_MISTRAL_API_ENV_KEY, DEFAULT_PROVIDERS
    from vibe.setup.auth import api_key_persistence
    from vibe.utils import keyring as keyring_module

    mistral = next(p for p in DEFAULT_PROVIDERS if p.name == "mistral")
    env_key = DEFAULT_MISTRAL_API_ENV_KEY

    cases: list[dict[str, Any]] = [
        {
            "case": "remove-clears-both-services-dotenv-and-environ",
            "script": _KeyringScript(
                stored={
                    "ai.mistral.vibe": "current-value",
                    "vibe": "legacy-value",
                }
            ),
        },
        {
            "case": "remove-with-nothing-stored-is-a-no-op",
            "script": _KeyringScript(),
        },
        {
            "case": "remove-with-no-backend-still-clears-the-rest",
            "script": _KeyringScript(
                delete_errors={
                    "ai.mistral.vibe": "no-keyring",
                    "vibe": "no-keyring",
                }
            ),
        },
        {
            "case": "remove-with-a-real-backend-error-clears-then-raises",
            "script": _KeyringScript(
                stored={"vibe": "legacy-value"},
                delete_errors={"ai.mistral.vibe": "keyring"},
            ),
        },
        {
            "case": "remove-empty-env-var-is-refused",
            "script": _KeyringScript(),
            "envKey": "",
        },
    ]

    records = []
    for case in cases:
        script: _KeyringScript = case["script"]
        case_env_key = case.get("envKey", env_key)
        provider = mistral.model_copy(update={"api_key_env_var": case_env_key})
        keyring_module.clear_api_key_keyring_cache()
        with contextlib.ExitStack() as stack:
            home = stack.enter_context(scratch_vibe_home())
            stack.enter_context(script.install())
            if case_env_key:
                stack.enter_context(
                    patched_environ({case_env_key: "process-copy"})
                )
                (home / ".env").write_text(f"{case_env_key}=dotenv-copy\n", newline="")

            raised: str | None = None
            try:
                api_key_persistence.remove_api_key(provider)
            except Exception as error:  # noqa: BLE001 - the verdict is the record
                raised = type(error).__name__

            process_value = os.environ.get(case_env_key) if case_env_key else None
            dotenv_value = (
                _read_env_file(home, case_env_key) if case_env_key else None
            )
        keyring_module.clear_api_key_keyring_cache()
        records.append(
            {
                "case": case["case"],
                "op": "remove",
                "envKey": case_env_key,
                "raised": raised,
                "processEnvValue": process_value,
                "dotenvValue": dotenv_value,
                "keyringStored": dict(sorted(script.stored.items())),
                "keyringCalls": script.calls,
            }
        )
    return records


def _capture_keyring_read_cases() -> list[dict[str, Any]]:
    from vibe.utils import keyring as keyring_module

    env_key = "ORACLE_READ_KEY"
    cases: list[dict[str, Any]] = [
        {
            "case": "read-resolves-from-the-current-service-first",
            "script": _KeyringScript(
                stored={"ai.mistral.vibe": "current-value", "vibe": "legacy-value"}
            ),
        },
        {
            "case": "read-falls-back-to-legacy-and-migrates",
            "script": _KeyringScript(stored={"vibe": "legacy-value"}),
        },
        {
            "case": "read-migration-write-failure-still-returns-the-key",
            "script": _KeyringScript(
                stored={"vibe": "legacy-value"}, set_error="keyring"
            ),
        },
        {
            "case": "read-migration-delete-failure-still-returns-the-key",
            "script": _KeyringScript(
                stored={"vibe": "legacy-value"},
                delete_errors={"vibe": "keyring"},
            ),
        },
        {
            "case": "read-nothing-stored-anywhere",
            "script": _KeyringScript(),
        },
        {
            "case": "read-backend-error-reads-as-absent",
            "script": _KeyringScript(get_error="keyring"),
        },
        {
            "case": "read-empty-env-key-consults-nothing",
            "script": _KeyringScript(stored={"ai.mistral.vibe": "current-value"}),
            "envKey": "",
        },
        {
            "case": "read-disabled-consults-nothing",
            "script": _KeyringScript(stored={"ai.mistral.vibe": "current-value"}),
            "disabled": True,
        },
        {
            "case": "read-second-lookup-is-served-from-the-cache",
            "script": _KeyringScript(stored={"ai.mistral.vibe": "current-value"}),
            "readTwice": True,
        },
    ]

    records = []
    for case in cases:
        script: _KeyringScript = case["script"]
        case_env_key = case.get("envKey", env_key)
        keyring_module.clear_api_key_keyring_cache()
        with contextlib.ExitStack() as stack:
            stack.enter_context(script.install())
            stack.enter_context(
                patched_environ(
                    {"VIBE_TEST_DISABLE_KEYRING": "1" if case.get("disabled") else None}
                )
            )
            value = keyring_module.get_api_key_from_keyring(case_env_key)
            if case.get("readTwice"):
                value = keyring_module.get_api_key_from_keyring(case_env_key)
        keyring_module.clear_api_key_keyring_cache()
        records.append(
            {
                "case": case["case"],
                "op": "keyringRead",
                "envKey": case_env_key,
                "value": value,
                "keyringStored": dict(sorted(script.stored.items())),
                "keyringCalls": script.calls,
            }
        )
    return records


# --------------------------------------------------------------------------
# signInProtocol: the service state machine over a stub gateway
# --------------------------------------------------------------------------


class _FakeClock:
    def __init__(self) -> None:
        self.now = CLOCK_START
        self.sleeps: list[float] = []

    def __call__(self) -> datetime:
        return self.now

    async def sleep(self, seconds: float) -> None:
        self.sleeps.append(seconds)
        self.now += timedelta(seconds=seconds)


class _StubGateway:
    """Scripted gateway: create yields a fixed process, polls follow a list."""

    def __init__(
        self,
        *,
        expires_in: float = 600.0,
        polls: list[Any] | None = None,
        create_error: str | None = None,
        exchange: Any = "oracle-exchanged-api-key",
    ) -> None:
        self.expires_in = expires_in
        self.polls = list(polls or [])
        self.create_error = create_error
        self.exchange_result = exchange
        self.calls: list[str] = []
        self.closed = 0
        self.challenge: str | None = None

    async def create_process(self, code_challenge: str) -> Any:
        from vibe.setup.auth.browser_sign_in_gateway import (
            BrowserSignInError,
            BrowserSignInErrorCode,
            BrowserSignInProcess,
        )

        self.calls.append("create")
        self.challenge = code_challenge
        if self.create_error is not None:
            raise BrowserSignInError(
                "scripted start failure",
                code=BrowserSignInErrorCode(self.create_error),
            )
        return BrowserSignInProcess(
            process_id="oracle-process",
            sign_in_url="https://console.mistral.ai/oracle/sign-in",
            poll_url="https://console.mistral.ai/api/oracle/poll",
            expires_at=CLOCK_START + timedelta(seconds=self.expires_in),
        )

    async def poll(self, poll_url: str) -> Any:
        from vibe.setup.auth.browser_sign_in_gateway import (
            BrowserSignInError,
            BrowserSignInErrorCode,
            BrowserSignInPollResult,
        )

        self.calls.append("poll")
        if not self.polls:
            step: Any = {"status": "pending"}
        else:
            step = self.polls.pop(0)
        if step.get("raise"):
            raise BrowserSignInError(
                "scripted poll failure",
                code=BrowserSignInErrorCode(step["raise"]),
            )
        return BrowserSignInPollResult(
            status=step["status"],
            exchange_token=step.get("exchangeToken"),
            message=step.get("message"),
        )

    async def exchange(
        self, process_id: str, exchange_token: str, code_verifier: str
    ) -> str:
        from vibe.setup.auth.browser_sign_in_gateway import (
            BrowserSignInError,
            BrowserSignInErrorCode,
        )

        self.calls.append(f"exchange:{process_id}:{exchange_token}")
        if isinstance(self.exchange_result, dict):
            raise BrowserSignInError(
                "scripted exchange failure",
                code=BrowserSignInErrorCode(self.exchange_result["raise"]),
            )
        return self.exchange_result

    async def aclose(self) -> None:
        self.closed += 1


def capture_service() -> tuple[list[dict[str, Any]], dict[str, list[str]]]:
    from vibe.setup.auth import browser_sign_in
    from vibe.setup.auth.browser_sign_in import (
        BrowserSignInAttemptStarted,
        BrowserSignInService,
        BrowserSignInStatusChanged,
    )
    from vibe.setup.auth.browser_sign_in_gateway import BrowserSignInError

    records = []
    messages: dict[str, list[str]] = {}
    for case in service_cases():
        gateway = _StubGateway(
            expires_in=case.get("expiresIn", 600.0),
            polls=case.get("polls"),
            create_error=case.get("createError"),
            exchange=case.get("exchange", "oracle-exchanged-api-key"),
        )
        clock = _FakeClock()
        opened: list[str] = []
        opener_kind = case.get("opener", "accept")

        def opener(url: str, _kind: str = opener_kind, _opened: list[str] = opened) -> bool:
            _opened.append(url)
            if _kind == "raise":
                raise RuntimeError("scripted opener failure")
            return _kind != "refuse"

        events: list[dict[str, Any]] = []

        def callback(event: Any, _events: list[dict[str, Any]] = events) -> None:
            if isinstance(event, BrowserSignInAttemptStarted):
                _events.append(
                    {
                        "event": "attemptStarted",
                        "signInUrl": event.sign_in_url,
                        "expiresAt": event.expires_at.isoformat(),
                    }
                )
            elif isinstance(event, BrowserSignInStatusChanged):
                _events.append({"event": "status", "status": event.status.value})

        service = BrowserSignInService(
            gateway,  # type: ignore[arg-type]
            open_browser=opener,
            sleep=clock.sleep,
            now=clock,
        )

        with patched(
            browser_sign_in, "_generate_code_verifier", lambda: SCRIPTED_VERIFIER
        ):
            try:
                api_key = asyncio.run(service.authenticate(callback))
                outcome: dict[str, Any] = {"apiKey": api_key}
            except BrowserSignInError as error:
                code = error.code.value if error.code else None
                outcome = {"errorCode": code}
                if code is not None:
                    messages.setdefault(code, []).append(str(error))

        records.append(
            {
                "case": case["case"],
                "script": {
                    "polls": case.get("polls", []),
                    "expiresIn": case.get("expiresIn", 600.0),
                    "createError": case.get("createError"),
                    "exchange": case.get("exchange", "oracle-exchanged-api-key"),
                    "opener": opener_kind,
                },
                "layer": "service",
                "events": events,
                "gatewayCalls": gateway.calls,
                "pollCount": sum(1 for call in gateway.calls if call == "poll"),
                "sleeps": clock.sleeps,
                "browserOpened": opened,
                "challenge": gateway.challenge,
                **outcome,
            }
        )
    return records, messages


# --------------------------------------------------------------------------
# signInProtocol: the HTTP gateway over a recording client
# --------------------------------------------------------------------------


class _StubHTTPClient:
    """Records every request and answers from a script."""

    def __init__(self, responses: list[dict[str, Any]]) -> None:
        self.responses = list(responses)
        self.requests: list[dict[str, Any]] = []

    def _next(self, method: str, url: str, payload: Any) -> Any:
        import httpx

        self.requests.append({"method": method, "url": url, "json": payload})
        if not self.responses:
            raise OracleError("the gateway issued more requests than scripted")
        step = self.responses.pop(0)
        if step.get("transportError"):
            raise httpx.ConnectError("scripted transport failure")
        body = step.get("body")
        if step.get("rawBody") is not None:
            return httpx.Response(
                step.get("status", 200),
                text=step["rawBody"],
                request=httpx.Request(method, url),
            )
        return httpx.Response(
            step.get("status", 200),
            json=body,
            request=httpx.Request(method, url),
        )

    async def post(self, url: str, *, json: Any = None) -> Any:
        return self._next("POST", url, json)

    async def get(self, url: str) -> Any:
        return self._next("GET", url, None)

    async def aclose(self) -> None:
        return None


def capture_gateway() -> tuple[list[dict[str, Any]], dict[str, list[str]]]:
    from vibe.setup.auth.browser_sign_in_gateway import BrowserSignInError
    from vibe.setup.auth.http_browser_sign_in_gateway import HttpBrowserSignInGateway

    records = []
    messages: dict[str, list[str]] = {}
    for case in gateway_cases():
        client = _StubHTTPClient(case["responses"])
        gateway = HttpBrowserSignInGateway(
            case.get("browserBase", BROWSER_BASE),
            case.get("apiBase", API_BASE),
            allow_origin_rewrite=case.get("allowOriginRewrite", False),
            client=client,  # type: ignore[arg-type]
        )

        async def drive() -> dict[str, Any]:
            if case["op"] == "create":
                process = await gateway.create_process("oracle-challenge")
                return {
                    "processId": process.process_id,
                    "signInUrl": process.sign_in_url,
                    "pollUrl": process.poll_url,
                    "expiresAt": process.expires_at.isoformat(),
                }
            if case["op"] == "poll":
                poll_url = case.get("pollUrl", f"{API_BASE}/oracle/poll")
                result = await gateway.poll(poll_url)
                return {
                    "status": result.status,
                    "exchangeToken": result.exchange_token,
                    "message": result.message,
                }
            result = await gateway.exchange(
                "oracle-process", "oracle-token", "oracle-verifier"
            )
            return {"apiKey": result}

        try:
            outcome = asyncio.run(drive())
        except BrowserSignInError as error:
            code = error.code.value if error.code else None
            outcome = {"errorCode": code}
            if code is not None:
                messages.setdefault(code, []).append(str(error))

        records.append(
            {
                "case": case["case"],
                "layer": "gateway",
                "op": case["op"],
                "browserBase": case.get("browserBase", BROWSER_BASE),
                "apiBase": case.get("apiBase", API_BASE),
                "allowOriginRewrite": case.get("allowOriginRewrite", False),
                **({"pollInput": case["pollUrl"]} if "pollUrl" in case else {}),
                "script": case["responses"],
                "requests": client.requests,
                **outcome,
            }
        )
    return records, messages


# --------------------------------------------------------------------------
# urlValidation
# --------------------------------------------------------------------------


def capture_url_validation() -> list[dict[str, Any]]:
    from vibe.setup.auth.browser_sign_in_gateway import (
        BrowserSignInError,
        BrowserSignInErrorCode,
    )
    from vibe.setup.auth.http_browser_sign_in_gateway import (
        _validate_url_against_base_url,
    )

    records = []
    for case in url_validation_cases():
        try:
            returned = _validate_url_against_base_url(
                case["value"],
                base_url=case["base"],
                message="oracle probe",
                code=BrowserSignInErrorCode.POLL_FAILED,
            )
            outcome: dict[str, Any] = {
                "verdict": "accepted",
                "returnedUnchanged": returned == case["value"],
            }
        except BrowserSignInError:
            outcome = {"verdict": "rejected"}
        records.append({**case, **outcome})
    return records


# --------------------------------------------------------------------------
# urlRewrite
# --------------------------------------------------------------------------


def capture_url_rewrite() -> list[dict[str, Any]]:
    """``_validate_url_against_base_url`` with the rewrite on. A value
    ``urlsplit`` refuses raises before the validation's own error, which the
    gateway's start turns into the same refusal, so both read as rejected."""

    from vibe.setup.auth.browser_sign_in_gateway import (
        BrowserSignInError,
        BrowserSignInErrorCode,
    )
    from vibe.setup.auth.http_browser_sign_in_gateway import (
        _validate_url_against_base_url,
    )

    records = []
    for case in url_rewrite_cases():
        try:
            returned = _validate_url_against_base_url(
                case["value"],
                base_url=case["base"],
                message="oracle probe",
                code=BrowserSignInErrorCode.POLL_FAILED,
                allow_origin_rewrite=True,
            )
            outcome: dict[str, Any] = {"verdict": "accepted", "returned": returned}
        except (BrowserSignInError, ValueError):
            outcome = {"verdict": "rejected"}
        records.append({**case, **outcome})
    return records


# --------------------------------------------------------------------------
# tenantDomains
# --------------------------------------------------------------------------


def capture_tenant_domains() -> list[dict[str, Any]]:
    """``_sanitize_tenant_url`` over hostile and benign values, then the
    reference's own ``resolve_tenant_domains`` against a scripted console: only
    the HTTP transport is replaced, so the gateway's status handling, the
    strict answer model and the adoption are all the reference's."""

    import httpx

    import vibe.setup.auth.whoami as whoami_module
    from vibe.core.config import DEFAULT_PROVIDERS

    records: list[dict[str, Any]] = []
    for candidate in tenant_sanitize_cases():
        records.append(
            {
                "case": f"sanitize:{candidate}" if candidate else "sanitize:<empty>",
                "candidate": candidate,
                "sanitized": whoami_module._sanitize_tenant_url(candidate, field="api"),
            }
        )

    mistral = next(p for p in DEFAULT_PROVIDERS if p.name == "mistral")
    original_client = whoami_module.VibeAsyncHTTPClient
    for answer in TENANT_ANSWERS:
        for console in ("https://console.internal.example", "https://connector.internal.example:8443/"):
            requests: list[dict[str, Any]] = []

            def handler(request: httpx.Request, answer: dict[str, Any] = answer) -> httpx.Response:
                requests.append(
                    {
                        "method": request.method,
                        "url": str(request.url),
                        "authorization": request.headers.get("authorization"),
                    }
                )
                if "rawBody" in answer:
                    return httpx.Response(answer["status"], text=answer["rawBody"])
                return httpx.Response(answer["status"], json=answer["body"])

            def client(**keywords: Any) -> Any:
                return original_client(transport=httpx.MockTransport(handler), **keywords)

            with patched(whoami_module, "VibeAsyncHTTPClient", client):
                provider, vibe_base_url = asyncio.run(
                    whoami_module.resolve_tenant_domains(
                        mistral, console, "oracle-key", "https://chat.mistral.ai"
                    )
                )
            records.append(
                {
                    "case": f"resolve:{answer['case']}@{console}",
                    "console": console,
                    "answer": {key: value for key, value in answer.items() if key != "case"},
                    "requests": requests,
                    "apiBase": provider.api_base,
                    "vibeBaseUrl": vibe_base_url,
                }
            )
    return records


# --------------------------------------------------------------------------
# providerCredentials
# --------------------------------------------------------------------------


def capture_provider_credentials() -> list[dict[str, Any]]:
    """The reference's own ``persist_provider_credentials`` against a scratch
    vibe home and an empty working directory, recording the result per field
    and every file the batch leaves under the home."""

    from vibe.core.config import DEFAULT_PROVIDERS
    from vibe.core.config.harness_files import init_harness_files_manager
    from vibe.setup.auth.api_key_persistence import (
        ProviderCredentialsPersistRequest,
        persist_provider_credentials,
    )

    # The sources the terminal and the editor initialize before onboarding.
    init_harness_files_manager("user", "project")
    mistral = next(p for p in DEFAULT_PROVIDERS if p.name == "mistral")
    records = []
    for case in provider_credentials_cases():
        saved = dict(os.environ)
        previous = os.getcwd()
        with tempfile.TemporaryDirectory(prefix="setup-auth-credentials-") as scratch:
            home = Path(scratch) / "vibe-home"
            home.mkdir()
            cwd = Path(scratch) / "work"
            cwd.mkdir()
            if case["user"] is not None:
                (home / "config.toml").write_text(case["user"], encoding="utf-8", newline="")
            try:
                for key in [key for key in os.environ if key.startswith("VIBE_")]:
                    os.environ.pop(key)
                os.environ["VIBE_HOME"] = str(home)
                os.chdir(cwd)
                request = ProviderCredentialsPersistRequest(
                    provider=mistral.model_copy(update=case["provider"]),
                    console_base_url=case.get("consoleBaseUrl"),
                    vibe_base_url=case.get("vibeBaseUrl"),
                )
                result = asyncio.run(persist_provider_credentials(request))
            finally:
                os.chdir(previous)
                os.environ.clear()
                os.environ.update(saved)
            files = {
                str(path.relative_to(home)): path.read_text(encoding="utf-8")
                for path in sorted(home.rglob("*"))
                if path.is_file()
            }
        records.append(
            {
                **case,
                "result": {
                    "provider": result.provider,
                    "consoleBaseUrl": result.console_base_url,
                    "vibeBaseUrl": result.vibe_base_url,
                },
                "firstFailure": result.first_failure(),
                "files": files,
            }
        )
    return records


# --------------------------------------------------------------------------
# tenantReconcile
# --------------------------------------------------------------------------


def capture_tenant_reconcile() -> list[dict[str, Any]]:
    """The reference's own ``reconcile_tenant_domains`` over an orchestrator
    built by ``build_default_orchestrator`` in a scratch home, recording the
    files it leaves under the home and the reason of every change event."""

    from vibe.app_server._account import reconcile_tenant_domains
    from vibe.core.config.default_orchestrator import build_default_orchestrator
    from vibe.core.config.harness_files import HarnessFilesManager
    from vibe.core.trusted_folders import TrustedFoldersManager
    from vibe.setup.auth.whoami import WhoAmIResult

    records = []
    for case in tenant_reconcile_cases():
        saved = dict(os.environ)
        previous = os.getcwd()
        reasons: list[str] = []
        with tempfile.TemporaryDirectory(prefix="setup-auth-reconcile-") as scratch:
            home = Path(scratch) / "vibe-home"
            home.mkdir()
            cwd = Path(scratch) / "work"
            cwd.mkdir()
            if case["user"] is not None:
                (home / "config.toml").write_text(case["user"], encoding="utf-8", newline="")
            try:
                for key in [key for key in os.environ if key.startswith("VIBE_")]:
                    os.environ.pop(key)
                os.environ["VIBE_HOME"] = str(home)
                os.chdir(cwd)

                async def run() -> None:
                    manager = HarnessFilesManager(
                        sources=("user", "project"),
                        cwd=cwd,
                        trust_store=TrustedFoldersManager(),
                    )
                    orchestrator = await build_default_orchestrator(harness_files=manager)
                    orchestrator.subscribe(lambda event: reasons.append(event.reason))
                    await reconcile_tenant_domains(
                        orchestrator,
                        WhoAmIResult.model_validate(
                            {"plan_type": "api", "plan_name": "oracle", **case["whoami"]}
                        ),
                        provider_name=case["providerName"],
                    )

                asyncio.run(run())
            finally:
                os.chdir(previous)
                os.environ.clear()
                os.environ.update(saved)
            files = {
                str(path.relative_to(home)): path.read_text(encoding="utf-8")
                for path in sorted(home.rglob("config.toml"))
            }
        records.append({**case, "files": files, "reasons": reasons})
    return records


# --------------------------------------------------------------------------
# acpSignIn
# --------------------------------------------------------------------------


def _acp_provider_fields(provider: Any) -> dict[str, Any]:
    return {
        "name": provider.name,
        "apiBase": provider.api_base,
        "browserAuthBaseUrl": provider.browser_auth_base_url,
        "browserAuthApiBaseUrl": provider.browser_auth_api_base_url,
        "allowOriginRewrite": provider.browser_auth_allow_origin_rewrite,
    }


def capture_acp_sign_in() -> list[dict[str, Any]]:
    """The reference's ``AcpAuthController`` over its own constructor ports:
    a scripted context, a stub sign-in service answering one key, recording
    key and credentials persisters, and the reference's own
    ``resolve_tenant_domains`` around a scripted ``fetch_whoami``."""

    # This directory holds an ``acp.py`` capture script that would shadow the
    # protocol package the reference controller imports.
    here = Path(__file__).resolve().parent
    sys.path[:] = [entry for entry in sys.path if Path(entry or ".").resolve() != here]
    import vibe.setup.auth.whoami as whoami_module
    from vibe.acp.auth import AcpAuthController
    from vibe.acp.exceptions import InternalError, InvalidRequestError
    from vibe.core.config import DEFAULT_PROVIDERS, ProviderConfig
    from vibe.setup.auth.api_key_persistence import ProviderCredentialsPersistResult
    from vibe.setup.auth.whoami import WhoAmIResult
    from vibe.setup.onboarding.context import OnboardingContext

    mistral = next(p for p in DEFAULT_PROVIDERS if p.name == "mistral")
    records = []
    for spec in acp_sign_in_cases():
        if spec["provider"] == "generic":
            provider = ProviderConfig(
                name="generic",
                api_base="https://llm.example/v1",
                api_key_env_var="GENERIC_API_KEY",
            )
        else:
            provider = mistral.model_copy(update=spec["provider"])
        context = OnboardingContext(
            provider=provider,
            vibe_base_url=spec["vibeBaseUrl"],
            console_base_url=spec["consoleBaseUrl"],
        )
        observed: dict[str, list[Any]] = {
            "serviceProviders": [],
            "apiKeyPersists": [],
            "tenantLookups": [],
            "persistRequests": [],
        }

        class _Service:
            async def authenticate(self) -> str:
                return "oracle-key"

            async def aclose(self) -> None:
                return None

        def service_factory(resolved: Any) -> Any:
            observed["serviceProviders"].append(_acp_provider_fields(resolved))
            return _Service()

        def api_key_persister(
            resolved: Any, api_key: str, *, custom_domain: bool = False
        ) -> str:
            observed["apiKeyPersists"].append(
                {
                    "envKey": resolved.api_key_env_var,
                    "backend": str(resolved.backend),
                    "apiKey": api_key,
                    "customDomain": custom_domain,
                }
            )
            return spec["apiKeyResult"]

        async def fetch_whoami(
            base_url: str, api_key: str, *, timeout: float | None = None
        ) -> Any:
            if spec["whoami"] is None:
                return None
            return WhoAmIResult.model_validate(
                {"plan_type": "api", "plan_name": "oracle", **spec["whoami"]}
            )

        async def tenant_resolver(
            resolved: Any, console: str, api_key: str, vibe: str
        ) -> Any:
            observed["tenantLookups"].append(
                {"consoleBaseUrl": console, "apiKey": api_key, "vibeBaseUrl": vibe}
            )
            with patched(whoami_module, "fetch_whoami", fetch_whoami):
                return await whoami_module.resolve_tenant_domains(
                    resolved, console, api_key, vibe
                )

        async def credentials_persister(request: Any) -> Any:
            observed["persistRequests"].append(
                {
                    "provider": _acp_provider_fields(request.provider),
                    "consoleBaseUrl": request.console_base_url,
                    "vibeBaseUrl": request.vibe_base_url,
                }
            )
            return ProviderCredentialsPersistResult(
                provider=spec["providerOk"],
                console_base_url=(
                    None if request.console_base_url is None else spec["consoleOk"]
                ),
                vibe_base_url=None if request.vibe_base_url is None else spec["vibeOk"],
            )

        controller = AcpAuthController(
            context_loader=lambda: context,
            service_factory=service_factory,
            api_key_persister=api_key_persister,
            credentials_persister=credentials_persister,
            tenant_domain_resolver=tenant_resolver,
            environ_before_dotenv_load={},
        )
        try:
            response = asyncio.run(
                controller.authenticate("browser-auth", spec["arguments"])
            )
            outcome: dict[str, Any] = {
                "meta": response.field_meta["browser-auth"],
            }
        except (InvalidRequestError, InternalError) as error:
            outcome = {"errorCode": error.code}
        records.append({**spec, "outcome": outcome, **observed})
    return records


# --------------------------------------------------------------------------
# errorTaxonomy and constants
# --------------------------------------------------------------------------


def capture_error_taxonomy(
    harvested: dict[str, list[str]],
) -> list[dict[str, Any]]:
    from vibe.setup.auth.browser_sign_in_gateway import BrowserSignInErrorCode

    scripted_markers = ("scripted", "oracle probe")
    records = []
    for code in BrowserSignInErrorCode:
        sentences = sorted(
            {
                sentence
                for sentence in harvested.get(code.value, [])
                if not any(marker in sentence for marker in scripted_markers)
            }
        )
        records.append(
            {
                "name": code.name,
                "value": code.value,
                "messages": [digest(sentence) for sentence in sentences],
            }
        )
    return records


def _module_source(module: str) -> str:
    """The pinned tree's source for `module`, located through the import path."""
    specification = importlib.util.find_spec(module)
    if specification is None or specification.origin is None:
        raise OracleError(f"the pinned tree does not publish {module}")
    return Path(specification.origin).read_text(encoding="utf-8")


def _keyword_string(call: ast.Call, name: str) -> str | None:
    for keyword in call.keywords:
        if (
            keyword.arg == name
            and isinstance(keyword.value, ast.Constant)
            and isinstance(keyword.value.value, str)
        ):
            return keyword.value.value
    return None


def _calls_named(source: str, name: str) -> list[ast.Call]:
    return [
        node
        for node in ast.walk(ast.parse(source))
        if isinstance(node, ast.Call)
        and isinstance(node.func, ast.Name)
        and node.func.id == name
    ]


def capture_acp_auth_prose() -> list[dict[str, Any]]:
    """The labels the reference's editor-protocol auth methods carry.

    The browser method is built in ``vibe/acp/auth.py`` and the terminal one in
    ``vibe/acp/agent.py``; both name and describe themselves in authored prose
    `NOTICE` forbids shipping, so only each run's length and SHA-256 is
    recorded and the ACP replay holds this port's own labels unequal to them.
    """
    browser = _calls_named(_module_source("vibe.acp.auth"), "AuthMethodAgent")
    terminal = _calls_named(_module_source("vibe.acp.agent"), "TerminalAuthMethod")
    if not browser or not terminal:
        raise OracleError("the pinned reference declares no ACP authentication method")
    runs: dict[str, str | None] = {
        "browserMethod/name": _keyword_string(browser[0], "name"),
        "browserMethod/description": _keyword_string(browser[0], "description"),
        "terminalMethod/name": _keyword_string(terminal[0], "name"),
        "terminalMethod/description": _keyword_string(terminal[0], "description"),
        "terminalMethod/label": next(
            (
                value.value
                for node in ast.walk(terminal[0])
                if isinstance(node, ast.Dict)
                for key, value in zip(node.keys, node.values, strict=True)
                if isinstance(key, ast.Constant)
                and key.value == "label"
                and isinstance(value, ast.Constant)
                and isinstance(value.value, str)
            ),
            None,
        ),
    }
    missing = sorted(surface for surface, run in runs.items() if run is None)
    if missing:
        raise OracleError(f"no reference label found for {missing}")
    return [
        {"surface": surface, "run": digest(run)}
        for surface, run in sorted(runs.items())
        if run is not None
    ]


def capture_constants() -> dict[str, Any]:
    from vibe.core.config._defaults import (
        DEFAULT_MISTRAL_API_ENV_KEY,
        DEFAULT_MISTRAL_BROWSER_AUTH_API_BASE_URL,
        DEFAULT_MISTRAL_BROWSER_AUTH_BASE_URL,
    )
    from vibe.setup.auth.browser_sign_in import (
        BrowserSignInService,
        BrowserSignInStatus,
        _generate_code_challenge,
        _generate_code_verifier,
    )
    from vibe.setup.auth.browser_sign_in_gateway import _DEFAULT_PORTS
    from vibe.setup.auth.http_browser_sign_in_gateway import HTTP_GONE
    from vibe.utils.keyring import _KEYRING_SERVICE, _LEGACY_KEYRING_SERVICES

    generated = _generate_code_verifier()
    unreserved = set(
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~"
    )
    return {
        "keyringService": _KEYRING_SERVICE,
        "legacyKeyringServices": list(_LEGACY_KEYRING_SERVICES),
        "defaultEnvKey": DEFAULT_MISTRAL_API_ENV_KEY,
        "browserAuthBaseUrl": DEFAULT_MISTRAL_BROWSER_AUTH_BASE_URL,
        "browserAuthApiBaseUrl": DEFAULT_MISTRAL_BROWSER_AUTH_API_BASE_URL,
        "pollIntervalSeconds": BrowserSignInService(
            gateway=None  # type: ignore[arg-type]
        )._poll_interval,
        "maxConsecutivePollFailures": BrowserSignInService._max_consecutive_poll_failures,
        "statuses": [status.value for status in BrowserSignInStatus],
        "httpGoneStatus": HTTP_GONE,
        "defaultPorts": dict(_DEFAULT_PORTS),
        "codeChallengeMethod": "S256",
        "signInPath": "/vibe/sign-in",
        "exchangePathTemplate": "/vibe/sign-in/{process_id}/exchange",
        "pkce": {
            "scriptedVerifier": SCRIPTED_VERIFIER,
            "scriptedChallenge": _generate_code_challenge(SCRIPTED_VERIFIER),
            "generatedLength": len(generated),
            "generatedCharsetIsUnreserved": set(generated) <= unreserved,
        },
    }


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


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
            "also write the committed corpus, which the Rust replay reads unconditionally "
            f"(default {DEFAULT_CORPUS})"
        ),
    )
    parser.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def build_corpus(reference: dict[str, str], families: dict[str, Any]) -> dict[str, Any]:
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": {"commit": reference["commit"]},
        "note": (
            "Setup-auth corpus: what the pinned reference's auth-state assessment, credential "
            "persistence, keyring migration, sign-in service and HTTP gateway answer for each "
            "scripted input. Values, vocabulary, call orders and verdicts are committed as they "
            "stand because the scenarios supplied them; every reference-authored error sentence "
            "and ACP method label is committed as a SHA-256 digest and a length. Regenerate with "
            "scripts/parity/setup_auth.py --corpus when the pinned reference moves."
        ),
        **families,
    }


def main() -> int:
    arguments = parse_arguments()
    try:
        commit, _ = reexecute_pinned(
            arguments.reference,
            arguments.expected_commit,
            cache=arguments.cache,
            override=arguments.python,
        )
        reference = {"commit": commit, "path": str(arguments.reference)}
        GUARD.install()
        with scratch_vibe_home():
            auth_state = capture_auth_state()
            persistence = capture_persistence()
            service_records, service_messages = capture_service()
            gateway_records, gateway_messages = capture_gateway()
            url_validation = capture_url_validation()
            url_rewrite = capture_url_rewrite()
            tenant_domains = capture_tenant_domains()
        provider_credentials = capture_provider_credentials()
        acp_sign_in = capture_acp_sign_in()
        tenant_reconcile = capture_tenant_reconcile()
        harvested: dict[str, list[str]] = {}
        for source in (service_messages, gateway_messages):
            for code, sentences in source.items():
                harvested.setdefault(code, []).extend(sentences)
        taxonomy = capture_error_taxonomy(harvested)
        acp_auth_prose = capture_acp_auth_prose()
        constants = capture_constants()
    except OracleError as error:
        print(f"setup-auth capture failed: {error}", file=sys.stderr)
        return 1

    if GUARD.attempts:
        print(
            f"setup-auth capture failed: network attempts {GUARD.attempts}",
            file=sys.stderr,
        )
        return 1

    missing = [entry["value"] for entry in taxonomy if not entry["messages"]]
    if missing:
        print(
            f"setup-auth capture failed: no reference sentence harvested for {missing}",
            file=sys.stderr,
        )
        return 1

    families = {
        "constants": constants,
        "authState": auth_state,
        "persistence": persistence,
        "signInProtocol": service_records + gateway_records,
        "urlValidation": url_validation,
        "urlRewrite": url_rewrite,
        "tenantDomains": tenant_domains,
        "providerCredentials": provider_credentials,
        "acpSignIn": acp_sign_in,
        "tenantReconcile": tenant_reconcile,
        "errorTaxonomy": taxonomy,
        "acpAuthProse": acp_auth_prose,
    }
    corpus = build_corpus(reference, families)

    full = {
        **corpus,
        "reference": reference,
        "platform": platform.system().lower(),
        "python": platform.python_version(),
    }
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    staged = arguments.output.with_name(f"{arguments.output.name}.{os.getpid()}.tmp")
    staged.write_text(
        json.dumps(full, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
        encoding="utf-8",
        newline="",
    )
    os.replace(staged, arguments.output)

    if arguments.corpus is not None:
        arguments.corpus.parent.mkdir(parents=True, exist_ok=True)
        staged = arguments.corpus.with_name(
            f"{arguments.corpus.name}.{os.getpid()}.tmp"
        )
        staged.write_text(
            json.dumps(corpus, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
            encoding="utf-8",
            newline="",
        )
        os.replace(staged, arguments.corpus)

    counted = {
        "authState": len(auth_state),
        "persistence": len(persistence),
        "signInProtocol": len(service_records) + len(gateway_records),
        "urlValidation": len(url_validation),
        "urlRewrite": len(url_rewrite),
        "tenantDomains": len(tenant_domains),
        "providerCredentials": len(provider_credentials),
        "acpSignIn": len(acp_sign_in),
        "tenantReconcile": len(tenant_reconcile),
        "errorTaxonomy": len(taxonomy),
        "acpAuthProse": len(acp_auth_prose),
    }
    total = sum(counted.values())
    print(
        f"captured {total} scenarios across {len(counted)} families "
        f"from {reference['commit'][:12]} into {arguments.output}"
    )
    for family, count in counted.items():
        print(f"  {family}: {count}")
    if arguments.corpus is not None:
        print(f"wrote the committed corpus to {arguments.corpus}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
