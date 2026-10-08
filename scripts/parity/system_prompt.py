#!/usr/bin/env python3
"""Capture of the system prompt and its project context, row 19 of ``docs/parity.md``.

Three families of scenarios, each built over a fresh tree: a home whose
``.vibe`` is the vibe home, a workspace, and whatever directories, Git
repositories, skills, agents, prompt files and ``AGENTS.md`` documents the
scenario lays out.

``compose`` asks the pinned reference's own ``get_universal_system_prompt``
(``vibe/core/system_prompt.py``) for the prompt of a session opened in that
tree, loading the configuration, the trust store, the skills and the agents
the way a session does. Every helper the function calls is observed, so the
prompt is split into its sections with certainty, and the split is checked to
rebuild the prompt byte for byte. Each section is then recorded as the data it
carries (paths, names, Git state, documents, skills as the prompt spells them)
and its prose as a length and a SHA-256.

``os`` asks the same module which operating system section a Windows session
gets for each combination of published shell tools and resolved shell, which a
POSIX host cannot otherwise observe.

``live`` runs the ``vibe`` entry point with ``-p`` behind a scripted
chat-completions endpoint and records what the system messages of every
request carry: how many there are and, in order, which of the scenario's data
they mention. That family drives either implementation, which is how
``crates/vibe-cli/tests/system_prompt_parity_tests.rs`` measures that the
port's live sessions send the prompt its composition builds.

``NOTICE`` forbids committing reference-authored prose, so no prompt text is
recorded: builtin prompts, templates and fixed sentences are lengths and
digests, and the replay counts two digests as equal. What the corpus pins is the
structure and the data.

Usage::

    python3 scripts/parity/system_prompt.py                # capture the reference
    python3 scripts/parity/system_prompt.py --check        # recapture and compare
    python3 scripts/parity/system_prompt.py --materialize compose/default --root /tmp/x
    python3 scripts/parity/system_prompt.py --live-binary target/debug/vibe --output /tmp/live.json
"""

from __future__ import annotations

import argparse
import asyncio
import copy
import datetime
import hashlib
import http.server
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import threading
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT, HARNESS_FLAGS  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-cli/tests/system-prompt-parity/corpus.json"
SCHEMA_VERSION = 1
RUN_TIMEOUT = 120.0
ROOT_PLACEHOLDER = "<root>"
DATE_PLACEHOLDER = "<DATE>"
#: A shell no machine has, so the value is recognizably the scenario's own.
ORACLE_SHELL = "/opt/oracle/bin/oracle-shell"
HASH = re.compile(r"^[0-9a-f]{7,40}(?= |$)")
CO_AUTHOR = "Co-Authored-By: Mistral Vibe <vibe@mistral.ai>"


class OracleError(RuntimeError):
    pass


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


def os_scenarios() -> list[dict[str, Any]]:
    bash = "C:\\Program Files\\Git\\bin\\bash.exe"
    cmd = "C:\\Windows\\System32\\cmd.exe"
    cases = [
        ("os/windows-git-bash-tool", ["git_bash", "powershell"], ("bash", bash)),
        ("os/windows-powershell-tool", ["powershell"], ("cmd", cmd)),
        ("os/windows-resolved-bash", [], ("bash", bash)),
        ("os/windows-resolved-cmd", ["bash"], ("cmd", cmd)),
    ]
    return [
        {"name": name, "family": "os", "tools": tools, "resolved": {"kind": kind, "executable": exe}}
        for name, tools, (kind, exe) in cases
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


def scenarios() -> list[dict[str, Any]]:
    return [*compose_scenarios(), *os_scenarios(), *live_scenarios()]


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
        {
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_AUTHOR_DATE": "2026-01-01T00:00:00Z",
            "GIT_COMMITTER_DATE": "2026-01-01T00:00:00Z",
        }
    )
    return environment


