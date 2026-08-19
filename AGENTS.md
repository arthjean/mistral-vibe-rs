# AGENTS.md

Mistral Vibe RS is an independent Rust reimplementation of Mistral Vibe. The
current objective is functional parity with the Python reference at public
boundaries: commands, flags, configuration, protocols, persisted state, tool
semantics, and user-visible output. Internal design is free to differ where Rust
offers a stronger one.

Write every repository artifact in US English: code, comments, documentation,
and commit messages. `color`, `behavior`, `normalize`, `analyzer`,
`acknowledgment`, `modeled`, `afterward`. The exception is a spelling the
reference or a dependency publishes, which is reproduced verbatim: `cancelled`
is the value the Python reference emits for `TodoStatus`, the stop reason ACP
declares, and the spelling of tokio's `CancellationToken::is_cancelled`, so it
stays British everywhere it names that concept.

## Licensing boundary

`NOTICE` declares that no upstream implementation source is copied, translated,
vendored, linked, or shipped. This binds every change:

- Never paste reference source, prompt files, or tool description text into this
  repository. Reproduce observed behavior and write original prose that covers
  the same directives.
- Captured corpora that carry reference-authored text stay local and gitignored
  under `.parity/`.
- Only names, JSON pointers, and normalized observations may be committed, as in
  `crates/vibe-app-server/tests/tool-surface/baseline.json` and
  `crates/vibe-cli/tests/runtime-parity/`.
- Cite reference paths in comments and documentation instead of quoting them.

## The behavioral oracle

The Python reference is a read-only checkout outside this repository. Never
write to it. The pin lives in exactly two places, one per language:
`vibe_core::parity::REFERENCE_COMMIT` (`crates/vibe-core/src/parity.rs`) and
`EXPECTED_COMMIT` in `scripts/parity/pin.py`. Every oracle cites one of them, and
`crates/vibe-core/src/parity/parity_tests.rs` fails when a third copy appears or
when the two disagree.

The checkout location is machine-dependent: `C:\dev\mistral-vibe` on Windows and
`/home/arthur/dev/mistral-vibe` on Linux. Both pin sources default to the Linux
path and read `VIBE_REFERENCE` as an override, with `--reference` winning over
both, so every Rust parity test now honors the variable through
`vibe_core::parity::reference_root`. A new parity test calls that function
rather than spelling a path. Reference paths written in comments and
documentation use the Linux form as the canonical spelling; read them relative
to whichever checkout is local.

- Read the reference before writing Rust that touches a public boundary. Open
  the owning module first, then implement. `vibe/cli/` is the terminal client,
  `vibe/app_server/` the session methods, `vibe/acp/` the editor protocol, and
  under `vibe/core/`: `tools/` the tool surface (`base.py` for naming and schema
  emission, `manager.py` for availability and filtering, `builtins/` for the
  published tools, `mcp/` and `connectors/` for remote naming), plus `config/`,
  `session/`, `skills/`, `agents/`, and `hooks/`. A contract can reach outside
  the module that publishes it: `ask_user_question` takes its argument model
  from `vibe/questions.py` and `task` from `vibe/core/subagents.py`. Grepping
  the reference does not replace reading the declaration it points at.
- Capture behavior with `scripts/parity/oracle.py` or
  `scripts/parity/tool_surface.py`, both accepting `--reference <path>` and
  re-executing themselves with the reference interpreter.
- Rust parity tests replay committed corpora unconditionally and skip only the
  live probe when the checkout is absent or off-pin
  (`crates/vibe-cli/src/tui/runtime_parity_tests.rs:46`). Keep new parity tests
  skippable the same way: a missing checkout must never fail `cargo test`.
- Re-pinning the reference means editing the two pin sources above and
  regenerating every committed corpus in the same change. A corpus and the
  constant asserting it must never disagree. When the local checkout sits at
  another commit, restore it with the command `vibe_core::parity::RESTORE_COMMAND`
  documents rather than re-pinning by accident.
- State a parity claim only from a measurement against the reference, and run
  the measurement wide enough to cover what changed. Filtering `cargo test` to
  the module you edited hides the assertions that live elsewhere and read the
  same fixture.

## Quality gates

Run the CI sequence before proposing a commit, from the workspace root:

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

`--all-features` is load-bearing: `vibe-app-server`'s `test-fixtures` feature
gates the fixture binary that `tests/mcp_stdio_e2e.rs` drives, so the file
compiles to nothing without it. Building needs the ALSA headers that `cpal`
links against (`libasound2-dev` on Debian and Ubuntu).

## Architecture

`[workspace.metadata.vibe] dependency-layers` in `Cargo.toml` declares the
layering, and no test enforces it. A crate never depends on a crate in a later
layer:

1. `vibe-protocol`, `vibe-core`
2. `vibe-app-server`
3. `vibe-cli`, `vibe-acp`

- `vibe-protocol` owns the JSON-RPC envelopes, the routed method inventory, and
  the `initialize` payloads, and nothing else. Every envelope struct denies
  unknown fields, which is what lets the untagged `Envelope` discriminate its
  variants; relaxing that makes variant declaration order silently load-bearing.
- `vibe-core` owns provider-neutral contracts: engine, tools, config, storage,
  policy, process, and platform.
- `vibe-app-server` owns session lifecycle and method dispatch.
- `vibe-cli` builds the `vibe` binary and `vibe-acp` the `vibe-acp` binary. Both
  are adapters: shared logic belongs one layer down.

## Rust conventions

Tooling enforces the lint set; these are the parts it cannot enforce.

- A new crate declares `[lints] workspace = true` and starts its root with
  `#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]`, which
  works around Clippy issue 13981 for integration tests.
- `unsafe_code` is forbidden workspace-wide, and `panic`, `unimplemented`, and
  `dbg_macro` are denied. In non-test code reach for `?`, `ok_or(...)?`,
  `unwrap_or`, `match`, or `if let`; reserve `expect("stated invariant")` for a
  documented invariant with no better boundary.
- Unit and differential tests live beside the code they cover as
  `#[cfg(test)] mod <name>_tests;` files under `src/`. `tests/` holds integration
  entry points, fixture binaries, and corpus files.

## Delivery

- `[workspace.package] version` is the source of truth, but the string is also
  hand-written in `action.yml`, `.github/workflows/action.yml`,
  `scripts/install.sh`, `scripts/install.ps1`, and the heading of
  `crates/vibe-cli/whats_new.md`. A bump updates all of them in one change, and
  `every_hand_written_version_matches_the_workspace_manifest` in
  `crates/vibe-cli/src/distribution/release_parity_tests.rs` fails both on a copy
  that disagrees and on a carrier that stops carrying the version at all.
- Commit with Conventional Commits and a crate scope: `fix(core):`,
  `refactor(app-server):`, `test(cli):`, `docs(protocol):`, `perf(acp):`, `ci:`.
  Imperative, lowercase, no trailing period.
- Record user-visible changes under `## Unreleased` in `CHANGELOG.md`, and
  rewrite `crates/vibe-cli/whats_new.md` for a release.

## Agent-local files

`.claude/` and `.codex/` are gitignored, so anything placed there is private to
one machine. Durable repository rules belong in this file or in a nested
`AGENTS.md`.
