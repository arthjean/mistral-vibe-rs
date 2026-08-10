#!/usr/bin/env python3
"""Capture the pinned reference's onboarding screen graph as observations.

Textual and ratatui cannot render the same cells, so this capture never
records output: it drives the real ``OnboardingApp`` through Textual's
headless pilot and records what `tasks/prd-chat-input-observable-parity.md`
calls observations: the installed screen set, the ordered transitions taken,
the focus target per screen, the validation class per input state, the
effects persisted and the terminating value. The sign-in service, the
credential persistence and the config orchestrator are injected or patched to
recorders, so a run touches no network, no keyring and no real ``VIBE_HOME``.

Four families come out, and are what the Rust replay compares:

``screenGraph``       pilot-driven scenarios over the installed screens
``terminating``       the five terminating values and the exit path each takes
``domainValidation``  the custom-domain validator, heuristic and derivations
``screenProse``       every authored run the screens carry, as length and digest

Two artifacts come out of a run::

    .parity/onboarding-corpus.json               the full capture, gitignored
    crates/vibe-cli/tests/onboarding/corpus.json   the committed corpus

The committed corpus carries screen names, transition edges, focus targets,
class names, effect records and terminating values. Anything
reference-authored, which is the exit-path messages, the armed-overwrite
warning and every sentence the screens draw, is committed as a length plus a
SHA-256 and never as text.

Usage::

    scripts/parity/onboarding.py --reference /path/to/reference --corpus

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
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import socket
import subprocess
import sys
import tarfile
import tempfile
from datetime import UTC, datetime
from types import SimpleNamespace
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT

SCHEMA_VERSION = 2
DEFAULT_OUTPUT = Path(".parity/onboarding-corpus.json")
DEFAULT_CORPUS = Path("crates/vibe-cli/tests/onboarding/corpus.json")
DEFAULT_CACHE = Path(".parity")

#: Set on the re-executed process so it does not extract and re-exec forever.
_REEXEC_MARKER = "VIBE_PARITY_PINNED_TREE"

#: A fixed, never-created ``VIBE_HOME`` so every path the reference formats
#: into a message is identical across machines and runs.
ORACLE_VIBE_HOME = "/oracle-vibe-home"

#: The fixed expiry stamped on every stub sign-in attempt.
ATTEMPT_EXPIRES_AT = datetime(2026, 1, 1, 0, 10, 0, tzinfo=UTC)
STUB_SIGN_IN_URL = "https://console.mistral.ai/oracle/sign-in"

VIEWPORT = (100, 40)
SETTLE_ROUNDS = 8
#: How long the harness is willing to wait for a timer-driven condition.
WAIT_TIMEOUT = 20.0


class OracleError(RuntimeError):
    """Raised when the corpus cannot be produced from an authoritative state."""


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
    """The pinned commit, read out of the checkout without depending on its HEAD."""

    if not reference.is_dir():
        raise OracleError(f"no reference checkout at {reference}")
    try:
        _git(reference, "cat-file", "-e", f"{expected}^{{commit}}")
    except OracleError as error:
        raise OracleError(
            f"{reference} does not contain the pinned commit {expected}: {error}"
        ) from error
    return {"commit": expected, "path": str(reference)}


def extract_pinned_tree(reference: Path, commit: str, cache: Path) -> Path:
    """The pinned source tree, materialized out of tree and reused across runs."""

    tree = (cache / f"reference-{commit[:12]}").resolve()
    marker = tree / "vibe" / "__init__.py"
    if marker.is_file():
        return tree
    tree.mkdir(parents=True, exist_ok=True)
    archive = tree.with_suffix(".tar")
    _git(reference, "archive", "--format=tar", "-o", str(archive), commit)
    with tarfile.open(archive) as bundle:
        bundle.extractall(tree, filter="data")
    archive.unlink(missing_ok=True)
    if not marker.is_file():
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
    """Fails the capture the moment anything tries to reach a network."""

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


@contextlib.contextmanager
def patched_environ(name: str, value: str | None):
    original = os.environ.get(name)
    if value is None:
        os.environ.pop(name, None)
    else:
        os.environ[name] = value
    try:
        yield
    finally:
        if original is None:
            os.environ.pop(name, None)
        else:
            os.environ[name] = original


@contextlib.contextmanager
def patched(target: Any, name: str, value: Any):
    original = getattr(target, name)
    setattr(target, name, value)
    try:
        yield
    finally:
        setattr(target, name, original)


def digest(value: str) -> dict[str, Any]:
    """A string recorded by its length and its SHA-256, never by its content."""

    return {
        "length": len(value),
        "digest": hashlib.sha256(value.encode("utf-8")).hexdigest(),
    }


# --------------------------------------------------------------------------
# Stub collaborators
# --------------------------------------------------------------------------


class StubSignInService:
    """Scripted stand-in for ``BrowserSignInService``.

    Each attempt is one script entry: the statuses to emit, then a returned
    key, a raised code, or hanging until cancelled.
    """

    def __init__(self, step: dict[str, Any], log: dict[str, Any]) -> None:
        self.step = step
        self.log = log

    async def authenticate(self, event_callback: Any) -> str:
        from vibe.setup.auth.browser_sign_in import (
            BrowserSignInAttemptStarted,
            BrowserSignInStatus,
            BrowserSignInStatusChanged,
        )
        from vibe.setup.auth.browser_sign_in_gateway import (
            BrowserSignInError,
            BrowserSignInErrorCode,
        )

        event_callback(
            BrowserSignInAttemptStarted(
                sign_in_url=STUB_SIGN_IN_URL, expires_at=ATTEMPT_EXPIRES_AT
            )
        )
        for status in self.step.get(
            "statuses",
            ["opening_browser", "waiting_for_browser_sign_in", "exchanging", "completed"],
        ):
            event_callback(
                BrowserSignInStatusChanged(status=BrowserSignInStatus(status))
            )
            await asyncio.sleep(0)
        if self.step.get("hang"):
            await asyncio.Event().wait()
        if self.step.get("raise"):
            raise BrowserSignInError(
                "scripted sign-in failure",
                code=BrowserSignInErrorCode(self.step["raise"]),
            )
        return self.step.get("apiKey", "oracle-signed-in-key")

    async def aclose(self) -> None:
        self.log["closed"] = self.log.get("closed", 0) + 1


def make_factory(attempts: list[dict[str, Any]], effects: dict[str, Any]):
    """A service factory whose successive calls consume ``attempts``."""

    effects.setdefault("factoryCalls", 0)

    def factory() -> StubSignInService:
        index = effects["factoryCalls"]
        effects["factoryCalls"] += 1
        if index >= len(attempts):
            raise OracleError("the screen started more attempts than scripted")
        log: dict[str, Any] = {}
        effects.setdefault("serviceLogs", []).append(log)
        return StubSignInService(attempts[index], log)

    return factory


class PersistRecorder:
    """Scripted stand-in for ``persist_api_key`` and the provider upsert."""

    def __init__(
        self, *, outcome: str = "completed", provider_write_ok: bool = True
    ) -> None:
        self.outcome = outcome
        self.provider_write_ok = provider_write_ok
        self.persist_calls: list[dict[str, Any]] = []
        self.provider_writes: list[dict[str, Any]] = []

    def persist_api_key(
        self,
        provider: Any,
        api_key: str,
        *,
        launch_context: Any = None,
        custom_domain: bool = False,
    ) -> str:
        self.persist_calls.append(
            {
                "provider": provider.name,
                "envKey": provider.api_key_env_var,
                "apiKey": api_key,
                "customDomain": custom_domain,
            }
        )
        return self.outcome

    def persist_provider_to_config(self, provider: Any) -> bool:
        self.provider_writes.append(
            {
                "provider": provider.name,
                "browserAuthBaseUrl": provider.browser_auth_base_url,
                "browserAuthApiBaseUrl": provider.browser_auth_api_base_url,
            }
        )
        return self.provider_write_ok


# --------------------------------------------------------------------------
# The pilot harness
# --------------------------------------------------------------------------


class GraphObserver:
    """Watches one app run and records graph-level observations."""

    def __init__(self, app: Any) -> None:
        self.app = app
        self.transitions: list[dict[str, Any]] = []
        self.focus: dict[str, Any] = {}

    def screen_name(self) -> str:
        for name, screen in self.app._installed_screens.items():
            resolved = self.app.get_screen(name)
            if resolved is self.app.screen:
                return name
        return type(self.app.screen).__name__

    def describe_focus(self) -> dict[str, Any] | None:
        focused = self.app.focused
        if focused is None:
            return None
        return {"widget": type(focused).__name__, "id": focused.id}

    async def settle(self, pilot: Any) -> None:
        for _ in range(SETTLE_ROUNDS):
            await pilot.pause()
            await asyncio.sleep(0)

    def record_focus(self) -> None:
        if self.app._exit or not self.app.screen_stack:
            return
        name = self.screen_name()
        self.focus.setdefault(name, self.describe_focus())

    async def event(self, pilot: Any, label: str, action: Any) -> None:
        before = self.screen_name()
        await action()
        await self.settle(pilot)
        if self.app._exit:
            self.transitions.append(
                {"event": label, "from": before, "to": "<exit>"}
            )
            return
        after = self.screen_name()
        if after != before:
            self.transitions.append({"event": label, "from": before, "to": after})
        self.record_focus()

    async def press(self, pilot: Any, *keys: str) -> None:
        label = "+".join(keys) if len(keys) < 8 else f"type:{len(keys)} keys"
        await self.event(pilot, f"press:{label}", lambda: pilot.press(*keys))

    async def wait_for(self, pilot: Any, condition: Any, label: str) -> None:
        async def waiter() -> None:
            elapsed = 0.0
            while not condition():
                if elapsed > WAIT_TIMEOUT:
                    raise OracleError(f"timed out waiting for {label}")
                await pilot.pause(0.1)
                elapsed += 0.1

        await self.event(pilot, f"wait:{label}", waiter)


def build_app(
    *,
    browser: bool,
    factory: Any = None,
    configured_domain: str | None = None,
) -> Any:
    from vibe.core.config import DEFAULT_PROVIDERS
    from vibe.setup.onboarding import OnboardingApp
    from vibe.setup.onboarding.context import OnboardingContext

    mistral = next(p for p in DEFAULT_PROVIDERS if p.name == "mistral")
    if browser:
        provider = mistral
        if configured_domain is not None:
            provider = mistral.model_copy(
                update={
                    "browser_auth_base_url": configured_domain,
                    "browser_auth_api_base_url": f"{configured_domain}/api",
                }
            )
    else:
        provider = mistral.model_copy(update={"browser_auth_base_url": None})

    context = OnboardingContext(provider=provider)
    return OnboardingApp(
        config=context,
        browser_sign_in_service_factory=factory,
        browser_sign_in_success_delay=0.0,
        browser_sign_in_url_help_delay=9999.0,
        copy_sign_in_url=lambda text: None,
    )


async def run_scenario(scenario: dict[str, Any]) -> dict[str, Any]:
    import vibe.setup.onboarding as onboarding_module

    effects: dict[str, Any] = {}
    attempts = scenario.get("attempts", [])
    factory = make_factory(attempts, effects) if scenario.get("browser") else None
    recorder = PersistRecorder(
        outcome=scenario.get("persistOutcome", "completed"),
        provider_write_ok=scenario.get("providerWriteOk", True),
    )
    app = build_app(
        browser=scenario.get("browser", False),
        factory=factory,
        configured_domain=scenario.get("configuredDomain"),
    )
    observer = GraphObserver(app)

    with (
        patched(onboarding_module, "persist_api_key", recorder.persist_api_key),
        patched(
            onboarding_module,
            "persist_provider_to_config",
            recorder.persist_provider_to_config,
        ),
    ):
        async with app.run_test(size=VIEWPORT) as pilot:
            await observer.settle(pilot)
            entry = observer.screen_name()
            observer.record_focus()
            installed = sorted(app._installed_screens)
            await scenario["drive"](app, pilot, observer)
            await observer.settle(pilot)

    result = app.return_value
    effects["serviceCloses"] = sum(
        log.get("closed", 0) for log in effects.get("serviceLogs", [])
    )
    effects.pop("serviceLogs", None)
    record: dict[str, Any] = {
        "case": scenario["case"],
        "supportsBrowserSignIn": app.supports_browser_sign_in,
        "installedScreens": installed,
        "entryScreen": entry,
        "transitions": observer.transitions,
        "focus": observer.focus,
        "effects": {
            **effects,
            "persistCalls": recorder.persist_calls,
            "providerWrites": recorder.provider_writes,
        },
        "result": result,
        "selectedTheme": app.selected_theme,
        "providerBrowserAuth": {
            "baseUrl": app._provider.browser_auth_base_url,
            "apiBaseUrl": app._provider.browser_auth_api_base_url,
        },
    }
    extra = scenario.get("extra")
    if extra:
        record.update(extra(app))
    return record


# --------------------------------------------------------------------------
# screenGraph scenarios
# --------------------------------------------------------------------------


def _typing_done(app: Any) -> bool:
    screen = app.get_screen("welcome")
    return screen._typing_done


async def _skip_welcome(app: Any, pilot: Any, observer: GraphObserver) -> None:
    """Fast-forwards the welcome typing animation, then advances."""

    welcome = app.get_screen("welcome")
    welcome._typing_done = True
    await observer.press(pilot, "enter")


def graph_scenarios() -> list[dict[str, Any]]:
    async def full_install(app: Any, pilot: Any, observer: GraphObserver) -> None:
        await observer.press(pilot, "escape")

    async def welcome_gates_on_typing(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await observer.press(pilot, "enter")
        observer.focus["welcome:screenAfterEarlyEnter"] = observer.screen_name()
        await observer.wait_for(
            pilot, lambda: _typing_done(app), "welcome typing to finish"
        )
        await observer.press(pilot, "enter")
        observer.focus["welcome:screenAfterLateEnter"] = observer.screen_name()
        await observer.press(pilot, "escape")

    async def theme_navigation(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "down")
        await observer.press(pilot, "enter")
        await observer.press(pilot, "escape")

    async def theme_to_api_key_without_browser(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        await observer.press(pilot, "escape")

    async def auth_method_browser(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        await observer.press(pilot, "escape")

    async def auth_method_manual(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        await observer.press(pilot, "down")
        await observer.press(pilot, "enter")
        await observer.press(pilot, "escape")

    async def auth_method_selection_wraps(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        screen = app.get_screen("auth_method")
        indexes = [screen._selected_index]
        for key in ("down", "down", "up"):
            await observer.press(pilot, key)
            indexes.append(screen._selected_index)
        observer.focus["auth_method:selectionWalk"] = indexes
        await observer.press(pilot, "escape")

    async def target_default_applies_mistral(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        await observer.press(pilot, "escape")

    async def target_other_leads_to_custom_domain(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        await observer.press(pilot, "down")
        await observer.press(pilot, "enter")
        await observer.press(pilot, "escape")

    async def target_back_returns_to_method(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        await observer.press(pilot, "escape")
        await observer.press(pilot, "escape")

    async def target_overwrite_arms_then_proceeds(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        screen = app.get_screen("sign_in_target")
        await observer.press(pilot, "enter")
        observer.focus["sign_in_target:armedAfterFirstEnter"] = (
            screen._override_confirm_armed
        )
        await observer.press(pilot, "enter")
        await observer.press(pilot, "escape")

    async def target_overwrite_disarms_on_move(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        screen = app.get_screen("sign_in_target")
        await observer.press(pilot, "enter")
        armed = screen._override_confirm_armed
        await observer.press(pilot, "down")
        observer.focus["sign_in_target:armedThenMoved"] = [
            armed,
            screen._override_confirm_armed,
        ]
        await observer.press(pilot, "escape")
        await observer.press(pilot, "escape")

    async def domain_validation_classes(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        await observer.press(pilot, "down")
        await observer.press(pilot, "enter")
        screen = app.screen
        states = []
        for label, value in (
            ("valid", "console.internal.example"),
            ("warning", "console.oracle.mistral.ai"),
            ("invalid", "http:/broken"),
        ):
            screen.domain_input.value = value
            await observer.settle(pilot)
            feedback = screen.query_one("#feedback")
            box = screen.query_one("#domain-box")
            states.append(
                {
                    "input": value,
                    "expected": label,
                    "feedbackClasses": sorted(
                        c
                        for c in feedback.classes
                        if c in ("error", "success", "warning")
                    ),
                    "boxClasses": sorted(
                        c
                        for c in box.classes
                        if c in ("valid", "invalid", "warning")
                    ),
                }
            )
        observer.focus["custom_domain:validationStates"] = states
        await observer.press(pilot, "escape")
        await observer.press(pilot, "escape")
        await observer.press(pilot, "escape")

    async def _submit_custom_domain(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        """Walks to the domain screen and submits a valid custom domain."""

        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        await observer.press(pilot, "down")
        await observer.press(pilot, "enter")
        screen = app.screen
        screen.domain_input.value = "console.internal.example"
        await observer.settle(pilot)
        await observer.press(pilot, "enter")

    async def domain_submit_valid(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _submit_custom_domain(app, pilot, observer)
        await observer.press(pilot, "escape")

    async def domain_submit_valid_then_signs_in(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        """The one path that reaches the provider write.

        ``persist_credentials`` upserts the provider only when the credential
        saved *and* the flow changed it, so a corpus whose scenarios never do
        both records an empty ``providerWrites`` everywhere, which reads as
        "the reference never writes" rather than "no scenario asked it to".
        """

        await _submit_custom_domain(app, pilot, observer)
        await observer.wait_for(
            pilot, lambda: app._exit, "the sign-in worker to exit the app"
        )

    async def domain_submit_invalid_stays(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        await observer.press(pilot, "down")
        await observer.press(pilot, "enter")
        screen = app.screen
        screen.domain_input.value = "http:/broken"
        await observer.settle(pilot)
        await observer.press(pilot, "enter")
        observer.focus["custom_domain:stillOnScreen"] = observer.screen_name()
        await observer.press(pilot, "escape")
        await observer.press(pilot, "escape")
        await observer.press(pilot, "escape")

    async def domain_back_returns_to_target(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        await observer.press(pilot, "down")
        await observer.press(pilot, "enter")
        await observer.press(pilot, "escape")
        await observer.press(pilot, "escape")
        await observer.press(pilot, "escape")

    async def browser_sign_in_success(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        await observer.wait_for(
            pilot, lambda: app._exit, "the sign-in worker to exit the app"
        )

    async def browser_sign_in_error_then_retry(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        screen = app.screen
        await observer.wait_for(
            pilot, lambda: screen.state.variant == "error", "the error state"
        )
        observer.focus["browser_sign_in:errorVariant"] = screen.state.variant
        await observer.press(pilot, "r")
        await observer.wait_for(
            pilot, lambda: app._exit, "the retried worker to exit the app"
        )

    async def browser_sign_in_retry_refused_while_running(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        screen = app.screen
        await observer.wait_for(
            pilot, lambda: screen.state.running, "the attempt to be running"
        )
        await observer.press(pilot, "r")
        await observer.press(pilot, "escape")

    async def browser_sign_in_manual_fallback(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        screen = app.screen
        await observer.wait_for(
            pilot, lambda: screen.state.running, "the attempt to be running"
        )
        await observer.press(pilot, "m")
        await observer.press(pilot, "escape")

    async def browser_sign_in_persist_failure_exits(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        await observer.wait_for(
            pilot, lambda: app._exit, "the persist failure to exit the app"
        )

    async def browser_sign_in_cancel_closes_service(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        screen = app.screen
        await observer.wait_for(
            pilot, lambda: screen.state.running, "the attempt to be running"
        )
        await observer.press(pilot, "escape")

    async def api_key_submit(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        screen = app.screen
        observer.focus["api_key:masked"] = screen.input_widget.password
        await observer.press(pilot, *"oracle-key")
        await observer.press(pilot, "enter")

    async def api_key_empty_submit_stays(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "enter")
        await observer.press(pilot, "enter")
        observer.focus["api_key:stillOnScreen"] = observer.screen_name()
        await observer.press(pilot, "escape")

    async def cancel_from_theme(
        app: Any, pilot: Any, observer: GraphObserver
    ) -> None:
        await _skip_welcome(app, pilot, observer)
        await observer.press(pilot, "escape")

    def theme_extra(app: Any) -> dict[str, Any]:
        from vibe.cli.textual_ui.widgets.theme_picker import sorted_theme_names

        return {"themes": sorted_theme_names()}

    hang = {"statuses": ["opening_browser", "waiting_for_browser_sign_in"], "hang": True}
    success = {
        "statuses": [
            "opening_browser",
            "waiting_for_browser_sign_in",
            "exchanging",
            "completed",
        ]
    }
    return [
        {
            "case": "graph-browser-provider-installs-seven-screens",
            "browser": True,
            "attempts": [],
            "drive": full_install,
        },
        {
            "case": "graph-manual-provider-installs-three-screens",
            "browser": False,
            "drive": theme_to_api_key_without_browser,
        },
        {
            "case": "welcome-advance-is-inert-until-typing-finishes",
            "browser": True,
            "attempts": [],
            "drive": welcome_gates_on_typing,
        },
        {
            "case": "theme-selection-applies-live-and-leads-on",
            "browser": True,
            "attempts": [],
            "drive": theme_navigation,
            "extra": theme_extra,
        },
        {
            "case": "auth-method-browser-leads-to-target",
            "browser": True,
            "attempts": [],
            "drive": auth_method_browser,
        },
        {
            "case": "auth-method-manual-leads-to-api-key",
            "browser": True,
            "attempts": [],
            "drive": auth_method_manual,
        },
        {
            "case": "auth-method-selection-wraps",
            "browser": True,
            "attempts": [],
            "drive": auth_method_selection_wraps,
        },
        {
            "case": "target-default-applies-mistral-and-signs-in",
            "browser": True,
            "attempts": [hang],
            "drive": target_default_applies_mistral,
        },
        {
            "case": "target-other-leads-to-custom-domain",
            "browser": True,
            "attempts": [],
            "drive": target_other_leads_to_custom_domain,
        },
        {
            "case": "target-back-returns-to-method",
            "browser": True,
            "attempts": [],
            "drive": target_back_returns_to_method,
        },
        {
            "case": "target-overwrite-arms-then-proceeds",
            "browser": True,
            "attempts": [hang],
            "configuredDomain": "https://console.internal.example",
            "drive": target_overwrite_arms_then_proceeds,
        },
        {
            "case": "target-overwrite-disarms-on-move",
            "browser": True,
            "attempts": [],
            "configuredDomain": "https://console.internal.example",
            "drive": target_overwrite_disarms_on_move,
        },
        {
            "case": "custom-domain-validation-classes",
            "browser": True,
            "attempts": [],
            "drive": domain_validation_classes,
        },
        {
            "case": "custom-domain-submit-derives-urls-and-signs-in",
            "browser": True,
            "attempts": [hang],
            "drive": domain_submit_valid,
        },
        {
            "case": "custom-domain-sign-in-upserts-the-provider",
            "browser": True,
            "attempts": [success],
            "drive": domain_submit_valid_then_signs_in,
        },
        {
            "case": "custom-domain-invalid-submission-stays",
            "browser": True,
            "attempts": [],
            "drive": domain_submit_invalid_stays,
        },
        {
            "case": "custom-domain-back-returns-to-target",
            "browser": True,
            "attempts": [],
            "drive": domain_back_returns_to_target,
        },
        {
            "case": "browser-sign-in-success-exits-completed",
            "browser": True,
            "attempts": [success],
            "drive": browser_sign_in_success,
        },
        {
            "case": "browser-sign-in-error-then-retry-succeeds",
            "browser": True,
            "attempts": [
                {
                    "statuses": ["opening_browser", "waiting_for_browser_sign_in"],
                    "raise": "poll_failed",
                },
                success,
            ],
            "drive": browser_sign_in_error_then_retry,
        },
        {
            "case": "browser-sign-in-retry-refused-while-running",
            "browser": True,
            "attempts": [hang],
            "drive": browser_sign_in_retry_refused_while_running,
        },
        {
            "case": "browser-sign-in-manual-fallback",
            "browser": True,
            "attempts": [hang],
            "drive": browser_sign_in_manual_fallback,
        },
        {
            "case": "browser-sign-in-persist-failure-exits-immediately",
            "browser": True,
            "attempts": [success],
            "persistOutcome": "save_error:scripted-detail",
            "drive": browser_sign_in_persist_failure_exits,
        },
        {
            "case": "browser-sign-in-cancel-closes-the-service",
            "browser": True,
            "attempts": [hang],
            "drive": browser_sign_in_cancel_closes_service,
        },
        {
            "case": "api-key-submit-terminates-with-the-persist-outcome",
            "browser": False,
            "drive": api_key_submit,
        },
        {
            "case": "api-key-empty-submission-stays",
            "browser": False,
            "drive": api_key_empty_submit_stays,
        },
        {
            "case": "cancel-from-theme-exits-with-nothing",
            "browser": True,
            "attempts": [],
            "drive": cancel_from_theme,
        },
    ]


async def capture_screen_graph() -> list[dict[str, Any]]:
    records = []
    for scenario in graph_scenarios():
        records.append(await run_scenario(scenario))
    return records


# --------------------------------------------------------------------------
# terminating: the five values and their exit paths
# --------------------------------------------------------------------------


def capture_terminating() -> list[dict[str, Any]]:
    """The five terminating values and the exit path ``run_onboarding`` takes.

    ``VIBE_HOME`` points at a fixed, never-created path for this family: the
    save-error message embeds the global env-file path, and a stable path keeps
    its digest identical across machines. Nothing writes here, because the
    orchestrator and the printer are recorders.
    """

    import vibe.setup.onboarding as onboarding_module

    values = [
        ("cancelled", None),
        ("completed", "completed"),
        ("env-var-error", "env_var_error:ORACLE_BAD_KEY"),
        ("save-error", "save_error:scripted-detail"),
        ("provider-config-error", "provider_config_error:scripted-detail"),
    ]

    records = []
    for label, value in values:
        printed: list[str] = []
        theme_writes: list[Any] = []
        exit_codes: list[int] = []

        def record_print(message: Any) -> None:
            printed.append(str(message))

        def record_exit(code: int = 0) -> None:
            exit_codes.append(code)
            raise SystemExit(code)

        class _Orchestrator:
            async def set_field(self, pointer: str, value: Any, reason: str) -> None:
                theme_writes.append({"pointer": pointer, "value": value, "reason": reason})

        async def fake_orchestrator() -> Any:
            return _Orchestrator()

        fake_app = SimpleNamespace(
            run=lambda _value=value: _value,
            theme="oracle-theme",
        )

        raised: int | None = None
        with (
            patched_environ("VIBE_HOME", ORACLE_VIBE_HOME),
            patched(onboarding_module, "rprint", record_print),
            patched(
                onboarding_module,
                "build_default_orchestrator",
                fake_orchestrator,
            ),
            patched(onboarding_module.sys, "exit", record_exit),
        ):
            try:
                onboarding_module.run_onboarding(app=fake_app)  # type: ignore[arg-type]
            except SystemExit as error:
                raised = int(error.code or 0)

        records.append(
            {
                "case": label,
                "value": value,
                "exitCode": raised,
                "themePersisted": bool(theme_writes),
                "themeWrites": theme_writes,
                "messages": [digest(message) for message in printed],
            }
        )
    return records


# --------------------------------------------------------------------------
# domainValidation
# --------------------------------------------------------------------------


def domain_cases() -> list[str]:
    return [
        "console.mistral.ai",
        "https://console.mistral.ai",
        "https://console.mistral.ai/",
        "console.internal.example",
        "https://console.internal.example",
        "http://localhost:8080",
        "localhost:8080",
        "console.oracle.mistral.ai",
        "https://console.oracle.mistral.ai",
        "console.mistral.ai.evil.example",
        "foo.mistral.ai",
        "console.example.com",
        "example.com/console",
        "example.com:8443",
        "192.168.1.10",
        "192.168.1.10:8080",
        "https://user:pass@example.com",
        "",
        "   ",
        "http:/broken",
        "not a domain",
        "ftp://example.com",
        "https://",
        "https://.",
        "console..mistral.ai",
        "münchen.example",
    ]


def capture_domain_validation() -> list[dict[str, Any]]:
    from vibe.setup.onboarding.context import (
        is_likely_mistral_private_cloud_domain,
        is_valid_custom_domain,
        resolve_browser_auth_urls,
    )

    records = []
    for value in domain_cases():
        valid = is_valid_custom_domain(value)
        record: dict[str, Any] = {
            "case": value if value.strip() else f"blank-{len(value)}",
            "input": value,
            "valid": valid,
        }
        if valid:
            base, api = resolve_browser_auth_urls(value)
            record["privateCloudWarning"] = is_likely_mistral_private_cloud_domain(
                value
            )
            record["derivedBaseUrl"] = base
            record["derivedApiBaseUrl"] = api
        records.append(record)
    return records


# --------------------------------------------------------------------------
# screenProse
# --------------------------------------------------------------------------

#: Call keywords whose string values are identifiers rather than prose:
#: widget ids, CSS classes and binding keys.
_IDENTIFIER_KEYWORDS = frozenset({"id", "classes", "name", "key", "action"})

#: A run shorter than this, or without a space, is a token rather than a
#: sentence; the guard the digests feed is about authored prose.
_MINIMUM_PROSE_LENGTH = 8


def _is_prose(value: str) -> bool:
    """Whether a string literal reads as an authored run rather than a token."""
    if len(value) < _MINIMUM_PROSE_LENGTH or " " not in value:
        return False
    # A CSS selector list or a class chain is lowercase, hyphenated and
    # punctuation-free; an authored run carries a capital or a mark.
    return any(character.isupper() or character in ",.!?'" for character in value)


def prose_runs(source: str) -> list[str]:
    """Every authored run a module's string literals carry.

    Docstrings and the identifier keywords above are excluded, so what remains
    is the text the reference draws. Over-collection is safe: the digests are
    only ever asserted *unequal* to this port's own sentences, so a run that is
    not really user-facing costs nothing while a missing run would weaken the
    guard.
    """
    tree = ast.parse(source)
    excluded: set[int] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Call):
            for keyword in node.keywords:
                if keyword.arg in _IDENTIFIER_KEYWORDS and isinstance(
                    keyword.value, ast.Constant
                ):
                    excluded.add(id(keyword.value))
        if isinstance(
            node, ast.Module | ast.ClassDef | ast.FunctionDef | ast.AsyncFunctionDef
        ):
            if ast.get_docstring(node, clean=False) is not None and node.body:
                first = node.body[0]
                if isinstance(first, ast.Expr):
                    excluded.add(id(first.value))
    runs = {
        node.value
        for node in ast.walk(tree)
        if isinstance(node, ast.Constant)
        and isinstance(node.value, str)
        and id(node) not in excluded
        and _is_prose(node.value)
    }
    return sorted(runs)


def capture_screen_prose() -> list[dict[str, Any]]:
    """The onboarding subtree's authored runs, by module, as length and digest.

    `NOTICE` forbids shipping the reference's sentences, so this port writes
    its own against the same directive coverage and the replay holds every one
    of them permanently unequal to these digests.
    """
    specification = importlib.util.find_spec("vibe.setup.onboarding")
    if specification is None or specification.origin is None:
        raise OracleError("the pinned tree does not publish vibe.setup.onboarding")
    package = Path(specification.origin).parent
    records = []
    for module in sorted(package.rglob("*.py")):
        if "__pycache__" in module.parts:
            continue
        runs = prose_runs(module.read_text(encoding="utf-8"))
        if not runs:
            continue
        records.append(
            {
                "module": module.relative_to(package).as_posix(),
                "runs": sorted(
                    (digest(run) for run in runs), key=lambda entry: entry["digest"]
                ),
            }
        )
    if not records:
        raise OracleError("no authored run was found in the onboarding screens")
    return records


# --------------------------------------------------------------------------
# constants
# --------------------------------------------------------------------------


def capture_constants() -> dict[str, Any]:
    from vibe.setup.onboarding.screens.browser_sign_in import (
        SIGN_IN_URL_HELP_DELAY_SECONDS,
        STEP_DESCRIPTIONS,
        SUCCESS_EXIT_DELAY_SECONDS,
        BrowserSignInStep,
    )
    from vibe.setup.onboarding.screens.theme_selection import (
        FADE_CLASSES,
        VISIBLE_NEIGHBORS,
    )
    from vibe.setup.onboarding.gradient_text import GRADIENT_COLORS

    return {
        "signInSteps": len(STEP_DESCRIPTIONS),
        "signInStepNames": [step.name for step in BrowserSignInStep],
        "successExitDelaySeconds": SUCCESS_EXIT_DELAY_SECONDS,
        "signInUrlHelpDelaySeconds": SIGN_IN_URL_HELP_DELAY_SECONDS,
        "themeVisibleNeighbors": VISIBLE_NEIGHBORS,
        "themeFadeClasses": list(FADE_CLASSES),
        "gradientColorCount": len(GRADIENT_COLORS),
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
        GUARD.install()
        os.environ["VIBE_TEST_DISABLE_KEYRING"] = "1"
        with tempfile.TemporaryDirectory(prefix="onboarding-oracle-") as scratch:
            os.environ["VIBE_HOME"] = str(Path(scratch) / "vibe-home")
            screen_graph = asyncio.run(capture_screen_graph())
            terminating = capture_terminating()
            domain_validation = capture_domain_validation()
            screen_prose = capture_screen_prose()
            constants = capture_constants()
    except OracleError as error:
        print(f"onboarding capture failed: {error}", file=sys.stderr)
        return 1

    if GUARD.attempts:
        print(
            f"onboarding capture failed: network attempts {GUARD.attempts}",
            file=sys.stderr,
        )
        return 1

    corpus = {
        "schemaVersion": SCHEMA_VERSION,
        "reference": {"commit": reference["commit"]},
        "note": (
            "Onboarding corpus: the reference's screen graph, transitions, focus targets, "
            "validation classes, persisted effects and terminating values, observed by driving "
            "the real OnboardingApp through Textual's headless pilot with the sign-in service, "
            "the credential persistence and the config orchestrator replaced by recorders. No "
            "rendered cell, SVG or styled text appears here, and every reference-authored "
            "message and screen sentence is committed as a SHA-256 digest and a length. "
            "Regenerate with scripts/parity/onboarding.py --corpus when the pinned reference "
            "moves."
        ),
        "constants": constants,
        "screenGraph": screen_graph,
        "terminating": terminating,
        "domainValidation": domain_validation,
        "screenProse": screen_prose,
    }

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
        )
        os.replace(staged, arguments.corpus)

    counted = {
        "screenGraph": len(screen_graph),
        "terminating": len(terminating),
        "domainValidation": len(domain_validation),
        "screenProse": sum(len(record["runs"]) for record in screen_prose),
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
