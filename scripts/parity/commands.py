#!/usr/bin/env python3
"""Capture what the pinned Python reference's command registry answers.

Row 2 of ``docs/parity.md`` covers slash commands, and until this script existed
it was the only row measured by a hand-diffed list of names rather than by a
replay. ``vibe/cli/commands.py`` publishes everything that row is about:
``CommandRegistry`` builds the table, filters it by availability, resolves an
alias into a key, splits a submitted line into a key and its arguments, and
renders the help document ``/help`` prints. This script drives that class
directly, over inputs it authors, and writes
``crates/vibe-cli/tests/commands/corpus.json``.

The corpus records eight families the Rust replay in
``crates/vibe-cli/src/tui/commands_parity_tests.rs`` compares this build
against::

    counts          how many keys, aliases, slash aliases and bare aliases exist
    inventory       every alias of every registry key, attributed to its key
    availability    which keys survive each context the CLI can produce
    parse           what a submitted line resolves to, over 61 authored probes
    helpDocument    the whole document's line and section totals
    helpSections    each section's position, heading level and line count
    helpCommands    each command line's position and ordered alias list
    helpProse       every non-blank help line as a length and a SHA-256

``helpProse`` is the licensing boundary made measurable: the reference's help
lines are authored prose ``NOTICE`` forbids reproducing, so the corpus records
each one as a length plus a digest and never as text. The replay compares this
port's own lines against those digests and requires permanent inequality.

Like every other oracle here the script re-executes itself under the
reference's virtual environment while importing the *pinned* tree, extracted out
of the checkout with ``git archive`` so the checkout is never moved and an
off-pin working tree is still an oracle. At the current pin the registry module
imports only the standard library and ``vibe.cli.constants``, so the re-exec is
what guarantees the *pinned* ``vibe`` package is the one imported rather than a
dependency requirement.

Usage::

    scripts/parity/commands.py
    scripts/parity/commands.py --reference /path/to/reference
    scripts/parity/commands.py --output target/commands-corpus.json

``VIBE_REFERENCE`` sets the checkout for machines that hold it elsewhere;
``--reference`` wins over it. ``VIBE_PARITY_PYTHON`` names an interpreter for a
checkout whose virtual environment sits somewhere else.
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
import shutil
import socket
import subprocess
import sys
import tarfile
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT, EXPECTED_VERSION, RESTORE_COMMAND

SCHEMA_VERSION = 3
DEFAULT_OUTPUT = Path("crates/vibe-cli/tests/commands/corpus.json")
DEFAULT_CACHE = Path(".parity")
INTERPRETER_VARIABLE = "VIBE_PARITY_PYTHON"

#: Set on the re-executed process so it does not extract and re-exec forever.
_REEXEC_MARKER = "VIBE_PARITY_PINNED_TREE"

#: What ``platform.system()`` answers on the one platform the reference lets
#: ``/paste-image`` exist on, and on a stand-in for every other. The predicate
#: reads the live module, so both branches are captured from one host.
CLIPBOARD_SYSTEM = "Darwin"
NON_CLIPBOARD_SYSTEM = "Linux"

#: The keys excluded by the one context that exercises ``excluded_commands``.
#: Three keys rather than one, so an exclusion is observed against a command
#: with a single alias, a command carrying two, and a command whose key is the
#: first in sort order.
EXCLUDED_KEYS = ("help", "mcp", "theme")

#: Every gate the registry reads, each opened alone and all opened together,
#: plus one context carrying a non-empty excluded set. ``CommandContext``
#: (``vibe/cli/commands.py:10-13``) carries ``registry_skills_enabled`` and
#: ``experimental_harness``, and ``/paste-image`` reads the host platform; no
#: predicate reads two of them, so one context per gate reaches every branch.
CONTEXTS: tuple[dict[str, Any], ...] = (
    {
        "id": "baseline",
        "registrySkillsEnabled": False,
        "experimentalHarness": False,
        "clipboardSupported": False,
        "excluded": [],
    },
    {
        "id": "clipboard",
        "registrySkillsEnabled": False,
        "experimentalHarness": False,
        "clipboardSupported": True,
        "excluded": [],
    },
    {
        "id": "registrySkills",
        "registrySkillsEnabled": True,
        "experimentalHarness": False,
        "clipboardSupported": False,
        "excluded": [],
    },
    {
        "id": "experimentalHarness",
        "registrySkillsEnabled": False,
        "experimentalHarness": True,
        "clipboardSupported": False,
        "excluded": [],
    },
    {
        "id": "full",
        "registrySkillsEnabled": True,
        "experimentalHarness": True,
        "clipboardSupported": True,
        "excluded": [],
    },
    {
        "id": "excluded",
        "registrySkillsEnabled": True,
        "experimentalHarness": True,
        "clipboardSupported": True,
        "excluded": list(EXCLUDED_KEYS),
    },
)

#: The context the inventory and the help document are captured under: every
#: command available, so the corpus describes the whole table rather than the
#: subset the capturing host happens to allow.
FULL_CONTEXT = "full"

#: The lines a submitted command line can be, with the context each is resolved
#: under. Chosen to reach the reference's own branches rather than to sample:
#: trimming, the whitespace split, bare aliases with and without arguments,
#: Unicode case folding, availability gating and the empty input.
PARSE_PROBES: tuple[tuple[str, str, str], ...] = (
    # (id, context, input)
    ("help-slash", FULL_CONTEXT, "/help"),
    ("help-upper", FULL_CONTEXT, "/HELP"),
    ("help-mixed-case", FULL_CONTEXT, "/HeLp"),
    ("help-leading-spaces", FULL_CONTEXT, "   /help"),
    ("help-trailing-spaces", FULL_CONTEXT, "/help   "),
    ("help-surrounding-spaces", FULL_CONTEXT, "  /help  "),
    ("help-leading-tab", FULL_CONTEXT, "\t/help"),
    ("help-trailing-newline", FULL_CONTEXT, "/help\n"),
    ("mcp-with-arguments", FULL_CONTEXT, "/mcp add server"),
    ("mcp-interior-whitespace-run", FULL_CONTEXT, "/mcp    add     server"),
    ("mcp-newline-separated-arguments", FULL_CONTEXT, "/mcp\nadd server"),
    ("mcp-tab-separated-arguments", FULL_CONTEXT, "/mcp\tadd"),
    ("connectors-alias", FULL_CONTEXT, "/connectors"),
    ("connectors-alias-with-arguments", FULL_CONTEXT, "/connectors add https://example.test"),
    ("new-alias", FULL_CONTEXT, "/new"),
    ("continue-alias", FULL_CONTEXT, "/continue"),
    ("exit-bare", FULL_CONTEXT, "exit"),
    ("quit-bare", FULL_CONTEXT, "quit"),
    ("colon-q-bare", FULL_CONTEXT, ":q"),
    ("colon-quit-bare", FULL_CONTEXT, ":quit"),
    ("exit-bare-with-arguments", FULL_CONTEXT, "exit now"),
    ("quit-bare-with-arguments", FULL_CONTEXT, "quit please stop"),
    ("colon-q-bare-with-arguments", FULL_CONTEXT, ":q now"),
    ("exit-bare-uppercase", FULL_CONTEXT, "EXIT"),
    ("exit-bare-surrounding-spaces", FULL_CONTEXT, "  exit  "),
    ("exit-slash", FULL_CONTEXT, "/exit"),
    ("exit-slash-with-arguments", FULL_CONTEXT, "/exit now"),
    ("empty", FULL_CONTEXT, ""),
    ("spaces-only", FULL_CONTEXT, "   "),
    ("tabs-only", FULL_CONTEXT, "\t\t"),
    ("newline-only", FULL_CONTEXT, "\n"),
    ("unknown-slash", FULL_CONTEXT, "/nope"),
    ("unknown-bare", FULL_CONTEXT, "nope"),
    ("unknown-slash-with-arguments", FULL_CONTEXT, "/nope arg"),
    ("double-slash", FULL_CONTEXT, "//help"),
    ("slash-only", FULL_CONTEXT, "/"),
    ("kelvin-sign-thinking", FULL_CONTEXT, "/THINKING"),
    ("kelvin-sign-in-arguments", FULL_CONTEXT, "/compact Kelvin"),
    ("dotted-capital-i-thinking", FULL_CONTEXT, "/THİNKING"),
    ("fullwidth-help", FULL_CONTEXT, "/ＨＥＬＰ"),
    ("compact-with-instructions", FULL_CONTEXT, "/compact focus on the parser"),
    ("loop-with-arguments", FULL_CONTEXT, "/loop 5m check the build"),
    ("rename-with-quoted-argument", FULL_CONTEXT, '/rename "My Session"'),
    ("retry-with-long-argument", FULL_CONTEXT, "/retry " + "continue the response " * 12),
    ("paste-image-full", FULL_CONTEXT, "/paste-image"),
    ("paste-image-baseline", "baseline", "/paste-image"),
    ("paste-image-experimental-harness", "experimentalHarness", "/paste-image"),
    ("paste-image-clipboard", "clipboard", "/paste-image"),
    ("teleport-full", FULL_CONTEXT, "/teleport"),
    ("teleport-baseline", "baseline", "/teleport"),
    ("teleport-experimental-harness", "experimentalHarness", "/teleport"),
    ("remote-project-baseline", "baseline", "/remote-project"),
    ("status-baseline", "baseline", "/status"),
    ("skills-baseline", "baseline", "/skills"),
    ("skills-registry-skills", "registrySkills", "/skills"),
    ("todo-baseline", "baseline", "/todo"),
    ("todo-experimental-harness", "experimentalHarness", "/todo"),
    ("help-excluded", "excluded", "/help"),
    ("connectors-excluded", "excluded", "/connectors"),
    ("theme-excluded", "excluded", "/theme"),
    ("status-excluded", "excluded", "/status"),
)


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
    """The pinned commit, read out of the checkout without depending on its HEAD.

    Everything downstream reads the pinned tree through ``git archive``, so a
    checkout parked on another revision is still an oracle as long as it holds
    the commit. A checkout that is absent, unreadable or missing the pin is not,
    and the refusal happens here, before a byte of the corpus is written.
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
    return {"commit": expected, "version": EXPECTED_VERSION}


