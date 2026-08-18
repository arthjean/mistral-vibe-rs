//! The terminal palette, expressed on the theme it is derived from.
//!
//! Every role resolves through [`ResolvedTheme::colored`], so `NO_COLOR` and a
//! terminal that declares no color support are honored in exactly one place
//! rather than at each call site.

use ratatui::style::{Color, Modifier, Style};

use crate::tui::setup::{ResolvedTheme, Theme};

const MISTRAL_ORANGE: Color = Color::Rgb(255, 130, 5);

impl ResolvedTheme {
    /// The foreground the transcript body is written in.
    pub(super) fn base(self) -> Style {
        if !self.colors_enabled {
            return Style::default();
        }
        match self.theme {
            Theme::Light => Style::default().fg(Color::Black),
            Theme::Dark | Theme::System => Style::default().fg(Color::White),
        }
    }

    pub(super) fn assistant(self) -> Style {
        self.colored(Color::Green).add_modifier(Modifier::BOLD)
    }

    pub(super) fn effect(self) -> Style {
        self.colored(Color::Magenta).add_modifier(Modifier::BOLD)
    }

    pub(super) fn warning(self) -> Style {
        self.colored(Color::Yellow).add_modifier(Modifier::BOLD)
    }

    pub(super) fn success(self) -> Style {
        self.colored(Color::Green)
    }

    pub(super) fn error(self) -> Style {
        self.colored(Color::Red)
    }

    pub(super) fn muted(self) -> Style {
        self.colored(Color::DarkGray)
    }

    pub(super) fn orange(self) -> Style {
        self.colored(MISTRAL_ORANGE).add_modifier(Modifier::BOLD)
    }

    pub(super) fn secondary(self) -> Style {
        self.colored(Color::Cyan)
    }

    /// `color` when the terminal takes color, and the terminal's own default
    /// otherwise.
    pub(super) fn colored(self, color: Color) -> Style {
        if self.colors_enabled {
            Style::default().fg(color)
        } else {
            Style::default()
        }
    }
}
