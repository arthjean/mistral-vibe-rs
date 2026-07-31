# Chat-input parity corpus

Canonical traces recorded from the pinned Python reference. They define chat
input parity as reproducible observations instead of interpretation.

## Layout

| File | Owner | Purpose |
|---|---|---|
| `manifest.json` | generated | schema version, pinned reference revision, workspace fixtures, audited gaps, trace index, traces skipped for a missing capability |
| `traces/<id>.json` | generated | initial state, normalised events, and one state / effect / render / history observation per event |
| `expectations.json` | hand-owned | per trace and per dimension, whether Rust is expected to match the reference today |

## Recording

The reference checkout is read-only; the harness only drives and observes it.

```console
scripts/parity/oracle.py --expect-commit <full-sha>
```

Generation refuses to write anything when the reference checkout is missing,
has uncommitted tracked changes, or is not the expected revision. A scenario
whose capability is unavailable on the host (for example macOS clipboard
images) is never written as an authoritative fixture: it is declared under
`unavailable` so the runner reports an explicit hole instead of a pass.

## Replaying

```console
cargo test -p vibe-cli --test chat_input_parity
```

The runner replays every trace through `tui::chat_input`. The oracle records
four dimensions; the runner compares the two the Rust composer can answer for
today:

- `state` (compared): text, mode, cursor, selection, completion candidates,
  history position.
- `effects` (compared): the ordered effects both implementations expose
  (submission, history, mode, completion reset, clipboard, feedback,
  recording, notifications). Effects that exist only as internal plumbing on
  one side (completion requests, paste normalisation, rejections) are not
  compared.
- `render` (deferred to US-018): composer prompt symbol, visual lines, cursor
  cell, popup rows and border chrome. Recorded now so the fixtures are ready,
  compared when the composer viewport lands.
- `history` (deferred to US-006): the persisted prompt-history file contents.
  Rust keeps history in memory only, so there is nothing to compare yet.

`expectations.json` declares one status per trace and per dimension:

| Status | Meaning |
|---|---|
| `parity` | Rust must match the reference; a divergence fails |
| `gap` | Rust is known to diverge; matching also fails, so closing a gap cannot go unrecorded |
| `deferred` | recorded by the oracle, not compared yet; the runner names the story that will |
| `unavailable` | the scenario could not be recorded on this host |

Two rules keep the contract honest:

- A trace with no expectation entry fails: nothing is ever assumed to pass.
- A dimension a trace *records* but does not *declare* fails. A recorded
  observation can never be silently dropped.

Individual fields inside `state` that Rust does not model yet are listed in
`UNMODELLED_STATE_PATHS` in the runner with the story that supplies them. They
are removed from the expected observation before comparison, so one missing
field cannot mask every other state assertion. A test fails if the composer
starts reporting one of them, which is the signal to delete the entry and turn
the field into a real assertion.

After a story closes a gap, recalibrate and review the diff:

```console
VIBE_PARITY_CALIBRATE=1 cargo test -p vibe-cli --test chat_input_parity
```

Calibration always reports a failure so it can never be mistaken for a passing
run.
