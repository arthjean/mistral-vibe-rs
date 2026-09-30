//! What the composition does with each input, one rule at a time.

use std::path::PathBuf;

use super::*;

fn inputs(root: &Path) -> SystemPromptInputs {
    SystemPromptInputs {
        prompt_id: "cli".to_owned(),
        base: "Base prompt, dated $current_date.".to_owned(),
        current_date: "2026-09-30 (Wednesday)".to_owned(),
        headless: false,
        include_commit_signature: true,
        include_model_info: true,
        include_prompt_detail: true,
        include_project_context: true,
        model_alias: "devstral".to_owned(),
        platform: "Linux".to_owned(),
        shell: ShellEnvironment::Posix {
            shell: "/bin/zsh".to_owned(),
        },
        skills: vec![PromptSkill {
            name: "review".to_owned(),
            description: "Reviews <diffs> & 'quotes'".to_owned(),
            path: Some(root.join("skills/review/SKILL.md")),
            model_invocable: true,
            user_invocable: true,
        }],
        subagents: vec![PromptSubagent {
            name: "explore".to_owned(),
            description: "Reads the codebase".to_owned(),
        }],
        scratchpad: Some(root.join("scratch")),
        project: ProjectInputs {
            cwd: root.to_path_buf(),
            home: None,
            settings: ProjectContextSettings::default(),
            project_roots: vec![root.to_path_buf()],
            user_instructions: (root.join("home/AGENTS.md"), String::new()),
            project_instructions: Vec::new(),
        },
    }
}

fn kinds(composed: &ComposedSystemPrompt) -> Vec<&'static str> {
    composed
        .sections
        .iter()
        .map(|section| section.kind.name())
        .collect()
}

#[test]
fn sections_follow_the_reference_order_and_join_with_a_blank_line() {
    let root = tempfile::tempdir().expect("root");
    let mut inputs = inputs(root.path());
    inputs.headless = true;
    inputs.project.project_instructions = vec![(root.path().to_path_buf(), "Rule.".to_owned())];
    let composed = compose(&inputs);
    assert_eq!(
        kinds(&composed),
        [
            "base",
            "headless",
            "commit_signature",
            "model_info",
            "operating_system",
            "skills",
            "subagents",
            "scratchpad",
            "project_context",
            "instructions",
        ]
    );
    let text = composed.text();
    assert!(text.starts_with("Base prompt, dated 2026-09-30 (Wednesday).\n\n# Headless Mode"));
    assert!(text.contains("Your model name is: `devstral`"));
}

#[test]
fn each_include_key_gates_its_own_sections() {
    let root = tempfile::tempdir().expect("root");
    let mut inputs = inputs(root.path());
    inputs.include_commit_signature = false;
    inputs.include_model_info = false;
    inputs.include_prompt_detail = false;
    inputs.include_project_context = false;
    assert_eq!(kinds(&compose(&inputs)), ["base"]);
}

#[test]
fn skills_list_only_what_the_model_may_load_escaped_and_sorted() {
    let mut skills = vec![
        PromptSkill {
            name: "zeta".to_owned(),
            description: "Last".to_owned(),
            path: None,
            model_invocable: true,
            user_invocable: false,
        },
        PromptSkill {
            name: "alpha".to_owned(),
            description: "A \"quoted\" <tag> & 'it'".to_owned(),
            path: Some(PathBuf::from("/skills/alpha/SKILL.md")),
            model_invocable: true,
            user_invocable: false,
        },
        PromptSkill {
            name: "hidden".to_owned(),
            description: "User only".to_owned(),
            path: None,
            model_invocable: false,
            user_invocable: false,
        },
    ];
    let section = skills_section(&skills).expect("a section");
    assert!(section.find("<name>alpha</name>") < section.find("<name>zeta</name>"));
    assert!(section.contains("A &quot;quoted&quot; &lt;tag&gt; &amp; &#x27;it&#x27;"));
    assert!(section.contains("<path>/skills/alpha/SKILL.md</path>"));
    assert!(!section.contains("hidden"));
    assert!(!section.contains("/skill-name"));

    for skill in &mut skills {
        skill.model_invocable = false;
    }
    assert_eq!(skills_section(&skills), None);
    skills[0].user_invocable = true;
    let section = skills_section(&skills).expect("the invocation note alone");
    assert!(section.contains("/skill-name"));
    assert!(!section.contains("<available_skills>"));
}

