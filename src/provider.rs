use crate::error::Error;
use crate::gmail::GmailSession;
use crate::jmap::JmapSession;
use crate::outlook::OutlookSession;
use crate::types::*;
use crate::{calendar, gmail, jmap, outlook};

// =============================================================================
// Provider Session — concrete enum, no traits
// =============================================================================

pub enum ProviderSession {
    /// All variants are boxed so `ProviderSession` itself stays small
    /// (one tag + one pointer). Without the boxes the enum's size is
    /// the size of the largest variant, which clippy flags as
    /// `large-enum-variant` once any provider gains another cache.
    Fastmail(Box<JmapSession>),
    Outlook(Box<OutlookSession>),
    Gmail(Box<GmailSession>),
}

impl ProviderSession {
    /// The email address / username for this provider
    pub fn username(&self) -> &str {
        match self {
            Self::Fastmail(s) => &s.username,
            Self::Outlook(s) => &s.email,
            Self::Gmail(s) => &s.email,
        }
    }

    pub fn provider_name(&self) -> &str {
        match self {
            Self::Fastmail(_) => "fastmail",
            Self::Outlook(_) => "outlook",
            Self::Gmail(_) => "gmail",
        }
    }

    /// Whether this provider sends RSVP emails automatically (via Graph API)
    /// so the caller should NOT send a manual iTIP reply.
    /// Gmail PATCHes Calendar attendees with sendUpdates=all (Milestone D),
    /// so it counts as auto-RSVP from day one of that milestone.
    pub fn sends_rsvp_automatically(&self) -> bool {
        matches!(self, Self::Outlook(_) | Self::Gmail(_))
    }
}

// =============================================================================
// Dispatch functions — mechanical match arms
// =============================================================================

pub async fn get_mailboxes(s: &ProviderSession) -> Result<Vec<Mailbox>, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::get_mailboxes(s).await,
        ProviderSession::Outlook(s) => outlook::get_mailboxes(s).await,
        ProviderSession::Gmail(s) => gmail::get_mailboxes(s).await,
    }
}

/// Ensure the shared Reminders mailbox/label exists using the provider's
/// native folder primitive.
pub async fn ensure_reminders_mailbox(s: &mut ProviderSession) -> Result<String, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::ensure_mailbox_by_name(s, "Reminders").await,
        ProviderSession::Outlook(s) => outlook::ensure_mailbox_by_name(s, "Reminders").await,
        ProviderSession::Gmail(s) => gmail::ensure_mailbox_by_name(s, "Reminders").await,
    }
}

/// Move one message into the provider-native Reminders mailbox. All folder
/// creation and move mechanics stay behind this dispatch boundary.
pub async fn remind(s: &mut ProviderSession, email_id: &str) -> Result<bool, Error> {
    let reminders_id = ensure_reminders_mailbox(s).await?;
    move_to_mailbox(s, email_id, &reminders_id).await
}

/// Move a reminder back to the Inbox and let the caller delete its durable
/// schedule row.
pub async fn cancel_reminder(s: &ProviderSession, email_id: &str) -> Result<bool, Error> {
    move_to_inbox(s, email_id).await
}

/// Move a reminder back to the Inbox. The Inbox is deliberately resolved by
/// role/name rather than using the recorded source: Superhuman always wakes
/// into Inbox, even when the message was originally filed elsewhere.
pub async fn move_to_inbox(s: &ProviderSession, email_id: &str) -> Result<bool, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::move_reminder_to_inbox(s, email_id).await,
        ProviderSession::Gmail(s) => gmail::move_reminder_to_inbox(s, email_id).await,
        ProviderSession::Outlook(s) => {
            let inbox = outlook::get_mailboxes(s)
                .await?
                .into_iter()
                .find(|mailbox| mailbox.role.as_deref() == Some("inbox"))
                .ok_or_else(|| Error::Internal("No Inbox mailbox".into()))?;
            outlook::move_to_mailbox(s, email_id, &inbox.id).await
        }
    }
}

/// Fetch the messages currently resting in the provider-native Reminders
/// mailbox. Missing mailbox is an empty list, not an error: no reminder has
/// been created yet for this account.
pub async fn list_reminders(s: &ProviderSession) -> Result<Vec<Email>, Error> {
    let mailbox = get_mailboxes(s)
        .await?
        .into_iter()
        .find(|mailbox| mailbox.name == "Reminders");
    let Some(mailbox) = mailbox else {
        return Ok(Vec::new());
    };
    let ids = query_emails(s, Some(&mailbox.id), 500, 0, None, EmailSort::DateDesc).await?;
    get_emails(s, &ids, false, None, false).await
}

pub async fn get_identities(s: &mut ProviderSession) -> Result<Vec<Identity>, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::get_identities(s).await,
        ProviderSession::Outlook(s) => outlook::get_identities(s).await,
        ProviderSession::Gmail(s) => gmail::get_identities(s).await,
    }
}

pub async fn query_emails(
    s: &ProviderSession,
    mailbox_id: Option<&str>,
    limit: usize,
    position: usize,
    query: Option<&ParsedQuery>,
    sort: EmailSort,
) -> Result<Vec<String>, Error> {
    match s {
        ProviderSession::Fastmail(s) => {
            jmap::query_emails(s, mailbox_id, limit, position, query, sort).await
        }
        ProviderSession::Outlook(s) => {
            outlook::query_emails(s, mailbox_id, limit, position, query, sort).await
        }
        ProviderSession::Gmail(s) => {
            gmail::query_emails(s, mailbox_id, limit, position, query, sort).await
        }
    }
}

