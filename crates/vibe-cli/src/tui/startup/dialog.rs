use std::path::Path;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use vibe_app_server::startup::{SavedSessionSummary, WorkspaceTrustDecision, WorkspaceTrustPrompt};

use super::super::terminal::{CrosstermOps, TerminalGuard};
use super::StartupError;
use super::session::ResumeResolution;

pub(super) fn run_trust_dialog(
    prompt: &WorkspaceTrustPrompt,
) -> Result<Option<WorkspaceTrustDecision>, StartupError> {
    let selected = prompt
        .decisions
        .iter()
        .position(|decision| *decision == WorkspaceTrustDecision::TrustDirectory)
        .unwrap_or_default();
    let mut dialog = StartupDialog::Trust { prompt, selected };
    match run_dialog(&mut dialog)? {
        DialogResult::Trust(decision) => Ok(Some(decision)),
        DialogResult::Abort
        | DialogResult::Continue
        | DialogResult::StartNew
        | DialogResult::Resume(_) => Ok(None),
    }
}

pub(super) fn run_location_warning_dialog(warning: &str) -> Result<bool, StartupError> {
    let mut dialog = StartupDialog::LocationWarning { warning };
    Ok(matches!(run_dialog(&mut dialog)?, DialogResult::Continue))
}

pub(super) fn run_resume_dialog(
    cwd: &Path,
    sessions: &[SavedSessionSummary],
) -> Result<ResumeResolution, StartupError> {
    let mut dialog = StartupDialog::Resume {
        cwd,
        sessions,
        selected: 0,
    };
    match run_dialog(&mut dialog)? {
        DialogResult::Resume(id) => Ok(ResumeResolution::Resume(id)),
        DialogResult::StartNew => Ok(ResumeResolution::StartNew),
        DialogResult::Abort | DialogResult::Continue | DialogResult::Trust(_) => {
            Ok(ResumeResolution::Abort)
        }
    }
}

#[derive(Debug, Clone)]
enum StartupDialog<'a> {
    Trust {
        prompt: &'a WorkspaceTrustPrompt,
        selected: usize,
    },
    Resume {
        cwd: &'a Path,
        sessions: &'a [SavedSessionSummary],
        selected: usize,
    },
    LocationWarning {
        warning: &'a str,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DialogResult {
    Trust(WorkspaceTrustDecision),
    Resume(String),
    StartNew,
    Continue,
    Abort,
}

fn run_dialog(dialog: &mut StartupDialog<'_>) -> Result<DialogResult, StartupError> {
    let mut guard = TerminalGuard::enter(CrosstermOps::stdout())
        .map_err(|error| StartupError::Terminal(error.to_string()))?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal =
        Terminal::new(backend).map_err(|error| StartupError::Terminal(error.to_string()))?;
    let result = loop {
        terminal
            .draw(|frame| draw_startup_dialog(frame, dialog))
            .map_err(|error| StartupError::Terminal(error.to_string()))?;
        let event = event::read().map_err(|error| StartupError::Terminal(error.to_string()))?;
        if let Event::Key(key) = event
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            && let Some(result) = reduce_dialog(dialog, key)
        {
            break result;
        }
    };
    drop(terminal);
    guard
        .restore()
        .map_err(|error| StartupError::Terminal(error.to_string()))?;
    Ok(result)
}

fn reduce_dialog(dialog: &mut StartupDialog<'_>, key: KeyEvent) -> Option<DialogResult> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'q'))
    {
        return Some(DialogResult::Abort);
    }
    match dialog {
        StartupDialog::Trust { prompt, selected } => match key.code {
            KeyCode::Left | KeyCode::Char('h') => {
                *selected = selected
                    .checked_sub(1)
                    .unwrap_or(prompt.decisions.len() - 1);
                None
            }
            KeyCode::Right | KeyCode::Char('l') => {
                *selected = (*selected + 1) % prompt.decisions.len();
                None
            }
            KeyCode::Char(value @ '1'..='3') => {
                let index = usize::from(value as u8 - b'1');
                prompt
                    .decisions
                    .get(index)
                    .copied()
                    .map(DialogResult::Trust)
            }
            KeyCode::Enter => prompt
                .decisions
                .get(*selected)
                .copied()
                .map(DialogResult::Trust),
            KeyCode::Esc => Some(DialogResult::Abort),
            _ => None,
        },
        StartupDialog::Resume {
            sessions, selected, ..
        } => match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                *selected = selected.checked_sub(1).unwrap_or(sessions.len() - 1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                *selected = (*selected + 1) % sessions.len();
                None
            }
            KeyCode::Enter => sessions
                .get(*selected)
                .map(|session| DialogResult::Resume(session.id.clone())),
            KeyCode::Esc | KeyCode::Char('n') => Some(DialogResult::StartNew),
            _ => None,
        },
        StartupDialog::LocationWarning { .. } => match key.code {
            KeyCode::Enter | KeyCode::Char('y') => Some(DialogResult::Continue),
            KeyCode::Esc | KeyCode::Char('n') => Some(DialogResult::Abort),
            _ => None,
        },
    }
}

