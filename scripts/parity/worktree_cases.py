"""The cases ``worktree.py`` drives the pinned reference's worktree code over.

A case names the scripted repository it needs, the names, branches, texts and
directories it hands the reference, and the lifecycle steps applied first.
Every value here is authored for the corpus.
"""

from __future__ import annotations

from typing import Any


#: The directory names a case root holds. `repo` is the primary checkout, which
#: also fixes `repo_root.name` and therefore the managed directory's prefix.
CHECKOUT = "repo"
VIBE_HOME = "home"
LINKED = "linked"
OUTSIDE = "outside"
TREE = "tree"
STATE = "state"


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


#: The claim time each lifecycle case stamps, in order of preparation, so the
#: retention sweep sorts by a time this capture chose rather than by the clock.
CLAIMED_AT = (
    "2001-02-03T04:05:06+00:00",
    "2001-02-03T04:05:07+00:00",
    "2001-02-03T04:05:08+00:00",
)


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


#: One restore per row: what happened to the worktree first, and where inside
#: it (or outside it) the restore is asked for.
RESTORE_CASES: tuple[tuple[str, tuple[str, ...], str], ...] = (
    ("retained", ("settle", "dirty", "prune"), "."),
    ("retained-committed", ("settle", "commit", "prune"), "."),
    ("not-retained", ("settle",), "."),
    ("outside", ("settle", "prune"), ".."),
)


#: One position per row: the setup, and the directory the repository is opened
#: at, relative to the case root.
POSITION_CASES: tuple[tuple[str, str, str], ...] = (
    ("checkout", "linked-worktrees", CHECKOUT),
    ("linked", "linked-worktrees", f"{LINKED}/alpha"),
    ("linked-only-directory", "linked-worktrees", f"{LINKED}/alpha/only-here"),
    ("detached", "linked-worktrees", f"{LINKED}/gamma"),
    ("not-a-repository", "target-tree", "."),
)