/// `priority: true` puts the underlying requests on the limiter's reserved
/// interactive lane (see `RateLimiter::execute_prioritized`). Use it only
/// for fetches a user is actively waiting on — a single email open, an
/// RSVP — never for bulk fan-outs, which would starve the lane.
pub async fn get_emails(
    s: &ProviderSession,
    ids: &[String],
    fetch_body: bool,
    properties_override: Option<&[&str]>,
    priority: bool,
) -> Result<Vec<Email>, Error> {
    match s {
        ProviderSession::Fastmail(s) => {
            // JMAP batches the whole slice into one Email/get, so there is
            // no per-id fan-out to jump ahead of — priority isn't plumbed.
            let _ = priority;
            jmap::get_emails(s, ids, fetch_body, properties_override).await
        }
        ProviderSession::Outlook(s) => {
            // properties_override is JMAP-specific; Graph returns the full
            // message resource with $expand=attachments.
            let _ = properties_override;
            outlook::get_emails(s, ids, fetch_body, priority).await
        }
        ProviderSession::Gmail(s) => {
            // A properties override marks metadata-only bulk work (currently
            // split counts). Gmail has no arbitrary property projection, but
            // it can preserve the same intent with format=metadata and by
            // skipping calendar MIME enrichment.
            let include_invite_data = properties_override.is_none();
            gmail::get_emails(s, ids, fetch_body, include_invite_data, priority).await
        }
    }
}

/// Chunk size for [`get_emails_chunked`] on providers whose `get_emails`
/// fans out one request per id (Gmail, Outlook). ~25 metadata fetches ≈ 2 s
/// on a rate-limited Gmail account (5 concurrent × 80 ms spacing), which
/// bounds how long any one read guard is held.
pub const GET_EMAILS_CHUNK: usize = 25;

/// Chunk size for JMAP (Fastmail), whose `get_emails` batches the whole id
/// slice into ONE `Email/get` round trip. A per-id-tuned chunk of 25 would
/// multiply its request count (6 calls for a 150-id list, 60 for a 1500-id
/// split-count sample) for no guard-hold benefit — one JMAP call already
/// holds the guard for just a single round trip (roborev 307 #2). 500
/// matches the pre-chunking split-counts batch size.
pub const JMAP_GET_EMAILS_CHUNK: usize = 500;

/// Like [`get_emails`], but re-acquires the session read guard per chunk of
/// ids instead of holding one guard across the whole fan-out.
///
/// Long fan-outs (a 150-message list refresh, a 1500-message split-count
/// sample) used to pin a read guard for their full duration — minutes on a
/// rate-limited Gmail account. tokio's `RwLock` queues fairly, so a writer
/// (most visibly `send_email_handler`, which needs `write()`) queued behind
/// such a guard stalled until the entire fan-out finished, and the UI showed
/// nothing. Releasing between chunks lets a queued writer in within one
/// chunk's latency instead.
pub async fn get_emails_chunked(
    session_lock: &crate::types::SessionLock,
    ids: &[String],
    fetch_body: bool,
    properties_override: Option<&[&str]>,
    chunk_size: usize,
) -> Result<Vec<Email>, Error> {
    // The caller's chunk_size is tuned for per-id fan-out providers; see
    // JMAP_GET_EMAILS_CHUNK for why Fastmail overrides it.
    let chunk_size = {
        let session = session_lock.read().await;
        match &*session {
            ProviderSession::Fastmail(_) => JMAP_GET_EMAILS_CHUNK,
            _ => chunk_size.max(1),
        }
    };
    let mut out = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(chunk_size) {
        let session = session_lock.read().await;
        // Chunked fetches are bulk by definition (lists, split counts,
        // warm passes) — always the main limiter pool, never the
        // interactive priority lane.
        out.extend(get_emails(&session, chunk, fetch_body, properties_override, false).await?);
    }
    Ok(out)
}

pub async fn mark_read(s: &ProviderSession, email_id: &str) -> Result<bool, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::mark_read(s, email_id).await,
        ProviderSession::Outlook(s) => outlook::mark_read(s, email_id).await,
        ProviderSession::Gmail(s) => gmail::mark_read(s, email_id).await,
    }
}

pub async fn mark_unread(s: &ProviderSession, email_id: &str) -> Result<bool, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::mark_unread(s, email_id).await,
        ProviderSession::Outlook(s) => outlook::mark_unread(s, email_id).await,
        ProviderSession::Gmail(s) => gmail::mark_unread(s, email_id).await,
    }
}

pub async fn toggle_flag(s: &ProviderSession, email_id: &str) -> Result<bool, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::toggle_flag(s, email_id).await,
        ProviderSession::Outlook(s) => outlook::toggle_flag(s, email_id).await,
        ProviderSession::Gmail(s) => gmail::toggle_flag(s, email_id).await,
    }
}

pub async fn archive(s: &ProviderSession, email_id: &str) -> Result<bool, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::archive(s, email_id).await,
        ProviderSession::Outlook(s) => outlook::archive(s, email_id).await,
        ProviderSession::Gmail(s) => gmail::archive(s, email_id).await,
    }
}

pub async fn trash(s: &ProviderSession, email_id: &str) -> Result<bool, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::trash(s, email_id).await,
        ProviderSession::Outlook(s) => outlook::trash(s, email_id).await,
        ProviderSession::Gmail(s) => gmail::trash(s, email_id).await,
    }
}

