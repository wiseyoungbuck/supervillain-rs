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
    Fastmail(JmapSession),
    Outlook(OutlookSession),
    Gmail(GmailSession),
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

/// Standard error for not-yet-implemented Gmail operations (Milestones B–D).
fn gmail_not_yet_implemented(op: &str, milestone: &str) -> Error {
    Error::BadRequest(format!(
        "Gmail: {op} lands in Phase 3 {milestone} (Milestone A ships read-only inbox)."
    ))
}

pub async fn get_mailboxes(s: &ProviderSession) -> Result<Vec<Mailbox>, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::get_mailboxes(s).await,
        ProviderSession::Outlook(_) => Err(Error::BadRequest(
            "Email operations not yet supported for Outlook accounts".into(),
        )),
        ProviderSession::Gmail(s) => gmail::get_mailboxes(s).await,
    }
}

pub async fn get_identities(s: &mut ProviderSession) -> Result<Vec<Identity>, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::get_identities(s).await,
        ProviderSession::Outlook(_) => Err(Error::BadRequest(
            "Email operations not yet supported for Outlook accounts".into(),
        )),
        ProviderSession::Gmail(s) => gmail::get_identities(s).await,
    }
}

pub async fn query_emails(
    s: &ProviderSession,
    mailbox_id: Option<&str>,
    limit: usize,
    position: usize,
    query: Option<&ParsedQuery>,
) -> Result<Vec<String>, Error> {
    match s {
        ProviderSession::Fastmail(s) => {
            jmap::query_emails(s, mailbox_id, limit, position, query).await
        }
        ProviderSession::Outlook(_) => Err(Error::BadRequest(
            "Email operations not yet supported for Outlook accounts".into(),
        )),
        ProviderSession::Gmail(s) => {
            gmail::query_emails(s, mailbox_id, limit, position, query).await
        }
    }
}

pub async fn get_emails(
    s: &ProviderSession,
    ids: &[String],
    fetch_body: bool,
    properties_override: Option<&[&str]>,
) -> Result<Vec<Email>, Error> {
    match s {
        ProviderSession::Fastmail(s) => {
            jmap::get_emails(s, ids, fetch_body, properties_override).await
        }
        ProviderSession::Outlook(_) => Err(Error::BadRequest(
            "Email operations not yet supported for Outlook accounts".into(),
        )),
        ProviderSession::Gmail(s) => {
            // properties_override is JMAP-specific; Gmail always returns the full payload.
            let _ = properties_override;
            gmail::get_emails(s, ids, fetch_body).await
        }
    }
}

pub async fn mark_read(s: &ProviderSession, email_id: &str) -> Result<bool, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::mark_read(s, email_id).await,
        ProviderSession::Outlook(_) => Err(Error::BadRequest(
            "Email operations not yet supported for Outlook accounts".into(),
        )),
        ProviderSession::Gmail(_) => Err(gmail_not_yet_implemented("mark_read", "Milestone B")),
    }
}

pub async fn mark_unread(s: &ProviderSession, email_id: &str) -> Result<bool, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::mark_unread(s, email_id).await,
        ProviderSession::Outlook(_) => Err(Error::BadRequest(
            "Email operations not yet supported for Outlook accounts".into(),
        )),
        ProviderSession::Gmail(_) => Err(gmail_not_yet_implemented("mark_unread", "Milestone B")),
    }
}

pub async fn toggle_flag(s: &ProviderSession, email_id: &str) -> Result<bool, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::toggle_flag(s, email_id).await,
        ProviderSession::Outlook(_) => Err(Error::BadRequest(
            "Email operations not yet supported for Outlook accounts".into(),
        )),
        ProviderSession::Gmail(_) => Err(gmail_not_yet_implemented("toggle_flag", "Milestone B")),
    }
}

pub async fn archive(s: &ProviderSession, email_id: &str) -> Result<bool, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::archive(s, email_id).await,
        ProviderSession::Outlook(_) => Err(Error::BadRequest(
            "Email operations not yet supported for Outlook accounts".into(),
        )),
        ProviderSession::Gmail(_) => Err(gmail_not_yet_implemented("archive", "Milestone B")),
    }
}

pub async fn trash(s: &ProviderSession, email_id: &str) -> Result<bool, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::trash(s, email_id).await,
        ProviderSession::Outlook(_) => Err(Error::BadRequest(
            "Email operations not yet supported for Outlook accounts".into(),
        )),
        ProviderSession::Gmail(_) => Err(gmail_not_yet_implemented("trash", "Milestone B")),
    }
}

pub async fn move_to_mailbox(
    s: &ProviderSession,
    email_id: &str,
    mailbox_id: &str,
) -> Result<bool, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::move_to_mailbox(s, email_id, mailbox_id).await,
        ProviderSession::Outlook(_) => Err(Error::BadRequest(
            "Email operations not yet supported for Outlook accounts".into(),
        )),
        ProviderSession::Gmail(_) => {
            Err(gmail_not_yet_implemented("move_to_mailbox", "Milestone B"))
        }
    }
}

pub async fn archive_batch(s: &ProviderSession, email_ids: &[String]) -> Result<usize, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::archive_batch(s, email_ids).await,
        ProviderSession::Outlook(_) => Err(Error::BadRequest(
            "Email operations not yet supported for Outlook accounts".into(),
        )),
        ProviderSession::Gmail(_) => Err(gmail_not_yet_implemented("archive_batch", "Milestone B")),
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
        ProviderSession::Outlook(_) => Err(Error::BadRequest(
            "Email operations not yet supported for Outlook accounts".into(),
        )),
        ProviderSession::Gmail(_) => Err(gmail_not_yet_implemented("send_email", "Milestone C")),
    }
}

