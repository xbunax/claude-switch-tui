#!/usr/bin/env bash
set -euo pipefail

BIN_DIR="$HOME/.local/bin"
BIN_PATH="$BIN_DIR/claude-switch"

echo "==> Building release..."
cargo build --release

echo "==> Installing to $BIN_PATH"
mkdir -p "$BIN_DIR"
cp target/release/claude-switch "$BIN_PATH"
chmod +x "$BIN_PATH"

echo
echo "Installation complete!"
echo
echo "Add the following to ~/.zshrc (or ~/.bashrc):"
echo
echo "  # Add ~/.local/bin to PATH (if not already added)"
echo "  export PATH=\"$BIN_DIR:\$PATH\""
echo "  # Register cs command"
"$BIN_PATH" --shell-init
echo
