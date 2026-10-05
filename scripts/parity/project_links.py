#!/usr/bin/env python3
"""Black-box capture of the session-less ``projectLinks/*`` surface.

Every scenario serves an app server over stdio in a fresh home, with a set of
directories laid out beside its workspace (plain directories, Git checkouts in
several states, a bare repository, a file), behind the scripted stand-in
``scripts/parity/teleport.py`` already plays for the Vibe Code projects
endpoint. It drives the nine ``projectLinks/*`` methods row 25 of
``docs/parity.md`` is about, and reads back the link store they share with
the ``vibeCode/projects/*`` picker, ``projects.toml`` under the vibe home.

What is recorded, per family:

- ``store``: the file the links live in, byte for byte with the scenario's
  directories replaced by placeholders, its mode, and what a hand-written or
  damaged file reads as;
- ``list``: how saved links are grouped and described;
- ``resolve``: what ``resolveRoot`` and ``inspectRoot`` report for every kind
  of directory, and which saved links ``inspectRoot`` keeps or clears;
- ``picker``: the candidate pages, their ranking and recommendation, the saved
  link a first page reconciles, and every way the listing fails;
- ``create``, ``link``, ``save``, ``unlink``: what each mutation checks, sends
  and persists;
- ``teleport``: how a link this surface saved reads to the session picker that
  shares its store.

Nothing is imported from the reference: its own ``vibe-app-server`` is the
oracle, which is what makes the same scenarios replayable against this port by
``crates/vibe-app-server/tests/project_links_parity_tests.rs``. Every string a
server authored is reduced to its length and SHA-256, which is what ``NOTICE``
requires of a committed corpus; the store is kept as text because it holds only
the scenario's own values under the reference's key names.

Usage::

    python3 scripts/parity/project_links.py                 # capture the reference
    python3 scripts/parity/project_links.py --check         # recapture and compare
    python3 scripts/parity/project_links.py --server target/debug/vibe-app-server-stdio-fixture \\
        --output /tmp/port.json
"""

from __future__ import annotations

import argparse
import concurrent.futures
import copy
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import time
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import acp  # noqa: E402
import agents  # noqa: E402
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT  # noqa: E402
import rewind  # noqa: E402
import teleport  # noqa: E402
from teleport import GITHUB_URL, ok, page, project  # noqa: E402

REPOSITORY = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = REPOSITORY / "crates/vibe-app-server/tests/project-links-parity/corpus.json"
SCHEMA_VERSION = 1

QUIET_SECONDS = 0.4
STORE = "projects.toml"


# --------------------------------------------------------------------------
# The directories
# --------------------------------------------------------------------------


