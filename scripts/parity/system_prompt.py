#!/usr/bin/env python3
"""Capture of the system prompt and its project context, row 19 of ``docs/parity.md``.

Every scenario builds a fresh tree: a home whose ``.vibe`` is the vibe home, a
workspace, and whatever directories, Git repositories, skills, agents, prompt
files and ``AGENTS.md`` documents it lays out. It then runs the ``vibe`` entry
point with ``-p`` behind a scripted chat-completions endpoint and records what
the system messages of every request carry: how many there are, whether one
leads, in order which of the scenario's data they mention, and the prompt
itself as a length and a SHA-256.

Through v2.26.0 most scenarios asked the reference's own
``get_universal_system_prompt`` for the prompt section by section. v3.0.0
moved the composition into its app server (``vibe/app_server/_runtime.py``)
and the Unified Harness core, which appends the capability sections natively,
so the prompt a session sends is now the one measurement that covers all of
it. The Windows shell sections, which only that in-process call could show on
a POSIX host, left with it. The same scenarios drive either implementation,
which is how ``crates/vibe-cli/tests/system_prompt_parity_tests.rs`` measures
this port's live sessions.

``NOTICE`` forbids committing reference-authored prose, so no prompt text is
recorded: the prompt is a length and a digest, taken with the scenario root,
the date and what a run mints (commit hashes, the scratchpad, identifiers)
replaced so it is the same from one run to the next. What the corpus pins in
cleartext is the structure and the data.

Usage::

    python3 scripts/parity/system_prompt.py                # capture the reference
    python3 scripts/parity/system_prompt.py --check        # recapture and compare
    python3 scripts/parity/system_prompt.py --materialize compose/default --root /tmp/x
    python3 scripts/parity/system_prompt.py --live-binary target/debug/vibe --output /tmp/live.json
"""

from __future__ import annotations

import argparse
import copy
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT  # noqa: E402
import provenance  # noqa: E402
from reference import (  # noqa: E402
    OracleError,
    reference_command,
)
import host_env  # noqa: E402
import stand_in  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-cli/tests/system-prompt-parity/corpus.json"
SCHEMA_VERSION = 1
RUN_TIMEOUT = 120.0
ROOT_PLACEHOLDER = "<root>"
DATE_PLACEHOLDER = "<DATE>"
#: A shell no machine has, so the value is recognizably the scenario's own.
ORACLE_SHELL = "/opt/oracle/bin/oracle-shell"


# --------------------------------------------------------------------------
# Scenario vocabulary
# --------------------------------------------------------------------------

BASE_CONFIG = (
    'active_model = "oracle-model"\n'
    "enable_telemetry = false\n"
    "enable_update_checks = false\n"
    "$EXTRA"
    "\n[experiments]\nenable = false\n"
    "\n[[providers]]\n"
    'name = "oracle-provider"\n'
    'api_base = "$BACKEND/v1"\n'
    'api_key_env_var = "MISTRAL_API_KEY"\n'
    'api_style = "openai"\n'
    'backend = "generic"\n'
    "\n[[models]]\n"
    'name = "oracle-model-v1"\n'
    'provider = "oracle-provider"\n'
    'alias = "oracle-model"\n'
)

USER_AGENTS = "User rule: answer in plain English."
WORKSPACE_AGENTS = "Workspace rule: indent with tabs."
PARENT_AGENTS = "Parent rule: keep commits small."
EXTRA_AGENTS = "Extra root rule: never touch vendored code."
NESTED_AGENTS = "Nested rule: this subtree is generated."
HOME_AGENTS = "Home rule: this file sits in the home directory."

SKILL = (
    "---\nname: oracle-review\n"
    "description: Reviews a diff for <risky> changes & 'quoted' \"names\"\n---\n"
    "Review the diff.\n"
)
USER_ONLY_SKILL = (
    "---\nname: oracle-deploy\ndescription: Deploys the service on request\n"
    "disable-model-invocation: true\n---\nDeploy.\n"
)
MODEL_ONLY_SKILL = (
    "---\nname: oracle-lint\ndescription: Lints the workspace quietly\n"
    "user-invocable: false\n---\nLint.\n"
)
SUBAGENT = (
    'display_name = "Oracle Reviewer"\n'
    'description = "Reviews code without changing it"\n'
    'agent_type = "subagent"\n'
    'safety = "safe"\n'
)
CUSTOM_PROMPT = (
    "Oracle custom prompt, dated $current_date and ${current_date}.\n"
    "Costs $$5, keeps $other_name and a lone $ sign.\n"
)

