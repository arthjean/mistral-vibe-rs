//! Rendering for the onboarding screens.
//!
//! The observation trace, not the rendered cells, is the parity surface, so
//! this module aims for the reference's structure: a progressively typed
//! welcome line with an indexed gradient highlight, a wrapping theme picker
//! with three fade levels and a clamped preview, two-option card screens, a
//! validated domain input, a three-step sign-in progress, and a masked key
//! input. Every sentence is this port's own.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::context::DomainFeedback;
use super::model::{
    GRADIENT_COLORS, OnboardingModel, ScreenId, SignInVariant, THEME_VISIBLE_NEIGHBORS,
    gradient_color, masked, theme_fade_class, theme_preview_height,
};
use crate::tui::setup::Theme;
use crate::tui::themes::theme_polarity;

const WELCOME_PREFIX: &str = "Welcome to ";
const WELCOME_HIGHLIGHT: &str = "Mistral Vibe";
const WELCOME_SUFFIX: &str = ", let's set things up!";

fn welcome_text_chars() -> usize {
    WELCOME_PREFIX.chars().count()
        + WELCOME_HIGHLIGHT.chars().count()
        + WELCOME_SUFFIX.chars().count()
}

/// The animation clock the driver ticks every 40 ms: one welcome character
/// per tick, and a gradient step every other tick.
#[derive(Debug, Default)]
pub struct Animation {
    tick: usize,
}

impl Animation {
    /// Advances one tick; answers whether the welcome text has fully typed,
    /// which is when the advance action arms.
    pub fn advance(&mut self) -> bool {
        self.tick = self.tick.saturating_add(1);
        self.tick >= welcome_text_chars()
    }

    fn typed_chars(&self) -> usize {
        self.tick.min(welcome_text_chars())
    }

    fn gradient_offset(&self) -> usize {
        (self.tick / 2) % GRADIENT_COLORS.len()
    }
}

fn hex_color(hex: &str) -> Color {
    let parse = |range| u8::from_str_radix(hex.get(range).unwrap_or("0"), 16).unwrap_or(0);
    Color::Rgb(parse(1..3), parse(3..5), parse(5..7))
}

/// The whole-frame style the selected theme's polarity implies, so a
/// selection applies to the surrounding screen and not only the preview.
fn polarity_style(model: &OnboardingModel) -> Style {
    match theme_polarity(model.selected_theme()) {
        Some(Theme::Light) => Style::default().bg(Color::White).fg(Color::Black),
        Some(Theme::Dark) => Style::default().bg(Color::Black).fg(Color::White),
        _ => Style::default(),
    }
}

/// Whether the current screen carries an animation a tick must repaint: the
/// welcome typing and gradient, and the confirm step's animated waiting
/// detail on the sign-in screen.
pub fn animates(model: &OnboardingModel) -> bool {
    match model.current_screen() {
        ScreenId::Welcome => true,
        ScreenId::BrowserSignIn => {
            let view = model.sign_in_view();
            view.variant == SignInVariant::Pending && view.step == 1
        }
        _ => false,
    }
}

pub fn draw(frame: &mut Frame<'_>, model: &OnboardingModel, animation: &Animation) {
    let area = frame.area();
    frame.render_widget(Block::default().style(polarity_style(model)), area);
    match model.current_screen() {
        ScreenId::Welcome => draw_welcome(frame, area, animation, model.typing_done()),
        ScreenId::ThemeSelection => draw_theme_selection(frame, area, model),
        ScreenId::AuthMethod => draw_auth_method(frame, area, model),
        ScreenId::SignInTarget => draw_sign_in_target(frame, area, model),
        ScreenId::CustomDomain => draw_custom_domain(frame, area, model),
        ScreenId::BrowserSignIn => draw_browser_sign_in(frame, area, model, animation),
        ScreenId::ApiKey => draw_api_key(frame, area, model),
    }
}

fn centered(area: Rect, height: u16) -> Rect {
    let [_, middle, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height),
        Constraint::Fill(1),
    ])
    .areas(area);
    middle
}

