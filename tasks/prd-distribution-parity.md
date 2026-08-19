[PRD]
# PRD: Distribution, Updates and Installers at Full Parity

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-19 | Arthur Jean | Initial draft: take parity row 1 from 95 to 100 |

## Problem Statement

Row 1 of `docs/parity.md` ("Distribution, updates, installers") scores 95. The
number is generous, and a strict remeasure against what the repository actually
publishes lands closer to 88. Six defects hold it there, each verified in both
trees at reference commit `b78b451`:

1. **The installers point at a repository that does not exist.**
   `scripts/install.sh:5` and `scripts/install.ps1:22` default their release base
   to `https://github.com/arthurjean/mistral-vibe-rs/...`, while `Cargo.toml:16`
   and the configured git remote both name `arthjean`. Every default-path
   install fetches a 404. Nothing in the repository binds the two strings.
2. **Nothing publishes the release the installers fetch.** `.github/workflows/`
   holds exactly two files, `action.yml` and `ci.yml`. There is no tag-driven
   build, no artifact upload, no GitHub release. The only exercise of
   `install.sh` is `.github/workflows/action.yml:17`, which packages one target
   locally and overrides the base URL with `file://`, so the published path is
   never tested. `install.ps1` has zero coverage.
3. **The aggregate checksum file holds one target.**
   `scripts/ci/package-release.sh:54` copies the per-target
   `SHA256SUMS.${target}` over `SHA256SUMS`. Across a five-target matrix, five
   jobs each write a one-line `SHA256SUMS` and the last upload wins. The
   installer's lookup (`awk '$2 == archive || $2 == "*" archive'`) already
   handles an aggregate file, so the defect is entirely on the producing side.
4. **The update notifier polls a different product.**
   `crates/vibe-cli/src/tui/startup/update.rs` sets `UPDATE_PROJECT =
   "mistral-vibe"` against `https://pypi.org`. A binary installed from a GitHub
   release therefore compares its own version against a Python package's PyPI
   index, and the update prompt advertises `uv tool upgrade mistral-vibe`, which
   installs something else. The reference implements a `GitHubUpdateGateway`
   (`vibe/cli/update_notifier/adapters/github_update_gateway.py`) that is dormant
   upstream and is exactly the adapter this port's distribution requires; it has
   no counterpart here and no ledger entry recording its absence.
5. **The on-disk update cache diverges and is unmeasured.**
   `UpdateCacheStore::load` (`crates/vibe-core/src/updates.rs:320`) has no
   fallback to the legacy `update_cache.json` that
   `filesystem_update_cache_repository.py` migrates. `UpdateCacheStore::store`
   (line 339) replaces the whole `[update_cache]` table where
   `vibe/utils/cache_store.py` merges into it, so an unknown key and a stale
   `dismissed_version` survive upstream and are dropped here. Neither divergence
   is detectable today: the capture fixture in
   `crates/vibe-cli/tests/runtime-parity/terminal-services-oracle.py` is an
   in-memory repository, so the TOML layout, the migration and the merge are all
   outside the oracle's reach.
6. **The version string is hand-copied into seven places.** `Cargo.toml:12`,
   `action.yml:27`, `.github/workflows/action.yml` twice,
   `crates/vibe-cli/whats_new.md:1`, `scripts/install.sh:4` and
   `scripts/install.ps1:2` all spell `2.23.1`. `AGENTS.md` states plainly that
   "nothing detects the drift". The workspace is also one minor version behind
   `vibe_core::parity::REFERENCE_VERSION`, which is `2.24.0`.

**Why now:** row 1 is the only part of the port whose failure is invisible to
every existing gate. `cargo test --workspace --all-features` passes with a
broken install path, because no test resolves the published URL, no test reads
the aggregate checksum file, and no oracle reads the cache file. Every other row
that regressed in the 2026-08-19 audit regressed on a measurement; this one
regresses on the absence of one.

## Overview

This PRD closes row 1 in three movements, then records what remains.

**Describe a release the repository could publish.** The installers, the
checksums and the version literal are all describing a release that has never
existed. One tag-driven workflow builds the five-target matrix, packages each
with the existing `scripts/ci/package-release.sh`, concatenates every per-target
line into a single `SHA256SUMS`, and attaches the set to a GitHub release. The
installers stop carrying a literal owner slug: a test binds their base URL to
`[package] repository`, and a second test binds every hand-written version
literal to `[workspace.package] version`, which moves to `2.24.0` to match
`vibe_core::parity::REFERENCE_VERSION`. This is the same
one-source-plus-a-scanner shape `crates/vibe-core/src/parity/parity_tests.rs`
already applies to the reference commit.

Running that workflow is not part of this PRD. US-218 was cancelled on
2026-08-19: publication waits until parity is proven across the scorecard, so
this movement ends at a repository whose release path is written and asserted,
not at a published release.

