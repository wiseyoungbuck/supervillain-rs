#!/usr/bin/env bash
set -euo pipefail

PORT=8000
LOG_FILE="${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}/supervillain.log"
REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
DRY_RUN=false

for arg in "$@"; do
    if [[ "$arg" == "--dry-run" ]]; then
        DRY_RUN=true
    fi
done

run() {
    if $DRY_RUN; then
        echo "[dry-run] $*"
    else
        "$@"
    fi
}

is_running() {
    if command -v ss &>/dev/null; then
        ss -tlnp 2>/dev/null | grep ":$PORT " | grep -q supervillain
    else
        lsof -i ":$PORT" -sTCP:LISTEN 2>/dev/null | grep -q supervillain
    fi
}

stop_server() {
    if ! is_running; then
        echo "Supervillain is not running."
        return
    fi
    echo "Stopping supervillain on port $PORT..."
    run pkill -x supervillain || true
    run sleep 0.5
}

install_binary() {
    echo "Building and installing from $REPO_DIR..."
    run cargo install --path "$REPO_DIR"
}

start_server() {
    echo "Starting supervillain..."
    if $DRY_RUN; then
        echo "[dry-run] nohup supervillain --no-browser > $LOG_FILE 2>&1 &"
        echo "[dry-run] Poll until :$PORT is listening (15s timeout)"
        return
    fi
    nohup supervillain --no-browser > "$LOG_FILE" 2>&1 &
    for _ in $(seq 1 30); do
        if is_running; then
            echo "Supervillain is running on port $PORT."
            return
        fi
        sleep 0.5
    done
    echo "Error: supervillain failed to start within 15s. Check $LOG_FILE" >&2
    exit 1
}

# Preflight
if ! command -v cargo &>/dev/null; then
    echo "Error: cargo not found. Install Rust: https://rustup.rs" >&2
    exit 1
fi

stop_server
install_binary
start_server
