#!/usr/bin/env bash
# Cross-platform launcher for Supervillain. Detects the host OS at runtime
# and routes through `open` (macOS) or `omarchy-launch-or-focus-webapp` /
# `xdg-open` (Linux). Used by the macOS .app bundle (install-macos-app.sh)
# and the Linux .desktop entry (install-omarchy.sh).

set -euo pipefail

# The binary defaults to loopback (no auth layer); this launcher opts in
# to LAN/tailnet reachability. Override by exporting SUPERVILLAIN_BIND.
# PORT derives from it so the port checks track a custom bind address.
export SUPERVILLAIN_BIND="${SUPERVILLAIN_BIND:-0.0.0.0:8000}"
PORT="${SUPERVILLAIN_BIND##*:}"
URL="http://127.0.0.1:${PORT}"
LOG_FILE="${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}/supervillain.log"

port_listening() {
    if command -v ss &>/dev/null; then
        ss -tln 2>/dev/null | grep -q ":${PORT} "
    else
        lsof -i ":${PORT}" -sTCP:LISTEN &>/dev/null
    fi
}

# Hand the URL to the user's preferred webapp surface. On macOS that's the
# LaunchServices default via `open`. On Omarchy we honor the
# launch-or-focus wrapper so a second invocation focuses the existing
# window instead of opening a duplicate. Elsewhere on Linux, xdg-open
# consults the system default.
open_webapp() {
    if [[ "$OSTYPE" == darwin* ]]; then
        open "$URL"
    elif command -v omarchy-launch-or-focus-webapp &>/dev/null; then
        omarchy-launch-or-focus-webapp "127.0.0.1:${PORT}" "$URL"
    else
        xdg-open "$URL"
    fi
}

# Surface a failure to the user via a native notification, falling back to
# stderr when no GUI channel is available.
notify_error() {
    local msg="$1"
    if [[ "$OSTYPE" == darwin* ]]; then
        osascript -e "display alert \"Supervillain\" message \"${msg}\" as critical" &>/dev/null || true
    elif command -v notify-send &>/dev/null; then
        notify-send "Supervillain" "$msg" &>/dev/null || true
    fi
    echo "Supervillain: $msg" >&2
}

# Tests source this file to exercise functions in isolation; either flag
# short-circuits before the main flow. Both names are kept for symmetry
# with the historical macOS / Linux launcher pair.
if [[ "${SUPERVILLAIN_LAUNCHER_SOURCE_ONLY:-0}" == "1" ]] || [[ "${SUPERVILLAIN_LAUNCHER_LINUX_SOURCE_ONLY:-0}" == "1" ]]; then
    return 0
fi

# Best-effort: pull latest source and rebuild if behind upstream. The
# installer writes a stamp file pointing at the repo; if it's absent we
# skip the check.
REPO_STAMP="${XDG_CONFIG_HOME:-$HOME/.config}/supervillain/repo"
if [[ -f "$REPO_STAMP" ]]; then
    REPO_DIR_FROM_STAMP="$(cat "$REPO_STAMP")"
    if [[ -f "$REPO_DIR_FROM_STAMP/scripts/check-and-update.sh" ]]; then
        # shellcheck disable=SC1091
        SUPERVILLAIN_REPO_DIR="$REPO_DIR_FROM_STAMP" source "$REPO_DIR_FROM_STAMP/scripts/check-and-update.sh"
        SUPERVILLAIN_REPO_DIR="$REPO_DIR_FROM_STAMP" check_and_update || true
    fi
fi

if ! BIN="$(command -v supervillain 2>/dev/null)"; then
    notify_error "supervillain binary not found on PATH — run: cargo install --path ~/scripture/supervillain"
    exit 1
fi

if port_listening; then
    open_webapp
    exit 0
fi

nohup "$BIN" --no-browser > "$LOG_FILE" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 30); do
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        notify_error "supervillain crashed at startup — check $LOG_FILE"
        exit 1
    fi
    if port_listening; then
        open_webapp
        wait "$SERVER_PID"
        exit 0
    fi
    sleep 0.5
done

notify_error "supervillain failed to start within 15s — check $LOG_FILE"
kill "$SERVER_PID" 2>/dev/null || true
exit 1
