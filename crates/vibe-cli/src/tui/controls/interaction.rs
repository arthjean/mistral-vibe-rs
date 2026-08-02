use std::collections::BTreeSet;

use super::{
    ApprovalScope, CallbackChoice, CallbackInput, CallbackInputOutcome, CallbackOption,
    CallbackQuestion, QuestionInteraction, UserInputChoice,
};

impl QuestionInteraction {
    pub(super) fn new(questions: &[CallbackQuestion]) -> Self {
        Self {
            current: 0,
            cursors: vec![0; questions.len()],
            selections: vec![BTreeSet::new(); questions.len()],
            other_text: vec![String::new(); questions.len()],
            answers: vec![None; questions.len()],
        }
    }
}

pub(super) fn approval_input(
    options: &[CallbackOption],
    selected: &mut usize,
    input: CallbackInput,
) -> CallbackInputOutcome {
    if options.is_empty() {
        return CallbackInputOutcome::Invalid("Approval has no available choice".to_owned());
    }
    match input {
        CallbackInput::Up => {
            *selected = selected.checked_sub(1).unwrap_or(options.len() - 1);
            CallbackInputOutcome::Updated
        }
        CallbackInput::Down => {
            *selected = selected.saturating_add(1) % options.len();
            CallbackInputOutcome::Updated
        }
        CallbackInput::Shortcut(index) => approval_choice(options, index)
            .map(CallbackInputOutcome::Submit)
            .unwrap_or(CallbackInputOutcome::Ignored),
        CallbackInput::Select => approval_choice(options, *selected)
            .map(CallbackInputOutcome::Submit)
            .unwrap_or_else(|| {
                CallbackInputOutcome::Invalid("Approval choice is unsupported".to_owned())
            }),
        CallbackInput::Cancel => CallbackInputOutcome::Submit(CallbackChoice::Deny {
            scope: ApprovalScope::Once,
        }),
        CallbackInput::PreviousQuestion
        | CallbackInput::NextQuestion
        | CallbackInput::Character(_)
        | CallbackInput::Backspace => CallbackInputOutcome::Ignored,
    }
}

fn approval_choice(options: &[CallbackOption], index: usize) -> Option<CallbackChoice> {
    match options.get(index)?.id.as_str() {
        "approve" => Some(CallbackChoice::Approve {
            scope: ApprovalScope::Once,
        }),
        "approve_for_session" => Some(CallbackChoice::Approve {
            scope: ApprovalScope::Session,
        }),
        "approve_permanently" => Some(CallbackChoice::Approve {
            scope: ApprovalScope::Permanent,
        }),
        "deny" => Some(CallbackChoice::Deny {
            scope: ApprovalScope::Once,
        }),
        "cancel_turn" => Some(CallbackChoice::Cancel),
        _ => None,
    }
}

