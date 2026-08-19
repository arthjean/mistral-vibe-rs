use std::path::Path;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use vibe_app_server::startup::{
    SavedSessionSummary, StartupHost, WorkspaceTrustDecision, WorkspaceTrustPrompt,
};

use super::super::session_picker::{SessionDeleteDecision, SessionDeleteState};
use super::super::terminal::{CrosstermOps, TerminalGuard};
use super::super::updates::{UPDATE_DIALOG_TITLE, UpdateChoice, UpdatePromptMode, version_line};
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
    match run_dialog(&mut dialog, None)? {
        DialogResult::Trust(decision) => Ok(Some(decision)),
        DialogResult::Abort
        | DialogResult::Continue
        | DialogResult::StartNew
        | DialogResult::Update(_)
        | DialogResult::Resume(_) => Ok(None),
    }
}

pub(super) fn run_location_warning_dialog(warning: &str) -> Result<bool, StartupError> {
    let mut dialog = StartupDialog::LocationWarning { warning };
    Ok(matches!(
        run_dialog(&mut dialog, None)?,
        DialogResult::Continue
    ))
}

/// The choice the operator made, or `None` when the dialog was aborted.
///
/// The reference dialog installs the update itself and answers with an
/// `UpdatePromptResult`; this loop is synchronous, so it answers with the
/// choice and the caller runs the upgrade.
pub(super) fn run_update_dialog(
    current_version: &str,
    latest_version: &str,
    mode: UpdatePromptMode,
) -> Result<Option<UpdateChoice>, StartupError> {
    let mut dialog = StartupDialog::Update {
        current_version,
        latest_version,
        mode,
        selected: 0,
    };
    Ok(match run_dialog(&mut dialog, None)? {
        DialogResult::Update(choice) => Some(choice),
        DialogResult::Continue => Some(UpdateChoice::Continue),
        DialogResult::Abort
        | DialogResult::StartNew
        | DialogResult::Resume(_)
        | DialogResult::Trust(_) => None,
    })
}

pub(super) fn run_resume_dialog(
    host: &StartupHost,
    cwd: &Path,
    sessions: &[SavedSessionSummary],
) -> Result<ResumeResolution, StartupError> {
    let mut dialog = StartupDialog::Resume {
        cwd,
        sessions: sessions.to_vec(),
        selected: 0,
        delete: None,
    };
    match run_dialog(&mut dialog, Some(host))? {
        DialogResult::Resume(id) => Ok(ResumeResolution::Resume(id)),
        DialogResult::StartNew => Ok(ResumeResolution::StartNew),
        DialogResult::Abort
        | DialogResult::Continue
        | DialogResult::Update(_)
        | DialogResult::Trust(_) => Ok(ResumeResolution::Abort),
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
        sessions: Vec<SavedSessionSummary>,
        selected: usize,
        delete: Option<SessionDeleteState>,
    },
    LocationWarning {
        warning: &'a str,
    },
    Update {
        current_version: &'a str,
        latest_version: &'a str,
        mode: UpdatePromptMode,
        selected: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DialogResult {
    Trust(WorkspaceTrustDecision),
    Resume(String),
    StartNew,
    Continue,
    Abort,
    Update(UpdateChoice),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DialogStep {
    Complete(DialogResult),
    DeleteSession(String),
}

fn run_dialog(
    dialog: &mut StartupDialog<'_>,
    startup_host: Option<&StartupHost>,
) -> Result<DialogResult, StartupError> {
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
            && let Some(step) = reduce_dialog(dialog, key)
        {
            match step {
                DialogStep::Complete(result) => break result,
                DialogStep::DeleteSession(session_id) => {
                    let result = startup_host.map_or_else(
                        || Err("startup session deletion is unavailable".to_owned()),
                        |host| {
                            host.delete_session(&session_id)
                                .map_err(|error| error.to_string())
                        },
                    );
                    if apply_session_delete_result(dialog, &session_id, result) {
                        break DialogResult::StartNew;
                    }
                }
            }
        }
    };
    drop(terminal);
    guard
        .restore()
        .map_err(|error| StartupError::Terminal(error.to_string()))?;
    Ok(result)
}

fn reduce_dialog(dialog: &mut StartupDialog<'_>, key: KeyEvent) -> Option<DialogStep> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'q'))
    {
        return Some(DialogStep::Complete(DialogResult::Abort));
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
                    .map(DialogStep::Complete)
            }
            KeyCode::Enter => prompt
                .decisions
                .get(*selected)
                .copied()
                .map(DialogResult::Trust)
                .map(DialogStep::Complete),
            KeyCode::Esc => Some(DialogStep::Complete(DialogResult::Abort)),
            _ => None,
        },
        StartupDialog::Resume {
            sessions,
            selected,
            delete,
            ..
        } => match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                *selected = selected.checked_sub(1).unwrap_or(sessions.len() - 1);
                *delete = None;
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                *selected = (*selected + 1) % sessions.len();
                *delete = None;
                None
            }
            KeyCode::Enter => sessions
                .get(*selected)
                .filter(|session| {
                    !delete
                        .as_ref()
                        .is_some_and(|delete| delete.session_id == session.id)
                })
                .map(|session| DialogStep::Complete(DialogResult::Resume(session.id.clone()))),
            KeyCode::Char('d') => {
                let session_id = sessions.get(*selected)?.id.clone();
                match SessionDeleteState::request(delete.as_ref(), &session_id, false) {
                    SessionDeleteDecision::Show(next) => {
                        *delete = Some(next);
                        None
                    }
                    SessionDeleteDecision::Delete => Some(DialogStep::DeleteSession(session_id)),
                }
            }
            KeyCode::Esc if delete.is_some() => {
                *delete = None;
                None
            }
            KeyCode::Esc | KeyCode::Char('n') => Some(DialogStep::Complete(DialogResult::StartNew)),
            _ => None,
        },
        StartupDialog::LocationWarning { .. } => match key.code {
            KeyCode::Enter | KeyCode::Char('y') => {
                Some(DialogStep::Complete(DialogResult::Continue))
            }
            KeyCode::Esc | KeyCode::Char('n') => Some(DialogStep::Complete(DialogResult::Abort)),
            _ => None,
        },
        // Reference `UpdatePromptDialog`: only left, right, and Enter act, and
        // Ctrl+C or Ctrl+Q quits without touching the installed version.
        StartupDialog::Update { selected, .. } => match key.code {
            KeyCode::Left => {
                *selected = (*selected + UpdateChoice::ORDER.len() - 1) % UpdateChoice::ORDER.len();
                None
            }
            KeyCode::Right => {
                *selected = (*selected + 1) % UpdateChoice::ORDER.len();
                None
            }
            KeyCode::Enter => UpdateChoice::ORDER
                .get(*selected)
                .copied()
                .map(DialogResult::Update)
                .map(DialogStep::Complete),
            _ => None,
        },
    }
}

