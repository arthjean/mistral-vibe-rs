# Parity System Audit, 2026-11-06

An audit of the machinery that measures Mistral Vibe RS against the Python
reference: the pin, the capture drivers, the committed corpora, the Rust
replays and their ledgers, the scorecard, CI, and the licensing boundary.

| Field | Value |
|---|---|
| Rust repository | `bb666b47` (`main`) |
| Pin | `376f6a3` (v2.26.0, committed 2026-10-06), `vibe_core::parity::REFERENCE_COMMIT` and `scripts/parity/pin.py` |
| Reference checkout | `C:\dev\mistral-vibe` at `c96a6db` (v3.1.0, 2026-11-04), clean, its `.venv` an editable install built for 3.1.0 |
| Releases after the pin | v2.26.1, v3.0.0, v3.0.1, v3.0.2, v3.0.3, v3.0.4, v3.1.0 |
| What was executed | `git` and `grep` against both repositories, three independent census scripts (methods, licensing overlap, corpus-to-test map), and `cargo test -p vibe-core --lib --all-features -- parity` (138 passed, 1 failed, see M8). No live probe could run: the checkout is off the pin and its environment is built for v3.1.0. |

## Verdict

The machinery that operates inside one pin is carefully engineered, in places
ahead of common practice. It is no longer measuring anything a user runs, and
several of its failure modes are silent.

1. **It has lost its object.** Upstream v3.0.0 removed the legacy backend that
   seventeen oracles select through `--legacy-harness`, the Python terminal
   client that the TUI, command and completion oracles drive, and the CLI
   modules the programmatic and CLI-surface oracles import. The pin is seven
   releases and one month behind, nothing in the repository notices, and the
   header row that states the score against upstream `HEAD` is a hand-written
   claim dated 2026-10-08.
2. **Green does not always mean measured.** No job runs a live probe, skips
   are hidden by the test harness, 27 committed corpora are never checked
   against the pin, and a port-mode capture overwrites a committed corpus by
   default. Each of these lets a replay pass while proving nothing.
3. **"100" means "no unledgered structural divergence", not conformance.**
   Multi-word text is unmeasured in nine replays, the protocol row compares
   shapes only, whole observation subtrees are ledgered, and ledger growth is
   unbounded. 35 of 36 rows sit at 100.
4. **The licensing claim in `NOTICE` is contradicted by the tree.** Reference
   docstrings appear verbatim in Rust doc comments, user-visible reference
   messages appear verbatim in Rust string literals and corpora, and no
   automated guard exists.

Answers to the questions asked:

| Question | Answer |
|---|---|
| Complete? | No. The coverage is broad at the pin (every row names an oracle, the method inventory matches 146 for 146), but 85 reference files are claimed by no row, `vibe/cli-rust/` (now upstream's only terminal client) is unclaimed, and nothing tracks upstream. |
| Robust? | Inside the pin, mostly; the bidirectional ledger and the pinned-tree re-execution are strong. It fails open in at least six places (H1, H2, M1, M2, M6, L3). |
| State of the art? | Partly. Pinned-tree re-execution, digest-only prose and stale-entry detection are above typical golden-file practice. Missing: corpus provenance by construction, measurement of the oracle's own sensitivity, drift detection, and a machine-derived scorecard. |
| Coherent? | Uneven. Four ledger styles, two off-pin flags, commit checks applied file by file after incidents, a scorecard method section that contradicts its own rows, and stale counts. |

## How the system works

```
pin (parity.rs + pin.py, held equal by parity_tests.rs)
  -> driver in scripts/parity/ (re-executes under the reference interpreter
     on a git-archive of the pin, or runs the checkout's console scripts)
  -> committed corpus under crates/*/tests/ (prose reduced to length + SHA-256)
  -> Rust replay: runs the same driver against this port's binary, diffs the
     two observations as JSON pointers, requires every difference to match a
     ledger entry and every entry to still reproduce
  -> live probe (only on a pinned checkout): recaptures the reference with
     --check and compares with the committed corpus
  -> docs/parity.md row (score typed by hand; total recomputed by
     scorecard_tests.rs; accepted-divergence evidence resolved by ledger_tests.rs)
```

## Method

Five sub-audits ran in parallel: core-domain oracles (`crates/vibe-core`),
app-server oracles, terminal client, editor protocol and voice oracles,
coverage and upstream drift, and the cross-cutting guarantees (CI, licensing,
scorecard, environment). The pin and replay core (`parity.rs`,
`parity/replay.rs`, `parity/*_tests.rs`, `reference.py`, `host_env.py`) was
read directly. Every finding below was re-verified by reading the cited lines
or rerunning the cited command; corrections are listed at the end.

## Verified strengths

- **Single-source pin.** `crates/vibe-core/src/parity/parity_tests.rs:71-191`
  fails on a third copy of the commit, a second Rust declaration, a Python
  redeclaration, or disagreement between the two sources.
- **Pinned-tree re-execution.** `scripts/parity/reference.py:171-221` extracts
  the pinned commit with `git archive`, re-runs the driver under the
  reference interpreter, and `_imports_pinned_vibe` (`:224-229`) refuses a
  run whose `vibe` did not come from that tree. Every in-process driver uses
  it, and no driver imports another.
- **Bidirectional ledger.** `Tally::conclude` (`replay.rs:632-668`) fails on
  an unexplained difference and on an entry that stopped reproducing; the
  v2.25.7 re-pin removed 30 entries this way. `Tally::compare` (`:603-627`)
  fails when the driver's scenario set or a scenario's specification drifted
  from the corpus.
- **Licensing by construction for prose.** Drivers reduce authored text to
  `{prose, sha256}` digests (`blackbox.py:459-461`, `system_prompt.py:448`,
  `acp.py:287`), `app_server_surface.py:863-876` refuses any captured string
  over 200 characters, and `voice.py:667-673` scans the serialized corpus for a
  credential sentinel before writing it.
- **Atomic last re-pin.** `d32d2086` regenerated 163 corpus files in one
  commit; no older pin hash survives in any corpus.
- **Honest history.** `docs/parity.md` records the score drops each re-pin
  caused (97.2 to 91.6, 87.8 to 63.7) instead of hiding them.
- **Counts that do match.** Where a row states a count, the spot checks agree:
  478 web-fetch pages (row 3), 146 tool-config pairs (row 4), 73 checkpoint
  engine scenarios (row 29), 165 TUI interaction observations (row 14), and
  every repository path the scorecard cites exists. The weakness (M4) is that
  nothing keeps them true, not that they are wrong today.
- **Strong individual oracles.** `tool_config` proves every (tool, key) pair
  has a live reader by writing and reading back a distinct value
  (`config_parity_tests.rs:281-313`); the config census is taken from the
  reference's own schema, so it covers every declared field by construction.

## Findings

| ID | Severity | Finding |
|---|---|---|
| C1 | Critical | Upstream v3.0.0 removed what most oracles measure |
| H1 | High | No live measurement runs anywhere, and drift is undetected |
| H2 | High | Corpus provenance is not enforced; green replays can prove nothing |
| H3 | High | Comparison blind spots behind every "100" |
| H4 | High | Reference text in comments, strings and corpora contradicts `NOTICE` |
| M1 | Medium | The reference environment is not attested |
| M2 | Medium | `off_pin_reason` fails open |
| M3 | Medium | The default backend is the least measured, and skipped on Windows |
| M4 | Medium | The scorecard is hand-asserted and partly stale |
| M5 | Medium | Re-pinning is not mechanized |
| M6 | Medium | Vacuity guards are weak |
| M7 | Medium | Coverage gaps at the pin |
| M8 | Medium | Test isolation leaks on Windows |
| M9 | Medium | Prose-directive coverage was audited at the previous pin |
| M10 | Medium | 66 000 lines of Python with no tooling, and their reference paths never run |
| L1 | Low | Architecture rules hold by convention only |
| L2 | Low | Inconsistencies and stale text |
| L3 | Low | CI retries can mask a flaky replay |
| L4 | Low | Linux CI does not run the documented test gate |

### C1. Upstream v3.0.0 removed what most oracles measure

- The reference's `CHANGELOG.md` at `c96a6db`, section 3.0.0 "Removed"
  (its "Removed" list), removes harness selection flags, the legacy
  execution runtime, three app-server methods, and the Python TUI with its
  `VIBE_CLI` switch; `vibe` now always starts the Rust TUI.
- `git grep -E 'legacy-harness|legacy_harness' c96a6db -- vibe harness`
  returns nothing, while `scripts/parity/pin.py:35` passes `--legacy-harness`
  to both sides of seventeen oracles.
- Gone at `c96a6db`: `vibe/cli/textual_ui/` (100 files at the pin),
  `vibe/core/agent_loop/`, `vibe/core/rewind/`, `vibe/cli/autocompletion/`,
  `vibe/core/llm/backend/`, `vibe/cli/commands.py`, `vibe/cli/programmatic.py`,
  `vibe/cli/entrypoint.py`, `vibe/core/middleware.py`.
- The `vibe` console script now maps to `vibe.cli.launcher:main`, which starts
  `vibe/cli-rust/` (907 `.rs` files). Its manifest changed from a proprietary
  license to Apache-2.0 in `a0c78d0a` (v3.0.0); `docs/parity.md:310-312` still
  describes it as proprietary.
- `SERVER_METHODS` grew from 146 to 174: 33 added, 5 removed, including the two
  worktree reap methods the v2.26.0 protocol pass had just measured
  (independent recount at both revisions).
- `exit_plan_mode` is no longer a builtin tool at `c96a6db`.
- Weight affected, from the coverage sub-audit's per-row check: rows whose
  reference module is deleted carry 22 of the 69 weight points; rows whose
  subject survives but whose oracle starts through the removed flag carry 33.

Why it matters: the 98.7 measures a product upstream stopped shipping. A
re-pin is not a recapture; it is a redesign of the oracles for a Unified-only,
Rust-fronted reference.

### H1. No live measurement runs anywhere, and drift is undetected

- `.github/workflows/ci.yml:3-7` triggers on push and pull request only; no
  workflow has a `schedule`, a self-hosted runner, or a reference checkout
  (`ci.yml:86`).
- The local checkout is at v3.1.0, so `off_pin_reason` skips every probe on
  this machine too.
- The skip is an `eprintln!` (`replay.rs:241-242`), which libtest captures in a
  passing test: a default run shows `ok` with no trace of the skip. AGENTS.md's
  rule about reporting runs that printed `skipping` relies on a message the
  default run hides.
- No workflow, script or test compares the pin with upstream tags.
- `docs/parity.md:16` still states that the score against upstream `HEAD`
  equals the pinned score, as measured on 2026-10-08.

### H2. Corpus provenance is not enforced; green replays can prove nothing

- `Replay::run` (`replay.rs:698-716`) never reads the `reference.commit` every
  corpus records. 27 corpora (plus one fixture file), replayed by 24 test
  files, have no commit check at all: every `Replay` user in `crates/vibe-app-server/tests/`, the ACP,
  programmatic, system-prompt and telemetry-session replays, the wire
  validation cases, `llm/backend_parity_tests.rs`, `plugins/plugins_parity_tests.rs`
  and `web_fetch/markdown/markdown_tests.rs`. Other replays assert it by hand
  (for example `compaction_parity_tests.rs:311-314`).
- The gap already caused an incident: `chat_input_parity_tests.rs:226-230`
  records a corpus that sat two releases behind the pin until a story
  recaptured it.
- Port mode overwrites the committed corpus by default: `review.py:709`,
  `sessions.py:897`, `hooks.py:879`, `agents.py:918`,
  `app_server_protocol.py:692`, `telemetry_session.py:617` and `acp.py:341-342`
  default `--output` to the committed path, and in port mode record
  `server-override`, `binary-override` or `agent-override` as the commit. One
  manual run without `--output` turns the corpus into a recording of the port,
  and the replay then compares the port with itself.
- `require_pin` accepts any commit when `--expected-commit ''` is passed
  (`reference.py:89`), and nothing downstream catches the result.
- The tool-surface oracle treats a stale corpus as a skip, not a failure
  (`tool_surface_parity_tests.rs:404-431`), and its main corpus is gitignored,
  so on CI its headline test returns early and passes (`:763-766`); only the
  committed digest test always runs.
- `web_fetch_markdown` has no `--check` and no live probe
  (`web_fetch_markdown.py:246-260`).

### H3. Comparison blind spots behind every "100"

- **Multi-word text.** `blackbox.py:459-461` digests any non-authored string
  that contains whitespace, and `Prose::OwnWords` treats any two digests as
  equal, length included (`replay.rs:86-88`, `:178-182`). Nine replays use it
  (acp, mcp_catalog, protocol, review, rewind, session, workspace_trust,
  programmatic, system_prompt). A wrong session title, tool output, error
  message or path with a space passes with zero ledgered differences.
- **System prompt.** Row 19 is "55/55 conformant" on structure; the words the
  model reads are not compared (`system_prompt_parity_tests.rs:513`).
- **Protocol values.** `app_server_protocol.py:364-373` reduces results to keys
  and types; lists of different lengths or orders compare equal.
- **Times and counters.** Every non-null value under a time key becomes
  `<time>` whatever its type (`app_server_client.py:368-369`); event counters
  collapse to `0` or `<nonzero>` (`:364-365`).
- **Whole subtrees ledgered.** `unified_harness_parity_tests.rs:121-156`
  excuses `/requests/` and `/steps/` for nine scenarios, and staleness needs
  only one difference per scenario, so these scenarios cannot detect any
  structural regression in their requests or steps.
- **Unbounded entries.** A ledger entry covers any number of differences; the
  printed count (`replay.rs:649-658`) is never asserted, so a regression
  inside a ledgered prefix is absorbed.
- **Prefix without a segment boundary.** `Divergence::covers` uses
  `starts_with` (`replay.rs:399`), as does `detail_presentation_parity_tests.rs:447`,
  so `/display/summary` also covers `/display/summaryDetail`.

### H4. Reference text in comments, strings and corpora contradicts `NOTICE`

`NOTICE` states that no upstream source is copied or translated. A scan of
8-word runs shared with the pinned reference (normalized; URL, regex and
config-file noise removed) found 432 runs: 152 in Rust comments, 183 in Rust
string literals, 55 in committed JSON, 25 in Markdown prompt assets.

- **Docstrings copied into doc comments.** 17 comment runs are 15 words or
  longer. `crates/vibe-core/src/worktree.rs:239-243` reproduces the class
  docstring of `vibe/core/git/worktree/repository.py:258-262`, and
  `crates/vibe-core/src/worktree/lifecycle.rs:34-35` the one of
  `vibe/core/session/worktrees.py:68-70`. `git log -S` dates both to the
  reference's `5e6aa0f6` (2026-08-18) and to this repository's `cee732b4`
  (2026-09-23). Others are close paraphrases with long shared runs
  (`shell/extract.rs:310`, `plugins/naming.rs:4`, `vibe-harness/src/rewind.rs:226`).
- **User-visible messages copied.** `crates/vibe-cli/src/tui/diagnostics.rs:85-96`
  (rate limit, context overflow with its recovery steps, refusal lead),
  `crates/vibe-cli/src/tui/command_handlers.rs:406` (a 56-word data notice),
  `crates/vibe-core/src/config/proxy.rs:53-57` (proxy variable descriptions),
  `crates/vibe-cli/src/update_notifier/gateway.rs:35`, and the corpora
  `runtime-parity/semantic-transcript.json:443-453` and
  `runtime-parity/terminal-services.json:915`.
- **The policy is inconsistent.** The accepted-divergence ledger rewrites
  other user-visible messages in original prose on licensing grounds
  (`docs/parity.md:353`, `:372`, `:415`, `:416`), while the messages above are
  copied. Nothing states where the line between "output contract" and
  "authored prose" falls.
- **No guard.** No test or CI step scans for overlap.

Apache-2.0 would permit these copies with attribution, so the issue is the
project's own clean-room claim and its consistency, not a license violation
per se.

### M1. The reference environment is not attested

- Black-box drivers run the checkout's console scripts, which import its
  working tree (`reference.py:10-11`); `require_pin` checks only
  `rev-parse HEAD` (`:88-89`), never `git status`. At least thirteen
  black-box drivers are affected.
- The checkout's `vibe` is an editable install
  (`.venv/Lib/site-packages/mistral_vibe-3.1.0.dist-info/direct_url.json`),
  built for 3.1.0. Restoring `HEAD` to the pin without `uv sync --frozen`
  would run the pinned code on v3.1.0 dependencies and native harness, and no
  check compares the environment with the pin's `uv.lock`.
- A probe that then fails says to regenerate the corpus (`replay.rs:316-322`),
  which would bake environment drift into the committed data.
- `oracle.py:49-56` reads the reference version from the working tree's
  `pyproject.toml`, not from the pinned tree.

### M2. `off_pin_reason` fails open

`crates/vibe-core/src/parity.rs:72-80`: when `git` cannot be spawned, or its
output is not UTF-8, the `?` operators return `None`, which means "this
checkout is the oracle", and the live probe runs against an unverified
checkout.

### M3. The default backend is the least measured, and skipped on Windows

- Only `unified_harness.py` (91 scenarios) and `connectors.py --harness
  unified` measure the backend upstream now ships exclusively; the editor and
  programmatic captures rerun with `--harness unified` add two more corpora.
- The whole 91-scenario replay is ignored on Windows
  (`unified_harness_parity_tests.rs:195-199`) although only its `mcp/*`
  scenarios need `#!` fixtures. At least eight parity replays are ignored on
  Windows by design (session, system prompt, plugins, agents, workspace trust,
  project links, unified harness, composer drag-and-drop), so on this
  workstation they never run; Linux CI runs them.
- Row 36 carries no conformant ratio, unlike its peers.

### M4. The scorecard is hand-asserted and partly stale

- `scorecard_tests.rs` recomputes only the weighted total. No test links a
  row's score or its "X/Y conformant" text to a replay's tally.
- Rows at 100 include 22 (51/104 conformant), 34 (21/33), 20 (29/38),
  4 (77/132 presentation cases) and 2 (751/780): 100 means every difference is
  ledgered, while the Method section defines 100 as the contract being "fully
  reproduced".
- 35 of 36 rows are at 100; only row 33 at 10 keeps the total below 100.
  The 3/2/1 weights are a stated judgment.
- `docs/parity.md:41-43` says scores come from inventories, not from running
  both implementations, which the "Measured by a differential oracle" rows
  contradict.
- Stale counts: 49 scripts and 57 replay modules (`docs/parity.md:71-85`)
  against 82 and 59 today.
- `ledger_tests.rs` resolves a symbol anywhere in the scanned sources, not in
  the cited file: `docs/parity.md:348` cites `is_prose` in
  `system_prompt_parity_tests.rs`, which contains no such symbol.

### M5. Re-pinning is not mechanized

- The procedure is one sentence (AGENTS.md, `parity.rs:25-27`). There is no
  manifest mapping corpus to driver, arguments, test and row, no run-all
  script, and no README under `scripts/parity/`.
- `Capture::command()` (`replay.rs:263-268`) prints a regeneration command
  without `--output` or the extra arguments the capture needs.
- AGENTS.md says a corpus changes only in a re-pin, yet nine commits after
  `d32d2086` (from `d234ed7c` to `f9a1f546`) edited corpora at the same pin.
  The rule is unusable as a staleness signal as written.

### M6. Vacuity guards are weak

- Floors sit far below the counts: telemetry 400 for 1 130, skills 120 for 364,
  checkpoints 40 for 73, web fetch `> 400` for 478 (the 78 hand-written
  fixtures could almost all disappear).
- `shell_policy`, `permission_surface` and `tool_config` have no floor; they
  check internal consistency only (`shell_parity_tests.rs:466`), so an empty
  extraction with a zero count passes.

### M7. Coverage gaps at the pin

- 85 reference files (about 6 050 lines) are claimed by no row, among them
  `vibe/cli/history_manager.py`, `vibe/core/loop.py`, `vibe/core/vision/`,
  `vibe/core/paths/`, `vibe/utils/keyring.py`, `vibe/core/tools/mcp_sampling.py`
  and `distribution/zed/`. Some are exercised indirectly (composer history
  traces), but no row answers for them.
- About fifteen routed methods have no behavioral scenario, only the
  shape-level census and probe (for example `shell/run`, `skills/list`,
  `session/ready/wait`, `mcp/login`).
- TUI parity compares state and layout projections, not the rendered screen
  (`scripts/parity/harness.py:555-587`); the out-of-tree oracle replaces
  geometry (`tui-interactions-oracle.py:285-287`). Colors, borders, scrolling
  and rich rendering are structurally outside the comparison.
- No row claims `vibe/cli-rust/`.

### M8. Test isolation leaks on Windows

- `discover_project_config` (`crates/vibe-core/src/config/harness.rs:62-76`)
  walks up from the working directory and stops only at the parent of
  `vibe_home`. With fixtures under `%TEMP%`, the walk reaches
  `C:\Users\<user>\.vibe\config.toml` and loads it as a project layer. The
  documented command line still fails
  `config::surface_parity_tests::every_model_scenario_validates_to_the_document_the_reference_validates`
  on this workstation.
- `host_env.isolated` disables the keyring through `DBUS_SESSION_BUS_ADDRESS`
  (`host_env.py:84`), which has no effect on the Windows Credential Manager;
  `app_server_protocol.py:276-334` writes to and cleans up the real vault under
  the product's own service name, so a killed run leaves an entry behind.

### M9. Prose-directive coverage was audited at the previous pin

`skills_parity_tests.rs:80-100` states that the original skill bodies cover the
directives the reference carried at `4a96003`. Between `4a96003` and the pin,
`vibe/core/skills/builtins/vibe.py` changed by +137/-59 lines. The entries only
assert that the digests differ, so new directives cannot be detected.

### M10. 66 000 lines of Python with no tooling

No `pyproject.toml`, ruff, mypy or pytest configuration is tracked, and CI
runs none. Drivers run only in port mode from Rust tests; their
reference-side `--check` and recapture paths never execute in CI and can rot
until the next re-pin.

### L1. Architecture rules hold by convention only

- No test checks the driver import rule or the dependency layers. Three tables
  import shared modules (`acp_scenarios.py:15`, `telemetry_session_scenarios.py:13`,
  `unified_harness_scenarios.py:15`), so they are not "pure data".
- AGENTS.md's layer list omits `vibe-e2e`, which `Cargo.toml:95` places in
  layer 1.

### L2. Inconsistencies and stale text

- Four hand-rolled ledger styles in `vibe-core` replays, none using `Tally`.
- `--allow-unpinned` in five drivers, `--expected-commit` elsewhere.
- Two digest shapes: `replay::digest` emits `{length, sha256}`
  (`replay.rs:226-231`), drivers emit `{prose, sha256}`.
- `tool_config.py:122` selects the shell branch from the host OS, so its live
  probe would fail on a pinned Windows checkout against the POSIX corpus.
- `RESTORE_COMMAND` hardcodes the Linux path and ignores `VIBE_REFERENCE`.
- Stale text: `compaction_parity_tests.rs:63` (a fourth entry that does not
  exist), `crates/vibe-cli/tests/parity/README.md:51` (render "deferred",
  yet 101 expectations declare it at parity), `shell_parity_tests.rs:58`.
- Fixed recapture paths under `target/` collide between concurrent runs
  (`compaction_parity_tests.rs:964`, `experiments_parity_tests.rs:2104`).

### L3. CI retries can mask a flaky replay

`.config/nextest.toml:27-30` reruns a failing test twice in the `ci` profile;
only e2e trials are exempt. A replay that fails intermittently is reported as
flaky in the log, not as a failure.

### L4. Linux CI does not run the documented test gate

`ci.yml:52` runs `cargo +stable test --workspace --all-features`, without the
`--no-fail-fast`, `VIBE_HOME` and `env -u FORCE_COLOR` the AGENTS.md gate
prescribes. Harmless on a clean runner, but the sequence CI runs is not the
one the repository documents.

## Corrections to sub-audit claims

- "Every core-domain replay except `web_fetch_markdown` asserts the corpus
  commit": refuted; `llm/backend_parity_tests.rs` and
  `plugins/plugins_parity_tests.rs` do not either.
- "`scorecard_rows` is used only by the CLI-surface and ACP tests": corrected;
  it is used by the ACP, hooks, project-links and session replays.
- "21 black-box replay tests lack a commit check": recounted as 24 test files
  over 27 corpora.

## Recommendations

Ordered by leverage. The first decides whether the rest is worth doing on the
current design.

1. **Decide the v3 strategy before re-pinning.** Retire the legacy rows,
   rebuild the server oracles on the Unified Harness only, and measure the
   terminal client black-box through a pseudoconsole, since its reference is
   now a Rust binary. Keep the clean-room stance explicit now that the
   upstream Rust is Apache-2.0.
2. **Provenance by construction.** Commit a parity manifest (corpus path,
   driver, arguments, test, row, commit, corpus SHA-256) and one workspace test
   that fails when a tracked corpus is missing from it, names another commit,
   or changed without its hash. Make `Replay::run` assert the commit, turn the
   tool-surface skip into a failure, and have drivers refuse to write a
   manifest path in port mode.
3. **An upstream sentinel.** A scheduled workflow that lists upstream tags and
   runs a dependency-free census at the newest one (methods, config keys,
   flags, tools, deleted modules), then writes the "against upstream `HEAD`"
   column itself and fails when the gap grows.
4. **Measure the oracle, not only the port.** Mutation-test each replay:
   perturb the port capture (drop a key, change a scalar, reorder a list,
   change a string's length) and require every mutation class to fail. A
   per-row sensitivity matrix would turn H3 from an argument into a number.
5. **Typed prose comparison.** Replace blanket `OwnWords` with a per-pointer
   prose ledger, compare lengths within a stated tolerance, and check
   directive coverage in the live probe against a checklist kept under
   `.parity/`, committing only verdicts.
6. **Bounded ledgers.** Each entry records how many differences it covers;
   growth fails.
7. **Attest the environment.** Refuse a dirty checkout, compare the
   environment with the pin's `uv.lock`, record the fingerprint in each
   corpus, and make a failing probe say whether behavior or environment drifted.
8. **Derive the scorecard.** Replays write machine-readable tallies; a test
   compares each row's ratio with them; a score derives from conformance and
   ledger category instead of being typed.
9. **A licensing guard.** Commit SHA-256 hashes of 8-word shingles of the
   pinned reference (hashes only, as `NOTICE` allows) and fail CI on overlap
   outside a written, ledgered output-contract exception.
10. **Loud skips.** A `VIBE_PARITY_REQUIRE_LIVE=1` mode that fails on any skip,
    and a skip count in the CI summary.
11. **Python tooling.** ruff and a type checker on `scripts/parity/`, an AST
    test for the import rules, and a layer test built from `cargo metadata`.