def extract_pinned_tree(reference: Path, commit: str, cache: Path) -> Path:
    """The pinned source tree, materialized out of tree and reused across runs.

    The extraction lands in a per-process directory and is moved into place at
    the end, so a concurrent run reads a complete tree or builds its own rather
    than importing a half-written one.
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


def _imports_pinned_vibe(tree: Path) -> bool:
    try:
        import vibe
    except Exception:
        return False
    return Path(vibe.__file__).resolve().is_relative_to(tree.resolve())


def reexecute_with_reference_interpreter(
    reference: Path, override: Path | None, tree: Path
) -> None:
    """Re-runs this script under an interpreter importing the *pinned* tree.

    The parent process never puts the extracted tree on ``sys.path``, so an
    ambient ``vibe`` (or none) is what it would import; the re-exec is what
    makes the pinned tree win over the checkout's editable install.
    """

    if os.environ.get(_REEXEC_MARKER) == str(tree):
        if not _imports_pinned_vibe(tree):
            raise OracleError(
                f"the reference interpreter did not import `vibe` from {tree}"
            )
        return
    if _imports_pinned_vibe(tree):
        return
    candidates = [override] if override else []
    if os.environ.get(INTERPRETER_VARIABLE):
        candidates.append(Path(os.environ[INTERPRETER_VARIABLE]))
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


# --------------------------------------------------------------------------
# Isolation
# --------------------------------------------------------------------------


class _SocketGuard:
    """Fails the capture the moment anything tries to reach a network.

    ``connect``, ``connect_ex``, ``create_connection`` and ``getaddrinfo`` are
    replaced with raisers; ``socketpair`` stays, because asyncio's self-pipe uses
    it without connecting anywhere. Nothing in the registry opens a socket, and
    the guard is what turns that from a belief into a measurement.
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


def digest(value: str) -> dict[str, Any]:
    """A string recorded by its length and its SHA-256, never by its content."""

    return {
        "length": len(value),
        "digest": hashlib.sha256(value.encode("utf-8")).hexdigest(),
    }


# --------------------------------------------------------------------------
# Capture
# --------------------------------------------------------------------------


@contextlib.contextmanager
def clipboard_support(module: Any, supported: bool):
    """The clipboard branch the availability predicate reads.

    ``/paste-image`` is available when ``platform.system()`` answers the
    reference's supported system, which is a property of the capturing host
    rather than of the registry. Both branches are recorded from one host by
    replacing that name for the duration of a ``refresh``, so the corpus
    describes the reference and not the workstation.
    """

    original = module.platform.system
    module.platform.system = lambda: (
        CLIPBOARD_SYSTEM if supported else NON_CLIPBOARD_SYSTEM
    )
    try:
        yield
    finally:
        module.platform.system = original


def build_registry(module: Any, context: dict[str, Any]) -> Any:
    with clipboard_support(module, context["clipboardSupported"]):
        return module.CommandRegistry(
            excluded_commands=list(context["excluded"]),
            context=module.CommandContext(
                registry_skills_enabled=context["registrySkillsEnabled"],
                experimental_harness=context["experimentalHarness"],
            ),
        )


def capture_inventory(registry: Any) -> list[dict[str, Any]]:
    """Every alias of every registry key, attributed to its key.

    The aliases are sorted rather than left in the frozenset's iteration order,
    which is not stable across runs and would make the corpus non-deterministic.
    """

    return [
        {"id": name, "aliases": sorted(command.aliases)}
        for name, command in sorted(registry.commands.items())
    ]


