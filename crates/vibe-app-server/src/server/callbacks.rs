//! Callback request and answer validation, and the settlement of callback
//! entries inside a session's projected history.
//!
//! The app-server is the only place that decides whether a client answer is a
//! legitimate reply to the request it was offered, so the checks live together
//! rather than next to the request routing.

use super::*;

/// A validated callback request, split into what crosses the wire and what does
/// not.
///
/// The reference publishes a plan review as its own notice entry rather than as
/// a field on the callback, so the marker the port's plan tool sets is read here
/// and never reaches `CallbackDetail`, which forbids a surplus field.
pub(super) struct CallbackRequestDetail {
    pub(super) detail: CallbackDetail,
    pub(super) plan_review_path: Option<String>,
}

/// The wire keys a callback detail may carry alongside the reference ones. They
/// are port-internal: the dispatcher sets them, this parse consumes them.
const LOCAL_CALLBACK_KEYS: [&str; 2] = ["planReview", "filePath"];

/// Validates a callback request and decodes it into the reference union.
///
/// Decoding is the second half of the check: a detail that passes the bounds
/// below but does not deserialize is a shape a conforming client would reject,
/// so it is refused here rather than published.
pub(super) fn parse_callback_request(
    kind: EngineCallbackKind,
    title: &str,
    detail: &Value,
) -> Result<CallbackRequestDetail, &'static str> {
    validate_callback_request(kind, title, detail)?;
    let plan_review = detail
        .get("planReview")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let plan_review_path = plan_review
        .then(|| {
            detail
                .get("filePath")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .ok_or("Plan-review callback omitted its file path")
        })
        .transpose()?;
    let mut published = detail.clone();
    if let Some(object) = published.as_object_mut() {
        for key in LOCAL_CALLBACK_KEYS {
            object.remove(key);
        }
    }
    let detail = serde_json::from_value::<CallbackDetail>(published)
        .map_err(|_| "Callback detail does not match the protocol union")?;
    Ok(CallbackRequestDetail {
        detail,
        plan_review_path,
    })
}

pub(super) fn validate_callback_output(output: &Value) -> Result<EngineCallbackKind, &'static str> {
    if serde_json::to_vec(output).map_or(true, |encoded| encoded.len() > MAX_CALLBACK_OUTPUT_BYTES)
    {
        return Err("Callback output exceeds the size limit");
    }
    let Some(object) = output.as_object() else {
        return Err("Callback output must be an object");
    };
    match output.get("type").and_then(Value::as_str) {
        Some("approval") => {
            // The reference approval output declares an optional operator note
            // alongside the decision, so a client that sends one is answered
            // rather than rejected.
            if !object.contains_key("decision")
                || object
                    .keys()
                    .any(|key| !matches!(key.as_str(), "type" | "decision" | "feedback"))
            {
                return Err("Approval output has unexpected fields");
            }
            if let Some(feedback) = object.get("feedback")
                && !feedback.is_null()
                && !feedback
                    .as_str()
                    .is_some_and(|value| value.len() <= MAX_CALLBACK_TEXT_BYTES)
            {
                return Err("Approval feedback is invalid");
            }
            let Some(decision) = output.get("decision").and_then(Value::as_object) else {
                return Err("Approval decision must be an object");
            };
            if decision.len() != 1
                || !matches!(
                    decision.get("type").and_then(Value::as_str),
                    Some(
                        "approve"
                            | "approve_for_session"
                            | "approve_permanently"
                            | "deny"
                            | "cancel_turn"
                    )
                )
            {
                return Err("Approval decision is unsupported");
            }
            Ok(EngineCallbackKind::Approval)
        }
        Some("user_input") => {
            if object.len() != 2 || !object.contains_key("result") {
                return Err("User-input output has unexpected fields");
            }
            let Some(result) = output.get("result").and_then(Value::as_object) else {
                return Err("User-input result must be an object");
            };
            if result.len() != 2 {
                return Err("User-input result has unexpected fields");
            }
            let Some(cancelled) = result.get("cancelled").and_then(Value::as_bool) else {
                return Err("User-input result requires a boolean cancelled field");
            };
            let Some(answers) = result.get("answers").and_then(Value::as_array) else {
                return Err("User-input result requires an answers array");
            };
            if answers.len() > MAX_CALLBACK_ANSWERS
                || (cancelled && !answers.is_empty())
                || (!cancelled && answers.is_empty())
            {
                return Err("User-input answer count is invalid");
            }
            for answer in answers {
                let Some(answer) = answer.as_object() else {
                    return Err("User-input answers must be objects");
                };
                if answer.len() != 3 {
                    return Err("User-input answer has unexpected fields");
                }
                let valid_question =
                    answer
                        .get("question")
                        .and_then(Value::as_str)
                        .is_some_and(|value| {
                            !value.is_empty() && value.len() <= MAX_CALLBACK_TEXT_BYTES
                        });
                let valid_answer = answer
                    .get("answer")
                    .and_then(Value::as_str)
                    .is_some_and(|value| value.len() <= MAX_CALLBACK_TEXT_BYTES);
                let valid_other = answer.get("isOther").is_some_and(Value::is_boolean);
                if !valid_question || !valid_answer || !valid_other {
                    return Err("User-input answer shape is invalid");
                }
            }
            Ok(EngineCallbackKind::UserInput)
        }
        _ => Err("Callback output kind is unsupported"),
    }
}

