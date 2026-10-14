use super::*;

fn context() -> PromptContext {
    PromptContext {
        instructions: "Host instructions.".to_owned(),
        background_processes: true,
        subagents: true,
        groups: vec!["file_system".to_owned(), "self".to_owned()],
        ..PromptContext::default()
    }
}

fn position(prompt: &str, needle: &str) -> usize {
    prompt
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} is missing from the prompt"))
}

#[test]
fn the_core_sections_come_before_the_host_instructions_and_the_catalogs() {
    let mut context = context();
    context.skills = vec![SkillEntry {
        name: "vibe:vibe".to_owned(),
        description: "Vibe's own reference".to_owned(),
        path: "/skills/vibe/SKILL.md".to_owned(),
    }];
    context.plugins = vec![PluginEntry {
        name: "vibe".to_owned(),
        description: "Bundled".to_owned(),
        path: "/plugins/vibe".to_owned(),
        namespaces: Vec::new(),
    }];
    let prompt = system_prompt(&context);
    let order = [
        "## Background processes",
        "## Subagents",
        "## Available Skills",
        "## The current time",
        "Host instructions.",
        "### Searchable connector groups\n\n- file_system\n- self",
        "<available-skills>\n  <skill>",
        "<available-plugins>",
    ];
    let positions: Vec<usize> = order
        .iter()
        .map(|needle| position(&prompt, needle))
        .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "{positions:?}"
    );
    assert!(prompt.contains("<description>Vibe&apos;s own reference</description>"));
}

#[test]
fn a_subagent_session_drops_the_subagent_sections() {
    let mut context = context();
    context.subagents = false;
    context.agent_types = vec![AgentTypeEntry {
        name: "reviewer".to_owned(),
        description: "Reviews".to_owned(),
        path: "/agents/reviewer.toml".to_owned(),
    }];
    let prompt = system_prompt(&context);
    assert!(!prompt.contains("## Subagents"));
    assert!(!prompt.contains("<available-agent-types>"));
    context.subagents = true;
    assert!(system_prompt(&context).contains("<name>reviewer</name>"));
}

#[test]
fn knowledge_folders_name_their_source() {
    let mut context = context();
    context.knowledge = vec![
        KnowledgeEntry {
            name: "notes".to_owned(),
            description: "Team notes".to_owned(),
            path: "/knowledge/notes".to_owned(),
            access: "read_only".to_owned(),
            plugin: Some("team".to_owned()),
        },
        KnowledgeEntry {
            name: "docs".to_owned(),
            description: "Docs".to_owned(),
            path: "/knowledge/docs".to_owned(),
            access: "read_write".to_owned(),
            plugin: None,
        },
    ];
    let prompt = system_prompt(&context);
    assert!(position(&prompt, "<name>docs</name>") < position(&prompt, "<name>notes</name>"));
    assert!(prompt.contains("<type>standalone</type>"));
    assert!(prompt.contains("<plugin-name>team</plugin-name>"));
}
