"""The cases ``skills.py`` drives the pinned reference's skills surface over.

A case names a skill file, a frontmatter mapping, a directory tree with the
configuration a manager is built under, a typed command, a registry item or a
registry the session syncs from. Every value here is authored for the corpus;
none is read from the reference.
"""

from __future__ import annotations

import base64
from typing import Any


FRONTMATTER_CASES: list[tuple[str, str]] = [
    ("valid-minimal", "---\nname: probe\ndescription: A probe skill\n---\n\nBody line.\n"),
    (
        "all-fields-nested-metadata",
        "---\n"
        "name: full-probe\n"
        "description: A probe with every field\n"
        "license: MIT\n"
        "compatibility: Requires nothing\n"
        "metadata:\n"
        "  display-name: Full Probe\n"
        "  default-prompt: run it\n"
        "allowed-tools: bash read_file\n"
        "user-invocable: false\n"
        "---\n\nInstructions here.\n",
    ),
    ("missing-frontmatter", "Just markdown content without frontmatter\n"),
    ("unclosed-frontmatter", "---\nname: incomplete\ndescription: no closing fence\n"),
    ("invalid-yaml", "---\nname: [broken\ndescription: oops\n---\n\nBody.\n"),
    ("sequence-document", "---\n- item-one\n- item-two\n---\n\nBody.\n"),
    ("scalar-document", "---\njust a scalar\n---\n\nBody.\n"),
    ("empty-frontmatter", "---\n---\n\nBody content.\n"),
    ("comment-only-frontmatter", "---\n# nothing but a comment\n---\n\nBody.\n"),
    ("no-body", "---\nname: minimal\ndescription: no body\n---\n"),
    (
        "leading-bom",
        "\ufeff---\nname: bom-probe\ndescription: BOM stripped\n---\n\nBody.\n",
    ),
    ("boundary-four-hyphens", "----\nname: probe\ndescription: wide fence\n----\n\nBody.\n"),
    (
        "boundary-trailing-whitespace",
        "---  \nname: probe\ndescription: padded fence\n---\t\n\nBody.\n",
    ),
    ("text-before-boundary", "prelude\n---\nname: probe\ndescription: late fence\n---\n\nBody.\n"),
    (
        "block-sequence-allowed-tools",
        "---\nname: probe\ndescription: list form\nallowed-tools:\n  - bash\n  - read_file\n---\n\nBody.\n",
    ),
    (
        "folded-scalar-description",
        "---\nname: probe\ndescription: >\n  folded across\n  two lines\n---\n\nBody.\n",
    ),
    (
        "literal-scalar-description",
        "---\nname: probe\ndescription: |\n  literal line one\n  literal line two\n---\n\nBody.\n",
    ),
    ("user-invocable-no", "---\nname: probe\ndescription: d\nuser-invocable: no\n---\n\nBody.\n"),
    ("user-invocable-cap-no", "---\nname: probe\ndescription: d\nuser-invocable: No\n---\n\nBody.\n"),
    ("user-invocable-off", "---\nname: probe\ndescription: d\nuser-invocable: OFF\n---\n\nBody.\n"),
    ("user-invocable-on", "---\nname: probe\ndescription: d\nuser-invocable: on\n---\n\nBody.\n"),
    ("user-invocable-y", "---\nname: probe\ndescription: d\nuser-invocable: y\n---\n\nBody.\n"),
    ("user-invocable-yes", "---\nname: probe\ndescription: d\nuser-invocable: yes\n---\n\nBody.\n"),
    (
        "user-invocable-quoted-no",
        '---\nname: probe\ndescription: d\nuser-invocable: "no"\n---\n\nBody.\n',
    ),
    (
        "duplicate-key-last-wins",
        "---\nname: first\nname: second\ndescription: d\n---\n\nBody.\n",
    ),
    (
        "body-keeps-later-fences",
        "---\nname: probe\ndescription: d\n---\n\nBody before.\n---\nBody after a fence.\n",
    ),
    (
        "fence-inside-literal-scalar",
        "---\nname: probe\ndescription: |\n  line one\n---\n  line two\n---\n\nBody.\n",
    ),
    ("tab-indented-yaml", "---\nname: probe\ndescription:\n\tvalue\n---\n\nBody.\n"),
    (
        "crlf-line-endings",
        "---\r\nname: probe\r\ndescription: carriage returns\r\n---\r\n\r\nBody.\r\n",
    ),
    (
        "numeric-scalars-kept",
        "---\nname: probe\ndescription: d\nmetadata:\n  version: 1.0\n  count: 42\n---\n\nBody.\n",
    ),
]


