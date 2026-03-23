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
use crate::{calendar, provider, search, splits, theme};

const SPLIT_OVERFETCH_MULTIPLIER: usize = 10;

const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/app.js");
const STYLE_CSS: &str = include_str!("../static/style.css");

const MOBILE_HTML: &str = include_str!("../static/mobile/index.html");
const MOBILE_APP_JS: &str = include_str!("../static/mobile/app.js");
const MOBILE_JMAP_JS: &str = include_str!("../static/mobile/jmap.js");
const MOBILE_MANIFEST: &str = include_str!("../static/mobile/manifest.json");
const MOBILE_SW: &str = include_str!("../static/mobile/sw.js");
const FAVICON_32: &[u8] = include_bytes!("../static/favicon-32.png");
const ICON_180: &[u8] = include_bytes!("../static/icon-180.png");
const ICON_192: &[u8] = include_bytes!("../static/icon-192.png");
const ICON_512: &[u8] = include_bytes!("../static/icon-512.png");
const SUPERVILLAIN_JPG: &[u8] = include_bytes!("../static/supervillain.jpg");

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
        .route("/favicon-32.png", get(favicon_32))
        .route("/icon-180.png", get(icon_180))
        .route("/icon-192.png", get(icon_192))
        .route("/icon-512.png", get(icon_512))
        .route("/supervillain.jpg", get(supervillain_jpg))
        // Mobile PWA
        .route("/mobile", get(mobile_html))
        .route("/mobile/", get(mobile_html))
        .route("/mobile/index.html", get(mobile_html))
        .route("/mobile/app.js", get(mobile_app_js))
        .route("/mobile/jmap.js", get(mobile_jmap_js))
        .route("/mobile/manifest.json", get(mobile_manifest))
        .route("/mobile/sw.js", get(mobile_sw))
        .route("/mobile/icon-180.png", get(icon_180))
        .route("/mobile/icon-192.png", get(icon_192))
        .route("/mobile/icon-512.png", get(icon_512))
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

async fn favicon_32() -> impl IntoResponse {
    ([("content-type", "image/png")], FAVICON_32)
}

async fn icon_180() -> impl IntoResponse {
    ([("content-type", "image/png")], ICON_180)
}

async fn icon_192() -> impl IntoResponse {
    ([("content-type", "image/png")], ICON_192)
}

async fn icon_512() -> impl IntoResponse {
    ([("content-type", "image/png")], ICON_512)
}

async fn supervillain_jpg() -> impl IntoResponse {
    ([("content-type", "image/jpeg")], SUPERVILLAIN_JPG)
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
    account: Option<String>,
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

#[derive(Deserialize, Default)]
struct AccountParam {
    account: Option<String>,
}

// =============================================================================
// Account resolution
// =============================================================================

fn resolve_session<'a>(
    state: &'a AppState,
    account: Option<&str>,
) -> Result<&'a tokio::sync::RwLock<crate::provider::ProviderSession>, Error> {
    let key = account.unwrap_or(&state.default_account);
    state
        .sessions
        .get(key)
        .ok_or_else(|| Error::BadRequest(format!("Unknown account: {key}")))
}

// =============================================================================
// Handlers
// =============================================================================

async fn list_accounts(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut accounts = Vec::new();
    for (name, session_lock) in &state.sessions {
        let session = session_lock.read().await;
        accounts.push(serde_json::json!({
            "id": name,
            "email": session.username(),
            "provider": session.provider_name(),
            "isDefault": *name == state.default_account
        }));
    }
    Json(serde_json::json!(accounts))
}

async fn list_identities(
    State(state): State<Arc<AppState>>,
    Query(params): Query<AccountParam>,
) -> Result<impl IntoResponse, Error> {
    let session_lock = resolve_session(&state, params.account.as_deref())?;
    let mut session = session_lock.write().await;
    let identities = provider::get_identities(&mut session).await?;
    Ok(Json(serde_json::json!(identities)))
}

async fn get_theme() -> impl IntoResponse {
    let theme_dir = dirs_next::config_dir()
        .unwrap_or_default()
        .join("omarchy/current/theme");

    // 1. Prefer supervillain.css (template-generated for colors.toml themes)
    if let Ok(css) = std::fs::read_to_string(theme_dir.join("supervillain.css"))
        && !css.is_empty()
    {
        return (StatusCode::OK, [("content-type", "text/css")], css);
    }

    // 2. Parse terminal color config (ghostty.conf → alacritty.toml)
    if let Some(colors) = theme::load_from_theme_dir(&theme_dir) {
        let is_light = theme::is_light_theme(&theme_dir);
        let css = theme::generate_theme_css(&colors, is_light);
        return (StatusCode::OK, [("content-type", "text/css")], css);
    }

    // 3. No theme available — base CSS defaults apply
    (
        StatusCode::OK,
        [("content-type", "text/css")],
        String::new(),
    )
}

