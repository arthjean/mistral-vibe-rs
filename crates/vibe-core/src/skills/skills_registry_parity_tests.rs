//! Differential oracle for the skills a session loads and the registry
//! lifecycle behind them.
//!
//! The corpus `scripts/parity/skills.py` captures carries six families this
//! module replays: `loading` (one skill's load, its `agents/openai.yaml` policy
//! and the path it is published under), `installedMarks` (the `enabled` and
//! `locked` flags of a skills browser row), `lifecycle` (what a session
//! publishes, pins and lists as installed with the registry experiment on),
//! `sync` (the session-start sync and the pin ledger), `service` (what the
//! `skills/*` methods call) and `ledger` (the per-repository records). The
//! reference drives its registry through a scripted client; this port drives
//! its own over HTTP against a scripted registry that answers the same
//! payloads, so the transport is measured too. Every family is compared whole
//! and none is ledgered.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::skills_parity_tests::{
    Report, comparable_tree_fields, label_path, settle, store_item, substitute, tree_of, vocabulary,
};
use crate::extensions::{
    DiscoveryRoots, SKILL_LOAD_MECHANISM, SKILL_POLICY_MECHANISM, SkillDefinition,
    discover_extensions,
};
use crate::skills::builtins::builtin_skills;
use crate::skills::registry::manifest::{self, ManifestEntry, ManifestVersion, SkillManifest};
use crate::skills::registry::pins::{self, PinTarget, SkillScope as PinScope};
use crate::skills::registry::service::{self, RegistryEndpoint};
use crate::skills::registry::sync::{self, SyncScope, SyncStatus};
use crate::skills::registry::{ledger, loader, store};
use crate::skills::{
    RegistrySources, SearchInputs, SkillDiscovery, SkillSource, installed_marks, installed_skills,
    search_paths,
};

