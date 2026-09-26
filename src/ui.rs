use crate::app::{App, ConfirmAction, CreateAuthType, Mode};
use crate::checker::CheckStatus;
use crate::tracker;
use ratatui::{
    prelude::*,
    widgets::*,
};

/// Maximum viewport height to reserve (always assumes expanded so the inline
/// area is large enough when the user presses Tab).
pub fn viewport_height(app: &App, terminal_height: u16) -> u16 {
    let max_env_count = app
        .backends
        .iter()
        .map(|b| b.env.len() as u16)
        .max()
        .unwrap_or(0);
    calc_dialog_height(app.backends.len(), max_env_count, terminal_height, app.mode)
}

/// Compute the dialog rendering height for the current expand state.
pub fn dialog_height(app: &App, terminal_height: u16) -> u16 {
    let env_count = if app.expanded && app.mode == Mode::Select && !app.backends.is_empty() {
        app.backends[app.selected].env.len() as u16
    } else {
        0
    };
    calc_dialog_height(app.backends.len(), env_count, terminal_height, app.mode)
}

fn calc_dialog_height(backend_count: usize, env_count: u16, terminal_height: u16, mode: Mode) -> u16 {
    let detail_extra: u16 = if mode == Mode::Select {
        (env_count + 10).min(30)  // +10 for token usage section below env vars
    } else {
        0
    };
    match mode {
        Mode::Select => {
            let list_rows = (backend_count as u16).min(10).max(3);
            // header + list + gap + detail + hint + outer border
            (1 + list_rows + 1 + detail_extra + 1 + 2)
                .max(20)
                .min(terminal_height.saturating_sub(4))
        }
        Mode::Create => 15.min(terminal_height.saturating_sub(4)),
    }
}

/// Render the TUI dialog.
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let dialog_width = area.width.saturating_sub(8).clamp(56, 92);
    let dialog_height = dialog_height(app, area.height);

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
    if app.expanded && !app.backends.is_empty() {
        render_select_expanded(frame, area, app);
    } else {
        render_select_normal(frame, area, app);
    }
}

fn render_select_normal(frame: &mut Frame, area: Rect, app: &App) {
    let layout = Layout::vertical([
        Constraint::Length(1),  // header
        Constraint::Fill(1),    // list
        Constraint::Length(1),  // status
        Constraint::Length(1),  // models
        Constraint::Length(1),  // hint
    ])
    .split(area);

    render_select_list(frame, area, app, layout);
}

fn render_select_expanded(frame: &mut Frame, area: Rect, app: &App) {
    let list_rows = (app.backends.len() as u16).min(10).max(3);
    let layout = Layout::vertical([
        Constraint::Length(1),         // header
        Constraint::Length(list_rows), // list
        Constraint::Fill(1),           // detail
        Constraint::Length(1),         // hint
    ])
    .split(area);

    render_select_list(frame, area, app, layout.clone());

    // Detail panel
    render_detail_panel(frame, layout[2], app);
}

fn render_select_list(frame: &mut Frame, _area: Rect, app: &App, layout: std::rc::Rc<[Rect]>) {
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

    // Status + models (only in normal mode, layout.len() == 5)
    if layout.len() >= 5 && !app.backends.is_empty() {
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

        let models_text = match &app.backend_status[app.selected] {
            CheckStatus::Reachable { models } if !models.is_empty() => {
                format!("Models: {}", models.join(", "))
            }
            CheckStatus::Reachable { .. } => "API reachable, no model list returned".into(),
            _ => String::new(),
        };
        let models_paragraph = Paragraph::new(models_text)
            .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM));
        frame.render_widget(models_paragraph, layout[3]);
    }

    // Hint
    let hint_text = if layout.len() >= 5 {
        "↑/↓ Select  Tab Expand  Enter Confirm  d Delete  r Check  ←/→ Tab  q/Esc Quit"
    } else {
        "↑/↓ Select  Tab Collapse  Enter Confirm  d Delete  r Check  ←/→ Tab  q/Esc Quit"
    };
    let hint = Paragraph::new(hint_text)
        .style(Style::default().add_modifier(Modifier::DIM));
    frame.render_widget(hint, layout[layout.len() - 1]);
}

// ---------------------------------------------------------------------------
// Detail panel (expanded backend view)
// ---------------------------------------------------------------------------