METADATA_CASES: list[tuple[str, dict[str, Any]]] = [
    ("minimal", {"name": "probe", "description": "A probe"}),
    (
        "all-fields",
        {
            "name": "full-probe",
            "description": "Everything set",
            "license": "MIT",
            "compatibility": "none needed",
            "metadata": {"display-name": "Full Probe"},
            "allowed-tools": ["bash", "read_file"],
            "user-invocable": False,
        },
    ),
    ("allowed-tools-string", {"name": "p", "description": "d", "allowed-tools": "bash read_file grep"}),
    ("allowed-tools-list", {"name": "p", "description": "d", "allowed-tools": ["bash", "read_file"]}),
    ("allowed-tools-null", {"name": "p", "description": "d", "allowed-tools": None}),
    ("allowed-tools-empty-string", {"name": "p", "description": "d", "allowed-tools": ""}),
    ("allowed-tools-underscore-spelling", {"name": "p", "description": "d", "allowed_tools": "bash"}),
    (
        "allowed-tools-both-spellings",
        {"name": "p", "description": "d", "allowed-tools": "bash", "allowed_tools": "grep"},
    ),
    ("user-invocable-hyphen", {"name": "p", "description": "d", "user-invocable": False}),
    ("user-invocable-underscore", {"name": "p", "description": "d", "user_invocable": False}),
    (
        "user-invocable-both-spellings",
        {"name": "p", "description": "d", "user-invocable": False, "user_invocable": True},
    ),
    ("user-invocable-string-false", {"name": "p", "description": "d", "user-invocable": "false"}),
    ("user-invocable-string-no", {"name": "p", "description": "d", "user-invocable": "no"}),
    ("user-invocable-string-junk", {"name": "p", "description": "d", "user-invocable": "maybe"}),
    ("metadata-non-string-values", {"name": "p", "description": "d", "metadata": {"version": 1.0, "count": 42, "flag": True, "empty": None}}),
    ("metadata-null", {"name": "p", "description": "d", "metadata": None}),
    ("metadata-int-keys", {"name": "p", "description": "d", "metadata": {1: "one"}}),
    ("extra-key-ignored", {"name": "p", "description": "d", "unknown-key": "ignored"}),
    ("name-64-chars", {"name": "a" * 64, "description": "d"}),
    ("description-1024-chars", {"name": "p", "description": "a" * 1024}),
    ("compatibility-500-chars", {"name": "p", "description": "d", "compatibility": "c" * 500}),
    ("license-null", {"name": "p", "description": "d", "license": None}),
    ("name-uppercase", {"name": "Probe-Skill", "description": "d"}),
    ("name-underscore", {"name": "probe_skill", "description": "d"}),
    ("name-consecutive-hyphens", {"name": "probe--skill", "description": "d"}),
    ("name-leading-hyphen", {"name": "-probe", "description": "d"}),
    ("name-trailing-hyphen", {"name": "probe-", "description": "d"}),
    ("name-65-chars", {"name": "a" * 65, "description": "d"}),
    ("name-empty", {"name": "", "description": "d"}),
    ("name-missing", {"description": "d"}),
    ("name-integer", {"name": 5, "description": "d"}),
    ("description-missing", {"name": "p"}),
    ("description-empty", {"name": "p", "description": ""}),
    ("description-1025-chars", {"name": "p", "description": "a" * 1025}),
    ("compatibility-501-chars", {"name": "p", "description": "d", "compatibility": "c" * 501}),
    ("license-integer", {"name": "p", "description": "d", "license": 7}),
    ("metadata-list", {"name": "p", "description": "d", "metadata": ["not", "a", "mapping"]}),
    ("allowed-tools-integer", {"name": "p", "description": "d", "allowed-tools": 3}),
    ("disable-model-invocation-true", {"name": "p", "description": "d", "disable-model-invocation": True}),
    ("disable-model-invocation-false", {"name": "p", "description": "d", "disable-model-invocation": False}),
    ("disable-model-invocation-underscore", {"name": "p", "description": "d", "disable_model_invocation": True}),
    ("disable-model-invocation-string-true", {"name": "p", "description": "d", "disable-model-invocation": "true"}),
    ("disable-model-invocation-string-yes", {"name": "p", "description": "d", "disable-model-invocation": "yes"}),
    ("disable-model-invocation-integer-one", {"name": "p", "description": "d", "disable-model-invocation": 1}),
    ("disable-model-invocation-junk", {"name": "p", "description": "d", "disable-model-invocation": "maybe"}),
    ("disable-model-invocation-null", {"name": "p", "description": "d", "disable-model-invocation": None}),
]


def skill_file(name: str, description: str, *, invocable: bool | None = None, body: str = "Probe body.") -> str:
    lines = [f"name: {name}", f"description: {description}"]
    if invocable is not None:
        lines.append(f"user-invocable: {'true' if invocable else 'false'}")
    return "---\n" + "\n".join(lines) + "\n---\n\n" + body + "\n"


DISCOVERY_SCENARIOS: list[dict[str, Any]] = [
    {"case": "empty-everywhere", "tree": {}},
    {
        "case": "project-vibe-skills",
        "tree": {"project": {".vibe/skills/alpha/SKILL.md": skill_file("alpha", "From project .vibe")}},
    },
    {
        "case": "project-agents-skills",
        "tree": {"project": {".agents/skills/beta/SKILL.md": skill_file("beta", "From project .agents")}},
    },
    {
        "case": "project-both-roots",
        "tree": {
            "project": {
                ".vibe/skills/alpha/SKILL.md": skill_file("alpha", "From project .vibe"),
                ".agents/skills/beta/SKILL.md": skill_file("beta", "From project .agents"),
            }
        },
    },
    {
        "case": "project-vibe-beats-agents",
        "tree": {
            "project": {
                ".vibe/skills/shared/SKILL.md": skill_file("shared", "Winner from .vibe"),
                ".agents/skills/shared/SKILL.md": skill_file("shared", "Loser from .agents"),
            }
        },
    },
    {
        "case": "user-vibe-skills",
        "tree": {"home": {".vibe/skills/gamma/SKILL.md": skill_file("gamma", "From user .vibe")}},
    },
    {
        "case": "user-agents-skills",
        "tree": {"home": {".agents/skills/delta/SKILL.md": skill_file("delta", "From user .agents")}},
    },
    {
        "case": "user-vibe-beats-agents",
        "tree": {
            "home": {
                ".vibe/skills/shared/SKILL.md": skill_file("shared", "Winner from user .vibe"),
                ".agents/skills/shared/SKILL.md": skill_file("shared", "Loser from user .agents"),
            }
        },
    },
    {
        "case": "legacy-extensions-root-unread",
        "tree": {"home": {".vibe/extensions/skills/ghost/SKILL.md": skill_file("ghost", "Invented path")}},
    },
    {
        "case": "configured-path",
        "skillPaths": ["${configured}"],
        "tree": {"configured": {"epsilon/SKILL.md": skill_file("epsilon", "From configured")}},
    },
    {
        "case": "configured-beats-project-beats-user",
        "skillPaths": ["${configured}"],
        "tree": {
            "configured": {"shared/SKILL.md": skill_file("shared", "Winner from configured")},
            "project": {".vibe/skills/shared/SKILL.md": skill_file("shared", "Loser from project")},
            "home": {".vibe/skills/shared/SKILL.md": skill_file("shared", "Loser from user")},
        },
    },
    {
        "case": "project-beats-user",
        "tree": {
            "project": {".vibe/skills/shared/SKILL.md": skill_file("shared", "Winner from project")},
            "home": {".vibe/skills/shared/SKILL.md": skill_file("shared", "Loser from user")},
        },
    },
    {
        "case": "two-configured-paths-in-order",
        "skillPaths": ["${configured}", "${configured2}"],
        "tree": {
            "configured": {"shared/SKILL.md": skill_file("shared", "Winner from first path")},
            "configured2": {
                "shared/SKILL.md": skill_file("shared", "Loser from second path"),
                "zeta/SKILL.md": skill_file("zeta", "Only in second path"),
            },
        },
    },
    {
        "case": "nonexistent-configured-path",
        "skillPaths": ["${configured}/missing"],
        "tree": {"project": {".vibe/skills/alpha/SKILL.md": skill_file("alpha", "Still found")}},
    },
    {
        "case": "configured-path-is-a-file",
        "skillPaths": ["${configured}/plain.txt"],
        "tree": {
            "configured": {"plain.txt": "not a directory\n"},
            "project": {".vibe/skills/alpha/SKILL.md": skill_file("alpha", "Still found")},
        },
    },
    {
        "case": "relative-configured-path",
        "skillPaths": ["./rel-skills"],
        "tree": {"project": {"rel-skills/eta/SKILL.md": skill_file("eta", "From relative path")}},
    },
    {
        "case": "tilde-configured-path",
        "skillPaths": ["~/tilde-skills"],
        "tree": {"home": {"tilde-skills/theta/SKILL.md": skill_file("theta", "From tilde path")}},
    },
    {
        "case": "symlinked-root-walked-once",
        "skillPaths": ["${configured}", "${configured}-link"],
        "symlinks": [{"link": "configured-link", "target": "configured"}],
        "tree": {"configured": {"iota/SKILL.md": skill_file("iota", "Behind a symlink too")}},
    },
    {
        "case": "duplicate-name-within-one-root",
        "pathUnrecorded": True,
        "tree": {
            "project": {
                ".vibe/skills/dir-a/SKILL.md": skill_file("twin", "Same either way"),
                ".vibe/skills/dir-b/SKILL.md": skill_file("twin", "Same either way"),
            }
        },
    },
    {
        "case": "clutter-ignored",
        "tree": {
            "project": {
                ".vibe/skills/no-manifest/README.md": "not a skill\n",
                ".vibe/skills/stray-file.md": "just a file\n",
                ".vibe/skills/alpha/SKILL.md": skill_file("alpha", "The only real one"),
            }
        },
    },
    {
        "case": "malformed-skill-becomes-issue",
        "tree": {
            "project": {
                ".vibe/skills/broken/SKILL.md": "no frontmatter here\n",
                ".vibe/skills/alpha/SKILL.md": skill_file("alpha", "Still loads"),
            }
        },
    },
    {
        "case": "missing-description-becomes-issue",
        "tree": {"project": {".vibe/skills/bare/SKILL.md": "---\nname: bare\n---\n\nBody.\n"}},
    },
    {
        "case": "three-issues-accumulate",
        "tree": {
            "project": {
                ".vibe/skills/bad-a/SKILL.md": "nope\n",
                ".vibe/skills/bad-b/SKILL.md": "---\nname: [\n---\n",
                ".vibe/skills/bad-c/SKILL.md": "---\nname: bad-c\n---\n\nBody.\n",
            }
        },
    },
    {
        "case": "builtin-name-reserved",
        "tree": {"project": {".vibe/skills/vibe/SKILL.md": skill_file("vibe", "Impostor")}},
    },
    {
        "case": "frontmatter-name-wins-over-directory",
        "tree": {"project": {".vibe/skills/folder-name/SKILL.md": skill_file("real-name", "Named by frontmatter")}},
    },
    {
        "case": "session-in-home-stays-global",
        "cwd": "home",
        "tree": {"home": {".vibe/skills/kappa/SKILL.md": skill_file("kappa", "Home is not a project")}},
    },
    {
        "case": "configured-project-root-stays-global",
        "skillPaths": ["${project}/.vibe/skills"],
        "tree": {"project": {".vibe/skills/lambda/SKILL.md": skill_file("lambda", "Configured first")}},
    },
    {
        "case": "untrusted-project-not-walked",
        "trusted": False,
        "tree": {
            "project": {".vibe/skills/alpha/SKILL.md": skill_file("alpha", "Should not load")},
            "home": {".vibe/skills/gamma/SKILL.md": skill_file("gamma", "Still loads")},
        },
    },
]