**Make the update path coherent.** `GitHubUpdateGateway` is ported with the
reference's cause mapping, its published-date sort, its draft and prerelease
skip and its `v` tag stripping, writing this port's own sentence for the one
authored message `NOTICE` forbids shipping. Production resolves the gateway from
the distribution the binary came from rather than assuming PyPI, and the update
action grows the reference's fourth outcome (`UPDATED`) with its two exit codes,
running this port's own upgrade commands instead of `uv tool upgrade
mistral-vibe`. The commands differ because the channel differs; the observable
contract does not.

**Measure the cache on disk.** The legacy JSON migration and the section merge
are ported, and a new `updateCacheStore` family is added to the
`terminal-services` corpus, shaped like the existing `promoState` family in
`crates/vibe-cli/src/tui/promo_parity_tests.rs`: a `cache.toml` document in, the
parsed cache and the document after write out. That family is what makes both
fixes falsifiable, and it is the reason this PRD treats the oracle extension as
a Must rather than a nicety.

What cannot be ported is recorded rather than left open. The standalone
`vibe-app-server` command, the compiled-in release notes, and the divergent
upgrade command list each get a row in the accepted-divergence table of
`docs/parity.md`, with the artifact that fails if the divergence ever closes
silently. The row is then remeasured and restated with the evidence each claim
rests on.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Parity row 1 score against the pin | 100, every sub-claim citing a measurement | 100 held across one re-pin |
| Hand-written version literals not asserted by a test | 0 of 6 (baseline 6 of 6) | 0 |
| Installer scripts exercised in CI on their native platform | 2 of 2 (baseline 1 of 2, on an overridden URL) | 2 of 2 |
| Update-cache comparisons replayed by an oracle | at least 24 in a new `updateCacheStore` family (baseline 0) | at least 24 |
| Published releases whose assets the committed installers resolve with no env override | not measured, publication deferred with US-218 | every tagged release |

## Target Users

### A person installing the port for the first time
- **Role:** a developer who read the README and pasted the `install.sh` one-liner.
- **Behaviors:** runs the script on Linux x86_64 or macOS arm64, expects `vibe --version` to answer, does not read the script first.
- **Pain points:** today the script resolves a 404 on the default path and exits before staging anything, with a message naming a URL that does not exist.
- **Current workaround:** clone the repository and `cargo build --release`, which needs a Rust toolchain and the ALSA headers.
- **Success looks like:** the one-liner installs `vibe` and `vibe-acp`, verifies both against a checksum, and prints the version.

### A person already running the port
- **Role:** a daily user of `vibe`, installed from a release archive.
- **Behaviors:** starts a session and reads what the startup prompt tells them; occasionally runs `vibe --check-upgrade`.
- **Pain points:** the notifier compares against the PyPI index for the Python product, so it either reports an upgrade that does not apply or advertises a command that installs a different program.
- **Current workaround:** rerun the installer manually and hope the version moved.
- **Success looks like:** the prompt names the release this binary came from and an upgrade command that upgrades this binary.

### The maintainer cutting a release
- **Role:** Arthur, publishing a tag.
- **Behaviors:** bumps the version, updates `CHANGELOG.md` and `whats_new.md`, pushes a tag.
- **Pain points:** six files carry the version by hand and nothing fails when one is missed; there is no workflow to run after the tag.
- **Current workaround:** none; no release has been published.
- **Success looks like:** one edit to `[workspace.package] version`, a failing test naming any file left behind, and a tag that produces a complete, checksummed release without further intervention.

## Research Findings

Key findings that informed this PRD.

### Competitive Context
- **The Python reference:** distributes through PyPI (`uv tool install mistral-vibe`), Homebrew, PyInstaller onedir archives attached to GitHub releases, a Nix flake and a Zed extension manifest. Its release path is two workflows: `.github/workflows/build-and-upload.yml` (five-target matrix plus an `almalinux:8` old-glibc smoke entry, `nix build`, artifact smoke tests, release attachment) and `.github/workflows/release.yml` (PyPI publish on `release: published`).
- **This port:** distributes prebuilt archives plus two installer scripts, with transactional `.new`/`.previous` staging and rollback traps the reference has no counterpart for. Row 1 already records this as exceeding upstream packaging.
- **Market gap:** the port ships transactional staging and rollback the reference has no counterpart for, and points at nothing. The gap is not capability, it is publication.

### Best Practices Applied
- **One source of truth plus a scanner.** `crates/vibe-core/src/parity/parity_tests.rs` already proves the pattern for the reference commit: one constant per language, and a test that walks the tree and fails on a third copy. The version literal and the repository slug get the same treatment rather than a new mechanism.
- **A single aggregate checksum manifest.** The installer's `awk` lookup already accepts both `sha256sum` and `shasum -a 256` output formats and both plain and `*`-prefixed filenames, so one concatenated `SHA256SUMS` covering every target needs no installer change.
- **Reproducible archives.** `package-release.sh` already passes `--sort=name --mtime="@${SOURCE_DATE_EPOCH:-0}" --owner=0 --group=0 --numeric-owner` to tar. The workflow sets `SOURCE_DATE_EPOCH` from the tagged commit so two runs of the same tag produce identical bytes.
- **Ad-hoc signed macOS binaries are not smoke-tested.** The reference disables its macOS standalone smoke tests pending Developer ID signing and notarization, because Gatekeeper rejects ad-hoc signatures. This port matches that posture rather than inventing a signing pipeline.

### Release Tooling Considered
Web research collected 2026-08-19. The alternatives to a hand-authored matrix
were compared before US-218 was scoped:

| Option | What it would cover | Why it is not chosen |
|--------|--------------------|----------------------|
| `dist` (formerly `cargo-dist`, v0.32.0, May 2026) | The whole path: matrix, archives, checksums, installers, release | Actively maintained but under post-axo governance, and it would replace `package-release.sh`, `install.sh` and `install.ps1`, all three of which already exceed the reference. Adopting it discards the transactional staging this row is credited for. |
| `cargo-release` `pre-release-replacements` | US-219 alone: rewriting arbitrary files on a version bump | A real alternative to the scanner, and a rewrite rather than an assertion: it fixes files at bump time and stays silent when a literal is added afterward. The scanner fails on the seventh copy; `pre-release-replacements` does not. |
| `release-plz` | Version bumps and changelog | Cannot do file replacements outside its own scope, so it does not address US-219 at all. |
| `taiki-e/upload-rust-binary-action` | Build, archive and attach per target | Emits one checksum file per artifact, which is the defect US-217 exists to remove. |

The hand-authored matrix is chosen because the parity-relevant target is the
reference's own workflow shape, and because every candidate above would either
replace an installer this row already scores above upstream on, or reintroduce
the per-target checksum split.

### Platform Constraints From Research
- **`actions/upload-artifact@v4` artifacts are immutable and a duplicate name fails the upload with a 409.** A five-leg matrix therefore uploads under `name: artifacts-${{ matrix.target }}` and the collection job runs `actions/download-artifact` with `pattern: artifacts-*` and `merge-multiple: true`. This is the concrete mechanism behind US-217 and US-218.
- **Hosted ARM Linux runners exist** (`ubuntu-24.04-arm`), so `linux-aarch64` is a native matrix leg rather than a cross-compilation problem. This is what moved the aarch64 risk down.
- **GitHub Artifact Attestations accept a `subject-checksums` input pointing at a checksum manifest**, which is exactly the aggregate `SHA256SUMS` US-217 produces, under `permissions: contents: write, id-token: write, attestations: write`.
- **`sha256sum -c SHA256SUMS --ignore-missing` is the verification a user runs**, and it resolves paths relative to the working directory, so the manifest must be generated from inside the archive directory or verification fails on every line.
- **macOS Gatekeeper applies only to quarantine-attributed files**, and a `curl | sh` download does not set the attribute, so this distribution channel does not require notarization. Marked as inference: no primary source was retrieved for it, and it does not change any story.

## Assumptions & Constraints

### Assumptions (to validate)
- **The GitHub releases API is an adequate version source for this port**, based on the reference having implemented exactly that adapter with a five-second timeout and a rate-limit cause. Validated by US-220's tests against recorded payloads.
- **Every target builds on a hosted runner without cross-compilation**, based on the reference building the same five and on hosted `ubuntu-24.04-arm` runners being available for Linux aarch64. The workflow fails the leg rather than cross-compiling it untested, but with US-218 cancelled nothing runs it, so this assumption stays unvalidated.
- **Merging the cache section breaks no existing reader**, based on `dismissed_version` and `seen_whats_new_version` being the only optional keys and both being written whenever set. Validated by US-225's corpus traces.

### Hard Constraints
- `NOTICE` forbids copying reference source or authored prose. The GitHub gateway's `NOT_FOUND` message is authored prose upstream; this port writes its own naming the same cause and the same next action, and the corpus records the reference's as a length plus a SHA-256 only.
- The reference checkout is read-only. No step of this PRD writes to it.
- The pin (`vibe_core::parity::REFERENCE_COMMIT` and `EXPECTED_COMMIT` in `scripts/parity/pin.py`) does not move in this PRD. Re-pinning requires regenerating every committed corpus in the same change, which is out of scope.
- Committed corpora replay unconditionally; only the live recapture probe may skip when the checkout is absent or off-pin. Any new parity test follows that rule.
- Publishing a GitHub release is an outward-facing, hard-to-reverse action. The workflow is authored and dry-run in this PRD; the first real tag push requires explicit maintainer authorization.
- The declared layering in `[workspace.metadata.vibe] dependency-layers` holds: no crate depends on a later layer.

## Reference Map

Every story names the reference files to open before writing Rust, so the
implementer reads the declaration instead of grepping for it.

**Root.** `/home/arthur/dev/mistral-vibe` on Linux, `C:\dev\mistral-vibe` on
Windows. `VIBE_REFERENCE` overrides both, `--reference` overrides that for the
capture scripts, and Rust resolves it through
`vibe_core::parity::reference_root()`. Reference paths in this document are
written relative to that root in the Linux spelling, which `AGENTS.md` declares
canonical; read them against whichever checkout is local.

**Pin.** `b78b451c39eab9213393ad2f45908e8562a5c5e7`, reference version `2.24.0`,
held by `vibe_core::parity::REFERENCE_COMMIT` and by `EXPECTED_COMMIT` in
`scripts/parity/pin.py`. The local checkout is not guaranteed to sit on it: at
the time of writing it is at `5e6aa0f`, which is `2.24.2`. Read at the pin
rather than at the working tree:

```sh
git -C "${VIBE_REFERENCE:-/home/arthur/dev/mistral-vibe}" show b78b451:<path>
```

Opening the working tree instead measures a different version and produces a
parity claim about code this port is not pinned to. `vibe_core::parity::RESTORE_COMMAND`
documents the restore when a checked-out tree is what a capture needs.

**Read-only, and read without copying.** No step of this PRD writes to the
reference. `NOTICE` forbids pasting its source, prompt files or tool description
text into this repository: reproduce the observed behavior and write original
prose covering the same directives. Only names, JSON pointers and normalized
observations are committed, which is why US-220 records the reference's
`NOT_FOUND` sentence as a length plus a SHA-256 rather than as text.

**Where row 1 lives upstream.** The scorecard's Reference column for row 1 names
`vibe/cli/update_notifier/`, `pyinstaller/`, `vibe.spec`, `vibe-acp.spec`,
`vibe-app-server.spec` and `action.yml`. Two areas it does not name carry
row-1 behavior and are read by this PRD: `vibe/utils/cache_store.py`, which owns
the section merge, and `scripts/bump_version.py`, which owns the version
literals. US-228 adds both to the column.

## Quality Gates

These commands must pass for every user story:
- `cargo fmt --all -- --check` - formatting is uniform across the workspace
- `cargo check --workspace --all-targets --all-features` - every target compiles, including the feature-gated fixture binaries
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` - no lint warning survives
- `cargo test --workspace --all-features` - the full suite, not a filtered subset, because parity fixtures are read from more than one module

