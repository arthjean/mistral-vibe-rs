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
- A user-visible message the reference authored (an error, a notice, a label)
  is rewritten in original words that keep its cause, value and limit, and
  ledgered under "Accepted divergences" in `docs/parity.md`, as the precedents
  there are.
- The clean-room guard, `crates/vibe-core/src/parity/licensing_tests.rs`, fails
  on any tracked run of 8 or more words of prose shared with the pinned
  reference, against the digests `python3 scripts/parity/repin.py shingles
  <sha>` writes under `crates/vibe-core/tests/licensing/`, less the
  third-party layer it writes beside them: the digests a distribution the
  reference's `uv.lock` resolves from an index, a Git repository or a URL
  also ships, which the reference only quotes. A run a library authored passes
  through that layer, never through a hand-kept exemption. Its baseline lists
  the runs known when it landed and only shrinks: fix a new run by rewriting
  it, never by adding a line. During a rolling re-pin the baseline stays cut
  at `PREVIOUS_COMMIT`, and a run the target finds passes only when it re-cuts
  a listed run, covering some of the same words of the same file and fewer
  than eight words outside them.

## The behavioral oracle

The Python reference is a separate checkout, pinned in exactly two places:
`vibe_core::parity::REFERENCE_COMMIT` (`crates/vibe-core/src/parity.rs`) and
`EXPECTED_COMMIT` in `scripts/parity/pin.py`, each beside the
`PREVIOUS_COMMIT` a rolling re-pin leaves (empty outside one).
`crates/vibe-core/src/parity/parity_tests.rs` fails when a third copy of either
commit appears or the two sources disagree. The checkout defaults to `/home/arthur/dev/mistral-vibe`;
`VIBE_REFERENCE` relocates it (on Windows it lives at `C:\dev\mistral-vibe`), and
a capture script's `--reference` wins over both. Reference paths in comments and
documentation use the Linux form; read them relative to the local checkout.

- The checkout is only an object store, at any `HEAD`: never check out or
  `uv sync` inside it, and `git fetch` there only to bring in a commit to
  measure. Captures and live probes run an environment materialized per commit,
  `python3 scripts/parity/repin.py materialize <sha>` (`--accepted` for every
  commit the manifest accepts), which exports the commit with `git archive`,
  builds it with `uv sync --locked --no-editable` under
  `.parity/reference-<sha12>/` and writes `attestation.json`;
  `scripts/parity/materialize.py` documents the layout. `repin.py attest <sha>`
  checks it, and an environment that drifted is rebuilt, never trusted. On
  Windows the build needs short paths, which `.parity/` under this repository
  gives.
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
  `crates/vibe-cli/tests/runtime-parity/`. `scripts/parity/reference.py` is
  the one place that resolves and checks the attested environment and
  re-executes a driver; a driver never
  imports another driver, only shared modules and its own `<driver>_cases.py`
  or `<driver>_scenarios.py` data table. A table holds pure data and imports
  neither the driver nor `vibe`; a scenario written as a closure that drives
  the reference stays in its driver, however long that makes the driver.
- Rust parity tests replay committed corpora unconditionally and gate only the
  live probe, on `vibe_core::parity::live::gate`: a host other than Linux,
  where the corpora are captured, and a missing environment skip, and one that
  no longer matches its attestation is refused as environment drift, all
  without failing `cargo test`; a probe whose oracle is the host's own
  platform gates through `gate_native` instead. Every skip is recorded in
  `target/parity/skips.jsonl`. `VIBE_PARITY_REQUIRE_LIVE=1` turns a skip into a
  failure. A new parity test builds on `vibe_core::parity::replay` (the
  `parity-replay` feature of a dev-dependency on `vibe-core`), whose `Replay`
  compares and ledgers a corpus and whose `Capture::probe` and
  `pinned_reference` pass through that gate.
- A replay's ledger is bounded and typed. Two prose digests that differ are a
  difference unless an `OwnProse` entry names their pointer and finds the
  port's length within its band; an entry that restates reference directives
  records the commit it was audited at. Every entry declares how many
  differences it covers, matches pointers on segment boundaries, names a
  scorecard row and what it stands on (`Class::Accepted` with the name of an
  "Accepted divergences" row, or `Class::Gap`), and fails the replay when its
  count moves. A `Class::Gap` entry names another row: a gap of the row its
  corpus measures is a measurement, never an entry. It lives in the corpus's
  measured debt under `scripts/parity/debt/`, which only
  `python3 scripts/parity/repin.py debt <corpus>` writes (on Linux, from runs
  that agree) and the manifest records against the corpus SHA-256. A
  difference outside the debt fails the replay, and so does a debt group that
  moves, so an unchanged corpus's debt only shrinks; a `Note` explains a debt
  group and counts nothing. An entry over a whole top-level observation lists
  the sub-pointers it may cover. Each replay, `Tally` or hand-rolled, ends with
  `vibe_core::parity::figures::assert_figures` against its line of the
  "Measured figures" table in `docs/parity.md`; a run appends what it counted
  to `target/parity/figures.jsonl`, the line to restate when one moves.
- `scripts/parity/manifest.json` lists every committed corpus with its driver,
  test, scorecard row, capture commit, SHA-256 and floors, and
  `python3 scripts/parity/repin.py capture <corpus>` is the only way to change
  one: `crates/vibe-core/src/parity/manifest_tests.rs` fails on a corpus that
  is unlisted, edited by hand, off the accepted commits, or below its floors.
  `repin.py list` prints the regeneration commands and `repin.py verify` runs
  the checks without Rust. A driver run against this port (`--server`,
  `--agent`, `--binary`, `--live-binary`, `--vibe`, `--vibe-acp`) needs an
  `--output` outside the manifest paths, and an off-pin capture runs only
  through `repin.py capture --commit <sha>`.
