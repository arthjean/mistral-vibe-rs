[PRD]
# PRD: Trustworthy Parity System and v3.1.0 Re-pin

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-11-06 | Arthur Jean | Initial draft from the verified parity audit of 2026-11-06 (`docs/parity-audit-2026-11-06.md`): every finding C1, H1 to H4, M1 to M10 and L1 to L4 and all eleven recommendations, phased into three releases |

## Problem Statement

The parity system measures Mistral Vibe RS against a pinned Python reference. The audit of 2026-11-06 verified that its in-pin machinery is sound but that its headline score no longer describes anything a user runs, and that several of its failure modes pass silently.

1. **The oracle lost its object.** Upstream v3.0.0 removed the legacy execution backend that seventeen oracles select through `--legacy-harness` (`scripts/parity/pin.py:35`), the Python Textual terminal client that eight runtime oracles and three capture drivers pilot, and the CLI modules four more import. The pin (`376f6a3`, v2.26.0) is seven releases and one month behind `c96a6db` (v3.1.0), whose app-server publishes 174 methods against the pin's 146 (audit C1).
2. **Nothing measures live, and nothing notices upstream moving.** CI has no reference checkout and no scheduled job; the workstation checkout sits at v3.1.0, so every live probe skips; the skip is an `eprintln!` a passing test hides (`crates/vibe-core/src/parity/replay.rs:241-242`); the "against upstream `HEAD`" header row is a hand-written claim dated 2026-10-08 (audit H1).
3. **A green replay can prove nothing.** 27 committed corpora are never checked against the pin (`Replay::run`, `replay.rs:698-716`); seven drivers write the committed corpus by default in port mode, recording `server-override` as its commit; a stale tool-surface corpus skips instead of failing. The gap already let a corpus sit two releases behind the pin (`crates/vibe-cli/src/tui/chat_input_parity_tests.rs:226-230`) (audit H2, M5, M6).
4. **"100" means "no unledgered structural divergence".** Multi-word text compares equal in nine replays (`blackbox.py:459-461` with `Prose::OwnWords`), the protocol row compares shapes only, whole observation subtrees are ledgered, and ledger growth is unbounded; 35 of 36 rows sit at 100, several at half conformance (audit H3, M4).
5. **The clean-room claim is contradicted.** `NOTICE` says no upstream source is copied, yet 432 runs of eight or more words are shared with the pinned reference, two reference docstrings sit verbatim in Rust doc comments, and user-visible reference messages sit verbatim in string literals and corpora, with no guard (audit H4).
6. **The reference environment is trusted, not attested.** Black-box captures run the checkout's editable working tree with only `HEAD` checked (`scripts/parity/reference.py:88-89`), the virtual environment is never compared with the pin's lockfile, and `off_pin_reason` fails open when `git` cannot run (`crates/vibe-core/src/parity.rs:72-80`) (audit M1, M2).

**Why now:** upstream's v3 line removed the backend and the client the scorecard measures, so every month of delay widens a gap the current oracles cannot even observe; the e2e PRD left the oracle alignment for "the next re-pin" (`tasks/prd-e2e-test-battery.md:882`), and that re-pin cannot be done safely with tooling that cannot tell a fresh corpus from a stale one.

## Overview

The plan rebuilds the parity system around three properties, then uses it to re-pin to v3.1.0. **Provenance by construction**: a committed manifest names every corpus with its driver, test, scorecard row, capture commit and SHA-256, and a meta-test fails on any corpus that is unlisted, off the accepted commits, or edited outside the capture tool. **Attested reference environments**: instead of requiring the shared checkout to sit at the pin, a materializer extracts any commit with `git archive`, builds its environment with `uv sync --locked --no-editable` under `.parity/`, and writes an attestation that the Rust gate checks; the checkout becomes an object store, and two pins can exist side by side. **Loud, derived measurement**: skips are recorded and can be made fatal, a scheduled no-secrets workflow runs every live probe weekly, an upstream sentinel takes a static census of the newest upstream tag daily, and every replay checks its own figures against the scorecard, which derives a Parity score (accepted divergences count) and a strict Conformance score per row.

The re-pin is rolling rather than atomic: the pin names a target commit and, while a committed transition list names it, the previous commit; each corpus records its own commit and the manifest test enforces both. The v3.1.0 measurement drops `--legacy-harness` from every capture (Unified only on both sides; the port keeps its flag as an open divergence), recaptures the thirteen black-box drivers, migrates or rewrites the in-process drivers whose reference modules were removed, and replaces the eight Textual-pilot oracles with a black-box differential runner built on the `vibe-e2e` pseudo-terminal harness, which drives upstream's Rust `vibe` and this port's `vibe` through the same keystroke scripts.

Licensing follows the precedent the accepted-divergence ledger already sets (`docs/parity.md:353`, `:372`, `:415`, `:416`): copied comments and messages are rewritten in original words and ledgered, a shingle-hash guard with a shrinking baseline keeps new copies out, and `NOTICE` stays true without amendment. The work ships in three releases: Release 1 is EP-001 to EP-003 (12 stories, at the current pin), Release 2 is EP-004 and EP-005 (10 stories, the re-pin), and Release 3 is EP-006 (7 stories). Release 3 measures the oracles themselves (mutation operators applied to captures, with a per-row sensitivity matrix) and clears the hygiene findings.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| No corpus replays without provenance | Release 1 shipped: 0 of 167 corpus files unlisted or unchecked against an accepted commit (baseline 27 unchecked) | Unchanged after every re-pin |
| Live measurement happens and is visible | At least 1 scheduled live run per week on `main`, every skip counted in the CI summary (baseline 0 runs) | 4 of 4 weekly runs green or failing on a named regression, never on the environment |
| Upstream drift is detected | A new upstream tag reported within 24 h by the sentinel (baseline never) | Pin lag at most 1 upstream release for at least 5 of 6 months |
| The scorecard measures the current reference | Release 2 shipped: target pin `c96a6db` (v3.1.0), transition list empty, every row at the target pin | Every row's Parity and Conformance derived from replay figures, 0 hand-typed ratios |
| Oracles detect what they claim to | Release 3 shipped: sensitivity matrix published for every `Replay` oracle | At least 95% of mutations detected per oracle, every undetected class ledgered with a reason |
| `NOTICE` is true | Shingle guard active with a baseline of the 432 known runs that may only shrink | Baseline 0 runs of 8 or more words outside the identifier and URL allowlist |

## Target Users

### Maintainer
- **Role:** Arthur Jean, sole contributor, owns the port, the scorecard and the re-pin decisions.
- **Behaviors:** works on Windows 11 with a reference checkout at `C:\dev\mistral-vibe`, re-pins by hand roughly once per upstream minor release, reads `docs/parity.md` to decide what to port next.
- **Pain points:** cannot tell whether a green run measured anything; re-pins take days and depend on memory of 80 driver command lines; the score says 98.7 against a product upstream no longer ships; the checkout must be moved off v3.1.0 to measure v2.26.0.
- **Current workaround:** runs parity tests with `--nocapture` to look for `skipping` lines, greps corpora for the pin hash, keeps the checkout at whatever commit was last needed.
- **Success looks like:** one command lists what a re-pin must recapture, every replay states which commit it measured, and the scorecard number changes only when a measurement does.

### Implementing agent
- **Role:** `/implement-epic` and `/review-epic` sessions that change parity code or port behavior.
- **Behaviors:** run the AGENTS.md gates, read a row of `docs/parity.md` before working on a part, recapture corpora with the driver named in a failing probe.
- **Pain points:** a probe failure says "regenerate" even when the environment drifted; ledger prefixes silently absorb new regressions; nothing fails when a reference sentence is pasted.
- **Current workaround:** trusts the summary line and the scorecard text.
- **Success looks like:** every failure names its cause class (behavior, environment, provenance, licensing) and the exact command that resolves it.