For stories that touch a script rather than Rust:
- `sh -n scripts/install.sh` and `pwsh -NoProfile -Command "[void][ScriptBlock]::Create((Get-Content -Raw scripts/install.ps1))"` - both installers parse
- the matching CI job runs green on its native platform

## Epics & User Stories

### EP-064: A release the installers can actually fetch

Publish the artifact set the two installer scripts already know how to consume,
and remove every hand-written string that can drift away from it.

**Definition of Done:** `install.sh` and `install.ps1` resolve their assets from
the repository the manifest declares with no environment override; one aggregate
`SHA256SUMS` covers every packaged target; and no version literal or repository
slug exists in the tree that a test does not assert.

Producing the release itself was scoped as US-218 and cancelled on 2026-08-19:
publication is not part of this phase, which works on the port rather than on its
distribution. `.github/workflows/release.yml` is retained, dormant until a `v*`
tag is pushed, and its shape is still asserted by
`crates/vibe-cli/src/distribution/release_parity_tests.rs`.

#### US-216: Bind the installers' release base to the declared repository
**Description:** As a person installing the port, I want the installer to fetch from the repository the manifest declares, so that the default path resolves instead of returning 404.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None
**Reference:** `.github/workflows/build-and-upload.yml` for the asset names a release actually carries, and `action.yml` for how upstream points a consumer at a release.

**Acceptance Criteria:**
- [ ] Given `scripts/install.sh` with no `VIBE_RELEASE_BASE_URL` set, when the base URL is computed, then it names the owner and repository `[package] repository` in `Cargo.toml` declares.
- [ ] Given `scripts/install.ps1` with no `VIBE_RELEASE_BASE_URL` set, when the base URL is computed, then it names the same owner and repository as `install.sh`.
- [ ] Given a test in `vibe-cli` that parses both scripts, when either script names an owner or repository the workspace manifest does not declare, then the test fails naming the offending file, the line and both strings.
- [ ] Given `VIBE_RELEASE_BASE_URL` set to a `file://` path, when the installer runs, then the override still wins and no network request is made.
- [ ] Given `VIBE_RELEASE_BASE_URL` set to an `http://` URL, when the installer runs, then it exits non-zero without fetching, preserving the existing scheme allowlist.

#### US-217: One aggregate SHA256SUMS across every packaged target
**Description:** As a person installing the port, I want a single checksum manifest covering every published archive, so that verification succeeds regardless of which target I am on.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None
**Reference:** `.github/workflows/build-and-upload.yml`, specifically how it checksums and attaches the matrix output.

