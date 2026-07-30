# Release 4 terminal stack decision

Status: accepted for US-033 on 2026-07-30.

The selected stack is Ratatui 0.29.0 with crossterm 0.28.1. Ratatui supplies immutable render-state inputs, `TestBackend` buffer assertions, viewport resizing, Unicode-aware cells, and a cross-platform crossterm backend without raising the workspace Rust 1.85 minimum. Ratatui 0.30.x requires Rust 1.88 and is outside the current compatibility requirement.

The measured alternative is a direct crossterm renderer. It provides the same input, resize, mouse, and raw-terminal primitives, but it requires a project-owned layout and screen-diff model and has no equivalent integration-test buffer. Termion is not viable because its documented platform set excludes Windows.

One `TerminalGuard` owns raw mode, alternate screen, mouse capture, bracketed paste, and cursor visibility. Setup rolls back completed steps. Restoration attempts every active step in reverse order on normal exit, panic unwinding, cancellation, SIGINT, terminal loss, and nested errors. Cleanup errors remain diagnostic and never prevent later cleanup steps.

Native certification remains in EP-006 for real signal delivery, legacy Windows consoles, terminal loss, platform clipboard behavior, and image protocols.
