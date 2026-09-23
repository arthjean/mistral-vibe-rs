#!/usr/bin/env python3
"""Capture the pinned Python reference's worktree contract over scripted repositories.

Row 5 of the scorecard was once scored from a reading of the reference's
worktree module rather than from a measurement, and a reading cannot tell a
missing guard from an unreachable one. This oracle drives the reference's own
functions and writes down what they answer, so the Rust replay compares a
verdict instead of a claim.

The contract lives in `vibe/core/git/worktree/repository.py` since v2.24.2,
which split the former `vibe/core/worktree.py` into that package and
`vibe/core/git/repo.py`; the error hierarchy is `vibe/core/git/errors.py`, where
`GitError` is the base every refusal below derives from. The capture opens the
repository the way `_enter_worktree` does (`vibe/cli/entrypoint.py:221-246`),
through `WorktreeRepository.open`, and catches `GitError` as that caller does.

These entry points are driven, each over its own family of cases:

``name``
    ``_is_portable_worktree_name`` and ``GitRepo.validate_branch`` over a list
    of names this capture authors, covering every shape the reference rejects
    on portability and every shape ``git check-ref-format --branch`` rejects.
``managedRoot``
    ``_worktree_root`` over synthetic common-git-dir strings, so the replay
    recomputes the twelve-hex naming rule instead of comparing a path that only
    exists on the machine that captured it.
``prepare``
    ``WorktreeRepository.prepare`` over scripted repositories, recording the
    prepared record plus what the call left behind when it failed.
``cleanup``
    ``PreparedWorktree.inspect_for_cleanup`` over a prepared worktree that was
    then dirtied, committed to, or committed to on a detached HEAD.
``list``
    ``WorktreeRepository.linked`` over a repository holding a primary checkout,
    two linked worktrees, a detached one and a prunable record.
``targetCwd``
    ``_target_cwd`` over a synthetic directory tree exercising each of its
    guards.
``slug``, ``suffix`` and ``autoName``
    ``worktree_name_from_text``, ``worktree_name_with_suffix`` and
    ``_auto_worktree_name`` over texts this capture authors; a random name is
    recorded only as random and portable.
``prepareAuto``
    ``WorktreeRepository.prepare_auto`` after a taken branch, a taken
    directory, an earlier automatic worktree and a retained one, recording the
    claim it wrote.
``release``, ``prune`` and ``restore``
    ``ManagedWorktree.release``, ``ManagedWorktree.prune`` and
    ``ManagedWorktree.restore`` over worktrees held, settled, dirtied,
    forgotten, deleted and retained, recording the outcome and what is left on
    disk, in git and in the claim store. Claim times are stamped so the sweep
    sorts by a time the capture chose.
``position``
    ``repository_counterpart``, ``repository_mapped_cwd`` and the branch of the
    checkout a repository is opened at.

Every scripted repository is built under a temporary directory this capture
owns, with ``GIT_CONFIG_GLOBAL``, ``GIT_CONFIG_SYSTEM``, the author identity,
both commit dates and the initial branch fixed, and with ``VIBE_HOME`` pointing
inside that same directory. No developer configuration reaches the corpus and
the user's real vibe home is neither read nor written.

Two artifacts come out of a run:

``.parity/worktree-corpus.json``
    The full capture, gitignored, because an error message is reference-authored
    prose and ``NOTICE`` forbids shipping that.

``crates/vibe-core/tests/worktree/corpus.json``
    The committed corpus, which the Rust replay reads unconditionally. Absolute
    paths are relativized against the case's temporary root, the managed
    directory's twelve-hex segment and the scripted repository's head commit
    become named placeholders, and every reference sentence becomes a
    ``{"described": "sha256:...", "length": n}`` marker. The corpus therefore
    carries names, relative pointers, counts and digests, and no reference
    prose, while a digest still fails the replay on any change.

Usage::

    scripts/parity/worktree.py --reference /path/to/reference --corpus

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it.

The wrapper re-executes itself with the reference interpreter when the current
one cannot import ``vibe``.
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT

SCHEMA_VERSION = 1
DEFAULT_OUTPUT = Path(".parity/worktree-corpus.json")
DEFAULT_CORPUS = Path("crates/vibe-core/tests/worktree/corpus.json")

#: Where the extracted pinned tree is cached between runs. Gitignored, and
#: keyed by commit so a re-pin extracts a new one instead of reusing the old.
DEFAULT_CACHE = Path(".parity")

#: Set on the re-executed process so it does not extract and re-exec forever.
_REEXEC_MARKER = "VIBE_PARITY_PINNED_TREE"

#: The marker a described string carries in place of its content. The key is
#: what makes a described value unmistakable in the corpus and in Rust, where an
#: authored string is a JSON string and a described one is a JSON object.
DESCRIBED = "described"

#: The placeholders the committed corpus carries in place of the three values
#: that change between machines: the temporary root a case was built under, the
#: twelve hex digits the managed directory name ends with, and the commit a
#: scripted repository happens to hash to.
CASE_ROOT = "{root}"
REPO_DIRECTORY = "{repoDir}"
HEAD_COMMIT = "{headCommit}"

#: The directory names a case root holds. `repo` is the primary checkout, which
#: also fixes `repo_root.name` and therefore the managed directory's prefix.
CHECKOUT = "repo"
VIBE_HOME = "home"
LINKED = "linked"
OUTSIDE = "outside"
TREE = "tree"
STATE = "state"

#: One instant, so a scripted repository hashes to the same commit on every run
#: and on every machine.
FIXED_DATE = "2001-02-03T04:05:06+00:00"
FIXED_AUTHOR = "Parity Oracle"
FIXED_EMAIL = "oracle@example.invalid"
DEFAULT_BRANCH = "main"


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
    """The pinned commit, taken from the checkout without depending on its HEAD.

    The checkout only has to *contain* the pin, not sit on it, so a workstation
    that moved HEAD can still capture and is never tempted into a re-pin by
    accident. ``git archive`` reads the commit out below without touching the
    working tree, which is what the read-only constraint asks for.
    """

    if not reference.is_dir():
        raise OracleError(
            f"no reference checkout at {reference}; set VIBE_REFERENCE to the checkout "
            "path or pass --reference"
        )
    try:
        _git(reference, "cat-file", "-e", f"{expected}^{{commit}}")
    except OracleError as error:
        raise OracleError(
            f"{reference} does not contain the pinned commit {expected}: {error}"
        ) from error
    return {"commit": expected, "path": str(reference)}


def extract_pinned_tree(reference: Path, commit: str, cache: Path) -> Path:
    """The pinned source tree, materialized out of tree and reused across runs."""

    import tarfile

    # Absolute, because `_git` runs with the reference as its working directory
    # and a relative output path would resolve inside the read-only checkout.
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
# The hermetic git environment
# --------------------------------------------------------------------------


def install_hermetic_git_environment(root: Path) -> None:
    """Cuts every path from this machine's git and vibe configuration.

    ``GIT_CONFIG_GLOBAL`` and ``GIT_CONFIG_SYSTEM`` point at a file this capture
    writes, so a developer's ``core.hooksPath``, ``init.defaultBranch`` or
    ``commit.gpgsign`` cannot reach a scripted repository. The dates and the
    identity are fixed so a commit hashes to the same object on every run, which
    is what makes ``base_commit`` reproducible.
    """

    configuration = root / "gitconfig"
    configuration.write_text("", encoding="utf-8")
    os.environ.update(
        {
            "GIT_CONFIG_GLOBAL": str(configuration),
            "GIT_CONFIG_SYSTEM": str(configuration),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_AUTHOR_NAME": FIXED_AUTHOR,
            "GIT_AUTHOR_EMAIL": FIXED_EMAIL,
            "GIT_COMMITTER_NAME": FIXED_AUTHOR,
            "GIT_COMMITTER_EMAIL": FIXED_EMAIL,
            "GIT_AUTHOR_DATE": FIXED_DATE,
            "GIT_COMMITTER_DATE": FIXED_DATE,
            "GIT_TERMINAL_PROMPT": "0",
            "GIT_OPTIONAL_LOCKS": "0",
        }
    )
    for inherited in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_CEILING_DIRECTORIES"):
        os.environ.pop(inherited, None)


def run_git(cwd: Path, *arguments: str) -> str:
    result = subprocess.run(
        ["git", *arguments],
        cwd=cwd,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise OracleError(
            f"git {' '.join(arguments)} failed in {cwd}: {result.stderr.strip()}"
        )
    return result.stdout


def write_file(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def commit_all(checkout: Path, message: str) -> None:
    run_git(checkout, "add", "--all")
    run_git(checkout, "commit", "--no-gpg-sign", "--quiet", "-m", message)


def initialize_checkout(checkout: Path, separate_git_dir: Path | None = None) -> None:
    checkout.mkdir(parents=True, exist_ok=True)
    separate = () if separate_git_dir is None else ("--separate-git-dir", str(separate_git_dir))
    run_git(checkout, "init", "--quiet", "--initial-branch", DEFAULT_BRANCH, *separate)
    write_file(checkout / "README.md", "fixture\n")
    write_file(checkout / "docs" / "guide.md", "guide\n")
    commit_all(checkout, "fixture")


def head_commit(checkout: Path) -> str:
    return run_git(checkout, "rev-parse", "HEAD").strip()


def branch_exists(checkout: Path, branch: str) -> bool:
    result = subprocess.run(
        ["git", "show-ref", "--verify", "--quiet", f"refs/heads/{branch}"],
        cwd=checkout,
        capture_output=True,
        text=True,
        check=False,
    )
    return result.returncode == 0


# --------------------------------------------------------------------------
# The scripted repositories
# --------------------------------------------------------------------------

#: Every setup the corpus names, so the Rust replay can assert it implements all
#: of them rather than silently skipping one it never learned about.
SETUPS = (
    "plain",
    "untracked-subdirectory",
    "attached-branch",
    "occupied-target",
    "detached-target",
    "foreign-target",
    "linked-worktrees",
    "separate-git-dir",
    "target-tree",
)


def managed_directory(worktree_module: Any, checkout: Path) -> Path:
    """The managed root for a checkout, from the reference's own naming rule.

    The common git directory is asked of git rather than assumed to be
    `.git` beside the checkout, because a repository created with
    `--separate-git-dir` keeps its state elsewhere and the naming rule hashes
    that location.
    """

    common_git_dir = Path(run_git(checkout, "rev-parse", "--git-common-dir").strip())
    if not common_git_dir.is_absolute():
        common_git_dir = checkout / common_git_dir
    return worktree_module._worktree_root(checkout.resolve(), common_git_dir.resolve())


def build_setup(worktree_module: Any, setup: str, root: Path) -> None:
    """Materializes one scripted case under `root`, which is otherwise empty."""

    checkout = root / CHECKOUT
    (root / VIBE_HOME).mkdir(parents=True, exist_ok=True)

    if setup == "target-tree":
        tree = root / TREE
        (tree / "sub").mkdir(parents=True)
        (tree / "deep" / "inner").mkdir(parents=True)
        (root / OUTSIDE).mkdir(parents=True)
        write_file(tree / "file.txt", "file\n")
        write_file(tree / "nested" / ".git", "gitdir: /nowhere\n")
        (tree / "escape").symlink_to(root / OUTSIDE)
        (tree / "aliased").symlink_to(tree / "sub")
        return

    if setup == "separate-git-dir":
        (root / STATE).mkdir(parents=True, exist_ok=True)
        initialize_checkout(checkout, root / STATE / "repo.git")
        linked = root / LINKED
        linked.mkdir(parents=True, exist_ok=True)
        run_git(checkout, "worktree", "add", "--quiet", "-b", "alpha", str(linked / "alpha"))
        return

    initialize_checkout(checkout)

    if setup == "plain":
        return
    if setup == "untracked-subdirectory":
        (checkout / "scratch").mkdir()
        write_file(checkout / "scratch" / "note.txt", "scratch\n")
        return
    if setup == "attached-branch":
        run_git(checkout, "branch", "review")
        return
    if setup == "occupied-target":
        target = managed_directory(worktree_module, checkout) / "review"
        write_file(target / "note.txt", "occupied\n")
        return
    if setup == "detached-target":
        target = managed_directory(worktree_module, checkout) / "review"
        target.parent.mkdir(parents=True, exist_ok=True)
        run_git(checkout, "worktree", "add", "--quiet", "-b", "review", str(target))
        run_git(target, "checkout", "--quiet", "--detach")
        return
    if setup == "foreign-target":
        target = managed_directory(worktree_module, checkout) / "review"
        initialize_checkout(target)
        run_git(target, "branch", "review")
        run_git(target, "checkout", "--quiet", "review")
        return
    if setup == "linked-worktrees":
        linked = root / LINKED
        linked.mkdir(parents=True, exist_ok=True)
        run_git(checkout, "worktree", "add", "--quiet", "-b", "alpha", str(linked / "alpha"))
        run_git(checkout, "worktree", "add", "--quiet", "-b", "beta", str(linked / "beta"))
        run_git(
            checkout,
            "worktree",
            "add",
            "--quiet",
            "--detach",
            str(linked / "gamma"),
        )
        run_git(checkout, "worktree", "add", "--quiet", "-b", "delta", str(linked / "delta"))
        shutil.rmtree(linked / "delta")
        return
    raise OracleError(f"unknown setup {setup!r}")


@contextlib.contextmanager
def case_root(worktree_module: Any, setup: str, temporary: Path) -> Any:
    """One case's own filesystem, with `VIBE_HOME` pointing inside it."""

    root = Path(tempfile.mkdtemp(prefix="case-", dir=temporary)).resolve()
    previous = os.environ.get("VIBE_HOME")
    os.environ["VIBE_HOME"] = str(root / VIBE_HOME)
    try:
        build_setup(worktree_module, setup, root)
        yield root
    finally:
        if previous is None:
            os.environ.pop("VIBE_HOME", None)
        else:
            os.environ["VIBE_HOME"] = previous
        shutil.rmtree(root, ignore_errors=True)


