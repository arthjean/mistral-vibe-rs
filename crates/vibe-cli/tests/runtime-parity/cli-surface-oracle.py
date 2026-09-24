#!/usr/bin/env python3
"""Capture the argument grammar the pinned reference's CLI parsers declare.

Row 7 of the scorecard reads the CLI surface, and every number in it was so far
a reading of two declaration sites placed side by side. This oracle introspects
the parsers the reference really builds and drives argv through them, so the
surface is measured instead of counted.

Four parsers are recorded: the root one ``vibe/cli/entrypoint.py`` builds, and
the three ``vibe/cli/mcp_command.py`` builds for ``vibe mcp``, ``vibe mcp add``
and ``vibe mcp remove``. For each, the corpus carries every action's option
strings, dest, nargs, const, default, choices, metavar, required flag, type by
name and action class, the mutually exclusive groups by dest, and the help
render decomposed line by line at ``COLUMNS=80``.

Alongside them, an argv matrix: every vector is driven through the reference's
own entry function and the record carries the exit code, which streams received
output, the last line of each stream and, for a vector that parses, the
namespace it produced.

``NOTICE`` forbids shipping reference-authored prose, so a help body, a
description, an epilog sentence or an error message the reference wrote is
committed as ``{"marker": "<described>", "chars": n, "sha256": ...}``. What
stays in cleartext is structure the reference did not author as prose: flag
spellings, metavars, section headings, bare command and environment variable
names, and the message templates CPython's ``argparse`` renders.

Usage::

    crates/vibe-cli/tests/runtime-parity/cli-surface-oracle.py
    crates/vibe-cli/tests/runtime-parity/cli-surface-oracle.py --check

``VIBE_REFERENCE`` names the checkout on a machine that does not hold it at the
default path and ``--reference`` wins over it. The script re-executes itself
under the reference interpreter with the pinned tree on ``PYTHONPATH``, so the
checkout is read through ``git archive`` and never modified.
"""

from __future__ import annotations

import argparse
import builtins
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "scripts" / "parity"))

from pin import (  # noqa: E402  the path insert above enables it
    DEFAULT_REFERENCE,
    EXPECTED_COMMIT,
    EXPECTED_VERSION,
    RESTORE_COMMAND,
)

SCHEMA_VERSION = 1

#: The corpus the Rust replay reads, beside this script.
DEFAULT_CORPUS = Path(__file__).resolve().with_name("cli-surface.json")

#: Where the extracted pinned tree is cached between runs. Gitignored, and keyed
#: by commit so a re-pin extracts a new one instead of reusing the old.
DEFAULT_CACHE = Path(".parity")

#: Set on the re-executed process so it does not extract and re-exec forever.
_REEXEC_MARKER = "VIBE_PARITY_PINNED_TREE"

#: The terminal width every help render is measured at. argparse reads it from
#: the environment, so a capture that did not pin it would record the width of
#: whichever terminal ran it.
COLUMNS = 80

#: The reference files this corpus is an oracle for. The root parser declares
#: two of its flags through helpers in ``vibe/_experimental_harness.py``.
SOURCE_FILES = (
    "vibe/cli/entrypoint.py",
    "vibe/cli/mcp_command.py",
    "vibe/_experimental_harness.py",
)

#: Where the pinned tree keeps the Unified Harness runtime package. The root
#: parser hides ``--experimental-harness`` and ``--smart-approve`` unless
#: ``importlib.util.find_spec`` locates ``mistralai_vibe_local_harness``
#: (``vibe/_experimental_harness.py:22-26``). The published wheel ships the
#: package (``pyproject.toml`` ``[tool.maturin] python-packages``), and the only
#: other copy an interpreter could find is a gitignored build staged at the
#: checkout root (``.gitignore:106``), so the capture puts the pinned tree's own
#: source directory on the path and the answer comes from the pin. ``find_spec``
#: never executes the package, so its compiled ``_native`` module is not needed.
HARNESS_RUNTIME_SOURCE = Path("harness/runtimes/python/python")

#: The marker a digested field carries, so an audit can tell a redaction from a
#: value that merely looks like one.
DESCRIBED = "<described>"

#: The floors the capture refuses to fall below. They are the same numbers the
#: Rust replay asserts, stated here so a capture that lost coverage fails at the
#: point that produced the loss.
CASE_FLOOR = 185
ACTION_FLOOR = 34
PARSER_FLOOR = 3

#: The error messages CPython's ``argparse`` renders itself. A match is committed
#: in cleartext because the sentence belongs to the standard library and this
#: port has to reproduce it exactly; everything else is a sentence the reference
#: wrote and is digested.
CPYTHON_ERROR_TEMPLATES = (
    re.compile(r"^unrecognized arguments: "),
    re.compile(r"^argument .+: invalid \w+ value: "),
    re.compile(r"^argument .+: invalid choice: "),
    re.compile(r"^argument .+: not allowed with argument "),
    re.compile(r"^argument .+: expected (one|at least one|at most one) argument"),
    re.compile(r"^argument .+: ignored explicit argument "),
    re.compile(r"^ambiguous option: "),
    re.compile(r"^the following arguments are required: "),
    re.compile(r"^invalid choice: "),
    re.compile(r"^expected one argument"),
)

#: Every ``prog`` a family can report an error under. A sub-parser renders its
#: own, so the classifier needs all of them to tell a template from prose.
ROOT_PROGS = ("vibe",)
MCP_PROGS = ("vibe mcp", "vibe mcp add", "vibe mcp remove")

#: A bare name in the epilog: a command name or an environment variable name.
#: The bright line is that it carries no whitespace and no punctuation, so no
#: sentence can pass for one.
EPILOG_NAME = re.compile(r"^([A-Za-z_][A-Za-z0-9_*-]*)(\s\s+|$)")

