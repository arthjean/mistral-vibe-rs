//! Replays the committed plugins resolve corpus against this port's resolver.
//!
//! `scripts/parity/plugins.py --family resolve` wrote the trees of
//! `tests/plugins-parity/fixtures.json` into a fresh home per case, resolved
//! them with the pinned reference's `PluginResolver` over a project, a user
//! and a built-in root, materialized the result without remote ports, and kept
//! a summary per case in `tests/plugins-parity/corpus.json`. This test writes
//! the same trees, resolves and materializes them here, builds the same
//! summary, and compares the two key by key.
//!
//! Diagnostic messages are left out on both sides, being this port's own
//! prose, and so is the prompt of a skill that declares tools, whose guidance
//! preamble is reworded. Every other difference has to fall under a `LEDGER`
//! entry, and every entry has to still reproduce.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde_json::{Map, Value, json};

use crate::plugins::compatibility::PluginMcpServer;
use crate::plugins::diagnostics::PluginConfigIssue;
use crate::plugins::materialize::{MaterializedPluginSet, PluginMaterializer};
use crate::plugins::native::{PluginResolver, ResolvedPluginSet};

const FIXTURES: &str = include_str!("../../tests/plugins-parity/fixtures.json");
const CORPUS: &str = include_str!("../../tests/plugins-parity/corpus.json");

/// The cases the corpus may not fall below.
const CASE_FLOOR: usize = 11;

/// A difference this port keeps in one case: every difference whose JSON
/// pointer starts with `pointer` is covered.
struct Divergence {
    case: &'static str,
    pointer: &'static str,
    reason: &'static str,
}

const LEDGER: &[Divergence] = &[];

const SKILL_PATH_METADATA: &str = "unified-harness.runtime-skill-path";

fn relative(home: &Path, path: &Path) -> Value {
    json!(path.strip_prefix(home).unwrap_or(path).to_string_lossy())
}

fn masked(home: &Path, text: &str) -> String {
    text.replace(&home.to_string_lossy().into_owned(), "<home>")
}

fn issue_rows(home: &Path, issues: &[PluginConfigIssue]) -> Value {
    let mut rows: Vec<Value> = issues
        .iter()
        .map(|issue| {
            json!([
                issue.code,
                issue.severity.as_str(),
                issue.fatal,
                issue.component,
                relative(home, &issue.file)
            ])
        })
        .collect();
    rows.sort_by_key(Value::to_string);
    Value::Array(rows)
}

