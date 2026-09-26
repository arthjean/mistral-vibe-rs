//! The skills browser's methods: the reads (`skills/installed`,
//! `skills/catalog`, `skills/updates`, `skills/versions`, `skills/detail`)
//! and the mutations that import, repin, remove, convert and toggle a skill.
//!
//! Reference `SkillsController` (`vibe/app_server/_skills_service.py`) and
//! `project_installed_skill_summaries` (`vibe/app_server/_projection.py`).
//! The registry half calls the Mistral provider's endpoint through
//! `vibe_core::skills::registry::service`.

use serde_json::{Value, json};
use toml::Value as TomlValue;
use vibe_core::config::DotenvValues;
use vibe_core::extensions::discover_extensions;
use vibe_core::matching::NameFilter;
use vibe_core::skills::registry::manifest::{self, SkillManifest};
use vibe_core::skills::registry::pins::{self, PinTarget, SkillScope};
use vibe_core::skills::registry::service::{self, RegistryEndpoint};
use vibe_core::skills::{SkillSource, skill_summary};
use vibe_core::worktree::python_repr;
use vibe_protocol::ProtocolErrorCode;

use super::{MISTRAL_KEY, WorkspaceService};

impl WorkspaceService {
    /// One row per skill the session could load, builtins aside, disabled
    /// ones included and marked, so a browser can turn them back on.
    ///
    /// `enabled` is whether the session loads the skill and `locked` whether
    /// a toggle could change that: an allowlist decides alone and locks every
    /// row, a skill a pattern disables, rather than its own name, stays
    /// disabled whatever a toggle writes, and so does one only another layer
    /// than the writable one disables, since `disabled_skills` concatenates
    /// across layers.
    #[must_use]
    pub fn skills_installed(&self) -> Value {
        let mut roots = self.discovery_roots.clone();
        let mut discovery =
            self.skill_discovery(&self.paths.working_directory, self.project_trusted);
        let allowed = std::mem::take(&mut discovery.enabled);
        let disabled = std::mem::take(&mut discovery.disabled);
        roots.skills = discovery;
        let catalog = discover_extensions(
            &roots,
            Default::default(),
            Default::default(),
            Default::default(),
        );
        let allow = NameFilter::new(&allowed);
        let deny = NameFilter::new(&disabled);
        let own = self
            .writable_disabled_skills()
            .unwrap_or_else(|| disabled.clone());
        let rows = catalog
            .skills
            .values()
            .filter(|skill| skill.source != SkillSource::Builtin)
            .map(|skill| {
                let name = skill.name.as_str();
                let (enabled, locked) = if !allowed.is_empty() {
                    (allow.matches(name), true)
                } else if disabled.is_empty() {
                    (true, false)
                } else {
                    let others = disabled
                        .iter()
                        .filter(|pattern| pattern.as_str() != name)
                        .collect::<Vec<_>>();
                    let denied = deny.matches(name);
                    let locked = NameFilter::new(&others).matches(name)
                        || (denied && !own.iter().any(|entry| entry == name));
                    (!denied, locked)
                };
                let mut row = skill_summary(skill);
                row["enabled"] = json!(enabled);
                row["locked"] = json!(locked);
                row
            })
            .collect::<Vec<_>>();
        json!({"skills": rows})
    }

    /// The registry catalog with the frozen pins it has overtaken. No
    /// credential answers an empty, loaded catalog marked unauthenticated; a
    /// registry that fails answers an empty catalog marked unloaded.
    pub async fn skills_catalog(&self) -> Value {
        let project_available = !self.project_manifest_paths().is_empty();
        let Some(endpoint) = self.registry_endpoint() else {
            return json!({
                "skills": [],
                "updates": {},
                "loaded": true,
                "projectAvailable": project_available,
                "authenticated": false,
            });
        };
        let Ok(catalog) = service::list_catalog(&endpoint).await else {
            return json!({
                "skills": [],
                "updates": {},
                "loaded": false,
                "projectAvailable": project_available,
                "authenticated": true,
            });
        };
        let updates = service::check_updates(&endpoint, &self.update_manifests())
            .await
            .into_iter()
            .map(|update| (update.name, json!(update.latest_version)))
            .collect::<serde_json::Map<_, _>>();
        let skills = catalog
            .into_iter()
            .map(|item| {
                json!({
                    "name": item.name,
                    "skillId": item.skill_id,
                    "description": item.description,
                    "latestVersion": item.latest_version,
                    "sharingScope": item.sharing_scope,
                })
            })
            .collect::<Vec<_>>();
        json!({
            "skills": skills,
            "updates": updates,
            "loaded": true,
            "projectAvailable": project_available,
            "authenticated": true,
        })
    }

