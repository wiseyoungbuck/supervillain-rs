# Changelog

Retrospective record of shipped work. Append-only; phases bundle features that
shipped together for sequencing reasons, not necessarily for architectural
ones.

## Outlook OAuth — switch to `/consumers` audience

Outlook sign-in now targets the `/consumers` OAuth endpoint instead of
`/common`. Microsoft blocks end-user consent to newly registered multitenant
apps until the publisher is verified (MPN ID), which is impractical for
self-hosted single-user installs. The matching app registration must use
"Personal Microsoft accounts only" — see README → Azure AD App Registration.

`User.Read` is now requested alongside the existing Mail/Calendar scopes so
Graph `/me` reliably returns the user's email; `fetch_user_email` also
checks the HTTP status before parsing and falls back to `otherMails[0]` for
personal accounts where `mail`/`userPrincipalName` can be null. Existing
Outlook users must delete `~/.config/supervillain/tokens/<account>.json` and
re-authorize, since the prior token grant lacks `User.Read` and was issued
against the wrong tenant.

## Phase 6 — Startup config error surfacing

Account config (`~/.config/supervillain/config`) parse errors are now
surfaced in a UI banner at startup instead of silently dropping malformed
sections. Same shape for `splits.json` and `timezone.json` parse failures
(missing files are still fine; only malformed content reports).

Behavior change worth noting for hand-edited configs: a section that omits
the `provider = ...` line is now reported as `missing required field
provider` and skipped, where it previously defaulted to `fastmail` and
emitted a misleading `missing required field username` instead. The
serializer always writes `provider =`, so any UI-created config is
unaffected — this only matters if you wrote the file by hand and omitted
the line.

XSS hardening on the parse-error UI surface: section names rejected by
`validate_section_name` are replaced with `<malformed section>` in the
banner (the original is kept in the `tracing::warn!` line for operator
debugging); unknown / hostile provider strings are replaced with
`<unknown provider>` for the same reason. Both close attribute-context
escape vectors against the UI's `escapeHtml` (which does not encode
`"` / `'`).

## Phase 5 — In-app account management + timezone-aware invites

Two largely orthogonal features that shipped together in commits `d00dd60` and
`ab7765d`. They share an `atomic_write_bytes` helper (extracted from
`atomic_write_config` so `src/timezone.rs` can reuse the same crash-safety
guarantees) but are otherwise independent.

