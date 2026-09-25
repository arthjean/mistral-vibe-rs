//! The key order an effect's input and output are dumped in.
//!
//! The reference projects a call's arguments and result through one pydantic
//! model per effect kind (`vibe/app_server/_effect_models.py`), so the keys of
//! `detail.input` and `state.output` come out in the order those models
//! declare them. This port carries both documents as JSON values, whose keys
//! serde orders alphabetically, so the programmatic writer puts them back in
//! the declared order before it prints them. A kind with no model, and a key a
//! model does not declare, keep the order they arrived in.

use super::pyjson::Ordered;

/// One model's keys, and the models of the lists it holds.
struct Model {
    keys: &'static [&'static str],
    lists: &'static [(&'static str, &'static Model)],
}

const fn flat(keys: &'static [&'static str]) -> Model {
    Model { keys, lists: &[] }
}

const TODO_ITEM: Model = flat(&["id", "content", "status", "priority"]);
const QUESTION_CHOICE: Model = flat(&["label", "description"]);
const QUESTION: Model = Model {
    keys: &["question", "header", "options", "multiSelect", "hideOther"],
    lists: &[("options", &QUESTION_CHOICE)],
};
const EDIT_CHANGE: Model = flat(&["oldString", "newString", "replaceAll"]);

/// Reference `*EffectInput`, by the kind's wire name.
fn input_model(kind: &str) -> Option<&'static Model> {
    const SHELL: Model = flat(&["command"]);
    const FILE_EDIT: Model = Model {
        keys: &[
            "filePath",
            "oldString",
            "newString",
            "replaceAll",
            "changes",
        ],
        lists: &[("changes", &EDIT_CHANGE)],
    };
    const FILE_SEARCH: Model = flat(&["pattern", "path", "maxMatches"]);
    const FILE_READ: Model = flat(&["filePath", "offset", "limit"]);
    const TODO: Model = Model {
        keys: &["action", "todos"],
        lists: &[("todos", &TODO_ITEM)],
    };
    const FILE_WRITE: Model = flat(&["filePath", "content"]);
    const USER_QUESTION: Model = Model {
        keys: &["questions", "footerNote"],
        lists: &[("questions", &QUESTION)],
    };
    const WEB_SEARCH: Model = flat(&["query"]);
    const WEB_FETCH: Model = flat(&["url", "timeout"]);
    const SKILL: Model = flat(&["name"]);
    const SUBAGENT: Model = flat(&["task", "agent"]);
    const WORKTREE: Model = flat(&["name", "branch", "path"]);
    Some(match kind {
        "shell" => &SHELL,
        "file_edit" => &FILE_EDIT,
        "file_search" => &FILE_SEARCH,
        "file_read" => &FILE_READ,
        "todo" => &TODO,
        "file_write" => &FILE_WRITE,
        "user_question" => &USER_QUESTION,
        "web_search" => &WEB_SEARCH,
        "web_fetch" => &WEB_FETCH,
        "skill" => &SKILL,
        "subagent" => &SUBAGENT,
        "worktree" => &WORKTREE,
        _ => return None,
    })
}

/// Reference `*EffectOutput`, by the kind's wire name.
fn output_model(kind: &str) -> Option<&'static Model> {
    const SHELL: Model = flat(&["stdout", "stderr", "output", "truncated"]);
    const OCCURRENCE: Model = flat(&["startLine", "oldText", "newText"]);
    const FILE_EDIT: Model = Model {
        keys: &["file", "oldString", "newString", "occurrences"],
        lists: &[("occurrences", &OCCURRENCE)],
    };
    const MATCH: Model = flat(&["path", "line"]);
    const FILE_SEARCH: Model = Model {
        keys: &["matches", "matchCount", "wasTruncated", "parsedMatches"],
        lists: &[("parsedMatches", &MATCH)],
    };
    const FILE_READ: Model = flat(&[
        "filePath",
        "content",
        "numLines",
        "startLine",
        "requestedOffset",
        "requestedLimit",
        "totalLines",
        "wasTruncated",
    ]);
    const TODO: Model = Model {
        keys: &["todos"],
        lists: &[("todos", &TODO_ITEM)],
    };
    const FILE_WRITE: Model = flat(&["filePath", "content"]);
    const ANSWER: Model = flat(&["question", "answer", "isOther"]);
    const USER_QUESTION: Model = Model {
        keys: &["answers", "cancelled"],
        lists: &[("answers", &ANSWER)],
    };
    const SOURCE: Model = flat(&["title", "url"]);
    const WEB_SEARCH: Model = Model {
        keys: &["query", "answer", "sources"],
        lists: &[("sources", &SOURCE)],
    };
    const WEB_FETCH: Model = flat(&["url", "content", "contentType", "wasTruncated"]);
    const SKILL: Model = flat(&["name", "content", "skillDir"]);
    const SUBAGENT: Model = flat(&["response", "turnsUsed", "completed"]);
    Some(match kind {
        "shell" => &SHELL,
        "file_edit" => &FILE_EDIT,
        "file_search" => &FILE_SEARCH,
        "file_read" => &FILE_READ,
        "todo" => &TODO,
        "file_write" => &FILE_WRITE,
        "user_question" => &USER_QUESTION,
        "web_search" => &WEB_SEARCH,
        "web_fetch" => &WEB_FETCH,
        "skill" => &SKILL,
        "subagent" => &SUBAGENT,
        _ => return None,
    })
}

/// Puts an entry's effect documents in their declared order: an effect's own
/// input and output, and the input of the effect an approval gates.
pub(super) fn arrange(entry: &mut Ordered) {
    let Some(entry_type) = entry.get("type").and_then(Ordered::as_str) else {
        return;
    };
    match entry_type {
        "effect" => {
            let kind = entry
                .get("detail")
                .and_then(|detail| detail.get("kind"))
                .and_then(Ordered::as_str)
                .map(str::to_owned);
            let Some(kind) = kind else {
                return;
            };
            if let (Some(input), Some(model)) = (
                entry
                    .get_mut("detail")
                    .and_then(|detail| detail.get_mut("input")),
                input_model(&kind),
            ) {
                apply(input, model);
            }
            if let (Some(output), Some(model)) = (
                entry
                    .get_mut("state")
                    .and_then(|state| state.get_mut("output")),
                output_model(&kind),
            ) {
                apply(output, model);
            }
        }
        "callback" => {
            let Some(effect) = entry
                .get_mut("detail")
                .and_then(|detail| detail.get_mut("effect"))
            else {
                return;
            };
            let kind = effect
                .get("kind")
                .and_then(Ordered::as_str)
                .map(str::to_owned);
            if let (Some(model), Some(input)) = (
                kind.as_deref().and_then(input_model),
                effect.get_mut("input"),
            ) {
                apply(input, model);
            }
        }
        _ => {}
    }
}

fn apply(value: &mut Ordered, model: &Model) {
    value.order_by(model.keys);
    for (field, item_model) in model.lists {
        if let Some(Ordered::Array(items)) = value.get_mut(field) {
            for item in items {
                apply(item, item_model);
            }
        }
    }
}
