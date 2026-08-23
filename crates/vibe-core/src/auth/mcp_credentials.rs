//! Where an MCP OAuth credential lives in the OS credential store.
//!
//! Reference `vibe/core/auth/mcp_oauth.py` keys a stored credential by the
//! server's configured name. This port keys it by the resource URL instead, so
//! one login serves every alias that addresses the same server and renaming an
//! entry never orphans its token. The naming lives here rather than beside the
//! OAuth client because two callers need it: the session that stores the
//! credential, and `vibe mcp remove`, which deletes it before dropping the
//! configuration entry.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use url::Url;

use super::keyring::{KeyringBackend, KeyringFailure};

/// The service every MCP OAuth credential is stored under.
pub const MCP_OAUTH_KEYRING_SERVICE: &str = "mistral-vibe-rs";

/// The account name `resource` is stored under, or `None` when the URL cannot
/// identify one.
///
/// A fragment never reaches the server, so a URL carrying one would key a
/// credential by something the resource itself never sees.
#[must_use]
pub fn mcp_oauth_account(resource: &Url) -> Option<String> {
    if resource.fragment().is_some() {
        return None;
    }
    let fingerprint = URL_SAFE_NO_PAD.encode(Sha256::digest(resource.as_str().as_bytes()));
    Some(format!("mcp-oauth:{fingerprint}"))
}

/// Deletes the credential stored for `resource`, if there is one.
///
/// Reference `delete_oauth_credentials`: an absent entry and an unusable store
/// are both answered as success, because neither leaves a token behind, and
/// only a store that exists and refused is a failure.
pub fn delete_mcp_oauth_credential(
    backend: &dyn KeyringBackend,
    resource: &Url,
) -> Result<(), KeyringFailure> {
    let Some(account) = mcp_oauth_account(resource) else {
        return Ok(());
    };
    match backend.delete(MCP_OAUTH_KEYRING_SERVICE, &account) {
        Ok(()) | Err(KeyringFailure::NoEntry | KeyringFailure::NoBackend) => Ok(()),
        Err(failure) => Err(failure),
    }
}
