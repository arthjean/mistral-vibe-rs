//! Reference `GitHubUpdateGateway`
//! (`vibe/cli/update_notifier/adapters/github_update_gateway.py`).

use std::time::Duration;

use super::gateway::{
    GATEWAY_TIMEOUT, UpdateFetch, UpdateGateway, UpdateGatewayCause, UpdateGatewayError, exchange,
    gateway_client,
};

/// Reference `GitHubUpdateGateway`: the releases of one repository, newest
/// published first.
pub struct GitHubUpdateGateway {
    owner: String,
    repository: String,
    base_url: String,
    token: Option<String>,
    client: reqwest::Client,
}

/// The `User-Agent` the reference sends under its own name. GitHub answers an
/// unidentified client with a 403, so this port names itself instead.
const GITHUB_USER_AGENT: &str = concat!(
    "mistral-vibe-rs-update-notifier/",
    env!("CARGO_PKG_VERSION")
);

impl GitHubUpdateGateway {
    /// The host the reference reads, and this port's default.
    pub const DEFAULT_BASE_URL: &'static str = "https://api.github.com";

    /// The sentence this port publishes for `NotFound`. `NOTICE` forbids
    /// shipping the reference's own prose, so this states the same cause and
    /// the same next action in this port's words, and
    /// `the_not_found_sentence_is_this_port_s_own` holds the two unequal.
    const NOT_FOUND_MESSAGE: &'static str = "The releases of this repository could not be read. Export a GITHUB_TOKEN that can read \
         them, then check again.";

    pub fn with_base_url(
        owner: impl Into<String>,
        repository: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self, UpdateGatewayError> {
        Self::with_timeout(owner, repository, base_url, GATEWAY_TIMEOUT)
    }

    fn with_timeout(
        owner: impl Into<String>,
        repository: impl Into<String>,
        base_url: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, UpdateGatewayError> {
        Ok(Self {
            owner: owner.into(),
            repository: repository.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            token: None,
            client: gateway_client(timeout)?,
        })
    }

    /// Reference `GitHubUpdateGateway.__init__`: the token is optional, and the
    /// reference's falsy check makes an empty one no token at all.
    #[must_use]
    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token.filter(|value| !value.trim().is_empty());
        self
    }

    #[must_use]
    pub fn releases_url(&self) -> String {
        format!(
            "{}/repos/{}/{}/releases",
            self.base_url, self.owner, self.repository
        )
    }
}

impl UpdateGateway for GitHubUpdateGateway {
    fn fetch_update(&self) -> UpdateFetch<'_> {
        Box::pin(async move {
            let mut request = self
                .client
                .get(self.releases_url())
                .header("Accept", "application/vnd.github+json")
                .header("User-Agent", GITHUB_USER_AGENT);
            if let Some(token) = &self.token {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            let (status, remaining, body) = exchange(request).await?;
            resolve_github_response(status, remaining.as_deref(), &body)
        })
    }
}

/// Reference `GitHubUpdateGateway.fetch_update` once the response is in hand.
/// `NotFound` carries this port's own sentence, as the reference carries its
/// own there.
pub fn resolve_github_response(
    status: u16,
    rate_limit_remaining: Option<&str>,
    body: &[u8],
) -> Result<Option<String>, UpdateGatewayError> {
    if let Some(cause) = github_status_cause(status, rate_limit_remaining) {
        return Err(UpdateGatewayError {
            cause,
            message: (cause == UpdateGatewayCause::NotFound)
                .then(|| GitHubUpdateGateway::NOT_FOUND_MESSAGE.to_owned()),
        });
    }
    let payload: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| UpdateGatewayError::new(UpdateGatewayCause::InvalidResponse))?;
    Ok(select_latest_release(&payload))
}

/// Reference `GitHubUpdateGateway.fetch_update` response branches, in the order
/// it takes them: the rate limit is read from the header on any status, so an
/// exhausted budget reports the limit rather than the status it arrived with.
#[must_use]
fn github_status_cause(
    status: u16,
    rate_limit_remaining: Option<&str>,
) -> Option<UpdateGatewayCause> {
    if status == 429 || rate_limit_remaining.is_some_and(|value| value == "0") {
        return Some(UpdateGatewayCause::TooManyRequests);
    }
    match status {
        403 => Some(UpdateGatewayCause::Forbidden),
        404 => Some(UpdateGatewayCause::NotFound),
        status if (400..600).contains(&status) => Some(UpdateGatewayCause::ErrorResponse),
        _ => None,
    }
}

/// Reference `GitHubUpdateGateway.fetch_update` selection: the most recently
/// published release that is neither a draft nor a prerelease, both flags read
/// for their truthiness.
///
/// A payload that is not a list of releases reads as no update rather than as
/// an error, which is where this port stops short of the reference: the
/// reference reaches an unhandled attribute error there.
#[must_use]
pub fn select_latest_release(payload: &serde_json::Value) -> Option<String> {
    let mut releases = payload
        .as_array()?
        .iter()
        .filter(|release| {
            !python_truthy(release.get("prerelease")) && !python_truthy(release.get("draft"))
        })
        .map(|release| {
            (
                release
                    .get("published_at")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default(),
                release,
            )
        })
        .collect::<Vec<_>>();
    // Python's `sorted` is stable, so releases published at the same instant
    // keep the order the API returned them in.
    releases.sort_by(|left, right| right.0.cmp(left.0));
    releases.into_iter().find_map(|(_, release)| {
        release
            .get("tag_name")
            .and_then(serde_json::Value::as_str)
            .and_then(extract_release_version)
    })
}

/// Reference `GitHubUpdateGateway._extract_version`: one optional `v` prefix
/// around a tag stripped of Python whitespace, and an empty tag is no version.
#[must_use]
fn extract_release_version(tag_name: &str) -> Option<String> {
    let tag = vibe_core::pycompat::strip(tag_name);
    let version = tag.strip_prefix(['v', 'V']).unwrap_or(tag);
    (!version.is_empty()).then(|| version.to_owned())
}

/// Python truthiness of one JSON value.
fn python_truthy(value: Option<&serde_json::Value>) -> bool {
    match value {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(flag)) => *flag,
        Some(serde_json::Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(serde_json::Value::String(text)) => !text.is_empty(),
        Some(serde_json::Value::Array(items)) => !items.is_empty(),
        Some(serde_json::Value::Object(entries)) => !entries.is_empty(),
    }
}

#[cfg(test)]
mod github_tests;
