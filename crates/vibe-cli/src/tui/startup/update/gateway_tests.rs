//! The update gateway must name the distribution the running binary came from.
//!
//! The port shipped a PyPI gateway pointed at the reference's project, so an
//! installed binary compared itself against a package it is not. These tests
//! pin the resolution to the repository the manifest declares and to the gate
//! that decides whether a gateway is built at all.

use super::{production_update_gateway, release_repository, scheduled_update_gateway};

/// The repository `[workspace.package] repository` declares, as an owner and a
/// name. `both_installers_fetch_from_the_declared_repository` in
/// `crates/vibe-cli/src/distribution/release_parity_tests.rs` holds the manifest
/// itself to this value.
const DECLARED_REPOSITORY: (&str, &str) = ("arthjean", "mistral-vibe-rs");

#[test]
fn the_update_gateway_names_the_repository_the_manifest_declares() {
    assert_eq!(
        release_repository(),
        Some(DECLARED_REPOSITORY),
        "the declared repository is no longer a GitHub owner and name the gateway can address"
    );
    let gateway = production_update_gateway().expect("a production build resolves a gateway");
    let (owner, repository) = DECLARED_REPOSITORY;
    assert!(
        gateway
            .releases_url()
            .ends_with(&format!("/repos/{owner}/{repository}/releases")),
        "the gateway reads a different repository's releases: {}",
        gateway.releases_url()
    );
}

#[test]
fn a_disabled_update_check_builds_no_gateway() {
    // The reference schedules no notification when the setting is off, so
    // nothing exists that could send a request.
    assert!(
        scheduled_update_gateway(false).is_none(),
        "a disabled update check still built a gateway"
    );
    assert!(
        scheduled_update_gateway(true).is_some(),
        "an enabled update check built no gateway, so the previous assertion proves nothing"
    );
}
