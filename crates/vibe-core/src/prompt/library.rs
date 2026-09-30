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
use std::path::PathBuf;

/// A prompt this crate ships, addressable by the identifier a setting carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UtilityPrompt {
    /// The wrapper the system prompt puts around the `AGENTS.md` documents,
    /// with a `$sections` placeholder for them.
    AgentsDoc,
    /// The compaction request, sent as a user message on the live transcript.
    Compact,
    /// The system message the dedicated fallback summarizer runs under.
    CompactSystem,
    /// The marker an older build wrote in front of an injected summary. It is
    /// read as a filter and never sent to a model.
    CompactSummaryPrefix,
    /// The project context a session opened in a dangerous directory gets
    /// instead of a Git scan, with `$reason` and `$abs_path` placeholders.
    DangerousDirectory,
    /// The project context block, with `$abs_path` and `$git_status`
    /// placeholders.
    ProjectContext,
    /// The system message the worktree naming model runs under
    /// (`vibe/core/prompts/worktree_name.md` upstream; this text is this
    /// repository's own).
    WorktreeName,
    /// The system message the session title model runs under
    /// (`vibe/core/prompts/session_title.md` upstream; this text is this
    /// repository's own).
    SessionTitle,
    /// The system message a turn summary runs under. Only reachable here as a
    /// bundled file a `system_prompt_id` can name.
    TurnSummary,
    /// The system message an image description runs under. Only reachable here
    /// as a bundled file a `system_prompt_id` can name.
    VisionDescribe,
}

impl UtilityPrompt {
    /// Every prompt this crate ships, in the reference's declaration order.
    pub const ALL: [Self; 10] = [
        Self::AgentsDoc,
        Self::Compact,
        Self::CompactSummaryPrefix,
        Self::CompactSystem,
        Self::DangerousDirectory,
        Self::ProjectContext,
        Self::SessionTitle,
        Self::TurnSummary,
        Self::VisionDescribe,
        Self::WorktreeName,
    ];

    /// The identifier this prompt answers to, which is what a setting names.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::AgentsDoc => "agents_doc",
            Self::Compact => "compact",
            Self::CompactSystem => "compact_system",
            Self::CompactSummaryPrefix => "compact_summary_prefix",
            Self::DangerousDirectory => "dangerous_directory",
            Self::ProjectContext => "project_context",
            Self::WorktreeName => "worktree_name",
            Self::SessionTitle => "session_title",
            Self::TurnSummary => "turn_summary",
            Self::VisionDescribe => "vision_describe",
        }
    }

    /// The shipped text, trimmed the way the reference trims every prompt it
    /// reads.
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            Self::AgentsDoc => include_str!("assets/agents_doc.md").trim_ascii(),
            Self::Compact => include_str!("assets/compact.md").trim_ascii(),
            Self::CompactSystem => include_str!("assets/compact_system.md").trim_ascii(),
            Self::CompactSummaryPrefix => {
                include_str!("assets/compact_summary_prefix.md").trim_ascii()
            }
            Self::WorktreeName => include_str!("assets/worktree_name.md").trim_ascii(),
            Self::SessionTitle => include_str!("assets/session_title.md").trim_ascii(),
            Self::DangerousDirectory => include_str!("assets/dangerous_directory.md").trim_ascii(),
            Self::ProjectContext => include_str!("assets/project_context.md").trim_ascii(),
            Self::TurnSummary => include_str!("assets/turn_summary.md").trim_ascii(),
            Self::VisionDescribe => include_str!("assets/vision_describe.md").trim_ascii(),
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
    validate_prompt_id(prompt_id, setting_name)?;
    let file_name = with_md_suffix(prompt_id);
    for directory in directories {
        let candidate = directory.join(&file_name);
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

/// Reference `_validate_prompt_id`: a prompt identifier is a bare file name.
///
/// # Errors
///
/// [`PromptFileError::InvalidId`] for an empty value, `.`, `..`, or a value
/// holding a path separator.
pub(crate) fn validate_prompt_id(
    prompt_id: &str,
    setting_name: &str,
) -> Result<(), PromptFileError> {
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
    Ok(())
}

/// The file name `Path(prompt_id).with_suffix(".md")` names: the identifier's
/// own suffix, if it has one, is replaced rather than extended, so `cli.v2`
/// looks for `cli.md`.
pub(crate) fn with_md_suffix(prompt_id: &str) -> String {
    let suffix = python_suffix(prompt_id);
    format!("{}.md", &prompt_id[..prompt_id.len() - suffix.len()])
}

/// `PurePath.suffix` of a single name: from its last dot, when that dot is
/// neither the first nor the last character.
fn python_suffix(name: &str) -> &str {
    match name.rfind('.') {
        Some(index) if index > 0 && index + 1 < name.len() => &name[index..],
        _ => "",
    }
}

/// `PurePath.stem` of a single name.
fn python_stem(name: &str) -> &str {
    &name[..name.len() - python_suffix(name).len()]
}

/// Every identifier the searched directories do carry, sorted and deduplicated,
/// so the error can list what the operator could have typed instead.
pub(crate) fn available_ids(directories: &[PathBuf]) -> Vec<String> {
    let mut found: Vec<String> = directories
        .iter()
        .filter_map(|directory| fs::read_dir(directory).ok())
        .flat_map(|entries| entries.flatten().collect::<Vec<_>>())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".md"))
        .map(|name| python_stem(&name).to_owned())
        .collect();
    found.sort_unstable();
    found.dedup();
    found
}
