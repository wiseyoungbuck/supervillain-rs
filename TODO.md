# Supervillain TODO

## Not yet implemented

- Threading/conversation grouping (`thread_id` is populated on `Email` but unused in the list view)
- Drafts (no `/api/drafts` endpoint, no compose save/restore)
- Contact suggestions/address book autocomplete on To/Cc (the `autocompleteIndex` in `app.js` is only for search operators)
- Email signatures (no field on `Identity`, no settings UI, no auto-append)
- Sorting options — list is hardcoded `receivedAt` desc in `jmap.rs`; no sort param on `ListEmailsParams`
- Offline mode for email data — mobile service worker caches the app shell (network-first) but explicitly bypasses `api.fastmail.com`; no IndexedDB cache of messages for offline read/compose

## Mobile / iPhone testing

- [ ] Make server bind address configurable (`SUPERVILLAIN_BIND` env var or `--bind` flag), default `127.0.0.1:8000`. Needed because `/api/*` has no server-side auth beyond per-request JMAP creds — flipping to `0.0.0.0` unconditionally exposes it to the LAN.
- [ ] Test on iPhone over LAN: run with `SUPERVILLAIN_BIND=0.0.0.0:8000`, visit `http://<host-ip>:8000/mobile/` in Safari, "Add to Home Screen", verify PWA cold start.
- [ ] Verify mobile JMAP token persists across PWA cold start (currently localStorage — confirm it survives standalone launch and service-worker reload).
- [ ] Decide mobile scope: keep `/mobile/` as the iPhone story, or make desktop `static/index.html` responsive. Current split doubles maintenance.

## DMARC hardening schedule

Both originally-scheduled dates have passed (today is 2026-04-18). Current state per `dig TXT _dmarc.<domain>`:
- aristoi.ai — `p=none` (not advanced)
- mattgpt.ai — `p=none` (not advanced)
- mattcoburn.ai — `p=quarantine` (step 1 done, step 2 pending)

- [ ] Advance aristoi.ai and mattgpt.ai to `p=quarantine` (check DMARC aggregate reports first for unauthorized senders)
- [ ] After ~2 weeks of clean reports, advance all three to `p=reject`
