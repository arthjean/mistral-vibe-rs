//! The read side of the registry service: the catalog, the pins a newer
//! version has overtaken, and the ledger of versions already announced.
//!
//! Reference `vibe/core/skills/registry/_service.py` (`list_catalog`,
//! `check_updates`, `check_new_versions`, `_resolve_latest_versions`) and
//! `_notify.py`. The endpoint is the Mistral provider's API base and the
//! credential its variable resolves to (`_resolve_endpoint`); a caller without
//! one never reaches the network.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::client::{
    RegistrySkillsClient, RegistrySkillsError, RegistryTransport, TransportResponse,
};
use super::manifest::{ManifestVersion, SkillManifest};

/// The catalog page size the reference requests.
const PAGE_SIZE: u32 = 100;
/// The reference client's request timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Where the registry lives and the credential it is called with.
#[derive(Debug, Clone)]
pub struct RegistryEndpoint {
    pub api_base: String,
    pub api_key: String,
}

/// One catalog row (reference `CatalogItem`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogItem {
    pub name: String,
    pub skill_id: String,
    pub description: String,
    pub latest_version: i64,
    pub sharing_scope: String,
}

/// A frozen pin the registry has a newer version of (reference `SkillUpdate`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillUpdate {
    pub name: String,
    pub current_version: i64,
    pub latest_version: i64,
}

/// The HTTP transport: a bearer credential and JSON accepted.
pub(super) struct HttpTransport {
    client: reqwest::Client,
    api_key: String,
}

impl RegistryTransport for HttpTransport {
    async fn get(
        &self,
        url: &str,
        params: &[(String, String)],
    ) -> Result<TransportResponse, String> {
        let response = self
            .client
            .get(url)
            .query(params)
            .bearer_auth(&self.api_key)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let body = response.bytes().await.map_err(|error| error.to_string())?;
        Ok(TransportResponse {
            status,
            body: body.to_vec(),
        })
    }
}

pub(super) fn open_client(
    endpoint: &RegistryEndpoint,
) -> Result<RegistrySkillsClient<HttpTransport>, RegistrySkillsError> {
    let http = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| RegistrySkillsError {
            reason: error.to_string(),
            status: None,
        })?;
    let mut client = RegistrySkillsClient::new(&endpoint.api_base);
    client.open(HttpTransport {
        client: http,
        api_key: endpoint.api_key.clone(),
    });
    Ok(client)
}

/// The registry's catalog, one row per item that resolves to a name.
///
/// # Errors
///
/// Any registry failure, which the caller reports as an unloaded catalog.
pub async fn list_catalog(
    endpoint: &RegistryEndpoint,
) -> Result<Vec<CatalogItem>, RegistrySkillsError> {
    let client = open_client(endpoint)?;
    let items = client.list_catalog(PAGE_SIZE).await?;
    Ok(items
        .into_iter()
        .filter_map(|item| {
            let name = item.resolved_name()?;
            let latest_version = if item.metadata.latest_version != 0 {
                item.metadata.latest_version
            } else {
                item.version
            };
            Some(CatalogItem {
                name,
                description: item.resolved_description(),
                skill_id: item.skill_id,
                latest_version,
                sharing_scope: item.metadata.sharing_scope,
            })
        })
        .collect())
}

/// The newest version of each skill, one request per skill; a skill the
/// registry no longer has is left out.
async fn latest_versions(
    client: &RegistrySkillsClient<HttpTransport>,
    skill_ids: &BTreeSet<String>,
) -> Result<BTreeMap<String, i64>, RegistrySkillsError> {
    let mut latest = BTreeMap::new();
    for skill_id in skill_ids {
        let item = match client.get_skill(skill_id, None, None).await {
            Ok(item) => item,
            Err(error) if error.status == Some(404) => continue,
            Err(error) => return Err(error),
        };
        let version = if item.metadata.latest_version > 0 {
            item.metadata.latest_version
        } else {
            item.version
        };
        latest.insert(skill_id.clone(), version);
    }
    Ok(latest)
}

