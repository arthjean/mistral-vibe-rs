//! Reference `PyPIUpdateGateway`
//! (`vibe/cli/update_notifier/adapters/pypi_update_gateway.py`).

use super::gateway::{
    GATEWAY_TIMEOUT, UpdateFetch, UpdateGateway, UpdateGatewayCause, UpdateGatewayError, exchange,
    gateway_client,
};
use super::version::{Version, artifact_version};

/// Reference `PyPIUpdateGateway`, including its non-yanked selection rule.
pub struct PyPiUpdateGateway {
    project: String,
    base_url: String,
    client: reqwest::Client,
}

impl PyPiUpdateGateway {
    pub fn new(project: impl Into<String>) -> Result<Self, UpdateGatewayError> {
        Self::with_base_url(project, "https://pypi.org")
    }

    pub fn with_base_url(
        project: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self, UpdateGatewayError> {
        Ok(Self {
            project: project.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client: gateway_client(GATEWAY_TIMEOUT)?,
        })
    }
}

impl UpdateGateway for PyPiUpdateGateway {
    fn fetch_update(&self) -> UpdateFetch<'_> {
        Box::pin(async move {
            let url = format!("{}/simple/{}/", self.base_url, self.project);
            let request = self
                .client
                .get(url)
                .header("Accept", "application/vnd.pypi.simple.v1+json");
            let (status, _, body) = exchange(request).await?;
            resolve_pypi_response(status, &body)
        })
    }
}

/// Reference `PyPIUpdateGateway.fetch_update` once the response is in hand:
/// the status first, then the JSON, then the selection.
pub fn resolve_pypi_response(
    status: u16,
    body: &[u8],
) -> Result<Option<String>, UpdateGatewayError> {
    if let Some(cause) = status_cause(status) {
        return Err(UpdateGatewayError::new(cause));
    }
    let payload: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| UpdateGatewayError::new(UpdateGatewayCause::InvalidResponse))?;
    Ok(select_latest_version(&payload))
}

/// Reference `_STATUS_CAUSES` plus the generic error branch.
#[must_use]
pub fn status_cause(status: u16) -> Option<UpdateGatewayCause> {
    match status {
        404 => Some(UpdateGatewayCause::NotFound),
        403 => Some(UpdateGatewayCause::Forbidden),
        429 => Some(UpdateGatewayCause::TooManyRequests),
        status if (400..600).contains(&status) => Some(UpdateGatewayCause::ErrorResponse),
        _ => None,
    }
}

/// Reference `PyPIUpdateGateway.fetch_update`: the highest listed version that
/// still has a non-yanked artifact, reported in its normalized spelling.
///
/// `versions` and `files` are iterated the way the reference iterates
/// `data.get(...) or []`: a list yields its items, a string its characters and
/// an object its keys. Each listed version goes through `str()` before it is
/// parsed, so a number is a candidate too.
#[must_use]
pub fn select_latest_version(payload: &serde_json::Value) -> Option<String> {
    let published = python_iter(payload.get("files"))
        .filter(|file| {
            file.is_object() && file.get("yanked") != Some(&serde_json::Value::Bool(true))
        })
        .filter_map(|file| {
            file.get("filename")
                .and_then(serde_json::Value::as_str)
                .and_then(artifact_version)
        })
        .collect::<Vec<_>>();
    let mut candidates = python_iter(payload.get("versions"))
        .filter_map(|raw| Version::parse(&python_str(&raw)?))
        .collect::<Vec<_>>();
    // Python's `sorted(..., reverse=True)` is stable, so equal versions keep
    // the order the index listed them in.
    candidates.sort_by(|left, right| right.cmp(left));
    candidates
        .into_iter()
        .find(|version| published.contains(version))
        .map(|version| version.to_string())
}

/// What `for item in value or []` visits. A truthy scalar the reference cannot
/// iterate raises there; this port visits nothing, which reads as no update.
fn python_iter(value: Option<&serde_json::Value>) -> impl Iterator<Item = serde_json::Value> {
    let items = match value {
        Some(serde_json::Value::Array(items)) => items.clone(),
        Some(serde_json::Value::String(text)) => text
            .chars()
            .map(|character| serde_json::Value::String(character.to_string()))
            .collect(),
        Some(serde_json::Value::Object(entries)) => entries
            .keys()
            .map(|key| serde_json::Value::String(key.clone()))
            .collect(),
        _ => Vec::new(),
    };
    items.into_iter()
}

/// `str(value)` for the JSON values that can spell a version: a string is
/// itself and a number is its Python spelling. Every other JSON value prints
/// as something no version grammar accepts.
fn python_str(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(_) | serde_json::Value::Number(_) => {
            Some(vibe_core::pycompat::str_json(value))
        }
        _ => None,
    }
}

#[cfg(test)]
mod pypi_tests;