#: A value a vector supplied, so a record reading one back is committed as it
#: stands rather than digested.
AUTHORED_VALUES = frozenset({
    "headless request",
    "review this repository",
    "reviewer",
    "review",
    "session-123",
    "bash",
    "read_file",
    "web_search",
    "sub/dir",
    "extra",
    "more",
})


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


def resolve_reference(reference: Path, expected: str) -> dict[str, Any]:
    """The pinned commit, refusing a checkout sitting anywhere else.

    The corpus is only an oracle for the commit it was captured from, so a
    checkout at another revision is refused by name rather than measured, and
    the restore command is printed with the mismatch.
    """

    if not reference.is_dir():
        raise OracleError(
            f"no reference checkout at {reference}; set VIBE_REFERENCE to the checkout "
            "path or pass --reference"
        )
    commit = _git(reference, "rev-parse", "HEAD")
    if commit != expected:
        raise OracleError(
            f"reference commit mismatch: expected {expected}, found {commit}; "
            f"restore it with: {RESTORE_COMMAND}"
        )
    return {
        "commit": commit,
        "version": EXPECTED_VERSION,
        "sourceFiles": list(SOURCE_FILES),
    }


def extract_pinned_tree(reference: Path, commit: str, cache: Path) -> Path:
    """The pinned source tree, materialized out of tree and reused across runs.

    ``git archive`` writes the commit's contents without moving HEAD, creating a
    branch or adding a worktree, so the reference checkout is observed and never
    modified.
    """

    import tarfile

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
            str(tree / HARNESS_RUNTIME_SOURCE),
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
# Redaction
# --------------------------------------------------------------------------


def described(text: str) -> dict[str, Any]:
    """A sentence the reference wrote, committed as a digest and a length.

    The marker names the redaction so an audit can tell it from a value, the
    length keeps a rewrite that only shortens the prose visible, and the digest
    fails the replay on any change to a character of it.
    """

    return {
        "marker": DESCRIBED,
        "chars": len(text),
        "sha256": hashlib.sha256(text.encode("utf-8")).hexdigest(),
    }


def _json_value(value: Any) -> Any:
    """A declaration value in a form JSON carries and Rust can compare."""

    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    if isinstance(value, Path):
        return str(value)
    if isinstance(value, (list, tuple)):
        return [_json_value(item) for item in value]
    if value is argparse.SUPPRESS:
        return "<suppress>"
    return repr(value)


def _type_name(value: Any) -> str | None:
    if value is None:
        return None
    return getattr(value, "__name__", None) or repr(value)


# --------------------------------------------------------------------------
# Parser declarations
# --------------------------------------------------------------------------


def _root_parser() -> argparse.ArgumentParser:
    """The parser ``parse_arguments`` builds, captured before it parses.

    The parser is not returned by the reference, so it is taken from the call it
    makes: ``parse_args`` is replaced for the length of the build, records the
    instance it was called on and stops the function there. Building it is the
    measurement; restating its declarations here would not be.
    """

    from vibe.cli import entrypoint

    captured: dict[str, argparse.ArgumentParser] = {}

    class _Stop(Exception):
        pass

    def _capture(self: argparse.ArgumentParser, *_args: Any, **_kwargs: Any) -> Any:
        captured["parser"] = self
        raise _Stop

    original = argparse.ArgumentParser.parse_args
    saved_argv = sys.argv
    argparse.ArgumentParser.parse_args = _capture  # type: ignore[method-assign]
    # argparse derives `prog` from argv[0]; the reference is installed as a
    # console script, so the name it renders is `vibe` and not this script's.
    sys.argv = ["vibe"]
    try:
        entrypoint.parse_arguments()
    except _Stop:
        pass
    finally:
        argparse.ArgumentParser.parse_args = original  # type: ignore[method-assign]
        sys.argv = saved_argv
    parser = captured.get("parser")
    if parser is None:
        raise OracleError("`parse_arguments` did not build a parser this capture saw")
    return parser


def _sub_parsers(parser: argparse.ArgumentParser) -> dict[str, argparse.ArgumentParser]:
    for action in parser._actions:
        if isinstance(action, argparse._SubParsersAction):
            return dict(action.choices)
    return {}


def build_parsers() -> tuple[dict[str, argparse.ArgumentParser], dict[str, Any]]:
    """The four parsers under measurement, with the purity of the build proved.

    A parser build that read a configuration or wrote a file would make every
    record below a measurement of this machine as much as of the reference, so
    the three ways it could happen are watched and named.
    """

    vibe_home = Path(os.environ["VIBE_HOME"])
    # The two entry modules are imported before the guarded region, so what the
    # region measures is the build and not the import. The difference is real:
    # `vibe.cli.entrypoint` keeps everything heavier than argparse out of module
    # scope, while `vibe.cli.mcp_command` imports the configuration stack and
    # creates the session home on the way, which is recorded rather than
    # asserted away.
    from vibe.cli import entrypoint  # noqa: F401  imported before the snapshot

    home_after_entrypoint_import = vibe_home.exists()
    from vibe.cli import mcp_command

    home_after_mcp_import = vibe_home.exists()

    opened: list[str] = []
    real_open = builtins.open

    def _recording_open(file: Any, *arguments: Any, **keywords: Any) -> Any:
        opened.append(str(file))
        return real_open(file, *arguments, **keywords)

    modules_before = set(sys.modules)
    builtins.open = _recording_open  # type: ignore[assignment]
    try:
        root = _root_parser()
        mcp = mcp_command._build_parser()
    finally:
        builtins.open = real_open  # type: ignore[assignment]
    configuration_modules = sorted(
        name
        for name in set(sys.modules) - modules_before
        if name.startswith("vibe.core.config")
    )
    if opened:
        raise OracleError(f"building the parsers opened {len(opened)} file(s): {opened[:3]}")
    if vibe_home.exists() != home_after_mcp_import:
        raise OracleError(
            "building the parsers created the session home, so it read $VIBE_HOME"
        )
    if configuration_modules:
        raise OracleError(
            "building the parsers loaded a configuration: " + ", ".join(configuration_modules)
        )

    children = _sub_parsers(mcp)
    missing = {"add", "remove"} - set(children)
    if missing:
        raise OracleError(f"`vibe mcp` declares no sub-parser for {sorted(missing)}")
    parsers = {
        "root": root,
        "mcp": mcp,
        "mcp-add": children["add"],
        "mcp-remove": children["remove"],
    }
    purity = {
        "filesOpenedByBuild": 0,
        "sessionHomeCreatedByBuild": False,
        "configurationModulesImportedByBuild": [],
        "sessionHomeCreatedByEntrypointImport": home_after_entrypoint_import,
        "sessionHomeCreatedByMcpImport": home_after_mcp_import,
    }
    return parsers, purity


