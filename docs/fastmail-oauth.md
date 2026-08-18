# Fastmail OAuth (Sign in with Fastmail)

Supervillain supports two auth modes for Fastmail accounts (kata ngzw):

- **`auth = api-token`** (default, legacy): api-token (Bearer, JMAP-only) +
  optional app-password (Basic, CalDAV). Two pasted secrets.
- **`auth = oauth`**: one OAuth 2.0 bearer used for **both** JMAP
  (`api.fastmail.com`) and CalDAV (`caldav.fastmail.com`) — the same model
  DAVx⁵ and Morgen ship. Zero pasted secrets; tokens auto-refresh.

## Config

```ini
[personal]
provider = fastmail
auth = oauth
# username is optional — discovered from the JMAP session at first authorize
```

Then start supervillain, open Settings, and click **Authorize** (or
`POST /api/accounts/personal/authorize`). The browser opens Fastmail's
consent screen; the callback lands on `http://127.0.0.1:8402/callback`.

Switching an existing api-token account is an in-place edit: set
`auth = oauth` (the stored api-token is kept as a fallback so reverting is
just a mode flip), restart, authorize.

## Client id — maintainer registration required (one-time)

Fastmail has **no self-service OAuth client portal**. Clients are
registered manually: email Fastmail via
<https://www.fastmail.com/for-developers/> / partnerships with:

- `clientName` (e.g. "supervillain"), `logoUrl`, `clientUrl`, `tosUrl`,
  `policyUrl`, `supportUrl`
- `redirectUris`: loopback — `http://127.0.0.1:8402/callback` (Fastmail
  allows `localhost`/`127.0.0.1`/`::1` with arbitrary port substitution)
- `scopes`: `urn:ietf:params:jmap:core urn:ietf:params:jmap:mail
  urn:ietf:params:jmap:submission` — **and ask which scope covers
  CalDAV/CardDAV access**. Fastmail's published scope list is JMAP-typed
  only, yet DAVx⁵ syncs CalDAV over the OAuth bearer, so either an
  unlisted DAV scope exists or the bearer grants protocol access with
  scopes only gating JMAP data types. Confirm the exact strings.

Fastmail assigns the `client_id` by hand. Ship it as
`oauth::FASTMAIL_CLIENT_ID`; until then (or to override), set:

```sh
SUPERVILLAIN_FASTMAIL_CLIENT_ID=<assigned-id> supervillain
```

## Token lifecycle

- Endpoints: authorize `https://api.fastmail.com/oauth/authorize`, token +
  refresh `https://api.fastmail.com/oauth/refresh` (one endpoint for
  both), revoke `https://api.fastmail.com/oauth/revoke`. PKCE S256 is
  mandatory.
- Tokens live in `~/.config/supervillain/tokens/<account>.json` (0600).
- **Rotate-on-refresh**: every refresh returns a new refresh token that
  replaces the stored one; replaying an old token revokes the grant. Never
  copy a token file between machines.
- A background task (60s tick) refreshes tokens expiring within 300s and
  swaps the new bearer into the live session; `invalid_grant` clears the
  stored tokens, drops the session (authStatus → pending), and surfaces a
  re-authorize banner.

## Fallback

The api-token + app-password path is unchanged and stays supported —
Fastmail Basic plans have no CalDAV at all (OAuth doesn't add it), and the
`CalendarAuthUnconfigured` banner from kata m5yp still applies there.
