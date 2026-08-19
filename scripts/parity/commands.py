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

The corpus records seven families the Rust replay in
``crates/vibe-cli/src/tui/commands_parity_tests.rs`` compares this build
against::

    counts          how many keys, aliases, slash aliases and bare aliases exist
    inventory       every alias of every registry key, attributed to its key
    availability    which keys survive each context the CLI can produce
    parse           what a submitted line resolves to, over 50 authored probes
    helpDocument    the whole document's line and section totals
    helpSections    each section's position, heading level and line count
    helpCommands    each command line's position and ordered alias list
    helpProse       every non-blank help line as a length and a SHA-256

``helpProse`` is the licensing boundary made measurable: the reference's help
lines are authored prose ``NOTICE`` forbids reproducing, so the corpus records
each one as a length plus a digest and never as text. The replay compares this
port's own lines against those digests and requires permanent inequality.

The registry imports ``vibe.utils``, which imports pydantic, so the ambient
interpreter is usually not enough: like every other oracle here the script
re-executes itself under the reference's virtual environment while importing the
*pinned* tree, extracted out of the checkout with ``git archive`` so the checkout
is never moved and an off-pin working tree is still an oracle.

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
import contextlib
import hashlib
import json
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

SCHEMA_VERSION = 1
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

#: Every context the CLI can produce: ``vibe_code_enabled`` crossed with
#: clipboard support, plus one carrying a non-empty excluded set.
CONTEXTS: tuple[dict[str, Any], ...] = (
    {"id": "baseline", "vibeCodeEnabled": False, "clipboardSupported": False, "excluded": []},
    {"id": "clipboard", "vibeCodeEnabled": False, "clipboardSupported": True, "excluded": []},
    {"id": "vibeCode", "vibeCodeEnabled": True, "clipboardSupported": False, "excluded": []},
    {"id": "full", "vibeCodeEnabled": True, "clipboardSupported": True, "excluded": []},
    {
        "id": "excluded",
        "vibeCodeEnabled": True,
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
    ("paste-image-vibe-code", "vibeCode", "/paste-image"),
    ("paste-image-clipboard", "clipboard", "/paste-image"),
    ("teleport-full", FULL_CONTEXT, "/teleport"),
    ("teleport-baseline", "baseline", "/teleport"),
    ("teleport-vibe-code", "vibeCode", "/teleport"),
    ("remote-project-baseline", "baseline", "/remote-project"),
    ("status-baseline", "baseline", "/status"),
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

    ``vibe.cli.commands`` imports ``vibe.utils``, which imports pydantic, so an
    ambient interpreter without the reference's dependencies cannot act as the
    oracle even though the registry itself is plain dataclasses.
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
            vibe_code_enabled=context["vibeCodeEnabled"],
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
                "vibeCodeEnabled": context["vibeCodeEnabled"],
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
