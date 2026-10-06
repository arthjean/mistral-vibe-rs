//! The session-start sync of the registry's pins.
//!
//! Reference `refresh_registry_skills`, `publish_local_pins` and their helpers
//! in `vibe/core/skills/registry/_service.py`. Every pin of the global and the
//! project manifests is resolved to a concrete version and every missing body
//! is downloaded into the shared store, so a teammate who clones a repository
//! with a committed `.vibe/skills.toml` gets the bodies; then this
//! repository's active versions are recorded in the ledger and the store is
//! pruned against the union of every repository's record. When no pin needs
//! the registry, nothing is fetched and the prune still runs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use futures_util::StreamExt as _;

use super::client::{RegistrySkillsClient, RegistrySkillsError, RegistryTransport};
use super::manifest::{self, ManifestEntry, ManifestVersion, REGISTRY_LATEST_ALIAS};
use super::service::{RegistryEndpoint, open_client};
use super::{ledger, pins, store};

/// How many versions download at once (reference `_MATERIALIZE_CONCURRENCY`).
const MATERIALIZE_CONCURRENCY: usize = 8;

/// How a sync ended (reference `RegistrySyncStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStatus {
    /// The experiment is off, no endpoint resolves, or no pin needed the
    /// registry.
    Skipped,
    /// The registry or the store failed; the session keeps the cached bodies.
    Failed,
    Ok,
}

/// What a sync did (reference `RegistrySyncResult`): how many versions it
/// wrote and how many it could not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncResult {
    pub status: SyncStatus,
    pub written: usize,
    pub skipped: usize,
}

impl SyncResult {
    const fn status(status: SyncStatus) -> Self {
        Self {
            status,
            written: 0,
            skipped: 0,
        }
    }
}

/// The repository a sync runs for: the Vibe home holding the global manifest,
/// the store and the ledger, and the session's project roots.
#[derive(Debug, Clone, Copy)]
pub struct SyncScope<'a> {
    pub vibe_home: &'a Path,
    pub roots: &'a [PathBuf],
}

impl SyncScope<'_> {
    fn store_root(&self) -> PathBuf {
        store::store_root(self.vibe_home)
    }

    fn project_manifests(&self) -> Vec<PathBuf> {
        manifest::project_manifest_paths(self.vibe_home, self.roots)
    }

    /// The global manifest, then the project manifests.
    fn manifest_paths(&self) -> Vec<PathBuf> {
        let mut paths = vec![manifest::global_manifest_path(self.vibe_home)];
        paths.extend(self.project_manifests());
        paths
    }

    fn entries(&self, paths: &[PathBuf]) -> Vec<ManifestEntry> {
        paths
            .iter()
            .flat_map(|path| manifest::load(path).manifest.skills)
            .filter(|entry| store::is_plain_component(&entry.skill_id))
            .collect()
    }
}

/// A registry or store failure, which the result reports as
/// [`SyncStatus::Failed`]: the reference logs the reason and keeps the cached
/// bodies.
struct SyncFailure;

impl From<RegistrySkillsError> for SyncFailure {
    fn from(_: RegistrySkillsError) -> Self {
        Self
    }
}

impl From<store::StoreError> for SyncFailure {
    fn from(_: store::StoreError) -> Self {
        Self
    }
}

/// Reference `refresh_registry_skills`: the session-start sync, skipped while
/// the experiment is off or no Mistral endpoint resolves.
pub async fn refresh_registry_skills(
    enabled: bool,
    endpoint: Option<&RegistryEndpoint>,
    scope: SyncScope<'_>,
) -> SyncResult {
    if !enabled {
        return SyncResult::status(SyncStatus::Skipped);
    }
    let Some(endpoint) = endpoint else {
        return SyncResult::status(SyncStatus::Skipped);
    };
    match open_client(endpoint) {
        Ok(client) => refresh_with(&client, scope).await,
        Err(_) => SyncResult::status(SyncStatus::Failed),
    }
}

/// [`refresh_registry_skills`] past its two gates, over any transport.
pub async fn refresh_with<T: RegistryTransport + Sync>(
    client: &RegistrySkillsClient<T>,
    scope: SyncScope<'_>,
) -> SyncResult {
    if !needs_sync(scope) {
        let active = scoped_active(scope, &scope.manifest_paths());
        prune_shared(scope, &active);
        return SyncResult::status(SyncStatus::Skipped);
    }
    let synced = async {
        let latest = latest_versions(client, &latest_pinned_ids(scope)).await?;
        sync_pins(client, &latest, scope).await
    }
    .await;
    match synced {
        Ok((active, written, skipped)) => {
            prune_shared(scope, &active);
            SyncResult {
                status: SyncStatus::Ok,
                written,
                skipped,
            }
        }
        Err(SyncFailure) => SyncResult::status(SyncStatus::Failed),
    }
}

/// Reference `publish_local_pins`: this repository's pins recorded again, so
/// a pin added or removed mid-session is visible to a sibling repository's
/// prune before the next sync.
pub fn publish_local_pins(scope: SyncScope<'_>) {
    record_pins(scope, &ledger::repo_key(scope.roots));
}