# --------------------------------------------------------------------------
# Projection
# --------------------------------------------------------------------------


def digest(value: str) -> str:
    return "sha256:" + hashlib.sha256(value.encode("utf-8")).hexdigest()[:32]


def describe(value: str) -> dict[str, Any]:
    """A reference sentence, reduced to something `NOTICE` allows shipping.

    The length is the normalized sentence's, not the raw one's, for the same
    reason the digest is: a raw message carries the temporary path the case ran
    under, and neither the digest nor the count may depend on it.
    """

    return {DESCRIBED: digest(value), "length": len(value)}


class Projection:
    """Rewrites one case's absolute paths into machine-independent pointers.

    Three substitutions make a record portable: the case root becomes the empty
    prefix, the managed directory's twelve-hex segment becomes `{repoDir}`, and
    the scripted repository's head commit becomes `{headCommit}`. Everything
    else a record carries is a name this capture authored.
    """

    def __init__(self, root: Path, repo_directory: str | None, commit: str | None) -> None:
        self.root = root
        self.repo_directory = repo_directory
        self.commit = commit

    def path(self, value: Path | str) -> str:
        text = str(value)
        prefix = str(self.root)
        if text == prefix:
            relative = "."
        elif text.startswith(prefix + os.sep):
            relative = text[len(prefix) + 1 :]
        else:
            raise OracleError(f"path {text} escapes the case root {prefix}")
        relative = relative.replace(os.sep, "/")
        if self.repo_directory:
            relative = relative.replace(self.repo_directory, REPO_DIRECTORY)
        return relative

    def commit_value(self, value: str) -> str:
        return HEAD_COMMIT if value == self.commit else value

    def text(self, value: str) -> str:
        """The same three substitutions, applied inside a sentence.

        A reference message names the paths it failed on, so digesting it raw
        would make the corpus differ between two runs of the same capture. The
        normalization happens before the digest and never after, so the sentence
        itself still goes nowhere near the committed corpus.
        """

        normalized = value.replace(str(self.root), CASE_ROOT)
        if self.repo_directory:
            normalized = normalized.replace(self.repo_directory, REPO_DIRECTORY)
        if self.commit:
            normalized = normalized.replace(self.commit, HEAD_COMMIT)
        return normalized


