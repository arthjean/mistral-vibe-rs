"""The cases ``tool_execution.py`` drives the pinned reference's tools over.

A case names a tool and the arguments it is called with. Every value here is
authored for the corpus.

Since v3.0 only ``todo`` still executes inside the reference's ``vibe``
package, so only its cases remain; ``tool_execution.py`` says where every other
tool's execution went. Of the five ``todo`` cases the v2 table carried, the one
that called an action the argument schema does not declare is gone too: at v3
the harness core rejects it against the published schema before the provided
tool executor runs (`harness/core/src/core/tools/resolved/contracts.rs`).
"""

from __future__ import annotations

from typing import Any


def cases() -> list[dict[str, Any]]:
    """Every execution case, ordered by tool then by name.

    Each case runs against a fresh executor, so one case can never read
    another's list.
    """

    return [
        # -- todo --------------------------------------------------------
        {
            "tool": "todo",
            "case": "write-two",
            "args": {
                "action": "write",
                "todos": [
                    {"id": "one", "content": "first task", "status": "pending"},
                    {"id": "two", "content": "second task", "status": "in_progress"},
                ],
            },
        },
        {"tool": "todo", "case": "read-empty", "args": {"action": "read"}},
        {
            "tool": "todo",
            "case": "duplicate-identifiers",
            "args": {
                "action": "write",
                "todos": [
                    {"id": "same", "content": "first", "status": "pending"},
                    {"id": "same", "content": "second", "status": "pending"},
                ],
            },
        },
        {
            "tool": "todo",
            "case": "write-empty-list",
            "args": {"action": "write", "todos": []},
        },
    ]