    /// The overtaken pins not announced before, now recorded as announced.
    pub async fn skills_updates(&self) -> Value {
        let updates = match self.registry_endpoint() {
            Some(endpoint) => {
                service::check_new_versions(
                    &endpoint,
                    &self.update_manifests(),
                    &self.paths.vibe_home,
                )
                .await
            }
            None => Vec::new(),
        };
        let updates = updates
            .into_iter()
            .map(|update| {
                json!({
                    "name": update.name,
                    "currentVersion": update.current_version,
                    "latestVersion": update.latest_version,
                })
            })
            .collect::<Vec<_>>();
        json!({"updates": updates})
    }

    /// Reference `SkillsController._versions`.
    pub async fn skills_versions(&self, skill_id: &str) -> Value {
        let versions = pins::skill_versions(self.registry_endpoint().as_ref(), skill_id).await;
        let versions = versions
            .into_iter()
            .map(|version| json!({"version": version.version, "aliases": version.aliases}))
            .collect::<Vec<_>>();
        json!({"versions": versions})
    }

    /// Reference `SkillsController._detail`: the version's registry object,
    /// else its body alone, else neither.
    pub async fn skills_detail(&self, skill_id: &str, version: Option<i64>) -> Value {
        let endpoint = self.registry_endpoint();
        if let Some(detail) = pins::skill_details(endpoint.as_ref(), skill_id, version).await {
            return json!({
                "detail": {
                    "name": detail.name,
                    "skillId": detail.skill_id,
                    "version": detail.version,
                    "body": detail.body,
                    "description": detail.description,
                    "createdBy": detail.created_by,
                    "createdAt": detail.created_at,
                    "lastModifiedAt": detail.last_modified_at,
                    "sharingScope": detail.sharing_scope,
                    "latestVersion": detail.latest_version,
                    "versionCreatedAt": detail.version_created_at,
                    "aliases": detail.aliases,
                    "notes": detail.notes,
                },
                "body": null,
            });
        }
        let body = pins::skill_body(endpoint.as_ref(), skill_id, version)
            .await
            .ok();
        json!({"detail": null, "body": body})
    }

