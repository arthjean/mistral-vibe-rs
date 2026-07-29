use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
use thiserror::Error;

const REQUIRED_CRATES: [&str; 6] = [
    "vibe-acp",
    "vibe-app-server",
    "vibe-cli",
    "vibe-compat",
    "vibe-core",
    "vibe-protocol",
];

#[derive(Debug, Error)]
pub enum WorkspacePolicyError {
    #[error("workspace policy I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("cargo metadata failed: {0}")]
    Metadata(String),
    #[error("workspace policy metadata is malformed: {0}")]
    Malformed(&'static str),
    #[error("workspace is missing owned crate `{0}`")]
    MissingCrate(String),
    #[error(
        "crate `{crate_name}` violates rule `workspace-lints`: [lints] workspace = true is required"
    )]
    MissingLints { crate_name: String },
    #[error(
        "crate `{crate_name}` violates rule `dependency-direction`: dependency `{dependency}` points from layer {owner_layer} to layer {dependency_layer}"
    )]
    DependencyDirection {
        crate_name: String,
        dependency: String,
        owner_layer: usize,
        dependency_layer: usize,
    },
    #[error("workspace dependency graph contains a cycle involving `{0}`")]
    Cycle(String),
}

pub fn validate(root: &Path) -> Result<(), WorkspacePolicyError> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(root)
        .output()?;
    if !output.status.success() {
        return Err(WorkspacePolicyError::Metadata(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let metadata: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| WorkspacePolicyError::Malformed("cargo metadata JSON"))?;
    let layers = parse_layers(&metadata)?;
    let packages = metadata["packages"]
        .as_array()
        .ok_or(WorkspacePolicyError::Malformed("packages"))?;
    let package_names = packages
        .iter()
        .filter_map(|package| package["name"].as_str())
        .collect::<BTreeSet<_>>();
    for required in REQUIRED_CRATES {
        if !package_names.contains(required) {
            return Err(WorkspacePolicyError::MissingCrate(required.to_owned()));
        }
    }
    let mut graph = BTreeMap::<String, Vec<String>>::new();
    for package in packages {
        let name = package["name"]
            .as_str()
            .ok_or(WorkspacePolicyError::Malformed("package.name"))?;
        let Some(&owner_layer) = layers.get(name) else {
            return Err(WorkspacePolicyError::Malformed("unlayered package"));
        };
        let manifest = package["manifest_path"]
            .as_str()
            .ok_or(WorkspacePolicyError::Malformed("package.manifest_path"))?;
        let manifest_text = fs::read_to_string(manifest)?;
        if !manifest_text.contains("[lints]\nworkspace = true") {
            return Err(WorkspacePolicyError::MissingLints {
                crate_name: name.to_owned(),
            });
        }
        let local_dependencies = package["dependencies"]
            .as_array()
            .ok_or(WorkspacePolicyError::Malformed("package.dependencies"))?
            .iter()
            .filter(|dependency| dependency["source"].is_null())
            .filter_map(|dependency| dependency["name"].as_str())
            .filter(|dependency| layers.contains_key(*dependency))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        for dependency in &local_dependencies {
            let dependency_layer = layers[dependency];
            if dependency_layer >= owner_layer {
                return Err(WorkspacePolicyError::DependencyDirection {
                    crate_name: name.to_owned(),
                    dependency: dependency.clone(),
                    owner_layer,
                    dependency_layer,
                });
            }
        }
        graph.insert(name.to_owned(), local_dependencies);
    }
    reject_cycles(&graph)?;
    Ok(())
}

fn parse_layers(metadata: &Value) -> Result<BTreeMap<String, usize>, WorkspacePolicyError> {
    let raw_layers = metadata["metadata"]["vibe"]["dependency-layers"]
        .as_array()
        .ok_or(WorkspacePolicyError::Malformed(
            "workspace.metadata.vibe.dependency-layers",
        ))?;
    let mut layers = BTreeMap::new();
    for (index, layer) in raw_layers.iter().enumerate() {
        for name in layer
            .as_array()
            .ok_or(WorkspacePolicyError::Malformed("dependency layer"))?
        {
            let name = name
                .as_str()
                .ok_or(WorkspacePolicyError::Malformed("dependency layer crate"))?;
            layers.insert(name.to_owned(), index);
        }
    }
    Ok(layers)
}

fn reject_cycles(graph: &BTreeMap<String, Vec<String>>) -> Result<(), WorkspacePolicyError> {
    fn visit(
        node: &str,
        graph: &BTreeMap<String, Vec<String>>,
        active: &mut BTreeSet<String>,
        complete: &mut BTreeSet<String>,
    ) -> Result<(), WorkspacePolicyError> {
        if complete.contains(node) {
            return Ok(());
        }
        if !active.insert(node.to_owned()) {
            return Err(WorkspacePolicyError::Cycle(node.to_owned()));
        }
        for dependency in graph.get(node).into_iter().flatten() {
            visit(dependency, graph, active, complete)?;
        }
        active.remove(node);
        complete.insert(node.to_owned());
        Ok(())
    }
    let mut active = BTreeSet::new();
    let mut complete = BTreeSet::new();
    for node in graph.keys() {
        visit(node, graph, &mut active, &mut complete)?;
    }
    Ok(())
}