def materialize(scenario: dict[str, Any], root: Path, backend: str = "http://127.0.0.1:9") -> None:
    """Lays the scenario's tree out under ``root``."""

    for directory in ("home/.vibe", "workspace", "scratch", "home/Documents"):
        (root / directory).mkdir(parents=True, exist_ok=True)
    for relative, content in scenario.get("files", {}).items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
    for directory in scenario.get("addDirs", []):
        (root / directory).mkdir(parents=True, exist_ok=True)
    config = BASE_CONFIG.replace("$BACKEND", backend)
    lines = scenario.get("config", "").splitlines(keepends=True)
    at = next((i for i, line in enumerate(lines) if line.lstrip().startswith("[")), len(lines))
    config = config.replace("$EXTRA", "".join(lines[:at])) + "\n" + "".join(lines[at:])
    (root / "home/.vibe/config.toml").write_text(config, encoding="utf-8")
    trusted = [str((root / path).resolve()) for path in scenario.get("trusted", [])]
    (root / "home/.vibe/trusted_folders.toml").write_text(
        "trusted = " + json.dumps(trusted) + "\nuntrusted = []\n", encoding="utf-8"
    )
    for repository in scenario.get("repositories", []):
        path = root / repository["path"]
        path.mkdir(parents=True, exist_ok=True)
        git(path, "init", "-q")
        git(path, "checkout", "-q", "-b", repository["branch"])
        for index, subject in enumerate(repository.get("commits", [])):
            (path / f"commit-{index}.txt").write_text(subject + "\n", encoding="utf-8")
            git(path, "add", f"commit-{index}.txt")
            git(path, "commit", "-q", "-m", subject)
        if repository.get("remoteMaster") and repository.get("commits"):
            git(path, "update-ref", "refs/remotes/origin/master", "HEAD")
        if repository.get("detach") and repository.get("commits"):
            git(path, "checkout", "-q", "--detach", "HEAD")


def environment_for(scenario: dict[str, Any], root: Path) -> dict[str, str]:
    environment = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(root / "home"),
        "VIBE_HOME": str(root / "home/.vibe"),
        "LANG": "C.UTF-8",
        "TERM": "dumb",
        "NO_COLOR": "1",
        "CI": "true",
        "DBUS_SESSION_BUS_ADDRESS": "unix:path=/nonexistent",
        "MISTRAL_API_KEY": "oracle-key",
        "GIT_CONFIG_GLOBAL": os.devnull,
        "GIT_CONFIG_NOSYSTEM": "1",
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

    def prose(self, value: str) -> dict[str, Any]:
        return digest(self.text(value))

    def commit(self, line: str) -> str:
        return HASH.sub("<hash>", self.text(line))


# --------------------------------------------------------------------------
# The reference, in-process (run in a child per scenario)
# --------------------------------------------------------------------------

SKILL_BLOCK = re.compile(r"<available_skills>\n.*\n</available_skills>", re.DOTALL)
SKILL_ENTRY = re.compile(
    r"  <skill>\n    <name>(.*?)</name>\n    <description>(.*?)</description>\n"
    r"(?:    <path>(.*?)</path>\n)?  </skill>",
    re.DOTALL,
)


