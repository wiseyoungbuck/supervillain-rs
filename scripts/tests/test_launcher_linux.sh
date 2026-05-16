#!/usr/bin/env bash
# Behavioral tests for the Linux/Omarchy launcher's open_webapp().
# Verifies the omarchy-vs-xdg-open branch and confirms the launcher
# survives being sourced under `set -e` (regression guard for the
# check-and-update.sh trailing-exit-status issue caught by review 258).
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"

setup() {
    TMP="$(mktemp -d)"
    BIN="$TMP/bin"
    mkdir -p "$BIN"

    # Track which launcher each stub was invoked as.
    cat > "$BIN/xdg-open" <<'STUB'
#!/usr/bin/env bash
printf 'xdg-open %s\n' "$@" > "$LAUNCH_ARGS_FILE"
STUB
    cat > "$BIN/omarchy-launch-webapp" <<'STUB'
#!/usr/bin/env bash
printf 'omarchy %s\n' "$@" > "$LAUNCH_ARGS_FILE"
STUB
    chmod +x "$BIN/xdg-open" "$BIN/omarchy-launch-webapp"

    export PATH="$BIN:$PATH"
    export LAUNCH_ARGS_FILE="$TMP/launch_args"
    export HOME="$TMP/home"
    mkdir -p "$HOME"
    export SUPERVILLAIN_LAUNCHER_LINUX_SOURCE_ONLY=1
}

teardown() {
    rm -rf "$TMP"
}

# Sourcing under `set -e` must NOT abort. Run in a subshell so the test
# itself can recover if the sourced file misbehaves.
source_launcher() {
    bash -c "set -e; source '$REPO/scripts/supervillain-launcher-linux.sh'; declare -f open_webapp > '$TMP/fnbody'; URL='http://127.0.0.1:8000'; open_webapp"
}

run_test() {
    local name="$1"; shift
    setup
    "$@"
    local rc=$?
    teardown
    if (( rc == 0 )); then
        echo "PASS: $name"
    else
        echo "FAIL: $name"
        exit 1
    fi
}

t_xdg_open_when_no_omarchy() {
    source_launcher
    local got
    got="$(cat "$LAUNCH_ARGS_FILE")"
    if [[ "$got" != "xdg-open http://127.0.0.1:8000" ]]; then
        echo "  expected xdg-open invocation, got: $got"
        return 1
    fi
}

t_omarchy_when_present() {
    mkdir -p "$HOME/.local/share/omarchy"
    source_launcher
    local got
    got="$(cat "$LAUNCH_ARGS_FILE")"
    if [[ "$got" != "omarchy http://127.0.0.1:8000" ]]; then
        echo "  expected omarchy-launch-webapp invocation, got: $got"
        return 1
    fi
}

run_test "no omarchy dir -> xdg-open"            t_xdg_open_when_no_omarchy
run_test "omarchy dir present -> omarchy launcher" t_omarchy_when_present

echo
echo "All Linux launcher tests passed."