pub(super) fn validate_callback_request(
    kind: EngineCallbackKind,
    title: &str,
    detail: &Value,
) -> Result<(), &'static str> {
    if title.trim().is_empty() || title.len() > MAX_CALLBACK_TEXT_BYTES {
        return Err("Callback title is empty or exceeds the size limit");
    }
    if serde_json::to_vec(detail).map_or(true, |encoded| encoded.len() > MAX_CALLBACK_REQUEST_BYTES)
    {
        return Err("Callback detail exceeds the size limit");
    }
    let Some(object) = detail.as_object() else {
        return Err("Callback detail must be an object");
    };
    if let Some(related_entry_id) = object.get("relatedEntryId")
        && !related_entry_id.is_null()
        && !related_entry_id
            .as_str()
            .is_some_and(|value| !value.trim().is_empty() && value.len() <= MAX_CALLBACK_TEXT_BYTES)
    {
        return Err("Callback related entry ID is invalid");
    }
    match kind {
        EngineCallbackKind::Approval => {
            if detail.get("kind").and_then(Value::as_str) != Some("approval") {
                return Err("Approval callback detail has the wrong kind");
            }
            if !detail.get("effect").is_some_and(Value::is_object) {
                return Err("Approval callback detail requires an effect");
            }
            let Some(choices) = detail.get("choices").and_then(Value::as_array) else {
                return Err("Approval callback detail requires choices");
            };
            if choices.is_empty() || choices.len() > MAX_CALLBACK_OPTIONS {
                return Err("Approval callback choice count is invalid");
            }
            let mut seen = BTreeSet::new();
            for choice in choices {
                let Some(choice) = choice.as_str() else {
                    return Err("Approval callback choices must be strings");
                };
                if !matches!(
                    choice,
                    "approve"
                        | "approve_for_session"
                        | "approve_permanently"
                        | "deny"
                        | "cancel_turn"
                ) || !seen.insert(choice)
                {
                    return Err("Approval callback choice is unsupported or duplicated");
                }
            }
            if let Some(permissions) = detail.get("requiredPermissions") {
                let Some(permissions) = permissions.as_array() else {
                    return Err("Approval callback permissions must be an array");
                };
                if permissions.len() > MAX_CALLBACK_OPTIONS
                    || permissions.iter().any(|permission| {
                        !permission.as_str().is_some_and(|value| {
                            !value.trim().is_empty() && value.len() <= MAX_CALLBACK_TEXT_BYTES
                        })
                    })
                {
                    return Err("Approval callback permission is invalid");
                }
            }
            Ok(())
        }
        EngineCallbackKind::UserInput => validate_user_input_request(detail),
        EngineCallbackKind::ConnectorAuth => Err("Connector-auth callbacks are unsupported"),
    }
}