def child_compose(scenario: dict[str, Any], root: Path) -> dict[str, Any]:
    from vibe.core.agents import AgentManager
    from vibe.core.config.default_orchestrator import build_default_orchestrator
    from vibe.core.config.harness_files import (
        get_harness_files_manager,
        init_harness_files_manager,
    )
    from vibe.core.paths import VIBE_HOME
    from vibe.core.prompts import MissingPromptFileError, load_system_prompt
    from vibe.core.skills.manager import SkillManager
    import vibe.core.system_prompt as sp

    normalizer = Normalizer(root)
    init_harness_files_manager(
        "user", "project", additional_dirs=[root / d for d in scenario.get("addDirs", [])]
    )
    harness = get_harness_files_manager().for_session(Path.cwd())
    try:
        orchestrator = asyncio.run(build_default_orchestrator())
    except Exception as error:  # the validator refused the prompt identifier
        return {"error": prompt_error(scenario, normalizer, load_system_prompt, MissingPromptFileError, error)}
    agent = scenario.get("agent")
    agent_manager = AgentManager(
        orchestrator,
        initial_agent=agent or "accept-edits",
        allow_subagent=agent is not None,
        harness_files=harness,
    )
    config = orchestrator.config
    skill_manager = SkillManager(lambda: orchestrator.config, harness_files=harness)
    scratchpad = root / "scratch" if scenario.get("scratchpad") and agent is None else None

    recorded: dict[str, Any] = {}

    def observe(name: str, function: Any) -> Any:
        def wrapper(*arguments: Any, **keywords: Any) -> Any:
            result = function(*arguments, **keywords)
            recorded[name] = {"arguments": arguments, "result": result}
            return result

        return wrapper

    for name in (
        "_interpolate_prompt",
        "_get_headless_section",
        "_add_commit_signature",
        "_get_tool_aware_os_system_prompt",
        "_get_windows_bash_system_prompt",
        "_get_windows_cmd_system_prompt",
        "_get_windows_powershell_system_prompt",
        "_get_available_skills_section",
        "_get_available_subagents_section",
        "_get_scratchpad_section",
        "is_dangerous_directory",
        "get_agents_md_section",
    ):
        setattr(sp, name, observe(name, getattr(sp, name)))
    provider = sp.ProjectContextProvider
    original_full = provider.get_full_context
    original_status = provider.get_git_status

    def full_context(self: Any) -> str:
        result = original_full(self)
        recorded["project_context"] = {"result": result, "root": self.root_path}
        return result

    def git_status(self: Any) -> str:
        result = original_status(self)
        recorded["git_status"] = result
        return result

    provider.get_full_context = full_context
    provider.get_git_status = git_status

    cwd = Path.cwd()
    prompt = sp.get_universal_system_prompt(
        config,
        skill_manager,
        agent_manager,
        scratchpad_dir=scratchpad,
        headless=bool(scenario.get("headless")),
        cwd=cwd,
        harness_files=harness,
        tool_manager=None,
    )

    sections: list[tuple[str, str, dict[str, Any]]] = []
    base = recorded["_interpolate_prompt"]["result"]
    sections.append(("base", base, base_data(config.system_prompt_id, harness, base, normalizer)))
    if "_get_headless_section" in recorded:
        sections.append(("headless", recorded["_get_headless_section"]["result"], {}))
    if "_add_commit_signature" in recorded:
        text = recorded["_add_commit_signature"]["result"]
        sections.append(("commit_signature", text, {"coAuthorTrailer": CO_AUTHOR in text}))
    if config.include_model_info:
        alias = config.get_active_model().alias
        sections.append(("model_info", f"Your model name is: `{alias}`", {"alias": alias}))
    if "_get_tool_aware_os_system_prompt" in recorded:
        text = recorded["_get_tool_aware_os_system_prompt"]["result"]
        sections.append(("operating_system", text, os_data(text, recorded)))
    skills = recorded.get("_get_available_skills_section", {}).get("result")
    if skills:
        sections.append(("skills", skills, skills_data(skills, skill_manager, normalizer)))
    subagents = recorded.get("_get_available_subagents_section", {}).get("result")
    if subagents:
        sections.append(("subagents", subagents, {"lines": [line for line in subagents.split("\n") if line.startswith("- ")]}))
    scratch = recorded.get("_get_scratchpad_section", {}).get("result")
    if scratch:
        sections.append(("scratchpad", scratch, {"path": normalizer.text(str(scratchpad))}))
    if config.include_project_context:
        dangerous, reason = recorded["is_dangerous_directory"]["result"]
        if dangerous:
            from string import Template

            from vibe.core.prompts import UtilityPrompt

            text = Template(UtilityPrompt.DANGEROUS_DIRECTORY.read()).safe_substitute(
                reason=reason.lower(), abs_path=cwd.resolve()
            )
            sections.append(
                (
                    "dangerous_directory",
                    text,
                    {"absPath": normalizer.text(str(cwd.resolve())), "description": reason.removeprefix("You are in the ")},
                )
            )
        else:
            context = recorded["project_context"]
            sections.append(
                (
                    "project_context",
                    context["result"],
                    {
                        "absPath": normalizer.text(str(context["root"])),
                        "git": git_data(recorded["git_status"], normalizer),
                    },
                )
            )
        extra = [root_path for root_path in harness.project_roots if root_path.resolve() != cwd.resolve()]
        if extra:
            text = (
                "Additional working directories (treated with the same "
                "file-access permissions as the primary working directory):\n"
                + "\n".join(f" - {d}" for d in extra)
            )
            sections.append(("additional_directories", text, {"directories": [normalizer.text(str(d)) for d in extra]}))
        agents = recorded["get_agents_md_section"]
        if agents["result"]:
            user_doc, project_docs = agents["arguments"]
            sections.append(
                (
                    "instructions",
                    agents["result"],
                    {
                        "user": (
                            {"path": normalizer.text(f"{VIBE_HOME.path}/AGENTS.md"), "content": user_doc.strip()}
                            if user_doc.strip()
                            else None
                        ),
                        "project": [
                            {"directory": normalizer.text(str(directory)), "content": content.strip()}
                            for directory, content in project_docs
                        ],
                    },
                )
            )
    rebuilt = "\n\n".join(text for _, text, _ in sections)
    if rebuilt != prompt:
        raise OracleError(f"{scenario['name']}: the recorded sections do not rebuild the prompt")
    return {
        "sections": [
            {"kind": kind, "text": normalizer.prose(text), **data} for kind, text, data in sections
        ]
    }


