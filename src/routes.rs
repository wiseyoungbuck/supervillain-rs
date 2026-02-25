use axum::{
    Router,
    body::Bytes,
    extract::{Json, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post, put},
};
use serde::Deserialize;
use std::sync::Arc;

use crate::error::Error;
use crate::types::*;
use crate::{calendar, jmap, search, splits};

const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/app.js");
const STYLE_CSS: &str = include_str!("../static/style.css");

const MOBILE_HTML: &str = include_str!("../static/mobile/index.html");
const MOBILE_APP_JS: &str = include_str!("../static/mobile/app.js");
const MOBILE_JMAP_JS: &str = include_str!("../static/mobile/jmap.js");
const MOBILE_MANIFEST: &str = include_str!("../static/mobile/manifest.json");
const MOBILE_SW: &str = include_str!("../static/mobile/sw.js");
const MOBILE_ICON_180: &[u8] = include_bytes!("../static/mobile/icon-180.png");
const MOBILE_ICON_192: &[u8] = include_bytes!("../static/mobile/icon-192.png");
const MOBILE_ICON_512: &[u8] = include_bytes!("../static/mobile/icon-512.png");

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
        .route("/api/upload", post(upload_blob))
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
            "/api/emails/{email_id}/attachments/{blob_id}/{filename}",
            get(download_attachment),
        )
        .route(
            "/api/emails/{email_id}/unsubscribe-and-archive-all",
            post(unsubscribe_and_archive),
        )
        .route("/api/split-counts", get(split_counts))
        .route("/api/splits", get(list_splits).post(create_split))
        .route(
            "/api/splits/{split_id}",
            put(update_split).delete(delete_split),
        )
        .with_state(state)
        .route("/", get(index_html))
        .route("/index.html", get(index_html))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        // Mobile PWA
        .route("/mobile", get(mobile_html))
        .route("/mobile/", get(mobile_html))
        .route("/mobile/index.html", get(mobile_html))
        .route("/mobile/app.js", get(mobile_app_js))
        .route("/mobile/jmap.js", get(mobile_jmap_js))
        .route("/mobile/manifest.json", get(mobile_manifest))
        .route("/mobile/sw.js", get(mobile_sw))
        .route("/mobile/icon-180.png", get(mobile_icon_180))
        .route("/mobile/icon-192.png", get(mobile_icon_192))
        .route("/mobile/icon-512.png", get(mobile_icon_512))
}

async fn index_html() -> impl IntoResponse {
    ([("content-type", "text/html; charset=utf-8")], INDEX_HTML)
}

async fn app_js() -> impl IntoResponse {
    (
        [("content-type", "application/javascript; charset=utf-8")],
        APP_JS,
    )
}

async fn style_css() -> impl IntoResponse {
    ([("content-type", "text/css; charset=utf-8")], STYLE_CSS)
}

async fn mobile_html() -> impl IntoResponse {
    ([("content-type", "text/html; charset=utf-8")], MOBILE_HTML)
}

async fn mobile_app_js() -> impl IntoResponse {
    (
        [("content-type", "application/javascript; charset=utf-8")],
        MOBILE_APP_JS,
    )
}

async fn mobile_jmap_js() -> impl IntoResponse {
    (
        [("content-type", "application/javascript; charset=utf-8")],
        MOBILE_JMAP_JS,
    )
}

async fn mobile_manifest() -> impl IntoResponse {
    (
        [("content-type", "application/manifest+json; charset=utf-8")],
        MOBILE_MANIFEST,
    )
}

async fn mobile_sw() -> impl IntoResponse {
    (
        [
            ("content-type", "application/javascript; charset=utf-8"),
            ("service-worker-allowed", "/mobile/"),
        ],
        MOBILE_SW,
    )
}

async fn mobile_icon_180() -> impl IntoResponse {
    ([("content-type", "image/png")], MOBILE_ICON_180)
}

async fn mobile_icon_192() -> impl IntoResponse {
    ([("content-type", "image/png")], MOBILE_ICON_192)
}

