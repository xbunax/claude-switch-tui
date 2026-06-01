# claude-switch

A TUI tool for quickly switching between Claude Code API backends (Anthropic official, custom gateways, or compatible services like DeepSeek).

## Features

- **Interactive TUI** — browse and select backends with keyboard navigation (↑/↓/j/k, Enter to confirm, q to quit)
- **API reachability check** — each backend is probed on startup to verify connectivity, with results shown inline (✓ reachable, ✗ unreachable)
- **Model discovery** — automatically fetches available models via Anthropic, OpenAI-compatible, and DeepSeek API patterns
- **Shell integration** — one-time setup gives you a `cs` command that switches backends and auto-exports the environment into your current shell

## Quick start

```bash
# Build & install
./install.sh

# Create example config files
claude-switch --init

# Set up the cs shell command (one-time)
claude-switch --shell-init >> ~/.zshrc && source ~/.zshrc

# Switch backends
cs
```

## Configuration

Backends are discovered from `*.env` files in `$XDG_CONFIG_HOME/claude-switch/` (falls back to `~/.config/claude-switch/`). The filename (minus `.env`) becomes the backend name.

```bash
# ~/.config/claude-switch/anthropic.env
ANTHROPIC_BASE_URL=https://api.anthropic.com
ANTHROPIC_API_KEY=sk-ant-xxx
```

`export` prefixes and quoted values are handled automatically. Both `ANTHROPIC_API_KEY` and `ANTHROPIC_AUTH_TOKEN` are recognized as API key fields.

## Usage

```
claude-switch [OPTIONS]
```

| Flag | Description |
|---|---|
| `--init` | Create example .env files in the config directory |
| `-o, --output <PATH>` | Write env file to a custom path (default: `~/.config/claude-switch/claude.env`) |
| `--eval` | Output bare `export` statements for shell eval (TUI renders to stderr) |
| `--shell-init` | Print the `cs` shell function for `.zshrc` / `.bashrc` |

## How it works

1. Backends are loaded from `.env` files in the config directory
2. The TUI displays all backends and concurrently checks API reachability for each
3. On selection, the backend's environment variables are written to `claude.env`
4. With `--eval`, the variables are printed as `export` statements — the `cs` shell function `eval`s this output so the environment updates in the current shell immediately

## Build from source

```bash
cargo build --release
# binary at target/release/claude-switch
```
