use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindTarget {
    pub message_index: usize,
    pub message: String,
    pub has_file_changes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewindAction {
    RestoreAndEdit,
    EditOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindState {
    targets: Vec<RewindTarget>,
    target: usize,
    action: usize,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewindEffect {
    None,
    Cancel,
    Scroll(isize),
    Accept {
        message_index: usize,
        restore_files: bool,
    },
}

impl RewindState {
    #[must_use]
    pub fn new(targets: Vec<RewindTarget>) -> Option<Self> {
        let target = targets.len().checked_sub(1)?;
        Some(Self {
            targets,
            target,
            action: 0,
            error: None,
        })
    }

    #[must_use]
    pub fn target(&self) -> &RewindTarget {
        &self.targets[self.target]
    }

    #[must_use]
    pub fn target_position(&self) -> (usize, usize) {
        (self.target.saturating_add(1), self.targets.len())
    }

    #[must_use]
    pub fn actions(&self) -> &'static [RewindAction] {
        if self.target().has_file_changes {
            &[RewindAction::RestoreAndEdit, RewindAction::EditOnly]
        } else {
            &[RewindAction::EditOnly]
        }
    }

    #[must_use]
    pub fn selected_action(&self) -> RewindAction {
        self.actions()[self.action.min(self.actions().len().saturating_sub(1))]
    }

    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn set_error(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
    }

    fn move_target(&mut self, delta: isize) {
        self.target = self
            .target
            .saturating_add_signed(delta)
            .min(self.targets.len().saturating_sub(1));
        self.action = 0;
        self.error = None;
    }

    fn move_action(&mut self, delta: isize) {
        let count = self.actions().len();
        self.action = if delta.is_negative() {
            self.action
                .checked_sub(1)
                .unwrap_or(count.saturating_sub(1))
        } else {
            (self.action + 1) % count
        };
        self.error = None;
    }

    fn select_action(&mut self, action: usize) -> bool {
        if action < self.actions().len() {
            self.action = action;
            self.error = None;
            true
        } else {
            false
        }
    }

    fn selection(&self) -> RewindEffect {
        RewindEffect::Accept {
            message_index: self.target().message_index,
            restore_files: self.selected_action() == RewindAction::RestoreAndEdit,
        }
    }
}

pub fn reduce_key(state: &mut RewindState, key: KeyEvent) -> RewindEffect {
    match key.code {
        KeyCode::Char('q') if key.modifiers.is_empty() => RewindEffect::Cancel,
        KeyCode::Esc | KeyCode::Left if key.modifiers.is_empty() => {
            state.move_target(-1);
            RewindEffect::None
        }
        KeyCode::Right if key.modifiers.is_empty() => {
            state.move_target(1);
            RewindEffect::None
        }
        KeyCode::Up if key.modifiers == KeyModifiers::SHIFT => RewindEffect::Scroll(-5),
        KeyCode::Down if key.modifiers == KeyModifiers::SHIFT => RewindEffect::Scroll(5),
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
            state.move_action(-1);
            RewindEffect::None
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
            state.move_action(1);
            RewindEffect::None
        }
        KeyCode::Char(value @ '1'..='2') if key.modifiers.is_empty() => {
            let action = usize::from(value as u8 - b'1');
            if state.select_action(action) {
                state.selection()
            } else {
                RewindEffect::None
            }
        }
        KeyCode::Enter if key.modifiers.is_empty() => state.selection(),
        _ => RewindEffect::None,
    }
}