pub async fn move_to_mailbox(
    s: &ProviderSession,
    email_id: &str,
    mailbox_id: &str,
) -> Result<bool, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::move_to_mailbox(s, email_id, mailbox_id).await,
        ProviderSession::Outlook(s) => outlook::move_to_mailbox(s, email_id, mailbox_id).await,
        ProviderSession::Gmail(s) => gmail::move_to_mailbox(s, email_id, mailbox_id).await,
    }
}

pub async fn archive_batch(s: &ProviderSession, email_ids: &[String]) -> Result<usize, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::archive_batch(s, email_ids).await,
        ProviderSession::Outlook(s) => outlook::archive_batch(s, email_ids).await,
        ProviderSession::Gmail(s) => gmail::archive_batch(s, email_ids).await,
    }
}

pub async fn send_email(
    s: &mut ProviderSession,
    sub: &EmailSubmission,
    from_addr: &str,
    identity_id_override: Option<&str>,
) -> Result<Option<String>, Error> {
    match s {
        ProviderSession::Fastmail(s) => {
            jmap::send_email(s, sub, from_addr, identity_id_override).await
        }
        ProviderSession::Outlook(s) => {
            // Roborev 181 #5: honor from_addr for shared-mailbox /
            // send-as scenarios. Pass through to outlook::send_email
            // which sets message.from.emailAddress when non-empty.
            // Milestone D.1: also thread identity_id_override so the
            // Outlook picker can validate the override against the
            // identity list (currently single-element via /me) and
            // refuse to fall back to a different identity.
            let from = if from_addr.is_empty() {
                None
            } else {
                Some(from_addr)
            };
            outlook::send_email(s, sub, from, identity_id_override).await
        }
        ProviderSession::Gmail(s) => {
            gmail::send_email(s, sub, from_addr, identity_id_override).await
        }
    }
}

// =============================================================================
// Persistent drafts (kata wm57) — Fastmail-only in v1
// =============================================================================
//
// Only the Fastmail (JMAP) arm is implemented. Gmail and Outlook return a
// clear BadRequest so the client can surface "not supported for this provider
// yet" instead of a generic failure. The client gates the whole draft feature
// on `provider === 'fastmail'`, so these arms are defense in depth.

fn drafts_unsupported(s: &ProviderSession) -> Error {
    Error::BadRequest(format!(
        "drafts are not supported for {} yet",
        s.provider_name()
    ))
}

pub async fn create_draft(
    s: &ProviderSession,
    sub: &EmailSubmission,
    from_addr: &str,
) -> Result<String, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::create_draft(s, sub, from_addr).await,
        other => Err(drafts_unsupported(other)),
    }
}

pub async fn update_draft(
    s: &ProviderSession,
    draft_id: &str,
    sub: &EmailSubmission,
    from_addr: &str,
) -> Result<String, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::update_draft(s, draft_id, sub, from_addr).await,
        other => Err(drafts_unsupported(other)),
    }
}

pub async fn destroy_draft(s: &ProviderSession, draft_id: &str) -> Result<bool, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::destroy_draft(s, draft_id).await,
        other => Err(drafts_unsupported(other)),
    }
}

pub async fn get_calendar_data(
    s: &ProviderSession,
    email_id: &str,
) -> Result<Option<String>, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::get_calendar_data(s, email_id).await,
        ProviderSession::Outlook(s) => outlook::get_calendar_data(s, email_id).await,
        ProviderSession::Gmail(s) => gmail::get_calendar_data(s, email_id).await,
    }
}

/// Detached calendar-read capability used by list enrichment so the provider
/// session RwLock is not held across CalDAV/Graph/Google Calendar I/O.
#[derive(Clone)]
pub(crate) enum CalendarEventReader {
    Fastmail(jmap::CalendarEventReader),
    Outlook(outlook::CalendarEventReader),
    Gmail(gmail::CalendarEventReader),
}

pub(crate) async fn calendar_event_reader(
    session: &ProviderSession,
) -> Result<CalendarEventReader, Error> {
    match session {
        ProviderSession::Fastmail(session) => Ok(CalendarEventReader::Fastmail(
            jmap::calendar_event_reader(session)?,
        )),
        ProviderSession::Outlook(session) => Ok(CalendarEventReader::Outlook(
            outlook::calendar_event_reader(session).await?,
        )),
        ProviderSession::Gmail(session) => Ok(CalendarEventReader::Gmail(
            gmail::calendar_event_reader(session).await?,
        )),
    }
}

pub(crate) async fn get_calendar_event_with_reader(
    reader: &CalendarEventReader,
    uid: &str,
    primary_tz: chrono_tz::Tz,
) -> Result<Option<CalendarEvent>, Error> {
    match reader {
        CalendarEventReader::Fastmail(reader) => {
            jmap::get_calendar_event_with_reader(reader, uid, primary_tz).await
        }
        CalendarEventReader::Outlook(reader) => {
            outlook::get_calendar_event_with_reader(reader, uid).await
        }
        CalendarEventReader::Gmail(reader) => {
            gmail::get_calendar_event_with_reader(reader, uid).await
        }
    }
}

/// Fetch the current calendar event from the calendar (CalDAV/Graph) by UID.
/// Returns None if the event doesn't exist in the calendar yet.
pub async fn get_calendar_event(
    s: &ProviderSession,
    uid: &str,
    primary_tz: chrono_tz::Tz,
) -> Result<Option<CalendarEvent>, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::get_calendar_event(s, uid, primary_tz).await,
        ProviderSession::Outlook(s) => outlook::get_calendar_event(s, uid).await,
        ProviderSession::Gmail(s) => gmail::get_calendar_event(s, uid).await,
    }
}