def git(world: acp.World, directory: Path, *args: str, check: bool = True) -> str:
    result = subprocess.run(
        ["git", *args], cwd=directory, env=git_env(world),
        capture_output=True, text=True, check=False,
    )
    if check and result.returncode != 0:
        raise rewind.OracleError(f"git {' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout.strip()


def git_env(world: acp.World) -> dict[str, str]:
    return {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(world.home),
        "GIT_CONFIG_NOSYSTEM": "1",
        "LANG": "C.UTF-8",
        "GIT_AUTHOR_DATE": "2026-01-01T00:00:00Z",
        "GIT_COMMITTER_DATE": "2026-01-01T00:00:00Z",
    }


def make_repository(world: acp.World, path: Path, spec: dict[str, Any]) -> None:
    """A checkout at ``path``: a commit on ``main`` unless ``commits`` is off,
    the remotes ``spec`` names (one GitHub ``origin`` by default), and
    ``origin/HEAD`` naming ``origin/main`` unless ``originHead`` is off."""

    path.mkdir(parents=True, exist_ok=True)
    if spec.get("bare"):
        git(world, path, "init", "-q", "--bare")
        return
    git(world, path, "init", "-q")
    for name, url in spec.get("remotes", {"origin": GITHUB_URL}).items():
        git(world, path, "remote", "add", name, url)
    if spec.get("commits", True):
        (path / "README.md").write_text("hello\n", encoding="utf-8")
        git(world, path, "add", "-A")
        git(world, path, "commit", "-q", "-m", "initial")
        for branch in spec.get("branches", []):
            git(world, path, "branch", branch)
        if spec.get("originHead", True) and "origin" in spec.get("remotes", {"origin": GITHUB_URL}):
            git(world, path, "update-ref", "refs/remotes/origin/main", "HEAD")
            git(world, path, "symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main")
        if branch := spec.get("checkout"):
            git(world, path, "checkout", "-q", "-b", branch)
        if spec.get("detached"):
            git(world, path, "checkout", "-q", "--detach")
    for key, value in spec.get("config", {}).items():
        git(world, path, "config", key, value)
    for nested in spec.get("nested", []):
        (path / nested).mkdir(parents=True, exist_ok=True)


def lay_out(world: acp.World, layout: dict[str, Any]) -> None:
    (world.home / ".gitconfig").write_text(
        "[user]\n\tname = Oracle\n\temail = oracle@example.com\n"
        "[init]\n\tdefaultBranch = main\n"
        f"[http]\n\tproxy = {teleport.DEAD_PROXY}\n"
        "[advice]\n\tdetachedHead = false\n",
        encoding="utf-8",
    )
    for name, spec in layout.items():
        path = world.root / name
        if spec == "plain":
            path.mkdir(parents=True, exist_ok=True)
        elif spec == "file":
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("not a directory\n", encoding="utf-8")
        else:
            make_repository(world, path, spec)


def substitute(value: Any, world: acp.World, session: rewind.Session,
               picker: str | None = None) -> Any:
    if isinstance(value, dict):
        return {key: substitute(item, world, session, picker) for key, item in value.items()}
    if isinstance(value, list):
        return [substitute(item, world, session, picker) for item in value]
    if not isinstance(value, str):
        return value
    value = (
        value.replace("$VH", str(world.vibe_home))
        .replace("$HOME", str(world.home))
    )
    if picker is not None:
        value = value.replace("$PICKER", picker)
    return session.substitute(value)


# --------------------------------------------------------------------------
# Running a scenario
# --------------------------------------------------------------------------


def store_view(world: acp.World) -> dict[str, Any]:
    path = world.vibe_home / STORE
    if not path.exists():
        return {"exists": False}
    return {
        "exists": True,
        "mode": oct(stat.S_IMODE(path.stat().st_mode)),
        "text": path.read_text(encoding="utf-8").split("\n"),
    }


def run_scenario(scenario: dict[str, Any], command: list[str], quiet: float) -> dict[str, Any]:
    backend = teleport.Backend(scenario.get("code", {}))
    root = Path(tempfile.mkdtemp(prefix="vibe-project-links-oracle-"))
    session = rewind.Session(root)
    world = session.world
    try:
        lay_out(world, scenario.get("layout", {}))
        (world.vibe_home / "config.toml").write_text(
            teleport.base_config(backend, scenario), encoding="utf-8"
        )
        trusted = ", ".join(json.dumps(str(path)) for path in (world.workspace, world.root / "repo"))
        (world.vibe_home / "trusted_folders.toml").write_text(
            f"trusted = [{trusted}]\nuntrusted = []\n", encoding="utf-8"
        )
        if (seed := scenario.get("store")) is not None:
            path = world.vibe_home / STORE
            path.write_text(substitute(seed, world, session), encoding="utf-8")
            path.chmod(int(scenario.get("storeMode", 0o600)))
        env = {
            **git_env(world),
            "VIBE_HOME": str(world.vibe_home),
            "MISTRAL_API_KEY": teleport.API_KEY,
            "VIBE_API_BASE": f"{backend.base}/v1/chat/completions",
            "TERM": "dumb",
            "NO_COLOR": "1",
            "CI": "true",
            "DBUS_SESSION_BUS_ADDRESS": "unix:path=/nonexistent",
        }
        for key, value in scenario.get("env", {}).items():
            if value is None:
                env.pop(key, None)
            else:
                env[key] = value
        server = agents.Server(command, env, world.workspace, {}, world, callbacks=[])
        steps: list[dict[str, Any]] = []
        try:
            run_steps(scenario, server, session, steps, quiet)
        except (rewind.OracleError, acp.OracleError) as error:
            steps.append({"failure": str(error)})
        finally:
            server.stop()
        return {
            "steps": steps,
            "sessions": session.sessions,
            "code": list(backend.code_log),
            "paths": {
                "workspace": str(world.workspace),
                "vibeHome": str(world.vibe_home),
                "home": str(world.home),
                "root": str(root),
                "bin": str(Path(command[0]).parent),
                "backend": backend.base,
            },
        }
    finally:
        backend.close()
        # A scenario may leave a store it made read-only.
        for path in (world.vibe_home / STORE, world.vibe_home):
            if path.exists():
                path.chmod(0o700)
        shutil.rmtree(root, ignore_errors=True)


def run_steps(
    scenario: dict[str, Any],
    server: rewind.Server,
    session: rewind.Session,
    steps: list[dict[str, Any]],
    quiet: float,
) -> None:
    world = session.world
    picker: str | None = None
    identifier = 1
    server.send({"jsonrpc": "2.0", "id": identifier, "method": "initialize",
                 "params": {"clientInfo": {"name": "project-links-oracle", "version": "0"},
                            "capabilities": {"callbackKinds": []}}})
    server.collect(identifier, quiet)
    server.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
    for step in scenario["steps"]:
        identifier += 1
        if "store" in step:
            steps.append({"store": store_view(world)})
            continue
        if "git" in step:
            directory = Path(substitute(step["cwd"], world, session))
            for arguments in step["git"]:
                git(world, directory, *arguments)
            continue
        if "remove" in step:
            shutil.rmtree(substitute(step["remove"], world, session))
            continue
        if "chmod" in step:
            Path(substitute(step["chmod"], world, session)).chmod(int(step["mode"]))
            continue
        if "start" in step:
            config = {"cwd": substitute(step["start"].get("cwd", "$WS"), world, session),
                      "agent": "auto-approve"}
            server.send({"jsonrpc": "2.0", "id": identifier, "method": "session/start",
                         "params": {"agentConfig": config}})
            observed = server.collect(identifier, quiet)
            for message in observed:
                session.learn(message)
            response = next((m for m in observed if m.get("id") == identifier and "method" not in m), None)
            steps.append({"start": {"error": response["error"]} if response and "error" in response
                          else {"started": response is not None}})
            continue
        request = substitute(step["send"], world, session, picker)
        server.send({"jsonrpc": "2.0", "id": identifier, **request})
        observed = server.collect(identifier, quiet)
        for message in observed:
            session.learn(message)
        response = next((m for m in observed if m.get("id") == identifier and "method" not in m), None)
        result = (response or {}).get("result")
        if isinstance(result, dict) and isinstance(result.get("pickerId"), str):
            picker = result["pickerId"]
        steps.append({
            "method": request["method"],
            "response": {k: v for k, v in (response or {"missing": True}).items()
                         if k not in {"jsonrpc", "id"}},
        })


# --------------------------------------------------------------------------
# Normalization
# --------------------------------------------------------------------------


class Normalizer(teleport.Normalizer):
    pass


def normalize_run(scenario: dict[str, Any], run: dict[str, Any]) -> dict[str, Any]:
    normalizer = Normalizer(scenario, run["paths"])
    for identity in run["sessions"]:
        normalizer.identity(identity)
    steps = []
    for step in run["steps"]:
        if "store" in step:
            view = dict(step["store"])
            if "text" in view:
                # The store is the scenario's own values under the reference's
                # key names, so it is kept as text, only its paths replaced.
                view["text"] = [normalizer.text(line) for line in view["text"]]
            steps.append({"store": view})
        else:
            normalizer.collect_ids(step)
            steps.append(normalizer.value(copy.deepcopy(step)))
    return {
        "steps": steps,
        "code": [normalizer.value(teleport.code_view(entry)) for entry in run["code"]],
    }


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------


def send(method: str, **params: Any) -> dict[str, Any]:
    return {"send": {"method": method, "params": params}}


def links(method: str, **params: Any) -> dict[str, Any]:
    return send(f"projectLinks/{method}", **params)


def listing() -> dict[str, Any]:
    return links("list")


def resolve(path: str) -> dict[str, Any]:
    return links("resolveRoot", rootPath=path)


def inspect(path: str) -> dict[str, Any]:
    return links("inspectRoot", rootPath=path)


def load(path: str = "$ROOT/repo") -> dict[str, Any]:
    return links("picker/load", rootPath=path)


def load_more(cursor: str, path: str = "$ROOT/repo") -> dict[str, Any]:
    return links("picker/loadMore", rootPath=path, cursor=cursor)


def create(name: str = "Fresh", branch: str = "main", path: str = "$ROOT/repo") -> dict[str, Any]:
    return links("create", rootPath=path, name=name, defaultBranch=branch)


def link(project_id: str, name: str = "Client name", path: str = "$ROOT/repo") -> dict[str, Any]:
    return links("link", rootPath=path, projectId=project_id, projectName=name)


def save(path: str, expected: str | None, project_id: str = "proj-1",
         name: str = "Saved") -> dict[str, Any]:
    return links("save", rootPath=path, projectId=project_id, projectName=name,
                 expectedGithubRepoUrl=expected)


def unlink(path: str) -> dict[str, Any]:
    return links("unlink", rootPath=path)


STORE_STEP = {"store": True}

REPO = {"repo": {}}
OTHER_URL = "https://github.com/someone/else.git"


def remote(root: str, project_id: str = "proj-1", name: str = "Repo",
           url: str = GITHUB_URL) -> dict[str, str]:
    return {"kind": "remote", "repo_root": root, "repo_url": url,
            "project_id": project_id, "project_name": name}


def local(directory: str, project_id: str = "proj-1", name: str = "Local") -> dict[str, str]:
    return {"kind": "local", "directory_path": directory, "project_id": project_id,
            "project_name": name}


def store(*entries: dict[str, Any], head: str = "version = 1\n") -> str:
    """A hand-written store: one ``[[projects]]`` table per entry."""

    text = head
    for entry in entries:
        text += "\n[[projects]]\n"
        for key, value in entry.items():
            text += f"{key} = {json.dumps(value)}\n"
    return text


ONE_PROJECT = {"pages": {"": page(project("proj-1", "Repo"))}}

#: Projects over two pages: the saved one on the first, a multi-repository
#: match, a read-only one and one for another repository, then a second page.
TWO_PAGES = {"pages": {
    "": page(
        project("proj-b", "beta"),
        project("proj-multi", "Alpha multi", repos=[GITHUB_URL, OTHER_URL]),
        project("proj-ro", "Read only", read_only=True),
        project("proj-other", "Elsewhere", repos=[OTHER_URL]),
        project("proj-a", "Alpha"),
        cursor="c2",
    ),
    "c2": page(project("proj-c", "Gamma"), project("proj-ro2", "Read only 2", read_only=True)),
}}

#: A second page holding nothing selectable, so paging carries on to a third.
SKIPPING_PAGES = {"pages": {
    "": page(project("proj-1", "Repo"), cursor="c2"),
    "c2": page(project("proj-other", "Elsewhere", repos=[OTHER_URL]),
               project("proj-ro", "Read only", read_only=True), cursor="c3"),
    "c3": page(project("proj-late", "Late"), project("proj-later", "Later"), cursor="c4"),
    "c4": page(project("proj-last", "Last")),
}}

#: No Mistral key. The port's fixture still needs a credential for its own
#: turns, which ``VIBE_ORACLE_CREDENTIAL`` names; the reference ignores both.
NO_KEY = {"env": {"MISTRAL_API_KEY": None, "OTHER_KEY": "other",
                  "VIBE_ORACLE_CREDENTIAL": "OTHER_KEY"}}


def failure(status: int, body: Any | None = None) -> dict[str, Any]:
    return {"status": status, "body": body if body is not None else {"detail": "no"}}


def scenarios() -> list[dict[str, Any]]:
    found: list[dict[str, Any]] = []

    def add(name: str, steps: list[dict[str, Any]], **extra: Any) -> None:
        found.append({"name": name, "steps": steps, **extra})

    # --------------------------------------------------------------- store
    add("store/absent", [listing(), STORE_STEP])
    add("store/save-creates", [save("$ROOT/plain", None), STORE_STEP],
        layout={"plain": "plain"})
    add("store/unlink-creates-empty", [unlink("$ROOT/plain"), STORE_STEP],
        layout={"plain": "plain"})
    add("store/upsert-replaces-in-place",
        [save("$ROOT/a", None, "proj-new", "New"), STORE_STEP, listing()],
        layout={"a": "plain", "b": "plain"},
        store=store(local("$ROOT/a", "proj-old", "Old"), local("$ROOT/b", "proj-b", "B")))
    add("store/keeps-unknown",
        [save("$ROOT/plain", None), STORE_STEP],
        layout={"plain": "plain"},
        store=(
            'comment = "kept"\nversion = 3\n\n[extra]\nflag = true\nlist = [1, 2]\n\n'
            '[[projects]]\nkind = "local"\ndirectory_path = "/nowhere/else"\n'
            'project_id = "proj-x"\nproject_name = "X"\nunknown = "field"\n\n'
            '[[projects]]\nkind = "mystery"\nwhatever = 1\n\n'
            '[[projects]]\nkind = "remote"\nrepo_root = "/missing/fields"\n'
        ))
    add("store/invalid-entries-skipped",
        [listing(), save("$ROOT/plain", None), STORE_STEP],
        layout={"plain": "plain"},
        store=(
            'version = 1\nprojects = [\n  "a string",\n  { kind = "local", directory_path = "/l", '
            'project_id = "p-l", project_name = "L" },\n  { kind = "local", project_id = "p" },\n'
            '  { repo_root = "/r", repo_url = "https://github.com/o/r.git", project_id = "p-r", '
            'project_name = "R" },\n  { kind = "remote", repo_root = "/n", repo_url = 7, '
            'project_id = "p-n", project_name = "N" },\n]\n'
        ))
    add("store/projects-not-a-list", [listing(), save("$ROOT/plain", None), STORE_STEP],
        layout={"plain": "plain"}, store='version = 1\nprojects = "nope"\n')
    add("store/corrupt", [listing(), save("$ROOT/plain", None), STORE_STEP],
        layout={"plain": "plain"}, store="version = = 1\n[[projects\n")
    add("store/existing-mode-kept", [save("$ROOT/plain", None), STORE_STEP],
        layout={"plain": "plain"}, store=store(), storeMode=0o644)
    add("store/inline-short-entries",
        [unlink("/x/y/deeper"), STORE_STEP],
        store=store(local("/x", "p1", "One"), local("/x/y", "p2", "Two")))
    add("store/escaped-values",
        [save("$ROOT/plain", None, 'proj "quoted"', 'Name\twith "quotes" \\ and é'),
         STORE_STEP, listing()],
        layout={"plain": "plain"})
    add("store/tilde-paths",
        [listing(), inspect("$HOME/linked"), unlink("$HOME/linked"), STORE_STEP],
        layout={"home/linked": "plain"},
        store=store(local("~/linked", "proj-1", "Tilde")))
    add("store/read-only-store",
        [inspect("$ROOT/repo"), STORE_STEP, save("$ROOT/plain", None), STORE_STEP],
        layout={"repo": {}, "plain": "plain"},
        store=store(remote("$ROOT/repo", url=OTHER_URL)), storeMode=0o444)

    # ---------------------------------------------------------------- list
    add("list/empty-store", [listing()], store=store())
    add("list/grouped",
        [listing()],
        layout={"repo": {}, "unborn": {"commits": False}, "plain": "plain",
                "repo/sub": "plain"},
        store=store(
            remote("$ROOT/repo", "proj-1", "Repo"),
            local("$ROOT/plain", "proj-2", "Plain"),
            local("$ROOT/unborn", "proj-1", "Repo"),
            local("$ROOT/missing", "proj-3", "Gone"),
            local("$ROOT/repo/sub", "proj-2", "Nested"),
        ))
    add("list/without-key", [listing()], layout=REPO,
        store=store(remote("$ROOT/repo")), **NO_KEY)

    # ------------------------------------------------------------- resolve
    resolve_layout = {
        "repo": {"nested": ["deep/er"]},
        "plain": "plain",
        "unborn": {"commits": False},
        "no-remote": {"remotes": {}},
        "gitlab": {"remotes": {"origin": "https://gitlab.com/o/r.git"}},
        "ssh": {"remotes": {"origin": "git@github.com:Oracle/Repo.git"}},
        "second": {"remotes": {"gitlab": "https://gitlab.com/x/y.git",
                               "upstream": "https://github.com/up/stream.git"}},
        "detached": {"detached": True},
        "topic": {"checkout": "topic"},
        "no-origin-head": {"originHead": False},
        "master": {"originHead": False, "remotes": {}, "config": {"init.defaultBranch": "trunk"}},
        "bare.git": {"bare": True},
        "a-file": "file",
    }
    for target in [
        "$ROOT/repo", "$ROOT/repo/deep/er", "$ROOT/repo/", "$ROOT/plain", "$ROOT/unborn",
        "$ROOT/no-remote", "$ROOT/gitlab", "$ROOT/ssh", "$ROOT/second", "$ROOT/detached",
        "$ROOT/topic", "$ROOT/no-origin-head", "$ROOT/master", "$ROOT/bare.git",
        "$ROOT/a-file", "$ROOT/missing", "$ROOT/repo/../plain", "$HOME", "../repo",
        "workspace-relative",
    ]:
        label = target.replace("$ROOT/", "").replace("$HOME", "home").strip("/").replace("/", "-")
        label = label.replace("..", "up") or "root"
        if target.endswith("/"):
            label += "-slash"
        add(f"resolve/{label}", [resolve(target), inspect(target), STORE_STEP],
            layout=resolve_layout)
    add("resolve/master-fallback",
        [resolve("$ROOT/m")],
        layout={"m": {"originHead": False, "remotes": {}, "branches": ["master"],
                      "config": {"init.defaultBranch": "absent"}}})

    add("resolve/inspect-matching-remote", [inspect("$ROOT/repo"), STORE_STEP],
        layout=REPO, store=store(remote("$ROOT/repo")))
    add("resolve/inspect-matching-spelling",
        [inspect("$ROOT/repo")], layout=REPO,
        store=store(remote("$ROOT/repo", url="git@github.com:Oracle/Repo")))
    add("resolve/inspect-stale-remote", [inspect("$ROOT/repo"), STORE_STEP, listing()],
        layout=REPO, store=store(remote("$ROOT/repo", url=OTHER_URL)))
    add("resolve/inspect-remote-on-plain", [inspect("$ROOT/plain"), STORE_STEP],
        layout={"plain": "plain"}, store=store(remote("$ROOT/plain")))
    add("resolve/inspect-local-on-repo", [inspect("$ROOT/repo"), STORE_STEP],
        layout=REPO, store=store(local("$ROOT/repo")))
    add("resolve/inspect-local-on-plain", [inspect("$ROOT/plain")],
        layout={"plain": "plain"}, store=store(local("$ROOT/plain")))
    add("resolve/inspect-nested-reads-root", [inspect("$ROOT/repo/sub")],
        layout={"repo": {"nested": ["sub"]}}, store=store(remote("$ROOT/repo")))
    add("resolve/inspect-stale-clear-fails", [inspect("$ROOT/repo"), STORE_STEP],
        layout=REPO, store=store(remote("$ROOT/repo", url=OTHER_URL)), storeMode=0o444)

    # -------------------------------------------------------------- picker
    add("picker/first-page", [load(), STORE_STEP], layout=REPO, code=TWO_PAGES)
    add("picker/saved-link-first", [load()], layout=REPO, code=TWO_PAGES,
        store=store(remote("$ROOT/repo", "proj-b", "beta")))
    add("picker/saved-link-off-page", [load()], layout=REPO, code=TWO_PAGES,
        store=store(remote("$ROOT/repo", "proj-c", "Gamma")))
    add("picker/saved-link-read-only", [load()], layout=REPO, code=TWO_PAGES,
        store=store(remote("$ROOT/repo", "proj-ro", "Read only")))
    add("picker/stale-link-cleared", [load(), STORE_STEP], layout=REPO, code=TWO_PAGES,
        store=store(remote("$ROOT/repo", "proj-b", "beta", url=OTHER_URL)))
    add("picker/local-link-ignored", [load(), STORE_STEP], layout=REPO, code=TWO_PAGES,
        store=store(local("$ROOT/repo", "proj-b", "beta")))
    add("picker/nested-root", [load("$ROOT/repo/sub")],
        layout={"repo": {"nested": ["sub"]}}, code=ONE_PROJECT)
    add("picker/empty", [load()], layout=REPO)
    add("picker/load-more", [load(), load_more("c2")], layout=REPO, code=TWO_PAGES)
    add("picker/load-more-skips", [load(), load_more("c2"), load_more("c4")],
        layout=REPO, code=SKIPPING_PAGES)
    add("picker/load-more-saved-link", [load_more("c2")], layout=REPO, code=TWO_PAGES,
        store=store(remote("$ROOT/repo", "proj-c", "Gamma")))
    add("picker/load-more-unknown-cursor", [load_more("nope")], layout=REPO, code=TWO_PAGES)
    add("picker/plain-directory", [load("$ROOT/plain"), load_more("c2", "$ROOT/plain")],
        layout={"plain": "plain"}, code=ONE_PROJECT)
    add("picker/no-github-remote", [load("$ROOT/gitlab")],
        layout={"gitlab": {"remotes": {"origin": "https://gitlab.com/o/r.git"}}},
        code=ONE_PROJECT)
    add("picker/no-commits", [load("$ROOT/unborn")],
        layout={"unborn": {"commits": False}}, code=ONE_PROJECT)
    add("picker/missing-root", [load("$ROOT/missing")], code=ONE_PROJECT)
    add("picker/detached", [load("$ROOT/detached")],
        layout={"detached": {"detached": True}}, code=ONE_PROJECT)
    add("picker/without-key", [load(), load_more("c2"), create(), link("proj-1")],
        layout=REPO, code=ONE_PROJECT, **NO_KEY)
    add("picker/without-key-plain", [load("$ROOT/plain")],
        layout={"plain": "plain"}, code=ONE_PROJECT, **NO_KEY)
    for status in (401, 403, 404, 500):
        add(f"picker/list-{status}", [load(), load_more("c2")], layout=REPO,
            code={"pages": {"": failure(status), "c2": failure(status)}})
    add("picker/list-body-names-key", [load()], layout=REPO,
        code={"pages": {"": failure(500, {"detail": "invalid API key"})}})
    add("picker/list-invalid-json", [load()], layout=REPO,
        code={"pages": {"": {"status": 200, "raw": "not json"}}})
    add("picker/list-invalid-schema", [load()], layout=REPO,
        code={"pages": {"": ok({"items": [{"name": "no id"}]})}})
    add("picker/list-dropped", [load()], layout=REPO,
        code={"pages": {"": {"drop": True}}})
    add("picker/stale-clear-fails", [load(), STORE_STEP], layout=REPO, code=TWO_PAGES,
        store=store(remote("$ROOT/repo", "proj-b", "beta", url=OTHER_URL)), storeMode=0o444)
    add("picker/load-more-fails-late", [load_more("c2")], layout=REPO,
        code={"pages": {"c2": page(project("proj-ro", "RO", read_only=True), cursor="c3"),
                        "c3": failure(500)}})

    # -------------------------------------------------------------- create
    add("create/links-the-new-project", [create("  Fresh  ", " main "), STORE_STEP, listing()],
        layout=REPO, code={**ONE_PROJECT, "create": [ok(project("proj-new", "Fresh"))]})
    add("create/replaces-a-saved-link", [create(), STORE_STEP], layout=REPO,
        code={**ONE_PROJECT, "create": [ok(project("proj-new", "Fresh"))]},
        store=store(local("$ROOT/repo", "proj-old", "Old")))
    add("create/blank-name", [create("   "), STORE_STEP], layout=REPO,
        code={**ONE_PROJECT, "create": [ok(project("proj-new", "Fresh"))]})
    add("create/blank-branch", [create("Fresh", "  ")], layout=REPO,
        code={**ONE_PROJECT, "create": [ok(project("proj-new", "Fresh"))]})
    add("create/listing-fails-first", [create(), STORE_STEP], layout=REPO,
        code={"pages": {"": failure(500)}, "create": [ok(project("proj-new", "Fresh"))]})
    for status in (401, 409, 500):
        add(f"create/refused-{status}", [create(), STORE_STEP], layout=REPO,
            code={**ONE_PROJECT, "create": [failure(status)]})
    add("create/invalid-answer", [create()], layout=REPO,
        code={**ONE_PROJECT, "create": [ok({"name": "missing id"})]})
    add("create/nested-root", [create(path="$ROOT/repo/sub"), STORE_STEP],
        layout={"repo": {"nested": ["sub"]}},
        code={**ONE_PROJECT, "create": [ok(project("proj-new", "Fresh"))]})
    add("create/store-fails", [create(), STORE_STEP], layout=REPO,
        code={**ONE_PROJECT, "create": [ok(project("proj-new", "Fresh"))]},
        store=store(), storeMode=0o444)
    add("create/plain-directory", [create(path="$ROOT/plain")],
        layout={"plain": "plain"}, code=ONE_PROJECT)

    # ---------------------------------------------------------------- link
    add("link/validated-target", [link("proj-c"), STORE_STEP, inspect("$ROOT/repo")],
        layout=REPO, code=TWO_PAGES)
    add("link/unknown", [link("proj-zz"), STORE_STEP], layout=REPO, code=TWO_PAGES)
    add("link/read-only", [link("proj-ro")], layout=REPO, code=TWO_PAGES)
    add("link/other-repository", [link("proj-other")], layout=REPO, code=TWO_PAGES)
    add("link/multi-repository", [link("proj-multi"), STORE_STEP], layout=REPO, code=TWO_PAGES)
    add("link/listing-fails", [link("proj-1")], layout=REPO,
        code={"pages": {"": failure(403)}})
    add("link/store-fails", [link("proj-1"), STORE_STEP], layout=REPO, code=ONE_PROJECT,
        store=store(), storeMode=0o444)
    add("link/plain-directory", [link("proj-1", path="$ROOT/plain")],
        layout={"plain": "plain"}, code=ONE_PROJECT)

    # ---------------------------------------------------------------- save
    add("save/plain-local", [save("$ROOT/plain", None), STORE_STEP, listing(),
                             inspect("$ROOT/plain")], layout={"plain": "plain"})
    add("save/plain-with-expected", [save("$ROOT/plain", GITHUB_URL), STORE_STEP],
        layout={"plain": "plain"})
    add("save/repo-matching", [save("$ROOT/repo", " git@github.com:oracle/repo.git "),
                               STORE_STEP, load()], layout=REPO, code=ONE_PROJECT)
    add("save/repo-mismatch", [save("$ROOT/repo", OTHER_URL), STORE_STEP], layout=REPO)
    add("save/repo-without-expected", [save("$ROOT/repo", None)], layout=REPO)
    add("save/gitlab-is-local", [save("$ROOT/gitlab", None), STORE_STEP],
        layout={"gitlab": {"remotes": {"origin": "https://gitlab.com/o/r.git"}}})
    add("save/nested-keyed-on-root", [save("$ROOT/repo/sub", GITHUB_URL), STORE_STEP],
        layout={"repo": {"nested": ["sub"]}})
    add("save/missing-root", [save("$ROOT/missing", None), STORE_STEP])
    add("save/file-root", [save("$ROOT/a-file", None)], layout={"a-file": "file"})
    add("save/without-key", [save("$ROOT/plain", None), STORE_STEP],
        layout={"plain": "plain"}, **NO_KEY)
    add("save/read-only-store", [save("$ROOT/plain", None)],
        layout={"plain": "plain"}, store=store(), storeMode=0o444)

    # -------------------------------------------------------------- unlink
    add("unlink/live-repository", [unlink("$ROOT/repo"), STORE_STEP], layout=REPO,
        store=store(remote("$ROOT/repo"), local("$ROOT/other", "proj-2", "Other")))
    add("unlink/nested-in-repository", [unlink("$ROOT/repo/sub"), STORE_STEP],
        layout={"repo": {"nested": ["sub"]}}, store=store(remote("$ROOT/repo")))
    add("unlink/plain-exact", [unlink("$ROOT/plain"), STORE_STEP],
        layout={"plain": "plain"}, store=store(local("$ROOT/plain")))
    add("unlink/plain-nested-keeps-ancestor", [unlink("$ROOT/plain/inner"), STORE_STEP],
        layout={"plain/inner": "plain"}, store=store(local("$ROOT/plain")))
    add("unlink/moved-checkout",
        [{"remove": "$ROOT/repo"}, unlink("$ROOT/repo/sub/deeper"), STORE_STEP],
        layout={"repo": {"nested": ["sub"]}},
        store=store(remote("$ROOT/repo"), local("$ROOT/repo/sub", "proj-2", "Sub"),
                    local("$ROOT/elsewhere", "proj-3", "Else")))
    add("unlink/moved-no-match", [unlink("$ROOT/gone"), STORE_STEP],
        store=store(local("$ROOT/elsewhere", "proj-3", "Else")))
    add("unlink/without-key", [unlink("$ROOT/plain"), STORE_STEP],
        layout={"plain": "plain"}, store=store(local("$ROOT/plain")), **NO_KEY)
    add("unlink/store-fails", [unlink("$ROOT/plain"), STORE_STEP],
        layout={"plain": "plain"}, store=store(local("$ROOT/plain")), storeMode=0o444)
    add("unlink/stale-store-fails", [unlink("$ROOT/gone"), STORE_STEP],
        store=store(local("$ROOT/gone")), storeMode=0o444)
    add("unlink/remote-on-plain-kind-agnostic", [unlink("$ROOT/plain"), STORE_STEP],
        layout={"plain": "plain"}, store=store(remote("$ROOT/plain")))

    # ------------------------------------------------------------ teleport
    # The session picker reads the checkout the server runs in, so these make
    # the workspace itself the repository.
    session_open = send("vibeCode/projects/open", sessionId="$S1", purpose="configure")
    in_workspace = {"workspace": {}}
    add("teleport/local-link-is-not-saved",
        [{"start": {}}, session_open, STORE_STEP],
        layout=in_workspace, code=ONE_PROJECT, store=store(local("$WS", "proj-1", "Repo")))
    add("teleport/session-select-replaces-local",
        [{"start": {}}, session_open,
         {"send": {"method": "vibeCode/projects/select",
                   "params": {"sessionId": "$S1", "pickerId": "$PICKER", "projectId": "proj-1"}}},
         STORE_STEP, listing()],
        layout=in_workspace, code=ONE_PROJECT, store=store(local("$WS", "proj-old", "Old")))
    add("teleport/link-seen-by-session",
        [link("proj-1", path="$WS"), {"start": {}},
         send("vibeCode/projects/open", sessionId="$S1", purpose="teleport", prompt="Go")],
        layout=in_workspace, code=ONE_PROJECT)
    add("teleport/stale-remote-cleared-by-session",
        [{"start": {}}, send("vibeCode/projects/open", sessionId="$S1", purpose="teleport",
                             prompt="Go"), STORE_STEP],
        layout=in_workspace, code=ONE_PROJECT,
        store=store(remote("$WS", url=OTHER_URL), local("$ROOT/else", "proj-2", "Else")))
    add("teleport/session-unlink-keeps-local",
        [{"start": {}}, session_open,
         {"send": {"method": "vibeCode/projects/unlink",
                   "params": {"sessionId": "$S1", "pickerId": "$PICKER"}}},
         STORE_STEP],
        layout=in_workspace, code=ONE_PROJECT, store=store(local("$WS", "proj-1", "Repo")))

    return found


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--server", type=Path, default=None, help="drive this binary instead")
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--only", action="append", default=[], help="scenario name filter")
    parser.add_argument("--raw", action="store_true", help="also keep the raw answers")
    parser.add_argument("--quiet", type=float, default=QUIET_SECONDS)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--jobs", type=int, default=4, help="scenarios run side by side")
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        if arguments.server is not None:
            command = [str(arguments.server.resolve())]
            reference = {"commit": "server-override"}
        else:
            reference = acp.resolve_reference(arguments.reference, arguments.expected_commit)
            binary = arguments.reference / ".venv/bin/vibe-app-server"
            if not binary.is_file():
                raise rewind.OracleError(f"no reference binary at {binary}; run `uv sync --frozen`")
            command = [str(binary)]
        selected = [
            scenario
            for scenario in scenarios()
            if not arguments.only or any(name in scenario["name"] for name in arguments.only)
        ]

        def capture(scenario: dict[str, Any]) -> dict[str, Any]:
            started_at = time.monotonic()
            run = run_scenario(scenario, command, arguments.quiet)
            entry = {
                "name": scenario["name"],
                "scenario": scenario,
                "observed": normalize_run(scenario, run),
            }
            if arguments.raw:
                entry["raw"] = run
            print(
                f"{scenario['name']}: {len(run['steps'])} observations in "
                f"{time.monotonic() - started_at:.1f}s",
                file=sys.stderr,
            )
            return entry

        with concurrent.futures.ThreadPoolExecutor(max_workers=arguments.jobs) as pool:
            captured = list(pool.map(capture, selected))
        corpus = {
            "schemaVersion": SCHEMA_VERSION,
            "reference": reference,
            "quietSeconds": arguments.quiet,
            "scenarios": captured,
        }
        if arguments.check:
            committed = json.loads(arguments.output.read_text(encoding="utf-8"))
            by_name = {entry["name"]: entry for entry in committed["scenarios"]}
            differing = [
                entry["name"]
                for entry in captured
                if by_name.get(entry["name"], {}).get("observed")
                != json.loads(json.dumps(entry["observed"]))
            ]
            if differing:
                raise rewind.OracleError("a fresh capture differs for " + ", ".join(differing))
            print(f"{len(captured)} scenarios match the committed corpus")
            return 0
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        staged = arguments.output.with_name(f"{arguments.output.name}.{os.getpid()}.tmp")
        staged.write_text(acp.rendered(corpus), encoding="utf-8")
        os.replace(staged, arguments.output)
        return 0
    except (rewind.OracleError, acp.OracleError) as error:
        print(f"project links oracle: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
