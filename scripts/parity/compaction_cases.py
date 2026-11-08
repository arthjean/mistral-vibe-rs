"""The cases ``compaction.py`` runs through the pinned reference's summary
parser.

A case names the text one call is fed. Every value here is authored for the
corpus; none is read from the reference.

Until 2.26.0 this table also scripted token counts, truncations, round drops,
envelope renders and parses, message selections and the manager's decision
tree. Those functions left the Python package at 3.0.0 (see the driver's module
docstring), so their cases went with them.
"""

from __future__ import annotations

from typing import Any


def summary_cases() -> list[dict[str, Any]]:
    return [
        {"case": "plain", "text": "<summary>done</summary>"},
        {"case": "surrounded", "text": "before <summary>done</summary> after"},
        {"case": "multiline", "text": "<summary>\nline one\nline two\n</summary>"},
        {"case": "whitespace-only", "text": "<summary>   \n  </summary>"},
        {"case": "empty-element", "text": "<summary></summary>"},
        {"case": "absent", "text": "no element at all"},
        {"case": "unclosed", "text": "<summary>never closed"},
        {"case": "two-elements", "text": "<summary>first</summary><summary>second</summary>"},
        {"case": "unclosed-then-closed", "text": "<summary>first<summary>second</summary>"},
        {"case": "close-before-open", "text": "</summary>stray<summary>real</summary>"},
        # The tag match is exact: case and attributes both defeat it.
        {"case": "uppercase-tag", "text": "<SUMMARY>loud</SUMMARY>"},
        {"case": "attribute-on-tag", "text": "<summary lang='en'>tagged</summary>"},
        # What the trim removes: CRLF and tabs, Unicode spaces, and the four
        # information separators Python counts as whitespace.
        {"case": "crlf-and-tabs", "text": "<summary>\r\n\tkept\t\r\n</summary>"},
        {"case": "unicode-spaces", "text": "<summary> 　kept </summary>"},
        {"case": "separators-around-text", "text": "<summary>\x1ckept\x1f</summary>"},
        {"case": "separators-only", "text": "<summary>\x1c\x1d\x1e\x1f</summary>"},
    ]