FILTERING_SCENARIOS: list[dict[str, Any]] = [
    {"case": "no-filters", "skills": ["alpha", "beta"]},
    {"case": "enabled-only", "skills": ["alpha", "beta", "gamma"], "enabled": ["alpha", "gamma"]},
    {"case": "disabled-only", "skills": ["alpha", "beta", "gamma"], "disabled": ["beta"]},
    {
        "case": "enabled-precedence",
        "skills": ["alpha", "beta"],
        "enabled": ["alpha"],
        "disabled": ["alpha"],
    },
    {"case": "glob-pattern", "skills": ["search-code", "search-docs", "other"], "enabled": ["search-*"]},
    {"case": "glob-case-insensitive", "skills": ["search-code", "other"], "enabled": ["SEARCH-*"]},
    {"case": "regex-pattern", "skills": ["skill-v1", "skill-v2", "other"], "enabled": ["re:skill-v\\d+"]},
    {"case": "regex-case-insensitive", "skills": ["skill-v1", "other"], "enabled": ["re:SKILL-V\\d+"]},
    {"case": "invalid-regex-matches-nothing", "skills": ["alpha", "beta"], "enabled": ["re:["]},
    {"case": "invalid-regex-disables-nothing", "skills": ["alpha", "beta"], "disabled": ["re:["]},
    {"case": "enabled-can-select-a-builtin", "skills": ["alpha"], "enabled": ["vibe"]},
    {"case": "disabled-can-withhold-a-builtin", "skills": ["alpha"], "disabled": ["vibe", "skill-creator"]},
    {"case": "blank-patterns-ignored", "skills": ["alpha", "beta"], "disabled": ["", "  "]},
]


COMMAND_FIXTURES: list[dict[str, Any]] = [
    {"name": "probe", "description": "A probe", "invocable": True, "body": "Run the probe checklist."},
    {"name": "hidden", "description": "Model only", "invocable": False, "body": "Hidden body."},
]

COMMAND_CASES: list[tuple[str, str]] = [
    ("plain-text", "hello world"),
    ("slash-only", "/"),
    ("unknown-skill", "/nonexistent"),
    ("simple", "/probe"),
    ("with-args", "/probe fix the bug"),
    ("multiple-spaces-kept-in-args", "/probe   spaced   args"),
    ("uppercase-name", "/PROBE"),
    ("leading-and-trailing-whitespace", "   /probe trailing   "),
    ("newline-separated-args", "/probe\nsecond line"),
    ("not-user-invocable", "/hidden"),
    ("builtin-skill-creator", "/skill-creator"),
    ("builtin-vibe-not-invocable", "/vibe"),
    ("slash-inside-text", "run /probe later"),
]


PROJECTION_CASES: list[tuple[str, dict[str, Any]]] = [
    ("local-defaults", {"name": "probe", "description": "A probe", "prompt": "Body."}),
    (
        "local-not-invocable",
        {"name": "hidden", "description": "Model only", "prompt": "Body.", "user_invocable": False},
    ),
    (
        "builtin-shape",
        {"name": "inline", "description": "Builtin shape", "prompt": "Inline prompt.", "source": "builtin", "user_invocable": False},
    ),
    (
        "registry-source",
        {"name": "remote", "description": "From the registry", "prompt": "Body.", "source": "registry"},
    ),
    ("unicode-fields", {"name": "accents", "description": "Précis à jour", "prompt": "Corps du texte."}),
    ("empty-prompt", {"name": "empty", "description": "No body", "prompt": ""}),
    (
        "rich-model-fields-do-not-reach-the-summary",
        {
            "name": "rich",
            "description": "Carries every model field",
            "prompt": "Body.",
            "license": "MIT",
            "compatibility": "anything",
            "metadata": {"display-name": "Rich"},
            "allowed_tools": ["bash"],
        },
    ),
    ("multiline-prompt", {"name": "long", "description": "d", "prompt": "line one\nline two\n"}),
    (
        "registry-frozen-ref",
        {
            "name": "pinned",
            "description": "Frozen pin",
            "prompt": "Body.",
            "source": "registry",
            "scope": "project",
            "registry": {"skill_id": "sid-1", "version": 3},
        },
    ),
    (
        "registry-alias-ref",
        {
            "name": "tracking",
            "description": "Alias pin",
            "prompt": "Body.",
            "source": "registry",
            "registry": {"skill_id": "sid-2", "version": 5, "alias": "latest"},
        },
    ),
    ("plugin-source", {"name": "plugged", "description": "From a plugin", "prompt": "Body.", "source": "plugin"}),
    ("project-scope", {"name": "local", "description": "Project root", "prompt": "Body.", "scope": "project"}),
]


