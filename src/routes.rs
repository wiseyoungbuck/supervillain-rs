use axum::{
    Router,
    extract::{Json, Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post, put},
};
use serde::Deserialize;
use std::sync::Arc;

use crate::error::Error;
use crate::types::*;
use crate::{calendar, jmap, search, splits};

// =============================================================================
// Router
// =============================================================================

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/accounts", get(list_accounts))
        .route("/api/identities", get(list_identities))
        .route("/api/theme", get(get_theme))
        .route("/api/mailboxes", get(list_mailboxes))
        .route("/api/emails", get(list_emails))
        .route("/api/emails/send", post(send_email_handler))
        .route("/api/emails/{email_id}", get(get_email))
        .route("/api/emails/{email_id}/archive", post(archive_email))
        .route("/api/emails/{email_id}/trash", post(trash_email))
        .route("/api/emails/{email_id}/mark-read", post(mark_read))
        .route("/api/emails/{email_id}/mark-unread", post(mark_unread))
        .route("/api/emails/{email_id}/toggle-flag", post(toggle_flag))
        .route("/api/emails/{email_id}/move", post(move_email))
        .route("/api/emails/{email_id}/rsvp", post(rsvp))
        .route(
            "/api/emails/{email_id}/add-to-calendar",
            post(add_to_calendar),
        )
        .route(
            "/api/emails/{email_id}/unsubscribe-and-archive-all",
            post(unsubscribe_and_archive),
        )
        .route("/api/splits", get(list_splits).post(create_split))
        .route(
            "/api/splits/{split_id}",
            put(update_split).delete(delete_split),
        )
        .with_state(state)
}

// =============================================================================
// Query/body types
// =============================================================================

#[derive(Deserialize)]
struct ListEmailsParams {
    mailbox_id: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
    split_id: Option<String>,
    search: Option<String>,
}

#[derive(Deserialize)]
struct MoveBody {
    mailbox_id: String,
}

#[derive(Deserialize)]
struct SendEmailBody {
    to: Vec<String>,
    #[serde(default)]
    cc: Vec<String>,
    #[serde(default)]
    bcc: Vec<String>,
    subject: String,
    body: String,
    in_reply_to: Option<String>,
    from_address: Option<String>,
}

#[derive(Deserialize)]
struct RsvpBody {
    status: String,
}

// =============================================================================
// Handlers
// =============================================================================

async fn list_accounts(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let session = state.session.read().await;
    Json(serde_json::json!([{
        "id": "fastmail",
        "username": session.username,
        "is_default": true
    }]))
}