def action_record(
    formatter: argparse.HelpFormatter, action: argparse.Action
) -> dict[str, Any]:
    choices = action.choices
    return {
        "class": type(action).__name__,
        "optionStrings": list(action.option_strings),
        "dest": action.dest,
        "nargs": _json_value(action.nargs),
        "const": _json_value(action.const),
        "default": _json_value(action.default),
        "choices": None if choices is None else [str(choice) for choice in choices],
        "metavar": _json_value(action.metavar),
        "required": bool(action.required),
        "type": _type_name(action.type),
        "invocation": formatter._format_action_invocation(action),
        "helpSuppressed": action.help is argparse.SUPPRESS,
        "help": None
        if action.help in (None, argparse.SUPPRESS)
        else described(action.help),
    }


def parser_record(name: str, parser: argparse.ArgumentParser) -> dict[str, Any]:
    formatter = parser._get_formatter()
    return {
        "parser": name,
        "prog": parser.prog,
        "formatter": parser.formatter_class.__name__,
        "allowAbbrev": bool(parser.allow_abbrev),
        "hasDescription": parser.description is not None,
        "hasEpilog": parser.epilog is not None,
        "actions": [action_record(formatter, action) for action in parser._actions],
        "actionGroups": [
            {"title": group.title, "dests": [a.dest for a in group._group_actions]}
            for group in parser._action_groups
            if group._group_actions
        ],
        "mutuallyExclusiveGroups": [
            {
                "required": bool(group.required),
                "dests": [a.dest for a in group._group_actions],
            }
            for group in parser._mutually_exclusive_groups
        ],
        "help": help_record(parser),
    }


# --------------------------------------------------------------------------
# Help renders
# --------------------------------------------------------------------------


def _invocations(parser: argparse.ArgumentParser) -> list[str]:
    """Every invocation the render can print, longest first.

    Longest first because ``-h`` is a prefix of ``-h, --help``: matching the
    short one would leave the rest of the line looking like prose.
    """

    formatter = parser._get_formatter()
    found: set[str] = set()
    for action in parser._actions:
        if action.help is argparse.SUPPRESS:
            continue
        found.add(formatter._format_action_invocation(action))
        if isinstance(action, argparse._SubParsersAction):
            for sub in action._get_subactions():
                found.add(formatter._format_action_invocation(sub))
    return sorted(found, key=len, reverse=True)


def _epilog_line(line: str) -> dict[str, Any]:
    stripped = line.strip()
    if not stripped:
        return {"kind": "blank", "cleartext": None, "described": None}
    if not line.startswith(" ") and stripped.endswith(":"):
        return {"kind": "epilogHeading", "cleartext": line, "described": None}
    match = EPILOG_NAME.match(stripped)
    if match:
        rest = stripped[match.end(1) :].strip()
        return {
            "kind": "epilogEntry",
            "cleartext": match.group(1),
            "described": described(rest) if rest else None,
        }
    return {"kind": "epilogContinuation", "cleartext": None, "described": described(stripped)}


def help_record(parser: argparse.ArgumentParser) -> dict[str, Any]:
    """The rendered help, line by line, with every authored sentence digested.

    The decomposition is anchored on what argparse itself produces rather than
    on a regular expression over the render: the usage block comes from
    ``format_usage``, the headings from the action groups, the invocations from
    the formatter, and the epilog from the parser's own attribute. A line that
    matches none of them is a wrapped sentence, so it is digested whole.
    """

    render = parser.format_help()
    lines = render.splitlines()
    usage_lines = parser.format_usage().splitlines()
    if lines[: len(usage_lines)] != usage_lines:
        raise OracleError(f"the {parser.prog!r} render does not open with its usage block")

    epilog_start = len(lines)
    if parser.epilog is not None:
        epilog_lines = parser.epilog.splitlines()
        epilog_start = len(lines) - len(epilog_lines)
        if lines[epilog_start:] != epilog_lines:
            raise OracleError(
                f"the {parser.prog!r} render does not close with its epilog verbatim"
            )

    headings = {f"{group.title}:" for group in parser._action_groups if group._group_actions}
    first_heading = next(
        (index for index, line in enumerate(lines) if line in headings), len(lines)
    )
    invocations = _invocations(parser)

    records: list[dict[str, Any]] = []
    for index, line in enumerate(lines):
        record: dict[str, Any]
        if index < len(usage_lines):
            record = {"kind": "usage", "cleartext": line, "described": None}
        elif index >= epilog_start:
            record = _epilog_line(line)
        elif not line.strip():
            record = {"kind": "blank", "cleartext": None, "described": None}
        elif line in headings:
            record = {"kind": "heading", "cleartext": line, "described": None}
        elif index < first_heading:
            record = {
                "kind": "description",
                "cleartext": None,
                "described": described(line.strip()),
            }
        else:
            stripped = line.lstrip(" ")
            invocation = next(
                (
                    candidate
                    for candidate in invocations
                    if stripped == candidate or stripped.startswith(f"{candidate}  ")
                ),
                None,
            )
            if invocation is None:
                record = {
                    "kind": "continuation",
                    "cleartext": None,
                    "described": described(stripped),
                }
            else:
                rest = stripped[len(invocation) :].strip()
                record = {
                    "kind": "invocation",
                    "cleartext": invocation,
                    "described": described(rest) if rest else None,
                }
        record["indent"] = len(line) - len(line.lstrip(" "))
        record["chars"] = len(line)
        records.append(record)

    return {
        "columns": COLUMNS,
        "lineCount": len(lines),
        "sha256": hashlib.sha256(render.encode("utf-8")).hexdigest(),
        "lines": records,
    }


