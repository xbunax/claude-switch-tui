use crate::checker::{self, CheckResult, CheckStatus};
use crate::config::{self, discover_backends, save_backend_env, Backend};
use crate::daemon;
use crate::oauth::{self, OauthOutcome};
use crate::tracker::BackendStats;
use crate::ui;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{prelude::*, TerminalOptions, Viewport};
use std::io::{self, Write};
use std::sync::mpsc;
use std::thread;
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

/// Auth method for the create form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateAuthType {
    ApiKey,
    ChatgptOauth,
}

/// Application state for the TUI.
pub struct App {
    pub mode: Mode,
    pub backends: Vec<Backend>,
    pub selected: usize,
    pub should_quit: bool,
    pub confirmed: bool,

    pub backend_status: Vec<CheckStatus>,
    pub backend_stats: Option<Vec<BackendStats>>,
    check_rx: Option<mpsc::Receiver<CheckResult>>,
    stats_rx: Option<mpsc::Receiver<Vec<BackendStats>>>,

    // Create-form fields
    pub create_name: String,
    pub create_base_url: String,
    pub create_api_key: String,
    pub create_description: String,
    pub create_active_field: usize,
    pub create_status: Option<String>,
    pub create_status_is_error: bool,
    pub create_auth_type: CreateAuthType,
    pub oauth_in_progress: bool,
    oauth_rx: Option<mpsc::Receiver<OauthOutcome>>,
    oauth_pending_name: String,
    oauth_pending_desc: String,

    /// One-line ChatGPT account info per backend (OAuth backends only).
    pub oauth_infos: Vec<Option<String>>,

