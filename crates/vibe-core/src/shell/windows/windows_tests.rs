use super::*;
use crate::platform::{Platform, parse_policy_path};
use crate::shell::{analyze_shell, override_requirements};

fn lists() -> ShellCommandLists {
    ShellCommandLists::from_config(
        &crate::tools::config::ToolConfigResolver::new()
            .with_posix_shell(false)
            .view("powershell"),
    )
}

fn context() -> ShellPolicyContext {
    ShellPolicyContext::new(
        Platform::Windows,
        parse_policy_path(Platform::Windows, r"C:\work\project").expect("cwd"),
    )
}

fn analyze_in(command: &str, context: &ShellPolicyContext) -> ShellAnalysis {
    analyze_shell(ShellFlavor::PowerShell, command, context, &lists())
}

fn analyze(command: &str) -> ShellAnalysis {
    analyze_in(command, &context())
}

fn patterns(analysis: &ShellAnalysis) -> Vec<String> {
    analysis
        .requirements
        .iter()
        .map(|requirement| requirement.invocation_pattern.clone())
        .collect()
}

#[test]
fn a_reader_inside_the_workspace_runs() {
    for command in [
        "type notes.txt",
        r"type C:\work\project\README.md",
        "dir",
        "type a.txt 2>&1 | more",
    ] {
        let analysis = analyze(command);
        assert_eq!(
            analysis.mode,
            PermissionMode::Always,
            "`{command}`: {analysis:?}"
        );
    }
}

#[test]
fn a_denylisted_program_is_refused_under_every_name_it_runs_by() {
    for command in [
        "notepad",
        "notepad.exe notes.txt",
        r"C:\Windows\notepad.exe notes.txt",
        "& 'C:\\Windows\\notepad.exe' notes.txt",
        "powershell -NoExit",
        "pwsh",
        "type a.txt; cmd",
    ] {
        assert_eq!(
            analyze(command).mode,
            PermissionMode::Never,
            "`{command}` is refused"
        );
    }
    // An interpreter carrying a command is not the standalone one.
    assert_eq!(
        analyze("powershell -Command Get-Date").mode,
        PermissionMode::Ask
    );
}

/// A discarded redirection raises no redirection requirement, but its `$null`
/// is still an argument the path walk cannot expand, as upstream.
#[test]
fn a_discarded_redirection_leaves_only_its_variable() {
    let analysis = analyze("type a.txt > $null");
    assert_eq!(patterns(&analysis), vec!["dynamic path: $null".to_owned()]);
}

#[test]
fn an_output_redirection_to_a_file_is_its_own_requirement() {
    let analysis = analyze("type a.txt > out.txt");
    assert_eq!(analysis.mode, PermissionMode::Ask);
    assert_eq!(
        patterns(&analysis),
        vec!["output redirection: out.txt".to_owned()]
    );
    assert_eq!(
        analysis.requirements[0].label,
        "output redirection (out.txt)"
    );
}

#[test]
fn a_path_that_cannot_be_positioned_is_asked_about_as_written() {
    for token in [
        r"$env:VIBE_SURELY_UNSET_VARIABLE\x",
        "C:notes.txt",
        r"Registry::HKLM\x",
    ] {
        let command = format!("type {token}");
        let analysis = analyze(&command);
        assert_eq!(analysis.mode, PermissionMode::Ask, "`{command}`");
        assert_eq!(
            patterns(&analysis),
            vec![format!("dynamic path: {token}")],
            "`{command}`"
        );
    }
}

#[test]
fn a_path_outside_the_workspace_names_its_directory() {
    let analysis = analyze(r"type \\server\share\secret.txt");
    assert_eq!(analysis.mode, PermissionMode::Ask);
    assert!(
        analysis
            .requirements
            .iter()
            .all(|requirement| requirement.scope == PermissionScope::OutsideDirectory),
        "{analysis:?}"
    );
}

#[test]
fn a_subexpression_runs_its_own_commands() {
    let analysis = analyze("type $(Get-Item x)");
    assert_eq!(analysis.mode, PermissionMode::Ask);
    assert_eq!(
        patterns(&analysis),
        vec![
            "Get-Item x".to_owned(),
            "dynamic path: $(Get-Item".to_owned()
        ]
    );
    assert_eq!(analysis.requirements[0].session_pattern, "Get-Item *");
}

#[test]
fn an_override_withholds_the_grant_and_follows_the_commands() {
    let context = context().managed(
        ShellFlavor::PowerShell,
        None,
        override_requirements(Some("pwsh.exe"), &["B".to_owned()]),
    );
    let analysis = analyze_in("type notes.txt", &context);
    assert_eq!(analysis.mode, PermissionMode::Ask);
    assert_eq!(
        patterns(&analysis),
        vec![
            "shell override: pwsh.exe".to_owned(),
            "env override: B".to_owned()
        ]
    );
}

#[test]
fn an_environment_override_is_expanded_in_a_path() {
    let context = context().with_environment(vec![(
        "VIBE_TEST_TARGET".to_owned(),
        r"C:\work\project\sub".to_owned(),
    )]);
    let analysis = analyze_in(r"type $env:VIBE_TEST_TARGET\notes.txt", &context);
    assert_eq!(analysis.mode, PermissionMode::Always, "{analysis:?}");
}

#[test]
fn a_pager_session_is_recognized_through_its_invocation() {
    for command in [
        "git log",
        "& 'C:\\Program Files\\Git\\bin\\git.exe' log",
        "type a | more",
    ] {
        assert!(runs_pager(command, &["git", "less", "more"]), "`{command}`");
    }
    assert!(!runs_pager("Get-Content a", &["git", "less", "more"]));
}

/// Kept on purpose: a colon inside a name the Windows path parser rejects
/// cannot be positioned here, so it is asked about as an outside directory
/// where the reference joins it to the cwd and grants it.
#[test]
fn an_operand_the_path_parser_rejects_is_asked_about() {
    let analysis = analyze(r"type Env:\SECRET");
    assert_eq!(analysis.mode, PermissionMode::Ask, "{analysis:?}");
    assert!(
        analysis
            .requirements
            .iter()
            .all(|requirement| requirement.scope == PermissionScope::OutsideDirectory),
        "{analysis:?}"
    );
}