/// Reference `_needs_sync`: any alias pin, which only the registry resolves,
/// or any frozen pin whose version is not on disk yet.
fn needs_sync(scope: SyncScope<'_>) -> bool {
    let root = scope.store_root();
    scope
        .entries(&scope.manifest_paths())
        .iter()
        .any(|entry| match &entry.version {
            ManifestVersion::Alias(_) => true,
            ManifestVersion::Frozen(version) => {
                !store::is_materialized(&root, &entry.skill_id, *version)
            }
        })
}

/// Reference `_latest_pinned_skill_ids`: the ids pinned to `latest` anywhere.
fn latest_pinned_ids(scope: SyncScope<'_>) -> BTreeSet<String> {
    scope
        .entries(&scope.manifest_paths())
        .into_iter()
        .filter(|entry| entry.alias() == Some(REGISTRY_LATEST_ALIAS))
        .map(|entry| entry.skill_id)
        .collect()
}

/// Reference `_resolve_latest_versions`: the newest version of each id, one
/// request per id; an id the registry no longer has is left out, and any
/// other failure fails the sync.
async fn latest_versions<T: RegistryTransport + Sync>(
    client: &RegistrySkillsClient<T>,
    skill_ids: &BTreeSet<String>,
) -> Result<BTreeMap<String, i64>, RegistrySkillsError> {
    let mut latest = BTreeMap::new();
    for skill_id in skill_ids {
        match client.get_skill(skill_id, None, None).await {
            Ok(item) => {
                let version = if item.metadata.latest_version > 0 {
                    item.metadata.latest_version
                } else {
                    item.version
                };
                latest.insert(skill_id.clone(), version);
            }
            Err(error) if error.status == Some(404) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(latest)
}

type Synced = (BTreeSet<(String, i64)>, usize, usize);

/// Reference `_sync_pins`: every pin resolved and materialized, answering the
/// active versions and how many versions were written and skipped.
async fn sync_pins<T: RegistryTransport + Sync>(
    client: &RegistrySkillsClient<T>,
    latest_by_id: &BTreeMap<String, i64>,
    scope: SyncScope<'_>,
) -> Result<Synced, SyncFailure> {
    let root = scope.store_root();
    let mut active = BTreeSet::new();
    let mut targets: Vec<(String, i64, String)> = Vec::new();
    let mut resolved_aliases: Vec<(String, String, i64)> = Vec::new();
    let mut aliases_seen: BTreeSet<(String, String)> = BTreeSet::new();
    let (mut written, mut skipped) = (0, 0);
    let mut add_target =
        |active: &mut BTreeSet<(String, i64)>, id: &str, version: i64, name: &str| {
            active.insert((id.to_owned(), version));
            if !targets
                .iter()
                .any(|(seen, known, _)| seen == id && *known == version)
            {
                targets.push((id.to_owned(), version, name.to_owned()));
            }
        };
    for entry in scope.entries(&scope.manifest_paths()) {
        let id = entry.skill_id.as_str();
        match &entry.version {
            ManifestVersion::Frozen(version) => add_target(&mut active, id, *version, &entry.name),
            ManifestVersion::Alias(alias) if alias != REGISTRY_LATEST_ALIAS => {
                if !aliases_seen.insert((id.to_owned(), alias.clone())) {
                    continue;
                }
                let (w, s) = sync_alias(
                    client,
                    scope,
                    &entry,
                    alias,
                    &mut active,
                    &mut resolved_aliases,
                )
                .await?;
                written += w;
                skipped += s;
            }
            ManifestVersion::Alias(_) => {
                let version = latest_by_id
                    .get(id)
                    .copied()
                    .or_else(|| store::latest_materialized(&root, id).ok().flatten());
                if let Some(version) = version {
                    add_target(&mut active, id, version, &entry.name);
                }
            }
        }
    }
    for (skill_id, alias, version) in &resolved_aliases {
        pins::record_resolved(scope.vibe_home, skill_id, alias, *version);
    }
    let (w, s) = materialize_missing(client, &root, targets).await?;
    Ok((prune_safe(&root, &active), written + w, skipped + s))
}

/// Reference `_sync_alias`: a custom alias resolved by the registry and its
/// version materialized; a registry failure or an empty body keeps the
/// version the alias last resolved to, else the newest on disk.
async fn sync_alias<T: RegistryTransport + Sync>(
    client: &RegistrySkillsClient<T>,
    scope: SyncScope<'_>,
    entry: &ManifestEntry,
    alias: &str,
    active: &mut BTreeSet<(String, i64)>,
    resolved_aliases: &mut Vec<(String, String, i64)>,
) -> Result<(usize, usize), SyncFailure> {
    let root = scope.store_root();
    let id = entry.skill_id.as_str();
    let fallback = || {
        pins::resolved_alias(scope.vibe_home, id, alias)
            .or_else(|| store::latest_materialized(&root, id).ok().flatten())
    };
    let Ok(item) = client.get_skill(id, None, Some(alias)).await else {
        if let Some(version) = fallback() {
            active.insert((id.to_owned(), version));
        }
        return Ok((0, 0));
    };
    resolved_aliases.push((id.to_owned(), alias.to_owned(), item.version));
    if store::is_materialized(&root, id, item.version) {
        active.insert((id.to_owned(), item.version));
        return Ok((0, 0));
    }
    if store::materialize(&root, &item, &entry.name)?.is_none() {
        if let Some(version) = fallback() {
            active.insert((id.to_owned(), version));
        }
        return Ok((0, 1));
    }
    active.insert((id.to_owned(), item.version));
    Ok((1, 0))
}

/// Reference `_materialize_missing`: every target not on disk fetched at its
/// version, a few at a time. A fetch that fails or a body that is empty is
/// skipped; a store that cannot be written fails the sync.
async fn materialize_missing<T: RegistryTransport + Sync>(
    client: &RegistrySkillsClient<T>,
    root: &Path,
    targets: Vec<(String, i64, String)>,
) -> Result<(usize, usize), SyncFailure> {
    let pending = targets
        .into_iter()
        .filter(|(id, version, _)| !store::is_materialized(root, id, *version))
        .collect::<Vec<_>>();
    let outcomes = futures_util::stream::iter(pending)
        .map(|(id, version, name)| async move {
            let Ok(item) = client.get_skill(&id, Some(version), None).await else {
                return Ok(false);
            };
            store::materialize(root, &item, &name).map(|stored| stored.is_some())
        })
        .buffer_unordered(MATERIALIZE_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    let mut written = 0;
    let mut total = 0;
    for outcome in outcomes {
        total += 1;
        if outcome? {
            written += 1;
        }
    }
    Ok((written, total - written))
}

/// Reference `_prune_safe`: the active set resolved to versions on disk. A
/// target that did not download stands in for the newest version on disk, so
/// a failed download never prunes a skill's only usable body.
fn prune_safe(root: &Path, active: &BTreeSet<(String, i64)>) -> BTreeSet<(String, i64)> {
    active
        .iter()
        .filter_map(|(id, version)| {
            if store::is_materialized(root, id, *version) {
                Some((id.clone(), *version))
            } else {
                store::latest_materialized(root, id)
                    .ok()
                    .flatten()
                    .map(|fallback| (id.clone(), fallback))
            }
        })
        .collect()
}

/// Reference `_pinned_version`: the version a pin currently claims, a custom
/// alias reading the version it last resolved to.
fn claimed_version(scope: SyncScope<'_>, root: &Path, entry: &ManifestEntry) -> Option<i64> {
    let latest = || {
        store::latest_materialized(root, &entry.skill_id)
            .ok()
            .flatten()
    };
    match &entry.version {
        ManifestVersion::Frozen(version) => Some(*version),
        ManifestVersion::Alias(alias) if alias == REGISTRY_LATEST_ALIAS => latest(),
        ManifestVersion::Alias(alias) => {
            pins::resolved_alias(scope.vibe_home, &entry.skill_id, alias).or_else(latest)
        }
    }
}

/// Reference `_scoped_active`: the versions the manifests at `paths` claim.
fn scoped_active(scope: SyncScope<'_>, paths: &[PathBuf]) -> BTreeSet<(String, i64)> {
    let root = scope.store_root();
    scope
        .entries(paths)
        .iter()
        .filter_map(|entry| {
            claimed_version(scope, &root, entry).map(|version| (entry.skill_id.clone(), version))
        })
        .collect()
}

/// Reference `_record_pins`: the global manifest's claims recorded under
/// `global` and, for a repository, its project manifests' claims under its
/// own key. Each scope records what it pins itself, so a pin dropped in one
/// scope leaves the union on the next sync.
fn record_pins(scope: SyncScope<'_>, key: &str) {
    let root = scope.store_root();
    let global = prune_safe(
        &root,
        &scoped_active(scope, &[manifest::global_manifest_path(scope.vibe_home)]),
    );
    let _ = ledger::record(scope.vibe_home, ledger::GLOBAL_KEY, &global);
    if key != ledger::GLOBAL_KEY {
        let project = prune_safe(&root, &scoped_active(scope, &scope.project_manifests()));
        let _ = ledger::record(scope.vibe_home, key, &project);
    }
}

/// Reference `_prune_shared`: this repository's claims recorded, then the
/// store pruned against the union of every record plus this sync's active
/// set, re-reading the union before the first removal.
fn prune_shared(scope: SyncScope<'_>, active: &BTreeSet<(String, i64)>) {
    let root = scope.store_root();
    let safe = prune_safe(&root, active);
    record_pins(scope, &ledger::repo_key(scope.roots));
    let mut keep = ledger::union(scope.vibe_home);
    keep.extend(safe);
    let recheck = || ledger::union(scope.vibe_home);
    let _ = store::prune(&root, &keep, Some(&recheck));
}