fn render_detail_panel(frame: &mut Frame, area: Rect, app: &App) {
    let backend = &app.backends[app.selected];
    let dim = Style::default().add_modifier(Modifier::DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let cyan = Style::default().fg(Color::Cyan);

    let block = Block::bordered()
        .title(format!(" {} ", backend.name))
        .title_style(bold);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();

    // Model list (always visible at top)
    let models_text = match &app.backend_status[app.selected] {
        CheckStatus::Reachable { models } if !models.is_empty() => {
            format!("Models: {}", models.join(", "))
        }
        CheckStatus::Reachable { .. } => "Models: (none)".into(),
        CheckStatus::InProgress => "Models: checking...".into(),
        CheckStatus::Pending => "Models: waiting...".into(),
        CheckStatus::Unreachable { error } => format!("Unreachable: {}", error),
        CheckStatus::Skipped { reason } => reason.clone(),
    };
    lines.push(Line::from(vec![Span::styled(models_text, cyan.add_modifier(Modifier::BOLD))]));

    // Env vars
    let mut keys: Vec<&String> = backend.env.keys().collect();
    keys.sort();
    for key in keys {
        let value = &backend.env[key];
        let display = if key.contains("KEY") || key.contains("TOKEN") {
            if value.len() > 10 {
                format!("{}...{}", &value[..6], &value[value.len()-4..])
            } else {
                "***".to_string()
            }
        } else {
            value.clone()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {} = ", key), dim),
            Span::raw(display),
        ]));
    }

    // ChatGPT OAuth account info (best-effort, loaded on refresh)
    if let Some(info) = app.oauth_infos.get(app.selected).and_then(|o| o.as_ref()) {
        lines.push(Line::from(vec![Span::styled(format!("  {}", info), dim)]));
    }

    // Token usage (loaded async via background thread)
    lines.push(Line::raw(""));
    match &app.backend_stats {
        None => {
            lines.push(Line::from(vec![Span::styled(
                " Token usage: scanning...",
                dim,
            )]));
        }
        Some(all_stats) if all_stats.is_empty() => {
            lines.push(Line::from(vec![Span::styled(
                " No token usage data found.",
                dim,
            )]));
            lines.push(Line::from(vec![Span::styled(
                " Start a Claude Code session to collect usage data.",
                Style::default().add_modifier(Modifier::DIM),
            )]));
        }
        Some(all_stats) => {
            let stats = all_stats
                .iter()
                .find(|s| s.backend == backend.name)
                .or_else(|| all_stats.iter().find(|s| s.backend == "(unattributed)"));
            match stats {
                Some(s) if !s.models.is_empty() => {
                    lines.push(Line::from(vec![Span::styled(
                        "── Token Usage ──",
                        Style::default().fg(Color::Green),
                    )]));
                    for m in &s.models {
                        let model_label = if m.model.len() > 28 {
                            format!("{}..", &m.model[..26])
                        } else {
                            m.model.clone()
                        };
                        lines.push(Line::from(vec![
                            Span::styled(format!("  {} ", model_label), cyan),
                            Span::raw(format!(
                                "{} in / {} out / {} cache / {} total",
                                tracker::fmt_count(m.input_tokens),
                                tracker::fmt_count(m.output_tokens),
                                tracker::fmt_count(m.cache_read),
                                tracker::fmt_count(m.total),
                            )),
                        ]));
                    }
                    lines.push(Line::from(vec![
                        Span::styled("  Total  ", bold),
                        Span::raw(format!(
                            "{} in / {} out / {} cache / {} total",
                            tracker::fmt_count(s.total_input),
                            tracker::fmt_count(s.total_output),
                            tracker::fmt_count(s.total_cache_read),
                            tracker::fmt_count(s.grand_total),
                        )),
                    ]));
                }
                Some(_) => {
                    lines.push(Line::from(vec![Span::styled(
                        " No model usage data for this backend yet.",
                        dim,
                    )]));
                }
                None => {
                    lines.push(Line::from(vec![Span::styled(
                        " No token usage data for this backend.",
                        dim,
                    )]));
                }
            }
        }
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

// ---------------------------------------------------------------------------
// Create form view
// ---------------------------------------------------------------------------

fn render_create(frame: &mut Frame, area: Rect, app: &App) {
    let label_style = Style::default().add_modifier(Modifier::BOLD);
    let active_style = Style::default().add_modifier(Modifier::REVERSED);
    let inactive_style = Style::default();
    let cursor_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);

    // Helper to render a labelled input field
    let field = |label: &str, value: &str, idx: usize| -> Line<'static> {
        let style = if idx == app.create_active_field {
            active_style
        } else {
            inactive_style
        };
        let mut spans = vec![Span::styled(label.to_string(), label_style)];
        spans.push(Span::styled(value.to_string(), style));
        if idx == app.create_active_field {
            spans.push(Span::styled("█", cursor_style));
        }
        Line::from(spans)
    };

    // Auth type selector: a focusable row (Tab to it, ←/→ to change it)
    let auth_focused = app.create_active_field == app.auth_field_index();
    let dim = Style::default().add_modifier(Modifier::DIM);
    let (api_style, oauth_style) = match app.create_auth_type {
        CreateAuthType::ApiKey => (active_style, inactive_style),
        CreateAuthType::ChatgptOauth => (inactive_style, active_style),
    };
    let mut auth_spans = vec![
        Span::styled(" Auth:      [ ", label_style),
        Span::styled("API Key", api_style),
        Span::styled(" | ", if auth_focused { active_style } else { dim }),
        Span::styled("ChatGPT OAuth", oauth_style),
        Span::styled(" ]", if auth_focused { active_style } else { Style::default() }),
    ];
    auth_spans.push(Span::styled(
        if auth_focused { "  ←/→ change" } else { "  (Tab here to change)" },
        dim,
    ));
    let auth_row = Line::from(auth_spans);

    let rows = Layout::vertical([
        Constraint::Length(1),  // name
        Constraint::Length(1),  // auth selector
        Constraint::Length(1),  // field: base url (api) / description (oauth)
        Constraint::Length(1),  // field: api key (api) / blank
        Constraint::Length(1),  // field: description (api) / blank
        Constraint::Length(1),  // spacer
        Constraint::Length(1),  // status
        Constraint::Length(1),  // spacer
        Constraint::Length(1),  // hint
    ])
    .split(area);

    frame.render_widget(Paragraph::new(field(" Name:       ", &app.create_name, 0)), rows[0]);
    frame.render_widget(Paragraph::new(auth_row), rows[1]);

    match app.create_auth_type {
        CreateAuthType::ApiKey => {
            frame.render_widget(Paragraph::new(field(" Base URL:   ", &app.create_base_url, 1)), rows[2]);
            frame.render_widget(Paragraph::new(field(" API Key:    ", &app.create_api_key, 2)), rows[3]);
            frame.render_widget(Paragraph::new(field(" Description:", &app.create_description, 3)), rows[4]);
        }
        CreateAuthType::ChatgptOauth => {
            frame.render_widget(Paragraph::new(field(" Description:", &app.create_description, 1)), rows[2]);
        }
    }

    // Status message
    if app.oauth_in_progress {
        let status = Paragraph::new("Waiting for browser login… complete it in the browser (120s timeout)")
            .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
        frame.render_widget(status, rows[6]);
    } else if let Some(ref msg) = app.create_status {
        let color = if app.create_status_is_error {
            Color::Red
        } else {
            Color::Green
        };
        let status = Paragraph::new(msg.as_str())
            .style(Style::default().fg(color).add_modifier(Modifier::BOLD));
        frame.render_widget(status, rows[6]);
    } else if app.create_auth_type == CreateAuthType::ChatgptOauth {
        // ToS warning shown whenever the OAuth type is selected
        let warn = Paragraph::new("Unofficial ChatGPT-subscription use — may be throttled or revoked (ToS risk)")
            .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::DIM));
        frame.render_widget(warn, rows[6]);
    }

    // Hint
    let hint_text = if auth_focused {
        "←/→ Change Auth  Tab/↓ Next  ↑ Prev  Enter Save  Esc Quit"
    } else {
        "Tab/↓ Next  ↑ Prev  Enter Save  ←/→ Switch Tab  q/Esc Quit"
    };
    let hint = Paragraph::new(hint_text).style(Style::default().add_modifier(Modifier::DIM));
    frame.render_widget(hint, rows[8]);
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
