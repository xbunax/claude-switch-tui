use crate::app::App;
use crate::checker::CheckStatus;
use ratatui::{
    prelude::*,
    widgets::*,
};

/// Render the TUI dialog.
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // Calculate dialog dimensions
    let dialog_width = area.width.saturating_sub(8).clamp(54, 92);
    let dialog_height = ((app.backends.len() as u16) + 9)
        .max(13)
        .min(area.height.saturating_sub(4));

    let dialog_area = centered_rect(dialog_width, dialog_height, area);

    // Clear the area behind the dialog and draw the border
    frame.render_widget(Clear, dialog_area);

    let title = Line::styled(
        " Claude Code Backend Switcher ",
        Style::default().add_modifier(Modifier::BOLD),
    );
    let block = Block::bordered()
        .title(title)
        .title_alignment(Alignment::Center);
    frame.render_widget(block.clone(), dialog_area);

    let inner = block.inner(dialog_area);

    let layout = Layout::vertical([
        Constraint::Length(1),  // header
        Constraint::Fill(1),    // list
        Constraint::Length(1),  // status
        Constraint::Length(1),  // models
        Constraint::Length(1),  // hint
    ])
    .split(inner);

    // Header
    let header = Paragraph::new("Select backend configuration to load:")
        .style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_widget(header, layout[0]);

    // Backend list with status icons
    let items: Vec<ListItem> = app
        .backends
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let (icon, icon_color) = status_icon(&app.backend_status[i]);
            let mut spans = vec![Span::styled(icon, Style::default().fg(icon_color))];
            spans.push(Span::raw(b.name.as_str()));
            if let Some((suffix, color)) = model_count(&app.backend_status[i]) {
                spans.push(Span::styled(suffix, Style::default().fg(color)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");

    frame.render_stateful_widget(list, layout[1], &mut list_state(app.selected));

    // Status line
    let sel_status = &app.backend_status[app.selected];
    let desc = app.selected_backend().description.as_str();
    let status_text = match sel_status {
        CheckStatus::Pending => "Pending".to_string(),
        CheckStatus::InProgress => "Checking...".to_string(),
        CheckStatus::Reachable { .. } => "API Reachable".to_string(),
        CheckStatus::Unreachable { error } => format!("API Unreachable — {}", error),
        CheckStatus::Skipped { reason } => reason.clone(),
    };
    let status_line = format!("{} | {}", desc, status_text);
    let status_paragraph = Paragraph::new(Line::from(vec![
        Span::styled(status_line, Style::default().add_modifier(Modifier::DIM)),
    ]));
    frame.render_widget(status_paragraph, layout[2]);

    // Models line
    let models_text = match &app.backend_status[app.selected] {
        CheckStatus::Reachable { models } if !models.is_empty() => {
            let width = layout[3].width.saturating_sub(2) as usize;
            let joined = models.join(", ");
            if joined.len() > width {
                let truncated: String = joined.chars().take(width.saturating_sub(3)).collect();
                format!("Models: {}...", truncated)
            } else {
                format!("Models: {}", joined)
            }
        }
        CheckStatus::Reachable { .. } => "API reachable, no model list returned".into(),
        _ => String::new(),
    };

    let models_paragraph = Paragraph::new(models_text)
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM));
    frame.render_widget(models_paragraph, layout[3]);

    // Hint
    let hint = Paragraph::new("↑/↓ Select  Enter Confirm  r Check  q/Esc Quit")
        .style(Style::default().add_modifier(Modifier::DIM));
    frame.render_widget(hint, layout[4]);
}

fn status_icon(status: &CheckStatus) -> (&'static str, Color) {
    match status {
        CheckStatus::Pending => (" ? ", Color::Yellow),
        CheckStatus::InProgress => (" . ", Color::Yellow),
        CheckStatus::Reachable { .. } => (" \u{2713} ", Color::Green),
        CheckStatus::Unreachable { .. } => (" \u{2717} ", Color::Red),
        CheckStatus::Skipped { .. } => (" - ", Color::DarkGray),
    }
}

fn model_count(status: &CheckStatus) -> Option<(String, Color)> {
    match status {
        CheckStatus::Reachable { models } if !models.is_empty() => {
            Some((format!(" [{}]", models.len()), Color::Cyan))
        }
        _ => None,
    }
}

/// Create a ListState positioned at the given index.
fn list_state(selected: usize) -> ListState {
    let mut state = ListState::default();
    state.select(Some(selected));
    state
}

/// Create a centered rectangle of the given size within `area`.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}