# --------------------------------------------------------------------------
# The argv matrix
# --------------------------------------------------------------------------
#
# Four families are generated from the parser itself, so an optional the
# reference adds is covered without this file being edited: every optional alone
# with a valid value, every value-taking optional with no value, every
# value-taking optional in its ``--flag=value`` form, and every optional at its
# shortest unambiguous prefix. A store-true optional has no distinct "with no
# value" form, which is why the second and third families cover twelve of the
# twenty-two rather than all of them. The remaining vectors are written out below.

#: A valid value per long option, keyed by its declared spelling. A new optional
#: in the reference lands here as a missing key rather than as silent coverage
#: loss.
VALID_VALUES: dict[str, list[str]] = {
    "--version": [],
    "--prompt": ["headless request"],
    "--max-turns": ["3"],
    "--max-price": ["1.5"],
    "--max-tokens": ["2048"],
    "--enabled-tools": ["bash"],
    "--disabled-tools": ["web_search"],
    "--output": ["json"],
    "--agent": ["reviewer"],
    "--experimental-harness": [],
    "--legacy-harness": [],
    "--smart-approve": [],
    "--auto-approve": [],
    "--setup": [],
    "--check-upgrade": [],
    "--workdir": ["sub/dir"],
    "--worktree": ["review"],
    "--add-dir": ["extra"],
    "--trust": [],
    "--teleport": [],
    "--continue": [],
    "--resume": ["session-123"],
}


def _long_options(parser: argparse.ArgumentParser) -> list[tuple[str, argparse.Action]]:
    found: list[tuple[str, argparse.Action]] = []
    for action in parser._actions:
        if isinstance(action, argparse._HelpAction):
            continue
        long_option = next((o for o in action.option_strings if o.startswith("--")), None)
        if long_option is not None:
            found.append((long_option, action))
    return found


def _shortest_unambiguous_prefix(option: str, every: list[str]) -> str:
    for length in range(3, len(option) + 1):
        prefix = option[:length]
        if sum(1 for other in every if other.startswith(prefix)) == 1:
            return prefix
    return option


