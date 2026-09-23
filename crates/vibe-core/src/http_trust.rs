//! Reference `build_ssl_context` in `vibe/utils/http.py`: the certificates
//! `SSL_CERT_FILE` and `SSL_CERT_DIR` name are trusted in addition to the roots a
//! client already trusts, never instead of them.
//!
//! The reference's own comment states the intent: custom certificates are
//! additive so a private-CA user keeps the public roots. The platform verifier
//! this port links answers the same two variables the other way on Linux, where
//! it loads only what they name, and ignores them on macOS and Windows. Both are
//! corrected here, so a client built through [`trust_certificate_environment`]
//! trusts the system store plus the named certificates on every platform.

use std::path::{Path, PathBuf};

use reqwest::{Certificate, ClientBuilder};

const CERTIFICATE_FILE_VARIABLE: &str = "SSL_CERT_FILE";
const CERTIFICATE_DIRECTORY_VARIABLE: &str = "SSL_CERT_DIR";

/// Adds the certificates the process environment names to `builder`.
///
/// A location that cannot be read contributes nothing, as the reference logs a
/// warning and keeps its base context when `load_verify_locations` fails.
pub fn trust_certificate_environment(builder: ClientBuilder) -> ClientBuilder {
    let file = non_empty_variable(CERTIFICATE_FILE_VARIABLE);
    let directories = non_empty_variable(CERTIFICATE_DIRECTORY_VARIABLE);
    if file.is_none() && directories.is_none() {
        return builder;
    }
    let directories = directories
        .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default();
    let custom = certificates(file.as_deref().map(Path::new), &directories);
    with_custom_roots(builder, custom)
}

fn non_empty_variable(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// The system store plus the named roots, as a closed set: the verifier would
/// otherwise read the same variables and trust only what they name.
#[cfg(all(unix, not(target_os = "macos")))]
fn with_custom_roots(builder: ClientBuilder, custom: Vec<Certificate>) -> ClientBuilder {
    let system = openssl_probe::candidate_cert_dirs()
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    let mut roots = certificates(None, &system);
    roots.extend(custom);
    builder.tls_certs_only(roots)
}

/// The named roots merged into the operating system store the verifier asks.
#[cfg(not(all(unix, not(target_os = "macos"))))]
fn with_custom_roots(builder: ClientBuilder, custom: Vec<Certificate>) -> ClientBuilder {
    builder.tls_certs_merge(custom)
}

/// Every PEM certificate in `file` and in the files of `directories`.
fn certificates(file: Option<&Path>, directories: &[PathBuf]) -> Vec<Certificate> {
    let mut found = rustls_native_certs::load_certs_from_paths(file, None).certs;
    for directory in directories {
        found.extend(rustls_native_certs::load_certs_from_paths(None, Some(directory)).certs);
    }
    found
        .iter()
        .filter_map(|der| Certificate::from_der(der.as_ref()).ok())
        .collect()
}

#[cfg(test)]
mod http_trust_tests;
