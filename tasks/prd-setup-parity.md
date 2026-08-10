[PRD]
# PRD: Setup, Onboarding and Authentication Parity

## Changelog

| Version | Date | Author | Summary |
|---------|------|--------|---------|
| 1.0 | 2026-08-10 | Arthur Jean | Initial PRD from the measured audit of `vibe/setup/` against the Python reference at commit `b78b451`: of the 3 554 lines the subtree holds, the 2 898 in `auth/` and `onboarding/` have no counterpart here. There is no browser sign-in although the two configuration keys that address it already ship declared, no credential provenance and therefore no sign-out, a keyring service name that no other build of this product can read, a setup flow rendered as chat transcript prompts that asks three questions the reference never asks and persists none of the trust decision it collects, one ACP authentication method where the reference declares up to three, and no oracle measuring any of it |

## Problem Statement

1. **There is no browser sign-in, and the configuration already claims otherwise.** The reference implements a PKCE `S256` flow over three endpoints, `POST {api_base}/vibe/sign-in`, `GET {poll_url}` and `POST {api_base}/vibe/sign-in/{process_id}/exchange` ([http_browser_sign_in_gateway.py](/home/arthur/dev/mistral-vibe/vibe/setup/auth/http_browser_sign_in_gateway.py)), driven by a four-status service with an eleven-value error taxonomy ([browser_sign_in.py:92](/home/arthur/dev/mistral-vibe/vibe/setup/auth/browser_sign_in.py), [browser_sign_in_gateway.py](/home/arthur/dev/mistral-vibe/vibe/setup/auth/browser_sign_in_gateway.py)). Nothing in `crates/` corresponds. Meanwhile `browser_auth_base_url` and `browser_auth_api_base_url` are declared in the schema with the reference defaults and published to every client (`crates/vibe-core/src/config/registry.rs:259-260,410-411`), and a workspace-wide grep finds exactly those four occurrences and no reader. `docs/parity.md:78` states the rule this violates in its own configuration row: declaring a key is not implementing its feature. An operator reading `config/fields/read` is told this build can sign in through a browser against a configurable console, and it cannot.

2. **Nothing knows where the credential came from, so there is no sign-out.** The reference classifies provenance into six states through `assess_auth_state` ([auth_state.py:95](/home/arthur/dev/mistral-vibe/vibe/setup/auth/auth_state.py)): `signed_out`, `auth_not_required`, `os_keyring`, `vibe_home_env_file`, `process_env` and `unsupported_provider`, resolved by a five-level precedence in which the global dotenv beats the keyring because it is injected into the process environment before the keyring is read. Two booleans hang off that classification, `can_use_active_provider` and `sign_out_available`, and the second gates the only sign-out path in the product. This port decides the same question inline and binary, credential found or not (`crates/vibe-cli/src/tui/mod.rs:216-234`), so it cannot answer where a key lives, cannot refuse to revoke a key it does not own, and offers no sign-out at all: `remove_api_key` ([api_key_persistence.py](/home/arthur/dev/mistral-vibe/vibe/setup/auth/api_key_persistence.py)) has no counterpart in `crates/`.

3. **The keyring service name is wrong, and it is already written on users' machines.** The reference stores under service `ai.mistral.vibe` with account equal to the provider's key variable, migrating any entry found under the legacy service `vibe` on read ([vibe/utils/keyring.py](/home/arthur/dev/mistral-vibe/vibe/utils/keyring.py)). This port stores under `mistral-vibe-rs` (`crates/vibe-cli/src/tui/mod.rs:215`). A credential saved by one implementation is invisible to the other in both directions. This is persisted state on the operator's machine, which is the exact argument that placed tool names at rank 1 of the execution order: the migration cost grows with every installation, and nothing detects the drift today.

4. **A keyring failure loses the credential instead of falling back.** The reference writes the process environment, then the keyring, and on `KeyringError` falls back to `set_key` in the global dotenv at `$VIBE_HOME/.env`, deleting the stale plaintext copy when the keyring later succeeds ([api_key_persistence.py:70-108](/home/arthur/dev/mistral-vibe/vibe/setup/auth/api_key_persistence.py)). This port collapses every non-`NoEntry` keyring outcome into a single `CredentialError::Unavailable` (`crates/vibe-cli/src/tui/setup.rs:152-156`) and answers a failure with a diagnostic telling the operator to restart with `--setup`, which will fail the same way. On a headless Linux host with no Secret Service running, which is the default backend the `keyring` v4 `v1` feature selects, the product is unusable and the key the operator just typed is discarded. The reference reads the same host successfully.

5. **The setup flow is a chat transcript, not a screen sequence, and it asks three questions the reference never asks.** `SetupFlow` walks `Provider`, `Authentication`, `WorkspaceTrust`, `Network`, `Model`, `Preferences` (`crates/vibe-cli/src/tui/setup.rs:195-204`), rendered as prompts inside the chat transcript and answered through the composer. The reference installs seven screens, three unconditionally and four gated on `supports_browser_sign_in`, navigating by `switch_screen` with no stack, and returns one of five values that `run_onboarding` maps onto three distinct exit paths ([onboarding/__init__.py:80-113,149-244](/home/arthur/dev/mistral-vibe/vibe/setup/onboarding/__init__.py)). It never asks for a network proxy, never asks for a model, and never asks about workspace trust. Three of the six steps here have no reference counterpart, and four reference screens have no counterpart here: authentication method, sign-in target, custom domain and browser sign-in.

6. **The setup flow's trust step persists nothing, and `--setup` skips the dialog that would.** `SetupStep::WorkspaceTrust` collects a decision that reaches `SetupResources::workspace_trusted` and never reaches `trusted_folders.toml`; the file is only written by `StartupHost::decide_workspace_trust` (`crates/vibe-app-server/src/startup.rs:75`), which the setup path does not call. `resolve_workspace_trust` returns early whenever `arguments.setup` is set (`crates/vibe-cli/src/tui/startup/trust.rs:28`). An operator who answers the trust question during setup is asked again on the next launch, and the answer they gave is silently dropped.

7. **The ACP binary declares one authentication method where the reference declares up to three.** `Agent::authenticate` accepts the single id `environment` and rejects everything else (`crates/vibe-acp/src/agent.rs:151,173`). The reference publishes `browser-auth`, adds `browser-auth-delegated` when the client advertises the capability, and adds a terminal method with id `vibe-setup` that relaunches this binary with `--setup` when the client advertises `terminal-auth`; it also serves the extension methods `auth/status` and `auth/signOut` and suppresses the method list entirely for a JetBrains client that can already use the active provider ([acp/auth.py](/home/arthur/dev/mistral-vibe/vibe/acp/auth.py), [acp/agent.py:274-344,837-849](/home/arthur/dev/mistral-vibe/vibe/acp/agent.py)). An editor integration written against the reference finds no way to authenticate through this port other than an environment variable the editor may not control.

8. **`account/read` can never answer `unauthorized`.** The reference reaches `GET {console_base_url}/api/vibe/whoami` with the bearer credential, maps the answer onto a plan vocabulary and derives teleport eligibility from it ([app_server/_account.py:174-247](/home/arthur/dev/mistral-vibe/vibe/app_server/_account.py)). This port classifies three of the four statuses locally and documents that the fourth is a console verdict it has no client for (`crates/vibe-app-server/src/release3.rs:442-452`). A key that is present but revoked reads as `ready` here and as `unauthorized` upstream, and `/whoami` shows a plan of `null` where the reference shows the operator's plan.