def prepared_record(prepared: Any, projection: Projection) -> dict[str, Any]:
    return {
        "name": prepared.name,
        "branch": prepared.branch,
        "root": projection.path(prepared.root),
        "path": projection.path(prepared.path),
        "repoRoot": projection.path(prepared.repo_root),
        "baseCommit": projection.commit_value(prepared.base_commit),
        "created": prepared.created,
        "branchCreated": prepared.branch_created,
    }


def error_record(error: BaseException, projection: Projection) -> dict[str, Any]:
    return {
        "outcome": "error",
        "errorClass": type(error).__name__,
        "message": describe(projection.text(str(error))),
    }


# --------------------------------------------------------------------------
# The families
# --------------------------------------------------------------------------

#: The names this capture authors. The list covers every shape the reference
#: rejects on portability, every shape `git check-ref-format --branch` rejects,
#: and enough accepted shapes that a port rejecting everything cannot pass.
AUTHORED_NAMES: tuple[tuple[str, str], ...] = (
    ("plain", "review"),
    ("hyphenated", "feature-1"),
    ("underscored", "under_score"),
    ("accented", "très"),
    ("emoji", "🌱"),
    ("digit-suffixed-device", "com0"),
    ("beyond-device-range", "lpt10"),
    ("dot-prefixed", ".hidden"),
    ("inner-space", "a b"),
    ("option-shaped", "-x"),
    ("double-dot", "foo..bar"),
    ("lock-suffixed", "foo.lock"),
    ("head", "HEAD"),
    ("device-aux", "aux"),
    ("device-aux-upper", "AUX"),
    ("device-aux-extension", "aux.txt"),
    ("device-con", "con"),
    ("device-nul", "nul"),
    ("device-nul-extension", "nul.txt"),
    ("device-com1", "com1"),
    ("device-lpt9", "lpt9"),
    ("device-conin", "conin$"),
    ("device-clock", "clock$"),
    ("trailing-dot", "foo."),
    ("trailing-space", "foo "),
    ("angle-bracket", "a<b"),
    ("pipe", "a|b"),
    ("quote", 'a"b'),
    ("question-mark", "a?b"),
    ("star", "a*b"),
    ("colon", "a:b"),
    ("tab", "a\tb"),
    ("empty", ""),
    ("dot", "."),
    ("double-dot-alone", ".."),
    ("posix-separator", "a/b"),
    ("windows-separator", "a\\b"),
    ("drive-prefixed", "C:name"),
)

#: Synthetic common-git-dir strings for the managed-root naming rule. The
#: replay hashes the same strings, so it recomputes the rule rather than
#: comparing a directory that only existed on the capturing machine.
SYNTHETIC_GIT_DIRS: tuple[tuple[str, str, str], ...] = (
    ("posix", "/synthetic/repo", "/synthetic/repo/.git"),
    ("sibling", "/synthetic/other", "/synthetic/other/.git"),
    ("separate-git-dir", "/synthetic/repo", "/synthetic/state/repo.git"),
    ("windows-shaped", "/synthetic/repo", "C:\\dev\\repo\\.git"),
    ("accented", "/synthetic/dépôt", "/synthetic/dépôt/.git"),
    ("nested", "/synthetic/a/b/c/repo", "/synthetic/a/b/c/repo/.git"),
)