    pub confirm_action: ConfirmAction,
    pub expanded: bool,
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
            backend_stats: None,
            check_rx: None,
            stats_rx: None,
            create_name: String::new(),
            create_base_url: String::new(),
            create_api_key: String::new(),
            create_description: String::new(),
            create_active_field: 0,
            create_status: None,
            create_status_is_error: false,
            create_auth_type: CreateAuthType::ApiKey,
            oauth_in_progress: false,
            oauth_rx: None,
            oauth_pending_name: String::new(),
            oauth_pending_desc: String::new(),
            oauth_infos: vec![None; count],
            confirm_action: ConfirmAction::None,
            expanded: false,
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
        self.refresh_oauth_infos(config_dir);
        self.start_checks();
    }

    /// Load one-line ChatGPT account info per OAuth backend (best-effort).
    pub fn refresh_oauth_infos(&mut self, config_dir: &std::path::Path) {
        self.oauth_infos = self
            .backends
            .iter()
            .map(|b| {
                if !config::is_oauth_backend(b) {
                    return None;
                }
                match oauth::load_credentials(config_dir, &b.name) {
                    Some(c) => {
                        let who = c
                            .email
                            .or(c.account_id)
                            .unwrap_or_else(|| "unknown account".into());
                        let plan = c.plan_type.as_deref().unwrap_or("unknown plan");
                        Some(format!("ChatGPT OAuth: {} ({})", who, plan))
                    }
                    None => Some("ChatGPT OAuth: not logged in".to_string()),
                }
            })
            .collect();
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

    /// Spawn a background thread to load token usage stats.
    pub fn start_stats_scan(&mut self, config_dir: &std::path::Path) {
        let (tx, rx) = mpsc::channel();
        self.stats_rx = Some(rx);
        self.backend_stats = None;

        let config_dir = config_dir.to_path_buf();
        thread::spawn(move || {
            let stats = crate::tracker::scan_usage(&config_dir);
            let _ = tx.send(stats);
        });
    }

    /// Drain completed stats result from the channel.
    pub fn poll_stats(&mut self) {
        if let Some(rx) = &self.stats_rx {
            match rx.try_recv() {
                Ok(stats) => self.backend_stats = Some(stats),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.backend_stats = Some(vec![]);
                }
                Err(mpsc::TryRecvError::Empty) => {}
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
        // OAuth backends keep credentials in a sibling file; drop those and
        // any running proxy daemon too.
        let _ = std::fs::remove_file(config::oauth_json_path(config_dir, &backend.name));
        daemon::kill_daemon(config_dir, &backend.name);
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

    /// Number of focusable rows in the create form. The auth selector is the
    /// last one; it holds no text, so `create_field_mut` returns None for it.
    pub fn create_field_count(&self) -> usize {
        match self.create_auth_type {
            CreateAuthType::ApiKey => 5,      // name, base url, api key, description, auth
            CreateAuthType::ChatgptOauth => 3, // name, description, auth
        }
    }

    /// Index of the auth selector row (always last).
    pub fn auth_field_index(&self) -> usize {
        self.create_field_count() - 1
    }

    /// The text buffer of the focused row, or None when the auth row is focused.
    fn create_field_mut(&mut self) -> Option<&mut String> {
        match (self.create_auth_type, self.create_active_field) {
            (CreateAuthType::ApiKey, 1) => Some(&mut self.create_base_url),
            (CreateAuthType::ApiKey, 2) => Some(&mut self.create_api_key),
            (CreateAuthType::ApiKey, 3) | (CreateAuthType::ChatgptOauth, 1) => {
                Some(&mut self.create_description)
            }
            // Row 0 is the name in both modes; the auth row accepts no text.
            (_, 0) => Some(&mut self.create_name),
            _ => None,
        }
    }

    /// Flip the auth type (only meaningful while the auth row is focused).
    pub fn toggle_auth_type(&mut self) {
        self.create_auth_type = match self.create_auth_type {
            CreateAuthType::ApiKey => CreateAuthType::ChatgptOauth,
            CreateAuthType::ChatgptOauth => CreateAuthType::ApiKey,
        };
        // The row count changes; park the cursor on a row that still exists.
        self.create_active_field = 0;
    }

    pub fn handle_create_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char(c) => {
                if let Some(field) = self.create_field_mut() {
                    field.push(c);
                }
            }
            KeyCode::Backspace => {
                if let Some(field) = self.create_field_mut() {
                    field.pop();
                }
            }
            KeyCode::Tab | KeyCode::Down => {
                let fields = self.create_field_count();
                self.create_active_field = (self.create_active_field + 1) % fields;
            }
            KeyCode::Up => {
                let fields = self.create_field_count();
                self.create_active_field = (self.create_active_field + fields - 1) % fields;
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
        self.create_auth_type = CreateAuthType::ApiKey;
        self.oauth_in_progress = false;
        self.oauth_rx = None;
        self.oauth_pending_name.clear();
        self.oauth_pending_desc.clear();
    }

    pub fn save_create_form(&mut self, config_dir: &std::path::Path) {
        let name = self.create_name.trim().to_string();
        if name.is_empty() {
            self.create_status = Some("Name is required".into());
            self.create_status_is_error = true;
            return;
        }

        match self.create_auth_type {
            CreateAuthType::ApiKey => match save_backend_env(
                config_dir,
                &name,
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
            },
            CreateAuthType::ChatgptOauth => {
                if self.backends.iter().any(|b| b.name == name) {
                    self.create_status = Some(format!("Backend '{}' already exists", name));
                    self.create_status_is_error = true;
                    return;
                }
                self.oauth_pending_name = name;
                self.oauth_pending_desc = self.create_description.trim().to_string();
                let (rx, _handle) =
                    oauth::start_login(config_dir.to_path_buf(), self.oauth_pending_name.clone());
                self.oauth_rx = Some(rx);
                self.oauth_in_progress = true;
                self.create_status =
                    Some("Waiting for browser login… (auto-opens; 120s timeout)".into());
                self.create_status_is_error = false;
            }
        }
    }

    /// Drain a completed OAuth login and persist the new backend.
    pub fn poll_oauth(&mut self, config_dir: &std::path::Path) {
        let Some(rx) = &self.oauth_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(OauthOutcome::Success { roles, .. }) => {
                self.oauth_rx = None;
                self.oauth_in_progress = false;
                match config::save_oauth_backend_env(
                    config_dir,
                    &self.oauth_pending_name,
                    &self.oauth_pending_desc,
                    &roles.main,
                    &roles.mid,
                    &roles.small,
                ) {
                    Ok(path) => {
                        self.create_status =
                            Some(format!("Saved: {} (ChatGPT OAuth)", path.display()));
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
            Ok(OauthOutcome::Error { message }) => {
                self.oauth_rx = None;
                self.oauth_in_progress = false;
                self.create_status = Some(format!("Login failed: {}", message));
                self.create_status_is_error = true;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.oauth_rx = None;
                self.oauth_in_progress = false;
                self.create_status = Some("Login failed unexpectedly".into());
                self.create_status_is_error = true;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    /// Abort an in-flight OAuth login from the create form.
    pub fn cancel_oauth(&mut self) {
        // Dropping the receiver unblocks nothing, but the login thread times
        // out on its own; the user can immediately re-trigger the login.
        self.oauth_rx = None;
        self.oauth_in_progress = false;
        self.create_status = Some("Login cancelled".into());
        self.create_status_is_error = true;
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
    terminal::enable_raw_mode()?;

    let height = ui::viewport_height(app, terminal::size()?.1);
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )?;
    let result = event_loop(&mut terminal, app, config_dir);

    // Clear the inline area so the shell prompt sits cleanly below
    let _ = terminal.clear();
    drop(terminal);

    terminal::disable_raw_mode()?;
    result?;
    Ok(app.confirmed)
}

/// `--eval` mode draws on stderr with the alternate screen. The inline
/// viewport used elsewhere is not an option here: it has to query the cursor
/// position, and crossterm sends that query to stdout — which in this mode is
/// the pipe consumed by `eval`, so the query would never be answered.
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
    app.start_stats_scan(config_dir);
    app.refresh_oauth_infos(config_dir);

    while !app.should_quit {
        app.poll_checks();
        app.poll_stats();
        app.poll_oauth(config_dir);
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

                    // In the create form, ←/→ changes the auth selector's
                    // value while that row is focused instead of switching tabs.
                    if app.mode == Mode::Create
                        && !app.oauth_in_progress
                        && app.create_active_field == app.auth_field_index()
                        && matches!(key.code, KeyCode::Left | KeyCode::Right)
                    {
                        app.toggle_auth_type();
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
                            KeyCode::Tab => app.expanded = !app.expanded,
                            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => app.quit(),
                            _ => {}
                        },
                        Mode::Create => {
                            // While the browser login runs, only allow cancelling.
                            if app.oauth_in_progress {
                                match key.code {
                                    KeyCode::Esc
                                    | KeyCode::Char('q')
                                    | KeyCode::Char('Q') => app.cancel_oauth(),
                                    _ => {}
                                }
                                continue;
                            }
                            match key.code {
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
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        App::new(Vec::new())
    }

    #[test]
    fn test_typing_accepts_all_printable_chars() {
        // Regression: 't' used to be swallowed as the auth-type toggle, which
        // made names like "gpt-test" impossible to type.
        let mut app = test_app();
        for c in "gpt-test".chars() {
            app.handle_create_key(KeyCode::Char(c));
        }
        assert_eq!(app.create_name, "gpt-test");

        app.handle_create_key(KeyCode::Backspace);
        assert_eq!(app.create_name, "gpt-tes");
    }

    #[test]
    fn test_tab_reaches_auth_row_and_wraps() {
        let mut app = test_app();
        assert_eq!(app.create_auth_type, CreateAuthType::ApiKey);
        assert_eq!(app.create_field_count(), 5);
        assert_eq!(app.auth_field_index(), 4);

        // Tab four times: name → base url → api key → description → auth
        for _ in 0..4 {
            app.handle_create_key(KeyCode::Tab);
        }
        assert_eq!(app.create_active_field, app.auth_field_index());

        // One more Tab wraps back to the name
        app.handle_create_key(KeyCode::Tab);
        assert_eq!(app.create_active_field, 0);
    }

    #[test]
    fn test_auth_toggle_switches_fields_and_resets_focus() {
        let mut app = test_app();
        app.create_active_field = app.auth_field_index();
        app.toggle_auth_type();
        assert_eq!(app.create_auth_type, CreateAuthType::ChatgptOauth);
        assert_eq!(app.create_field_count(), 3);
        assert_eq!(app.auth_field_index(), 2);
        assert_eq!(app.create_active_field, 0, "focus must land on a valid row");

        app.toggle_auth_type();
        assert_eq!(app.create_auth_type, CreateAuthType::ApiKey);
    }

    #[test]
    fn test_typing_on_auth_row_is_ignored() {
        let mut app = test_app();
        app.create_active_field = app.auth_field_index();
        app.handle_create_key(KeyCode::Char('x'));
        assert!(app.create_name.is_empty());
        assert!(app.create_description.is_empty());
    }

    #[test]
    fn test_up_from_name_wraps_to_auth_row() {
        let mut app = test_app();
        app.handle_create_key(KeyCode::Up);
        assert_eq!(app.create_active_field, app.auth_field_index());
    }

    #[test]
    fn test_oauth_mode_field_mapping() {
        let mut app = test_app();
        app.create_auth_type = CreateAuthType::ChatgptOauth;
        // OAuth mode: name, description, auth
        assert_eq!(app.create_field_count(), 3);
        app.create_active_field = 1;
        app.handle_create_key(KeyCode::Char('d'));
        assert_eq!(app.create_description, "d");
    }
}