STORE_CASES: list[dict[str, Any]] = [
    {"case": "unsafe-id-empty", "op": "skillDir", "id": "", "version": 1},
    {"case": "unsafe-id-dot", "op": "skillDir", "id": ".", "version": 1},
    {"case": "unsafe-id-dotdot", "op": "skillDir", "id": "..", "version": 1},
    {"case": "unsafe-id-traversal", "op": "skillDir", "id": "../escape", "version": 1},
    {"case": "unsafe-id-separator", "op": "skillDir", "id": "a/b", "version": 1},
    {"case": "unsafe-id-inner-traversal", "op": "skillDir", "id": "foo/../bar", "version": 1},
    {"case": "safe-id", "op": "skillDir", "id": "ok", "version": 3},
    {
        "case": "materialize-basic",
        "op": "materialize",
        "item": {"skillId": "abc", "version": 1, "description": "does X", "body": "# Hi"},
        "name": "my-skill",
    },
    {
        "case": "materialize-fallback-description",
        "op": "materialize",
        "item": {"skillId": "nod", "version": 1, "body": "# Hi"},
        "name": "nameless",
    },
    {
        "case": "materialize-empty-body",
        "op": "materialize",
        "item": {"skillId": "emp", "version": 1, "body": "   "},
        "name": "empty",
    },
    {
        "case": "empty-body-drops-existing-cache",
        "op": "materialize",
        "prior": [{"item": {"skillId": "emp", "version": 1, "body": "# was here"}, "name": "empty"}],
        "item": {"skillId": "emp", "version": 1, "body": "  "},
        "name": "empty",
    },
    {
        "case": "strips-embedded-frontmatter",
        "op": "materialize",
        "item": {
            "skillId": "fm",
            "version": 1,
            "description": "kept",
            "body": "---\nname: ignored\ndescription: ignored\n---\n\n# Real",
        },
        "name": "skill-one",
    },
    {
        "case": "assets-and-exec-bit",
        "op": "materialize",
        "item": {
            "skillId": "tool",
            "version": 1,
            "description": "with assets",
            "body": "# Hi",
            "assets": [
                {"path": "ref/notes.md", "text": "n"},
                {
                    "path": "bin/run",
                    "base64": base64.b64encode(b"#!/bin/sh\n").decode(),
                    "executable": True,
                },
            ],
        },
        "name": "tooling",
    },
    {
        "case": "unsafe-assets-skipped",
        "op": "materialize",
        "item": {
            "skillId": "safe",
            "version": 1,
            "description": "d",
            "body": "# real",
            "assets": [
                {"path": "../escape.txt", "text": "no"},
                {"path": "/absolute.txt", "text": "no"},
                {"path": ".", "text": "no"},
                {"path": "sub/../SKILL.md", "text": "PWNED"},
                {"path": "SKILLS.MD", "text": "PWNED"},
                {"path": "nested/SKILL.md", "text": "allowed deeper"},
                {"path": "undecodable.bin", "base64": "!!not-base64!!"},
                {"path": "kept.md", "text": "kept"},
            ],
        },
        "name": "safe",
    },
    {
        "case": "rematerialize-drops-stale-assets",
        "op": "materialize",
        "prior": [
            {
                "item": {
                    "skillId": "re",
                    "version": 1,
                    "description": "d",
                    "body": "# v1",
                    "assets": [
                        {"path": "keep.md", "text": "k"},
                        {"path": "old.md", "text": "o"},
                    ],
                },
                "name": "re",
            }
        ],
        "item": {
            "skillId": "re",
            "version": 1,
            "description": "d",
            "body": "# v1 again",
            "assets": [{"path": "keep.md", "text": "k"}],
        },
        "name": "re",
    },
    {
        "case": "failed-write-preserves-previous",
        "op": "materialize",
        "failWrite": True,
        "prior": [{"item": {"skillId": "pre", "version": 1, "description": "d", "body": "# good"}, "name": "pre"}],
        "item": {"skillId": "pre", "version": 1, "description": "d", "body": "# new"},
        "name": "pre",
    },
    {
        "case": "latest-materialized",
        "op": "latestMaterialized",
        "prior": [
            {"item": {"skillId": "v", "version": 1, "body": "# one"}, "name": "v"},
            {"item": {"skillId": "v", "version": 3, "body": "# three"}, "name": "v"},
        ],
        "id": "v",
    },
    {"case": "latest-materialized-missing", "op": "latestMaterialized", "id": "none"},
    {
        "case": "export-local",
        "op": "exportLocal",
        "prior": [
            {
                "item": {
                    "skillId": "exp",
                    "version": 2,
                    "metadataName": "exported",
                    "description": "shipped out",
                    "body": "# Exported",
                    "assets": [{"path": "notes.md", "text": "n"}],
                },
                "name": "exported",
            }
        ],
        "id": "exp",
        "version": 2,
    },
    {
        "case": "materialize-keeps-disabled-invocation",
        "op": "materialize",
        "item": {
            "skillId": "pol",
            "version": 1,
            "description": "explicit only",
            "body": "---\nname: ignored\ndescription: ignored\ndisable-model-invocation: true\n---\n\n# Body",
        },
        "name": "policy",
    },
    {
        "case": "materialize-keeps-enabled-invocation",
        "op": "materialize",
        "item": {
            "skillId": "pol",
            "version": 2,
            "description": "explicit flag off",
            "body": "---\nname: ignored\ndescription: ignored\ndisable-model-invocation: false\n---\n\n# Body",
        },
        "name": "policy",
    },
    {
        "case": "materialize-drops-non-boolean-invocation",
        "op": "materialize",
        "item": {
            "skillId": "pol",
            "version": 3,
            "description": "string flag",
            "body": "---\nname: ignored\ndescription: ignored\ndisable-model-invocation: \"yes\"\n---\n\n# Body",
        },
        "name": "policy",
    },
    {
        "case": "export-local-keeps-invocation",
        "op": "exportLocal",
        "prior": [
            {
                "item": {
                    "skillId": "expol",
                    "version": 1,
                    "description": "exported explicit only",
                    "body": "---\nname: x\ndescription: y\ndisable-model-invocation: true\n---\n\n# Exported",
                },
                "name": "exported-policy",
            }
        ],
        "id": "expol",
        "version": 1,
    },
    {
        "case": "prune-recheck-keeps-claimed",
        "op": "prune",
        "prior": [
            {"item": {"skillId": "a", "version": 1, "body": "# a1"}, "name": "a"},
            {"item": {"skillId": "a", "version": 2, "body": "# a2"}, "name": "a"},
            {"item": {"skillId": "a", "version": 3, "body": "# a3"}, "name": "a"},
        ],
        "active": [["a", 3]],
        "recheck": [["a", 1]],
    },
    {
        "case": "prune-keeps-active",
        "op": "prune",
        "prior": [
            {"item": {"skillId": "a", "version": 1, "body": "# a1"}, "name": "a"},
            {"item": {"skillId": "a", "version": 2, "body": "# a2"}, "name": "a"},
            {"item": {"skillId": "b", "version": 1, "body": "# b1"}, "name": "b"},
        ],
        "active": [["a", 2]],
    },
]