#: One preparation case per row: the setup it needs, the name and branch it asks
#: for, and the directory inside the case root it calls preparation from.
PREPARE_CASES: tuple[dict[str, Any], ...] = (
    {"case": "fresh-branch", "setup": "plain", "name": "review", "base": CHECKOUT},
    {"case": "reuse", "setup": "plain", "name": "review", "base": CHECKOUT, "twice": True},
    {"case": "attached-branch", "setup": "attached-branch", "name": "review", "base": CHECKOUT},
    {
        "case": "distinct-branch",
        "setup": "plain",
        "name": "review",
        "branch": "feature/x",
        "base": CHECKOUT,
    },
    {
        "case": "subdirectory-base",
        "setup": "plain",
        "name": "review",
        "base": f"{CHECKOUT}/docs",
    },
    {
        "case": "missing-base",
        "setup": "untracked-subdirectory",
        "name": "review",
        "base": f"{CHECKOUT}/scratch",
    },
    {"case": "outside-repository", "setup": "plain", "name": "review", "base": "."},
    {"case": "unportable-name", "setup": "plain", "name": "aux", "base": CHECKOUT},
    {
        "case": "invalid-branch",
        "setup": "plain",
        "name": "review",
        "branch": "foo.lock",
        "base": CHECKOUT,
    },
    {"case": "occupied-target", "setup": "occupied-target", "name": "review", "base": CHECKOUT},
    {"case": "detached-target", "setup": "detached-target", "name": "review", "base": CHECKOUT},
    {"case": "foreign-target", "setup": "foreign-target", "name": "review", "base": CHECKOUT},
    {
        "case": "linked-base",
        "setup": "linked-worktrees",
        "name": "review",
        "base": f"{LINKED}/alpha",
    },
    {
        "case": "separate-git-dir-base",
        "setup": "separate-git-dir",
        "name": "review",
        "base": CHECKOUT,
    },
    {
        "case": "separate-git-dir-linked-base",
        "setup": "separate-git-dir",
        "name": "review",
        "base": f"{LINKED}/alpha",
    },
)

#: One cleanup case per row: what happens inside the prepared worktree before
#: `PreparedWorktree.inspect_for_cleanup` is asked what it sees.
CLEANUP_CASES: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("clean", ()),
    ("uncommitted", ("modify",)),
    ("untracked", ("add",)),
    ("uncommitted-and-untracked", ("modify", "add")),
    ("one-commit", ("modify", "commit")),
    ("two-commits", ("modify", "commit", "modify", "commit")),
    ("detached-commit", ("detach", "modify", "commit")),
)

#: One target-directory case per row: the relative base handed to `_target_cwd`
#: against the synthetic tree `target-tree` builds.
TARGET_CWD_CASES: tuple[tuple[str, str], ...] = (
    ("root", "."),
    ("subdirectory", "sub"),
    ("nested-subdirectory", "deep/inner"),
    ("missing", "missing"),
    ("regular-file", "file.txt"),
    ("escaping-symlink", "escape"),
    ("aliased-component", "aliased"),
    ("nested-git", "nested"),
)

#: One enumeration case per row: the setup and the directory the caller is in.
LIST_CASES: tuple[tuple[str, str, str], ...] = (
    ("none", "plain", CHECKOUT),
    ("several", "linked-worktrees", CHECKOUT),
    ("subdirectory-base", "linked-worktrees", f"{CHECKOUT}/docs"),
    ("not-a-repository", "target-tree", "."),
)


def capture_names(worktree_module: Any) -> list[dict[str, Any]]:
    """Both name verdicts, taken from the reference's own two functions.

    `GitRepo.validate_branch` wants a repository because it runs
    `git check-ref-format` through it, so one scripted checkout serves the whole
    list.
    """

    from vibe.core.git.errors import GitError
    from vibe.core.git.repo import GitRepo

    records: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory(prefix="vibe-worktree-names-") as temporary:
        checkout = Path(temporary).resolve() / CHECKOUT
        initialize_checkout(checkout)
        with GitRepo.open(checkout) as repo:
            for case, name in AUTHORED_NAMES:
                observed: dict[str, Any] = {
                    "portable": worktree_module._is_portable_worktree_name(name)
                }
                try:
                    repo.validate_branch(name)
                except GitError:
                    observed["branchValid"] = False
                else:
                    observed["branchValid"] = True
                records.append(
                    {
                        "family": "name",
                        "case": case,
                        "input": {"name": name},
                        "observed": observed,
                    }
                )
    return records


def capture_managed_roots(worktree_module: Any) -> list[dict[str, Any]]:
    """The managed-root naming rule, over inputs no filesystem has to hold."""

    records: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory(prefix="vibe-worktree-roots-") as temporary:
        home = Path(temporary).resolve() / VIBE_HOME
        os.environ["VIBE_HOME"] = str(home)
        managed = (home / "worktrees").resolve()
        for case, repo_root, common_git_dir in SYNTHETIC_GIT_DIRS:
            resolved = worktree_module._worktree_root(
                Path(repo_root), Path(common_git_dir)
            )
            relative = resolved.relative_to(managed)
            records.append(
                {
                    "family": "managedRoot",
                    "case": case,
                    "input": {"repoRoot": repo_root, "commonGitDir": common_git_dir},
                    "observed": {
                        "repoRootName": Path(repo_root).name,
                        "directory": str(relative),
                    },
                }
            )
        os.environ.pop("VIBE_HOME", None)
    return records


def prepare(
    worktree_module: Any, name: str, base: Path, branch: str | None = None
) -> Any:
    """One named preparation, opened the way `_enter_worktree` opens it.

    A reused worktree comes back holding a pending attachment claim, which the
    reference's own callers release once a session holds it or startup is
    abandoned; the capture holds no session, so it releases the claim at once.
    """

    with worktree_module.WorktreeRepository.open(base) as repository:
        prepared = repository.prepare(name, branch=branch)
    if prepared.pending_hold is not None:
        prepared.pending_hold.release()
    return prepared


def capture_prepare(worktree_module: Any, temporary: Path) -> list[dict[str, Any]]:
    from vibe.core.git.errors import GitError

    records: list[dict[str, Any]] = []
    for specification in PREPARE_CASES:
        setup = specification["setup"]
        name = specification["name"]
        branch = specification.get("branch")
        with case_root(worktree_module, setup, temporary) as root:
            checkout = root / CHECKOUT
            projection = Projection(
                root,
                managed_directory(worktree_module, checkout).name,
                head_commit(checkout),
            )
            base = root if specification["base"] == "." else root / specification["base"]
            observed: dict[str, Any]
            try:
                prepared = prepare(worktree_module, name, base, branch)
                if specification.get("twice"):
                    prepared = prepare(worktree_module, name, base, branch)
                observed = {
                    "outcome": "prepared",
                    "prepared": prepared_record(prepared, projection),
                }
            except GitError as error:
                observed = error_record(error, projection)
            target = managed_directory(worktree_module, checkout) / name
            observed["residue"] = {
                "target": target.exists(),
                "branch": branch_exists(checkout, branch or name),
            }
            records.append(
                {
                    "family": "prepare",
                    "case": specification["case"],
                    "setup": setup,
                    "input": {
                        "name": name,
                        "branch": branch,
                        "base": specification["base"],
                        "twice": bool(specification.get("twice")),
                    },
                    "observed": observed,
                }
            )
    return records


