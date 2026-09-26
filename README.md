# claude-switch

![TUI showcase](showcase/TUI.png)

A TUI tool for quickly switching between Claude Code API backends (Anthropic official, custom gateways, or compatible services like DeepSeek).

## Features

- **Inline overlay TUI** — renders below the cursor without taking over the entire terminal, like fzf
- **Two tabs** — Backend Switcher and Create New Backend, switchable with ←/→
- **Expandable detail panel** — press `Tab` to expand and see environment variables and available models
- **Dynamic height** — dialog resizes to fit expanded content
- **Create backends** — fill in name, base URL, API key, and description directly in the TUI; saved as `.env` files
- **ChatGPT (Codex OAuth) backends** — log in with your ChatGPT account in the browser and use GPT models in Claude Code through a built-in translation proxy (see below)
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

## ChatGPT (Codex OAuth) backends

> **Warning** — this is an unofficial use of a ChatGPT subscription. It borrows the OAuth client and backend API that the Codex CLI uses, and may violate OpenAI's and/or Anthropic's terms of service. Access can be throttled, rate-limited, or revoked at any time. Not affiliated with OpenAI or Anthropic.

Claude Code speaks the Anthropic Messages API, while ChatGPT's consumer backend speaks the OpenAI Responses API. claude-switch bridges the two with a small translation proxy it runs locally.

### Setup

1. Press `→` to open the Create tab.
2. Type a name, then `Tab` to the last row (Auth) and press `←`/`→` to select **ChatGPT OAuth** (the Base URL and API Key fields disappear).
3. Press `Enter`, confirm with `y` — your browser opens the ChatGPT login page.
4. After you authorize, the credentials are stored and the backend appears in the list.

Then select the backend with `cs` as usual. claude-switch starts the proxy automatically and points `ANTHROPIC_BASE_URL` at it; switching to any other backend stops the proxy.

### What gets written

```
# ~/.config/claude-switch/gpt.env
CS_BACKEND_KIND=chatgpt-oauth
ANTHROPIC_BASE_URL=http://127.0.0.1:18765
ANTHROPIC_AUTH_TOKEN=<random local token — the proxy ignores its value>
ANTHROPIC_MODEL=gpt-6-astra
CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
ANTHROPIC_DEFAULT_OPUS_MODEL=gpt-6-astra
ANTHROPIC_DEFAULT_SONNET_MODEL=gpt-6-sol
ANTHROPIC_DEFAULT_HAIKU_MODEL=gpt-5.5
ANTHROPIC_SMALL_FAST_MODEL=gpt-5.5
CLAUDE_CODE_SUBAGENT_MODEL=gpt-6-sol
CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK=1
CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1
CLAUDE_CODE_ATTRIBUTION_HEADER=0
CLAUDE_CODE_AUTO_COMPACT_WINDOW=272000
```

OAuth tokens live in a sibling `gpt.oauth.json` (mode `0600`) and are refreshed automatically; `CS_*` keys are internal and never reach `claude.env` or your shell.

### Notes

- **Models** — at login the account's real model catalog is fetched, and the proxy serves it from `GET /v1/models` so Claude Code's `/model` picker can list every entry. Two things make that work, and both are written into the `.env` for you:
  - `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` — gateway model discovery is off by default, so the picker would otherwise ignore `/v1/models` entirely.
  - The proxy advertises ids as `claude-<upstream-id>`. Discovery silently drops any entry whose id contains neither `claude` nor `anthropic`, so the alias prefix is load-bearing, not cosmetic. It's stripped again before the request goes upstream.
  - The `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL` keys retarget Claude Code's built-in rows at GPT models; they don't add rows. Without them those rows would ask for `claude-opus-5` and land on the main model.
- **Model names change often** and upstream renames them wholesale — nothing here keys off a name pattern. The ids live in the `.env` file, so override them by hand if you like. Credentials written before the catalog existed get it backfilled on the proxy's first start, and a backend's `.env` is backfilled with any of the keys above that it's missing (existing values are never rewritten). To see why a catalog fetch failed, run the proxy by hand with `CS_CATALOG_DEBUG=1`; for which endpoints Claude Code calls, use `CS_PROXY_DEBUG=1`.
- **Port** — the proxy listens on `127.0.0.1:18765`. If that port is busy it picks the next free one and rewrites `ANTHROPIC_BASE_URL` accordingly. Set `CS_PROXY_PORT` in the `.env` to choose a different starting port.
- **Logs** — the daemon logs to `~/.config/claude-switch/.serve-<name>.log`; its PID file is `.serve-<name>.pid`.
- **Login expired** — if the ChatGPT refresh token stops working, Claude Code reports an authentication error; delete the backend and create it again to log in afresh.
- **Tool calling, streaming, and images** are translated; extended thinking blocks are not forwarded in this version.

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
| `Tab` / `↓` | Next field (the last row is the Auth selector) |
| `↑` | Previous field |
| `←` / `→` | On the Auth row: switch between API Key and ChatGPT OAuth |
| `Enter` | Save new backend (with confirmation) / start ChatGPT login |
| `q` / `Esc` | Quit (cancels an in-progress login) |

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
| `--login-oauth <NAME>` | Run the ChatGPT OAuth browser login headlessly for a backend (advanced) |
| `--serve --backend <PATH>` | Run the translation proxy daemon for one backend (advanced; normally spawned automatically) |

## How it works

1. Backends are loaded from `.env` files in `~/.config/claude-switch/`
2. The TUI appears inline below the cursor with live reachability status and model counts
3. Press `Tab` to expand the selected backend and inspect its environment variables
4. Backends can be created or deleted directly in the TUI; the config directory is kept in sync
5. On selection, the backend's environment variables are written to `claude.env`. ChatGPT OAuth backends also get their local translation proxy started (and any other proxy stopped)
6. The `cs` shell function sources that file after the TUI exits, updating the current shell

## Development

```bash
cargo build
cargo test
```
