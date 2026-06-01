use crate::checker::{self, CheckResult, CheckStatus};
use crate::config::{discover_backends, save_backend_env, Backend};
use crate::ui;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use std::io::{self, Write};
use std::sync::mpsc;
use std::time::Duration;

/// Which screen is currently active.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Select,
    Create,
}

/// Pending confirmation action.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ConfirmAction {
    None,
    DeleteBackend,
    SaveBackend,
}

/// Application state for the TUI.
pub struct App {
    pub mode: Mode,
    pub backends: Vec<Backend>,
    pub selected: usize,
    pub should_quit: bool,
    pub confirmed: bool,

    pub backend_status: Vec<CheckStatus>,
    check_rx: Option<mpsc::Receiver<CheckResult>>,

    // Create-form fields
    pub create_name: String,
    pub create_base_url: String,
    pub create_api_key: String,
    pub create_description: String,
    pub create_active_field: usize,
    pub create_status: Option<String>,
    pub create_status_is_error: bool,

    pub confirm_action: ConfirmAction,
}

impl App {
    pub fn new(backends: Vec<Backend>) -> Self {
        let count = backends.len();
        Self {
            mode: Mode::Select,
            backends,
            selected: 0,
            should_quit: false,
            confirmed: false,
            backend_status: vec![CheckStatus::Pending; count],
            check_rx: None,
            create_name: String::new(),
            create_base_url: String::new(),
            create_api_key: String::new(),
            create_description: String::new(),
            create_active_field: 0,
            create_status: None,
            create_status_is_error: false,
            confirm_action: ConfirmAction::None,
        }
    }

    pub fn next(&mut self) {
        self.selected = (self.selected + 1) % self.backends.len();
    }

    pub fn previous(&mut self) {
        self.selected = self
            .selected
            .checked_sub(1)
            .unwrap_or(self.backends.len() - 1);
    }

    pub fn confirm(&mut self) {
        self.confirmed = true;
        self.should_quit = true;
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    pub fn selected_backend(&self) -> &Backend {
        &self.backends[self.selected]
    }

    /// Re-discover backends from disk and start fresh checks.
    pub fn refresh_backends(&mut self, config_dir: &std::path::Path) {
        if let Ok(fresh) = discover_backends(config_dir) {
            self.backends = fresh;
            self.selected = self.selected.min(self.backends.len().saturating_sub(1));
            self.backend_status = vec![CheckStatus::Pending; self.backends.len()];
        }
        self.start_checks();
    }

    /// Spawn check threads for all backends.
    pub fn start_checks(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.check_rx = Some(rx);

        self.backend_status = vec![CheckStatus::Pending; self.backends.len()];

        for (i, backend) in self.backends.iter().enumerate() {
            let base_url = backend.env.get("ANTHROPIC_BASE_URL");
            let api_key = backend
                .env
                .get("ANTHROPIC_API_KEY")
                .or_else(|| backend.env.get("ANTHROPIC_AUTH_TOKEN"));

            match (base_url, api_key) {
                (Some(url), Some(key)) => {
                    self.backend_status[i] = CheckStatus::InProgress;
                    checker::spawn_check(i, url.clone(), key.clone(), tx.clone());
                }
                _ => {
                    self.backend_status[i] = CheckStatus::Skipped {
                        reason: "Missing ANTHROPIC_BASE_URL or ANTHROPIC_API_KEY".into(),
                    };
                }
            }
        }
    }

    /// Delete the currently selected backend's .env file from disk.
    pub fn execute_delete(&mut self, config_dir: &std::path::Path) {
        if self.backends.is_empty() {
            return;
        }
        let backend = &self.backends[self.selected];
        // Use the description (file path) to know which file to delete
        let path = std::path::PathBuf::from(&backend.description);
        let _ = std::fs::remove_file(&path);
        self.confirm_action = ConfirmAction::None;
        self.refresh_backends(config_dir);
    }

    /// Drain any completed check results from the channel.
    pub fn poll_checks(&mut self) {
        if let Some(rx) = &self.check_rx {
            while let Ok(result) = rx.try_recv() {
                self.backend_status[result.backend_idx] = result.status;
            }
        }
    }

    // ------------------------------------------------------------------
    // Create-form methods
    // ------------------------------------------------------------------

    fn create_field_mut(&mut self) -> &mut String {
        match self.create_active_field {
            0 => &mut self.create_name,
            1 => &mut self.create_base_url,
            2 => &mut self.create_api_key,
            3 => &mut self.create_description,
            _ => &mut self.create_name,
        }
    }

    pub fn handle_create_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char(c) => self.create_field_mut().push(c),
            KeyCode::Backspace => {
                self.create_field_mut().pop();
            }
            KeyCode::Tab | KeyCode::Down => {
                self.create_active_field = (self.create_active_field + 1) % 4;
            }
            KeyCode::Up => {
                self.create_active_field = (self.create_active_field + 3) % 4;
            }
            _ => {}
        }
        self.create_status = None;
    }

    pub fn reset_create_form(&mut self) {
        self.create_name.clear();
        self.create_base_url.clear();
        self.create_api_key.clear();
        self.create_description.clear();
        self.create_active_field = 0;
        self.create_status = None;
        self.create_status_is_error = false;
    }

    pub fn save_create_form(&mut self, config_dir: &std::path::Path) {
        if self.create_name.trim().is_empty() {
            self.create_status = Some("Name is required".into());
            self.create_status_is_error = true;
            return;
        }

        match save_backend_env(
            config_dir,
            self.create_name.trim(),
            self.create_base_url.trim(),
            self.create_api_key.trim(),
            self.create_description.trim(),
        ) {
            Ok(path) => {
                self.create_status = Some(format!("Saved: {}", path.display()));
                self.create_status_is_error = false;
                self.reset_create_form();
                self.refresh_backends(config_dir);
                self.mode = Mode::Select;
            }
            Err(e) => {
                self.create_status = Some(format!("Error: {}", e));
                self.create_status_is_error = true;
            }
        }
    }
}

