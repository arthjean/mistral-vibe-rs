"""The fixtures, probes and queries ``autocompletion.py`` drives the reference over.

Every path, name, rules file, query, prompt and seed here is authored for the
corpus; none is read from the reference.
"""

from __future__ import annotations

from typing import Any

#: What a fixture file holds. The index never reads a file's bytes, so one
#: constant covers every fixture and keeps the corpus free of invented prose.
FILE_BODY = "fixture\n"

#: The scratch trees every family is measured over. A path ending in `/` is a
#: directory; every other path is a file whose parents are created with it.
#: Names are chosen here so the corpus carries fixture-supplied vocabulary and
#: nothing the reference authored.
FIXTURES: dict[str, dict[str, Any]] = {
    "plain": {
        "tree": [
            "readme.md",
            "src/main.rs",
            "src/lib.rs",
            "src/render/mod.rs",
            "src/render/table.rs",
            "docs/guide.md",
            "docs/api/index.md",
            "empty-dir/",
        ],
        "gitignore": None,
    },
    "ignored": {
        "tree": [
            "keep.txt",
            "build/output.bin",
            "target/debug/app",
            "node_modules/left-pad/index.js",
            "__pycache__/module.cpython-312.pyc",
            "src/app.pyc",
            "logs/run.log",
            "notes.log",
            "vendor/pkg/file.go",
            ".coverage",
            "bundle.min.js",
            "src/vendor/keep.go",
        ],
        "gitignore": None,
    },
    "gitignored": {
        "tree": [
            "keep.log",
            "drop.log",
            "report-7.txt",
            "report-x.txt",
            "alpha.tmp",
            "blpha.tmp",
            "secrets/key.pem",
            "public/asset.css",
            "nested/build/artifact.o",
            "build/artifact.o",
            "hash#name.txt",
            "spaced name.txt",
        ],
        "gitignore": (
            "# a comment line\n"
            "\n"
            "*.log\n"
            "!keep.log\n"
            "report-[0-9].txt\n"
            "[!a]lpha.tmp\n"
            "secrets/\n"
            "/build/\n"
            "trailing.txt # an inline comment\n"
            "!  \n"
        ),
    },
    "hidden": {
        "tree": [
            ".config/settings.toml",
            ".hidden-file",
            "visible.txt",
            ".git/HEAD",
            "src/.env",
            "src/index.ts",
        ],
        "gitignore": None,
    },
    "ranking": {
        "tree": [
            "Cargo.toml",
            "Cargo.lock",
            "src/main.rs",
            "src/lib.rs",
            "src/parser/mod.rs",
            "src/parser/tokens.rs",
            "src/parser/tokenStream.rs",
            "src/render/table.rs",
            "src/render/tableCell.rs",
            "tests/parser_test.rs",
            "tests/render_test.rs",
            "docs/parser.md",
            "docs/render.md",
            "scripts/build.sh",
            "packages/parser/package.json",
            "packages/parser/src/index.ts",
            "packages/render/package.json",
            "café/menu.txt",
            "café/naïve.txt",
        ],
        "gitignore": None,
    },
    "wide": {
        # 130 top-level files, past the 100-match cap, so the cap is measured
        # rather than assumed.
        "tree": [f"entry-{index:03}.txt" for index in range(130)],
        "gitignore": None,
    },
    "edgeignore": {
        # The rules file as an editor can leave it: behind a byte-order mark,
        # with line boundaries `splitlines` honors and `\n` does not, a lone
        # carriage return, a doubled negation and a doubled anchor.
        "tree": [
            "double.txt",
            "!double.txt",
            "slashed",
            "x.bom",
            "a.vt",
            "a.ls",
            "a.us",
            "a.cr2",
            "keep.md",
        ],
        "gitignore": (
            "\ufeff*.bom\n"
            "*.txt\n"
            "!!double.txt\n"
            "//slashed\n"
            "*.ff\x0c*.vt\n"
            "*.ps\u2028*.ls\n"
            "\x1f*.us\n"
            "*.cr\r*.cr2\n"
        ),
    },
    "collect": {
        "tree": [
            "README.md",
            "Makefile",
            ".env",
            "src/main.rs",
            "src/parser/mod.rs",
            "src/parser/lexer.rs",
            "docs/guide.md",
            "empty-dir/",
        ],
        "gitignore": None,
    },
}