def _policy_skill(name: str, *, disable: str | None = None) -> str:
    lines = [f"name: {name}", f"description: Policy probe {name}"]
    if disable is not None:
        lines.append(f"disable-model-invocation: {disable}")
    return "---\n" + "\n".join(lines) + "\n---\n\nPolicy body.\n"


LOADING_SCENARIOS: list[dict[str, Any]] = [
    {"case": "openai-absent", "files": {"probe/SKILL.md": _policy_skill("probe")}},
    {
        "case": "openai-policy-false",
        "files": {
            "probe/SKILL.md": _policy_skill("probe"),
            "probe/agents/openai.yaml": "policy:\n  allow_implicit_invocation: false\n",
        },
    },
    {
        "case": "openai-policy-true",
        "files": {
            "probe/SKILL.md": _policy_skill("probe"),
            "probe/agents/openai.yaml": "policy:\n  allow_implicit_invocation: true\n",
        },
    },
    {
        "case": "openai-policy-yaml-no",
        "files": {
            "probe/SKILL.md": _policy_skill("probe"),
            "probe/agents/openai.yaml": "policy:\n  allow_implicit_invocation: no\n",
        },
    },
    {
        "case": "openai-policy-null",
        "files": {
            "probe/SKILL.md": _policy_skill("probe"),
            "probe/agents/openai.yaml": "policy:\n  allow_implicit_invocation: null\n",
        },
    },
    {
        "case": "openai-empty-file",
        "files": {"probe/SKILL.md": _policy_skill("probe"), "probe/agents/openai.yaml": ""},
    },
    {
        "case": "openai-null-policy",
        "files": {"probe/SKILL.md": _policy_skill("probe"), "probe/agents/openai.yaml": "policy: null\n"},
    },
    {
        "case": "openai-extra-top-level-keys",
        "files": {
            "probe/SKILL.md": _policy_skill("probe"),
            "probe/agents/openai.yaml": "interface:\n  display_name: Probe\npolicy:\n  allow_implicit_invocation: false\n",
        },
    },
    {
        "case": "openai-products-list",
        "files": {
            "probe/SKILL.md": _policy_skill("probe"),
            "probe/agents/openai.yaml": "policy:\n  products: [codex]\n",
        },
    },
    {
        "case": "openai-products-not-a-list",
        "files": {
            "probe/SKILL.md": _policy_skill("probe"),
            "probe/agents/openai.yaml": "policy:\n  products: codex\n",
        },
    },
    {
        "case": "openai-unknown-policy-key",
        "files": {
            "probe/SKILL.md": _policy_skill("probe"),
            "probe/agents/openai.yaml": "policy:\n  allow_implicit_invocations: true\n",
        },
    },
    {
        "case": "openai-string-boolean",
        "files": {
            "probe/SKILL.md": _policy_skill("probe"),
            "probe/agents/openai.yaml": "policy:\n  allow_implicit_invocation: \"true\"\n",
        },
    },
    {
        "case": "openai-invalid-yaml",
        "files": {"probe/SKILL.md": _policy_skill("probe"), "probe/agents/openai.yaml": "policy: [unclosed\n"},
    },
    {
        "case": "openai-scalar-document",
        "files": {"probe/SKILL.md": _policy_skill("probe"), "probe/agents/openai.yaml": "just text\n"},
    },
    {
        "case": "openai-policy-scalar",
        "files": {"probe/SKILL.md": _policy_skill("probe"), "probe/agents/openai.yaml": "policy: off\n"},
    },
    {
        "case": "openai-metadata-is-a-directory",
        "files": {"probe/SKILL.md": _policy_skill("probe"), "probe/agents/openai.yaml/keep.txt": "x\n"},
    },
    {"case": "frontmatter-disables-invocation", "files": {"probe/SKILL.md": _policy_skill("probe", disable="true")}},
    {
        "case": "frontmatter-and-openai-both-allow",
        "files": {
            "probe/SKILL.md": _policy_skill("probe", disable="false"),
            "probe/agents/openai.yaml": "policy:\n  allow_implicit_invocation: true\n",
        },
    },
    {
        "case": "invalid-policy-on-reserved-name",
        "files": {
            "vibe/SKILL.md": _policy_skill("vibe"),
            "vibe/agents/openai.yaml": "policy: [unclosed\n",
        },
    },
    {
        "case": "invalid-policy-on-malformed-skill",
        "files": {"broken/SKILL.md": "no frontmatter\n", "broken/agents/openai.yaml": "policy: [unclosed\n"},
    },
    {
        "case": "symlinked-skill-file-keeps-its-directory",
        "files": {"elsewhere/source.md": _policy_skill("linked")},
        "links": [["configured", "linked/SKILL.md", "project", "elsewhere/source.md"]],
        "outside": {"project": {"elsewhere/source.md": _policy_skill("linked")}},
    },
    {
        "case": "symlinked-skill-directory-resolves",
        "files": {},
        "links": [["configured", "aliased", "project", "real-skill"]],
        "outside": {"project": {"real-skill/SKILL.md": _policy_skill("aliased")}},
    },
]


