<p align="center">
  <img src="static/icon-512.png" width="200" alt="Supervillain">
</p>

<h1 align="center">Supervillain</h1>

<p align="center">
  Email for people who'd rather be typing.<br>
  Vim-native, zero-Electron, talks to Fastmail, Gmail, and Outlook (calendar).
</p>


<p align="center">
  <img src="https://img.shields.io/badge/rust-1.85%2B-orange?logo=rust" alt="Rust 1.85+">
  <img src="https://img.shields.io/badge/edition-2024-blue" alt="Rust Edition 2024">
  <img src="https://img.shields.io/badge/protocol-JMAP-green" alt="JMAP">
  <img src="https://img.shields.io/badge/version-0.2.0-purple" alt="v0.2.0">
</p>

---

Supervillain is a keyboard-first email client built in Rust. It runs as a local web server, serves a zero-dependency vanilla JS frontend, and talks to your email provider's native API — [JMAP](https://jmap.io/) for Fastmail, Microsoft Graph for Outlook calendar. No Electron, no Node.js, no build step, no framework. Just `cargo install` and go.

## Features

- **Multi-provider** — Fastmail (JMAP + CalDAV), Outlook (calendar via Microsoft Graph), Gmail (full email + Google Calendar)
- **Multi-account** — Switch between accounts with `1`-`9` keys
- **Calendar sync per provider** — CalDAV (Fastmail), Outlook Calendar API, Google Calendar
- **Vim keybindings** — `j`/`k` navigation, `gg`/`G`, modal editing in compose, `/` search
- **Split inbox** — Filterable tabs by sender, recipient, subject, or calendar invites. Auto-generated from your identities on first run
- **Gmail-style search** — `from:`, `to:`, `subject:`, `has:attachment`, `is:unread`, `before:`, `newer_than:`, and more
- **Command palette** — `Ctrl+K` for quick actions
- **Multiple identities** — All your addresses in one inbox. Replies auto-select the matching From address
- **Calendar invites** — View ICS details and RSVP directly from email
- **Attachments** — Download inline or as files
- **Undo** — `z` to reverse archive, trash, and read-state changes
- **PWA support** — Installable on mobile with offline-capable service worker
- **Zero JavaScript dependencies** — Vanilla JS frontend, no transpilation, no bundler

## Keyboard shortcuts

### Navigation

| Key | Action |
|-----|--------|
| `j` / `k` | Move down / up |
| `gg` | Jump to top |
| `G` | Jump to bottom |
| `Enter` / `o` | Open email |
| `q` / `Esc` | Back to list |
| `Space` / `Shift+Space` | Page down / up in detail view |
| `Tab` / `Shift+Tab` | Next / previous split tab |
| `Ctrl+1-9` | Jump to split tab |
| `1`-`9` | Switch account |
| `R` | Refresh |
| `?` | Show keyboard shortcuts |

### Actions

| Key | Action |
|-----|--------|
| `c` | Compose |
| `r` | Reply |
| `a` | Reply all |
| `f` | Forward |
| `e` | Archive |
| `#` | Trash |
| `u` | Toggle read / unread |
| `s` | Star / flag |
| `U` | Unsubscribe and archive all from sender |
| `z` | Undo last action |
| `/` | Search |
| `Ctrl+K` | Command palette |

### Compose

| Key | Action |
|-----|--------|
| `Ctrl+Enter` | Send |
| `Esc` | Cancel |

## Requirements

