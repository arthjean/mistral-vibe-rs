//! The rewind panel: which earlier message to go back to, what to do with the
//! files, and whether to stay in the session or fork it.
//!
//! Reference `RewindApp` (`vibe/cli/textual_ui/widgets/rewind_app.py`): a
//! two-step flow, the edit action first and the persistence choice second,
//! with `Esc` stepping back from the second to the first.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindTarget {
    /// The stable history identity the server resolves the rewind against.
    pub entry_id: String,
    pub message: String,
    pub has_file_changes: bool,
}

/// One option the panel offers. The first two belong to the action step, the
/// last two to the persistence step; the names match the reference's choices
/// so a trace reads the same on both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewindChoice {
    EditAndRestore,
    EditOnly,
    InPlace,
    Fork,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RewindStep {
    Action,
    Persistence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindState {
    targets: Vec<RewindTarget>,
    target: usize,
    step: RewindStep,
    restore_files: bool,
    option: usize,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewindEffect {
    None,
    Cancel,
    Scroll(isize),
    Accept {
        entry_id: String,
        restore_files: bool,
        inplace: bool,
    },
}

impl RewindState {
    #[must_use]
    pub fn new(targets: Vec<RewindTarget>) -> Option<Self> {
        let target = targets.len().checked_sub(1)?;
        Some(Self {
            targets,
            target,
            step: RewindStep::Action,
            restore_files: false,
            option: 0,
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

    /// Whether the panel is asking where the rewind should land rather than
    /// what it should do to the files.
    #[must_use]
    pub fn choosing_persistence(&self) -> bool {
        self.step == RewindStep::Persistence
    }

    #[must_use]
    pub fn actions(&self) -> &'static [RewindChoice] {
        match self.step {
            RewindStep::Persistence => &[RewindChoice::InPlace, RewindChoice::Fork],
            RewindStep::Action if self.target().has_file_changes => {
                &[RewindChoice::EditAndRestore, RewindChoice::EditOnly]
            }
            RewindStep::Action => &[RewindChoice::EditOnly],
        }
    }

    #[must_use]
    pub fn selected_action(&self) -> RewindChoice {
        self.actions()[self.option.min(self.actions().len().saturating_sub(1))]
    }

    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn set_error(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
    }

    /// Records whether the selected point would change files.
    ///
    /// The panel shows one point at a time, so this is asked per selection
    /// rather than for the whole list: the answer costs a read of the stored
    /// session and of every path a restore to that point would touch.
    pub fn set_target_file_changes(&mut self, has_file_changes: bool) {
        if let Some(target) = self.targets.get_mut(self.target) {
            target.has_file_changes = has_file_changes;
        }
    }

    /// Selects the point at `index`, starting over from the action step, which
    /// is what the reference does whenever the highlighted message changes.
    pub fn select_target(&mut self, index: usize) {
        self.target = index.min(self.targets.len().saturating_sub(1));
        self.reset_to_action_step();
    }

    fn move_target(&mut self, delta: isize) {
        self.select_target(self.target.saturating_add_signed(delta));
    }

    fn reset_to_action_step(&mut self) {
        self.step = RewindStep::Action;
        self.restore_files = false;
        self.option = 0;
        self.error = None;
    }

    fn move_option(&mut self, delta: isize) {
        let count = self.actions().len();
        self.option = if delta.is_negative() {
            self.option
                .checked_sub(1)
                .unwrap_or(count.saturating_sub(1))
        } else {
            (self.option + 1) % count
        };
        self.error = None;
    }

    /// Reference `RewindApp._handle_selection`: an action advances to the
    /// persistence step, and a persistence choice confirms.
    fn choose(&mut self, option: usize) -> RewindEffect {
        let Some(choice) = self.actions().get(option).copied() else {
            return RewindEffect::None;
        };
        self.option = option;
        self.error = None;
        let inplace = match choice {
            RewindChoice::EditAndRestore | RewindChoice::EditOnly => {
                self.restore_files = choice == RewindChoice::EditAndRestore;
                self.step = RewindStep::Persistence;
                self.option = 0;
                return RewindEffect::None;
            }
            RewindChoice::InPlace => true,
            RewindChoice::Fork => false,
        };
        RewindEffect::Accept {
            entry_id: self.target().entry_id.clone(),
            restore_files: self.restore_files,
            inplace,
        }
    }
}

pub fn reduce_key(state: &mut RewindState, key: KeyEvent) -> RewindEffect {
    match key.code {
        KeyCode::Char('q') if key.modifiers.is_empty() => RewindEffect::Cancel,
        // Reference `_handle_rewind_app_escape`: the persistence step goes
        // back to the action step, and otherwise `Esc` is `←`.
        KeyCode::Esc if key.modifiers.is_empty() && state.choosing_persistence() => {
            state.reset_to_action_step();
            RewindEffect::None
        }
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
            state.move_option(-1);
            RewindEffect::None
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
            state.move_option(1);
            RewindEffect::None
        }
        KeyCode::Char(value @ '1'..='2') if key.modifiers.is_empty() => {
            state.choose(usize::from(value as u8 - b'1'))
        }
        KeyCode::Enter if key.modifiers.is_empty() => {
            let option = state.option.min(state.actions().len().saturating_sub(1));
            state.choose(option)
        }
        _ => RewindEffect::None,
    }
}
