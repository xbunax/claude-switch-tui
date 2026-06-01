# claude-switch

![TUI showcase](showcase/TUI.png)

A TUI tool for quickly switching between Claude Code API backends (Anthropic official, custom gateways, or compatible services like DeepSeek).

## Features

- **Inline overlay TUI** — renders below the cursor without taking over the entire terminal, like fzf
- **Two tabs** — Backend Switcher and Create New Backend, switchable with ←/→
- **Expandable detail panel** — press `Tab` to expand and see environment variables and available models
- **Dynamic height** — dialog resizes to fit expanded content
- **Create backends** — fill in name, base URL, API key, and description directly in the TUI; saved as `.env` files
- **Delete backends** — remove unwanted backends with `d` (confirmation required)
- **API reachability check** — each backend is probed on startup to verify connectivity, with results shown inline (✓ reachable, ✗ unreachable)
- **Model discovery** — automatically fetches available models via Anthropic, OpenAI-compatible, and DeepSeek API patterns
- **Shell integration** — one-time setup gives you a `cs` command that switches backends inline and auto-sources the environment into your current shell

## Installation

### Homebrew (macOS)

```bash
brew tap xbunax/tap
brew install claude-switch
```

### Manual (build from source)

```bash
git clone https://github.com/xbunax/claude-switch-tui.git
cd claude-switch-tui
cargo build --release
cp target/release/claude-switch ~/.local/bin/
```

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

# Switch backends (inline TUI, auto-activates selection)
cs
```

The `cs` function runs `claude-switch` directly (inline TUI on stdout, no command substitution), then sources the generated env file so environment variables update in the current shell immediately.

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
| `Tab` | Expand/collapse detail panel (env vars + models) |
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
| `--eval` | Output bare `export` statements for shell eval; TUI renders in fullscreen on stderr |
| `--shell-init` | Print the `cs` shell function for `.zshrc` / `.bashrc` |

## How it works

1. Backends are loaded from `.env` files in `~/.config/claude-switch/`
2. The TUI appears inline below the cursor with live reachability status and model counts
3. Press `Tab` to expand the selected backend and inspect its environment variables
4. Backends can be created or deleted directly in the TUI; the config directory is kept in sync
5. On selection, the backend's environment variables are written to `claude.env`
6. The `cs` shell function sources that file after the TUI exits, updating the current shell

## Development

```bash
cargo build
cargo test
```
