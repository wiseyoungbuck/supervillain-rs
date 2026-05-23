# Supervillain TODO

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