#: What surrounds a `collect` root, relative to an enclosure holding the root at
#: `top/root`, so a `..` query lists directories this script authored.
COLLECT_OUTSIDE: list[str] = [
    "top/sibling/alpha.md",
    "top/sibling/a.txt",
    "top/sibling/Beta/inner.txt",
    "top/sibling/.hidden",
    "top/peer.txt",
    "other/x.txt",
]
COLLECT_ROOT = "top/root"

#: The `(rel, name, is_dir)` triples the ignore family asks the reference about.
#: Each is a fixture id and a probe; a probe need not exist on disk, since
#: `should_ignore` is a pure function of the three strings once the root is
#: compiled.
IGNORE_PROBES: list[tuple[str, str, bool]] = [
    ("plain", "src", True),
    ("plain", "src/main.rs", False),
    ("plain", "readme.md", False),
    ("ignored", ".git", True),
    ("ignored", "src/.git", True),
    ("ignored", ".git", False),
    ("ignored", "__pycache__", True),
    ("ignored", "src/__pycache__", True),
    ("ignored", "node_modules", True),
    ("ignored", "src/node_modules", True),
    ("ignored", ".DS_Store", False),
    ("ignored", "src/app.pyc", False),
    ("ignored", "notes.log", False),
    ("ignored", "logs", True),
    ("ignored", "logs", False),
    ("ignored", ".vscode", True),
    ("ignored", ".idea", True),
    ("ignored", "build", True),
    ("ignored", "src/build", True),
    ("ignored", "dist", True),
    ("ignored", "target", True),
    ("ignored", "src/target", True),
    ("ignored", ".next", True),
    ("ignored", ".nuxt", True),
    ("ignored", "coverage", True),
    ("ignored", ".nyc_output", True),
    ("ignored", "vibe.egg-info", False),
    ("ignored", "vibe.egg-info", True),
    ("ignored", ".pytest_cache", True),
    ("ignored", ".tox", True),
    ("ignored", "vendor", True),
    ("ignored", "src/vendor", True),
    ("ignored", "third_party", True),
    ("ignored", "deps", True),
    ("ignored", "bundle.min.js", False),
    ("ignored", "site.min.css", False),
    ("ignored", "app.bundle.js", False),
    ("ignored", "app.chunk.js", False),
    ("ignored", ".cache", True),
    ("ignored", "tmp", True),
    ("ignored", "temp", True),
    ("ignored", ".uv-cache", True),
    ("ignored", ".ruff_cache", True),
    ("ignored", ".venv", True),
    ("ignored", "venv", True),
    ("ignored", ".mypy_cache", True),
    ("ignored", "htmlcov", True),
    ("ignored", ".coverage", False),
    ("ignored", "keep.txt", False),
    ("gitignored", "drop.log", False),
    ("gitignored", "keep.log", False),
    ("gitignored", "nested/keep.log", False),
    ("gitignored", "report-7.txt", False),
    ("gitignored", "report-x.txt", False),
    ("gitignored", "alpha.tmp", False),
    ("gitignored", "blpha.tmp", False),
    ("gitignored", "secrets", True),
    ("gitignored", "secrets", False),
    ("gitignored", "nested/secrets", True),
    ("gitignored", "build", True),
    ("gitignored", "nested/build", True),
    ("gitignored", "trailing.txt", False),
    ("gitignored", "public/asset.css", False),
    ("gitignored", "hash#name.txt", False),
    ("gitignored", "spaced name.txt", False),
]