def root_vectors(parser: argparse.ArgumentParser) -> list[tuple[str, list[str]]]:
    options = _long_options(parser)
    spellings = [option for option, _ in options] + ["--help"]
    missing = sorted(option for option, _ in options if option not in VALID_VALUES)
    if missing:
        raise OracleError(f"no valid value is declared for {missing}")

    vectors: list[tuple[str, list[str]]] = []
    for option, _action in options:
        vectors.append((f"alone{option}", [option, *VALID_VALUES[option]]))
    for option, action in options:
        if action.nargs != 0:
            vectors.append((f"no-value{option}", [option]))
    for option, action in options:
        if action.nargs != 0:
            vectors.append((f"equals{option}", [f"{option}={VALID_VALUES[option][0]}"]))
    for option, _action in options:
        prefix = _shortest_unambiguous_prefix(option, spellings)
        vectors.append((f"prefix{option}", [prefix, *VALID_VALUES[option]]))

    vectors += [
        ("output-text", ["--output", "text"]),
        ("output-streaming", ["--output", "streaming"]),
        ("output-invalid", ["--output", "bogus"]),
        ("output-repeated", ["--output", "json", "--output", "text"]),
        ("continue-with-resume", ["-c", "--resume"]),
        ("continue-with-resume-value", ["-c", "--resume", "session-123"]),
        ("repeated-enabled-tools", ["--enabled-tools", "bash", "--enabled-tools", "read_file"]),
        (
            "repeated-disabled-tools",
            ["--disabled-tools", "bash", "--disabled-tools", "web_search"],
        ),
        ("repeated-add-dir", ["--add-dir", "extra", "--add-dir", "more"]),
        ("unknown-long-flag", ["--bogus"]),
        ("unknown-short-flag", ["-x"]),
        ("ambiguous-prefix-a", ["--a"]),
        ("ambiguous-prefix-m", ["--m", "3"]),
        ("ambiguous-prefix-t", ["--t"]),
        ("negative-max-turns", ["--max-turns", "-5"]),
        ("negative-max-tokens", ["--max-tokens", "-1"]),
        ("negative-max-price", ["--max-price", "-2.5"]),
        ("non-numeric-max-turns", ["--max-turns", "x"]),
        ("non-numeric-max-tokens", ["--max-tokens", "1e3"]),
        ("non-numeric-max-price", ["--max-price", "cheap"]),
        ("empty-max-turns", ["--max-turns", ""]),
        ("zero-max-turns", ["--max-turns", "0"]),
        ("integral-max-price", ["--max-price", "2"]),
        ("prompt-empty-string", ["-p", ""]),
        ("prompt-whitespace-only", ["-p", "   "]),
        ("prompt-short-with-value", ["-p", "headless request"]),
        ("continue-short", ["-c"]),
        ("help-short", ["-h"]),
        ("help-long", ["--help"]),
        ("version-short", ["-v"]),
        ("bare", []),
        ("update-command", ["update"]),
        ("update-command-with-prompt", ["update", "review this repository"]),
        ("update-command-with-setup", ["update", "--setup"]),
        ("update-after-separator", ["--", "update"]),
        ("update-after-flag", ["-c", "update"]),
        ("update-command-capitalized", ["Update"]),
        ("positional-prompt", ["review this repository"]),
        ("two-positionals", ["first", "second"]),
        ("separator-alone", ["--"]),
        ("separator-then-flag-shaped-prompt", ["--", "--not-a-flag"]),
        ("flag-shaped-option-value", ["--agent", "--trust"]),
        ("repeated-agent", ["--agent", "reviewer", "--agent", "auditor"]),
        ("yolo-alias", ["--yolo"]),
        ("resume-empty-value", ["--resume", ""]),
        ("enabled-tools-glob", ["--enabled-tools", "web_*"]),
        ("workdir-dot", ["--workdir", "."]),
        ("add-dir-nested", ["--add-dir", "sub/dir"]),
        (
            "combined-prompt-with-agent-and-trust",
            ["review this repository", "--agent", "reviewer", "--trust"],
        ),
        (
            "combined-programmatic-json",
            ["-p", "headless request", "--output", "json", "--max-turns", "2"],
        ),
        (
            "combined-tool-filters",
            [
                "--enabled-tools",
                "bash",
                "--enabled-tools",
                "read_file",
                "--disabled-tools",
                "web_search",
            ],
        ),
        (
            "combined-directories",
            ["--workdir", "sub/dir", "--add-dir", "extra", "--add-dir", "more"],
        ),
        ("combined-worktree-auto-approve", ["--worktree", "review", "--trust", "--auto-approve"]),
        ("combined-continue-streaming", ["-c", "--output", "streaming"]),
        ("combined-resume-with-budget", ["--resume", "session-123", "--max-price", "0.5"]),
        ("combined-setup-and-check-upgrade", ["--setup", "--check-upgrade"]),
        # The harness flags: one exclusive pair, and a smart-approve rewrite
        # that runs after the parse and so never meets that exclusion.
        ("harness-flags-together", ["--experimental-harness", "--legacy-harness"]),
        (
            "harness-flags-together-reversed",
            ["--legacy-harness", "--experimental-harness"],
        ),
        ("smart-approve-with-agent", ["--smart-approve", "--agent", "plan"]),
        ("smart-approve-with-legacy-harness", ["--smart-approve", "--legacy-harness"]),
        (
            "smart-approve-with-experimental-harness",
            ["--smart-approve", "--experimental-harness"],
        ),
        ("smart-approve-programmatic", ["-p", "headless request", "--smart-approve"]),
        # A repeated option keeps its last occurrence, a flag included.
        ("repeated-trust", ["--trust", "--trust"]),
        ("repeated-prompt", ["-p", "first", "-p", "second"]),
        ("repeated-worktree", ["--worktree", "one", "--worktree"]),
        # Tokens argparse reads as values although they open with a hyphen.
        ("negative-shaped-agent", ["--agent", "-5"]),
        ("negative-shaped-prompt", ["-p", "-1"]),
        ("negative-shaped-positional", ["-5"]),
        ("fraction-shaped-positional", ["-.5"]),
        ("spaced-dash-positional", ["-x y"]),
        ("positional-before-flag", ["review", "--trust"]),
        # What `int()` and `float()` accept beyond plain digits.
        ("wide-max-turns", ["--max-turns", "4294967296"]),
        ("grouped-max-turns", ["--max-turns", "1_000"]),
        ("padded-max-turns", ["--max-turns", " 3 "]),
        ("signed-max-turns", ["--max-turns", "+3"]),
        ("bad-grouping-max-turns", ["--max-turns", "1__0"]),
        ("exponent-max-price", ["--max-price", "1e2"]),
        ("grouped-max-price", ["--max-price", "1_000.5"]),
        # Abbreviations beyond the shortest one per option.
        ("prefix-with-equals", ["--max-tu=3"]),
        ("ambiguous-prefix-with-equals", ["--max-t=3"]),
        ("ambiguous-prefix-c", ["--c"]),
        ("prefix-of-alias", ["--yo"]),
        ("flag-with-explicit-value", ["--trust=yes"]),
        ("refusal-before-ambiguous-prefix", ["--output", "bogus", "--a"]),
        ("help-before-ambiguous-prefix", ["--help", "--a"]),
    ]
    return vectors


