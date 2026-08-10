//! Credential provenance, storage and lifecycle.
//!
//! Reference `vibe/setup/auth/` and `vibe/utils/keyring.py`: the six-state
//! provenance assessment, the OS credential store with its service-name
//! migration, and the persistence path whose keyring failure falls back to the
//! global dotenv instead of discarding the key. `vibe-cli` and `vibe-acp` are
//! adapters over this module; nothing here draws a screen or speaks a wire
//! protocol.
//!
//! The reference mutates `os.environ` when it persists or removes a key. This
//! port cannot: `std::env::set_var` is `unsafe` under edition 2024 and
//! `unsafe_code` is forbidden workspace-wide, so every operation that would
//! touch the process environment takes the caller's environment map and edits
//! that instead, which is also what makes the corpus replay deterministic.

pub mod env_file;
pub mod keyring;
pub mod persistence;
pub mod sign_in;
pub mod sign_in_http;
pub mod state;

#[cfg(test)]
mod env_file_tests;
#[cfg(test)]
mod keyring_tests;
#[cfg(test)]
mod persistence_tests;
#[cfg(test)]
mod sign_in_http_tests;
#[cfg(test)]
mod sign_in_tests;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
pub(crate) mod testing;

pub use env_file::{remove_env_file_key, write_env_file_key};
pub use keyring::{
    KEYRING_SERVICE, KeyringBackend, KeyringFailure, KeyringStore, LEGACY_KEYRING_SERVICES,
    NativeKeyringBackend, PRIOR_BUILD_KEYRING_SERVICE,
};
pub use persistence::{
    ApiKeyAddedEvent, PersistOutcome, PersistReport, RemoveError, persist_api_key, remove_api_key,
};
pub use sign_in::{
    CODE_CHALLENGE_METHOD, MAX_CONSECUTIVE_POLL_FAILURES, POLL_INTERVAL_SECONDS, SignInAttempt,
    SignInError, SignInErrorCode, SignInEvent, SignInGateway, SignInPoll, SignInProcess,
    SignInRuntime, SignInService, SignInStatus, SystemSignInRuntime, UtcTimestamp, code_challenge,
    generate_code_verifier, open_system_browser,
};
pub use sign_in_http::{
    DEFAULT_BROWSER_AUTH_API_BASE_URL, DEFAULT_BROWSER_AUTH_BASE_URL, HttpSignInGateway,
    ReqwestSignInClient, SignInHttpClient, SignInHttpResponse, SignInTransportError, UrlRejection,
    browser_sign_in_bases, validate_url_against_base,
};
pub use state::{
    AuthState, AuthStateKind, DEFAULT_MISTRAL_API_ENV_KEY, assess_auth_state, resolve_api_key,
};