**Acceptance Criteria:**
- [ ] Given `scripts/ci/package-release.sh` run for a target into an output directory that already holds `SHA256SUMS`, when it finishes, then `SHA256SUMS` contains both the pre-existing lines and the new target's line, sorted by archive name, with no duplicate archive names.
- [ ] Given the same target packaged twice into the same directory, when the second run finishes, then `SHA256SUMS` holds exactly one line for that archive.
- [ ] Given a `SHA256SUMS` produced by three targets, when `install.sh` looks up its own archive name, then it resolves exactly one digest and ignores the other lines.
- [ ] Given `SHA256SUMS` that names the archive with no matching digest line, when the installer runs, then it exits non-zero before staging any file and names the archive it could not verify.
- [ ] Given `SOURCE_DATE_EPOCH` fixed, when `package-release.sh` runs twice for the same target from the same commit, then the two `.tar.gz` archives are byte-identical.

#### US-218: A tag-driven release workflow that publishes every target - CANCELLED
**Description:** As the maintainer, I want pushing a version tag to build, package, checksum and attach every supported target, so that a release exists without manual steps.

**Cancelled:** 2026-08-19. Every criterion below that a repository can satisfy is
implemented and asserted; the three that remain state properties of a publication
event, and the maintainer defers publishing until parity is proven across the
whole scorecard. Certifying them would have required an outward-facing release of
an incomplete port, and leaving the story open made US-228 unreachable, because
declaring parity was blocked on a release that was itself waiting on parity. The
workflow file stays in the tree; only the obligation to prove a published release
is withdrawn.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-216, US-217
**Reference:** `.github/workflows/build-and-upload.yml` and `.github/workflows/release.yml` for the two-workflow split and the target set, plus `vibe.spec`, `vibe-acp.spec` and `vibe-app-server.spec` for what upstream packages per binary.

**Acceptance Criteria:**
- [ ] Given a tag matching `v*` is pushed, when the workflow runs, then it builds and packages `linux-x86_64`, `linux-aarch64`, `macos-x86_64`, `macos-aarch64` and `windows-x86_64`, mirroring the reference's target set in `.github/workflows/build-and-upload.yml`.
- [ ] Given every matrix leg succeeded, when the collection job runs, then it publishes one GitHub release carrying one archive per target plus exactly one `SHA256SUMS` holding one line per archive.
- [ ] Given the matrix uploads its artifacts, when each leg runs, then it uploads under a name unique to its target and the collection job downloads them with a wildcard pattern and merges them into one directory, because `actions/upload-artifact@v4` rejects a duplicate artifact name with a 409.
- [ ] Given the aggregate manifest is produced, when the collection job builds it, then it runs the checksum tool from inside the archive directory so each line records a bare filename, and a subsequent `sha256sum -c SHA256SUMS --ignore-missing` in that directory exits zero.
- [ ] Given the release is published, when a smoke job runs the committed `install.sh` against the real release URL with no override, then `vibe --version` prints the tagged version and `vibe-acp --help` exits zero.
- [ ] Given the tag does not match `[workspace.package] version`, when the workflow starts, then it fails in the first job naming both values, before building anything.
- [ ] Given one matrix leg fails, when the collection job is reached, then no release is published and the failure names the target that did not build.
- [ ] Given `dist/` is written by a local `package-release.sh` run, when `git status` is inspected, then the directory is ignored and no build output appears as untracked.

#### US-219: One version literal, asserted everywhere else
**Description:** As the maintainer, I want `[workspace.package] version` to be the only place the version is written by hand, so that a bump cannot silently leave a file behind.

The reference solves the same problem in the opposite direction: `update_hard_values_files` rewrites each known literal when the bump runs, and `tests/test_bump_version.py` covers the rewriter. A rewriter stays silent when a seventh literal is added afterward, an assertion does not. This story keeps the assertion and records the mechanism difference as internal design, which `AGENTS.md` leaves free to differ, not as a parity gap.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None
**Reference:** `scripts/bump_version.py`, in particular `update_hard_values_files`, and its test `tests/test_bump_version.py`; the literals it rewrites live in `pyproject.toml`, `vibe/__init__.py` and `distribution/zed/extension.toml`. Upstream solves this by rewriting at bump time, not by asserting; see the note below.

**Acceptance Criteria:**
- [ ] Given `[workspace.package] version` is set to `2.24.0`, matching `vibe_core::parity::REFERENCE_VERSION`, when the workspace builds, then `vibe --version` prints `2.24.0`.
- [ ] Given a test that walks `action.yml`, `.github/workflows/action.yml`, `scripts/install.sh`, `scripts/install.ps1` and `crates/vibe-cli/whats_new.md`, when any of them carries a version string other than the workspace version, then the test fails naming the file, the line and both values.
- [ ] Given the same test, when a file that is supposed to carry the version stops carrying it at all, then the test fails rather than passing vacuously, mirroring the second half of `the_reference_commit_is_written_in_exactly_two_places`.
- [ ] Given `crates/vibe-cli/whats_new.md` after the bump, when `updates::whats_new_content()` is read, then its heading names the workspace version and the existing `starts_with("# What's new in v")` assertion still holds.
- [ ] Given `vibe_core::parity::REFERENCE_VERSION` and the workspace version disagree, when the parity tests run, then the disagreement is reported as a stated fact rather than a failure, because a future re-pin will move one before the other.

---

### EP-065: The update path names the distribution it ships

Make the version check, the prompt and the upgrade action describe the artifact
the running binary actually came from.

**Definition of Done:** a binary installed from a GitHub release checks GitHub
releases, the prompt names an upgrade command that upgrades that binary, and the
prompt's outcome vocabulary and exit codes match the reference's.

#### US-220: Port the GitHub releases update gateway
**Description:** As a person already running the port, I want the update check to read this project's GitHub releases, so that the version it compares against is the one I can actually install.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None
**Reference:** `vibe/cli/update_notifier/adapters/github_update_gateway.py` for the causes and the release selection, and `vibe/cli/update_notifier/ports/update_gateway.py` for the port it satisfies.

**Acceptance Criteria:**
- [ ] Given a releases payload, when the gateway resolves it, then it returns the most recently published release that is neither a draft nor a prerelease, sorted by published date descending, matching `github_update_gateway.py`.
- [ ] Given a tag of `v2.24.0` or `V2.24.0`, when the version is extracted, then the leading `v` is stripped; given a tag with no leading `v`, then it is used verbatim; given an empty or whitespace-only tag, then that release is skipped.
- [ ] Given an HTTP 429, or an `X-RateLimit-Remaining` header of `0`, when the gateway resolves, then it reports the too-many-requests cause.
- [ ] Given HTTP 403, then the forbidden cause; given 404, then the not-found cause with this port's own sentence, not the reference's; given any other error status, then the error-response cause; given a body that is not JSON, then the invalid-response cause; given a transport failure, then the request-failed cause.
- [ ] Given an empty releases list, or a list where every release is a draft or a prerelease, when the gateway resolves, then it reports no update rather than an error.
- [ ] Given the sentence this port writes for the not-found cause, when it is compared against the reference's recorded SHA-256, then the two differ and the sentence is non-empty.
- [ ] Given a request that does not complete within the five-second gateway timeout, when the check runs, then it reports the request-failed cause and the caller is not blocked.

