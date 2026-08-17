//! The ACP authentication surface: method declaration, browser sign-in over
//! the editor protocol, status, and sign-out.
//!
//! Reference `vibe/acp/auth.py`. The controller mirrors `AcpAuthController`:
//! it advertises the browser methods under the provider predicate, drives the
//! delegated attempt lifecycle, persists what a sign-in returned, and owns the
//! only credential removal path the product has. Everything ambient, the
//! effective configuration, the credential stores and the sign-in transport,
//! sits behind [`AcpAuthEnvironment`] so the adapter tests script it the way
//! the reference injects its constructor ports.

pub(crate) mod controller;
pub(crate) mod environment;

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;

use serde_json::{Value, json};
use toml::Table;
use vibe_core::auth::{AuthState, PersistOutcome, RemoveError, SignInAttempt, SignInError};

pub(crate) use controller::AuthController;
pub use environment::ProductionAuthEnvironment;

#[cfg(test)]
mod prose_tests;

/// The reference's sign-in target vocabulary.
const SIGN_IN_TARGET_MISTRAL: &str = "mistral";
const SIGN_IN_TARGET_CUSTOM: &str = "custom";

/// The default home the production environment resolves when the process
/// environment names none, mirroring the CLI's `$VIBE_HOME` then `~/.vibe`.
#[must_use]
pub fn default_vibe_home() -> PathBuf {
    std::env::var_os("VIBE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".vibe"))
        })
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".vibe")
        })
}

pub type AuthKeyFuture<'a> = Pin<Box<dyn Future<Output = Result<String, SignInError>> + Send + 'a>>;
pub type AuthAttemptFuture<'a> =
    Pin<Box<dyn Future<Output = Result<SignInAttempt, SignInError>> + Send + 'a>>;

/// The ambient world the authentication surface runs against. Reference
/// `AcpAuthController`'s constructor ports: the context loader, the sign-in
/// service factory, the key persister and remover, and the provider persister.
pub trait AcpAuthEnvironment: Send + Sync {
    /// The provider the flows authenticate, resolved from the effective
    /// configuration; the shipped Mistral entry when nothing resolves.
    fn load_provider(&self) -> Table;

    /// Reassesses credential provenance for `env_key`, rereading the global
    /// dotenv, which is what the reference's `load_dotenv_values` reload
    /// amounts to.
    ///
    /// # Errors
    ///
    /// Propagates the dotenv read failure when the file exists and cannot be
    /// read.
    fn assess(&self, env_key: &str) -> io::Result<AuthState>;

    /// Persists `api_key` with the reference's outcomes: environment, then
    /// keyring, then the global dotenv when the keyring refuses.
    fn persist_api_key(
        &self,
        env_key: &str,
        backend_is_mistral: bool,
        api_key: &str,
        custom_domain: bool,
    ) -> PersistOutcome;

    /// Removes the credential for `env_key` from every source it owns.
    ///
    /// # Errors
    ///
    /// The deferred backend failure and the dotenv rewrite failure, as
    /// `vibe_core::auth::remove_api_key` reports them.
    fn remove_api_key(&self, env_key: &str) -> Result<(), RemoveError>;

    /// Upserts `provider` into the configuration; `false` when the write did
    /// not land.
    fn persist_provider(&self, provider: &Table) -> bool;

    /// The whole browser flow for `provider`: create, open, poll, exchange.
    fn browser_authenticate<'a>(&'a self, provider: &'a Table) -> AuthKeyFuture<'a>;

    /// Creates a sign-in process without opening a browser, which is the
    /// delegated start where the editor owns the browser.
    fn start_attempt<'a>(&'a self, provider: &'a Table) -> AuthAttemptFuture<'a>;

    /// Polls and exchanges a previously started attempt.
    fn complete_attempt<'a>(
        &'a self,
        provider: &'a Table,
        attempt: &'a SignInAttempt,
    ) -> AuthKeyFuture<'a>;
}

/// One advertised browser method. The reference's `AuthMethodAgent` carries
/// an id, a name and a description; the sentences are this port's own, which
/// `NOTICE` keeps permanently unequal to the reference's.
fn browser_method(method_id: &str) -> Value {
    json!({
        "id": method_id,
        "name": "Sign in with Mistral",
        "description": "Open your browser to connect Mistral Vibe to your Mistral account.",
    })
}

/// The command that reaches the setup flow. The reference relaunches its own
/// entrypoint because that entrypoint runs the onboarding itself; here the
/// flow ships in the `vibe` binary, so the method names the sibling installed
/// beside this one and falls back to the bare name for a `PATH` lookup. An
/// editor that runs it gets the setup flow, which is what the method promises.
fn setup_command() -> String {
    let binary = if cfg!(windows) { "vibe.exe" } else { "vibe" };
    std::env::current_exe()
        .ok()
        .map(|path| path.with_file_name(binary))
        .filter(|path| path.is_file())
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| binary.to_owned())
}

/// The terminal method a `terminal-auth` client receives: the command and
/// arguments that relaunch this build in setup mode. Reference
/// `TerminalAuthMethod` with id `vibe-setup`.
pub(crate) fn terminal_method() -> Value {
    let command = setup_command();
    let args = json!(["--setup"]);
    json!({
        "type": "terminal",
        "id": "vibe-setup",
        "name": "Add an API key",
        "description": "Run the Mistral Vibe setup flow in a terminal to save an API key.",
        "args": args,
        "_meta": {
            "terminal-auth": {
                "command": command,
                "args": args,
                "label": "Vibe setup",
            }
        },
    })
}
