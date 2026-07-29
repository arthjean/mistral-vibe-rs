# CLI diagnostic formatting

The Rust CLI accepts the pinned 2.23.1 programmatic flags, preserves their
intent, reports `vibe 2.23.1`, and uses the same success and failure exit
classes. Its help layout and invalid-argument prose come from Clap rather than
Python argparse, so whitespace, wrapping, capitalization, and diagnostic
wording are intentionally not byte-identical.

This divergence excludes successful stdout payloads, JSON and streaming
schemas, flag conflicts, session intent, and exit status.
