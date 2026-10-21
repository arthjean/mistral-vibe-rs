//! `ui.ask_user_question`: the questions a call puts to the user, and their
//! answers.

use serde_json::{Value, json};

use super::{Projected, call_display, completed_state, detail, result_display, structured};

/// Reference `UserQuestionRequest` with its defaults, or `None` when the
/// arguments do not validate.
fn question_request(input: &Value) -> Option<Value> {
    let questions = input
        .get("questions")?
        .as_array()
        .filter(|q| !q.is_empty())?;
    let mut projected = Vec::new();
    for question in questions {
        let options = question
            .get("options")?
            .as_array()
            .filter(|o| o.len() >= 2)?;
        let header = question.get("header").map_or(Some(""), Value::as_str)?;
        if header.chars().count() > 20 {
            return None;
        }
        let mut choices = Vec::new();
        for option in options {
            choices.push(json!({
                "label": option.get("label")?.as_str()?,
                "description": option.get("description").map_or(Some(""), Value::as_str)?,
            }));
        }
        projected.push(json!({
            "question": question.get("question")?.as_str()?,
            "header": header,
            "options": choices,
            "multiSelect": question.get("multiSelect").map_or(Some(false), Value::as_bool)?,
            "hideOther": question.get("hideOther").map_or(Some(false), Value::as_bool)?,
        }));
    }
    let footer = match input.get("footerNote") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(note)) => json!(note),
        Some(_) => return None,
    };
    Some(json!({"questions": projected, "footerNote": footer}))
}

pub(super) fn project_question(name: &str, input: &Value) -> Option<Projected> {
    let request = question_request(input)?;
    let questions = request["questions"].as_array()?;
    let text = if questions.len() == 1 {
        questions[0]["question"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    } else {
        format!("{} questions", questions.len())
    };
    let detail = detail(
        "user_question",
        name,
        request,
        call_display(
            format!("Asking: {text}"),
            "Asking",
            text.as_str(),
            "Asked",
            text.as_str(),
            "Waiting for user input",
        ),
    );
    Some(Projected {
        detail,
        result: Some(Box::new(|state| {
            let result = structured(state)?;
            let cancelled = result
                .get("cancelled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut answers = Vec::new();
            for answer in result.get("answers")?.as_array()? {
                answers.push(json!({
                    "question": answer.get("question")?.as_str()?,
                    "answer": answer.get("answer")?.as_str()?,
                    "isOther": answer.get("isOther").map_or(Some(false), Value::as_bool)?,
                }));
            }
            let display = if cancelled {
                result_display(false, "Cancelled", "by user")
            } else {
                let parts: Vec<String> = answers
                    .iter()
                    .map(|answer| {
                        format!(
                            "\"{}\" → {}{}",
                            answer["question"].as_str().unwrap_or_default(),
                            if answer["isOther"] == true {
                                "(Other) "
                            } else {
                                ""
                            },
                            answer["answer"].as_str().unwrap_or_default()
                        )
                    })
                    .collect();
                result_display(true, "Answered", parts.join(" · "))
            };
            let output = json!({"answers": answers, "cancelled": cancelled});
            Some(completed_state(state, output, display, None))
        })),
    })
}