def capture_counts(inventory: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """The four totals, observed from the inventory rather than written by hand."""

    aliases = [alias for entry in inventory for alias in entry["aliases"]]
    return [
        {"id": "keys", "count": len(inventory)},
        {"id": "aliases", "count": len(aliases)},
        {
            "id": "slashAliases",
            "count": len([alias for alias in aliases if alias.startswith("/")]),
        },
        {
            "id": "bareAliases",
            "count": len([alias for alias in aliases if not alias.startswith("/")]),
        },
    ]


def capture_availability(module: Any) -> list[dict[str, Any]]:
    """Which keys survive ``refresh`` under each context the CLI can produce."""

    cases: list[dict[str, Any]] = []
    for context in CONTEXTS:
        registry = build_registry(module, context)
        keys = sorted(registry.commands)
        cases.append(
            {
                "id": context["id"],
                "registrySkillsEnabled": context["registrySkillsEnabled"],
                "experimentalHarness": context["experimentalHarness"],
                "clipboardSupported": context["clipboardSupported"],
                "excluded": list(context["excluded"]),
                "keys": keys,
                "count": len(keys),
            }
        )
    return cases


def capture_parse(module: Any) -> list[dict[str, Any]]:
    """What each authored line resolves to, under the context it names.

    ``parse_command`` answers a key, the command and the arguments, and the alias
    that matched is the alias-map entry ``get_command_name`` looked the head word
    up under. That entry is recorded too: it is the value a port has to agree on
    to prove it resolved through the same alias rather than through a different
    one that happens to share a key.
    """

    registries = {
        context["id"]: build_registry(module, context) for context in CONTEXTS
    }
    cases: list[dict[str, Any]] = []
    for identifier, context_id, text in PARSE_PROBES:
        registry = registries[context_id]
        parsed = registry.parse_command(text)
        if parsed is None:
            key: str | None = None
            alias: str | None = None
            arguments: str | None = None
        else:
            key, _command, arguments = parsed
            alias = text.strip().split(None, 1)[0].lower()
        cases.append(
            {
                "id": identifier,
                "context": context_id,
                "input": text,
                "key": key,
                "alias": alias,
                "arguments": arguments,
            }
        )
    return cases


def _section_bounds(lines: list[str]) -> list[tuple[int, str, int]]:
    """Each heading's index, its text and the index the next one starts at."""

    headings = [
        (index, line) for index, line in enumerate(lines) if line.startswith("#")
    ]
    bounds = []
    for position, (index, line) in enumerate(headings):
        end = headings[position + 1][0] if position + 1 < len(headings) else len(lines)
        bounds.append((index, line, end))
    return bounds


def capture_help(module: Any, registry: Any) -> dict[str, list[dict[str, Any]]]:
    """The help document reduced to structure plus a per-line digest.

    Nothing here records a reference sentence. The headings and the bullet lines
    are authored prose ``NOTICE`` forbids reproducing, so what is written is
    where each line sits, how many there are, which key a command line belongs
    to, which aliases it lists in which order, and a length plus a SHA-256 per
    non-blank line. Blank lines carry no prose and are counted rather than
    digested, so a port whose own document also separates its sections does not
    trip the inequality guard on the empty string.
    """

    text = registry.get_help_text()
    lines = text.split("\n")
    keys = sorted(registry.commands)

    sections: list[dict[str, Any]] = []
    slugs = ("keyboardShortcuts", "specialFeatures", "commands")
    bounds = _section_bounds(lines)
    if len(bounds) != len(slugs):
        raise OracleError(
            f"the help document carries {len(bounds)} sections, not {len(slugs)}"
        )
    for position, ((index, heading, end), slug) in enumerate(zip(bounds, slugs)):
        body = [line for line in lines[index + 1 : end] if line.strip()]
        sections.append(
            {
                "id": slug,
                "index": position,
                "headingLine": index,
                "level": len(heading) - len(heading.lstrip("#")),
                "lineCount": len(body),
            }
        )

    commands_start = bounds[-1][0]
    command_lines = [
        (index, line)
        for index, line in enumerate(lines)
        if index > commands_start and line.strip()
    ]
    if len(command_lines) != len(keys):
        raise OracleError(
            f"the command section holds {len(command_lines)} lines for {len(keys)} keys"
        )
    commands: list[dict[str, Any]] = []
    for position, ((index, line), key) in enumerate(zip(command_lines, keys)):
        canonical = f"/{key}"
        ordered = sorted(
            registry.commands[key].aliases,
            key=lambda alias: (alias != canonical, alias),
        )
        rendered = ", ".join(f"`{alias}`" for alias in ordered)
        if not line.startswith(f"- {rendered}:"):
            raise OracleError(
                f"the command line at {index} does not lead with the aliases of `{key}`"
            )
        commands.append(
            {
                "id": key,
                "index": position,
                "line": index,
                "aliases": ordered,
            }
        )

    prose = [
        {"id": f"line-{index:02d}", **digest(line)}
        for index, line in enumerate(lines)
        if line.strip()
    ]

    document = [
        {"id": "lineCount", "count": len(lines)},
        {"id": "blankLineCount", "count": len(lines) - len(prose)},
        {"id": "sectionCount", "count": len(sections)},
        {"id": "commandLineCount", "count": len(commands)},
    ]

    return {
        "helpDocument": document,
        "helpSections": sections,
        "helpCommands": commands,
        "helpProse": prose,
    }


# --------------------------------------------------------------------------
# Command traits
# --------------------------------------------------------------------------


def capture_traits(registry: Any) -> list[dict[str, Any]]:
    """What each command declares about how it runs: whether it runs on the side
    channel while a job holds the composer, and whether running it ends the
    session."""

    return [
        {"id": name, "sideChannel": command.side_channel, "exits": command.exits}
        for name, command in sorted(registry.commands.items())
    ]


# --------------------------------------------------------------------------
# Handlers, dispatch and the log-level panel
# --------------------------------------------------------------------------
#
# The reference's command handlers are methods of ``VibeApp``, a Textual app
# that needs a terminal, an app server and a session to exist. The capture runs
# those methods anyway, bound to ``StubApp``: an object whose UI surface records
# what it is asked to show and whose app-server resources answer from a fixture
# the scenario authors. Every attribute ``StubApp`` does not define resolves to
# the reference's own ``VibeApp`` member, so the code that decides what a
# command does is the pinned reference's. What is recorded is what an operator
# observes: messages by kind, each text as a length and a SHA-256, panels by
# name, telemetry, submitted turns, the clipboard, exits and refusals.


class _Boom(Exception):
    """The failure a fixture raises. Its text is authored by this script."""


def _ns(**values: Any) -> Any:
    import types

    return types.SimpleNamespace(**values)


def _objectify(value: Any) -> Any:
    if isinstance(value, dict):
        return _ns(**{key: _objectify(item) for key, item in value.items()})
    if isinstance(value, list):
        return [_objectify(item) for item in value]
    return value


def _fail_if_requested(value: Any) -> None:
    if isinstance(value, dict) and "raise" in value:
        raise _Boom(value["raise"])
    if isinstance(value, dict) and "error" in value:
        raise _response_error(value["error"])


def _response_error(message: str) -> Exception:
    from vibe.app_server.protocol import (
        AppServerResponseError,
        ProtocolError,
        ProtocolErrorCode,
    )

    return AppServerResponseError(
        ProtocolError(code=ProtocolErrorCode.INVALID_PARAMS, message=message)
    )


class _Capture:
    """Everything one scenario did that an operator could observe, in order."""

    def __init__(self) -> None:
        self.items: list[Any] = []

    def add(self, **effect: Any) -> None:
        self.items.append(effect)

    def widget(self, widget: Any) -> None:
        self.items.append(widget)


#: Refusal sentences the reference publishes through ``_warn_not_queueable``,
#: recorded by the reason they state rather than by their prose.
_REFUSALS = (
    ("Slash commands cannot be queued", "slashCommand"),
    ("Teleport cannot be queued", "teleport"),
    ("Shell commands cannot be queued", "shell"),
    ("Input cannot be queued while a shell command is running", "shellRunning"),
    ("A slash command is already running", "sideChannelBusy"),
)


def _widget_effect(widget: Any) -> dict[str, Any]:
    from vibe.cli.textual_ui.widgets.status_message import IndicatorState, StatusMessage

    name = type(widget).__name__
    if name == "SlashCommandMessage":
        return {"type": "echo", "text": widget._content}  # noqa: SLF001
    if name == "UserCommandMessage":
        return {"type": "message", "text": widget._content}  # noqa: SLF001
    if name == "ErrorMessage":
        return {"type": "error", "text": widget._error}  # noqa: SLF001
    if name == "WarningMessage":
        return {"type": "warning", "text": widget._message}  # noqa: SLF001
    if isinstance(widget, StatusMessage):
        return {
            "type": "status",
            "text": widget.get_content(),
            "ok": widget._state is not IndicatorState.ERROR,  # noqa: SLF001
        }
    raise OracleError(f"a handler mounted a widget the capture cannot read: {name}")


def _serialize(capture: _Capture, hints: dict[str, str]) -> list[dict[str, Any]]:
    effects: list[dict[str, Any]] = []
    for item in capture.items:
        effect = dict(item) if isinstance(item, dict) else _widget_effect(item)
        if effect["type"] == "notify" and effect["severity"] == "warning":
            refusal = next(
                (reason for prefix, reason in _REFUSALS if effect["text"].startswith(prefix)),
                None,
            )
            if refusal is not None:
                hint = next(
                    (name for name, text in hints.items() if effect["text"].endswith(text)),
                    "none",
                )
                effects.append({"type": "refused", "reason": refusal, "hint": hint})
                continue
        if isinstance(effect.get("text"), str):
            effect["text"] = digest(effect["text"])
        effects.append(effect)
    return effects


class _StubApp:
    """A ``VibeApp`` whose UI surface records and whose resources are fixtures."""

    def __init__(self, vibe_app: type, registry: Any, fixture: dict[str, Any]) -> None:
        from vibe.cli.textual_ui.app import BottomApp
        from vibe.cli.textual_ui.scheduled_loop_runner import ScheduledLoopCommands

        values = {
            "_vibe_app": vibe_app,
            "capture": _Capture(),
            "fixture": fixture,
            "commands": registry,
            "_tools_collapsed": False,
            "_mount_first": False,
            "_pending_turn": False,
            "_agent_task": None,
            "_bash_task": None,
            "_debug_console": None,
            "_todo_tracker": None,
            "_show_resume_picker": False,
            "_loading_widget": None,
            "_banner": None,
            "_whats_new_message": None,
            "_current_bottom_app": BottomApp.Input,
        }
        self.__dict__.update(values)
        server = _FakeAppServer(self, fixture)
        self.__dict__.update(
            {
                "app_server": server,
                "_app_server": server,
                "event_handler": _ns(
                    current_compact=None,
                    begin_retry=lambda command=None: bool(fixture.get("retryOffered")),
                ),
                "_queue": _FakeQueue(self, fixture),
                "_side_channel": _ns(enqueue=self._enqueue_side_channel),
                "_messages_area": _ns(mount=self._mount_and_scroll),
                "_loading_area": _ns(mount=self._ignore),
                "_chat_widget": _ns(scroll_home=lambda **_: None),
                "_load_more": _ns(hide=self._ignore),
                "_terminal_notifier": _ns(
                    set_default_title=lambda title: self.capture.add(
                        type="title", text=title
                    )
                ),
                "_vibe_code_project_picker": _ns(clear_teleport=lambda: None),
                "_session_ready": _ns(wait=self._ignore),
                "config": _ns(
                    active_model=_ns(
                        display_name=fixture.get("activeModelDisplayName", "Model")
                    )
                ),
                "_loop_commands": ScheduledLoopCommands(
                    _FakeLoops(fixture), tools_collapsed=lambda: False
                ),
            }
        )

    def __getattr__(self, name: str) -> Any:
        import inspect
        import types

        raw = inspect.getattr_static(self._vibe_app, name)
        if isinstance(raw, staticmethod):
            return raw.__func__
        if isinstance(raw, property):
            return raw.fget(self)
        if callable(raw):
            return types.MethodType(raw, self)
        return raw

    def __setattr__(self, name: str, value: Any) -> None:
        self.__dict__[name] = value

    async def _ignore(self, *_arguments: Any, **_keywords: Any) -> None:
        return None

    def _noop(self, *_arguments: Any, **_keywords: Any) -> None:
        return None

    # What the operator sees ----------------------------------------------

    async def _mount_and_scroll(self, widget: Any, *_arguments: Any, **_keywords: Any) -> None:
        self.capture.widget(widget)

    async def mount(self, widget: Any, *_arguments: Any, **_keywords: Any) -> None:
        if type(widget).__name__ != "DebugConsole":
            raise OracleError(f"unexpected mount of {type(widget).__name__}")
        self.capture.add(type="panel", name="debugConsole")

    def notify(self, message: Any, *, severity: str = "information", **_keywords: Any) -> None:
        self.capture.add(type="notify", severity=severity, text=str(message))

    def push_screen(self, screen: Any, *_arguments: Any, **_keywords: Any) -> None:
        names = {"ConfigScreen": "config", "TodoOverlayScreen": "todos"}
        name = type(screen).__name__
        self.capture.add(type="panel", name=names.get(name, name))

    async def _switch_from_input(self, widget: Any, *_arguments: Any, **_keywords: Any) -> None:
        marker = getattr(widget, "panel_effect", None)
        if marker is not None:
            self.capture.add(**marker)
            return
        names = {
            "SkillsBrowserApp": "skills",
            "PluginsApp": "plugins",
            "LogLevelPickerApp": "logLevel",
        }
        name = type(widget).__name__
        self.capture.add(type="panel", name=names.get(name, name))

    async def _switch_to_input_app(self, *_arguments: Any, **_keywords: Any) -> None:
        self.capture.add(type="closePanel")

    def _panel(name: str):  # noqa: N805  evaluated in the class body
        async def switch(self: _StubApp, *_arguments: Any, **_keywords: Any) -> None:
            self.capture.add(type="panel", name=name)

        return switch

    _switch_to_model_picker_app = _panel("model")
    _switch_to_thinking_picker_app = _panel("thinking")
    _switch_to_theme_picker_app = _panel("theme")
    _switch_to_voice_app = _panel("voice")
    _switch_to_proxy_setup_app = _panel("proxySetup")
    _show_vibe_code_project_picker = _panel("remoteProjects")
    del _panel

    def _build_picker(self, sessions: list[Any], *, loading: bool = False) -> Any:
        capture = self.capture

        def load_sessions(loaded: list[Any], _titles: dict[str, str]) -> None:
            capture.add(type="sessionsLoaded", count=len(loaded))

        return _ns(
            is_mounted=True,
            load_sessions=load_sessions,
            panel_effect={"type": "panel", "name": "sessions"},
        )

    def _get_last_assistant_message_text(self) -> str | None:
        return self.fixture.get("lastAssistantMessage")

    @contextlib.contextmanager
    def batch_update(self):
        yield

    def run_worker(self, awaitable: Any, **_keywords: Any) -> None:
        self.__dict__.setdefault("_workers", []).append(awaitable)

    def exit(self, *_arguments: Any, **_keywords: Any) -> None:
        self.capture.add(type="exit")

    async def _begin_shutdown(self) -> None:
        return None

    async def _reset_message_widgets(self) -> None:
        self.capture.add(type="resetTranscript")

    async def _handle_user_message(self, text: str, *_arguments: Any, **_keywords: Any) -> None:
        self.capture.add(type="submit", text=text, injected=False)

    async def _handle_turn(self, text: str, *, injected: bool = False, **_keywords: Any) -> None:
        self.capture.add(type="submit", text=text, injected=injected)

    async def _handle_teleport_command(
        self, value: str | None = None, show_message: bool = True
    ) -> None:
        self.capture.add(type="teleport", target=value or "", echo=show_message)

    async def _handle_bash_command(self, command: str) -> None:
        self.capture.add(type="shell", text=command)

    async def _enqueue_prompt_with_resources(
        self, content: str, *, skill_name: str | None = None
    ) -> bool:
        self.capture.add(type="queued", kind="skill" if skill_name else "prompt")
        return True

    def _enqueue_side_channel(self, name: str, _command: Any, _arguments: str, _display: str) -> bool:
        if not self.fixture.get("sideChannelFree", True):
            return False
        self.capture.add(type="run", command=name)
        return True

    def action_rewind_prev(self) -> None:
        self.capture.add(type="panel", name="rewind")

    def action_show_todos(self) -> None:
        self.capture.add(type="panel", name="todos")


    def query_one(self, *_arguments: Any, **_keywords: Any) -> Any:
        return self.__dict__.setdefault("_input", _ns(value=""))

    async def _persist_config_changes(self, changes: dict[str, Any]) -> None:
        _fail_if_requested(self.fixture.get("persist", {}))
        for key, value in changes.items():
            self.capture.add(type="configWrite", key=key, value=value)

    async def _remove_config_field(self, field: str) -> None:
        _fail_if_requested(self.fixture.get("persist", {}))
        self.capture.add(type="configWrite", key=field, value=None)

    _refresh_context_progress = _noop
    _refresh_banner = _noop
    _reset_todo_presentation = _noop
    _sync_terminal_title = _noop
    _mark_session_ready = _noop
    _reset_ui_state = _noop
    _on_busy_state_changed = _noop
    _apply_config_to_ui = _ignore
    _ensure_loading_widget = _ignore
    _remove_loading_widget = _ignore
    _process_startup_prompt_when_available = _ignore


class _FakeAppServer:
    def __init__(self, app: _StubApp, fixture: dict[str, Any]) -> None:
        self._fixture = fixture
        self.session_id = fixture.get("sessionId", "0123456789abcdef-session")
        self.cwd = "/workspace"
        self.history = fixture.get("history", [])
        self.turn_active = fixture.get("turnActive", False)
        self.resources = _FakeResources(app, fixture)

    async def clear_history(self) -> None:
        _fail_if_requested(self._fixture.get("clearHistory", {}))

    async def compact(self, *, extra_instructions: str = "") -> None:
        _fail_if_requested(self._fixture.get("compact", {}))

    def exit_summary(self) -> Any:
        return None


class _FakeResources:
    def __init__(self, app: _StubApp, fixture: dict[str, Any]) -> None:
        async def answer(key: str) -> Any:
            value = fixture.get(key)
            _fail_if_requested(value)
            return _objectify(value)

        self.telemetry = _ns(
            record=lambda event, properties=None, **_: app.capture.add(
                type="telemetry", event=event, properties=dict(properties or {})
            )
        )
        self.runtime = _FakeRuntime(fixture)
        self.sessions = _FakeSessions(app, fixture)
        self.identity = _ns(read=lambda: answer("identity"))
        self.account = _ns(read=lambda: answer("account"), current=None)
        self.config = _FakeConfig(fixture)
        self.mcp = _FakeMcp(app, fixture)
        self.plugins = _ns(read=lambda: answer("plugins"), reload=lambda: answer("plugins"))
        self.agents = _FakeAgents(app, fixture)
        self.skills = _ns(
            read_installed=lambda: answer("installedSkills"),
            catalog=lambda: answer("skillCatalog"),
        )
        self.vibe_code = _ns(open_projects=lambda: answer("openProjects"))


class _FakeRuntime:
    def __init__(self, fixture: dict[str, Any]) -> None:
        stats = fixture.get("stats", {})
        self.stats = _ns(
            steps=stats.get("steps", 0),
            session_prompt_tokens=stats.get("sessionPromptTokens", 0),
            session_cached_tokens=stats.get("sessionCachedTokens", 0),
            session_completion_tokens=stats.get("sessionCompletionTokens", 0),
            session_total_llm_tokens=stats.get("sessionTotalLlmTokens", 0),
            last_turn_total_tokens=stats.get("lastTurnTotalTokens", 0),
            last_turn_cached_tokens=stats.get("lastTurnCachedTokens", 0),
            session_cost=stats.get("sessionCost", 0.0),
        )
        log = fixture.get("sessionLog", {})
        self.session_log = _ns(
            enabled=log.get("enabled", True),
            persisted=log.get("persisted", True),
            path=log.get("path", "/home/operator/.vibe/logs/session/0123456789abcdef-session"),
        )
        self.experimental_harness = False

    async def wait_until_ready(self) -> None:
        return None

    def get_skill(self, name: str) -> Any:
        if name == "review":
            return _ns(name="review", user_invocable=True)
        return None


class _FakeSessions:
    def __init__(self, app: _StubApp, fixture: dict[str, Any]) -> None:
        self._app = app
        self._fixture = fixture

    async def read_log(self) -> Any:
        return self._app.app_server.resources.runtime.session_log

    async def rename(self, title: str) -> str:
        _fail_if_requested(self._fixture.get("rename", {}))
        return title

    async def fork(self, entry_id: str | None = None, *, attach: bool = True) -> Any:
        _fail_if_requested(self._fixture.get("fork", {}))
        self._app.capture.add(type="fork", attach=attach)
        new_id = self._fixture.get("forkedSessionId", "fedcba9876543210-copy")
        return _ns(state=_ns(session=_ns(id=new_id)))

    async def list(self, cwd: str) -> list[Any]:
        return [
            _ns(id=f"session-{index}", title=None, preview="saved")
            for index in range(self._fixture.get("savedSessions", 0))
        ]


class _FakeConfig:
    def __init__(self, fixture: dict[str, Any]) -> None:
        self._fixture = fixture
        self.current = _ns(experimental_enable_registry_skills=False)

    async def reload(self, *, reload_runtime: bool = False) -> int:
        value = self._fixture.get("reload", {})
        _fail_if_requested(value)
        return value.get("strippedImages", 0)


class _FakeMcp:
    def __init__(self, app: _StubApp, fixture: dict[str, Any]) -> None:
        self._app = app
        self._fixture = fixture
        self.state = None

    async def read(self) -> Any:
        from vibe.app_server.models import MCPSourceKind

        mcp = self._fixture.get("mcp", {})
        sources = [
            _ns(name=name, kind=MCPSourceKind.SERVER) for name in mcp.get("sources", [])
        ] + [
            _ns(name=name, kind=MCPSourceKind.CONNECTOR)
            for name in mcp.get("connectors", [])
        ]
        return _ns(
            sources=sources,
            connector_error=mcp.get("connectorError"),
            statuses=dict(mcp.get("statuses", {})),
        )

    async def add(self, **keywords: Any) -> Any:
        added = self._fixture.get("mcpAdd", {})
        _fail_if_requested(added)
        self._app.capture.add(
            type="mcpAdd",
            url=keywords["url"],
            name=keywords["name"] or "",
            scopes=list(keywords["scopes"]),
            transport=keywords["transport"],
            allowInsecureHttp=keywords["allow_insecure_http"],
        )
        return _ns(name=added.get("name", "server"), created=added.get("created", True))

    async def login(self, alias: str):
        login = self._fixture.get("mcpLogin", {})
        _fail_if_requested(login)
        for url in login.get("urls", []):
            yield _ns(url=url)

    async def logout(self, alias: str) -> None:
        _fail_if_requested(self._fixture.get("mcpLogout", {}))
        self._app.capture.add(type="mcpLogout", alias=alias)


class _FakeAgents:
    def __init__(self, app: _StubApp, fixture: dict[str, Any]) -> None:
        self._app = app
        self.all = [_ns(name=name) for name in fixture.get("agents", ["default"])]

    async def set_installed(self, name: str, *, installed: bool) -> None:
        self._app.capture.add(type="agentInstalled", name=name, installed=installed)


class _FakeLoops:
    def __init__(self, fixture: dict[str, Any]) -> None:
        self._fixture = fixture.get("loops", {})

    async def list(self) -> list[Any]:
        _fail_if_requested(self._fixture)
        return [_objectify(loop) for loop in self._fixture.get("list", [])]

    async def create(self, interval: str, prompt: str) -> Any:
        _fail_if_requested(self._fixture)
        created = {"id": "loop-1", "interval_seconds": 300, **self._fixture.get("created", {})}
        created["prompt"] = prompt
        return _objectify(created)

    async def delete(self, loop_id: str) -> Any:
        _fail_if_requested(self._fixture)
        return _objectify({"id": loop_id, "prompt": self._fixture.get("deletedPrompt", "check")})

    async def clear(self) -> int:
        _fail_if_requested(self._fixture)
        return self._fixture.get("cleared", 0)


class _FakeQueue:
    def __init__(self, app: _StubApp, fixture: dict[str, Any]) -> None:
        self._app = app
        self.paused = False
        self.has_server_work = False

    async def clear_server_queue(self) -> None:
        return None

    async def resume(self) -> None:
        self._app.capture.add(type="queueResumed")


#: The instant the loop list measures "next in" from, so the capture is stable.
LOOP_CLOCK = 1_800_000_000.0


@contextlib.contextmanager
def _patched_reference(app: _StubApp, module: Any):
    """Module-level seams the handlers reach without going through ``self``."""

    from vibe.cli import clipboard
    from vibe.cli.textual_ui import app as app_module
    from vibe.cli.textual_ui import scheduled_loop_runner
    from vibe.cli.textual_ui.widgets import messages

    capture = app.capture
    fixture = app.fixture

    def connector_auth_app_class() -> Any:
        def build(**keywords: Any) -> Any:
            return _ns(
                panel_effect={
                    "type": "panel",
                    "name": "connectorAuth",
                    "initial": keywords.get("connector_name") or "",
                }
            )

        return build

    def mcp_app_class() -> Any:
        def build(**keywords: Any) -> Any:
            return _ns(
                panel_effect={
                    "type": "panel",
                    "name": "mcp",
                    "initial": keywords.get("initial_source") or "",
                }
            )

        return build

    def copy_to_clipboard(text: str) -> bool:
        capture.add(type="clipboard", text=text)
        return bool(fixture.get("clipboardVerified", True))

    async def paste_image(_app: Any, *, notify_when_empty: bool = False) -> None:
        capture.add(type="clipboardImage", notifyWhenEmpty=notify_when_empty)

    async def remove_echo(_widget: Any) -> None:
        capture.add(type="removeEcho")

    def open_browser(url: str, *_arguments: Any, **_keywords: Any) -> bool:
        capture.add(type="openUrl", url=url)
        return True

    replaced = [
        (app_module, "_get_mcp_app_class", mcp_app_class),
        (app_module, "_get_connector_auth_app_class", connector_auth_app_class),
        (clipboard, "copy_to_clipboard", copy_to_clipboard),
        (app_module, "handle_clipboard_image_paste", paste_image),
        (messages.SlashCommandMessage, "remove", remove_echo),
        (app_module.webbrowser, "open", open_browser),
        (scheduled_loop_runner.time, "time", lambda: LOOP_CLOCK),
    ]
    saved = [(owner, name, getattr(owner, name)) for owner, name, _ in replaced]
    for owner, name, value in replaced:
        setattr(owner, name, value)
    try:
        with clipboard_support(module, bool(fixture.get("clipboardSupported", False))):
            yield
    finally:
        for owner, name, value in saved:
            setattr(owner, name, value)


def _stub_registry(module: Any, context: dict[str, Any]) -> Any:
    merged = {
        "registrySkillsEnabled": True,
        "experimentalHarness": True,
        "clipboardSupported": True,
        "excluded": [],
        **context,
    }
    return build_registry(module, merged)


async def _drain(app: _StubApp) -> None:
    current = asyncio.current_task()
    for _ in range(10):
        pending = [
            task for task in asyncio.all_tasks() if task is not current and not task.done()
        ]
        workers = app.__dict__.pop("_workers", [])
        if not pending and not workers:
            return
        for worker in workers:
            await worker
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)