async fn mobile_icon_512() -> impl IntoResponse {
    ([("content-type", "image/png")], MOBILE_ICON_512)
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
    html_body: Option<String>,
    in_reply_to: Option<String>,
    from_address: Option<String>,
    #[serde(default)]
    attachments: Vec<Attachment>,
}

#[derive(Deserialize)]
struct RsvpBody {
    status: crate::types::RsvpStatus,
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

    // Overfetch 10x when filtering by split to fill the screen even for sparse splits
    let fetch_limit = if params.split_id.is_some() {
        limit * 10
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

    let mut emails = jmap::get_emails(&session, &email_ids, false, None).await?;

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

    let emails = jmap::get_emails(&session, std::slice::from_ref(&email_id), true, None).await?;
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
        && let Some(event) = calendar::parse_ics(&ics_data)
    {
        // Auto-add invitations to calendar (non-blocking, won't overwrite existing)
        if event.method == "REQUEST" {
            let state_clone = state.clone();
            let ics_clone = ics_data.clone();
            let uid = event.uid.clone();
            tokio::spawn(async move {
                let s = state_clone.session.read().await;
                if let Err(e) = jmap::add_to_calendar(&s, &ics_clone, &uid, true).await {
                    tracing::warn!("CalDAV auto-add failed for {uid}: {e}");
                }
            });
        } else if event.method == "CANCEL" {
            let state_clone = state.clone();
            let uid = event.uid.clone();
            tokio::spawn(async move {
                let s = state_clone.session.read().await;
                if let Err(e) = jmap::remove_from_calendar(&s, &uid).await {
                    tracing::warn!("CalDAV auto-remove failed for {uid}: {e}");
                }
            });
        }
        calendar_event = Some(event);
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
        "attachments": email.attachments,
    })))
}

fn is_safe_path_segment(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains('\0')
        && s != "."
        && s != ".."
}

fn sanitize_filename_for_header(name: &str) -> String {
    name.chars()
        .filter(|&c| c != '"' && c != '\\' && c != '\r' && c != '\n')
        .collect()
}

async fn download_attachment(
    State(state): State<Arc<AppState>>,
    Path((_email_id, blob_id, filename)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, Error> {
    if !is_safe_path_segment(&blob_id) || !is_safe_path_segment(&filename) {
        return Err(Error::BadRequest("Invalid blob_id or filename".into()));
    }

    let session = state.session.read().await;
    let account_id = session.account_id.as_ref().ok_or(Error::NotConnected)?;
    let download_url = session.download_url.as_ref().ok_or(Error::NotConnected)?;

    let url = download_url
        .replace("{accountId}", account_id)
        .replace("{blobId}", &blob_id)
        .replace("{name}", &filename)
        .replace("{type}", "application/octet-stream");

    let resp = session
        .client
        .get(&url)
        .header("Authorization", &session.auth_header)
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(Error::NotFound("Attachment not found".into()));
    }

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    let bytes = resp.bytes().await?;

    let safe_filename = sanitize_filename_for_header(&filename);
    Ok((
        StatusCode::OK,
        [
            ("content-type", content_type),
            (
                "content-disposition",
                format!("attachment; filename=\"{}\"", safe_filename),
            ),
        ],
        bytes,
    ))
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
        html_body: body.html_body,
        in_reply_to: body.in_reply_to,
        references: None,
        attachments: body.attachments,
        calendar_ics: None,
    };

    let result = jmap::send_email(&mut session, &submission, &from_addr, None).await?;

    match result {
        Some(id) => Ok(Json(serde_json::json!({"success": true, "emailId": id}))),
        None => Err(Error::Internal("Failed to send email".into())),
    }
}

const MAX_UPLOAD_SIZE: usize = 25 * 1024 * 1024; // 25 MB

