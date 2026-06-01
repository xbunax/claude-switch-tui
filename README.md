# claude-switch

A TUI tool for quickly switching between Claude Code API backends (Anthropic official, custom gateways, or compatible services like DeepSeek).

## Features

- **Interactive TUI** — two tabs: Backend Switcher and Create New Backend, switchable with ←/→
- **Create backends** — fill in name, base URL, API key, and description directly in the TUI; saved as `.env` files
- **Delete backends** — remove unwanted backends with `d` (confirmation required)
- **API reachability check** — each backend is probed on startup to verify connectivity, with results shown inline (✓ reachable, ✗ unreachable)
- **Model discovery** — automatically fetches available models via Anthropic, OpenAI-compatible, and DeepSeek API patterns
- **Shell integration** — one-time setup gives you a `cs` command that switches backends and auto-exports the environment into your current shell

## Installation

### Homebrew (macOS)

```bash
brew install xbunax/tap/claude-switch
```

### Manual (build from source)

```bash
git clone https://github.com/xbunax/claude-switch-tui.git
cd claude-switch-tui
./install.sh
```

`install.sh` compiles the release binary and copies it to `~/.local/bin/claude-switch`.

### Shell setup

Add the `cs` command to your shell rc file:

```bash
claude-switch --shell-init >> ~/.zshrc   # or ~/.bashrc
source ~/.zshrc
```

Then:

```bash
# Create example config files
claude-switch --init

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

## TUI keybindings

| Key | Action |
|---|---|
| `←` / `→` | Switch between Backend Switcher and Create tabs |
| **Backend Switcher** | |
| `↑` `↓` / `j` `k` | Navigate backend list |
| `Enter` | Confirm selection and exit |
| `d` | Delete selected backend (with confirmation) |
| `r` | Refresh backend list and re-check reachability |
| `q` / `Esc` | Quit |
| **Create New Backend** | |
| `Tab` / `↓` | Next field |
| `↑` | Previous field |
| `Enter` | Save new backend (with confirmation) |
| `q` / `Esc` | Quit |

## CLI usage

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

1. Backends are loaded from `.env` files in `~/.config/claude-switch/`
2. The TUI displays all backends with live reachability status and model counts
3. Backends can be created or deleted directly in the TUI; the config directory is kept in sync
4. On selection, the backend's environment variables are written to `claude.env`
5. With `--eval`, the variables are printed as `export` statements — the `cs` shell function `eval`s this output so the environment updates in the current shell immediately

## Development

```bash
cargo build
cargo test
```
