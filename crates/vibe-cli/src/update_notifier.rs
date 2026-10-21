//! Update discovery pinned to the reference `vibe.cli.update_notifier` contract:
//! the release gateways, the update cache, and the decisions read from both.
//!
//! The commands an accepted prompt installs with run through
//! `vibe_core::upgrade`, which owns the process groups they need.

mod cache;
mod gateway;
mod github;
mod pypi;
mod update;
mod version;

pub use cache::{UpdateCache, UpdateCacheStore};
pub use gateway::{UpdateFetch, UpdateGateway, UpdateGatewayCause, UpdateGatewayError};
pub use github::{GitHubUpdateGateway, resolve_github_response};
pub use pypi::{PyPiUpdateGateway, resolve_pypi_response};
pub use update::{
    CachePlan, FetchResolution, UpdateAvailability, UpdateError, dismiss_update,
    get_update_if_available, mark_version_as_seen, pending_update_from_cache, plan_update_check,
    resolve_fetch, should_show_whats_new,
};
pub use version::Version;
