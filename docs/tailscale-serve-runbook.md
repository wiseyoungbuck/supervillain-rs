# Runbook: serving Supervillain over the tailnet (HTTPS) + PWA install

Operator runbook for kata **8e3w** — the last, manual step of the Mobile v2
close-out chain. All the *code* for this ticket has shipped; what remains is
one `tailscale serve` command at the machine and the click-through checklist
below. Completing it unblocks **3fh5** (the on-device iPhone test) and with
it the **1v8z** epic gate.

## What is already in place (no action needed)

Verified on `track/d` at the time of writing:

- **Loopback-default bind.** `bind_addr()` (`src/main.rs`) reads
  `SUPERVILLAIN_BIND` and defaults to `127.0.0.1:8000`. There is no auth
  layer in the server; the tailnet is the auth and transport boundary, and
  the server's job ends at loopback. Do **not** add TLS termination, auth
  middleware, or a config file to the server itself.
- **Versioned service-worker cache.** `static/mobile/sw.js` embeds the crate
  version in `CACHE_NAME` (substituted at serve time by the `mobile_sw`
  route), old caches are deleted on `activate`, and the
  `/mobile/index.html` alias is in `APP_SHELL` — no manual cache-name bump,
  no stranded stale clients after a deploy.
- **Secure-context-gated SW registration.** `static/mobile/app.js` checks
  `window.isSecureContext` before `serviceWorker.register(...)` — over plain
  http the registration is skipped visibly rather than failing silently.
- **README section** "Serving over the tailnet (HTTPS)" documents the serve
  command, `SUPERVILLAIN_BIND`, and the non-tailscale Caddy fallback.

## `SUPERVILLAIN_BIND` reference

| Value | Effect |
|---|---|
| *(unset)* | `127.0.0.1:8000` — loopback only. The right setting for tailnet serving. |
| `127.0.0.1:9000` | Loopback on another port (adjust the serve command to match). |
| `0.0.0.0:8000` | Exposes the **unauthenticated** API to every network the machine joins. Only for deliberate LAN exposure; never needed for tailscale serve. |

The launcher and `scripts/upgrade.sh` both leave it unset (loopback).

## Setup (~5 minutes at the machine)

1. **One-time tailnet enablement.** HTTPS serving must be enabled once for
   the tailnet in the Tailscale admin console. If it isn't, step 2's CLI
   prints the enablement link — open it, click enable, re-run the command.
   (As of 2026-07-28 this click was still pending; the CLI printed the link.)

2. **Start the serve proxy** (persists across reboots until cleared):

   ```sh
   tailscale serve --bg --https=443 http://127.0.0.1:8000
   ```

3. **Confirm it took:**

   ```sh
   tailscale serve status
   ```

   Expected: an HTTPS 443 → `http://127.0.0.1:8000` mapping. `No serve
   config` means step 2 didn't stick (usually the step-1 click is missing).

4. The app is now at `https://<host>.<tailnet>.ts.net/` (desktop UI) and
   `https://<host>.<tailnet>.ts.net/mobile/` (mobile PWA), with a valid
   certificate, from any device on the tailnet.

To undo: `tailscale serve --https=443 off` (or `tailscale serve reset`).

### Non-tailscale fallback

Nothing in Supervillain assumes Tailscale. Any TLS-terminating reverse
proxy pointed at loopback works, e.g. Caddy:

```
mail.example.com {
    reverse_proxy 127.0.0.1:8000
}
```

## PWA install (iPhone, on the tailnet)

1. Ensure the phone is on the tailnet (Tailscale app connected; the
   target machine visible under Machines).
2. Safari → `https://<host>.<tailnet>.ts.net/mobile/` — the account list
   renders with no login screen (the tailnet **is** the login).
3. Share → **Add to Home Screen** → Add.
4. Launch from the new icon: it opens standalone (no Safari chrome).

## Live-verify checklist (click through, in order)

- [ ] `tailscale serve status` shows the 443 → `127.0.0.1:8000` mapping.
- [ ] From the phone (on the tailnet), `https://<host>.<tailnet>.ts.net/mobile/`
      loads and lists accounts.
- [ ] The service worker registered under the secure context — either
      Settings → Safari → Advanced → Website Data shows an entry for the
      `.ts.net` host, or `navigator.serviceWorker.getRegistrations()` from a
      connected Web Inspector returns one registration.
- [ ] Add to Home Screen → launching from the icon opens standalone.
- [ ] After the next `./scripts/upgrade.sh`, a relaunch picks up the new
      version without a manual cache clear (the versioned `CACHE_NAME` doing
      its job).

When every box is checked: close **8e3w**, then run the full on-device
daily-drive script recorded on **3fh5** (read/triage/compose/search/RSVP/
background-resume), whose pass closes 3fh5 and the 1v8z gate.