fn draw_startup_dialog(frame: &mut ratatui::Frame<'_>, dialog: &StartupDialog<'_>) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(frame.area());
    let (title, lines, help) = match dialog {
        StartupDialog::Trust { prompt, selected } => {
            let mut lines = vec![Line::from(
                "Malicious configs can modify AI behavior, exfiltrate data, run destructive commands, or silently alter your code.",
            )];
            append_detected_files(
                &mut lines,
                "Detected in current folder:",
                &prompt.detected_files,
            );
            append_detected_files(
                &mut lines,
                "Detected in repository context:",
                &prompt.repo_detected_files,
            );
            lines.extend([Line::from(""), Line::from(prompt.cwd.display().to_string())]);
            if let Some(repo) = &prompt.repo_root
                && repo != &prompt.cwd
            {
                let label = if prompt.repo_explicitly_untrusted {
                    "git repository is marked untrusted"
                } else {
                    "git repository"
                };
                lines.push(Line::from(format!("{label}: {}", repo.display())));
            }
            lines.push(Line::from(""));
            lines.extend(
                prompt
                    .decisions
                    .iter()
                    .enumerate()
                    .map(|(index, decision)| {
                        let label = match decision {
                            WorkspaceTrustDecision::TrustRepository => "Trust full repo",
                            WorkspaceTrustDecision::TrustDirectory => "Trust folder",
                            WorkspaceTrustDecision::Decline => "Don't trust",
                        };
                        selected_line(index, *selected, format!("{}. {label}", index + 1))
                    }),
            );
            lines.extend([
                Line::from(""),
                Line::from(format!(
                    "Setting will be saved in: {}",
                    prompt.settings_path.display()
                )),
            ]);
            (
                if prompt
                    .decisions
                    .contains(&WorkspaceTrustDecision::TrustRepository)
                {
                    " Trust folder or repository? "
                } else {
                    " Trust this folder? "
                },
                lines,
                "Left/Right navigate  Enter select  Ctrl+C abort",
            )
        }
        StartupDialog::Resume {
            cwd,
            sessions,
            selected,
        } => {
            let mut lines = vec![
                Line::from(format!("local {}", cwd.display())),
                Line::from(""),
            ];
            lines.extend(sessions.iter().enumerate().map(|(index, session)| {
                let short_id = session.id.chars().take(8).collect::<String>();
                selected_line(
                    index,
                    *selected,
                    format!("{:10}  {short_id}  {}", session.end_time, session.preview),
                )
            }));
            (
                " Resume a saved session ",
                lines,
                "Up/Down navigate  Enter resume  n/Esc start new  Ctrl+C abort",
            )
        }
        StartupDialog::LocationWarning { warning } => (
            " Sensitive location warning ",
            vec![
                Line::from(warning.to_string()),
                Line::from(""),
                Line::from("Continue startup from this location?"),
            ],
            "Enter/y continue  n/Esc/Ctrl+C abort",
        ),
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false }),
        body,
    );
    frame.render_widget(Paragraph::new(help).alignment(Alignment::Center), footer);
}

fn append_detected_files(lines: &mut Vec<Line<'static>>, title: &str, files: &[String]) {
    if files.is_empty() {
        return;
    }
    lines.extend([Line::from(""), Line::from(title.to_owned())]);
    lines.extend(files.iter().map(|path| Line::from(format!("  {path}"))));
}

fn selected_line(index: usize, selected: usize, content: String) -> Line<'static> {
    let prefix = if index == selected { ">" } else { " " };
    let line = format!("{prefix} {content}");
    if index == selected {
        Line::styled(line, Style::default().add_modifier(Modifier::BOLD))
    } else {
        Line::from(line)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::backend::TestBackend;

    use super::*;

    fn sessions() -> Vec<SavedSessionSummary> {
        vec![SavedSessionSummary {
            id: "session-1".to_owned(),
            end_time: "2026-08-02T10:00:00Z".to_owned(),
            preview: "finish startup parity".to_owned(),
        }]
    }

    #[test]
    fn startup_dialogs_render_at_parity_widths() {
        let root = Path::new("/workspace/project");
        let prompt = WorkspaceTrustPrompt {
            cwd: root.to_path_buf(),
            repo_root: Some(PathBuf::from("/workspace")),
            detected_files: vec![".vibe/".to_owned()],
            repo_detected_files: vec!["AGENTS.md".to_owned()],
            repo_explicitly_untrusted: false,
            settings_path: PathBuf::from("/home/test/.vibe/trusted_folders.toml"),
            decisions: vec![
                WorkspaceTrustDecision::TrustRepository,
                WorkspaceTrustDecision::TrustDirectory,
                WorkspaceTrustDecision::Decline,
            ],
        };
        let sessions = sessions();
        for width in [40, 80, 120] {
            for dialog in [
                StartupDialog::Trust {
                    prompt: &prompt,
                    selected: 1,
                },
                StartupDialog::Resume {
                    cwd: root,
                    sessions: &sessions,
                    selected: 0,
                },
                StartupDialog::LocationWarning {
                    warning: "WARNING: sensitive location",
                },
            ] {
                let backend = TestBackend::new(width, 24);
                let mut terminal = Terminal::new(backend).expect("terminal");
                terminal
                    .draw(|frame| draw_startup_dialog(frame, &dialog))
                    .expect("dialog render");
                let text = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(
                    text.contains("Trust") || text.contains("Resume") || text.contains("Sensitive")
                );
            }
        }
    }

    #[test]
    fn resume_dialog_preserves_new_session_and_abort_semantics() {
        let sessions = sessions();
        let mut dialog = StartupDialog::Resume {
            cwd: Path::new("/workspace"),
            sessions: &sessions,
            selected: 0,
        };
        assert_eq!(
            reduce_dialog(&mut dialog, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(DialogResult::StartNew)
        );
        assert_eq!(
            reduce_dialog(
                &mut dialog,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            Some(DialogResult::Abort)
        );
    }

    #[test]
    fn location_warning_requires_an_explicit_continue() {
        let mut dialog = StartupDialog::LocationWarning {
            warning: "WARNING: sensitive location",
        };
        assert_eq!(
            reduce_dialog(
                &mut dialog,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            Some(DialogResult::Continue)
        );
        assert_eq!(
            reduce_dialog(&mut dialog, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(DialogResult::Abort)
        );
    }
}