STANDARD_FILES = {
    "home/.vibe/AGENTS.md": USER_AGENTS + "\n",
    "workspace/AGENTS.md": "\n" + WORKSPACE_AGENTS + "\n\n",
    "home/.vibe/skills/oracle-review/SKILL.md": SKILL,
    "home/.vibe/agents/oracle-reviewer.toml": SUBAGENT,
}
STANDARD_REPOSITORY = {
    "path": "workspace",
    "branch": "feature-oracle",
    "commits": ["Initial oracle commit", "Add the parser (#12)", "Fix (the) edge case"],
    "remoteMaster": True,
}


def scenario(name: str, **fields: Any) -> dict[str, Any]:
    base: dict[str, Any] = {
        "name": name,
        "family": "compose",
        "files": dict(STANDARD_FILES),
        "config": "",
        "trusted": ["workspace"],
        "repositories": [copy.deepcopy(STANDARD_REPOSITORY)],
        "cwd": "workspace",
        "addDirs": [],
        "headless": False,
        "scratchpad": True,
        "agent": None,
        "shell": ORACLE_SHELL,
    }
    base.update(fields)
    return base


def with_files(extra: dict[str, str | None]) -> dict[str, str]:
    files = dict(STANDARD_FILES)
    for path, content in extra.items():
        if content is None:
            files.pop(path, None)
        else:
            files[path] = content
    return files


