<p align="center">
  <img src="static/supervillain.jpg" width="200" alt="Supervillain">
</p>

<h1 align="center">Supervillain</h1>

<p align="center">
  Email for people who'd rather be typing. Vim-native, zero-Electron, talks JMAP to Fastmail.
</p>

## Requirements

- Rust 1.85+ (edition 2024)
- A [Fastmail](https://www.fastmail.com/) account with an API token

## Setup

1. Create a Fastmail API token at **Settings > Privacy & Security > Integrations > API tokens**. The token needs `Mail` and `Calendars` scopes.

2. Create `~/.config/supervillain/config`:

```sh
mkdir -p ~/.config/supervillain
```

```
# ~/.config/supervillain/config

username = you@fastmail.com
api-token = fmu1-xxxxxxxxxxxxxxxx
```

3. Install and run:

```sh
cargo install --path .
supervillain
```

## Installation

Clone the repo, then install:

### macOS

```sh
# Install Rust if needed
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install supervillain
git clone <repo-url>
cd supervillain-rs
cargo install --path .
```

### Omarchy Linux

Rust is pre-installed on Omarchy. Just:

```sh
git clone <repo-url>
cd supervillain-rs
cargo install --path .
```

This puts the `supervillain` binary in `~/.cargo/bin/`, which is on your PATH. Run it from anywhere:

```sh
supervillain
```

### Updating

Pull the latest changes and upgrade (stops running server, rebuilds, restarts):

```sh
cd supervillain-rs
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

Simple `key = value` format. Lines starting with `#` are comments.

```
# Fastmail credentials
username = you@fastmail.com
api-token = fmu1-xxxxxxxxxxxxxxxx
```

Environment variables `FASTMAIL_USERNAME` and `FASTMAIL_API_TOKEN` work as fallbacks.

### Multiple email addresses

If you have multiple identities in Fastmail (e.g. you@aristoi.ai, you@gmail.com, you@aristotle.ai), supervillain handles this automatically:

- **Receiving:** Forward Gmail/Outlook/etc. to Fastmail. All mail lands in one inbox.
- **Sending:** All Fastmail identities appear in the From dropdown. Replies auto-select the matching address.
- **Inbox tabs:** On first run, supervillain auto-creates split tabs from your identities (one per domain). These appear as tabs across the top of the inbox, filterable with `Tab`/`Shift+Tab` or `Ctrl+1-9`.

No multi-account configuration needed. One Fastmail connection, multiple addresses.

### Splits (inbox tabs)

Splits filter your inbox into tabs. Config is stored at `~/.config/supervillain/splits.json`.

On first run with no splits configured, supervillain auto-generates identity-based splits from your Fastmail identities (one tab per domain). After that, you manage them yourself.

**Managing splits:**

- **Add via UI:** Press `Ctrl+K` to open the command palette, select "New Split". Choose a filter type (from, to, subject, calendar), enter a pattern, and save.
- **Delete via UI:** Press `Ctrl+K`, type "delete", select the split to remove.
- **Edit JSON directly:** Edit `~/.config/supervillain/splits.json` and refresh.
- **Re-generate from identities:** Delete `~/.config/supervillain/splits.json` and restart. Splits will be re-created from your current Fastmail identities.

Example config:

```json
{
  "splits": [
    {
      "id": "aristoi",
      "name": "aristoi",
      "icon": "https://cdn.jsdelivr.net/gh/walkxcode/dashboard-icons/svg/aristotle.svg",
      "filters": [{ "type": "to", "pattern": "*@aristoi.ai" }]
    },
    {
      "id": "gmail",
      "name": "gmail",
      "icon": "https://cdn.jsdelivr.net/gh/walkxcode/dashboard-icons/svg/gmail.svg",
      "filters": [{ "type": "to", "pattern": "*@gmail.com" }]
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

The optional `icon` field sets a URL for the tab icon (e.g. from [dashboard-icons](https://github.com/walkxcode/dashboard-icons)). Tabs without an icon fall back to built-in icons or plain text.

**Filter types:**

| Type | Pattern | Matches |
|------|---------|---------|
| `from` | Glob (`*@example.com`) | Sender email address |
| `to` | Glob (`*@aristoi.ai`) | To/CC addresses |
| `subject` | Regex (`invite\|meeting`) | Subject line (falls back to substring if regex is invalid) |
| `calendar` | `*` | Emails with calendar invites |

**Match modes:** `any` (default) matches if any filter hits. `all` requires every filter to match.

The `VIMMAIL_SPLITS` environment variable can override the config file with inline JSON.

### Environment variables

All optional when using the config file.

| Variable | Description |
|----------|-------------|
| `FASTMAIL_USERNAME` | Fallback for `username` in config file |
| `FASTMAIL_API_TOKEN` | Fallback for `api-token` in config file |
| `VIMMAIL_SPLITS` | Inline JSON splits config (overrides file) |
| `XDG_CONFIG_HOME` | Config directory (default: `~/.config`) |
| `RUST_LOG` | Log level, e.g. `info`, `debug`, `vimmail=debug` |

## Search syntax

The search bar supports Gmail-style operators:

```
from:alice@example.com           # from address
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

Operators can be combined with free text: `from:@github.com is:unread pull request`

## API

All endpoints are under `/api/`. The frontend at `/` communicates with these.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/accounts` | List connected accounts |
| GET | `/api/identities` | List sender identities |
| GET | `/api/mailboxes` | List mailboxes |
| GET | `/api/emails?mailbox_id=&limit=&offset=&split_id=&search=` | List emails |
| GET | `/api/emails/{id}` | Get full email (auto marks read) |
| POST | `/api/emails/send` | Send email |
| POST | `/api/emails/{id}/archive` | Archive |
| POST | `/api/emails/{id}/trash` | Trash |
| POST | `/api/emails/{id}/mark-read` | Mark read |
| POST | `/api/emails/{id}/mark-unread` | Mark unread |
| POST | `/api/emails/{id}/toggle-flag` | Toggle star/flag |
| POST | `/api/emails/{id}/move` | Move to mailbox |
| POST | `/api/emails/{id}/rsvp` | RSVP to calendar invite |
| POST | `/api/emails/{id}/add-to-calendar` | Add invite to calendar |
| POST | `/api/emails/{id}/unsubscribe-and-archive-all` | Archive all from sender |
| GET | `/api/emails/{id}/attachments/{blob_id}/{filename}` | Download attachment |
| GET | `/api/splits` | List splits |
| POST | `/api/splits` | Create split |
| PUT | `/api/splits/{id}` | Update split |
| DELETE | `/api/splits/{id}` | Delete split |

## Testing

```sh
cargo test
```

Tests cover types, glob matching, split filtering, identity-based split seeding, search parsing, ICS calendar parsing, JMAP filter translation, and MIME detection.

## Project structure

```
src/
  main.rs        Entry point + config file parser
  lib.rs         Module declarations
  types.rs       All data types
  error.rs       Error enum + HTTP response mapping
  jmap.rs        JMAP client (connect, query, actions, send, calendar)
  routes.rs      All HTTP handlers
  search.rs      Search query parser + JMAP filter translation
  splits.rs      Split inbox filtering + config
  calendar.rs    ICS parsing + RSVP generation (hand-rolled)
  glob.rs        Glob pattern matching (hand-rolled)
  validate.rs    Validation macro
static/
  index.html     Frontend
  app.js         Frontend logic
  style.css      Styles
```
