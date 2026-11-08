#!/usr/bin/env python3
"""Capture the argument surface the pinned reference's ``vibe`` binary declares.

Row 7 of the scorecard reads the CLI surface. Since v3.0.0 the reference's
``vibe`` is a Rust binary built with clap (``vibe/cli-rust/src/cli.rs``) that
the materialized console script launches (``vibe/cli/launcher.py``), so this
oracle is a black box: it reads the surface from what the binary prints and
how it exits, never from the source.

Two kinds of scenario are recorded:

``help/<command>``  for ``vibe``, every subcommand its help lists, and theirs in
                    turn, the help render decomposed: the usage line, the
                    subcommands, the positional arguments and every option with
                    its spellings, its value, the choices, default, environment
                    variable and aliases clap annotates it with
``case/<name>``     an argument vector driven through the binary, with its exit
                    code, which streams it wrote and the first line of each

The vectors are generated from the help: ``--help`` and ``-h`` per command,
the version flags, every option that takes a value left without one, every
option with choices given a value outside them, a command whose required
argument is missing, and a flag no command declares (which is also how the
binary answers the backend flags v3.0.0 retired, so no vector names them).
Only a vector that cannot start a session is driven, and the run has no
terminal, so a vector that does parse stops on the binary's own refusal to run
without one.

``NOTICE`` forbids shipping reference-authored prose, so every description and
every line the binary words itself is committed as ``{"prose": n, "sha256":
...}``. What stays in cleartext is structure: flag spellings, value names,
command names, environment variable names, the annotations clap renders around
them, and the error templates clap words itself, which name only what the
vector supplied and what the declaration spells.

Usage::

    crates/vibe-cli/tests/runtime-parity/cli-surface-oracle.py --corpus
    crates/vibe-cli/tests/runtime-parity/cli-surface-oracle.py --check

``VIBE_REFERENCE`` names the checkout on a machine that does not hold it at the
default path and ``--reference`` wins over it; the binary is the attested
environment's ``vibe`` console script.
"""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "scripts" / "parity"))

from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT  # noqa: E402
from reference import OracleError, reference_command  # noqa: E402
import host_env  # noqa: E402

SCHEMA_VERSION = 2

#: The corpus the Rust replay reads, beside this script.
DEFAULT_CORPUS = Path(__file__).resolve().with_name("cli-surface.json")

#: How long one vector may run. A vector that parses stops on the missing
#: terminal at once, so a run this long is a vector that started something.
TIMEOUT_SECONDS = 30

#: Flags no command declares, driven against each command.
UNKNOWN_FLAGS = ("--no-such-flag",)

#: A value no option with choices accepts.
NOT_A_CHOICE = "not-a-choice"

#: Vectors that parse and then fail on the directories they name, before a
#: session starts. ``present`` exists in the workspace and ``missing`` does
#: not; the last one runs from a working directory removed under it.
#: ``tests/startup_directory_failures.rs`` replays them against this port's
#: binary, at the process boundary where the stream a report lands on exists.
STARTUP = (
    ("workdir-missing", ["--workdir", "missing", "-p", "hi"]),
    ("add-dir-missing", ["--add-dir", "missing", "-p", "hi"]),
    ("add-dir-second-missing", ["--add-dir", "present", "--add-dir", "missing", "-p", "hi"]),
    ("check-upgrade-workdir-missing", ["--check-upgrade", "--workdir", "missing"]),
    ("update-add-dir-missing", ["update", "--add-dir", "missing"]),
    ("working-directory-deleted", ["-p", "hi"]),
)

#: A section heading of a clap help render.
HEADING = re.compile(r"^([A-Z][A-Za-z ]*):$")

#: Where an entry's spelling ends and its description begins on one line.
GAP = re.compile(r"\s{2,}")

#: The annotations clap appends to a description, in the order it renders them.
ANNOTATION = re.compile(r"\s*\[(env|default|possible values|aliases?|short aliases?): ([^\]]*)\]\s*$")