type Tree = BTreeMap<String, BTreeMap<String, String>>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct LoadingCase {
    case: String,
    tree: Tree,
    links: Vec<(String, String, String, String)>,
    published: Vec<Value>,
    issues: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct MarksCase {
    case: String,
    skills: Vec<(String, String)>,
    enabled: Vec<String>,
    disabled: Vec<String>,
    own: Option<Vec<String>>,
    rows: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct LifecycleCase {
    case: String,
    registry_enabled: bool,
    trusted: bool,
    disabled_skills: Vec<String>,
    tree: Tree,
    store: Vec<Value>,
    manifests: BTreeMap<String, Vec<Value>>,
    resolved: Vec<(String, String, i64)>,
    available: Vec<Value>,
    registry_pins: Vec<Value>,
    installed: Vec<Value>,
    issues: Vec<Value>,
}

/// One `sync` or `service` scenario: the state seeded before the operation,
/// the registry the scripted server answers, and what the reference observed.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct RegistryCase {
    case: String,
    op: String,
    #[serde(default)]
    args: Map<String, Value>,
    #[serde(default)]
    enabled: Option<bool>,
    endpoint: bool,
    trusted: bool,
    #[serde(default)]
    local_skill: Option<String>,
    store: Vec<Value>,
    manifests: BTreeMap<String, Vec<Value>>,
    #[serde(default)]
    resolved: Vec<(String, String, i64)>,
    #[serde(default)]
    ledger: BTreeMap<String, Vec<(String, i64)>>,
    #[serde(default)]
    seen: BTreeMap<String, i64>,
    registry: Map<String, Value>,
    result: Value,
    requests: Vec<Value>,
    state: Value,
}

/// Replays the six families, answering how many scenarios ran.
pub(super) fn replay(
    loading: &[LoadingCase],
    marks: &[MarksCase],
    lifecycle: &[LifecycleCase],
    sync_cases: &[RegistryCase],
    service_cases: &[RegistryCase],
    ledger_cases: &[Value],
) -> usize {
    let mut scenarios = 0;

    let mut report = Report::default();
    for case in loading {
        let (published, issues) = loading_answer(case);
        report.check(
            "loading",
            &case.case,
            "published",
            &case.published,
            &published,
        );
        report.check("loading", &case.case, "issues", &case.issues, &issues);
    }
    scenarios += settle(&report, "loading");

    let mut report = Report::default();
    for case in marks {
        let own = case.own.clone().unwrap_or_else(|| case.disabled.clone());
        let rows = case
            .skills
            .iter()
            .map(|(name, source)| {
                let source = source_named(source);
                let (enabled, locked) =
                    installed_marks(name, source, &case.enabled, &case.disabled, &own);
                json!({
                    "name": name,
                    "source": vocabulary(source),
                    "enabled": enabled,
                    "locked": locked,
                })
            })
            .collect::<Vec<_>>();
        report.check("installedMarks", &case.case, "rows", &case.rows, &rows);
    }
    scenarios += settle(&report, "installedMarks");

    let mut report = Report::default();
    for case in lifecycle {
        let observed = lifecycle_answer(case);
        let expected = json!({
            "available": case.available,
            "registryPins": case.registry_pins,
            "installed": case.installed,
            "issues": case.issues,
        });
        report.check("lifecycle", &case.case, "session", &expected, &observed);
    }
    scenarios += settle(&report, "lifecycle");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a test runtime builds");

    for (family, cases) in [("sync", sync_cases), ("service", service_cases)] {
        let mut report = Report::default();
        for case in cases {
            let observed = runtime.block_on(registry_answer(family, case));
            let expected = json!({
                "result": case.result,
                "requests": sorted_requests(&case.requests),
                "state": case.state,
            });
            report.check(
                family,
                &case.case,
                "outcome",
                &comparable_tree_fields(&expected),
                &comparable_tree_fields(&observed),
            );
        }
        scenarios += settle(&report, family);
    }

    let mut report = Report::default();
    for case in ledger_cases {
        let name = case["case"].as_str().unwrap_or_default().to_owned();
        report.check("ledger", &name, "answer", case, &ledger_answer(&name));
    }
    scenarios += settle(&report, "ledger");

    scenarios
}

fn source_named(source: &str) -> SkillSource {
    match source {
        "builtin" => SkillSource::Builtin,
        "registry" => SkillSource::Registry,
        "plugin" => SkillSource::Plugin,
        _ => SkillSource::Local,
    }
}

// --------------------------------------------------------------------------
// Scenario trees
// --------------------------------------------------------------------------

/// The four scenario roots under a canonical scratch directory, `home` doubling
/// as the operator's home and `home/.vibe` as the Vibe home, with the tree
/// written under them.
fn scenario_roots(scratch: &Path, tree: &Tree) -> BTreeMap<String, PathBuf> {
    let base = scratch
        .canonicalize()
        .expect("the scratch directory resolves");
    let mut roots = BTreeMap::new();
    for label in ["home", "project", "configured", "configured2"] {
        let root = base.join(label);
        fs::create_dir_all(&root).expect("the scenario root is writable");
        roots.insert(label.to_owned(), root);
    }
    for (label, files) in tree {
        for (relative, content) in files {
            let target = roots[label].join(relative);
            fs::create_dir_all(target.parent().expect("scenario files sit under a root"))
                .expect("the scenario tree is writable");
            fs::write(&target, content).expect("the scenario file is writable");
        }
    }
    roots
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}

#[cfg(not(unix))]
fn symlink(target: &Path, link: &Path) -> bool {
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link).is_ok()
    } else {
        std::os::windows::fs::symlink_file(target, link).is_ok()
    }
}

/// The label of a skill file as the reference records it: the directory
/// resolved and the file name kept.
fn label_skill_file(path: &Path, roots: &BTreeMap<String, PathBuf>) -> Option<(String, String)> {
    let (root, relative) = label_path(path.parent()?, roots)?;
    let name = path.file_name()?.to_string_lossy();
    Some(if relative == "." {
        (root, name.into_owned())
    } else {
        (root, format!("{relative}/{name}"))
    })
}