async fn list_identities(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, Error> {
    let mut session = state.session.write().await;
    let identities = jmap::get_identities(&mut session).await?;
    Ok(Json(serde_json::json!(identities)))
}

async fn get_theme() -> impl IntoResponse {
    // Try to load system theme
    let theme_path = dirs_next::config_dir()
        .unwrap_or_default()
        .join("omarchy/current/theme/supervillain.css");

    match std::fs::read_to_string(&theme_path) {
        Ok(css) => (StatusCode::OK, [("content-type", "text/css")], css),
        Err(_) => (
            StatusCode::OK,
            [("content-type", "text/css")],
            String::new(),
        ),
    }
}

async fn list_mailboxes(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, Error> {
    let session = state.session.read().await;
    let mailboxes = jmap::get_mailboxes(&session).await?;
    Ok(Json(serde_json::json!(mailboxes)))
}

async fn list_emails(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListEmailsParams>,
) -> Result<impl IntoResponse, Error> {
    let session = state.session.read().await;
    let limit = params.limit.unwrap_or(150);
    let offset = params.offset.unwrap_or(0);

    let query = params.search.as_deref().map(search::parse_query);
    let query_ref = query.as_ref();

    // Overfetch 3x when filtering by split to ensure enough results after filtering
    let fetch_limit = if params.split_id.is_some() {
        limit * 3
    } else {
        limit
    };

    let email_ids = jmap::query_emails(
        &session,
        params.mailbox_id.as_deref(),
        fetch_limit,
        offset,
        query_ref,
    )
    .await?;

    let mut emails = jmap::get_emails(&session, &email_ids, false).await?;

    // Apply split filtering
    if let Some(ref split_id) = params.split_id {
        let config = splits::load_splits(
            &state.splits_config_path,
            std::env::var("VIMMAIL_SPLITS").ok().as_deref(),
        );
        emails = splits::filter_by_split(emails, split_id, &config);
        emails.truncate(limit);
    }

    // Serialize emails for frontend
    let response: Vec<serde_json::Value> = emails
        .iter()
        .map(|e| {
            serde_json::json!({
                "id": e.id,
                "threadId": e.thread_id,
                "subject": e.subject,
                "from": e.from,
                "to": e.to,
                "cc": e.cc,
                "preview": e.preview,
                "receivedAt": e.received_at,
                "isUnread": e.is_unread(),
                "isFlagged": e.is_flagged(),
                "hasAttachment": e.has_attachment,
                "hasCalendar": e.has_calendar,
            })
        })
        .collect();

    Ok(Json(response))
}

async fn get_email(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
) -> Result<impl IntoResponse, Error> {
    let session = state.session.read().await;

    let emails = jmap::get_emails(&session, std::slice::from_ref(&email_id), true).await?;
    let email = emails
        .first()
        .ok_or_else(|| Error::NotFound("Email not found".into()))?;

    // Auto mark-read
    if email.is_unread() {
        let _ = jmap::mark_read(&session, &email_id).await;
    }

    // Check for calendar event
    let mut calendar_event = None;
    if email.has_calendar
        && let Ok(Some(ics_data)) = jmap::get_calendar_data(&session, &email_id).await
    {
        calendar_event = calendar::parse_ics(&ics_data);
    }

    Ok(Json(serde_json::json!({
        "id": email.id,
        "threadId": email.thread_id,
        "subject": email.subject,
        "from": email.from,
        "to": email.to,
        "cc": email.cc,
        "preview": email.preview,
        "receivedAt": email.received_at,
        "isUnread": email.is_unread(),
        "isFlagged": email.is_flagged(),
        "hasAttachment": email.has_attachment,
        "hasCalendar": email.has_calendar,
        "textBody": email.text_body,
        "htmlBody": email.html_body,
        "calendarEvent": calendar_event,
    })))
}

async fn archive_email(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
) -> Result<impl IntoResponse, Error> {
    let session = state.session.read().await;
    let success = jmap::archive(&session, &email_id).await?;
    Ok(Json(serde_json::json!({"success": success})))
}

async fn trash_email(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
) -> Result<impl IntoResponse, Error> {
    let session = state.session.read().await;
    let success = jmap::trash(&session, &email_id).await?;
    Ok(Json(serde_json::json!({"success": success})))
}

async fn mark_read(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
) -> Result<impl IntoResponse, Error> {
    let session = state.session.read().await;
    let success = jmap::mark_read(&session, &email_id).await?;
    Ok(Json(serde_json::json!({"success": success})))
}

async fn mark_unread(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
) -> Result<impl IntoResponse, Error> {
    let session = state.session.read().await;
    let success = jmap::mark_unread(&session, &email_id).await?;
    Ok(Json(serde_json::json!({"success": success})))
}

async fn toggle_flag(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
) -> Result<impl IntoResponse, Error> {
    let session = state.session.read().await;
    let success = jmap::toggle_flag(&session, &email_id).await?;
    Ok(Json(serde_json::json!({"success": success})))
}

async fn move_email(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
    Json(body): Json<MoveBody>,
) -> Result<impl IntoResponse, Error> {
    let session = state.session.read().await;
    let success = jmap::move_to_mailbox(&session, &email_id, &body.mailbox_id).await?;
    Ok(Json(serde_json::json!({"success": success})))
}

async fn send_email_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SendEmailBody>,
) -> Result<impl IntoResponse, Error> {
    let mut session = state.session.write().await;
    let from_addr = body
        .from_address
        .as_deref()
        .unwrap_or(&session.username)
        .to_string();

    let submission = EmailSubmission {
        to: body.to,
        cc: body.cc,
        subject: body.subject,
        text_body: body.body,
        bcc: if body.bcc.is_empty() {
            None
        } else {
            Some(body.bcc)
        },
        html_body: None,
        in_reply_to: body.in_reply_to,
        references: None,
    };

    let result = jmap::send_email(&mut session, &submission, &from_addr, None).await?;

    match result {
        Some(id) => Ok(Json(serde_json::json!({"success": true, "emailId": id}))),
        None => Err(Error::Internal("Failed to send email".into())),
    }
}

async fn rsvp(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
    Json(body): Json<RsvpBody>,
) -> Result<impl IntoResponse, Error> {
    let mut session_guard = state.session.write().await;

    // Get calendar data
    let ics_data = jmap::get_calendar_data(&session_guard, &email_id)
        .await?
        .ok_or_else(|| Error::NotFound("No calendar data found".into()))?;

    let event = calendar::parse_ics(&ics_data)
        .ok_or_else(|| Error::Internal("Failed to parse calendar data".into()))?;

    // Determine attendee email (use account username)
    let attendee_email = {
        // Try To addresses first, then CC, then username
        let emails =
            jmap::get_emails(&session_guard, std::slice::from_ref(&email_id), false).await?;
        let email = emails
            .first()
            .ok_or_else(|| Error::NotFound("Email not found".into()))?;

        let mut found = None;
        for addr in email.to.iter().chain(email.cc.iter()) {
            if event
                .attendees
                .iter()
                .any(|a| a.email.eq_ignore_ascii_case(&addr.email))
            {
                found = Some(addr.email.clone());
                break;
            }
        }
        found.unwrap_or_else(|| session_guard.username.clone())
    };

    let rsvp_ics = calendar::generate_rsvp(&event, &attendee_email, &body.status);

    // Send RSVP as email to organizer
    let submission = EmailSubmission {
        to: vec![event.organizer_email.clone()],
        cc: vec![],
        subject: format!("Re: {}", event.summary),
        text_body: format!(
            "{} has {} the invitation: {}",
            attendee_email,
            body.status.to_lowercase(),
            event.summary
        ),
        bcc: None,
        html_body: None,
        in_reply_to: None,
        references: None,
    };

    let _ = jmap::send_email(&mut session_guard, &submission, &attendee_email, None).await;

    // Try to add to calendar (non-fatal)
    let _ = jmap::add_to_calendar(&session_guard, &rsvp_ics).await;

    // Return updated event
    let updated_event = calendar::parse_ics(&rsvp_ics).unwrap_or(event);
    Ok(Json(serde_json::json!(updated_event)))
}

async fn add_to_calendar(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
) -> Result<impl IntoResponse, Error> {
    let session = state.session.read().await;

    let ics_data = jmap::get_calendar_data(&session, &email_id)
        .await?
        .ok_or_else(|| Error::NotFound("No calendar data found".into()))?;

    let success = jmap::add_to_calendar(&session, &ics_data).await?;

    if success {
        Ok(Json(serde_json::json!({"success": true})))
    } else {
        Err(Error::Internal("Failed to add to calendar".into()))
    }
}

async fn unsubscribe_and_archive(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
) -> Result<impl IntoResponse, Error> {
    let session = state.session.read().await;

    // Get the email to find the sender
    let emails = jmap::get_emails(&session, std::slice::from_ref(&email_id), true).await?;
    let email = emails
        .first()
        .ok_or_else(|| Error::NotFound("Email not found".into()))?;

    let sender_email = email
        .from
        .first()
        .map(|a| a.email.clone())
        .unwrap_or_default();

    if sender_email.is_empty() {
        return Err(Error::BadRequest("No sender found".into()));
    }

    // Query all emails from this sender
    let query = search::parse_query(&format!("from:{sender_email}"));
    let all_ids = jmap::query_emails(&session, None, 500, 0, Some(&query)).await?;

    // Archive all
    let archived = jmap::archive_batch(&session, &all_ids).await?;

    Ok(Json(serde_json::json!({
        "success": true,
        "archived": archived,
        "sender": sender_email
    })))
}

// =============================================================================
// Splits CRUD
// =============================================================================

async fn list_splits(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = splits::load_splits(
        &state.splits_config_path,
        std::env::var("VIMMAIL_SPLITS").ok().as_deref(),
    );
    Json(serde_json::json!(config.splits))
}

async fn create_split(
    State(state): State<Arc<AppState>>,
    Json(new_split): Json<SplitInbox>,
) -> Result<impl IntoResponse, Error> {
    let mut config = splits::load_splits(
        &state.splits_config_path,
        std::env::var("VIMMAIL_SPLITS").ok().as_deref(),
    );

    // Check for duplicate ID
    if config.splits.iter().any(|s| s.id == new_split.id) {
        return Err(Error::BadRequest(format!(
            "Split with id '{}' already exists",
            new_split.id
        )));
    }

    config.splits.push(new_split);
    splits::save_splits(&config, &state.splits_config_path)?;

    Ok(Json(serde_json::json!(config.splits)))
}

async fn update_split(
    State(state): State<Arc<AppState>>,
    Path(split_id): Path<String>,
    Json(updated): Json<SplitInbox>,
) -> Result<impl IntoResponse, Error> {
    let mut config = splits::load_splits(
        &state.splits_config_path,
        std::env::var("VIMMAIL_SPLITS").ok().as_deref(),
    );

    let existing = config
        .splits
        .iter_mut()
        .find(|s| s.id == split_id)
        .ok_or_else(|| Error::NotFound(format!("Split '{split_id}' not found")))?;

    *existing = updated;
    splits::save_splits(&config, &state.splits_config_path)?;

    Ok(Json(serde_json::json!(config.splits)))
}

async fn delete_split(
    State(state): State<Arc<AppState>>,
    Path(split_id): Path<String>,
) -> Result<impl IntoResponse, Error> {
    let mut config = splits::load_splits(
        &state.splits_config_path,
        std::env::var("VIMMAIL_SPLITS").ok().as_deref(),
    );

    let original_len = config.splits.len();
    config.splits.retain(|s| s.id != split_id);

    if config.splits.len() == original_len {
        return Err(Error::NotFound(format!("Split '{split_id}' not found")));
    }

    splits::save_splits(&config, &state.splits_config_path)?;

    Ok(Json(serde_json::json!(config.splits)))
}

// External dep for theme path
mod dirs_next {
    pub fn config_dir() -> Option<std::path::PathBuf> {
        std::env::var("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .ok()
            .or_else(|| {
                std::env::var("HOME")
                    .map(|h| std::path::PathBuf::from(h).join(".config"))
                    .ok()
            })
    }
}