#[allow(clippy::too_many_lines)]
fn summarize(
    home: &Path,
    resolved: &ResolvedPluginSet,
    materialized: &MaterializedPluginSet,
) -> Value {
    let plugins: Vec<Value> = resolved
        .plugins
        .iter()
        .map(|plugin| {
            json!({
                "name": plugin.name,
                "version": plugin.version,
                "namespace": plugin.namespace,
                "description": plugin.description,
                "author": plugin.author,
                "scope": plugin.scope,
                "source_format": plugin.source_format.as_str(),
                "manifest_digest": plugin.manifest_digest,
                "content_digest": plugin.content_digest,
                "manifest_path": relative(home, &plugin.manifest_path),
                "root": relative(home, &plugin.root),
                "data_root": relative(home, &plugin.data_root),
            })
        })
        .collect();
    let skills: Vec<Value> = resolved
        .skills
        .iter()
        .map(|(alias, skill)| {
            let metadata: Map<String, Value> = skill
                .metadata
                .iter()
                .filter(|(key, _)| key.as_str() != SKILL_PATH_METADATA)
                .map(|(key, value)| (key.clone(), json!(value)))
                .collect();
            json!({
                "alias": alias,
                "description": skill.description,
                "model_invocable": skill.model_invocable,
                "user_invocable": skill.user_invocable,
                "allowed_tools": skill.allowed_tools,
                "metadata": metadata,
                "path": skill.path.as_deref().map(|path| relative(home, path)),
                "prompt": skill.allowed_tools.is_empty().then(|| skill.body.clone()),
            })
        })
        .collect();
    let hooks: Vec<Value> = resolved
        .runtime_hooks
        .iter()
        .map(|hook| {
            json!({
                "name": hook.config.name,
                "type": hook.config.hook_type.label(),
                "match": hook.config.matcher,
                "timeout": hook.config.timeout,
                "protocol": hook.protocol,
                "env": hook.environment.keys().collect::<Vec<_>>(),
                "cwd": hook.cwd.as_deref().map(|cwd| relative(home, cwd)),
                "config_file": relative(home, &hook.config_file),
                "order": hook.order,
            })
        })
        .collect();
    let mcp: Vec<Value> = resolved
        .mcp_servers
        .iter()
        .map(|definition| {
            let mut row = json!({
                "plugin": definition.plugin_name,
                "source_id": definition.source_id,
                "alias": definition.private_alias,
                "transport": definition.server.transport(),
                "config_file": relative(home, &definition.config_file),
            });
            match &definition.server {
                PluginMcpServer::Stdio {
                    command,
                    args,
                    env,
                    cwd,
                    ..
                } => {
                    row["command"] = json!(
                        command
                            .iter()
                            .map(|part| masked(home, part))
                            .collect::<Vec<_>>()
                    );
                    row["args"] = json!(
                        args.iter()
                            .map(|part| masked(home, part))
                            .collect::<Vec<_>>()
                    );
                    row["env"] = env
                        .iter()
                        .map(|(key, value)| (key.clone(), json!(masked(home, value))))
                        .collect::<Map<_, _>>()
                        .into();
                    row["cwd"] = json!(cwd.as_deref().map(|cwd| masked(home, cwd)));
                }
                PluginMcpServer::Http { url, .. }
                | PluginMcpServer::AuthenticatedHttp { url, .. } => {
                    row["url"] = json!(url);
                    row["headers"] = json!(definition.server.http_headers());
                }
            }
            row
        })
        .collect();
    let knowledge: Vec<Value> = resolved
        .knowledge
        .iter()
        .map(|item| {
            json!({
                "plugin": item.plugin_name,
                "name": item.name,
                "source_name": item.source_name,
                "description": item.description,
                "display_name": item.display_name,
                "icon": item.icon,
                "source_root": relative(home, &item.source_root),
                "source_entrypoint": relative(home, &item.source_entrypoint),
                "runtime_root": relative(home, &item.runtime_root),
                "runtime_entrypoint": relative(home, &item.runtime_entrypoint),
            })
        })
        .collect();
    let agents: Vec<Value> = resolved
        .agents
        .iter()
        .map(|item| {
            json!({
                "plugin": item.plugin_name,
                "name": item.name,
                "source_name": item.source_name,
                "source_file": relative(home, &item.source_file),
                "display_name": item.display_name,
                "description": item.description,
                "safety": item.safety,
                "agent_type": item.agent_type,
                "instructions": item.instructions,
                "overrides": item.overrides,
            })
        })
        .collect();
    let libraries: Vec<Value> = resolved
        .libraries
        .iter()
        .map(|item| {
            json!({
                "plugin": item.plugin_name,
                "language": item.language,
                "alias": item.alias,
                "source_path": relative(home, &item.source_path),
                "runtime_path": relative(home, &item.runtime_path),
                "config_file": relative(home, &item.config_file),
            })
        })
        .collect();
    let connectors: Vec<Value> = resolved
        .connectors
        .iter()
        .map(|item| {
            json!({
                "plugin": item.plugin_name,
                "source_id": item.source_id,
                "tools": item.tools,
                "config_file": relative(home, &item.config_file),
            })
        })
        .collect();
    let unsupported: Vec<Value> = resolved
        .unsupported_components
        .iter()
        .map(|item| {
            json!([
                item.plugin_name,
                item.kind,
                item.reason,
                relative(home, &item.path)
            ])
        })
        .collect();
    let data = home.join("plugin-data");
    let mut staged = Vec::new();
    collect_files(&data, home, &mut staged);
    staged.sort();
    json!({
        "plugins": plugins,
        "skills": skills,
        "hooks": hooks,
        "mcp": mcp,
        "knowledge": knowledge,
        "agents": agents,
        "libraries": libraries,
        "connectors": connectors,
        "issues": issue_rows(home, &resolved.issues),
        "unsupported": unsupported,
        "materialized": {
            "knowledge": materialized
                .knowledge
                .iter()
                .map(|item| json!([item.name, relative(home, &item.runtime_root), relative(home, &item.runtime_entrypoint)]))
                .collect::<Vec<_>>(),
            "libraries": materialized
                .libraries
                .iter()
                .map(|item| json!([item.language, item.alias, relative(home, &item.runtime_path)]))
                .collect::<Vec<_>>(),
            "environment": materialized
                .process_environment
                .iter()
                .map(|(key, value)| (key.clone(), json!(masked(home, value))))
                .collect::<BTreeMap<_, _>>(),
            "staged": staged,
            "issues": issue_rows(home, &materialized.issues),
        },
    })
}

fn collect_files(directory: &Path, home: &Path, found: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, home, found);
        } else if path.is_file() {
            found.push(
                path.strip_prefix(home)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
}

async fn resolve_case(case: &Value) -> Value {
    let directory = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(directory.path()).unwrap();
    for (path, content) in case["files"].as_object().unwrap() {
        let target = home.join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, content.as_str().unwrap()).unwrap();
    }
    let root = |name: &str| {
        let path = home.join(name);
        if path.is_dir() {
            vec![path]
        } else {
            Vec::new()
        }
    };
    let configured: Option<BTreeSet<String>> = case.get("configuredMcp").map(|names| {
        names
            .as_array()
            .unwrap()
            .iter()
            .map(|name| name.as_str().unwrap().to_owned())
            .collect()
    });
    let resolved = PluginResolver {
        project_roots: root("project"),
        user_roots: root("user"),
        builtin_roots: root("builtin"),
        data_root_base: Some(home.join("plugin-data")),
        configured_mcp_names: configured,
        ..PluginResolver::default()
    }
    .resolve();
    let materialized = PluginMaterializer {
        mcp_discovery: None,
        connector_catalog: None,
    }
    .materialize(resolved.clone())
    .await;
    summarize(&home, &resolved, &materialized)
}