def apply_mutation(worktree: Path, step: str) -> None:
    if step == "modify":
        existing = (worktree / "README.md").read_text(encoding="utf-8")
        write_file(worktree / "README.md", existing + "edit\n")
    elif step == "add":
        write_file(worktree / "note.txt", "note\n")
    elif step == "commit":
        commit_all(worktree, "session")
    elif step == "detach":
        run_git(worktree, "checkout", "--quiet", "--detach")
    else:
        raise OracleError(f"unknown mutation {step!r}")


def capture_cleanup(worktree_module: Any, temporary: Path) -> list[dict[str, Any]]:
    from vibe.core.git.errors import GitError

    records: list[dict[str, Any]] = []
    for case, mutations in CLEANUP_CASES:
        with case_root(worktree_module, "plain", temporary) as root:
            checkout = root / CHECKOUT
            projection = Projection(
                root,
                managed_directory(worktree_module, checkout).name,
                head_commit(checkout),
            )
            prepared = prepare(worktree_module, "review", checkout)
            for step in mutations:
                apply_mutation(prepared.root, step)
            observed: dict[str, Any]
            try:
                state = prepared.inspect_for_cleanup()
                observed = {
                    "outcome": "inspected",
                    "hasUncommittedChanges": state.has_uncommitted_changes,
                    "hasUntrackedFiles": state.has_untracked_files,
                    "newCommitCount": state.new_commit_count,
                    "isClean": state.is_clean,
                    "reasons": [describe(reason) for reason in state.reasons],
                }
            except GitError as error:
                observed = error_record(error, projection)
            records.append(
                {
                    "family": "cleanup",
                    "case": case,
                    "setup": "plain",
                    "input": {"name": "review", "mutations": list(mutations)},
                    "observed": observed,
                }
            )
    return records


def capture_list(worktree_module: Any, temporary: Path) -> list[dict[str, Any]]:
    from vibe.core.git.errors import GitError

    records: list[dict[str, Any]] = []
    for case, setup, base in LIST_CASES:
        with case_root(worktree_module, setup, temporary) as root:
            projection = Projection(root, None, None)
            observed: dict[str, Any]
            try:
                with worktree_module.WorktreeRepository.open(
                    root if base == "." else root / base
                ) as repository:
                    linked = repository.linked()
                observed = {
                    "outcome": "listed",
                    "worktrees": [
                        {
                            "name": worktree.name,
                            "branch": worktree.branch,
                            "root": projection.path(worktree.root),
                            "path": projection.path(worktree.path),
                            "repoRoot": projection.path(worktree.repo_root),
                        }
                        for worktree in linked
                    ],
                }
            except GitError as error:
                observed = error_record(error, projection)
            records.append(
                {
                    "family": "list",
                    "case": case,
                    "setup": setup,
                    "input": {"base": base},
                    "observed": observed,
                }
            )
    return records


def capture_target_cwd(worktree_module: Any, temporary: Path) -> list[dict[str, Any]]:
    from vibe.core.git.errors import GitError

    records: list[dict[str, Any]] = []
    for case, relative_base in TARGET_CWD_CASES:
        with case_root(worktree_module, "target-tree", temporary) as root:
            projection = Projection(root, None, None)
            observed: dict[str, Any]
            try:
                resolved = worktree_module._target_cwd(
                    root / TREE, Path(relative_base)
                )
                observed = {"outcome": "resolved", "path": projection.path(resolved)}
            except GitError as error:
                observed = error_record(error, projection)
            records.append(
                {
                    "family": "targetCwd",
                    "case": case,
                    "setup": "target-tree",
                    "input": {"root": TREE, "relativeBase": relative_base},
                    "observed": observed,
                }
            )
    return records


#: Texts the slug rule is driven over: stop words at the end, the word and
#: length limits, compatibility characters NFKD folds, and texts that reduce to
#: nothing or to a reserved device name.
SLUG_TEXTS: tuple[tuple[str, str], ...] = (
    ("sentence", "Fix the login redirect bug"),
    ("trailing-stop-words", "Update the docs for the"),
    ("only-stop-words", "the and of"),
    ("accented", "Crème brûlée recipe"),
    ("ligature", "ﬁle ﬂow"),
    ("full-width", "ＡＢＣ ｄｅｆ"),
    ("emoji", "Grow 🌱 garden"),
    ("seven-words", "one two three four five six seven"),
    ("long-words", "internationalization localization accessibility"),
    ("boundary-hyphen", "abcdefghij abcdefghij abcdefghij abcdefghi xyz"),
    ("single-long-word", "a" * 45),
    ("punctuation-only", "!!! ???"),
    ("empty", ""),
    ("device-name", "NUL"),
    ("digits", "Issue 1234 follow up"),
    ("mixed-separators", "snake_case/kebab-case.dotted"),
)

#: Names and suffixes the suffix rule is driven over.
SUFFIX_CASES: tuple[tuple[str, str, int], ...] = (
    ("short", "review", 2),
    ("two-digit", "review", 12),
    ("at-limit", "abcdefghij-abcdefghij-abcdefghij-abcdefgh", 3),
    ("stop-word-exposed", "fix-the-login-redirect-for-the-admin-panel", 2),
    ("single-long-word", "a" * 40, 5),
)

#: Prompt and suggestion pairs the auto-naming walk is driven over.
AUTO_NAME_CASES: tuple[tuple[str, str | None, str | None], ...] = (
    ("prompt-only", "Fix the login redirect", None),
    ("suggestion-wins", "Fix the login redirect", "Login redirect fix"),
    ("unusable-suggestion", "Fix the login redirect", "!!!"),
    ("device-suggestion", "Fix the login redirect", "nul"),
    ("suggestion-only", None, "Tidy imports"),
    ("unusable-both", "???", "the"),
    ("neither", None, None),
)


def capture_slugs(worktree_module: Any) -> list[dict[str, Any]]:
    from vibe.core.git.worktree import naming

    records: list[dict[str, Any]] = []
    for case, text in SLUG_TEXTS:
        name = naming.worktree_name_from_text(text)
        records.append(
            {
                "family": "slug",
                "case": case,
                "input": {"text": text},
                "observed": {
                    "name": name,
                    "portable": worktree_module._is_portable_worktree_name(name),
                },
            }
        )
    for case, name, suffix in SUFFIX_CASES:
        records.append(
            {
                "family": "suffix",
                "case": case,
                "input": {"name": name, "suffix": suffix},
                "observed": {"name": naming.worktree_name_with_suffix(name, suffix)},
            }
        )
    return records


def capture_auto_names(worktree_module: Any) -> list[dict[str, Any]]:
    """Where an automatic name comes from, and the name when it is not random.

    A random slug differs on every call, so the record keeps only that it was
    random and portable; the replay checks the same two facts.
    """

    from vibe.core.git.worktree import naming

    records: list[dict[str, Any]] = []
    for case, prompt, suggested in AUTO_NAME_CASES:
        name = worktree_module._auto_worktree_name(prompt, suggested)
        source = "random"
        for label, text in (("suggested", suggested), ("prompt", prompt)):
            if text is not None and naming.worktree_name_from_text(text) == name:
                source = label
                break
        observed: dict[str, Any] = {
            "source": source,
            "portable": worktree_module._is_portable_worktree_name(name),
        }
        if source != "random":
            observed["name"] = name
        records.append(
            {
                "family": "autoName",
                "case": case,
                "input": {"prompt": prompt, "suggested": suggested},
                "observed": observed,
            }
        )
    return records


