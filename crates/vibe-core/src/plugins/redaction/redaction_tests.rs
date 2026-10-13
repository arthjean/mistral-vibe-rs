//! URL redaction against what the pinned reference's `redact_url` answers
//! (`vibe/core/plugins/_redaction.py` over `vibe/utils/url.py` at `376f6a3`).

use super::*;

#[test]
fn a_url_is_redacted_as_the_reference_redacts_it() {
    for (url, expected) in [
        (
            "https://user:pw@example.com:8443/mcp?token=x#f",
            "https://example.com:8443/mcp",
        ),
        ("http://[::1]:8080/mcp", "http://[::1]:8080/mcp"),
        ("http://[::1]/mcp", "http://[::1]/mcp"),
        ("http://[fe80::1%eth0]:9/x", "http://[fe80::1%eth0]:9/x"),
        ("http://host:abc/x", "http://host:abc/x"),
        ("http://user:pw@host:99999/x?q=1", "http://host:99999/x"),
        ("http://[::1/mcp", ""),
        ("HTTP://Example.COM/A", "http://example.com/A"),
        ("example.com/path?x", "example.com/path"),
        ("//host/p", "//host/p"),
        ("http://h:/p", "http://h/p"),
        ("", ""),
        ("mcp", "mcp"),
    ] {
        assert_eq!(redact_url(url), expected, "{url}");
    }
}
