//! The registry version store.
//!
//! Reference `vibe/core/skills/registry/_store.py`: one directory per skill id
//! under the store root, one directory per version under it, each holding a
//! generated `SKILL.md` plus the payload's assets. A version is built whole in
//! a staging directory and swapped in atomically, so a failed download never
//! leaves a partial cache; asset paths are normalized and rejected when they
//! escape the version directory or name its entrypoint; and executable assets
//! get the owner bit only, because registry content is external.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use super::models::RegistrySkillItem;
use crate::skills::parser::parse_skill_markdown;

/// The frontmatter `source` value every materialized skill carries.
const REGISTRY_SOURCE: &str = "ai-registry";

/// Entrypoint names an asset may never take in the version root, compared
/// after normalization and case folding so `sub/../SKILL.md` and `SKILLS.MD`
/// are both caught.
const RESERVED_ENTRYPOINTS: [&str; 2] = ["skill.md", "skills.md"];

/// What the store can answer instead of a path.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The registry-supplied skill id is not a plain single path component,
    /// rejected before any filesystem call so distinct ids can never collide
    /// or escape the store root.
    #[error("the registry skill id {0:?} is not a plain path component")]
    UnsafeId(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// The store root under a Vibe home, `skills-registry-cache/store`.
#[must_use]
pub fn store_root(vibe_home: &Path) -> PathBuf {
    vibe_home.join("skills-registry-cache").join("store")
}

/// A skill's cache directory under the resolved store root, guarding the id.
///
/// The id comes from the registry and is used as a single path segment:
/// anything empty, `.`, `..`, or holding a separator is rejected here, before
/// any filesystem call.
fn skill_root(root: &Path, skill_id: &str) -> Result<PathBuf, StoreError> {
    if !is_plain_component(skill_id) {
        return Err(StoreError::UnsafeId(skill_id.to_owned()));
    }
    let resolved = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    Ok(resolved.join(skill_id))
}

/// Whether `id` names exactly one normal path component, spelled as itself.
fn is_plain_component(id: &str) -> bool {
    if id.is_empty() || id == "." || id == ".." {
        return false;
    }
    let mut components = Path::new(id).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(first)), None) if first == OsStr::new(id)
    )
}

/// One version's directory under the store.
pub fn skill_dir(root: &Path, skill_id: &str, version: i64) -> Result<PathBuf, StoreError> {
    Ok(skill_root(root, skill_id)?.join(version.to_string()))
}

/// Whether one version is fully materialized, which its `SKILL.md` witnesses.
pub fn is_materialized(root: &Path, skill_id: &str, version: i64) -> Result<bool, StoreError> {
    Ok(skill_dir(root, skill_id, version)?
        .join("SKILL.md")
        .is_file())
}

/// The highest materialized version for a skill, or [`None`] when nothing is.
///
/// Used to resolve alias pins like `latest` to a concrete on-disk version,
/// offline-safe.
pub fn latest_materialized(root: &Path, skill_id: &str) -> Result<Option<i64>, StoreError> {
    let id_dir = skill_root(root, skill_id)?;
    if !id_dir.is_dir() {
        return Ok(None);
    }
    let mut latest = None;
    for entry in id_dir.read_dir()?.flatten() {
        if !entry.path().join("SKILL.md").is_file() {
            continue;
        }
        let Ok(version) = entry.file_name().to_string_lossy().parse::<i64>() else {
            continue;
        };
        latest = Some(latest.map_or(version, |known: i64| known.max(version)));
    }
    Ok(latest)
}

/// Writes one skill version into the store, answering its directory, or
/// [`None`] when the body is empty and nothing was stored.
pub fn materialize(
    root: &Path,
    item: &RegistrySkillItem,
    name: &str,
) -> Result<Option<PathBuf>, StoreError> {
    materialize_with(root, item, name, &write_assets)
}