**In-app account management** — New `src/accounts.rs`: typed `AccountConfig`
enum (Fastmail / Outlook / Gmail), pure INI parse/serialize,
`atomic_write_config` (tmpfile → fsync → rename → parent-dir fsync, per-call
`AtomicU64` seq counter so concurrent same-PID writers can't clobber, mode
0600), path-traversal-safe `validate_section_name` (see the doc-comment in
`src/accounts.rs` for the canonical rule list; enforced at parse time too so
hand-edited configs can't smuggle traversal names). Routes:

- `POST /api/accounts/{id}` — upsert; new Fastmail connects sync OUTSIDE the
  registry lock; OAuth providers return `authStatus: pending`.
- `DELETE /api/accounts/{id}` — removes session + token file, promotes the
  alphabetically-first remaining account to default.
- `PUT /api/accounts/{id}/default` — idempotent default setter.
- `POST /api/accounts/{id}/authorize` — long-poll, single-flight via
  `AuthorizingGuard` RAII so panics can't leak the slot (roborev 186 #4).

`AppState.accounts` is now `RwLock<AccountRegistry>` with an in-memory
`account_configs` cache so handlers never re-read disk under the write lock.
First run with no config no longer `exit(1)`s — it surfaces a `setup` sentinel
error and the frontend auto-routes to the settings view. Frontend:
`#settings-view` (master/detail in `static/index.html`), `g s` chord +
`:settings` command, settings-mode keybindings (`a` add, `d` delete + confirm,
`Shift+D` set default, `Ctrl+Enter` save, `Esc` back), `api()` allowlist regex
so settings routes don't get auto-tagged with `?account=`. `clientId` echoed
on the GET response so editing OAuth accounts doesn't make the user retype it;
secrets never echoed.

**Timezone-aware calendar invites** — New `src/timezone.rs`: `TimezoneConfig`
persisted to `~/.config/supervillain/timezone.json` via `atomic_write_bytes`,
IANA validation via `chrono-tz`, system-TZ detection via `iana-time-zone`,
primary + additional display zones. Routes:

- `GET/PUT /api/timezone`
- `POST /api/timezone/accept-system`
- `POST /api/timezone/dismiss-change` — `seen_system` TOCTOU check → 409 if
  the system TZ moved between banner display and click.
- `GET /api/timezone/zones`

`AppState.timezone_write_lock` serializes load → mutate → save so concurrent
updates can't lose writes.

Calendar changes:

- `calendar::generate_invite` (iTIP REQUEST with `DTSTART;TZID=...`,
  synthesized VTIMEZONE including `X-LIC-LOCATION` per libical/RFC 7808 so
  strict parsers caching VTIMEZONE by TZID can map back to IANA).
- `calendar::generate_rsvp_with_tz` (REPLY in user's primary TZ instead of
  UTC-Z).
- `chrono_tz::Tz::from_str` resolves `TZID` first in the parser (fixes
  documented single-offset-per-TZID DST limitation); VTIMEZONE-offset
  fallback retained for non-IANA labels.
- ICS-injection hardening: `escape_param_value` and `sanitize_address` strip
  CR/LF/control chars so attacker-controlled summaries or organizer names
  can't inject a second property line (roborev 188 #1B + carryover #2).
- `POST /api/calendar/invite` rejects `dtend <= dtstart` and threads
  attachments through.
- `provider::rsvp` doc-comment documents that `reply_tz` is Fastmail-only
  (Outlook/Gmail use Graph/Calendar PATCH which renders in the recipient's TZ).

## Phase 4 — Outlook email

- **PR 0** — `src/provider_utils.rs` extraction. `mime_type_from_filename`,
  `encode_path_segment`, `should_clear_tokens_on_refresh_failure`, and the
  upload-cap constants (`UPLOAD_CACHE_CAP = 32`, `MAX_BLOB_BYTES = 25 MiB`,
  `MAX_UPLOAD_CACHE_BYTES = 50 MiB`) moved out of `gmail.rs` so Outlook and
  Gmail share one tuning point. Gmail tests for these helpers moved with them.
- **Milestone A** — Read-only Outlook inbox + 401 clearing. `get_mailboxes`,
  `get_identities`, `query_emails`, `get_emails`, `get_calendar_data` against
  Microsoft Graph. `OutlookSession` extended with `folder_cache` (60s TTL) and
  `page_cache` (opaque `@odata.nextLink` URLs). New `BlobRef::OutlookAttachment`
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
  plain-text bodies (3 RTTs). `pick_send_path`,
  `build_graph_message_with_from_identity`, `build_graph_reply_patch_body`,
  and `format_send_failure_with_cleanup` pure helpers pin every branch.
  Orphan-draft cleanup with `DraftCleanup` enum: `AlreadyGone` surfaces
  "Check Sent Items before resending to avoid a duplicate" when the prior
  `/send` may have succeeded despite a network error. Upload cache mirrors
  Gmail's caps.
- **Milestone D** — Polish. Pure `pick_outlook_display_name` mirrors Gmail's
  identity-picker discipline: explicit `identity_id_override` that doesn't
  match an identity returns `None` + `tracing::warn!` (refuses to mislabel
  From). `build_graph_message_with_from_identity` threads the resolved name
  onto `from.emailAddress.name`. `identity_id_override` now flows through
  `provider::send_email` to Outlook (previously discarded).

## Phase 3 — Gmail provider

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
  `remove_from_calendar` (404-tolerant), `respond_to_event`
  (read-modify-write PATCH attendees with `sendUpdates=all`).
  `mutate_attendee_status` pure helper pins Google's "PATCH must include
  full array" quirk. `get_calendar_data` extracts `text/calendar` from
  message payload. `sends_rsvp_automatically()` true for Gmail.
- **Milestone E** — Polish. 401-on-revoke token clearing: `ensure_token`
  detects `invalid_grant` and calls `clear_stored_tokens` so the next launch
  re-runs OAuth instead of looping on a doomed refresh. Pure
  `should_clear_tokens_on_refresh_failure(status, body)` helper for
  testability.

Plus: platform abstraction (`src/platform/`) with `TokenStore` trait,
`FsTokenStore` chmod 0600/0700 on Unix, `acquire_oauth_callback` with
5-min timeout and error-redirect handling. Cargo deps tightened for iOS
portability (rustls-tls; explicit tokio features).