#: The ``vibe mcp`` vectors, in the order they run. They share one session home,
#: so the two that persist a server run before the ones that read it back.
MCP_VECTORS: list[tuple[str, list[str]]] = [
    ("mcp-bare", []),
    ("mcp-help-short", ["-h"]),
    ("mcp-help-long", ["--help"]),
    ("mcp-add-help", ["add", "-h"]),
    ("mcp-remove-help", ["remove", "-h"]),
    ("mcp-unknown-subcommand", ["list"]),
    ("mcp-remove-without-name", ["remove"]),
    ("mcp-remove-two-names", ["remove", "first", "second"]),
    ("mcp-add-without-name", ["add"]),
    ("mcp-add-invalid-transport", ["add", "docs", "--transport", "bogus"]),
    ("mcp-add-url-without-value", ["add", "docs", "--url"]),
    ("mcp-add-stdio-with-remote-flag", ["add", "docs", "--transport", "stdio", "--url", "u"]),
    ("mcp-add-remote-without-url", ["add", "docs"]),
    (
        "mcp-add-http-oauth-no-login",
        [
            "add",
            "docs",
            "--transport",
            "http",
            "--url",
            "https://example.invalid/docs",
            "--no-login",
        ],
    ),
    (
        "mcp-add-streamable-http-static-auth",
        [
            "add",
            "notes",
            "--transport",
            "streamable-http",
            "--url",
            "https://example.invalid/notes",
            "--api-key-env",
            "PARITY_TOKEN_VARIABLE",
        ],
    ),
    (
        "mcp-add-stdio-command",
        ["add", "local", "--transport", "stdio", "--command", "/bin/true", "--arg", "serve"],
    ),
    (
        "mcp-add-stdio-again",
        ["add", "local", "--transport", "stdio", "--command", "/bin/true", "--arg", "serve"],
    ),
    (
        "mcp-add-duplicate-url",
        [
            "add",
            "extra",
            "--transport",
            "streamable-http",
            "--url",
            "https://example.invalid/docs",
            "--no-login",
        ],
    ),
    (
        "mcp-add-remote-static-carrier",
        [
            "add",
            "reports",
            "--transport",
            "streamable-http",
            "--url",
            "https://example.invalid/reports",
            "--bearer-token-env-var",
            "PARITY_REPORTS_TOKEN",
            "--api-key-header",
            "X-Parity-Key",
            "--api-key-format",
            "Token {token}",
            "--header",
            "X-Parity-Trace=on",
            "--startup-timeout-sec",
            "12.5",
        ],
    ),
    (
        "mcp-add-stdio-environment",
        [
            "add",
            "tools",
            "--transport",
            "stdio",
            "--command",
            "/bin/true",
            "--env",
            "PARITY_MODE=on",
            "--tool-timeout-sec",
            "7.5",
        ],
    ),
    (
        "mcp-add-insecure-http-refused",
        [
            "add",
            "lan",
            "--transport",
            "streamable-http",
            "--url",
            "http://lan.example.invalid/mcp",
            "--no-login",
        ],
    ),
    (
        "mcp-add-insecure-http-allowed",
        [
            "add",
            "lan",
            "--transport",
            "streamable-http",
            "--url",
            "http://lan.example.invalid/mcp",
            "--no-login",
            "--allow-insecure-http",
        ],
    ),
    (
        "mcp-add-stdio-with-insecure-flag",
        [
            "add",
            "plain",
            "--transport",
            "stdio",
            "--command",
            "/bin/true",
            "--allow-insecure-http",
        ],
    ),
    (
        "mcp-add-abbreviated-options",
        ["add", "short", "--tr", "stdio", "--com", "/bin/true", "--ar", "-5"],
    ),
    ("mcp-add-ambiguous-prefix", ["add", "vague", "--api", "PARITY_TOKEN"]),
    ("mcp-help-prefix", ["--he"]),
    ("mcp-remove-absent", ["remove", "absent-server"]),
    ("mcp-remove-stdio-server", ["remove", "local"]),
]

#: Vectors whose outcome depends on something this capture must not touch.
UNAVAILABLE: list[dict[str, str]] = [
    {
        "id": "mcp-remove-oauth-server",
        "argv": "mcp remove docs",
        "reason": (
            "removing a persisted OAuth server deletes its keyring credentials before "
            "the config entry (vibe/app_server/mcp_catalog.py:866-881 and 1092-1103, "
            "vibe/app_server/_mcp_auth.py:263-280), and this capture drives no keyring"
        ),
    },
    {
        "id": "mcp-add-http-oauth-with-login",
        "argv": "mcp add docs --transport http --url https://example.invalid/mcp",
        "reason": (
            "an OAuth add without --no-login starts a browser login against a live "
            "authorization server (vibe/cli/mcp_command.py:221-259)"
        ),
    },
    {
        "id": "check-upgrade-against-the-index",
        "argv": "vibe --check-upgrade",
        "reason": (
            "the flag parses here, but the upgrade check it selects queries the package "
            "index, so only its parse is recorded"
        ),
    },
]


#: The startup failures the reference reports after the parse and before the app
#: server is reached (`vibe/cli/entrypoint.py:420-457`). Every vector is driven
#: from a fixture directory with a relative argument, so the argv a case commits
#: names no machine path, and every one of them is expected to exit 1.
STARTUP_VECTORS: tuple[tuple[str, list[str]], ...] = (
    ("startup-workdir-missing", ["--workdir", "missing", "-p", "hi"]),
    ("startup-add-dir-missing", ["--add-dir", "missing", "-p", "hi"]),
    (
        "startup-add-dir-second-missing",
        ["--add-dir", "present", "--add-dir", "missing", "-p", "hi"],
    ),
    ("startup-check-upgrade-workdir-missing", ["--check-upgrade", "--workdir", "missing"]),
    ("startup-update-add-dir-missing", ["update", "--add-dir", "missing"]),
    ("startup-working-directory-deleted", ["-p", "hi"]),
)


