use super::*;

#[test]
fn the_command_and_its_mentions_name_skills_outside_code() {
    assert_eq!(
        invoked_skill_names("/Review then /lint, and /usr/bin or /Users"),
        vec!["review".to_owned(), "lint".to_owned()]
    );
    assert_eq!(
        mentioned_skill_names("see `/inline` and\n```\n/fenced\n```\nthen /after"),
        vec!["after".to_owned()]
    );
    assert_eq!(
        mentioned_skill_names("```\n/open fence never closes"),
        Vec::<String>::new()
    );
    assert_eq!(mentioned_skill_names("a/b /c."), vec!["c".to_owned()]);
    assert_eq!(
        mentioned_skill_names("x ``a` /k ````b`` /m"),
        vec!["m".to_owned()]
    );
    assert_eq!(
        mentioned_skill_names("  ```` x\n/y\n ````` /z\n/w"),
        vec!["z".to_owned(), "w".to_owned()]
    );
}

#[test]
fn an_appended_body_becomes_a_settled_skill_effect() {
    let body = format!(
        "{}\n# Skill: review\n\nDo it.\n</skill_content>",
        skill_content_marker("review")
    );
    let message = json!({
        "id": "user-1",
        "sessionId": "s",
        "turnId": "t",
        "createdAt": 1,
        "updatedAt": 2,
        "type": "message",
        "role": "user",
        "content": [{"type": "text", "text": "/review now"}, {"type": "text", "text": body}],
        "userDisplayContent": with_display(None, &["review".to_owned()]),
    });
    let entries = expand_message(&message);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["content"].as_array().map(Vec::len), Some(1));
    assert_eq!(entries[0]["userDisplayContent"], Value::Null);
    assert_eq!(entries[1]["id"], "user-1:invoked-skill:review");
    assert_eq!(entries[1]["state"]["outputText"], body);
}

#[test]
fn a_preview_loses_the_body_appended_to_it() {
    let body = format!(
        "{}\n# Skill: review\n\nDo it.\n</skill_content>",
        skill_content_marker("review")
    );
    assert_eq!(
        public_preview(&format!("/review now {body}")),
        "/review now"
    );
    assert_eq!(
        public_preview("/review now <skill_content na…"),
        "/review now"
    );
    assert_eq!(public_preview("plain text"), "plain text");
}