pub(super) fn validate_user_input_request(detail: &Value) -> Result<(), &'static str> {
    if detail.get("kind").and_then(Value::as_str) != Some("user_input") {
        return Err("User-input callback detail has the wrong kind");
    }
    if detail
        .get("planReview")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err("User-input plan-review marker must be a boolean");
    }
    let Some(request) = detail.get("request").and_then(Value::as_object) else {
        return Err("User-input callback detail requires a request");
    };
    if let Some(footer) = request.get("footerNote")
        && !footer.is_null()
        && !footer
            .as_str()
            .is_some_and(|value| value.len() <= MAX_CALLBACK_TEXT_BYTES)
    {
        return Err("User-input callback footer is invalid");
    }
    let Some(questions) = request.get("questions").and_then(Value::as_array) else {
        return Err("User-input callback requires questions");
    };
    if questions.is_empty() || questions.len() > MAX_CALLBACK_ANSWERS {
        return Err("User-input callback question count is invalid");
    }
    for question in questions {
        let Some(question) = question.as_object() else {
            return Err("User-input callback questions must be objects");
        };
        if !question
            .get("question")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty() && value.len() <= MAX_CALLBACK_TEXT_BYTES)
        {
            return Err("User-input callback question text is invalid");
        }
        if question.get("header").is_some_and(|value| {
            !value
                .as_str()
                .is_some_and(|value| value.len() <= MAX_CALLBACK_TEXT_BYTES)
        }) {
            return Err("User-input callback question header is invalid");
        }
        for field in ["multiSelect", "hideOther"] {
            if question.get(field).is_some_and(|value| !value.is_boolean()) {
                return Err("User-input callback question flags must be booleans");
            }
        }
        let Some(options) = question.get("options").and_then(Value::as_array) else {
            return Err("User-input callback question requires options");
        };
        if options.len() < 2 || options.len() > MAX_CALLBACK_OPTIONS {
            return Err("User-input callback option count is invalid");
        }
        let mut labels = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for option in options {
            let Some(option) = option.as_object() else {
                return Err("User-input callback options must be objects");
            };
            let Some(label) = option.get("label").and_then(Value::as_str) else {
                return Err("User-input callback option label is invalid");
            };
            if label.trim().is_empty()
                || label.len() > MAX_CALLBACK_TEXT_BYTES
                || !labels.insert(label)
            {
                return Err("User-input callback option label is invalid or duplicated");
            }
            if option.get("description").is_some_and(|value| {
                !value
                    .as_str()
                    .is_some_and(|value| value.len() <= MAX_CALLBACK_TEXT_BYTES)
            }) {
                return Err("User-input callback option description is invalid");
            }
            if let Some(id) = option.get("id") {
                let Some(id) = id.as_str() else {
                    return Err("User-input callback option ID is invalid");
                };
                if id.trim().is_empty() || id.len() > MAX_CALLBACK_TEXT_BYTES || !ids.insert(id) {
                    return Err("User-input callback option ID is invalid or duplicated");
                }
            }
        }
    }
    Ok(())
}

pub(super) fn callback_entry_detail(entry: &PublicHistoryEntry) -> Option<&CallbackDetail> {
    match entry {
        PublicHistoryEntry::Callback { detail, .. } => Some(detail),
        _ => None,
    }
}

pub(super) fn validate_callback_output_against_request(
    output: &Value,
    callback: &PendingCallback,
) -> Result<EngineCallbackKind, &'static str> {
    let kind = validate_callback_output(output)?;
    if kind != callback.kind {
        return Err("Callback kind does not match");
    }
    match kind {
        EngineCallbackKind::Approval => {
            let decision = output
                .pointer("/decision/type")
                .and_then(Value::as_str)
                .ok_or("Approval output omitted its decision")?;
            let offered = match callback_entry_detail(&callback.entry) {
                Some(CallbackDetail::Approval { choices, .. }) => choices.iter().any(|choice| {
                    serde_json::to_value(choice)
                        .ok()
                        .as_ref()
                        .and_then(Value::as_str)
                        == Some(decision)
                }),
                _ => false,
            };
            if !offered {
                return Err("Approval decision was not offered");
            }
        }
        EngineCallbackKind::UserInput => {
            validate_user_input_output_against_request(output, &callback.entry)?;
        }
        EngineCallbackKind::ConnectorAuth => {
            return Err("Connector-auth callbacks are unsupported");
        }
    }
    Ok(kind)
}