    /// One of the skills browser's mutations, answering, for
    /// `skills/convertLocal`, whether a local skill was written.
    ///
    /// # Errors
    ///
    /// The refusal the reference answers: `invalid_params` with the registry's
    /// reason, or with the toggle a name cannot take.
    pub async fn skills_mutation(
        &self,
        method: &str,
        params: &serde_json::Map<String, Value>,
    ) -> Result<Option<bool>, (ProtocolErrorCode, String)> {
        let text = |key: &str| params.get(key).and_then(Value::as_str);
        let invalid = |reason: String| (ProtocolErrorCode::InvalidParams, reason);
        let scope = match text("scope") {
            Some("project") => SkillScope::Project,
            _ => SkillScope::Global,
        };
        let name = text("name").unwrap_or_default();
        let version = params.get("version").and_then(Value::as_i64);
        let roots = if self.project_trusted {
            self.config.harness_files().project_roots()
        } else {
            Vec::new()
        };
        let target = PinTarget {
            vibe_home: &self.paths.vibe_home,
            roots: &roots,
        };
        let endpoint = self.registry_endpoint();
        let endpoint = endpoint.as_ref();
        match method {
            "skills/import" => {
                pins::import_skill(
                    endpoint,
                    &target,
                    text("skillId").unwrap_or_default(),
                    version,
                    text("alias"),
                    scope,
                )
                .await
                .map_err(|error| invalid(error.reason))?;
            }
            "skills/setVersion" => {
                pins::repin_skill(endpoint, &target, name, version, None, scope)
                    .await
                    .map_err(|error| invalid(error.reason))?;
            }
            "skills/setLatest" => {
                pins::repin_skill(endpoint, &target, name, None, None, scope)
                    .await
                    .map_err(|error| invalid(error.reason))?;
            }
            "skills/setAlias" => {
                pins::repin_skill(endpoint, &target, name, None, text("alias"), scope)
                    .await
                    .map_err(|error| invalid(error.reason))?;
            }
            "skills/remove" => {
                pins::remove_skill(&target, name, scope)
                    .map_err(|error| invalid(error.to_string()))?;
            }
            "skills/convertLocal" => {
                let converted = pins::convert_skill_to_local(&target, name, scope)
                    .map_err(|error| invalid(error.to_string()))?;
                return Ok(Some(converted.is_some()));
            }
            "skills/setEnabled" => {
                let installed = self.skills_installed();
                let row = installed["skills"]
                    .as_array()
                    .and_then(|rows| rows.iter().find(|row| row["name"] == name))
                    .ok_or_else(|| {
                        invalid(format!("no installed skill named {}", python_repr(name)))
                    })?;
                if row["locked"] == true {
                    return Err(invalid(format!(
                        "{} is fixed by configuration and cannot be toggled here",
                        python_repr(name)
                    )));
                }
                let enabled = params
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.config
                    .persist_skill_toggle(name, enabled)
                    .map_err(|error| {
                        (
                            ProtocolErrorCode::InternalError,
                            format!("Failed to update configuration: {error}"),
                        )
                    })?;
            }
            _ => {}
        }
        Ok(None)
    }

    /// Reference `_resolve_endpoint`: the active provider when it is a
    /// Mistral one, the first Mistral provider otherwise, and the credential
    /// its variable resolves to.
    fn registry_endpoint(&self) -> Option<RegistryEndpoint> {
        let snapshot = self.config.load().ok()?;
        let is_mistral = |provider: &toml::Table| {
            provider
                .get("backend")
                .and_then(TomlValue::as_str)
                .unwrap_or("mistral")
                == "mistral"
        };
        let provider = snapshot
            .active_provider()
            .filter(is_mistral)
            .or_else(|| snapshot.entries("providers").into_iter().find(is_mistral))?;
        let variable = provider
            .get("api_key_env_var")
            .and_then(TomlValue::as_str)
            .filter(|variable| !variable.is_empty())
            .unwrap_or(MISTRAL_KEY);
        let environ = DotenvValues::global(&self.paths.vibe_home).environment();
        let store = vibe_core::auth::KeyringStore::native();
        let api_key = vibe_core::auth::resolve_api_key(variable, &environ, &store)
            .filter(|key| !key.is_empty())?;
        let api_base = provider
            .get("api_base")
            .and_then(TomlValue::as_str)
            .unwrap_or_default()
            .to_owned();
        Some(RegistryEndpoint { api_base, api_key })
    }

    /// Reference `writable_disabled_skills`: the writable layer's own
    /// `disabled_skills`, or [`None`] when the configuration cannot be read.
    fn writable_disabled_skills(&self) -> Option<Vec<String>> {
        let snapshot = self.config.load().ok()?;
        Some(
            snapshot
                .target_values
                .get(&snapshot.selected_target)
                .and_then(|table| table.get("disabled_skills"))
                .and_then(TomlValue::as_array)
                .map(|names| {
                    names
                        .iter()
                        .filter_map(TomlValue::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        )
    }

    /// The project manifests of the session's project roots.
    fn project_manifest_paths(&self) -> Vec<std::path::PathBuf> {
        let roots = if self.project_trusted {
            self.config.harness_files().project_roots()
        } else {
            Vec::new()
        };
        manifest::project_manifest_paths(&self.paths.vibe_home, &roots)
    }

    /// The manifests updates are checked against, project ones first so a
    /// project pin's name wins over a global one.
    fn update_manifests(&self) -> Vec<SkillManifest> {
        self.project_manifest_paths()
            .into_iter()
            .chain([manifest::global_manifest_path(&self.paths.vibe_home)])
            .map(|path| manifest::load(&path).manifest)
            .collect()
    }
}
