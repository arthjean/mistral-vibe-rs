//! Which file a prompt identifier resolves to, and what an unresolved one says.

use std::fs;
use std::path::PathBuf;

use tempfile::TempDir;

use super::library::{PromptFileError, UtilityPrompt, load_prompt};

const SETTING: &str = "compaction_prompt_id";

fn builtins() -> Vec<UtilityPrompt> {
    vec![UtilityPrompt::Compact]
}

fn write(directory: &TempDir, name: &str, contents: &str) -> PathBuf {
    let path = directory.path().join(name);
    fs::write(&path, contents).expect("the fixture prompt is written");
    path
}

/// US-152: a project override wins over a user one, and both win over the
/// built-in.
#[test]
fn a_project_override_wins_over_a_user_one_and_over_the_builtin() {
    let project = TempDir::new().expect("a project prompt directory");
    let user = TempDir::new().expect("a user prompt directory");
    write(&project, "compact.md", "from the project\n");
    write(&user, "compact.md", "from the user");
    let directories = vec![project.path().to_path_buf(), user.path().to_path_buf()];

    assert_eq!(
        load_prompt("compact", SETTING, &directories, &builtins()),
        Ok("from the project".to_owned())
    );
    assert_eq!(
        load_prompt("compact", SETTING, &directories[1..], &builtins()),
        Ok("from the user".to_owned())
    );
    assert_eq!(
        load_prompt("compact", SETTING, &[], &builtins()),
        Ok(UtilityPrompt::Compact.text().to_owned())
    );
}

/// US-152: an identifier that matches nothing names the setting, the value, the
/// available built-ins and the directories searched.
#[test]
fn an_unmatched_identifier_names_the_setting_the_builtins_and_the_directories() {
    let project = TempDir::new().expect("a project prompt directory");
    write(&project, "terse.md", "be terse");
    let directories = vec![project.path().to_path_buf()];

    let error = load_prompt("nowhere", SETTING, &directories, &builtins())
        .expect_err("an unknown identifier does not resolve");
    let message = error.to_string();
    assert!(message.contains(SETTING), "{message}");
    assert!(message.contains("'nowhere'"), "{message}");
    assert!(message.contains("\"compact\""), "{message}");
    assert!(
        message.contains(&project.path().display().to_string()),
        "{message}"
    );
    assert!(
        message.contains("\"terse\""),
        "the error lists what the directory does carry: {message}"
    );
}

/// US-152: a value carrying a path separator is refused before any directory is
/// touched, so a setting can never read a file outside the prompt directories.
#[test]
fn an_identifier_with_a_separator_is_refused() {
    for value in ["../secrets", "a/b", "a\\b", "", ".", ".."] {
        assert!(
            matches!(
                load_prompt(value, SETTING, &[], &builtins()),
                Err(PromptFileError::InvalidId { .. })
            ),
            "`{value}` must be refused as an identifier"
        );
    }
}

/// The three compaction prompts ship, are non-empty, and carry the directives
/// their consumers depend on: the summary element the extractor reads, and the
/// no-tool-call instruction the tool-call failure exists to catch.
#[test]
fn the_shipped_compaction_prompts_carry_their_load_bearing_directives() {
    let request = UtilityPrompt::Compact.text();
    assert!(request.contains("<summary></summary>"), "{request}");
    assert!(request.contains("tool"), "{request}");
    let system = UtilityPrompt::CompactSystem.text();
    assert!(system.contains("<summary></summary>"), "{system}");
    assert!(!UtilityPrompt::CompactSummaryPrefix.text().is_empty());
    assert_eq!(UtilityPrompt::Compact.id(), "compact");
    assert_eq!(UtilityPrompt::CompactSystem.id(), "compact_system");
    assert_eq!(
        UtilityPrompt::CompactSummaryPrefix.id(),
        "compact_summary_prefix"
    );
}
