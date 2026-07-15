#!/usr/bin/env bash
# Behavioral tests for check_and_update().
#
# Stubs `git` and `cargo` on PATH so we can observe whether the function
# decides to rebuild. The stubs read from env vars to simulate state.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"

setup() {
    TMP="$(mktemp -d)"
    BIN="$TMP/bin"
    mkdir -p "$BIN"
    # Simulated repo dir — must look enough like a git repo to pass our guard.
    FAKE_REPO="$TMP/repo"
    mkdir -p "$FAKE_REPO/.git"

    # git stub: behavior driven by env vars from the test.
    cat > "$BIN/git" <<'STUB'
#!/usr/bin/env bash
# Recognized invocations:
#   git -C <dir> fetch ...          -> exit ${GIT_FETCH_EXIT:-0}
#   git -C <dir> rev-parse HEAD     -> echo ${GIT_HEAD:-aaa}
#   git -C <dir> rev-parse @{u}     -> echo ${GIT_UPSTREAM:-aaa}
for ((i=1; i<=$#; i++)); do
    case "${!i}" in
        fetch) exit "${GIT_FETCH_EXIT:-0}" ;;
        rev-parse)
            next=$((i+1))
            case "${!next}" in
                HEAD) echo "${GIT_HEAD:-aaa}"; exit 0 ;;
                '@{u}') echo "${GIT_UPSTREAM:-aaa}"; exit 0 ;;
                --short | --short=*)
                    if [[ "${GIT_SHORT_EXIT:-0}" != 0 ]]; then exit "${GIT_SHORT_EXIT}"; fi
                    echo "${GIT_SHORT_HEAD:-abc}"; exit 0 ;;
            esac
            ;;
    esac
done
exit 0
STUB
    chmod +x "$BIN/git"

    # cargo stub: record argv to a file; never actually build.
    cat > "$BIN/cargo" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$@" >> "$CARGO_CALLS_FILE"
STUB
    chmod +x "$BIN/cargo"

    # supervillain stub: reports the "installed" binary's build id.
    cat > "$BIN/supervillain" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$@" >> "${SUPERVILLAIN_STUB_CALLS:-/dev/null}"
if [[ "${SUPERVILLAIN_STUB_EXIT:-0}" != 0 ]]; then exit "${SUPERVILLAIN_STUB_EXIT}"; fi
echo "${SUPERVILLAIN_STUB_BUILD_ID:-abc}"
STUB
    chmod +x "$BIN/supervillain"
    export SUPERVILLAIN_STUB_CALLS="$TMP/supervillain_calls"
    : > "$SUPERVILLAIN_STUB_CALLS"

    export PATH="$BIN:$PATH"
    export CARGO_CALLS_FILE="$TMP/cargo_calls"
    : > "$CARGO_CALLS_FILE"
    export SUPERVILLAIN_REPO_DIR="$FAKE_REPO"
    export SUPERVILLAIN_CHECK_SOURCE_ONLY=1
    # shellcheck disable=SC1091
    source "$REPO/scripts/check-and-update.sh"
}

teardown() {
    rm -rf "$TMP"
    unset GIT_HEAD GIT_UPSTREAM GIT_FETCH_EXIT SUPERVILLAIN_REPO_DIR \
        GIT_SHORT_HEAD GIT_SHORT_EXIT SUPERVILLAIN_STUB_BUILD_ID SUPERVILLAIN_STUB_EXIT
}

assert_no_cargo_install() {
    if [[ -s "$CARGO_CALLS_FILE" ]]; then
        echo "  FAIL ($1): expected NO cargo invocations, got:"
        sed 's/^/    /' "$CARGO_CALLS_FILE"
        return 1
    fi
}

