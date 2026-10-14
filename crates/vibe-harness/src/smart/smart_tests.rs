use super::*;

#[test]
fn a_path_that_decides_what_runs_later_is_protected() {
    let bash = |command: &str| protected_target("file_system.bash", &json!({"command": command}));
    assert!(bash("echo hi >> ~/.bashrc").is_some());
    assert!(bash("cp hook .git/hooks/pre-commit").is_some());
    assert!(bash("cat .git/x/../hooks/pre-push").is_some());
    assert!(bash("ls .VIBE/config.toml").is_some());
    assert!(bash("cd .vibe/worktrees/feature && ls").is_none());
    assert!(bash("ls .vibe/worktrees/../config.toml").is_some());
    assert!(bash("ls my.vibe.txt").is_none());
    assert!(bash("touch made.txt").is_none());
    let write = |path: &str| protected_target("file_system.write_file", &json!({"path": path}));
    assert_eq!(write(".zshrc").as_deref(), Some(".zshrc"));
    assert!(write("notes/zshrc.md").is_none());
    assert!(protected_target("file_system.read_file", &json!({"path": ".bashrc"})).is_none());
}

#[test]
fn the_verdict_follows_the_policy_rather_than_the_safe_flag() {
    let verdict = |text: &str| parse(text).map(|classification| classification.verdict);
    assert_eq!(
        verdict(r#"{"safe": true, "risk_level": "low"}"#),
        Ok(Verdict::Allow)
    );
    assert_eq!(
        verdict(r#"{"safe": true, "risk_level": "critical"}"#),
        Ok(Verdict::Prompt)
    );
    assert_eq!(
        verdict(r#"{"safe": true, "risk_level": "high", "user_authorization": "low"}"#),
        Ok(Verdict::Prompt)
    );
    assert_eq!(
        verdict(
            "```json\n{\"safe\": true, \"risk_level\": \"HIGH\", \"user_authorization\": \"Medium\"}\n```"
        ),
        Ok(Verdict::Allow)
    );
    assert_eq!(verdict(r#"{"reason": "no flag"}"#), Ok(Verdict::Prompt));
    assert!(parse(r#"{"safe": "yes"}"#).is_err());
    assert!(parse("not a verdict").is_err());
    assert!(parse("[true]").is_err());
}

#[test]
fn a_repeat_or_a_streak_goes_to_the_person() {
    let key = |name: &str| (name.to_owned(), "hash".to_owned());
    let mut state = SmartState::default();
    state.enter_turn("turn-1");
    assert!(!state.deny(key("a"), false, false));
    assert!(state.deny(key("a"), false, false));
    state.enter_turn("turn-2");
    assert!(!state.deny(key("b"), false, false));
    assert!(!state.deny(key("c"), false, false));
    assert!(state.deny(key("d"), false, false));
    state.proceed();
    assert!(state.deny(key("e"), false, true));
    assert!(state.deny(key("f"), true, false));
}
