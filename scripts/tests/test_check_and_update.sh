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
    unset GIT_HEAD GIT_UPSTREAM GIT_FETCH_EXIT SUPERVILLAIN_REPO_DIR
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
run_test "source under set -e: does not abort caller"  t_sourced_under_errexit_does_not_abort

echo
echo "All check_and_update tests passed."
