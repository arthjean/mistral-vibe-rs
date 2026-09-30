//! What every integration shares: the error it reports and the redaction its
//! free-form text passes through before it is published.

mod shared;

pub use shared::{IntegrationError, redact};

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
