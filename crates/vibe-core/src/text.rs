//! Canonical text bounding helpers.
//!
//! Every byte budget in this crate is enforced through [`truncate_utf8`] so
//! that a limit can never land inside a multi-byte character. `String::truncate`
//! panics on such a boundary, which is unacceptable for text that arrives from
//! providers, MCP servers, hooks, and tools.

/// Returns the longest prefix of `value` that fits within `max_bytes`, never
/// splitting a UTF-8 character.
#[must_use]
pub fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value.get(..end).unwrap_or_default()
}

/// Bounds `value` to `max_bytes`, appending `marker` when content was dropped.
#[must_use]
pub fn bounded_utf8(value: &str, max_bytes: usize, marker: &str) -> String {
    let truncated = truncate_utf8(value, max_bytes);
    if truncated.len() == value.len() {
        return value.to_owned();
    }
    format!("{truncated}{marker}")
}

/// Matches `value` against a `*` wildcard `pattern`.
///
/// Each `*` stands for any run of characters, including none. A pattern without
/// a `*` matches literally.
#[must_use]
pub fn matches_wildcard(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == value;
    }
    let mut remainder = value;
    for (index, part) in pattern.split('*').enumerate() {
        if part.is_empty() {
            continue;
        }
        let Some(position) = remainder.find(part) else {
            return false;
        };
        if index == 0 && !pattern.starts_with('*') && position != 0 {
            return false;
        }
        remainder = remainder
            .get(position.saturating_add(part.len())..)
            .unwrap_or_default();
    }
    pattern.ends_with('*') || remainder.is_empty()
}

/// Identity of a URL for duplicate detection: no fragment, no trailing slash.
#[must_use]
pub fn canonical_url(url: &url::Url) -> String {
    let mut canonical = url.clone();
    canonical.set_fragment(None);
    canonical.to_string().trim_end_matches('/').to_owned()
}

/// Reports whether `url` is safe to send credentials to.
///
/// HTTPS always is; plain HTTP only when it cannot leave the machine.
#[must_use]
pub fn is_secure_transport(url: &url::Url) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => url.host_str().is_some_and(is_loopback_host),
        _ => false,
    }
}

/// Reports whether `host` always resolves to this machine.
#[must_use]
pub fn is_loopback_host(host: &str) -> bool {
    host == "localhost"
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Lowercase hexadecimal encoding of `bytes`.
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_never_splits_a_character() {
        assert_eq!(truncate_utf8("héllo", 2), "h");
        assert_eq!(truncate_utf8("héllo", 3), "hé");
        assert_eq!(truncate_utf8("hello", 99), "hello");
        assert_eq!(truncate_utf8("é", 0), "");
    }

    #[test]
    fn markers_are_appended_only_when_content_is_dropped() {
        assert_eq!(bounded_utf8("hello", 99, "…"), "hello");
        assert_eq!(bounded_utf8("héllo", 3, "…"), "hé…");
    }

    #[test]
    fn wildcards_anchor_both_ends() {
        assert!(matches_wildcard("*", "anything"));
        assert!(matches_wildcard("read", "read"));
        assert!(!matches_wildcard("read", "readme"));
        assert!(matches_wildcard("mcp_*", "mcp_docs_search"));
        assert!(!matches_wildcard("mcp_*", "connector_docs"));
        assert!(matches_wildcard("a*b", "azzb"));
        assert!(!matches_wildcard("a*b", "zab"));
        assert!(matches_wildcard("*b", "ab"));
    }

    #[test]
    fn loopback_covers_every_local_address_form() {
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.3.2.1"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
        assert!(!is_loopback_host("example.test"));
        assert!(!is_loopback_host("10.0.0.1"));
    }

    #[test]
    fn secure_transport_allows_https_and_loopback_http_only() {
        let secure = |raw: &str| is_secure_transport(&url::Url::parse(raw).expect("fixture URL"));
        assert!(secure("https://example.test/mcp"));
        assert!(secure("http://127.0.0.1:8080/mcp"));
        assert!(!secure("http://example.test/mcp"));
        assert!(!secure("ftp://example.test/mcp"));
    }

    #[test]
    fn hex_encoding_is_lowercase_and_padded() {
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(hex_encode(&[]), "");
    }
}
