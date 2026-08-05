//! Bringing a configuration file written by an older client forward.
//!
//! Reference `vibe/core/config/_migration.py` runs four migrations over each
//! writable TOML layer before the first merge: the bash allowlist repair, the
//! one-shot read-only command sync recorded in `applied_migrations`, the
//! `devstral-2` model rename, and the `read`/`search_replace` tool rename with
//! its option transfer. They run in that order and each reports whether it
//! changed the document, so a file that needs nothing is never rewritten.
//!
//! Command names and migration identifiers are behavioral observations taken
//! from the pinned reference, not authored prose.

use toml::{Table, Value};

use crate::platform::Platform;

/// The one-shot identifier recorded in `applied_migrations` once the default
/// read-only commands have been unioned into an existing allowlist.
pub const BASH_READ_ONLY_MIGRATION: &str = "bash_read_only_defaults_v1";

/// Old tool name to new tool name. The new tools replaced these in place, so a
/// configuration keyed by the old names has its settings moved over.
const RENAMED_TOOLS: [(&str, &str); 2] = [("read", "read_file"), ("search_replace", "edit")];

/// Options the old tool carried that the new one does not, dropped on transfer.
const DROPPED_TOOL_OPTIONS: [(&str, &[&str]); 1] =
    [("edit", &["max_content_size", "create_backup"])];

const TOOL_LIST_FIELDS: [&str; 2] = ["enabled_tools", "disabled_tools"];
const APPLIED_MIGRATIONS: &str = "applied_migrations";
const RENAMED_MODEL_NAME: &str = "mistral-vibe-cli-latest";
const RENAMED_MODEL_OLD_ALIAS: &str = "devstral-2";
const RENAMED_MODEL_NEW_ALIAS: &str = "mistral-medium-3.5";

const READ_ONLY_COMMANDS_WINDOWS: [&str; 6] = ["dir", "findstr", "more", "type", "ver", "where"];
const READ_ONLY_COMMANDS_POSIX: [&str; 37] = [
    "basename",
    "cat",
    "comm",
    "cut",
    "date",
    "diff",
    "dirname",
    "du",
    "file",
    "find",
    "fmt",
    "fold",
    "grep",
    "head",
    "join",
    "less",
    "ls",
    "md5sum",
    "more",
    "nl",
    "od",
    "paste",
    "pwd",
    "readlink",
    "sha1sum",
    "sha256sum",
    "shasum",
    "sort",
    "stat",
    "sum",
    "tac",
    "tail",
    "tr",
    "uname",
    "uniq",
    "wc",
    "which",
];

/// The commands the reference allowlists as read-only for `platform`.
#[must_use]
pub fn default_read_only_commands(platform: Platform) -> Vec<&'static str> {
    match platform {
        Platform::Posix | Platform::GitBash => READ_ONLY_COMMANDS_POSIX.to_vec(),
        Platform::Windows => READ_ONLY_COMMANDS_WINDOWS.to_vec(),
    }
}

/// The platform whose command set this process migrates against.
#[must_use]
pub const fn host_platform() -> Platform {
    if cfg!(windows) {
        Platform::Windows
    } else {
        Platform::Posix
    }
}

/// Applies every migration to `document` in order, and reports whether any of
/// them changed it. Reference `migrate_config`.
pub fn migrate_document(document: &mut Table) -> bool {
    let platform = host_platform();
    let mut changed = migrate_bash_allowlist(document);
    changed |= migrate_bash_read_only(document, platform);
    changed |= migrate_model_renames(document);
    changed |= migrate_renamed_tools(document);
    changed
}

/// Adds `find` to the bash allowlist and strips any trailing wildcard.
/// Reference `_migrate_bash_allowlist`.
fn migrate_bash_allowlist(document: &mut Table) -> bool {
    let Some(allowlist) = bash_allowlist_mut(document) else {
        return false;
    };
    let mut entries: Vec<String> = allowlist
        .iter()
        .filter_map(|entry| entry.as_str().map(str::to_owned))
        .collect();
    if entries.len() != allowlist.len() {
        // A non-string entry is not something this migration can reason about.
        return false;
    }
    let mut changed = false;
    if !entries.iter().any(|entry| entry == "find") {
        entries.push("find".to_owned());
        entries.sort();
        changed = true;
    }
    if entries.iter().any(|entry| entry.ends_with(" *")) {
        let mut stripped: Vec<String> = entries
            .iter()
            .map(|entry| {
                entry
                    .strip_suffix(" *")
                    .map_or_else(|| entry.clone(), str::to_owned)
            })
            .collect();
        stripped.sort();
        stripped.dedup();
        entries = stripped;
        changed = true;
    }
    if changed {
        *allowlist = entries.into_iter().map(Value::String).collect();
    }
    changed
}

