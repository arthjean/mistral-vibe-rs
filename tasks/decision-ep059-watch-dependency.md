# EP-059 Watch Dependency Decision

## Status

Approved and implemented on 2026-08-11 with US-202. One new production
dependency, `notify`, enters `[workspace.dependencies]` and
`crates/vibe-cli/Cargo.toml`. No other crate takes it.

## Finding

`file_watcher_for_autocomplete` was declared at
`crates/vibe-core/src/config/registry.rs:817`, published as
`fileWatcherForAutocomplete`, and read by nothing. The completion index
rebuilt only when the workspace root changed, so a file written during a
session stayed invisible to `@` completion for the life of the process.

The reference watches the root with `watchfiles==1.2.0`
(`vibe/cli/autocompletion/file_indexer/watcher.py`, `pyproject.toml:123`) on a
daemon thread, with `step=200`, `yield_on_timeout=True`, a 0.5 second readiness
wait and a 1 second join. `watchfiles` is a thin Python wrapper over the
`notify` crate, so the platform backends and the event categories the reference
observes are the ones `notify` produces: inotify on Linux, FSEvents on macOS,
ReadDirectoryChangesW on Windows, kqueue on the BSDs.

## Alternatives considered

- **Polling the tree on a timer.** Rejected: a poll interval short enough to
  meet the PRD's one second staleness bound costs a full walk per interval on
  every workspace, which is the cost US-204 exists to remove.
- **`notify-debouncer-full`.** Rejected: a second crate, and its debouncing
  window is not the reference's. The reference's `step` is an accumulation
  window over raw events, which `WatchController` reproduces directly with a
  `recv_timeout` loop, so the debouncer would add a dependency to reimplement
  behavior that is 20 lines here.
- **The `ignore` crate's walker with a watcher.** Rejected: `ignore` is already
  a workspace dependency for `grep`, but it carries no watch backend, so it
  would not remove this addition.

## Version and features

`notify = "8.2.0"` with default features. The default set is `macos_fsevent`
alone; `crossbeam-channel` and `flume` are optional and stay off, because the
watch thread reads `std::sync::mpsc`, for which `notify` already implements
`EventHandler`. The resolved additions are `notify`, `notify-types`,
`inotify`, `inotify-sys`, `fsevent-sys`, `kqueue` and `kqueue-sys`, all
platform-gated except the first two. No TLS stack, no async runtime, and no
build-time code generation enters the graph.

## Platform consequences

Linux uses inotify, which is subject to the per-user
`fs.inotify.max_user_watches` limit; a container that has exhausted it fails at
`Watcher::watch` rather than at process start. That failure is reported once as
a diagnostic and completion keeps answering from the last built index
(`crates/vibe-cli/src/tui/completion/path.rs`, `sync_watcher`), which is the
FR-18 requirement. No new system package is needed on any platform, unlike the
ALSA headers `cpal` introduced.

A rename is reported as a pair by inotify (`IN_MOVED_FROM`, `IN_MOVED_TO`) and
mapped onto the delete-then-add pair the store already handles. Where a backend
reports a rename without naming which side a path is on, existence decides. The
mapping is asserted by `platform_events_map_onto_the_three_applied_categories`.

## Decision

The dependency and the two manifest changes are approved on the same terms as
`cpal` and `tokio-tungstenite` in `tasks/decision-ep005-voice-boundary.md`:
one crate, one layer, and a named degradation path when the platform backend is
unavailable.

## Primary sources

- Pinned Python oracle at `b78b451c39eab9213393ad2f45908e8562a5c5e7`:
  `vibe/cli/autocompletion/file_indexer/watcher.py`, `indexer.py`, `store.py`.
- `watchfiles` on `notify`: https://github.com/samuelcolvin/watchfiles
- `notify` backends and feature set: https://docs.rs/notify/8.2.0