fn draw_welcome(frame: &mut Frame<'_>, area: Rect, animation: &Animation, typing_done: bool) {
    let typed = animation.typed_chars();
    let offset = animation.gradient_offset();
    let prefix_length = WELCOME_PREFIX.chars().count();
    let highlight_length = WELCOME_HIGHLIGHT.chars().count();
    let mut spans = vec![Span::raw(
        WELCOME_PREFIX.chars().take(typed).collect::<String>(),
    )];
    if typed > prefix_length {
        let visible = (typed - prefix_length).min(highlight_length);
        spans.extend(WELCOME_HIGHLIGHT.chars().take(visible).enumerate().map(
            |(index, character)| {
                Span::styled(
                    character.to_string(),
                    Style::default()
                        .fg(hex_color(gradient_color(index, offset)))
                        .add_modifier(Modifier::BOLD),
                )
            },
        ));
    }
    if typed > prefix_length + highlight_length {
        spans.push(Span::raw(
            WELCOME_SUFFIX
                .chars()
                .take(typed - prefix_length - highlight_length)
                .collect::<String>(),
        ));
    }
    let mut lines = vec![Line::from(spans).alignment(Alignment::Center)];
    if typing_done {
        lines.push(Line::raw(""));
        lines.push(
            Line::styled(
                "Press Enter \u{21b5}",
                Style::default().add_modifier(Modifier::DIM),
            )
            .alignment(Alignment::Center),
        );
    }
    frame.render_widget(Paragraph::new(lines), centered(area, 3));
}