def _reject_hints(app_module: Any) -> dict[str, str]:
    return {
        "busy": app_module._REJECT_HINT_BUSY,  # noqa: SLF001
        "paused": app_module._REJECT_HINT_PAUSED,  # noqa: SLF001
    }


#: ``(id, line, context, fixture)``: every command, and every branch of each
#: handler an operator can reach, over fixtures this script authors. The
#: context defaults to every gate open, so a gated command is reachable.
HANDLER_SCENARIOS: tuple[tuple[str, str, dict[str, Any], dict[str, Any]], ...] = (
    ("help", "/help", {}, {}),
    ("config", "/config", {}, {}),
    ("config-arguments", "/config set theme dark", {}, {}),
    ("model", "/model", {}, {}),
    ("model-arguments", "/model devstral-small", {}, {}),
    ("skills", "/skills", {}, {"installedSkills": [], "skillCatalog": {"skills": [], "updates": [], "project_available": False, "loaded": False, "authenticated": False}}),
    ("thinking", "/thinking", {}, {}),
    ("thinking-arguments", "/thinking high", {}, {}),
    ("reload", "/reload", {}, {}),
    ("reload-one-image", "/reload", {}, {"reload": {"strippedImages": 1}, "activeModelDisplayName": "Devstral Small"}),
    ("reload-images", "/reload", {}, {"reload": {"strippedImages": 3}, "activeModelDisplayName": "Devstral Small"}),
    ("reload-failure", "/reload", {}, {"reload": {"raise": "the configuration file is unreadable"}}),
    ("clear", "/clear", {}, {}),
    ("clear-new-alias", "/new", {}, {}),
    ("clear-not-persisted", "/clear", {}, {"sessionLog": {"persisted": False}}),
    ("clear-logging-disabled", "/clear", {}, {"sessionLog": {"enabled": False}}),
    ("clear-seed", "/clear   write the missing tests  ", {}, {}),
    ("clear-failure", "/clear seed", {}, {"clearHistory": {"raise": "the session store is locked"}}),
    ("copy-nothing", "/copy", {}, {}),
    ("copy", "/copy", {}, {"lastAssistantMessage": "The answer is 42."}),
    ("copy-unverified", "/copy", {}, {"lastAssistantMessage": "The answer is 42.", "clipboardVerified": False}),
    ("paste-image", "/paste-image", {}, {"clipboardSupported": True}),
    ("log", "/log", {}, {}),
    ("log-disabled", "/log", {}, {"sessionLog": {"enabled": False}}),
    ("log-not-persisted", "/log", {}, {"sessionLog": {"persisted": False}}),
    ("log-level", "/log-level", {}, {}),
    ("log-level-arguments", "/log-level debug", {}, {}),
    ("debug", "/debug", {}, {}),
    ("compact", "/compact", {}, {"history": ["entry"]}),
    ("compact-instructions", "/compact   keep the API notes ", {}, {"history": ["entry"]}),
    ("compact-empty", "/compact", {}, {"history": []}),
    ("compact-busy", "/compact", {}, {"history": ["entry"], "turnActive": True}),
    ("compact-failure", "/compact", {}, {"history": ["entry"], "compact": {"raise": "the summary could not be produced"}}),
    ("exit", "/exit", {}, {}),
    ("exit-bare", "quit", {}, {}),
    ("status", "/status", {}, {}),
    ("status-cached", "/status", {}, {"stats": {"steps": 7, "sessionPromptTokens": 12345, "sessionCachedTokens": 1000, "sessionCompletionTokens": 2345, "sessionTotalLlmTokens": 14690, "lastTurnTotalTokens": 3210, "lastTurnCachedTokens": 512, "sessionCost": 0.12345}}),
    ("status-uncached", "/status", {}, {"stats": {"steps": 1234567, "sessionPromptTokens": 9876543, "sessionCompletionTokens": 1, "sessionTotalLlmTokens": 9876544, "lastTurnTotalTokens": 42, "sessionCost": 1234.5}}),
    ("whoami-no-identity", "/whoami", {}, {}),
    ("whoami", "/whoami", {}, {"identity": {"name": "Ada Lovelace", "email": "ada@example.test", "workspace": {"id": "w", "name": "Analytical"}, "organization": {"id": "o", "name": "Engines"}}, "account": {"plan": {"title": "Pro"}}}),
    ("whoami-name-is-email", "/whoami", {}, {"identity": {"name": "ada@example.test", "email": "ada@example.test", "workspace": None, "organization": None}, "account": {"plan": None}}),
    ("whoami-sparse", "/whoami", {}, {"identity": {"name": None, "email": None, "workspace": None, "organization": None}}),
    ("whoami-identity-failure", "/whoami", {}, {"identity": {"raise": "unreachable"}, "account": {"plan": {"title": "Pro"}}}),
    ("whoami-account-failure", "/whoami", {}, {"identity": {"name": "Ada", "email": "ada@example.test", "workspace": None, "organization": None}, "account": {"raise": "unreachable"}}),
    ("teleport", "/teleport", {}, {}),
    ("teleport-arguments", "/teleport ship it", {}, {}),
    ("remote-project", "/remote-project", {}, {"openProjects": ["view", "picker"]}),
    ("remote-project-failure", "/remote-project", {}, {"openProjects": {"error": "Vibe Code is unavailable"}}),
    ("proxy-setup", "/proxy-setup", {}, {}),
    ("proxy-setup-arguments", "/proxy-setup HTTPS_PROXY http://proxy.test", {}, {}),
    ("resume", "/resume", {}, {"savedSessions": 2}),
    ("resume-continue-alias", "/continue", {}, {"savedSessions": 1}),
    ("resume-arguments", "/resume session-1", {}, {"savedSessions": 1}),
    ("resume-none", "/resume", {}, {}),
    ("resume-logging-disabled", "/resume", {}, {"savedSessions": 2, "sessionLog": {"enabled": False}}),
    ("rename-empty", "/rename   ", {}, {}),
    ("rename", "/rename  Parser rewrite ", {}, {}),
    ("rename-failure", "/rename Parser rewrite", {}, {"rename": {"raise": "the title is too long"}}),
    ("mcp-empty", "/mcp", {}, {}),
    ("mcp-connectors-alias", "/connectors", {}, {"mcp": {"sources": ["docs", "search"]}}),
    ("mcp", "/mcp", {}, {"mcp": {"sources": ["docs", "search"]}}),
    ("mcp-named", "/mcp search", {}, {"mcp": {"sources": ["docs", "search"]}}),
    ("mcp-unknown", "/mcp nothing", {}, {"mcp": {"sources": ["docs", "search"]}}),
    ("mcp-connector-error", "/mcp", {}, {"mcp": {"connectorError": "the connector catalog timed out"}}),
    ("mcp-connector-error-with-sources", "/mcp", {}, {"mcp": {"sources": ["docs"], "connectorError": "the connector catalog timed out"}}),
    ("mcp-status-empty", "/mcp status", {}, {}),
    ("mcp-status", "/mcp status", {}, {"mcp": {"statuses": {"search": "authenticated", "docs": "needs_login"}}}),
    ("mcp-status-arguments", "/mcp status extra", {}, {}),
    ("mcp-login-usage", "/mcp login", {}, {}),
    ("mcp-login", "/mcp login docs", {}, {"mcpLogin": {"urls": ["https://auth.example.test/authorize"]}}),
    ("mcp-login-connector", "/mcp login search", {}, {"mcp": {"sources": ["docs"], "connectors": ["search"]}}),
    ("mcp-login-shared-alias", "/mcp login docs", {}, {"mcp": {"sources": ["docs"], "connectors": ["docs"]}, "mcpLogin": {"urls": ["https://auth.example.test/authorize"]}}),
    ("mcp-connectors-listed", "/mcp search", {}, {"mcp": {"sources": ["docs"], "connectors": ["search"]}}),
    ("mcp-login-failure", "/mcp login docs", {}, {"mcpLogin": {"error": "Unknown MCP server: docs"}}),
    ("mcp-logout-usage", "/mcp logout   ", {}, {}),
    ("mcp-logout", "/mcp logout docs", {}, {}),
    ("mcp-logout-failure", "/mcp logout docs", {}, {"mcpLogout": {"error": "Unknown MCP server: docs"}}),
    ("mcp-add-help", "/mcp add --help", {}, {}),
    ("mcp-add-short-help", "/mcp add -h", {}, {}),
    ("mcp-add-usage", "/mcp add", {}, {}),
    ("mcp-add-two-urls", "/mcp add https://a.test https://b.test", {}, {}),
    ("mcp-add-unknown-option", "/mcp add https://a.test --verbose", {}, {}),
    ("mcp-add-name-twice", "/mcp add https://a.test --name a --name b", {}, {}),
    ("mcp-add-name-missing", "/mcp add https://a.test --name", {}, {}),
    ("mcp-add-transport-twice", "/mcp add https://a.test --transport http --transport http", {}, {}),
    ("mcp-add-transport-invalid", "/mcp add https://a.test --transport stdio", {}, {}),
    ("mcp-add-scope-missing", "/mcp add https://a.test --scope --no-login", {}, {}),
    ("mcp-add-quoting", "/mcp add 'https://a.test", {}, {}),
    ("mcp-add", "/mcp add https://mcp.example.test --no-login", {}, {"mcpAdd": {"name": "mcp-example"}}),
    ("mcp-add-options", "/mcp add https://mcp.example.test --name docs --scope read --scope write --transport http --allow-insecure-http --no-login", {}, {"mcpAdd": {"name": "docs"}}),
    ("mcp-add-existing", "/mcp add https://mcp.example.test --no-login", {}, {"mcpAdd": {"name": "docs", "created": False}}),
    ("mcp-add-login", "/mcp add https://mcp.example.test --name docs", {}, {"mcpAdd": {"name": "docs"}, "mcpLogin": {"urls": ["https://auth.example.test/authorize"]}}),
    ("mcp-add-failure", "/mcp add https://mcp.example.test", {}, {"mcpAdd": {"error": "An MCP server named docs already exists"}}),
    ("plugins", "/plugins", {}, {}),
    ("reload-plugins", "/reload-plugins", {}, {}),
    ("todo", "/todo", {}, {}),
    ("voice", "/voice", {}, {}),
    ("leanstall", "/leanstall", {}, {"agents": ["default", "plan"]}),
    ("leanstall-present", "/leanstall", {}, {"agents": ["default", "lean"]}),
    ("unleanstall", "/unleanstall", {}, {"agents": ["default", "lean"]}),
    ("unleanstall-absent", "/unleanstall", {}, {"agents": ["default"]}),
    ("rewind", "/rewind", {}, {}),
    ("branch", "/branch", {}, {}),
    ("branch-arguments", "/branch ignored words", {}, {}),
    ("branch-failure", "/branch", {}, {"fork": {"raise": "the session has no history yet"}}),
    ("retry-not-offered", "/retry", {}, {}),
    ("retry", "/retry", {}, {"retryOffered": True}),
    ("retry-instructions", "/retry  keep it short ", {}, {"retryOffered": True}),
    ("retry-busy", "/retry", {}, {"retryOffered": True, "turnActive": True}),
    ("loop-list-empty", "/loop", {}, {}),
    ("loop-list", "/loop list", {}, {"loops": {"list": [{"id": "loop-1", "prompt": "check | the build\nnow", "interval_seconds": 3700, "next_fire_at": LOOP_CLOCK + 125}, {"id": "loop-2", "prompt": "ping", "interval_seconds": 86400, "next_fire_at": LOOP_CLOCK - 5}]}}),
    ("loop-ls", "/loop LS", {}, {}),
    ("loop-create", "/loop 5m   check the build  ", {}, {"loops": {"created": {"id": "loop-7", "interval_seconds": 300}}}),
    ("loop-create-interval-only", "/loop 90s", {}, {"loops": {"created": {"id": "loop-8", "interval_seconds": 90}}}),
    ("loop-cancel", "/loop cancel loop-1", {}, {"loops": {"deletedPrompt": "check the build"}}),
    ("loop-cancel-verb", "/loop RM loop-1", {}, {"loops": {"deletedPrompt": "check the build"}}),
    ("loop-cancel-all", "/loop stop all", {}, {"loops": {"cleared": 3}}),
    ("loop-cancel-missing", "/loop delete", {}, {}),
    ("loop-error", "/loop 5x check", {}, {"loops": {"error": "Invalid interval: 5x"}}),
    ("data-retention", "/data-retention", {}, {}),
    ("theme", "/theme", {}, {}),
)