pub async fn get_calendar_data(
    s: &ProviderSession,
    email_id: &str,
) -> Result<Option<String>, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::get_calendar_data(s, email_id).await,
        ProviderSession::Outlook(_) => Err(Error::BadRequest(
            "Email operations not yet supported for Outlook accounts".into(),
        )),
        ProviderSession::Gmail(_) => Err(gmail_not_yet_implemented(
            "get_calendar_data",
            "Milestone D",
        )),
    }
}

/// Fetch the current calendar event from the calendar (CalDAV/Graph) by UID.
/// Returns None if the event doesn't exist in the calendar yet.
pub async fn get_calendar_event(
    s: &ProviderSession,
    uid: &str,
) -> Result<Option<CalendarEvent>, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::get_calendar_event(s, uid).await,
        ProviderSession::Outlook(s) => outlook::get_calendar_event(s, uid).await,
        ProviderSession::Gmail(_) => Err(gmail_not_yet_implemented(
            "get_calendar_event",
            "Milestone D",
        )),
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
        ProviderSession::Outlook(_) => Err(Error::BadRequest(
            "Blob upload not supported for Outlook accounts".into(),
        )),
        ProviderSession::Gmail(_) => Err(gmail_not_yet_implemented("upload_blob", "Milestone C")),
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
        ProviderSession::Outlook(_) => Err(Error::BadRequest(
            "Attachment downloads not supported for Outlook accounts".into(),
        )),
        ProviderSession::Gmail(_) => Err(gmail_not_yet_implemented("download_blob", "Milestone B")),
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
) -> Result<bool, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::add_to_calendar(s, ics_data, uid, only_if_new).await,
        ProviderSession::Outlook(s) => {
            let event = calendar::parse_ics(ics_data).ok_or_else(|| {
                Error::Internal("Failed to parse ICS for Outlook calendar".into())
            })?;
            if only_if_new {
                // For auto-add on email open, just add if missing (add_to_calendar checks)
                outlook::add_to_calendar(s, ics_data, &event).await
            } else {
                // Explicit add — remove first then re-add to handle updates
                let _ = outlook::remove_from_calendar(s, uid).await;
                outlook::add_to_calendar(s, ics_data, &event).await
            }
        }
        ProviderSession::Gmail(_) => {
            Err(gmail_not_yet_implemented("add_to_calendar", "Milestone D"))
        }
    }
}

pub async fn remove_from_calendar(s: &ProviderSession, uid: &str) -> Result<bool, Error> {
    match s {
        ProviderSession::Fastmail(s) => jmap::remove_from_calendar(s, uid).await,
        ProviderSession::Outlook(s) => outlook::remove_from_calendar(s, uid).await,
        ProviderSession::Gmail(_) => Err(gmail_not_yet_implemented(
            "remove_from_calendar",
            "Milestone D",
        )),
    }
}

/// Full RSVP flow — dispatches the entire accept/decline/tentative flow per provider.
///
/// Fastmail: generate iTIP reply email + CalDAV upsert/delete
/// Outlook: Graph API respond_to_event (sends RSVP email automatically)
pub async fn rsvp(
    s: &mut ProviderSession,
    ics_data: &str,
    event: &CalendarEvent,
    attendee_email: &str,
    status: &RsvpStatus,
) -> Result<(), Error> {
    match s {
        ProviderSession::Fastmail(s) => {
            // Send iTIP reply email to organizer
            let rsvp_ics = calendar::generate_rsvp(event, attendee_email, status);
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

            // CalDAV: decline = remove, accept/tentative = upsert with updated PARTSTAT
            if *status == RsvpStatus::Declined {
                if let Err(e) = jmap::remove_from_calendar(s, &event.uid).await {
                    tracing::warn!("CalDAV delete failed for {}: {e}", event.uid);
                }
            } else {
                let updated_ics = calendar::update_partstat(ics_data, attendee_email, status);
                if let Err(e) = jmap::add_to_calendar(s, &updated_ics, &event.uid, false).await {
                    tracing::warn!("CalDAV write failed for {}: {e}", event.uid);
                }
            }
        }
        ProviderSession::Outlook(s) => {
            // Ensure the event exists in the calendar first
            let event_parsed = calendar::parse_ics(ics_data)
                .ok_or_else(|| Error::Internal("Failed to parse ICS for Outlook RSVP".into()))?;
            let _ = outlook::add_to_calendar(s, ics_data, &event_parsed).await;

            // Graph handles RSVP + email sending in one call
            if !outlook::respond_to_event(s, &event.uid, status).await? {
                return Err(Error::Internal(format!(
                    "Outlook RSVP failed for event {}",
                    event.uid
                )));
            }
        }
        ProviderSession::Gmail(_) => {
            return Err(gmail_not_yet_implemented("rsvp", "Milestone D"));
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
        ProviderSession::Fastmail(JmapSession::new("user@fastmail.com", "Bearer token"))
    }

    fn make_outlook_session() -> ProviderSession {
        ProviderSession::Outlook(OutlookSession {
            client: reqwest::Client::new(),
            token: tokio::sync::Mutex::new(crate::outlook::OutlookToken {
                access_token: "test".into(),
                refresh_token: "test".into(),
                token_expiry: chrono::Utc::now(),
            }),
            client_id: "test".into(),
            token_path: std::path::PathBuf::from("/tmp/test"),
            email: "user@outlook.com".into(),
        })
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
        ProviderSession::Gmail(session)
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
}