def compose_scenarios() -> list[dict[str, Any]]:
    return [
        scenario("compose/default"),
        scenario("compose/headless", headless=True),
        scenario(
            "compose/sections-off",
            config=(
                "include_commit_signature = false\ninclude_model_info = false\n"
                "include_project_context = false\ninclude_prompt_detail = false\n"
            ),
        ),
        scenario("compose/no-prompt-detail", config="include_prompt_detail = false\n"),
        scenario("compose/no-project-context", config="include_project_context = false\n"),
        scenario(
            "compose/no-signature-no-model",
            config="include_commit_signature = false\ninclude_model_info = false\n",
        ),
        scenario("compose/no-scratchpad", scratchpad=False),
        scenario("compose/shell-unset", shell=None),
        scenario("compose/untrusted", trusted=[]),
        scenario(
            "compose/trust-root-above",
            trusted=["."],
            files=with_files({"AGENTS.md": PARENT_AGENTS + "\n"}),
        ),
        scenario(
            "compose/add-dir",
            addDirs=["extra-root"],
            files=with_files({"extra-root/AGENTS.md": EXTRA_AGENTS}),
        ),
        scenario(
            "compose/add-dir-untrusted-cwd",
            trusted=[],
            addDirs=["extra-root"],
            files=with_files({"extra-root/AGENTS.md": EXTRA_AGENTS}),
        ),
        scenario("compose/add-dir-is-cwd", addDirs=["workspace"]),
        scenario(
            "compose/add-dir-contains-cwd",
            addDirs=["."],
            files=with_files({"AGENTS.md": PARENT_AGENTS}),
        ),
        scenario(
            "compose/nested-agents-below-cwd",
            files=with_files({"workspace/generated/AGENTS.md": NESTED_AGENTS}),
        ),
        scenario(
            "compose/blank-agents",
            files=with_files({"workspace/AGENTS.md": "   \n\n", "home/.vibe/AGENTS.md": "\n"}),
        ),
        scenario("compose/user-agents-only", files=with_files({"workspace/AGENTS.md": None})),
        scenario("compose/project-agents-only", files=with_files({"home/.vibe/AGENTS.md": None})),
        scenario(
            "compose/cwd-is-home",
            cwd="home",
            trusted=["home"],
            repositories=[],
            files=with_files({"home/AGENTS.md": HOME_AGENTS}),
        ),
        scenario("compose/cwd-is-documents", cwd="home/Documents", trusted=["home/Documents"], repositories=[]),
        scenario("compose/not-a-repository", repositories=[]),
        scenario(
            "compose/repository-without-commits",
            repositories=[{"path": "workspace", "branch": "empty", "commits": []}],
        ),
        scenario(
            "compose/detached-head",
            repositories=[{**STANDARD_REPOSITORY, "detach": True, "remoteMaster": False}],
        ),
        scenario("compose/commit-count-one", config="[project_context]\ndefault_commit_count = 1\n"),
        scenario("compose/commit-count-zero", config="[project_context]\ndefault_commit_count = 0\n"),
        scenario("compose/prompt-explore", config='system_prompt_id = "explore"\n'),
        scenario("compose/prompt-tests", config='system_prompt_id = "tests"\n'),
        scenario("compose/prompt-lean", config='system_prompt_id = "lean"\n'),
        scenario("compose/prompt-minimal", config='system_prompt_id = "minimal"\n'),
        scenario("compose/prompt-v2", config='system_prompt_id = "cli_2026-07_v2"\n'),
        scenario("compose/prompt-v3", config='system_prompt_id = "cli_2026-08_v3"\n'),
        scenario("compose/prompt-upper-case", config='system_prompt_id = "CLI"\n'),
        scenario("compose/prompt-suffix-replaced", config='system_prompt_id = "minimal.v9"\n'),
        scenario("compose/prompt-bundled-utility", config='system_prompt_id = "worktree_name"\n'),
        scenario(
            "compose/prompt-project-file",
            config='system_prompt_id = "oracle"\n',
            files=with_files({"workspace/.vibe/prompts/oracle.md": CUSTOM_PROMPT}),
        ),
        scenario(
            "compose/prompt-user-file",
            config='system_prompt_id = "oracle"\n',
            files=with_files({"home/.vibe/prompts/oracle.md": "User oracle prompt.\n"}),
        ),
        scenario(
            "compose/prompt-project-over-user",
            config='system_prompt_id = "oracle"\n',
            files=with_files(
                {
                    "workspace/.vibe/prompts/oracle.md": "Project oracle prompt.",
                    "home/.vibe/prompts/oracle.md": "User oracle prompt.",
                }
            ),
        ),
        scenario(
            "compose/prompt-file-overrides-builtin",
            config='system_prompt_id = "cli"\n',
            files=with_files({"home/.vibe/prompts/cli.md": "Replaced cli prompt on $current_date."}),
        ),
        scenario(
            "compose/prompt-project-file-untrusted",
            trusted=[],
            config='system_prompt_id = "oracle"\n',
            files=with_files(
                {
                    "workspace/.vibe/prompts/oracle.md": "Project oracle prompt.",
                    "home/.vibe/prompts/oracle.md": "User oracle prompt.",
                }
            ),
        ),
        scenario(
            "compose/prompt-missing",
            config='system_prompt_id = "nowhere"\n',
            files=with_files({"home/.vibe/prompts/alpha.md": "a", "home/.vibe/prompts/beta.v1.md": "b"}),
        ),
        scenario("compose/prompt-path", config='system_prompt_id = "../escape"\n'),
        scenario(
            "compose/skills-user-and-model-only",
            files=with_files(
                {
                    "home/.vibe/skills/oracle-deploy/SKILL.md": USER_ONLY_SKILL,
                    "home/.vibe/skills/oracle-lint/SKILL.md": MODEL_ONLY_SKILL,
                }
            ),
        ),
        scenario(
            "compose/skills-only-user-invocable",
            config='enabled_skills = ["oracle-deploy"]\n',
            files=with_files({"home/.vibe/skills/oracle-deploy/SKILL.md": USER_ONLY_SKILL}),
        ),
        scenario(
            "compose/skills-only-model-invocable",
            config='enabled_skills = ["oracle-lint"]\n',
            files=with_files({"home/.vibe/skills/oracle-lint/SKILL.md": MODEL_ONLY_SKILL}),
        ),
        scenario("compose/skills-none", config='enabled_skills = ["nothing-matches"]\n'),
        scenario(
            "compose/project-skill",
            files=with_files({"workspace/.vibe/skills/oracle-lint/SKILL.md": MODEL_ONLY_SKILL}),
        ),
        scenario("compose/subagents-none", config='disabled_agents = ["explore", "oracle-reviewer"]\n'),
        scenario("compose/subagent-explore", agent="explore"),
        scenario("compose/subagent-custom", agent="oracle-reviewer", headless=True),
    ]