assert_cargo_install_for_repo() {
    local expected_repo="$1"
    local label="$2"
    local got
    got="$(tr '\n' ' ' < "$CARGO_CALLS_FILE")"
    local want="install --path $expected_repo "
    if [[ "$got" != "$want" ]]; then
        echo "  FAIL ($label): cargo invocation mismatch"
        echo "    expected: $want"
        echo "    actual:   $got"
        return 1
    fi
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

t_up_to_date() {
    GIT_HEAD=abc123 GIT_UPSTREAM=abc123 check_and_update
    assert_no_cargo_install "up-to-date"
}

t_behind_triggers_install() {
    GIT_HEAD=abc123 GIT_UPSTREAM=def456 check_and_update
    assert_cargo_install_for_repo "$FAKE_REPO" "behind"
}

t_fetch_failure_is_nonfatal() {
    # fetch fails -> no install, function still returns success (don't block startup)
    GIT_HEAD=abc123 GIT_UPSTREAM=def456 GIT_FETCH_EXIT=1 check_and_update
    assert_no_cargo_install "fetch-failure"
}

t_missing_repo_is_nonfatal() {
    SUPERVILLAIN_REPO_DIR="$TMP/does-not-exist" check_and_update
    assert_no_cargo_install "missing-repo"
}

t_not_a_git_repo_is_nonfatal() {
    local plain="$TMP/plain"
    mkdir -p "$plain"  # no .git inside
    SUPERVILLAIN_REPO_DIR="$plain" check_and_update
    assert_no_cargo_install "not-a-git-repo"
}

t_worktree_with_git_file() {
    # In a git worktree, `.git` is a FILE (gitdir pointer), not a dir.
    # The repo guard must accept either.
    local wt="$TMP/worktree"
    mkdir -p "$wt"
    printf 'gitdir: /somewhere/else\n' > "$wt/.git"
    GIT_HEAD=abc123 GIT_UPSTREAM=def456 \
        SUPERVILLAIN_REPO_DIR="$wt" check_and_update
    assert_cargo_install_for_repo "$wt" "worktree-git-file"
}

# ---- installed-binary freshness (kata tgax) ----
# Merging to main doesn't deploy: the binary embeds its build id at compile
# time, so check_and_update must also compare the INSTALLED binary against
# the repo HEAD — repo-vs-upstream alone missed a 5-day-stale binary.

t_stale_binary_triggers_install() {
    GIT_HEAD=abc123 GIT_UPSTREAM=abc123 GIT_SHORT_HEAD=abc123 \
        SUPERVILLAIN_STUB_BUILD_ID=old111 check_and_update
    assert_cargo_install_for_repo "$FAKE_REPO" "stale-binary"
}

t_fresh_binary_no_install() {
    GIT_HEAD=abc123 GIT_UPSTREAM=abc123 GIT_SHORT_HEAD=abc123 \
        SUPERVILLAIN_STUB_BUILD_ID=abc123 check_and_update
    assert_no_cargo_install "fresh-binary"
}

t_missing_binary_triggers_install() {
    GIT_HEAD=abc123 GIT_UPSTREAM=abc123 GIT_SHORT_HEAD=abc123 \
        SUPERVILLAIN_STUB_EXIT=127 check_and_update
    assert_cargo_install_for_repo "$FAKE_REPO" "missing-binary"
}

t_offline_stale_binary_still_rebuilds() {
    # A failed fetch must not defeat the local binary-vs-HEAD check —
    # freshness against the local repo needs no network.
    GIT_FETCH_EXIT=1 GIT_HEAD=abc123 GIT_UPSTREAM=def456 \
        GIT_SHORT_HEAD=abc123 SUPERVILLAIN_STUB_BUILD_ID=old111 check_and_update
    assert_cargo_install_for_repo "$FAKE_REPO" "offline-stale"
}

t_short_head_failure_is_nonfatal() {
    GIT_HEAD=abc123 GIT_UPSTREAM=abc123 GIT_SHORT_EXIT=1 \
        SUPERVILLAIN_STUB_BUILD_ID=old111 check_and_update
    assert_no_cargo_install "short-head-failure"
}

t_probe_passes_no_browser() {
    # Pre---build-id binaries ignore the flag and run full startup; without
    # --no-browser that startup pops the user's browser before the timeout
    # cap kills it (roborev 332). Old binaries DO honor --no-browser.
    GIT_HEAD=abc123 GIT_UPSTREAM=abc123 GIT_SHORT_HEAD=abc123 \
        SUPERVILLAIN_STUB_BUILD_ID=abc123 check_and_update
    grep -qx -- '--build-id' "$SUPERVILLAIN_STUB_CALLS" &&
        grep -qx -- '--no-browser' "$SUPERVILLAIN_STUB_CALLS"
}

t_short_sha_width_is_pinned() {
    # git's default --short width grows with the object count, so an
    # unpinned width can disagree with the id embedded at build time for
    # the SAME commit -> spurious rebuild on every launch until reinstall.
    # The contract spans three files that must agree; any one drifting is
    # a failure.
    local f
    for f in build.rs scripts/check-and-update.sh scripts/supervillain-launcher.sh; do
        grep -q -- '--short=12' "$REPO/$f" || {
            echo "  FAIL: $f does not pin --short=12"
            return 1
        }
    done
}

t_sourced_under_errexit_does_not_abort() {
    # check-and-update.sh's last statement must leave $? == 0, otherwise
    # callers running `set -e` will abort when they `source` it.
    bash -c "set -e; source '$REPO/scripts/check-and-update.sh'; echo OK" \
        | grep -q '^OK$'
}

run_test "up-to-date: skips reinstall"           t_up_to_date
run_test "behind: triggers cargo install --path" t_behind_triggers_install
run_test "fetch failure: no install, no crash"   t_fetch_failure_is_nonfatal
run_test "missing repo dir: no install, no crash" t_missing_repo_is_nonfatal
run_test "no .git in repo: no install, no crash" t_not_a_git_repo_is_nonfatal
run_test "worktree (.git is a file): triggers install" t_worktree_with_git_file
run_test "stale binary: triggers cargo install"        t_stale_binary_triggers_install
run_test "fresh binary: skips reinstall"                t_fresh_binary_no_install
run_test "missing binary: triggers cargo install"       t_missing_binary_triggers_install
run_test "offline + stale binary: still rebuilds"       t_offline_stale_binary_still_rebuilds
run_test "short-HEAD failure: no install, no crash"     t_short_head_failure_is_nonfatal
run_test "probe passes --no-browser alongside --build-id" t_probe_passes_no_browser
run_test "short-sha width is pinned"                    t_short_sha_width_is_pinned
run_test "source under set -e: does not abort caller"  t_sourced_under_errexit_does_not_abort

echo
echo "All check_and_update tests passed."