#: The error lines clap words itself. Each names only what the vector supplied
#: and what the declaration spells, so it is committed in cleartext.
CLAP_TEMPLATES = (
    re.compile(r"^error: unexpected argument '[^']*' found$"),
    re.compile(r"^error: a value is required for '[^']*' but none was supplied$"),
    re.compile(r"^error: invalid value '[^']*' for '[^']*'$"),
    re.compile(r"^error: the following required arguments were not provided:$"),
    re.compile(r"^error: unrecognized subcommand '[^']*'$"),
    re.compile(r"^error: the argument '[^']*' cannot be used with '[^']*'$"),
    re.compile(r"^error: the argument '[^']*' cannot be used multiple times$"),
    re.compile(r"^error: '[^']*' requires a subcommand but one was not provided$"),
    re.compile(r"^Usage: [^ ].*$"),
    re.compile(r"^vibe [0-9][0-9A-Za-z.+-]*$"),
)


def prose(text: str) -> dict[str, Any]:
    """A sentence the reference wrote, as its length and digest."""

    return {"prose": len(text), "sha256": hashlib.sha256(text.encode("utf-8")).hexdigest()}


def line_record(text: str | None) -> Any:
    """A stream's first line: cleartext when clap worded it, else a digest."""

    if text is None:
        return None
    if any(template.match(text) for template in CLAP_TEMPLATES):
        return text
    return prose(text)


# --------------------------------------------------------------------------
# Running the binary
# --------------------------------------------------------------------------


