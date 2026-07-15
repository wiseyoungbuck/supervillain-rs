#!/usr/bin/env bash
# Cross-platform launcher for Supervillain. Detects the host OS at runtime
# and routes through `open` (macOS) or `omarchy-launch-or-focus-webapp` /
# `xdg-open` (Linux). Used by the macOS .app bundle (install-macos-app.sh)
# and the Linux .desktop entry (install-omarchy.sh).

set -euo pipefail

# The binary defaults to loopback (no auth layer); this launcher mirrors
# that default. Opt in to LAN/tailnet reachability by exporting
# SUPERVILLAIN_BIND (e.g. 0.0.0.0:8000), or better, serve the loopback
# port over your tailnet — see README.md "Serving over the tailnet
# (HTTPS)". PORT derives from the bind so the port checks track it.
export SUPERVILLAIN_BIND="${SUPERVILLAIN_BIND:-127.0.0.1:8000}"
PORT="${SUPERVILLAIN_BIND##*:}"
# {1,5} also blocks 64-bit wraparound: a 20-digit port can evaluate to an
# in-range value under bash's modular arithmetic.
if [[ "$SUPERVILLAIN_BIND" != *:* ]] || ! [[ "$PORT" =~ ^[0-9]{1,5}$ ]] ||
    ((10#$PORT < 1 || 10#$PORT > 65535)); then
    echo "Error: SUPERVILLAIN_BIND='${SUPERVILLAIN_BIND}' must be host:port with a port in 1-65535" >&2
    exit 1
fi
# Normalize leading zeros: ss reports the listener as :8000, so an
# unnormalized 08000 would never match the port checks.
PORT=$((10#$PORT))
# URL host mirrors the binary's browser_url(): a wildcard bind is
# reachable at loopback; a specific interface only listens on itself.
HOST="${SUPERVILLAIN_BIND%:*}"
case "$HOST" in
    "0.0.0.0" | "[::]" | "::" | "") HOST="127.0.0.1" ;;
esac
URL="http://${HOST}:${PORT}"
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
        omarchy-launch-or-focus-webapp "${HOST}:${PORT}" "$URL"
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

# A running server keeps serving the code compiled into its binary, so a
# merged fix stays invisible until restart (kata tgax). Stale = the running
# server's /api/build-id differs from the repo HEAD. Conservative on
# unknowns: can't read the repo → not stale (never restart on bad
# information); can't read the endpoint → stale (only pre-endpoint binaries
# lack it, and those are by definition old).
server_is_stale() {
    local repo="$1"
    [[ -n "$repo" && -e "$repo/.git" ]] || return 1
    command -v curl &>/dev/null || return 1
    # --short=12 pinned to match build.rs — see check-and-update.sh.
    local repo_head running_id
    repo_head="$(git -C "$repo" rev-parse --short=12 HEAD 2>/dev/null)" || return 1
    [[ -n "$repo_head" ]] || return 1
    running_id="$(curl -fsS --max-time 2 "$URL/api/build-id" 2>/dev/null)" || running_id=""
    [[ "$running_id" != "$repo_head" ]]
}

# Stop whatever holds $PORT and wait for the port to be released; fails if
# it's still held after ~5s. Port-scoped, not name-scoped: the staleness
# check talked to $URL, so kill exactly that listener — a healthy second
# instance on another port must survive, and a non-supervillain process
# squatting the port still gets stopped instead of pkill silently missing.
stop_stale_server() {
    if command -v lsof &>/dev/null; then
        lsof -ti ":$PORT" -sTCP:LISTEN 2>/dev/null | xargs -r kill 2>/dev/null || true
    elif command -v fuser &>/dev/null; then
        fuser -k "${PORT}/tcp" 2>/dev/null || true
    else
        pkill -x supervillain 2>/dev/null || true
    fi
    for _ in $(seq 1 20); do
        port_listening || return 0
        sleep 0.25
    done
    ! port_listening
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

# An already-running server is only reused if it's running the code the
# repo is at; otherwise stop it and fall through to start the freshly
# installed binary (check_and_update above already rebuilt it). This main
# flow is below the source-only short-circuit and intentionally untested;
# the decisions it composes (server_is_stale, stop_stale_server) are
# behavior-tested in scripts/tests/test_launcher_stale.sh.
if port_listening; then
    if server_is_stale "${REPO_DIR_FROM_STAMP:-}"; then
        echo "Supervillain: running server is stale — restarting..."
        if ! stop_stale_server; then
            notify_error "couldn't stop the stale server on port $PORT — restart it manually"
            exit 1
        fi
    else
        open_webapp
        exit 0
    fi
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