/// [`materialize`] with the asset writer injectable, which is what lets a test
/// prove a failed write preserves the previous version.
pub(crate) fn materialize_with(
    root: &Path,
    item: &RegistrySkillItem,
    name: &str,
    write_assets_fn: &dyn Fn(&Path, &RegistrySkillItem) -> io::Result<()>,
) -> Result<Option<PathBuf>, StoreError> {
    let dest = skill_dir(root, &item.skill_id, item.version)?;
    let body = strip_frontmatter(&item.skill.skill_body).trim().to_owned();
    if body.is_empty() {
        // Drop any prior cache so `is_materialized` cannot report a stale hit.
        let _ = fs::remove_dir_all(&dest);
        return Ok(None);
    }

    // Build the whole version in a staging directory first, so a failed write
    // never leaves a partial cache.
    let parent = dest
        .parent()
        .ok_or_else(|| io::Error::other("the version directory has a parent"))?;
    fs::create_dir_all(parent)?;
    let suffix = crate::session_id::generate_session_id(None);
    let dest_name = dest.file_name().unwrap_or_default().to_string_lossy();
    let staging = parent.join(format!(".{dest_name}.tmp-{suffix}"));
    let backup = parent.join(format!(".{dest_name}.bak-{suffix}"));
    fs::create_dir(&staging)?;
    let built = fs::write(
        staging.join("SKILL.md"),
        build_skill_markdown(name, item, &body),
    )
    .and_then(|()| write_assets_fn(&staging, item));
    if let Err(error) = built {
        let _ = fs::remove_dir_all(&staging);
        return Err(error.into());
    }

    // Swap in atomically: move the old version aside, move the new one in,
    // then drop the old. If the swap fails, restore the previous cache.
    let had_previous = dest.exists();
    if had_previous {
        fs::rename(&dest, &backup)?;
    }
    if let Err(error) = fs::rename(&staging, &dest) {
        let _ = fs::remove_dir_all(&staging);
        if had_previous {
            let _ = fs::rename(&backup, &dest);
        }
        return Err(error.into());
    }
    if had_previous {
        let _ = fs::remove_dir_all(&backup);
    }
    Ok(Some(dest))
}

/// The fallback description a payload without one is published under. The
/// reference's sentence is its own prose, so this one is this port's, and the
/// replay holds the two digests permanently unequal.
pub(crate) fn fallback_description(name: &str) -> String {
    format!("Registry skill '{name}' published without a description.")
}

/// The generated entrypoint: original frontmatter carrying the resolved name
/// and description plus the registry provenance, then the stripped body.
fn build_skill_markdown(name: &str, item: &RegistrySkillItem, body: &str) -> String {
    let resolved = item.resolved_description();
    let description = if resolved.is_empty() {
        fallback_description(name)
    } else {
        resolved
    };
    format!(
        "---\nname: {}\ndescription: {}\nmetadata:\n  source: {}\n  skill_id: {}\n  version: {}\n---\n\n{body}\n",
        yaml_scalar(name),
        yaml_scalar(&description),
        yaml_scalar(REGISTRY_SOURCE),
        yaml_scalar(&item.skill_id),
        yaml_scalar(&item.version.to_string()),
    )
}

/// A body that already carries frontmatter loses it, so the generated
/// frontmatter is the only one; a body whose frontmatter does not parse is
/// kept whole.
fn strip_frontmatter(body: &str) -> String {
    if !body.trim_start().starts_with("---") {
        return body.to_owned();
    }
    match parse_skill_markdown(body) {
        Ok((_frontmatter, markdown)) => markdown,
        Err(_) => body.to_owned(),
    }
}

