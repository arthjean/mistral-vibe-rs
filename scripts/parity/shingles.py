"""Digests of the reference's prose, for the clean-room guard.

``NOTICE`` declares that no upstream source is copied into this repository.
The guard ``crates/vibe-core/src/parity/licensing_tests.rs`` checks it: every
tracked file is reduced to its normalized words, every run of eight
consecutive words (a *shingle*) is hashed, and a hash that the reference's
own prose also produces marks a shared run.

This module builds the reference side. It reads the pristine tree of a
materialized environment (``.parity/reference-<sha12>/source/``, checked
against its attestation first), keeps only the tokens that carry prose, and
writes the sorted, deduplicated digests of their shingles. Only digests are
committed, never the text, which keeps the file on the same footing under
``NOTICE`` as the prose digests the corpora already record.

**Prose-bearing tokens.** In a Python file: comments, and string literals
(docstrings included), each taken as the raw source text of the literal so
that an f-string reads the same on every interpreter version. Consecutive
comments form one unit, and so do adjacent string literals (implicit
concatenation). In a Rust file: line, block and doc comments, consecutive
comments again forming one unit, and string literals. A Markdown file is one
unit. A shingle never spans two units of the reference; the scanned side is
read as one stream per file, which finds every shingle a unit holds.

**Normalization**, shared word for word with the Rust guard: the two-character
escapes ``\\n``, ``\\r`` and ``\\t`` become a space, then every maximal run of
ASCII letters and digits is a word, lowercased; every other character
separates words. A shingle is eight consecutive words joined by single
spaces; its digest is the first eight bytes of the SHA-256 of that UTF-8
text, read as a big-endian 64-bit integer. The digest file holds the sorted
distinct digests as consecutive big-endian 8-byte values.

A shared module, not a driver: ``repin.py`` imports it, it imports no driver.
"""

from __future__ import annotations

import hashlib
import io
from pathlib import Path
import re
import struct
import tokenize
from typing import Iterable, Iterator

#: How many words a shingle holds.
WIDTH = 8

#: The most the digest set of one commit may weigh, in bytes.
LIMIT = 4 * 1024 * 1024

#: Where the digest sets and the baseline are committed.
LICENSING = Path(__file__).resolve().parents[2] / "crates" / "vibe-core" / "tests" / "licensing"

_ESCAPE = re.compile(r"\\[nrt]")
_WORD = re.compile(r"[A-Za-z0-9]+")
_PREFIX = re.compile(r"^[rRbBuUfFtT]{0,2}")

#: Directories of the reference tree that hold no authored source.
_SKIPPED = {".git", ".venv", "node_modules", "target", "__pycache__", "dist", "build"}


class ShingleError(RuntimeError):
    """The digest set cannot be built as asked."""


def words(text: str) -> list[str]:
    """The normalized words of ``text``."""

    return [word.lower() for word in _WORD.findall(_ESCAPE.sub(" ", text))]


def digest(shingle: Iterable[str]) -> int:
    """The 64-bit digest of one shingle."""

    return int.from_bytes(hashlib.sha256(" ".join(shingle).encode("utf-8")).digest()[:8], "big")


def shingles(unit: list[str]) -> Iterator[int]:
    """The digest of every shingle of one unit's words."""

    for start in range(len(unit) - WIDTH + 1):
        yield digest(unit[start : start + WIDTH])


def _offsets(source: str) -> list[int]:
    starts = [0]
    for line in source.splitlines(keepends=True):
        starts.append(starts[-1] + len(line))
    return starts


def python_units(source: str) -> list[str]:
    """The prose-bearing units of a Python file, as source text."""

    starts = _offsets(source)

    def position(row: int, column: int) -> int:
        return starts[row - 1] + column

    units: list[str] = []
    current: list[str] = []
    kind: str | None = None
    fstring_depth = 0
    fstring_start = 0
    neutral = {tokenize.NL, tokenize.NEWLINE, tokenize.INDENT, tokenize.DEDENT}
    fstring_start_type = getattr(tokenize, "FSTRING_START", None)
    fstring_end_type = getattr(tokenize, "FSTRING_END", None)
    tstring_start_type = getattr(tokenize, "TSTRING_START", None)
    tstring_end_type = getattr(tokenize, "TSTRING_END", None)
    opening = {value for value in (fstring_start_type, tstring_start_type) if value is not None}
    closing = {value for value in (fstring_end_type, tstring_end_type) if value is not None}

    def flush() -> None:
        nonlocal current, kind
        if current:
            units.append(" ".join(current))
        current = []
        kind = None

    def add(text: str, token_kind: str) -> None:
        nonlocal kind
        if kind != token_kind:
            flush()
            kind = token_kind
        current.append(text)

    try:
        for token in tokenize.generate_tokens(io.StringIO(source).readline):
            if token.type in opening:
                if fstring_depth == 0:
                    fstring_start = position(*token.start)
                fstring_depth += 1
                continue
            if token.type in closing:
                fstring_depth -= 1
                if fstring_depth == 0:
                    raw = source[fstring_start : position(*token.end)]
                    add(_PREFIX.sub("", raw), "string")
                continue
            if fstring_depth:
                continue
            if token.type == tokenize.COMMENT:
                add(token.string, "comment")
            elif token.type == tokenize.STRING:
                add(_PREFIX.sub("", token.string), "string")
            elif token.type in neutral:
                continue
            else:
                flush()
    except (tokenize.TokenError, SyntaxError, IndentationError) as error:
        raise ShingleError(f"cannot tokenize: {error}") from error
    flush()
    return units