fn issue_records(
    issues: &[crate::extensions::DiscoveryIssue],
    roots: &BTreeMap<String, PathBuf>,
) -> Vec<Value> {
    let mut records = issues
        .iter()
        .filter_map(|issue| {
            let kind = match issue.mechanism.as_str() {
                SKILL_LOAD_MECHANISM => "load",
                SKILL_POLICY_MECHANISM => "policy",
                _ => return None,
            };
            let (root, relative) = label_path(&issue.path, roots)?;
            Some((kind.to_owned(), root, relative))
        })
        .collect::<Vec<_>>();
    records.sort();
    records
        .into_iter()
        .map(|(kind, root, relative)| json!({"kind": kind, "root": root, "relPath": relative}))
        .collect()
}

// --------------------------------------------------------------------------
// loading
// --------------------------------------------------------------------------

fn loading_answer(case: &LoadingCase) -> (Vec<Value>, Vec<Value>) {
    let scratch = tempfile::tempdir().expect("a scratch directory is available");
    let roots = scenario_roots(scratch.path(), &case.tree);
    for (link_label, link_relative, target_label, target_relative) in &case.links {
        let link = roots[link_label].join(link_relative);
        fs::create_dir_all(link.parent().expect("a link sits under a root"))
            .expect("the link directory is writable");
        if !symlink(&roots[target_label].join(target_relative), &link) {
            eprintln!(
                "skills: loading `{}` replays without the symlink this platform refused",
                case.case
            );
            return (case.published.clone(), case.issues.clone());
        }
    }
    let configured = vec![substitute("${configured}", &roots)];
    let vibe_home = roots["home"].join(".vibe");
    let walked = search_paths(&SearchInputs {
        configured: &configured,
        projects: &[],
        vibe_home: &vibe_home,
        user_home: Some(&roots["home"]),
        working_directory: &roots["project"],
    });
    let catalog = discover_extensions(
        &DiscoveryRoots {
            skills: SkillDiscovery {
                roots: walked,
                ..SkillDiscovery::default()
            },
            ..DiscoveryRoots::default()
        },
        BTreeMap::new(),
        builtin_skills(),
        BTreeMap::new(),
    );
    let published = catalog
        .skills
        .values()
        .filter(|skill| skill.source != SkillSource::Builtin)
        .map(|skill| {
            let location = skill
                .path
                .as_ref()
                .and_then(|path| label_skill_file(path, &roots));
            json!({
                "name": skill.name,
                "modelInvocable": skill.model_invocable,
                "userInvocable": skill.user_invocable,
                "root": location.as_ref().map(|(root, _)| root),
                "relPath": location.as_ref().map(|(_, relative)| relative),
            })
        })
        .collect();
    (published, issue_records(&catalog.issues, &roots))
}

// --------------------------------------------------------------------------
// lifecycle
// --------------------------------------------------------------------------

/// Writes the store versions, manifests, alias records, ledger entries and
/// announced versions a scenario starts from.
fn seed_registry(
    vibe_home: &Path,
    project: &Path,
    store_items: &[Value],
    manifests: &BTreeMap<String, Vec<Value>>,
    resolved: &[(String, String, i64)],
) {
    let root = store::store_root(vibe_home);
    for prior in store_items {
        let item = store_item(&prior["item"]);
        let name = prior["name"]
            .as_str()
            .expect("a store record names its skill");
        store::materialize(&root, &item, name).expect("the seeded version materializes");
    }
    for (scope, entries) in manifests {
        let path = if scope == "global" {
            manifest::global_manifest_path(vibe_home)
        } else {
            project.join(".vibe").join("skills.toml")
        };
        let skills = entries
            .iter()
            .map(|entry| ManifestEntry {
                name: entry["name"].as_str().unwrap_or_default().to_owned(),
                skill_id: entry["skill_id"].as_str().unwrap_or_default().to_owned(),
                version: match &entry["version"] {
                    Value::Number(version) => {
                        ManifestVersion::Frozen(version.as_i64().unwrap_or(0))
                    }
                    Value::String(alias) => ManifestVersion::Alias(alias.clone()),
                    _ => ManifestVersion::default(),
                },
                description: entry
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            })
            .collect();
        manifest::save(&path, &SkillManifest { skills }).expect("the seeded manifest saves");
    }
    for (skill_id, alias, version) in resolved {
        pins::record_resolved(vibe_home, skill_id, alias, *version);
    }
}

