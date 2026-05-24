#!/usr/bin/env bash
# Automates the scriptable parts of Supervillain's GCP setup.
# Run this, then follow infra/SETUP.md for the manual OAuth steps.
# Tested with gcloud version 472.0.0 (2026-05-24).
#
# All three knobs are env-overridable so you don't have to edit (and
# accidentally commit) the script for a one-off project ID change:
#
#   PROJECT_ID=my-other-id ./infra/bootstrap.sh
#   BILLING_ACCOUNT=XXXXXX-XXXXXX-XXXXXX ./infra/bootstrap.sh
set -euo pipefail
trap 'echo "FAILED line $LINENO: $BASH_COMMAND" >&2' ERR

PROJECT_ID="${PROJECT_ID:-supervillain-mail-prod}"
PROJECT_NAME="${PROJECT_NAME:-Supervillain Mail}"
BILLING_ACCOUNT="${BILLING_ACCOUNT:-}"
readonly APIS=(
  gmail.googleapis.com
  calendar-json.googleapis.com
)

# Pre-declare so the EXIT trap is registered BEFORE mktemp runs — closes
# the window where Ctrl-C between mktemp and trap leaves a stray tmpfile.
describe_err=""
trap 'rm -f "${describe_err:-}"' EXIT
describe_err=$(mktemp)

if ! gcloud projects describe "$PROJECT_ID" >/dev/null 2>"$describe_err"; then
  # NOTE: deliberately do NOT redirect create's stderr — gcloud's own error
  # message is informative ("not logged in", "project ID already exists",
  # "billing required", etc.) and the user should see it directly. We only
  # capture describe's stderr because its failure is the *expected* path
  # (the project doesn't exist yet), so we'd usually want to hide it; we
  # only surface it as supplementary context when create also fails.
  if ! gcloud projects create "$PROJECT_ID" --name="$PROJECT_NAME"; then
    {
      echo
      echo "Project setup failed. Common causes:"
      echo "  - Not logged in           → run: gcloud auth login"
      echo "  - Project ID globally taken → override: PROJECT_ID=foo ./infra/bootstrap.sh"
      echo "  - Org policy or billing denial"
      echo
      echo "describe stderr (from checking if project existed first):"
      sed 's/^/  /' "$describe_err"
    } >&2
    exit 1
  fi
fi

if [[ -n "$BILLING_ACCOUNT" ]]; then
  gcloud billing projects link "$PROJECT_ID" --billing-account="$BILLING_ACCOUNT"
fi

if ! gcloud services enable "${APIS[@]}" --project="$PROJECT_ID"; then
  {
    echo
    echo "Enabling APIs failed. Likely causes:"
    echo "  - Billing not linked (some APIs require it) → set BILLING_ACCOUNT=… and re-run"
    echo "  - Typo in APIS=(…) at top of this script"
    echo "  - Insufficient permissions on project '$PROJECT_ID'"
  } >&2
  exit 1
fi

cat <<EOF

Automated setup complete for project: $PROJECT_ID

Manual steps remain (Google has no API for these). Open these in a browser:

  Consent screen:    https://console.cloud.google.com/apis/credentials/consent?project=$PROJECT_ID
  OAuth credentials: https://console.cloud.google.com/apis/credentials?project=$PROJECT_ID

Full walkthrough: infra/SETUP.md

After you finish, update the "Recorded state" section in infra/SETUP.md
with the new OAuth client_id and today's date.
EOF