#: The claim time each lifecycle case stamps, in order of preparation, so the
#: retention sweep sorts by a time this capture chose rather than by the clock.
CLAIMED_AT = (
    "2001-02-03T04:05:06+00:00",
    "2001-02-03T04:05:07+00:00",
    "2001-02-03T04:05:08+00:00",
)


def claim_of(worktree_module: Any, path: Path) -> Any:
    managed = worktree_module.ManagedWorktree.at(path)
    if managed is None:
        raise OracleError(f"{path} is not a managed worktree")
    return managed


def stamp_claim(worktree_module: Any, path: Path, claimed_at: str) -> None:
    from datetime import datetime

    claim = claim_of(worktree_module, path).claim
    record = claim.read()
    claim.write(record.model_copy(update={"claimed_at": datetime.fromisoformat(claimed_at)}))


def settle(worktree_module: Any, prepared: Any, holder: str = "finished") -> None:
    """Holds and lets go of a prepared worktree, as a session that ran and ended."""

    managed = claim_of(worktree_module, prepared.path)
    managed.hold(holder, prepared.pending_hold)
    managed.release_holder(holder)


def claim_record(worktree_module: Any, prepared: Any, projection: Projection) -> dict[str, Any]:
    managed = claim_of(worktree_module, prepared.path)
    record = managed.claim.read()
    return {
        "record": None
        if record is None
        else {
            "version": record.version,
            "name": record.name,
            "branch": record.branch,
            "repoRoot": projection.path(record.repo_root),
            "baseCommit": None
            if record.base_commit is None
            else projection.commit_value(record.base_commit),
            "branchCreated": record.branch_created,
        },
        "starting": managed.claim.is_starting(),
        "holders": len(managed.holders()),
    }


#: One automatic preparation per row: what the case does to the repository
#: first, and the prompt and suggestion it prepares with.
PREPARE_AUTO_CASES: tuple[dict[str, Any], ...] = (
    {"case": "from-prompt", "prompt": "Fix the login redirect", "suggested": None},
    {"case": "from-suggestion", "prompt": "anything", "suggested": "Login redirect fix"},
    {"case": "branch-taken", "prompt": "review", "before": ("branch vibe/review",)},
    {"case": "directory-taken", "prompt": "review", "before": ("occupy review",)},
    {"case": "never-reused", "prompt": "review", "before": ("prepare-auto review",)},
    {"case": "retained-name", "prompt": "review", "before": ("retain review",)},
)


def apply_before(worktree_module: Any, root: Path, step: str) -> None:
    checkout = root / CHECKOUT
    verb, _, argument = step.partition(" ")
    if verb == "branch":
        run_git(checkout, "branch", argument)
    elif verb == "occupy":
        write_file(managed_directory(worktree_module, checkout) / argument / "note.txt", "x\n")
    elif verb == "prepare-auto":
        with worktree_module.WorktreeRepository.open(checkout) as repository:
            prepared = repository.prepare_auto(prompt=argument)
        settle(worktree_module, prepared)
    elif verb == "retain":
        with worktree_module.WorktreeRepository.open(checkout) as repository:
            prepared = repository.prepare_auto(prompt=argument)
        settle(worktree_module, prepared)
        worktree_module.ManagedWorktree.prune(0)
    else:
        raise OracleError(f"unknown step {step!r}")


def capture_prepare_auto(worktree_module: Any, temporary: Path) -> list[dict[str, Any]]:
    from vibe.core.git.errors import GitError

    records: list[dict[str, Any]] = []
    for specification in PREPARE_AUTO_CASES:
        with case_root(worktree_module, "plain", temporary) as root:
            checkout = root / CHECKOUT
            projection = Projection(
                root,
                managed_directory(worktree_module, checkout).name,
                head_commit(checkout),
            )
            for step in specification.get("before", ()):
                apply_before(worktree_module, root, step)
            observed: dict[str, Any]
            try:
                with worktree_module.WorktreeRepository.open(checkout) as repository:
                    prepared = repository.prepare_auto(
                        prompt=specification["prompt"],
                        suggested_name=specification.get("suggested"),
                    )
                observed = {
                    "outcome": "prepared",
                    "prepared": prepared_record(prepared, projection),
                    "pendingHold": prepared.pending_hold is not None,
                    **claim_record(worktree_module, prepared, projection),
                }
            except GitError as error:
                observed = error_record(error, projection)
            records.append(
                {
                    "family": "prepareAuto",
                    "case": specification["case"],
                    "setup": "plain",
                    "input": {
                        "prompt": specification["prompt"],
                        "suggested": specification.get("suggested"),
                        "before": list(specification.get("before", ())),
                    },
                    "observed": observed,
                }
            )
    return records


#: One release per row: the steps that lead up to it, then who releases.
RELEASE_CASES: tuple[dict[str, Any], ...] = (
    {"case": "clean", "steps": ("hold s1",), "session": "s1"},
    {"case": "dirty", "steps": ("hold s1", "dirty"), "session": "s1"},
    {"case": "committed", "steps": ("hold s1", "commit"), "session": "s1"},
    {"case": "held-by-another", "steps": ("hold s1", "hold s2"), "session": "s1"},
    {"case": "still-starting", "steps": (), "session": None},
    {"case": "never-held", "steps": ("settle",), "session": None},
    {"case": "record-forgotten", "steps": ("settle", "forget"), "session": None},
    {"case": "directory-gone", "steps": ("settle", "delete-directory"), "session": None},
    {"case": "attached-branch", "steps": ("hold s1",), "session": "s1", "attached": True},
    {"case": "retained", "steps": ("settle", "dirty", "prune"), "session": None},
    {"case": "retained-with-session", "steps": ("settle", "prune"), "session": "s1"},
)


def apply_lifecycle_step(worktree_module: Any, prepared: Any, step: str) -> None:
    verb, _, argument = step.partition(" ")
    if verb == "hold":
        claim_of(worktree_module, prepared.path).hold(argument, prepared.pending_hold)
    elif verb == "settle":
        settle(worktree_module, prepared)
    elif verb == "dirty":
        write_file(prepared.root / "note.txt", "note\n")
    elif verb == "commit":
        apply_mutation(prepared.root, "modify")
        apply_mutation(prepared.root, "commit")
    elif verb == "forget":
        claim_of(worktree_module, prepared.path).forget()
    elif verb == "delete-directory":
        shutil.rmtree(prepared.root)
    elif verb == "prune":
        worktree_module.ManagedWorktree.prune(0)
    else:
        raise OracleError(f"unknown step {step!r}")


