//! Reference `UpdateGateway` (`vibe/cli/update_notifier/ports/update_gateway.py`)
//! and the one HTTP exchange both gateways send through.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use thiserror::Error;

pub(super) const GATEWAY_TIMEOUT: Duration = Duration::from_secs(5);

/// The header GitHub spends down on every answer, error responses included.
const RATE_LIMIT_REMAINING_HEADER: &str = "X-RateLimit-Remaining";

/// Reference `UpdateGatewayCause`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateGatewayCause {
    TooManyRequests,
    Forbidden,
    NotFound,
    RequestFailed,
    ErrorResponse,
    InvalidResponse,
    Unknown,
}

impl UpdateGatewayCause {
    /// Reference `DEFAULT_GATEWAY_MESSAGES`.
    #[must_use]
    pub const fn default_message(self) -> &'static str {
        match self {
            Self::TooManyRequests => "Rate limit exceeded while checking for updates.",
            Self::Forbidden => "Request was forbidden while checking for updates.",
            Self::NotFound => "Unable to fetch the releases. Please check your permissions.",
            Self::RequestFailed => "Network error while checking for updates.",
            Self::ErrorResponse => "Unexpected response received while checking for updates.",
            Self::InvalidResponse => "Received an invalid response while checking for updates.",
            Self::Unknown => "Unable to determine whether an update is available.",
        }
    }
}

/// Reference `UpdateGatewayError`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{}", self.user_message())]
pub struct UpdateGatewayError {
    pub cause: UpdateGatewayCause,
    pub message: Option<String>,
}

impl UpdateGatewayError {
    #[must_use]
    pub const fn new(cause: UpdateGatewayCause) -> Self {
        Self {
            cause,
            message: None,
        }
    }

    /// Reference `_describe_gateway_error`.
    #[must_use]
    pub fn user_message(&self) -> String {
        self.message
            .clone()
            .unwrap_or_else(|| self.cause.default_message().to_owned())
    }
}

pub type UpdateFetch<'a> =
    Pin<Box<dyn Future<Output = Result<Option<String>, UpdateGatewayError>> + Send + 'a>>;

/// Reference `UpdateGateway`.
pub trait UpdateGateway: Send + Sync {
    fn fetch_update(&self) -> UpdateFetch<'_>;
}

/// The client both gateways send through: `httpx` applies its five-second
/// timeout to connecting and to each read rather than to the whole exchange,
/// follows no redirect, and `build_ssl_context` adds the certificates the
/// environment names.
pub(super) fn gateway_client(timeout: Duration) -> Result<reqwest::Client, UpdateGatewayError> {
    vibe_core::http_trust::trust_certificate_environment(
        reqwest::Client::builder()
            .connect_timeout(timeout)
            .read_timeout(timeout)
            // `httpx` answers a redirect as the response it is.
            .redirect(reqwest::redirect::Policy::none()),
    )
    .build()
    .map_err(|_| UpdateGatewayError::new(UpdateGatewayCause::RequestFailed))
}

/// One request as `httpx.AsyncClient.get` performs it: the whole body is read
/// before the caller sees the status, so a connection that fails mid-body is a
/// failed request rather than a response.
pub(super) async fn exchange(
    request: reqwest::RequestBuilder,
) -> Result<(u16, Option<String>, Vec<u8>), UpdateGatewayError> {
    let failed = |_| UpdateGatewayError::new(UpdateGatewayCause::RequestFailed);
    let response = request.send().await.map_err(failed)?;
    let status = response.status().as_u16();
    let remaining = response
        .headers()
        .get(RATE_LIMIT_REMAINING_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response.bytes().await.map_err(failed)?;
    Ok((status, remaining, body.to_vec()))
}