def _describe_stream_line(
    line: str | None, progs: tuple[str, ...]
) -> dict[str, Any] | None:
    """The last line of a stream, cleartext where the standard library wrote it.

    A sub-parser reports under its own ``prog``, so every name the family can
    render is tried, longest first: ``vibe mcp`` is a prefix of ``vibe mcp add``
    and matching it first would leave the sub-command in the message.
    """

    if line is None:
        return None
    for prog in sorted(progs, key=len, reverse=True):
        prefix = f"{prog}: error: "
        if not line.startswith(prefix):
            continue
        message = line[len(prefix) :]
        if any(pattern.search(message) for pattern in CPYTHON_ERROR_TEMPLATES):
            return {"cleartext": line, "described": None}
        return {"cleartext": prefix, "described": described(message)}
    if line in AUTHORED_VALUES:
        return {"cleartext": line, "described": None}
    return {"cleartext": None, "described": described(line)}


def _last_line(text: str) -> str | None:
    lines = [line for line in text.splitlines() if line.strip()]
    return lines[-1] if lines else None


def _namespace(namespace: argparse.Namespace | None) -> dict[str, Any] | None:
    if namespace is None:
        return None
    return {key: _json_value(value) for key, value in sorted(vars(namespace).items())}


def _drive(call: Any, progs: tuple[str, ...]) -> dict[str, Any]:
    stdout, stderr = io.StringIO(), io.StringIO()
    exit_code = 0
    result: argparse.Namespace | None = None
    try:
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            result = call()
    except SystemExit as exit_status:
        code = exit_status.code
        exit_code = 0 if code is None else int(code)
    out, err = stdout.getvalue(), stderr.getvalue()
    return {
        "exit": exit_code,
        "streams": {"stdout": bool(out), "stderr": bool(err)},
        "stdoutLastLine": _describe_stream_line(_last_line(out), progs),
        "stderrLastLine": _describe_stream_line(_last_line(err), progs),
        "namespace": _namespace(result if isinstance(result, argparse.Namespace) else None),
    }


def drive_root(case: str, argv: list[str]) -> dict[str, Any]:
    from vibe.cli import entrypoint

    saved = sys.argv
    sys.argv = ["vibe", *argv]
    try:
        record = _drive(entrypoint.parse_arguments, ROOT_PROGS)
    finally:
        sys.argv = saved
    return {"case": case, "parser": "root", "argv": list(argv), **record}


def drive_mcp(case: str, argv: list[str]) -> dict[str, Any]:
    """One ``vibe mcp`` vector, driven through the command the reference runs.

    The exit code and the streams come from ``run_mcp_cli``, which is the entry
    the reference's own ``main`` calls. The namespace comes from a second, pure
    parse, because ``run_mcp_cli`` consumes it rather than returning it.
    """

    from vibe.cli import mcp_command

    record = _drive(lambda: mcp_command.run_mcp_cli(list(argv)), MCP_PROGS)
    parsed = _drive(lambda: mcp_command._build_parser().parse_args(list(argv)), MCP_PROGS)
    record["namespace"] = parsed["namespace"]
    return {"case": case, "parser": "mcp", "argv": list(argv), **record}


def drive_startup(case: str, argv: list[str]) -> dict[str, Any]:
    """One startup failure, driven through the reference's ``main``.

    The ``vibe`` console script lands on ``vibe.cli.launcher:main``, which hands
    every argv to ``vibe.cli.entrypoint.main`` unless ``VIBE_CLI=rust`` selects
    the reference's own Rust client (``vibe/cli/launcher.py:16-31``), so the
    entry point is driven directly.

    The record keeps the exit code and which stream carried the report, and
    drops the two last-line fields on purpose: the ``--workdir`` report prints
    the path once it was resolved, which is a temporary directory here, so a
    digest of that line would differ between two captures of the same behavior.
    The sentences are the reference's own and could not be committed either way.
    """

    from vibe.cli import entrypoint

    saved = sys.argv
    sys.argv = ["vibe", *argv]
    try:
        record = _drive(entrypoint.main, ROOT_PROGS)
    finally:
        sys.argv = saved
    record["stdoutLastLine"] = None
    record["stderrLastLine"] = None
    if record["exit"] != 1:
        raise OracleError(
            f"the {case} vector exited {record['exit']} rather than failing the startup"
        )
    return {"case": case, "parser": "startup", "argv": list(argv), **record}


def drive_startup_matrix(root: Path) -> list[dict[str, Any]]:
    """The startup matrix, each vector driven from the directory it needs.

    Three of them fail on a name that is not there, so they run from a fixture
    holding one directory that is. The fourth fails because the directory the
    process sits in was removed underneath it, which no argument can express, so
    it is driven from a directory this function deletes first. The working
    directory is restored either way: everything captured after this reads it.
    """

    fixture = root / "startup"
    (fixture / "present").mkdir(parents=True)
    deleted = root / "startup-deleted"
    deleted.mkdir()
    origin = Path.cwd()
    cases: list[dict[str, Any]] = []
    try:
        os.chdir(fixture)
        cases += [
            drive_startup(case, argv)
            for case, argv in STARTUP_VECTORS
            if case != "startup-working-directory-deleted"
        ]
        os.chdir(deleted)
        shutil.rmtree(deleted)
        cases.append(drive_startup(*STARTUP_VECTORS[-1]))
    finally:
        os.chdir(origin)
    return cases



# --------------------------------------------------------------------------
# Capture
# --------------------------------------------------------------------------


def _fingerprint(path: Path) -> tuple[bool, int, int]:
    if not path.is_file():
        return (False, 0, 0)
    status = path.stat()
    return (True, status.st_size, status.st_mtime_ns)