def base_data(prompt_id: str, harness: Any, text: str, normalizer: Normalizer) -> dict[str, Any]:
    directories = [*harness.project_prompts_dirs, *harness.user_prompts_dirs]
    file_name = Path(prompt_id).with_suffix(".md").name
    for directory in directories:
        if (directory / file_name).is_file():
            return {
                "promptId": prompt_id,
                "source": "file",
                "file": normalizer.text(str(directory / file_name)),
                "content": normalizer.text(text),
            }
    return {
        "promptId": prompt_id,
        "source": "builtin",
        "dated": normalizer.date in text,
    }


def prompt_error(
    scenario: dict[str, Any], normalizer: Normalizer, load: Any, missing: Any, raised: Exception
) -> dict[str, Any]:
    prompt_id = re.search(r'system_prompt_id = "(.*)"', scenario.get("config", ""))
    if prompt_id is None:
        raise OracleError(f"{scenario['name']}: configuration failed to load: {raised}") from raised
    try:
        load(prompt_id.group(1))
    except missing as error:
        message = str(error)
        builtins = re.search(r"available prompts \((.*?)\)", message)
        available = re.search(r"\(available: (.*)\)$", message)
        directories = re.search(r"\.md file in (.*) \(available", message)
        return {
            "kind": "missing",
            "promptId": error.prompt_id,
            "builtins": re.findall(r'"([^"]*)"', builtins.group(1)) if builtins else [],
            "directories": [normalizer.text(d) for d in directories.group(1).split(" or ")] if directories else [],
            "available": re.findall(r'"([^"]*)"', available.group(1)) if available else [],
        }
    except ValueError:
        return {"kind": "invalid", "promptId": prompt_id.group(1)}
    raise OracleError(f"{scenario['name']}: configuration failed but the prompt resolves: {raised}")


def os_data(text: str, recorded: dict[str, Any]) -> dict[str, Any]:
    first = text.split("\n", 1)[0]
    match = re.search(r"The operating system is (.*) with shell `(.*)`$", first)
    if match is None:
        raise OracleError(f"unrecognized operating system line: {first!r}")
    rules = None
    for name, label in (
        ("_get_windows_bash_system_prompt", "git_bash"),
        ("_get_windows_cmd_system_prompt", "cmd"),
        ("_get_windows_powershell_system_prompt", "powershell"),
    ):
        if name in recorded:
            rules = label
    return {"platform": match.group(1), "shell": match.group(2), "rules": rules}


def skills_data(text: str, skill_manager: Any, normalizer: Normalizer) -> dict[str, Any]:
    available = skill_manager.available_skills
    block = SKILL_BLOCK.search(text)
    entries = []
    if block is not None:
        for name, description, path in SKILL_ENTRY.findall(block.group(0)):
            builtin = next(
                (skill for skill in available.values() if skill.name == name and skill.skill_path is None),
                None,
            )
            entries.append(
                {
                    "name": name,
                    "description": normalizer.prose(description) if builtin is not None else description,
                    "path": normalizer.text(path) if path else None,
                }
            )
    return {
        "modelIntro": any(skill.model_invocable for skill in available.values()),
        "userIntro": any(skill.user_invocable for skill in available.values()),
        "entries": entries,
    }


def git_data(status: str, normalizer: Normalizer) -> dict[str, Any]:
    if status.startswith("Current branch:"):
        lines = status.split("\n")
        commits = lines[3:] if len(lines) > 2 and lines[2] == "Recent commits:" else []
        return {
            "kind": "repository",
            "currentBranch": lines[0].removeprefix("Current branch: "),
            "mainBranch": lines[1].split(": ", 1)[1],
            "commits": [normalizer.commit(line) for line in commits],
        }
    if status.startswith("Git operations timed out"):
        return {"kind": "timed_out"}
    if status.startswith("Not a git repository"):
        return {"kind": "unavailable"}
    return {"kind": "failed"}


