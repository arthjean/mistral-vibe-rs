[PRD]
# PRD: Native Windows Support for Mistral Vibe RS

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-10-15 | Arthur Jean | Initial draft from the verified Windows audit of 2026-10-14 |
| 1.1 | 2026-10-15 | Arthur Jean | US-006 measured the reference on Windows (`docs/parity.md`, "Windows observations"): FR-01, US-008, US-013, US-015, US-021 and the Open Questions amended where the measurement contradicts this draft |
| 1.2 | 2026-10-16 | Arthur Jean | EP-001 review: US-017 names the ConPTY cursor-position stall and the teardown tags it owns; US-025 added to EP-002 for the keyring divergence US-006 measured |
| 1.3 | 2026-10-16 | Arthur Jean | EP-002 implementation: the stranded-state notice question answered (no notice, manual move documented in `CHANGELOG.md`); persisted path comparisons checked before US-010; US-017 also owns the ACP unified replay, whose bash scenarios time out on windows-2022 |
| 1.4 | 2026-10-17 | Arthur Jean | EP-003 implementation: the US-013 launcher-failure question answered (match the reference); US-017 owns a 14th ConPTY stall tag, the managed `cwd` test US-012 unblocked, whose command then stalls under ConPTY |

## Problem Statement

Mistral Vibe RS compiles on Windows and ships a `windows-x86_64` package with a CI-verified `scripts/install.ps1`, but a Windows user who installs it gets a product that is unsafe in two places and broken in several others. A full audit on Windows 11 (rustc 1.98 MSVC, French locale) reproduced every defect below.

1. **Security boundaries do not hold on Windows.** The permission matcher splits paths on `/` only (`crates/vibe-core/src/policy.rs:1497`), so `sensitive_patterns` such as `.env` never match a Windows path: `read_file` reads a `.env` without asking, `grep` stops filtering sensitive files, and a denylist entry naming a Windows path never denies. Separately, the browser launcher runs `cmd /C start <url>` (`crates/vibe-core/src/auth/sign_in.rs:463`, `crates/vibe-cli/src/tui/workflow/mcp.rs:628`): reproduced, the URL is cut at the first `&` and the remainder runs as a command, so an MCP server can inject commands through its OAuth URL.
2. **State lands in the wrong place.** The CLI, the app-server, and `vibe-acp` resolve the vibe home from `HOME` only (`crates/vibe-cli/src/tui/startup.rs:137`, `crates/vibe-app-server/src/host.rs:38`, `crates/vibe-acp/src/auth.rs:42`). `HOME` does not exist on Windows outside Git Bash (verified absent at User and Machine scope), so config, trust decisions, history, and sessions fall back to `<cwd>\.vibe`, which is also the project-local config directory. `vibe-core` resolves `USERPROFILE` correctly (`crates/vibe-core/src/config/effective.rs:783`), so two homes coexist.
3. **Verbatim paths break features.** 103 production `canonicalize` calls return `\\?\C:\...` paths and only 5 sites strip the prefix. `git worktree add` fails on `//?/C:/...` (every worktree feature is broken, about 35 tests), tool results publish `//?/C:/...` to the model and to editors, and nested `AGENTS.md` discovery silently stops (`crates/vibe-core/src/workspace.rs:954`).
4. **Processes and shells misbehave.** MCP stdio servers configured as `npx` fail with "program not found" because spawning skips PATHEXT (`crates/vibe-core/src/mcp/session.rs:626`). The Unified Harness `process.*` tools look only for `zsh`, `bash`, `sh` (`crates/vibe-harness/src/processes.rs:195`) and always fail with `shell_resolution`. The `cmd.exe` fallback corrupts quotes (`echo "a b"` prints `\"a b\"`). ConPTY sessions kill only the leader process and can report dropped output.
5. **The TUI cannot be typed into on international layouts.** `shortcuts.rs:308` swallows every unclaimed Ctrl chord, and Windows reports AltGr as Ctrl+Alt, so `@ # { } [ ] | \ ~ €` cannot be typed on French AZERTY. crossterm 0.29 never emits `Event::Paste` on Windows, so a multi-line paste submits each line as a separate prompt.
6. **There is no Windows verification signal.** CI runs `cargo test` on ubuntu-22.04 only. On Windows the workspace suite passes 2403 tests, fails 176, and hangs twice (one hang cost 2 h 40 min) because tests clear `SystemRoot` (Winsock then fails), wait without bounds, assert POSIX paths, and replay corpora through Python drivers that assume POSIX. Product regressions and test portability failures are indistinguishable.

**Why now:** the v2.26.0 parity pass made the Unified Harness the default backend (`docs/parity.md`, row 36), which puts the broken `process.*` tools on every Windows session's default path. The release workflow already packages `windows-x86_64` and CI already verifies `install.ps1`, so Windows users can install a build whose sign-in is injectable and whose `.env` protection is off. Peer agents (Claude Code, Codex CLI) shipped native Windows in 2025, which sets the bar users compare against.

## Overview

This PRD makes Mistral Vibe RS behave on native Windows 10 and 11 the way the Python reference behaves there, at the same public boundaries the project already measures: commands, flags, configuration, persisted state, tool semantics, permission decisions, and user-visible output. It does so in six epics ordered by dependency and risk: first a Windows test signal that can be trusted, then path and home identity, then the security boundaries, then process execution, then TUI input, then distribution polish.

The key decisions: (a) the Windows CI job is required from the start, and every test that fails because of a product defect owned by a story here carries `#[cfg_attr(windows, ignore = "windows: <cause> (US-NNN)")]`, so the 2403 tests that pass today are protected immediately and the backlog is mechanically countable; (b) platform knowledge is consolidated in `vibe-core` (layer 1), where the correct home resolver, the verbatim strip, and the Windows shell ladder already live, and the adapters stop reimplementing it; (c) the permission matcher adopts Windows path semantics (both separators, case-insensitive, verbatim prefix removed) on Windows only, so Linux parity corpora replay unchanged; (d) no command interpreter ever parses a URL or a user-controlled string for launching; (e) a validation spike stands up the pinned reference on Windows before the stories whose target behavior depends on it.

Delivery is phased. **Release 1** holds the 17 P0 stories (US-001 to US-015, US-018, US-019) and ends when a Windows user can sign in safely, keep state in `%USERPROFILE%\.vibe`, rely on `.env` protection, run MCP servers through `npx`, run harness processes, create worktrees, type AltGr characters, and paste multi-line text. **Release 2** holds the 8 P1 and P2 stories (US-016, US-017, US-020 to US-025).

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Windows test signal is trustworthy | Required windows-2022 CI job green on every PR, zero hangs, ≤ 90 min | Zero `windows:` ignore tags; 100% of workspace tests pass on Windows |
| Security boundaries hold on Windows | 0 open reproduced security defects (URL injection, sensitive-pattern bypass, denylist bypass) | Unchanged, with regression tests on the required job |
| Windows user flows work end to end | Release 1 shipped: the 6 reproduced blocker root causes closed | Release 2 shipped: 0 open Windows majors from the audit |

## Target Users

### Windows terminal developer
- **Role:** Developer on Windows 10 or 11 using `vibe` in Windows Terminal or conhost with PowerShell 5.1 or 7, often on a non-US layout (French AZERTY, German QWERTZ).
- **Behaviors:** Installs through `install.ps1`, configures MCP servers with `npx`, works in Git repositories, signs in through the browser.
- **Pain points:** Cannot type `@` to mention files or `{` and `[` in code on AZERTY; a pasted stack trace submits one prompt per line; config and trust decisions scatter into each project's `.vibe`; `npx` MCP servers do not start; worktrees fail; sign-in URLs are passed through `cmd`.
- **Current workaround:** Runs the Linux build inside WSL, or runs from Git Bash with `HOME` exported by hand, and types special characters through the clipboard.
- **Success looks like:** Installs, signs in, and works natively in PowerShell with the same capabilities as on Linux, without WSL and without environment tweaks.