def ref_exists(checkout: Path, ref: str) -> bool:
    result = subprocess.run(
        ["git", "-C", str(checkout), "show-ref", "--verify", "--quiet", ref],
        capture_output=True,
        check=False,
    )
    return result.returncode == 0


def capture_release(worktree_module: Any, temporary: Path) -> list[dict[str, Any]]:
    from vibe.core.git.errors import GitError

    records: list[dict[str, Any]] = []
    for specification in RELEASE_CASES:
        setup = "attached-branch" if specification.get("attached") else "plain"
        with case_root(worktree_module, setup, temporary) as root:
            checkout = root / CHECKOUT
            projection = Projection(
                root,
                managed_directory(worktree_module, checkout).name,
                head_commit(checkout),
            )
            with worktree_module.WorktreeRepository.open(checkout) as repository:
                prepared = repository.prepare("review")
            for step in specification["steps"]:
                apply_lifecycle_step(worktree_module, prepared, step)
            managed = worktree_module.ManagedWorktree.at(prepared.path)
            snapshot_ref = f"{worktree_module.SNAPSHOT_REF_PREFIX}/review"
            observed: dict[str, Any]
            try:
                release = managed.release(specification["session"])
                observed = {
                    "outcome": release.outcome.value,
                    "root": None if release.root is None else projection.path(release.root),
                    "branch": release.branch,
                    "branchDeleted": release.branch_deleted,
                    "reasons": [describe(reason) for reason in release.reasons],
                    "snapshotRef": release.snapshot_ref,
                }
            except GitError as error:
                observed = error_record(error, projection)
            observed["residue"] = {
                "directory": prepared.root.exists(),
                "branch": branch_exists(checkout, "review"),
                "record": managed.claim.read() is not None,
                "recovery": managed.claim.has_recovery(),
                "snapshot": ref_exists(checkout, snapshot_ref),
            }
            records.append(
                {
                    "family": "release",
                    "case": specification["case"],
                    "setup": setup,
                    "input": {
                        "steps": list(specification["steps"]),
                        "session": specification["session"],
                    },
                    "observed": observed,
                }
            )
    return records


#: One sweep per row: the worktrees prepared in claim order, the steps applied
#: to each by name, and the limit the sweep runs under.
PRUNE_CASES: tuple[dict[str, Any], ...] = (
    {"case": "under-limit", "names": ("b", "a", "c"), "limit": 3, "steps": ()},
    {"case": "oldest-first", "names": ("b", "a", "c"), "limit": 1, "steps": ()},
    {"case": "held-skipped", "names": ("b", "a", "c"), "limit": 1, "steps": ("hold b",)},
    {"case": "starting-skipped", "names": ("b", "a"), "limit": 0, "steps": ("unsettled a",)},
    {"case": "dirty-saved", "names": ("b", "a"), "limit": 0, "steps": ("dirty b",)},
    {"case": "directory-gone", "names": ("b", "a"), "limit": 1, "steps": ("delete-directory b",)},
)


def capture_prune(worktree_module: Any, temporary: Path) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    for specification in PRUNE_CASES:
        with case_root(worktree_module, "plain", temporary) as root:
            checkout = root / CHECKOUT
            prepared_by_name: dict[str, Any] = {}
            unsettled = {
                step.split(" ", 1)[1]
                for step in specification["steps"]
                if step.startswith("unsettled ")
            }
            for index, name in enumerate(specification["names"]):
                with worktree_module.WorktreeRepository.open(checkout) as repository:
                    prepared = repository.prepare(name)
                stamp_claim(worktree_module, prepared.path, CLAIMED_AT[index])
                if name not in unsettled:
                    settle(worktree_module, prepared)
                prepared_by_name[name] = prepared
            for step in specification["steps"]:
                verb, _, name = step.partition(" ")
                if verb == "unsettled":
                    continue
                apply_lifecycle_step(worktree_module, prepared_by_name[name], f"{verb} held")
            removed = worktree_module.ManagedWorktree.prune(specification["limit"])
            observed = {
                "removed": removed,
                "remaining": sorted(
                    name for name, prepared in prepared_by_name.items() if prepared.root.exists()
                ),
                "retained": sorted(
                    name
                    for name, prepared in prepared_by_name.items()
                    if worktree_module.ManagedWorktree.at(prepared.path) is not None
                    and worktree_module.ManagedWorktree.at(prepared.path).claim.has_recovery()
                ),
                "snapshots": sorted(
                    name
                    for name in prepared_by_name
                    if ref_exists(checkout, f"{worktree_module.SNAPSHOT_REF_PREFIX}/{name}")
                ),
                "branches": sorted(
                    name for name in prepared_by_name if branch_exists(checkout, name)
                ),
            }
            records.append(
                {
                    "family": "prune",
                    "case": specification["case"],
                    "setup": "plain",
                    "input": {
                        "names": list(specification["names"]),
                        "limit": specification["limit"],
                        "steps": list(specification["steps"]),
                    },
                    "observed": observed,
                }
            )
    return records


#: One restore per row: what happened to the worktree first, and where inside
#: it (or outside it) the restore is asked for.
RESTORE_CASES: tuple[tuple[str, tuple[str, ...], str], ...] = (
    ("retained", ("settle", "dirty", "prune"), "."),
    ("retained-committed", ("settle", "commit", "prune"), "."),
    ("not-retained", ("settle",), "."),
    ("outside", ("settle", "prune"), ".."),
)


def capture_restore(worktree_module: Any, temporary: Path) -> list[dict[str, Any]]:
    from vibe.core.git.errors import GitError

    records: list[dict[str, Any]] = []
    for case, steps, relative in RESTORE_CASES:
        with case_root(worktree_module, "plain", temporary) as root:
            checkout = root / CHECKOUT
            projection = Projection(
                root,
                managed_directory(worktree_module, checkout).name,
                head_commit(checkout),
            )
            with worktree_module.WorktreeRepository.open(checkout) as repository:
                prepared = repository.prepare("review")
            for step in steps:
                apply_lifecycle_step(worktree_module, prepared, step)
            managed = worktree_module.ManagedWorktree.at(prepared.path)
            requested = prepared.root if relative == "." else prepared.root / relative
            mapping = managed.retained_repository_mapping(prepared.root)
            observed: dict[str, Any] = {
                "mapping": None
                if mapping is None
                else {
                    "root": projection.path(mapping.root),
                    "cwd": projection.path(mapping.cwd),
                }
            }
            try:
                observed["outcome"] = "restored" if managed.restore(requested) else "unchanged"
            except GitError as error:
                observed.update(error_record(error, projection))
            readme = prepared.root / "README.md"
            observed["residue"] = {
                "directory": prepared.root.is_dir(),
                "branch": branch_exists(checkout, "review"),
                "record": managed.claim.read() is not None,
                "recovery": managed.claim.has_recovery(),
                "note": (prepared.root / "note.txt").is_file(),
                "readme": None
                if not readme.is_file()
                else ("edited" if readme.read_text(encoding="utf-8").endswith("edit\n") else "original"),
                "starting": managed.claim.is_starting(),
            }
            records.append(
                {
                    "family": "restore",
                    "case": case,
                    "setup": "plain",
                    "input": {"steps": list(steps), "relative": relative},
                    "observed": observed,
                }
            )
    return records


