#!/usr/bin/env python3
"""The pinned Python reference the capture scripts measure against.

This is the Python half of the pin. The Rust half is
``vibe_core::parity::REFERENCE_COMMIT``, and
``crates/vibe-core/src/parity/parity_tests.rs`` fails when the two disagree, so
a re-pin is one edit per language rather than eleven scattered constants that
nothing forces to agree.

A re-pin is rolling: it moves :data:`EXPECTED_COMMIT` (here and in Rust) to
the target, names the commit it leaves in :data:`PREVIOUS_COMMIT`, and lists in
the manifest's ``transition`` every corpus not yet recaptured at the target.
Each ``repin.py capture`` at the target drops its corpus from the list, and the
transition ends when the list is empty and :data:`PREVIOUS_COMMIT` is cleared.

Oracles that live outside ``scripts/parity`` import this module by inserting
this directory on ``sys.path``.
"""

from __future__ import annotations

import os
from pathlib import Path

#: The target of the pin: the commit every corpus records outside a transition.
EXPECTED_COMMIT = "c96a6dbb34d34f8a868b3f67b279ecabbf1aa616"

#: The commit a re-pin leaves, still accepted for the corpora the manifest's
#: ``transition`` list names; empty outside a transition. The Rust half is
#: ``vibe_core::parity::PREVIOUS_COMMIT``.
PREVIOUS_COMMIT = ""

#: The package version :data:`EXPECTED_COMMIT` publishes.
EXPECTED_VERSION = "3.1.0"


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