INSTALLED_MARK_CASES: list[dict[str, Any]] = [
    {"case": "no-filters", "skills": [["alpha", "local"], ["beta", "registry"]]},
    {"case": "allowlist-decides", "skills": [["alpha", "local"], ["beta", "local"]], "enabled": ["alpha"]},
    {"case": "allowlist-pattern", "skills": [["alpha", "local"], ["beta", "registry"]], "enabled": ["re:al.*"]},
    {
        "case": "allowlist-ignores-disabled",
        "skills": [["alpha", "local"], ["beta", "local"]],
        "enabled": ["alpha"],
        "disabled": ["alpha"],
        "own": ["alpha"],
    },
    {"case": "disabled-by-own-name", "skills": [["alpha", "local"], ["beta", "local"]], "disabled": ["alpha"], "own": ["alpha"]},
    {"case": "disabled-in-another-layer", "skills": [["alpha", "local"]], "disabled": ["alpha"], "own": []},
    {"case": "disabled-by-pattern", "skills": [["alpha", "local"], ["beta", "local"]], "disabled": ["al*"], "own": ["al*"]},
    {
        "case": "disabled-by-name-and-pattern",
        "skills": [["alpha", "local"]],
        "disabled": ["alpha", "a*"],
        "own": ["alpha", "a*"],
    },
    {"case": "single-layer-assumed", "skills": [["alpha", "local"]], "disabled": ["alpha"], "own": None},
    {"case": "unrelated-disabled", "skills": [["alpha", "registry"]], "disabled": ["zeta"], "own": ["zeta"]},
    {"case": "blank-disabled-pattern", "skills": [["alpha", "local"]], "disabled": ["  "], "own": ["  "]},
    {
        "case": "plugin-always-locked",
        "skills": [["alpha", "plugin"], ["beta", "plugin"]],
        "disabled": ["alpha"],
        "own": ["alpha"],
    },
    {"case": "plugin-under-allowlist", "skills": [["alpha", "plugin"], ["beta", "local"]], "enabled": ["gamma"]},
    {"case": "case-insensitive-glob", "skills": [["alpha", "local"]], "disabled": ["ALPHA"], "own": ["ALPHA"]},
]


def _store_item(skill_id: str, version: int, name: str, body: str = "# Registry body", **extra: Any) -> dict[str, Any]:
    return {
        "item": {"skillId": skill_id, "version": version, "description": f"{name} v{version}", "body": body, **extra},
        "name": name,
    }


LIFECYCLE_SCENARIOS: list[dict[str, Any]] = [
    {
        "case": "experiment-off-loads-no-pin",
        "registryEnabled": False,
        "store": [_store_item("sid-a", 1, "remote")],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 1}]},
    },
    {
        "case": "frozen-pin-loads",
        "store": [_store_item("sid-a", 1, "remote"), _store_item("sid-a", 2, "remote")],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 1}]},
    },
    {
        "case": "latest-pin-loads-newest-on-disk",
        "store": [_store_item("sid-a", 1, "remote"), _store_item("sid-a", 4, "remote")],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": "latest"}]},
    },
    {
        "case": "custom-alias-loads-recorded-version",
        "store": [_store_item("sid-a", 1, "remote"), _store_item("sid-a", 2, "remote")],
        "resolved": [["sid-a", "stable", 1]],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": "stable"}]},
    },
    {
        "case": "custom-alias-recorded-but-missing-falls-back",
        "store": [_store_item("sid-a", 2, "remote")],
        "resolved": [["sid-a", "stable", 1]],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": "stable"}]},
    },
    {
        "case": "custom-alias-unrecorded-falls-back",
        "store": [_store_item("sid-a", 3, "remote")],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": "stable"}]},
    },
    {
        "case": "frozen-pin-not-materialized-skipped",
        "store": [_store_item("sid-a", 2, "remote")],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 1}]},
    },
    {
        "case": "latest-pin-nothing-on-disk-skipped",
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": "latest"}]},
    },
    {
        "case": "unsafe-id-skipped",
        "store": [_store_item("sid-a", 1, "remote")],
        "manifests": {
            "global": [
                {"name": "escape", "skill_id": "../sid-a", "version": 1},
                {"name": "remote", "skill_id": "sid-a", "version": 1},
            ]
        },
    },
    {
        "case": "local-skill-wins-over-pin",
        "tree": {"project": {".vibe/skills/remote/SKILL.md": skill_file("remote", "Local copy")}},
        "store": [_store_item("sid-a", 1, "remote")],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 1}]},
    },
    {
        "case": "builtin-name-wins-over-pin",
        "store": [_store_item("sid-v", 1, "vibe")],
        "manifests": {"global": [{"name": "vibe", "skill_id": "sid-v", "version": 1}]},
    },
    {
        "case": "project-pin-wins-over-global",
        "store": [_store_item("sid-g", 1, "shared"), _store_item("sid-p", 2, "shared")],
        "manifests": {
            "global": [{"name": "shared", "skill_id": "sid-g", "version": 1}],
            "project": [{"name": "shared", "skill_id": "sid-p", "version": 2}],
        },
    },
    {
        "case": "untrusted-project-manifest-unread",
        "trusted": False,
        "store": [_store_item("sid-g", 1, "global-one"), _store_item("sid-p", 2, "project-one")],
        "manifests": {
            "global": [{"name": "global-one", "skill_id": "sid-g", "version": 1}],
            "project": [{"name": "project-one", "skill_id": "sid-p", "version": 2}],
        },
    },
    {
        "case": "disabled-pin-hidden-but-installed",
        "disabledSkills": ["remote"],
        "store": [_store_item("sid-a", 1, "remote")],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 1}]},
    },
    {
        "case": "pin-keeps-disabled-invocation",
        "store": [
            _store_item(
                "sid-a",
                1,
                "remote",
                body="---\nname: x\ndescription: y\ndisable-model-invocation: true\n---\n\n# Explicit only",
            )
        ],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 1}]},
    },
    {
        "case": "installed-keeps-shadowed-rows",
        "tree": {
            "project": {".vibe/skills/shared/SKILL.md": skill_file("shared", "Project copy")},
            "home": {
                ".vibe/skills/shared/SKILL.md": skill_file("shared", "User copy"),
                ".agents/skills/shared/SKILL.md": skill_file("shared", "Second user copy"),
                ".vibe/skills/vibe/SKILL.md": skill_file("vibe", "Reserved name"),
                ".vibe/skills/solo/SKILL.md": skill_file("solo", "Only here"),
            },
        },
        "store": [_store_item("sid-s", 1, "shared")],
        "manifests": {
            "global": [{"name": "shared", "skill_id": "sid-s", "version": 1}],
            "project": [{"name": "shared", "skill_id": "sid-s", "version": 1}],
        },
    },
    {
        "case": "same-pin-in-both-scopes",
        "store": [_store_item("sid-a", 1, "remote"), _store_item("sid-a", 2, "remote")],
        "manifests": {
            "global": [{"name": "remote", "skill_id": "sid-a", "version": 1}],
            "project": [{"name": "remote", "skill_id": "sid-a", "version": "latest"}],
        },
    },
]


def _remote(latest: int, *, name: str, versions: dict[int, str] | None = None, **extra: Any) -> dict[str, Any]:
    bodies = versions or {version: f"# {name} v{version}" for version in range(1, latest + 1)}
    return {
        "name": name,
        "description": f"{name} from the registry",
        "latest": latest,
        "versions": {str(version): body for version, body in bodies.items()},
        **extra,
    }


