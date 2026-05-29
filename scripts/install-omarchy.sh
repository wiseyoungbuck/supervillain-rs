#!/usr/bin/env bash
# Install Supervillain as an Omarchy/Linux desktop app: builds the binary,
# drops the launcher into ~/.local/bin, registers a .desktop entry as the
# mailto: handler, and stamps the repo path so the launcher can auto-update.
set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"

cargo install --path "$REPO_DIR"

mkdir -p "$HOME/.local/bin"
cp "$REPO_DIR/scripts/supervillain-launcher.sh" "$HOME/.local/bin/supervillain-launcher"
chmod +x "$HOME/.local/bin/supervillain-launcher"

ICONS_DIR="$HOME/.local/share/applications/icons"
mkdir -p "$ICONS_DIR"
cp "$REPO_DIR/static/icon-512.png" "$ICONS_DIR/Supervillain.png"

# Stamp the repo path so check-and-update.sh can rebuild on launch when
# upstream moves ahead (mirrors install-macos-app.sh).
STAMP_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/supervillain"
mkdir -p "$STAMP_DIR"
printf '%s\n' "$REPO_DIR" > "$STAMP_DIR/repo"

cat > "$HOME/.local/share/applications/Supervillain.desktop" << EOF
[Desktop Entry]
Version=1.0
Name=Supervillain
Comment=Supervillain
Exec=supervillain-launcher
Terminal=false
Type=Application
Icon=$HOME/.local/share/applications/icons/Supervillain.png
StartupNotify=true
MimeType=x-scheme-handler/mailto;
EOF