// =============================================================================
// Blob upload/download dispatch
// =============================================================================

/// Upload a blob (attachment). Returns (blob_id, size).
pub async fn upload_blob(
    s: &ProviderSession,
    content_type: &str,
    body: &[u8],
) -> Result<(String, i64), Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::upload_blob(s, content_type, body).await,
        ProviderSession::Outlook(s) => outlook::upload_blob(s, content_type, body).await,
        ProviderSession::Gmail(s) => gmail::upload_blob(s, content_type, body).await,
    }
}

/// Download a blob (attachment). Returns (content_type, bytes).
pub async fn download_blob(
    s: &ProviderSession,
    blob_id: &str,
    filename: &str,
) -> Result<(String, Vec<u8>), Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::download_blob(s, blob_id, filename).await,
        ProviderSession::Outlook(s) => outlook::download_blob(s, blob_id, filename).await,
        ProviderSession::Gmail(s) => gmail::download_blob(s, blob_id, filename).await,
    }
}

// =============================================================================
// Calendar dispatch — Outlook uses Graph API, Fastmail uses CalDAV
// =============================================================================

pub async fn add_to_calendar(
    s: &ProviderSession,
    ics_data: &str,
    uid: &str,
    only_if_new: bool,
    primary_tz: chrono_tz::Tz,
) -> Result<bool, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::add_to_calendar(s, ics_data, uid, only_if_new).await,
        ProviderSession::Outlook(s) => {
            let event = calendar::parse_ics(ics_data, primary_tz).ok_or_else(|| {
                Error::Internal("Failed to parse ICS for Outlook calendar".into())
            })?;
            if only_if_new {
                // For auto-add on email open, just add if missing (add_to_calendar checks)
                outlook::add_to_calendar(s, ics_data, &event, false).await
            } else {
                // Explicit add / invite update. The overwrite semantics live
                // inside `outlook::add_to_calendar` (kata wca3): an
                // Exchange-processed invite event is never deleted — Exchange
                // applies organizer updates to it itself, and it is the only
                // copy Graph can RSVP, so the old remove+re-add here would
                // have replaced it with an un-RSVP-able orphan. Only a
                // supervillain-created copy is ours to replace, and a failed
                // delete aborts before the create (no duplicate). If the
                // create fails right after a successful delete, the calendar
                // would silently lack the meeting until the next email open —
                // retry once immediately before giving up (roborev 292 #3).
                let first = outlook::add_to_calendar(s, ics_data, &event, true).await;
                if matches!(first, Ok(true)) {
                    first
                } else {
                    tracing::warn!(
                        "Outlook calendar update did not succeed for {uid} ({first:?}); retrying once"
                    );
                    outlook::add_to_calendar(s, ics_data, &event, true).await
                }
            }
        }
        ProviderSession::Gmail(s) => {
            let event = calendar::parse_ics(ics_data, primary_tz)
                .ok_or_else(|| Error::Internal("Failed to parse ICS for Gmail calendar".into()))?;
            if only_if_new {
                gmail::add_to_calendar(s, ics_data, &event).await
            } else {
                // Explicit add — remove then re-add to handle updates.
                // Propagate remove errors: if the delete failed (5xx, network)
                // the event still exists and the follow-up add hits the
                // existence check and short-circuits to Ok(true), leaving the
                // user thinking the update landed while the content is stale.
                gmail::remove_from_calendar(s, uid).await?;
                // If the import fails right after a successful remove, the
                // calendar would silently lack the meeting until the next
                // email open — retry once immediately before giving up
                // (roborev 292 #3).
                let first = gmail::add_to_calendar(s, ics_data, &event).await;
                if matches!(first, Ok(true)) {
                    first
                } else {
                    tracing::warn!(
                        "Gmail add-after-remove did not succeed for {uid} ({first:?}); retrying once"
                    );
                    gmail::add_to_calendar(s, ics_data, &event).await
                }
            }
        }
    }
}

pub async fn remove_from_calendar(s: &ProviderSession, uid: &str) -> Result<bool, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::remove_from_calendar(s, uid).await,
        ProviderSession::Outlook(s) => outlook::remove_from_calendar(s, uid).await,
        ProviderSession::Gmail(s) => gmail::remove_from_calendar(s, uid).await,
    }
}

