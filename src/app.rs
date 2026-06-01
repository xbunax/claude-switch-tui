use crate::checker::{self, CheckResult, CheckStatus};
use crate::config::Backend;
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

/// Application state for the TUI.
pub struct App {
    pub backends: Vec<Backend>,
    pub selected: usize,
    pub should_quit: bool,
    pub confirmed: bool,

    pub backend_status: Vec<CheckStatus>,
    check_rx: Option<mpsc::Receiver<CheckResult>>,
}

impl App {
    pub fn new(backends: Vec<Backend>) -> Self {
        let count = backends.len();
        Self {
            backends,
            selected: 0,
            should_quit: false,
            confirmed: false,
            backend_status: vec![CheckStatus::Pending; count],
            check_rx: None,
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

    /// Spawn check threads for all backends.
    /// Resets all statuses to Pending and starts new checks.
    pub fn start_checks(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.check_rx = Some(rx);

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

    /// Drain any completed check results from the channel.
    pub fn poll_checks(&mut self) {
        if let Some(rx) = &self.check_rx {
            while let Ok(result) = rx.try_recv() {
                self.backend_status[result.backend_idx] = result.status;
            }
        }
    }
}

/// Run the TUI event loop. Returns true if the user confirmed a selection.
///
/// When `use_stderr` is true (i.e. `--eval` mode), the TUI renders to stderr
/// so that stdout stays clean for the `export` statements consumed by eval.
pub fn run_app(app: &mut App, use_stderr: bool) -> io::Result<bool> {
    if use_stderr {
        run_on_stderr(app)
    } else {
        run_on_stdout(app)
    }
}

fn run_on_stdout(app: &mut App) -> io::Result<bool> {
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    terminal::enable_raw_mode()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let result = event_loop(&mut terminal, app);

    terminal::disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;

    result?;
    Ok(app.confirmed)
}

fn run_on_stderr(app: &mut App) -> io::Result<bool> {
    let mut stderr = io::stderr();
    execute!(stderr, EnterAlternateScreen)?;
    terminal::enable_raw_mode()?;

    let backend = CrosstermBackend::new(io::stderr());
    let mut terminal = Terminal::new(backend)?;
    let result = event_loop(&mut terminal, app);

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
) -> io::Result<()> {
    app.start_checks();

    while !app.should_quit {
        app.poll_checks();
        terminal.draw(|frame| ui::render(frame, app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Up | KeyCode::Char('k') => app.previous(),
                        KeyCode::Down | KeyCode::Char('j') => app.next(),
                        KeyCode::Enter => app.confirm(),
                        KeyCode::Char('r') | KeyCode::Char('R') => app.start_checks(),
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => app.quit(),
                        _ => {}
                    }
                }
            }
        }
    }

    Ok(())
}