- [Rust](https://www.rust-lang.org/) 1.85+ (edition 2024)
- A [Fastmail](https://www.fastmail.com/) account with an API token, and/or:
- Microsoft app registration (for Outlook calendar — email support planned for Phase 2), and/or:
- Google Cloud project with OAuth credentials (full Gmail + Google Calendar today)

## Quick start

**1. Create credentials for your provider:**

- **Fastmail** — Settings > Privacy & Security > Integrations > API tokens. The token needs `Mail` and `Calendars` scopes.
- **Outlook** — Register an app in Azure AD with Calendars.ReadWrite permissions (see [Azure AD setup](#azure-ad-app-registration) below).
- **Gmail** — Create OAuth credentials in Google Cloud Console (see [Google Cloud setup](#google-cloud-app-registration) below). Both `client-id` and `client-secret` are required — unlike Outlook, Google's OAuth needs a client_secret even on Desktop / PKCE flows.

**2. Create the config file:**

```sh
mkdir -p ~/.config/supervillain
```

```ini
# ~/.config/supervillain/config

# Single Fastmail account (simplest config)
username = you@fastmail.com
api-token = fmu1-xxxxxxxxxxxxxxxx

# Or: multiple accounts with [sections]
# [fastmail]
# provider = fastmail
# username = you@fastmail.com
# api-token = fmu1-xxxxxxxxxxxxxxxx
#
# [outlook]
# provider = outlook
# username = you@company.com
# client-id = xxxx-xxxx-xxxx
# # Phase 1: calendar only. Email support coming in Phase 2.
#
# [gmail]
# provider = gmail
# client-id = xxxx.apps.googleusercontent.com
# client-secret = GOCSPX-xxxxxxxxxxxx
# # Gmail also requires client-secret (Google quirk for PKCE).
# # Full Gmail support: read/write/send/calendar (Phase 3 complete).
```

`chmod 600 ~/.config/supervillain/config` is recommended — the file holds API tokens and OAuth secrets.

**3. Build and run:**

```sh
git clone https://github.com/AristoiAI/supervillain.git
cd supervillain
cargo install --path .
supervillain
```

This installs the `supervillain` binary to `~/.cargo/bin/` (on your PATH) and opens `http://127.0.0.1:8000` in your browser.

For Outlook accounts, first run opens a browser for OAuth2 authorization. Tokens are saved to `~/.config/supervillain/tokens/{account_id}.json` and auto-refresh before expiry.

## Installation

### macOS

```sh
# Install Rust if needed
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

git clone https://github.com/AristoiAI/supervillain.git
cd supervillain
cargo install --path .
```

### Linux

```sh
# Install Rust if needed (pre-installed on Omarchy)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

git clone https://github.com/AristoiAI/supervillain.git
cd supervillain
cargo install --path .
```

### Updating

Pull the latest changes and rebuild (stops the running server, rebuilds, restarts):

```sh
cd supervillain
git pull
./scripts/upgrade.sh
```

To rebuild without restarting:

```sh
cargo install --path .
```

## Configuration

### Config file

`~/.config/supervillain/config` (or `$XDG_CONFIG_HOME/supervillain/config`)

Two formats supported:

**Simple format** — single Fastmail account, `key = value` pairs. Lines starting with `#` are comments.

```ini
# Fastmail credentials
username = you@fastmail.com
api-token = fmu1-xxxxxxxxxxxxxxxx
```

**Multi-account format** — INI-style `[sections]`, each with a `provider` field.

```ini
[fastmail]
provider = fastmail
username = you@fastmail.com
api-token = fmu1-xxxxxxxxxxxxxxxx

[outlook]
provider = outlook
username = you@company.com
client-id = xxxx-xxxx-xxxx
# Phase 1: calendar only

[gmail]
provider = gmail
client-id = xxxx.apps.googleusercontent.com
client-secret = GOCSPX-xxxxxxxxxxxx
# Gmail needs client-secret too (Google quirk; not really secret).
```

The sectionless format is fully backward compatible — it's treated as a single Fastmail account.

### Azure AD App Registration

To use Outlook calendar sync, register an app in Azure AD:

1. Go to [Azure Portal > App registrations](https://portal.azure.com/#blade/Microsoft_AAD_RegisteredApps/ApplicationsListBlade)
2. Click **New registration**
3. Name: "Supervillain" (or whatever you like)
4. Supported account types: "Accounts in any organizational directory and personal Microsoft accounts"
5. Redirect URI: **Web** → `http://localhost:8400/callback`
6. After creation, copy the **Application (client) ID** — this is your `client-id`
7. Under **API permissions**, add: `Calendars.ReadWrite` (delegated)
8. No client secret needed — Supervillain uses PKCE (public client)

Put the client ID in your config:

```ini
[outlook]
provider = outlook
username = you@company.com
client-id = your-application-client-id
```

### Google Cloud App Registration

> **Milestones A + B + C + D (Phase 3 complete):** OAuth sign-in, mailbox listing, message reading,
> search, mark read/unread, star, archive, trash, move, batch-archive, attachment download,
> compose + reply + send with attachments, Google Calendar add/remove, RSVP via Calendar PATCH.

To use Gmail with Supervillain you'll create your own OAuth client in a Google Cloud project (one-time, ~5 minutes):

1. Go to [Google Cloud Console > APIs & Services](https://console.cloud.google.com/apis/dashboard). Create a new project if you don't have one.
2. Enable APIs: under **Enabled APIs & services**, enable **Gmail API** and **Google Calendar API**.
3. Configure the OAuth consent screen: **APIs & Services > OAuth consent screen** → User type: **External** → fill in app name, your email, etc.
4. **Add yourself as a Test User** under **Audience > Test users**. This is critical: unverified apps only work for listed test users, and refresh tokens for non-test users expire after 7 days.
5. Create credentials: **APIs & Services > Credentials** → **Create Credentials** → **OAuth client ID** → Application type: **Web application**. Add `http://localhost:8401/callback` as an authorized redirect URI.
6. Copy both the **Client ID** and **Client Secret**. Yes, both — Google's OAuth token endpoint requires `client_secret` even for "Desktop" / PKCE flows. It's not really secret, but the API rejects requests without it.

Put both in your config:

```ini
[gmail]
provider = gmail
client-id = your-client-id.apps.googleusercontent.com
client-secret = GOCSPX-xxxxxxxxxxxx
```

First run opens a browser for OAuth2 authorization. Tokens are saved to `~/.config/supervillain/tokens/gmail.json` (or whatever account name you used) and auto-refresh.

**Troubleshooting:**

- *"Refresh token expired or revoked"* — your OAuth app is in **Testing** state and you're not listed as a Test User, or you've been listed for more than 7 days. Add yourself as a Test User and re-authenticate (delete the tokens file to force OAuth).
- *"Google did not return a refresh_token on initial consent"* — your client was created without `access_type=offline` semantics, or Google de-duplicated the consent. Revoke the app in [Google Account permissions](https://myaccount.google.com/permissions) and re-authenticate.

### Multiple identities

If you have multiple addresses in Fastmail (e.g. you@company.com, you@gmail.com, you@personal.dev), Supervillain handles this automatically:

- **Receiving** — Forward Gmail/Outlook/etc. to Fastmail. All mail lands in one inbox.
- **Sending** — All Fastmail identities appear in the From dropdown. Replies auto-select the matching address.
- **Splits** — On first run, auto-creates one tab per domain from your identities.

No multi-account configuration needed.

### Splits (inbox tabs)

Splits filter your inbox into tabs. Stored at `~/.config/supervillain/splits.json`.

On first run with no splits configured, Supervillain auto-generates one tab per email domain from your identities.

**Managing splits:**

| Action | How |
|--------|-----|
| Add | `Ctrl+K` > "New Split" |
| Delete | `Ctrl+K` > type "delete" > select split |
| Edit | Edit `~/.config/supervillain/splits.json` directly |
| Regenerate | Delete `splits.json` and restart |

**Example config:**

```json
{
  "splits": [
    {
      "id": "work",
      "name": "Work",
      "icon": "https://cdn.jsdelivr.net/gh/walkxcode/dashboard-icons/svg/fastmail.svg",
      "filters": [{ "type": "to", "pattern": "*@company.com" }]
    },
    {
      "id": "newsletters",
      "name": "Newsletters",
      "match_mode": "any",
      "filters": [
        { "type": "from", "pattern": "*@substack.com" },
        { "type": "subject", "pattern": "newsletter|digest|weekly" }
      ]
    }
  ]
}
```

**Filter types:**

| Type | Pattern | Matches |
|------|---------|---------|
| `from` | Glob (`*@example.com`) | Sender address |
| `to` | Glob (`*@company.com`) | To/CC addresses |
| `subject` | Regex (`invite\|meeting`) | Subject line |
| `calendar` | `*` | Emails with calendar invites |

**Match modes:** `any` (default) matches if any filter hits. `all` requires every filter to match.

### Environment variables

All optional when using the config file.

| Variable | Description |
|----------|-------------|
| `FASTMAIL_USERNAME` | Fallback for `username` |
| `FASTMAIL_API_TOKEN` | Fallback for `api-token` |
| `VIMMAIL_SPLITS` | Inline JSON splits config (overrides file) |
| `XDG_CONFIG_HOME` | Config directory (default: `~/.config`) |
| `RUST_LOG` | Log level (`info`, `debug`, `vimmail=debug`) |

## Search syntax

The search bar (`/`) supports Gmail-style operators:

```
from:alice@example.com           # sender address
to:team@company.com              # to/cc address
subject:meeting                  # subject contains
subject:"quarterly review"       # quoted phrases
has:attachment                   # has attachments
is:unread / is:read              # read state
is:starred / is:flagged          # flagged
before:2026-01-15                # before date
after:2026-01-15                 # after date
newer_than:7d                    # relative (d/w/m)
older_than:3m                    # relative (d/w/m)
```

Operators combine with free text: `from:@github.com is:unread pull request`

## Architecture

Supervillain is a single Rust binary that runs a local [Axum](https://github.com/tokio-rs/axum) web server on `127.0.0.1:8000`. Every API endpoint takes an optional `?account={id}` parameter that selects which provider session to use.

```
Browser (localhost:8000)
    │ REST API (/api/*?account={id})
Axum HTTP Server
    │ resolve_account() → match ProviderSession
    ├── Fastmail → JMAP + CalDAV
    ├── Outlook → Microsoft Graph (Calendar only)
    └── Gmail → Gmail REST API + Google Calendar v3
```

### Provider dispatch

No traits, no vtables. Each provider is a match arm on a concrete enum:

```rust
enum ProviderSession {
    Fastmail(jmap::JmapSession),
    Outlook(outlook::OutlookSession),
    Gmail(gmail::GmailSession),
}
```

Each provider module exports plain functions (`jmap::query_emails()`, `outlook::add_to_calendar()`, `gmail::get_mailboxes()`) that take a session struct and return the same `Email`/`Mailbox`/`Identity` types. The route handler has the match statement.

### Calendar dispatch

- **Fastmail** — CalDAV PUT/DELETE (existing)
- **Outlook** — Microsoft Graph (`POST /me/events`, lookup by `iCalUId` filter)

### Search dispatch

- **Fastmail** — `to_jmap_filter()` (existing)
- Outlook search planned for Phase 2
- **Gmail** — `gmail::translate_query_to_q()` (Gmail's native `q=` syntax — essentially a superset of our DSL)

### OAuth2 flow (Outlook, Gmail)

- First run: local callback server on port 8400, browser opens auth URL with PKCE, exchange code for tokens
- Tokens saved to `~/.config/supervillain/tokens/{account_id}.json`
- Auto-refresh before expiry on each API call via interior mutability
- Same pattern as `gcloud auth login` / `gh auth login`
- Used by Outlook and Gmail. The callback acquisition is in `platform::desktop::acquire_oauth_callback`; iOS will substitute `ASWebAuthenticationSession`.

### Tech stack

| Layer | Technology |
|-------|------------|
| Backend | Rust, Axum 0.8, Tokio, reqwest |
| Frontend | Vanilla JS, CSS3 (no framework, no build step) |
| Protocols | JMAP ([RFC 8620](https://www.rfc-editor.org/rfc/rfc8620), [RFC 8621](https://www.rfc-editor.org/rfc/rfc8621)), Microsoft Graph API (calendar) |
| Auth | Bearer token (Fastmail), OAuth2 PKCE (Outlook) |
| Providers | Fastmail (email + calendar), Outlook (calendar only), Gmail (email + calendar) |

## API

All endpoints live under `/api/`. The frontend communicates exclusively through these. Multi-account endpoints accept `?account={id}`.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/accounts` | List connected accounts |
| GET | `/api/identities` | List sender identities |
| GET | `/api/mailboxes` | List mailboxes |
| GET | `/api/emails?mailbox_id=&limit=&offset=&split_id=&search=` | List emails |
| GET | `/api/emails/{id}` | Get full email (auto-marks read) |
| POST | `/api/emails/send` | Send email |
| POST | `/api/emails/{id}/archive` | Archive |
| POST | `/api/emails/{id}/trash` | Trash |
| POST | `/api/emails/{id}/mark-read` | Mark read |
| POST | `/api/emails/{id}/mark-unread` | Mark unread |
| POST | `/api/emails/{id}/toggle-flag` | Toggle star/flag |
| POST | `/api/emails/{id}/move` | Move to mailbox |
| POST | `/api/emails/{id}/rsvp` | RSVP to calendar invite |
| POST | `/api/emails/{id}/add-to-calendar` | Add invite to calendar |
| POST | `/api/emails/{id}/unsubscribe-and-archive-all` | Unsubscribe + archive all from sender |
| GET | `/api/emails/{id}/attachments/{blob_id}/{filename}` | Download attachment |
| GET | `/api/splits` | List splits |
| POST | `/api/splits` | Create split |
| PUT | `/api/splits/{id}` | Update split |
| DELETE | `/api/splits/{id}` | Delete split |
| GET | `/api/split-counts` | Get unread counts per split |
| GET | `/api/theme` | Get theme configuration |
| POST | `/api/upload` | Upload attachment for compose |

## Project structure

```
src/
  main.rs          Entry point, config parsing, server startup
  lib.rs           Module declarations
  types.rs         Data types (Email, Mailbox, Identity, Attachment, etc.)
  error.rs         Error enum + HTTP response mapping
  jmap.rs          JMAP client — Fastmail (connect, query, send, calendar, MIME parsing)
  outlook.rs       Microsoft Graph client (Calendar)
  gmail.rs         Gmail REST client + Google Calendar v3 (full email + RSVP)
  oauth.rs         OAuth2 PKCE primitives (shared by Outlook and Gmail)
  platform/        OS-specific shims: TokenStore, browser, OAuth callback, log sink
                   — desktop today, iOS module planned (Tauri-mobile)
  provider.rs      Provider dispatch — routes call provider::*, which dispatches per-provider
  routes.rs        HTTP handlers + split management
  search.rs        Search query parser + per-provider filter translation
  splits.rs        Split inbox filtering + persistence
  calendar.rs      ICS parsing + RSVP generation
  glob.rs          Glob pattern matching
  theme.rs         Theme configuration
  validate.rs      Validation macro
static/
  index.html       Frontend shell
  app.js           All frontend logic (vanilla JS)
  style.css        Terminal-style dark theme
  icon-*.png       Favicon + PWA icons
scripts/
  upgrade.sh       Stop, rebuild, restart
```

## Development

```sh
# Build
cargo build

# Run tests
cargo test

# Lint
cargo clippy -- -D warnings

# Format
cargo fmt

# Run in development
cargo run
```

Tests cover JMAP types, glob matching, split filtering, identity-based split seeding, Gmail-style search parsing, ICS calendar parsing, JMAP filter translation, MIME type detection, config parsing, and provider dispatch.

## Contributing

1. Fork the repo and create a feature branch from `main`
2. Make your changes
3. Run `cargo fmt`, `cargo clippy -- -D warnings`, and `cargo test`
4. Open a pull request

## Roadmap

- **Outlook email** — Mail.ReadWrite + Mail.Send via Microsoft Graph (Phase 2)
- **iOS app via Tauri-mobile** — `src/platform/ios.rs` (KeychainTokenStore, ASWebAuthenticationSession, os_log sink)
- Threading / conversation grouping
- Drafts
- Contact suggestions / address book
- Email signatures
- Offline mode