#: The change sequences the incremental store is driven through. Each step
#: mutates the tree, hands `apply_changes` a list of `(kind, path)` pairs, and
#: the capture records the entry set and the stats that result.
#:
#: `path` is relative to the index root; a path starting with `../` deliberately
#: escapes it, which is the change category the store skips.
CHANGE_SEQUENCES: list[dict[str, Any]] = [
    {
        "case": "add-one-file",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [{"op": "createFile", "path": "src/added.rs"}],
                "changes": [["added", "src/added.rs"]],
            }
        ],
    },
    {
        "case": "modify-one-file",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [{"op": "modifyFile", "path": "src/main.rs"}],
                "changes": [["modified", "src/main.rs"]],
            }
        ],
    },
    {
        "case": "delete-one-file",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [{"op": "delete", "path": "src/lib.rs"}],
                "changes": [["deleted", "src/lib.rs"]],
            }
        ],
    },
    {
        "case": "delete-directory-by-prefix",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [{"op": "delete", "path": "src/render"}],
                "changes": [["deleted", "src/render"]],
            }
        ],
    },
    {
        "case": "add-directory-recursively",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [
                    {"op": "createFile", "path": "added/one.txt"},
                    {"op": "createFile", "path": "added/deep/two.txt"},
                    {"op": "createDir", "path": "added/deep/empty"},
                ],
                "changes": [["added", "added"]],
            }
        ],
    },
    {
        "case": "add-directory-whose-descendants-are-ignored",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [
                    {"op": "createFile", "path": "generated/keep.txt"},
                    {"op": "createFile", "path": "generated/target/app"},
                    {"op": "createFile", "path": "generated/notes.log"},
                ],
                "changes": [["added", "generated"]],
            }
        ],
    },
    {
        "case": "add-ignored-file",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [{"op": "createFile", "path": "run.log"}],
                "changes": [["added", "run.log"]],
            }
        ],
    },
    {
        "case": "change-outside-the-root",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [],
                "changes": [["added", "../outside/stranger.txt"]],
            }
        ],
    },
    {
        "case": "add-a-path-that-does-not-exist",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [],
                "changes": [["added", "src/never-written.rs"]],
            }
        ],
    },
    {
        "case": "delete-a-path-the-index-never-held",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [],
                "changes": [["deleted", "src/never-written.rs"]],
            }
        ],
    },
    {
        "case": "rename-a-file",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [
                    {"op": "rename", "path": "docs/guide.md", "to": "docs/manual.md"}
                ],
                "changes": [
                    ["deleted", "docs/guide.md"],
                    ["added", "docs/manual.md"],
                ],
            }
        ],
    },
    {
        "case": "rename-a-directory",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [{"op": "rename", "path": "src/render", "to": "src/draw"}],
                "changes": [
                    ["deleted", "src/render"],
                    ["added", "src/draw"],
                ],
            }
        ],
    },
    {
        "case": "two-steps-add-then-delete",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [{"op": "createFile", "path": "src/scratch.rs"}],
                "changes": [["added", "src/scratch.rs"]],
            },
            {
                "mutations": [{"op": "delete", "path": "src/scratch.rs"}],
                "changes": [["deleted", "src/scratch.rs"]],
            },
        ],
    },
    {
        "case": "unreported-change-leaves-the-index-stale",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [{"op": "createFile", "path": "src/silent.rs"}],
                "changes": [],
            }
        ],
    },
    {
        "case": "batch-at-the-mass-change-threshold",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [
                    {"op": "createFile", "path": f"bulk/file-{index:03}.txt"}
                    for index in range(200)
                ],
                "changes": [
                    ["added", f"bulk/file-{index:03}.txt"] for index in range(200)
                ],
            }
        ],
    },
    {
        "case": "batch-past-the-mass-change-threshold",
        "fixture": "plain",
        "steps": [
            {
                "mutations": [
                    {"op": "createFile", "path": f"bulk/file-{index:03}.txt"}
                    for index in range(201)
                ],
                "changes": [
                    ["added", f"bulk/file-{index:03}.txt"] for index in range(201)
                ],
            }
        ],
    },
]