def live_scenarios() -> list[dict[str, Any]]:
    base = scenario("live/programmatic")
    base.update({"family": "live", "backend": [{"text": "Done."}], "args": ["-p", "hello"]})
    task = scenario("live/subagent")
    task.update(
        {
            "family": "live",
            "backend": [
                {"toolCalls": [{"id": "call_task", "name": "task", "arguments": {"task": "Look around.", "agent": "explore"}}]},
                {"text": "Child done."},
                {"text": "Done."},
            ],
            "args": ["-p", "delegate"],
        }
    )
    return [base, task]


#: Compose scenarios a headless run cannot tell from the default one: every
#: live run is headless, and a session places its own scratchpad.
INDISTINCT = {"compose/headless", "compose/no-scratchpad"}


def as_live(entry: dict[str, Any]) -> dict[str, Any]:
    """A compose scenario run through ``vibe -p`` in the tree it lays out.

    Since v3.0.0 the reference composes the prompt across its app server
    (``vibe/app_server/_runtime.py``) and the Unified Harness core, which
    appends the capability sections natively, so no in-process function
    answers for the whole prompt any more; what a session sends the model is
    the measurement.
    """

    entry = copy.deepcopy(entry)
    arguments = ["-p", "hello"]
    for directory in entry.get("addDirs", []):
        arguments += ["--add-dir", f"$ROOT/{directory}"]
    if entry.get("agent"):
        arguments += ["--agent", entry["agent"]]
    entry.update({"family": "live", "backend": [{"text": "Done."}], "args": arguments})
    return entry


def scenarios() -> list[dict[str, Any]]:
    composed = [as_live(entry) for entry in compose_scenarios() if entry["name"] not in INDISTINCT]
    return [*composed, *live_scenarios()]


# --------------------------------------------------------------------------
# Building a tree
# --------------------------------------------------------------------------


def git(repository: Path, *arguments: str) -> None:
    environment = git_environment()
    subprocess.run(
        ["git", "-c", "user.name=Oracle", "-c", "user.email=oracle@example.invalid",
         "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main", *arguments],
        cwd=repository,
        check=True,
        capture_output=True,
        env=environment,
    )


def git_environment() -> dict[str, str]:
    environment = {
        key: value for key, value in os.environ.items() if not key.startswith("GIT_")
    }
    environment.update(
        host_env.git_isolated(
            GIT_AUTHOR_DATE="2026-01-01T00:00:00Z",
            GIT_COMMITTER_DATE="2026-01-01T00:00:00Z",
        )
    )
    return environment


def materialize(scenario: dict[str, Any], root: Path, backend: str = "http://127.0.0.1:9") -> None:
    """Lays the scenario's tree out under ``root``."""

    for directory in ("home/.vibe", "workspace", "scratch", "home/Documents"):
        (root / directory).mkdir(parents=True, exist_ok=True)
    for relative, content in scenario.get("files", {}).items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8", newline="")
    for directory in scenario.get("addDirs", []):
        (root / directory).mkdir(parents=True, exist_ok=True)
    config = BASE_CONFIG.replace("$BACKEND", backend)
    lines = scenario.get("config", "").splitlines(keepends=True)
    at = next((i for i, line in enumerate(lines) if line.lstrip().startswith("[")), len(lines))
    config = config.replace("$EXTRA", "".join(lines[:at])) + "\n" + "".join(lines[at:])
    (root / "home/.vibe/config.toml").write_text(config, encoding="utf-8", newline="")
    trusted = [str((root / path).resolve()) for path in scenario.get("trusted", [])]
    (root / "home/.vibe/trusted_folders.toml").write_text(
        "trusted = " + json.dumps(trusted) + "\nuntrusted = []\n", encoding="utf-8", newline=""
    )
    for repository in scenario.get("repositories", []):
        path = root / repository["path"]
        path.mkdir(parents=True, exist_ok=True)
        git(path, "init", "-q")
        git(path, "checkout", "-q", "-b", repository["branch"])
        for index, subject in enumerate(repository.get("commits", [])):
            (path / f"commit-{index}.txt").write_text(subject + "\n", encoding="utf-8", newline="")
            git(path, "add", f"commit-{index}.txt")
            git(path, "commit", "-q", "-m", subject)
        if repository.get("remoteMaster") and repository.get("commits"):
            git(path, "update-ref", "refs/remotes/origin/master", "HEAD")
        if repository.get("detach") and repository.get("commits"):
            git(path, "checkout", "-q", "--detach", "HEAD")


