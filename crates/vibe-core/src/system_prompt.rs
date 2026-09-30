//! The system message every request of a session opens with.
//!
//! Reference `get_universal_system_prompt` (`vibe/core/system_prompt.py`)
//! joins up to eleven sections with a blank line: the configured prompt with
//! its date filled in, the headless directive, the commit trailer, the model
//! name, then (under `include_prompt_detail`) the operating system and shell,
//! the skills, the subagents and the scratchpad, and finally (under
//! `include_project_context`) the repository snapshot or the dangerous
//! directory notice, the other open roots, and the `AGENTS.md` documents.
//! [`compose`] reproduces that order and every condition in it.
//!
//! The words are this port's own. `NOTICE` forbids shipping the reference's
//! prompt files and the prose its module writes, so every builtin prompt,
//! template and fixed sentence here covers the same directives in different
//! wording, and the parity oracle compares the structure and the data a
//! section carries rather than its prose (`docs/parity.md`, row 19).

use std::path::{Path, PathBuf};

use crate::prompt::library::{PromptFileError, UtilityPrompt};

pub mod project_context;
pub mod template;

#[cfg(test)]
mod system_prompt_tests;

pub use project_context::{GitContext, ProjectContextSettings, git_context, parse_git_log};
pub use template::safe_substitute;

/// The setting a system prompt identifier is read from.
pub const SYSTEM_PROMPT_SETTING: &str = "system_prompt_id";

/// The system prompt a configuration that names none runs under.
pub const DEFAULT_SYSTEM_PROMPT: &str = "cli";

/// A builtin system prompt. Reference `SystemPrompt`, in its declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemPrompt {
    Cli,
    Explore,
    Tests,
    Lean,
    Minimal,
}

impl SystemPrompt {
    pub const ALL: [Self; 5] = [
        Self::Cli,
        Self::Explore,
        Self::Tests,
        Self::Lean,
        Self::Minimal,
    ];

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Explore => "explore",
            Self::Tests => "tests",
            Self::Lean => "lean",
            Self::Minimal => "minimal",
        }
    }

    /// The shipped text, trimmed as the reference trims every prompt it reads.
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            Self::Cli => include_str!("system_prompt/assets/cli.md").trim_ascii(),
            Self::Explore => include_str!("system_prompt/assets/explore.md").trim_ascii(),
            Self::Tests => include_str!("system_prompt/assets/tests.md").trim_ascii(),
            Self::Lean => include_str!("system_prompt/assets/lean.md").trim_ascii(),
            Self::Minimal => include_str!("system_prompt/assets/minimal.md").trim_ascii(),
        }
    }
}

/// The two experiment variants the reference bundles beside its builtins,
/// which a `system_prompt_id` reaches through the bundled-file fallback.
const BUNDLED_VARIANTS: [(&str, &str); 2] = [
    (
        "cli_2026-07_v2",
        include_str!("system_prompt/assets/cli_2026-07_v2.md"),
    ),
    (
        "cli_2026-08_v3",
        include_str!("system_prompt/assets/cli_2026-08_v3.md"),
    ),
];

/// The text of a file the reference ships in its prompt directory, by file
/// name. Every one of them is reachable as a system prompt: reference
/// `load_system_prompt` admits any bundled `.md` file an identifier names.
fn bundled_file(file_name: &str) -> Option<&'static str> {
    let stem = file_name.strip_suffix(".md")?;
    SystemPrompt::ALL
        .into_iter()
        .find(|prompt| prompt.id() == stem)
        .map(SystemPrompt::text)
        .or_else(|| {
            BUNDLED_VARIANTS
                .iter()
                .find(|(id, _)| *id == stem)
                .map(|(_, text)| text.trim_ascii())
        })
        .or_else(|| {
            UtilityPrompt::ALL
                .into_iter()
                .find(|prompt| prompt.id() == stem)
                .map(UtilityPrompt::text)
        })
}