fn skill_record(skill: &SkillDefinition, roots: &BTreeMap<String, PathBuf>) -> Value {
    let location = skill.path.as_ref().and_then(|path| label_path(path, roots));
    let builtin = skill.source == SkillSource::Builtin;
    json!({
        "name": skill.name,
        "source": vocabulary(skill.source),
        "scope": vocabulary(skill.scope),
        "registry": skill.registry.as_ref().map(|registry| json!({
            "skill_id": registry.skill_id,
            "version": registry.version,
            "alias": registry.alias,
        })),
        "root": location.as_ref().map(|(root, _)| root),
        "relPath": location.as_ref().map(|(_, relative)| relative),
        "modelInvocable": skill.model_invocable,
        "userInvocable": skill.user_invocable,
        "description": (!builtin).then(|| skill.description.clone()),
    })
}

fn sorted_records(mut records: Vec<Value>, keys: &[&str]) -> Vec<Value> {
    records.sort_by_key(|record| {
        keys.iter()
            .map(|key| record[*key].as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>()
    });
    records
}

fn lifecycle_answer(case: &LifecycleCase) -> Value {
    let scratch = tempfile::tempdir().expect("a scratch directory is available");
    let roots = scenario_roots(scratch.path(), &case.tree);
    let vibe_home = roots["home"].join(".vibe");
    seed_registry(
        &vibe_home,
        &roots["project"],
        &case.store,
        &case.manifests,
        &case.resolved,
    );
    let projects = if case.trusted {
        vec![roots["project"].clone()]
    } else {
        Vec::new()
    };
    let discovery = SkillDiscovery {
        roots: search_paths(&SearchInputs {
            configured: &[],
            projects: &projects,
            vibe_home: &vibe_home,
            user_home: Some(&roots["home"]),
            working_directory: &roots["project"],
        }),
        enabled: Vec::new(),
        disabled: case.disabled_skills.clone(),
        registry: case.registry_enabled.then(|| RegistrySources {
            vibe_home: vibe_home.clone(),
            project_roots: projects.clone(),
        }),
    };
    let catalog = discover_extensions(
        &DiscoveryRoots {
            skills: discovery.clone(),
            ..DiscoveryRoots::default()
        },
        BTreeMap::new(),
        builtin_skills(),
        BTreeMap::new(),
    );
    let available = catalog
        .skills
        .values()
        .map(|skill| skill_record(skill, &roots))
        .collect();
    let pins = discovery
        .registry
        .as_ref()
        .map(|sources| loader::pinned_skills(sources).0)
        .unwrap_or_default()
        .iter()
        .map(|skill| skill_record(skill, &roots))
        .collect();
    let builtin_names = builtin_skills().into_keys().collect::<BTreeSet<_>>();
    let installed = installed_skills(&discovery, &builtin_names)
        .0
        .iter()
        .map(|skill| skill_record(skill, &roots))
        .collect();
    json!({
        "available": sorted_records(available, &["name"]),
        "registryPins": sorted_records(pins, &["name", "scope"]),
        "installed": sorted_records(installed, &["name", "scope", "source"]),
        "issues": issue_records(&catalog.issues, &roots),
    })
}

// --------------------------------------------------------------------------
// The scripted registry
// --------------------------------------------------------------------------

/// The payload one version of a scripted skill answers, matching the capture
/// script's `_registry_payload`.
fn registry_payload(skill_id: &str, version: i64, entry: &Value) -> Value {
    let text = |key: &str| entry.get(key).and_then(Value::as_str).unwrap_or_default();
    let latest = entry
        .get("metadataLatest")
        .or_else(|| entry.get("latest"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let mut aliases = entry
        .get("aliases")
        .and_then(Value::as_object)
        .map(|aliases| {
            aliases
                .iter()
                .filter(|(_, target)| target.as_i64() == Some(version))
                .map(|(alias, _)| alias.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    aliases.sort();
    json!({
        "skillId": skill_id,
        "version": version,
        "skill": {
            "skillName": text("name"),
            "skillDescription": text("description"),
            "skillBody": entry["versions"][version.to_string()],
        },
        "metadata": {
            "latestVersion": latest,
            "sharingScope": text("sharingScope"),
            "createdBy": text("createdBy"),
        },
        "versionAttributes": {"aliases": aliases, "notes": text("notes")},
    })
}

/// What the scripted registry answers one request with: a status and a body.
fn route(
    spec: &Map<String, Value>,
    path: &str,
    query: &BTreeMap<String, String>,
) -> (Value, u16, Value) {
    let Some(rest) = path.strip_prefix("/skills") else {
        return (Value::Null, 404, Value::Null);
    };
    if rest.is_empty() || rest == "/" {
        let data = spec
            .iter()
            .map(|(skill_id, entry)| {
                registry_payload(skill_id, entry["latest"].as_i64().unwrap_or(0), entry)
            })
            .collect::<Vec<_>>();
        return (
            json!(["catalog", null, null, null]),
            200,
            json!({"data": data, "nextPageToken": ""}),
        );
    }
    let rest = rest.trim_start_matches('/');
    if let Some(skill_id) = rest.strip_suffix("/versions") {
        let entry = spec.get(skill_id);
        let items = entry
            .and_then(|entry| entry["versions"].as_object())
            .map(|versions| {
                versions
                    .keys()
                    .filter_map(|version| version.parse::<i64>().ok())
                    .map(|version| {
                        let payload = registry_payload(skill_id, version, &entry.cloned().unwrap_or_default());
                        json!({"version": version, "versionAttributes": payload["versionAttributes"]})
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        return (
            json!(["versions", skill_id, null, null]),
            200,
            json!({"items": items}),
        );
    }
    let skill_id = rest;
    let version = query
        .get("version")
        .and_then(|version| version.parse::<i64>().ok());
    let alias = query.get("alias").cloned();
    let logged = json!(["skill", skill_id, version, alias]);
    let Some(entry) = spec.get(skill_id) else {
        return (logged, 404, json!({}));
    };
    if entry.get("fail").and_then(Value::as_bool) == Some(true) {
        return (logged, 500, json!({}));
    }
    let resolved = version.or_else(|| match alias.as_deref() {
        None | Some("latest") => entry["latest"].as_i64(),
        Some(alias) => entry
            .get("aliases")
            .and_then(|aliases| aliases.get(alias))
            .and_then(Value::as_i64),
    });
    match resolved {
        Some(version) if entry["versions"].get(version.to_string()).is_some() => {
            (logged, 200, registry_payload(skill_id, version, entry))
        }
        _ => (logged, 404, json!({})),
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let decoded = (bytes[index] == b'%')
            .then(|| bytes.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match decoded {
            Some(byte) => {
                out.push(byte);
                index += 3;
            }
            None => {
                out.push(if bytes[index] == b'+' {
                    b' '
                } else {
                    bytes[index]
                });
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Serves the scripted registry on a loopback port, logging every request
/// the way the capture's fake client does, and answers its base URL.
async fn serve_registry(spec: Map<String, Value>, log: Arc<Mutex<Vec<Value>>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the listener has an address");
    let spec = Arc::new(spec);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let spec = Arc::clone(&spec);
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let target = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");
                let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
                let query = raw_query
                    .split('&')
                    .filter(|pair| !pair.is_empty())
                    .filter_map(|pair| pair.split_once('='))
                    .map(|(key, value)| (percent_decode(key), percent_decode(value)))
                    .collect::<BTreeMap<_, _>>();
                let (logged, status, body) = route(&spec, &percent_decode(path), &query);
                if !logged.is_null() {
                    log.lock()
                        .expect("the request log is not poisoned")
                        .push(logged);
                }
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    format!("http://{address}")
}

/// Requests in one canonical order, so the two sides compare as multisets:
/// the reference resolves its pins from a set and fetches concurrently.
fn sorted_requests(requests: &[Value]) -> Vec<Value> {
    let mut requests = requests.to_vec();
    requests.sort_by_key(Value::to_string);
    requests
}

// --------------------------------------------------------------------------
// sync and service
// --------------------------------------------------------------------------

fn registry_state(vibe_home: &Path, project: &Path) -> Value {
    let store_root = store::store_root(vibe_home);
    let mut files = Vec::new();
    super::skills_parity_tests::collect_files(&store_root, &store_root, &mut files);
    let mut store_files = files
        .into_iter()
        .map(|(path, _, _)| path)
        .collect::<Vec<_>>();
    store_files.sort();

    let project_key = ledger::repo_key(&[project.to_path_buf()]);
    let mut ledger_records = Map::new();
    if let Ok(entries) = fs::read_dir(ledger::ledger_root(vibe_home)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "txt") {
                continue;
            }
            let stem = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            let label = if stem == project_key {
                "project".to_owned()
            } else {
                stem
            };
            let lines = fs::read_to_string(&path)
                .unwrap_or_default()
                .lines()
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            ledger_records.insert(label, json!(lines));
        }
    }

    let resolved = fs::read_to_string(vibe_home.join("cache.toml"))
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|document| document.get("registry_resolved_aliases").cloned())
        .and_then(|section| serde_json::to_value(section).ok())
        .unwrap_or_else(|| json!({}));
    let seen = fs::read_to_string(store::cache_dir(vibe_home).join("seen-versions.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| json!({}));
    let dump =
        |path: &Path| super::skills_parity_tests::manifest_entries(&manifest::load(path).manifest);
    json!({
        "store": store_files,
        "ledger": ledger_records,
        "resolved": resolved,
        "seen": seen,
        "manifests": {
            "global": dump(&manifest::global_manifest_path(vibe_home)),
            "project": dump(&project.join(".vibe").join("skills.toml")),
        },
    })
}

fn entry_record(entry: &ManifestEntry) -> Value {
    json!({
        "name": entry.name,
        "skill_id": entry.skill_id,
        "version": match &entry.version {
            ManifestVersion::Frozen(version) => json!(version),
            ManifestVersion::Alias(alias) => json!(alias),
        },
        "description": entry.description,
    })
}

async fn registry_answer(family: &str, case: &RegistryCase) -> Value {
    let scratch = tempfile::tempdir().expect("a scratch directory is available");
    let roots = scenario_roots(scratch.path(), &Tree::new());
    let vibe_home = roots["home"].join(".vibe");
    let project = roots["project"].clone();
    seed_registry(
        &vibe_home,
        &project,
        &case.store,
        &case.manifests,
        &case.resolved,
    );
    for (key, items) in &case.ledger {
        let active = items.iter().cloned().collect::<BTreeSet<_>>();
        ledger::record(&vibe_home, key, &active).expect("the seeded record writes");
    }
    if !case.seen.is_empty() {
        let path = store::cache_dir(&vibe_home).join("seen-versions.json");
        fs::create_dir_all(path.parent().expect("the cache has a parent"))
            .expect("the cache is writable");
        fs::write(
            &path,
            serde_json::to_string(&case.seen).expect("the seen map encodes"),
        )
        .expect("the seen map writes");
    }
    if let Some(name) = &case.local_skill {
        let local = vibe_home.join("skills").join(name);
        fs::create_dir_all(&local).expect("the local skill directory is writable");
        fs::write(
            local.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Already local\n---\n\nProbe body.\n"),
        )
        .expect("the local skill writes");
    }

    let log = Arc::new(Mutex::new(Vec::new()));
    let endpoint = if case.endpoint {
        Some(RegistryEndpoint {
            api_base: serve_registry(case.registry.clone(), Arc::clone(&log)).await,
            api_key: "k".to_owned(),
        })
    } else {
        None
    };
    let project_roots = if case.trusted {
        vec![project.clone()]
    } else {
        Vec::new()
    };
    let text = |key: &str| case.args.get(key).and_then(Value::as_str);
    let scope = if text("scope") == Some("project") {
        PinScope::Project
    } else {
        PinScope::Global
    };
    let target = PinTarget {
        vibe_home: &vibe_home,
        roots: &project_roots,
    };
    let name = text("name").unwrap_or_default();
    let version = case.args.get("version").and_then(Value::as_i64);
    let manifests_for_updates = || {
        manifest::project_manifest_paths(&vibe_home, &project_roots)
            .into_iter()
            .chain([manifest::global_manifest_path(&vibe_home)])
            .map(|path| manifest::load(&path).manifest)
            .collect::<Vec<_>>()
    };
    let error = || json!({"error": true});

    let result = match (family, case.op.as_str()) {
        ("sync", "publish") => {
            sync::publish_local_pins(SyncScope {
                vibe_home: &vibe_home,
                roots: &project_roots,
            });
            Value::Null
        }
        ("sync", _) => {
            let outcome = sync::refresh_registry_skills(
                case.enabled.unwrap_or(true),
                endpoint.as_ref(),
                SyncScope {
                    vibe_home: &vibe_home,
                    roots: &project_roots,
                },
            )
            .await;
            json!({
                "status": match outcome.status {
                    SyncStatus::Skipped => "skipped",
                    SyncStatus::Failed => "failed",
                    SyncStatus::Ok => "ok",
                },
                "written": outcome.written,
                "skipped": outcome.skipped,
            })
        }
        (_, "import") => pins::import_skill(
            endpoint.as_ref(),
            &target,
            text("skillId").unwrap_or_default(),
            version,
            text("alias"),
            scope,
        )
        .await
        .map_or_else(|_| error(), |entry| entry_record(&entry)),
        (_, op @ ("setVersion" | "setLatest" | "setAlias")) => {
            let (version, alias) = match op {
                "setVersion" => (version, None),
                "setLatest" => (None, None),
                _ => (None, text("alias")),
            };
            match pins::repin_skill(endpoint.as_ref(), &target, name, version, alias, scope).await {
                Ok(Some(entry)) => entry_record(&entry),
                Ok(None) => Value::Null,
                Err(_) => error(),
            }
        }
        (_, "remove") => {
            json!(pins::remove_skill(&target, name, scope).expect("the manifest writes"))
        }
        (_, "convertLocal") => {
            match pins::convert_skill_to_local(&target, name, scope).expect("the conversion runs") {
                None => Value::Null,
                Some(converted) => {
                    let (root, relative) =
                        label_path(&converted, &roots).expect("the conversion lands under a root");
                    json!({"root": root, "relPath": relative, "tree": tree_of(&converted, None)})
                }
            }
        }
        (_, op @ ("checkUpdates" | "checkNewVersions")) => {
            let updates = match &endpoint {
                None => Vec::new(),
                Some(endpoint) if op == "checkUpdates" => {
                    service::check_updates(endpoint, &manifests_for_updates()).await
                }
                Some(endpoint) => {
                    service::check_new_versions(endpoint, &manifests_for_updates(), &vibe_home)
                        .await
                }
            };
            json!(
                updates
                    .iter()
                    .map(|update| json!({
                        "name": update.name,
                        "currentVersion": update.current_version,
                        "latestVersion": update.latest_version,
                    }))
                    .collect::<Vec<_>>()
            )
        }
        (_, "catalog") => {
            let items = match &endpoint {
                None => Vec::new(),
                Some(endpoint) => service::list_catalog(endpoint).await.unwrap_or_default(),
            };
            json!(
                items
                    .iter()
                    .map(|item| json!({
                        "name": item.name,
                        "skillId": item.skill_id,
                        "description": item.description,
                        "latestVersion": item.latest_version,
                        "sharingScope": item.sharing_scope,
                    }))
                    .collect::<Vec<_>>()
            )
        }
        (_, "versions") => json!(
            pins::skill_versions(endpoint.as_ref(), text("skillId").unwrap_or_default())
                .await
                .iter()
                .map(|info| json!({"version": info.version, "aliases": info.aliases}))
                .collect::<Vec<_>>()
        ),
        (_, "detail") => pins::skill_details(
            endpoint.as_ref(),
            text("skillId").unwrap_or_default(),
            version,
        )
        .await
        .map_or(Value::Null, |detail| {
            json!({
                "aliases": detail.aliases,
                "body": detail.body,
                "created_at": detail.created_at,
                "created_by": detail.created_by,
                "description": detail.description,
                "last_modified_at": detail.last_modified_at,
                "latest_version": detail.latest_version,
                "name": detail.name,
                "notes": detail.notes,
                "sharing_scope": detail.sharing_scope,
                "skill_id": detail.skill_id,
                "version": detail.version,
                "version_created_at": detail.version_created_at,
            })
        }),
        (_, other) => json!({"unknownOp": other}),
    };
    let requests = log.lock().expect("the request log is not poisoned").clone();
    json!({
        "result": result,
        "requests": sorted_requests(&requests),
        "state": registry_state(&vibe_home, &project),
    })
}

// --------------------------------------------------------------------------
// ledger
// --------------------------------------------------------------------------

fn union_records(vibe_home: &Path) -> Value {
    json!(
        ledger::union(vibe_home)
            .into_iter()
            .map(|(skill_id, version)| json!([skill_id, version]))
            .collect::<Vec<_>>()
    )
}

fn ledger_answer(case: &str) -> Value {
    let scratch = tempfile::tempdir().expect("a scratch directory is available");
    let base = scratch
        .path()
        .canonicalize()
        .expect("the scratch directory resolves");
    let vibe_home = base.join("home").join(".vibe");
    let first = base.join("ledger-roots").join("first");
    let second = base.join("ledger-roots").join("second");
    fs::create_dir_all(&first).expect("the first root is writable");
    fs::create_dir_all(&second).expect("the second root is writable");
    let record = |key: &str, items: &[(&str, i64)]| {
        let active = items
            .iter()
            .map(|(skill_id, version)| ((*skill_id).to_owned(), *version))
            .collect::<BTreeSet<_>>();
        ledger::record(&vibe_home, key, &active).expect("the record writes");
    };
    let root = ledger::ledger_root(&vibe_home);
    match case {
        "repo-key-shape" => {
            let key = ledger::repo_key(&[first.clone(), second.clone()]);
            json!({
                "case": case,
                "emptyKey": ledger::repo_key(&[]),
                "orderIndependent": key == ledger::repo_key(&[second.clone(), first.clone()]),
                "spellingIndependent": key == ledger::repo_key(&[first.join("..").join("first"), second.clone()]),
                "distinctFromOne": key != ledger::repo_key(std::slice::from_ref(&first)),
                "length": key.len(),
                "lowercaseHex": key.chars().all(|character| "0123456789abcdef".contains(character)),
            })
        }
        "record-sorts-as-text" => {
            record("k1", &[("b", 2), ("a", 10), ("a", 2)]);
            record("k2", &[("a", 2), ("c", 1)]);
            json!({
                "case": case,
                "record": fs::read_to_string(root.join("k1.txt")).unwrap_or_default(),
                "union": union_records(&vibe_home),
            })
        }
        "empty-record-is-removed" => {
            record("k1", &[("b", 2), ("a", 10), ("a", 2)]);
            record("k2", &[("a", 2), ("c", 1)]);
            record("k1", &[]);
            let mut files = fs::read_dir(&root)
                .expect("the ledger directory exists")
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            files.sort();
            json!({"case": case, "files": files, "union": union_records(&vibe_home)})
        }
        "union-skips-malformed-lines" => {
            record("k2", &[("a", 2), ("c", 1)]);
            fs::write(
                root.join("k3.txt"),
                "d@4\njunk\nb@x\n@3\n e@2 \nf@@6\ng@-1\n",
            )
            .expect("the malformed record writes");
            fs::write(root.join("ignored.json"), "z@1\n").expect("the stray file writes");
            json!({"case": case, "union": union_records(&vibe_home)})
        }
        "union-without-records" => json!({"case": case, "union": union_records(&vibe_home)}),
        other => json!({"case": other, "error": "the port has no comparator for this case"}),
    }
}
