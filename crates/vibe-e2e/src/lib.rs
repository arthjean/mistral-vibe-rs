//! The hermetic end-to-end harness of `tasks/prd-e2e-test-battery.md`.
//!
//! A scenario drives a shipped binary (`vibe`, `vibe-acp`) the way a user or an
//! editor does, in a [`World`] of its own: a home, a vibe home whose
//! `config.toml` points the provider and every service host at a loopback
//! [`Platform`], a Git work tree, and an egress sentinel exported as the proxy.
//! A scenario can add scripted MCP servers ([`Mcp`]): over stdio, the e2e
//! binary itself serves them; over HTTP, the platform does, connector gateways
//! included.
//! On Linux the whole test binary runs inside a network namespace that holds
//! only loopback ([`jail`]), so nothing a trial starts can reach a real host,
//! and the sentinel names any host a binary still reaches for.
//!
//! Each package declares one `e2e` test target with `harness = false` whose
//! `main` builds a [`Suite`]. The suite enters the jail, resolves the subject
//! (`VIBE_E2E_SUBJECT=reference` runs the pinned Python reference instead of
//! the port), and hands one libtest-mimic trial per [`Scenario`] to the runner.
//!
//! # Measured jail and host facts (US-001, 2026-10-28)
//!
//! The probe ran each wrapper around a child that binds loopback, connects to
//! `1.1.1.1:443`, prints its uid, opens a PTY, and compares a grandchild's
//! network namespace with its own.
//!
//! | Host | `bwrap --unshare-net` | nested `unshare` |
//! |------|-----------------------|------------------|
//! | Fedora 44 (kernel 7.2, bwrap 0.12.0, util-linux 2.41.6) | works, 1.6 ms median start overhead | works, 3.1 ms |
//! | ubuntu-22.04 runner (kernel 6.8 azure, bwrap 0.6.1 from apt, util-linux 2.37.2) | works after `apt-get install bubblewrap`, 2.5 ms | works out of the box, 4.8 ms |
//! | ubuntu-24.04 runner (kernel 6.17 azure, bwrap 0.9.0, util-linux 2.39.3) | fails: `bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted` | fails: `unshare: write failed /proc/self/uid_map: Operation not permitted` |
//!
//! Where a wrapper works, loopback binds, a public connect fails at once with
//! `ENETUNREACH`, the uid inside equals the uid outside, a PTY opens, and a
//! grandchild shares the namespace. On ubuntu-24.04 the failures come from
//! `kernel.apparmor_restrict_unprivileged_userns=1`; `sudo sysctl -w
//! kernel.apparmor_restrict_unprivileged_userns=0` restores both wrappers
//! (2.5 ms and 4.4 ms). Neither runner image ships bubblewrap. The nested
//! `unshare` wrapper also needs `ip` from iproute2 to bring loopback up.
//!
//! With only the provider redirected and `HTTPS_PROXY`, `HTTP_PROXY`, and
//! `ALL_PROXY` pointing at the sentinel (`NO_PROXY=127.0.0.1,localhost`), the
//! port and the pinned reference contact these hosts through `vibe -p`, the
//! TUI started then exited, and `vibe-acp` through `initialize` and
//! `session/new`:
//!
//! | Host | Contacted by | Redirected by |
//! |------|--------------|---------------|
//! | the provider's host | every surface | `[[providers]] api_base`, which also serves `/v1/users/me`, `/v1/connectors/bootstrap`, and `/v1/datalake/events` for a Mistral backend |
//! | `chat.mistral.ai:443` | every surface, Mistral backend only | `vibe_base_url` (`/api/v1/code/managed-config`); `vibe_code_sessions_base_url` shares the default |
//! | `console.mistral.ai:443` | every surface, Mistral backend only | `console_base_url` (`/api/vibe/whoami`) |
//! | `experiments.mistral.services:443` | every surface, Mistral backend only | `[experiments] api_host` (`/api/eval/<client_key>`) |
//! | `api.github.com:443` | the port's TUI | `enable_update_checks = false` |
//! | `pypi.org:443` | the reference's TUI, whatever the backend | `enable_update_checks = false` |
//!
//! With every key above pointed at loopback, neither implementation reaches the
//! sentinel. Browser sign-in and Vertex token hosts are hardcoded and stay
//! unredirectable; no startup path contacts them. Both implementations honor
//! the proxy variables: a provider `api_base` of `https://api.probe.example`
//! reaches the sentinel as `CONNECT api.probe.example:443` from both (the
//! reference then exits 1, the port keeps retrying). The port refuses a plain
//! `http` base on a non-loopback host before any connection.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod artifacts;
mod http;
pub mod jail;
mod mcp;
mod platform;
mod process;
mod pty;
mod routes;
mod rpc;
pub mod seams;
mod sentinel;
mod suite;
mod world;

use std::fmt;
use std::time::Duration;

pub use mcp::{Mcp, StdioMcp, text_result};
pub use platform::{Entry, Gate, Platform, Reply};
pub use process::{Child, Command, Output};
pub use pty::{Key, LIVE_PTYS, Screen, Tui, TuiExit};
pub use routes::RouteReply;
pub use rpc::{Answer, Exchange, Rpc};
pub use suite::{Ctx, Scenario, Subject, Suite};
pub use world::{PROVIDER_KEY_VARIABLE, Provider, Setup, World};

/// Why a trial failed, in words a maintainer can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure(pub String);

impl Failure {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

impl From<serde_json::Error> for Failure {
    fn from(error: serde_json::Error) -> Self {
        Self(error.to_string())
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self(message)
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Self(message.to_owned())
    }
}

/// What a scenario step returns.
pub type Outcome<T = ()> = Result<T, Failure>;

/// Fails with `message` unless `condition` holds.
pub fn ensure(condition: bool, message: impl FnOnce() -> String) -> Outcome {
    if condition {
        Ok(())
    } else {
        Err(Failure(message()))
    }
}

/// How long a wait lasts when a scenario names no deadline.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(30);

/// Multiplies every deadline, for a machine slower than the ones the defaults
/// were measured on.
pub const TIME_SCALE_VARIABLE: &str = "VIBE_E2E_TIME_SCALE";

/// `deadline` multiplied by [`TIME_SCALE_VARIABLE`] when it holds a positive
/// number.
#[must_use]
pub fn scaled(deadline: Duration) -> Duration {
    deadline.mul_f64(scale_from(
        std::env::var(TIME_SCALE_VARIABLE).ok().as_deref(),
    ))
}

/// The factor a [`TIME_SCALE_VARIABLE`] value names: a positive number, or 1.
fn scale_from(value: Option<&str>) -> f64 {
    value
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|scale| scale.is_finite() && *scale > 0.0)
        .unwrap_or(1.0)
}

/// The last `limit` bytes of `bytes`, decoded lossily.
fn tail(bytes: &[u8], limit: usize) -> String {
    let start = bytes.len().saturating_sub(limit);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}