/// Unions the default read-only commands into an existing allowlist once, and
/// records the identifier so a later run leaves the list alone. Reference
/// `_migrate_bash_read_only`.
fn migrate_bash_read_only(document: &mut Table, platform: Platform) -> bool {
    let Some(allowlist) = bash_allowlist_mut(document) else {
        return false;
    };
    let mut entries: Vec<String> = allowlist
        .iter()
        .filter_map(|entry| entry.as_str().map(str::to_owned))
        .collect();
    if entries.len() != allowlist.len() {
        return false;
    }
    let applied = applied_migrations(document);
    if applied
        .iter()
        .any(|entry| entry == BASH_READ_ONLY_MIGRATION)
    {
        return false;
    }
    entries.extend(
        default_read_only_commands(platform)
            .into_iter()
            .map(str::to_owned),
    );
    entries.sort();
    entries.dedup();
    let Some(allowlist) = bash_allowlist_mut(document) else {
        return false;
    };
    *allowlist = entries.into_iter().map(Value::String).collect();
    let mut recorded = applied;
    recorded.push(BASH_READ_ONLY_MIGRATION.to_owned());
    document.insert(
        APPLIED_MIGRATIONS.to_owned(),
        Value::Array(recorded.into_iter().map(Value::String).collect()),
    );
    true
}

/// Renames the `devstral-2` alias and brings its pricing and thinking level
/// forward, rekeying an alias-keyed map and repointing `active_model`.
/// Reference `_migrate_model_renames`.
fn migrate_model_renames(document: &mut Table) -> bool {
    let mut changed = false;
    let mut rekey = false;
    match document.get_mut("models") {
        Some(Value::Table(models)) => {
            for (_, entry) in models.iter_mut() {
                if let Some(model) = entry.as_table_mut() {
                    let (updated, renamed) = migrate_model_entry(model);
                    changed |= updated;
                    rekey |= renamed;
                }
            }
            if rekey && let Some(entry) = models.remove(RENAMED_MODEL_OLD_ALIAS) {
                models.insert(RENAMED_MODEL_NEW_ALIAS.to_owned(), entry);
            }
        }
        Some(Value::Array(models)) => {
            for entry in models.iter_mut() {
                if let Some(model) = entry.as_table_mut() {
                    changed |= migrate_model_entry(model).0;
                }
            }
        }
        _ => {}
    }
    if document
        .get("active_model")
        .and_then(Value::as_str)
        .is_some_and(|active| active == RENAMED_MODEL_OLD_ALIAS)
    {
        document.insert(
            "active_model".to_owned(),
            Value::String(RENAMED_MODEL_NEW_ALIAS.to_owned()),
        );
        changed = true;
    }
    changed
}

/// Migrates one model entry, reporting whether it changed and whether the entry
/// carried the alias a keyed map has to rename.
fn migrate_model_entry(model: &mut Table) -> (bool, bool) {
    let named = model
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|name| name == RENAMED_MODEL_NAME);
    if !named {
        return (false, false);
    }
    let mut changed = false;
    let mut renamed = false;
    if model
        .get("alias")
        .and_then(Value::as_str)
        .is_some_and(|alias| alias == RENAMED_MODEL_OLD_ALIAS)
    {
        model.insert(
            "alias".to_owned(),
            Value::String(RENAMED_MODEL_NEW_ALIAS.to_owned()),
        );
        model.insert("temperature".to_owned(), Value::Float(1.0));
        model.insert("input_price".to_owned(), Value::Float(1.5));
        model.insert("output_price".to_owned(), Value::Float(7.5));
        model.insert("thinking".to_owned(), Value::String("high".to_owned()));
        changed = true;
        renamed = true;
    }
    if model
        .get("alias")
        .and_then(Value::as_str)
        .is_some_and(|alias| alias == RENAMED_MODEL_NEW_ALIAS)
        && !model.contains_key("supports_images")
    {
        model.insert("supports_images".to_owned(), Value::Boolean(true));
        changed = true;
    }
    (changed, renamed)
}

/// Moves settings from the old tool names to the new ones and rewrites the
/// names inside the enable and disable lists. Reference
/// `_migrate_renamed_tools`.
fn migrate_renamed_tools(document: &mut Table) -> bool {
    let mut changed = false;
    if let Some(Value::Table(tools)) = document.get_mut("tools") {
        for (old, new) in RENAMED_TOOLS {
            let Some(mut settings) = tools.remove(old) else {
                continue;
            };
            changed = true;
            // An already-present new key wins: the operator configured it
            // deliberately and the old one is dropped rather than merged.
            if tools.contains_key(new) {
                continue;
            }
            if let Some(table) = settings.as_table_mut() {
                for (tool, dropped) in DROPPED_TOOL_OPTIONS {
                    if tool == new {
                        for option in dropped {
                            table.remove(*option);
                        }
                    }
                }
            }
            tools.insert(new.to_owned(), settings);
        }
    }
    for field in TOOL_LIST_FIELDS {
        let Some(Value::Array(names)) = document.get_mut(field) else {
            continue;
        };
        let mut renamed = false;
        for name in names.iter_mut() {
            let Some(current) = name.as_str() else {
                continue;
            };
            if let Some((_, new)) = RENAMED_TOOLS.iter().find(|(old, _)| *old == current) {
                *name = Value::String((*new).to_owned());
                renamed = true;
            }
        }
        changed |= renamed;
    }
    changed
}

fn bash_allowlist_mut(document: &mut Table) -> Option<&mut Vec<Value>> {
    match document
        .get_mut("tools")?
        .as_table_mut()?
        .get_mut("bash")?
        .as_table_mut()?
        .get_mut("allowlist")?
    {
        Value::Array(entries) => Some(entries),
        _ => None,
    }
}

fn applied_migrations(document: &Table) -> Vec<String> {
    document
        .get(APPLIED_MIGRATIONS)
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}
