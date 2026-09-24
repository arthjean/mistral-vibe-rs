use super::*;

pub(super) fn draw_rewind(frame: &mut Frame<'_>, rewind: &RewindState, theme: ResolvedTheme) {
    let outer = frame.area();
    let width = outer.width.saturating_sub(4).clamp(1, 86);
    let inner_width = usize::from(width.saturating_sub(4).max(1));
    let target = rewind.target();
    let (position, count) = rewind.target_position();
    let mut lines = Vec::new();
    lines.push(Line::styled(
        format!("Message {position} of {count}"),
        theme.muted(),
    ));
    lines.push(Line::styled(
        truncate_width(
            &sanitize_inline(target.message.lines().next().unwrap_or_default()),
            inner_width,
        ),
        theme.base().add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::default());
    for (index, action) in rewind.actions().iter().enumerate() {
        let selected = *action == rewind.selected_action();
        let label = match action {
            RewindChoice::EditAndRestore => "Edit & restore files to this point",
            RewindChoice::EditOnly if target.has_file_changes => "Edit without restoring files",
            RewindChoice::EditOnly => "Edit message from here",
            RewindChoice::InPlace => "Stay in this session",
            RewindChoice::Fork => "Fork into a new session",
        };
        let style = if selected {
            theme.secondary().add_modifier(Modifier::BOLD)
        } else {
            theme.base()
        };
        lines.push(Line::styled(
            format!(
                "{} {}. {label}",
                if selected { "›" } else { " " },
                index + 1
            ),
            style,
        ));
    }
    if !target.has_file_changes && !rewind.choosing_persistence() {
        lines.push(Line::styled(
            "No file changes need restoration at this point.",
            theme.muted(),
        ));
    }
    if let Some(error) = rewind.error() {
        lines.push(Line::default());
        lines.push(Line::styled(error.to_owned(), theme.warning()));
    }
    lines.push(Line::default());
    lines.push(Line::styled(
        if rewind.choosing_persistence() {
            "↑↓/jk choose  Enter confirm  Esc back  q cancel"
        } else {
            "←/Esc previous  → next  Shift+↑↓ scroll  ↑↓/jk choose  Enter accept  q cancel"
        },
        theme.muted(),
    ));
    let visual_rows = lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(inner_width.max(1)))
        .sum::<usize>();
    let content_height = u16::try_from(visual_rows)
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let height = content_height.min(outer.height.saturating_sub(2).max(1));
    let area = Rect::new(
        outer
            .x
            .saturating_add(outer.width.saturating_sub(width) / 2),
        outer
            .y
            .saturating_add(outer.height.saturating_sub(height) / 2),
        width,
        height,
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Edit an earlier message ")
                    .padding(Padding::horizontal(1))
                    .style(theme.base()),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}