def capture_handlers(module: Any) -> list[dict[str, Any]]:
    """What each command's handler does, scenario by scenario."""

    from vibe.cli.textual_ui import app as app_module

    hints = _reject_hints(app_module)
    cases: list[dict[str, Any]] = []
    for identifier, line, context, fixture in HANDLER_SCENARIOS:

        async def drive() -> list[dict[str, Any]]:
            registry = _stub_registry(module, context)
            app = _StubApp(app_module.VibeApp, registry, fixture)
            with _patched_reference(app, module):
                handled = await app_module.VibeApp._handle_command(app, line)  # noqa: SLF001
                await _drain(app)
            if not handled:
                app.capture.add(type="unhandled")
            return _serialize(app.capture, hints)

        cases.append(
            {
                "id": identifier,
                "line": line,
                "context": context,
                "fixture": fixture,
                "effects": asyncio.run(drive()),
            }
        )
    return cases


#: The composer states a submitted line can meet: nothing running, a model
#: turn, a shell command, and each of those with the queue paused.
DISPATCH_STATES = ("idle", "busy", "shell", "paused", "pausedBusy", "pausedShell")

#: One line of every kind ``classify`` tells apart, plus the side-channel and
#: exiting commands and a slash line that names nothing.
DISPATCH_INPUTS = (
    ("side-channel", "/status"),
    ("side-channel-exit", "exit"),
    ("command", "/model"),
    ("prompt", "hello"),
    ("skill", "/review this change"),
    ("shell", "!ls -la"),
    ("empty-shell", "!"),
    ("teleport", "&ship it"),
    ("unknown-slash", "/nothing"),
)


