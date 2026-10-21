//! The composer: the prompt box under the transcript, its border colored by
//! the session's safety, and the caret and skill ghost inside it.

use super::*;

pub(super) fn draw_input(
    frame: &mut Frame<'_>,
    area: Rect,
    editor: &PromptEditor,
    input_mode: InputMode,
    theme: ResolvedTheme,
    context: UiContext<'_>,
    (composer, caret, ghost): (&ComposerLayout, bool, Option<&str>),
) {
    let input_width = composer.width();
    let title = if context.secret_input {
        " API key (stored in native keyring) "
    } else if context.switching {
        " Switching "
    } else if context.feedback_active {
        " Rate response: 1-3, 0 later, Esc dismisses "
    } else if context.agent_name.is_empty() {
        ""
    } else {
        context.agent_name
    };
    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(composer_border_style(
            theme,
            context.safety,
            context.voice_phase,
        ))
        .title_style(theme.muted())
        .title_alignment(Alignment::Right)
        .title(title);
    frame.render_widget(block, area);
    let body = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(CHROME_HEIGHT),
    );
    let [prompt_area, input_area, _trailing_area] = Layout::horizontal([
        Constraint::Length(PROMPT_WIDTH),
        Constraint::Length(u16::try_from(input_width).unwrap_or(u16::MAX)),
        Constraint::Min(0),
    ])
    .areas(body);
    let text = editor_text(
        editor,
        context.secret_input,
        composer.visual(),
        composer.scroll(),
        usize::from(input_area.height),
    );
    const PEAK_BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    const FILL_BLOCKS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
    let indicator = usize::from(context.voice_indicator.min(7));
    let prompt = if context.voice_phase == VoicePhase::Transcribing {
        FILL_BLOCKS[indicator]
    } else if context.voice_phase.is_active() {
        PEAK_BLOCKS[indicator]
    } else if context.switching {
        '⠋'
    } else {
        input_mode.symbol()
    };
    frame.render_widget(
        Paragraph::new(prompt.to_string()).style(theme.orange()),
        prompt_area,
    );
    let input_style = if context.voice_phase.is_active() {
        theme.muted()
    } else {
        theme.base()
    };
    frame.render_widget(Paragraph::new(text).style(input_style), input_area);
    if caret && let Some(ghost) = ghost {
        draw_inline_ghost(frame, input_area, composer, ghost, theme);
    }
    if caret && input_area.width > 0 && input_area.height > 0 {
        frame.set_cursor_position((
            input_area.x.saturating_add(
                u16::try_from(composer.cursor_column())
                    .unwrap_or(u16::MAX)
                    .min(input_area.width.saturating_sub(1)),
            ),
            input_area.y.saturating_add(
                u16::try_from(composer.cursor_row().saturating_sub(composer.scroll()))
                    .unwrap_or(u16::MAX)
                    .min(input_area.height.saturating_sub(1)),
            ),
        ));
    }
}

/// Textual's `TextArea.suggestion`, which the reference's mid-prompt skill
/// ghost sets: drawn dim at the caret, with the rest of the caret's row pushed
/// right and cropped at the composer's edge.
fn draw_inline_ghost(
    frame: &mut Frame<'_>,
    area: Rect,
    composer: &ComposerLayout,
    ghost: &str,
    theme: ResolvedTheme,
) {
    let Some(row) = composer.cursor_row().checked_sub(composer.scroll()) else {
        return;
    };
    let (Ok(row), Ok(column)) = (u16::try_from(row), u16::try_from(composer.cursor_column()))
    else {
        return;
    };
    if row >= area.height || column >= area.width {
        return;
    }
    let y = area.y.saturating_add(row);
    let start = area.x.saturating_add(column);
    let right = area.right();
    let buffer = frame.buffer_mut();
    let tail = (start..right)
        .map(|x| buffer[(x, y)].clone())
        .collect::<Vec<_>>();
    let width = usize::from(right.saturating_sub(start));
    let style = theme.base().add_modifier(Modifier::DIM);
    let (end, _) = buffer.set_stringn(start, y, ghost, width, style);
    for (offset, cell) in tail.into_iter().enumerate() {
        let Some(x) = u16::try_from(offset)
            .ok()
            .and_then(|offset| end.checked_add(offset))
            .filter(|x| *x < right)
        else {
            break;
        };
        buffer[(x, y)] = cell;
    }
}

pub(super) fn composer_border_style(
    theme: ResolvedTheme,
    safety: Safety,
    voice: VoicePhase,
) -> Style {
    if voice.is_active() {
        return theme.orange();
    }
    match safety {
        Safety::Neutral => theme.muted(),
        Safety::Safe => theme.success(),
        Safety::Destructive => theme.warning(),
        Safety::Yolo => theme.error(),
    }
}

/// Maps an absolute terminal coordinate into the scrolled editor body.
/// Coordinates outside the visible composer return `None` without inventing
/// a nearest selection target.
pub(crate) fn editor_mouse_cell(
    editor: &PromptEditor,
    screen: Rect,
    _secret_input: bool,
    input_mode: InputMode,
    x: u16,
    y: u16,
) -> Option<(usize, usize)> {
    let composer =
        ComposerLayout::for_viewport(editor, screen.width, screen.height, input_mode.prefix_len());
    let input_height = composer.input_height();
    let input_y = screen
        .y
        .saturating_add(screen.height)
        .saturating_sub(1)
        .saturating_sub(input_height);
    let body_y = input_y.saturating_add(1);
    let body_height = composer.body_height();
    let input_x = screen.x.saturating_add(PROMPT_WIDTH);
    let input_width = u16::try_from(composer.width()).unwrap_or(u16::MAX);
    if x < input_x
        || x >= input_x.saturating_add(input_width)
        || y < body_y
        || y >= body_y.saturating_add(body_height)
    {
        return None;
    }
    Some((
        usize::from(y.saturating_sub(body_y)).saturating_add(composer.scroll()),
        usize::from(x.saturating_sub(input_x)),
    ))
}

fn editor_text(
    editor: &PromptEditor,
    secret_input: bool,
    layout: &VisualLayout,
    scroll: usize,
    height: usize,
) -> Text<'static> {
    let selection = editor.selection();
    let mut lines = Vec::with_capacity(height);
    for range in layout.lines().iter().skip(scroll).take(height) {
        let mut line = Vec::new();
        let mut column = 0usize;
        for index in range.clone() {
            let grapheme = layout.grapheme(editor.text(), index);
            let visible = if secret_input {
                "•".to_owned()
            } else if grapheme == "\t" {
                " ".repeat(4 - (column % 4))
            } else if grapheme.chars().all(char::is_control) {
                String::new()
            } else {
                sanitize_inline(grapheme)
            };
            column = column.saturating_add(UnicodeWidthStr::width(visible.as_str()));
            let selected = selection
                .as_ref()
                .is_some_and(|selection| selection.contains(&index));
            line.push(if selected {
                Span::styled(visible, Style::default().add_modifier(Modifier::REVERSED))
            } else {
                Span::raw(visible)
            });
        }
        lines.push(line);
    }
    Text::from(lines.into_iter().map(Line::from).collect::<Vec<_>>())
}