#### US-221: Resolve the update gateway from the running distribution
**Description:** As a person already running the port, I want the update check to pick the source my binary came from, so that it never reports on a different product.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-220
**Reference:** `vibe/cli/update_notifier/update.py` and `vibe/cli/update_notifier/__init__.py` for how the gateway is selected, and `vibe/cli/update_notifier/adapters/pypi_update_gateway.py` for the adapter this port ships today.

**Acceptance Criteria:**
- [ ] Given a production build with no override, when `production_update_gateway` resolves, then it returns the GitHub gateway pointed at the repository `[package] repository` declares, not the PyPI gateway pointed at `mistral-vibe`.
- [ ] Given `VIBE_UPDATE_BASE_URL` is set, when the gateway resolves, then the override wins, preserving the existing environment contract used by the corpus probe.
- [ ] Given `enable_update_checks` is false, when startup runs, then no gateway is constructed and no request is made.
- [ ] Given `--check-upgrade` and a GitHub gateway that reports a newer version, when the command runs, then it prints the existing `{current} → {latest}` line and the existing dialog title.
- [ ] Given `--check-upgrade` and a gateway failure, when the command runs, then it prints the existing `✗ Update check failed: {reason}` line carrying the cause's message and exits without starting a session.
- [ ] Given the PyPI gateway, when the existing `update-gateway` corpus trace replays, then all 13 events still conform, because the adapter is retained rather than removed.

#### US-222: The update action reports the reference's four outcomes
**Description:** As a person already running the port, I want choosing "Update now" to actually attempt an upgrade and tell me what happened, so that the prompt is an action rather than a notice.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-221
**Reference:** `vibe/cli/update_notifier/update.py` for the four outcomes and the terminate timeout, `vibe/setup/update_prompt/update_prompt_dialog.py` for what the dialog returns, and `vibe/cli/cli.py` for the exit code each outcome produces.

**Acceptance Criteria:**
- [ ] Given the update prompt, when its result type is inspected, then it carries four outcomes matching `UpdatePromptResult` in `vibe/setup/update_prompt/update_prompt_dialog.py`: continue, updated, update-failed and quit.
- [ ] Given a configured upgrade command that exits zero, when "Update now" is chosen, then the outcome is updated, the message names the old and the new version, and the process exits with code 0, matching `vibe/cli/cli.py:298-303`.
- [ ] Given every configured upgrade command exits non-zero, when "Update now" is chosen, then the outcome is update-failed, the message names a manual upgrade path for this port, and the process exits with code 1.
- [ ] Given more than one configured upgrade command and any one of them exits zero, when the action runs, then the outcome is updated, matching the reference's any-succeeded rule.
- [ ] Given the action is cancelled while a command is running, when it unwinds, then the child process is terminated and, if it does not exit within two seconds, killed, matching `_terminate`.
- [ ] Given "Continue with current version" is chosen, when the prompt closes, then the session starts and no command runs.
- [ ] Given the `update-presentation` corpus trace, when it replays, then the six events still conform and the `automatic update installation` entry is removed from the corpus `unavailable` list.

---

### EP-066: The on-disk update cache is measured

Port the two filesystem behaviors the reference's cache repository has, and
build the instrument that can tell whether they are correct.

**Definition of Done:** update-cache traces in the terminal-services corpus
replay at least 24 events covering the parsed cache and the document after
write, declaring no divergence for this port's cache store.

#### US-223: Migrate the legacy update_cache.json
**Description:** As a person upgrading from an older layout, I want my recorded update state to survive, so that I am not re-shown release notes I have already read.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None
**Reference:** `vibe/cli/update_notifier/adapters/filesystem_update_cache_repository.py` for the migration read, `vibe/cli/update_notifier/ports/update_cache_repository.py` for the port, and `vibe/utils/cache_store.py` for the file it reads.

**Acceptance Criteria:**
- [ ] Given no `[update_cache]` section in `cache.toml` and a sibling `update_cache.json` holding a valid object, when the store loads, then it returns the values from the JSON file and writes them into `cache.toml`, matching `_read_section` in `filesystem_update_cache_repository.py`.
- [ ] Given the JSON file holds keys whose value is null, when the migration writes the section, then those keys are omitted from the written TOML.
- [ ] Given both a populated `[update_cache]` section and a legacy JSON file, when the store loads, then the TOML section wins and the JSON file is not read.
- [ ] Given a legacy JSON file that is not valid JSON, or is not an object, or cannot be read, when the store loads, then it returns no cache and writes nothing, rather than propagating an error.
- [ ] Given a legacy JSON file whose `latest_version` is not a string or whose `stored_at_timestamp` is not an integer, when the store loads, then it returns no cache, matching the reference's `_parse` guard.
- [ ] Given the migration writes the section and the write fails, when the store loads, then the values are still returned to the caller.

#### US-224: Merge the cache section instead of replacing it
**Description:** As a person whose `cache.toml` is shared by more than one feature, I want a write to my update state to leave everything else alone, so that unrelated keys are not silently dropped.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None
**Reference:** `vibe/utils/cache_store.py`, specifically `write_section` and its update-in-place semantics.

**Acceptance Criteria:**
- [ ] Given a `[update_cache]` section holding a key this port does not model, when the store writes, then that key is still present in the file afterward, matching `write_section` in `vibe/utils/cache_store.py`.
- [ ] Given a section holding `dismissed_version` and a cache whose `dismissed_version` is none, when the store writes, then the existing `dismissed_version` is preserved, because the reference's merge never removes a key.
- [ ] Given another top-level table in `cache.toml`, when the store writes the update section, then that table is unchanged.
- [ ] Given `[update_cache]` exists and is not a table, when the store writes, then it is replaced by a table holding only the written keys, matching the reference's `isinstance` guard.
- [ ] Given a file larger than the one-megabyte ceiling, when the store loads, then it returns no cache and the write path still produces a valid file rather than appending to an unreadable one.
- [ ] Given the write fails, when the store returns, then it reports the cache-write error and the pre-existing file is left intact, because the write is staged and renamed.

#### US-225: Update-cache traces measured against the reference
**Description:** As the maintainer, I want the on-disk cache layout replayed against the reference, so that the two behaviors above are falsifiable rather than asserted.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-223, US-224
**Reference:** the two files US-223 and US-224 name, driven through the local capture script `crates/vibe-cli/tests/runtime-parity/terminal-services-oracle.py`.