async fn list_mailboxes(
    State(state): State<Arc<AppState>>,
    Query(params): Query<AccountParam>,
) -> Result<impl IntoResponse, Error> {
    let session_lock = resolve_session(&state, params.account.as_deref())?;
    let session = session_lock.read().await;
    let mailboxes = provider::get_mailboxes(&session).await?;
    Ok(Json(serde_json::json!(mailboxes)))
}

async fn list_emails(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListEmailsParams>,
) -> Result<impl IntoResponse, Error> {
    let session_lock = resolve_session(&state, params.account.as_deref())?;
    let session = session_lock.read().await;
    let limit = params.limit.unwrap_or(150);
    let offset = params.offset.unwrap_or(0);

    let query = params.search.as_deref().map(search::parse_query);
    let query_ref = query.as_ref();

    let fetch_limit = if params.split_id.is_some() {
        limit * SPLIT_OVERFETCH_MULTIPLIER
    } else {
        limit
    };

    let email_ids = provider::query_emails(
        &session,
        params.mailbox_id.as_deref(),
        fetch_limit,
        offset,
        query_ref,
    )
    .await?;

    let mut emails = provider::get_emails(&session, &email_ids, false, None).await?;

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
    Query(params): Query<AccountParam>,
) -> Result<impl IntoResponse, Error> {
    let account_key = params
        .account
        .clone()
        .unwrap_or_else(|| state.default_account.clone());
    let session_lock = resolve_session(&state, Some(&account_key))?;
    let session = session_lock.read().await;

    let emails =
        provider::get_emails(&session, std::slice::from_ref(&email_id), true, None).await?;
    let email = emails
        .first()
        .ok_or_else(|| Error::NotFound("Email not found".into()))?;

    // Auto mark-read
    if email.is_unread() {
        let _ = provider::mark_read(&session, &email_id).await;
    }

    // Check for calendar event
    let mut calendar_event = None;
    if email.has_calendar
        && let Ok(Some(ics_data)) = jmap::get_calendar_data(&session, &email_id).await
        && let Some(mut event) = calendar::parse_ics(&ics_data)
    {
        // Auto-add invitations to calendar (non-blocking, won't overwrite existing)
        if event.method == "REQUEST" {
            let state_clone = state.clone();
            let ics_clone = ics_data.clone();
            let uid = event.uid.clone();
            let acct = account_key.clone();
            tokio::spawn(async move {
                if let Ok(s_lock) = resolve_session(&state_clone, Some(&acct)) {
                    let s = s_lock.read().await;
                    if let Err(e) = provider::add_to_calendar(&s, &ics_clone, &uid, true).await {
                        tracing::warn!("Calendar auto-add failed for {uid}: {e}");
                    }
                }
            });
        } else if event.method == "CANCEL" {
            let state_clone = state.clone();
            let uid = event.uid.clone();
            let acct = account_key.clone();
            tokio::spawn(async move {
                if let Ok(s_lock) = resolve_session(&state_clone, Some(&acct)) {
                    let s = s_lock.read().await;
                    if let Err(e) = provider::remove_from_calendar(&s, &uid).await {
                        tracing::warn!("Calendar auto-remove failed for {uid}: {e}");
                    }
                }
            });
        }
<<<<<<< HEAD
        // Merge current PARTSTAT from calendar (CalDAV/Graph) so the UI
        // reflects the user's actual RSVP status, not the stale email ICS
        let mut event = event;
        match provider::get_calendar_event(&session, &event.uid).await {
            Ok(Some(cal_event)) => {
                for att in &mut event.attendees {
                    if let Some(cal_att) = cal_event
                        .attendees
                        .iter()
                        .find(|a| a.email.eq_ignore_ascii_case(&att.email))
                    {
                        att.status = cal_att.status.clone();
                    }
                }
                tracing::debug!("Merged calendar PARTSTAT for event {}", event.uid);
            }
            Ok(None) => {
                tracing::debug!("Event {} not in calendar yet, using email ICS", event.uid);
            }
            Err(e) => {
                tracing::warn!(
                    "Calendar fetch failed for {}, falling back to email ICS: {e}",
                    event.uid
                );
=======
        // Query CalDAV for persisted RSVP status
        if event.method == "REQUEST" {
            let attendee_email = determine_attendee_email(email, &event, &session.username);
            if let Some(status) = jmap::get_rsvp_status(&session, &event.uid, &attendee_email).await
            {
                event.user_rsvp_status = Some(status);
>>>>>>> 63466e3 (Persist RSVP state via CalDAV and add re-RSVP guard + keyboard shortcuts)
            }
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
        .filter(|&c| c != '"' && c != '\\' && c != '\r' && c != '\n' && c != '\0')
        .collect()
}

async fn download_attachment(
    State(state): State<Arc<AppState>>,
    Path((_email_id, blob_id, filename)): Path<(String, String, String)>,
    Query(params): Query<AccountParam>,
) -> Result<impl IntoResponse, Error> {
    if !is_safe_path_segment(&blob_id) || !is_safe_path_segment(&filename) {
        return Err(Error::BadRequest("Invalid blob_id or filename".into()));
    }

    let session_lock = resolve_session(&state, params.account.as_deref())?;
    let session = session_lock.read().await;

    let (content_type, bytes) = provider::download_blob(&session, &blob_id, &filename).await?;

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
    Query(params): Query<AccountParam>,
) -> Result<impl IntoResponse, Error> {
    let session_lock = resolve_session(&state, params.account.as_deref())?;
    let session = session_lock.read().await;
    let success = provider::archive(&session, &email_id).await?;
    Ok(Json(serde_json::json!({"success": success})))
}

async fn trash_email(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
    Query(params): Query<AccountParam>,
) -> Result<impl IntoResponse, Error> {
    let session_lock = resolve_session(&state, params.account.as_deref())?;
    let session = session_lock.read().await;
    let success = provider::trash(&session, &email_id).await?;
    Ok(Json(serde_json::json!({"success": success})))
}

async fn mark_read(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
    Query(params): Query<AccountParam>,
) -> Result<impl IntoResponse, Error> {
    let session_lock = resolve_session(&state, params.account.as_deref())?;
    let session = session_lock.read().await;
    let success = provider::mark_read(&session, &email_id).await?;
    Ok(Json(serde_json::json!({"success": success})))
}

async fn mark_unread(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
    Query(params): Query<AccountParam>,
) -> Result<impl IntoResponse, Error> {
    let session_lock = resolve_session(&state, params.account.as_deref())?;
    let session = session_lock.read().await;
    let success = provider::mark_unread(&session, &email_id).await?;
    Ok(Json(serde_json::json!({"success": success})))
}

async fn toggle_flag(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
    Query(params): Query<AccountParam>,
) -> Result<impl IntoResponse, Error> {
    let session_lock = resolve_session(&state, params.account.as_deref())?;
    let session = session_lock.read().await;
    let success = provider::toggle_flag(&session, &email_id).await?;
    Ok(Json(serde_json::json!({"success": success})))
}

async fn move_email(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
    Query(params): Query<AccountParam>,
    Json(body): Json<MoveBody>,
) -> Result<impl IntoResponse, Error> {
    let session_lock = resolve_session(&state, params.account.as_deref())?;
    let session = session_lock.read().await;
    let success = provider::move_to_mailbox(&session, &email_id, &body.mailbox_id).await?;
    Ok(Json(serde_json::json!({"success": success})))
}

async fn send_email_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<AccountParam>,
    Json(body): Json<SendEmailBody>,
) -> Result<impl IntoResponse, Error> {
    let session_lock = resolve_session(&state, params.account.as_deref())?;
    let mut session = session_lock.write().await;
    let from_addr = body
        .from_address
        .as_deref()
        .unwrap_or(session.username())
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

    let result = provider::send_email(&mut session, &submission, &from_addr, None).await?;

    match result {
        Some(id) => Ok(Json(serde_json::json!({"success": true, "emailId": id}))),
        None => Err(Error::Internal("Failed to send email".into())),
    }
}

const MAX_UPLOAD_SIZE: usize = 25 * 1024 * 1024; // 25 MB

async fn upload_blob(
    State(state): State<Arc<AppState>>,
    Query(params): Query<AccountParam>,
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

    let session_lock = resolve_session(&state, params.account.as_deref())?;
    let session = session_lock.read().await;

    let (blob_id, size) = provider::upload_blob(&session, content_type, &body).await?;

    Ok(Json(serde_json::json!({
        "blob_id": blob_id,
        "name": filename,
        "mime_type": content_type,
        "size": size,
    })))
}

fn determine_attendee_email(email: &Email, event: &CalendarEvent, fallback: &str) -> String {
    for addr in email.to.iter().chain(email.cc.iter()) {
        if event
            .attendees
            .iter()
            .any(|a| a.email.eq_ignore_ascii_case(&addr.email))
        {
            return addr.email.clone();
        }
    }
    fallback.to_string()
}

async fn rsvp(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
    Query(params): Query<AccountParam>,
    Json(body): Json<RsvpBody>,
) -> Result<impl IntoResponse, Error> {
    let session_lock = resolve_session(&state, params.account.as_deref())?;
    let mut session_guard = session_lock.write().await;

    // Get calendar data
    let ics_data = provider::get_calendar_data(&session_guard, &email_id)
        .await?
        .ok_or_else(|| Error::NotFound("No calendar data found".into()))?;

    let event = calendar::parse_ics(&ics_data)
        .ok_or_else(|| Error::Internal("Failed to parse calendar data".into()))?;

    // Determine attendee email (use account username as fallback)
    let attendee_email = {
        let emails =
            provider::get_emails(&session_guard, std::slice::from_ref(&email_id), false, None)
                .await?;
        let email = emails
            .first()
            .ok_or_else(|| Error::NotFound("Email not found".into()))?;
<<<<<<< HEAD

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
        found.unwrap_or_else(|| session_guard.username().to_string())
=======
        determine_attendee_email(email, &event, &session_guard.username)
>>>>>>> 63466e3 (Persist RSVP state via CalDAV and add re-RSVP guard + keyboard shortcuts)
    };

    // Dispatch full RSVP flow to provider (Fastmail: iTIP email + CalDAV, Outlook: Graph API)
    provider::rsvp(
        &mut session_guard,
        &ics_data,
        &event,
        &attendee_email,
        &body.status,
    )
    .await?;

    // Update the parsed event's attendee status for the frontend response
    let mut updated_event = event;
    if let Some(att) = updated_event
        .attendees
        .iter_mut()
        .find(|a| a.email.eq_ignore_ascii_case(&attendee_email))
    {
        att.status = body.status.as_ics_str().to_string();
    }
<<<<<<< HEAD
=======
    updated_event.user_rsvp_status = Some(body.status.as_ics_str().to_string());
>>>>>>> 63466e3 (Persist RSVP state via CalDAV and add re-RSVP guard + keyboard shortcuts)
    Ok(Json(serde_json::json!({ "calendarEvent": updated_event })))
}

async fn add_to_calendar(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
    Query(params): Query<AccountParam>,
) -> Result<impl IntoResponse, Error> {
    let session_lock = resolve_session(&state, params.account.as_deref())?;
    let session = session_lock.read().await;

    let ics_data = provider::get_calendar_data(&session, &email_id)
        .await?
        .ok_or_else(|| Error::NotFound("No calendar data found".into()))?;

    let event = calendar::parse_ics(&ics_data)
        .ok_or_else(|| Error::Internal("Failed to parse calendar data".into()))?;

    // Cancellations should remove, not add
    let success = if event.method == "CANCEL" {
        provider::remove_from_calendar(&session, &event.uid).await?
    } else {
        provider::add_to_calendar(&session, &ics_data, &event.uid, false).await?
    };

    if success {
        Ok(Json(serde_json::json!({"success": true})))
    } else {
        Err(Error::Internal("Failed to update calendar".into()))
    }
}

async fn unsubscribe_and_archive(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
    Query(params): Query<AccountParam>,
) -> Result<impl IntoResponse, Error> {
    let session_lock = resolve_session(&state, params.account.as_deref())?;
    let session = session_lock.read().await;

    // Get the email to find the sender
    let emails =
        provider::get_emails(&session, std::slice::from_ref(&email_id), true, None).await?;
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
    let all_ids = provider::query_emails(&session, None, 500, 0, Some(&query)).await?;

    // Archive all
    let archived = provider::archive_batch(&session, &all_ids).await?;

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
    account: Option<String>,
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

    let session_lock = resolve_session(&state, params.account.as_deref())?;
    let session = session_lock.read().await;

    let fetch_limit = 150 * SPLIT_OVERFETCH_MULTIPLIER;
    let email_ids =
        provider::query_emails(&session, Some(&params.mailbox_id), fetch_limit, 0, None).await?;

    let minimal_props: &[&str] = &["id", "from", "to", "cc", "subject"];
    let mut all_emails = Vec::new();
    for batch in email_ids.chunks(500) {
        let emails = provider::get_emails(&session, batch, false, Some(minimal_props)).await?;
        all_emails.extend(emails);
    }

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
    async fn favicon_32_serves_png() {
        assert_png_icon(favicon_32().await.into_response(), "32").await;
    }

    #[tokio::test]
    async fn icon_180_serves_png() {
        assert_png_icon(icon_180().await.into_response(), "180").await;
    }

    #[tokio::test]
    async fn icon_192_serves_png() {
        assert_png_icon(icon_192().await.into_response(), "192").await;
    }

    #[tokio::test]
    async fn icon_512_serves_png() {
        assert_png_icon(icon_512().await.into_response(), "512").await;
    }

    #[tokio::test]
    async fn supervillain_jpg_serves_jpeg() {
        let resp = supervillain_jpg().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(ct, "image/jpeg");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            body.starts_with(&[0xFF, 0xD8, 0xFF]),
            "supervillain.jpg should have JPEG magic bytes"
        );
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

    // =========================================================================
    // Theme tests
    // =========================================================================

    #[test]
    fn style_css_no_undefined_variables() {
        assert!(
            !STYLE_CSS.contains("--text-primary"),
            "style.css should not reference undefined --text-primary"
        );
        assert!(
            !STYLE_CSS.contains("--text-secondary"),
            "style.css should not reference undefined --text-secondary"
        );
    }

    #[tokio::test]
    async fn theme_endpoint_returns_css_content_type() {
        let resp = get_theme().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(ct, "text/css");
    }

    #[test]
    fn theme_fallback_generates_css_from_ghostty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ghostty.conf"),
            "\
background =#1d2021
foreground =#d5c4a1
palette = 0=#1d2021
palette = 1=#cc241d
palette = 2=#b8bb26
palette = 3=#d79921
palette = 4=#83a598
palette = 5=#d3869b
palette = 6=#8ec07c
palette = 7=#d5c4a1
palette = 8=#665c54
palette = 9=#cc241d
palette = 10=#b8bb26
palette = 11=#d79921
palette = 12=#83a598
palette = 13=#d3869b
palette = 14=#b8bb26
palette = 15=#ebdbb2
",
        )
        .unwrap();

        let colors = theme::load_from_theme_dir(dir.path()).unwrap();
        let css = theme::generate_theme_css(&colors, false);
        assert!(css.contains("--bg: #1d2021;"));
        assert!(css.contains("--fg: #d5c4a1;"));
        assert!(css.contains("--accent: #8ec07c;"));
        assert!(css.contains("#help-overlay"));
        assert!(css.contains("#split-modal"));
    }

    #[test]
    fn theme_fallback_generates_css_from_alacritty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("alacritty.toml"),
            "\
[colors.primary]
background = '#fdf6e3'
foreground = '#586e75'

[colors.normal]
black   = '#073642'
red     = '#dc322f'
green   = '#859900'
yellow  = '#b58900'
blue    = '#268bd2'
magenta = '#d33682'
cyan    = '#2aa198'
white   = '#eee8d5'

[colors.bright]
black   = '#002b36'
red     = '#cb4b16'
green   = '#586e75'
yellow  = '#657b83'
blue    = '#839496'
magenta = '#6c71c4'
cyan    = '#93a1a1'
white   = '#fdf6e3'
",
        )
        .unwrap();

        let colors = theme::load_from_theme_dir(dir.path()).unwrap();
        let css = theme::generate_theme_css(&colors, false);
        assert!(css.contains("--bg: #fdf6e3;"));
        assert!(css.contains("--accent: #2aa198;"));
    }

    #[test]
    fn theme_light_mode_detected_from_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!theme::is_light_theme(dir.path()));

        std::fs::write(dir.path().join("light.mode"), "").unwrap();
        assert!(theme::is_light_theme(dir.path()));
    }

    // =========================================================================
    // Attachment tests
    // =========================================================================

    #[test]
    fn send_email_body_deserializes_without_attachments() {
        let json = r#"{"to":["a@b.com"],"subject":"Hi","body":"Hello"}"#;
        let body: SendEmailBody = serde_json::from_str(json).unwrap();
        assert!(body.attachments.is_empty());
    }

    #[test]
    fn send_email_body_deserializes_with_attachments() {
        let json = r#"{"to":["a@b.com"],"subject":"Hi","body":"Hello","attachments":[{"blob_id":"B1","name":"doc.pdf","mime_type":"application/pdf","size":1024}]}"#;
        let body: SendEmailBody = serde_json::from_str(json).unwrap();
        assert_eq!(body.attachments.len(), 1);
        assert_eq!(body.attachments[0].blob_id, "B1");
        assert_eq!(body.attachments[0].name, "doc.pdf");
        assert_eq!(body.attachments[0].mime_type, "application/pdf");
        assert_eq!(body.attachments[0].size, 1024);
    }

    #[test]
    fn upload_max_size_constant() {
        assert_eq!(MAX_UPLOAD_SIZE, 25 * 1024 * 1024);
    }

    #[test]
    fn sanitize_filename_unicode() {
        assert_eq!(sanitize_filename_for_header("résumé.pdf"), "résumé.pdf");
    }

    #[test]
    fn sanitize_filename_null_bytes() {
        assert_eq!(
            sanitize_filename_for_header("file\0name.pdf"),
            "filename.pdf"
        );
    }

    #[test]
    fn sanitize_filename_empty() {
        assert_eq!(sanitize_filename_for_header(""), "");
    }

    #[test]
    fn app_js_has_attachment_upload() {
        assert!(
            APP_JS.contains("/api/upload"),
            "app.js should call the upload endpoint"
        );
    }

    #[test]
    fn app_js_has_attachment_size_validation() {
        assert!(
            APP_JS.contains("25 * 1024 * 1024"),
            "app.js should validate 25 MB size limit"
        );
    }

    #[test]
    fn app_js_has_attachment_list_rendering() {
        assert!(
            APP_JS.contains("renderComposeAttachments"),
            "app.js should render the compose attachment list"
        );
    }

    #[test]
    fn app_js_blocks_send_during_upload() {
        assert!(
            APP_JS.contains("status === 'uploading'"),
            "app.js should block send while uploads are in progress"
        );
    }

    #[test]
    fn app_js_clears_attachments_on_cancel() {
        assert!(
            APP_JS.contains("pendingAttachments = []"),
            "app.js should clear attachments when compose is cancelled"
        );
    }

    #[test]
    fn app_js_has_abort_controller() {
        assert!(
            APP_JS.contains("AbortController"),
            "app.js should support cancelling uploads"
        );
    }

    // =========================================================================
    // P1 feature tests
    // =========================================================================

    #[test]
    fn app_js_has_drag_and_drop() {
        assert!(
            APP_JS.contains("dragenter"),
            "app.js should handle dragenter events"
        );
        assert!(
            APP_JS.contains("dataTransfer"),
            "app.js should read dropped files from dataTransfer"
        );
    }

    #[test]
    fn app_js_has_clipboard_paste() {
        assert!(
            APP_JS.contains("clipboardData"),
            "app.js should read pasted files from clipboardData"
        );
        assert!(
            APP_JS.contains("pasted-image"),
            "app.js should generate names for pasted images"
        );
    }

    #[test]
    fn app_js_has_upload_progress() {
        assert!(
            APP_JS.contains("upload.onprogress") || APP_JS.contains("onprogress"),
            "app.js should track upload progress"
        );
    }

    #[test]
    fn app_js_has_attach_keyboard_shortcut() {
        assert!(
            APP_JS.contains("e.ctrlKey && e.shiftKey"),
            "app.js should have Ctrl+Shift+A shortcut for attaching files"
        );
    }

    #[test]
    fn style_css_has_drag_over_style() {
        assert!(
            STYLE_CSS.contains("drag-over"),
            "style.css should style the drag-over state"
        );
    }

    // =========================================================================
    // URL linkification tests
    // =========================================================================

    #[test]
    fn app_js_has_segment_urls_helper() {
        assert!(
            APP_JS.contains("function segmentUrls(text, raw)"),
            "app.js should have a shared segmentUrls helper with raw parameter"
        );
    }

    #[test]
    fn app_js_linkify_text_uses_segment_urls() {
        assert!(
            APP_JS.contains("segmentUrls(text, true)"),
            "linkifyText should find URLs in raw text before escaping"
        );
        assert!(
            APP_JS.contains("escapeHtml(p.url)"),
            "linkifyText should escape URLs individually for safe HTML output"
        );
    }

    #[test]
    fn app_js_linkify_text_escapes_non_url_text() {
        assert!(
            APP_JS.contains("escapeHtml(p.text)"),
            "linkifyText should escape non-URL text segments individually"
        );
    }

    #[test]
    fn app_js_linkify_text_no_bulk_escape() {
        // The old pattern was: escapeHtml(text) then segmentUrls(escaped, false).
        // Now we find URLs in raw text first, then escape each segment.
        // Verify there's no `escapeHtml(text)` call followed by non-raw segmentUrls.
        let linkify_fn = APP_JS
            .find("function linkifyText")
            .expect("should have linkifyText");
        let fn_body = &APP_JS[linkify_fn..APP_JS.len().min(linkify_fn + 300)];
        assert!(
            !fn_body.contains("escapeHtml(text)"),
            "linkifyText must not bulk-escape the input text before URL parsing"
        );
    }

    #[test]
    fn app_js_segment_urls_trims_trailing_punctuation() {
        assert!(
            APP_JS.contains(r#"replace(/[.,;:!?]+$/"#),
            "segmentUrls should trim trailing punctuation from URLs"
        );
    }

    #[test]
    fn app_js_sanitize_html_linkifies_bare_urls() {
        assert!(
            APP_JS.contains("createTreeWalker"),
            "sanitizeHtml should walk text nodes to linkify bare URLs"
        );
        assert!(
            APP_JS.contains("NodeFilter.SHOW_TEXT"),
            "sanitizeHtml should filter for text nodes only"
        );
    }

    #[test]
    fn app_js_sanitize_html_skips_existing_links() {
        assert!(
            APP_JS.contains("closest('a')"),
            "sanitizeHtml should not double-linkify URLs already inside <a> tags"
        );
    }

    #[test]
    fn app_js_sanitize_html_sets_link_security_attrs() {
        // The TreeWalker block sets target and rel on created <a> elements
        let tree_walker_section = APP_JS
            .find("createTreeWalker")
            .expect("should have TreeWalker");
        let after_walker = &APP_JS[tree_walker_section..];
        assert!(
            after_walker.contains("noopener noreferrer"),
            "linkified URLs in sanitizeHtml should have rel=noopener noreferrer"
        );
    }

    #[test]
    fn app_js_segment_urls_raw_mode_allows_ampersand() {
        assert!(
            APP_JS.contains("segmentUrls(node.textContent, true)"),
            "sanitizeHtml should pass raw=true to segmentUrls for unescaped text nodes"
        );
        // The raw regex should not exclude &
        assert!(
            APP_JS.contains(r#"? /https?:\/\/[^\s<>"')\]]+/g"#),
            "raw mode regex should allow & in URLs for query strings"
        );
    }

    #[test]
    fn style_css_email_links_no_underline_by_default() {
        assert!(
            STYLE_CSS.contains("#email-body a"),
            "style.css should style email body links"
        );
        assert!(
            STYLE_CSS.contains("text-decoration: none"),
            "email body links should have no underline by default"
        );
    }

    // =========================================================================
    // determine_attendee_email tests
    // =========================================================================

    fn test_email_with_recipients(to: Vec<&str>, cc: Vec<&str>) -> Email {
        Email {
            id: "test-id".into(),
            blob_id: "blob-id".into(),
            thread_id: "thread-id".into(),
            mailbox_ids: std::collections::HashMap::new(),
            keywords: std::collections::HashMap::new(),
            received_at: chrono::Utc::now(),
            subject: "Test".into(),
            from: vec![EmailAddress {
                name: None,
                email: "sender@example.com".into(),
            }],
            to: to
                .into_iter()
                .map(|e| EmailAddress {
                    name: None,
                    email: e.into(),
                })
                .collect(),
            cc: cc
                .into_iter()
                .map(|e| EmailAddress {
                    name: None,
                    email: e.into(),
                })
                .collect(),
            preview: String::new(),
            has_attachment: false,
            size: 0,
            text_body: None,
            html_body: None,
            has_calendar: false,
            attachments: vec![],
        }
    }

    fn test_calendar_event(attendee_emails: Vec<&str>) -> CalendarEvent {
        CalendarEvent {
            uid: "uid@example.com".into(),
            summary: "Test".into(),
            dtstart: chrono::Utc::now(),
            dtend: None,
            location: None,
            description: None,
            organizer_email: "org@example.com".into(),
            organizer_name: None,
            attendees: attendee_emails
                .into_iter()
                .map(|e| Attendee {
                    email: e.into(),
                    name: None,
                    status: "NEEDS-ACTION".into(),
                })
                .collect(),
            sequence: 0,
            method: "REQUEST".into(),
            raw_ics: String::new(),
            user_rsvp_status: None,
        }
    }

    #[test]
    fn determine_attendee_email_matches_to() {
        let email = test_email_with_recipients(vec!["bob@example.com"], vec![]);
        let event = test_calendar_event(vec!["bob@example.com"]);
        assert_eq!(
            determine_attendee_email(&email, &event, "fallback@example.com"),
            "bob@example.com"
        );
    }

    #[test]
    fn determine_attendee_email_matches_cc() {
        let email = test_email_with_recipients(vec![], vec!["carol@example.com"]);
        let event = test_calendar_event(vec!["carol@example.com"]);
        assert_eq!(
            determine_attendee_email(&email, &event, "fallback@example.com"),
            "carol@example.com"
        );
    }

    #[test]
    fn determine_attendee_email_prefers_to_over_cc() {
        let email = test_email_with_recipients(vec!["bob@example.com"], vec!["carol@example.com"]);
        let event = test_calendar_event(vec!["bob@example.com", "carol@example.com"]);
        assert_eq!(
            determine_attendee_email(&email, &event, "fallback@example.com"),
            "bob@example.com"
        );
    }

    #[test]
    fn determine_attendee_email_case_insensitive() {
        let email = test_email_with_recipients(vec!["Bob@Example.COM"], vec![]);
        let event = test_calendar_event(vec!["bob@example.com"]);
        assert_eq!(
            determine_attendee_email(&email, &event, "fallback@example.com"),
            "Bob@Example.COM"
        );
    }

    #[test]
    fn determine_attendee_email_falls_back_to_username() {
        let email = test_email_with_recipients(vec!["unrelated@example.com"], vec![]);
        let event = test_calendar_event(vec!["someone@example.com"]);
        assert_eq!(
            determine_attendee_email(&email, &event, "fallback@example.com"),
            "fallback@example.com"
        );
    }

    #[test]
    fn determine_attendee_email_empty_recipients() {
        let email = test_email_with_recipients(vec![], vec![]);
        let event = test_calendar_event(vec!["someone@example.com"]);
        assert_eq!(
            determine_attendee_email(&email, &event, "user@fastmail.com"),
            "user@fastmail.com"
        );
    }

    // =========================================================================
    // RSVP state persistence static content tests
    // =========================================================================

    #[test]
    fn app_js_has_user_rsvp_status_preference() {
        assert!(
            APP_JS.contains("event.user_rsvp_status || getUserRsvpStatus(event)"),
            "renderCalendarCard should prefer server-authoritative user_rsvp_status"
        );
    }

    #[test]
    fn app_js_has_same_status_guard() {
        assert!(
            APP_JS.contains("event?.user_rsvp_status === status"),
            "rsvpToEvent should guard against duplicate same-status RSVPs"
        );
    }

    #[test]
    fn app_js_has_optimistic_user_rsvp_status() {
        assert!(
            APP_JS.contains("event.user_rsvp_status = status"),
            "rsvpToEvent should optimistically set user_rsvp_status"
        );
    }

    #[test]
    fn app_js_has_rsvp_status_label() {
        assert!(
            APP_JS.contains("rsvp-status-label"),
            "app.js should reference the RSVP status label element"
        );
        assert!(
            APP_JS.contains("You responded"),
            "app.js should show 'You responded' label text"
        );
    }

    #[test]
    fn app_js_has_rsvp_keyboard_shortcuts() {
        // y for accept, n for decline
        assert!(
            APP_JS.contains("rsvpToEvent('ACCEPTED')"),
            "app.js should have Accept RSVP shortcut"
        );
        assert!(
            APP_JS.contains("rsvpToEvent('DECLINED')"),
            "app.js should have Decline RSVP shortcut"
        );
    }

    #[test]
    fn app_js_has_rsvp_button_enter_passthrough() {
        assert!(
            APP_JS.contains("rsvp-btn"),
            "app.js should check for rsvp-btn class on focused element"
        );
    }

    #[test]
    fn index_html_has_rsvp_status_label() {
        assert!(
            INDEX_HTML.contains("rsvp-status-label"),
            "index.html should have the RSVP status label element"
        );
    }

    #[test]
    fn index_html_has_rsvp_keyboard_help() {
        assert!(
            INDEX_HTML.contains("RSVP Accept"),
            "help overlay should document y for RSVP Accept"
        );
        assert!(
            INDEX_HTML.contains("RSVP Decline"),
            "help overlay should document n for RSVP Decline"
        );
    }

    #[test]
    fn style_css_has_rsvp_status_label() {
        assert!(
            STYLE_CSS.contains(".rsvp-status-label"),
            "style.css should style the RSVP status label"
        );
    }

    #[test]
    fn style_css_email_links_underline_on_hover() {
        assert!(
            STYLE_CSS.contains("#email-body a:hover"),
            "style.css should have hover state for email links"
        );
        // Verify the hover rule contains underline
        let hover_pos = STYLE_CSS
            .find("#email-body a:hover")
            .expect("should have hover rule");
        let after_hover = &STYLE_CSS[hover_pos..];
        let block_end = after_hover.find('}').expect("should close rule");
        let hover_block = &after_hover[..block_end];
        assert!(
            hover_block.contains("text-decoration: underline"),
            "email links should underline on hover"
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