#: The queries the ranking family is measured over, as the fragment that
#: follows `@` in the prompt.
RANKING_QUERIES: list[tuple[str, str]] = [
    ("ranking", ""),
    ("ranking", "src/"),
    ("ranking", "src/parser/"),
    ("ranking", "packages/parser/"),
    ("ranking", "main"),
    ("ranking", "main.rs"),
    ("ranking", "lib"),
    ("ranking", "cargo"),
    ("ranking", "Cargo.toml"),
    ("ranking", "cargo.lock"),
    ("ranking", "parser"),
    ("ranking", "parser/"),
    ("ranking", "src/parser"),
    ("ranking", "tokens"),
    ("ranking", "tokenstream"),
    ("ranking", "tkst"),
    ("ranking", "table"),
    ("ranking", "tablecell"),
    ("ranking", "render"),
    ("ranking", "rendertest"),
    ("ranking", "srcmain"),
    ("ranking", "pkgjson"),
    ("ranking", "package.json"),
    ("ranking", ".ts"),
    ("ranking", "md"),
    ("ranking", "docs/render.md"),
    ("ranking", "zzzz"),
    ("ranking", "café"),
    ("ranking", "naïve"),
    ("ranking", "cafe"),
    ("hidden", ""),
    ("hidden", "."),
    ("hidden", ".env"),
    ("hidden", "visible"),
    ("hidden", "src/"),
    ("plain", ""),
    ("plain", "empty-dir/"),
    ("plain", "guide"),
    ("wide", ""),
    ("wide", "entry-0"),
]


#: The edge cases the ignore family adds over the `edgeignore` rules file.
IGNORE_PROBES += [
    ("edgeignore", "x.bom", False),
    ("edgeignore", "double.txt", False),
    ("edgeignore", "!double.txt", False),
    ("edgeignore", "slashed", False),
    ("edgeignore", "a.ff", False),
    ("edgeignore", "a.vt", False),
    ("edgeignore", "a.ps", False),
    ("edgeignore", "a.ls", False),
    ("edgeignore", "a.us", False),
    ("edgeignore", "a.cr", False),
    ("edgeignore", "a.cr2", False),
    ("edgeignore", "keep.md", False),
]

#: The work tree the git families are measured over. Every path is relative to
#: the repository root; `tracked` is staged with `git add -f`, `deleted` is
#: removed from disk after staging, and `excludesFile` is wired through the
#: repository's own `core.excludesFile`, which outranks any global one.
GIT_FIXTURE: dict[str, Any] = {
    "tracked": [
        "src/main.rs",
        "vendor/pkg/lib.go",
        "logs/app.log",
        "build/out.txt",
        ".vscode/settings.json",
        "docs/guide.md",
        "docs/gone.md",
        "nested/inner/keep.txt",
        "café/menu.txt",
        "spaced name.txt",
        "ignored-dir/forced.txt",
    ],
    "untracked": [
        "notes.txt",
        "nested/scratch.draft",
        "nested/inner/extra.txt",
        "nested/inner/deep/new.txt",
        "local.secret",
        "cache.junk",
        "node_modules/x/index.js",
        "a.tmp",
        "ignored-dir/file.txt",
    ],
    "deleted": ["docs/gone.md"],
    "emptyDirs": ["empty-dir", "nested/empty"],
    "gitignores": {
        ".gitignore": "*.tmp\nignored-dir/\n",
        "nested/.gitignore": "*.draft\n",
    },
    "infoExclude": "*.secret\n",
    "excludesFile": "*.junk\n",
}

#: The roots the git walk family lists, relative to the repository root.
GIT_ROOTS: list[str] = ["", "nested", "ignored-dir", "docs"]