**Acceptance Criteria:**
- [ ] Given the capture script `crates/vibe-cli/tests/runtime-parity/terminal-services-oracle.py`, when it runs against the pinned checkout, then it drives the reference's real cache store over a temporary directory rather than the current in-memory fixture.
- [ ] Given the corpus is trace-shaped and not family-shaped, when the measurement is added, then it lands as new `Event` variants carrying the document before the call and the document after it, and as new traces in `crates/vibe-cli/tests/runtime-parity/terminal-services.json`, not as a `Family` copied from `crates/vibe-cli/src/tui/promo_parity_tests.rs`.
- [ ] Given the new `Event` variants are declared in `crates/vibe-cli/src/tui/runtime_parity_tests/terminal_services.rs`, when they are added, then they keep `#[serde(tag = "kind", deny_unknown_fields)]` so a corpus field the replay does not read fails deserialization instead of being ignored.
- [ ] Given the replay asserts an exact story set and an exact `reference.source_files` count, when the new traces land, then both assertions are updated in the same change and a trace dropped from the corpus fails the story-set assertion naming it.
- [ ] Given the committed corpus, when the replay runs, then the new traces contribute at least 24 events, each with its own expected line, and the existing `trace.events.len() == trace.expected.len()` check fails an incomplete trace naming its id.
- [ ] Given a declared divergence whose mismatch has stopped reproducing, when the replay runs, then it fails as stale rather than passing silently.
- [ ] Given the reference checkout is absent or off-pin, when the tests run, then the committed corpus still replays and only the recapture probe skips, naming both commits and the restore command.
- [ ] Given the traces cover the cases US-223 and US-224 name, when the replay runs after both land, then those traces declare no divergences.

---

### EP-067: Installer and completion assurance

Cover the two surfaces that ship to users and that no gate reads today.

**Definition of Done:** `install.ps1` runs green in CI on Windows, and the
committed completion files cannot drift from the flags clap declares.

US-226 was reviewed on 2026-08-19 and left at `IN_REVIEW`. The job, the
verification script and the tests that bind their shapes are committed and
asserted, and `scripts/ci/verify-install-ps1.ps1` was driven green through all
four paths under PowerShell 7 against an archive `scripts/ci/package-release.sh`
had just packaged, which proves the installer's transaction logic. What is not
proven is the Windows leg: the job has never run on a `windows-2022` runner, so
the Windows build, the MSYS path handling `package-release.sh` relies on, and
the job's wall clock are asserted rather than observed. That observation arrives
with the first push and needs no further change.

US-228 therefore does not wait on it, and no longer lists US-226 among its
blockers. The scorecard states what the repository holds: row 1 may credit the
installer coverage as committed and asserted on Windows, and must not claim a
green Windows run until one exists.

#### US-226: Exercise install.ps1 on Windows in CI
**Description:** As a person installing on Windows, I want the PowerShell installer to be tested on Windows, so that a change to it fails before it reaches me.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-217
**Reference:** none. The reference ships no installer script, so this story measures a surface with no upstream counterpart and is scored as an addition rather than as parity.

**Acceptance Criteria:**
- [ ] Given a Windows runner, when the job packages `windows-x86_64` and runs `scripts/install.ps1` against a `file://` base, then `vibe --version` prints the workspace version and `vibe-acp --help` exits zero.
- [ ] Given the archive is present and `SHA256SUMS` names a digest that does not match, when the installer runs, then it exits non-zero before moving any file into the install directory.
- [ ] Given a `.new` or `.previous` file already present in the install directory, when the installer runs, then it refuses and reports a partial upgrade, matching `install.sh`.
- [ ] Given a successful install followed by `-Uninstall`, when the switch runs, then both binaries and the completion file are removed and the command exits zero.
- [ ] Given the job, when it completes, then it takes no longer than 20 minutes, keeping the release matrix within its budget.

#### US-227: Hold the shell completions to clap's declared flags
**Description:** As a person using tab completion, I want the completion files to offer every flag the binary accepts, so that a newly added flag is not invisible.

**Priority:** P2
**Size:** M (3 pts)
**Dependencies:** None
**Reference:** none. The reference ships no shell completions; the oracle for this story is this port's own clap declaration in `crates/vibe-cli/src/lib.rs`.

**Acceptance Criteria:**
- [ ] Given a test that builds the `vibe` command from its clap definition, when it collects every long flag that is not hidden, then the set matches the flag list in `completions/vibe.bash` exactly, with a failure naming each flag present in one and absent from the other.
- [ ] Given `--auto-approve` carries the visible alias `--yolo`, when the comparison runs, then the alias is required in the completion file, closing the one flag that is missing today.
- [ ] Given a flag marked `hide = true`, when the comparison runs, then its absence from the completion file is accepted rather than reported.
- [ ] Given the same comparison applied to `completions/_vibe`, `completions/vibe.fish` and `completions/vibe.ps1`, when any of the four drifts, then the test names the file and the flag.
- [ ] Given a new flag is added to the clap definition and no completion file is updated, when the suite runs, then it fails, naming the flag and every file that omits it.

---

### EP-068: The scorecard states what it measured

Record what cannot be ported, and restate row 1 from evidence rather than
judgement.

**Definition of Done:** every remaining row-1 divergence has a ledger entry with
the artifact that fails if it closes silently, and row 1 reads 100 with each
sub-claim citing the measurement behind it.

#### US-228: Record every remaining divergence and remeasure row 1
**Description:** As a reader of the scorecard, I want each thing this port answers differently to be a decided and evidenced row, so that the score is reproducible from the table.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-219, US-222, US-225, US-226
**Reference:** none upstream. The artifact is local: `docs/parity.md` row 1 and its two divergence tables.

**Acceptance Criteria:**
- [ ] Given `docs/parity.md`, when the accepted-divergence table is read, then it carries a row for the absent standalone `vibe-app-server` command, naming that the reference's own CI never builds `vibe-app-server.spec` and that the console script belongs to a PyPI channel this port does not use.
- [ ] Given the same table, then it carries a row for the release notes being compiled in with `include_str!` rather than read from a runtime root, naming the assertion in `crates/vibe-cli/src/tui/updates.rs` that fails if the file stops shipping.
- [ ] Given the same table, then it carries a row for the upgrade command list differing from `UPDATE_COMMANDS`, naming the artifact that fails if this port's list ever becomes the reference's.
- [ ] Given each new row, when it is read, then its Evidence column names a test, constant or ledger entry that fails when the divergence closes, matching the shape every existing row uses.
- [ ] Given the row-1 State-and-gaps cell, when it is rewritten, then it names the new update-cache traces, their event count, the installer coverage on both platforms, and the version assertion, and it states the score as 100.
- [ ] Given the row-1 score moves, when the weighted total and the last-remeasure field are recomputed, then both are updated in the same change and the arithmetic is stated rather than asserted.
- [ ] Given the `automatic update installation` entry in the corpus `unavailable` list, when US-222 has landed, then the entry is removed rather than reworded.
- [ ] Given the row-1 Reference column, when it is rewritten, then it adds `vibe/utils/cache_store.py` and `scripts/bump_version.py`, the two upstream areas this PRD measured that the column did not name, so the next reader finds the oracle without rediscovering it.
- [ ] Given a claim in the rewritten row-1 cell that no committed test, corpus or constant backs, when the row is reviewed, then the claim is deleted and the score is stated below 100 rather than the claim being restated.

