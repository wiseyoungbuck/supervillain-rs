#!/usr/bin/env bash
# Behavior tests for infra/bootstrap.sh.
# Mocks `gcloud` via PATH and asserts on the call log + script output.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BOOTSTRAP="$HERE/../bootstrap.sh"
MOCK_BIN="$HERE/bin"

pass=0
fail=0

run_test() {
  local name=$1
  local log
  log=$(mktemp)
  local stdout stderr exit_code
  stdout=$(mktemp)
  stderr=$(mktemp)

  MOCK_GCLOUD_LOG="$log" \
  MOCK_GCLOUD_DESCRIBE_EXIT="${MOCK_GCLOUD_DESCRIBE_EXIT:-0}" \
  MOCK_GCLOUD_CREATE_EXIT="${MOCK_GCLOUD_CREATE_EXIT:-0}" \
  MOCK_GCLOUD_ENABLE_EXIT="${MOCK_GCLOUD_ENABLE_EXIT:-0}" \
  MOCK_GCLOUD_BILLING_EXIT="${MOCK_GCLOUD_BILLING_EXIT:-0}" \
  PATH="$MOCK_BIN:$PATH" \
    "$BOOTSTRAP" >"$stdout" 2>"$stderr"
  exit_code=$?

  TEST_LOG=$log TEST_STDOUT=$stdout TEST_STDERR=$stderr TEST_EXIT=$exit_code \
    _assert_$name && {
      printf '  PASS  %s\n' "$name"
      pass=$((pass+1))
    } || {
      printf '  FAIL  %s\n' "$name"
      printf '    --- gcloud log ---\n'; sed 's/^/    /' "$log"
      printf '    --- stdout ---\n';     sed 's/^/    /' "$stdout"
      printf '    --- stderr ---\n';     sed 's/^/    /' "$stderr"
      printf '    --- exit %d ---\n' "$exit_code"
      fail=$((fail+1))
    }

  rm -f "$log" "$stdout" "$stderr"
}

# ---------- assertions ----------

# B1: project already exists → describe succeeds → create is NOT called
_assert_B1_idempotent_skips_create() {
  [[ "$TEST_EXIT" == "0" ]] || return 1
  grep -q "^projects describe" "$TEST_LOG" || return 1
  ! grep -q "^projects create" "$TEST_LOG"
}

# B2: project does not exist → describe fails → create IS called once
_assert_B2_create_when_missing() {
  [[ "$(grep -c '^projects create' "$TEST_LOG")" == "1" ]]
}

# B3: empty BILLING_ACCOUNT → no billing call
_assert_B3_skips_billing_when_empty() {
  [[ "$TEST_EXIT" == "0" ]] || return 1
  grep -q "^services enable" "$TEST_LOG" || return 1
  ! grep -q "^billing projects" "$TEST_LOG"
}

# B4: services enable called once, with both APIs in the same invocation
_assert_B4_batched_services_enable() {
  local line
  line=$(grep '^services enable' "$TEST_LOG") || return 1
  [[ "$(grep -c '^services enable' "$TEST_LOG")" == "1" ]] || return 1
  [[ "$line" == *"gmail.googleapis.com"* ]] || return 1
  [[ "$line" == *"calendar-json.googleapis.com"* ]] || return 1
}

# B5: project create fails → helpful error on stderr + exit 1
_assert_B5_create_conflict_reports_helpfully() {
  [[ "$TEST_EXIT" != "0" ]] || return 1
  grep -q "globally taken" "$TEST_STDERR"
}

# B6: success → stdout points user to infra/SETUP.md
_assert_B6_prints_next_steps() {
  grep -q "infra/SETUP.md" "$TEST_STDOUT"
}

# ---------- run ----------

echo "Running bootstrap.sh behavior tests..."

# B1: describe succeeds (project exists)
MOCK_GCLOUD_DESCRIBE_EXIT=0 run_test B1_idempotent_skips_create

# B2: describe fails, create succeeds
MOCK_GCLOUD_DESCRIBE_EXIT=1 MOCK_GCLOUD_CREATE_EXIT=0 run_test B2_create_when_missing

# B3: empty billing (script's default), describe succeeds
MOCK_GCLOUD_DESCRIBE_EXIT=0 run_test B3_skips_billing_when_empty

# B4: describe succeeds, enable runs
MOCK_GCLOUD_DESCRIBE_EXIT=0 run_test B4_batched_services_enable

# B5: describe fails, create fails (e.g. project ID globally taken)
MOCK_GCLOUD_DESCRIBE_EXIT=1 MOCK_GCLOUD_CREATE_EXIT=1 run_test B5_create_conflict_reports_helpfully

# B6: happy path
MOCK_GCLOUD_DESCRIBE_EXIT=0 run_test B6_prints_next_steps

echo
printf '%d passed, %d failed\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