#: The watched change sequences the git-backed index is driven through, each
#: from a fresh repository, through the indexer's own change handler.
GIT_CHANGE_SEQUENCES: list[dict[str, Any]] = [
    {
        "case": "untracked-file-appears-after-the-next-query",
        "mutations": [{"op": "createFile", "path": "added.txt"}],
        "changes": [["added", "added.txt"]],
    },
    {
        "case": "ignored-file-stays-out",
        "mutations": [{"op": "createFile", "path": "fresh.tmp"}],
        "changes": [["added", "fresh.tmp"]],
    },
    {
        "case": "deleted-tracked-file-leaves",
        "mutations": [{"op": "delete", "path": "src/main.rs"}],
        "changes": [["deleted", "src/main.rs"]],
    },
    {
        "case": "modified-file-marks-dirty",
        "mutations": [{"op": "modifyFile", "path": "notes.txt"}],
        "changes": [["modified", "notes.txt"]],
    },
    {
        "case": "no-change-no-rebuild",
        "mutations": [],
        "changes": [],
    },
    {
        "case": "change-outside-the-root-still-marks-dirty",
        "mutations": [],
        "changes": [["added", "../outside.txt"]],
    },
]

#: The prompts the `collect` family hands `PathCompleter`, as `(fixture, text,
#: cursor)`; a cursor of `None` sits at the end of the text.
COLLECT_QUERIES: list[tuple[str, str, int | None]] = [
    ("collect", "@", None),
    ("collect", "@..", None),
    ("collect", "@../", None),
    ("collect", "@../s", None),
    ("collect", "@../SIB", None),
    ("collect", "@../sibling/", None),
    ("collect", "@../sibling/a", None),
    ("collect", "@../sibling/.", None),
    ("collect", "@../sibling/b", None),
    ("collect", "@../sibling/Beta", None),
    ("collect", "@../sibling/Beta/", None),
    ("collect", "@../missing", None),
    ("collect", "@../missing/x", None),
    ("collect", "@../..", None),
    ("collect", "@../../", None),
    ("collect", "@../../o", None),
    ("collect", "@..x", None),
    ("collect", "@../root/", None),
    ("collect", "@../root/src", None),
    ("collect", "@./src/", None),
    ("collect", "@src\\par", None),
    ("collect", "@src\\parser\\", None),
    ("collect", "@srx/", None),
    ("collect", "@/", None),
    ("collect", "@empty-dir/", None),
    ("collect", "@parser/", None),
    ("collect", "@Src/", None),
    ("collect", "@src/", None),
    ("collect", "@main", None),
    ("collect", "look at @src/ma", None),
    ("collect", "@src/main.rs tail", 6),
    ("collect", "@foo bar", None),
    ("collect", "no mention here", None),
    ("collect", "a@src/ma", None),
    ("collect", "@.e", None),
    ("wide", "@", None),
    ("wide", "@entry", None),
]

#: The skills the inline family offers, in the `(alias, description)` shape
#: the chat input hands its controller.
INLINE_SKILLS: list[tuple[str, str]] = [
    ("/implement", ""),
    ("/implement-plan", ""),
    ("/Review", ""),
    ("/review-pr", ""),
    ("/deploy", ""),
    ("/a", ""),
]

#: The prompts the inline family types, as `(text, cursor, default_mode)`.
INLINE_CASES: list[tuple[str, int | None, bool]] = [
    ("fix /imp", None, True),
    ("fix /implement", None, True),
    ("fix /implement-plan", None, True),
    ("/imp", None, True),
    ("   /imp", None, True),
    ("fix /IMP", None, True),
    ("fix /rev", None, True),
    ("fix /imp tail", 8, True),
    ("fix /imptail", 8, True),
    ("fix /", None, True),
    ("fix /a/b", None, True),
    ("fix /a@b", None, True),
    ("mail@x /dep", None, True),
    ("fix\n/imp", None, True),
    ("fix /imp", None, False),
    ("fix /imp", 0, True),
    ("fix /a", None, True),
    ("fix /zzz", None, True),
    ("fix\t/dep", None, True),
]

#: The fixture the prompt family resolves mentions against.
PROMPT_TREE: list[str] = [
    "docs/guide.md",
    "img/shot.PNG",
    "dir with space/file.txt",
    "it's.txt",
    "notes.md",
    "archive.tar.gz",
]

