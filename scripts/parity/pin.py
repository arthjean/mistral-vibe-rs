#!/usr/bin/env python3
"""The pinned Python reference the capture scripts measure against.

This is the Python half of the pin. The Rust half is
``vibe_core::parity::REFERENCE_COMMIT``, and
``crates/vibe-core/src/parity/parity_tests.rs`` fails when the two disagree, so
a re-pin is one edit per language rather than eleven scattered constants that
nothing forces to agree.

Re-pinning means changing :data:`EXPECTED_COMMIT` here, the Rust constant, and
regenerating every committed corpus in the same change.

Oracles that live outside ``scripts/parity`` import this module by inserting
this directory on ``sys.path``.
"""

from __future__ import annotations

import os
from pathlib import Path

#: The reference commit every committed corpus was captured from. A checkout at
#: any other revision is not an oracle for those corpora.
EXPECTED_COMMIT = "376f6a33413a3eec9b3795b0c0e004066c47b5c1"

#: The package version :data:`EXPECTED_COMMIT` publishes.
EXPECTED_VERSION = "2.26.0"

#: The flags a capture adds to every entry point it starts, on both sides of a
#: replay. Since v2.26.0 the reference starts on its Unified Harness unless told
#: otherwise (``vibe/_experimental_harness.py``), while this port reproduces the
#: legacy backend, so the corpora measure that backend through its escape hatch
#: and the Unified Harness is scorecard row 36. The port receives the same flags
#: so the selection a session reports stays comparable.
HARNESS_FLAGS = ("--legacy-harness",)

#: Where the read-only reference checkout lives. ``VIBE_REFERENCE`` overrides
#: the default for machines that hold it elsewhere, and ``--reference`` wins
#: over both.
DEFAULT_REFERENCE = Path(
    os.environ.get("VIBE_REFERENCE") or "/home/arthur/dev/mistral-vibe"
)

#: The single documented command that returns a checkout to the pin.
RESTORE_COMMAND = (
    f"git -C /home/arthur/dev/mistral-vibe checkout {EXPECTED_COMMIT}"
)
