use std::collections::BTreeSet;
use std::path::{Component, Path};

use thiserror::Error;
use vibe_protocol::SERVER_METHODS;

use crate::model::{CapabilityMatrix, DiscoveredSurfaces, SupportClass};
use crate::{DISCOVERED_TOML, MATRIX_TOML};

#[derive(Debug, Error)]
pub enum MatrixError {
    #[error("capability matrix is invalid TOML: {0}")]
    MatrixToml(#[source] toml::de::Error),
    #[error("discovered-surface inventory is invalid TOML: {0}")]
    DiscoveryToml(#[source] toml::de::Error),
    #[error("capability matrix rule failed for `{row}`: {rule}")]
    Rule { row: String, rule: String },
    #[error("discovered public behavior has no matrix row: {0}")]
    MissingDiscovery(String),
    #[error("matrix references missing dependency `{dependency}` from `{row}`")]
    MissingDependency { row: String, dependency: String },
}

pub fn load() -> Result<CapabilityMatrix, MatrixError> {
    toml::from_str(MATRIX_TOML).map_err(MatrixError::MatrixToml)
}

pub fn validate(checkout: &Path) -> Result<CapabilityMatrix, MatrixError> {
    let matrix = load()?;
    let discovered: DiscoveredSurfaces =
        toml::from_str(DISCOVERED_TOML).map_err(MatrixError::DiscoveryToml)?;
    if matrix.schema_version != 2 || discovered.schema_version != 2 {
        return Err(rule("matrix", "unsupported schema version"));
    }
    let ids = matrix
        .rows
        .iter()
        .map(|row| row.id.as_str())
        .collect::<BTreeSet<_>>();
    if ids.len() != matrix.rows.len() {
        return Err(rule("matrix", "row IDs must be unique"));
    }
    let discovered_ids = discovered
        .known_rows
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if discovered_ids.len() != discovered.known_rows.len() {
        return Err(rule("discovery", "known row IDs must be unique"));
    }
    if ids != discovered_ids {
        return Err(rule(
            "discovery",
            "known rows must exactly match the capability matrix",
        ));
    }
    for row in &matrix.rows {
        if row.owner.is_empty()
            || !matches!(row.priority.as_str(), "P0" | "P1" | "P2")
            || row.symbols.is_empty()
            || row.fixture_class.is_empty()
            || !matches!(
                row.rust_status.as_str(),
                "planned" | "implemented" | "blocked" | "excluded"
            )
            || !matches!(
                row.divergence_status.as_str(),
                "none" | "intentional" | "unresolved"
            )
        {
            return Err(rule(
                &row.id,
                "required ownership/status fields are invalid",
            ));
        }
        match row.support {
            SupportClass::RequiredNative if row.rust_status == "excluded" => {
                return Err(rule(
                    &row.id,
                    "required-native rows cannot have excluded Rust status",
                ));
            }
            SupportClass::Excluded
                if row.rust_status != "excluded" || row.divergence_status != "intentional" =>
            {
                return Err(rule(
                    &row.id,
                    "excluded rows require excluded Rust status and an intentional divergence",
                ));
            }
            _ => {}
        }
        for path in row.source_paths.iter().chain(&row.test_paths) {
            validate_reference(checkout, &row.id, path)?;
        }
        for dependency in &row.dependencies {
            if !ids.contains(dependency.as_str()) {
                return Err(MatrixError::MissingDependency {
                    row: row.id.clone(),
                    dependency: dependency.clone(),
                });
            }
        }
        if row.divergence_status == "intentional" {
            let declaration = row.divergence.as_ref().ok_or_else(|| {
                rule(
                    &row.id,
                    "intentional divergence requires rationale, scope, fixtures, and documentation",
                )
            })?;
            if [
                &declaration.rationale,
                &declaration.scope,
                &declaration.upstream_fixture,
                &declaration.rust_fixture,
                &declaration.documentation,
            ]
            .into_iter()
            .any(|value| value.trim().is_empty())
            {
                return Err(rule(
                    &row.id,
                    "intentional divergence declaration fields must not be empty",
                ));
            }
            for path in [
                &declaration.upstream_fixture,
                &declaration.rust_fixture,
                &declaration.documentation,
            ] {
                validate_root_relative(&row.id, path)?;
            }
        } else if row.divergence.is_some() {
            return Err(rule(
                &row.id,
                "divergence details require intentional divergence status",
            ));
        }
    }
    let excluded = matrix
        .rows
        .iter()
        .filter(|row| row.support == SupportClass::Excluded)
        .collect::<Vec<_>>();
    if excluded.len() != 1 || excluded[0].id != "surface.python-custom-tools" {
        return Err(rule(
            "surface.python-custom-tools",
            "the Python custom-tool boundary must be the only excluded row",
        ));
    }
    let python_row = excluded[0];
    if python_row.owner != "US-031" {
        return Err(rule(
            &python_row.id,
            "the excluded Python custom-tool row must be owned by US-031",
        ));
    }
    if ids.contains("surface.python-host-feasibility") || ids.contains("surface.python-tools") {
        return Err(rule(
            "surface.python-custom-tools",
            "legacy Python host capability rows are forbidden",
        ));
    }
    let mcp_stdio_row = matrix
        .rows
        .iter()
        .find(|row| row.id == "surface.mcp-stdio-extension")
        .ok_or_else(|| MatrixError::MissingDiscovery("surface.mcp-stdio-extension".to_owned()))?;
    let expected_dependencies = [
        "surface.config-layers",
        "surface.extension-discovery",
        "surface.mcp",
        "surface.tools",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    let actual_dependencies = mcp_stdio_row
        .dependencies
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if mcp_stdio_row.owner != "US-032"
        || mcp_stdio_row.support != SupportClass::RequiredNative
        || mcp_stdio_row.rust_status != "implemented"
        || actual_dependencies != expected_dependencies
        || actual_dependencies.contains("surface.python-custom-tools")
    {
        return Err(rule(
            &mcp_stdio_row.id,
            "the MCP stdio extension row must be native, implemented, owned by US-032, and depend on config, discovery, MCP, and tools only",
        ));
    }
    let protocol_row = matrix
        .rows
        .iter()
        .find(|row| row.id == "protocol.app-server")
        .ok_or_else(|| MatrixError::MissingDiscovery("protocol.app-server".to_owned()))?;
    let matrix_methods = protocol_row
        .items
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let expected_methods = SERVER_METHODS.into_iter().collect::<BTreeSet<_>>();
    if matrix_methods != expected_methods || protocol_row.items.len() != matrix_methods.len() {
        return Err(rule(
            "protocol.app-server",
            "method inventory must exactly match all 79 pinned methods",
        ));
    }
    let native_row = matrix
        .rows
        .iter()
        .find(|row| row.id == "surface.native-targets")
        .ok_or_else(|| MatrixError::MissingDiscovery("surface.native-targets".to_owned()))?;
    if native_row.items.len() != 5 {
        return Err(rule(
            "surface.native-targets",
            "exactly five native targets are required",
        ));
    }
    Ok(matrix)
}

fn validate_reference(checkout: &Path, row: &str, raw: &str) -> Result<(), MatrixError> {
    validate_root_relative(row, raw)?;
    let path = Path::new(raw);
    if !checkout.join(path).exists() {
        return Err(rule(row, &format!("referenced path does not exist: {raw}")));
    }
    Ok(())
}

fn validate_root_relative(row: &str, raw: &str) -> Result<(), MatrixError> {
    let path = Path::new(raw);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(rule(row, &format!("path is not root-relative: {raw}")));
    }
    Ok(())
}

fn rule(row: &str, rule: &str) -> MatrixError {
    MatrixError::Rule {
        row: row.to_owned(),
        rule: rule.to_owned(),
    }
}