def child_os(scenario: dict[str, Any]) -> dict[str, Any]:
    from types import SimpleNamespace

    import vibe.core.system_prompt as sp
    from vibe.core.utils import WindowsShellKind

    resolved = scenario["resolved"]
    shell = SimpleNamespace(
        kind=WindowsShellKind.BASH if resolved["kind"] == "bash" else WindowsShellKind.CMD,
        executable=resolved["executable"],
    )
    sp.is_windows = lambda: True
    sp.get_platform_display_name = lambda: "Windows"
    sp.resolve_windows_shell = lambda: shell
    recorded: dict[str, Any] = {}
    for name in (
        "_get_windows_bash_system_prompt",
        "_get_windows_cmd_system_prompt",
        "_get_windows_powershell_system_prompt",
    ):
        function = getattr(sp, name)

        def wrapper(function: Any = function, name: str = name) -> str:
            recorded[name] = True
            return function()

        setattr(sp, name, wrapper)
    tools = SimpleNamespace(available_tools={name: None for name in scenario["tools"]})
    text = sp._get_tool_aware_os_system_prompt(tools)
    return {"section": {"kind": "operating_system", "text": digest(text), **os_data(text, recorded)}}


def run_child(scenario: dict[str, Any], interpreter: Path) -> dict[str, Any]:
    root = Path(tempfile.mkdtemp(prefix="vibe-system-prompt-oracle-")).resolve()
    try:
        if scenario["family"] == "compose":
            materialize(scenario, root)
            cwd = root / scenario["cwd"]
            environment = environment_for(scenario, root)
        else:
            cwd = root
            environment = environment_for({}, root)
        process = subprocess.run(
            [str(interpreter), str(Path(__file__).resolve()), "--child", json.dumps(scenario), "--root", str(root)],
            cwd=cwd,
            env=environment,
            capture_output=True,
            text=True,
            timeout=RUN_TIMEOUT,
        )
        if process.returncode != 0:
            raise OracleError(f"{scenario['name']}: the reference child failed:\n{process.stderr}")
        return json.loads(process.stdout.strip().splitlines()[-1])
    finally:
        shutil.rmtree(root, ignore_errors=True)


# --------------------------------------------------------------------------
# The live family: a black-box run of `vibe -p`
# --------------------------------------------------------------------------


class Backend:
    """Serves scripted completions and keeps every request body."""

    def __init__(self, responses: list[dict[str, Any]]) -> None:
        import acp

        self.responses = list(responses)
        self.bodies: list[dict[str, Any]] = []
        backend = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_: Any) -> None:
                return

            def do_GET(self) -> None:  # noqa: N802
                self.reply(404, {"error": "not found"})

            def do_POST(self) -> None:  # noqa: N802
                length = int(self.headers.get("content-length") or 0)
                raw = self.rfile.read(length) if length else b""
                try:
                    body = json.loads(raw or b"{}")
                except json.JSONDecodeError:
                    body = {}
                if not self.path.rstrip("/").endswith("/chat/completions"):
                    self.reply(404, {"error": "not found"})
                    return
                backend.bodies.append(body)
                response = backend.responses.pop(0) if backend.responses else {"text": "Done."}
                model = str(body.get("model", "model"))
                payload = b"".join(
                    b"data: " + json.dumps(item).encode() + b"\n\n"
                    for item in acp.completion_chunks(response, model)
                ) + b"data: [DONE]\n\n"
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.send_header("content-length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def reply(self, status: int, body: Any) -> None:
                payload = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def base(self) -> str:
        return f"http://127.0.0.1:{self.server.server_address[1]}"

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()


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


def run_live(scenario: dict[str, Any], command: list[str]) -> dict[str, Any]:
    root = Path(tempfile.mkdtemp(prefix="vibe-system-prompt-live-")).resolve()
    backend = Backend(copy.deepcopy(scenario["backend"]))
    try:
        materialize(scenario, root, backend.base)
        environment = environment_for(scenario, root)
        process = subprocess.run(
            [*command, *scenario["args"]],
            cwd=root / scenario["cwd"],
            env=environment,
            capture_output=True,
            text=True,
            timeout=RUN_TIMEOUT,
            stdin=subprocess.DEVNULL,
        )
        requests = []
        tokens = live_tokens(root)
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
                    "systemMessages": len(systems),
                    "leadingSystem": bool(body.get("messages")) and body["messages"][0].get("role") == "system",
                    "carries": [{"data": label, "occurrences": count} for _, label, count in found],
                }
            )
        return {"exit": process.returncode, "requests": requests}
    finally:
        backend.close()
        shutil.rmtree(root, ignore_errors=True)