9. **Nothing measures any of it.** Ten differential oracles live in `scripts/parity/`, covering the tool surface, tool execution, tool configuration, the configuration surface, the app-server wire surface, checkpoints and their opcodes, compaction, skills, the shell policy and the permission surface. None covers setup, onboarding or authentication. `docs/parity.md:79` scores the part 35 by reading module presence. The reference carries 75 test functions over `vibe/setup/auth/` alone (13 in `tests/setup/auth/test_auth_state.py`, 18 in `test_api_key_persistence.py`, 15 in `tests/browser_sign_in/test_browser_sign_in.py`, 29 in `test_browser_sign_in_http.py`) and at least 46 more driving the onboarding app end to end, and `cargo test --workspace --all-features` passes green against a flow that shares none of that behavior.

**Why now:** `docs/parity.md` places browser sign-in and onboarding at rank 13, and everything above it is `DONE` or `PARTIAL` with its blocking work shipped. Two of the nine defects above have a cost of deferral that compounds and the rest do not. Defect 3 is persisted state: every week the keyring service name stays divergent, more operators accumulate a credential that only one of the two implementations can read, and the eventual migration has to cover them all. Defect 1 is a published contract: the two browser-auth keys are already in the schema every client reads, so the longer they ship without a consumer, the more configuration files exist on disk carrying a custom console URL this build silently ignores. Both are the same argument that put tool names at rank 1, applied to state the operator owns rather than to identifiers this port emits.

## Overview

This initiative makes setup, onboarding and authentication behaviorally equivalent to the reference at every boundary an operator, an editor or a stored file can observe: which credential sources are consulted and in what order, where a credential is written and what happens when that write fails, what a browser sign-in requests and how it recovers, which screens exist and what each one persists, and what the ACP protocol says about all of it. Equivalence is defined mechanically: for a given configuration document, environment, credential store state and key sequence, this port must reach the same auth state, issue the same requests, install the same screens, persist the same values and terminate with the same outcome.

Sequencing puts the instrument first, following this repository's own record: every part measured by an oracle scores 92 or above, and every part measured by module presence sits between 25 and 85. The first epic builds two capture scripts and their corpora. The reference makes this affordable in both halves. `BrowserSignInService` takes its gateway, its browser opener, its sleep function and its clock as constructor arguments, so a capture drives the whole state machine with no network and no browser, exactly as the compaction oracle drives `CompactionManager` over a stubbed completion. `OnboardingApp` is a Textual `App`, so a capture drives it through the same headless pilot that `scripts/parity/oracle.py` already uses on the chat input.

The second and third epics port the logic, which is where the measurable surface is densest and the licensing constraint lightest: the auth-state precedence, the credential persistence and its fallback, the keyring rename with its migration, and the sign-in protocol with its URL validation and its error taxonomy. The fourth epic replaces the chat-transcript setup with the reference's screen graph, retiring the three steps that have no counterpart and moving the trust decision back to the pre-session dialog that already persists it. The fifth publishes the ACP authentication surface and the only sign-out path the product has. The sixth records what cannot be ported and remeasures the scorecard.

Two boundaries are decided in advance rather than discovered during implementation. `NOTICE` forbids shipping the reference's authored prose, and this subtree is dense with it: eight sign-in error sentences, the welcome text, every screen label, hint and subtitle, and the ACP method descriptions. Following the precedent set by the builtin skills and the compaction envelope, each run is reproduced originally against the same directive coverage and recorded in the corpus as a length plus a SHA-256, so structure is compared for equality and prose for permanent inequality. Separately, Textual and ratatui cannot render the same cells, so the onboarding oracle captures normalized observations rather than output: the installed screen set, the transition taken, the focus target, the validation class, the effects persisted and the terminating value. The precedent is `tasks/prd-chat-input-observable-parity.md`.

The reference is a read-only checkout pinned for this PRD at commit `b78b451c39eab9213393ad2f45908e8562a5c5e7` (v2.24.0), which every measurement in this document was taken from. This PRD does **not** re-pin: `vibe_core::parity::REFERENCE_COMMIT` and `EXPECTED_COMMIT` in `scripts/parity/pin.py` already name it. Its location is machine-dependent, `C:\dev\mistral-vibe` on Windows and `/home/arthur/dev/mistral-vibe` on Linux; reference links below use the Linux form and resolve against whichever checkout is local, through `VIBE_REFERENCE` or `--reference`, and Rust tests reach it through `vibe_core::parity::reference_root`.

## Goals

| Goal | Month-1 Target | Month-6 Target |
|------|---------------|----------------|
| Classify credential provenance | 6 of 6 auth states reached with the reference's 5-level precedence, 0 divergent over the captured matrix | 0 maintained |
| Speak the sign-in protocol | 3 of 3 endpoints, 4 of 4 statuses and 11 of 11 error codes reproduced, 0 invented codes | 0 maintained |
| Validate every server-supplied URL | 29 of 29 reference validation cases replayed with the reference verdict, 0 wrongly accepted | 0 maintained |
| Stop losing credentials | A keyring failure falls back to `$VIBE_HOME/.env` in 100% of cases, 0 credentials discarded | 0 maintained |
| Share the credential with the reference | Keyring service `ai.mistral.vibe`, legacy `vibe` and prior-build `mistral-vibe-rs` all readable, 0 operators required to re-enter a key | Legacy read paths retired only after a release that migrates on read |
| Install the reference's screens | 7 of 7 screens with the 4 conditional ones gated on the reference predicate, 5 of 5 terminating values mapped to the reference exit paths | 0 maintained |
| Stop asking questions the reference does not ask | 0 setup steps without a reference counterpart, trust persisted through the existing dialog | 0 maintained |
| Publish the ACP auth surface | 3 of 3 authentication methods declared under their reference gates, 2 of 2 extension methods served | 0 maintained |
| Consume the declared configuration | 2 of 2 browser-auth keys read by a real code path, each proven by a test that changes the key and observes the request change | 0 declared-only auth keys |
| Make conformance mechanically enforced | Corpus replays at least 180 scenarios across 8 families and fails on any divergence outside a named ledger | Ledger holds only `NOTICE` entries and the two recorded form divergences |
| Raise the measured score | `docs/parity.md` Setup, onboarding, authentication from 35 to 100, measured by the new oracle | Weighted total restated with the rows this work touches |

## Target Users

### Developer running the binary for the first time

- **Role:** Engineer who installed the CLI and launched it in a project directory with no credential configured.
- **Behaviors:** Expects the tool to take them to a working state without leaving the terminal; reaches for a browser sign-in if offered, and for a pasted key if not.
- **Pain points:** The current flow asks six questions in a chat transcript, three of which (proxy, TLS certificate path, model) they have no opinion about on first run, and it has no browser path at all, so the only way through is to leave the terminal, find the console, generate a key and paste it back.
- **Current workaround:** Generate an API key by hand in the web console before running the tool at all.
- **Success looks like:** One keypress opens the browser, the terminal shows the three steps advancing, and the session starts signed in.

### Operator on a private or self-hosted Mistral deployment