/// Run the TUI event loop. Returns true if the user confirmed a selection.
///
/// When `use_stderr` is true (i.e. `--eval` mode), the TUI renders to stderr
/// so that stdout stays clean for the `export` statements consumed by eval.
pub fn run_app(app: &mut App, config_dir: &std::path::Path, use_stderr: bool) -> io::Result<bool> {
    if use_stderr {
        run_on_stderr(app, config_dir)
    } else {
        run_on_stdout(app, config_dir)
    }
}

fn run_on_stdout(app: &mut App, config_dir: &std::path::Path) -> io::Result<bool> {
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    terminal::enable_raw_mode()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let result = event_loop(&mut terminal, app, config_dir);

    terminal::disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;

    result?;
    Ok(app.confirmed)
}

fn run_on_stderr(app: &mut App, config_dir: &std::path::Path) -> io::Result<bool> {
    let mut stderr = io::stderr();
    execute!(stderr, EnterAlternateScreen)?;
    terminal::enable_raw_mode()?;

    let backend = CrosstermBackend::new(io::stderr());
    let mut terminal = Terminal::new(backend)?;
    let result = event_loop(&mut terminal, app, config_dir);

    terminal::disable_raw_mode()?;
    execute!(io::stderr(), LeaveAlternateScreen)?;

    // Flush stderr so the TUI is fully cleared before we write to stdout
    io::stderr().flush()?;

    result?;
    Ok(app.confirmed)
}

fn event_loop<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    app: &mut App,
    config_dir: &std::path::Path,
) -> io::Result<()> {
    app.start_checks();

    while !app.should_quit {
        app.poll_checks();
        terminal.draw(|frame| ui::render(frame, app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    // Shared: confirmation prompt
                    if app.confirm_action != ConfirmAction::None {
                        match key.code {
                            KeyCode::Enter
                            | KeyCode::Char('y')
                            | KeyCode::Char('Y') => match app.confirm_action {
                                ConfirmAction::DeleteBackend => {
                                    app.execute_delete(config_dir);
                                }
                                ConfirmAction::SaveBackend => {
                                    app.save_create_form(config_dir);
                                }
                                ConfirmAction::None => {}
                            },
                            KeyCode::Esc
                            | KeyCode::Char('n')
                            | KeyCode::Char('N') => {
                                app.confirm_action = ConfirmAction::None;
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Shared: tab switching
                    match key.code {
                        KeyCode::Left => {
                            app.mode = Mode::Select;
                            continue;
                        }
                        KeyCode::Right => {
                            app.mode = Mode::Create;
                            continue;
                        }
                        _ => {}
                    }

                    match app.mode {
                        Mode::Select => match key.code {
                            KeyCode::Up | KeyCode::Char('k') => app.previous(),
                            KeyCode::Down | KeyCode::Char('j') => app.next(),
                            KeyCode::Enter => app.confirm(),
                            KeyCode::Char('r') | KeyCode::Char('R') => {
                                app.refresh_backends(config_dir);
                            }
                            KeyCode::Char('d') | KeyCode::Char('D') => {
                                if !app.backends.is_empty() {
                                    app.confirm_action = ConfirmAction::DeleteBackend;
                                }
                            }
                            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => app.quit(),
                            _ => {}
                        },
                        Mode::Create => match key.code {
                            KeyCode::Enter => {
                                if app.create_name.trim().is_empty() {
                                    app.create_status = Some("Name is required".into());
                                    app.create_status_is_error = true;
                                } else {
                                    app.confirm_action = ConfirmAction::SaveBackend;
                                }
                            }
                            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => app.quit(),
                            _ => app.handle_create_key(key.code),
                        },
                    }
                }
            }
        }
    }

    Ok(())
}