#: One position per row: the setup, and the directory the repository is opened
#: at, relative to the case root.
POSITION_CASES: tuple[tuple[str, str, str], ...] = (
    ("checkout", "linked-worktrees", CHECKOUT),
    ("linked", "linked-worktrees", f"{LINKED}/alpha"),
    ("linked-only-directory", "linked-worktrees", f"{LINKED}/alpha/only-here"),
    ("detached", "linked-worktrees", f"{LINKED}/gamma"),
    ("not-a-repository", "target-tree", "."),
)


def capture_positions(worktree_module: Any, temporary: Path) -> list[dict[str, Any]]:
    from vibe.core.git.errors import GitError

    records: list[dict[str, Any]] = []
    for case, setup, base in POSITION_CASES:
        with case_root(worktree_module, setup, temporary) as root:
            projection = Projection(root, None, None)
            directory = root if base == "." else root / base
            directory.mkdir(parents=True, exist_ok=True)
            observed: dict[str, Any]
            try:
                with worktree_module.WorktreeRepository.open(directory) as repository:
                    counterpart = repository.repository_counterpart
                    observed = {
                        "outcome": "opened",
                        "root": projection.path(repository.root),
                        "counterpart": None
                        if counterpart is None
                        else projection.path(counterpart),
                        "mappedCwd": projection.path(repository.repository_mapped_cwd),
                    }
                with worktree_module.GitRepo.open(directory) as git_repository:
                    observed["branch"] = git_repository.branch()
            except GitError as error:
                observed = error_record(error, projection)
            records.append(
                {
                    "family": "position",
                    "case": case,
                    "setup": setup,
                    "input": {"base": base},
                    "observed": observed,
                }
            )
    return records


def capture(pinned: Path) -> list[dict[str, Any]]:
    # The module rather than the package: the package re-exports only public
    # names, and the capture also drives `_worktree_root`, `_target_cwd` and
    # `_is_portable_worktree_name`.
    from vibe.core.git.worktree import repository as worktree_module

    module_path = Path(worktree_module.__file__).resolve()
    if not module_path.is_relative_to(pinned.resolve()):
        raise OracleError(
            f"`vibe.core.git.worktree.repository` was imported from {module_path}, "
            f"not {pinned}"
        )

    with tempfile.TemporaryDirectory(prefix="vibe-worktree-") as raw:
        temporary = Path(raw).resolve()
        install_hermetic_git_environment(temporary)
        return [
            *capture_names(worktree_module),
            *capture_managed_roots(worktree_module),
            *capture_prepare(worktree_module, temporary),
            *capture_cleanup(worktree_module, temporary),
            *capture_list(worktree_module, temporary),
            *capture_target_cwd(worktree_module, temporary),
            *capture_slugs(worktree_module),
            *capture_auto_names(worktree_module),
            *capture_prepare_auto(worktree_module, temporary),
            *capture_release(worktree_module, temporary),
            *capture_prune(worktree_module, temporary),
            *capture_restore(worktree_module, temporary),
            *capture_positions(worktree_module, temporary),
        ]


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def build_corpus(records: list[dict[str, Any]], reference: dict[str, str]) -> dict[str, Any]:
    return {
        "schemaVersion": SCHEMA_VERSION,
        "referenceCommit": reference["commit"],
        "platform": platform.system().lower(),
        "setups": list(SETUPS),
        "note": (
            "Worktree corpus: what the pinned reference's own worktree functions answer over "
            "repositories this capture scripts under a hermetic git environment. Paths are "
            "relative to the case root, the managed directory's hashed segment is "
            f"{REPO_DIRECTORY} and the scripted head commit is {HEAD_COMMIT}, so the corpus is "
            "machine-independent. A reference sentence is committed as a {described, length} "
            "marker carrying a SHA-256 digest, so no reference prose ships while any change "
            "still fails the replay. Regenerate with scripts/parity/worktree.py --corpus when "
            "the pinned reference moves."
        ),
        "cases": records,
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
            "also write the committed corpus, which the Rust replay reads unconditionally "
            f"(default {DEFAULT_CORPUS})"
        ),
    )
    parser.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    parser.add_argument("--expected-commit", default=EXPECTED_COMMIT)
    parser.add_argument(
        "--check",
        action="store_true",
        help=(
            "capture and compare against the committed corpus instead of rewriting it; "
            "a difference exits non-zero, which is what proves a re-run with no change "
            "in between is byte-identical"
        ),
    )
    return parser.parse_args()


def rendered_corpus(records: list[dict[str, Any]], reference: dict[str, str]) -> str:
    return (
        json.dumps(build_corpus(records, reference), indent=2, sort_keys=True, ensure_ascii=False)
        + "\n"
    )


def main() -> int:
    arguments = parse_arguments()
    try:
        reference = resolve_reference(arguments.reference, arguments.expected_commit)
        pinned = extract_pinned_tree(arguments.reference, reference["commit"], arguments.cache)
        reexecute_with_reference_interpreter(arguments.reference, arguments.python, pinned)
        records = capture(pinned)
        if arguments.check:
            target = arguments.corpus or DEFAULT_CORPUS
            if not target.is_file():
                raise OracleError(f"no committed corpus at {target} to check against")
            if target.read_text(encoding="utf-8") != rendered_corpus(records, reference):
                raise OracleError(
                    f"a fresh capture differs from the committed corpus at {target}"
                )
            print(f"{target} matches a fresh capture of {len(records)} cases")
            return 0
        full = {
            "schemaVersion": SCHEMA_VERSION,
            "reference": reference,
            "platform": platform.system().lower(),
            "python": platform.python_version(),
            "git": run_git(Path.cwd(), "--version").strip(),
            "cases": records,
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
            arguments.corpus.write_text(rendered_corpus(records, reference), encoding="utf-8")
    except OracleError as error:
        print(f"worktree capture failed: {error}", file=sys.stderr)
        return 1
    families = sorted({record["family"] for record in records})
    print(
        f"captured {len(records)} cases across {len(families)} families "
        f"({', '.join(families)}) from {reference['commit'][:12]} into {arguments.output}"
    )
    if arguments.corpus is not None:
        print(f"wrote the committed corpus to {arguments.corpus}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
