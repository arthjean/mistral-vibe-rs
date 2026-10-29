[PRD]
# PRD: End-to-End Test Battery for Mistral Vibe RS

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-10-28 | Arthur Jean | Initial draft from the jailed side-by-side exploration of 2026-10-28 (HEAD `e48c5ba`, reference pin `376f6a33`): 31 verified port-only defects, the harness design, and the scenario catalog |
| 1.1 | 2026-10-28 | Arthur Jean | US-001 measured (details in the `vibe-e2e` crate documentation): `bwrap --unshare-net` and nested `unshare` both work on Fedora 44 (1.6 ms and 3.1 ms median start overhead) and on ubuntu-22.04 (2.5 ms after `apt-get install bubblewrap`, 4.8 ms out of the box), with loopback, a public connect failing at once, the uid preserved, a PTY, and a grandchild in the namespace; on ubuntu-24.04 both fail (`bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted`, `unshare: write failed /proc/self/uid_map: Operation not permitted`) until `kernel.apparmor_restrict_unprivileged_userns=0`. Hosts reached with only the provider redirected: `chat.mistral.ai` (`vibe_base_url`), `console.mistral.ai` (`console_base_url`), `experiments.mistral.services` (`[experiments] api_host`) for a Mistral backend, `api.github.com` (port TUI) and `pypi.org` (reference TUI) silenced by `enable_update_checks = false`; none once all are redirected; both honor the proxy variables. US-008 correction: on the Unified Harness the reference's chat request carries its SDK's default user agent (`mistral-client-python/2.6.0`), so the subject self-test compares the user agents of the whole journal, where telemetry carries each version |
| 1.2 | 2026-10-28 | Arthur Jean | EP-002 measured against the pinned reference: on the legacy harness the reference does not retry an incomplete stream (cut body, missing finish reason, SSE error event) and exits 1 after one request, so the US-011 legacy scenarios assert that; a bare `-p` never takes the positional; SIGINT during `--output streaming` exits 0 in both, with a farewell line after the NDJSON; both back off 0.5, 1, 2, 4 s within `api_retry_max_elapsed_time`; `--resume <id>` crosses harnesses in the reference while `-c` does not; with a closed stdout the port exits 1 and the reference 120 (an uncaught `BrokenPipeError`); `--check-upgrade` and `vibe update` reach `pypi.org` (reference, hardcoded in `vibe/cli/update_notifier/adapters/pypi_update_gateway.py`) and `api.github.com` (port, redirectable only through the port-only `VIBE_UPDATE_BASE_URL`), so the update criterion of US-013 is rewritten as `port_only` scenarios through `VIBE_UPDATE_BASE_URL` (now a declared seam) and its TTY prompt moves to US-026; the harness release route now serves the releases list both GitHub gateways read; new register rows E2E-D32 to E2E-D36, and E2E-D24 also covers `vibe -p` |
| 1.3 | 2026-10-28 | Arthur Jean | EP-002 review: `--output streaming` writes whole entries in both implementations, so the paced-streaming criterion of US-012 asserts that the user entry precedes the model stream and no assistant text precedes the stream's end; the port prints an interrupted answer as a completed entry (new E2E-D37); the US-011 legacy fault criterion and the US-013 closed-stdout criterion restate the measured reference behavior; US-012 covers every `include_*` setting |
| 1.4 | 2026-10-28 | Arthur Jean | EP-003 measured against the pinned reference: tool messages are compared through a SHA-256 of the normalized message and disk state, because they carry reference-authored text; `search_tool_functions` best-match scores derive from description prose, so its candidates compare as a set; a smart-approve classification is answered from a new platform utility queue (requests that offer no tools); stdio MCP servers are the e2e binary itself started with `--vibe-e2e-mcp-server`, HTTP servers and connector gateways are served by the platform; on the reference, `--auto-approve` bypasses `permission = "never"` and denylists but not `--enabled-tools` or `--disabled-tools`, `[tools.write_file] permission = "never"` does not stop a write, and a write to `.env`, outside the work tree, or through an escaping symlink asks (denied under `-p`); E2E-D07 also covers `--enabled-tools`; E2E-D11 reproduces under `--auto-approve` only, a legacy MCP tool set to `always` runs on the port; E2E-D21 reproduces under `-p` for an interrupted turn only: a `bash` call past its timeout leaves no process in either implementation, and the reference leaves a background `bash` job alive after `vibe -p` exits, so that US-016 trial keeps the no-survivor contract as `port_only`; `read_file` decodes invalid UTF-8 as Latin-1 in both implementations (`shared_with_reference`); `read_file` stops at 2,000 lines and `bash` bounds its stdout, so the US-016 large-output cases use long lines and escaped bytes; a project `hooks.toml` loads only in a trusted folder, so "not yet approved" is measured as untrusted, beside a `--trust` case; new register rows E2E-D38 to E2E-D42 |
| 1.5 | 2026-10-29 | Arthur Jean | EP-003 review: the US-019 skill matrix installs a user and a project plugin in every cell; on the Unified Harness both implementations offer a plugin skill as `<plugin>:<skill>`, the project plugin's only in a trusted folder, and on the legacy harness neither offers a plugin skill |
| 1.6 | 2026-10-29 | Arthur Jean | EP-004 measured against the pinned reference: the app-server scenarios drive `vibe-app-server-e2e` (`vibe-cli`, `test-fixtures` feature), composed by the `vibe -p` route with the summary provider `HeadlessService` attaches; every `vibe-acp` prompt is stopped on the port by E2E-D02, so a prompt scenario no narrower defect owns declares E2E-D02; the reference also writes `meta.json` without a trailing newline, so the E2E-D28 store criterion exempts it; with a pending permission, Zed's order (`session/cancel`, then the `cancelled` answer) never ends the reference's prompt (its store rejects the answer) and the other order fails it with -32603, so that US-022 trial keeps the cancellation contract as `port_only`; an MCP server passed in `session/new` and called through `run_typescript` receives the call and the reference reports `mcp_transport_failed`, which the trial asserts; a dismissed permission is the `cancelled` outcome without `session/cancel`; a malformed ACP line is answered by nothing in both and the connection survives; `initialize` reports the subject's own version; a batch array or a null id ends the app-server connection in both, a CRLF line is answered in both; `session/updated` patches neither the model nor the reasoning effort in either implementation; the method sweep commits the reference's response class per routed method; the `vibe-acp --setup` criterion moves from US-023 to US-029, which has the PTY driver of US-026 and the onboarding flow it drives, so US-023 no longer depends on US-026; new register rows E2E-D43 to E2E-D49 |
| 1.7 | 2026-10-29 | Arthur Jean | EP-005 measured: the TUI scenarios drive the shipped `vibe` on a PTY (`portable-pty`, screen model `vt100`), at most 2 live sessions per process; the reference's Textual UI is not driven, so every TUI trial is `port_only` and cites the reference module its expectation is read from; an approval or question dialog ignores a selection for 500 ms after it opens (`_INPUT_GRACE_PERIOD_S` in vibe/cli/textual_ui/widgets/approval_app.py and question_app.py), so trials answer after that grace; Esc on an approval denies; an unknown slash command goes to the model as a prompt; history records `/exit`; the session title request goes to the utility queue; E2E-D49 also reproduces through the TUI's "Always allow"; E2E-D25 also covers `/config` answering an app-server `InvalidParams` error; E2E-D26 did not reproduce under four timings and stays undeclared pending a decision; the VS Code promotion is asserted absent, the divergence the `docs/parity.md` row 33 ledger keeps; new register rows E2E-D50 to E2E-D54 |
| 1.8 | 2026-10-29 | Arthur Jean | EP-005 review: US-026, US-027, US-029, and US-030 certified; the 19 TUI and setup known-defect trials still fail on the port; after `vibe -c`, `/rewind` on the default harness offers no file to restore instead of failing with "Cannot find user entry", recorded under E2E-D22; the PTY driver self-tests run on Unix only, until US-034; E2E-D26 did not reproduce, so it leaves the US-028 criterion and the definition of done, and its trial guards the behavior undeclared; EP-005 certified |
| 1.9 | 2026-10-29 | Arthur Jean | The TUI quality gate no longer asks for final screens attached to a PR description, since stories land on `main` without one: a failing run quotes its final screen and raw bytes, and each known-defect scenario fails under `--ignored` with a screen matching its declared cause, which the EP-005 review measured |

## Problem Statement

`docs/parity.md` scores every part at 100 except row 33 (the VS Code promotion), yet the shipped binaries break in ordinary use. On 2026-10-28, nine exploration passes ran the real `vibe` and `vibe-acp` binaries and the app-server fixture side by side with the pinned Python reference, in isolated worlds behind a network namespace, against a scripted chat-completions stand-in. The main session reproduced or read in code 31 port-only defects (register under Research Findings). Several are severe: the TUI ignores a configured non-Mistral provider and sends turns to `api.mistral.ai`; `vibe-acp` ignores the provider in `config.toml`; on the default Unified Harness, `--auto-approve` bypasses `--enabled-tools` and `--disabled-tools`, `--disabled-tools write_file` still writes, and hooks never run.

1. **The oracles measure compositions the user never runs.** 18 parity drivers pass `--legacy-harness` (`scripts/parity/pin.py:35`) while the binary starts on the Unified Harness; the four oracles that measure the Unified Harness have no scenario for the defects above. App-server oracles drive `vibe-app-server-stdio-fixture`, whose composition hardcodes its prompt and empties providers and models (`crates/vibe-app-server/tests/fixtures/app_server_stdio_fixture.rs:78-97`) instead of reusing the composition `vibe` builds (`crates/vibe-cli/src/bootstrap.rs:318-338`).
2. **Test environments hide configuration routing.** Eight drivers inject the port-only `VIBE_API_BASE`, and the PTY tests pass the hidden `--api-base http://127.0.0.1:9/v1` with `MISTRAL_API_KEY=fixture` (`crates/vibe-cli/tests/tui_pty.rs:158-161`). A provider configured in `config.toml`, as a user configures it, is never what a test runs, which is why the TUI and ACP routing defects stayed invisible.
3. **No test of the shipped TUI runs a model turn.** Every PTY test points at an unreachable backend, so rendering after a turn, `!` commands, rewind, `/debug`, `/log`, and resume by short id are measured nowhere on the binary.
4. **Comparisons are too coarse to fail.** Programmatic captures keep sorted tool names and message roles only (`scripts/parity/programmatic.py:104-105`), never the content of tool messages or the state left in `VIBE_HOME`; the stand-in writes a whole SSE stream in one write (`scripts/parity/stand_in.py:85-93`), so progressive rendering, mid-stream cancellation, and truncated streams are never exercised.
5. **Nothing stops a test from reaching the real network.** Both implementations contact Mistral hosts at startup, even without a key; during the exploration, probes run before the jail became mandatory reached real endpoints with a dummy key, and the reference attempted a self-update.
6. **Defects have no executable, durable reproduction.** The 31 reproductions exist as throwaway scripts on a tmpfs scratchpad. An agent fixing one has no test that turns green, and nothing prevents a regression.

**Why now:** the v2.26.0 pass of 2026-10-14 made the Unified Harness the default backend for every launch (`docs/parity.md`, row 36), so every defect above is on the default path of every session. The maintainer reports that many features do not work as they should in real use, and fixing 31 defects without regression tests would reopen them silently.

## Overview

This PRD builds a jailed, real-binary end-to-end battery that drives the shipped `vibe` and `vibe-acp` binaries, and the app-server protocol over stdio, the way a user and an editor do. Each scenario runs in its own world: a home, a vibe home whose `config.toml` points the provider and every other host at a local scripted platform, and a Git work tree. Each world runs inside a Linux network namespace, and an egress sentinel fails any trial that reaches for a host. Scenarios assert on what the user and the platform observe: stdout, exit codes, the journal of every request the platform received, files on disk, JSON-RPC messages, and a terminal screen model.

Key decisions: (a) no port-only seam in scenarios: the provider comes from `config.toml`, never `--api-base` or `VIBE_API_BASE`, except in scenarios that test the seam itself; (b) a reference subject mode runs every stdio and JSON-RPC scenario against the pinned reference, so each expectation is the reference's measured behavior rather than a guess; (c) a scenario that catches an open defect asserts the reference behavior and is declared `known_defect("E2E-Dnn", ...)`, ignored until the fix lands and counted in the CI summary, so fixing a defect means removing one tag; (d) the harness is a new layer-1 crate, `crates/vibe-e2e`, with one `e2e` test binary per shipped package on libtest-mimic, which lets the binary enter the jail before any trial and generate named trials from tables; (e) the app-server is exercised through a new test-only binary composed by the same function `vibe` uses, leaving the parity fixture and its replays untouched; (f) this PRD fixes no product defect: fixes belong to a follow-on PRD that removes the tags.