fn apply_session_delete_result(
    dialog: &mut StartupDialog<'_>,
    session_id: &str,
    result: Result<(), String>,
) -> bool {
    let StartupDialog::Resume {
        sessions,
        selected,
        delete,
        ..
    } = dialog
    else {
        return false;
    };
    match result {
        Ok(()) => {
            sessions.retain(|session| session.id != session_id);
            *selected = (*selected).min(sessions.len().saturating_sub(1));
            *delete = None;
            sessions.is_empty()
        }
        Err(error) => {
            *delete = Some(SessionDeleteState::failure(session_id, error));
            false
        }
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
            delete,
        } => {
            let mut lines = vec![
                Line::from(format!("local {}", cwd.display())),
                Line::from(""),
            ];
            lines.extend(sessions.iter().enumerate().map(|(index, session)| {
                let short_id = session.id.chars().take(8).collect::<String>();
                let preview = delete
                    .as_ref()
                    .filter(|delete| delete.session_id == session.id)
                    .map_or(session.preview.as_str(), SessionDeleteState::message);
                selected_line(
                    index,
                    *selected,
                    format!("{:10}  {short_id}  {preview}", session.end_time),
                )
            }));
            (
                " Resume a saved session ",
                lines,
                "Up/Down navigate  Enter resume  d delete  n/Esc start new  Ctrl+C abort",
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
        StartupDialog::Update {
            current_version,
            latest_version,
            mode,
            selected,
        } => {
            let mut lines = vec![
                Line::from(UPDATE_DIALOG_TITLE),
                Line::from(version_line(current_version, latest_version)),
                Line::from(""),
            ];
            lines.extend(
                UpdateChoice::ORDER
                    .iter()
                    .enumerate()
                    .map(|(index, choice)| {
                        selected_line(index, *selected, choice.label(*mode).to_owned())
                    }),
            );
            (
                " Update available ",
                lines,
                "Left/Right navigate  Enter select  Ctrl+C quit",
            )
        }
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
                    sessions: sessions.clone(),
                    selected: 0,
                    delete: None,
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
            sessions,
            selected: 0,
            delete: None,
        };
        assert_eq!(
            reduce_dialog(&mut dialog, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(DialogStep::Complete(DialogResult::StartNew))
        );
        assert_eq!(
            reduce_dialog(
                &mut dialog,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            Some(DialogStep::Complete(DialogResult::Abort))
        );
    }

    #[test]
    fn resume_delete_requires_confirmation_and_recovers_from_failure() {
        let mut available = sessions();
        available.push(SavedSessionSummary {
            id: "session-2".to_owned(),
            end_time: "2026-08-02T11:00:00Z".to_owned(),
            preview: "retain on failure".to_owned(),
        });
        let mut dialog = StartupDialog::Resume {
            cwd: Path::new("/workspace"),
            sessions: available,
            selected: 0,
            delete: None,
        };
        let delete = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE);

        assert_eq!(reduce_dialog(&mut dialog, delete), None);
        assert_eq!(
            reduce_dialog(&mut dialog, delete),
            Some(DialogStep::DeleteSession("session-1".to_owned()))
        );
        assert!(!apply_session_delete_result(
            &mut dialog,
            "session-1",
            Err("storage unavailable".to_owned())
        ));
        let StartupDialog::Resume {
            sessions, delete, ..
        } = &dialog
        else {
            unreachable!("resume dialog remains open")
        };
        assert_eq!(sessions.len(), 2);
        assert!(
            delete
                .as_ref()
                .is_some_and(|delete| delete.message().contains("storage unavailable"))
        );
    }

    #[test]
    fn deleting_the_final_startup_session_selects_start_new() {
        let mut dialog = StartupDialog::Resume {
            cwd: Path::new("/workspace"),
            sessions: sessions(),
            selected: 0,
            delete: None,
        };
        let delete = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(reduce_dialog(&mut dialog, delete), None);
        assert_eq!(
            reduce_dialog(&mut dialog, delete),
            Some(DialogStep::DeleteSession("session-1".to_owned()))
        );
        assert!(apply_session_delete_result(
            &mut dialog,
            "session-1",
            Ok(())
        ));
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
            Some(DialogStep::Complete(DialogResult::Continue))
        );
        assert_eq!(
            reduce_dialog(&mut dialog, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(DialogStep::Complete(DialogResult::Abort))
        );
    }
}
