#!/usr/bin/env bash
set -euo pipefail

# The binary defaults to loopback (no auth layer); this deployment opts in
# to LAN/tailnet reachability. Override by exporting SUPERVILLAIN_BIND.
# PORT derives from it so stop/poll checks track a custom bind address.
export SUPERVILLAIN_BIND="${SUPERVILLAIN_BIND:-0.0.0.0:8000}"
PORT="${SUPERVILLAIN_BIND##*:}"
if ! [[ "$PORT" =~ ^[0-9]+$ ]] || ((10#$PORT < 1 || 10#$PORT > 65535)); then
    echo "Error: SUPERVILLAIN_BIND='${SUPERVILLAIN_BIND}' must be host:port with a port in 1-65535" >&2
    exit 1
fi
# Normalize leading zeros: ss reports the listener as :8000, so an
# unnormalized 08000 would never match the stop/poll checks.
PORT=$((10#$PORT))
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
        lsof -i ":$PORT" -sTCP:LISTEN 2>/dev/null | grep -q supervill
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
    run cargo install --path "$REPO_DIR" --force
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