def capture_dispatch(module: Any) -> list[dict[str, Any]]:
    """What happens to a submitted line in each composer state."""

    from vibe.cli.textual_ui import app as app_module

    hints = _reject_hints(app_module)
    scenarios = [
        (f"{state}/{kind}", state, text, True)
        for state in DISPATCH_STATES
        for kind, text in DISPATCH_INPUTS
    ] + [
        ("busy/side-channel-occupied", "busy", "/status", False),
        ("paused/side-channel-occupied", "paused", "/help", False),
    ]
    cases: list[dict[str, Any]] = []
    for identifier, state, text, free in scenarios:

        async def drive() -> list[dict[str, Any]]:
            fixture = {"sideChannelFree": free}
            app = _StubApp(app_module.VibeApp, _stub_registry(module, {}), fixture)
            if state in ("busy", "pausedBusy"):
                app.__dict__["_pending_turn"] = True
            held_shell = None
            if state in ("shell", "pausedShell"):
                held_shell = asyncio.get_running_loop().create_future()
                app.__dict__["_bash_task"] = held_shell
            app._queue.paused = state.startswith("paused")  # noqa: SLF001

            async def run_command(value: str) -> bool:
                resolved = app.commands.parse_command(value)
                app.capture.add(type="run", command=resolved[0] if resolved else "")
                return True

            app.__dict__["_handle_command"] = run_command
            with _patched_reference(app, module):
                await app_module.VibeApp._dispatch_submitted_value(app, text)  # noqa: SLF001
                if held_shell is not None:
                    held_shell.cancel()
                await _drain(app)
            effects = _serialize(app.capture, hints)
            restored = app.__dict__.get("_input")
            if restored is not None and restored.value:
                effects.append({"type": "restored"})
            return effects

        cases.append(
            {
                "id": identifier,
                "state": state,
                "input": text,
                "sideChannelFree": free,
                "effects": asyncio.run(drive()),
            }
        )
    return cases