fn draw_theme_selection(frame: &mut Frame<'_>, area: Rect, model: &OnboardingModel) {
    let neighbors = THEME_VISIBLE_NEIGHBORS as isize;
    let mut lines = vec![
        Line::styled(
            "Select your preferred theme",
            Style::default().add_modifier(Modifier::BOLD),
        )
        .alignment(Alignment::Center),
        Line::styled(
            "Navigate with Up/Down, confirm with Enter",
            Style::default().add_modifier(Modifier::DIM),
        )
        .alignment(Alignment::Center),
        Line::raw(""),
    ];
    for offset in -neighbors..=neighbors {
        let theme = model.theme_at_offset(offset);
        let line = match theme_fade_class(offset) {
            None => Line::styled(
                format!(" {theme} "),
                Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
            ),
            Some("fade-1") => Line::styled(theme, Style::default().fg(Color::Gray)),
            Some("fade-2") => Line::styled(theme, Style::default().fg(Color::DarkGray)),
            Some(_) => Line::styled(
                theme,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
        };
        lines.push(line.alignment(Alignment::Center));
    }
    let list_height = lines.len() as u16;
    let preview_height = theme_preview_height(area.height).min(area.height.saturating_sub(1));
    let [list_area, preview_area] = Layout::vertical([
        Constraint::Length(list_height + 1),
        Constraint::Length(preview_height),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(lines), list_area);
    let preview = Paragraph::new(vec![
        Line::styled(
            "Sample heading",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::from(vec![
            Span::styled("bold", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(", "),
            Span::styled("italic", Style::default().add_modifier(Modifier::ITALIC)),
            Span::raw(", and "),
            Span::styled("inline code", Style::default().fg(Color::Yellow)),
        ]),
        Line::raw("- a bullet item"),
        Line::raw("1. a numbered item"),
        Line::styled(
            "fn sample() -> bool { true }",
            Style::default().fg(Color::Cyan),
        ),
        Line::styled(
            "> a quoted line",
            Style::default().add_modifier(Modifier::DIM),
        ),
    ])
    .wrap(Wrap { trim: false })
    .block(Block::default().borders(Borders::ALL).title(" Preview "));
    frame.render_widget(preview, preview_area);
}

/// A two-option card screen: the shared body of the method and target
/// screens.
#[allow(clippy::too_many_arguments)]
fn draw_option_screen(
    frame: &mut Frame<'_>,
    area: Rect,
    heading: &str,
    subtitle: &str,
    options: [(&str, Option<&str>, &str); 2],
    selected: usize,
    warning: Option<String>,
    help: &str,
) {
    let mut lines = vec![
        Line::styled(
            heading.to_owned(),
            Style::default().add_modifier(Modifier::BOLD),
        )
        .alignment(Alignment::Center),
        Line::styled(
            subtitle.to_owned(),
            Style::default().add_modifier(Modifier::DIM),
        )
        .alignment(Alignment::Center),
        Line::raw(""),
    ];
    for (index, (title, badge, description)) in options.iter().enumerate() {
        let marker = if index == selected { "> " } else { "  " };
        let mut title_style = Style::default().add_modifier(Modifier::BOLD);
        if index == selected {
            title_style = title_style.fg(Color::Yellow);
        }
        let mut spans = vec![
            Span::raw(marker.to_owned()),
            Span::styled((*title).to_owned(), title_style),
        ];
        if let Some(badge) = badge {
            spans.push(Span::styled(
                format!("  [{badge}]"),
                Style::default().fg(Color::Green),
            ));
        }
        lines.push(Line::from(spans));
        lines.push(Line::styled(
            format!("    {description}"),
            Style::default().add_modifier(Modifier::DIM),
        ));
        lines.push(Line::raw(""));
    }
    if let Some(warning) = warning {
        lines.push(Line::styled(warning, Style::default().fg(Color::Yellow)));
        lines.push(Line::raw(""));
    }
    lines.push(Line::styled(
        help.to_owned(),
        Style::default().add_modifier(Modifier::DIM),
    ));
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        centered(area, 12),
    );
}

fn draw_auth_method(frame: &mut Frame<'_>, area: Rect, model: &OnboardingModel) {
    draw_option_screen(
        frame,
        area,
        "Mistral Vibe setup",
        "How do you want to sign in?",
        [
            (
                "Open the browser",
                Some("Recommended"),
                "Sign in on the Mistral console; setup finishes on its own.",
            ),
            (
                "Paste an API key",
                None,
                "Enter a key you already generated.",
            ),
        ],
        model.method_selection(),
        None,
        "Up/Down navigate  Enter select  Esc cancel",
    );
}

fn draw_sign_in_target(frame: &mut Frame<'_>, area: Rect, model: &OnboardingModel) {
    let warning = model.override_armed().then(|| {
        let domain = super::context::configured_custom_domain(model.initial_provider())
            .unwrap_or_default()
            .to_owned();
        format!(
            "Continuing replaces the configured custom domain ({domain}). Press Enter once \
             more to confirm."
        )
    });
    draw_option_screen(
        frame,
        area,
        "Browser sign-in",
        "Which console do you sign in to?",
        [
            (
                "Mistral AI",
                Some("Recommended"),
                "The hosted Mistral console.",
            ),
            (
                "Other",
                None,
                "A Mistral-compatible deployment on your own domain.",
            ),
        ],
        model.target_selection(),
        warning,
        "Up/Down navigate  Enter select  Esc back",
    );
}

fn draw_custom_domain(frame: &mut Frame<'_>, area: Rect, model: &OnboardingModel) {
    let feedback = model.domain_feedback();
    let border_color = match feedback {
        Some(DomainFeedback::Valid) => Color::Green,
        Some(DomainFeedback::Warning) => Color::Yellow,
        Some(DomainFeedback::Invalid) => Color::Red,
        None => Color::Reset,
    };
    let value = if model.domain_value().is_empty() {
        Span::styled(
            "console.mistral.ai",
            Style::default().add_modifier(Modifier::DIM),
        )
    } else {
        Span::raw(model.domain_value().to_owned())
    };
    let feedback_line = match feedback {
        Some(DomainFeedback::Valid) => {
            Line::styled("Press Enter to continue", Style::default().fg(Color::Green))
        }
        Some(DomainFeedback::Warning) => Line::styled(
            "This looks like a Mistral private-cloud domain. Mistral-hosted accounts sign in \
             at console.mistral.ai; continue only for a self-hosted deployment.",
            Style::default().fg(Color::Yellow),
        ),
        Some(DomainFeedback::Invalid) => Line::styled(
            "This is not a usable domain or URL.",
            Style::default().fg(Color::Red),
        ),
        None => Line::raw(""),
    };
    let lines = vec![
        Line::styled(
            "Use a custom domain",
            Style::default().add_modifier(Modifier::BOLD),
        )
        .alignment(Alignment::Center),
        Line::styled(
            "Point the CLI at a Mistral-compatible deployment.",
            Style::default().add_modifier(Modifier::DIM),
        )
        .alignment(Alignment::Center),
    ];
    let [header_area, input_area, feedback_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
    ])
    .areas(centered(area, 9));
    frame.render_widget(Paragraph::new(lines), header_area);
    frame.render_widget(
        Paragraph::new(Line::from(value)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title(" Custom domain "),
        ),
        input_area,
    );
    frame.render_widget(
        Paragraph::new(vec![
            feedback_line,
            Line::styled(
                "Esc goes back",
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])
        .wrap(Wrap { trim: false }),
        feedback_area,
    );
}

fn draw_browser_sign_in(
    frame: &mut Frame<'_>,
    area: Rect,
    model: &OnboardingModel,
    animation: &Animation,
) {
    let view = model.sign_in_view();
    let steps = [
        (
            "Open the browser",
            "The browser opens on its own",
            "Browser opened",
        ),
        (
            "Confirm the sign-in",
            "Waiting while you sign in...",
            "Sign-in confirmed",
        ),
        (
            "Finish setup",
            "The session starts on its own",
            "Setup finished",
        ),
    ];
    let mut lines = vec![
        Line::styled(
            "Browser sign-in",
            Style::default().add_modifier(Modifier::BOLD),
        )
        .alignment(Alignment::Center),
        Line::raw(""),
    ];
    for (index, (title, pending, done)) in steps.iter().enumerate() {
        let (marker, style) = if index < view.step {
            ("  ", Style::default().fg(Color::Green))
        } else if index == view.step {
            ("> ", Style::default().add_modifier(Modifier::BOLD))
        } else {
            ("  ", Style::default().add_modifier(Modifier::DIM))
        };
        lines.push(Line::styled(format!("{marker}{title}"), style));
        let detail = if index < view.step {
            Line::styled(format!("    {done}"), Style::default().fg(Color::Green))
        } else if index == view.step {
            match view.variant {
                SignInVariant::Error => Line::styled(
                    format!("    {}", view.message),
                    Style::default().fg(Color::Red),
                ),
                SignInVariant::Success => Line::styled(
                    format!("    {}", view.message),
                    Style::default().fg(Color::Green),
                ),
                SignInVariant::Pending if index == 1 => {
                    let offset = animation.gradient_offset();
                    Line::from(
                        std::iter::once(Span::raw("    ".to_owned()))
                            .chain(view.message.chars().enumerate().map(|(index, character)| {
                                Span::styled(
                                    character.to_string(),
                                    Style::default()
                                        .fg(hex_color(gradient_color(index, offset)))
                                        .add_modifier(Modifier::BOLD),
                                )
                            }))
                            .collect::<Vec<_>>(),
                    )
                }
                SignInVariant::Pending => Line::styled(
                    format!("    {}", view.message),
                    Style::default().add_modifier(Modifier::DIM),
                ),
            }
        } else {
            Line::styled(
                format!("    {pending}"),
                Style::default().add_modifier(Modifier::DIM),
            )
        };
        lines.push(detail);
        lines.push(Line::raw(""));
    }
    if view.show_url_help && view.variant != SignInVariant::Success {
        if view.reveal_url {
            if let Some(url) = &view.sign_in_url {
                lines.push(Line::raw(format!(
                    "Copy this URL if the clipboard copy did not work: {url}"
                )));
            }
        } else {
            lines.push(Line::styled(
                "If the browser did not open, press c to copy the sign-in URL.",
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        lines.push(Line::raw(""));
    }
    let hint = match view.variant {
        SignInVariant::Success => "Starting your session...",
        SignInVariant::Error => "Press r to retry  m for manual entry  Esc cancel",
        SignInVariant::Pending => "Press m for manual entry  Esc cancel",
    };
    lines.push(Line::styled(
        hint,
        Style::default().add_modifier(Modifier::DIM),
    ));
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        centered(area, 14),
    );
}

fn draw_api_key(frame: &mut Frame<'_>, area: Rect, model: &OnboardingModel) {
    let provider_name = model
        .provider()
        .get("name")
        .and_then(toml::Value::as_str)
        .unwrap_or("provider")
        .to_owned();
    // The console link belongs to the shipped provider; another entry names
    // its own console outside this build's knowledge.
    let help_url = (provider_name == "mistral").then(|| {
        format!(
            "{}/code/extensions?focus=key",
            model.vibe_base_url().trim_end_matches('/')
        )
    });
    let feedback_line = match model.key_feedback() {
        Some(true) => Line::styled("Press Enter to continue", Style::default().fg(Color::Green)),
        Some(false) => Line::styled("An API key is required.", Style::default().fg(Color::Red)),
        None => Line::raw(""),
    };
    let [header_area, input_area, feedback_area] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(3),
        Constraint::Length(2),
    ])
    .areas(centered(area, 9));
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                format!("Paste your {provider_name} API key"),
                Style::default().add_modifier(Modifier::BOLD),
            )
            .alignment(Alignment::Center),
            Line::styled(
                "Generate or copy a key from your console:",
                Style::default().add_modifier(Modifier::DIM),
            )
            .alignment(Alignment::Center),
            help_url
                .map_or_else(
                    || Line::raw(""),
                    |url| Line::styled(url, Style::default().fg(Color::Cyan)),
                )
                .alignment(Alignment::Center),
        ]),
        header_area,
    );
    frame.render_widget(
        Paragraph::new(Line::raw(masked(model.key_value_len()))).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Paste API key "),
        ),
        input_area,
    );
    frame.render_widget(
        Paragraph::new(vec![
            feedback_line,
            Line::styled(
                "Esc cancels setup",
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]),
        feedback_area,
    );
}