SYNC_SCENARIOS: list[dict[str, Any]] = [
    {
        "case": "experiment-off-skips",
        "enabled": False,
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 1}]},
        "registry": {"sid-a": _remote(1, name="remote")},
    },
    {
        "case": "no-endpoint-skips",
        "endpoint": False,
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 1}]},
        "registry": {"sid-a": _remote(1, name="remote")},
    },
    {
        "case": "no-pins-skips-and-records-nothing",
        "registry": {"sid-a": _remote(1, name="remote")},
    },
    {
        "case": "frozen-pin-downloads",
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 1}]},
        "registry": {"sid-a": _remote(2, name="remote")},
    },
    {
        "case": "frozen-pin-on-disk-takes-the-fast-path",
        "store": [_store_item("sid-a", 1, "remote"), _store_item("sid-a", 2, "remote")],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 1}]},
        "registry": {"sid-a": _remote(2, name="remote")},
    },
    {
        "case": "latest-pin-resolves-and-prunes",
        "store": [_store_item("sid-a", 1, "remote")],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": "latest"}]},
        "registry": {"sid-a": _remote(3, name="remote")},
    },
    {
        "case": "latest-pin-gone-from-registry-keeps-disk",
        "store": [_store_item("sid-a", 2, "remote")],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": "latest"}]},
        "registry": {},
    },
    {
        "case": "latest-pin-with-nothing-anywhere",
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": "latest"}]},
        "registry": {},
    },
    {
        "case": "latest-metadata-zero-uses-item-version",
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": "latest"}]},
        "registry": {"sid-a": _remote(2, name="remote", metadataLatest=0)},
    },
    {
        "case": "registry-failure-fails-the-sync",
        "store": [_store_item("sid-a", 1, "remote"), _store_item("sid-a", 2, "remote")],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": "latest"}]},
        "registry": {"sid-a": _remote(2, name="remote", fail=True)},
    },
    {
        "case": "custom-alias-downloads-and-records",
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": "stable"}]},
        "registry": {"sid-a": _remote(3, name="remote", aliases={"stable": 2})},
    },
    {
        "case": "custom-alias-already-on-disk",
        "store": [_store_item("sid-a", 2, "remote"), _store_item("sid-a", 3, "remote")],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": "stable"}]},
        "registry": {"sid-a": _remote(3, name="remote", aliases={"stable": 2})},
    },
    {
        "case": "custom-alias-failure-keeps-recorded",
        "store": [_store_item("sid-a", 1, "remote"), _store_item("sid-a", 2, "remote")],
        "resolved": [["sid-a", "stable", 1]],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": "stable"}]},
        "registry": {"sid-a": _remote(2, name="remote", fail=True)},
    },
    {
        "case": "custom-alias-empty-body-keeps-newest",
        "store": [_store_item("sid-a", 1, "remote")],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": "stable"}]},
        "registry": {"sid-a": _remote(2, name="remote", versions={1: "# one", 2: "   "}, aliases={"stable": 2})},
    },
    {
        "case": "same-alias-twice-asks-once",
        "manifests": {
            "global": [{"name": "remote", "skill_id": "sid-a", "version": "stable"}],
            "project": [{"name": "remote", "skill_id": "sid-a", "version": "stable"}],
        },
        "registry": {"sid-a": _remote(2, name="remote", aliases={"stable": 1})},
    },
    {
        "case": "empty-body-is-skipped",
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 1}]},
        "registry": {"sid-a": _remote(1, name="remote", versions={1: "  "})},
    },
    {
        "case": "missing-version-is-skipped",
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 5}]},
        "registry": {"sid-a": _remote(2, name="remote")},
    },
    {
        "case": "global-and-project-recorded-apart",
        "manifests": {
            "global": [{"name": "alpha", "skill_id": "sid-a", "version": 1}],
            "project": [{"name": "beta", "skill_id": "sid-b", "version": "latest"}],
        },
        "registry": {"sid-a": _remote(1, name="alpha"), "sid-b": _remote(2, name="beta")},
    },
    {
        "case": "sibling-ledger-keeps-its-version",
        "store": [_store_item("sid-a", 1, "remote")],
        "ledger": {"sibling": [["sid-a", 1]]},
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 2}]},
        "registry": {"sid-a": _remote(2, name="remote")},
    },
    {
        "case": "unpinned-id-left-alone",
        "store": [_store_item("sid-a", 1, "remote"), _store_item("sid-b", 1, "other")],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 1}]},
        "registry": {"sid-a": _remote(1, name="remote")},
    },
    {
        "case": "unsafe-id-never-requested",
        "manifests": {
            "global": [
                {"name": "escape", "skill_id": "../sid-a", "version": "latest"},
                {"name": "remote", "skill_id": "sid-a", "version": 1},
            ]
        },
        "registry": {"sid-a": _remote(1, name="remote")},
    },
    {
        "case": "untrusted-session-reads-global-only",
        "trusted": False,
        "manifests": {
            "global": [{"name": "alpha", "skill_id": "sid-a", "version": 1}],
            "project": [{"name": "beta", "skill_id": "sid-b", "version": 1}],
        },
        "registry": {"sid-a": _remote(1, name="alpha"), "sid-b": _remote(1, name="beta")},
    },
    {
        "case": "dropped-pin-leaves-the-ledger",
        "store": [_store_item("sid-a", 1, "remote")],
        "ledger": {"global": [["sid-a", 1], ["sid-gone", 3]]},
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 1}]},
        "registry": {"sid-a": _remote(1, name="remote")},
    },
    {
        "case": "publish-local-pins",
        "op": "publish",
        "store": [_store_item("sid-a", 1, "alpha"), _store_item("sid-b", 2, "beta")],
        "manifests": {
            "global": [{"name": "alpha", "skill_id": "sid-a", "version": 1}],
            "project": [{"name": "beta", "skill_id": "sid-b", "version": "latest"}],
        },
        "registry": {},
    },
    {
        "case": "publish-with-nothing-pinned-clears",
        "op": "publish",
        "ledger": {"global": [["sid-a", 1]]},
        "registry": {},
    },
]


