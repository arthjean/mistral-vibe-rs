//! The redaction an integration's free-form text passes through before it is
//! published.

use crate::text::truncate_utf8;

const MAX_PUBLIC_LOG_MESSAGE: usize = 2_048;

/// `message` bounded to what a log may publish, or replaced whole when it
/// carries a credential marker or the user info of a URI.
pub fn redact(message: &str) -> String {
    let bounded = truncate_utf8(message, MAX_PUBLIC_LOG_MESSAGE);
    let lowered = bounded.to_ascii_lowercase();
    let sensitive = [
        "authorization:",
        "bearer ",
        "api_key=",
        "api-key=",
        "apikey=",
        "api key",
        "x-api-key",
        "token=",
        "access_token",
        "refresh_token",
        "password=",
        "secret=",
        "client_secret",
        "client-secret",
    ];
    let uri_userinfo = lowered.find("://").is_some_and(|scheme| {
        lowered[scheme + 3..]
            .split('/')
            .next()
            .is_some_and(|authority| authority.contains('@'))
    });
    if uri_userinfo || sensitive.iter().any(|marker| lowered.contains(marker)) {
        return "[redacted sensitive error]".to_owned();
    }
    bounded.to_owned()
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn redaction_never_projects_secrets() {
        assert_eq!(
            redact("https://alice:password@example.test/path"),
            "[redacted sensitive error]"
        );
        assert_eq!(
            redact("refresh_token=never-project-this"),
            "[redacted sensitive error]"
        );
    }
}
