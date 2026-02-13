# Supervillain

A keyboard-driven email client with vim keybindings. Rust backend + vanilla JS frontend, connecting to Fastmail via JMAP.

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

3. Build and run:

```sh
cargo build --release
cargo run --release
```

Open http://127.0.0.1:8000 in your browser.

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

### Splits (inbox tabs)

Splits let you filter your inbox into tabs (like Superhuman's splits). Config is stored at `~/.config/supervillain/splits.json`.

You can manage splits through the UI or edit the JSON directly:

```json
{
  "splits": [
    {
      "id": "newsletters",
      "name": "Newsletters",
      "match_mode": "any",
      "filters": [
        { "type": "from", "pattern": "*@substack.com" },
        { "type": "from", "pattern": "noreply@medium.com" },
        { "type": "subject", "pattern": "newsletter|digest|weekly" }
      ]
    },
    {
      "id": "calendar",
      "name": "Calendar",
      "filters": [
        { "type": "calendar", "pattern": "*" }
      ]
    }
  ]
}
```

**Filter types:**

| Type | Pattern | Matches |
|------|---------|---------|
| `from` | Glob (`*@example.com`) | Sender email address |
| `to` | Glob (`team-*@company.com`) | To/CC addresses |
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
| GET | `/api/splits` | List splits |
| POST | `/api/splits` | Create split |
| PUT | `/api/splits/{id}` | Update split |
| DELETE | `/api/splits/{id}` | Delete split |

## Testing

```sh
cargo test
```

119 tests covering types, glob matching, split filtering, search parsing, ICS calendar parsing, JMAP filter translation, and MIME detection.

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