def build_corpus(reference: dict[str, Any], session_home: Path) -> dict[str, Any]:
    parsers, purity = build_parsers()
    parser_records = [parser_record(name, parser) for name, parser in parsers.items()]

    cases = [drive_root(case, argv) for case, argv in root_vectors(parsers["root"])]

    # Every `vibe mcp` vector writes through the configuration, so the guard on
    # the real user file is re-read here rather than only at the end: a capture
    # that resolved the wrong home must fail before it touches it.
    real_config = Path("~/.vibe/config.toml").expanduser()
    before = _fingerprint(real_config)
    if Path(os.environ["VIBE_HOME"]).resolve() != session_home.resolve():
        raise OracleError("$VIBE_HOME does not point at this run's temporary session home")
    cases += [drive_mcp(case, argv) for case, argv in MCP_VECTORS]
    if _fingerprint(real_config) != before:
        raise OracleError(f"the matrix changed the real user configuration at {real_config}")
    cases += drive_startup_matrix(session_home.parent)

    actions = sum(len(record["actions"]) for record in parser_records)
    if len(parser_records) < PARSER_FLOOR:
        raise OracleError(f"{len(parser_records)} parsers is below the floor of {PARSER_FLOOR}")
    if actions < ACTION_FLOOR:
        raise OracleError(f"{actions} actions is below the floor of {ACTION_FLOOR}")
    if len(cases) < CASE_FLOOR:
        raise OracleError(f"{len(cases)} argv cases is below the floor of {CASE_FLOOR}")

    corpus = {
        "schemaVersion": SCHEMA_VERSION,
        "reference": reference,
        "interpreter": {"major": sys.version_info.major, "minor": sys.version_info.minor},
        "capture": {"columns": COLUMNS, **purity, "realUserConfigurationUntouched": True},
        "parsers": parser_records,
        "cases": cases,
        "unavailable": UNAVAILABLE,
    }
    audit_cleartext(corpus, session_home)
    return corpus


def audit_cleartext(corpus: dict[str, Any], session_home: Path) -> None:
    """Refuses a corpus carrying a machine path or an undeclared marker.

    A temporary path reaching the corpus would make two runs differ, and a
    redaction missing its marker would let an audit read a digest as a value, so
    both fail the capture rather than being committed.
    """

    volatile = (str(session_home), str(Path.home()), tempfile.gettempdir())

    def walk(node: Any, pointer: str) -> None:
        if isinstance(node, dict):
            if node.get("marker") == DESCRIBED:
                if not isinstance(node.get("sha256"), str) or not isinstance(
                    node.get("chars"), int
                ):
                    raise OracleError(f"the redaction at {pointer} carries no digest")
                return
            if "sha256" in node and "marker" not in node and "lineCount" not in node:
                raise OracleError(f"the digest at {pointer} carries no {DESCRIBED} marker")
            for key, value in node.items():
                walk(value, f"{pointer}/{key}")
        elif isinstance(node, list):
            for index, value in enumerate(node):
                walk(value, f"{pointer}/{index}")
        elif isinstance(node, str):
            for fragment in volatile:
                if fragment and fragment in node:
                    raise OracleError(f"a machine path reached the corpus at {pointer}")

    walk(corpus, "")


def rendered(corpus: dict[str, Any]) -> str:
    return json.dumps(corpus, indent=2, sort_keys=True, ensure_ascii=False) + "\n"


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--corpus", type=Path, default=DEFAULT_CORPUS)
    parser.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    parser.add_argument("--python", type=Path, default=None)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    parser.add_argument(
        "--check",
        action="store_true",
        help="compare a fresh capture against the committed corpus and write nothing",
    )
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    # Pinned before anything imports the reference: argparse renders help at the
    # width it finds, and a colored or wider render would be this terminal's
    # rather than the reference's.
    os.environ["COLUMNS"] = str(COLUMNS)
    os.environ["TERM"] = "dumb"
    os.environ["NO_COLOR"] = "1"
    os.environ.pop("FORCE_COLOR", None)
    temporary = None
    try:
        reference = resolve_reference(arguments.reference, arguments.expected_commit)
        tree = extract_pinned_tree(
            arguments.reference, reference["commit"], arguments.cache
        )
        reexecute_with_reference_interpreter(arguments.reference, arguments.python, tree)
        temporary = Path(tempfile.mkdtemp(prefix="vibe-cli-surface-"))
        session_home = temporary / "vibe-home"
        # Set before the first reference import: the session home is resolved
        # from the environment when a command asks for it, and the user's own
        # `~/.vibe` must never be the answer.
        os.environ["VIBE_HOME"] = str(session_home)
        corpus = build_corpus(reference, session_home)
        if arguments.check:
            if not arguments.corpus.is_file():
                raise OracleError(f"no committed corpus at {arguments.corpus} to check against")
            if arguments.corpus.read_text(encoding="utf-8") != rendered(corpus):
                raise OracleError(
                    f"a fresh capture differs from the committed corpus at {arguments.corpus}"
                )
            print(
                f"the committed corpus matches a fresh capture of {len(corpus['parsers'])} "
                f"parsers and {len(corpus['cases'])} argv cases"
            )
            return 0
        arguments.corpus.parent.mkdir(parents=True, exist_ok=True)
        staged = arguments.corpus.with_name(f"{arguments.corpus.name}.{os.getpid()}.tmp")
        staged.write_text(rendered(corpus), encoding="utf-8")
        os.replace(staged, arguments.corpus)
    except OracleError as error:
        print(f"cli-surface capture failed: {error}", file=sys.stderr)
        return 1
    finally:
        if temporary is not None:
            shutil.rmtree(temporary, ignore_errors=True)
    actions = sum(len(record["actions"]) for record in corpus["parsers"])
    print(
        f"captured {len(corpus['parsers'])} parsers, {actions} actions and "
        f"{len(corpus['cases'])} argv cases from {reference['commit'][:12]} "
        f"into {arguments.corpus}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
