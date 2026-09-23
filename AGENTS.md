# AGENTS.md

Mistral Vibe RS is an independent Rust reimplementation of Mistral Vibe. The
current objective is functional parity with the Python reference at public
boundaries: commands, flags, configuration, protocols, persisted state, tool
semantics, and user-visible output. Internal design is free to differ where Rust
offers a stronger one.

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

The Python reference is a separate checkout, pinned in exactly two places:
`vibe_core::parity::REFERENCE_COMMIT` (`crates/vibe-core/src/parity.rs`) and
`EXPECTED_COMMIT` in `scripts/parity/pin.py`.
`crates/vibe-core/src/parity/parity_tests.rs` fails when a third copy appears or
the two disagree. The checkout defaults to `/home/arthur/dev/mistral-vibe`;
`VIBE_REFERENCE` relocates it (on Windows it lives at `C:\dev\mistral-vibe`), and
a capture script's `--reference` wins over both. Reference paths in comments and
documentation use the Linux form; read them relative to the local checkout.

- Keep the checkout's tracked files at the pin. It accepts two writes:
  `vibe_core::parity::RESTORE_COMMAND` when it sits at another commit (restore
  it rather than re-pinning by accident), and `uv sync --frozen` after a pull or
  re-pin, which rebuilds its gitignored environment and native harness. It is an
  oracle once `.venv/bin/python -c "import vibe.cli.entrypoint"` succeeds there.
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
- Capture behavior with the oracle that owns the surface: an entry point in
  `scripts/parity/`, which takes `--reference <path>` and re-executes itself
  under the reference interpreter, or the `*-oracle.py` beside its corpus in
  `crates/vibe-cli/tests/runtime-parity/`.
- Rust parity tests replay committed corpora unconditionally and gate only the
  live probe on `vibe_core::parity::off_pin_reason`, so a missing or off-pin
  checkout skips instead of failing `cargo test`. A new parity test resolves
  the checkout through `vibe_core::parity::reference_root` and gates the same
  way.
- A committed corpus changes only in a re-pin, which edits both pin sources and
  regenerates every corpus in the same change. A replay that drifts outside a
  re-pin is a regression or an environment leak (see Quality gates); fix the
  cause and leave the corpus as captured.
- State a parity claim only from a measurement against the reference, run wide
  enough to cover what changed: filtering `cargo test` to the edited module
  hides assertions elsewhere that read the same fixture. A run that printed
  `skipping the live ... probe` measured the committed corpora only; report it
  that way.

`docs/parity.md` is the scorecard: one numbered row per part, the open
divergences, and the ledger of divergences kept on purpose. Read a part's row
before working on it. Work that moves a part's parity restates its row and the
header's weighted total in the same change (`scorecard_tests` recomputes the
total), and a divergence kept on purpose gets a ledger row naming what holds it
in place (`ledger_tests` resolves every path and symbol the row names).

## Quality gates

Run the CI sequence from the workspace root before proposing a commit:

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
env -u FORCE_COLOR VIBE_HOME="$(mktemp -d)" cargo test --workspace --all-features --no-fail-fast
```

`--all-features` is load-bearing: `vibe-app-server`'s `test-fixtures` feature
gates the fixture binary that `tests/mcp_stdio_e2e.rs` drives, so the file
compiles to nothing without it. Building needs the ALSA headers that `cpal`
links against (`libasound2-dev` on Debian and Ubuntu).

The test line removes two workstation inputs CI never has. The in-process
app-server tests read the ambient vibe home, so a real `~/.vibe/config.toml`
fails them. The live parity probes spawn the reference with the inherited
environment, so an exported `FORCE_COLOR` makes it emit ANSI that no corpus
holds. The opposite leak hides failures: credentials resolve from
`MISTRAL_API_KEY`, then the OS keyring. To reproduce a failure seen only on the
runner, add `env -u MISTRAL_API_KEY DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent
CI=true VIBE_REFERENCE=/nonexistent`, which removes the key, the keyring, and
the reference checkout as the runner lacks them.

## Architecture

`[workspace.metadata.vibe] dependency-layers` in `Cargo.toml` declares the
layering. No test enforces it, so check any new dependency edge by hand: a
crate never depends on a crate in a later layer.

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

`Cargo.toml` and `clippy.toml` carry the lint set; these are the parts they
cannot state.

- A new crate declares `[lints] workspace = true` and starts its root with
  `#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]`, which
  works around Clippy issue 13981 for integration tests.
- In non-test code reach for `?`, `ok_or(...)?`, `unwrap_or`, `match`, or
  `if let`; reserve `expect("stated invariant")` for a documented invariant with
  no better boundary.
- Unit and differential tests live beside the code they cover as
  `#[cfg(test)] mod <name>_tests;` files under `src/`; an inline `mod tests`
  block is the older form. `tests/` holds integration entry points, fixture
  binaries, and corpus files.

## US English

Write every repository artifact in US English: code, comments, documentation,
and commit messages (`color`, `behavior`, `normalize`, `modeled`, `afterward`).
A spelling the reference or a dependency publishes is reproduced verbatim:
`cancelled` is the value the Python reference emits for `TodoStatus`, the stop
reason ACP declares, and the spelling of tokio's
`CancellationToken::is_cancelled`, so it stays British everywhere it names that
concept.

## Delivery

- `[workspace.package] version` is the source of truth. A bump also edits every
  hand-written copy in the same change: `action.yml`,
  `.github/workflows/action.yml`, `scripts/install.sh`, `scripts/install.ps1`,
  and the heading of `crates/vibe-cli/whats_new.md`.
  `every_hand_written_version_matches_the_workspace_manifest` in
  `crates/vibe-cli/src/distribution/release_parity_tests.rs` fails on a copy
  that disagrees and on a carrier that stops carrying the version.
- Commit with Conventional Commits, scoped by crate when the change stays in
  one: `fix(core):`, `refactor(app-server):`, `test(cli):`, `docs(protocol):`,
  `perf(acp):`, `ci:`. Imperative, lowercase, no trailing period.
- Record user-visible changes under `## Unreleased` in `CHANGELOG.md`, and
  rewrite `crates/vibe-cli/whats_new.md` for a release.

## Agent-local files

`.claude/` and `.codex/` are gitignored, so anything placed there is private to
one machine. Durable repository rules belong in this file or in a nested
`AGENTS.md`.