### Port user comparing with upstream
- **Role:** someone choosing between Mistral Vibe and this port, or switching between them.
- **Behaviors:** reads the README and the scorecard header.
- **Pain points:** the published score describes v2.26.0 legacy behavior, not the v3 product they run.
- **Current workaround:** none; the header row against upstream `HEAD` is stale.
- **Success looks like:** the header states the measured upstream version and how far behind the newest release it is, updated by machinery.

## Research Findings

Key findings that informed this PRD:

### Competitive Context
- uutils/coreutils: runs the GNU test suite against its binaries and publishes per-release pass counts and a trend chart in a separate tracking repository; this PRD adopts the generated dashboard and goes further with per-row strict conformance next to accepted parity.
- Deno `node_compat`: vendors one Node version's tests, keeps a reasoned `ignore` list per platform, and bumps the Node version in an explicit commit; this PRD adopts the explicit bump and the per-entry reason, and replaces the static ignore list with ledgers that fail when an entry stops reproducing or covers more differences than it declares.
- test262: published machine-generated conformance tables from daily runs against engine nightlies; this PRD adopts the scheduled run, against the pinned reference and, for the census, against the newest upstream tag.
- **Market gap:** none of these measures the sensitivity of its own oracle or attests the reference environment; a reimplementation tracked against a moving, partly closed-to-copying reference needs both.

### Best Practices Applied
- Attest the environment with `uv sync --locked` (fails when the lockfile and project disagree) rather than `--frozen` (trusts the lockfile), and install the project with `--no-editable` so the environment does not read a mutable tree (uv documentation, sync and Docker guides).
- Record skipped and flaky tests in machine-readable form: nextest JUnit with `report-skipped = "ignored"` and `flakyFailure` entries for passes on retry (nextest JUnit and retries documentation).
- Execute third-party code in CI only in a job with no secrets, a read-only token, a commit-pinned checkout, and no cache shared with pull-request workflows (GitHub Actions security guidance for `pull_request_target`).
- Keep exception lists reasoned and pruned; an over-broad allowlist is the common failure of conformance suites (Deno `node_compat` practice, audit H3).

*Full research sources: `docs/parity-audit-2026-11-06.md` and the sources it cites.*

## Assumptions & Constraints

### Assumptions (to validate)
- A `git archive` export of v2.26.0 and of v3.1.0 builds with `uv sync --locked --no-editable` into a relocated environment on Linux and Windows, including the maturin-built native harness, because the version is declared in `pyproject.toml` rather than read from git. Validated by US-004.
- The non-editable v3.1.0 wheel build bundles `vibe-rs` under `vibe/_bin/`, as the maturin `include` of `vibe/_bin/*` suggests (`pyproject.toml` at `c96a6db`); otherwise the materializer runs `cargo build --release` in `vibe/cli-rust/` as the launcher would. Validated by US-004.
- Upstream's Rust `vibe`, with its Python `vibe-app-server`, runs under the `vibe-e2e` pseudo-terminal on Linux and under ConPTY, renders the same screen across 5 runs of the same script after normalization, and its OSC sequences are observable through `vt100::Callbacks`. Validated by US-019.
- SHA-256 digests truncated to 64 bits of normalized 8-word shingles from the reference's prose-bearing tokens fit in at most 4 MB per pin and stand on the same footing under `NOTICE` as the prose digests corpora already commit. Validated by US-012.
- The thirteen black-box drivers survive v3.1.0 once `--legacy-harness` is dropped, as the exploration's driver table indicates; protocol differences beyond the 33 added and 5 removed methods surface as ledger gaps, not driver crashes. Validated by US-014.
- A daily static census (AST over `protocol.py`, `vibe_schema.py`, the tools registry, and a pattern scan of `vibe/cli-rust` argument definitions) is precise enough to flag a release as moving a public boundary without executing upstream code. Validated by US-008.

### Hard Constraints
- Licensing boundary (`NOTICE`, `AGENTS.md`): no reference source, prompt text, tool description, docstring, or reference-authored message enters this repository; corpora commit names, pointers, normalized observations, lengths and digests; reference behavior is cited by path. The upstream Rust tree became Apache-2.0 at v3.0.0 (`a0c78d0a`), which changes nothing here.
- The pin sources stay two (`vibe_core::parity::REFERENCE_COMMIT`, `EXPECTED_COMMIT` in `scripts/parity/pin.py`), held equal by `crates/vibe-core/src/parity/parity_tests.rs`; the rolling re-pin adds a previous commit to both under the same guard.
- `cargo test` never fails because a reference checkout or environment is missing; it fails only when `VIBE_PARITY_REQUIRE_LIVE=1` asks for live measurement.
- No CI job that holds secrets or a write token executes upstream code; the live workflow runs with `permissions: contents: read` and no repository secrets.
- `unsafe_code = "forbid"` workspace-wide (`Cargo.toml`); the dependency layers of `[workspace.metadata.vibe]` hold (`vibe-e2e` in layer 1).
- Product behavior is out of scope except where a story names it: the licensing rewrites of US-025 and the test-fixture placement of US-028.

## Quality Gates

