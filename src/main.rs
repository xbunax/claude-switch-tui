mod app;
mod checker;
mod config;
mod ui;

use clap::Parser;
use config::{app_dir, default_env_path, discover_backends, expand_path, init_config, shell_quote, write_env_file};
use std::process;

/// Switch Claude Code backend environment variables via TUI
///
/// Backends are discovered from *.env files in $XDG_CONFIG_HOME/claude-switch/
/// and from backends.json (both formats coexist).
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Output env file path, default $XDG_CONFIG_HOME/claude-switch/claude.env
    #[arg(short = 'o', long)]
    output: Option<String>,

    /// Create example configuration under $XDG_CONFIG_HOME/claude-switch/
    #[arg(long)]
    init: bool,

    /// Output export statements for eval (status goes to stderr)
    /// Usage: eval "$(claude-switch --eval)"
    #[arg(long)]
    eval: bool,

    /// Print shell function for .zshrc/.bashrc, then use cs command directly
    /// Usage: claude-switch --shell-init >> ~/.zshrc && source ~/.zshrc
    #[arg(long)]
    shell_init: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // --shell-init: print shell function and exit, no TUI needed
    if cli.shell_init {
        print_shell_init();
        return Ok(());
    }

    let config_dir = app_dir();
    let env_file = cli
        .output
        .as_deref()
        .map(expand_path)
        .unwrap_or_else(default_env_path);

    if cli.init {
        init_config(&config_dir)?;
        eprintln!();
        eprintln!("Shell integration (one-time setup, then use cs command directly):");
        eprintln!(
            "  {} --shell-init >> ~/.zshrc && source ~/.zshrc",
            current_exe_name()
        );
        return Ok(());
    }

    let backends = discover_backends(&config_dir)?;

    // In --eval mode the TUI renders to stderr so stdout stays clean for eval
    let mut app_state = app::App::new(backends);
    let confirmed = app::run_app(&mut app_state, cli.eval)?;

    if !confirmed {
        eprintln!("Cancelled");
        process::exit(1);
    }

    let backend = app_state.selected_backend();
    write_env_file(&env_file, backend)?;

    if cli.eval {
        // stdout: only export statements (consumed by eval)
        // stderr: human-readable status
        eprintln!("Switched to: {}", backend.name);
        let mut keys: Vec<&String> = backend.env.keys().collect();
        keys.sort();
        for key in keys {
            println!("export {}={}", key, shell_quote(&backend.env[key]));
        }
    } else {
        println!("Switched to: {}", backend.name);
        println!("Environment file: {}", env_file.display());
        println!(
            "Activate in current shell: source {}",
            shell_quote(&env_file.display().to_string())
        );
        println!();
        println!(
            "Tip: run {} --shell-init >> ~/.zshrc to set up the cs command for instant switching",
            current_exe_name()
        );
    }

    Ok(())
}

fn current_exe_name() -> String {
    std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "claude-switch".into())
}

fn print_shell_init() {
    let exe = current_exe_name();
    println!(
        r#"# Claude Switch — auto-activates the selected backend
# Usage: cs
cs() {{
  eval "$({exe} --eval "$@")"
}}"#,
        exe = exe
    );
}
