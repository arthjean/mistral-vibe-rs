use serde_json::Value;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) enum CloudWorkflowState {
    #[default]
    Idle,
    ConfiguringProject {
        picker_id: String,
    },
    SelectingTeleportProject {
        picker_id: String,
        prompt: Option<String>,
    },
    Teleporting {
        operation_id: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ProjectSelection {
    Configured,
    StartTeleport {
        picker_id: String,
        prompt: Option<String>,
    },
}

impl CloudWorkflowState {
    pub(super) fn ensure_idle(&self) -> Result<(), &'static str> {
        match self {
            Self::Idle => Ok(()),
            Self::ConfiguringProject { .. } | Self::SelectingTeleportProject { .. } => {
                Err("Finish or cancel the active remote project selection first")
            }
            Self::Teleporting { .. } => Err("A Teleport operation is already active"),
        }
    }

    pub(super) fn picker_id(&self) -> Option<&str> {
        match self {
            Self::ConfiguringProject { picker_id }
            | Self::SelectingTeleportProject { picker_id, .. } => Some(picker_id),
            Self::Idle | Self::Teleporting { .. } => None,
        }
    }

    pub(super) fn teleport_operation_id(&self) -> Option<&str> {
        match self {
            Self::Teleporting { operation_id } => Some(operation_id),
            Self::Idle
            | Self::ConfiguringProject { .. }
            | Self::SelectingTeleportProject { .. } => None,
        }
    }

    pub(super) fn configure_project(&mut self, picker_id: String) -> Result<(), &'static str> {
        self.ensure_idle()?;
        *self = Self::ConfiguringProject { picker_id };
        Ok(())
    }

    pub(super) fn select_teleport_project(
        &mut self,
        picker_id: String,
        prompt: Option<String>,
    ) -> Result<(), &'static str> {
        self.ensure_idle()?;
        *self = Self::SelectingTeleportProject { picker_id, prompt };
        Ok(())
    }

    pub(super) fn complete_project_selection(&mut self) -> Option<ProjectSelection> {
        match std::mem::take(self) {
            Self::ConfiguringProject { .. } => Some(ProjectSelection::Configured),
            Self::SelectingTeleportProject { picker_id, prompt } => {
                Some(ProjectSelection::StartTeleport { picker_id, prompt })
            }
            current @ (Self::Idle | Self::Teleporting { .. }) => {
                *self = current;
                None
            }
        }
    }

    pub(super) fn cancel_project_selection(&mut self) {
        if matches!(
            self,
            Self::ConfiguringProject { .. } | Self::SelectingTeleportProject { .. }
        ) {
            *self = Self::Idle;
        }
    }

    pub(super) fn start_teleport(&mut self, operation_id: String) -> Result<(), &'static str> {
        self.ensure_idle()?;
        *self = Self::Teleporting { operation_id };
        Ok(())
    }

    pub(super) fn complete_teleport(&mut self) {
        if matches!(self, Self::Teleporting { .. }) {
            *self = Self::Idle;
        }
    }
}

pub(super) fn format_loop_list(loops: &Value, now_seconds: u64) -> Result<String, &'static str> {
    let loops = loops.as_array().ok_or("Scheduled-loop list is malformed")?;
    if loops.is_empty() {
        return Ok("No scheduled loops.".to_owned());
    }
    let mut rows = vec![
        "| Prompt | Next in | Every | ID |".to_owned(),
        "|--------|------|-------|----|".to_owned(),
    ];
    for scheduled in loops {
        let id = scheduled
            .get("id")
            .and_then(Value::as_str)
            .ok_or("Scheduled loop omitted its ID")?;
        let prompt = scheduled
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or("Scheduled loop omitted its prompt")?
            .replace('|', "\\|")
            .replace('\n', " ");
        let interval = scheduled
            .get("intervalSeconds")
            .and_then(Value::as_u64)
            .ok_or("Scheduled loop omitted its interval")?;
        let next = scheduled
            .get("nextFireAt")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(|value| value as u64)
            .ok_or("Scheduled loop omitted its next run")?;
        rows.push(format!(
            "| {prompt} | {} | {} | `{id}` |",
            format_duration(next.saturating_sub(now_seconds), true),
            format_duration(interval, false),
        ));
    }
    Ok(rows.join("\n"))
}

