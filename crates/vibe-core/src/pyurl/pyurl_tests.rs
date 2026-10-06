use super::{InvalidUrl, PyUrl, normalize_url_origin};

#[test]
fn a_split_keeps_the_path_its_encoding_and_its_dot_segments() {
    let parts = PyUrl::split("HTTPS://User@Host.Example:8443/api/./a/%2e%2e/b?x=1#frag");
    assert_eq!(parts.scheme, "https");
    assert_eq!(parts.netloc, "User@Host.Example:8443");
    assert_eq!(parts.path, "/api/./a/%2e%2e/b");
    assert_eq!(parts.query, "x=1");
    assert_eq!(parts.fragment, "frag");
    assert_eq!(parts.hostname().as_deref(), Some("host.example"));
    assert_eq!(parts.port(), Ok(Some(8443)));
}

#[test]
fn leading_controls_go_and_embedded_tabs_and_newlines_are_dropped() {
    let parts = PyUrl::split(" \x01https://ho\tst.exa\nmple/p");
    assert_eq!(parts.scheme, "https");
    assert_eq!(parts.netloc, "host.example");
}

#[test]
fn a_relative_reference_splits_without_a_scheme_or_a_host() {
    let parts = PyUrl::split("/api/poll?x");
    assert_eq!(parts.scheme, "");
    assert_eq!(parts.netloc, "");
    assert_eq!(parts.path, "/api/poll");
    assert_eq!(parts.hostname(), None);
}

#[test]
fn a_port_must_be_ascii_digits_in_range() {
    assert_eq!(PyUrl::split("https://h:0080").port(), Ok(Some(80)));
    assert_eq!(PyUrl::split("https://h:").port(), Ok(None));
    assert_eq!(PyUrl::split("https://h:65536").port(), Err(InvalidUrl));
    assert_eq!(PyUrl::split("https://h:x1").port(), Err(InvalidUrl));
}

#[test]
fn a_bracketed_host_must_be_an_ipv6_literal() {
    assert!(PyUrl::try_split("https://[::1]:8080/x").is_ok());
    assert!(PyUrl::try_split("https://[fe80::1%eth0]/x").is_ok());
    assert!(PyUrl::try_split("https://[v1.fe]/x").is_ok());
    assert_eq!(PyUrl::try_split("https://[::1/x"), Err(InvalidUrl));
    assert_eq!(PyUrl::try_split("https://[127.0.0.1]/x"), Err(InvalidUrl));
    assert_eq!(PyUrl::try_split("https://a[::1]/x"), Err(InvalidUrl));
    assert_eq!(
        PyUrl::split("https://[FE80::1%Zone]:1/")
            .hostname()
            .as_deref(),
        Some("fe80::1%Zone")
    );
}

#[test]
fn unsplit_writes_the_authority_marker_for_netloc_schemes() {
    let mut parts = PyUrl::split("https://h/a?b#c");
    assert_eq!(parts.unsplit(), "https://h/a?b#c");
    parts.netloc.clear();
    assert_eq!(parts.unsplit(), "https:///a?b#c");
    assert_eq!(PyUrl::split("mailto:x@y").unsplit(), "mailto:x@y");
}

#[test]
fn an_origin_fills_in_the_default_port() {
    assert_eq!(
        normalize_url_origin(&PyUrl::split("HTTPS://Host")),
        Ok(("https".to_owned(), Some("host".to_owned()), Some(443)))
    );
    assert_eq!(
        normalize_url_origin(&PyUrl::split("ftp://host")),
        Ok(("ftp".to_owned(), Some("host".to_owned()), None))
    );
    assert_eq!(
        normalize_url_origin(&PyUrl::split("https://host:bad")),
        Err(InvalidUrl)
    );
}
