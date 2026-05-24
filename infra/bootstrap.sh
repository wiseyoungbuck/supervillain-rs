#!/usr/bin/env bash
# Automates the scriptable parts of Supervillain's GCP setup.
# Run this, then follow infra/SETUP.md for the manual OAuth steps.
# Tested with gcloud version 472.0.0 (2026-05-24).
set -euo pipefail
trap 'echo "FAILED line $LINENO: $BASH_COMMAND" >&2' ERR

readonly PROJECT_ID="supervillain-mail-prod"
readonly PROJECT_NAME="Supervillain Mail"
readonly BILLING_ACCOUNT=""    # set to your billing account ID to link
readonly APIS=(
  gmail.googleapis.com
  calendar-json.googleapis.com
)

describe_err=$(mktemp)
trap 'rm -f "$describe_err"' EXIT
if ! gcloud projects describe "$PROJECT_ID" >/dev/null 2>"$describe_err"; then
  if ! gcloud projects create "$PROJECT_ID" --name="$PROJECT_NAME" 2>/dev/null; then
    echo "Project create failed. ID '$PROJECT_ID' may be globally taken," >&2
    echo "or describe failed for another reason. Describe stderr was:" >&2
    sed 's/^/  /' "$describe_err" >&2
    exit 1
  fi
fi

if [[ -n "$BILLING_ACCOUNT" ]]; then
  gcloud billing projects link "$PROJECT_ID" --billing-account="$BILLING_ACCOUNT"
fi

gcloud services enable "${APIS[@]}" --project="$PROJECT_ID"

cat <<EOF

Automated setup complete for project: $PROJECT_ID

Manual steps remain (Google has no API for these). Open these in a browser:

  Consent screen:    https://console.cloud.google.com/apis/credentials/consent?project=$PROJECT_ID
  OAuth credentials: https://console.cloud.google.com/apis/credentials?project=$PROJECT_ID

Full walkthrough: infra/SETUP.md

After you finish, update the "Recorded state" section in infra/SETUP.md
with the new OAuth client_id and today's date.
EOF