def environment_for(scenario: dict[str, Any], root: Path) -> dict[str, str]:
    environment = {
        **host_env.isolated(root / "home", root / "home/.vibe"),
        "MISTRAL_API_KEY": "oracle-key",
        **host_env.git_isolated(),
    }
    if scenario.get("shell"):
        environment["SHELL"] = scenario["shell"]
    return environment


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


def digest(value: str) -> dict[str, Any]:
    return {"prose": len(value), "sha256": hashlib.sha256(value.encode("utf-8")).hexdigest()}


def today() -> str:
    date = datetime.date.today()
    return f"{date.isoformat()} ({date.strftime('%A')})"


class Normalizer:
    def __init__(self, root: Path) -> None:
        self.roots = sorted({str(root), str(root.resolve())}, key=len, reverse=True)
        self.date = today()

    def text(self, value: str) -> str:
        for root in self.roots:
            value = value.replace(root, ROOT_PLACEHOLDER)
        return value.replace(self.date, DATE_PLACEHOLDER)


# --------------------------------------------------------------------------
# A live run
# --------------------------------------------------------------------------

def live_tokens(root: Path) -> list[tuple[str, str]]:
    """The scenario data a live system message may carry, as (label, text)."""

    return [
        ("user-agents", USER_AGENTS),
        ("workspace-agents", WORKSPACE_AGENTS),
        ("skill-name", "oracle-review"),
        ("skill-description", "Reviews a diff for &lt;risky&gt; changes &amp; &#x27;quoted&#x27; &quot;names&quot;"),
        ("subagent-explore", "**explore**"),
        ("subagent-custom", "**oracle-reviewer**: Reviews code without changing it"),
        ("model", "`oracle-model`"),
        ("platform", "Linux"),
        ("shell", ORACLE_SHELL),
        ("scratchpad", "vibe-scratchpad-"),
        ("branch", "feature-oracle"),
        ("commit-first", "Initial oracle commit"),
        ("commit-parser", "Add the parser\n"),
        ("commit-edge", "Fix (the) edge case"),
        ("workspace-path", str(root / "workspace") + "\n"),
        ("date", today()),
        ("headless", "# Headless Mode"),
    ]


