#!/usr/bin/env bash
# Behavioral tests for server_is_stale() in supervillain-launcher.sh.
#
# A running server keeps serving code compiled into its binary, so a merged
# fix stays invisible until restart (kata tgax). The launcher must detect a
# stale RUNNING server by comparing its /api/build-id against the repo HEAD
# — and treat "can't tell" conservatively: unknown repo state → don't
# restart; unknown server build id (old binary without the endpoint) →
# restart.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"

setup() {
    TMP="$(mktemp -d)"
    BIN="$TMP/bin"
    mkdir -p "$BIN"
    FAKE_REPO="$TMP/repo"
    mkdir -p "$FAKE_REPO/.git"

    # git stub: rev-parse --short HEAD driven by env vars.
    cat > "$BIN/git" <<'STUB'
#!/usr/bin/env bash
if [[ "${GIT_SHORT_EXIT:-0}" != 0 ]]; then exit "${GIT_SHORT_EXIT}"; fi
echo "${GIT_SHORT_HEAD:-abc}"
STUB
    chmod +x "$BIN/git"

    # curl stub: simulates GET /api/build-id on the running server.
    cat > "$BIN/curl" <<'STUB'
#!/usr/bin/env bash
if [[ "${CURL_EXIT:-0}" != 0 ]]; then exit "${CURL_EXIT}"; fi
echo "${CURL_BUILD_ID:-abc}"
STUB
    chmod +x "$BIN/curl"

    export PATH="$BIN:$PATH"
    export SUPERVILLAIN_LAUNCHER_SOURCE_ONLY=1
    # shellcheck disable=SC1091
    source "$REPO/scripts/supervillain-launcher.sh"
}

teardown() {
    rm -rf "$TMP"
    unset GIT_SHORT_HEAD GIT_SHORT_EXIT CURL_BUILD_ID CURL_EXIT
}

run_test() {
    local name="$1"; shift
    setup
    if "$@"; then
        echo "PASS: $name"
    else
        echo "FAIL: $name"
        teardown
        exit 1
    fi
    teardown
}

# ---------- test cases ----------

t_fresh_server_is_not_stale() {
    GIT_SHORT_HEAD=abc123 CURL_BUILD_ID=abc123 \
        server_is_stale "$FAKE_REPO" && return 1 || return 0
}

t_mismatched_build_id_is_stale() {
    GIT_SHORT_HEAD=abc123 CURL_BUILD_ID=old111 server_is_stale "$FAKE_REPO"
}

t_missing_endpoint_is_stale() {
    # Pre-endpoint binaries can't report a build id — that alone proves
    # they predate this mechanism, so treat them as stale.
    GIT_SHORT_HEAD=abc123 CURL_EXIT=22 server_is_stale "$FAKE_REPO"
}

t_git_failure_is_not_stale() {
    # Can't determine what fresh means -> don't restart the user's server.
    GIT_SHORT_EXIT=1 CURL_BUILD_ID=old111 \
        server_is_stale "$FAKE_REPO" && return 1 || return 0
}

t_missing_repo_is_not_stale() {
    server_is_stale "$TMP/does-not-exist" && return 1 || return 0
}

t_empty_repo_arg_is_not_stale() {
    server_is_stale "" && return 1 || return 0
}

run_test "fresh server: not stale"                 t_fresh_server_is_not_stale
run_test "mismatched build id: stale"              t_mismatched_build_id_is_stale
run_test "no /api/build-id endpoint: stale"        t_missing_endpoint_is_stale
run_test "git failure: not stale (can't tell)"     t_git_failure_is_not_stale
run_test "missing repo dir: not stale"             t_missing_repo_is_not_stale
run_test "empty repo arg: not stale"               t_empty_repo_arg_is_not_stale

echo
echo "All server_is_stale tests passed."