async fn upload_blob(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, Error> {
    if body.len() > MAX_UPLOAD_SIZE {
        return Err(Error::BadRequest(format!(
            "File too large ({} bytes, max {})",
            body.len(),
            MAX_UPLOAD_SIZE
        )));
    }

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream");

    let raw_filename = headers
        .get("x-filename")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("attachment");
    let filename = sanitize_filename_for_header(raw_filename);

    let session = state.session.read().await;
    let account_id = session.account_id.as_ref().ok_or(Error::NotConnected)?;
    let upload_url = session.upload_url.as_ref().ok_or(Error::NotConnected)?;

    let url = upload_url.replace("{accountId}", account_id);

    let resp = session
        .client
        .post(&url)
        .header("Authorization", &session.auth_header)
        .header("Content-Type", content_type)
        .body(reqwest::Body::from(body))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(Error::Internal(format!("Upload failed ({status}): {text}")));
    }

    let result: serde_json::Value = resp.json().await?;
    let blob_id = result["blobId"]
        .as_str()
        .ok_or_else(|| Error::Internal("Missing blobId in upload response".into()))?;
    let size = result["size"].as_i64().unwrap_or(0);

    Ok(Json(serde_json::json!({
        "blob_id": blob_id,
        "name": filename,
        "mime_type": content_type,
        "size": size,
    })))
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
            jmap::get_emails(&session_guard, std::slice::from_ref(&email_id), false, None).await?;
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

    // Send RSVP as email to organizer with text/calendar MIME part
    let submission = EmailSubmission {
        to: vec![event.organizer_email.clone()],
        cc: vec![],
        subject: format!("Re: {}", event.summary),
        text_body: format!(
            "{} has {} the invitation: {}",
            attendee_email,
            body.status.as_ics_str().to_lowercase(),
            event.summary
        ),
        bcc: None,
        html_body: None,
        in_reply_to: None,
        references: None,
        attachments: vec![],
        calendar_ics: Some(rsvp_ics),
    };

    if let Err(e) = jmap::send_email(&mut session_guard, &submission, &attendee_email, None).await {
        tracing::warn!(
            "Failed to send iTIP reply to {}: {e}",
            event.organizer_email
        );
    }

    // Decline = remove from calendar; Accept/Maybe = upsert original ICS with updated PARTSTAT
    if body.status == RsvpStatus::Declined {
        if let Err(e) = jmap::remove_from_calendar(&session_guard, &event.uid).await {
            tracing::warn!("CalDAV delete failed for {}: {e}", event.uid);
        }
    } else {
        let updated_ics = calendar::update_partstat(&ics_data, &attendee_email, &body.status);
        if let Err(e) = jmap::add_to_calendar(&session_guard, &updated_ics, &event.uid, false).await
        {
            tracing::warn!("CalDAV write failed for {}: {e}", event.uid);
        }
    }

    // Update the parsed event's attendee status for the frontend response
    let mut updated_event = event;
    if let Some(att) = updated_event
        .attendees
        .iter_mut()
        .find(|a| a.email.eq_ignore_ascii_case(&attendee_email))
    {
        att.status = body.status.as_ics_str().to_string();
    }
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

    let event = calendar::parse_ics(&ics_data)
        .ok_or_else(|| Error::Internal("Failed to parse calendar data".into()))?;

    let success = jmap::add_to_calendar(&session, &ics_data, &event.uid, false).await?;

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
    let emails = jmap::get_emails(&session, std::slice::from_ref(&email_id), true, None).await?;
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

    // Query all emails from this sender using structured filter (not string interpolation)
    let query = crate::types::ParsedQuery {
        from: vec![sender_email.clone()],
        ..Default::default()
    };
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

#[derive(Deserialize)]
struct SplitCountsParams {
    mailbox_id: String,
}

async fn split_counts(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SplitCountsParams>,
) -> Result<impl IntoResponse, Error> {
    let start = std::time::Instant::now();

    let config = splits::load_splits(
        &state.splits_config_path,
        std::env::var("VIMMAIL_SPLITS").ok().as_deref(),
    );
    if config.splits.is_empty() {
        return Ok(Json(serde_json::json!({})));
    }

    let session = state.session.read().await;

    // Use the same window as the list view (150 * 10 = 1500) so counts match what's shown
    let fetch_limit = 1500;
    let email_ids =
        jmap::query_emails(&session, Some(&params.mailbox_id), fetch_limit, 0, None).await?;

    let minimal_props: &[&str] = &["id", "from", "to", "cc", "subject"];
    let all_emails =
        jmap::get_emails(&session, &email_ids, false, Some(minimal_props)).await?;

    let mut counts = serde_json::Map::new();
    for split in &config.splits {
        let count = all_emails
            .iter()
            .filter(|e| splits::matches_split(e, split))
            .count();
        counts.insert(split.id.clone(), serde_json::json!(count));
    }

    tracing::debug!(
        "split-counts: {} emails, {} splits, {:.0}ms",
        all_emails.len(),
        config.splits.len(),
        start.elapsed().as_millis()
    );

    Ok(Json(serde_json::Value::Object(counts)))
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    #[tokio::test]
    async fn index_html_contains_html() {
        let resp = index_html().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(ct, "text/html; charset=utf-8");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(
            text.contains("<html"),
            "index.html should contain <html tag"
        );
    }

    #[tokio::test]
    async fn app_js_contains_javascript() {
        let resp = app_js().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(ct, "application/javascript; charset=utf-8");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(!body.is_empty(), "app.js should not be empty");
    }

    #[tokio::test]
    async fn style_css_contains_css() {
        let resp = style_css().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(ct, "text/css; charset=utf-8");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(!body.is_empty(), "style.css should not be empty");
    }

    #[test]
    fn identity_serialization_preserves_email_field() {
        let identity = crate::types::Identity {
            id: "id1".into(),
            email: "test@example.com".into(),
            name: "Test User".into(),
        };
        let json = serde_json::to_string(&identity).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["email"], "test@example.com");
        assert_eq!(parsed["name"], "Test User");
    }

    #[test]
    fn safe_path_segment_rejects_traversal() {
        assert!(!is_safe_path_segment("../etc/passwd"));
        assert!(!is_safe_path_segment(".."));
        assert!(!is_safe_path_segment("."));
        assert!(!is_safe_path_segment("foo/bar"));
        assert!(!is_safe_path_segment("foo\\bar"));
        assert!(!is_safe_path_segment(""));
        assert!(!is_safe_path_segment("foo\0bar"));
    }

    #[test]
    fn safe_path_segment_accepts_valid() {
        assert!(is_safe_path_segment("blob-abc123"));
        assert!(is_safe_path_segment("report.pdf"));
        assert!(is_safe_path_segment("G1234abcdef"));
        assert!(is_safe_path_segment("file..backup.pdf"));
    }

    #[test]
    fn sanitize_filename_strips_dangerous_chars() {
        assert_eq!(sanitize_filename_for_header("normal.pdf"), "normal.pdf");
        assert_eq!(sanitize_filename_for_header("file\".txt"), "file.txt");
        assert_eq!(
            sanitize_filename_for_header("file\r\ninjected"),
            "fileinjected"
        );
        assert_eq!(
            sanitize_filename_for_header("file\\\"name.txt"),
            "filename.txt"
        );
    }

    #[test]
    fn compose_defaults_to_first_identity() {
        assert!(
            APP_JS.contains("state.identities[0].email"),
            "clearCompose should default to the first identity's email"
        );
    }

    // =========================================================================
    // Mobile PWA tests
    // =========================================================================

    #[tokio::test]
    async fn mobile_html_serves_pwa_shell() {
        let resp = mobile_html().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(ct, "text/html; charset=utf-8");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(
            text.contains("<html"),
            "mobile html should contain <html tag"
        );
        assert!(
            text.contains("apple-mobile-web-app-capable"),
            "mobile html should have iOS PWA meta tag"
        );
        assert!(
            text.contains("manifest.json"),
            "mobile html should link to manifest"
        );
        assert!(
            text.contains("viewport-fit=cover"),
            "mobile html should use viewport-fit=cover for notch support"
        );
    }

    #[test]
    fn mobile_jmap_module_has_session_discovery() {
        assert!(
            MOBILE_JMAP_JS.contains("api.fastmail.com/jmap/session"),
            "jmap.js should connect directly to Fastmail JMAP"
        );
        assert!(
            MOBILE_JMAP_JS.contains("Bearer"),
            "jmap.js should use Bearer token auth"
        );
    }

    #[test]
    fn mobile_jmap_module_handles_offline_errors() {
        assert!(
            MOBILE_JMAP_JS.contains("JmapAuthError"),
            "jmap.js should throw JmapAuthError on auth failure"
        );
        assert!(
            MOBILE_JMAP_JS.contains("JmapNetworkError"),
            "jmap.js should throw JmapNetworkError on network failure"
        );
    }

    #[tokio::test]
    async fn mobile_manifest_serves_json() {
        let resp = mobile_manifest().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(ct, "application/manifest+json; charset=utf-8");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&body).expect("manifest should be valid JSON");
        assert_eq!(manifest["display"], "standalone");
        assert_eq!(manifest["start_url"], "/mobile/");
        assert!(
            manifest["icons"].as_array().unwrap().len() >= 2,
            "manifest should have at least 2 icon sizes"
        );
    }

    #[tokio::test]
    async fn mobile_sw_serves_javascript_with_scope_header() {
        let resp = mobile_sw().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(ct, "application/javascript; charset=utf-8");
        let scope = resp
            .headers()
            .get("service-worker-allowed")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(scope, "/mobile/");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(
            text.contains("api.fastmail.com"),
            "SW should exclude JMAP API from caching"
        );
        assert!(
            text.contains("resp.ok"),
            "SW should only cache successful responses"
        );
    }

    async fn assert_png_icon(resp: axum::response::Response, label: &str) {
        assert_eq!(resp.status(), StatusCode::OK, "icon-{label} should be OK");
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(ct, "image/png", "icon-{label} should be image/png");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            body.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
            "icon-{label} should have PNG magic bytes"
        );
    }

    #[tokio::test]
    async fn mobile_icon_180_serves_png() {
        assert_png_icon(mobile_icon_180().await.into_response(), "180").await;
    }

    #[tokio::test]
    async fn mobile_icon_192_serves_png() {
        assert_png_icon(mobile_icon_192().await.into_response(), "192").await;
    }

    #[tokio::test]
    async fn mobile_icon_512_serves_png() {
        assert_png_icon(mobile_icon_512().await.into_response(), "512").await;
    }

    #[tokio::test]
    async fn mobile_jmap_js_serves_es_module() {
        let resp = mobile_jmap_js().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(ct, "application/javascript; charset=utf-8");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(
            text.contains("export async function connect("),
            "jmap.js should export connect()"
        );
        assert!(
            text.contains("export async function jmapCall("),
            "jmap.js should export jmapCall()"
        );
        assert!(
            text.contains("export function getSession("),
            "jmap.js should export getSession()"
        );
    }

    #[test]
    fn mobile_jmap_js_has_error_types() {
        assert!(
            MOBILE_JMAP_JS.contains("export class JmapAuthError"),
            "jmap.js should export JmapAuthError"
        );
        assert!(
            MOBILE_JMAP_JS.contains("export class JmapNetworkError"),
            "jmap.js should export JmapNetworkError"
        );
    }

    #[test]
    fn mobile_jmap_js_uses_jmap_capabilities() {
        assert!(
            MOBILE_JMAP_JS.contains("urn:ietf:params:jmap:core"),
            "jmap.js should declare jmap:core capability"
        );
        assert!(
            MOBILE_JMAP_JS.contains("urn:ietf:params:jmap:mail"),
            "jmap.js should declare jmap:mail capability"
        );
        assert!(
            MOBILE_JMAP_JS.contains("urn:ietf:params:jmap:submission"),
            "jmap.js should declare jmap:submission capability"
        );
    }

    #[test]
    fn mobile_jmap_js_handles_auth_errors() {
        // 401 and 403 should throw auth errors, not network errors
        assert!(
            MOBILE_JMAP_JS.contains("resp.status === 401"),
            "jmap.js should handle 401 status"
        );
        assert!(
            MOBILE_JMAP_JS.contains("resp.status === 403"),
            "jmap.js should handle 403 status"
        );
    }

    #[test]
    fn mobile_jmap_js_has_blob_url_builder() {
        assert!(
            MOBILE_JMAP_JS.contains("export function blobUrl("),
            "jmap.js should export blobUrl()"
        );
        assert!(
            MOBILE_JMAP_JS.contains("{accountId}"),
            "blobUrl should replace accountId template"
        );
        assert!(
            MOBILE_JMAP_JS.contains("{blobId}"),
            "blobUrl should replace blobId template"
        );
    }

    #[test]
    fn mobile_html_imports_app_module() {
        assert!(
            MOBILE_HTML.contains("type=\"module\""),
            "mobile html script should be type=module"
        );
        assert!(
            MOBILE_HTML.contains("src=\"/mobile/app.js\""),
            "mobile html should load app.js as ES module"
        );
    }

    #[test]
    fn mobile_html_has_email_list_structure() {
        assert!(
            MOBILE_HTML.contains("id=\"email-list-wrap\""),
            "mobile html should have email list scroll container"
        );
        assert!(
            MOBILE_HTML.contains("id=\"email-list\""),
            "mobile html should have email list element"
        );
        assert!(
            MOBILE_HTML.contains("id=\"pull-indicator\""),
            "mobile html should have pull-to-refresh indicator"
        );
    }

    #[tokio::test]
    async fn mobile_app_js_serves_es_module() {
        let resp = mobile_app_js().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(ct, "application/javascript; charset=utf-8");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(
            text.contains("from '/mobile/jmap.js'"),
            "app.js should import from jmap.js"
        );
    }

    #[test]
    fn mobile_app_js_has_email_list_rendering() {
        assert!(
            MOBILE_APP_JS.contains("renderEmailList"),
            "app.js should have renderEmailList function"
        );
        assert!(
            MOBILE_APP_JS.contains("email-row"),
            "app.js should render email rows"
        );
        assert!(
            MOBILE_APP_JS.contains("getDateGroup"),
            "app.js should group emails by date"
        );
    }

    #[test]
    fn mobile_app_js_has_pull_to_refresh() {
        assert!(
            MOBILE_APP_JS.contains("setupPullToRefresh"),
            "app.js should set up pull-to-refresh"
        );
        assert!(
            MOBILE_APP_JS.contains("touchstart"),
            "pull-to-refresh should use touchstart events"
        );
        assert!(
            MOBILE_APP_JS.contains("touchend"),
            "pull-to-refresh should use touchend events"
        );
    }

    #[test]
    fn mobile_app_js_has_infinite_scroll() {
        assert!(
            MOBILE_APP_JS.contains("setupInfiniteScroll"),
            "app.js should set up infinite scroll"
        );
        assert!(
            MOBILE_APP_JS.contains("loadMoreEmails"),
            "app.js should load more emails on scroll"
        );
    }

    #[test]
    fn mobile_jmap_js_has_data_functions() {
        assert!(
            MOBILE_JMAP_JS.contains("export async function getMailboxes("),
            "jmap.js should export getMailboxes()"
        );
        assert!(
            MOBILE_JMAP_JS.contains("export async function getIdentities("),
            "jmap.js should export getIdentities()"
        );
        assert!(
            MOBILE_JMAP_JS.contains("export async function queryEmails("),
            "jmap.js should export queryEmails()"
        );
        assert!(
            MOBILE_JMAP_JS.contains("export async function getEmails("),
            "jmap.js should export getEmails()"
        );
    }

    #[test]
    fn mobile_jmap_js_parses_email_keywords() {
        assert!(
            MOBILE_JMAP_JS.contains("$seen"),
            "jmap.js should check $seen keyword for unread status"
        );
        assert!(
            MOBILE_JMAP_JS.contains("$flagged"),
            "jmap.js should check $flagged keyword for star status"
        );
    }

    #[test]
    fn mobile_sw_caches_app_shell() {
        assert!(
            MOBILE_SW.contains("/mobile/app.js"),
            "service worker should cache app.js in app shell"
        );
        assert!(
            MOBILE_SW.contains("/mobile/jmap.js"),
            "service worker should cache jmap.js in app shell"
        );
    }

    #[test]
    fn mobile_html_has_touch_optimized_styles() {
        assert!(
            MOBILE_HTML.contains("min-height: 72px"),
            "email rows should have min-height for 44px+ touch targets"
        );
        assert!(
            MOBILE_HTML.contains("-webkit-overflow-scrolling: touch"),
            "email list should use momentum scrolling"
        );
    }
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