/// The text `prompt_id` names, searching `directories` (the project prompt
/// directories, then the user one) before the builtins.
///
/// Reference `load_system_prompt` over `load_prompt`. A file in a directory
/// wins, found under the identifier with its suffix replaced by `.md` as
/// `Path.with_suffix` replaces it. Then a bundled file of exactly that name,
/// then a builtin matched case-insensitively. An identifier that answers to
/// none of them is an error naming the five builtins and the directories.
///
/// # Errors
///
/// [`PromptFileError::InvalidId`] for an identifier that is not a bare file
/// name, [`PromptFileError::Missing`] for one nothing answers to.
pub fn load_system_prompt(
    prompt_id: &str,
    directories: &[PathBuf],
) -> Result<String, PromptFileError> {
    crate::prompt::library::validate_prompt_id(prompt_id, SYSTEM_PROMPT_SETTING)?;
    let file_name = crate::prompt::library::with_md_suffix(prompt_id);
    for directory in directories {
        let candidate = directory.join(&file_name);
        if candidate.is_file()
            && let Some(text) = read_prompt_file(&candidate)
        {
            return Ok(text);
        }
    }
    let lowered = prompt_id.to_lowercase();
    if let Some(builtin) = SystemPrompt::ALL
        .into_iter()
        .find(|prompt| prompt.id() == lowered)
    {
        return Ok(builtin.text().to_owned());
    }
    if let Some(text) = bundled_file(&file_name) {
        return Ok(text.to_owned());
    }
    Err(PromptFileError::Missing {
        setting_name: SYSTEM_PROMPT_SETTING.to_owned(),
        prompt_id: prompt_id.to_owned(),
        builtins: SystemPrompt::ALL
            .into_iter()
            .map(|prompt| prompt.id().to_owned())
            .collect(),
        directories: directories.to_vec(),
        available: crate::prompt::library::available_ids(directories),
    })
}

/// A prompt file decoded as the reference's `read_safe` decodes it, stripped.
fn read_prompt_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let decoded = crate::workspace::text_file::decode(&bytes);
    Some(crate::text::python_strip(&decoded.text).to_owned())
}

/// Today's date as the base prompt's `$current_date` spells it:
/// `YYYY-MM-DD (Weekday)`, in the local time zone. Reference
/// `_format_current_date`, which reads `date.today()`.
#[must_use]
pub fn current_date() -> String {
    format_date(jiff::Zoned::now().date())
}

/// `date` as `YYYY-MM-DD (Weekday)`, with the English day names `strftime`
/// writes under the C locale.
#[must_use]
pub fn format_date(date: jiff::civil::Date) -> String {
    let weekday = match date.weekday() {
        jiff::civil::Weekday::Monday => "Monday",
        jiff::civil::Weekday::Tuesday => "Tuesday",
        jiff::civil::Weekday::Wednesday => "Wednesday",
        jiff::civil::Weekday::Thursday => "Thursday",
        jiff::civil::Weekday::Friday => "Friday",
        jiff::civil::Weekday::Saturday => "Saturday",
        jiff::civil::Weekday::Sunday => "Sunday",
    };
    format!(
        "{:04}-{:02}-{:02} ({weekday})",
        date.year(),
        date.month(),
        date.day()
    )
}

/// The platform name the prompt states. Reference `get_platform_display_name`,
/// whose fallback for an unlisted platform is `Unix-like`.
#[must_use]
pub fn platform_display_name() -> &'static str {
    match std::env::consts::OS {
        "windows" => "Windows",
        "macos" => "macOS",
        "linux" => "Linux",
        "freebsd" => "FreeBSD",
        "openbsd" => "OpenBSD",
        "netbsd" => "NetBSD",
        _ => "Unix-like",
    }
}

/// The shell the operating system section describes.
///
/// Reference `_get_tool_aware_os_system_prompt`: a POSIX host names `$SHELL`;
/// a Windows session that publishes the `git_bash` tool, or failing that the
/// `powershell` one, gets that shell's rules; any other Windows session gets
/// the rules of the shell the legacy command tool resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellEnvironment {
    /// A POSIX host; the value is `$SHELL`, or `sh` when it is unset.
    Posix { shell: String },
    /// Windows with the `git_bash` tool published.
    GitBashTool,
    /// Windows with the `powershell` tool published and no `git_bash`.
    PowerShellTool,
    /// Windows driving a detected Git Bash through the command tool.
    Bash { executable: String },
    /// Windows driving `cmd.exe` through the command tool.
    Cmd { executable: String },
}

