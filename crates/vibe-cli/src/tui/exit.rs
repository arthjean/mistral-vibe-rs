//! Exit contract pinned to the reference `QuitManager` and
//! `vibe.cli.session_exit`.

use super::interaction::QuitConfirmation;

/// Reference `action_suspend_with_message`.
pub const SUSPEND_MESSAGE: &str =
    "Mistral Vibe has been suspended. Run fg to bring Mistral Vibe back.";

/// Reference `TokenUsage`, as the exit summary reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl SessionUsage {
    #[must_use]
    pub const fn total_tokens(self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// What the process prints after the terminal is restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionExitSummary {
    pub session_id: Option<String>,
    pub usage: SessionUsage,
}

/// Reference `shorten_session_id`.
#[must_use]
pub fn shorten_session_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

/// Reference `format_session_usage`.
#[must_use]
pub fn format_session_usage(usage: SessionUsage) -> String {
    format!(
        "Total tokens used this session: input={} output={} (total={})",
        thousands(usage.input_tokens),
        thousands(usage.output_tokens),
        thousands(usage.total_tokens()),
    )
}

/// Reference `print_session_resume_message`, including its blank lines.
#[must_use]
pub fn session_resume_lines(summary: &SessionExitSummary) -> Vec<String> {
    let Some(session_id) = summary.session_id.as_deref() else {
        return Vec::new();
    };
    vec![
        String::new(),
        format_session_usage(summary.usage),
        String::new(),
        "To continue this session, run: vibe --continue".to_owned(),
        format!("Or: vibe --resume {}", shorten_session_id(session_id)),
    ]
}

/// Reference `QueueController.quit_warning_extra`.
#[must_use]
pub fn quit_warning_extra(queued: usize) -> String {
    if queued == 0 {
        return String::new();
    }
    let plural = if queued == 1 { "" } else { "s" };
    format!("{queued} queued message{plural} will be discarded")
}

/// Reference `QuitManager.request_confirmation`.
#[must_use]
pub fn quit_prompt(key: &str, queued: usize) -> String {
    let extra = quit_warning_extra(queued);
    if extra.is_empty() {
        format!("Press {key} again to quit")
    } else {
        format!("Press {key} again to quit ({extra})")
    }
}

/// Reference `action_interrupt_or_quit` and `action_delete_right_or_quit`: only
/// `Ctrl+D` honours `ask_confirmation_on_exit`, and a confirmation is bound to
/// the key that requested it.
pub fn resolve_quit(
    key: &str,
    ask_confirmation_on_exit: bool,
    confirmation: &mut QuitConfirmation,
    now_ms: u64,
) -> bool {
    if key == "Ctrl+D" && !ask_confirmation_on_exit {
        return true;
    }
    confirmation.request(key, now_ms)
}

/// Python's `f"{value:,}"`.
fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_is_grouped_like_the_reference() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(
            format_session_usage(SessionUsage {
                input_tokens: 1_234,
                output_tokens: 567,
            }),
            "Total tokens used this session: input=1,234 output=567 (total=1,801)"
        );
    }

    #[test]
    fn resume_output_needs_a_persisted_session() {
        assert!(
            session_resume_lines(&SessionExitSummary {
                session_id: None,
                usage: SessionUsage::default(),
            })
            .is_empty()
        );
        assert_eq!(
            session_resume_lines(&SessionExitSummary {
                session_id: Some("0123456789abcdef".to_owned()),
                usage: SessionUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            }),
            vec![
                String::new(),
                "Total tokens used this session: input=10 output=5 (total=15)".to_owned(),
                String::new(),
                "To continue this session, run: vibe --continue".to_owned(),
                "Or: vibe --resume 01234567".to_owned(),
            ]
        );
    }

    #[test]
    fn the_quit_ladder_follows_the_reference_key_rules() {
        let mut confirmation = QuitConfirmation::default();
        // Ctrl+C always confirms, whatever the preference says.
        assert!(!resolve_quit("Ctrl+C", false, &mut confirmation, 0));
        assert!(resolve_quit("Ctrl+C", false, &mut confirmation, 500));

        // Ctrl+D quits immediately only when confirmation is disabled.
        assert!(resolve_quit("Ctrl+D", false, &mut confirmation, 1_000));
        assert!(!resolve_quit("Ctrl+D", true, &mut confirmation, 2_000));
        assert_eq!(confirmation.pending_key(), Some("Ctrl+D"));
        assert!(resolve_quit("Ctrl+D", true, &mut confirmation, 2_500));

        // A different key, or a late second press, starts a new confirmation.
        assert!(!resolve_quit("Ctrl+D", true, &mut confirmation, 3_000));
        assert!(!resolve_quit("Ctrl+C", true, &mut confirmation, 3_100));
        assert!(!resolve_quit("Ctrl+C", true, &mut confirmation, 5_000));
    }

    #[test]
    fn quit_prompts_carry_the_queue_warning() {
        assert_eq!(quit_prompt("Ctrl+C", 0), "Press Ctrl+C again to quit");
        assert_eq!(
            quit_prompt("Ctrl+D", 1),
            "Press Ctrl+D again to quit (1 queued message will be discarded)"
        );
        assert_eq!(
            quit_prompt("Ctrl+C", 3),
            "Press Ctrl+C again to quit (3 queued messages will be discarded)"
        );
    }
}
