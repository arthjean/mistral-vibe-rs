//! The native manifest and its `ai.mistral.vibe` extension. Reference
//! `_PluginManifest` and `_VibePluginExtension` in
//! `vibe/core/plugins/_native.py`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::super::compatibility::{PluginToolOverride, read_text};
use super::super::diagnostics::PluginConfigIssue;
use super::super::strict::{self, ValidationErrors, at, char_len};
use super::{Resolve, VIBE_EXTENSION};

#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static PLUGIN_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-z0-9](?:[a-z0-9.-]*[a-z0-9])?$").expect("plugin name pattern compiles")
});
#[expect(clippy::expect_used, reason = "compile-time constant patterns")]
static TYPESCRIPT_IDENTIFIER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z_$][A-Za-z0-9_$]*$").expect("identifier pattern compiles")
});

/// The validated native manifest. Reference `_PluginManifest`.
#[derive(Debug, Clone)]
pub(super) struct Manifest {
    pub(super) name: String,
    pub(super) version: Option<String>,
    pub(super) description: Option<String>,
    pub(super) author_name: Option<String>,
    extensions: Option<Map<String, Value>>,
    pub(super) dump: Value,
}

/// The validated `ai.mistral.vibe` extension. Reference
/// `_VibePluginExtension`.
#[derive(Debug, Clone, Default)]
pub(super) struct Extension {
    pub(super) tool_namespace: Option<String>,
    pub(super) tool_overrides: BTreeMap<String, PluginToolOverride>,
}

impl Resolve<'_> {
    pub(super) fn parse_vibe_extension(
        &mut self,
        manifest: &Manifest,
        manifest_path: &Path,
    ) -> Option<Extension> {
        let raw = manifest.extensions.as_ref()?.get(VIBE_EXTENSION)?;
        // The reference reads a missing extension as `None` with `.get`, and an
        // explicit `null` the same way.
        if raw.is_null() {
            return None;
        }
        match parse_extension(raw) {
            Ok(extension) => Some(extension),
            Err(error) => {
                self.issues.push(PluginConfigIssue::plain(
                    manifest_path,
                    format!("Could not load the {VIBE_EXTENSION} extension: {error}"),
                ));
                None
            }
        }
    }
}

pub(super) fn parse_manifest(path: &Path) -> Result<Manifest, String> {
    let text = read_text(path).map_err(|error| error.to_string())?;
    let raw: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
    let mut errors = ValidationErrors::default();
    let allowed = [
        "$schema",
        "schema_id",
        "name",
        "version",
        "description",
        "author",
        "homepage",
        "repository",
        "license",
        "keywords",
        "extensions",
    ];
    let Some(map) = strict::object(&raw, "", &allowed, &mut errors) else {
        return Err(errors.render("plugin manifest"));
    };
    let schema = map.get("$schema").or_else(|| map.get("schema_id"));
    match schema {
        Some(Value::String(schema)) if schema == super::super::compatibility::NATIVE_SCHEMA => {}
        None => errors.push("$schema", "is required"),
        Some(_) => errors.push("$schema", "must name the Agent Plugins 1.0 schema"),
    }
    let name = strict::required_string(map, "", "name", &mut errors);
    if let Some(name) = &name {
        let length = char_len(name);
        if length == 0 || length > 64 {
            errors.push("name", "must hold 1 to 64 characters");
        } else if !PLUGIN_NAME.is_match(name) {
            errors.push(
                "name",
                "must be lowercase letters, digits, dots and hyphens",
            );
        } else if name.contains("--") || name.contains("..") {
            errors.push("name", "cannot repeat a separator");
        }
    }
    let version = strict::optional_string(map, "", "version", &mut errors);
    let description = strict::optional_string(map, "", "description", &mut errors);
    let author = match map.get("author") {
        None | Some(Value::Null) => Some(None),
        Some(value) => {
            let mut author_errors = ValidationErrors::default();
            let fields = strict::object(
                value,
                "author",
                &["name", "email", "url"],
                &mut author_errors,
            );
            let parsed = fields.map(|fields| {
                (
                    strict::optional_string(fields, "author", "name", &mut author_errors),
                    strict::optional_string(fields, "author", "email", &mut author_errors),
                    strict::optional_string(fields, "author", "url", &mut author_errors),
                )
            });
            let valid = author_errors.is_empty();
            errors.0.extend(author_errors.0);
            match parsed {
                Some((Some(name), Some(email), Some(url))) if valid => {
                    Some(Some((name, email, url)))
                }
                _ => None,
            }
        }
    };
    let homepage = strict::optional_string(map, "", "homepage", &mut errors);
    let repository = strict::optional_string(map, "", "repository", &mut errors);
    let license = strict::optional_string(map, "", "license", &mut errors);
    let keywords = strict::string_list(map, "", "keywords", true, &mut errors);
    let extensions = match map.get("extensions") {
        None | Some(Value::Null) => Some(None),
        Some(Value::Object(entries)) => Some(Some(entries.clone())),
        Some(_) => {
            errors.push("extensions", "must be an object or null");
            None
        }
    };
    if !errors.is_empty() {
        return Err(errors.render("plugin manifest"));
    }
    let (
        Some(name),
        Some(version),
        Some(description),
        Some(author),
        Some(homepage),
        Some(repository),
        Some(license),
        Some(keywords),
        Some(extensions),
    ) = (
        name,
        version,
        description,
        author,
        homepage,
        repository,
        license,
        keywords,
        extensions,
    )
    else {
        return Err(errors.render("plugin manifest"));
    };
    let optional = |value: &Option<String>| value.clone().map_or(Value::Null, Value::String);
    let dump = serde_json::json!({
        "schema_id": super::super::compatibility::NATIVE_SCHEMA,
        "name": name,
        "version": optional(&version),
        "description": optional(&description),
        "author": author.as_ref().map_or(Value::Null, |(name, email, url)| serde_json::json!({
            "name": optional(name),
            "email": optional(email),
            "url": optional(url),
        })),
        "homepage": optional(&homepage),
        "repository": optional(&repository),
        "license": optional(&license),
        "keywords": keywords.clone().map_or(Value::Null, |keywords| Value::Array(keywords.into_iter().map(Value::String).collect())),
        "extensions": extensions.clone().map_or(Value::Null, Value::Object),
    });
    Ok(Manifest {
        name,
        version,
        description,
        author_name: author.and_then(|(name, _, _)| name),
        extensions,
        dump,
    })
}