/// Writes the payload's assets under the staged version directory, skipping
/// unsafe destinations and undecodable content.
fn write_assets(dest: &Path, item: &RegistrySkillItem) -> io::Result<()> {
    let base = fs::canonicalize(dest).unwrap_or_else(|_| dest.to_path_buf());
    for (raw_path, asset) in &item.skill.skill_assets {
        let Some(target) = safe_dest(&base, raw_path) else {
            continue;
        };
        let Some(content) = asset.to_bytes() else {
            continue;
        };
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, content)?;
        #[cfg(unix)]
        if asset.is_executable {
            use std::os::unix::fs::PermissionsExt as _;
            // Owner-execute only: registry content is external, so it is
            // never made group- or world-executable.
            let mut permissions = fs::metadata(&target)?.permissions();
            permissions.set_mode(permissions.mode() | 0o100);
            fs::set_permissions(&target, permissions)?;
        }
    }
    Ok(())
}

/// The normalized destination of one asset path, or [`None`] when it must be
/// skipped: empty, escaping the version directory, naming the directory
/// itself, or normalizing to a reserved entrypoint name in the version root.
fn safe_dest(base: &Path, raw_path: &str) -> Option<PathBuf> {
    let cleaned = raw_path.trim();
    if cleaned.is_empty() {
        return None;
    }
    let target = normalize(&base.join(cleaned));
    // Must land strictly inside the version directory: the directory itself
    // and every traversal out of it are rejected.
    if target == base || !target.starts_with(base) {
        return None;
    }
    // Entrypoint names are rejected after normalization so `sub/../SKILL.md`
    // cannot slip past and overwrite the generated `SKILL.md`.
    if target.parent() == Some(base)
        && target.file_name().is_some_and(|name| {
            let folded = name.to_string_lossy().to_lowercase();
            RESERVED_ENTRYPOINTS.contains(&folded.as_str())
        })
    {
        return None;
    }
    Some(target)
}

/// Lexical normalization: `.` disappears, `..` pops, and popping at the root
/// stays at the root, which is how the reference's non-strict `resolve`
/// answers over the symlink-free staging tree.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

/// Copies a materialized version into `target` as a standalone local skill.
///
/// The registry frontmatter (`source`, `skill_id`, `version`) is dropped so
/// the result reads as a plain, user-owned skill; a copy whose `SKILL.md`
/// does not parse is left untouched, because it already carries valid text of
/// its own.
pub fn export_local(
    root: &Path,
    skill_id: &str,
    version: i64,
    target: &Path,
) -> Result<(), StoreError> {
    copy_tree(&skill_dir(root, skill_id, version)?, target)?;
    let skill_file = target.join("SKILL.md");
    let text = fs::read_to_string(&skill_file)?;
    let Ok((frontmatter, body)) = parse_skill_markdown(&text) else {
        return Ok(());
    };
    let name = frontmatter
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty())
        .map_or_else(
            || {
                target
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            },
            ToOwned::to_owned,
        );
    let description = frontmatter
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    fs::write(
        &skill_file,
        format!(
            "---\nname: {}\ndescription: {}\n---\n\n{}\n",
            yaml_scalar(&name),
            yaml_scalar(description),
            body.trim(),
        ),
    )?;
    Ok(())
}

fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    fs::create_dir_all(target)?;
    for entry in source.read_dir()?.flatten() {
        let destination = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &destination)?;
        } else {
            // `fs::copy` carries the permission bits, so an exported
            // executable asset stays executable.
            fs::copy(entry.path(), &destination)?;
        }
    }
    Ok(())
}

/// Removes every store entry outside the active `(skill_id, version)` set,
/// and every id directory the removal leaves empty.
pub fn prune(root: &Path, active: &BTreeSet<(String, i64)>) -> Result<(), StoreError> {
    if !root.is_dir() {
        return Ok(());
    }
    for id_entry in root.read_dir()?.flatten() {
        let id_dir = id_entry.path();
        if !id_dir.is_dir() {
            continue;
        }
        let id = id_entry.file_name().to_string_lossy().into_owned();
        for version_entry in id_dir.read_dir()?.flatten() {
            let version_dir = version_entry.path();
            if !version_dir.is_dir() {
                continue;
            }
            let Ok(version) = version_entry.file_name().to_string_lossy().parse::<i64>() else {
                continue;
            };
            if !active.contains(&(id.clone(), version)) {
                let _ = fs::remove_dir_all(&version_dir);
            }
        }
        if id_dir
            .read_dir()
            .is_ok_and(|mut entries| entries.next().is_none())
        {
            let _ = fs::remove_dir(&id_dir);
        }
    }
    Ok(())
}

