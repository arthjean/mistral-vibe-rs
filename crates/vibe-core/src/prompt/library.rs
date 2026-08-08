//! The prompt files a setting names, and the chain that resolves one.
//!
//! A prompt identifier is a bare filename. Resolution prefers a `.md` file in a
//! project prompt directory, then one in a user prompt directory, then the
//! built-in of that name, which is the order `load_prompt` walks in the
//! reference. An identifier that matches nothing is an error naming the setting,
//! the value, the built-ins and the directories searched, because the operator
//! typed the value and has to be told which of the four it should have been.
//!
//! The built-in texts are this port's own prose. `NOTICE` forbids shipping the
//! reference's, and a prompt is functional rather than decorative here, so each
//! one is written to cover the same directives as its counterpart and the
//! divergence is recorded in `docs/parity.md`.
//!
//! Reference: `vibe/core/prompts/__init__.py` at the pinned commit.

use std::fs;
use std::path::{Path, PathBuf};

/// A prompt this crate ships, addressable by the identifier a setting carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UtilityPrompt {
    /// The compaction request, sent as a user message on the live transcript.
    Compact,
    /// The system message the dedicated fallback summarizer runs under.
    CompactSystem,
    /// The marker an older build wrote in front of an injected summary. It is
    /// read as a filter and never sent to a model.
    CompactSummaryPrefix,
}

impl UtilityPrompt {
    /// The identifier this prompt answers to, which is what a setting names.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::CompactSystem => "compact_system",
            Self::CompactSummaryPrefix => "compact_summary_prefix",
        }
    }

    /// The shipped text, trimmed the way the reference trims every prompt it
    /// reads.
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            Self::Compact => include_str!("assets/compact.md").trim_ascii(),
            Self::CompactSystem => include_str!("assets/compact_system.md").trim_ascii(),
            Self::CompactSummaryPrefix => {
                include_str!("assets/compact_summary_prefix.md").trim_ascii()
            }
        }
    }
}

/// Why a prompt identifier resolved to nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptFileError {
    /// The value is not a bare filename, so it can never name a prompt file.
    InvalidId {
        setting_name: String,
        prompt_id: String,
    },
    /// No directory and no built-in carries that identifier.
    Missing {
        setting_name: String,
        prompt_id: String,
        builtins: Vec<String>,
        directories: Vec<PathBuf>,
        available: Vec<String>,
    },
}

impl std::fmt::Display for PromptFileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidId {
                setting_name,
                prompt_id,
            } => write!(
                formatter,
                "invalid {setting_name} value: '{prompt_id}' must be a bare filename without path \
                 separators"
            ),
            Self::Missing {
                setting_name,
                prompt_id,
                builtins,
                directories,
                available,
            } => {
                let builtin_hint = quoted(builtins);
                let dirs_hint = if directories.is_empty() {
                    "<no prompt dirs>".to_owned()
                } else {
                    directories
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(" or ")
                };
                let available_hint = if available.is_empty() {
                    "<none>".to_owned()
                } else {
                    quoted(available)
                };
                write!(
                    formatter,
                    "invalid {setting_name} value: '{prompt_id}'. Must be one of the available \
                     prompts ({builtin_hint}), or correspond to a .md file in {dirs_hint} \
                     (available: {available_hint})"
                )
            }
        }
    }
}

impl std::error::Error for PromptFileError {}

fn quoted(values: &[String]) -> String {
    values
        .iter()
        .map(|value| format!("\"{value}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The text `prompt_id` resolves to, searching `directories` in order before
/// falling back to `builtins`.
///
/// `directories` is the project prompt directories followed by the user ones, so
/// a project override wins over a user one and both win over the built-in. The
/// identifier is matched case-sensitively against a directory and lowercased
/// against the built-ins, which is what the reference does.
///
/// # Errors
///
/// Returns [`PromptFileError::InvalidId`] when the value carries a path separator or
/// names a directory traversal, and [`PromptFileError::Missing`] when nothing
/// answers to it.
pub fn load_prompt(
    prompt_id: &str,
    setting_name: &str,
    directories: &[PathBuf],
    builtins: &[UtilityPrompt],
) -> Result<String, PromptFileError> {
    if prompt_id.is_empty()
        || prompt_id == "."
        || prompt_id == ".."
        || prompt_id.contains('/')
        || prompt_id.contains('\\')
    {
        return Err(PromptFileError::InvalidId {
            setting_name: setting_name.to_owned(),
            prompt_id: prompt_id.to_owned(),
        });
    }
    for directory in directories {
        let candidate = directory.join(format!("{prompt_id}.md"));
        if candidate.is_file()
            && let Ok(text) = fs::read_to_string(&candidate)
        {
            return Ok(text.trim().to_owned());
        }
    }
    let lowered = prompt_id.to_lowercase();
    if let Some(builtin) = builtins.iter().find(|builtin| builtin.id() == lowered) {
        return Ok(builtin.text().to_owned());
    }
    Err(PromptFileError::Missing {
        setting_name: setting_name.to_owned(),
        prompt_id: prompt_id.to_owned(),
        builtins: builtins
            .iter()
            .map(|builtin| builtin.id().to_owned())
            .collect(),
        directories: directories.to_vec(),
        available: available_ids(directories),
    })
}

/// Every identifier the searched directories do carry, sorted and deduplicated,
/// so the error can list what the operator could have typed instead.
fn available_ids(directories: &[PathBuf]) -> Vec<String> {
    let mut found: Vec<String> = directories
        .iter()
        .filter_map(|directory| fs::read_dir(directory).ok())
        .flat_map(|entries| entries.flatten().collect::<Vec<_>>())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        .filter_map(|path| stem_of(&path))
        .collect();
    found.sort_unstable();
    found.dedup();
    found
}

fn stem_of(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(ToOwned::to_owned)
}
