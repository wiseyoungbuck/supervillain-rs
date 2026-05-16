#!/usr/bin/env bash
# Linux/Omarchy launcher for Supervillain.
# Symlink or point your Omarchy keybind / .desktop entry at this script.

set -euo pipefail

BINARY="${HOME}/.cargo/bin/supervillain"
PORT=8000
URL="http://127.0.0.1:${PORT}"
LOG_FILE="${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}/supervillain.log"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

port_listening() {
    if command -v ss &>/dev/null; then
        ss -tln 2>/dev/null | grep -q ":${PORT} "
    else
        lsof -i ":${PORT}" -sTCP:LISTEN &>/dev/null
    fi
}

# Open the URL using the user's default browser. On Omarchy we honor the
# omarchy-launch-webapp wrapper so the link lands in their preferred
# webapp surface; everywhere else, xdg-open consults the system default.
open_webapp() {
    if [[ -d "$HOME/.local/share/omarchy" ]] && command -v omarchy-launch-webapp &>/dev/null; then
        omarchy-launch-webapp "$URL"
    else
        xdg-open "$URL"
    fi
}

[[ "${SUPERVILLAIN_LAUNCHER_LINUX_SOURCE_ONLY:-0}" == "1" ]] && return 0

# Best-effort update check. Failures don't block startup.
if [[ -f "$SCRIPT_DIR/check-and-update.sh" ]]; then
    # shellcheck disable=SC1091
    SUPERVILLAIN_REPO_DIR="$REPO_DIR" source "$SCRIPT_DIR/check-and-update.sh"
    SUPERVILLAIN_REPO_DIR="$REPO_DIR" check_and_update || true
fi

if [[ ! -x "$BINARY" ]]; then
    echo "Error: supervillain not found at $BINARY" >&2
    echo "Build it: cargo install --path $REPO_DIR" >&2
    exit 1
fi

# Already running — just open a window.
if port_listening; then
    open_webapp
    exit 0
fi

nohup "$BINARY" --no-browser > "$LOG_FILE" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 30); do
    if ! kill -0 $SERVER_PID 2>/dev/null; then
        echo "Error: supervillain crashed at startup. Check $LOG_FILE" >&2
        exit 1
    fi
    if port_listening; then
        open_webapp
        wait $SERVER_PID
        exit 0
    fi
    sleep 0.5
done

echo "Error: supervillain failed to start within 15s. Check $LOG_FILE" >&2
kill $SERVER_PID 2>/dev/null || true
exit 1
