# Supervillain — Google Cloud setup

This is the source of truth for the GCP side of Supervillain. If you walk away for six months and come back, this file plus `bootstrap.sh` should be enough to rebuild it.

## What this gives you

A Google Cloud project with the Gmail and Calendar APIs enabled, plus an OAuth client (Desktop app) that Supervillain uses to talk to your inbox. You go through this **once**; after that the app just runs.

Two parts:

1. **Automated** (`bootstrap.sh`) — creates the project, enables APIs. ~30 seconds.
2. **Manual** — OAuth consent screen + Desktop client. Google has no public API for these. ~5 minutes of clicking.

## Recorded state

Update these after your first successful setup so future-you knows what's deployed.

- **Project ID:** `supervillain-mail-prod`
- **Created:** _YYYY-MM-DD_ with gcloud version _X.Y.Z_
- **OAuth client ID:** _fill in after creating; this value is public, safe to commit_
- **OAuth client secret:** stored in `~/.config/supervillain/config` (not committed)
- **Test users:** matt.coburn@gmail.com

## Prerequisites

- `gcloud` installed (<https://cloud.google.com/sdk/docs/install>)
- Logged in: `gcloud auth login`
- Default account is the one you want this project under

## Part 1 — Run the script

```sh
./infra/bootstrap.sh
```

The script:

- Creates the project `supervillain-mail-prod` (or skips if it already exists)
- Enables Gmail API + Google Calendar API
- Prints the two URLs you need for Part 2

If the project ID is globally taken (Google project IDs are unique across all of GCP), edit `PROJECT_ID` at the top of `bootstrap.sh` to something else and re-run. Don't forget to update the "Recorded state" section above.

**Optional:** to link a billing account, set `BILLING_ACCOUNT` at the top of the script to your billing account ID and re-run. You only need this if you exceed the free Gmail/Calendar API quotas, which a personal mailbox won't.

## Part 2 — OAuth consent screen (in browser)

The script prints a URL like `https://console.cloud.google.com/apis/credentials/consent?project=supervillain-mail-prod`. Open it.

**Goal:** end up with an OAuth consent screen configured as **External** user type, with your own email listed as a **Test user**.

1. **User type → External.** (Internal is only available for Workspace orgs.)
2. **App information:**
   - App name: `Supervillain` (or whatever you want)
   - User support email: your email
   - Developer contact: your email
3. **Scopes step:** leave empty. Supervillain requests scopes at runtime, not at consent-screen time. Click "Save and continue."
4. **Test users → add your Google address.** This is the most-skipped step and the most-painful one to debug: without it, your refresh tokens expire after **7 days** and Supervillain breaks until you re-auth. Add yourself, then save.

You don't need to publish the app or submit for verification. "Testing" status is fine and intended for personal-use OAuth apps.

> **2026 Cloud Console note:** the consent screen page has been split into separate "Branding," "Audience," and "Data access" sub-pages. The state you're aiming for is the same; only the navigation differs. "Test users" lives under **Audience**.

## Part 3 — Create the OAuth client (in browser)

The script also prints `https://console.cloud.google.com/apis/credentials?project=supervillain-mail-prod`. Open it.

**Goal:** create a Desktop-app OAuth client and copy its `client_id` and `client_secret`.

1. **Create Credentials → OAuth client ID.**
2. **Application type → Desktop app.** This auto-allows `http://127.0.0.1:*` loopback redirects, which is exactly what Supervillain uses (see `src/gmail.rs:44`).
3. **Name:** `Supervillain Desktop` (only shown to you in the console; not visible to end-users).
4. **Create.** A dialog shows the resulting **Client ID** and **Client secret**. Copy both — the secret is shown once.

> **About the "client secret" for Desktop apps:** the OAuth spec says public/desktop clients shouldn't need a secret (PKCE protects the exchange — and Supervillain uses PKCE, see `src/oauth.rs`). Google requires it anyway. Treat it as a low-sensitivity credential; it's not committed to the repo, but it doesn't need a vault either.

## Part 4 — Wire credentials into your config

Edit `~/.config/supervillain/config`:

```ini
[gmail]
provider = gmail
client-id = your-client-id.apps.googleusercontent.com
client-secret = GOCSPX-xxxxxxxxxxxx
```

## Part 5 — Authorize and use

Start Supervillain. On first run for a Gmail mailbox, it opens a browser for OAuth consent. Sign in as the address you added as a Test user. Tokens are saved to `~/.config/supervillain/tokens/<account>.json` and auto-refreshed.

## Part 6 — Update the recorded state

Edit the "Recorded state" section at the top of this file with your project ID (if you changed it), today's date, your gcloud version (`gcloud version | head -1`), and the new `client_id` (the ID is public — committing it is fine).

## Troubleshooting

- **"Refresh token expired or revoked"** after ~7 days → you're not listed as a Test user, or you were added more than 7 days ago and the token aged out. Re-add yourself, delete `~/.config/supervillain/tokens/<account>.json`, restart.
- **"Google did not return a refresh_token on initial consent"** → consent was deduplicated. Revoke the app at <https://myaccount.google.com/permissions> and re-authorize.
- **`Error 400: redirect_uri_mismatch`** when authorizing → you created a Web application client by mistake. Delete it, create a Desktop app client instead. (Or, if you really want Web, register `http://127.0.0.1:8401/callback` as an authorized redirect URI — not `localhost`, which Google rejects for these flows.)

## Scopes and redirect URI live in code

These are runtime-authoritative — don't duplicate them here:

- Scopes: `src/gmail.rs:52-56` (`gmail.modify`, `gmail.send`, `calendar`)
- Redirect URI: `src/gmail.rs:44` (`http://127.0.0.1:8401/callback`)

If you change them in code, no GCP-side change is needed: Desktop OAuth clients auto-allow `127.0.0.1` loopback redirects on any port, and Supervillain requests scopes dynamically at consent time.

## Verify current cloud state

```sh
gcloud projects describe supervillain-mail-prod
gcloud services list --enabled --project=supervillain-mail-prod | grep -E 'gmail|calendar'
```

The OAuth client and consent screen state are not API-readable — eyeball them in the console URLs that `bootstrap.sh` prints.

## Run the script's tests

```sh
./infra/tests/test_bootstrap.sh
```

Mocks `gcloud` via PATH and asserts on six behaviors (idempotency, billing skip, batched API enable, error reporting, etc.). Needs no GCP access.

## What this does NOT solve

For zero manual steps (no per-user GCP setup), you'd **publish the OAuth client** through Google's verification process (one-time, can take weeks) and ship a single embedded `client_id`/`client_secret` with the app — same model as Thunderbird/Mutt. That's a separate project, orthogonal to this IaC.