pub(super) fn question_input(
    questions: &[CallbackQuestion],
    interaction: &mut QuestionInteraction,
    input: CallbackInput,
) -> CallbackInputOutcome {
    let Some(question) = questions.get(interaction.current) else {
        return CallbackInputOutcome::Invalid("Question callback is empty".to_owned());
    };
    let total = question.options.len()
        + usize::from(question.allows_free_text)
        + usize::from(question.multi_select);
    if total == 0 {
        return CallbackInputOutcome::Invalid("Question has no available answer".to_owned());
    }
    match input {
        CallbackInput::Up => {
            let cursor = &mut interaction.cursors[interaction.current];
            *cursor = cursor.checked_sub(1).unwrap_or(total - 1);
            CallbackInputOutcome::Updated
        }
        CallbackInput::Down => {
            let cursor = &mut interaction.cursors[interaction.current];
            *cursor = cursor.saturating_add(1) % total;
            CallbackInputOutcome::Updated
        }
        CallbackInput::PreviousQuestion => {
            interaction.current = interaction
                .current
                .checked_sub(1)
                .unwrap_or(questions.len().saturating_sub(1));
            CallbackInputOutcome::Updated
        }
        CallbackInput::NextQuestion => {
            let current = interaction.current;
            if questions[current].allows_free_text
                && interaction.cursors[current] == questions[current].options.len()
                && interaction.other_text[current].trim().is_empty()
            {
                return CallbackInputOutcome::Invalid(
                    "Other requires a non-empty answer".to_owned(),
                );
            }
            interaction.current = interaction.current.saturating_add(1) % questions.len();
            CallbackInputOutcome::Updated
        }
        CallbackInput::Shortcut(index) => {
            if index >= question.options.len() + usize::from(question.allows_free_text) {
                return CallbackInputOutcome::Ignored;
            }
            interaction.cursors[interaction.current] = index;
            if question.allows_free_text && index == question.options.len() {
                return CallbackInputOutcome::Updated;
            }
            select_question_answer(questions, interaction)
        }
        CallbackInput::Select => select_question_answer(questions, interaction),
        CallbackInput::Cancel => CallbackInputOutcome::Submit(CallbackChoice::Cancel),
        CallbackInput::Character(character) => {
            if !question.allows_free_text
                || interaction.cursors[interaction.current] != question.options.len()
                || character.is_control()
            {
                return CallbackInputOutcome::Ignored;
            }
            interaction.other_text[interaction.current].push(character);
            if question.multi_select {
                interaction.selections[interaction.current].insert(question.options.len());
            }
            CallbackInputOutcome::Updated
        }
        CallbackInput::Backspace => {
            if !question.allows_free_text
                || interaction.cursors[interaction.current] != question.options.len()
            {
                return CallbackInputOutcome::Ignored;
            }
            interaction.other_text[interaction.current].pop();
            if question.multi_select && interaction.other_text[interaction.current].is_empty() {
                interaction.selections[interaction.current].remove(&question.options.len());
            }
            CallbackInputOutcome::Updated
        }
    }
}

fn select_question_answer(
    questions: &[CallbackQuestion],
    interaction: &mut QuestionInteraction,
) -> CallbackInputOutcome {
    let current = interaction.current;
    let question = &questions[current];
    let cursor = interaction.cursors[current];
    if question.multi_select {
        let submit_index = question.options.len() + usize::from(question.allows_free_text);
        if cursor != submit_index {
            if cursor == question.options.len()
                && question.allows_free_text
                && interaction.other_text[current].trim().is_empty()
            {
                return CallbackInputOutcome::Invalid(
                    "Other requires a non-empty answer".to_owned(),
                );
            }
            let selections = &mut interaction.selections[current];
            if !selections.remove(&cursor) {
                selections.insert(cursor);
            }
            return CallbackInputOutcome::Updated;
        }
    }
    let answer = match selected_question_answer(question, interaction, current) {
        Ok(answer) => answer,
        Err(message) => return CallbackInputOutcome::Invalid(message),
    };
    interaction.answers[current] = Some(answer);
    if let Some(next) = (0..questions.len()).find(|index| interaction.answers[*index].is_none()) {
        interaction.current = next;
        return CallbackInputOutcome::Updated;
    }
    let answers = interaction
        .answers
        .iter()
        .cloned()
        .collect::<Option<Vec<_>>>();
    let Some(mut answers) = answers else {
        return CallbackInputOutcome::Invalid("Not every question has an answer".to_owned());
    };
    if answers.len() == 1 {
        let Some(answer) = answers.pop() else {
            return CallbackInputOutcome::Invalid("Question answer disappeared".to_owned());
        };
        return CallbackInputOutcome::Submit(match answer {
            UserInputChoice::Option { id } => CallbackChoice::Option { id },
            UserInputChoice::Options { ids } => CallbackChoice::Options { ids },
            UserInputChoice::Combined { ids, other } => CallbackChoice::UserInput {
                answers: vec![UserInputChoice::Combined { ids, other }],
            },
            UserInputChoice::FreeText { value } => CallbackChoice::FreeText { value },
        });
    }
    CallbackInputOutcome::Submit(CallbackChoice::UserInput { answers })
}