#: ``(id, chain, applied, persist failure)`` for the panel's apply handler.
LOG_LEVEL_APPLY: tuple[tuple[str, dict[str, Any], dict[str, Any], str | None], ...] = (
    ("unchanged", {}, {"session": None, "config": None, "cleared": False}, None),
    ("session", {}, {"session": "DEBUG", "config": None, "cleared": False}, None),
    ("session-cleared", {"session": "INFO"}, {"session": None, "config": None, "cleared": False}, None),
    ("config", {}, {"session": None, "config": "ERROR", "cleared": False}, None),
    ("config-kept", {"config": "INFO"}, {"session": None, "config": "INFO", "cleared": False}, None),
    ("config-cleared", {"config": "INFO"}, {"session": None, "config": None, "cleared": True}, None),
    ("both", {"session": "ERROR", "config": "INFO"}, {"session": "DEBUG", "config": "CRITICAL", "cleared": False}, None),
    ("env-wins", {"env": "INFO"}, {"session": None, "config": "ERROR", "cleared": False}, None),
    ("debug-mode", {"debugMode": True}, {"session": None, "config": None, "cleared": False}, None),
    ("persist-failure", {}, {"session": "DEBUG", "config": "ERROR", "cleared": False}, "the configuration file is read-only"),
)


@contextlib.contextmanager
def _log_level_state(chain: dict[str, Any]):
    from vibe.observability import logging as observability

    state = observability._log_level_state  # noqa: SLF001
    saved = (state._session_override, state._config_level)  # noqa: SLF001
    environment = {key: os.environ.get(key) for key in ("LOG_LEVEL", "DEBUG_MODE")}
    os.environ.pop("LOG_LEVEL", None)
    os.environ.pop("DEBUG_MODE", None)
    if chain.get("env"):
        os.environ["LOG_LEVEL"] = chain["env"]
    if chain.get("debugMode"):
        os.environ["DEBUG_MODE"] = "true"
    state._session_override = chain.get("session")  # noqa: SLF001
    state._config_level = chain.get("config")  # noqa: SLF001
    try:
        yield observability
    finally:
        state._session_override, state._config_level = saved  # noqa: SLF001
        for key, value in environment.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value