- A corpus changes only through `repin.py capture`. A re-pin is rolling: it
  moves both pin sources to the target, names the commit it leaves as
  `PREVIOUS_COMMIT` in both, and lists in the manifest's `transition` every
  corpus not yet recaptured, which may keep that commit while the list names
  it. A capture at the target drops its corpus from the list, a live probe
  gates on the commit its test file's corpora record, and the transition
  closes by clearing `PREVIOUS_COMMIT` once the list is empty
  (`manifest_tests` fails on an open, stale or inconsistent transition).
  `repin.py plan <sha>` prints the recapture order with the story owning each
  driver (`scripts/parity/repin_plan.json`). A replay that drifts outside a
  recapture is a regression or an environment leak (see Quality gates); fix
  the cause and leave the corpus as captured.
- State a parity claim only from a measurement against the reference, run wide
  enough to cover what changed: filtering `cargo test` to the edited module
  hides assertions elsewhere that read the same fixture. A run that printed
  `skipping the live ... probe` measured the committed corpora only; report it
  that way. `python3 scripts/parity/live_summary.py` counts what a run
  measured, and a live parity claim comes from a Linux run with
  `VIBE_PARITY_REQUIRE_LIVE=1` (Windows probes answer the reference's Windows
  behavior, which Linux-captured corpora do not hold). The scheduled
  `.github/workflows/parity-live.yml` is that run (it also recaptures, through
  `repin.py check --unprobed`, every driver no live probe calls), and the daily
  `upstream-sentinel.yml` reports how far the pin is behind upstream
  (`repin.py census`).

`docs/parity.md` is the scorecard: one numbered row per part, the open
divergences, and the ledger of divergences kept on purpose. Read a part's row
before working on it. Work that moves a part's parity restates its figures
lines, its row and the header's weighted total in the same change:
`scorecard_tests` derives each row's Parity and Conformance from the
"Measured figures" lines and recomputes the total, and a row no replay
measures reads `structural` and stays at or below 50. A divergence kept on
purpose gets a ledger row naming what holds it in place (`ledger_tests`
resolves every path and symbol the row names).

## Quality gates

Run the CI sequence from the workspace root before proposing a commit:

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
home="$(mktemp -d)"
env -u FORCE_COLOR VIBE_HOME="$home" cargo test --workspace --all-features --no-fail-fast
chmod -R u+w "$home" && rm -rf "$home"
```

`--all-features` is load-bearing: `vibe-app-server`'s `test-fixtures` feature
gates the fixture binary that `tests/mcp_stdio_e2e.rs` drives, so the file
compiles to nothing without it. Building needs the ALSA headers that `cpal`
links against (`libasound2-dev` on Debian and Ubuntu).

The test lines remove two workstation inputs CI never has. The in-process
app-server tests read the ambient vibe home, so a real `~/.vibe/config.toml`
fails them. The live parity probes spawn the reference with the inherited
environment, so an exported `FORCE_COLOR` makes it emit ANSI that no corpus
holds. The opposite leak hides failures: credentials resolve from
`MISTRAL_API_KEY`, then the OS keyring. To reproduce a failure seen only on the
runner, add `env -u MISTRAL_API_KEY DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent
CI=true`, which removes the key and the keyring as the runner lacks them. The
runner also holds no materialized reference environment, so on a Linux
workstation that materialized one, move `.parity/reference-*` aside as well:
every live probe then skips as it does there. The tests write logs and
trust state into that throwaway home and harden some of its directories, so the
last line restores write access before removing it.

CI runs the same four gates on `windows-2022` from Git Bash, with `python3` on
`PATH` for the parity drivers. Its test gate runs through
`cargo nextest run --profile ci` (one process per test, a hung test ended after
five minutes; see `.config/nextest.toml`) followed by `cargo test --doc`. A test that fails there because of a product
defect a story of `tasks/prd-windows-native-support.md` owns carries
`#[cfg_attr(windows, ignore = "windows: <cause> (US-NNN)")]`, and the job
summary counts those tags; a test that cannot run on Windows at all says why
in an ignore reason starting `windows by design:`. A test that clears the child
environment keeps the Windows base set (`SystemRoot` above all, without which
Winsock fails to start), as `crates/vibe-cli/tests/support` and
`scripts/parity/host_env.py` do.

## Architecture

`[workspace.metadata.vibe] dependency-layers` in `Cargo.toml` declares the
layering. No test enforces it, so check any new dependency edge by hand: a
crate never depends on a crate in a later layer.

1. `vibe-protocol`, `vibe-core`
2. `vibe-harness`
3. `vibe-app-server`, `vibe-voice`
4. `vibe-cli`, `vibe-acp`

- `vibe-protocol` owns the JSON-RPC envelopes, the routed method inventory, and
  the `initialize` payloads, and nothing else. Every envelope struct denies
  unknown fields, which is what lets the untagged `Envelope` discriminate its
  variants; relaxing that makes variant declaration order silently load-bearing.
- `vibe-core` owns provider-neutral contracts: engine, tools, config, storage,
  policy, process, and platform.
- `vibe-harness` owns the Unified Harness core a unified session runs on: its
  state and projection, the turn engine, the code-mode sandbox, the approval
  gate contract, and the session store. What a host provides (MCP servers,
  connectors, approvals) reaches it through traits, never a crate edge.
- `vibe-app-server` owns session lifecycle and method dispatch.
- `vibe-voice` owns the audio stack both adapters drive: the recorder, the
  player, the realtime transcription and speech clients, and the voice and
  narrator managers.
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