- **Role:** Engineer whose organization runs a console on its own domain rather than on the public one.
- **Behaviors:** Sets a custom base URL in configuration and expects every authenticated call to follow it.
- **Pain points:** `browser_auth_base_url` and `browser_auth_api_base_url` are published in the schema and honored by nothing, so a configuration file that points at the internal console is accepted and ignored. There is no screen to enter a domain and no validation to catch a typo in one.
- **Current workaround:** None inside the product; the credential has to be provisioned entirely outside it.
- **Success looks like:** A domain typed once is validated, derived into its base and API forms, used by every sign-in request, and persisted to the provider entry.

### Editor integration author speaking ACP

- **Role:** Author of an IDE extension driving the `vibe-acp` binary over the editor protocol.
- **Behaviors:** Reads `authMethods` at `initialize`, calls `authenticate` with the id it chose, and expects to be able to show the user their sign-in state and offer a sign-out.
- **Pain points:** The only advertised method is `environment`, so an editor that cannot set process environment variables has no path. There is no `auth/status` to render and no `auth/signOut` to offer. The delegated variant, where the editor owns the browser and reports back, does not exist.
- **Current workaround:** Instruct the user to configure a shell environment variable outside the editor.
- **Success looks like:** The advertised method set matches what the reference advertises under the same client capabilities, and both extension methods answer.

### Operator on a headless host or in CI

- **Role:** Engineer running the binary over SSH, in a container, or in a pipeline, with no browser and often no Secret Service.
- **Behaviors:** Provides the credential through the environment or a dotenv file and never expects an interactive prompt.
- **Pain points:** A keyring write failure is fatal rather than falling back to a file, and the advice given is to rerun the command that just failed. If a browser sign-in were attempted, there is no code path that prints the URL instead of opening it.
- **Current workaround:** Export the environment variable and avoid `--setup` entirely.
- **Success looks like:** The credential resolves from the environment or the dotenv without a prompt, a keyring that cannot be reached degrades to a file, and a browser that cannot open reports the specific failure and shows the URL.

## Research Findings

Key findings that informed this PRD.

### Competitive Context

- **GitHub CLI (`gh auth login`):** browser flow by default with a one-time code, `--with-token` on stdin as the headless fallback, `--hostname` for Enterprise, keyring storage introduced opt-in in 2.24 and later made the default with an `--insecure-storage` escape. Its 2023 storage migration broke extensions that read the credential, which is direct evidence for treating a keyring service rename as a compatibility event rather than a rename.
- **Vercel CLI:** moved to the RFC 8628 Device Authorization Grant in September 2025, retiring both its email login and its out-of-band flow.
- **Stripe CLI:** a proprietary pairing-code flow with polling, structurally the same shape as the reference's sign-in process.
- **gcloud and wrangler:** loopback redirect on `127.0.0.1`.
- **Market gap:** none of these offers a first-class custom-domain path in the sign-in flow itself; the reference does, through its sign-in target and custom domain screens, and reproducing that is a differentiator this port currently forfeits.

### Best Practices Applied

- RFC 8252 §6 and §8.12: the system browser, never an embedded user agent, and PKCE for public clients. The reference complies and this port must.
- RFC 7636 §4.1, §4.2 and §7.1: `code_verifier` drawn from the unreserved charset, at least 256 bits of entropy, challenge as base64url of the SHA-256 of the ASCII verifier with no padding. The reference uses `secrets.token_urlsafe(64)`, which is 512 bits, comfortably above the floor.
- RFC 9700 §2.1 and RFC 8628 §5.4: polled flows are phishable at a distance in a way loopback flows are not, and exact matching of URLs is the mitigation available to a client. No published BCP covers a client validating URLs its own server handed it, which is precisely what the reference does through origin and path-prefix validation on both the sign-in URL and every poll; this PRD keeps that behavior and treats it as the security property it is.
- Where RFC 8628 §3.5 recommends a 5 second poll interval with a `slow_down` escalation, the reference polls at 3.0 seconds with no backoff and bounds the loop by the server's `expires_at`. Parity wins over the recommendation because a different interval issues a different number of requests over the same window, which is a measurable divergence; the deviation is recorded rather than silently inherited.

*Full research sources available in project documentation.*

## Assumptions & Constraints

### Assumptions (to validate)

- The reference's sign-in endpoints are reachable only with a real console account, so no acceptance criterion in this PRD depends on a live call; every protocol assertion is made against a recording transport or a stub gateway. Validated by US-181 producing a corpus with zero network access.
- `keyring` v4.1.5 with default features selects Secret Service through zbus on Linux, so a host with no D-Bus secret service has no backend at all and the dotenv fallback is the only path. The exact error variant surfaced in that case is `NoDefaultStore` rather than `NoEntry`, but this is drawn from repository autodocs rather than from the released crate. Validated by US-184's acceptance criterion asserting the observed variant on a host with the service stopped.
- No published prior art exists for migrating a credential between keyring service names, so the read-new, fall-back-legacy, rewrite-new, delete-legacy pattern is inferred rather than cited. Validated by US-184's migration test.
- The reference's onboarding is reachable from the CLI only through `--setup` or a `MissingAPIKeyError` in interactive mode, so no other entry point needs a counterpart. Validated by the screen-graph corpus in US-182.

### Hard Constraints

- `NOTICE` forbids copying, translating or vendoring reference source, prompt files or tool description text. Every user-facing sentence in this subtree is written originally and recorded in the corpus as a length plus a SHA-256.
- The reference checkout is read-only and pinned at `b78b451`. This PRD does not re-pin, and no capture may write to the checkout.
- Rust parity tests replay committed corpora unconditionally and skip only the live probe when the checkout is absent or off-pin. A missing checkout must never fail `cargo test`.
- The dependency layering in `[workspace.metadata.vibe]` holds: the sign-in service, the auth state and the credential store belong to `vibe-core`; `vibe-cli` and `vibe-acp` are adapters over it.
- `unsafe_code` is forbidden workspace-wide and `panic`, `unimplemented` and `dbg_macro` are denied in non-test code.
- A credential written by any previously released build of this port must remain readable after the service rename, without operator action.

## Quality Gates

These commands must pass for every user story, run from the workspace root:

- `cargo fmt --all -- --check` - formatting
- `cargo check --workspace --all-targets --all-features` - compilation across every target and feature
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` - lints as errors
- `cargo test --workspace --all-features` - the whole suite, never filtered to the edited module

For stories that change a terminal view, additionally:

- Drive the flow under the PTY harness and confirm the observation trace, not the rendered cells, matches the committed corpus.

## Epics & User Stories

### EP-052: The Setup Oracle and Its Corpus

Build the instrument before the code. Two capture scripts drive the reference's authentication logic and its onboarding app, write a committed corpus of normalized observations, and a Rust replay fails on any divergence outside a named ledger.

**Definition of Done:** Both capture scripts run against the pinned checkout without network access, the corpus is committed with no reference-authored prose, and `cargo test --workspace --all-features` replays every family unconditionally with a ledger that fails on both new and stale divergences.

#### US-180: Capture the authentication surface
**Description:** As a parity engineer, I want a capture script that records the reference's auth-state precedence, credential persistence outcomes and sign-in protocol so that every later claim about this subtree is measured rather than asserted.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** None

**Acceptance Criteria:**
- [ ] Given the pinned checkout, when `scripts/parity/setup_auth.py` runs, then it writes `crates/vibe-core/tests/setup-auth/corpus.json` with families `authState`, `persistence`, `signInProtocol`, `urlValidation` and `errorTaxonomy`
- [ ] Given the `authState` family, when it is captured, then it covers every combination of the five sources the reference consults and records the resulting state, `can_use_active_provider` and `sign_out_available` for each
- [ ] Given the `signInProtocol` family, when it is captured, then it drives `BrowserSignInService` through a stub gateway, a stub clock and a stub browser opener, and records the ordered event sequence, the poll count and the terminal outcome per scenario
- [ ] Given any captured string that is authored prose, when it is written to the corpus, then it appears as a length plus a SHA-256 and never as text
- [ ] Given a run with no reference checkout present, when the script is invoked, then it exits with a named error and writes no partial corpus
- [ ] Given a run of any family, when it completes, then zero network sockets were opened, asserted by the script itself

#### US-181: Replay the authentication surface in Rust
**Description:** As a parity engineer, I want the committed authentication corpus replayed against this build so that a regression in provenance, persistence or protocol fails the suite.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-180

**Acceptance Criteria:**
- [ ] Given the committed corpus, when `cargo test -p vibe-core --all-features setup_auth_parity_tests -- --nocapture` runs, then it prints per-family conforming counts and the ledger
- [ ] Given a divergence outside the ledger, when the replay runs, then the test fails naming the family, the scenario and the field
- [ ] Given a ledger entry whose divergence no longer reproduces, when the replay runs, then the test fails as a stale entry
- [ ] Given an absent or off-pin reference checkout, when the suite runs, then the replay still executes from the committed corpus and only the recapture probe skips
- [ ] Given a corpus scenario with no matching Rust assertion, when the replay runs, then the test fails rather than silently skipping the scenario

#### US-182: Capture and replay the onboarding screen graph
**Description:** As a parity engineer, I want the reference onboarding app driven through Textual's headless pilot so that the screen graph, the conditional installation and the terminating values are measured as observations rather than as rendered cells.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-180

**Acceptance Criteria:**
- [ ] Given the pinned checkout, when `scripts/parity/onboarding.py` runs, then it writes `crates/vibe-cli/tests/onboarding/corpus.json` recording, per scenario, the installed screen set, the ordered transitions taken, the focus target per screen, the validation class per input state, the effects persisted and the terminating value
- [ ] Given a provider whose `supports_browser_sign_in` predicate is false, when the graph is captured, then the corpus records three installed screens and a direct edge from theme selection to the key screen
- [ ] Given each of the five terminating values the reference can return, when they are captured, then the corpus records the exit path the caller takes for each
- [ ] Given rendered output of any kind, when the corpus is written, then no cell content, SVG or styled text appears in it
- [ ] Given the committed corpus, when the Rust replay runs, then it asserts the graph, the gating predicate and the terminating map, and fails on any divergence outside the ledger
- [ ] Given a screen the reference installs that this build does not yet implement, when the replay runs before EP-055 lands, then the scenario is recorded as a ledgered gap naming the story that closes it, and the ledger entry fails once the screen exists

---

### EP-053: The Auth State and the Credential

Port the provenance classification and the credential lifecycle, including the keyring service rename and its migration, and the dotenv fallback that keeps a credential when the keyring cannot take it.

**Definition of Done:** All six auth states are reachable with the reference precedence, a credential written by any prior build of this port still resolves, a keyring failure never discards a key, and every outcome string matches the reference vocabulary.

#### US-183: Classify credential provenance
**Description:** As an operator, I want the product to know where my credential came from so that it can tell me my sign-in state and refuse to revoke a key it does not own.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-181

**Acceptance Criteria:**
- [ ] Given a provider whose key variable is empty, when the state is assessed, then it is `auth_not_required` with `can_use_active_provider` true
- [ ] Given no credential in any source, when the state is assessed, then it is `signed_out` with `can_use_active_provider` false and the key variable reported
- [ ] Given a credential present and a key variable other than the default Mistral one, when the state is assessed, then it is `unsupported_provider` with `sign_out_available` false
- [ ] Given a credential in both the global dotenv and the keyring, when the state is assessed, then it is `vibe_home_env_file`, reproducing the reference precedence in which the dotenv is injected before the keyring is read
- [ ] Given a credential that was present in the process environment before the dotenv was loaded, when the state is assessed, then it is `process_env` with `sign_out_available` false
- [ ] Given a source holding an empty string, when the state is assessed, then that source is treated as absent
- [ ] Given a source that cannot be read at all, such as an unreadable dotenv or an unavailable keyring, when the state is assessed, then the assessment completes on the remaining sources rather than failing

#### US-184: Rename the keyring service and migrate on read
**Description:** As an operator who already signed in with an earlier build, I want my stored credential to keep working after the service name changes so that an upgrade never asks me to sign in again.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-181

**Acceptance Criteria:**
- [ ] Given a credential stored under `ai.mistral.vibe`, when it is read, then it resolves without consulting any other service name
- [ ] Given a credential stored only under the prior-build service `mistral-vibe-rs`, when it is read, then it resolves, is rewritten under `ai.mistral.vibe`, and the prior-build entry is deleted
- [ ] Given a credential stored only under the reference legacy service `vibe`, when it is read, then it resolves and migrates the same way
- [ ] Given a credential written by this build, when the reference reads the same host, then it finds it under the service and account the reference expects
- [ ] Given a host with no credential backend available, when a read is attempted, then the error distinguishes an absent backend from an absent entry, and neither is reported as the other
- [ ] Given a migration where the rewrite succeeds but the legacy delete fails, when the read completes, then the credential is still returned and the failed delete is recorded as a diagnostic rather than propagated

#### US-185: Persist and remove the credential with the reference's outcomes
**Description:** As an operator, I want my key saved even when the keyring is unavailable, and removable when it is mine, so that a storage failure never costs me the key I just entered.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-183, US-184

**Acceptance Criteria:**
- [ ] Given a successful save, when persistence completes, then the process environment carries the key, the keyring holds it, any stale dotenv copy is removed, and the outcome is the reference's completion value
- [ ] Given a keyring that rejects the write, when persistence runs, then the key is written to `$VIBE_HOME/.env` with parent directories created, and the outcome reports the fallback rather than a failure
- [ ] Given a provider whose key variable is empty, when persistence runs, then the outcome is the reference's empty-variable error value and nothing is written
- [ ] Given both the keyring and the dotenv fallback failing, when persistence runs, then the outcome carries the save error and no partial state is left behind
- [ ] Given a state where `sign_out_available` is false, when removal is requested, then it is refused with the reference's error rather than clearing anything
- [ ] Given removal from a state where it is available, when it runs, then both the current and the legacy keyring services are cleared, the dotenv entry is unset, the process environment variable is dropped, and a deletion failure on an entry that does not exist is a no-op rather than an error
- [ ] Given a removal where one keyring service raises a real backend error, when it runs, then the remaining sources are still cleared and the error is surfaced afterward

#### US-186: Persist the provider entry the reference persists
**Description:** As an operator who signed in against a custom console, I want the provider written to my configuration exactly as the reference writes it so that the same file works with either implementation.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-185

**Acceptance Criteria:**
- [ ] Given a provider modified during the flow, when it is persisted, then it is upserted into the `providers` array keyed by name, carrying only fields that differ from their defaults
- [ ] Given a provider identical to the one the flow started with, when the flow completes, then no configuration write occurs
- [ ] Given an existing provider entry with fields this port does not model, when the upsert runs, then those fields survive the write unchanged
- [ ] Given a configuration write that fails, when it is attempted, then the outcome reports the provider error and the credential already persisted is not rolled back
- [ ] Given a custom browser-auth base URL on the entry, when the upsert runs, then it is preserved rather than replaced by the shipped default

---

### EP-054: The Browser Sign-In Flow

Port the PKCE sign-in protocol, the URL validation that guards it, the polling state machine and its error taxonomy, and the browser opening with its no-browser path.

**Definition of Done:** All three endpoints are spoken with the reference's payloads, all 29 captured URL validation cases return the reference verdict, all 11 error codes are produced by their reference conditions, and no code path opens a URL that failed validation.

#### US-187: Speak the sign-in protocol
**Description:** As an operator, I want the product to create a sign-in process, poll it and exchange its token so that a browser sign-in can complete at all.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-181

**Acceptance Criteria:**
- [ ] Given a sign-in start, when the process is created, then a `POST` reaches the configured API base at the reference path carrying the challenge and the `S256` method name
- [ ] Given a `code_verifier`, when it is generated, then it is drawn from the unreserved character set, carries at least 256 bits of entropy, and its challenge is the unpadded base64url encoding of the SHA-256 of its ASCII bytes
- [ ] Given a creation response, when it is parsed, then the process id, sign-in URL, poll URL and expiry are read, with a trailing `Z` accepted on the timestamp and normalized to UTC
- [ ] Given a poll response with HTTP 410, when it is handled, then it is treated as an expiry rather than as a transport failure
- [ ] Given an exchange that returns success without a key, when it is handled, then the missing-key error code is produced rather than an empty credential
- [ ] Given any transport failure, non-success status or unparseable body on each of the three endpoints, when it occurs, then the endpoint's own error code is produced and no partially built credential is returned
- [ ] Given the two browser-auth configuration keys, when either is changed, then the requests observably follow it, proven by a test that asserts the changed host

#### US-188: Validate every server-supplied URL before using it
**Description:** As an operator, I want the sign-in and poll URLs checked against my configured console so that a compromised or misconfigured server cannot send my browser or my credential somewhere else.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-187

**Acceptance Criteria:**
- [ ] Given a sign-in URL whose origin differs from the configured browser base, when it is validated, then it is rejected with the start error code and the browser is never opened
- [ ] Given a poll URL whose origin differs from the configured API base, when it is validated on any poll including polls after the first, then it is rejected with the poll error code
- [ ] Given origins that differ only by an explicit default port, when they are compared, then they are treated as equal for both schemes
- [ ] Given a path containing dot segments or percent-encoded dot segments, when it is validated, then it is decoded and normalized before the prefix comparison
- [ ] Given a path that shares a textual prefix with the base but not a segment boundary, when it is validated, then it is rejected
- [ ] Given the 29 validation cases captured from the reference, when they are replayed, then 29 return the reference verdict
- [ ] Given any log line emitted during validation or during a request, when it is captured, then it contains no credential, no exchange token and no full URL, asserted by a test

#### US-189: Drive the sign-in state machine
**Description:** As an operator watching the terminal, I want the sign-in to advance through its steps, tolerate a transient failure and stop for a real one so that I always know whether to wait or to act.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-187

**Acceptance Criteria:**
- [ ] Given a full run, when it succeeds, then the attempt-started event precedes the four status changes in the reference order and the credential is returned only after the final status
- [ ] Given a poll loop, when it waits, then the interval is 3.0 seconds and the wait never exceeds the time remaining to the recorded expiry
- [ ] Given two consecutive poll failures followed by a success, when the loop runs, then the failure streak resets and the flow continues
- [ ] Given three consecutive poll failures, when the third occurs, then the flow stops with the poll error code
- [ ] Given a poll answering denied, expired or provider error, when it is handled, then the matching error code is produced immediately without further polling
- [ ] Given a status value outside the five the reference declares, when it is received, then the unknown-state error code is produced
- [ ] Given the recorded expiry passing with no terminal answer, when the loop exits, then the timeout error code is produced
- [ ] Given a cancellation while waiting, when it occurs, then the gateway is closed and no credential is persisted

#### US-190: Open the browser, and survive not being able to
**Description:** As an operator on a machine with no browser, I want the failure named and the URL shown so that I can finish signing in from another device.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-188, US-189

**Acceptance Criteria:**
- [ ] Given a validated sign-in URL, when the flow reaches the opening step, then the system browser is launched and the child process produces no output on the terminal this build is drawing to
- [ ] Given a host where no browser can be launched, when the open is attempted, then the open-browser error code is produced and the sign-in URL is displayed for manual use
- [ ] Given a URL that failed validation, when the flow reaches the opening step, then no browser is launched
- [ ] Given a browser launch that blocks, when it is invoked, then the terminal remains responsive and the poll loop still starts
- [ ] Given a successful open, when the flow continues, then the sign-in URL remains retrievable so the operator can reopen it without restarting the attempt

---

### EP-055: The Onboarding Screens

Replace the chat-transcript setup with the reference's screen graph, gated by the reference predicate, retiring the three steps that have no counterpart and returning the trust decision to the dialog that persists it.

**Definition of Done:** Seven screens exist under the reference gating, the five terminating values map onto the reference exit paths, no step without a reference counterpart remains in the flow, and the observation trace conforms to the committed corpus.

#### US-191: Install the screen graph and its outcomes
**Description:** As a first-run developer, I want the setup to be a sequence of screens with the same structure the reference has so that documentation and expectations transfer between the two implementations.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-182, US-185

**Acceptance Criteria:**
- [ ] Given a provider supporting browser sign-in, when the flow starts, then seven screens are installed and the entry screen is the welcome screen
- [ ] Given a provider not supporting it, when the flow starts, then three screens are installed and theme selection leads directly to the key screen
- [ ] Given any screen, when the operator advances, then the previous screen is replaced rather than stacked, so a back action returns to the screen the reference returns to and no deeper
- [ ] Given each of the five terminating values, when the flow ends with it, then the caller takes the reference's exit path for that value
- [ ] Given a cancellation on any screen, when it occurs, then nothing beyond the theme is persisted and the process exits successfully
- [ ] Given the previous flow's `Network`, `Model` and `WorkspaceTrust` steps, when the new flow runs, then none of them appears, the proxy and certificate settings remain reachable from their existing command, and workspace trust is decided by the pre-session dialog
- [ ] Given `--setup` on the command line, when it runs, then the trust dialog is no longer suppressed for a workspace whose trust is undecided

#### US-192: The welcome and theme screens
**Description:** As a first-run developer, I want to see the product identify itself and to pick a theme with a live preview so that the terminal looks the way I want before I do anything else.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-191

**Acceptance Criteria:**
- [ ] Given the welcome screen, when it mounts, then the text renders progressively and the advance action is inert until it finishes
- [ ] Given the highlighted span of the welcome text, when it is rendered, then it cycles through the reference's ten-entry color table by index rather than by interpolation
- [ ] Given the theme screen, when the selection moves, then the theme applies immediately to the preview and the surrounding screen
- [ ] Given the theme list, when it is navigated past either end, then it wraps, and the items at increasing distance from the selection carry the reference's three fade levels
- [ ] Given a terminal too short for the preview, when the screen renders, then the preview is clamped to the reference's minimum rather than overflowing or panicking
- [ ] Given a theme chosen and the flow later cancelled, when the process exits, then the theme is not persisted
- [ ] Given a theme chosen and the flow completed, when the process exits, then the theme is persisted once, after the screens have closed

#### US-193: The authentication method, sign-in target and custom domain screens
**Description:** As an operator on a private deployment, I want to choose how to authenticate and against which console so that a self-hosted domain is a first-class answer rather than a configuration file edit.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-191

**Acceptance Criteria:**
- [ ] Given the method screen, when a method is chosen, then the browser option leads to the target screen and the manual option leads to the key screen
- [ ] Given the target screen with no custom domain already configured, when the default target is confirmed, then the flow applies the shipped defaults and continues to sign-in
- [ ] Given the target screen with a custom domain already configured, when the default target is confirmed once, then a warning naming the configured domain is shown and a second confirmation is required to overwrite it
- [ ] Given that armed confirmation, when the selection moves or the screen is re-entered, then it disarms
- [ ] Given the domain input, when a value is typed, then it is validated live and the input carries the reference's valid, invalid and warning classes, including the private-cloud heuristic that warns without blocking
- [ ] Given an invalid domain, when it is submitted, then the flow stays on the screen and the failure reason is shown
- [ ] Given a valid domain, when it is submitted, then the base and API URLs are derived from it and used by the sign-in that follows
- [ ] Given a back action on either screen, when it is taken, then it returns to the reference's predecessor rather than cancelling

#### US-194: The browser sign-in screen
**Description:** As a first-run developer, I want to watch the sign-in advance and to have a way out when it stalls so that I am never stuck on a screen with nothing to do.

**Priority:** P0
**Size:** L (5 pts)
**Dependencies:** Blocked by US-190, US-193

**Acceptance Criteria:**
- [ ] Given the screen mounting, when the attempt starts, then three steps are shown and the current one advances as the service emits its statuses
- [ ] Given an attempt that has been running past the reference's delay, when the delay elapses, then the URL help is revealed
- [ ] Given the copy action, when it is taken, then the URL is copied and revealed in full
- [ ] Given a failed attempt, when the retry action is taken, then a new attempt starts, and events belonging to the previous attempt are ignored
- [ ] Given a running attempt, when the retry action is taken, then it is refused rather than starting a second concurrent attempt
- [ ] Given the manual fallback action, when it is taken before success, then the flow moves to the key screen
- [ ] Given a successful sign-in, when it completes, then the manual and cancel actions are both inert, the success state is held briefly, and the flow terminates with the completion value
- [ ] Given a successful sign-in whose credential fails to persist, when it completes, then the flow terminates immediately with the persistence outcome rather than showing success
- [ ] Given a cancellation at any point, when it occurs, then the sign-in service is closed even though the flow is unwinding

#### US-195: The API key screen
**Description:** As an operator without a browser, I want to paste a key and be told whether it was accepted so that the manual path is complete on its own.

**Priority:** P0
**Size:** M (3 pts)
**Dependencies:** Blocked by US-185, US-191

**Acceptance Criteria:**
- [ ] Given the key input, when characters are typed, then they are masked and never written to the transcript, the log or the observation trace
- [ ] Given an empty submission, when it is attempted, then it is rejected and the flow stays on the screen
- [ ] Given a submission, when it succeeds, then the flow terminates with the persistence outcome for that key
- [ ] Given a persistence failure, when it occurs, then the outcome is surfaced and the operator can retry without restarting the flow
- [ ] Given the default provider, when the screen renders, then it shows where to obtain a key, derived from the configured base URL rather than hard-coded
- [ ] Given a provider whose key variable is empty, when the screen resolves its provider, then it falls back to the shipped default entry rather than failing

---

### EP-056: The ACP Authentication Surface and Sign-Out

Publish the authentication methods the reference publishes under the same client-capability gates, serve the two extension methods, and give the product its only credential removal path.

**Definition of Done:** The advertised method set matches the reference for every combination of client capabilities the corpus records, both extension methods answer with the reference field names, and sign-out is refused in exactly the states where the reference refuses it.

#### US-196: Declare and serve the authentication methods
**Description:** As an editor integration author, I want the same authentication methods the reference advertises so that an editor that cannot set environment variables still has a path.

**Priority:** P1
**Size:** L (5 pts)
**Dependencies:** Blocked by US-189, US-183

**Acceptance Criteria:**
- [ ] Given a provider supporting browser sign-in, when the client initializes, then the browser method is advertised under the reference's id
- [ ] Given a client advertising the delegated capability, when it initializes, then the delegated method is advertised in addition, and its start and complete actions are both accepted
- [ ] Given a client advertising the terminal capability, when it initializes, then a terminal method is advertised carrying the command and arguments that relaunch this binary in setup mode
- [ ] Given a provider not supporting browser sign-in, when the client initializes, then no browser method is advertised
- [ ] Given a delegated start, when it succeeds, then the response carries the attempt identity, the expiry in the reference's serialization and the sign-in URL
- [ ] Given a delegated complete for an unknown attempt, when it is called, then it is refused with an invalid-request error rather than starting a new attempt
- [ ] Given an attempt that failed with a recoverable error, when the failure is handled, then the attempt remains completable, and for any other failure it is discarded
- [ ] Given an unknown method id, when authenticate is called, then it is refused with the unsupported-method error the current build already produces

#### US-197: Serve auth status and sign-out
**Description:** As an editor integration author, I want to read the user's sign-in state and offer a sign-out so that the editor can show and change it without shelling out.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-183, US-185, US-196

**Acceptance Criteria:**
- [ ] Given a status request, when it is served, then it reloads the global dotenv before assessing and answers with the reference's four field names
- [ ] Given a custom console configured, when status is served, then the custom domain is reported and the shipped default is reported as absent
- [ ] Given a state where sign-out is unavailable, when sign-out is called, then it is refused with an invalid-request error and nothing is cleared
- [ ] Given a state where it is available, when sign-out is called, then the credential is removed from every source and the next status reports the signed-out state
- [ ] Given a storage error during removal, when it occurs, then an internal error is returned and the sources that were cleared stay cleared

---

### EP-057: The Divergence Ledger and the Scorecard

Record what cannot be ported and why, then remeasure the affected rows so the score reflects the instrument rather than a reading of module presence.

**Definition of Done:** Every prose run this subtree cannot ship is recorded with its length and digest and guarded by a stale check, the two form divergences the audit surfaced are recorded, and `docs/parity.md` carries the remeasured rows with the command that reproduces them.

#### US-198: Record the licensing and form divergences
**Description:** As a reviewer, I want each divergence named with what holds it in place so that a later parity review does not relitigate a decision already taken.

**Priority:** P1
**Size:** M (3 pts)
**Dependencies:** Blocked by US-194, US-197

**Acceptance Criteria:**
- [ ] Given each authored prose run this subtree carries, when the corpus is written, then it records the reference run's length and SHA-256 and the replay fails if this port's text ever matches the digest
- [ ] Given the accepted-divergences table, when it is updated, then it carries a row for the sign-in error sentences, a row for the onboarding screen text, and a row for the ACP method descriptions, each naming the artifact that enforces it
- [ ] Given the polling interval, when it is recorded, then the row states that the reference's 3.0 second interval without backoff is reproduced deliberately over the RFC 8628 recommendation, and names the test that fails if the interval changes
- [ ] Given the observation-based onboarding measurement, when it is recorded, then the row explains why rendered output is not compared and names the precedent PRD
- [ ] Given the update prompt's absent automatic installation, when it is recorded, then the row states the divergence that today is undocumented, since this port publishes three of the reference's four prompt outcomes
- [ ] Given a divergence row whose evidence no longer exists, when the suite runs, then the guarding test fails

#### US-199: Remeasure and restate the scorecard
**Description:** As a maintainer, I want the scorecard restated from the oracle so that the number reported is one a command reproduces.

**Priority:** P1
**Size:** S (2 pts)
**Dependencies:** Blocked by US-198

**Acceptance Criteria:**
- [ ] Given the new oracles, when the setup row is restated, then it cites the reproducing command and the per-family conforming counts rather than a module inventory
- [ ] Given the execution-order table, when rank 13 is restated, then its status changes from `TODO` to `DONE` and names the PRD that delivered it
- [ ] Given the configuration row, when it is restated, then the two browser-auth keys are recorded as consumed, each with the test that proves a change in the key changes the behavior
- [ ] Given the app-server row, when it is restated, then the `account/read` divergence is either closed or restated with what still blocks it
- [ ] Given the last-remeasure field, when the document is saved, then it states which rows were restated on this date and which carry an older measurement, so the weighted total is not read as uniformly current

---

## Functional Requirements

- FR-01: The system must classify credential provenance into the six reference states using the reference precedence order.
- FR-02: The system must read a stored credential from the current service name, then the reference legacy name, then the prior-build name, and must rewrite any credential it found under a non-current name.
- FR-03: The system must write a credential to the global dotenv when the credential store refuses the write, and must remove the dotenv copy once a store write succeeds.
- FR-04: The system must never report a successful sign-in before the credential has been durably persisted by at least one mechanism.
- FR-05: When the operator requests a sign-out, the system must refuse it unless the assessed state marks it available.
- FR-06: The system must derive both sign-in base URLs from the provider entry, honoring `browser_auth_base_url` and `browser_auth_api_base_url`.
- FR-07: The system must validate the origin and path prefix of every server-supplied URL before issuing a request to it or opening a browser at it, on every use and not only on first receipt.
- FR-08: The system must generate a PKCE verifier from the unreserved character set carrying at least 256 bits of entropy, and derive its challenge as unpadded base64url of the SHA-256 of the verifier's ASCII bytes.
- FR-09: The system must poll at 3.0 second intervals, must never sleep past the recorded expiry, must tolerate two consecutive poll failures and stop on the third, and must stop immediately on a denied, expired, provider-error or unknown status.
- FR-10: The system must produce each of the eleven reference error codes from its reference condition, and must not invent a twelfth.
- FR-11: The system must install the reference's screen set, gating the four browser screens on the reference's provider predicate.
- FR-12: The system must map each of the five terminating values onto the reference's exit path for that value.
- FR-13: The system must NOT ask for a network proxy, a certificate path or a model during onboarding.
- FR-14: The system must NOT suppress the workspace trust dialog when launched in setup mode with trust undecided.
- FR-15: The system must advertise the reference's authentication methods under the reference's client-capability gates, and must serve the two authentication extension methods.
- FR-16: The system must NOT write a credential, an exchange token or a full server-supplied URL to any log, transcript or observation trace.

## Non-Functional Requirements

- **Performance:** The poll loop issues at most one request per 3.0 seconds and never more than one request per 100 ms under any clock behavior. Adding the onboarding screens increases the startup path by no more than 50 ms measured on a run that reaches the welcome screen. The full corpus replay completes within 60 seconds as part of `cargo test --workspace --all-features`.
- **Security:** PKCE `S256` with a verifier of at least 256 bits of entropy per RFC 7636 §7.1. Every server-supplied URL passes origin and path-prefix validation before use, with 0 of the 29 captured negative cases accepted. 0 occurrences of a credential, an exchange token or a code verifier in any log line, asserted by a test that scans captured output. The credential is stored in the OS credential store by default, and the dotenv fallback file is created with owner-only permissions.
- **Reliability:** A credential is never lost to a storage failure: in 100% of keyring-failure cases the fallback write is attempted before the flow reports any outcome. Two consecutive transient poll failures are tolerated. A sign-in cancelled at any point closes its gateway in 100% of cases.
- **Compatibility:** A credential written by any previously released build of this port resolves after upgrade with 0 operator actions. A configuration file written by the reference round-trips through this port with 0 provider fields dropped.
- **Accessibility:** Every screen is fully operable from the keyboard with 0 mouse-only paths. Every input state is distinguishable without color alone.
- **Observability:** Every terminal error code appears in a diagnostic with its code name, so 11 of 11 failure modes are distinguishable from a log.

## Edge Cases & Error States

| # | Scenario | Trigger | Expected Behavior | User Message |
|---|----------|---------|-------------------|--------------|
| 1 | No credential anywhere | First run on a clean machine | Onboarding runs; state is signed out | Prompt to choose an authentication method |
| 2 | No credential backend | Headless Linux with no Secret Service | Credential written to the global dotenv; flow continues | Notice naming the file used and why |
| 3 | No browser available | SSH session, container, CI | Open-browser error code; sign-in URL displayed for manual use | Failure named, URL shown, manual fallback offered |
| 4 | Browser opens but the user never finishes | Attempt left idle until server expiry | Timeout error code after the recorded expiry; retry offered | State reported as expired with a retry action |
| 5 | Transient network failure mid-poll | Two failed polls then a success | Streak resets, flow continues, no visible interruption | None |
| 6 | Sustained network failure | Three consecutive failed polls | Poll error code; attempt stops | Failure named with a retry action |
| 7 | Server answers a status this build does not know | Protocol drift | Unknown-state error code; attempt stops | Failure named as an unexpected server state |
| 8 | Server hands a URL on another origin | Misconfiguration or compromise | Validation rejects it; no request issued and no browser opened | Failure named as a rejected server URL |
| 9 | Credential present but revoked | Key deleted in the console | State remains classified by provenance; the console verdict is not claimed locally | Account status reported as the build can determine it |
| 10 | Custom domain typed with a typo | Invalid host in the domain input | Submission refused, screen retained, reason shown | Validation reason on the field |
| 11 | Custom domain valid but private-cloud shaped | Internal hostname | Accepted with a warning class, not blocked | Warning that the domain looks like a private deployment |
| 12 | Overwriting an already configured domain | Default target chosen while a custom one is configured | First confirmation arms and warns; second proceeds | Warning naming the configured domain |
| 13 | Terminal resized below the preview minimum | Small window during theme selection | Preview clamped to the minimum; no panic and no overflow | None |
| 14 | Cancellation during a successful sign-in | Escape pressed as the credential is exchanged | Cancel is inert during success; gateway closed on any other cancel | None |
| 15 | Keyring write succeeds after a prior dotenv fallback | Backend becomes available on a later run | Store write succeeds and the stale dotenv copy is removed | None |
| 16 | Sign-out requested for a key from the process environment | Key exported in the shell | Refused; nothing cleared | Refusal explaining the credential is not owned by this store |

## Risks & Mitigations

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | The keyring rename strands credentials for operators who downgrade after upgrading | Medium | High | Read all three service names for at least one release; write only the current one; retire legacy reads only after a release that migrated on read. Assert the migration in US-184 |
| 2 | The onboarding corpus proves the screen graph but not the experience, so the flow conforms and still feels wrong | High | Medium | Keep the PTY harness assertion in the quality gates for every screen story; treat the corpus as necessary and not sufficient |
| 3 | The Textual-to-ratatui gap makes some reference behavior unreproducible, discovered late | Medium | High | US-182 captures the graph before any screen is written, so an unreproducible behavior becomes a ledger entry at the start rather than a surprise at the end |
| 4 | The sign-in endpoints cannot be exercised without a real account, so the protocol is proven only against a stub | High | Medium | Stub the gateway for the state machine and record the wire contract from the reference's own client rather than from a live call; keep the live probe optional and skippable |
| 5 | Prose reproduction drifts toward the reference under pressure to match | Low | High | Every prose run is recorded as a digest with a stale check that fails the suite the moment this port's text matches |
| 6 | Six epics and twenty stories overrun, and the scorecard is restated on partial work | Medium | Medium | The epics are ordered so that EP-052 to EP-054 are independently shippable as the logic half; the scorecard is only restated in EP-057 |
| 7 | Retiring the setup flow's model and network steps removes a path some operator depends on | Low | Medium | Both settings remain reachable from their existing commands; US-191 asserts that reachability rather than assuming it |
| 8 | `keyring` v4 behavior on a backendless host differs from the documented variant | Medium | Low | US-184 asserts the observed variant rather than the documented one, and the assumption is listed for validation |

## Non-Goals

- **Reaching the console API for account status.** Closing the `account/read` `unauthorized` gap needs a client for the console whoami endpoint, which is app-server parity work with its own credential handling. US-199 restates the divergence rather than closing it.
- **Automatic in-product updates.** The reference installs an update from inside its prompt dialog. This port keeps the prompt without the installation and records the divergence in US-198; revisiting it belongs to the distribution row.
- **Porting `identity/read`.** The method is declared and unrouted at the current pin and is tracked by the app-server PRD.
- **A loopback redirect flow.** The reference polls, and adding a second flow shape would be an invented surface no reference behavior measures, however much RFC 8628 §5.4 prefers it.
- **Telemetry on onboarding events.** The reference sends an event when a key is added. The telemetry envelope is already a recorded divergence, so the event is kept locally on the same terms as `telemetry/record`.
- **Snapshot-equal rendering.** No story asserts terminal cells against the reference's SVG snapshots; the measurement is the observation trace.
- **A `/login` or `/logout` slash command.** The reference has neither; sign-out exists only on the editor protocol and this PRD does not invent a CLI counterpart.

## Files NOT to Modify

- `crates/vibe-core/src/parity.rs` and `scripts/parity/pin.py`: the two pin sources. This PRD does not re-pin, and a third copy fails `parity_tests.rs`.
- `crates/vibe-app-server/src/resources/mcp_oauth.rs`: a different feature. Its local callback server and token storage may be read for reference but the sign-in flow does not share its shape.
- `crates/vibe-protocol/src/lib.rs` `SERVER_METHODS`: no wire method is added by this work; adding one fails the app-server surface oracle on an invented name.
- `crates/vibe-app-server/tests/app-server-surface/corpus.json` and the other committed corpora: regenerated only by their own capture scripts.
- `/home/arthur/dev/mistral-vibe/**`: the reference checkout is read-only.

## Technical Considerations

Framed as questions for engineering input, not mandates.

- **Layering:** The sign-in service, the auth state and the credential store are provider-neutral contracts, so they belong in `vibe-core` with `vibe-cli` and `vibe-acp` as adapters. Recommended: a `vibe-core::auth` module mirroring the reference's split between a gateway trait, an HTTP implementation and a service that owns the state machine. Engineering to confirm the trait boundary is worth its indirection given only one implementation ships.
- **Browser opening:** `webbrowser` 1.2.4 is recommended over `open` 5.4.1, because it documents an explicit error when no browser is available and offers suppression of the child process output, which matters when the terminal is being drawn by ratatui. Alternative: shell out as `tui/workflow/mcp.rs:667` already does, at the cost of reimplementing platform detection.
- **Credential store:** `keyring` 4.1.5 ships with the `v1` feature selecting Secret Service through zbus on Linux, so a host without D-Bus has no backend at all. Question for engineering: enable the `cli` feature to gain a local file store as a third tier, or keep the dotenv fallback as the only one? The reference has only the dotenv fallback, which argues for keeping the surface identical.
- **Screen infrastructure:** `crates/vibe-cli/src/tui/startup/dialog.rs` already sequences full-screen dialogs before the main TUI mounts, which is where the onboarding screens plug in. Question: extend `StartupDialog` with the onboarding variants, or give onboarding its own loop with the same shape? The first reuses the event plumbing; the second keeps a seven-screen flow out of an enum built for four one-shot dialogs.
- **Migration:** The keyring rename is the only data migration. Backward compatibility is required in the read direction for at least one release. Rollback plan: an operator who downgrades finds their credential under the new service name, which the older build cannot read, so the release notes must say so and the dotenv fallback remains readable by both.
- **Corpus placement:** The authentication corpus fits under `crates/vibe-core/tests/` beside the config and compaction corpora; the onboarding corpus belongs under `crates/vibe-cli/tests/` beside the runtime-parity traces. Engineering to confirm the split rather than a single corpus, given the two are captured by different harnesses.

## Success Metrics

| Metric | Baseline (current) | Target | Timeframe | How Measured |
|--------|-------------------|--------|-----------|-------------|
| `docs/parity.md` setup row | 35 | 100 | Month-1 | The two new oracles, reproduced by their named commands |
| Auth states reachable | 0 of 6 | 6 of 6 | Month-1 | `authState` corpus family conforming count |
| Sign-in error codes produced | 0 of 11 | 11 of 11 | Month-1 | `errorTaxonomy` corpus family conforming count |
| URL validation cases conforming | 0 of 29 | 29 of 29 | Month-1 | `urlValidation` corpus family conforming count |
| Reference screens implemented | 0 of 7 | 7 of 7 | Month-1 | Onboarding corpus graph family |
| Setup steps without a reference counterpart | 3 | 0 | Month-1 | Onboarding corpus graph family |
| ACP authentication methods advertised | 1 of 3 | 3 of 3 | Month-1 | ACP stdio tests against the capability matrix |
| Declared-only browser-auth keys | 2 | 0 | Month-1 | Workspace grep plus the behavior-change test named in US-187 |
| Corpus scenarios replayed | 0 | ≥ 180 across 8 families | Month-1 | Replay output |
| Credentials lost to a storage failure | 1 per failure | 0 | Month-1 | US-185 fallback tests |
| Operators required to re-enter a key after upgrade | Unknown, all | 0 | Month-6 | US-184 migration test plus release feedback |

## Open Questions

- Does the `keyring` v4.1.5 release surface `NoDefaultStore` on a backendless host, or a different variant? Owner: implementer of US-184, answered before that story leaves `IN_PROGRESS`; the assumption list and the error mapping depend on it.
- Should the legacy read of the prior-build service name `mistral-vibe-rs` be retired after one release or kept indefinitely? Owner: Arthur, before the release that follows this PRD; the answer changes whether US-184's migration is transitional or permanent.
- Should the onboarding screens extend `StartupDialog` or own their loop? Owner: implementer of US-191, decided at the start of EP-055; it changes the shape of five stories.
- Is a local file credential store worth adding as a third tier through the `keyring` `cli` feature, or does that diverge from a reference that has only two? Owner: Arthur, before US-185; it changes the fallback chain and therefore the corpus.
- The reference's account plan mapping is reachable only with a live console. Should a fixture-based client land with this PRD, or stay with the app-server row? Owner: Arthur, before US-199 restates the scorecard.
[/PRD]
