use thiserror::Error;

use crate::text::truncate_utf8;

const MAX_PUBLIC_LOG_MESSAGE: usize = 2_048;

#[derive(Debug, Error)]
pub enum IntegrationError {
    #[error("{0} lock is poisoned")]
    LockPoisoned(&'static str),
    #[error("connector `{0}` was not found")]
    ConnectorNotFound(String),
    #[error("invalid connector: {0}")]
    InvalidConnector(String),
    #[error("connector tool failed: {0}")]
    Tool(String),
}

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