#: The messages the prompt family parses. `{root}` stands for the resolved
#: fixture root, substituted on both sides.
PROMPT_MESSAGES: list[str] = [
    "",
    "read @docs/guide.md please",
    "@docs/guide.md and @docs/guide.md",
    "@docs/guide.md then @./docs/../docs/guide.md",
    "email a@docs/guide.md",
    "x_@docs/guide.md",
    "é@docs/guide.md",
    "(@docs/guide.md)",
    "see @docs",
    "see @img/shot.PNG",
    'see @"dir with space/file.txt"',
    "see @'dir with space/file.txt'",
    "see @'it\\'s.txt'",
    'see @"it\'s.txt"',
    'see @"unterminated',
    'see @"" now',
    "see @'' now",
    'see @"missing @docs/guide.md"',
    "see @missing.md",
    "see @ alone",
    "see @@docs/guide.md",
    'see @"{root}/notes.md"',
    "see @docs/guide.md,@notes.md",
    "see @'docs/guide.md'@notes.md",
    "see @archive.tar.gz and @img",
    "trailing @",
]

#: The changed paths the watch filter is asked about, as segments under a
#: watched root.
WATCH_FILTER_PROBES: list[list[str]] = [
    ["src", "main.rs"],
    [".git", "index"],
    [".git"],
    ["a", ".git", "HEAD"],
    ["node_modules", "x", "i.js"],
    ["__pycache__", "m.cpython-312.pyc"],
    ["m.pyc"],
    ["m.pyo"],
    ["m.pyd"],
    ["m.py"],
    ["x.pyc.txt"],
    [".venv", "lib"],
    [".idea", "ws.xml"],
    [".hg"],
    [".svn", "x"],
    [".tox"],
    [".mypy_cache", "x"],
    [".pytest_cache"],
    [".hypothesis", "x"],
    ["f.swp"],
    ["f.swx"],
    ["f.sw"],
    ["notes~"],
    ["a~b"],
    [".#lock"],
    [".DS_Store"],
    ["a", ".DS_Store"],
    ["DS_Store"],
    ["flycheck_main.rs"],
    ["src", "flycheck_x"],
    ["x.___jb_tmp___"],
    ["x.___jb_old___"],
    ["x.___jb_ld___"],
    ["venv", "x"],
    ["target", "x"],
    ["git"],
    ["my.git"],
]

#: The seeded sweep the fuzzy family records. Both sides draw the same pairs
#: from a SplitMix64 stream with integer arithmetic only, so nothing but the
#: seed and the alphabet has to travel in the corpus.
FUZZY_SEED = 0x5EED_A11C
FUZZY_COUNT = 4096
FUZZY_ALPHABET = "abcdeABCDE/-_.012éÉßİΣσǅⒶ"
_MASK64 = (1 << 64) - 1


class SplitMix64:
    """The generator the fuzzy sweep draws from, mirrored in the Rust replay."""

    def __init__(self, seed: int) -> None:
        self.state = seed & _MASK64

    def next(self) -> int:
        self.state = (self.state + 0x9E3779B97F4A7C15) & _MASK64
        z = self.state
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & _MASK64
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & _MASK64
        return z ^ (z >> 31)

    def below(self, bound: int) -> int:
        return self.next() % bound


def fuzzy_pairs() -> list[tuple[str, str]]:
    """The sweep's `(pattern, text)` pairs: half drawn as a subsequence of the
    text, so most of them match, and half drawn at random."""

    stream = SplitMix64(FUZZY_SEED)
    alphabet = list(FUZZY_ALPHABET)
    pairs: list[tuple[str, str]] = []
    for _ in range(FUZZY_COUNT):
        text = "".join(
            alphabet[stream.below(len(alphabet))] for _ in range(stream.below(17))
        )
        length = 1 + stream.below(4)
        if text and stream.below(2) == 0:
            indices = sorted(stream.below(len(text)) for _ in range(length))
            pattern = "".join(text[index] for index in indices)
        else:
            pattern = "".join(
                alphabet[stream.below(len(alphabet))] for _ in range(length)
            )
        pairs.append((pattern, text))
    return pairs
