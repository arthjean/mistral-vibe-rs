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
}