fn parse_extension(raw: &Value) -> Result<Extension, String> {
    let mut errors = ValidationErrors::default();
    let allowed = [
        "schemaVersion",
        "schema_version",
        "toolNamespace",
        "tool_namespace",
        "toolOverrides",
        "tool_overrides",
    ];
    let Some(map) = strict::object(raw, "", &allowed, &mut errors) else {
        return Err(errors.render("ai.mistral.vibe extension"));
    };
    let mut aliased = map.clone();
    for (alias, name) in [
        ("schemaVersion", "schema_version"),
        ("toolNamespace", "tool_namespace"),
        ("toolOverrides", "tool_overrides"),
    ] {
        if let Some(value) = map.get(alias).or_else(|| map.get(name)) {
            aliased.insert(alias.to_owned(), value.clone());
        }
    }
    strict::integer_literal(&aliased, "", "schemaVersion", 1, true, &mut errors);
    let tool_namespace = strict::optional_string(&aliased, "", "toolNamespace", &mut errors);
    if let Some(Some(namespace)) = &tool_namespace
        && !TYPESCRIPT_IDENTIFIER.is_match(namespace)
    {
        errors.push("toolNamespace", "must be an identifier");
    }
    let mut tool_overrides = BTreeMap::new();
    match aliased.get("toolOverrides") {
        None => {}
        Some(Value::Object(overrides)) => {
            for (key, value) in overrides {
                let location = at("toolOverrides", key);
                let Some(fields) =
                    strict::object(value, &location, &["name", "exposure"], &mut errors)
                else {
                    continue;
                };
                let name = strict::optional_string(fields, &location, "name", &mut errors);
                if let Some(Some(name)) = &name
                    && !TYPESCRIPT_IDENTIFIER.is_match(name)
                {
                    errors.push(at(&location, "name"), "must be an identifier");
                }
                let exposure = match fields.get("exposure") {
                    None | Some(Value::Null) => Some(None),
                    _ => strict::string_choice(
                        fields,
                        &location,
                        "exposure",
                        &["programmatic", "direct", "direct_and_programmatic"],
                        &mut errors,
                    ),
                };
                if let (Some(name), Some(exposure)) = (name, exposure) {
                    tool_overrides.insert(key.clone(), PluginToolOverride { name, exposure });
                }
            }
        }
        Some(_) => errors.push("toolOverrides", "must be an object"),
    }
    if !errors.is_empty() {
        return Err(errors.render("ai.mistral.vibe extension"));
    }
    Ok(Extension {
        tool_namespace: tool_namespace.flatten(),
        tool_overrides,
    })
}