Delivery is phased. **Release 1** (18 stories: US-001 to US-011, US-014, US-015, US-017 to US-019, US-026, US-027) delivers the harness, the reference subject, the programmatic CLI, tools and permissions, integrations, and the first TUI scenarios, and covers 15 of the 31 defects. **Release 2** (5 stories: US-020, US-021, US-024, US-028, US-031) adds ACP, the app-server, TUI interactions, and telemetry, and brings coverage to 23 defects. **Release 3** (11 stories: US-012, US-013, US-016, US-022, US-023, US-025, US-029, US-030, US-032 to US-034) covers the remaining depth, voice, and Windows, and all 31 defects.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Every verified defect has an executable reproduction | Releases 1 and 2 shipped: 23 of the 31 defects declared as `known_defect` scenarios | 31 of 31, plus every defect found later registered the same way |
| Expectations are measured, not guessed | 100% of stdio and JSON-RPC scenarios pass against the reference subject or are declared `port_only` with a reason | Unchanged after the next re-pin, rerun in the same change |
| The suite never reaches the network | 0 hosts recorded by the sentinel across all trials on every CI run | Unchanged, also on windows-2022 |
| The suite is trustworthy | 0 e2e failures in 10 consecutive CI runs on `main`, no retries | Unchanged with at least 100 scenarios |

## Target Users

### Maintainer
- **Role:** The project maintainer, who owns parity with the Python reference and gates every change on the AGENTS.md quality gates.
- **Behaviors:** Replays committed corpora, runs live probes against the pinned reference, ledgers kept divergences in `docs/parity.md`, delegates implementation to agents through `/implement-epic` and `/review-epic`.
- **Pain points:** A 100 score that does not hold in real use; defects found only by hand; no test that turns green when a fix lands.
- **Current workaround:** Manual sessions in a terminal and an editor, and one-off probe scripts that vanish with the scratchpad.
- **Success looks like:** A red e2e trial always means the shipped binary misbehaves, names the defect, and leaves the artifacts needed to debug it.

### Implementing agent
- **Role:** A Claude Code or Codex session implementing a story or fixing a registered defect.
- **Behaviors:** Reads the PRD and the failing trial, edits code, runs targeted tests, hands off at `IN_REVIEW`.
- **Pain points:** Parity replays pass while the default path is broken, so "done" cannot be proven from a real entry point; reproducing a bug means rebuilding a stand-in and a world by hand.
- **Current workaround:** Ad hoc Python probes against `target/debug` binaries, without a jail.
- **Success looks like:** One command reproduces the defect, the same command proves the fix, and the reference subject proves the expectation.

### Vibe user on a third-party provider or an editor
- **Role:** A developer running `vibe` with a generic or local provider, or `vibe-acp` from Zed or another ACP editor.
- **Behaviors:** Configures providers in `config.toml`, approves tools, resumes sessions by the short id the exit summary prints.
- **Pain points:** The TUI forces Mistral onboarding, ACP prompts hang with a configured provider, permission flags do not hold, rewind and resume fail.
- **Current workaround:** Falls back to the Python reference.
- **Success looks like:** The behaviors the reference offers work in the Rust binary and stay working.

## Research Findings

Key findings that informed this PRD:

