#!/usr/bin/env bash

BINARY="${HOME}/.cargo/bin/supervillain"
PORT=8000
URL="http://127.0.0.1:${PORT}"
LOG_FILE="${TMPDIR:-/tmp}/supervillain.log"

port_listening() {
    /usr/sbin/lsof -i ":${PORT}" -sTCP:LISTEN &>/dev/null
}

# Hand the URL to the user's default browser. Plain `open URL` honors
# the macOS LaunchServices default — respecting that is worth more than
# the chromeless Chrome/Edge `--app=` window we used to force.
open_webapp() {
    open "$URL"
}

# Tests source this file to exercise functions in isolation; skip the
# main flow when that flag is set.
[[ "${SUPERVILLAIN_LAUNCHER_SOURCE_ONLY:-0}" == "1" ]] && return 0

# Best-effort: pull latest source and rebuild the binary if the repo is
# behind upstream. Failures are non-fatal — we still launch what we have.
REPO_STAMP="${XDG_CONFIG_HOME:-$HOME/.config}/supervillain/repo"
if [[ -f "$REPO_STAMP" ]]; then
    REPO_DIR="$(cat "$REPO_STAMP")"
    if [[ -f "$REPO_DIR/scripts/check-and-update.sh" ]]; then
        # shellcheck disable=SC1091
        source "$REPO_DIR/scripts/check-and-update.sh"
        check_and_update || true
    fi
fi

if [[ ! -x "$BINARY" ]]; then
    osascript -e 'display alert "Supervillain not found" message "Run: cargo install --path /path/to/supervillain-rs" as critical'
    exit 1
fi

# If already running, just open a webapp window
if port_listening; then
    open_webapp
    exit 0
fi

# Start the server
"$BINARY" --no-browser > "$LOG_FILE" 2>&1 &
SERVER_PID=$!

# Wait for server to be ready
for _ in $(seq 1 30); do
    # Check the process is still alive
    if ! kill -0 $SERVER_PID 2>/dev/null; then
        osascript -e 'display alert "Supervillain crashed on startup" message "Check '"$LOG_FILE"' for details." as critical'
        exit 1
    fi
    if port_listening; then
        open_webapp
        wait $SERVER_PID
        exit 0
    fi
    sleep 0.5
done

osascript -e 'display alert "Supervillain failed to start" message "Check '"$LOG_FILE"' for details." as critical'
kill $SERVER_PID 2>/dev/null || true
exit 1
