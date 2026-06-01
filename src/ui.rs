use crate::app::{App, ConfirmAction, Mode};
use crate::checker::CheckStatus;
use ratatui::{
    prelude::*,
    widgets::*,
};

/// Render the TUI dialog.
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let dialog_width = area.width.saturating_sub(8).clamp(56, 92);
    let dialog_height = match app.mode {
        Mode::Select => ((app.backends.len() as u16) + 10)
            .max(15)
            .min(area.height.saturating_sub(4)),
        Mode::Create => 15.min(area.height.saturating_sub(4)),
    };

    let dialog_area = centered_rect(dialog_width, dialog_height, area);

    frame.render_widget(Clear, dialog_area);

    let block = Block::bordered()
        .title_top(render_tabs(app))
        .title_alignment(Alignment::Center);
    frame.render_widget(block.clone(), dialog_area);

    let inner = block.inner(dialog_area);

    match app.mode {
        Mode::Select => render_select(frame, inner, app),
        Mode::Create => render_create(frame, inner, app),
    }

    render_confirm_bar(frame, dialog_area, app);
}

fn render_tabs(app: &App) -> Line<'static> {
    let active_style = Style::default().add_modifier(Modifier::REVERSED);
    let inactive_style = Style::default();

    let (select_style, create_style) = match app.mode {
        Mode::Select => (active_style, inactive_style),
        Mode::Create => (inactive_style, active_style),
    };

    Line::from(vec![
        Span::styled(" Backend Switcher ", select_style),
        Span::raw(" "),
        Span::styled(" Create New Backend ", create_style),
    ])
}

// ---------------------------------------------------------------------------
// Select (backend list) view
// ---------------------------------------------------------------------------

fn render_select(frame: &mut Frame, area: Rect, app: &App) {
    let layout = Layout::vertical([
        Constraint::Length(1),  // header
        Constraint::Fill(1),    // list
        Constraint::Length(1),  // status
        Constraint::Length(1),  // models
        Constraint::Length(1),  // hint
    ])
    .split(area);

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
    if !app.backends.is_empty() {
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
    }

    // Hint
    let hint = Paragraph::new("↑/↓ Select  Enter Confirm  d Delete  r Check  ←/→ Tab  q/Esc Quit")
        .style(Style::default().add_modifier(Modifier::DIM));
    frame.render_widget(hint, layout[4]);
}

// ---------------------------------------------------------------------------
// Create form view
// ---------------------------------------------------------------------------

fn render_create(frame: &mut Frame, area: Rect, app: &App) {
    let label_style = Style::default().add_modifier(Modifier::BOLD);
    let active_style = Style::default().add_modifier(Modifier::REVERSED);
    let inactive_style = Style::default();

    let field_style = |idx: usize| {
        if idx == app.create_active_field {
            active_style
        } else {
            inactive_style
        }
    };

    let cursor_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);

    // Helper to render a labelled field
    let field = |label: &str, value: &str, idx: usize| -> Line<'static> {
        let mut spans = vec![Span::styled(label.to_string(), label_style)];
        spans.push(Span::styled(value.to_string(), field_style(idx)));
        if idx == app.create_active_field {
            spans.push(Span::styled("█", cursor_style));
        }
        Line::from(spans)
    };

    let rows = Layout::vertical([
        Constraint::Length(1),  // name
        Constraint::Length(1),  // base url
        Constraint::Length(1),  // api key
        Constraint::Length(1),  // description
        Constraint::Length(1),  // spacer
        Constraint::Length(1),  // status
        Constraint::Length(1),  // spacer
        Constraint::Length(1),  // hint
    ])
    .split(area);

    frame.render_widget(Paragraph::new(field(" Name:       ", &app.create_name, 0)), rows[0]);
    frame.render_widget(Paragraph::new(field(" Base URL:   ", &app.create_base_url, 1)), rows[1]);
    frame.render_widget(Paragraph::new(field(" API Key:    ", &app.create_api_key, 2)), rows[2]);
    frame.render_widget(Paragraph::new(field(" Description:", &app.create_description, 3)), rows[3]);

    // Status message
    if let Some(ref msg) = app.create_status {
        let color = if app.create_status_is_error {
            Color::Red
        } else {
            Color::Green
        };
        let status = Paragraph::new(msg.as_str())
            .style(Style::default().fg(color).add_modifier(Modifier::BOLD));
        frame.render_widget(status, rows[5]);
    }

    // Hint
    let hint = Paragraph::new("Tab/↓ Next  ↑ Prev  Enter Save  ←/→ Tab  q/Esc Quit")
        .style(Style::default().add_modifier(Modifier::DIM));
    frame.render_widget(hint, rows[7]);
}

// ---------------------------------------------------------------------------
// Confirmation overlay
// ---------------------------------------------------------------------------

fn render_confirm_bar(frame: &mut Frame, dialog_area: Rect, app: &App) {
    let msg = match app.confirm_action {
        ConfirmAction::None => return,
        ConfirmAction::DeleteBackend => {
            let name = if !app.backends.is_empty() {
                app.backends[app.selected].name.as_str()
            } else {
                ""
            };
            format!(" Delete '{}'? (y/n) ", name)
        }
        ConfirmAction::SaveBackend => {
            format!(" Save backend '{}'? (y/n) ", app.create_name.trim())
        }
    };

    let bar_width = msg.len() as u16 + 4;
    let bar_area = Rect {
        x: dialog_area.x + (dialog_area.width.saturating_sub(bar_width)) / 2,
        y: dialog_area.y + dialog_area.height.saturating_sub(2),
        width: bar_width,
        height: 1,
    };

    let style = Style::default()
        .bg(Color::Red)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let bar = Paragraph::new(msg.as_str()).style(style);
    frame.render_widget(Clear, bar_area);
    frame.render_widget(bar, bar_area);
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

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

fn list_state(selected: usize) -> ListState {
    let mut state = ListState::default();
    state.select(Some(selected));
    state
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}