def rust_units(source: str) -> list[str]:
    """The prose-bearing units of a Rust file: comments, consecutive ones
    merged, and string literals."""

    units: list[str] = []
    comment: list[str] = []
    index = 0
    length = len(source)

    def flush() -> None:
        if comment:
            units.append(" ".join(comment))
            comment.clear()

    while index < length:
        character = source[index]
        if source.startswith("//", index):
            end = source.find("\n", index)
            end = length if end < 0 else end
            comment.append(source[index:end])
            index = end
            continue
        if source.startswith("/*", index):
            depth = 0
            cursor = index
            while cursor < length:
                if source.startswith("/*", cursor):
                    depth += 1
                    cursor += 2
                elif source.startswith("*/", cursor):
                    depth -= 1
                    cursor += 2
                    if depth == 0:
                        break
                else:
                    cursor += 1
            comment.append(source[index:cursor])
            index = cursor
            continue
        if character.isspace():
            index += 1
            continue
        raw = re.match(r'(?:b|c)?r(#*)"', source[index : index + 260])
        if raw and (index == 0 or not (source[index - 1].isalnum() or source[index - 1] == "_")):
            flush()
            terminator = '"' + raw.group(1)
            begin = index + raw.end()
            end = source.find(terminator, begin)
            end = length if end < 0 else end
            units.append(source[begin:end])
            index = end + len(terminator)
            continue
        if character == '"' or (
            character in "bc"
            and source.startswith('"', index + 1)
            and (index == 0 or not (source[index - 1].isalnum() or source[index - 1] == "_"))
        ):
            flush()
            begin = index + (1 if character == '"' else 2)
            cursor = begin
            while cursor < length and source[cursor] != '"':
                cursor += 2 if source[cursor] == "\\" else 1
            units.append(source[begin:cursor])
            index = cursor + 1
            continue
        if character == "'":
            # A character literal ('x', '\n', '\u{1F600}') or a lifetime ('a).
            literal = re.match(r"'(?:\\(?:u\{[0-9A-Fa-f]+\}|x[0-9A-Fa-f]{2}|.)|[^\\'])'", source[index : index + 16])
            index += literal.end() if literal else 1
            flush()
            continue
        flush()
        index += 1
    flush()
    return units


def units(path: Path) -> list[str]:
    """The prose-bearing units of one reference file."""

    source = path.read_text(encoding="utf-8", errors="replace")
    if path.suffix == ".py":
        return python_units(source)
    if path.suffix == ".rs":
        return rust_units(source)
    return [source]


def sources(root: Path) -> list[Path]:
    """Every Python, Rust and Markdown file of the reference tree at ``root``."""

    found = []
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root)
        if any(part in _SKIPPED for part in relative.parts[:-1]):
            continue
        if path.is_file() and path.suffix in {".py", ".rs", ".md"}:
            found.append(path)
    return found


def digests(root: Path) -> tuple[list[int], int]:
    """The sorted distinct shingle digests of the reference tree at ``root``,
    and how many files were read."""

    found: set[int] = set()
    files = sources(root)
    for path in files:
        try:
            pieces = units(path)
        except ShingleError as error:
            raise ShingleError(f"{path.relative_to(root)}: {error}") from error
        for unit in pieces:
            found.update(shingles(words(unit)))
    return sorted(found), len(files)


def digest_file(commit: str) -> Path:
    """Where the digest set of ``commit`` is committed."""

    return LICENSING / f"shingles-{commit[:12]}.bin"


def encode(values: list[int]) -> bytes:
    return b"".join(struct.pack(">Q", value) for value in values)


def write(commit: str, values: list[int]) -> Path:
    """Writes the digest set of ``commit``, refusing one above :data:`LIMIT`."""

    payload = encode(values)
    if len(payload) > LIMIT:
        raise ShingleError(
            f"the digest set of {commit[:12]} would weigh {len(payload)} bytes, above the "
            f"{LIMIT}-byte limit; nothing was written. Decide the open question of "
            "tasks/prd-trustworthy-parity-v3.md (US-012) first: commit a Bloom filter over the "
            "digests instead of the sorted set, trading exactness for size"
        )
    target = digest_file(commit)
    target.parent.mkdir(parents=True, exist_ok=True)
    staged = target.with_suffix(".bin.tmp")
    staged.write_bytes(payload)
    staged.replace(target)
    return target