/// The frozen pins, over `manifests` in precedence order, the registry holds
/// a newer version of. A tracking pin adopts every release and is never
/// behind; a pin whose id is not a safe path segment is skipped. A registry
/// failure answers no update, as the reference logs it and moves on.
pub async fn check_updates(
    endpoint: &RegistryEndpoint,
    manifests: &[SkillManifest],
) -> Vec<SkillUpdate> {
    let frozen = manifests
        .iter()
        .flat_map(|manifest| &manifest.skills)
        .filter(|entry| {
            matches!(entry.version, ManifestVersion::Frozen(_))
                && super::store::is_plain_component(&entry.skill_id)
        })
        .map(|entry| entry.skill_id.clone())
        .collect::<BTreeSet<_>>();
    if frozen.is_empty() {
        return Vec::new();
    }
    let Ok(client) = open_client(endpoint) else {
        return Vec::new();
    };
    let Ok(latest) = latest_versions(&client, &frozen).await else {
        return Vec::new();
    };
    let mut updates = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in manifests.iter().flat_map(|manifest| &manifest.skills) {
        if !seen.insert(entry.name.clone()) {
            continue;
        }
        let ManifestVersion::Frozen(current) = entry.version else {
            continue;
        };
        if let Some(&newest) = latest.get(&entry.skill_id)
            && newest > current
        {
            updates.push(SkillUpdate {
                name: entry.name.clone(),
                current_version: current,
                latest_version: newest,
            });
        }
    }
    updates
}

/// The updates not announced before, which are recorded as announced so
/// the next session does not repeat them (reference `check_new_versions`).
pub async fn check_new_versions(
    endpoint: &RegistryEndpoint,
    manifests: &[SkillManifest],
    vibe_home: &Path,
) -> Vec<SkillUpdate> {
    let updates = check_updates(endpoint, manifests).await;
    if updates.is_empty() {
        return updates;
    }
    let mut ids = BTreeMap::new();
    for entry in manifests.iter().flat_map(|manifest| &manifest.skills) {
        ids.entry(entry.name.clone())
            .or_insert_with(|| entry.skill_id.clone());
    }
    let key = |update: &SkillUpdate| {
        ids.get(&update.name)
            .cloned()
            .unwrap_or_else(|| update.name.clone())
    };
    let seen = load_seen(vibe_home);
    let fresh = updates
        .into_iter()
        .filter(|update| {
            update.latest_version > seen.get(&key(update)).copied().unwrap_or_default()
        })
        .collect::<Vec<_>>();
    mark_seen(
        vibe_home,
        fresh
            .iter()
            .map(|update| (key(update), update.latest_version)),
    );
    fresh
}

/// The ledger of announced versions, in the global registry cache.
fn seen_path(vibe_home: &Path) -> PathBuf {
    vibe_home
        .join("skills-registry-cache")
        .join("seen-versions.json")
}

fn load_seen(vibe_home: &Path) -> BTreeMap<String, i64> {
    std::fs::read_to_string(seen_path(vibe_home))
        .ok()
        .and_then(|text| {
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text).ok()
        })
        .map(|entries| {
            entries
                .into_iter()
                .filter_map(|(id, version)| version.as_i64().map(|version| (id, version)))
                .collect()
        })
        .unwrap_or_default()
}

/// Merges `updates` into the ledger, the higher version winning. A write
/// that fails leaves the ledger as it was.
fn mark_seen(vibe_home: &Path, updates: impl IntoIterator<Item = (String, i64)>) {
    let mut current = load_seen(vibe_home);
    let mut changed = false;
    for (id, version) in updates {
        if version > current.get(&id).copied().unwrap_or_default() {
            current.insert(id, version);
            changed = true;
        }
    }
    if !changed {
        return;
    }
    let path = seen_path(vibe_home);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(&current) {
        let _ = std::fs::write(path, text);
    }
}