impl ShellEnvironment {
    /// What this host offers a session publishing `available_tools`.
    #[must_use]
    pub fn detect(available_tools: &[String]) -> Self {
        if !cfg!(windows) {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_owned());
            return Self::Posix { shell };
        }
        Self::for_windows(
            available_tools,
            crate::tools::shell::resolve_windows_shell(),
        )
    }

    /// The Windows answer for a session publishing `available_tools` on a host
    /// whose command tool resolves to `resolved`.
    #[must_use]
    pub fn for_windows(
        available_tools: &[String],
        resolved: crate::tools::shell::WindowsShell,
    ) -> Self {
        let publishes = |name: &str| available_tools.iter().any(|tool| tool == name);
        if publishes("git_bash") {
            return Self::GitBashTool;
        }
        if publishes("powershell") {
            return Self::PowerShellTool;
        }
        match resolved {
            crate::tools::shell::WindowsShell::Bash(executable) => Self::Bash {
                executable: executable.display().to_string(),
            },
            crate::tools::shell::WindowsShell::Cmd(executable) => Self::Cmd {
                executable: executable.display().to_string(),
            },
        }
    }
}

/// One skill as the prompt lists it. Reference `SkillInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSkill {
    pub name: String,
    pub description: String,
    pub path: Option<PathBuf>,
    pub model_invocable: bool,
    pub user_invocable: bool,
}

/// One subagent as the prompt lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSubagent {
    pub name: String,
    pub description: String,
}

/// What the project half of the prompt reads.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectInputs {
    /// The session's working directory.
    pub cwd: PathBuf,
    /// The operator's home, which the dangerous directory table hangs off.
    pub home: Option<PathBuf>,
    pub settings: ProjectContextSettings,
    /// The open project roots, resolved: the trusted working directory, then
    /// the `--add-dir` entries. Reference `HarnessFilesManager.project_roots`.
    pub project_roots: Vec<PathBuf>,
    /// Where the user-level `AGENTS.md` lives, and what it says.
    pub user_instructions: (PathBuf, String),
    /// The project `AGENTS.md` documents, outermost first.
    pub project_instructions: Vec<(PathBuf, String)>,
}

/// Everything [`compose`] reads.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemPromptInputs {
    /// The `system_prompt_id` the base was loaded under.
    pub prompt_id: String,
    /// The configured prompt's text, before its date is filled in.
    pub base: String,
    /// What `$current_date` becomes. See [`current_date`].
    pub current_date: String,
    pub headless: bool,
    pub include_commit_signature: bool,
    pub include_model_info: bool,
    pub include_prompt_detail: bool,
    pub include_project_context: bool,
    /// The alias of the model the session runs.
    pub model_alias: String,
    pub platform: String,
    pub shell: ShellEnvironment,
    /// The skills the session can load, in discovery order.
    pub skills: Vec<PromptSkill>,
    /// The subagents the session can start, in registry order.
    pub subagents: Vec<PromptSubagent>,
    /// The session's scratchpad; a subagent has none.
    pub scratchpad: Option<PathBuf>,
    pub project: ProjectInputs,
}

/// Which part of the prompt a section is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    Base,
    Headless,
    CommitSignature,
    ModelInfo,
    OperatingSystem,
    Skills,
    Subagents,
    Scratchpad,
    ProjectContext,
    DangerousDirectory,
    AdditionalDirectories,
    Instructions,
}

