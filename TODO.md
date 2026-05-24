# Supervillain TODO

## Completed in Phase 5 (in-app account management + timezone-aware invites)

- **Accounts refactor** — `src/accounts.rs` extracted from `main.rs`. Typed
  `AccountConfig` enum (Fastmail / Outlook / Gmail), pure INI parse/serialize
  (round-trip safe), `atomic_write_config` → `atomic_write_bytes(path, data,
  secret)` helper (tmpfile → fsync → rename → fsync parent dir; PID + monotonic
  counter on tmp name to avoid intra-process collision; mode 0600 for secrets
  and 0644 for non-sensitive config). In-app account management routes
  (`POST/PUT/DELETE /api/accounts/{id}`, `POST /api/accounts/{id}/authorize`)
  so users add/edit/delete accounts and re-authorize OAuth from the UI;
  first run lands on the settings screen. `AccountRegistry` mirrors the
  on-disk config in memory under `tokio::sync::RwLock` so handlers don't
  re-read the file under lock. `Error::Conflict` variant for duplicate-name
  responses.
- **OAuth single-flight** — `AuthorizingGuard` RAII type around `AuthorizingSlot`
  (`std::sync::Mutex<Option<String>>`) so a panic in the OAuth callback flow
  releases the slot on Drop instead of leaking it (roborev 186 #4). Adjacent
  fix: `validate_section_name` rejects `/`, `\`, `.`, `..`, leading dot, NUL,
  control chars, brackets, newlines — section names are joined with
  `tokens_dir` as filenames, so path-safety matters. Defense in depth:
  `parse_config_str` also validates section headers at parse time (a
  hand-edited config can't smuggle a traversal id through startup);
  `token_file_path` has a `debug_assert` tripwire.
- **Timezone-aware calendar invites** — `src/timezone.rs` with
  `TimezoneConfig` persisted to `~/.config/supervillain/timezone.json`
  (atomic write via the shared helper, mode 0644). IANA validation via
  `chrono-tz`; system-TZ detection via `iana-time-zone`. Routes:
  `GET/PUT /api/timezone`, `POST /api/timezone/accept-system`,
  `POST /api/timezone/dismiss-change` (body `{ seen_system }` returns 409
  on TOCTOU mismatch so the user can't dismiss a change they never saw),
  `GET /api/timezone/zones`, `POST /api/calendar/invite`. `AppState`
  gained `timezone_write_lock: tokio::sync::Mutex<()>` to serialize the
  three timezone mutation handlers' load → mutate → save windows.
- **Calendar parser upgrade** — `parse_ics_datetime_property` tries
  `chrono_tz::Tz::from_str(tzid)` first (correct DST resolution at the
  event's instant); falls back to the existing VTIMEZONE-offset table for
  non-IANA TZIDs (e.g. Outlook's "Pacific Standard Time"). Fixes the
  documented single-offset DST limitation. Regression tests for
  summer/winter on `America/Los_Angeles`.
- **Invite + RSVP generation** — `calendar::generate_invite` (iTIP REQUEST,
  TZID-qualified DTSTART/DTEND, synthesized minimal VTIMEZONE scoped to
  the event instant — correct for one-shot events; we don't emit RRULE)
  and `calendar::generate_rsvp_with_tz` (iTIP REPLY with the responder's
  primary TZ rather than UTC-Z, so organizers see the reply in your
  locale). All text/atom fields routed through `escape_text` (handles
  `\r`, `\n`, `,`, `;`, `\\`) and `sanitize_token` / `sanitize_address` so
  an attacker-controlled summary or organizer name can't inject iCal
  properties on round-trip (roborev 188 #1B and carryover #2).
  `provider::rsvp` threads `reply_tz` through; Outlook/Gmail arms
  document why they don't use it (their Graph/Calendar APIs send RSVPs
  automatically, not via iTIP REPLY emails).
- **Frontend** — Timezone settings panel in the Settings view (system vs
  manual primary radio, IANA datalist from `GET /api/timezone/zones`,
  chip-style additional-TZ editor). `formatEventTimeMultiTz` renders one
  line per configured display TZ on every event card (primary bold,
  secondaries dimmed) using `Intl.DateTimeFormat` with `timeZone` +
  `timeZoneName: 'short'`. TZ-change banner above the email list when the
  OS TZ moves; dismiss posts the seen value back so the server can refuse
  stale dismissals. Compose modal "Attach calendar invite" toggle reveals
  summary / location / start / end / TZ fields and posts to
  `/api/calendar/invite`; attachments and `end > start` validation are
  threaded through.

## Completed in Phase 3 (Gmail provider)

- **Milestone A** — Gmail OAuth + read-only inbox. PKCE flow via shared
  `platform::acquire_oauth_callback` (port 8401), `gmail.modify`/`gmail.send`/
  `calendar` scopes, refresh-token retention. Mailbox list with 60s TTL cache
  + parallel `labels.get`. Identities via `settings.sendAs`. Bounded cursor
  pagination (`MAX_REWALK_PAGES = 20`). Concurrent `messages.get` + payload-
  tree parsing. Label→role mapping excludes STARRED/IMPORTANT/CATEGORY_*.
  `q=` translator with quoting rules and slash-format dates.
- **Milestone B** — Write actions. `mark_read`/`mark_unread`/`toggle_flag`
  (STARRED)/`archive`/`trash`/`move_to_mailbox`/`archive_batch`, each
  invalidating `label_cache`. Typed `BlobRef::{Synthetic(uuid),
  GmailAttachment{msg_id, att_id}}` in `types.rs`. `download_blob` via
  `messages.attachments.get` with extension-based MIME guessing +
  `X-Content-Type-Options: nosniff` on the route. `classify_gmail_error`
  helper: 4xx → BadRequest, 5xx → Internal.
- **Milestone C** — Compose + send. `mail-builder = "0.4"` for RFC822
  construction (multipart/alternative + multipart/mixed). Session-local
  upload cache with three caps (32 entries, 25 MiB per blob, 50 MiB
  aggregate). `peek_blob_bytes` (read-only) + `drain_consumed_synthetic_blobs`
  (post-send) so partial build failure doesn't lose synthetic blobs.
  `lookup_parent_message_id` resolves Gmail message IDs to RFC822
  `Message-ID:` headers for In-Reply-To threading.
- **Milestone D** — Google Calendar v3 + RSVP. `get_calendar_event`,
  `add_to_calendar` (via `events.import`, preserves `iCalUID`),
  `remove_from_calendar` (404-tolerant), `respond_to_event` (read-modify-write
  PATCH attendees with `sendUpdates=all`). `mutate_attendee_status` pure
  helper pins Google's "PATCH must include full array" quirk.
  `get_calendar_data` extracts `text/calendar` from message payload.
  `sends_rsvp_automatically()` true for Gmail.
- **Milestone E** — Polish. 401-on-revoke token clearing: `ensure_token`
  detects `invalid_grant` and calls `clear_stored_tokens` so the next launch
  re-runs OAuth instead of looping on a doomed refresh. Pure
  `should_clear_tokens_on_refresh_failure(status, body)` helper for testability.

Plus: platform abstraction (`src/platform/`) with `TokenStore` trait,
`FsTokenStore` chmod 0600/0700 on Unix, `acquire_oauth_callback` with
5-min timeout and error-redirect handling. Cargo deps tightened for iOS
portability (rustls-tls; explicit tokio features).

## Completed in Phase 4 (Outlook email)

- **PR 0** — `src/provider_utils.rs` extraction. `mime_type_from_filename`,
  `encode_path_segment`, `should_clear_tokens_on_refresh_failure`, and the
  upload-cap constants (`UPLOAD_CACHE_CAP = 32`, `MAX_BLOB_BYTES = 25 MiB`,
  `MAX_UPLOAD_CACHE_BYTES = 50 MiB`) moved out of `gmail.rs` so Outlook and
  Gmail share one tuning point. Gmail tests for these helpers moved with them.
- **Milestone A** — Read-only Outlook inbox + 401 clearing. `get_mailboxes`,
  `get_identities`, `query_emails`, `get_emails`, `get_calendar_data` against
  Microsoft Graph. `OutlookSession` extended with `folder_cache` (60s TTL)
  and `page_cache` (opaque `@odata.nextLink` URLs). New `BlobRef::OutlookAttachment`
  variant with `outlook:` parse prefix and base64-aware URL safety.
  `translate_query_to_odata` splits free-text into `$search` and structured
  terms into `$filter` (KQL escape rules pinned by tests).
- **Milestone B** — Outlook write actions. `mark_read`/`mark_unread`/`toggle_flag`/
  `archive`/`trash`/`move_to_mailbox`/`archive_batch`, each invalidating
  `folder_cache` + `page_cache` via `invalidate_caches_after_mutation`.
  `archive_batch` chunks at Graph's 20-per-`/$batch` cap; partial failures
  invalidate caches per-chunk. `move_plan_outlook` rejects system folders
  (Drafts, Sent, Junk) by both well-known name and resolved opaque ID.
  `download_blob` via `/me/messages/{id}/attachments/{aid}/$value`.
- **Milestone C** — Outlook send/compose. Three-path send via Graph's typed
  Message resource (no RFC822, no mail-builder): `POST /me/sendMail` for new
  mail, `POST /me/messages/{id}/reply` for HTML-only no-attachment replies
  (1 RTT), `createReply → PATCH → send` for replies with attachments or
  plain-text bodies (3 RTTs). `pick_send_path`, `build_graph_message_with_from_identity`,
  `build_graph_reply_patch_body`, and `format_send_failure_with_cleanup`
  pure helpers pin every branch. Orphan-draft cleanup with `DraftCleanup`
  enum: `AlreadyGone` surfaces "Check Sent Items before resending to avoid
  a duplicate" when the prior `/send` may have succeeded despite a network
  error. Upload cache mirrors Gmail's caps.
- **Milestone D** — Polish. Pure `pick_outlook_display_name` mirrors Gmail's
  identity-picker discipline: explicit `identity_id_override` that doesn't
  match an identity returns `None` + `tracing::warn!` (refuses to mislabel
  From). `build_graph_message_with_from_identity` threads the resolved name
  onto `from.emailAddress.name`. `identity_id_override` now flows through
  `provider::send_email` to Outlook (previously discarded). README + this
  TODO updated to reflect full Outlook parity with Gmail/Fastmail.

## Completed in Phase 5 (in-app account management + timezone-aware invites)

- **In-app account management** — New `src/accounts.rs`: typed `AccountConfig`
  enum (Fastmail / Outlook / Gmail), pure INI parse/serialize, `atomic_write_config`
  (tmpfile → fsync → rename → parent-dir fsync, per-call `AtomicU64` seq counter
  so concurrent same-PID writers can't clobber, mode 0600), path-traversal-safe
  `validate_section_name` (rejects `/`, `\`, `.`, `..`, leading dot, NUL,
  control chars, brackets, `=`, `#`, newlines; enforced at parse time too so
  hand-edited configs can't smuggle traversal names). Routes:
  `POST /api/accounts/{id}` (upsert; new Fastmail connects sync OUTSIDE the
  registry lock; OAuth providers return `authStatus: pending`),
  `DELETE /api/accounts/{id}` (removes session + token file, promotes
  alphabetically-first remaining account to default), `PUT /api/accounts/{id}/default`,
  `POST /api/accounts/{id}/authorize` (long-poll, single-flight via
  `AuthorizingGuard` RAII so panics can't leak the slot). `AppState.accounts`
  is now `RwLock<AccountRegistry>` with an in-memory `account_configs` cache
  so handlers never re-read disk under the write lock. First run with no
  config no longer `exit(1)`s — it surfaces a `setup` sentinel error and the
  frontend auto-routes to the settings view. Frontend: `#settings-view`
  (master/detail in `static/index.html`), `g s` chord + `:settings` command,
  settings-mode keybindings (`a` add, `d` delete + confirm, `Shift+D` set
  default, `Ctrl+Enter` save, `Esc` back), `api()` allowlist regex so settings
  routes don't get auto-tagged with `?account=`. `clientId` echoed on the GET
  response so editing OAuth accounts doesn't make the user retype it; secrets
  never echoed.
- **Timezone-aware calendar invites** — New `src/timezone.rs`: `TimezoneConfig`
  persisted to `~/.config/supervillain/timezone.json` via `atomic_write_bytes`,
  IANA validation via `chrono-tz`, system-TZ detection via `iana-time-zone`,
  primary + additional display zones. Routes: `GET/PUT /api/timezone`,
  `POST /api/timezone/accept-system`, `POST /api/timezone/dismiss-change`
  (`seen_system` TOCTOU check → 409 if system TZ moved between banner display
  and click), `GET /api/timezone/zones`. `AppState.timezone_write_lock`
  serializes load→mutate→save so concurrent updates can't lose writes.
  `calendar.rs` gained `generate_invite` (iTIP REQUEST with `DTSTART;TZID=...`,
  synthesized VTIMEZONE including `X-LIC-LOCATION` per libical/RFC 7808 so
  strict parsers caching VTIMEZONE by TZID can map back to IANA) and
  `generate_rsvp_with_tz` (REPLY in user's primary TZ instead of UTC-Z).
  `chrono-tz::Tz::from_str` resolves `TZID` first in the parser (fixes
  documented single-offset-per-TZID DST limitation); VTIMEZONE-offset fallback
  retained for non-IANA labels. ICS-injection hardening: `escape_param_value`
  strips CR/LF/control/`:`/`"` and DQUOTE-wraps when `,`/`;`/space present;
  `sanitize_address` strips CR/LF/control/`,`/`;`/`"` so CRLF injection in
  attendee names or organizer addresses can't produce a second property
  line. `POST /api/calendar/invite` rejects `dtend <= dtstart` and threads
  attachments through to the outbound mail (compose toggle "Attach calendar
  invite" in the frontend). `provider::rsvp` doc-comment now explicitly
  documents that `reply_tz` is Fastmail-only (Outlook/Gmail use Graph/Calendar
  PATCH which renders in the recipient's TZ).
- **Roborev 186 + 188 hardening** — applied throughout the above: see commits
  `d00dd60` and `ab7765d`.

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