SERVICE_SCENARIOS: list[dict[str, Any]] = [
    {"case": "import-defaults-to-latest", "op": "import", "args": {"skillId": "sid-a"}, "registry": {"sid-a": _remote(2, name="Remote Skill")}},
    {"case": "import-frozen-version", "op": "import", "args": {"skillId": "sid-a", "version": 1}, "registry": {"sid-a": _remote(2, name="remote")}},
    {"case": "import-custom-alias", "op": "import", "args": {"skillId": "sid-a", "alias": "stable"}, "registry": {"sid-a": _remote(3, name="remote", aliases={"stable": 2})}},
    {"case": "import-project-scope", "op": "import", "args": {"skillId": "sid-a", "scope": "project"}, "registry": {"sid-a": _remote(1, name="remote")}},
    {
        "case": "import-project-scope-without-roots",
        "op": "import",
        "trusted": False,
        "args": {"skillId": "sid-a", "scope": "project"},
        "registry": {"sid-a": _remote(1, name="remote")},
    },
    {"case": "import-without-endpoint", "op": "import", "endpoint": False, "args": {"skillId": "sid-a"}, "registry": {"sid-a": _remote(1, name="remote")}},
    {"case": "import-empty-body", "op": "import", "args": {"skillId": "sid-a"}, "registry": {"sid-a": _remote(1, name="remote", versions={1: " "})}},
    {"case": "import-unknown-skill", "op": "import", "args": {"skillId": "sid-x"}, "registry": {}},
    {"case": "import-unnamed-uses-the-id", "op": "import", "args": {"skillId": "ab-12"}, "registry": {"ab-12": _remote(1, name="")}},
    {
        "case": "import-replaces-the-same-id",
        "op": "import",
        "args": {"skillId": "sid-a", "version": 2},
        "manifests": {"global": [{"name": "old-name", "skill_id": "sid-a", "version": 1}, {"name": "kept", "skill_id": "sid-k", "version": 1}]},
        "registry": {"sid-a": _remote(2, name="new-name")},
    },
    {
        "case": "set-version-freezes",
        "op": "setVersion",
        "args": {"name": "remote", "version": 1},
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": "latest"}]},
        "registry": {"sid-a": _remote(2, name="remote")},
    },
    {
        "case": "set-latest-tracks",
        "op": "setLatest",
        "args": {"name": "remote", "scope": "project"},
        "manifests": {"project": [{"name": "remote", "skill_id": "sid-a", "version": 1}]},
        "registry": {"sid-a": _remote(2, name="remote")},
    },
    {
        "case": "set-alias-follows",
        "op": "setAlias",
        "args": {"name": "remote", "alias": "stable"},
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 2}]},
        "registry": {"sid-a": _remote(2, name="remote", aliases={"stable": 1})},
    },
    {
        "case": "set-version-unknown-name",
        "op": "setVersion",
        "args": {"name": "absent", "version": 1},
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 2}]},
        "registry": {"sid-a": _remote(2, name="remote")},
    },
    {
        "case": "set-version-other-scope-only",
        "op": "setVersion",
        "args": {"name": "remote", "version": 1, "scope": "project"},
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 2}]},
        "registry": {"sid-a": _remote(2, name="remote")},
    },
    {
        "case": "remove-global-only",
        "op": "remove",
        "args": {"name": "remote"},
        "manifests": {
            "global": [{"name": "remote", "skill_id": "sid-a", "version": 1}],
            "project": [{"name": "remote", "skill_id": "sid-a", "version": 1}],
        },
    },
    {
        "case": "remove-project-only",
        "op": "remove",
        "args": {"name": "remote", "scope": "project"},
        "manifests": {
            "global": [{"name": "remote", "skill_id": "sid-a", "version": 1}],
            "project": [{"name": "remote", "skill_id": "sid-a", "version": 1}],
        },
    },
    {"case": "remove-missing", "op": "remove", "args": {"name": "absent"}, "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 1}]}},
    {
        "case": "convert-global-frozen",
        "op": "convertLocal",
        "args": {"name": "remote"},
        "store": [_store_item("sid-a", 1, "remote", body="---\nname: q\ndescription: r\ndisable-model-invocation: true\n---\n\n# Kept body")],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 1}]},
    },
    {
        "case": "convert-project-latest",
        "op": "convertLocal",
        "args": {"name": "remote", "scope": "project"},
        "store": [_store_item("sid-a", 1, "remote"), _store_item("sid-a", 2, "remote")],
        "manifests": {"project": [{"name": "remote", "skill_id": "sid-a", "version": "latest"}]},
    },
    {
        "case": "convert-not-materialized",
        "op": "convertLocal",
        "args": {"name": "remote"},
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 1}]},
    },
    {
        "case": "convert-local-already-exists",
        "op": "convertLocal",
        "args": {"name": "remote"},
        "localSkill": "remote",
        "store": [_store_item("sid-a", 1, "remote")],
        "manifests": {"global": [{"name": "remote", "skill_id": "sid-a", "version": 1}]},
    },
    {
        "case": "check-updates",
        "op": "checkUpdates",
        "manifests": {
            "global": [
                {"name": "behind", "skill_id": "sid-a", "version": 1},
                {"name": "tracking", "skill_id": "sid-b", "version": "latest"},
                {"name": "shared", "skill_id": "sid-c", "version": 1},
                {"name": "current", "skill_id": "sid-d", "version": 2},
            ],
            "project": [{"name": "shared", "skill_id": "sid-c", "version": 3}],
        },
        "registry": {
            "sid-a": _remote(3, name="behind"),
            "sid-b": _remote(4, name="tracking"),
            "sid-c": _remote(2, name="shared"),
            "sid-d": _remote(2, name="current"),
        },
    },
    {
        "case": "check-updates-registry-failure",
        "op": "checkUpdates",
        # One pin only: the reference resolves its ids from a set, so with two
        # the failing one may or may not be asked after the other.
        "manifests": {"global": [{"name": "broken", "skill_id": "sid-b", "version": 1}]},
        "registry": {"sid-b": _remote(2, name="broken", fail=True)},
    },
    {
        "case": "check-new-versions-records",
        "op": "checkNewVersions",
        "manifests": {"global": [{"name": "behind", "skill_id": "sid-a", "version": 1}, {"name": "seen", "skill_id": "sid-b", "version": 1}]},
        "seen": {"sid-b": 2},
        "registry": {"sid-a": _remote(3, name="behind"), "sid-b": _remote(2, name="seen")},
    },
    {
        "case": "catalog",
        "op": "catalog",
        "registry": {
            "sid-a": _remote(3, name="Alpha Skill", sharingScope="workspace"),
            "sid-b": _remote(1, name="beta", metadataLatest=0),
        },
    },
    {"case": "catalog-without-endpoint", "op": "catalog", "endpoint": False, "registry": {"sid-a": _remote(1, name="alpha")}},
    {"case": "versions", "op": "versions", "args": {"skillId": "sid-a"}, "registry": {"sid-a": _remote(3, name="alpha", aliases={"stable": 2, "main": 2})}},
    {"case": "detail", "op": "detail", "args": {"skillId": "sid-a", "version": 2}, "registry": {"sid-a": _remote(3, name="alpha", aliases={"stable": 2}, createdBy="someone", notes="n")}},
    {"case": "detail-missing", "op": "detail", "args": {"skillId": "sid-x"}, "registry": {}},
]
