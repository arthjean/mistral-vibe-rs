use std::path::PathBuf;

use super::certificates;

#[test]
fn a_location_that_cannot_be_read_contributes_nothing() {
    let missing = PathBuf::from("/nonexistent/vibe-trust/ca.pem");
    assert!(certificates(Some(&missing), std::slice::from_ref(&missing)).is_empty());
}

/// The certificates a named location holds are added to the system roots
/// rather than replacing them, so the client still builds with both sets.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn named_certificates_join_the_system_roots() {
    let system = openssl_probe::candidate_cert_dirs()
        .map(std::path::Path::to_path_buf)
        .collect::<Vec<_>>();
    let roots = certificates(None, &system);
    if roots.is_empty() {
        eprintln!("skipping: this host exposes no system certificate directory");
        return;
    }
    let custom = certificates(None, &system[..1]);
    assert!(!custom.is_empty());
    assert!(
        super::with_custom_roots(reqwest::Client::builder(), custom)
            .build()
            .is_ok()
    );
}