fn selected_question_answer(
    question: &CallbackQuestion,
    interaction: &QuestionInteraction,
    index: usize,
) -> Result<UserInputChoice, String> {
    if !question.multi_select {
        let cursor = interaction.cursors[index];
        if cursor < question.options.len() {
            return Ok(UserInputChoice::Option {
                id: question.options[cursor].id.clone(),
            });
        }
        let value = interaction.other_text[index].trim();
        return if question.allows_free_text && !value.is_empty() {
            Ok(UserInputChoice::FreeText {
                value: value.to_owned(),
            })
        } else {
            Err("Other requires a non-empty answer".to_owned())
        };
    }
    let selections = &interaction.selections[index];
    let ids = selections
        .iter()
        .filter_map(|selected| question.options.get(*selected))
        .map(|option| option.id.clone())
        .collect::<Vec<_>>();
    let other_selected = question.allows_free_text && selections.contains(&question.options.len());
    let other = interaction.other_text[index].trim();
    if ids.is_empty() && (!other_selected || other.is_empty()) {
        return Err("Select at least one answer".to_owned());
    }
    if other_selected && !other.is_empty() {
        return Ok(UserInputChoice::Combined {
            ids,
            other: other.to_owned(),
        });
    }
    Ok(UserInputChoice::Options { ids })
}

pub(super) fn render_question_lines(
    questions: &[CallbackQuestion],
    footer_note: Option<&str>,
    interaction: &QuestionInteraction,
    lines: &mut Vec<String>,
) -> usize {
    if questions.len() > 1 {
        lines.push(
            questions
                .iter()
                .enumerate()
                .map(|(index, question)| {
                    let header = if question.header.is_empty() {
                        format!("Q{}", index + 1)
                    } else {
                        question.header.clone()
                    };
                    let complete = if interaction.answers[index].is_some() {
                        " ✓"
                    } else {
                        ""
                    };
                    if index == interaction.current {
                        format!("[{header}{complete}]")
                    } else {
                        format!(" {header}{complete} ")
                    }
                })
                .collect::<Vec<_>>()
                .join("  "),
        );
    }
    let question = &questions[interaction.current];
    lines.push(question.question.clone());
    let focus_line = lines
        .len()
        .saturating_add(interaction.cursors[interaction.current]);
    for (index, option) in question.options.iter().enumerate() {
        let focused = interaction.cursors[interaction.current] == index;
        let checked =
            question.multi_select && interaction.selections[interaction.current].contains(&index);
        lines.push(format!(
            "{}{}. {}{}{}",
            if focused { "› " } else { "  " },
            index + 1,
            if question.multi_select {
                if checked { "[x] " } else { "[ ] " }
            } else {
                ""
            },
            option.label,
            if option.description.is_empty() {
                String::new()
            } else {
                format!(" - {}", option.description)
            }
        ));
    }
    if question.allows_free_text {
        let index = question.options.len();
        let focused = interaction.cursors[interaction.current] == index;
        let checked =
            question.multi_select && interaction.selections[interaction.current].contains(&index);
        lines.push(format!(
            "{}{}. {}Other: {}",
            if focused { "› " } else { "  " },
            index + 1,
            if question.multi_select {
                if checked { "[x] " } else { "[ ] " }
            } else {
                ""
            },
            interaction.other_text[interaction.current]
        ));
    }
    if question.multi_select {
        let submit = question.options.len() + usize::from(question.allows_free_text);
        lines.push(format!(
            "{}Submit →",
            if interaction.cursors[interaction.current] == submit {
                "› "
            } else {
                "  "
            }
        ));
    }
    if let Some(footer) = footer_note {
        lines.push(footer.to_owned());
    }
    lines.push(if questions.len() > 1 {
        "←→ questions  ↑↓/jk navigate  Enter select  Esc cancel".to_owned()
    } else {
        "↑↓/jk navigate  Enter select  Esc cancel".to_owned()
    });
    focus_line
}
