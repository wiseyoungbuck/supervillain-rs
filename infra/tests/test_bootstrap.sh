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
  local log stdout stderr exit_code
  log=$(mktemp)
  stdout=$(mktemp)
  stderr=$(mktemp)

  MOCK_GCLOUD_LOG="$log" \
  MOCK_GCLOUD_DESCRIBE_EXIT="${MOCK_GCLOUD_DESCRIBE_EXIT:-0}" \
  MOCK_GCLOUD_CREATE_EXIT="${MOCK_GCLOUD_CREATE_EXIT:-0}" \
  MOCK_GCLOUD_ENABLE_EXIT="${MOCK_GCLOUD_ENABLE_EXIT:-0}" \
  MOCK_GCLOUD_BILLING_EXIT="${MOCK_GCLOUD_BILLING_EXIT:-0}" \
  PROJECT_ID="${PROJECT_ID:-}" \
  PROJECT_NAME="${PROJECT_NAME:-}" \
  BILLING_ACCOUNT="${BILLING_ACCOUNT:-}" \
  PATH="$MOCK_BIN:$PATH" \
    "$BOOTSTRAP" >"$stdout" 2>"$stderr"
  exit_code=$?

  # Robust against the assertion block silently changing — explicit
  # if/then/else can't fall through if a success-branch statement ever
  # returns non-zero. (The previous `&& {…} || {…}` idiom would have.)
  if TEST_LOG=$log TEST_STDOUT=$stdout TEST_STDERR=$stderr TEST_EXIT=$exit_code \
       _assert_$name; then
    printf '  PASS  %s\n' "$name"
    pass=$((pass+1))
  else
    printf '  FAIL  %s\n' "$name"
    printf '    --- gcloud log ---\n'; sed 's/^/    /' "$log"
    printf '    --- stdout ---\n';     sed 's/^/    /' "$stdout"
    printf '    --- stderr ---\n';     sed 's/^/    /' "$stderr"
    printf '    --- exit %d ---\n' "$exit_code"
    fail=$((fail+1))
  fi

  rm -f "$log" "$stdout" "$stderr"
}

# ---------- assertions ----------

# B1: project already exists → describe succeeds → create is NOT called
_assert_B1_idempotent_skips_create() {
  [[ "$TEST_EXIT" == "0" ]] || return 1
  grep -q "^projects describe" "$TEST_LOG" || return 1
  ! grep -q "^projects create" "$TEST_LOG"
}

# B2: project does not exist → describe fails → create IS called once,
# with the --name flag set. A refactor that drops --name would silently
# pass without the flag assertion.
_assert_B2_create_when_missing() {
  [[ "$(grep -c '^projects create' "$TEST_LOG")" == "1" ]] || return 1
  grep '^projects create' "$TEST_LOG" | grep -q -- '--name='
}

# B3: empty BILLING_ACCOUNT → no billing call
_assert_B3_skips_billing_when_empty() {
  [[ "$TEST_EXIT" == "0" ]] || return 1
  grep -q "^services enable" "$TEST_LOG" || return 1
  ! grep -q "^billing projects" "$TEST_LOG"
}

# B4: services enable called once, with both APIs in the same invocation,
# AND with --project= set. A refactor that drops --project would silently
# pass on a default-project gcloud config without the flag assertion.
_assert_B4_batched_services_enable() {
  local line
  line=$(grep '^services enable' "$TEST_LOG") || return 1
  [[ "$(grep -c '^services enable' "$TEST_LOG")" == "1" ]] || return 1
  [[ "$line" == *"gmail.googleapis.com"* ]] || return 1
  [[ "$line" == *"calendar-json.googleapis.com"* ]] || return 1
  [[ "$line" == *"--project="* ]] || return 1
}

# B5: project create fails → helpful error on stderr + exit 1, AND the
# message mentions the gcloud-auth-login hint (not just the historical
# "globally taken" wild-goose-chase phrasing).
_assert_B5_create_conflict_reports_helpfully() {
  [[ "$TEST_EXIT" != "0" ]] || return 1
  grep -q "gcloud auth login" "$TEST_STDERR" || return 1
  grep -q "globally taken" "$TEST_STDERR"
}

# B6: success → stdout points user to infra/SETUP.md
_assert_B6_prints_next_steps() {
  grep -q "infra/SETUP.md" "$TEST_STDOUT"
}

# B7: services enable fails → exit non-zero AND stderr names a likely
# cause (billing or typo) instead of just the generic ERR trap line.
_assert_B7_enable_failure_reports_helpfully() {
  [[ "$TEST_EXIT" != "0" ]] || return 1
  grep -q "Enabling APIs failed" "$TEST_STDERR" || return 1
  grep -q "Billing not linked" "$TEST_STDERR"
}

# B8: BILLING_ACCOUNT env-set → billing link IS called, with the right
# project ID AND with --billing-account= carrying the env value.
_assert_B8_billing_link_when_set() {
  [[ "$TEST_EXIT" == "0" ]] || return 1
  local line
  line=$(grep '^billing projects link' "$TEST_LOG") || return 1
  [[ "$line" == *"--billing-account=BILLACCT-TEST-123"* ]]
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

# B5: describe fails, create fails (e.g. project ID globally taken, or not
# authenticated, or org policy denial — message now lists multiple causes)
MOCK_GCLOUD_DESCRIBE_EXIT=1 MOCK_GCLOUD_CREATE_EXIT=1 run_test B5_create_conflict_reports_helpfully

# B6: happy path
MOCK_GCLOUD_DESCRIBE_EXIT=0 run_test B6_prints_next_steps

# B7: enable fails (common: billing not linked for an API that requires it)
MOCK_GCLOUD_DESCRIBE_EXIT=0 MOCK_GCLOUD_ENABLE_EXIT=1 run_test B7_enable_failure_reports_helpfully

# B8: billing positive branch — env-set BILLING_ACCOUNT must trigger the
# `gcloud billing projects link` call with the expected --billing-account.
BILLING_ACCOUNT=BILLACCT-TEST-123 MOCK_GCLOUD_DESCRIBE_EXIT=0 run_test B8_billing_link_when_set

echo
printf '%d passed, %d failed\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