# --------------------------------------------------------------------------
# Driver
# --------------------------------------------------------------------------


def reference_interpreter(reference: Path) -> Path:
    for candidate in (reference / ".venv/bin/python", reference / ".venv/Scripts/python.exe"):
        if candidate.is_file():
            return candidate
    raise OracleError(f"no reference interpreter under {reference}; run `uv sync --frozen`")


def resolve_reference(reference: Path, expected: str | None) -> dict[str, str]:
    if not reference.is_dir():
        raise OracleError(f"no reference checkout at {reference}")
    commit = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=reference, capture_output=True, text=True, check=False
    ).stdout.strip()
    if expected and commit != expected:
        raise OracleError(f"the reference is at {commit}, not the pinned {expected}")
    return {"commit": commit}


def capture(scenario: dict[str, Any], interpreter: Path | None, command: list[str]) -> dict[str, Any]:
    if scenario["family"] == "live":
        observed = run_live(scenario, command)
    else:
        if interpreter is None:
            raise OracleError("the compose and os families need the reference interpreter")
        observed = run_child(scenario, interpreter)
    return {"name": scenario["name"], "family": scenario["family"], "scenario": scenario, "observed": observed}


def rendered(payload: dict[str, Any]) -> str:
    return json.dumps(payload, indent=2, sort_keys=True, ensure_ascii=False) + "\n"


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--only", action="append", default=[])
    parser.add_argument("--live-binary", type=Path, default=None, help="run the live family against this `vibe`")
    parser.add_argument("--materialize", default=None, help="lay one scenario's tree out under --root")
    parser.add_argument("--child", default=None, help=argparse.SUPPRESS)
    parser.add_argument("--root", type=Path, default=None)
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        if arguments.child is not None:
            scenario = json.loads(arguments.child)
            result = child_os(scenario) if scenario["family"] == "os" else child_compose(scenario, arguments.root)
            print(json.dumps(result))
            return 0
        if arguments.materialize is not None:
            chosen = next((s for s in scenarios() if s["name"] == arguments.materialize), None)
            if chosen is None or arguments.root is None:
                raise OracleError("--materialize needs a known scenario and --root")
            materialize(chosen, arguments.root)
            print(json.dumps({"cwd": chosen["cwd"], "environment": environment_for(chosen, arguments.root)}))
            return 0
        if arguments.live_binary is not None:
            command = [str(arguments.live_binary.resolve()), *HARNESS_FLAGS]
            selected = [s for s in live_scenarios() if not arguments.only or any(n in s["name"] for n in arguments.only)]
            captured = [capture(s, None, command) for s in selected]
            arguments.output.write_text(rendered({"scenarios": captured}), encoding="utf-8")
            print(f"captured {len(captured)} live scenarios into {arguments.output}")
            return 0
        reference = resolve_reference(arguments.reference, arguments.expected_commit)
        interpreter = reference_interpreter(arguments.reference)
        command = [str(arguments.reference / ".venv/bin/vibe"), *HARNESS_FLAGS]
        selected = [s for s in scenarios() if not arguments.only or any(n in s["name"] for n in arguments.only)]
        captured = []
        for chosen in selected:
            entry = capture(chosen, interpreter, command)
            if not arguments.check:
                again = capture(chosen, interpreter, command)
                if again["observed"] != entry["observed"]:
                    raise OracleError(f"{chosen['name']} is not deterministic across two captures")
            captured.append(entry)
            print(f"{chosen['name']}: captured", file=sys.stderr)
        corpus = {
            "schemaVersion": SCHEMA_VERSION,
            "reference": reference,
            "note": (
                "Captured by scripts/parity/system_prompt.py from the pinned reference. Sections "
                "carry their data with paths under <root> and the date as <DATE>; every prompt "
                "text, template and fixed sentence the reference authored is a length and a SHA-256."
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
        arguments.output.write_text(rendered(corpus), encoding="utf-8")
        print(f"captured {len(captured)} scenarios into {arguments.output}")
    except (OracleError, subprocess.TimeoutExpired, subprocess.CalledProcessError) as error:
        print(f"system prompt capture failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