// --------------------------------------------------------------------------
// The YAML scalar emitter
// --------------------------------------------------------------------------

/// One string as a YAML scalar, in the presentation the reference's dumper
/// chooses for these documents: plain where the plain form reads back as the
/// same string, single-quoted where it would resolve to another type or
/// break the syntax, and double-quoted with escapes where it holds control
/// characters.
fn yaml_scalar(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    if value.chars().any(|character| character.is_control()) {
        let mut quoted = String::from('"');
        for character in value.chars() {
            match character {
                '"' => quoted.push_str("\\\""),
                '\\' => quoted.push_str("\\\\"),
                '\n' => quoted.push_str("\\n"),
                '\t' => quoted.push_str("\\t"),
                '\r' => quoted.push_str("\\r"),
                other if other.is_control() => {
                    quoted.push_str(&format!("\\x{:02x}", other as u32));
                }
                other => quoted.push(other),
            }
        }
        quoted.push('"');
        return quoted;
    }
    if is_plain_safe(value) {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "''"))
}

/// Whether the plain presentation of `value` reads back as the same string.
fn is_plain_safe(value: &str) -> bool {
    let first = value.chars().next().unwrap_or_default();
    let last = value.chars().last().unwrap_or_default();
    if first.is_whitespace() || last.is_whitespace() {
        return false;
    }
    if "?:,[]{}#&*!|>'\"%@`".contains(first) {
        return false;
    }
    if first == '-' && value.chars().nth(1).is_none_or(char::is_whitespace) {
        return false;
    }
    if value.contains(": ") || value.ends_with(':') || value.contains(" #") {
        return false;
    }
    !resolves_implicitly(value)
}

/// Whether the YAML 1.1 resolver the reference dumps against would read the
/// plain form as a non-string: a boolean, a null, an integer or a float.
fn resolves_implicitly(value: &str) -> bool {
    const BOOLS: [&str; 18] = [
        "yes", "Yes", "YES", "no", "No", "NO", "true", "True", "TRUE", "false", "False", "FALSE",
        "on", "On", "ON", "off", "Off", "OFF",
    ];
    const NULLS: [&str; 5] = ["~", "null", "Null", "NULL", ""];
    if BOOLS.contains(&value) || NULLS.contains(&value) {
        return true;
    }
    let unsigned = value
        .strip_prefix(['-', '+'])
        .unwrap_or(value)
        .replace('_', "");
    if unsigned.is_empty() {
        return false;
    }
    let is_digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if is_digits(&unsigned)
        || unsigned.strip_prefix("0x").is_some_and(|rest| {
            !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        || unsigned.strip_prefix("0b").is_some_and(|rest| {
            !rest.is_empty() && rest.bytes().all(|byte| byte == b'0' || byte == b'1')
        })
        || unsigned.split(':').count() > 1 && unsigned.split(':').all(is_digits)
    {
        return true;
    }
    // Float shapes, including the 1.1 word forms.
    if [".inf", ".Inf", ".INF", ".nan", ".NaN", ".NAN"].contains(&unsigned.as_str()) {
        return true;
    }
    if unsigned.matches('.').count() == 1 {
        let (whole, fraction) = unsigned.split_once('.').unwrap_or_default();
        let mantissa_ok = (whole.is_empty() || is_digits(whole))
            && (fraction.is_empty() || is_digits(fraction) || {
                let (digits, exponent) = fraction.split_once(['e', 'E']).unwrap_or((fraction, "0"));
                (digits.is_empty() || is_digits(digits))
                    && is_digits(exponent.strip_prefix(['-', '+']).unwrap_or(exponent))
            });
        if mantissa_ok && unsigned != "." {
            return true;
        }
    }
    false
}