### Competitive Context
- OpenAI Codex CLI (Rust): a scripted `wiremock` model server serves SSE responses in order, asserts exact call counts, records every request and checks that every tool output answers a tool call; a WebSocket variant holds an event back until the test releases it; `assert_cmd` drives the real `codex exec` binary; the TUI renders into a `vt100` screen with `insta` snapshots ([responses.rs](https://raw.githubusercontent.com/openai/codex/main/codex-rs/core/tests/common/responses.rs), [tui/Cargo.toml](https://raw.githubusercontent.com/openai/codex/main/codex-rs/tui/Cargo.toml)). We adopt the scripted server, strict counts, the consistency check, and the hold gate, and differ by driving the TUI through a real PTY, because the defects found here live in the binary's wiring, not in rendering.
- Gemini CLI: a separate integration suite runs against the bundled binary, in several sandbox modes, with a keep-output switch for debugging ([docs](https://geminicli.com/docs/integration-tests/)). We adopt the separate entry point and the failure artifacts.
- **Market gap:** neither peer measures its end-to-end expectations against an oracle. This suite runs its stdio and JSON-RPC scenarios against the pinned reference as a second subject, which extends the project's parity discipline to the shipped binary.

### Best Practices Applied
- Scripted responses rather than recorded ones, one isolated server per test, and failure on any missing or extra request (Codex, [wiremock](https://docs.rs/wiremock)).
- Waits on events with deadlines, and server-side gates instead of sleeps (Codex hold gate).
- No automatic retries for end-to-end trials: a retry hides the flake it should expose (Gemini's deflake scripts run with `--retry=0`).
- A custom harness on libtest-mimic 0.4.0 or at least 0.5.2 runs under cargo-nextest ([nextest](https://nexte.st/docs/design/custom-test-harnesses)); libtest-mimic does not capture output, so the harness captures child output itself ([docs.rs](https://docs.rs/libtest-mimic)).
- Unprivileged user namespaces are restricted by AppArmor on Ubuntu 24.04, not on 22.04, where this project's Linux CI runs (`.github/workflows/ci.yml:14`) ([analysis](https://www.creationline.com/tech-blog/?p=83183)).

### Verified Defect Register

Evidence: **ran** means reproduced by the main session inside the network jail against the pinned reference; **read** means confirmed in code. Each row names the story whose scenario declares it.

| ID | Defect (port behavior, against the reference) | Evidence | Story |
|----|-----------------------------------------------|----------|-------|
| E2E-D01 | The TUI ignores a configured non-Mistral provider: onboarding is forced without `MISTRAL_API_KEY` (`crates/vibe-cli/src/tui/startup/preflight.rs:103-123`), and with the key turns go to `api.mistral.ai`, because the configured `api_base` is adopted only when its backend is Mistral (`crates/vibe-cli/src/bootstrap.rs:65-72`) | read, strace | US-027 |
| E2E-D02 | `vibe-acp` takes its provider from `VIBE_API_BASE`, `VIBE_PROVIDER_STYLE`, `VIBE_CREDENTIAL_ENV` (`crates/vibe-acp/src/main.rs:78-98`): with a config-only `api_base` the prompt hangs, with a generic provider it fails | ran | US-021 |
| E2E-D03 | `vibe-acp` with no key accepts `session/new` and fails at the first prompt; the reference refuses `session/new` with `-32000`, which starts the editor's sign-in | ran | US-021 |
| E2E-D04 | A project `.vibe/config.toml` is ignored under `vibe -p --trust` (active model, default agent, tool settings) | ran | US-010 |
| E2E-D05 | Configuration type validation is lost: a string price and a numeric model name are accepted and the turn runs | ran | US-010 |
| E2E-D06 | Default harness: `--auto-approve` bypasses `--enabled-tools` and `--disabled-tools` (`crates/vibe-app-server/src/server/unified/gate.rs:116-120`) | ran | US-015 |
| E2E-D07 | Default harness: `--disabled-tools write_file` still writes the file without any approval flag | ran | US-015 |
| E2E-D08 | Default harness: `[tools.bash] permission = "never"` loses to the default allowlist (`crates/vibe-app-server/src/server/unified/gate.rs:247-267`) | ran | US-015 |
| E2E-D09 | An agent profile that sets a tool to `never` (plan, custom, `default_agent`) is not applied as policy: the port raises an approval request, and on the legacy harness the turn stops without an answer | ran | US-010 |
| E2E-D10 | Default harness: file tools ignore `--add-dir` (`crates/vibe-app-server/src/server/unified/environment.rs:219-223` sets only the working directory as a root) | ran | US-015 |
| E2E-D11 | Legacy harness: MCP tools can never be approved; `--auto-approve` does not cover them and ACP never sends `session/request_permission` (`BackendDenyApproval`, `crates/vibe-app-server/src/resources/core_backend.rs:183`), masked in tests by `crates/vibe-app-server/tests/mcp_stdio_e2e.rs:127-129` | ran | US-018, US-022 |
| E2E-D12 | Default harness: no hook runs; a `pre_tool` hook that denies `bash` prevents nothing | ran | US-017 |
| E2E-D13 | `--agent` naming an unknown agent is accepted and the turn runs; the reference exits 1 | ran | US-010 |
| E2E-D14 | Default harness: an incomplete SSE stream (cut, no finish reason, error event) is not retried and the run exits 1; the reference retries and recovers | ran | US-011 |
| E2E-D15 | Default harness: compaction ignores `[compaction_model]`, sends an unstreamed request, and does not retry a summary that carries tool calls | ran | US-012 |
| E2E-D16 | Default harness: an agent's `instructions` and `system_prompt_id` are missing from the system prompt, for the main agent and for a custom subagent | ran | US-010, US-012 |
| E2E-D17 | `subagent.spawn` always publishes a null agent name (`crates/vibe-core/src/events/detail.rs:477-478`); legacy turn requests omit `tool_choice: "auto"` (`crates/vibe-app-server/src/client/live.rs:400`) | read | US-012 |
| E2E-D18 | Default harness: project skills (`.vibe/skills`, `.agents/skills`) are never loaded, whatever the trust | ran | US-019 |
| E2E-D19 | A code-mode deadline fails the whole turn (exit 1) instead of returning the error to the model | ran | US-016 |
| E2E-D20 | Tool output above 40,000 characters is sent whole instead of being saved under `tool-results/` with an envelope | ran | US-016 |
| E2E-D21 | Background and canceled `bash` commands leave grandchildren alive (`crates/vibe-harness/src/tools/shell.rs:101-116`), in `vibe -p` and through ACP `session/cancel` | ran | US-016, US-022 |
| E2E-D22 | Default harness: `/rewind` fails with "Cannot find user entry" and restores no file; after `vibe -c` it offers no file to restore | ran | US-028 |
| E2E-D23 | The output of a `!` command is invisible after the first model turn until the next turn resyncs the transcript | ran | US-028 |
| E2E-D24 | `vibe --resume <short id>`, the form the exit summary prints, fails with "Session not found", in the TUI and under `vibe -p`; a bare `--resume` does not offer Unified Harness sessions | ran | US-027, US-010 |
| E2E-D25 | `/debug` fails (`diagnostics/logs/read` sent without `sessionId`, `crates/vibe-cli/src/tui/workflow.rs:376-379`), `/log` reports an unpersisted log, `/config` is read-only | ran, read | US-029 |
| E2E-D26 | `ask_user_question` right after typing loses the arrow key and sends the first option | ran (exploration); not reproduced through the PTY by US-028 under four timings, so no trial declares it | US-028 |
| E2E-D27 | `session/resume` of another session on a connection with a root (both harnesses), or an attached `session/fork` (default harness), succeeds, then every turn fails `not_found` while the old root keeps serving | ran | US-024 |
| E2E-D28 | The Unified Harness store writes JSON without a trailing newline (`crates/vibe-harness/src/store.rs:44-46`); the reference's reader rejects such a session and its `session/list` silently drops it | ran | US-024 |
| E2E-D29 | Default harness telemetry: no `vibe.request_sent` (only the legacy engine emits it, `crates/vibe-core/src/engine/turn.rs:315`), no `user_plan` metadata, missing `tool_call_finished`; 3 of 36 programmatic and ACP scenarios and 0 of 5 TUI scenarios conform | ran (request_sent), agent-measured (rest) | US-031 |
| E2E-D30 | `vibe.log` stays empty at INFO and DEBUG, and is created with modes 644 and 755 instead of 600 and 700 | ran | US-032 |
| E2E-D31 | `vibe-acp` sends no `_session/retrying` notification while it retries | read | US-022 |
| E2E-D32 | `--worktree` without a name names the worktree from the prompt locally; the reference asks the model for the name through one extra request | ran | US-013 |
| E2E-D33 | An argument error starts with a one-line `usage: vibe [OPTIONS] [PROMPT]` synopsis; the reference's usage line lists the options (`[-h] [-v] [-p [TEXT]] ...`); the error line itself matches | ran | US-013 |
| E2E-D34 | The vibe home after one `vibe -p` run diverges: the port adds `.config.lock` and `builtin-plugins/`, its plugin store has no `blobs/` or `manifests/`, and `logs/` and plugin package shards are created with wider modes | ran | US-013 |
| E2E-D35 | `vibe -p` started with SIGINT ignored (`trap '' INT; exec vibe ...`) still stops on SIGINT; the reference keeps the inherited disposition and finishes the turn | ran | US-013 |
| E2E-D36 | `vibe -p --legacy-harness --resume <id>` cannot find a session the Unified Harness saved; the reference resumes it (the other direction works in both) | ran | US-013 |
| E2E-D37 | SIGINT during `vibe -p --output streaming` prints the partial assistant answer as an entry whose `generationStatus` is `completed`; the reference prints no assistant entry for the interrupted stream | ran | US-012 |
| E2E-D38 | Default harness: `run_typescript` without `code` answers with a schema violation; the reference refuses the missing code string with its own `InvalidToolCall` message | ran | US-014 |
| E2E-D39 | Default harness: a `[tools.write_file] denylist` pattern does not refuse a matching write without an approval flag; the reference refuses it by policy | ran | US-015 |
| E2E-D40 | Default harness: `write_file` to `.env`, to a path outside the work tree, or through a symlink leaving it is approved by config; the reference asks (denied under `-p`), and the port writes `.env` | ran | US-015 |
| E2E-D41 | Default harness: the declaration of an MCP tool types its return as `unknown`, dropping the server's `outputSchema` | ran | US-018 |
| E2E-D42 | Default harness: an MCP call past `tool_timeout_sec` fails as `mcp_transport_failed`; the reference reports `mcp_timeout` | ran | US-018 |
| E2E-D43 | An app server killed with SIGKILL during a turn loses the turn: a fresh server resumes the session with an empty history and an idle status; the reference keeps the user message and marks the turn and the session failed | ran | US-024 |
| E2E-D44 | `vibe-acp` `_session/delete` of a live session with no turn yet fails with -32603 (`Session not found`); the reference answers `{}` | ran | US-023 |
| E2E-D45 | `vibe-acp` `session/new` with a relative `cwd` fails with -31002; the reference opens the session | ran | US-023 |
| E2E-D46 | The app server ends the connection on a frame above 8 MiB (a 9 MB request, a 12 MB image attachment), a message without `jsonrpc`, or a leading UTF-8 byte-order mark; the reference answers each and keeps the connection | ran | US-025 |
| E2E-D47 | 11 routed methods answer another response class than the reference: with a root, `agents/list`, `diagnostics/list`, and `tools/list` answer a result, and `feedback/record`, `review/approve`, `review/baseline`, `review/hunks`, `review/revert`, `review/turnDiff`, and `session/relocate` answer `invalid_params`, where the reference answers `method_not_found`; without a root, `session/history/clear` answers `conflict` where the reference answers `internal_error` | ran | US-025 |
| E2E-D48 | `session/compact` on a session with no turn sends summary requests again and again and never answers; the reference answers `internal_error` | ran | US-025 |
| E2E-D49 | An approval answered `approve_permanently` for `touch first.txt` persists the bash allowlist rule `touch *`; the reference persists `touch` | ran | US-025 |
| E2E-D50 | Onboarding (`vibe` and `vibe-acp --setup`) turns bracketed paste on and drops the paste event (`crates/vibe-cli/src/tui/onboarding/driver.rs`), so a pasted API key never reaches the field; the reference saves it to `.env` (vibe/setup/auth/api_key_persistence.py) | ran (port), read (reference) | US-029 |
| E2E-D51 | The generated session title never reaches the window title, which stays `Vibe`; on the default harness `meta.json` keeps `"title": null` and the title is only journaled; the reference sets the terminal title from it (`_on_session_title_changed` in vibe/cli/textual_ui/app.py) | ran (port), read (reference) | US-030 |
| E2E-D52 | Keys typed within about 200 ms of a resize are painted only when another event arrives | ran (port) | US-030 |
| E2E-D53 | While a failing request is retried the TUI shows only its generic loading status; the reference shows `Retrying` (`RETRYING_LOADING_STATUS`, vibe/cli/textual_ui/widgets/loading.py) | ran (port), read (reference) | US-030 |
| E2E-D54 | An idle TUI writes about 24 KB in 10 s (a style reset and cursor move per frame, plus the banner animation), above the 4 KiB budget | ran (port) | US-030 |

Reported by exploration agents with saved scripts, not reproduced by the main session; the owning story measures each against the reference subject and registers it as a new `E2E-Dnn` row when the port diverges: SIGKILL during a turn loses the user message, an 8 MiB frame and a missing `jsonrpc` field or a BOM end the app-server connection, missing `session/updated` notifications (US-024, US-025); the session title is not persisted in `meta.json` and no terminal title is set (US-030); ALSA error lines are written over the TUI when no PCM device exists (US-033); the model picker preselects the first entry, and "always allow" persists a glob of the whole command (US-028, US-029); MCP `outputSchema` is lost and timeouts report `mcp_transport_failed` (US-018); valid UTF-8 is decoded as Latin-1 (US-016); OpenTelemetry spans carry the model alias and miss temperature, response id, and status code (US-032); unnamed worktrees are named locally without a utility request, and cross-harness resume fails (US-013); a Zed-style cancel leaves the tool card `in_progress`, and a relative `cwd` is refused (US-022, US-023); extra `vibe.new_session` events on resume and a `nb_skills` mismatch (US-031); the VS Code promotion (scorecard row 33) is not wired (US-030).

Shared with the reference, so parity oracles count them as conformant, and this PRD asserts the shared behavior with a `shared_with_reference` note: `--max-tokens` and `--max-price` are not enforced on the Unified Harness; `--enabled-tools` does not narrow the functions offered on the Unified Harness; text output prints the Python representation of reasoning; `-c` resumes a session from another repository; an open stdin pipe blocks `-p`; a refused connection stays silent for about 305 s; the legacy `-p` mode exits silently on an approval request; an unknown model alias falls back to Mistral silently.

*Full exploration evidence: the main session's ledger (`LEDGER.md`), the French plan (`PLAN-E2E.md`), and the rerunnable per-domain probe scripts, kept locally under `.parity/e2e-exploration/` (gitignored, because they carry reference-authored text; outputs and worlds were not kept).*

## Assumptions & Constraints

### Assumptions (to validate)
- A rootless network namespace (`bwrap --unshare-net`, or an outer `unshare -rn` with an inner user namespace mapping back to the invoking uid) runs the binaries on the ubuntu-22.04 runner with the uid preserved, loopback up, and PTYs available. Based on the exploration's working jail on Fedora 44 and the AppArmor restriction applying from Ubuntu 23.10. Validated by US-001.
- Both implementations send every outbound HTTP request through `HTTPS_PROXY` and honor `NO_PROXY` for loopback, so a CONNECT sentinel attributes egress to a host. Based on reqwest's default environment proxies (the one opt-out is `crates/vibe-core/src/llm/vertex.rs:368`) and httpx's `trust_env` default. Validated by US-001.
- The set of hosts each binary contacts at startup is finite and redirectable through `config.toml` keys. Based on the keys found (`crates/vibe-core/src/auth/provider.rs:23-30`, `crates/vibe-app-server/src/vibe_code.rs:38`, `crates/vibe-core/src/experiments.rs:244`); sign-in (`crates/vibe-core/src/auth/sign_in_http.rs:35-36`) and Vertex tokens (`crates/vibe-core/src/llm/vertex.rs:27`) are hardcoded and stay out of scope. Validated by US-001.
- The reference's real `vibe-app-server` and `vibe-acp` read the same `config.toml` and can serve as the reference subject for every JSON-RPC scenario. Validated by US-008.
- vt100 0.18 exposes the window title only through its callbacks, since `Screen` documents no title accessor (docs.rs, vt100 0.18.1). Validated by US-026.
- libtest-mimic 0.8.2 lists and runs trials under the pinned nextest 0.9.146 (`.config/nextest.toml`). Validated by US-007.
- The e2e binaries add at most 6 minutes to the Linux `quality` job at Release 3 size (no measurement yet; one `-p` scenario took about 1 s during the exploration). Validated by US-007 and remeasured per release.

### Hard Constraints
- Licensing boundary (`NOTICE`, `AGENTS.md`): no reference source, prompt text, tool description, or reference-authored message enters a scenario, fixture, or this repository; expectations are written as observations in original words; reference behavior is cited by path.
- `unsafe_code = "forbid"` workspace-wide (`Cargo.toml:79`): the jail is entered through an external wrapper process, never `pre_exec` or FFI.
- Dependency layering (`Cargo.toml:90-96`): `vibe-e2e` sits in layer 1 and depends on no workspace crate; checked by hand.
- No committed corpus, pin source, or ledger row changes outside a re-pin; the parity fixture `vibe-app-server-stdio-fixture` and every parity driver stay unchanged.
- Worlds live under the system temporary directory, outside `$HOME` and outside any Git work tree (tempdirs under the home or under `target/` change config target selection and repository detection), and stay small because `/tmp` is a shared tmpfs.
- Scenarios never pass a port-only seam (`--api-base`, `--credential-environment`, `--fake-response`, `--provider-style`, `--model`, `--session-root`, `VIBE_API_BASE`, `VIBE_PROVIDER_STYLE`, `VIBE_CREDENTIAL_ENV`, `VIBE_MODEL`, `VIBE_TEST_*`) unless the scenario tests that seam.
- No test reaches a real Mistral or third-party endpoint, and no real credential or keyring is read.

## Quality Gates

These commands must pass for every user story:
- `cargo fmt --all -- --check` - formatting
- `cargo check --workspace --all-targets --all-features` - compilation of every target and feature
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` - lints with warnings denied
- `env -u FORCE_COLOR VIBE_HOME="$(mktemp -d)" cargo test --workspace --all-features --no-fail-fast` - the full workspace suite, which includes the e2e binaries once US-007 lands

For stories that add stdio or JSON-RPC scenarios (from US-009 on), additional gate:
- `VIBE_E2E_SUBJECT=reference cargo test -p vibe-cli -p vibe-acp --all-features --test e2e` - every new scenario passes against the pinned reference or is declared `port_only` with a reason; run locally with the reference checkout at the pin, since CI has no checkout

For TUI stories (US-026 to US-030), additional gate:
- A failing run of each new scenario quotes its final screen and raw bytes (FR-09), and each known-defect scenario fails under `--ignored` with a screen matching its declared cause.

## Epics & User Stories

### EP-001: Hermetic E2E Harness

Build the harness every scenario runs on: worlds, the network jail, the scripted platform, process supervision, the test binaries, CI accounting, and the reference subject, so later stories only write scenarios.

**Definition of Done:** `cargo test --workspace --all-features` runs both e2e binaries jailed on ubuntu-22.04 and Fedora 44; the harness self-tests pass; the CI summary reports the known-defect count; `VIBE_E2E_SUBJECT=reference` runs a scenario against the pinned reference.

#### US-001: Validate assumption: a rootless network jail and an egress sentinel work on the CI runner and the workstation
**Description:** As a maintainer, I want the jail and sentinel mechanics measured on ubuntu-22.04 and Fedora 44 so that the harness rests on measured behavior rather than on the exploration's ad hoc scripts.

**Priority:** P0
**Size:** S (2 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given a throwaway `debug/**` branch workflow on ubuntu-22.04 and the Fedora 44 workstation, when `bwrap --unshare-net` and nested `unshare` each wrap a probe, then the record states for each wrapper whether a loopback bind succeeds, a connect to a public address fails within 1 s, `id -u` inside equals outside, a PTY opens, and a grandchild shares the namespace.
- [ ] Given each working wrapper, when it starts a probe 20 times, then the median start overhead in milliseconds is recorded.
- [ ] Given an ubuntu-24.04 runner, when the same probes run, then the record states the failure mode and whether `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` restores them.
- [ ] Given a world that redirects only the provider, a CONNECT sentinel exported as `HTTPS_PROXY`, `HTTP_PROXY`, and `ALL_PROXY`, and `NO_PROXY=127.0.0.1,localhost`, all inside the jail, when `vibe -p`, the TUI started then exited, and `vibe-acp` through `initialize` and `session/new` run for the port and the pinned reference, then every host each contacts is listed with the configuration key that redirects it, or marked unredirectable.
- [ ] Given a provider `api_base` on a non-loopback host, when each binary runs inside the jail, then the record states whether the sentinel saw the host (proxy honored) or the connection failed unseen.
- [ ] Given neither wrapper is usable, when the probe runs, then the exact error each wrapper prints is recorded for the harness failure message.
- [ ] The findings are added to this PRD's Changelog and to the `vibe-e2e` crate documentation, and the throwaway branch is deleted with the maintainer's approval.

#### US-002: Create the vibe-e2e crate and the hermetic world builder
**Description:** As an implementing agent, I want one call that builds an isolated world with a config pointing every host at loopback so that a scenario starts from the same clean state a new user has.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given the workspace manifest, when `crates/vibe-e2e` is added, then it is a member with `publish = false`, `[lints] workspace = true`, the crate-root `#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]`, a layer-1 entry in `[workspace.metadata.vibe] dependency-layers`, and no workspace crate among its dependencies.
- [ ] Given `World::new()`, when it returns, then the world root lies under `std::env::temp_dir()`, outside `$HOME` and outside any Git work tree, and holds `home/`, `vibe-home/`, `tmp/`, and `work/`, a Git repository committed with a world-local Git configuration.
- [ ] Given a world, when it writes `vibe-home/config.toml`, then the provider, the model, and every host key from the US-001 redirect table point at loopback, and update checks are disabled.
- [ ] Given a child command built by a world, when its environment is inspected, then it was built from empty and holds only `PATH`, `HOME`, `VIBE_HOME`, `TMPDIR` set to the world's `tmp/`, `LANG`, `TERM`, `DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent`, the proxy variables, the world marker `VIBE_E2E_WORLD`, and the variables the scenario adds.
- [ ] Given the test process exports `MISTRAL_API_KEY` and `FORCE_COLOR` and a `~/.vibe/config.toml` exists, when a world child runs, then none of them reaches it (harness self-test).
- [ ] Given a scenario source under `crates/*/tests/e2e/`, when the harness self-test scans it, then a use of any port-only seam named under Hard Constraints fails the self-test unless the scenario declares the seam it tests.
- [ ] Given a seed path that resolves outside the world root, when a scenario writes it, then the write is refused with an error naming the path.
- [ ] Given a world holding read-only directories, when it drops, then it restores owner write access, removes the whole tree, and leaves no `vibe-scratchpad-*` directory in the system temporary directory.

#### US-003: Jail every e2e process and detect egress
**Description:** As a maintainer, I want every e2e trial to run inside a network namespace with an egress sentinel so that no test can reach a real endpoint and any attempt names the host.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-001, US-002

**Acceptance Criteria:**
- [ ] Given Linux, when an e2e binary starts outside the jail, then it re-executes itself under the wrapper US-001 selected, with its arguments unchanged, before any trial runs.
- [ ] Given `cargo test` and `cargo nextest run` (one trial per process), when each runs the e2e binaries, then every trial runs inside the jail.
- [ ] Given no usable wrapper and `VIBE_E2E_UNJAILED` unset, when an e2e binary starts, then it exits non-zero within 2 s with a message naming the missing wrapper and the opt-out, and runs no trial.
- [ ] Given `VIBE_E2E_UNJAILED=1`, when an e2e binary starts, then it prints one warning line and runs its trials with the sentinel only.
- [ ] Given a world, when it starts, then a loopback sentinel answers every proxy `CONNECT` and absolute-URI request with 403 and records the target host and port.
- [ ] Given a scenario whose provider `api_base` names a non-loopback host, when the trial ends, then it fails with a message naming that host as egress, even if its own assertions passed.
- [ ] Given a jailed trial, when the test process connects to a public address, then the connect fails within 1 s (harness self-test).
- [ ] Given Windows or macOS, when an e2e binary starts, then it runs with the sentinel only and says so in one line.

#### US-004: Script the model platform's chat completions
**Description:** As an implementing agent, I want a scripted chat-completions platform with paced streaming, gates, routing, and a full request journal so that a scenario controls every model answer and asserts on every request.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**
- [ ] Given a script of turn responses, when a binary posts `/v1/chat/completions` with `stream: true`, then each response is served in order as SSE, each event flushed in its own write, spaced by the scripted pace (default 0 ms, settable per response).
- [ ] Given the script vocabulary, when a scenario declares it, then text, tool calls, generic `reasoning_content`, Mistral thinking chunks, usage counts, finish reasons, an HTTP status with body and headers including `Retry-After`, raw SSE lines, a stream cut before its last event, a connection dropped mid-stream, and a malformed line are each served (one self-test per item).
- [ ] Given a request with `max_tokens` equal to 1 and no tools, when it arrives, then it is answered from the probe queue and never consumes a turn response.
- [ ] Given a scenario `when` marker (a substring of the serialized messages), when a request contains it, then it is answered from that marker's queue before the turn queue, which keeps compaction and utility requests from shifting turn answers.
- [ ] Given a gate on the Nth response, when that request arrives, then the platform holds the response until the scenario releases it, and `await_request(n, deadline)` returns once the Nth request is journaled.
- [ ] Given any request, when it arrives, then the journal records its path, headers, parsed body, queue, and arrival time, in arrival order.
- [ ] Given a request carrying a tool message whose `tool_call_id` no earlier assistant message declared, when the platform checks it, then the trial fails naming both ids.
- [ ] Given the turn queue is exhausted, when another turn request arrives, then the platform answers 500 with a body saying so and the trial fails naming the unexpected request; given `exact` counts declared, a missing request also fails the trial.
- [ ] Given 5 chunks scripted at a 100 ms pace, when a loopback reader receives them, then consecutive arrivals are at least 90 ms apart (harness self-test).

#### US-005: Answer every service route the binaries contact
**Description:** As a maintainer, I want the platform to serve every non-model route the binaries reach so that startup, telemetry, updates, and connectors behave as offline-but-healthy and never reach the network.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-004

**Acceptance Criteria:**
- [ ] Given the US-001 redirect table, when a world starts, then the platform serves on loopback the managed configuration, account identity, telemetry datalake, experiments evaluation, release metadata, connector bootstrap and gateway routes, and static pages for `web_fetch`.
- [ ] Given no scenario override, when a route is hit, then it answers a neutral default (no managed configuration, no newer release, an empty connector catalog, no experiment) that leaves the reference subject's startup output unchanged from a run where the route is absent.
- [ ] Given a scenario override, when the route is hit, then it answers the scripted status, body, or delay.
- [ ] Given a request to a path no route serves, when it arrives, then the platform answers 404 and the trial fails naming the method and path.
- [ ] Given any route, when it is hit, then the request lands in the shared journal tagged with the route name.
- [ ] Given telemetry enabled and a Mistral-backend provider on a loopback `http` base, when a binary emits an event, then the datalake route records it.

#### US-006: Supervise processes, bound every wait, and keep failure artifacts
**Description:** As an implementing agent, I want every child bounded by deadlines, killed with its descendants, and its evidence kept on failure so that a red trial is diagnosable without rerunning it.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-002

**Acceptance Criteria:**
- [ ] Given a stdio child, when a world spawns it, then its stdin is the null device unless the scenario pipes it, and on Unix it leads its own process group.
- [ ] Given a wait with a deadline (default 30 s, multiplied by `VIBE_E2E_TIME_SCALE` when set), when it expires, then the supervisor kills the child's process group and fails the trial with the last 4 KiB of stdout and stderr.
- [ ] Given a child that ignores SIGTERM, when teardown runs, then it is killed with SIGKILL after 2 s.
- [ ] Given teardown on Linux, when a process whose environment carries the world's `VIBE_E2E_WORLD` value is still alive, then the trial fails naming its pid and command line, and the process is killed.
- [ ] Given a failed trial, when it ends, then `$CARGO_TARGET_TMPDIR/e2e-artifacts/<trial>/` holds the journal, the sentinel log, stdout, stderr, `vibe-home/logs/`, a sized listing of the world tree, `config.toml`, and the child environment with values of variables named with `KEY`, `TOKEN`, or `SECRET` replaced, within 20 MB.
- [ ] Given a passing trial, when it ends, then no artifact directory is written and the world is removed.
- [ ] Given Windows, when teardown runs, then the child's process tree is ended with `taskkill /T /F`.

#### US-007: Ship the e2e test binaries, their CI wiring, and known-defect accounting
**Description:** As a maintainer, I want the e2e binaries in the workspace suite with known defects counted in CI so that every open defect is visible on every run and a fix shows up as a trial that passes.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-003, US-004, US-006

**Acceptance Criteria:**
- [ ] Given `crates/vibe-cli/tests/e2e/main.rs` and `crates/vibe-acp/tests/e2e/main.rs` declared with `harness = false` on libtest-mimic 0.8, when `cargo test --workspace --all-features` and `cargo nextest run` run, then both binaries list and run their trials, and `--exact <name>` runs one.
- [ ] Given a scenario declared `known_defect("E2E-Dnn", "<cause>")`, when the suite runs, then the trial reports as ignored and the binary prints the id and cause.
- [ ] Given the CI `quality` job, when it ends, then its summary counts `known_defect("E2E-D` declarations per binary, as `windows:` tags are counted (`.github/workflows/ci.yml:143-152`).
- [ ] Given the known-defect trials, when a non-blocking CI step runs both e2e binaries with `--ignored`, then the summary lists every known-defect trial that now passes.
- [ ] Given the `quality` job on ubuntu-22.04, when it runs the e2e binaries, then they run jailed (the job installs what US-001 requires and never sets `VIBE_E2E_UNJAILED`) and their wall time appears in the summary.
- [ ] Given two scenarios registering the same trial name, when a binary starts, then it exits non-zero naming the duplicate before any trial runs.
- [ ] Given `.config/nextest.toml`, when an e2e trial fails, then nextest does not retry it in any profile (`retries = 0` for `binary(e2e)`).
- [ ] Given the `windows-quality` job, when it runs the e2e binaries before US-034, then they run zero trials and print that Windows support lands in US-034.

#### US-008: Run any stdio or JSON-RPC scenario against the pinned reference
**Description:** As an implementing agent, I want to run a scenario against the pinned reference with one variable so that every expectation I write is the reference's measured behavior.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-007

**Acceptance Criteria:**
- [ ] Given `VIBE_E2E_SUBJECT=reference` and `vibe_core::parity::off_pin_reason` returning `None`, when an e2e binary runs, then each stdio and JSON-RPC scenario spawns the reference's `.venv/bin/vibe`, `vibe-acp`, or `vibe-app-server` in the same world and jail, with the same assertions.
- [ ] Given a scenario declared `port_only("<reason>")`, when the subject is the reference, then it is skipped with the reason printed.
- [ ] Given a `known_defect` scenario, when the subject is the reference, then it runs rather than being ignored.
- [ ] Given the reference missing or off the pin, when `VIBE_E2E_SUBJECT=reference` is set, then the binary exits non-zero with the reason and runs no trial.
- [ ] Given the default subject, when the suite runs, then no reference process is spawned and no reference checkout is needed.
- [ ] Given a self-test scenario that runs one turn under each subject, when both runs end, then the journaled user agents differ (the port reports the workspace version, the reference its package version), proving the switch changes the binary spawned.

---

### EP-002: Programmatic CLI and Engine

Cover `vibe -p` end to end: output formats, sessions, agents, configuration layering, stream faults, retries, and the engine features a turn depends on.

**Definition of Done:** every scenario of the epic runs in the workspace suite, passes against the reference subject or is declared `port_only`, and E2E-D04, D05, D09, D13, D14, D15, D16, D17 are declared as known defects.

#### US-009: Golden programmatic turns and the tool loop
**Description:** As a Vibe user on a third-party provider, I want `vibe -p` proven to answer through the provider in `config.toml` so that the basic path is guarded on both harnesses.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-005, US-007, US-008

**Acceptance Criteria:**
- [ ] Given a generic-backend provider with its own key variable and no `MISTRAL_API_KEY`, when `vibe -p "hello"` runs with `--output text`, `json`, and `streaming`, then each exits 0, prints the scripted answer in that format, and the journal holds exactly one turn request naming the configured model.
- [ ] Given a Mistral-backend provider on loopback, when the same prompt runs, then it exits 0 and the request carries the key from the provider's configured variable.
- [ ] Given a prompt on stdin, an empty stdin, `-p` with a positional prompt, and a 300 KB prompt, when each runs, then the exit code and the prompt reaching the platform match the reference subject.
- [ ] Given a loop scripted as tool call, tool call, text, when it runs on the default harness and with `--legacy-harness`, then the journal holds three turn requests and each later request carries every earlier assistant message and tool result.
- [ ] Given the provider answers 401, when `vibe -p` runs, then it exits non-zero within 10 s with an error naming authentication, as the reference subject does.
- [ ] Every scenario of this story passes against the reference subject.

#### US-010: Session continuation, agent selection, and configuration layering
**Description:** As a Vibe user, I want sessions, agents, and project configuration to apply in `vibe -p` exactly as in the reference so that scripts and CI jobs get the behavior they configure.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-009

**Acceptance Criteria:**
- [ ] Given a session created by `vibe -p`, when `vibe -p -c`, `--resume <full id>`, and `--resume <first 8 characters>` run, then the next turn request carries the earlier history; given an unknown id, then the exit code and message match the reference subject.
- [ ] Given `--agent nosuch`, when `vibe -p` runs, then it exits 1 naming the agent and the journal holds no request; declared `known_defect("E2E-D13")`.
- [ ] Given a project `.vibe/config.toml` that sets the active model, the default agent, and a `[tools.bash]` permission, when `vibe -p --trust` runs and when the folder is trusted through `trusted_folders.toml`, then each setting applies to the turn and `--trust` leaves `trusted_folders.toml` unwritten; declared `known_defect("E2E-D04")`.
- [ ] Given `active_model = 42`, a model with a string `input_price`, or malformed TOML, when `vibe -p` runs, then it exits non-zero before any request and names the invalid key; the typed cases declared `known_defect("E2E-D05")`.
- [ ] Given `--agent plan`, a custom agent whose profile sets `write_file` to `never`, and `default_agent` naming that agent, when the model calls `write_file`, then the call is refused by policy without an approval request, the turn reaches a final answer, and the file does not exist; declared `known_defect("E2E-D09")`.
- [ ] Given a custom agent whose `instructions` carry a marker and whose `system_prompt_id` selects a prompt file carrying another marker, when it runs on the default harness, then the system message of the turn request carries both markers; declared `known_defect("E2E-D16")`.
- [ ] Every scenario of this story passes against the reference subject.

#### US-011: Stream faults, HTTP errors, and retries
**Description:** As a Vibe user on an unreliable network, I want transient provider failures retried and permanent ones reported exactly as in the reference so that a dropped stream does not end a run.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-009

**Acceptance Criteria:**
- [ ] Given a stream cut before its last event, a stream without a finish reason, an SSE error event, and `[DONE]` without a finish reason, each served first and a valid answer second, when `vibe -p` runs on the default harness, then it exits 0 with the second answer and the journal holds two identical turn requests, declared `known_defect("E2E-D14")`; with `--legacy-harness`, then it exits 1 after one request, as the reference subject does.
- [ ] Given statuses 408, 409, 425, 429, 500, 502, 503, and 504, each followed by a valid answer, when `vibe -p` runs, then the request count for each equals the reference subject's.
- [ ] Given statuses 400, 401, 404, and 422, when `vibe -p` runs, then each exits non-zero after the request count the reference subject makes.
- [ ] Given 429 with `Retry-After: 1`, when the retry happens, then the second request arrives at least 1 s after the first.
- [ ] Given 503 on every attempt, when retries are exhausted, then `vibe -p` exits non-zero and the request count equals the reference subject's.
- [ ] Given a malformed SSE line and a `length` finish reason, when each is served, then exit code and output match the reference subject.
- [ ] Given no key anywhere, and a key present only in `VIBE_HOME/.env`, when `vibe -p` runs, then exit codes and the requests made match the reference subject.
- [ ] Every scenario of this story passes against the reference subject.

#### US-012: Engine context: reasoning, subagents, compaction, project context, wire metadata, and paced streaming
**Description:** As a maintainer, I want the features a long session depends on measured at the binary boundary so that the Unified Harness keeps parity beyond a single turn.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-009

**Acceptance Criteria:**
- [ ] Given Mistral thinking chunks and generic `reasoning_content`, when the next turn request is sent, then the reasoning it replays matches the reference subject's request on both harnesses, and the text output of reasoning matches the reference subject with a `shared_with_reference` note.
- [ ] Given a custom subagent whose prompt carries a marker, when the model delegates to it on the default harness, then the child's request carries the marker and the published `subagent.spawn` event names the agent; declared `known_defect("E2E-D16")` and `known_defect("E2E-D17")` respectively.
- [ ] Given a history that crosses the auto-compaction threshold on the default harness, when compaction runs, then the compaction request is streamed, names the `[compaction_model]` model and temperature, and the next turn request carries the summary; given a summary answered with tool calls, then one retry follows before the error; declared `known_defect("E2E-D15")`.
- [ ] Given user, project, and nested `AGENTS.md` files carrying distinct markers and each `include_*` setting, when a turn runs trusted and untrusted, then the markers present in the system message match the reference subject's (asserted by marker, never by length, because the prompt names the weekday).
- [ ] Given the legacy harness with tools offered, when a turn request is sent, then it carries `tool_choice: "auto"`; declared `known_defect("E2E-D17")`.
- [ ] Given a Mistral-backend provider, when a turn request is sent, then its metadata, its affinity header, and the version in its user agent match the reference subject's shape and `vibe --version`.
- [ ] Given 10 chunks paced at 200 ms with `--output streaming`, when the run streams, then the user entry reaches stdout before the platform serves the last chunk and no assistant text reaches it before the stream ends (whole entries, `shared_with_reference`), and SIGINT after the third chunk ends the process within 5 s with the reference subject's exit code and prints no partial answer; the last clause declared `known_defect("E2E-D37")`.
- [ ] Every scenario of this story passes against the reference subject or is declared `port_only` with a reason.

#### US-013: CLI surface: worktrees, update checks, signals, argument errors, budgets, cross-harness resume, and home inventory
**Description:** As a maintainer, I want every remaining `vibe` flag and subcommand outside the TUI measured end to end so that no CLI surface is covered by unit tests alone.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-009

**Acceptance Criteria:**
- [ ] Given `--worktree <name>` twice, `--worktree` without a name, and `--worktree` outside a Git repository, when each runs, then the worktree created or reused, the utility request made for an unnamed one, the branch name, and the error outside a repository match the reference subject.
- [ ] Given the release route answering up to date, then a newer release, and the port's release index redirected through `VIBE_UPDATE_BASE_URL`, when `--check-upgrade` and `vibe update` run without a TTY, then the index is asked once, the update cache records the latest version, no check failure is reported, and the up-to-date run exits 0 naming the installed version; declared `port_only` because the reference's index (`pypi.org`, `vibe/cli/update_notifier/adapters/pypi_update_gateway.py`) is hardcoded and a jailed check reaches the sentinel. The TTY prompt moves to US-026.
- [ ] Given a running `vibe -p --output streaming`, when SIGINT arrives with the default disposition and with an inherited `SIG_IGN`, then exit codes match the reference subject and the output stays valid NDJSON up to the interruption, the `SIG_IGN` case declared `known_defect("E2E-D35")`; when stdout closes early, then the run exits non-zero, the port with 1 where the reference dies in an uncaught `BrokenPipeError` with 120, which is not encoded.
- [ ] Given a table of argument errors (unknown flag, missing value, conflicting flags), when each runs, then the exit code and the first line of stderr match the reference subject.
- [ ] Given `--max-turns 2` on both harnesses, when the model keeps calling tools, then the run stops after 2 turns; given `--max-tokens` and `--max-price` on the default harness, then the behavior matches the reference subject with a `shared_with_reference` note.
- [ ] Given a session created on one harness, when `-c` and `--resume` run on the other, then the outcome matches the reference subject.
- [ ] Given a completed `vibe -p` run, when the `VIBE_HOME` tree is listed, then its paths and modes match the reference subject's after normalizing ids and timestamps.
- [ ] Every scenario of this story passes against the reference subject or is declared `port_only` with a reason.

---

### EP-003: Tools, Permissions, and Integrations

Cover what the model can do and what stops it: every published tool, the permission rules, workspace boundaries, code mode, hooks, MCP servers, skills, plugins, and connectors.

**Definition of Done:** the permission matrix runs one named trial per cell; every scenario passes against the reference subject or is declared `port_only`; E2E-D06, D07, D08, D10, D11, D12, D18, D19, D20, D21 are declared as known defects.

#### US-014: Tool behavior matrix on both harnesses
**Description:** As a maintainer, I want each published tool exercised for success, failure, and invalid input so that tool semantics are guarded where the model sees them.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-009

**Acceptance Criteria:**
- [ ] Given the default harness, when the model calls `read_file`, `write_file`, `edit`, `bash`, `todo`, `unified_harness_scratchpad`, `cron`, `skill`, `search_tool_functions`, and `run_typescript` once with valid arguments, once for a tool error, and once with invalid arguments, then each tool message and the disk state match the reference subject after normalization.
- [ ] Given the legacy harness, when the model calls `grep`, `web_fetch` against a platform page, `task`, and the file tools the same three ways, then each tool message and the disk state match the reference subject.
- [ ] Given each harness, when the first turn request is sent, then the offered tool names and the hash of each parameter schema match the reference subject's.
- [ ] Given a tool call naming a tool that does not exist, when the model sends it, then the error returned to the model and the continuation of the turn match the reference subject.
- [ ] Every scenario of this story passes against the reference subject.

#### US-015: Permission and workspace boundary matrix
**Description:** As a Vibe user, I want every permission flag, rule, and workspace boundary to hold exactly as in the reference so that a tool I disabled or denied never runs.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-009

**Acceptance Criteria:**
- [ ] Given a table of approval modes (none, `--auto-approve`, `--smart-approve`), rules (`--enabled-tools` excluding the tool, `--disabled-tools` naming it, `permission = "never"`, `permission = "always"`, an allowlist pattern, a denylist pattern), and calls (`bash`, `write_file`, `bash` through `run_typescript`), when the binary starts, then one trial per cell is generated and named after the cell.
- [ ] Given each cell, when its trial runs, then the tool message's outcome, the existence of the marker file the call would create, and the recorded approval source match the reference subject.
- [ ] Given `--auto-approve` with `--enabled-tools` excluding `bash`, when the model calls `bash`, then the call is refused by policy and the marker file does not exist; declared `known_defect("E2E-D06")`.
- [ ] Given `--disabled-tools write_file` and no approval flag, when the model calls `write_file`, then it is refused and the file does not exist; declared `known_defect("E2E-D07")`.
- [ ] Given `[tools.bash] permission = "never"` and a command on the default allowlist, when the model calls it, then it is refused; declared `known_defect("E2E-D08")`.
- [ ] Given `--add-dir <dir>` and an agent that accepts edits, when the model reads and writes inside `<dir>`, then both succeed; declared `known_defect("E2E-D10")`.
- [ ] Given a path outside the work tree and the added directories, a symlink inside the work tree pointing outside it, and a `.env` file, when the model reads or writes each, then the outcome matches the reference subject.
- [ ] Every cell and scenario of this story passes against the reference subject.

#### US-016: Code mode deadlines, large outputs, process hygiene, and encodings
**Description:** As a Vibe user, I want runaway code, oversized output, and background processes handled as in the reference so that one tool call cannot end a turn or leave processes behind.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-014

**Acceptance Criteria:**
- [ ] Given `run_typescript` running an infinite loop past its deadline, and a promise that never resolves, when each runs, then the error returns to the model, the turn reaches a final answer, and the run exits 0; declared `known_defect("E2E-D19")`.
- [ ] Given `run_typescript` using the `process.*` API to start, read, and wait on a command, when it runs, then the results returned to the model match the reference subject.
- [ ] Given `read_file`, `bash`, and a `run_typescript` return each producing more than 40,000 characters, when the tool message is sent, then it carries the envelope and the full output exists under the session's `tool-results/` directory; declared `known_defect("E2E-D20")`.
- [ ] Given `sleep 300 &` in `bash`, a `bash` call that hits its timeout, and a turn interrupted during `bash`, when the run ends, then no process carrying the world marker survives; declared `known_defect("E2E-D21")`.
- [ ] Given a file mixing valid UTF-8 and invalid bytes, when `read_file` and `bash cat` return it, then the text reaching the model matches the reference subject.
- [ ] Every scenario of this story passes against the reference subject.

#### US-017: Hooks on both harnesses
**Description:** As a Vibe user who enforces policy with hooks, I want every hook to run with the reference's payload and effect so that a deny hook actually prevents the command.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-009

**Acceptance Criteria:**
- [ ] Given a recorder hook the world writes for `pre_tool`, `post_tool`, and `post_agent`, when a turn calls `bash` on each harness, then each hook runs and the keys of its stdin payload match the reference subject's.
- [ ] Given a `pre_tool` hook that denies `bash`, when the model calls `bash`, then the command does not run (marker file absent) and the denial reason reaches the model in the tool message; declared `known_defect("E2E-D12")` on the default harness.
- [ ] Given a `post_tool` hook that adds context, when the next turn request is sent, then it carries that context.
- [ ] Given one turn, when it ends, then `post_agent` ran exactly once.
- [ ] Given a hook that times out, a strict hook that fails, a hook answering malformed JSON, and a project `.vibe/hooks.toml` not yet approved, when a turn runs, then each outcome matches the reference subject.
- [ ] Every scenario of this story passes against the reference subject.

#### US-018: MCP servers over stdio and HTTP
**Description:** As a Vibe user with MCP servers, I want their tools discovered, approved, and called as in the reference so that the servers I configure are usable from `vibe -p`.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-009

**Acceptance Criteria:**
- [ ] Given a stdio MCP fixture declared in `config.toml`, when a default-harness turn runs, then its tools are reachable through `run_typescript`, the declaration offered to the model types the return from the server's `outputSchema`, and the call result matches the reference subject.
- [ ] Given the legacy harness, when the model calls an MCP tool under `--auto-approve` and under `permission = "always"`, then the tool runs and its result reaches the model; declared `known_defect("E2E-D11")`.
- [ ] Given a streamable HTTP MCP server, when its tool succeeds, returns a JSON-RPC error, returns `isError`, times out, and crashes then restarts before the next call, then each outcome and error code match the reference subject.
- [ ] Given server entries using `env`, `cwd`, headers, an API key variable, and `disabled_tools`, when a turn runs, then each setting is honored as in the reference subject.
- [ ] Given `vibe mcp add` and `vibe mcp remove` with valid and invalid arguments, when each runs, then exit codes and the parsed `config.toml` afterward match the reference subject.
- [ ] Given an MCP server whose command does not exist, when a turn runs, then the turn continues and the error reported matches the reference subject.
- [ ] Every scenario of this story passes against the reference subject.

#### US-019: Skills, plugins, and connectors
**Description:** As a Vibe user, I want my user, project, and plugin skills and my connectors available as in the reference so that project-specific workflows load in every session.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-009

**Acceptance Criteria:**
- [ ] Given skills in the user directory, the project `.vibe/skills` and `.agents/skills`, a plugin, and the builtin set, under untrusted, `--trust`, and `trusted_folders.toml`, when a turn runs on each harness, then the skills offered to the model match the reference subject's; the default-harness project cases declared `known_defect("E2E-D18")`.
- [ ] Given a skill invoked by its slash command in `vibe -p`, when the turn request is sent, then it carries the skill's marker.
- [ ] Given a user plugin and a project plugin, one shipping an MCP server, when a session starts, then the materialized plugin state under `VIBE_HOME` and the plugin's tools match the reference subject.
- [ ] Given connectors served by the platform's bootstrap and gateway routes, with and without a key, when a turn runs, then the connector tools offered and a call result match the reference subject.
- [ ] Given a malformed `SKILL.md` beside a valid one, when a session starts, then the valid skill loads and the outcome for the malformed one matches the reference subject.
- [ ] Every scenario of this story passes against the reference subject.

---

### EP-004: Editor and Server Protocols

Cover `vibe-acp` as an editor drives it and the app-server protocol as its clients drive it, through a JSON-RPC driver and an app-server binary composed like `vibe`.

**Definition of Done:** the app-server scenarios run against a fixture composed by the production function; every scenario passes against the reference subject or is declared `port_only`; E2E-D02, D03, D11, D21, D27, D28, D31, and the measured D43 to D49 are declared as known defects.

#### US-020: JSON-RPC driver and a production-composed app-server binary
**Description:** As an implementing agent, I want a JSON-RPC driver and an app-server binary built the way `vibe` builds its server so that protocol scenarios measure the shipped composition.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-007, US-008

**Acceptance Criteria:**
- [ ] Given a child speaking newline-delimited JSON-RPC on stdio, when a scenario sends requests, then the driver correlates responses by id and `await_notification(predicate, deadline)` returns the first matching notification.
- [ ] Given scripted auto-responders, when the child sends `session/request_permission`, `fs/read_text_file`, `fs/write_text_file`, `terminal/*`, `callback/call`, or a client tool call, then the scripted answer is sent and the exchange is journaled.
- [ ] Given raw mode, when a scenario writes arbitrary bytes, then the driver sends them unframed and returns raw lines.
- [ ] Given the `test-fixtures` feature, when `vibe-cli` builds, then a test-only binary serves the app-server protocol over stdio, composed by the same function `vibe` uses to build its app-server (`crates/vibe-cli/src/bootstrap.rs:318`), with the provider read from `config.toml` and no hardcoded prompt.
- [ ] Given `cargo build --release -p vibe-cli -p vibe-acp --bins` without features, when it ends, then only `vibe` and `vibe-acp` are produced.
- [ ] Given `vibe-app-server-stdio-fixture`, when this story lands, then its source is unchanged and its parity replays pass.
- [ ] Given a server that exits while a request is pending, when the driver waits, then it fails within the deadline with the exit status and the last 4 KiB of stderr.
- [ ] Given the reference subject, when a self-test runs `initialize` against the reference's `vibe-app-server`, then the round trip succeeds.

#### US-021: ACP provider routing, authentication, and the golden prompt
**Description:** As an editor user, I want `vibe-acp` to use the provider in my `config.toml` and to ask for sign-in when no key exists so that the editor integration works without hidden variables.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-005, US-020

**Acceptance Criteria:**
- [ ] Given a config-only provider (Mistral backend on loopback, generic backend with its own key variable, and a local model needing no key), when an editor sends `initialize`, `session/new`, and `session/prompt`, then the prompt ends with `stopReason: end_turn` within 10 s and the journal holds one turn request naming the configured model; declared `known_defect("E2E-D02")`.
- [ ] Given no resolvable key, when the editor sends `session/new`, then the error code is `-32000` and `initialize` advertised at least one authentication method; declared `known_defect("E2E-D03")`.
- [ ] Given a prompt scripted to think, call `read_file`, and answer, when it runs, then the sequence of `session/update` kinds (message chunks, thought chunks, tool call, tool call updates, usage, available commands) matches the reference subject's.
- [ ] Given the provider answers 401, when the editor sends `session/prompt`, then the error matches the reference subject's.
- [ ] Every scenario of this story passes against the reference subject.

#### US-022: ACP permissions, cancellation, retries, MCP, and client delegation
**Description:** As an editor user, I want permissions, cancellation, retries, and MCP tools to behave in the editor as in the reference so that approving or canceling from the editor does what it says.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-021, US-018

**Acceptance Criteria:**
- [ ] Given a tool needing approval, when the editor answers allow once, allow always, reject, and dismissed, then the tool outcome and whether the next call asks again match the reference subject; given a path outside the work tree, then the offered options match the reference subject's.
- [ ] Given `sh -c 'sleep 3; echo late > late.txt'` running, when the editor sends `session/cancel`, then the prompt ends with `stopReason: cancelled`, `late.txt` does not exist after 5 s, and no process carrying the world marker survives; declared `known_defect("E2E-D21")`.
- [ ] Given a pending permission request, when the editor cancels the prompt and answers the request with the `cancelled` outcome, then the prompt ends `cancelled` and the tool call's last update is not `in_progress`; declared `port_only`, because the reference never ends that prompt (measured 2026-10-29).
- [ ] Given 500 then 200, and 429 then 200, when the prompt runs, then one `_session/retrying` notification is sent per retry; declared `known_defect("E2E-D31")`.
- [ ] Given a legacy-harness MCP tool, when the model calls it, then `session/request_permission` is sent and, once approved, the tool runs and the next request carries its result; declared `known_defect("E2E-D11")`.
- [ ] Given an MCP server the editor passes in `session/new`, when a default-harness turn calls it through `run_typescript`, then the server receives one call and the result matches the reference subject (`mcp_transport_failed`, measured 2026-10-29).
- [ ] Given the legacy harness with client file system and terminal capabilities, when the model reads, writes, and runs a command, then the order of `fs/*` and `terminal/*` requests matches the reference subject's.
- [ ] Every scenario of this story passes against the reference subject.

#### US-023: ACP sessions, extensions, setup, and protocol robustness
**Description:** As an editor user, I want sessions and extensions to work across restarts and clients so that editor history and session commands are reliable.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-021

**Acceptance Criteria:**
- [ ] Given sessions created through `vibe-acp`, when the editor lists, loads after a restart, forks, closes, and resumes them, then the replayed history and ids match the reference subject.
- [ ] Given a session created by `vibe -p`, when the editor lists and loads it, then it appears and replays as in the reference subject.
- [ ] Given the advertised slash commands and the title, delete, rewind, and trust extensions, when the editor uses each, then the outcomes match the reference subject; deleting a live session with no turn declared `known_defect("E2E-D44")`.
- [ ] Given a relative `cwd` in `session/new`, an unknown method, and a malformed line, when each is sent, then the responses match the reference subject's (the relative `cwd` declared `known_defect("E2E-D45")`), and the agent version in `initialize` equals the workspace version (the pinned reference version for the reference subject).
- [ ] Every scenario of this story passes against the reference subject.

#### US-024: App-server session lifecycle and persistence
**Description:** As a maintainer, I want the app-server's sessions to survive restarts, root switches, and other clients so that sessions written by the port stay readable by every client, the reference included.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-020, US-005

**Acceptance Criteria:**
- [ ] Given a client that starts a session, runs two turns, lists, reads, restarts the server, resumes, and runs a third turn, when the sequence ends, then history and ids match the reference subject's.
- [ ] Given a connection with a root, when the client resumes another saved session, or forks attached on the default harness, and then runs a turn, then the turn runs on the resumed or forked session; declared `known_defect("E2E-D27")`.
- [ ] Given a session created by `vibe -p`, when the server lists and resumes it, then it appears and continues; given a session created by the server, then `vibe -p --resume` continues it.
- [ ] Given the default-harness store, when a session is written, then every JSON document in it but `meta.json`, which the reference also writes without one, ends with a newline; declared `known_defect("E2E-D28")`.
- [ ] Given a session written by the port and the reference at the pin, when the reference's `vibe-app-server` runs on the same `VIBE_HOME`, then its `session/list` includes the session and loads it; skipped with the `off_pin_reason` when the checkout is missing; declared `known_defect("E2E-D28")`.
- [ ] Given SIGKILL during a turn, when a fresh server reads the session, then the user message and the lease state match the reference subject's; declared `known_defect("E2E-D43")`.
- [ ] Every scenario of this story except the cross-subject one passes against the reference subject.

#### US-025: App-server wire robustness, turn control, and method sweep
**Description:** As a maintainer, I want the app-server's framing, turn control, and full method inventory measured so that one malformed message never ends a client's connection when the reference keeps it.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-024

**Acceptance Criteria:**
- [ ] Given a 9 MB frame and a 12 MB image attachment, when each is sent, then the response matches the reference subject's and the next request is still answered; declared `known_defect("E2E-D46")`.
- [ ] Given a message missing `jsonrpc`, a UTF-8 byte-order mark, a CRLF line ending, a batch array, and a null id, when each is sent, then the response and whether the connection survives match the reference subject; the first two declared `known_defect("E2E-D46")`.
- [ ] Given a scripted sequence of operations, when each completes, then the state rebuilt from notifications equals `session/read`.
- [ ] Given a default-harness approval through `callback/call`, when the client denies, approves once, approves for the session, and approves permanently, then each tool outcome matches the reference subject; the rule a permanent approval persists declared `known_defect("E2E-D49")`.
- [ ] Given gates holding a turn open, when the client interrupts, steers, queues a prompt, and interrupts a shell command, then each outcome matches the reference subject.
- [ ] Given every method in the routed inventory of `vibe-protocol`, when each is called without a root and with a root, then the response class (result, error code) matches the reference subject's; the divergent methods declared `known_defect("E2E-D47")` and `known_defect("E2E-D48")`.
- [ ] Given two servers on one `VIBE_HOME`, when both open the same session, then the lease outcome matches the reference subject's, as do the values, marks, and worktree methods.

---

### EP-005: Terminal UI under a Pseudo-Terminal

Drive the shipped TUI through a real PTY with a screen model, from a configured provider through approvals, shell commands, rewind, and slash commands.

**Definition of Done:** TUI scenarios run in the workspace suite with at most 2 live PTY sessions per process; E2E-D01, D22, D23, D24, D25 are declared as known defects; each expectation cites the reference module it reproduces.

#### US-026: PTY driver with a terminal screen model
**Description:** As an implementing agent, I want a PTY driver that answers terminal queries and exposes a real screen model so that TUI scenarios assert on what the user sees.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-006, US-007

**Acceptance Criteria:**
- [ ] Given a world child spawned in a PTY (portable-pty 0.9) at a given size, when it writes output, then a vt100 0.18 screen model exposes its text, cell attributes, cursor position, and alternate-screen state.
- [ ] Given the TUI sends a cursor position report request or a primary device attributes request, when the driver reads it, then it answers, so startup never stalls on it.
- [ ] Given `wait_screen(predicate, deadline)`, when the deadline expires, then the trial fails with the final screen text and the last 8 KiB of raw bytes.
- [ ] Given the key encoder, when a scenario sends text, Enter, Esc, Tab, arrows, Ctrl chords, and a bracketed paste, then the TUI receives the corresponding bytes (self-test against the composer).
- [ ] Given a resize, when it happens, then the PTY and the screen model change size and the TUI repaints at the new size.
- [ ] Given an OSC 0 or 2 sequence, when the TUI sets the window title, then the driver captures it.
- [ ] Given parallel trials in one process, when they open PTYs, then at most 2 PTY sessions are live at once.
- [ ] Given a child that exits before a predicate holds, when the driver waits, then it fails within 2 s of the exit with the exit status and the final screen.
- [ ] Given a newer release on the release route and the port's index redirected through `VIBE_UPDATE_BASE_URL`, when `vibe --check-upgrade` and `vibe update` run in the PTY, then the update prompt renders and each choice exits as the reference's `_show_update_prompt` (`vibe/cli/cli.py`) does; declared `port_only` (moved from US-013).

#### US-027: TUI provider routing, golden turn, and resume
**Description:** As a Vibe user on a third-party provider, I want the TUI to use my configured provider and to resume the session it tells me to resume so that the interactive client works without Mistral credentials.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-026, US-005

**Acceptance Criteria:**
- [ ] Given a generic-backend provider with its own key variable and no `MISTRAL_API_KEY`, when the TUI starts and a prompt is submitted, then no onboarding screen appears, the journal holds one turn request naming the configured model, the answer appears on screen, and the sentinel records no host; declared `known_defect("E2E-D01")`.
- [ ] Given a Mistral-backend provider on loopback, when a prompt is submitted, then the answer appears on screen and the request carries the configured key.
- [ ] Given an answer paced in 5 chunks with a gate before the last, when the TUI renders it, then the screen shows a partial answer before the gate is released, and the token footer updates after it.
- [ ] Given `/exit`, when the TUI ends, then it prints the continuation lines with a short session id.
- [ ] Given that short id, when `vibe --resume <short id>` starts, then the earlier history is on screen and the next turn request carries it; declared `known_defect("E2E-D24")`.
- [ ] Given `vibe -c`, and a bare `--resume` with sessions the binary created on the default harness, when each starts, then the last session opens, and the picker lists the sessions and opens the selected one; the picker case declared `known_defect("E2E-D24")`.
- [ ] Given the provider answers 500 on every attempt, when a prompt is submitted, then an error appears on screen within the time-scaled deadline and `/exit` still ends the TUI.

#### US-028: TUI approvals, shell commands, interruption, rewind, and questions
**Description:** As a Vibe user, I want approvals, `!` commands, interruption, rewind, and questions to work after a model turn so that the interactive features I rely on are guarded.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-027

**Acceptance Criteria:**
- [ ] Given a `bash` call needing approval, when the user picks allow once, allow for the session, allow always, deny, and Esc, then the command's marker file appears only after an allow, and "always" persists the rule the reference persists for that command (`vibe/cli/` approval flow).
- [ ] Given a completed model turn, when the user runs `!echo <marker>`, then the marker appears on screen within 5 s and no continuity-loss notice appears; declared `known_defect("E2E-D23")`.
- [ ] Given a paced stream, when the user presses Esc, and in another trial Ctrl+C, then the turn stops, chunks served after the interrupt never appear on screen, and a second interrupt behaves as in the reference.
- [ ] Given a turn that edited files, when the user rewinds it in the live session and after `vibe -c`, then the files are restored and the rewound prompt is in the composer; declared `known_defect("E2E-D22")`.
- [ ] Given `ask_user_question` arriving while the user is typing, when the user presses Down as the dialog opens and Enter once its 500 ms input grace is over, then the answer sent to the model is the second option and the typed text stays in the composer; E2E-D26 did not reproduce, so the trial guards the behavior undeclared.
- [ ] Given a denied approval, when the turn continues, then the denial reaches the model and the turn reaches a final answer.

#### US-029: Slash commands, configuration editor, model picker, composer, and onboarding
**Description:** As a Vibe user, I want every slash command and the composer to work after a model turn so that no command fails with a server error in real use.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-027

**Acceptance Criteria:**
- [ ] Given a trial generated per command of the TUI command registry, when each command runs after a completed turn, then it opens and closes without an app-server error on screen and the process stays alive.
- [ ] Given `LOG_LEVEL=DEBUG`, when the user runs `/debug` and `/log`, then `/debug` lists log entries and `/log` shows a path that exists; declared `known_defect("E2E-D25")`.
- [ ] Given `/config`, when the user changes a value, then `config.toml` holds the new value; declared `known_defect("E2E-D25")`.
- [ ] Given two models configured, when the user opens `/model`, then the current model is preselected and Esc leaves it active.
- [ ] Given multiline input, a bracketed paste, history across sessions, and `@` and `/` completion, when the user submits, then the content reaching the platform matches what the composer showed, and a paste submits nothing by itself.
- [ ] Given onboarding with no key, when the user pastes a key, then `VIBE_HOME/.env` holds it and the next turn succeeds.
- [ ] Given `vibe-acp --setup` under a PTY, when a key is pasted, then `VIBE_HOME/.env` holds it with the mode the reference writes, read from the reference source since its Textual TUI is not driven (moved from US-023).
- [ ] Given an unknown slash command, when the user submits it, then the message shown and the composer content afterward match the reference.

#### US-030: Session title, startup dialogs, resize, degraded provider, and idle output
**Description:** As a maintainer, I want the TUI's peripheral behaviors measured so that titles, dialogs, and degraded states do not regress silently.

**Priority:** P2
**Size:** M (3 pts)
**Dependencies:** Blocked by US-027

**Acceptance Criteria:**
- [ ] Given the first answer of a session, when the utility model returns a title, then `meta.json` holds it before the TUI exits and the window title carries it, on both harnesses.
- [ ] Given an untrusted folder with a project config, a What's new cache older than the version, and `TERM_PROGRAM=vscode`, when the TUI starts, then the trust dialog, What's new shown once with the cache updated, and the VS Code promotion behavior match the reference (`docs/parity.md` row 33).
- [ ] Given sizes 60x20, 100x30, and 140x40, when the PTY is resized during a stream and during an approval, then no prompt is lost and the approval is still answerable.
- [ ] Given a provider answering 500 permanently, when a prompt is submitted, then a visible state appears within 10 s; a refused connection is asserted with a `shared_with_reference` note.
- [ ] Given a started TUI left idle for 10 s, when bytes are counted, then it writes at most 4 KiB.

---

### EP-006: Observability, Voice, and Platform Reach

Cover what the binaries report (telemetry, logs, spans), the voice stack without audio hardware, and the Windows runner.

**Definition of Done:** telemetry, log, span, and voice scenarios run in the workspace suite; E2E-D29 and D30 are declared as known defects; the windows-2022 job runs the stdio and JSON-RPC scenarios.

#### US-031: Telemetry event sets per surface
**Description:** As a maintainer, I want the telemetry each surface emits measured on the default harness so that product analytics reflect real sessions.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-009, US-021, US-027

**Acceptance Criteria:**
- [ ] Given telemetry enabled on `vibe -p` and `vibe-acp` on the default harness, when each scripted scenario runs, then the multiset of datalake events matches the reference subject's: one `vibe.request_sent` per chat request, `user_plan` in request metadata, `tool_call_finished` after approved tools, and no extra `vibe.new_session` or `vibe.ready` on `-c`, `--resume`, or `session/load`; declared `known_defect("E2E-D29")`.
- [ ] Given the TUI on the default harness, when a prompt runs, an interrupt happens, and `/clear` runs, then the event multiset matches the set recorded from the reference TUI for the same sequence (event names and counts only, owner `vibe/core/telemetry/`); declared `known_defect("E2E-D29")`.
- [ ] Given `enable_telemetry = false`, no Mistral key, or only a third-party provider, when a session runs, then the datalake route receives no request.
- [ ] Given the managed-configuration route failing, when a session starts, then the events emitted and the time to the first answer match the reference subject.
- [ ] Given ACP `_telemetry/send`, when the editor sends a known event with a session, an unknown event, and an event without a session, then only the first reaches the datalake, on both harnesses.
- [ ] Given the legacy harness, when the same scenarios run, then the event multisets match the reference subject's.

#### US-032: Log file and OpenTelemetry spans
**Description:** As a maintainer, I want the log file and spans measured so that diagnostics exist when a user reports a problem.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-009, US-005

**Acceptance Criteria:**
- [ ] Given `LOG_LEVEL=INFO`, `LOG_LEVEL=DEBUG`, and `DEBUG_MODE=true`, when `vibe -p` runs, then `vibe.log` is non-empty, every line parses as a log record, the `logs` directory has mode 0700, and the file has mode 0600; declared `known_defect("E2E-D30")`.
- [ ] Given an invalid and a small `LOG_MAX_BYTES`, when `vibe -p` runs, then the behavior matches the reference subject or a `docs/parity.md` ledger row names the kept divergence.
- [ ] Given `enable_otel` with `otel_endpoint` on the platform's trace route, when a legacy-harness turn runs, then span names and the model, temperature, response id, and status code attributes match the reference subject's; on the default harness, the span export matches the reference subject's.
- [ ] Given the trace route answering 500, when a turn runs, then it completes and exits 0.

#### US-033: Voice without audio hardware
**Description:** As a maintainer, I want dictation and narration driven through a file-backed audio device and local speech stand-ins so that the voice stack is measured on CI without hardware.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-021, US-026

**Acceptance Criteria:**
- [ ] Given a Linux sandbox whose ALSA configuration routes the default capture to a file and playback to a null sink, with `/dev/snd` hidden, when a binary enumerates devices, then it sees exactly that device.
- [ ] Given a realtime transcription WebSocket stand-in and a speech route on the platform, when ACP `_voice/transcribeStart`, `transcribeStop`, and `narrate` run, then a `_voice/transcriptionDelta` carries the stand-in's text, the session update frame's format and rate match the reference subject's, the speech request body matches, and narration preparation precedes completion.
- [ ] Given no capture device, when ACP `transcribeStart` runs and the TUI receives Ctrl+R, then the error matches the reference subject's, and the screen shows the notice with no line containing `ALSA lib`.
- [ ] Given voice enabled in the TUI, when the user presses Ctrl+R then a key, then the transcript lands in the composer; Ctrl+C cancels without text; with voice disabled, Ctrl+R has no effect.
- [ ] Given the narrator enabled, when an answer arrives, then the speech request carries the summary, and `/voice` toggles persist to `config.toml`.
- [ ] Given the transcription stand-in sending an error frame and the speech route answering 500, when each runs, then the error surfaces as in the reference subject.

#### US-034: Run the e2e suite on windows-2022
**Description:** As a Windows user, I want the e2e scenarios to run on the Windows runner so that Windows regressions at the binary boundary are caught.

**Priority:** P2
**Size:** L (5 pts)
**Dependencies:** Blocked by US-009, US-021, US-024, US-026

**Acceptance Criteria:**
- [ ] Given windows-2022, when the e2e binaries start, then they run with the sentinel only, every child environment keeps the Windows base set and `USERPROFILE`, and teardown ends process trees with `taskkill /T /F`.
- [ ] Given the `windows-quality` job, when it runs, then every stdio and JSON-RPC scenario runs, and each one failing because of a product defect carries the `windows:` tag convention of `AGENTS.md`.
- [ ] Given the PTY driver on ConPTY through portable-pty, when the TUI scenarios run, then they pass or carry the tag convention.
- [ ] Given scenarios that need Linux-only facilities (the ALSA sandbox, `/proc` scans), when the binaries run on Windows, then those scenarios are skipped with a `windows by design:` reason.
- [ ] Given a scenario whose provider names a non-loopback host, when it runs on windows-2022, then the trial fails naming the host, as on Linux.
- [ ] Given the job before and after this story, when durations are compared, then the job grows by at most 15 minutes.

## Functional Requirements

- FR-01: Every e2e scenario must run the real `vibe` or `vibe-acp` binary, or the production-composed app-server binary, as a child process in its own world.
- FR-02: A world must configure the provider and every host through `config.toml`; the harness must not pass a port-only seam unless the scenario tests that seam.
- FR-03: On Linux, every e2e process must run inside a network namespace with only loopback; without one, the e2e test binaries must refuse to run unless `VIBE_E2E_UNJAILED=1` is set.
- FR-04: Every trial must fail when the egress sentinel recorded a host.
- FR-05: The platform must serve chat completions with ordered, paced, gated, and fault-injected responses, and journal every request with its headers, body, queue, and arrival time.
- FR-06: The platform must fail the trial on an unexpected request, an unknown route, or a tool message answering an undeclared tool call.
- FR-07: Every wait must have a deadline, scaled by `VIBE_E2E_TIME_SCALE`; no scenario may sleep to synchronize.
- FR-08: Teardown must kill every process the trial started and fail the trial when one carrying the world marker survives.
- FR-09: A failed trial must leave its artifacts under `$CARGO_TARGET_TMPDIR/e2e-artifacts/<trial>/`; a passing trial must leave nothing.
- FR-10: A scenario catching an open defect must assert the reference behavior and declare `known_defect("E2E-Dnn", "<cause>")`; the CI summary must count these declarations and list those that now pass.
- FR-11: With `VIBE_E2E_SUBJECT=reference`, every stdio and JSON-RPC scenario not declared `port_only` must run against the pinned reference with the same assertions.
- FR-12: The harness must generate one named trial per row of a scenario table (permission matrix, slash commands, argument errors, protocol methods).
- FR-13: The harness must not write outside the world root, `$CARGO_TARGET_TMPDIR/e2e-artifacts/`, and the system temporary directory.
- FR-14: No scenario, fixture, or expectation may contain reference source, prompt text, tool descriptions, or reference-authored messages.

## Non-Functional Requirements

- **Performance:** a world builds in at most 200 ms (median over 20); a programmatic scenario completes in at most 2 s median; a PTY scenario in at most 30 s; the e2e binaries add at most 3 minutes to the Linux `quality` job at Release 1 and at most 6 minutes at Release 3.
- **Security:** 0 hosts recorded by the sentinel on 100% of trials; child environments never inherit `MISTRAL_API_KEY`, a keyring session bus, or `FORCE_COLOR`; 0 real credentials in any artifact (values of variables named with `KEY`, `TOKEN`, or `SECRET` replaced).
- **Reliability:** 0 e2e failures in 10 consecutive `quality` runs on `main`, with 0 retries; every wait bounded at 30 s by default; 0 processes carrying a world marker alive after a trial.
- **Resource use:** at most 50 MB per world; at most 20 MB of artifacts per failed trial; 0 directories left in the system temporary directory after a passing run.
- **Scalability:** the harness supports at least 150 scenarios across both binaries with world setup staying at or under 200 ms median; at most 2 live PTY sessions per process.
- **Accessibility:** not applicable; the feature adds no user-facing interface.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | Empty state | A world with no key and a Mistral provider (US-029 onboarding), or a fresh `VIBE_HOME` (US-013 inventory) | The binary behaves as for a first-time user, matching the reference | The binary's own onboarding text, asserted by presence |
| 2 | Loading state | A paced or gated stream (US-012, US-027) | Partial output is visible before the stream ends | None |
| 3 | Error state | HTTP 4xx and 5xx, truncated SSE, malformed SSE (US-011) | Retry or report exactly as the reference does | The binary's error line, compared to the reference subject's |
| 4 | Network degradation | A refused connection, a managed-configuration route failing (US-030, US-031) | Bounded by the time scale; shared behavior annotated | Harness failure naming the wait that expired |
| 5 | Permission changes | An "always" rule persisted, then a config setting the tool to `never` (US-015, US-028) | The later rule wins as in the reference | None |
| 6 | Concurrent modifications | Two app-servers on one session (US-025); `vibe -p` and the server on one session (US-024) | Lease outcome matches the reference | None |
| 7 | Boundary values | Tool output at 40,000 characters, a 300 KB prompt, a 9 MB frame (US-016, US-009, US-025) | Spill, accept, or reject as the reference does | None |
| 8 | Undo and reversal | `/rewind` and the ACP rewind extension (US-028, US-023) | Files restored, prompt returned to the composer | None |
| 9 | Interrupted flows | SIGINT, SIGKILL mid-turn, `session/cancel`, Esc (US-013, US-024, US-022, US-028) | No surviving process, no lost user message, reference stop reason | None |
| 10 | External dependencies | MCP server crash, missing command, timeout; speech stand-in errors (US-018, US-033) | Turn continues and the error matches the reference | The binary's error, compared to the reference subject's |
| 11 | Harness failure | The jail is unavailable, the reference is off the pin (US-003, US-008) | The binary exits non-zero before any trial | "jail unavailable: install bubblewrap or set VIBE_E2E_UNJAILED=1" and the `off_pin_reason` text |
| 12 | Egress attempt | A hardcoded host reached despite redirection (US-003) | The trial fails naming the host | "egress to <host>:<port> recorded by the sentinel" |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | PTY and live scenarios flake under full-workspace load, as several existing tests do | High | High | Deadlines instead of sleeps, server-side gates, at most 2 PTY sessions, waits on screen predicates after the alternate screen is entered, 0 retries so flakes surface, failure artifacts with raw bytes |
| 2 | The e2e binaries make the quality gate too slow for local use | Med | Med | Release budgets in the NFRs, wall time printed in CI, platform and worlds in-process, `VIBE_E2E_TIME_SCALE` for slow machines |
| 3 | A future runner image restricts unprivileged user namespaces | Med | High | US-001 records the 24.04 behavior and the sysctl workaround; the binaries fail closed with a message; the runner stays pinned to ubuntu-22.04 |
| 4 | Known-defect tags rot after fixes land | Med | Med | A CI step runs ignored trials and lists those that now pass |
| 5 | A re-pin changes reference behavior and expectations go stale | Med | Med | Rerun the reference subject in the re-pin change, as corpora are regenerated there |
| 6 | Scenarios absorb reference-authored text | Low | High | FR-14, expectations written as observations, reference paths cited instead of quoted, review checklist item |
| 7 | The test-only app-server binary ships | Low | High | Feature-gated `required-features`, and the release-build criterion of US-020 |
| 8 | The suite duplicates parity tests and doubles maintenance | Med | Low | E2E asserts at the shipped binary boundary with the reference as subject; parity replays keep measuring corpora; no corpus is touched |
| 9 | An exploration finding is wrong and a known-defect scenario encodes it | Low | Med | Every known-defect scenario must pass against the reference subject before it lands (US-008) |

## Non-Goals

What this version explicitly does NOT include:

- Fixing any defect in the register; fixes belong to a follow-on PRD whose stories remove `known_defect` tags.
- Changing parity drivers, committed corpora, the parity fixture, pin sources, or scorecard rows; aligning the oracles with the Unified Harness and with config-only providers belongs to the next re-pin.
- Browser sign-in and MCP OAuth login end to end: both need a keyring backend the jail cannot provide today.
- Real audio hardware, macOS CI, and a paced audio device for comparing dictation against the reference.
- The feedback prompt, whose 5% random draw needs a seam in the shipped binary.
- Load, latency, or memory benchmarking of the binaries.
- Driving the reference's Textual TUI under a PTY.
- Cloud success paths (teleport, Vibe Code sessions) and experiment variations beyond the evaluation route's neutral default.

## Files NOT to Modify

- `crates/vibe-core/src/parity.rs` (`REFERENCE_COMMIT`) and `scripts/parity/pin.py` (`EXPECTED_COMMIT`): the pin changes only in a re-pin, guarded by `crates/vibe-core/src/parity/parity_tests.rs`.
- `scripts/parity/*.py`: the oracles change only in a re-pin.
- `crates/vibe-cli/tests/runtime-parity/`, `crates/vibe-app-server/tests/tool-surface/baseline.json`, and every other committed corpus: they change only in a re-pin.
- `crates/vibe-app-server/tests/fixtures/app_server_stdio_fixture.rs`: the parity replays depend on its composition.
- `crates/vibe-app-server/src/wire_validation/schema.json`: production input; a change poisons unrelated tests.
- `docs/parity.md`: this PRD moves no part's parity (only US-032 may add a ledger row it names).
- Product source under `crates/*/src/`, except the `test-fixtures` export US-020 adds to `vibe-cli`.
- `crates/vibe-cli/tests/support/`, `tui_pty.rs`, `tui_conpty.rs`: existing tests stay as they are; consolidation is not in scope.

## Technical Considerations

- **Architecture:** Harness entry point; recommended: libtest-mimic 0.8 custom harness per package, so the binary enters the jail before any trial and generates trials from tables. Alternatives: the standard libtest harness with a Cargo `target.runner` that jails only binaries named `e2e` (global configuration for every developer), or a per-test re-execution (fragile test-name detection). Engineering to confirm in US-007.
- **Jail:** Wrapper choice; recommended: `bwrap --unshare-net` first, nested `unshare` second, as US-001 measures. Does the inner user namespace need util-linux 2.38's `--map-user` on ubuntu-22.04, or is bubblewrap required there?
- **App-server composition:** recommended: a test-only `[[bin]]` in `vibe-cli` behind `test-fixtures`, calling a `#[doc(hidden)]` export of the existing composition. Alternatives: move the composition into `vibe-app-server` with `Arguments` and the utility provider passed as plain parameters (a product refactor), or ship a real `vibe-app-server` binary as the reference does (product scope). Engineering to confirm in US-020.
- **Platform implementation:** recommended: `std::net` listeners and threads, like the existing fixtures, so trials stay synchronous. Is a hand-written SSE writer with per-event flush enough, or does any binary require HTTP/2 or chunked-encoding behavior a minimal server cannot serve?
- **Fixture binaries:** the MCP fixtures live in `vibe-app-server` (`crates/vibe-app-server/Cargo.toml:13-39`), so their `CARGO_BIN_EXE_*` is not visible from `vibe-cli` tests. Recommended: locate them beside `CARGO_BIN_EXE_vibe` in the same profile directory and fail with an actionable message when missing; alternative: a minimal MCP fixture inside `vibe-e2e`.
- **Span capture:** decode OTLP with the protobuf types already in the dependency tree through `opentelemetry-otlp`, or record raw bytes and compare span names only?
- **Dependencies:** libtest-mimic 0.8.2 and vt100 0.18.1 as dev-dependencies (new); portable-pty 0.9.0 (already a Windows dev-dependency, extended to Unix); tungstenite for the transcription stand-in in US-033 (already in the lockfile through tokio-tungstenite 0.30); bubblewrap as a CI system package if US-001 selects it.
- **Migration:** none for product data. Existing PTY tests keep their seams; a later change may port them to the harness. Rollback is deleting the e2e binaries and the crate.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| Verified defects with an executable reproduction | 0 of 31 | 23 of 31 | Month-1 | `known_defect("E2E-D` count in the CI summary |
| Verified defects with an executable reproduction | 0 of 31 | 31 of 31 | Month-6 | Same |
| Real-binary scenarios with a model turn and a config-only provider | 0 | At least 100 | Month-6 | Trial count of the e2e binaries |
| Scenarios validated against the reference subject | N/A (new) | 100% of stdio and JSON-RPC scenarios, or `port_only` with a reason | Every release | `VIBE_E2E_SUBJECT=reference` run attached to each scenario story's PR |
| Egress recorded during e2e runs | Not measured | 0 hosts | Every CI run | Sentinel logs, trial failures |
| E2E flake rate | N/A (new) | 0 failures in 10 consecutive `quality` runs on `main` | Month-1 and Month-6 | CI history |
| E2E wall time on the Linux `quality` job | N/A (new) | At most 3 min (Release 1), at most 6 min (Release 3) | Per release | CI summary |
| Known defects that pass but keep their tag | N/A (new) | 0 for more than one merged change | Continuous | Non-blocking `--ignored` CI step |

## Open Questions

- Who owns the fixes, and in what order? A follow-on PRD for the defect register, ordered by severity (E2E-D01, D02, D06, D07, D08 first); the maintainer decides after Release 1, since each fix removes one tag.
- Should the next re-pin align the oracles (parameterize `HARNESS_FLAGS` for the Unified Harness, stop injecting `VIBE_API_BASE`, compose the parity fixture like `vibe`, record tool message content and `VIBE_HOME` state)? The maintainer decides at the re-pin; the scorecard should be remeasured then, since several rows at 100 do not measure the shipped path.
- For defects shared with the reference (unenforced budgets, the reasoning representation, cross-repository `-c`), keep parity or fix with a ledger row? The maintainer decides per item; the scenarios carry a `shared_with_reference` note until then.
- Which keyring backend would make OAuth scenarios possible (a headless Secret Service in the jail, or a test backend behind a feature)? Needed before browser sign-in or MCP OAuth can be scoped; no date.
- Which paced audio device (a private PipeWire null sink, `snd-aloop`, or an input seam) makes dictation comparable with the reference? Needed only to extend US-033 to the reference subject.
[/PRD]