## Functional Requirements

- FR-01: The installers must derive their default release base from the repository the workspace manifest declares, and a test must fail when the two disagree.
- FR-02: The packaging script must accumulate checksum lines into one `SHA256SUMS` covering every archive in its output directory, with no duplicate archive names.
- FR-03: A tag matching `v*` must produce a GitHub release carrying one archive per supported target and exactly one aggregate `SHA256SUMS`.
- FR-04: The release workflow must refuse to build when the tag and `[workspace.package] version` disagree, and must publish nothing when any matrix leg fails.
- FR-05: A test must fail when any of the five files carrying the version by hand names a version other than `[workspace.package] version`, and must also fail when one of them stops carrying it.
- FR-06: The update check must resolve its gateway from the distribution the binary came from, defaulting to the GitHub releases of the declared repository.
- FR-07: The GitHub gateway must map its failures onto the seven existing gateway causes, and must skip drafts and prereleases when selecting the latest release.
- FR-08: The system must NOT ship the reference's authored not-found sentence; it must write its own and a test must hold the two permanently unequal.
- FR-09: The update prompt must expose four outcomes and must exit 0 after a successful update and 1 after a failed one.
- FR-10: The update cache store must read a legacy `update_cache.json` when no TOML section exists, and must write the migrated values into the TOML file.
- FR-11: The update cache store must merge into its section rather than replace it, preserving unknown keys and sibling tables.
- FR-12: Committed corpus traces must replay the on-disk cache layout against the reference, with an event floor and an audited divergence list.
- FR-13: The completion files must list exactly the non-hidden long flags and visible aliases the clap definition declares.

## Non-Functional Requirements

- **Performance:** the startup update check must not delay the first rendered frame; with a gateway that never answers, `preflight` must return in under 100 ms while the five-second gateway timeout runs on its own task.
- **Performance:** the release matrix must complete in under 45 minutes wall clock across all five targets, and the Windows installer job in under 20 minutes.
- **Security:** the installers must accept only `https://` and `file://` schemes, and must verify the archive's SHA-256 before any file is moved into the install directory: 0 bytes executed before verification.
- **Reliability:** an installer failure at any point after staging must leave the previously installed binaries in place: 0 partial installs across a fault-injection matrix covering download, checksum, extraction and move failures.
- **Reliability:** the update cache write must be atomic; a process killed mid-write must leave either the previous file or the complete new file, never a truncated one.
- **Reproducibility:** two runs of `package-release.sh` for the same target from the same commit with the same `SOURCE_DATE_EPOCH` must produce byte-identical `.tar.gz` archives.
- **Coverage:** the new update-cache traces must replay at least 24 events and must fail below that floor.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Empty state | First run, no `cache.toml` and no legacy JSON | The check is planned as a fetch; nothing is shown until an answer arrives | none |
| 2 | Async in progress | Startup refresh running while the session opens | The session opens; the refresh completes on its own task and never blocks input | none |
| 3 | Release asset missing | `SHA256SUMS` present, archive returns 404 | Exit non-zero before staging | names the archive and the resolved URL |
| 4 | Network offline | No route to `api.github.com` during the check | The request-failed cause; the session continues | "✗ Update check failed: {reason}" on `--check-upgrade`, silent at startup |
| 5 | Boundary value | `cache.toml` exceeds one megabyte | The store returns no cache and the write path still produces a valid file | none |
| 6 | Concurrency | Two `vibe` processes writing `cache.toml` at once | Each write is staged and renamed; the file is always complete, last writer wins | none |
| 7 | Permissions | Install directory not writable | Exit non-zero before staging; nothing is left behind | names the directory and the required permission |
| 8 | Interrupted operation | Installer killed between staging and swap | A rerun refuses and reports the partial upgrade rather than compounding it | "partial upgrade detected" naming the `.new` or `.previous` file |
| 9 | Malformed data | Legacy `update_cache.json` is a list, not an object | No cache is returned; nothing is written; no error propagates | none |
| 10 | Version and compatibility | Every GitHub release is a draft or a prerelease | No update is reported, distinct from a gateway failure | none |
| 11 | Rate limiting | `X-RateLimit-Remaining: 0` or HTTP 429 | The too-many-requests cause, using the existing default message | the existing cause message |
| 12 | Authorization | HTTP 404 from a private or misnamed repository | The not-found cause with this port's own sentence naming a token as the next action | this port's sentence, never the reference's |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | Publishing the first real release is outward-facing and hard to reverse; a wrong asset name ships to users | Med | High | The workflow is authored and validated end to end against a prerelease tag on a scratch branch before any public tag; the first public tag requires explicit maintainer authorization, which this PRD does not grant |
| 2 | Linux aarch64 does not build on hosted runners without cross-compilation setup, stalling the first release | Low | Med | The matrix leg is allowed to be the last to land; if native runners are unavailable, the target is dropped from the first release and recorded as a divergence rather than faked, and `install.sh` reports it as unsupported instead of 404 |
| 3 | Switching the production gateway to GitHub changes what every user's update check reports, and a mistake is silent | Med | High | The PyPI gateway and its 13-event corpus trace are retained unchanged, so the switch is a resolution change with both adapters measured; US-221 asserts the resolution rather than deleting the alternative |
| 4 | Merging the cache section changes when `dismissed_version` clears, which no test covers today | Med | Med | US-225 lands the measuring traces before the behavior is trusted; US-224's criteria state the preservation rule explicitly and the traces replay it |
| 5 | Moving the workspace version to 2.24.0 makes the update prompt compare against a version the port has never published | Med | Med | US-218's cancellation leaves this permanent rather than transient: no tag exists, so the GitHub gateway must report no update rather than an error, which criterion 5 of US-220 covers and which US-221 must hold before the gateway is resolved in production |
| 6 | Extending the terminal-services oracle risks invalidating the five existing update traces | Low | High | The new traces and event variants are additive and the existing traces are unchanged; the replay's exact story-set and source-file assertions fail loudly if either drifts |