def capture_log_level_apply(module: Any) -> list[dict[str, Any]]:
    """What closing the log-level panel writes and reports."""

    from vibe.cli.textual_ui import app as app_module
    from vibe.cli.textual_ui.widgets.log_level_picker import LogLevelPickerApp

    hints = _reject_hints(app_module)
    cases: list[dict[str, Any]] = []
    for identifier, chain, applied, failure in LOG_LEVEL_APPLY:

        async def drive() -> list[dict[str, Any]]:
            fixture = {"persist": {"raise": failure}} if failure else {}
            app = _StubApp(app_module.VibeApp, _stub_registry(module, {}), fixture)
            message = LogLevelPickerApp.Applied(
                applied["session"], applied["config"], config_cleared=applied["cleared"]
            )
            with _log_level_state(chain) as observability:
                await app_module.VibeApp.on_log_level_picker_app_applied(app, message)
                after = observability.get_log_level_chain()
            effects = _serialize(app.capture, hints)
            effects.append({"type": "sessionOverride", "level": after.session})
            return effects

        cases.append(
            {
                "id": identifier,
                "chain": chain,
                "applied": applied,
                "persistFailure": failure,
                "effects": asyncio.run(drive()),
            }
        )
    return cases


#: ``(id, chain, actions)`` for the panel itself. An action is ``highlight``
#: with a level, ``badge`` with ``session`` or ``config``, or ``toggle``.
LOG_LEVEL_PICKER: tuple[tuple[str, dict[str, Any], list[list[str]]], ...] = (
    ("open-default", {}, []),
    ("open-session", {"session": "DEBUG", "config": "ERROR"}, []),
    ("open-env", {"env": "INFO"}, []),
    ("open-config", {"config": "ERROR"}, []),
    ("set-session", {}, [["highlight", "INFO"], ["toggle"]]),
    ("unset-session", {"session": "INFO"}, [["toggle"]]),
    ("set-config", {}, [["highlight", "CRITICAL"], ["badge", "config"], ["toggle"]]),
    ("clear-config", {"config": "ERROR"}, [["highlight", "ERROR"], ["badge", "config"], ["toggle"]]),
    ("move-config", {"config": "ERROR"}, [["highlight", "DEBUG"], ["badge", "config"], ["toggle"]]),
    ("back-to-session", {}, [["badge", "config"], ["badge", "session"], ["highlight", "ERROR"], ["toggle"]]),
    ("both", {"env": "WARNING"}, [["highlight", "DEBUG"], ["toggle"], ["badge", "config"], ["highlight", "INFO"], ["toggle"]]),
)


def capture_log_level_picker(module: Any) -> list[dict[str, Any]]:
    """The panel's own state machine: where it opens, what each toggle does to
    the subtitle, and what closing it applies."""

    from vibe.cli.textual_ui.widgets.log_level_picker import LogLevelPickerApp

    cases: list[dict[str, Any]] = []
    for identifier, chain, actions in LOG_LEVEL_PICKER:
        with _log_level_state(chain) as observability:
            picker = LogLevelPickerApp(chain=observability.get_log_level_chain())
            picker._redraw = lambda: None  # noqa: SLF001  nothing is mounted
            posted: list[Any] = []
            picker.post_message = posted.append  # type: ignore[method-assign]
            subtitles = [digest(picker._subtitle_text())]  # noqa: SLF001
            highlighted = [picker._highlighted_level]  # noqa: SLF001
            for action in actions:
                match action:
                    case ["highlight", level]:
                        picker._highlighted_level = level  # noqa: SLF001
                    case ["badge", badge]:
                        picker._focused_badge = badge  # noqa: SLF001
                    case ["toggle"]:
                        picker._toggle_badge()  # noqa: SLF001
                        subtitles.append(digest(picker._subtitle_text()))  # noqa: SLF001
                    case _:
                        raise OracleError(f"unknown log-level action {action}")
            picker.action_apply()
            applied = posted[-1]
        cases.append(
            {
                "id": identifier,
                "chain": chain,
                "actions": actions,
                "initialHighlight": highlighted[0],
                "subtitles": subtitles,
                "applied": {
                    "session": applied.session_level,
                    "config": applied.config_level,
                    "cleared": applied.config_cleared,
                },
            }
        )
    return cases


NOTE = (
    "Captured from the pinned reference by scripts/parity/commands.py. Registry "
    "keys, aliases, availability sets, parse results and document structure are "
    "observations. Every reference-authored help line is recorded as a length "
    "and a SHA-256 under helpProse and never as text, because NOTICE forbids "
    "shipping reference-authored prose; the replay in "
    "crates/vibe-cli/src/tui/commands_parity_tests.rs holds this port's own help "
    "lines permanently unequal to every digest here."
)


def build_corpus(reference: Path, tree: Path, expected: str) -> dict[str, Any]:
    pin = resolve_reference(reference, expected)
    sys.path.insert(0, str(tree))
    import vibe

    if vibe.__version__ != EXPECTED_VERSION:
        raise OracleError(
            f"the extracted tree publishes {vibe.__version__}, not the pinned "
            f"{EXPECTED_VERSION}"
        )
    import vibe.cli.commands as module

    # Handler failures are logged with a traceback the capture records as an
    # effect instead; the log lines would only be noise on stderr.
    logging.getLogger("vibe").addHandler(logging.NullHandler())
    full = next(context for context in CONTEXTS if context["id"] == FULL_CONTEXT)
    registry = build_registry(module, full)
    inventory = capture_inventory(registry)
    if not inventory:
        raise OracleError("the reference enumerated no commands")
    lowercase = [
        alias
        for entry in inventory
        for alias in entry["aliases"]
        if alias != alias.lower()
    ]
    if lowercase:
        # The parse family records the matched alias as the alias-map entry,
        # which is only the same string as the declared alias while every
        # declared alias is already lowercase.
        raise OracleError(f"the reference declares non-lowercase aliases: {lowercase}")

    corpus: dict[str, Any] = {
        "schemaVersion": SCHEMA_VERSION,
        "reference": pin,
        "note": NOTE,
        "counts": capture_counts(inventory),
        "inventory": inventory,
        "availability": capture_availability(module),
        "parse": capture_parse(module),
    }
    corpus.update(capture_help(module, registry))
    corpus["traits"] = capture_traits(registry)
    corpus["dispatch"] = capture_dispatch(module)
    corpus["handlers"] = capture_handlers(module)
    corpus["logLevelApply"] = capture_log_level_apply(module)
    corpus["logLevelPicker"] = capture_log_level_picker(module)
    if GUARD.attempts:
        raise OracleError(f"the capture reached the network: {GUARD.attempts}")
    return corpus


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    parser.add_argument(
        "--interpreter",
        type=Path,
        default=None,
        help="Python that can import `vibe`; also read from " + INTERPRETER_VARIABLE,
    )
    arguments = parser.parse_args()

    GUARD.install()
    try:
        pin = resolve_reference(arguments.reference, EXPECTED_COMMIT)
        tree = extract_pinned_tree(
            arguments.reference, pin["commit"], arguments.cache
        )
        reexecute_with_reference_interpreter(
            arguments.reference, arguments.interpreter, tree
        )
        corpus = build_corpus(arguments.reference, tree, EXPECTED_COMMIT)
    except OracleError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(
        json.dumps(corpus, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    counts = {entry["id"]: entry["count"] for entry in corpus["counts"]}
    print(
        f"wrote {arguments.output} ({counts['keys']} keys, {counts['aliases']} aliases, "
        f"{len(corpus['parse'])} parse probes, {len(corpus['helpProse'])} help lines)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