#[test]
fn the_dangerous_directory_replaces_the_repository_snapshot() {
    let root = tempfile::tempdir().expect("root");
    let home = std::fs::canonicalize(root.path()).expect("home");
    let mut inputs = inputs(&home);
    inputs.project.home = Some(home.clone());
    let composed = compose(&inputs);
    assert_eq!(composed.dangerous, Some("home directory"));
    assert_eq!(composed.git, None);
    assert!(kinds(&composed).contains(&"dangerous_directory"));
    assert!(!kinds(&composed).contains(&"project_context"));
    assert_eq!(
        dangerous_directory(&home.join("Downloads"), Some(&home)),
        Some("Downloads folder")
    );
    assert_eq!(dangerous_directory(&home.join("src"), Some(&home)), None);
}

#[test]
fn other_open_roots_are_listed_without_the_working_directory() {
    let root = tempfile::tempdir().expect("root");
    let cwd = std::fs::canonicalize(root.path()).expect("cwd");
    let mut inputs = inputs(&cwd);
    inputs.project.project_roots = vec![cwd.clone(), cwd.join("extra")];
    let composed = compose(&inputs);
    assert_eq!(composed.additional_directories, [cwd.join("extra")]);
}

#[test]
fn instructions_put_the_user_document_before_the_project_ones() {
    let section = instructions_section(
        &(
            PathBuf::from("/home/.vibe/AGENTS.md"),
            "  User rule.\n".to_owned(),
        ),
        &[
            (PathBuf::from("/repo"), "Outer rule.".to_owned()),
            (PathBuf::from("/repo/app"), "Inner rule.".to_owned()),
        ],
    )
    .expect("a section");
    let user = section.find("User rule.").expect("user");
    let outer = section.find("From /repo/AGENTS.md:").expect("outer");
    let inner = section.find("From /repo/app/AGENTS.md:").expect("inner");
    assert!(user < outer && outer < inner);
    assert_eq!(
        instructions_section(&(PathBuf::from("/h/AGENTS.md"), " \n".to_owned()), &[]),
        None
    );
}

#[test]
fn safe_substitute_follows_the_python_rules() {
    let values = [("name", "value")];
    assert_eq!(safe_substitute("$name ${name}", &values), "value value");
    assert_eq!(safe_substitute("$names $other", &values), "$names $other");
    assert_eq!(
        safe_substitute("$$name costs $5 ${", &values),
        "$name costs $5 ${"
    );
}

#[test]
fn the_git_log_loses_a_trailing_parenthetical_but_keeps_the_refs() {
    assert_eq!(
        parse_git_log("abc1234 (HEAD -> main) Fix (the) edge case\n\ndef5678 Add parser (#12)\n"),
        ["abc1234 (HEAD -> main) Fix", "def5678 Add parser"]
    );
    assert_eq!(
        parse_git_log("abc1234 (tag) only refs"),
        ["abc1234 (tag) only refs"]
    );
}

#[test]
fn a_prompt_identifier_resolves_through_directories_then_builtins_then_bundled_files() {
    let directory = tempfile::tempdir().expect("prompts");
    std::fs::write(directory.path().join("cli.md"), "  custom cli  \n").expect("override");
    std::fs::write(directory.path().join("house.md"), "house").expect("custom");
    let directories = [directory.path().to_path_buf()];
    assert_eq!(
        load_system_prompt("cli", &directories).expect("file"),
        "custom cli"
    );
    assert_eq!(
        load_system_prompt("house.txt", &directories).expect("suffix"),
        "house"
    );
    assert_eq!(
        load_system_prompt("EXPLORE", &[]).expect("builtin"),
        SystemPrompt::Explore.text()
    );
    assert!(load_system_prompt("cli_2026-08_v3", &[]).is_ok());
    assert!(load_system_prompt("compact", &[]).is_ok());
    assert!(matches!(
        load_system_prompt("nowhere", &directories),
        Err(PromptFileError::Missing { available, builtins, .. })
            if available == ["cli", "house"] && builtins.len() == 5
    ));
    assert!(matches!(
        load_system_prompt("../escape", &directories),
        Err(PromptFileError::InvalidId { .. })
    ));
}

#[test]
fn the_date_names_its_weekday() {
    let date = jiff::civil::date(2026, 9, 30);
    assert_eq!(format_date(date), "2026-09-30 (Wednesday)");
}

#[test]
fn a_windows_session_prefers_the_published_shell_tools() {
    let cmd = || crate::tools::shell::WindowsShell::Cmd(PathBuf::from("C:\\cmd.exe"));
    let tools = |names: &[&str]| {
        names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ShellEnvironment::for_windows(&tools(&["powershell", "git_bash"]), cmd()),
        ShellEnvironment::GitBashTool
    );
    assert_eq!(
        ShellEnvironment::for_windows(&tools(&["powershell"]), cmd()),
        ShellEnvironment::PowerShellTool
    );
    assert_eq!(
        ShellEnvironment::for_windows(&tools(&["bash"]), cmd()),
        ShellEnvironment::Cmd {
            executable: "C:\\cmd.exe".to_owned()
        }
    );
}