class Binary:
    """The reference's ``vibe``, run in a home and a workspace of its own."""

    def __init__(self, command: list[str], root: Path) -> None:
        self.command = command
        self.home = root / "home"
        self.vibe_home = root / "vibe-home"
        self.workspace = root / "workspace"
        for directory in (self.home, self.vibe_home, self.workspace / "present"):
            directory.mkdir(parents=True, exist_ok=True)

    def run(self, argv: list[str], *, deleted_cwd: bool = False) -> tuple[int, str, str]:
        # An update check that got past its validation must not reach the
        # real release index.
        environment = host_env.isolated(
            self.home, self.vibe_home, VIBE_UPDATE_BASE_URL="http://127.0.0.1:9"
        )
        command, cwd = [*self.command, *argv], self.workspace
        if deleted_cwd:
            gone = self.workspace.parent / "gone"
            gone.mkdir(exist_ok=True)
            words = " ".join(f"'{word}'" for word in command)
            command, cwd = ["/bin/sh", "-c", f"cd '{gone}' && rmdir '{gone}' && exec {words}"], None
        try:
            completed = subprocess.run(
                command,
                cwd=cwd,
                env=environment,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                timeout=TIMEOUT_SECONDS,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            raise OracleError(f"`vibe {' '.join(argv)}` ran past {TIMEOUT_SECONDS}s") from error
        return (
            completed.returncode,
            completed.stdout.decode("utf-8", "replace"),
            completed.stderr.decode("utf-8", "replace"),
        )


# --------------------------------------------------------------------------
# Help renders
# --------------------------------------------------------------------------


def split_annotations(text: str) -> tuple[str, dict[str, Any]]:
    """A description without its trailing clap annotations, and those."""

    found: dict[str, Any] = {}
    while True:
        match = ANNOTATION.search(text)
        if match is None:
            return text.strip(), found
        kind, value = match.group(1), match.group(2)
        text = text[: match.start()]
        if kind == "possible values":
            found["possibleValues"] = [item.strip() for item in value.split(",")]
        elif kind.startswith("alias"):
            found["aliases"] = [item.strip() for item in value.split(",")]
        elif kind.startswith("short alias"):
            found["shortAliases"] = [item.strip() for item in value.split(",")]
        else:
            found[kind] = value


def option_entry(spelling: str) -> dict[str, Any]:
    """An option's spellings and value from its help entry, ``-v, --x <X>``."""

    value = None
    optional = False
    head = spelling
    match = re.search(r"\s(\[?<[^>]+>\]?(?:\.\.\.)?)$", spelling)
    if match is not None:
        value = match.group(1)
        head = spelling[: match.start()]
        optional = value.startswith("[")
    short = None
    long = None
    for part in (item.strip() for item in head.split(",")):
        if part.startswith("--"):
            long = part
        elif part.startswith("-"):
            short = part
    return {"short": short, "long": long, "value": value, "optionalValue": optional}


def parse_help(text: str) -> dict[str, Any]:
    """A clap help render decomposed into what the replay compares."""

    lines = text.split("\n")
    description: list[str] = []
    usage = None
    sections: dict[str, list[dict[str, Any]]] = {}
    title: str | None = None
    for line in lines:
        if line.startswith("Usage: "):
            usage = line[len("Usage: "):]
            title = None
            continue
        heading = HEADING.match(line)
        if heading is not None:
            # A heading that comes back is the reference's own text after the
            # generated help (the root lists `update` that way), kept apart.
            title = heading.group(1)
            while title in sections:
                title += " (again)"
            sections[title] = []
            continue
        stripped = line.strip()
        if not stripped:
            continue
        if title is None:
            if usage is None:
                description.append(stripped)
            continue
        current = sections[title]
        indent = len(line) - len(line.lstrip(" "))
        # An option entry starts with its first spelling two columns in, or
        # six when it has no short form; any other entry starts two in.
        # Anything deeper continues the description of the entry above, which
        # clap sets below it when it lays the help out long.
        if title == "Options":
            starts = indent <= 6 and stripped.startswith("-")
        else:
            starts = indent == 2
        if not starts:
            if current:
                current[-1]["text"] = f"{current[-1]['text']} {stripped}".strip()
            continue
        parts = GAP.split(stripped, maxsplit=1)
        current.append({"entry": parts[0], "text": parts[1] if len(parts) > 1 else ""})
    options: list[dict[str, Any]] = []
    for item in sections.get("Options", []):
        text, annotations = split_annotations(item["text"])
        options.append({
            **option_entry(item["entry"]),
            **annotations,
            "description": prose(text) if text else None,
        })
    def listed(title: str) -> list[dict[str, Any]]:
        out = []
        for item in sections.get(title, []):
            text, _ = split_annotations(item["text"])
            out.append({"name": item["entry"], "description": prose(text) if text else None})
        return out

    commands = listed("Commands")
    trailing = listed("Commands (again)")
    arguments = []
    for item in sections.get("Arguments", []):
        text, annotations = split_annotations(item["text"])
        arguments.append({
            "name": item["entry"],
            "required": item["entry"].startswith("<"),
            **annotations,
            "description": prose(text) if text else None,
        })
    return {
        "description": prose(" ".join(description)) if description else None,
        "usage": usage,
        "commands": commands,
        "trailingCommands": trailing,
        "arguments": arguments,
        "options": options,
        "sections": sorted(sections),
    }


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------


def discover(binary: Binary) -> dict[tuple[str, ...], dict[str, Any]]:
    """Every command the help reaches, with its decomposed help."""

    found: dict[tuple[str, ...], dict[str, Any]] = {}
    pending: list[tuple[str, ...]] = [()]
    while pending:
        path = pending.pop(0)
        code, stdout, stderr = binary.run([*path, "--help"])
        if code != 0:
            raise OracleError(f"`vibe {' '.join(path)} --help` exited {code}: {stderr.strip()[:200]}")
        record = parse_help(stdout)
        found[path] = record
        for command in record["commands"]:
            if command["name"] != "help":
                pending.append((*path, command["name"]))
    return found


def vectors(commands: dict[tuple[str, ...], dict[str, Any]]) -> list[tuple[str, list[str]]]:
    """The argument vectors driven, each under a stable name."""

    out: list[tuple[str, list[str]]] = [("version-long", ["--version"]), ("version-short", ["-v"])]
    for path, record in commands.items():
        prefix = "-".join(path) or "root"
        out.append((f"{prefix}/help-long", [*path, "--help"]))
        out.append((f"{prefix}/help-short", [*path, "-h"]))
        out += [(f"{prefix}/unknown{flag}", [*path, flag]) for flag in UNKNOWN_FLAGS]
        if record["commands"] and not record["arguments"] and path:
            out.append((f"{prefix}/no-subcommand", [*path]))
            out.append((f"{prefix}/unknown-subcommand", [*path, "no-such-command"]))
        if any(argument["required"] for argument in record["arguments"]):
            out.append((f"{prefix}/missing-argument", [*path]))
        for option in record["options"]:
            flag = option["long"] or option["short"]
            if flag in ("--help", "-h", "--version", "-v"):
                continue
            if option["value"] is not None and not option["optionalValue"]:
                out.append((f"{prefix}/missing-value{flag}", [*path, flag]))
            if option.get("possibleValues"):
                out.append((f"{prefix}/not-a-choice{flag}", [*path, flag, NOT_A_CHOICE]))
    return out


def first_line(text: str) -> str | None:
    return next((line for line in text.split("\n") if line.strip()), None)


def capture(binary: Binary) -> list[dict[str, Any]]:
    commands = discover(binary)
    scenarios: list[dict[str, Any]] = []
    for path, record in commands.items():
        scenarios.append({
            "name": "help/" + (" ".join(path) or "vibe"),
            "scenario": {"command": list(path)},
            "observed": record,
        })
    driven = [(f"case/{name}", argv, False) for name, argv in vectors(commands)]
    if Path("/bin/sh").exists():
        driven += [(f"case/startup/{name}", argv, name == "working-directory-deleted") for name, argv in STARTUP]
    else:
        driven += [(f"case/startup/{name}", argv, False) for name, argv in STARTUP if name != "working-directory-deleted"]
    for name, argv, deleted_cwd in driven:
        code, stdout, stderr = binary.run(argv, deleted_cwd=deleted_cwd)
        # A help render is the help scenarios' subject; here only which
        # stream carried it counts.
        renders_help = argv[-1] in ("--help", "-h")
        scenarios.append({
            "name": name,
            "scenario": {"argv": argv},
            "observed": {
                "exit": code,
                "stdout": bool(stdout),
                "stderr": bool(stderr),
                "stdoutFirstLine": None if renders_help else line_record(first_line(stdout)),
                "stderrFirstLine": line_record(first_line(stderr)),
            },
        })
    return scenarios


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--corpus", type=Path, nargs="?", const=DEFAULT_CORPUS, default=None)
    parser.add_argument("--output", type=Path, default=None)
    parser.add_argument("--check", action="store_true", help="recapture and compare")
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    arguments = parser.parse_args()
    try:
        commit, command = reference_command(arguments.reference, "vibe", arguments.expected_commit)
        with tempfile.TemporaryDirectory(prefix="cli-surface-") as scratch:
            binary = Binary(command, Path(scratch))
            code, stdout, _ = binary.run(["--version"])
            version = stdout.strip().removeprefix("vibe ")
            if code != 0 or not version:
                raise OracleError("the reference binary printed no version")
            scenarios = capture(binary)
    except OracleError as error:
        print(f"cli surface capture failed: {error}", file=sys.stderr)
        return 1
    corpus = {
        "schemaVersion": SCHEMA_VERSION,
        "reference": {"commit": commit, "version": version},
        "scenarios": scenarios,
    }
    rendered = json.dumps(corpus, indent=1, ensure_ascii=False) + "\n"
    if arguments.check:
        committed = DEFAULT_CORPUS.read_text(encoding="utf-8")
        if committed != rendered:
            print("the committed corpus differs from a fresh capture", file=sys.stderr)
            return 1
        print(f"corpus matches a fresh capture of {len(scenarios)} scenarios")
        return 0
    target = arguments.output or arguments.corpus
    if target is None:
        sys.stdout.write(rendered)
        return 0
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(rendered, encoding="utf-8", newline="")
    print(f"captured {len(scenarios)} scenarios from {commit[:12]} into {target}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