/// Full RSVP flow — dispatches the entire accept/decline/tentative flow per provider.
///
/// Fastmail: generate iTIP reply email + CalDAV upsert/delete
/// Outlook: Graph API respond_to_event (sends RSVP email automatically)
///
/// `reply_tz` only affects the **Fastmail** path: it controls the timezone
/// in which `DTSTART`/`DTEND` are quoted in the client-generated iTIP REPLY
/// (`generate_rsvp_with_tz`). Outlook and Gmail use Graph's
/// `respond_to_event` / Calendar API which the providers render in the
/// recipient's display timezone; the parameter is accepted but unused by
/// those arms. Roborev 186 #5: this asymmetry is intentional, documented
/// here so future maintainers don't assume `reply_tz` is universal.
pub async fn rsvp(
    s: &mut ProviderSession,
    ics_data: &str,
    event: &CalendarEvent,
    attendee_email: &str,
    status: &RsvpStatus,
    reply_tz: chrono_tz::Tz,
) -> Result<(), Error> {
    match s {
        ProviderSession::Fastmail(s) => {
            // Fail fast before any outward-facing side effect (kata m5yp,
            // roborev 376 #2): a missing app password makes the CalDAV write
            // fail, so don't email the organizer an acceptance the calendar
            // can't back — each retry would otherwise send another duplicate
            // iTIP REPLY. The credential is knowable from the session before
            // any network I/O, so surface it here with zero side effects.
            if s.caldav_auth_header.is_empty() {
                return Err(Error::CalendarAuthUnconfigured);
            }
            // Send iTIP reply email to organizer, with DTSTART quoted in the user's
            // primary timezone instead of UTC-Z.
            let rsvp_ics = calendar::generate_rsvp_with_tz(event, attendee_email, status, reply_tz);
            let submission = EmailSubmission {
                to: vec![event.organizer_email.clone()],
                cc: vec![],
                subject: format!("Re: {}", event.summary),
                text_body: format!(
                    "{} has {} the invitation: {}",
                    attendee_email,
                    status.as_ics_str().to_lowercase(),
                    event.summary
                ),
                bcc: None,
                html_body: None,
                in_reply_to: None,
                references: None,
                attachments: vec![],
                calendar_ics: Some(rsvp_ics),
            };

            if let Err(e) = jmap::send_email(s, &submission, attendee_email, None).await {
                tracing::warn!(
                    "Failed to send iTIP reply to {}: {e}",
                    event.organizer_email
                );
            }

            // CalDAV: decline = remove, accept/tentative = upsert with updated
            // PARTSTAT. Propagate the error (kata m5yp): a missing/wrong app
            // password must surface to the RSVP response via the route's `?`,
            // not be `warn!`-ed past an `Ok(())` so the UI reports success
            // while Morgen never sees the event.
            if *status == RsvpStatus::Declined {
                jmap::remove_from_calendar(s, &event.uid).await?;
            } else {
                let updated_ics = calendar::update_partstat(ics_data, attendee_email, status);
                // SCHEDULE-AGENT=CLIENT: this flow already emailed the iTIP
                // REPLY above; without the param Fastmail's server-side
                // scheduling fires its own REPLY on the PARTSTAT write, so
                // the organizer gets two responses per click (kata 8gn5,
                // verified live).
                let updated_ics = calendar::set_schedule_agent_client(&updated_ics);
                jmap::add_to_calendar(s, &updated_ics, &event.uid, false).await?;
            }
            // Consume the scheduling-Inbox invitation (RFC 6638) — left
            // behind, Fastmail/Morgen keep rendering the invite as pending
            // regardless of the RSVP, the kata 8gn5 "Maybe/Decline don't
            // update my calendar" symptom. Best-effort by design: the reply
            // email and calendar write above already succeeded, and failing
            // the whole RSVP here would invite retries that re-send replies
            // (same duplicate-reply hazard as the m5yp comment above).
            if let Err(e) = jmap::clear_scheduling_inbox(s, &event.uid).await {
                tracing::warn!(
                    "RSVP succeeded but clearing the scheduling inbox for {} failed: {e}",
                    event.uid
                );
            }
        }
        ProviderSession::Outlook(s) => {
            // `reply_tz` is intentionally unused: Graph's respond_to_event
            // renders the response in the recipient's display TZ. See doc
            // comment on this fn (roborev 186 #5).
            let _ = reply_tz;
            // No pre-add (kata wca3, verified live): when Exchange processed
            // the invite the event is already on the calendar and accept /
            // decline moves it — the add is a no-op. When Exchange did NOT
            // process it, a copy we create is one Graph can't RSVP (400
            // "you're the meeting organizer"), and creating it with attendees
            // would email bogus duplicate invitations from this user to every
            // attendee. Either way the pre-add can only hurt.
            //
            // Graph handles RSVP + calendar update + organizer email in one
            // call; Ok(false) means no event with this UID exists to respond
            // to (Err covers actual Graph failures).
            if !outlook::respond_to_event(s, &event.uid, status).await? {
                return Err(Error::NotFound(format!(
                    "Outlook has no calendar entry for this invite (event {}). Exchange did not \
                     process the invite as a meeting request, so Graph has nothing to RSVP to.",
                    event.uid
                )));
            }
        }
        ProviderSession::Gmail(s) => {
            // `reply_tz` is intentionally unused: Google Calendar's
            // PATCH+sendUpdates handles the recipient's TZ. See doc
            // comment on this fn (roborev 186 #5).
            let _ = reply_tz;
            // Ensure the event exists before we try to PATCH its attendees.
            // `add_to_calendar` is idempotent (Ok(true) when already present);
            // we log-and-continue on failure so the more diagnostic upstream
            // error isn't lost behind the generic "event missing" message
            // respond_to_event would otherwise produce.
            if let Err(e) = gmail::add_to_calendar(s, ics_data, event).await {
                tracing::warn!(
                    uid = %event.uid,
                    "Gmail RSVP pre-add failed (continuing to PATCH in case event already exists): {e}"
                );
            }

            // Google's PATCH attendees + sendUpdates=all sends the
            // organizer email automatically — no separate iTIP needed.
            if !gmail::respond_to_event(s, &event.uid, attendee_email, status).await? {
                return Err(Error::Internal(format!(
                    "Gmail RSVP failed for event {} (event missing or attendee {} not in invitees)",
                    event.uid, attendee_email
                )));
            }
        }
    }
    Ok(())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{FsTokenStore, TokenStore, Tokens};
    use std::sync::Arc;

    fn make_fastmail_session() -> ProviderSession {
        ProviderSession::Fastmail(Box::new(JmapSession::new(
            "user@fastmail.com",
            "token",
            Some("app-pass"),
        )))
    }

    fn make_outlook_session() -> ProviderSession {
        ProviderSession::Outlook(Box::new(OutlookSession {
            client: reqwest::Client::new(),
            token: tokio::sync::Mutex::new(crate::outlook::OutlookToken {
                access_token: "test".into(),
                refresh_token: "test".into(),
                token_expiry: chrono::Utc::now(),
            }),
            client_id: "test".into(),
            token_path: std::path::PathBuf::from("/tmp/test"),
            email: "user@outlook.com".into(),
            folder_cache: tokio::sync::Mutex::new(None),
            page_cache: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            upload_cache: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            identity_cache: tokio::sync::Mutex::new(None),
            folder_role_cache: tokio::sync::Mutex::new(None),
            limiter: crate::outlook::build_outlook_limiter(),
            graph_base: crate::outlook::GRAPH_BASE.to_string(),
        }))
    }

    fn make_gmail_session() -> ProviderSession {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn TokenStore> = Arc::new(FsTokenStore::new(dir.path().to_path_buf()));
        // Seed a token file so the session has something to load
        store
            .save(
                "gmail",
                &Tokens {
                    access_token: "test".into(),
                    refresh_token: "test".into(),
                    token_expiry: chrono::Utc::now(),
                    email: "user@gmail.com".into(),
                },
            )
            .unwrap();
        let session = crate::gmail::load_session(store, "gmail", "client-id", "client-secret")
            .expect("session should load");
        // Keep tempdir alive for the test (intentional leak; tests are short-lived)
        std::mem::forget(dir);
        ProviderSession::Gmail(Box::new(session))
    }

    #[test]
    fn username_returns_fastmail_username() {
        let s = make_fastmail_session();
        assert_eq!(s.username(), "user@fastmail.com");
    }

    #[test]
    fn username_returns_outlook_email() {
        let s = make_outlook_session();
        assert_eq!(s.username(), "user@outlook.com");
    }

    #[test]
    fn username_returns_gmail_email() {
        let s = make_gmail_session();
        assert_eq!(s.username(), "user@gmail.com");
    }

    #[test]
    fn provider_name_gmail() {
        let s = make_gmail_session();
        assert_eq!(s.provider_name(), "gmail");
    }

    #[test]
    fn sends_rsvp_automatically_true_for_outlook() {
        let s = make_outlook_session();
        assert!(s.sends_rsvp_automatically());
    }

    #[test]
    fn sends_rsvp_automatically_true_for_gmail() {
        let s = make_gmail_session();
        assert!(s.sends_rsvp_automatically());
    }

    #[test]
    fn sends_rsvp_automatically_false_for_fastmail() {
        let s = make_fastmail_session();
        assert!(!s.sends_rsvp_automatically());
    }

    // --- draft provider gating (kata wm57) ---

    fn draft_submission() -> EmailSubmission {
        EmailSubmission {
            to: vec!["bob@example.com".into()],
            cc: vec![],
            subject: "Draft".into(),
            text_body: "wip".into(),
            bcc: None,
            html_body: None,
            in_reply_to: None,
            references: None,
            attachments: vec![],
            calendar_ics: None,
        }
    }

    fn assert_drafts_unsupported(err: Error, provider: &str) {
        match err {
            Error::BadRequest(msg) => {
                assert!(
                    msg.contains("not supported") && msg.contains(provider),
                    "expected a clear per-provider not-supported message, got: {msg}"
                );
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_draft_rejected_for_gmail() {
        let s = make_gmail_session();
        let err = create_draft(&s, &draft_submission(), "me@gmail.com")
            .await
            .expect_err("gmail must reject draft creation in v1");
        assert_drafts_unsupported(err, "gmail");
    }

    #[tokio::test]
    async fn update_draft_rejected_for_outlook() {
        let s = make_outlook_session();
        let err = update_draft(&s, "draft-1", &draft_submission(), "me@outlook.com")
            .await
            .expect_err("outlook must reject draft update in v1");
        assert_drafts_unsupported(err, "outlook");
    }

    #[tokio::test]
    async fn destroy_draft_rejected_for_gmail() {
        let s = make_gmail_session();
        let err = destroy_draft(&s, "draft-1")
            .await
            .expect_err("gmail must reject draft destroy in v1");
        assert_drafts_unsupported(err, "gmail");
    }

    // --- calendar remove-then-add retry (roborev 292 #3) ---
    //
    // Outlook/Gmail's only_if_new=false path is remove-then-import; if the
    // import half fails right after a successful remove, the calendar would
    // silently lack the meeting until the next email open. Both provider
    // arms must retry the add once before giving up. This can't be
    // exercised with real HTTP without a mock server (none is wired up in
    // this codebase), so — following the existing pattern for untestable
    // async wiring elsewhere in this codebase (e.g. accounts.rs's
    // `every_app_config_write_site_resets_the_baseline` and routes.rs's
    // `get_email_reaches_only_if_new_false_on_update`) — assert on the
    // source shape instead. `mod tests` is sliced off first so this test's
    // own literal strings can't inflate the counts.
    /// Slice out the body of `pub async fn add_to_calendar` from this file's
    /// own source, so the scan below only sees that one dispatch function
    /// (there are several other `match s { ProviderSession::Outlook... }`
    /// dispatchers in this file that must not be counted).
    fn add_to_calendar_fn_src() -> String {
        let src = include_str!("provider.rs");
        let handler_src = src.split("mod tests").next().unwrap_or(src);
        let after_start = handler_src
            .split("pub async fn add_to_calendar(")
            .nth(1)
            .expect("add_to_calendar fn must exist");
        after_start
            .split("pub async fn remove_from_calendar(")
            .next()
            .expect("remove_from_calendar fn must follow add_to_calendar")
            .to_string()
    }

    #[test]
    fn add_to_calendar_retries_outlook_overwrite_once() {
        let fn_src = add_to_calendar_fn_src();
        let outlook_block = fn_src
            .split("ProviderSession::Outlook(s) => {")
            .nth(1)
            .and_then(|rest| rest.split("ProviderSession::Gmail(s) => {").next())
            .expect("Outlook arm of add_to_calendar dispatch must exist");
        assert!(
            outlook_block
                .matches("outlook::add_to_calendar(s, ics_data, &event, true).await")
                .count()
                >= 2,
            "the only_if_new=false Outlook branch must retry the overwrite add once after a failed attempt"
        );
        assert!(
            outlook_block.contains("outlook::add_to_calendar(s, ics_data, &event, false).await"),
            "the only_if_new=true Outlook branch must use the non-overwrite add"
        );
    }

    // --- Outlook update must not delete the Exchange-processed event ---
    //
    // kata wca3: the only_if_new=false Outlook branch used to remove+re-add
    // by UID. For an Exchange-processed invite that deleted the only
    // RSVP-able copy (Graph 400s accept/decline on a self-created copy and
    // ignores caller-supplied iCalUId, so the replacement was also
    // unfindable). The processed/self-created split now lives inside
    // `outlook::add_to_calendar(overwrite)`; this pins that the provider
    // arm doesn't reintroduce a remove. Same source-shape assertion pattern
    // as the retry test above.

    #[test]
    fn add_to_calendar_outlook_never_removes_before_adding() {
        let fn_src = add_to_calendar_fn_src();
        let outlook_block = fn_src
            .split("ProviderSession::Outlook(s) => {")
            .nth(1)
            .and_then(|rest| rest.split("ProviderSession::Gmail(s) => {").next())
            .expect("Outlook arm of add_to_calendar dispatch must exist");
        assert!(
            !outlook_block.contains("outlook::remove_from_calendar"),
            "the Outlook branch must not delete by UID before adding — that destroys the \
             Exchange-processed (only RSVP-able) copy on invite updates"
        );
    }

    // --- Outlook rsvp arm: honest failure semantics (kata wca3) ---
    //
    // `respond_to_event` returns Ok(false) for exactly one condition — no
    // event with this UID on the calendar (Exchange never processed the
    // invite) — which the arm must surface as NotFound, not a generic 500.
    // Graph rejections arrive as Err and must propagate unmodified via `?`.
    // The arm must also stay free of the old pre-add: it was a no-op for
    // processed invites and emailed bogus invitations for unprocessed ones.

    fn rsvp_outlook_arm_src() -> String {
        let src = include_str!("provider.rs");
        let handler_src = src.split("mod tests").next().unwrap_or(src);
        handler_src
            .split("pub async fn rsvp(")
            .nth(1)
            .expect("rsvp fn must exist")
            .split("ProviderSession::Outlook(s) => {")
            .nth(1)
            .and_then(|rest| rest.split("ProviderSession::Gmail(s) => {").next())
            .expect("Outlook arm of rsvp dispatch must exist")
            .to_string()
    }

    #[tokio::test]
    async fn rsvp_fastmail_wire_carries_client_scheduling_and_consumes_inbox() {
        // Behavioral pin for the full Fastmail RSVP arm (kata 8gn5) against
        // a scripted CalDAV recorder — the source-shape test below can't
        // catch a refactor that calls set_schedule_agent_client but drops
        // its return value. The iTIP reply email is attempted first and
        // fails fast (no JMAP account_id on the session); by design that is
        // warn-only and must not gate the calendar write.
        use crate::jmap::caldav_recorder;
        let uid = "uid-8gn5-behavioral";
        let inbox_obj = "/dav/calendars/user/u/Inbox/queued.ics".to_string();
        let multistatus = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
 <D:response>
  <D:href>{inbox_obj}</D:href>
  <D:propstat><D:prop><D:getetag>"e1"</D:getetag>
   <C:calendar-data>BEGIN:VCALENDAR
BEGIN:VEVENT
UID:{uid}
END:VEVENT
END:VCALENDAR</C:calendar-data></D:prop>
   <D:status>HTTP/1.1 200 OK</D:status></D:propstat>
 </D:response>
</D:multistatus>"#
        );
        let (base, recorded) = caldav_recorder::spawn_scripted(move |method, _path| {
            if method == "REPORT" {
                (
                    axum::http::StatusCode::MULTI_STATUS,
                    multistatus.clone().into_bytes(),
                )
            } else {
                (axum::http::StatusCode::NO_CONTENT, Vec::new())
            }
        })
        .await;

        let mut jmap_sess =
            crate::jmap::JmapSession::new("att@aristoi.test", "token", Some("app-pass"));
        jmap_sess.caldav_base = base.clone();
        jmap_sess
            .caldav_collection_url
            .set(crate::jmap::DiscoveredCalendars {
                default_collection: format!("{base}/dav/calendars/user/u/coll"),
                schedule_inbox: Some(format!("{base}/dav/calendars/user/u/Inbox")),
            })
            .expect("prefill discovery");
        let mut session = ProviderSession::Fastmail(Box::new(jmap_sess));

        let ics = format!(
            "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
             ORGANIZER:mailto:org@example.com\r\n\
             ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:att@aristoi.test\r\n\
             END:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        let event = CalendarEvent {
            uid: uid.into(),
            summary: "8gn5 behavioral".into(),
            dtstart: chrono::DateTime::parse_from_rfc3339("2026-08-20T15:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            dtend: None,
            location: None,
            description: None,
            organizer_email: "org@example.com".into(),
            organizer_name: None,
            attendees: vec![crate::types::Attendee {
                email: "att@aristoi.test".into(),
                name: None,
                status: "NEEDS-ACTION".into(),
            }],
            sequence: 0,
            method: "REQUEST".into(),
            raw_ics: ics.clone(),
            user_rsvp_status: None,
            is_update: false,
        };

        rsvp(
            &mut session,
            &ics,
            &event,
            "att@aristoi.test",
            &crate::types::RsvpStatus::Tentative,
            chrono_tz::UTC,
        )
        .await
        .expect("fastmail rsvp must succeed against the loopback");

        let rec = recorded.lock().unwrap();
        let puts: Vec<_> = rec.iter().filter(|r| r.method == "PUT").collect();
        assert_eq!(puts.len(), 1, "exactly one PARTSTAT PUT expected: {rec:?}");
        let put_body = std::str::from_utf8(&puts[0].body).unwrap_or("");
        assert!(
            put_body.contains("ORGANIZER;SCHEDULE-AGENT=CLIENT:mailto:org@example.com"),
            "the stored copy must suppress server-side scheduling: {put_body}"
        );
        assert!(
            put_body.contains("PARTSTAT=TENTATIVE"),
            "the stored copy must carry the updated PARTSTAT: {put_body}"
        );
        let reports: Vec<_> = rec.iter().filter(|r| r.method == "REPORT").collect();
        assert_eq!(
            reports.len(),
            1,
            "exactly one inbox REPORT expected: {rec:?}"
        );
        assert!(
            reports[0].path.ends_with("/dav/calendars/user/u/Inbox"),
            "REPORT must address the schedule-inbox: {}",
            reports[0].path
        );
        let dels: Vec<_> = rec.iter().filter(|r| r.method == "DELETE").collect();
        assert_eq!(
            dels.len(),
            1,
            "the queued invitation must be consumed: {rec:?}"
        );
        assert_eq!(dels[0].path, inbox_obj);
        assert_eq!(dels[0].header("schedule-reply"), Some("F"));
    }

    fn rsvp_fastmail_arm_src() -> String {
        let src = include_str!("provider.rs");
        let handler_src = src.split("mod tests").next().unwrap_or(src);
        handler_src
            .split("pub async fn rsvp(")
            .nth(1)
            .expect("rsvp fn must exist")
            .split("ProviderSession::Fastmail(s) => {")
            .nth(1)
            .and_then(|rest| rest.split("ProviderSession::Outlook(s) => {").next())
            .expect("Fastmail arm of rsvp dispatch must exist")
            .to_string()
    }

    #[test]
    fn rsvp_fastmail_arm_suppresses_server_scheduling_and_clears_inbox() {
        // kata 8gn5 (live-verified): without SCHEDULE-AGENT=CLIENT the
        // PARTSTAT PUT makes Fastmail email its own REPLY on top of ours,
        // and without the inbox clear the scheduling-Inbox invitation keeps
        // rendering as pending regardless of the RSVP.
        let arm = rsvp_fastmail_arm_src();
        assert!(
            arm.contains("calendar::set_schedule_agent_client("),
            "the PARTSTAT write must carry SCHEDULE-AGENT=CLIENT so the server doesn't double-reply"
        );
        assert!(
            arm.contains("jmap::clear_scheduling_inbox(s, &event.uid)"),
            "every RSVP must consume the scheduling-Inbox invitation for the event's UID"
        );
        assert!(
            !arm.contains("clear_scheduling_inbox(s, &event.uid).await?"),
            "inbox clearing is best-effort — propagating its error would invite RSVP retries that re-send replies"
        );
    }

    #[test]
    fn rsvp_outlook_arm_maps_not_found_and_propagates_errors() {
        let arm = rsvp_outlook_arm_src();
        assert!(
            arm.contains("outlook::respond_to_event(s, &event.uid, status).await?"),
            "respond_to_event errors must propagate via `?`, not be swallowed"
        );
        assert!(
            arm.contains("Error::NotFound"),
            "Ok(false) (no event to respond to) must surface as NotFound, not a generic 500"
        );
        assert!(
            !arm.contains("add_to_calendar"),
            "the rsvp arm must not pre-add: harmful for unprocessed invites, useless otherwise"
        );
    }

    #[test]
    fn add_to_calendar_retries_gmail_add_after_remove_failure() {
        let fn_src = add_to_calendar_fn_src();
        let gmail_block = fn_src
            .split("ProviderSession::Gmail(s) => {")
            .nth(1)
            .expect("Gmail arm of add_to_calendar dispatch must exist");
        assert!(
            gmail_block
                .matches("gmail::add_to_calendar(s, ics_data, &event).await")
                .count()
                >= 2,
            "the only_if_new=false Gmail branch must retry add_to_calendar once after a failed attempt"
        );
        assert!(
            gmail_block.contains("gmail::remove_from_calendar(s, uid).await?"),
            "the Gmail branch must still propagate remove errors before re-adding"
        );
    }
}