These commands must pass for every user story:
- `cargo fmt --all -- --check` - formatting
- `cargo check --workspace --all-targets --all-features` - compilation of every target and feature
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` - lints with warnings denied
- `env -u FORCE_COLOR VIBE_HOME="$(mktemp -d)" cargo test --workspace --all-features --no-fail-fast` - the full workspace suite, on ubuntu-22.04 and on windows-2022 from Git Bash

From US-001 on, additional gate for every story:
- `python3 scripts/parity/repin.py verify` - every tracked corpus is listed in the manifest, at an accepted commit, and unchanged since its capture

For stories that change a capture driver, a live probe, or a corpus (from US-005 on), additional gate:
- `VIBE_PARITY_REQUIRE_LIVE=1 env -u FORCE_COLOR VIBE_HOME="$(mktemp -d)" cargo test --workspace --all-features --no-fail-fast` - against the materialized environment of every commit the change touches, on Linux; a skip fails this run

For TUI stories (US-019 to US-021), additional gate:
- A failing run of each new TUI scenario prints the normalized screens of both subjects and the first differing cell, and is reproduced once under ConPTY on Windows 11, recorded in the PR description.

## Epics & User Stories

### EP-001: Fail-Closed Corpus Provenance

Release 1. Every committed corpus is tied to its driver, test, scorecard row and capture commit, so a replay can no longer pass on a stale, partial, or self-recorded corpus.

**Definition of Done:** `repin.py verify` and the manifest meta-test pass on `main`; corrupting one corpus's commit, deleting one corpus entry, or running any driver in port mode without `--output` each fails with a message naming the corpus and the fix; the 27 previously unchecked corpora are checked.

#### US-001: Parity manifest and its meta-test
**Description:** As the maintainer, I want one committed manifest naming every corpus with its driver, arguments, test, scorecard row, capture commit and SHA-256, so that a re-pin or a recapture can be verified mechanically instead of from memory.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given the repository at `main`, when `scripts/parity/repin.py list` runs, then it prints one line per manifest entry with corpus path, driver command, test name, row and commit, and the manifest covers every tracked JSON file under `crates/*/tests/` that carries a `reference` key or a `commit` field, plus the non-corpus fixtures it lists explicitly as fixtures
- [ ] Given a corpus file tracked by git but absent from the manifest, when the workspace tests run, then a test in `crates/vibe-core/src/parity/` fails naming the file and the command that adds it
- [ ] Given a manifest entry whose corpus SHA-256 differs from the file on disk, when the workspace tests run, then the meta-test fails naming the corpus and `repin.py capture <entry>` as the only sanctioned way to change it
- [ ] Given a corpus whose recorded commit is neither `REFERENCE_COMMIT` nor an accepted transition commit, when the workspace tests run, then the meta-test fails naming both commits
- [ ] Given a driver run through `repin.py capture <entry>`, when it completes, then the corpus is written atomically and the manifest entry's SHA-256 and commit are updated in the same invocation
- [ ] Given a manifest entry whose test name no longer resolves to a `#[test]` function in the named file, when the meta-test runs, then it fails naming the entry

#### US-002: Replays fail closed on provenance, staleness and vacuity
**Description:** As the maintainer, I want every replay to assert the corpus commit and schema and to fail rather than skip on a stale or shrunken corpus, so that green means measured.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**
- [ ] Given a `Replay` corpus whose `reference.commit` is `server-override`, `agent-override`, `binary-override` or any commit outside the accepted set, when `Replay::run` executes, then it fails naming the corpus, the recorded commit and the accepted ones
- [ ] Given the tool-surface corpus captured from another commit, when its replay runs, then it fails instead of returning early (`tool_surface_parity_tests.rs:404-431`)
- [ ] Given each replay with a scenario floor, when it runs, then its floor is at least 95% of the corpus count recorded in the manifest, and `shell_policy`, `permission_surface` and `tool_config` gain floors
- [ ] Given `web_fetch_markdown`, when its live probe runs on an attested environment, then a `--check` mode recaptures the reference and fails on any differing page
- [ ] Given a corpus whose case list is empty, when any replay reads it, then it fails before comparing

#### US-003: Port-mode captures cannot overwrite a committed corpus
**Description:** As an implementing agent, I want a driver pointed at this port's binary to refuse writing a manifest-listed path, so that one forgotten `--output` cannot turn a corpus into a recording of the port.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**
- [ ] Given any driver run with `--server`, `--agent`, `--binary` or `--live-binary` and no `--output`, when it starts, then it exits non-zero before capturing, naming the flag and telling the caller to pass `--output` outside the manifest paths
- [ ] Given a port-mode run whose `--output` resolves to a manifest-listed path, when it starts, then it exits non-zero naming the corpus
- [ ] Given a reference capture with `--expected-commit ''` or `--allow-unpinned`, when it runs outside `repin.py`, then it exits non-zero; through `repin.py capture --commit <sha>` it records that commit and the manifest marks the entry as captured off-target
- [ ] Given the Rust `Capture::port` path, when a replay runs, then it still passes `--output` under the target directory and the replay is unaffected

---

### EP-002: Attested Live Measurement and Upstream Tracking

Release 1. Live probes run against an environment proven to be the pinned commit, on a schedule, loudly; and upstream releases are detected within a day.

**Definition of Done:** the scheduled `parity-live` workflow has run green or red-on-behavior twice on `main`; a deliberately dirtied or desynchronized environment is refused with an environment-class message; the sentinel has reported the current gap between the pin and the newest upstream tag in a job summary.

#### US-004: Validate assumption: pinned environments materialize from an archive on Linux and Windows
**Description:** As the maintainer, I want evidence that `git archive` plus `uv sync --locked --no-editable` builds a working reference environment for v2.26.0 and v3.1.0 on both platforms, so that the materializer design rests on measurement.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given exports of `376f6a3` and `c96a6db` under a scratch directory, when `uv sync --locked --no-editable` runs with `UV_PROJECT_ENVIRONMENT` pointing outside the tree, then `vibe-app-server`, `vibe-acp` and `vibe` start and answer their version or `initialize`, on Fedora or Ubuntu and on Windows 11, with wall times recorded
- [ ] Given the v3.1.0 environment, when the build completes, then the location of `vibe-rs` (bundled under `vibe/_bin/` or built in `vibe/cli-rust/target/release/`) is recorded, and the cold build time on the ubuntu-22.04 runner is measured
- [ ] Given an export whose `uv.lock` was edited, when `uv sync --locked` runs, then it fails, and the error text is recorded for the materializer's message
- [ ] Given either environment, when `import vibe` runs in it, then `vibe.__file__` resolves inside the environment rather than the exported tree, proving the install is not editable
- [ ] Given a failure on one platform, then the finding names the failing step and the fallback the materializer will use, and `docs/parity.md` "Windows observations" records it

#### US-005: Reference materializer and attestation gate
**Description:** As the maintainer, I want any pinned commit materialized into `.parity/` with an attestation the Rust gate checks, so that live probes run only against an environment proven to be that commit, whatever the shared checkout's `HEAD`.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-004

**Acceptance Criteria:**
- [ ] Given a commit, when `scripts/parity/repin.py materialize <sha>` runs, then it extracts the tree, builds the environment and any bundled binary under `.parity/reference-<sha12>/`, and writes `attestation.json` with the commit, the `uv.lock` SHA-256, the Python version, the platform, the hashes of the installed wheels and binaries, and the tool versions
- [ ] Given a materialization interrupted at any step, when it is rerun, then the partial directory is discarded and never used, because the final rename happens only after the attestation is written
- [ ] Given an attested environment, when a Rust live probe gates on it, then `vibe_core::parity` reads the attestation instead of the checkout's `HEAD`, and the probe runs with the shared checkout at any commit
- [ ] Given an environment whose installed files no longer match the attestation, when a probe gates on it, then the probe is refused with a message classed as environment drift that names `repin.py materialize <sha>` and never says "regenerate the corpus"
- [ ] Given `git` cannot be spawned or prints non-UTF-8, when the gate runs, then it returns a skip reason instead of `None` (`crates/vibe-core/src/parity.rs:72-80`)
- [ ] Given a black-box driver, when it captures, then it runs the materialized console scripts, and a modified file in the shared checkout's working tree cannot reach the corpus
- [ ] Given no materialized environment and no checkout, when `cargo test` runs without `VIBE_PARITY_REQUIRE_LIVE`, then every replay runs and every probe records a skip, as today

#### US-006: Loud skips and the documented gate in CI
**Description:** As the maintainer, I want every skipped live probe recorded and countable, and a mode that makes a skip fatal, so that a run that measured nothing cannot read as a pass.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-005

**Acceptance Criteria:**
- [ ] Given any probe that skips, when it does, then it appends one JSON line naming the probe, the reason class and the commit to `target/parity/skips.jsonl`
- [ ] Given `VIBE_PARITY_REQUIRE_LIVE=1`, when any probe would skip, then the test fails with the skip reason
- [ ] Given the `quality` and `windows-quality` jobs, when they finish, then the job summary states the number of probes skipped and the number of replays run, read from `target/parity/`
- [ ] Given the Linux `quality` job, when it runs the test gate, then it runs the AGENTS.md sequence (`env -u FORCE_COLOR`, a throwaway `VIBE_HOME`, `--no-fail-fast`) rather than a bare `cargo test` (`.github/workflows/ci.yml:52`)
- [ ] Given the nextest `ci` profile, when a test under `parity` fails, then it is not retried, and the profile writes JUnit with `report-skipped = "ignored"`

#### US-007: Scheduled live parity workflow
**Description:** As the maintainer, I want a weekly no-secrets workflow that materializes every accepted commit and runs the whole suite in require-live mode, so that live measurement happens without my workstation.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-006

**Acceptance Criteria:**
- [ ] Given `.github/workflows/parity-live.yml`, when it is read, then it triggers on a weekly `schedule` and `workflow_dispatch` only, declares `permissions: contents: read`, references no secret, and pins every action by commit SHA
- [ ] Given a run, when it starts, then it materializes every commit the manifest accepts, with the uv and cargo caches keyed by commit and never restored from a pull-request run
- [ ] Given a run, when the suite runs with `VIBE_PARITY_REQUIRE_LIVE=1`, then every driver's reference-side `--check` or recapture path executes at least once, and the summary lists probes run, probes failed, and failures by class (behavior, environment, provenance)
- [ ] Given a materialization failure, when the run ends, then it fails with an environment-class summary and does not report any behavior result
- [ ] Given a run over 90 minutes, when the timeout hits, then the job fails with the last completed probe named

#### US-008: Upstream sentinel and static census
**Description:** As the maintainer, I want a daily job that compares the pin with the newest upstream tag and takes a static census of its public boundaries, so that drift is reported within a day without executing upstream code.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001

**Acceptance Criteria:**
- [ ] Given `scripts/parity/census.py <tree>`, when it runs on an exported tree, then it reads `SERVER_METHODS`, the configuration schema fields, the builtin tool names, the CLI flags of `vibe/cli-rust` argument definitions, and the existence of every reference path the scorecard rows cite, by AST and pattern scan only, without importing upstream code
- [ ] Given the census at `376f6a3` and at `c96a6db`, when they are diffed, then the methods diff is exactly 33 added and 5 removed, matching the audit
- [ ] Given the daily sentinel workflow, when the newest upstream tag (from `git ls-remote --tags`) differs from `REFERENCE_VERSION`, then the job fails and its summary lists the releases behind and the census diff by boundary
- [ ] Given upstream is unreachable, when the sentinel runs, then it fails with an unreachable message, never with "up to date"
- [ ] Given the "Weighted score against upstream `HEAD`" header row, when the sentinel reports, then the row names the newest tag and the census diff committed by `repin.py census`, and `scorecard_tests` fails when the row's recorded tag is older than the newest tag in `scripts/parity/upstream-census.json`

---

### EP-003: Comparison Framework

Release 1. The replay framework gains the comparison and ledger semantics every re-pinned corpus will be written against, so the v3 recaptures are written once.

**Definition of Done:** `replay.rs` exposes typed prose, bounded ledger entries and figure assertions; at least the nine former `OwnWords` replays and the unified-harness replay run on them; the shingle guard runs on every CI job with a baseline that only shrinks.

#### US-009: Typed prose comparison
**Description:** As an implementing agent, I want prose compared per declared pointer instead of a blanket "own words" mode, so that a wrong title, error or output with a space is a difference while licensed rewrites stay ledgered.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given `Prose::OwnWords`, when this story lands, then it no longer exists, and every replay that used it declares its own-words pointers in a prose ledger
- [ ] Given two digests at a pointer no prose ledger entry names, when they differ, then the replay reports a difference
- [ ] Given an own-words entry, when the port's prose length falls outside the entry's declared band (default 0.5 to 2 times the reference length), then the replay reports a difference naming the band
- [ ] Given an own-words entry for a body that covers reference directives, when it is declared, then it records the commit its directives were audited at, and the replay fails when that commit differs from the corpus commit (audit M9)
- [ ] Given an own-words entry that no difference exercises, when the replay concludes, then it fails as stale

#### US-010: Bounded, segment-exact ledger entries
**Description:** As the maintainer, I want each ledger entry to state how many differences it covers and to match pointers on segment boundaries, so that a regression inside a ledgered area is no longer absorbed.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given a `Divergence` or `FieldDivergence`, when it is declared, then it carries the number of differences it covers per scenario, and the replay fails when the count grows or shrinks, naming the entry and both counts
- [ ] Given an entry with pointer `/display/summary`, when a difference occurs at `/display/summaryDetail`, then the entry does not cover it
- [ ] Given an entry whose pointer is a top-level observation key followed by nothing (for example `/requests/` or `/steps/`), when the ledger is asserted, then it fails unless the entry also lists the sub-pointers it may cover
- [ ] Given `PatternDivergence`, when it is asserted, then it names a scorecard row like the other entry kinds
- [ ] Given the unified-harness ledger (`unified_harness_parity_tests.rs:121-156`), when it is migrated, then no entry covers a whole `/requests/` or `/steps/` subtree

#### US-011: Replays check their declared figures
**Description:** As the maintainer, I want every replay's conformant, scenario and ledgered counts declared in machine-readable form in `docs/parity.md` and asserted by the replay itself, so that no ratio in the scorecard is typed by hand.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-010

**Acceptance Criteria:**
- [ ] Given a "Measured figures" table in `docs/parity.md` with one line per replay heading (row, commit, conformant, scenarios, ledgered by entry class), when `Tally::conclude` runs, then it asserts its counts equal its line and fails naming the line to update
- [ ] Given the hand-rolled `vibe-core` ledgers, when they conclude, then they assert their figures the same way through a shared helper
- [ ] Given the figures table, when `scorecard_tests` runs, then it derives each row's Parity (conformant plus accepted-divergence differences over total) and Conformance (conformant over total), and fails when a row's stated scores disagree
- [ ] Given a row with no figure line, when `scorecard_tests` runs, then the row's score must be marked as a structural judgment and cannot exceed 50
- [ ] Given a replay run filtered to one test, when it concludes, then it still checks its own line, independent of other replays

#### US-012: Clean-room shingle guard with a shrinking baseline
**Description:** As the maintainer, I want CI to fail when a tracked file shares an 8-word run with the reference outside a reviewed baseline, so that `NOTICE` stays true while the existing copies are removed.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-005

**Acceptance Criteria:**
- [ ] Given `repin.py shingles <sha>`, when it runs on a materialized tree, then it writes 64-bit digests of normalized 8-word shingles from docstrings, comments, string literals and Markdown of the reference's Python and Rust sources, sorted, at most 4 MB, under `crates/vibe-core/tests/licensing/`
- [ ] Given a tracked file that contains a shingle in the set, when the workspace tests run, then a test fails naming the file, the line and the run length, unless the run is in the committed baseline
- [ ] Given the baseline, when a run listed in it no longer occurs, then the test fails until the entry is removed, so the baseline can only shrink
- [ ] Given runs made only of identifiers, URLs, protocol method names, configuration keys or digits, when they match, then the allowlist rule excludes them, and the rule is documented in the test
- [ ] Given the digest set would exceed 4 MB, when it is generated, then the command fails and names the Bloom-filter fallback question rather than committing a larger file
- [ ] Given AGENTS.md, when this story lands, then its licensing section names the guard and states that user-visible reference messages are rewritten in original words and ledgered, like the precedents in `docs/parity.md` "Accepted divergences"

---

### EP-004: Rolling Re-pin and v3.1.0 Server-Side Oracles

Release 2. The pin moves to `c96a6db` (v3.1.0) without a red `main`: the previous pin stays accepted while a committed transition list names it, and every server-side oracle is recaptured on the Unified Harness only.

**Definition of Done:** `REFERENCE_COMMIT` is `c96a6db`; every server, editor, programmatic and in-process corpus is captured at it; the transition list names only the terminal-client corpora EP-005 owns; `--legacy-harness` appears in no capture.

#### US-013: Two-pin transition mechanics
**Description:** As the maintainer, I want the pin to name a target and, while a transition list names it, a previous commit, so that a re-pin can land oracle by oracle with every corpus still provenance-checked.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-002, US-005

**Acceptance Criteria:**
- [ ] Given `parity.rs` and `pin.py`, when this story lands, then both declare `PREVIOUS_COMMIT` (empty outside a transition) beside the target, and `parity_tests.rs` holds the four values equal and allows no third copy
- [ ] Given the manifest's transition list, when a corpus is at `PREVIOUS_COMMIT`, then the meta-test accepts it only if the list names it, and fails when the list names a corpus already at the target
- [ ] Given an empty transition list and a non-empty `PREVIOUS_COMMIT`, when the meta-test runs, then it fails asking for `PREVIOUS_COMMIT` to be cleared
- [ ] Given a corpus at either commit, when its live probe runs, then it gates on that commit's attested environment
- [ ] Given AGENTS.md, when this story lands, then "A committed corpus changes only in a re-pin" is replaced by the rolling rule and by "a corpus changes only through `repin.py capture`"
- [ ] Given `repin.py plan <sha>`, when it runs, then it lists every manifest entry with its current commit, driver command and blocking story, in the order a re-pin recaptures them

#### US-014: Unified-only server session oracles at v3.1.0
**Description:** As the maintainer, I want the session, review, rewind, hooks, agents, agent-loop and protocol oracles recaptured at v3.1.0 without `--legacy-harness`, so that rows 9, 15, 16, 17, 18, 20 and 34 measure the backend upstream ships.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-009, US-010, US-013

**Acceptance Criteria:**
- [ ] Given `pin.py`, when this story lands, then `HARNESS_FLAGS` is removed and no driver passes a harness flag to either side
- [ ] Given the drivers `sessions`, `review`, `rewind`, `hooks`, `agents`, `agent_loop`, `app_server_protocol`, `app_server_surface` and `app_server_params`, when they are recaptured through `repin.py capture`, then each corpus records `c96a6db` and each replay passes with its differences ledgered as gaps or accepted divergences under the bounded, typed rules
- [ ] Given a scenario whose reference subject was removed at v3.0.0 (for example the two worktree reap methods), when it is recaptured, then it is either deleted with its ledger entries or kept as a port-only scenario with a ledger row naming the removal
- [ ] Given the method census, when the protocol replay runs, then the 33 methods added at v3 are counted as unmeasured in the figures table rather than absent from it
- [ ] Given a driver that crashes against v3.1.0, when it does, then the capture fails naming the scenario and the story records the driver change, not a ledger entry

#### US-015: Editor, programmatic and system-prompt oracles at v3.1.0
**Description:** As the maintainer, I want the ACP, `vibe -p`, telemetry-session and system-prompt oracles recaptured at v3.1.0, so that rows 8, 13, 19 and 27 measure the Rust-fronted reference.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-014

**Acceptance Criteria:**
- [ ] Given `acp.py`, `programmatic.py`, `telemetry_session.py` and `system_prompt.py`, when they are recaptured, then each corpus records `c96a6db` and each replay passes with ledgered differences only
- [ ] Given `vibe -p` is now the reference's Rust binary, when the programmatic oracle runs, then it drives the materialized `vibe` console script, and the new exit code for budget limits is a scenario
- [ ] Given the unified and legacy corpora pairs (`corpus.json` and `unified-corpus.json`), when they are recaptured, then one corpus per oracle remains and the manifest lists it
- [ ] Given the ACP argv cases that rejected `--legacy-harness` with `--experimental-harness`, when the reference no longer knows either flag, then the cases record the reference's rejection of an unknown flag and the port's acceptance is an open divergence on row 8
- [ ] Given the system-prompt replay, when it runs, then its own-words entries record `c96a6db` as their audit commit
- [ ] Given a scenario the v3.1.0 reference answers with an error the v2.26.0 corpus did not record, when the replay runs, then the difference is reported under its scenario and is never absorbed by an entry written for v2.26.0

#### US-016: Integration oracles at v3.1.0
**Description:** As the maintainer, I want the connectors, MCP, plugins, project-links, teleport, workspace-trust and unified-harness oracles recaptured at v3.1.0, so that rows 11, 12, 21, 22, 25, 35 and 36 measure the current reference.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-014

**Acceptance Criteria:**
- [ ] Given `connectors`, `mcp_catalog`, `mcp_oauth`, `mcp_transport`, `plugins`, `project_links`, `teleport`, `workspace_trust` and `unified_harness`, when they are recaptured, then each corpus records `c96a6db` and each replay passes with ledgered differences only
- [ ] Given the unified-harness replay, when it runs on Windows, then only the `mcp/*` scenarios are ignored by design and the other scenarios run (`unified_harness_parity_tests.rs:195-199`)
- [ ] Given the `--harness unified` corpus variants, when the legacy backend no longer exists upstream, then the variant flag is removed from the drivers and the manifest
- [ ] Given a plugin or connector fixture whose reference behavior changed at v3, when the replay differs, then the difference is a gap with a ledger row, never a widened prefix

#### US-017: In-process oracles whose reference modules survive
**Description:** As the maintainer, I want the configuration, tools, shell, checkpoints, skills, telemetry, setup, worktree, permission, web-fetch, voice and experiments oracles recaptured at v3.1.0, so that their rows measure the current modules.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-009, US-010, US-013

**Acceptance Criteria:**
- [ ] Given every in-process driver whose imports resolve at `c96a6db`, when it is recaptured through `repin.py capture`, then its corpus records `c96a6db` and its replay passes with ledgered differences only
- [ ] Given the configuration census, when it is recaptured, then the fields added at v3 are counted as unmeasured, the removed fields are ledgered as removed upstream, and the renamed field is recorded as a rename
- [ ] Given the skills replay, when its own-words bodies are audited, then their audit commit is `c96a6db` and the skills directives added since `4a96003` are covered or listed as a gap
- [ ] Given `tool_config`, when its live probe runs on Windows, then it compares the Windows shell branch against a corpus captured on Windows, or the probe is ignored there by design with a reason
- [ ] Given a driver whose import fails at `c96a6db`, when it runs, then it fails naming the missing module and the driver moves to US-018

#### US-018: In-process oracles whose reference modules were removed
**Description:** As the maintainer, I want the oracles that imported removed modules (LLM backends, agent loop, middleware, CLI entry point, Textual-bound onboarding and experiments paths) rewritten against their v3 owners or retired, so that no oracle measures code upstream deleted.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-017

**Acceptance Criteria:**
- [ ] Given `llm_backends.py`, `experiments.py`, `onboarding.py`, `config_surface.py`, `telemetry.py` and `runtime-parity/cli-surface-oracle.py`, when they are revised, then none imports a module absent at `c96a6db`, and each either measures the v3 owner of the behavior (for example `harness/` runtimes or the Rust CLI's argument surface through its binary) or is retired with its corpus removed from the manifest
- [ ] Given a retired oracle, when it is removed, then its scorecard row states which oracle now measures it or carries no figure line and is capped by US-011's structural rule
- [ ] Given the CLI-surface oracle, when it measures the v3 `vibe`, then it reads the reference's flags from the materialized binary's help and argument errors, never from `vibe/cli-rust` source text
- [ ] Given a rewritten oracle, when its replay first runs, then its figures line exists and its floor is set from the new count

---

### EP-005: v3 Terminal Client and Scorecard Restatement

Release 2. The terminal client is measured black-box against upstream's Rust TUI, the Textual-pilot oracles are retired, and the scorecard is restated at v3.1.0 from replay figures.

**Definition of Done:** the transition list is empty and `PREVIOUS_COMMIT` cleared; three TUI families replay against corpora captured from the v3.1.0 `vibe`; `docs/parity.md` carries "What the re-pin moved: v2.26.0 to v3.1.0" and a total derived entirely from figures.

#### US-019: Validate assumption: upstream's Rust TUI is drivable and deterministic under a pseudo-terminal
**Description:** As the maintainer, I want evidence that the v3.1.0 `vibe` runs under the `vibe-e2e` pseudo-terminal with a stand-in backend and renders reproducibly, so that the black-box TUI oracle rests on measurement.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-005

**Acceptance Criteria:**
- [ ] Given the materialized v3.1.0 environment and a `vibe-e2e` world, when `Subject::Reference` starts `vibe` at 120 by 40 with the loopback stand-in backend, then the first screen appears within 10 s on Linux and under ConPTY
- [ ] Given one keystroke script (type a prompt, open the command popup, submit a turn), when it runs 5 times, then the normalized screens are identical across runs, or the varying cells are listed with their cause
- [ ] Given the OSC sequences the reference writes (window title and status reporting), when they occur, then they are observed through `vt100::Callbacks` or the gap is recorded
- [ ] Given the reference TUI spawns `vibe-app-server`, when the script runs, then the app-server method sequence can be recorded through a wrapper named by the reference's app-server command variable, as the fallback oracle the risk table names
- [ ] Given a failure on either platform, then the finding names the failing step and whether the fallback replaces the screen oracle for that platform

#### US-020: Black-box TUI differential runner with the startup family
**Description:** As the maintainer, I want a Rust runner that plays one keystroke script against both subjects, normalizes the screens and replays a committed reference capture, so that the terminal client is measured without piloting reference internals.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-019, US-009, US-010

**Acceptance Criteria:**
- [ ] Given a scenario of keys, resizes and waits, when the runner captures the reference, then the corpus stores per screen the row count, each row as a digest and length unless the text is scenario-authored, attribute runs reduced to classes (foreground, background, bold, inverse), the cursor, the alternate-screen flag and window titles, and no reference-authored row text
- [ ] Given the port and the corpus, when the replay runs, then differences are JSON pointers into the normalized screens, ledgered under the bounded and typed rules, and the figures line is asserted
- [ ] Given the startup family (first screen, resize to 80 by 24, help overlay, quit), when it replays, then it passes with every difference ledgered
- [ ] Given a screen wait that times out, when it does, then the failure prints both subjects' last normalized screens and the first differing cell
- [ ] Given the capture, when it is written, then `repin.py capture` records it in the manifest like any corpus

#### US-021: Composer and transcript TUI families; retire the Textual-pilot oracles
**Description:** As the maintainer, I want the composer (typing, history, command and path completion popups) and transcript (a scripted turn with a tool call and an error) families on the black-box runner, so that the eight pilot oracles can be retired without losing their rows' measurement.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-020

**Acceptance Criteria:**
- [ ] Given the composer and transcript families, when they replay against v3.1.0 captures, then they pass with every difference ledgered and their figures asserted
- [ ] Given the eight `crates/vibe-cli/tests/runtime-parity/*-oracle.py`, `scripts/parity/oracle.py`, `scripts/parity/harness.py` and the pilot paths of `commands.py` and `autocompletion.py`, when this story lands, then they and their corpora are removed and the manifest lists none of them
- [ ] Given rows 2, 14 and 23, when the pilot oracles are retired, then each names the TUI families that measure it, and any behavior no family covers is listed in the row as unmeasured
- [ ] Given a composer scenario that depends on the clipboard or an external editor, when no hermetic seam exists for the reference, then the scenario is port-only with a reason rather than silently dropped

#### US-022: Scorecard restatement at v3.1.0
**Description:** As the maintainer and as a port user, I want `docs/parity.md` restated at v3.1.0 with rows retired, merged and added to match upstream's modules and every score derived from figures, so that the headline number describes the product upstream ships.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-015, US-016, US-018, US-021, US-011

**Acceptance Criteria:**
- [ ] Given the v3.1.0 census, when the table is restated, then every row cites reference paths that exist at `c96a6db`, rows whose subject was deleted are retired or merged with a note, and new rows exist for the surfaces added at v3 (artifacts, imports, projects, OIDC setup, local skills, subagents, the Rust terminal client) with Parity derived from their figures or 0 when unmeasured
- [ ] Given the reference tree at `c96a6db`, when a coverage test runs on its materialized tree, then every Python module under `vibe/` and every top-level module of `vibe/cli-rust/src/` is claimed by a row or listed as out of scope with a reason, and an unclaimed module fails the test
- [ ] Given the restated table, when `scorecard_tests` runs, then the header total is derived from figures, reports Parity and Conformance, and states the measured commit and the newest upstream tag from the census
- [ ] Given the section "What the re-pin moved: v2.26.0 to v3.1.0", when it is written, then it states the drop from 98.7 with its causes per row, cites paths rather than quoting them, and records that the upstream Rust tree is now Apache-2.0
- [ ] Given the transition list, when this story lands, then it is empty and `PREVIOUS_COMMIT` is cleared, or the story stays open

---

### EP-006: Oracle Strength and Hygiene

Release 3. The oracles are measured for what they can detect, the normalizations that erase meaning are narrowed, the copied text is removed, and the remaining hygiene findings are closed.

**Definition of Done:** a sensitivity matrix exists for every `Replay` oracle and the TUI runner with at least 95% detection or reasoned ledger entries; the shingle baseline is empty; Python drivers pass lint and type checks in CI; the Windows isolation leaks are closed.

#### US-023: Sensitivity harness for replay oracles
**Description:** As the maintainer, I want each replay to apply mutation operators to the port's capture and require the comparison to report each one, so that an oracle's claimed coverage is a measured number.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-022

**Acceptance Criteria:**
- [ ] Given a `Replay` or the TUI runner, when the sensitivity pass runs on its port capture, then it applies at least these operators at sampled pointers: delete a key, add a key, change a scalar's value, change a scalar's type, change a string's length, reorder a list, drop a list element
- [ ] Given each mutation, when the differ and ledger evaluate it, then a mutation the replay does not report counts as undetected, and the pass writes a matrix of operator by row to `target/parity/sensitivity/`
- [ ] Given an oracle below 95% detection, when the pass concludes, then it fails unless each undetected operator class has a ledger entry stating why the oracle cannot see it
- [ ] Given the pass, when it runs in the default workspace suite, then it adds at most 60 s to the Linux `quality` job, sampling pointers deterministically from a seed recorded in the matrix

#### US-024: Normalization sensitivity and narrowed normalizations
**Description:** As the maintainer, I want the normalizations that erase meaning (time values, event counters, protocol shapes, whitespace digests) narrowed and measured, so that a type, ordering or length regression is visible.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-023

**Acceptance Criteria:**
- [ ] Given a time key, when a value is normalized, then the token keeps its type class (ISO-8601 string, integer epoch, float, null), so an integer where the reference sends a string is a difference
- [ ] Given event counters, when they are normalized, then they become their rank within the run, so a non-monotonic or reset sequence is a difference
- [ ] Given the protocol shape census, when a result holds a list, then the shape records whether the list is empty and, where the reference orders it, the order of item kinds
- [ ] Given a black-box driver run with `--raw`, when `--renormalize <raw> --mutate <operator>` runs, then it re-normalizes a mutated raw observation without re-running the subject, and the sensitivity pass reports which raw mutations normalization erases

#### US-025: Remove the copied reference text
**Description:** As the maintainer, I want every copied docstring, comment and user-visible message rewritten in original words and ledgered, so that the shingle baseline reaches zero and `NOTICE` is true.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-012, US-022

**Acceptance Criteria:**
- [ ] Given every baseline run in Rust comments (including `crates/vibe-core/src/worktree.rs:239` and `crates/vibe-core/src/worktree/lifecycle.rs:34`), when this story lands, then each comment is rewritten in original words and removed from the baseline
- [ ] Given every baseline run in string literals (including `crates/vibe-cli/src/tui/diagnostics.rs:85-96`, `crates/vibe-cli/src/tui/command_handlers.rs:406`, `crates/vibe-core/src/config/proxy.rs:53-57`, `crates/vibe-cli/src/update_notifier/gateway.rs:35`), when it is rewritten, then the message names the same cause, value and limit, and an accepted-divergence row records it
- [ ] Given committed corpora and prompt assets in the baseline, when they are revised, then reference rows are stored as digests and the assets reworded, and the corpora are recaptured through `repin.py capture`
- [ ] Given the baseline file, when this story lands, then it is empty and the guard fails on any new run

#### US-026: Scorecard and ledger hygiene
**Description:** As the maintainer, I want the scorecard's prose to match its mechanics, so that the document stops contradicting its own rows.

**Priority:** P2
**Size:** M (3 pts)
**Dependencies:** Blocked by US-022

**Acceptance Criteria:**
- [ ] Given the Method section, when it is rewritten, then it states what Parity and Conformance mean and no longer says scores come only from inventories (`docs/parity.md:41-43`)
- [ ] Given the counts of capture scripts and replay modules, when the document states them, then a test computes them from the tree and fails on disagreement
- [ ] Given `ledger_tests`, when a row cites a symbol in a file, then the symbol must appear in that file, and the `is_prose` citation at `docs/parity.md:348` is corrected
- [ ] Given a statement that the upstream Rust sources are proprietary, when the document is revised, then none remains

#### US-027: Python tooling and mechanical architecture rules
**Description:** As an implementing agent, I want the 66 000 lines of Python under `scripts/parity/` linted, type-checked and structurally tested, and the dependency layers checked by a test, so that the conventions in AGENTS.md hold without review.

**Priority:** P2
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given `scripts/parity/`, when CI runs, then ruff and a type checker run on it with a committed configuration and pass
- [ ] Given a driver that imports another driver, or a `_cases`/`_scenarios` table that imports anything but the standard library, when the workspace tests run, then an AST test fails naming the import, and the three tables that import shared modules today (`acp_scenarios.py`, `telemetry_session_scenarios.py`, `unified_harness_scenarios.py`) are fixed
- [ ] Given a crate edge that points to a later layer of `[workspace.metadata.vibe] dependency-layers`, when the workspace tests run, then a test built on `cargo metadata` fails naming the edge
- [ ] Given AGENTS.md, when this story lands, then its layer list includes `vibe-e2e` in layer 1

#### US-028: Windows isolation and retries
**Description:** As the maintainer on Windows, I want parity tests isolated from my real configuration and credential vault, so that a local run neither fails nor passes because of my machine.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given a configuration test fixture, when it discovers project configuration, then its working directory sits under the fixture root whose parent bounds the walk, and `config::surface_parity_tests::every_model_scenario_validates_to_the_document_the_reference_validates` passes on a Windows profile holding `%USERPROFILE%\.vibe\config.toml`
- [ ] Given `host_env.isolated`, when a driver builds a child environment on Windows, then the child cannot read or write the Windows Credential Manager entries of the product's service, through the keyring test switch both implementations honor or a scenario-scoped service name
- [ ] Given a driver killed during a credential scenario, when the next run starts, then it removes the entries the previous run left before capturing
- [ ] Given a test that fails on Windows only because of a parity fixture, when it is tagged, then the tag follows AGENTS.md and the CI summary counts it

#### US-029: Consistency cleanup
**Description:** As an implementing agent, I want one way to write a ledger, one off-pin flag and one digest shape, so that oracles read alike and the scorecard's conventions agree.

**Priority:** P2
**Size:** M (3 pts)
**Dependencies:** Blocked by US-011

**Acceptance Criteria:**
- [ ] Given the four hand-rolled ledger styles in `vibe-core` replays, when this story lands, then they use the `replay.rs` entry kinds, or each keeps its own with a comment naming the capability `replay.rs` lacks
- [ ] Given `--allow-unpinned` and `--expected-commit`, when drivers parse their arguments, then one name remains, routed through `repin.py`, and a driver passed the retired name exits non-zero naming the remaining one
- [ ] Given `replay::digest` and the drivers' `{prose, sha256}`, when this story lands, then one digest shape remains
- [ ] Given the stale texts the audit lists (`compaction_parity_tests.rs:63`, `crates/vibe-cli/tests/parity/README.md:51`, `shell_parity_tests.rs:58`) and the fixed recapture paths under `target/`, when this story lands, then the texts are corrected and the paths are per-run
- [ ] Given `RESTORE_COMMAND`, when it is printed, then it names the configured checkout path, or is removed if materialization made it obsolete

---

## Functional Requirements

- FR-01: The system must list every committed corpus in a manifest with its driver command, test, scorecard row, capture commit and SHA-256, and fail the workspace suite on any unlisted, off-commit or out-of-band-edited corpus.
- FR-02: Every replay must assert its corpus commit and schema, fail rather than skip on a stale corpus, and enforce a scenario floor of at least 95% of its manifest count.
- FR-03: A driver in port mode must not write a manifest-listed path.
- FR-04: Live probes must gate on an attestation of a materialized environment, not on the shared checkout's `HEAD`, and must fail closed when the attestation cannot be read.
- FR-05: Every skipped probe must be recorded under `target/parity/`; `VIBE_PARITY_REQUIRE_LIVE=1` must turn any skip into a failure.
- FR-06: A scheduled workflow with no secrets must run every live probe weekly; a daily sentinel must compare the pin with the newest upstream tag through a static census.
- FR-07: Replays must compare prose per declared pointer, bound each ledger entry's difference count, and match pointers on segment boundaries.
- FR-08: Every replay must assert its figures against `docs/parity.md`, and the scorecard must derive Parity and Conformance per row and in total.
- FR-09: The workspace suite must fail on an 8-word run shared with the reference outside a baseline that may only shrink.
- FR-10: The pin must support a target and a previous commit during a transition named in the manifest, and no other.
- FR-11: No capture may pass `--legacy-harness` after US-014.
- FR-12: The terminal client must be measured black-box under a pseudo-terminal against upstream's v3.1.0 `vibe`; no oracle may import a reference module absent at the pin.
- FR-13: The system must NOT execute upstream code in a job that holds secrets or a write token.
- FR-14: The system must NOT commit reference-authored text; screens, messages and prompts are stored as digests and lengths unless scenario-authored.

## Non-Functional Requirements

- **Performance:** the manifest, provenance and figure checks add at most 20 s to the Linux `quality` job; the sensitivity pass adds at most 60 s; a warm materialization (environment present and attested) is verified in at most 5 s; a cold v3.1.0 materialization on ubuntu-22.04 completes in at most 30 min including the `vibe-rs` release build.
- **Reliability:** 0 retries for any test under `parity` in every nextest profile; the live workflow finishes within 90 min; the sentinel reports a new upstream tag within 24 h of its publication on at least 95% of days.
- **Security:** the live and sentinel workflows declare `permissions: contents: read`, reference 0 secrets, pin 100% of actions by commit SHA, and share no cache key with any `pull_request` workflow; tests reach no host other than loopback.
- **Scalability:** the manifest and its meta-test handle at least 400 corpus entries within the 20 s budget; the shingle digest set stays at most 4 MB per accepted commit.
- **Accuracy:** each `Replay` oracle detects at least 95% of applied mutations after Release 3, or ledgers each undetected class with a reason.
- **Accessibility:** 100% of scorecard figures and CI summary counts are plain text in tables, and 0 results are conveyed by color alone.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | No reference at all | Fresh clone or CI without materialized environments | Every replay runs; every probe records a skip; the suite passes unless require-live is set | "skipping the live <probe> probe: no attested environment for <sha12>; run `repin.py materialize <sha>`" |
| 2 | Environment drift | A file of a materialized environment edited, or its `uv.lock` hash changed | Probe refused as environment drift; no behavior verdict | "environment for <sha12> no longer matches its attestation; rerun `repin.py materialize <sha>`" |
| 3 | Interrupted materialization | Process killed mid-build | Partial directory ignored and removed on rerun | "discarding the partial environment at <path>" |
| 4 | Self-recorded corpus | A driver run with `--server` and no `--output` | Driver exits before capturing | "port-mode capture needs `--output` outside the manifest paths" |
| 5 | Off-commit corpus | Corpus records `server-override` or an unaccepted commit | Manifest test and replay fail | "<corpus> was captured from <commit>; accepted: <target>[, <previous>]" |
| 6 | Empty or shrunken corpus | Recapture that lost scenarios | Replay fails on the floor before comparing | "<corpus> holds <n> scenarios, below the floor of <m>" |
| 7 | Upstream unreachable | Sentinel cannot list tags | Sentinel fails, never reports up to date | "upstream unreachable: <error>; drift unknown" |
| 8 | Upstream tag moved or deleted | Force-pushed or removed tag | Pins stay by commit SHA; the census notes the tag change | "tag <name> now names <sha>; the pin is unaffected" |
| 9 | Concurrent captures | Two `repin.py capture` runs for one entry | Staged write plus manifest hash: the second run fails on a changed hash | "<corpus> changed during capture; rerun" |
| 10 | Platform-specific corpus | A POSIX-captured corpus replayed on Windows | Fails unless the family is declared per platform; declared families are ignored by design with a reason | "<corpus> records linux; declare the family per platform or capture it on windows" |
| 11 | Abandoned re-pin | Transition reverted before completion | Reverting the target and transition list restores the previous state; corpora at the abandoned target fail the manifest test | "<corpus> is at <sha>, not an accepted commit" |
| 12 | Boundary of the shingle guard | A run of exactly 8 words, or one made of identifiers | Counted when it is prose; excluded by the allowlist rule otherwise | "<file>:<line> shares a <n>-word run with the reference" |
| 13 | Nondeterministic TUI cell | A clock or spinner cell differs between runs | Normalized by a declared mask with a reason, never by a broader ledger prefix | "cell <r>,<c> varies between runs; declare a mask" |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Upstream's Rust TUI cannot be driven deterministically under a pseudo-terminal | Med | High | US-019 measures it on both platforms first; the fallback oracle records the app-server method sequence the TUI issues through a wrapper, and the rows stay capped until a family replays |
| 2 | The non-editable v3.1.0 build does not bundle `vibe-rs`, or takes over 30 min on the runner | Med | Med | US-004 measures it; the materializer falls back to `cargo build --release` in the exported tree and caches by commit |
| 3 | The restated score drops far below 98.7 and reads as a regression | High | Med | US-022 states the drop per row with causes, as the two previous re-pin sections did; Parity and Conformance are both published |
| 4 | Executing upstream code in CI exposes the repository | Low | High | No secrets, read-only token, SHA-pinned actions, a separate workflow, caches keyed by commit and never shared with pull requests |
| 5 | The rolling transition becomes permanent | Med | Med | The meta-test fails on a stale transition; EP-005's definition of done requires an empty list; the sentinel summary shows corpora per commit |
| 6 | The shingle digest set exceeds 4 MB | Low | Low | US-012 fails rather than committing a larger file and names the Bloom-filter fallback |
| 7 | Rewriting copied messages lowers byte parity of user-visible output | High | Low | Accepted divergences with ledger rows, following the existing precedents; the message keeps cause, value and limit |
| 8 | Sensitivity passes slow the default suite | Med | Low | Deterministic sampling with a 60 s budget; the full matrix runs in the weekly live workflow |
| 9 | Effort (29 stories, 112 points) outlasts upstream's release cadence | High | Med | The rolling re-pin and the sentinel make the next re-pin incremental; Release 2 starts as soon as EP-001 and EP-002 land |

## Non-Goals

What this version explicitly does NOT include:

- Closing the product gaps the v3.1.0 measurement reveals (the 33 added methods, the new configuration keys, the new CLI flags and subcommands, the Rust TUI's new features); follow-on PRDs per part, ordered by the derived scores of US-022.
- Removing the port's `--legacy-harness` flag or legacy backend; a product decision recorded as an open divergence after US-015, revisited in the CLI-surface follow-on.
- Judging whether original prose covers the reference's directives with a language model; the typed prose ledger records the audit commit instead; revisit after Release 3.
- Code-level plagiarism detection over Rust token sequences; the guard covers prose-bearing tokens only; revisit if the port starts reading `vibe/cli-rust` for design.
- A hosted conformance dashboard website; the scorecard and CI summaries are the dashboard.
- A Windows live workflow; Windows runs replays only, Linux runs live probes; revisit when US-004 shows the Windows materialization time.
- Pixel-exact visual comparison of the terminal client; attributes are reduced to classes.

## Files NOT to Modify

- `crates/vibe-protocol/src/`: its envelopes deny unknown fields, and nothing here changes the wire.
- `crates/vibe-app-server/src/harness.rs`, `crates/vibe-cli/src/bootstrap.rs`, `crates/vibe-acp/src/argv.rs`: backend selection stays as it is (the port keeps `--legacy-harness`).
- `crates/vibe-app-server/src/wire_validation/schema.json` and `crates/vibe-app-server/tests/fixtures/app_server_stdio_fixture.rs`: production input and the shared parity fixture, changed only if a recapture proves they must be, in that story's PR.
- `NOTICE`: the plan keeps it true by removing copies, not by amending it.
- The shared checkout `C:\dev\mistral-vibe` (or `/home/arthur/dev/mistral-vibe`): read through `git archive` and `git ls-remote` only; no `checkout`, no `uv sync` inside it once US-005 lands.
- Product source under `crates/*/src/` other than parity test modules, except the string literals and doc comments US-025 rewrites and the fixture placement US-028 moves.

## Technical Considerations

Frame as questions for engineering input, not mandates:

- **Architecture:** Should `repin.py` be one Python entry point (list, plan, capture, materialize, census, shingles, verify) or a Rust `xtask`? Recommended: Python, beside the drivers it orchestrates and under the existing `reference.py` pin check; engineering to confirm that the Rust meta-test can verify the manifest without invoking Python.
- **Data Model:** Should the manifest live at `scripts/parity/manifest.json` or beside the corpora? Recommended: one file under `scripts/parity/`, since a single sorted list makes a re-pin plan and a diff readable. Trade-off: one hot file in every capture commit.
- **Attestation:** Is hashing installed wheels and binaries enough, or should the attestation hash every file of the environment? Recommended: wheel `RECORD` hashes plus binaries, verified in at most 5 s; a full-tree hash is the fallback if a drift slips through.
- **Figures:** Should figures live in `docs/parity.md` or in a JSON file the document embeds? Recommended: a Markdown table the replays parse, keeping the scorecard the single place a reader looks; engineering to confirm parsing stays under the 20 s budget.
- **TUI oracle:** Should the black-box runner live in `vibe-e2e` (layer 1) or in a new test crate? Recommended: in `vibe-e2e`, which already holds `Subject`, `World`, `Tui` and `Screen`; confirm that adding corpus replay there keeps it free of workspace dependencies other than `vibe-core`'s `parity-replay` feature, or move the replay half to a test of `vibe-cli`.
- **Migration:** The rolling re-pin needs no data migration; rollback is reverting the target commit and the transition list. Backward compatibility of corpora: corpora at the previous commit stay valid until the list drops them.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Corpora without a provenance check | 27 of 167 | 0 | Month-1 (Release 1) | `repin.py verify` and the manifest meta-test |
| Scheduled live runs per week | 0 | At least 1 | Month-1 | `parity-live` workflow history |
| Probe skips visible in CI | Not reported | Counted in every job summary | Month-1 | `target/parity/skips.jsonl` read by the summary step |
| Delay to detect a new upstream tag | Never detected | At most 24 h | Month-1 | Sentinel workflow history against upstream tag dates |
| Releases between the pin and upstream's newest | 7 | 0 at Release 2, at most 1 afterward | Month-6 | Sentinel census |
| Rows whose scores are derived from figures | 0 of 36 | 100% of rows | Month-6 | `scorecard_tests` |
| Mutation detection per `Replay` oracle | Not measured | At least 95% or ledgered | Month-6 (Release 3) | Sensitivity matrix under `target/parity/sensitivity/` |
| Shared 8-word runs outside the allowlist | 432 | 0 | Month-6 (Release 3) | Shingle guard baseline length |

## Open Questions

- Should the sentinel open a GitHub issue on drift, which needs `issues: write` in a job that otherwise stays read-only? The maintainer decides before US-008; the default is a failing job with a summary.
- Should the rolling re-pin ever accept more than two commits, for example to keep a slow oracle on an older pin across two upstream releases? The maintainer decides at the first transition that would need it; this PRD allows two.
- Which TUI families come after composer and transcript (pickers, session resume, voice indicators, the `/debug` console upstream added), and in which follow-on PRD? The maintainer decides after US-021, from the rows left unmeasured.
- Should the terminal client also be measured through the app-server method sequence it issues, as a second oracle next to the screen? Depends on US-019's fallback finding; the maintainer decides before US-021.
- If the maintainer prefers to keep byte parity of user-visible messages, `NOTICE` would need a written output-contract exception instead of US-025's rewrites; to be decided before US-025 starts, since it changes that story's scope.
[/PRD]