fn differences(reference: &Value, port: &Value, pointer: &str, found: &mut Vec<String>) {
    match (reference, port) {
        (Value::Object(left), Value::Object(right)) => {
            let keys: BTreeSet<&String> = left.keys().chain(right.keys()).collect();
            for key in keys {
                let child = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
                match (left.get(key), right.get(key)) {
                    (Some(left), Some(right)) => differences(left, right, &child, found),
                    _ => found.push(child),
                }
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            for index in 0..left.len().max(right.len()) {
                let child = format!("{pointer}/{index}");
                match (left.get(index), right.get(index)) {
                    (Some(left), Some(right)) => differences(left, right, &child, found),
                    _ => found.push(child),
                }
            }
        }
        _ if reference != port => found.push(pointer.to_owned()),
        _ => {}
    }
}

#[tokio::test]
async fn every_plugin_tree_resolves_as_the_reference_resolves_it_or_as_the_ledger_names() {
    let fixtures: Value = serde_json::from_str(FIXTURES).unwrap();
    let corpus: Value = serde_json::from_str(CORPUS).unwrap();
    let recorded = corpus["cases"].as_object().unwrap();
    let declared = fixtures["cases"].as_object().unwrap();
    assert!(
        recorded.len() >= CASE_FLOOR,
        "the corpus holds {} cases",
        recorded.len()
    );
    assert_eq!(
        recorded.keys().collect::<Vec<_>>(),
        declared.keys().collect::<Vec<_>>(),
        "fixtures.json declares other cases than the corpus records; recapture with \
         `scripts/parity/plugins.py --family resolve`"
    );
    let mut unexplained = Vec::new();
    let mut reproduced = vec![0_usize; LEDGER.len()];
    let mut conformant = 0;
    for (name, reference) in recorded {
        let port = resolve_case(&declared[name]).await;
        let mut found = Vec::new();
        differences(reference, &port, "", &mut found);
        if found.is_empty() {
            conformant += 1;
        }
        for pointer in found {
            match LEDGER
                .iter()
                .position(|entry| entry.case == name && pointer.starts_with(entry.pointer))
            {
                Some(index) => reproduced[index] += 1,
                None => unexplained.push(format!(
                    "{name} {pointer}\n  reference: {}\n  port:      {}",
                    reference
                        .pointer(&pointer)
                        .map_or_else(|| "<absent>".to_owned(), Value::to_string),
                    port.pointer(&pointer)
                        .map_or_else(|| "<absent>".to_owned(), Value::to_string),
                )),
            }
        }
    }
    let stale: Vec<String> = LEDGER
        .iter()
        .zip(&reproduced)
        .filter(|(_, count)| **count == 0)
        .map(|(entry, _)| format!("{} {} ({})", entry.case, entry.pointer, entry.reason))
        .collect();
    println!(
        "plugins resolve parity: {conformant}/{} cases conformant, {} ledgered differences",
        recorded.len(),
        reproduced.iter().sum::<usize>()
    );
    assert!(
        unexplained.is_empty(),
        "the port departs from the corpus where no ledger entry says it may:\n{}",
        unexplained.join("\n")
    );
    assert!(
        stale.is_empty(),
        "these ledger entries no longer reproduce: {stale:?}"
    );
}

/// Recaptures the pinned reference and asserts the committed corpus is still
/// what it answers. The replay above runs regardless, which is what keeps a
/// missing checkout from failing `cargo test`.
#[test]
fn the_committed_corpus_still_matches_the_pinned_reference() {
    let root = crate::parity::reference_root();
    if let Some(reason) = crate::parity::off_pin_reason(&root, "plugins resolve") {
        eprintln!("{reason}");
        eprintln!(
            "the committed corpus replayed regardless; restore with `{}`",
            crate::parity::RESTORE_COMMAND
        );
        return;
    }
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = std::process::Command::new("python3")
        .arg(repository.join("scripts/parity/plugins.py"))
        .args(["--family", "resolve", "--check", "--reference"])
        .arg(&root)
        .current_dir(&repository)
        .env_remove("FORCE_COLOR")
        .output()
        .expect("python3 runs the plugins capture script");
    assert!(
        output.status.success(),
        "the pinned reference no longer answers what the corpus records; regenerate it with \
         `scripts/parity/plugins.py --family resolve`: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