### Editor user through ACP
- **Role:** Developer using an ACP-capable editor (Zed, JetBrains, VS Code bridge) on Windows that launches `vibe-acp`.
- **Behaviors:** Reviews tool call locations and diffs in the editor; never sees a terminal.
- **Pain points:** Locations and diffs carry `\\?\C:\...` paths the editor cannot open; the vibe home resolves under the editor's working directory; console windows flash when children start.
- **Current workaround:** Uses WSL remote development so `vibe-acp` runs on Linux.
- **Success looks like:** Clickable native paths, one vibe home shared with the terminal client, no console flashes.

### Maintainer
- **Role:** The project maintainer, who develops on Windows and Linux and gates every change on the AGENTS.md quality gates.
- **Behaviors:** Runs the full workspace suite before proposing a commit; replays parity corpora; ledgers kept divergences in `docs/parity.md`.
- **Pain points:** No Windows signal in CI; a local Windows run takes 3 h 40 min including hangs and mixes 176 failures of different natures; a Windows regression is invisible until a user reports it.
- **Current workaround:** Manual Windows runs and manual triage of each failure.
- **Success looks like:** A required Windows job that finishes in ≤ 90 min, where a red result always means a new regression.

## Research Findings

Key findings that informed this PRD:

### Competitive Context
- Claude Code: native Windows since 2025; uses Git Bash and, since v2.1.120, falls back to PowerShell when Git Bash is absent; users report bash-written commands breaking under the fallback ([issue #53068](https://github.com/anthropics/claude-code/issues/53068), [setup docs](https://code.claude.com/docs/en/setup)). We differ by keeping the reference's own Windows shell ladder, already measured in `crates/vibe-core/src/tools/shell/host.rs`.
- OpenAI Codex CLI (Rust, ratatui): native Windows with PowerShell as the agent shell ([docs](https://developers.openai.com/codex/windows.md)); shipped an AltGr fix that inserts printable Ctrl+Alt characters as text ([commit mirror](https://forge.lthn.ai/core/core-agent-ide/commit/702238f0043b3bbc95bd97ba5131eea6e1160e5a)); had a Windows paste bug dropping characters ([#5843](https://github.com/openai/codex/issues/5843)); users ask for a configurable Windows shell ([#16717](https://github.com/openai/codex/issues/16717)). Closest technical peer: same language and TUI stack.
- Gemini CLI: native, but runs commands through `cmd.exe` by default and is criticized for it ([pitfalls guide](https://claw.aguidetocloud.com/google/gemini-cli/pitfalls/)).
- **Market gap:** none of the peers measures its Windows behavior against an oracle. This port can claim behavioral parity with the Python reference on Windows, with ledgered divergences, which is the project's existing differentiator extended to a second platform.

### Best Practices Applied
- Simplify verbatim paths before display, comparison, or passing to child processes (`dunce::simplified` keeps the prefix only for UNC, reserved device names, and non-Unicode paths; verified in the dunce 1.0.5 source).
- Resolve the home from `USERPROFILE` on Windows, resolve executables through `PATHEXT`, and open URLs without a command interpreter.
- Propagate `SystemRoot` into any cleared child environment: without it `WSAStartup` fails with 10106 ([jpassing](https://jpassing.com/2009/12/28/the-hidden-danger-of-forgetting-to-specify-systemroot-in-a-custom-environment-block)); reproduced here, `vibe update` reports a network error without it and succeeds with it.
- Handle `io::ErrorKind::InvalidInput` when spawning `.bat`/`.cmd` with arguments std cannot escape, the Rust 1.77.2 hardening for BatBadBut ([CVE-2024-24576](https://tenable.com/cve/CVE-2024-24576)).
- Do not rely on `Event::Paste` on Windows: crossterm 0.29 reads `ReadConsoleInput` records and has no paste arm in its Windows event source (crossterm source via Context7).
- Pin line endings with `.gitattributes`; never assert on localized OS error text.

## Assumptions & Constraints

### Assumptions (to validate)
- The pinned Python reference runs on Windows and defines the target behavior there, based on its Windows-specific code paths (`_windows.py` with a ConPTY and WinPTY ladder, ledgered at `docs/parity.md:367`). Validated by US-006.
- The reference resolves the home with `Path.home()`, which on Python 3.8+ prefers `USERPROFILE` and ignores `HOME` on Windows. Validated by US-006.
- The reference matches sensitive patterns with `PurePath.match` on the host flavor and the denylist with `fnmatch`, so on Windows both are case-insensitive and treat `/` and `\` alike, against a resolved path without the `\\?\` prefix (inferred from the comments at `crates/vibe-core/src/policy.rs:1442` and `:1495`). Validated by US-006.
- The ConPTY master reader reaches EOF only when the pseudoconsole closes, not when the child exits (inference from ConPTY semantics; the code chain at `crates/vibe-core/src/pty.rs:84` and `crates/vibe-core/src/process.rs:556` is confirmed). Validated in US-017.
- `clip.exe` mangles non-ASCII text received as UTF-8 without a BOM (suspected from `crates/vibe-cli/src/tui/clipboard.rs:206`, not reproduced). Validated in US-023.
- The Windows keyring entry written by this port is interoperable with the reference's (unverified; the Rust target is `MISTRAL_API_KEY.ai.mistral.vibe`). Validated by US-006.
- `notepad.exe` is an acceptable default external editor when `VISUAL` and `EDITOR` are unset (no reference evidence collected). Validated by US-006.

### Hard Constraints
- Licensing boundary (`NOTICE`, `AGENTS.md`): no reference source, prompt, or tool description text enters the repository; Windows observations are committed only as names, JSON pointers, and normalized observations.
- Linux and macOS behavior is unchanged: every committed corpus replays identically on Linux; no corpus is regenerated outside a re-pin.
- `unsafe_code = forbid` at workspace level (`Cargo.toml:76`): no direct FFI; Windows APIs are reached through std or existing dependencies.
- Dependency layering (`Cargo.toml:88`): shared platform logic lives in `vibe-core`; adapters never gain an edge to a later layer.
- Parity discipline: a kept divergence gets a ledger row in `docs/parity.md` that `ledger_tests` resolves; work that moves a part's parity restates its row and the weighted total (`scorecard_tests`).
- Supported targets: Windows 10 22H2 and Windows 11, x86_64 MSVC, PowerShell 5.1 and 7, Windows Terminal and conhost.

## Quality Gates

These commands must pass for every user story:
- `cargo fmt --all -- --check` - formatting
- `cargo check --workspace --all-targets --all-features` - compilation of every target and feature
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` - lints with warnings denied
- `env -u FORCE_COLOR VIBE_HOME="$(mktemp -d)" cargo test --workspace --all-features --no-fail-fast` - the full workspace suite, run on ubuntu-22.04 and, once US-007 lands, on windows-2022 from Git Bash

For TUI stories (US-018 to US-021, US-023), additional gates:
- Manual verification in Windows Terminal and in conhost on Windows 11 with the French AZERTY and US layouts, recorded in the story's PR description.

## Epics & User Stories

### EP-001: Trustworthy Windows Verification Signal

Make the workspace suite run to completion on Windows and make a red Windows result mean a product regression, so every later story has an executable definition of done.

**Definition of Done:** the windows-2022 job runs the four quality gates on every PR as a required check, finishes in ≤ 90 min with zero hangs, and every remaining Windows-only failure is an `ignore` tag naming a story of this PRD.

#### US-001: Pin line endings in the repository
**Description:** As a maintainer building on Windows, I want every text file checked out with LF endings so that embedded prompts and committed corpora are byte-identical to the Linux build.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given a fresh clone on Windows with `core.autocrlf=true`, when the working tree is checked out, then `git ls-files --eol` reports `w/lf` for every tracked text file.
- [ ] Given `crates/vibe-app-server/tests/tool-execution/tree/crlf.txt`, when it is checked out, then it keeps its CRLF bytes and `.gitattributes` states why in a comment.
- [ ] Given the binary fixture tracked with `-text` today, when `.gitattributes` applies, then its bytes are unchanged.
- [ ] Given `git add --renormalize .` after adding `.gitattributes`, when the index is diffed, then no tracked file changes.
- [ ] Given every production `include_str!` asset, when the workspace is built on Windows, then a test asserts that none of the embedded strings contains `\r`.
- [ ] Given `tool_execution_parity_tests::the_fixture_tree_is_committed_and_deterministic`, when run on Windows, then it passes.

#### US-002: Bound every test wait and keep SystemRoot in cleared environments
**Description:** As a maintainer, I want integration tests that clear the environment to keep the Windows base variables and every wait to be bounded so that a Windows run never hangs and never fails on Winsock initialization.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given the five integration test files that call `env_clear()` (`argv_failures.rs`, `startup_directory_failures.rs`, `update_routes.rs`, `worktree_git_absent.rs`, `worktree_lifecycle.rs` under `crates/vibe-cli/tests/`), when they spawn `vibe` on Windows, then the child environment carries `SystemRoot`, `SystemDrive`, `windir`, `COMSPEC`, `PATHEXT`, `TEMP`, and `TMP` from the parent.
- [ ] Given a test that isolates the home by setting `HOME`, when it runs on Windows, then the same fixture directory is also set as `USERPROFILE`.
- [ ] Given `update_routes::update_is_the_check_upgrade_command`, when the binary never connects to the local server, then the test fails within 30 s with a message naming the missing connection.
- [ ] Given `tools::shell::tests::a_session_that_did_not_complete_fails_even_on_a_zero_exit_code`, when the session never appears in `bash_sessions list`, then the test fails within 30 s.
- [ ] Given every test server thread under `crates/*/tests/` that blocks on `accept()` or `join()`, when the client never arrives, then the wait ends within 60 s.
- [ ] Given a spawn that fails because the binary is missing, when the test runs, then it fails with the spawn error instead of waiting.

#### US-003: Make the Python parity drivers Windows-safe
**Description:** As a maintainer, I want the capture scripts that drive the port to run unchanged on Windows so that corpus replays measure the port rather than the driver.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given the 15 drivers under `scripts/parity/` and `crates/*/tests/` that build a scratch environment from `PATH` alone, when they spawn the port on Windows, then the environment also carries the Windows base set named in US-002.
- [ ] Given `write_tree` (`scripts/parity/acp.py:499`) and every other fixture write, when it runs on Windows, then the bytes on disk equal the corpus content (no newline translation).
- [ ] Given a port process that has exited, when the driver writes to its stdin on Windows and gets `OSError` with `EINVAL`, then the driver treats it like `BrokenPipeError` (`scripts/parity/acp.py:323`) and the scenario records the exit instead of aborting the capture.
- [ ] Given `scripts/parity/telemetry_session.py`, when it is imported on Windows, then the import succeeds (no unconditional `fcntl` import).
- [ ] Given a Linux run of every capture script against the pinned reference, when the corpora are regenerated, then they are byte-identical to the committed ones.
- [ ] Given `--reference` pointing at a missing checkout on Windows, when a driver starts, then it reports the missing checkout and exits non-zero without a traceback.

#### US-004: Make vibe-core tests platform-aware
**Description:** As a maintainer, I want vibe-core tests to assert platform-correct expectations so that a Windows failure in vibe-core always points at product code.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-001, US-002

**Acceptance Criteria:**
- [ ] Given the managed-shell tests in `crates/vibe-core/src/tools/shell/tests.rs` that assume `ShellFamily::Bash`, when they run on Windows, then they exercise the host's managed family (Git Bash or PowerShell) through the same `HostShells` value production uses.
- [ ] Given `the_windows_shell_search_skips_the_wsl_launcher_and_keeps_the_reference_order`, when Git Bash is installed on the host, then the test isolates its search path so the host's real Git Bash cannot be found.
- [ ] Given assertions that spell paths with `/` or `/tmp` (`events::detail::detail_tests`, `parity::parity_tests`, `tools::shell::tests::the_legacy_variant_ignores_the_managed_overrides`), when they run on Windows, then they compare platform-native paths.
- [ ] Given `config::layers_parity_tests` and `telemetry::telemetry_parity_tests`, when a corpus records a host-dependent value (path separator, platform, OS), then the port under test receives the corpus host as an injected value, following the system-prompt `os` family pattern (`docs/parity.md`, row 19).
- [ ] Given tests that assert OS error text, when run under a non-English Windows locale, then they compare `io::ErrorKind` or the raw OS code.
- [ ] Given a vibe-core test that fails on Windows because of a product defect owned by a story of this PRD, then it carries `#[cfg_attr(windows, ignore = "windows: <cause> (US-NNN)")]`; given a test that cannot run on Windows by design, then its ignore reason states why, and no test is deleted.

#### US-005: Make adapter, harness, and voice tests platform-aware
**Description:** As a maintainer, I want the vibe-app-server, vibe-cli, vibe-acp, vibe-harness, and vibe-voice tests to assert platform-correct expectations so that their Windows failures always point at product code.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001, US-002, US-003

**Acceptance Criteria:**
- [ ] Given `server::server_tests::profile_tests` (the `/plans/*` rule) and `resource_tests` (managed shell family), when run on Windows, then they expect the Windows separator and the host's managed family.
- [ ] Given `vibe-voice` `voice_parity_tests` (`no-input-device`) and `vibe-acp` voice parity (`dictation-refused`), when replayed on Windows, then the microphone access hint is selected from the corpus platform, not the host.
- [ ] Given `projects::store::store_tests::a_failed_write_reads_as_python_reports_an_os_error` under a French locale, when run, then it compares the error kind or code, not "Permission denied".
- [ ] Given `connector_catalog_tests::a_cached_catalog_round_trips_while_it_is_fresh` and `mcp_stdio_e2e`, when they fail on Windows, then the cause is diagnosed and recorded in the test's ignore reason or fixed.
- [ ] Given `crates/vibe-cli/tests/tui_pty.rs` and `installer_scripts.rs`, which compile to zero tests on Windows, then each file header states why, and the Windows install path is covered by `scripts/ci/verify-install-ps1.ps1`.
- [ ] Given a test that fails on Windows because of a product defect owned by a story of this PRD, then it carries the `windows:` ignore tag naming that story.

#### US-006: Validate assumption: the pinned reference runs on Windows and fixes the Windows contracts
**Description:** As a maintainer, I want normalized observations of the reference's Windows behavior so that the stories that depend on it implement measured behavior rather than guesses.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given `C:\dev\mistral-vibe` checked out at `vibe_core::parity::REFERENCE_COMMIT`, when `uv sync --frozen` runs, then `.venv\Scripts\python.exe -c "import vibe.cli.entrypoint"` succeeds, or the blocking error is recorded verbatim in this PRD's Open Questions.
- [ ] Given the reference on Windows, when observed, then one normalized observation is recorded for each of: home directory resolution with and without `HOME`; sensitive-pattern, allowlist, and denylist decisions for `\`, `/`, and mixed-case paths; the Unified Harness shell and `process.*` launcher; the keyring target name for the API key; the browser launcher used by sign-in; the default external editor.
- [ ] Given each observation, when committed, then it carries no reference-authored text and lives either as a unit-test expectation or as a Windows note in `docs/parity.md`.
- [ ] Given an observation that contradicts a decision in this PRD, when recorded, then the affected story's acceptance criteria are updated before that story starts.
- [ ] Given the reference cannot run on Windows at the pin, when the spike ends, then US-008, US-011, US-015, and US-021 proceed on the decisions logged here and each divergence they introduce gets a ledger row.

#### US-007: Gate every change on a required Windows test run
**Description:** As a maintainer, I want a required windows-2022 CI job running the four quality gates so that no Windows regression merges unnoticed.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001, US-002, US-003, US-004, US-005

**Acceptance Criteria:**
- [ ] Given a pull request, when CI runs, then a windows-2022 job runs `cargo fmt --check`, `cargo check`, `cargo clippy -D warnings`, and the workspace test command from Git Bash with `VIBE_HOME` pointing at a fresh temporary directory.
- [ ] Given the 14 Windows-only compiler warnings found by the audit (unread `group` fields in `crates/vibe-core/src/child.rs:64`, `crates/vibe-core/src/pty.rs:81`, `crates/vibe-harness/src/processes.rs:113`; unused imports and unreachable code in test and TUI modules), when clippy runs on Windows, then it reports zero warnings.
- [ ] Given the job, when it completes, then its wall time is ≤ 90 min, the test log is uploaded as an artifact, and the job summary prints the count of `windows:` ignore tags.
- [ ] Given a hung test, when the test step exceeds 100 min, then the step is canceled and the uploaded log shows the last test binary started.
- [ ] Given the branch protection settings, when the job is green on `main`, then it is listed as a required check.

---

### EP-002: Path and Home Identity

Give every crate one vibe home and one non-verbatim path form, so that state, tool results, editors, and git all see the same paths a Windows user types.

**Definition of Done:** `vibe`, `vibe-acp`, and the app-server resolve the same vibe home on Windows; no tool result, wire payload, ACP location, or git argument contains `\\?\` or `//?/`; every worktree test passes on Windows; the port and the reference read each other's stored credentials on Windows.

#### US-008: Resolve one vibe home on every entry point
**Description:** As a Windows terminal developer, I want `vibe`, `vibe-acp`, and the app-server to keep their state in `%USERPROFILE%\.vibe` so that my config, trust decisions, and history follow me across projects.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given Windows with `USERPROFILE=C:\Users\A` and no `HOME`, when `vibe`, `vibe-acp`, and the app-server start, then all three resolve `C:\Users\A\.vibe`.
- [ ] Given `VIBE_HOME` set (including a value starting with `~`), when any entry point starts, then `VIBE_HOME` wins on every platform.
- [ ] Given Windows with `HOME` exported by Git Bash to a different directory, when an entry point starts, then `USERPROFILE` decides; US-006 measured that the reference never reads `HOME` on Windows.
- [ ] Given Linux or macOS with `HOME` set, when any entry point starts, then the resolved home is unchanged from today.
- [ ] Given Windows with neither `USERPROFILE` nor `HOMEDRIVE`+`HOMEPATH` set (whether or not `HOME` is set, as US-006 measured), when an entry point starts, then it exits non-zero with a message naming `VIBE_HOME` and never writes `<cwd>\.vibe`.
- [ ] Given the sites `crates/vibe-cli/src/tui/startup.rs:137`, `crates/vibe-app-server/src/host.rs:38`, `crates/vibe-acp/src/auth.rs:42`, `crates/vibe-cli/src/tui/startup/trust.rs:77`, `crates/vibe-cli/src/tui/startup/worktree.rs:281`, and `crates/vibe-harness/src/tools/files.rs:100`, when the change lands, then each calls the single `vibe-core` resolver and no other production code reads `HOME` directly.

#### US-009: Normalize verbatim paths at the core boundary
**Description:** As an editor user, I want every path vibe-core publishes to be the form I would type so that tool results, diffs, and instruction discovery work on Windows.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given a workspace opened at `C:\work\proj`, when any file tool returns a path, then the path contains neither `\\?\` nor `//?/`.
- [ ] Given a nested `AGENTS.md` under the workspace on Windows, when a file below it is read, then the instruction file is injected once per session (`workspace::tests::a_subdirectory_instruction_file_is_injected_once_per_session` passes).
- [ ] Given a UNC share path, a path containing a reserved device name such as `con.txt`, or a non-Unicode path, when normalized, then the verbatim prefix is kept and file operations on it still succeed.
- [ ] Given the existing strip sites (`crates/vibe-core/src/worktree.rs:1105` and its callers in `shell.rs`, `trust.rs`, `worktree/record.rs`), when the change lands, then they use the shared helper and `strip_verbatim_prefix` has one implementation.
- [ ] Given a path whose tail does not exist, when resolved for policy, then the result is a non-verbatim absolute path and a `missing/../../escape.txt` traversal out of the root is still denied (`policy::policy_tests::a_path_below_a_missing_directory_resolves_without_admitting_a_traversal` passes).

#### US-010: Publish non-verbatim paths from every crate
**Description:** As a Windows terminal developer, I want worktrees, review, skills, rewind, and editor projections to use non-verbatim paths so that git accepts them and every surface shows clickable paths.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-009

**Acceptance Criteria:**
- [ ] Given a Git repository on Windows, when a managed worktree is created through the CLI, the app-server, or vibe-core, then `git worktree add` receives a non-verbatim path and succeeds (the worktree tests in all three crates pass).
- [ ] Given `review/state` on Windows, when it answers, then every `path` field is non-verbatim.
- [ ] Given ACP tool call locations and diffs (`crates/vibe-acp/src/projection.rs:769`, `crates/vibe-acp/src/tools.rs:644`), when sent to the editor, then their paths are non-verbatim.
- [ ] Given skill diagnostics and rewind checkpoints in the app-server, and the harness rewind file list (`crates/vibe-harness/src/rewind/rewind_tests.rs:58`), when published, then their paths are non-verbatim.
- [ ] Given the remaining production `canonicalize(` calls, when reviewed, then each either goes through the shared helper or carries a comment stating why the verbatim form is required.
- [ ] Given a worktree name that git rejects on Windows, when creation fails, then the error names the path and neither the worktree directory nor the branch is left behind.

#### US-025: Share stored credentials with the reference on Windows
**Description:** As a Windows terminal developer who has used the Python reference, I want the port to read and write my API key and MCP OAuth tokens where the reference keeps them so that switching between the two never asks me to sign in again.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given an API key the reference stored on Windows (generic credential, target `ai.mistral.vibe`, user name `MISTRAL_API_KEY`, as US-006 measured), when the port resolves credentials without `MISTRAL_API_KEY` in the environment, then it finds that key.
- [ ] Given the port stores an API key on Windows, when the reference resolves credentials, then it finds that key, and the port writes no new entry under its former target `MISTRAL_API_KEY.ai.mistral.vibe`.
- [ ] Given another account already holds the plain `ai.mistral.vibe` target (an MCP OAuth token, for example), when either side stores a second secret, then both sides read it under the compound target `<account>@ai.mistral.vibe` the reference uses.
- [ ] Given a key a previous port build stored under `MISTRAL_API_KEY.ai.mistral.vibe`, when the port resolves credentials, then it still finds the key and moves it to the reference's target, as `PRIOR_BUILD_KEYRING_SERVICE` already does for an older service name (`crates/vibe-core/src/auth/keyring.rs:22`).
- [ ] Given `mcp-oauth:{alias}:tokens` and `mcp-oauth:{alias}:client_info` (`crates/vibe-core/src/auth/mcp_oauth.rs:5`), when stored on Windows, then they follow the same target rule as the API key.
- [ ] Given Linux or macOS, when credentials are stored or read, then the keyring entries are unchanged.

---

### EP-003: Security Boundaries on Windows

Make the permission policy, URL opening, and MCP process launch as safe on Windows as on Linux.

**Definition of Done:** sensitive patterns, allowlists, and denylists decide Windows paths as the reference does; no user- or server-controlled string reaches a command interpreter for launching; MCP stdio servers start through PATHEXT with BatBadBut failures reported.

#### US-011: Match permission patterns with Windows path semantics
**Description:** As a Windows terminal developer, I want `.env` protection, allowlists, and denylists to apply to Windows paths so that the agent cannot read secrets or denied files without my approval.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-006, US-009

**Acceptance Criteria:**
- [ ] Given the default `sensitive_patterns`, when `read_file` targets `C:\proj\.env` or `C:\PROJ\.ENV`, then a sensitive-file requirement is raised even when the tool permission is `always`.
- [ ] Given a workspace containing `.env`, when `grep` runs on Windows, then matches inside `.env` are dropped.
- [ ] Given a denylist entry `C:/proj/notes/*` or `C:\proj\notes\*`, when `read_file` targets `C:\proj\notes\a.txt`, then the permission is `never`, and the denylist is consulted before the allowlist.
- [ ] Given an allowlist entry and a target differing only in letter case, when resolved on Windows, then the target is granted.
- [ ] Given a pattern anchored on another drive letter, when a path on `C:` is checked, then it does not match.
- [ ] Given Linux, when the policy parity tests replay, then every decision is unchanged.

#### US-012: Detect outside paths and home operands in Windows shell commands
**Description:** As a Windows terminal developer, I want shell commands that touch files outside the workspace or under my home to be asked about so that the shell tool cannot bypass the file policy.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-008, US-011

**Acceptance Criteria:**
- [ ] Given `cat C:\outside\secret.txt` or `cat C:/outside/secret.txt` in the Git Bash family, when resolved, then the operand is positioned outside the workspace and approval is required.
- [ ] Given `cat ~/secret` or `cat ~\secret` on Windows, when resolved, then `~` expands to the home resolved by US-008 and approval is required (`shell::tests::a_home_relative_operand_is_expanded_before_it_is_positioned` passes).
- [ ] Given `cd <outside> && git status`, when resolved, then the requirement and its invocation pattern match the reference (`shell::shell_parity_tests` pass on Windows).
- [ ] Given an operand the Windows path parser cannot position (a bare `C:`, a literal `\\?\` prefix), when resolved, then the command is asked about and never silently granted.
- [ ] Given the `Env:\SECRET` behavior ledgered at `docs/parity.md:440`, when this story lands, then that ledger row still holds.

#### US-013: Open URLs without a command interpreter
**Description:** As a Windows terminal developer, I want sign-in and MCP authorization URLs opened directly by the OS so that no server-provided URL can run a command on my machine.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given a URL containing `&`, `|`, `^`, `%PATH%`, and `&echo pwned>%TEMP%\vibe-pwned.txt`, when sign-in or MCP authorization opens it, then the browser receives the full URL and `%TEMP%\vibe-pwned.txt` is not created.
- [ ] Given a URL whose scheme is not `http` or `https`, when an open is requested, then it is refused and printed for manual copy. US-006 measured that the reference opens any scheme, so this hardening is a kept divergence and lands with a ledger row.
- [ ] Given the launcher fails (no default browser), when an open is requested, then the port answers as the reference US-006 measured: sign-in stops with the `OPEN_BROWSER_FAILED` error code, and MCP authorization leaves the URL printed and keeps waiting for the callback. Decided by the maintainer on 2026-10-17 (match the reference, no ledger row).
- [ ] Given `crates/vibe-core/src/auth/sign_in.rs:463` and `crates/vibe-cli/src/tui/workflow/mcp.rs:628`, when the change lands, then both call one shared launcher and no production code spawns `cmd` with `start`.
- [ ] Given Linux or macOS, when an open is requested, then the existing launcher behavior is unchanged.

#### US-014: Launch MCP stdio servers through PATHEXT
**Description:** As a Windows terminal developer, I want MCP servers configured as `npx` or `bunx` to start so that my MCP configuration works unchanged across platforms.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given `command = "npx"` on Windows with Node installed, when the server starts, then `npx.cmd` is resolved through `PATH` and `PATHEXT` and the server completes its handshake.
- [ ] Given a command naming an absolute path or a name with an extension, when the server starts, then it is used as written.
- [ ] Given an argument std refuses to escape for a `.cmd` target (`io::ErrorKind::InvalidInput`), when the server starts, then the connection fails with a message naming the server and the unescapable argument.
- [ ] Given a program that resolves to nothing, when the server starts, then the error lists the names tried.
- [ ] Given the duplicated resolvers `crates/vibe-core/src/worktree/git.rs:118` and `crates/vibe-core/src/plugins/materialize.rs:469`, when the change lands, then MCP, git, and plugin materialization share one resolver.
- [ ] Given `crates/vibe-app-server/tests/mcp_stdio_e2e.rs`, when run on Windows, then it passes with no dispatch warnings.

---

### EP-004: Process Execution on Windows

Make every process the agent starts run under a resolvable shell with intact arguments, complete output, and no descendant process left after it ends.

**Definition of Done:** Unified Harness `process.*` runs on Windows; the `cmd.exe` fallback preserves quoting; PTY sessions deliver complete output and leave no surviving descendants.

#### US-015: Run harness processes under a Windows shell
**Description:** As a Windows terminal developer on the default Unified Harness, I want `process.*` and the harness shell tool to run under Git Bash or PowerShell so that background processes work on Windows.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-006

**Acceptance Criteria:**
- [ ] Given a unified session on Windows with Git Bash installed, when `process.start` runs `echo hi`, then the process runs under Git Bash and its output is `hi`.
- [ ] Given a unified session on Windows without Git Bash, when `process.start` runs `Write-Output hi`, then it runs under PowerShell with PowerShell arguments (no `-lc`) and its output is `hi`.
- [ ] Given a configured shell path that does not exist, when `process.start` runs, then it fails with `start_failed("shell_resolution")` and the message names the path tried.
- [ ] Given the harness shell tool (`crates/vibe-harness/src/tools/shell.rs:211`) and `process.*` (`crates/vibe-harness/src/processes.rs:195`), when both run on the same host, then they resolve the same shell, through the `HostShells` ladder in `crates/vibe-core/src/tools/shell/host.rs`.
- [ ] Given the harness prompt's description of the shell tool, when the session runs on Windows, then it names the shell family actually resolved, as US-006 measured: one command environment (`git_bash` when Git Bash is found, `powershell` otherwise) serves both the shell tool and `process.*`, and a Git Bash process runs as `bash -c` (not `-lc`).

#### US-016: Preserve quoting in the cmd.exe fallback
**Description:** As a Windows terminal developer without Git Bash or PowerShell on the shell path, I want commands with quotes to reach `cmd.exe` intact so that the fallback shell runs what the model wrote.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given the `cmd.exe` fallback, when `echo "a b"` runs, then the output is `"a b"`.
- [ ] Given the `cmd.exe` fallback, when `findstr /c:"x y" file.txt` runs on a file containing `x y`, then the matching line is returned.
- [ ] Given the sites `crates/vibe-core/src/tools/shell/host.rs:557`, `crates/vibe-core/src/tools/shell.rs:878`, and `crates/vibe-core/src/process.rs:267`, when the change lands, then they pass the command line as one raw argument, as `crates/vibe-core/src/hooks/executor.rs:122` does.
- [ ] Given a command with an unbalanced quote, when it runs, then `cmd.exe`'s own error and exit code are returned unchanged.

#### US-017: Drain ConPTY output and kill whole process trees
**Description:** As a Windows terminal developer, I want PTY sessions to return their complete output and to leave no orphaned processes so that long-running commands behave as they do on Linux.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given a PTY session whose child exits after printing 1 MiB, when the session completes, then the reader reaches EOF within 2 s of the exit, `output_dropped` is false, and the full output is returned in 50 of 50 runs.
- [ ] Given a PTY session that started a background grandchild, when the session is killed or times out, then no descendant process survives 2 s later (`crates/vibe-core/src/pty.rs:385` and `crates/vibe-harness/src/processes.rs:1039`).
- [ ] Given the pseudoconsole cannot be closed without error, when the session ends, then the session reports an error and the child tree is still killed.
- [ ] Given a pseudoconsole created with `PSEUDOCONSOLE_INHERIT_CURSOR` (the portable-pty 0.9 default), when it sends its startup cursor-position request (`ESC[6n`), then the session answers it, the child's first output arrives without waiting on the answer, and the 14 tests tagged "ConPTY stalls on an unanswered cursor-position request (US-017)" in `crates/vibe-core/src/tools/shell/tests.rs` pass on windows-2022 with their tags removed.
- [ ] Given the 9 tests tagged "a ConPTY session outlives its teardown on Windows Server 2022 (US-017)" in the same file, when this story lands, then they pass on windows-2022 with their tags removed.
- [ ] Given `acp_parity_tests::the_port_answers_every_unified_scenario_as_the_corpus_records_or_as_the_ledger_names` in `crates/vibe-acp/tests/`, tagged "the bash scenarios time out under ConPTY on Windows Server 2022 (US-017)" after EP-002 measured `prompt/bash-approved*` failing on windows-2022 while passing on Windows 11, when this story lands, then it passes on windows-2022 with its tag removed.
- [ ] Given `docs/parity.md` "The Windows terminal ladder is one rung" (row at line 367), when this story lands, then the ledger row still holds.

---

### EP-005: TUI Input on Windows

Make the composer accept every character and every paste a Windows user produces.

**Definition of Done:** AltGr characters type on AZERTY and QWERTZ, multi-line pastes land as one prompt, Windows paths become valid mentions, and the external editor opens on Windows.

#### US-018: Type AltGr characters in the TUI
**Description:** As a Windows terminal developer on French AZERTY, I want AltGr combinations to insert their characters so that I can type `@` mentions and code.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given French AZERTY, when AltGr with `0`, `3`, `4`, `5`, `6`, `8`, `=`, `)`, `2`, and `E` is pressed, then `@ # { [ | \ } ] ~ €` are inserted.
- [ ] Given German QWERTZ, when AltGr with `Q`, `7`, and `0` is pressed, then `@ { }` are inserted.
- [ ] Given a synthetic `KeyEvent` with `KeyCode::Char('@')` and modifiers `CONTROL | ALT`, when handled by `crates/vibe-cli/src/tui/shortcuts.rs`, then `@` is inserted (unit test).
- [ ] Given Ctrl+Alt with an ASCII letter that produces the unshifted letter, when handled, then it is not inserted as text.
- [ ] Given AltGr+`0` on AZERTY at the start of a word, when typed, then the file mention completion opens.

#### US-019: Paste multi-line text on Windows
**Description:** As a Windows terminal developer, I want a pasted multi-line block to land in the composer as one prompt so that pasting a stack trace never sends partial prompts.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given a 50-line clipboard pasted with Ctrl+V or right-click in Windows Terminal and in conhost, when the paste completes, then the composer holds all 50 lines and nothing is submitted.
- [ ] Given a 10,000-character paste, when it completes, then all characters are present within 1 s.
- [ ] Given a person typing with at least 20 ms between keys and pressing Enter, when Enter is pressed, then the prompt is submitted.
- [ ] Given a paste containing Tab or Esc characters, when it lands, then it neither triggers completion nor cancels the composer.
- [ ] Given a paste ending with a newline, when the burst ends, then the trailing newline is inserted rather than submitting.
- [ ] Given Linux or macOS, when a bracketed paste arrives as `Event::Paste`, then handling is unchanged.

#### US-020: Accept Windows paths in mentions and image attachments
**Description:** As a Windows terminal developer, I want dropped and typed Windows paths to become valid mentions so that I can attach images and files by path.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given a dropped or pasted `C:\Users\A\image one.png`, when the composer converts it, then it inserts a quoted mention and no `\ ` escape.
- [ ] Given an unquoted `C:\x.png` typed in a prompt, when parsed (`crates/vibe-cli/src/tui/path_mentions.rs:168`), then it is recognized as an image mention.
- [ ] Given a UNC path `\\server\share\a.png`, when dropped, then it is inserted as a quoted mention.
- [ ] Given a dropped path that does not exist, when converted, then no attachment is created and the text stays as typed.

#### US-021: Open the external editor on Windows
**Description:** As a Windows terminal developer, I want the external editor command to work with Windows paths and defaults so that I can compose long prompts in my editor.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-006

**Acceptance Criteria:**
- [ ] Given Windows with neither `VISUAL` nor `EDITOR` set, when the external editor is requested, then `notepad.exe` opens. US-006 measured that the reference falls back to `nano` on every platform (a spawn that fails on stock Windows); decision required before this story starts: match the reference, or keep `notepad.exe` with a ledger row.
- [ ] Given `EDITOR="C:\Program Files\Microsoft VS Code\bin\code.cmd" --wait`, when the editor is requested, then the executable path keeps its backslashes and `--wait` is passed as an argument (`crates/vibe-cli/src/tui/input.rs:934`).
- [ ] Given `EDITOR` naming a missing executable, when the editor is requested, then an error names the executable and the composer content is unchanged.

---

### EP-006: Windows Distribution and Polish

Make the installed Windows package reachable, verified at release, and free of console windows under editors.

**Definition of Done:** `install.ps1` puts `vibe` on the user PATH, the release smoke job covers Windows, clipboard copy preserves Unicode, and `vibe-acp` children never flash a console.

#### US-022: Ship and smoke-test the Windows package
**Description:** As a Windows terminal developer, I want the installer to put `vibe` on my PATH and every release to be smoke-tested on Windows so that a fresh install works from any new shell.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-007

**Acceptance Criteria:**
- [ ] Given a fresh install through `scripts/install.ps1`, when a new PowerShell starts, then `vibe --version` resolves from the user PATH.
- [ ] Given the install directory already on the user PATH, when `install.ps1` runs again, then PATH holds the directory once.
- [ ] Given a user PATH that cannot be written, when `install.ps1` runs, then it prints the manual command and exits non-zero.
- [ ] Given a release, when the smoke job runs, then it covers the `windows-x86_64` package with `vibe --version` and a programmatic `-p` run against a local stub backend.
- [ ] Given `scripts/ci/verify-install-ps1.ps1`, when it runs, then it asserts the PATH update.

#### US-023: Copy Unicode text to the Windows clipboard
**Description:** As a Windows terminal developer, I want text copied from the TUI to paste back unchanged so that accented and non-Latin text survives a copy.

**Priority:** P2
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given the text `é ü 漢字 🙂`, when copied from the TUI and pasted into Notepad, then Notepad shows the same text.
- [ ] Given the copy mechanism is unavailable, when a copy is requested, then the TUI shows an error and keeps running.
- [ ] Given the audit's suspicion of mojibake (`crates/vibe-cli/src/tui/clipboard.rs:206`), when this story starts, then the defect is reproduced or the story is closed as not reproducible with the evidence recorded.

#### US-024: Harden vibe-acp children and environment on Windows
**Description:** As an editor user, I want `vibe-acp` to start its children without console windows and to tolerate any environment so that the editor integration stays invisible and never crashes.

**Priority:** P2
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given `vibe-acp` launched by an editor without a console, when it starts shell, MCP, or git children, then no console window appears.
- [ ] Given the `vibe` TUI running in a console, when it starts children, then interactive commands still share the console.
- [ ] Given an environment variable whose value is not valid Unicode, when `vibe-acp` reads its environment (`crates/vibe-acp/src/auth/environment.rs:57`), then it skips or reports the variable and does not panic.

---

## Functional Requirements

- FR-01: On Windows, the system must resolve the vibe home from `VIBE_HOME`, then `USERPROFILE`, then `HOMEDRIVE` plus `HOMEPATH`, through one `vibe-core` resolver used by every entry point; `HOME` is not read on Windows (measured by US-006).
- FR-02: The system must NOT write user state under `<cwd>\.vibe` because no home could be resolved; it must exit with an error naming `VIBE_HOME`.
- FR-03: The system must publish paths without the `\\?\` or `//?/` prefix in tool results, wire payloads, ACP projections, logs shown to users, and child process arguments, except where removing it changes the path's meaning.
- FR-04: On Windows, sensitive patterns, allowlists, and denylists must match case-insensitively and treat `/` and `\` as equivalent separators.
- FR-05: The system must NOT pass a URL or any server- or model-provided string to a command interpreter in order to launch a program.
- FR-06: The system must refuse to open URLs whose scheme is not `http` or `https`, and print them instead.
- FR-07: When spawning a configured program by bare name on Windows, the system must resolve it through `PATH` and `PATHEXT`.
- FR-08: When std refuses to escape an argument for a `.bat` or `.cmd` target, the system must report the failure with the program and argument instead of a generic spawn error.
- FR-09: On Windows, Unified Harness `process.*` and the harness shell tool must run under the shell resolved by the `HostShells` ladder.
- FR-10: The `cmd.exe` fallback must pass the command line to `cmd.exe /d /c` without re-quoting.
- FR-11: When a PTY session ends, the system must return all output written before the child exited and terminate every descendant process.
- FR-12: The composer must insert printable characters delivered with `CONTROL | ALT` modifiers as text, except an ASCII letter equal to its unshifted key.
- FR-13: On Windows, the composer must treat a burst of key events as a paste, inserting newlines rather than submitting.
- FR-14: Integration tests and parity drivers that clear the environment must carry the Windows base variables (`SystemRoot`, `SystemDrive`, `windir`, `COMSPEC`, `PATHEXT`, `TEMP`, `TMP`).
- FR-15: Every test wait on a socket, a thread join, or a polling loop must be bounded.
- FR-16: A test failing on Windows because of a known product defect must carry an ignore tag naming the owning story; no test may be deleted to make Windows green.

## Non-Functional Requirements

- **Performance:** The required Windows CI job completes in ≤ 90 min on windows-2022; no single test binary exceeds 15 min. A 10,000-character paste lands in ≤ 1 s. Vibe home resolution adds ≤ 1 ms to startup.
- **Security:** 0 call sites spawn `cmd` with user-, model-, or server-controlled data except the shell tools, hooks, and fallback shell whose contract is to run commands; sensitive-pattern matching on Windows detects 100% of the cases in the policy tests across `\`, `/`, and case variants; 0 regressions in Linux policy parity replays.
- **Accessibility:** 100% of printable characters on the US, French AZERTY, and German QWERTZ layouts are typeable in the composer; every action reachable by keyboard on Linux stays reachable on Windows.
- **Scalability:** Path normalization handles paths up to 32,767 characters (the Windows extended-length limit) without error; paste detection handles bursts of 100,000 characters without dropping input.
- **Reliability:** 0 hangs in 10 consecutive Windows CI runs; PTY output completeness in 50 of 50 runs; 0 surviving descendant processes 2 s after a session kill; Linux corpora replay byte-identical (0 drift).
- **Compatibility:** Windows 10 22H2 and Windows 11 x86_64; PowerShell 5.1 and 7; Windows Terminal and conhost; with and without Git for Windows installed.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Empty state: no home | Service account with no `USERPROFILE` and no `HOMEDRIVE`+`HOMEPATH` | Exit non-zero before writing any state | "Cannot determine the home directory; set VIBE_HOME to choose where Vibe keeps its state." |
| 2 | Error state: BatBadBut refusal | MCP `npx` server with an argument std cannot escape for `npx.cmd` | Connection fails for that server only; other servers start | "MCP server '<name>': argument '<arg>' cannot be passed safely to a .cmd launcher." |
| 3 | Error state: program not found | MCP command resolves to nothing through PATH and PATHEXT | Connection fails with the names tried | "MCP server '<name>': '<cmd>' not found (tried <cmd>.COM, <cmd>.EXE, <cmd>.BAT, <cmd>.CMD)." |
| 4 | Network degradation: no browser | No default browser registered during sign-in | Print the URL and keep waiting for the callback | "Open this URL to continue: <url>" |
| 5 | Permission change: case variant | `read_file` on `C:\PROJ\.ENV` with default sensitive patterns | Sensitive-file approval is required | Existing approval prompt naming the path |
| 6 | Boundary value: long path | Workspace path over 260 characters | Verbatim prefix kept internally; operations succeed; display shows the full path | None |
| 7 | Boundary value: reserved name or UNC | File `con.txt` or workspace on `\\server\share` | Verbatim prefix kept where required; no `\\?\` in display when removable | None |
| 8 | Undo and reversal: read-only file | Rewind or review revert of a file marked read-only (`crates/vibe-core/src/workspace/review.rs:171`) | Revert succeeds after clearing the read-only attribute, or reports the file it could not restore | "Could not restore <path>: the file is read-only." |
| 9 | Interrupted flow: cancel during PTY session | Ctrl+C or session timeout while a command spawned grandchildren | Whole process tree terminated within 2 s; partial output returned | Existing cancellation notice |
| 10 | External dependency: Git Bash absent | Windows host without Git for Windows | Shell ladder selects PowerShell; harness and shell tools state the family | None |
| 11 | External dependency: WSL launcher on PATH | `C:\Windows\System32\bash.exe` is the first `bash` on PATH | Skipped as a Git Bash candidate, as the legacy ladder already does | None |
| 12 | Boundary value: paste versus typing | A typist reaches 20 ms between keys | Treated as typing, since a burst needs three or more key events within 8 ms; Enter submits | None |
| 13 | Error state: unescapable URL scheme | MCP server returns `file:` or `javascript:` authorization URL | Refused; URL printed | "Refusing to open a non-web URL: <url>" |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | A shared-code change shifts Linux behavior and drifts a parity corpus | Med | High | Windows-specific logic behind `cfg(windows)` or the platform value; the Linux job stays required and corpora are never regenerated outside a re-pin (US-003, US-011) |
| 2 | The reference cannot run on Windows at the pin, leaving target behavior unmeasured | Med | High | US-006 runs first; fallback decisions are logged here and every divergence gets a ledger row |
| 3 | Verbatim normalization breaks long, UNC, or reserved-name paths | Med | Med | Use `dunce` semantics that keep the prefix in those cases; edge cases 6 and 7 are acceptance criteria of US-009 |
| 4 | Paste-burst detection misclassifies typing at under 20 ms per key or pastes slower than 8 ms per key | Med | Med | Measurable thresholds in US-019; Enter inside a burst inserts a newline, so a misclassification never loses text |
| 5 | Ignore tags become permanent and hide defects | Med | Med | Tags must name a story ID; the job summary prints the count; Month-6 target is zero |
| 6 | Windows CI cost and duration slow every PR | Med | Low | ≤ 90 min budget; GitHub bills Windows runner minutes at a 2x multiplier on private repositories, so the trigger policy is an open question |
| 7 | Another `cmd` spawn with untrusted data remains | Low | High | US-013 criterion: one shared launcher and no `cmd` plus `start` spawn in production; review every `Command::new("cmd")` site during US-016 |
| 8 | The AltGr rule breaks an existing Ctrl+Alt shortcut | Low | Med | Letter chords stay chords (US-018); unit tests for both cases |
| 9 | The French-locale development machine diverges from the English CI runner | Med | Low | Tests compare error kinds and codes, never localized text (US-004, US-005) |

## Non-Goals

What this version explicitly does NOT include:

- WSL-specific behavior: WSL users run the Linux build; no WSL path translation or `wsl.exe` integration.
- Windows on ARM64 builds; revisit when a user requests it and a runner is available.
- A WinPTY rung after ConPTY: kept out on purpose by the ledger row at `docs/parity.md:367`.
- OEM code page or statistical decoding of shell output: kept out on purpose by the ledger row at `docs/parity.md:441`.
- A Windows sandbox (restricted tokens, AppContainer, integrity levels) like Codex ships; a separate PRD if the reference adds one.
- MSI, winget, or Scoop packaging and Authenticode code signing; `install.ps1` stays the distribution channel for v1.
- Capturing Windows-specific parity corpora: v1 records targeted normalized observations (US-006) and replays the Linux corpora on Windows.
- Image paste from the Windows clipboard (macOS-only today, `crates/vibe-cli/src/tui/clipboard.rs:96`) and voice capture tuning on WASAPI; revisit after Release 2 if reported.
- Keyring changes beyond the target interoperability US-025 owns (US-006 measured the divergence).

## Files NOT to Modify

- `crates/vibe-core/src/parity.rs` (`REFERENCE_COMMIT`) and `scripts/parity/pin.py` (`EXPECTED_COMMIT`): change only in a re-pin.
- Committed corpora (`crates/vibe-app-server/tests/tool-surface/baseline.json`, `crates/vibe-cli/tests/runtime-parity/`, every `*/tests/**/corpus.json`): change only in a re-pin that regenerates all of them.
- `NOTICE`: the licensing boundary this PRD operates under.
- `vibe-protocol` envelope structs: `deny_unknown_fields` is load-bearing for the untagged `Envelope`.
- Version carriers (`action.yml`, `.github/workflows/action.yml`, `scripts/install.sh`, `scripts/install.ps1` version string, `crates/vibe-cli/whats_new.md` heading): change only in a version bump; US-022 edits `install.ps1` logic, not its version.
- Existing ledger rows in `docs/parity.md` at lines 367, 440, and 441: stories must keep them true.

## Technical Considerations

Frame as questions for engineering input, not mandates:

- **Architecture:** Where should consolidated platform knowledge live? Recommended: a `pub` module in `vibe-core` (for example `vibe_core::platform`) exposing the home resolver now private at `crates/vibe-core/src/config/effective.rs:783` and `crates/vibe-core/src/llm/availability.rs:354`, the verbatim simplifier, and one PATHEXT resolver replacing `crates/vibe-core/src/worktree/git.rs:118` and `crates/vibe-core/src/plugins/materialize.rs:469`. Engineering to confirm the module boundary against the layering in `Cargo.toml:88`.
- **Data Model:** No persisted schema change is planned. Should the first Windows start after US-008 detect user-level state left in `<cwd>\.vibe` by the HOME-only fallback? Recommended: no automatic migration (that directory is also the project-local config directory, so moving files risks data loss); document the manual move in `CHANGELOG.md`. Trade-off: users with existing Windows state must move it by hand.
- **API Design:** No new methods. Wire payloads keep their shape; only path values change form on Windows. Does any client compare paths byte-wise against previously persisted `\\?\` values (session files, trust records)? Engineering to check `crates/vibe-core/src/worktree/record.rs:179` and trust storage before US-010. Checked during EP-002: the managed bucket digest and the trusted folder list were already stored without the prefix, project links are re-resolved on every comparison, and a worktree record's `repo_root` is only opened, never compared; the one byte-wise comparison left is the CLI session listing's `cwd` filter, whose old entries sit in the abandoned `<cwd>\.vibe` home anyway.
- **Dependencies:** `dunce` 1.0.5 is already in `Cargo.lock` transitively; recommended as a direct workspace dependency over extending `strip_verbatim_prefix`, because it already encodes the UNC, reserved-name, and non-Unicode exceptions. URL opening: recommended `rundll32.exe url.dll,FileProtocolHandler <url>` spawned with `Command` (no new dependency; `unsafe_code = forbid` rules out direct `ShellExecuteW`); alternative: the `opener` crate, which wraps `ShellExecuteW`. Engineering to choose after testing URLs with `&` and `%`.
- **Input handling:** Paste-burst thresholds (initial proposal: three or more key events within 8 ms open a burst, which closes after 30 ms of silence) need tuning in Windows Terminal and conhost. Should the burst logic live behind the existing `accepts_key_event` filter (`crates/vibe-cli/src/tui/shortcuts.rs:240`) or in the composer?
- **PTY lifecycle:** portable-pty 0.9 kills only the child handle. Recommended: put PTY children in a Job Object (as the pipe path does through `command-group` in `crates/vibe-core/src/child.rs`) and drop the master after `wait()` so the pseudoconsole closes. Engineering to verify the close ordering in portable-pty's `win/conpty.rs` before implementation.
- **Migration:** Backward compatibility on Linux and macOS is required and measured by the corpora. Rollback plan: each story is an isolated change behind platform checks, so reverting one story's commit restores the previous behavior without touching the others.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Workspace tests passing on Windows | 93.1% (2403 of 2581; 176 failed, 2 hung) | 100% with 0 `windows:` ignore tags | Month-6 | Required windows-2022 job, test log artifact |
| Hung tests on Windows | 2 | 0 | Month-1 | windows-2022 job completes within its step timeout in 10 consecutive runs |
| Reproduced security defects on Windows | 3 (URL injection, sensitive-pattern bypass, denylist bypass) | 0 | Month-1 | US-011 and US-013 regression tests on the required job |
| Integration parity test binaries failing on Windows | 20 | 0 | Month-6 | Required windows-2022 job |
| Wall time of a full Windows test signal | 3 h 40 min locally including hangs | ≤ 90 min | Month-1 | Job duration in CI |
| `windows:` ignore tags in the tree | N/A (new) | ≤ 60 at Release 1, 0 at Release 2 | Month-1, Month-6 | Count printed in the job summary |
| Open Windows blocker root causes from the audit | 6 | 0 | Month-1 (Release 1) | Story status in `tasks/prd-windows-native-support-status.json` |

## Open Questions

- ~~Does the pinned reference run on Windows, and what does it do for each item listed in US-006?~~ Answered by US-006 on 2026-10-15: it runs (`uv sync --frozen` and `import vibe.cli.entrypoint` succeed at the pin), and the six observations are recorded in `docs/parity.md`, "Windows observations".
- Should the Windows job run on every PR or only on `main` and release branches, given the 2x minute multiplier if the repository is private? Owner: maintainer, before US-007 lands.
- ~~Should the Unified Harness shell tool on Windows stay PowerShell while `process.*` follows the Git Bash first ladder?~~ Answered by US-006: the reference uses one ladder for both.
- ~~Should users with state stranded in `<cwd>\.vibe` get a one-time notice, given that the directory doubles as project config?~~ Answered by the maintainer on 2026-10-16: no notice and no migration; `CHANGELOG.md` documents the manual move.
- ~~Does the port's Windows keyring entry interoperate with the reference's?~~ Answered by US-006: it does not (the reference stores the key under target `ai.mistral.vibe`, user `MISTRAL_API_KEY`; the port under `MISTRAL_API_KEY.ai.mistral.vibe`). US-025 owns the fix.
- US-013 (browser launcher failure) and US-021 (default editor) each need a decision between matching the reference US-006 measured and keeping this PRD's behavior with a ledger row. Owner: maintainer, before those stories start. US-013 answered on 2026-10-17: match the reference (sign-in stops with `OPEN_BROWSER_FAILED`; MCP authorization keeps the URL printed and waits).
[/PRD]