pub(super) fn validate_user_input_output_against_request(
    output: &Value,
    entry: &PublicHistoryEntry,
) -> Result<(), &'static str> {
    let Some(CallbackDetail::UserInput { request, .. }) = callback_entry_detail(entry) else {
        return Err("User-input callback request omitted questions");
    };
    let questions = &request.questions;
    let answers = output
        .pointer("/result/answers")
        .and_then(Value::as_array)
        .ok_or("User-input output omitted answers")?;
    let cancelled = output
        .pointer("/result/cancelled")
        .and_then(Value::as_bool)
        .ok_or("User-input output omitted cancelled")?;
    if cancelled {
        return if answers.is_empty() {
            Ok(())
        } else {
            Err("Cancelled user-input output must not include answers")
        };
    }
    if answers.len() != questions.len() {
        return Err("User-input answer count does not match the request");
    }
    for (answer, question) in answers.iter().zip(questions) {
        if answer.get("question").and_then(Value::as_str) != Some(question.question.as_str()) {
            return Err("User-input answer does not match its question");
        }
        let answer_text = answer
            .get("answer")
            .and_then(Value::as_str)
            .ok_or("User-input answer text is invalid")?;
        if answer_text.trim().is_empty() {
            return Err("User-input answer text is empty");
        }
        let is_other = answer
            .get("isOther")
            .and_then(Value::as_bool)
            .ok_or("User-input answer omitted isOther")?;
        if is_other {
            if question.hide_other {
                return Err("User-input question does not allow free text");
            }
            continue;
        }
        let selected = if question.multi_select {
            answer_text.split(", ").collect::<Vec<_>>()
        } else {
            vec![answer_text]
        };
        if selected.is_empty()
            || selected
                .iter()
                .enumerate()
                .any(|(index, label)| selected[..index].contains(label))
            || selected
                .iter()
                .any(|label| !question.options.iter().any(|option| option.label == *label))
        {
            return Err("User-input answer selected an invalid option");
        }
    }
    Ok(())
}

pub(super) fn callback_requests_turn_cancel(output: &Value) -> bool {
    output.pointer("/decision/type").and_then(Value::as_str) == Some("cancel_turn")
}

pub(super) fn settle_pending_callback(
    session: &mut SessionRuntime,
    callback_id: &str,
    terminal_state: PublicCallbackState,
) {
    let timestamp = now_millis();
    if let Some(PendingCallback { entry, .. }) = session
        .pending_callback
        .as_mut()
        .filter(|callback| callback.id == callback_id)
    {
        settle_callback_entry(entry, terminal_state.clone(), timestamp);
    }
    if let Some(snapshot) = session.snapshot.as_mut()
        && let Some(entry) = snapshot.history.iter_mut().rev().find(|entry| {
            matches!(
                entry,
                PublicHistoryEntry::Callback {
                    callback_id: existing,
                    ..
                } if existing == callback_id
            )
        })
    {
        settle_callback_entry(entry, terminal_state, timestamp);
    }
}

pub(super) fn cancel_pending_callback(session: &mut SessionRuntime, reason: &str) -> bool {
    let Some(callback_id) = session
        .pending_callback
        .as_ref()
        .map(|callback| callback.id.clone())
    else {
        return false;
    };
    settle_pending_callback(
        session,
        &callback_id,
        PublicCallbackState::Cancelled {
            reason: reason.to_owned(),
        },
    );
    session.pending_callback = None;
    true
}

pub(super) fn settle_callback_entry(
    entry: &mut PublicHistoryEntry,
    terminal_state: PublicCallbackState,
    timestamp: u64,
) {
    if let PublicHistoryEntry::Callback {
        metadata, state, ..
    } = entry
    {
        metadata.updated_at = timestamp;
        metadata.generation_status = PublicEntryGenerationStatus::Completed;
        *state = terminal_state;
    }
}

pub(super) fn merge_server_callback_history(
    existing: Option<&ProjectionSnapshot>,
    mut incoming: ProjectionSnapshot,
) -> ProjectionSnapshot {
    let Some(existing) = existing else {
        return incoming;
    };
    let target_session_id = incoming.session_id.clone();
    let mut merged = existing.history.clone();
    for entry in &mut merged {
        entry.rebind_session(&target_session_id);
    }
    for mut entry in std::mem::take(&mut incoming.history) {
        entry.rebind_session(&target_session_id);
        if let Some(position) = merged
            .iter()
            .position(|current| current.metadata().id == entry.metadata().id)
        {
            let existing_callback_is_terminal = matches!(
                &merged[position],
                PublicHistoryEntry::Callback {
                    state: PublicCallbackState::Answered { .. }
                        | PublicCallbackState::Cancelled { .. }
                        | PublicCallbackState::Expired { .. },
                    ..
                }
            );
            if !existing_callback_is_terminal {
                merged[position] = entry;
            }
        } else {
            merged.push(entry);
        }
    }
    merged.sort_by_key(|entry| entry.metadata().created_at);
    incoming.history = merged;
    if incoming.title.is_none() {
        incoming.title.clone_from(&existing.title);
    }
    incoming
}