## Non-Goals

Explicit boundaries: what this version does NOT include.

- **Re-pinning the reference to v2.24.2.** `AGENTS.md` requires regenerating every committed corpus in the same change. That is a separate PRD, and this one deliberately closes the first column of the scorecard rather than the second.
- **Publishing to crates.io, PyPI, Homebrew, Nix or the Zed extension registry.** Parity is a contract, not a channel count, and the row already exceeds upstream on packaging. Revisit when a user asks for a package-manager install.
- **macOS Developer ID signing and notarization.** The reference disables its own macOS standalone smoke tests for exactly this reason; matching that posture is parity, inventing a signing pipeline is not.
- **A standalone `vibe-app-server` binary.** Recorded as an accepted divergence by US-228 instead. The reference's CI never builds its spec, and the console script belongs to a PyPI channel this port does not use.
- **The VS Code extension promo.** That is parity row 33, measured by its own instrument in `crates/vibe-cli/src/tui/promo_parity_tests.rs`, and its suffix on the release-notes body belongs there.
- **The plan-offer call to action** appended to the release-notes body upstream. It is published by `vibe/cli/plan_offer/`, a part row 1's Reference column does not name, and this port has no plan-offer surface at all.
- **Reading release notes from a runtime root.** `include_str!` is retained; recorded as an accepted divergence.

## Files NOT to Modify

- `/home/arthur/dev/mistral-vibe` and every path inside it: the behavioral oracle is read-only, on every platform and under every `VIBE_REFERENCE` value.
- `crates/vibe-core/src/parity.rs` and `scripts/parity/pin.py`: the two pin sources. Re-pinning is out of scope and moving one without the other fails `the_python_mirror_agrees_with_the_rust_pin`.
- `crates/vibe-app-server/tests/tool-surface/baseline.json` and every committed corpus other than `crates/vibe-cli/tests/runtime-parity/terminal-services.json`: regenerating a corpus this PRD does not measure would hide a change it did not make.
- `crates/vibe-cli/tests/promo/corpus.json` and `crates/vibe-cli/src/tui/promo_parity_tests.rs`: row 33's instrument, read here only as the pattern to copy.
- `NOTICE`: the licensing boundary this PRD works within rather than adjusts.

## Technical Considerations

- **Architecture:** the version and slug assertions, recommended: extend the existing scanner pattern in `crates/vibe-core/src/parity/parity_tests.rs` with a sibling test module rather than adding a build script or a code generator. Engineering to confirm the scanner can read the two shell scripts and the two YAML files without a parser dependency.
- **Architecture:** the GitHub gateway placement, recommended: alongside `PyPiUpdateGateway` in `crates/vibe-core/src/updates.rs`, with the resolution living in `crates/vibe-cli/src/tui/startup/update.rs` where `production_update_gateway` already is. This keeps `vibe-core` provider-neutral and the choice in the adapter.
- **Data Model:** the cache section merge: the store currently rebuilds the section as a fresh `toml::map::Map`. Option A: read the existing table and insert into it. Option B: model the section as a typed struct with a `#[serde(flatten)]` catch-all. Trade-off: A is three lines and preserves everything; B is self-documenting and risks reordering keys on write, which would make the corpus's `documentAfterWrite` comparison noisy. A is recommended.
- **API Design:** the upgrade command list, recommended: a constant naming the installer rerun for each platform, resolved the way `UPDATE_COMMANDS` is, so the corpus can record the list as a value rather than as behavior.
- **Dependencies:** no new crate. The GitHub gateway uses the already-pinned `reqwest 0.13.4` with rustls. US-227 needs no `clap_complete` either: clap's own builder API exposes `Command::get_arguments(&self) -> impl Iterator<Item = &Arg>` (clap_builder `command.rs:4006`) and `Arg::get_long(&self) -> Option<&str>`, reached through `<Arguments as CommandFactory>::command()`, which is enough to enumerate every declared long flag and diff it against the committed completion files. `clap_complete` would only be needed to generate the files rather than to assert them, and generating them is not what US-227 asks for. This PRD therefore carries no dependency decision.
- **Migration:** the legacy `update_cache.json` read is one-directional and backward compatible; nothing this port writes creates a JSON file. Rollback plan: the migration is additive to `load`, so reverting it restores today's behavior with no data change.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Parity row 1 score against the pin | 95, unmeasured on distribution | 100, every sub-claim citing a measurement | Month-1 | `docs/parity.md` row 1 after US-228 |
| Version literals not asserted by a test | 6 of 6 | 0 of 6 | Month-1 | the scanner test in US-219 |
| Installer scripts covered in CI on their native platform | 1 of 2, and on a `file://` override | 2 of 2, one of them against the real release URL | Month-1 | the release workflow's smoke jobs |
| Update-cache events replayed | 0 | at least 24 | Month-1 | the new traces in `crates/vibe-cli/tests/runtime-parity/terminal-services.json` |
| Corpus `unavailable` entries for row 1 | 1 (automatic update installation) | 0 | Month-1 | `crates/vibe-cli/tests/runtime-parity/terminal-services.json` |
| Published releases whose assets the committed installers resolve unmodified | 0 | deferred with US-218 | not this phase | the post-publish smoke job |
| Row-1 divergences with no ledger entry | 4 (GitHub gateway, app-server binary, compiled-in notes, command list) | 0 | Month-1 | the accepted-divergence table |

## Open Questions

- **Does the release workflow publish build attestations?** The aggregate `SHA256SUMS` from US-217 is directly usable as an attestation subject, and the permissions it needs are known, but the reference publishes none, so attesting is a departure from parity rather than a step toward it. Deferred with US-218; it adds one job and no story whenever publication is scoped.
- **Which upgrade commands should the update action run?** Rerunning `install.sh` is the only path that certainly works, but it re-downloads rather than upgrading in place. Maintainer to decide before US-222; the answer is one constant and does not block US-220 or US-221.
- **Which `actions/attest-build-provenance` major version applies?** Only relevant if the question above is answered yes; the current major was not verified during research and must be read from the action's own repository before it is pinned.
- ~~**Does the first public tag get published from this work?**~~ Answered 2026-08-19: no. US-218 is cancelled and no release is published from this PRD. The workflow stays in the tree, dormant until a `v*` tag is pushed, and its shape is asserted by the tests US-217 and US-219 landed. The maintainer publishes once parity is proven across the scorecard, which is what EP-068 measures.
[/PRD]
