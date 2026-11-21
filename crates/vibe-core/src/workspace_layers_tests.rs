//! The crate layering `[workspace.metadata.vibe] dependency-layers` declares
//! in the workspace `Cargo.toml` (AGENTS.md, "Architecture"), read from
//! `cargo metadata`.
//!
//! The layers are numbered from 1 in the order the manifest lists them. The
//! rules:
//!
//! - every workspace member sits in exactly one layer, and every name a layer
//!   lists is a workspace member;
//! - a crate depends only on crates of its own layer or of an earlier one.
//!   Normal, build and dev-dependencies all count: a test that reached up to
//!   an adapter would couple the lower crate to it as surely as its library
//!   code would, and Cargo would build the adapter to test the core. Every
//!   dev-dependency edge of the workspace points down or within its layer (the
//!   adapters' dev-dependency on `vibe-e2e`, in layer 1, among them).

use std::collections::BTreeMap;
use std::process::Command;

use serde_json::{Value, json};

use crate::parity::replay::repository;

/// What breaks the layering in a `cargo metadata --format-version 1` document,
/// one line each.
fn layer_problems(metadata: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    let members: BTreeMap<&str, &Value> = metadata["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|package| {
            metadata["workspace_members"]
                .as_array()
                .is_some_and(|ids| ids.contains(&package["id"]))
        })
        .filter_map(|package| Some((package["name"].as_str()?, package)))
        .collect();
    let Some(layers) = metadata["metadata"]["vibe"]["dependency-layers"].as_array() else {
        return vec!["`[workspace.metadata.vibe] dependency-layers` is missing".to_owned()];
    };
    let mut layer_of: BTreeMap<&str, usize> = BTreeMap::new();
    for (index, layer) in layers.iter().enumerate() {
        let number = index + 1;
        for name in layer.as_array().into_iter().flatten() {
            let Some(name) = name.as_str() else {
                problems.push(format!("layer {number} lists {name}, not a crate name"));
                continue;
            };
            if !members.contains_key(name) {
                problems.push(format!(
                    "layer {number} lists `{name}`, which is not a workspace member"
                ));
            }
            if let Some(earlier) = layer_of.get(name) {
                problems.push(format!("`{name}` sits in layers {earlier} and {number}"));
            } else {
                layer_of.insert(name, number);
            }
        }
    }
    for (name, package) in &members {
        let Some(&layer) = layer_of.get(name) else {
            problems.push(format!("`{name}` sits in no layer"));
            continue;
        };
        for dependency in package["dependencies"].as_array().into_iter().flatten() {
            let Some(target) = dependency["name"].as_str() else {
                continue;
            };
            let Some(&target_layer) = layer_of.get(target) else {
                continue;
            };
            if target_layer > layer {
                let kind = match dependency["kind"].as_str() {
                    Some("dev") => "a dev-dependency",
                    Some("build") => "a build-dependency",
                    _ => "a dependency",
                };
                problems.push(format!(
                    "`{name}` (layer {layer}) has {kind} on `{target}` (layer {target_layer}), a later layer"
                ));
            }
        }
    }
    problems
}

#[test]
fn every_member_sits_in_one_layer_and_depends_only_on_its_own_or_earlier_layers() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
        ])
        .arg("--manifest-path")
        .arg(repository().join("Cargo.toml"))
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata prints JSON");
    let members = metadata["workspace_members"].as_array().map_or(0, Vec::len);
    assert!(members >= 8, "only {members} workspace members read");
    let problems = layer_problems(&metadata);
    assert!(
        problems.is_empty(),
        "the workspace breaks its dependency layers (Cargo.toml, AGENTS.md \"Architecture\"):\n{}",
        problems.join("\n")
    );
}

fn package(name: &str, dependencies: &[(&str, Option<&str>)]) -> Value {
    json!({
        "name": name,
        "id": format!("path+file:///w/{name}#0.1.0"),
        "dependencies": dependencies
            .iter()
            .map(|(target, kind)| json!({"name": target, "kind": kind}))
            .collect::<Vec<_>>(),
    })
}

#[test]
fn a_synthetic_workspace_names_each_edge_into_a_later_layer() {
    let packages = vec![
        package("core", &[("serde", None), ("protocol", None)]),
        package("protocol", &[]),
        package("server", &[("core", None), ("core", Some("dev"))]),
        package("adapter", &[("server", None)]),
        // Reaches up from layer 1 three ways, each named.
        package(
            "leaf",
            &[
                ("server", None),
                ("adapter", Some("dev")),
                ("adapter", Some("build")),
            ],
        ),
        package("stray", &[]),
    ];
    let ids: Vec<Value> = packages
        .iter()
        .map(|package| package["id"].clone())
        .collect();
    let metadata = json!({
        "packages": packages,
        "workspace_members": ids,
        "metadata": {"vibe": {"dependency-layers": [
            ["core", "protocol", "leaf"],
            ["server"],
            ["adapter", "server", "gone"],
        ]}},
    });
    assert_eq!(
        layer_problems(&metadata),
        [
            "`server` sits in layers 2 and 3",
            "layer 3 lists `gone`, which is not a workspace member",
            "`leaf` (layer 1) has a dependency on `server` (layer 2), a later layer",
            "`leaf` (layer 1) has a dev-dependency on `adapter` (layer 3), a later layer",
            "`leaf` (layer 1) has a build-dependency on `adapter` (layer 3), a later layer",
            "`stray` sits in no layer",
        ]
    );
    let missing = json!({"packages": [], "workspace_members": [], "metadata": null});
    assert_eq!(
        layer_problems(&missing),
        ["`[workspace.metadata.vibe] dependency-layers` is missing"]
    );
}