pub(super) fn format_created_loop(scheduled: &Value) -> Result<String, &'static str> {
    let id = scheduled
        .get("id")
        .and_then(Value::as_str)
        .ok_or("Scheduled loop omitted its ID")?;
    let prompt = scheduled
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or("Scheduled loop omitted its prompt")?;
    let interval = scheduled
        .get("intervalSeconds")
        .and_then(Value::as_u64)
        .ok_or("Scheduled loop omitted its interval")?;
    Ok(format!(
        "Scheduled loop `{id}` every {}: {prompt}",
        format_duration(interval, false)
    ))
}

pub(super) fn format_cancelled_loop(scheduled: &Value) -> Result<String, &'static str> {
    let id = scheduled
        .get("id")
        .and_then(Value::as_str)
        .ok_or("Cancelled loop omitted its ID")?;
    let prompt = scheduled
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or("Cancelled loop omitted its prompt")?;
    Ok(format!("Cancelled loop `{id}`: {prompt}"))
}

fn format_duration(mut seconds: u64, short: bool) -> String {
    let mut parts = Vec::new();
    for (unit_seconds, suffix) in [(86_400, "d"), (3_600, "h"), (60, "m"), (1, "s")] {
        let value = seconds / unit_seconds;
        if value > 0 {
            parts.push(format!("{value}{suffix}"));
            seconds %= unit_seconds;
        }
    }
    if parts.is_empty() {
        parts.push("0s".to_owned());
    }
    if short {
        parts.remove(0)
    } else {
        parts.concat()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_cloud_ownership_cannot_be_overwritten() {
        for mut state in [
            CloudWorkflowState::ConfiguringProject {
                picker_id: "configure".to_owned(),
            },
            CloudWorkflowState::SelectingTeleportProject {
                picker_id: "teleport-picker".to_owned(),
                prompt: Some("deploy".to_owned()),
            },
            CloudWorkflowState::Teleporting {
                operation_id: "operation".to_owned(),
            },
        ] {
            let expected = format!("{state:?}");
            assert!(state.ensure_idle().is_err());
            assert!(state.configure_project("replacement".to_owned()).is_err());
            assert!(
                state
                    .select_teleport_project("replacement".to_owned(), None)
                    .is_err()
            );
            assert_eq!(format!("{state:?}"), expected);
        }
    }

    #[test]
    fn project_selection_and_teleport_have_one_owned_transition_path() {
        let mut state = CloudWorkflowState::Idle;
        state
            .configure_project("cancelled-picker".to_owned())
            .expect("configuration starts");
        state.cancel_project_selection();
        assert_eq!(state, CloudWorkflowState::Idle);
        state
            .select_teleport_project("picker".to_owned(), Some("deploy".to_owned()))
            .expect("idle project selection starts");
        assert_eq!(
            state.complete_project_selection(),
            Some(ProjectSelection::StartTeleport {
                picker_id: "picker".to_owned(),
                prompt: Some("deploy".to_owned()),
            })
        );
        assert_eq!(state, CloudWorkflowState::Idle);
        state
            .start_teleport("operation".to_owned())
            .expect("idle workflow starts");
        assert_eq!(state.teleport_operation_id(), Some("operation"));
        state.complete_teleport();
        assert_eq!(state, CloudWorkflowState::Idle);
    }

    #[test]
    fn scheduled_loops_use_reference_messages_and_table_shape() {
        let loops = serde_json::json!([
            {
                "id": "loop-1",
                "prompt": "check | deploy\nreport",
                "intervalSeconds": 3661,
                "nextFireAt": 130.0,
            }
        ]);
        assert_eq!(
            format_loop_list(&loops, 100).expect("loop table"),
            "| Prompt | Next in | Every | ID |\n|--------|------|-------|----|\n| check \\| deploy report | 30s | 1h1m1s | `loop-1` |"
        );
        assert_eq!(
            format_created_loop(&loops[0]).expect("created message"),
            "Scheduled loop `loop-1` every 1h1m1s: check | deploy\nreport"
        );
        assert_eq!(
            format_cancelled_loop(&loops[0]).expect("cancelled message"),
            "Cancelled loop `loop-1`: check | deploy\nreport"
        );
        assert_eq!(
            format_loop_list(&serde_json::json!([]), 100).expect("empty loops"),
            "No scheduled loops."
        );
    }
}
