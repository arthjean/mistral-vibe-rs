# Mistral Vibe RS

**An independent, from-scratch Rust implementation of [Mistral Vibe](https://github.com/mistralai/mistral-vibe).**

Mistral Vibe RS ships the `vibe` and `vibe-acp` binaries as native executables
for Linux, macOS, and Windows, with no Python runtime to install. Its goal is
functional parity with the Python reference at every public boundary: commands,
flags, configuration, protocols, persisted state, tool semantics, and
user-visible output.

## Status

| | |
|---|---|
| Parity against Mistral Vibe v3.1.0 | **40.7/100** Parity and **38.2/100** Conformance, weighted over 43 parts ([scorecard](docs/parity.md)) |
| Parts at 100 | 4 of 43: worktrees, trusted folders and permissions, checkpoints, and setup. The scorecard explains the drop from 98.7 at v2.26.0 ([what the re-pin moved](docs/parity.md#what-the-re-pin-moved-v2260-to-v310)) |
| Reference pin | [`c96a6db`](https://github.com/mistralai/mistral-vibe/releases/tag/v3.1.0), the v3.1.0 tag, upstream's newest release as of 2026-11-08 |
| Default backend | The Unified Harness, the reference's only backend since v3.0.0. `--legacy-harness` selects this port's previous one |
| Platforms | Release targets for Linux (x86_64, aarch64), macOS (x86_64, aarch64), and Windows (x86_64). CI runs every gate on Linux and Windows |
| Releases | None published yet. Build from source below |

## Performance

Measured on 2026-10-20, Windows 11 on a Ryzen 7 7800X3D, against Mistral Vibe
2.26.0 installed in its own `uv` environment. Each figure is the median of 20
runs after 3 warm-up runs, with `VIBE_HOME` pointed at an empty directory.

| | Mistral Vibe RS | Mistral Vibe 2.26.0 |
|---|---|---|
| `vibe --version` | 10.2 ms | 172.5 ms |
| `vibe --help` | 10.3 ms | 173.8 ms |
| Install footprint | 203 MiB: `vibe` (104 MiB) and `vibe-acp` (99 MiB), each embedding V8 for the Unified Harness sandbox | 281 MiB Python environment |

## How parity is measured

Differential oracles drive the Python reference and this port with the same
inputs and compare what each one emits: the CLI surface and programmatic mode,
the terminal interface, the app-server and ACP protocols, the tool schemas and
their results, MCP servers, connectors, hooks, compaction, checkpoints,
sessions, plugins, and the Unified Harness. The drivers live in
[`scripts/parity/`](scripts/parity) and
[`crates/vibe-cli/tests/runtime-parity/`](crates/vibe-cli/tests/runtime-parity).

Their captures are committed as normalized corpora that `cargo test` replays on
every run, so a regression fails CI without the reference installed. When a
checkout of the reference sits at the pinned commit, the tests also probe it
live. Every divergence kept on purpose is recorded with the code that holds it
in place, and [`docs/parity.md`](docs/parity.md) scores each of the 36 parts
with what its score rests on.

## Build from source

Requires Rust 1.97 or later. On Linux, the voice stack links against ALSA
(`libasound2-dev` on Debian and Ubuntu).

```sh
git clone https://github.com/arthjean/mistral-vibe-rs
cd mistral-vibe-rs
cargo install --locked --path crates/vibe-cli
cargo install --locked --path crates/vibe-acp
vibe
```

`vibe` reads the same `~/.vibe` home, configuration, and sessions as the Python
release, so an existing setup carries over.

## Architecture

The workspace is layered, and a crate never depends on a later layer:

1. `vibe-protocol` (JSON-RPC envelopes and the routed method inventory) and
   `vibe-core` (provider-neutral contracts: engine, tools, config, storage,
   policy, process, platform)
2. `vibe-harness`, the Unified Harness core: session state, the turn engine,
   the code-mode sandbox, and the approval gate
3. `vibe-app-server` (session lifecycle and method dispatch) and `vibe-voice`
   (recording, playback, realtime transcription, and speech)
4. `vibe-cli` and `vibe-acp`, thin adapters that build the two binaries

## How it is built

This port is developed by orchestrating coding agents, held in check by the
oracles above and by the CI gates: `rustfmt`, `cargo check`, Clippy with
warnings denied, and the full test suite on Linux and Windows. The reference
itself, through those oracles, is the judge of whether a change is correct.

## Relationship to Mistral Vibe

[Mistral Vibe](https://github.com/mistralai/mistral-vibe) is created and
maintained by Mistral AI under the Apache License 2.0. It serves here only as a
behavioral reference: no upstream implementation source is copied, translated,
vendored, linked, or shipped, as [`NOTICE`](NOTICE) states.

Mistral Vibe RS is not an official Mistral AI project and is not affiliated
with or endorsed by Mistral AI. "Mistral" and "Mistral Vibe" are names of
Mistral AI.

## License

[Apache License 2.0](LICENSE).