impl SectionKind {
    /// The name the parity corpus records the section under.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Headless => "headless",
            Self::CommitSignature => "commit_signature",
            Self::ModelInfo => "model_info",
            Self::OperatingSystem => "operating_system",
            Self::Skills => "skills",
            Self::Subagents => "subagents",
            Self::Scratchpad => "scratchpad",
            Self::ProjectContext => "project_context",
            Self::DangerousDirectory => "dangerous_directory",
            Self::AdditionalDirectories => "additional_directories",
            Self::Instructions => "instructions",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSection {
    pub kind: SectionKind,
    pub text: String,
}

/// The composed prompt, with what its data-bearing sections were built from.
#[derive(Debug, Clone, PartialEq)]
pub struct ComposedSystemPrompt {
    pub sections: Vec<PromptSection>,
    /// The repository snapshot, when the project context section carries one.
    pub git: Option<GitContext>,
    /// The dangerous directory the working directory is, when it is one.
    pub dangerous: Option<&'static str>,
    /// The open roots listed besides the working directory.
    pub additional_directories: Vec<PathBuf>,
}

impl ComposedSystemPrompt {
    /// The system message: every section, separated by a blank line.
    #[must_use]
    pub fn text(&self) -> String {
        self.sections
            .iter()
            .map(|section| section.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// Reference `get_universal_system_prompt`.
#[must_use]
pub fn compose(inputs: &SystemPromptInputs) -> ComposedSystemPrompt {
    let mut composed = ComposedSystemPrompt {
        sections: Vec::new(),
        git: None,
        dangerous: None,
        additional_directories: Vec::new(),
    };
    let mut sections: Vec<PromptSection> = Vec::new();
    let mut push = |kind: SectionKind, text: String| {
        sections.push(PromptSection { kind, text });
    };
    push(
        SectionKind::Base,
        safe_substitute(&inputs.base, &[("current_date", &inputs.current_date)]),
    );
    if inputs.headless {
        push(SectionKind::Headless, HEADLESS_SECTION.to_owned());
    }
    if inputs.include_commit_signature {
        push(SectionKind::CommitSignature, COMMIT_SIGNATURE.to_owned());
    }
    if inputs.include_model_info {
        push(
            SectionKind::ModelInfo,
            format!("Your model name is: `{}`", inputs.model_alias),
        );
    }
    if inputs.include_prompt_detail {
        push(
            SectionKind::OperatingSystem,
            operating_system_section(&inputs.platform, &inputs.shell),
        );
        if let Some(skills) = skills_section(&inputs.skills) {
            push(SectionKind::Skills, skills);
        }
        if let Some(subagents) = subagents_section(&inputs.subagents) {
            push(SectionKind::Subagents, subagents);
        }
        if let Some(scratchpad) = &inputs.scratchpad {
            push(SectionKind::Scratchpad, scratchpad_section(scratchpad));
        }
    }
    if inputs.include_project_context {
        let project = &inputs.project;
        let cwd = crate::config::harness::resolve_lenient(&project.cwd);
        match dangerous_directory(&cwd, project.home.as_deref()) {
            Some(description) => {
                composed.dangerous = Some(description);
                push(
                    SectionKind::DangerousDirectory,
                    safe_substitute(
                        UtilityPrompt::DangerousDirectory.text(),
                        &[
                            (
                                "reason",
                                &format!("the working directory is the {description}"),
                            ),
                            ("abs_path", &cwd.display().to_string()),
                        ],
                    ),
                );
            }
            None => {
                let git = git_context(&cwd, project.settings);
                push(
                    SectionKind::ProjectContext,
                    safe_substitute(
                        UtilityPrompt::ProjectContext.text(),
                        &[
                            ("abs_path", &cwd.display().to_string()),
                            ("git_status", &git.render()),
                        ],
                    ),
                );
                composed.git = Some(git);
            }
        }
        let additional = project
            .project_roots
            .iter()
            .filter(|root| crate::config::harness::resolve_lenient(root) != cwd)
            .cloned()
            .collect::<Vec<_>>();
        if !additional.is_empty() {
            let lines = additional
                .iter()
                .map(|root| format!(" - {}", root.display()))
                .collect::<Vec<_>>()
                .join("\n");
            push(
                SectionKind::AdditionalDirectories,
                format!(
                    "Other working directories, with the same file-access permissions as the \
                     primary one:\n{lines}"
                ),
            );
            composed.additional_directories = additional;
        }
        if let Some(instructions) =
            instructions_section(&project.user_instructions, &project.project_instructions)
        {
            push(SectionKind::Instructions, instructions);
        }
    }
    composed.sections = sections;
    composed
}

/// The directive a run with no human behind it carries. Reference
/// `_get_headless_section`.
pub const HEADLESS_SECTION: &str = "# Headless Mode\n\n\
     Nobody is at the keyboard for this run.\n\
     Ask no questions, request no confirmation and wait for no input.\n\
     When something is ambiguous, take the most reasonable reading and carry on.\n\
     Finish the whole task in one pass and deliver a complete final result.\n\
     This replaces any earlier instruction to pause for confirmation or to ask the user.";

/// Reference `_add_commit_signature`. The trailer is reproduced as the
/// reference writes it, because it is what ends up in the repository's history
/// rather than prose addressed to the model.
const COMMIT_SIGNATURE: &str = "Make commits with the `git commit` shell command.\n\
     End every commit message with the trailer below, which credits Mistral Vibe as a co-author.\n\
     Always follow this layout exactly:\n\n\
     ```bash\n\
     git commit -m <commit message>\n\n\
     Generated by Mistral Vibe.\n\
     Co-Authored-By: Mistral Vibe <vibe@mistral.ai>\n\
     ```";

/// Reference `_get_os_system_prompt`.
fn operating_system_section(platform: &str, shell: &ShellEnvironment) -> String {
    match shell {
        ShellEnvironment::Posix { shell } => {
            format!("Operating system: {platform}. Commands run in the shell `{shell}`")
        }
        ShellEnvironment::GitBashTool => format!(
            "Operating system: {platform}. Commands run in the shell `Git Bash`\n{GIT_BASH_RULES}"
        ),
        ShellEnvironment::PowerShellTool => format!(
            "Operating system: {platform}. Commands run in the shell `PowerShell`\n{POWERSHELL_RULES}"
        ),
        ShellEnvironment::Bash { executable } => format!(
            "Operating system: {platform}. Commands run in the shell `bash ({executable})`\n\
             {GIT_BASH_RULES}"
        ),
        ShellEnvironment::Cmd { executable } => format!(
            "Operating system: {platform}. Commands run in the shell `{executable}`\n{CMD_RULES}"
        ),
    }
}

const GIT_BASH_RULES: &str = "### SHELL RULES FOR THIS HOST (MANDATORY):\n\
     - Commands go through bash (Git Bash), so `ls`, `grep`, `cat` and `find` are available; this is neither cmd.exe nor PowerShell\n\
     - Throw output away with `2>/dev/null`, never `2>nul` or `2>$null`\n\
     - `&&` and `||` chain commands\n\
     - Write paths with forward slashes; bash reaches Windows drives as `/c/Users/...`\n\
     - Test whether a command exists with `command -v <command>`\n\
     ### CHECK THAT A COMMAND SUITS THIS PLATFORM BEFORE PROPOSING IT";

const CMD_RULES: &str = "### SHELL RULES FOR THIS HOST (MANDATORY):\n\
     - The shell is cmd.exe, neither bash nor PowerShell\n\
     - Unix commands such as `ls`, `grep` and `cat` are missing; use `dir`, `findstr` and `type`\n\
     - Write paths with backslashes (\\\\)\n\
     - Throw output away with `2>nul`, never `2>/dev/null` or `2>$null`\n\
     - `&&` and `||` chain commands in cmd.exe\n\
     - Test whether a command exists with `where command`\n\
     - Shebang lines do not apply on Windows\n\
     ### CHECK THAT A COMMAND SUITS THIS PLATFORM BEFORE PROPOSING IT";

const POWERSHELL_RULES: &str = "### SHELL RULES FOR THIS HOST (MANDATORY):\n\
     - The shell is PowerShell, neither bash nor cmd.exe\n\
     - Write variables, quoting, pipelines, redirections and conditions in PowerShell syntax\n\
     - Write Windows paths with backslashes (\\\\) unless a command explicitly takes another form\n\
     - Throw output away with `*> $null` or `2>$null` as fits, never `2>/dev/null` or `2>nul`\n\
     - Test whether a command exists with `Get-Command <command>`\n\
     - Where no dedicated Vibe tool fits, prefer `Get-ChildItem`, `Get-Content` and `Select-String` to Unix-only commands\n\
     ### CHECK THAT A COMMAND SUITS THIS PLATFORM BEFORE PROPOSING IT";

/// Reference `_get_available_skills_section`: the model sees the skills it may
/// load, sorted by name, and learns how a `/name` message invokes one when any
/// skill is user-invocable.
fn skills_section(skills: &[PromptSkill]) -> Option<String> {
    let mut model: Vec<&PromptSkill> = skills
        .iter()
        .filter(|skill| skill.model_invocable)
        .collect();
    let user = skills.iter().any(|skill| skill.user_invocable);
    if model.is_empty() && !user {
        return None;
    }
    model.sort_by(|left, right| left.name.cmp(&right.name));
    let mut lines: Vec<String> = vec!["# Available Skills".to_owned(), String::new()];
    if !model.is_empty() {
        lines.push(
            "The skills below are available. When a task fits the description of one,".to_owned(),
        );
        lines.push(
            "load its full instructions with the `skill` tool if you have it; otherwise read its files yourself when they exist."
                .to_owned(),
        );
        lines.push(String::new());
    }
    if user {
        lines.push(
            "A user message that is exactly `/skill-name`, possibly followed by more".to_owned(),
        );
        lines.push(
            "instructions, is the user invoking that skill on purpose. Its instructions".to_owned(),
        );
        lines.push(
            "are loaded for you: a `skill` tool call and its result follow that message".to_owned(),
        );
        lines.push(
            "directly. Treat what was loaded as your current instructions and act on it;"
                .to_owned(),
        );
        lines.push("there is no need to call the `skill` tool yourself.".to_owned());
        lines.push(String::new());
    }
    if !model.is_empty() {
        lines.push("<available_skills>".to_owned());
        for skill in model {
            lines.push("  <skill>".to_owned());
            lines.push(format!("    <name>{}</name>", html_escape(&skill.name)));
            lines.push(format!(
                "    <description>{}</description>",
                html_escape(&skill.description)
            ));
            if let Some(path) = &skill.path {
                lines.push(format!(
                    "    <path>{}</path>",
                    html_escape(&path.display().to_string())
                ));
            }
            lines.push("  </skill>".to_owned());
        }
        lines.push("</available_skills>".to_owned());
    }
    Some(lines.join("\n"))
}

/// Python's `html.escape` with `quote=True`.
#[must_use]
pub fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Reference `_get_available_subagents_section`, in registry order.
fn subagents_section(subagents: &[PromptSubagent]) -> Option<String> {
    if subagents.is_empty() {
        return None;
    }
    let mut lines = vec![
        "# Available Subagents".to_owned(),
        String::new(),
        "You can hand work to these subagents through the Task tool:".to_owned(),
    ];
    for agent in subagents {
        lines.push(format!("- **{}**: {}", agent.name, agent.description));
    }
    Some(lines.join("\n"))
}

/// Reference `_get_scratchpad_section`.
fn scratchpad_section(scratchpad: &Path) -> String {
    format!(
        "# Scratchpad Directory\n\n\
         Your scratchpad directory is `{}`.\n\n\
         Put temporary material there: intermediate results, draft scripts, working files, \
         anything that does not belong in the project.\n\
         Writing there never asks for permission.\n\
         It lasts for this session and subagents share it.",
        scratchpad.display()
    )
}

/// Reference `get_agents_md_section`: the user document, then the project
/// ones under a heading of their own, inside the `agents_doc` wrapper, or
/// nothing when no document has content.
#[must_use]
pub fn instructions_section(
    user: &(PathBuf, String),
    project: &[(PathBuf, String)],
) -> Option<String> {
    let mut sections = Vec::new();
    let user_doc = crate::text::python_strip(&user.1);
    if !user_doc.is_empty() {
        sections.push(format!(
            "## User instructions\n\nFrom {} (the user's own instructions):\n\n{user_doc}",
            user.0.display()
        ));
    }
    if !project.is_empty() {
        sections.push("## Project instructions (checked into the codebase)".to_owned());
    }
    for (directory, content) in project {
        sections.push(format!(
            "From {}/{}:\n\n{}",
            directory.display(),
            crate::config::harness::AGENTS_FILE,
            crate::text::python_strip(content)
        ));
    }
    if sections.is_empty() {
        return None;
    }
    Some(safe_substitute(
        UtilityPrompt::AgentsDoc.text(),
        &[("sections", &sections.join("\n\n"))],
    ))
}

/// Which dangerous directory `path` is, if any. Reference
/// `is_dangerous_directory`, whose table compares the resolved path with the
/// home directory as the environment spells it and with five system folders.
#[must_use]
pub fn dangerous_directory(path: &Path, home: Option<&Path>) -> Option<&'static str> {
    let path = crate::config::harness::resolve_lenient(path);
    let mut table: Vec<(PathBuf, &'static str)> = Vec::new();
    if let Some(home) = home {
        table.extend([
            (home.to_path_buf(), "home directory"),
            (home.join("Documents"), "Documents folder"),
            (home.join("Desktop"), "Desktop folder"),
            (home.join("Downloads"), "Downloads folder"),
            (home.join("Pictures"), "Pictures folder"),
            (home.join("Movies"), "Movies folder"),
            (home.join("Music"), "Music folder"),
            (home.join("Library"), "Library folder"),
        ]);
    }
    table.extend([
        (PathBuf::from("/Applications"), "Applications folder"),
        (PathBuf::from("/System"), "System folder"),
        (PathBuf::from("/Library"), "System Library folder"),
        (PathBuf::from("/usr"), "System usr folder"),
        (PathBuf::from("/private"), "System private folder"),
    ]);
    table
        .into_iter()
        .find(|(candidate, _)| *candidate == path)
        .map(|(_, description)| description)
}