#: A commit hash, a scratchpad directory and a session identifier, which a
#: run mints and the prompt names.
MINTED = (
    (re.compile(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b"), "<id>"),
    (re.compile(r"\b[0-9a-f]{7,40}\b"), "<hash>"),
    (re.compile(r"\S*vibe-scratchpad-[A-Za-z0-9_]+"), "<scratchpad>"),
)


def prompt_text(text: str, normalizer: Normalizer) -> str:
    """The system text with the scenario root, the date and what a run mints
    replaced, so its digest is the same from one run to the next."""

    text = normalizer.text(text)
    for pattern, placeholder in MINTED:
        text = pattern.sub(placeholder, text)
    return text


def run_live(scenario: dict[str, Any], command: list[str]) -> dict[str, Any]:
    with host_env.scratch_root("vibe-system-prompt-live-") as root:
        backend = stand_in.Backend(copy.deepcopy(scenario["backend"]), stream=stand_in.always)
        try:
            materialize(scenario, root, backend.base)
            environment = environment_for(scenario, root)
            arguments = [word.replace("$ROOT", str(root)) for word in scenario["args"]]
            process = subprocess.run(
                [*command, *arguments],
                cwd=root / scenario["cwd"],
                env=environment,
                capture_output=True,
                text=True,
                timeout=RUN_TIMEOUT,
                stdin=subprocess.DEVNULL,
            )
            requests = []
            tokens = live_tokens(root)
            normalizer = Normalizer(root)
            for body in backend.bodies:
                systems = [
                    message.get("content") or ""
                    for message in body.get("messages") or []
                    if message.get("role") == "system"
                ]
                joined = "\n\n".join(content if isinstance(content, str) else json.dumps(content) for content in systems)
                found = sorted(
                    (joined.find(text), label, joined.count(text))
                    for label, text in tokens
                    if text in joined
                )
                requests.append(
                    {
                        "prompt": digest(prompt_text(joined, normalizer)) if joined else None,
                        "systemMessages": len(systems),
                        "leadingSystem": bool(body.get("messages")) and body["messages"][0].get("role") == "system",
                        "carries": [{"data": label, "occurrences": count} for _, label, count in found],
                    }
                )
            return {"exit": process.returncode, "requests": requests}
        finally:
            backend.close()


# --------------------------------------------------------------------------
# Driver
# --------------------------------------------------------------------------


def capture(scenario: dict[str, Any], command: list[str]) -> dict[str, Any]:
    observed = run_live(scenario, command)
    return {"name": scenario["name"], "family": scenario["family"], "scenario": scenario, "observed": observed}


def rendered(payload: dict[str, Any]) -> str:
    return json.dumps(payload, indent=2, sort_keys=True, ensure_ascii=False) + "\n"


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    parser.add_argument("--output", type=Path, default=None)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--only", action="append", default=[])
    parser.add_argument("--live-binary", type=Path, default=None, help="run the live family against this `vibe`")
    parser.add_argument("--materialize", default=None, help="lay one scenario's tree out under --root")
    parser.add_argument("--root", type=Path, default=None)
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    arguments.output = provenance.port_output(arguments.output, DEFAULT_OUTPUT, live_binary=arguments.live_binary)
    try:
        if arguments.materialize is not None:
            chosen = next((s for s in scenarios() if s["name"] == arguments.materialize), None)
            if chosen is None or arguments.root is None:
                raise OracleError("--materialize needs a known scenario and --root")
            materialize(chosen, arguments.root)
            print(json.dumps({"cwd": chosen["cwd"], "environment": environment_for(chosen, arguments.root)}))
            return 0
        if arguments.live_binary is not None:
            command = [str(arguments.live_binary.resolve())]
            selected = [s for s in scenarios() if not arguments.only or any(n in s["name"] for n in arguments.only)]
            captured = [capture(s, command) for s in selected]
            arguments.output.write_text(rendered({"scenarios": captured}), encoding="utf-8", newline="")
            print(f"captured {len(captured)} live scenarios into {arguments.output}")
            return 0
        commit, vibe = reference_command(arguments.reference, "vibe", arguments.expected_commit)
        reference = {"commit": commit}
        command = [*vibe]
        selected = [s for s in scenarios() if not arguments.only or any(n in s["name"] for n in arguments.only)]
        captured = []
        for chosen in selected:
            entry = capture(chosen, command)
            if not arguments.check:
                again = capture(chosen, command)
                if again["observed"] != entry["observed"]:
                    raise OracleError(f"{chosen['name']} is not deterministic across two captures")
            captured.append(entry)
            print(f"{chosen['name']}: captured", file=sys.stderr)
        corpus = {
            "schemaVersion": SCHEMA_VERSION,
            "reference": reference,
            "note": (
                "Captured by scripts/parity/system_prompt.py from the pinned reference's `vibe -p`. "
                "Each request records which of the scenario's data its system messages carry, and "
                "the prompt itself, with paths under <root>, the date as <DATE> and what a run mints "
                "replaced, as a length and a SHA-256."
            ),
            "scenarios": captured,
        }
        if arguments.check:
            committed = json.loads(arguments.output.read_text(encoding="utf-8"))
            by_name = {entry["name"]: entry for entry in committed["scenarios"]}
            differing = [e["name"] for e in captured if by_name.get(e["name"], {}).get("observed") != e["observed"]]
            if differing:
                raise OracleError("a fresh capture differs for " + ", ".join(differing))
            print(f"{len(captured)} scenarios match the committed corpus")
            return 0
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        arguments.output.write_text(rendered(corpus), encoding="utf-8", newline="")
        print(f"captured {len(captured)} scenarios into {arguments.output}")
    except (OracleError, subprocess.TimeoutExpired, subprocess.CalledProcessError) as error:
        print(f"system prompt capture failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
