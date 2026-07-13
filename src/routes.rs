use axum::{
    Router,
    body::Bytes,
    extract::{Json, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post, put},
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

use crate::error::Error;
use crate::types::*;
use crate::{accounts, calendar, provider, search, splits, theme, timezone};

pub(crate) const SPLIT_OVERFETCH_MULTIPLIER: usize = 10;

/// Inbox list size used by the UI's default account-switch fetch.
///
/// Kept `pub(crate)` so the prefetch warmer and the `is_cacheable` gate
/// in `list_emails` reference the same value — if these drift, the
/// warmer caches a 150-row list that the route handler then rejects
/// because `params.limit.unwrap_or(N) != N`, and every account switch
/// silently bypasses the cache.
pub(crate) const DEFAULT_INBOX_LIMIT: usize = 150;

const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/app.js");
const API_JS: &str = include_str!("../static/api.js");
const STYLE_CSS: &str = include_str!("../static/style.css");

const MOBILE_HTML: &str = include_str!("../static/mobile/index.html");
const MOBILE_APP_JS: &str = include_str!("../static/mobile/app.js");
const MOBILE_MANIFEST: &str = include_str!("../static/mobile/manifest.json");
const MOBILE_SW: &str = include_str!("../static/mobile/sw.js");
const FAVICON_32: &[u8] = include_bytes!("../static/favicon-32.png");
const ICON_180: &[u8] = include_bytes!("../static/icon-180.png");
const ICON_192: &[u8] = include_bytes!("../static/icon-192.png");
const ICON_512: &[u8] = include_bytes!("../static/icon-512.png");
const SUPERVILLAIN_JPG: &[u8] = include_bytes!("../static/supervillain.jpg");
const FONT_JBM_REGULAR: &[u8] = include_bytes!("../static/fonts/JetBrainsMono-Regular.woff2");
const FONT_JBM_SEMIBOLD: &[u8] = include_bytes!("../static/fonts/JetBrainsMono-SemiBold.woff2");
const FONT_JBM_BOLD: &[u8] = include_bytes!("../static/fonts/JetBrainsMono-Bold.woff2");

// =============================================================================
// Router
// =============================================================================

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .merge(accounts::router())
        .route("/api/accounts", get(list_accounts))
        .route("/api/identities", get(list_identities))
        .route("/api/theme", get(get_theme))
        .route("/api/mailboxes", get(list_mailboxes))
        .route("/api/emails", get(list_emails))
        .route("/api/upload", post(upload_blob))
        .route("/api/emails/send", post(send_email_handler))
        .route("/api/drafts", post(create_draft_handler))
        .route(
            "/api/drafts/{draft_id}",
            put(update_draft_handler).delete(delete_draft_handler),
        )
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
        .route("/api/timezone", get(get_timezone).put(put_timezone))
        .route("/api/timezone/accept-system", post(accept_system_timezone))
        .route(
            "/api/timezone/dismiss-change",
            post(dismiss_timezone_change),
        )
        .route("/api/timezone/zones", get(list_timezones))
        .route("/api/calendar/invite", post(send_invite_handler))
        .with_state(state)
        .route("/", get(index_html))
        .route("/index.html", get(index_html))
        .route("/app.js", get(app_js))
        .route("/api.js", get(api_js))
        .route("/style.css", get(style_css))
        .route("/fonts/JetBrainsMono-Regular.woff2", get(font_jbm_regular))
        .route(
            "/fonts/JetBrainsMono-SemiBold.woff2",
            get(font_jbm_semibold),
        )
        .route("/fonts/JetBrainsMono-Bold.woff2", get(font_jbm_bold))
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
        .route("/mobile/manifest.json", get(mobile_manifest))
        .route("/mobile/sw.js", get(mobile_sw))
        .route("/mobile/icon-180.png", get(icon_180))
        .route("/mobile/icon-192.png", get(icon_192))
        .route("/mobile/icon-512.png", get(icon_512))
}

// Restrictive CSP for the app shell: defense-in-depth so that any future
// innerHTML sink cannot evaluate inline script. Email HTML is rendered inside
// a sandboxed iframe (see static/app.js `renderHtmlBodyIframe`) which kills
// the only attacker-controlled-HTML path that could try to reach this origin.
//
// `style-src 'unsafe-inline'` is required because the codebase emits some
// inline style="..." attributes via innerHTML (status messages, etc); the
// security-critical directive is `script-src 'self'`. Per CSP3, srcdoc
// iframes inherit this policy from the parent rather than being matched
// against `frame-src` — `frame-src 'self'` is kept for any future non-srcdoc
// embeds, not because srcdoc needs it.
const APP_CSP: &str = "default-src 'self'; \
    script-src 'self'; \
    style-src 'self' 'unsafe-inline'; \
    img-src 'self' data: https: http:; \
    font-src 'self' data:; \
    connect-src 'self'; \
    frame-src 'self'; \
    object-src 'none'; \
    base-uri 'none'; \
    form-action 'self'";

fn html_headers() -> [(&'static str, &'static str); 2] {
    [
        ("content-type", "text/html; charset=utf-8"),
        ("content-security-policy", APP_CSP),
    ]
}

async fn index_html() -> impl IntoResponse {
    (html_headers(), INDEX_HTML)
}

async fn app_js() -> impl IntoResponse {
    (
        [("content-type", "application/javascript; charset=utf-8")],
        APP_JS,
    )
}

async fn api_js() -> impl IntoResponse {
    (
        [("content-type", "application/javascript; charset=utf-8")],
        API_JS,
    )
}

async fn style_css() -> impl IntoResponse {
    ([("content-type", "text/css; charset=utf-8")], STYLE_CSS)
}

async fn mobile_html() -> impl IntoResponse {
    (html_headers(), MOBILE_HTML)
}

async fn mobile_app_js() -> impl IntoResponse {
    (
        [("content-type", "application/javascript; charset=utf-8")],
        MOBILE_APP_JS,
    )
}

async fn mobile_manifest() -> impl IntoResponse {
    (
        [("content-type", "application/manifest+json; charset=utf-8")],
        MOBILE_MANIFEST,
    )
}

async fn mobile_sw() -> impl IntoResponse {
    // Version-bust the cache name at serve time so a new BUILD always
    // gets a fresh CACHE_NAME without a manual bump in the source file.
    // CARGO_PKG_VERSION alone isn't enough: deploys happen per-commit via
    // scripts/upgrade.sh and the crate version rarely changes, so
    // consecutive deploys would otherwise share one cache. SUPERVILLAIN_BUILD_ID
    // (set by build.rs from the git short sha) makes every build distinct.
    // `Cache-Control: no-cache` on sw.js itself matters more than usual:
    // browsers only re-check a service worker file at most once every 24h
    // by spec, and a stale cached copy would keep serving the old
    // (unreplaced) placeholder, defeating version-busting entirely.
    let body = MOBILE_SW.replace(
        "__SUPERVILLAIN_VERSION__",
        concat!(
            env!("CARGO_PKG_VERSION"),
            "-",
            env!("SUPERVILLAIN_BUILD_ID")
        ),
    );
    (
        [
            ("content-type", "application/javascript; charset=utf-8"),
            ("service-worker-allowed", "/mobile/"),
            ("cache-control", "no-cache"),
        ],
        body,
    )
}

async fn font_jbm_regular() -> impl IntoResponse {
    ([("content-type", "font/woff2")], FONT_JBM_REGULAR)
}

async fn font_jbm_semibold() -> impl IntoResponse {
    ([("content-type", "font/woff2")], FONT_JBM_SEMIBOLD)
}

async fn font_jbm_bold() -> impl IntoResponse {
    ([("content-type", "font/woff2")], FONT_JBM_BOLD)
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
    starred: Option<bool>,
    /// List sort order. Absent means "use the default" (newest-first,
    /// today's behavior); an unrecognized value is rejected at
    /// deserialization (400), never silently coerced to the default —
    /// see `EmailSort`'s doc comment (kata 09ef).
    sort: Option<EmailSort>,
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

/// Body for the persistent-draft routes (kata wm57). Same field style as
/// `/emails/send` minus attachments/bcc/html: v1 drafts are plain-text only.
#[derive(Deserialize)]
struct DraftBody {
    #[serde(default)]
    to: Vec<String>,
    #[serde(default)]
    cc: Vec<String>,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    body: String,
    in_reply_to: Option<String>,
    from_address: Option<String>,
}

#[derive(Deserialize, Default)]
struct AccountParam {
    account: Option<String>,
}

/// Params for `GET /api/emails/{id}`. Like `AccountParam` plus an opt-out
/// from the auto-mark-read behavior — mobile's adjacent-email prefetch
/// warms the body cache without the user ever opening the email, so it
/// must not silently consume that email's unread state.
#[derive(Deserialize, Default)]
struct GetEmailParams {
    account: Option<String>,
    mark_read: Option<bool>,
}

// =============================================================================
// Account resolution
// =============================================================================

/// Uniform unknown-account rejection for every path that accepts a
/// client-supplied account id. One message format, one place to change
/// validation (e.g. future case normalization).
fn ensure_known_account(reg: &AccountRegistry, id: &str) -> Result<(), Error> {
    if !reg.account_configs.contains_key(id) {
        return Err(Error::BadRequest(format!("Unknown account '{id}'")));
    }
    Ok(())
}

async fn resolve_session(state: &AppState, account: Option<&str>) -> Result<SessionLock, Error> {
    let reg = state.accounts.read().await;
    let key = account.unwrap_or(&reg.default_account);
    reg.sessions
        .get(key)
        .cloned()
        .ok_or_else(|| Error::BadRequest(format!("Unknown account '{key}'")))
}

/// Resolve just the account ID (default if None), without requiring the
/// session to exist. Used by cache-aware handlers so a cached response can
/// be served before doing any session lookup.
async fn resolve_account_id(state: &AppState, account: Option<&str>) -> Result<String, Error> {
    let reg = state.accounts.read().await;
    let id = match account {
        Some(a) => a.to_string(),
        None => reg.default_account.clone(),
    };
    if id.is_empty() {
        return Err(Error::BadRequest("No account specified".into()));
    }
    ensure_known_account(&reg, &id)?;
    Ok(id)
}

// =============================================================================
// Handlers
// =============================================================================

async fn list_accounts(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Re-read the config from disk before taking the registry lock: the read
    // is async (no blocking the runtime) and the lock isn't held across IO.
    // Missing/unreadable file parses as empty, same as parse_config.
    let disk_content = tokio::fs::read_to_string(&state.config_path)
        .await
        .unwrap_or_default();
    let (disk, disk_parse_errors) = crate::accounts::parse_config_str(&disk_content);

    let reg = state.accounts.read().await;
    let mut live = std::collections::HashMap::new();
    for (name, session_lock) in &reg.sessions {
        let session = session_lock.read().await;
        live.insert(
            name.clone(),
            (
                session.username().to_string(),
                session.provider_name().to_string(),
            ),
        );
    }
    let accounts =
        crate::accounts::wire_account_list(&reg.account_configs, &live, &reg.default_account);

    let mut errors = state.account_errors.read().await.clone();
    // Hand-edits made after startup never take effect (config is loaded once
    // in main); tell the user instead of letting the edit silently rot.
    let baseline = state
        .config_error_baseline
        .read()
        .expect("config_error_baseline lock poisoned")
        .clone();
    if let Some(banner) = crate::accounts::stale_config_banner(
        &state.config_path,
        &disk,
        &disk_parse_errors,
        &baseline,
        &reg.account_configs,
    ) {
        errors.push(banner);
    }
    Json(serde_json::json!({
        "accounts": accounts,
        "errors": errors,
    }))
}

async fn list_identities(
    State(state): State<Arc<AppState>>,
    Query(params): Query<AccountParam>,
) -> Result<impl IntoResponse, Error> {
    let id = resolve_account_id(&state, params.account.as_deref()).await?;
    let identities = state
        .prefetch
        .identities_or_fetch(&id, || async {
            let session_lock = resolve_session(&state, Some(&id)).await?;
            let mut session = session_lock.write().await;
            provider::get_identities(&mut session).await
        })
        .await?;
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
    let id = resolve_account_id(&state, params.account.as_deref()).await?;
    let mailboxes = state
        .prefetch
        .mailboxes_or_fetch(&id, || async {
            let session_lock = resolve_session(&state, Some(&id)).await?;
            let session = session_lock.read().await;
            provider::get_mailboxes(&session).await
        })
        .await?;
    Ok(Json(serde_json::json!(mailboxes)))
}

/// Whether a `list_emails` request is eligible for the prefetch cache.
///
/// Default-inbox shape (mailbox_id set, no split, no search, no starred,
/// default offset/limit, **and default sort**) goes through the prefetch
/// cache. Anything else always fetches live — no cache read, no cache
/// write:
///
/// - mailbox/split/search/starred/offset/limit: cache key would explode,
///   and the data is per-query anyway.
/// - sort: the background warmer only ever re-warms the `DateDesc` slot
///   (see `prefetch::warm_all_mailboxes`), and there's no TTL on cache
///   entries. If a non-default sort were cacheable, a user sitting in
///   "Oldest first" would read from a slot the warmer never refreshes and
///   would never see new mail until some unrelated local mutation
///   invalidated the whole account's cache (roborev 291). Simplest fix:
///   non-default sorts just aren't cacheable, full stop.
fn list_is_cacheable(params: &ListEmailsParams, offset: usize, sort: EmailSort) -> bool {
    params.mailbox_id.is_some()
        && params.split_id.is_none()
        && params.search.is_none()
        && params.starred != Some(true)
        && offset == 0
        && params.limit.unwrap_or(DEFAULT_INBOX_LIMIT) == DEFAULT_INBOX_LIMIT
        && sort == EmailSort::default()
}

async fn list_emails(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListEmailsParams>,
) -> Result<impl IntoResponse, Error> {
    let limit = params.limit.unwrap_or(DEFAULT_INBOX_LIMIT);
    let offset = params.offset.unwrap_or(0);
    let sort = params.sort.unwrap_or_default();

    let mut query = params.search.as_deref().map(search::parse_query);
    // The sidebar Starred toggle takes precedence: when ?starred=true is
    // set we always restrict to flagged mail, regardless of any is_flagged
    // value parsed from the search string.
    if params.starred == Some(true) {
        query.get_or_insert_with(Default::default).is_flagged = Some(true);
    }
    let query_ref = query.as_ref();

    // Resolved once and reused for both the fetch (cached or live) and the
    // split-filter block below — a second resolve_account_id call can't
    // disagree with this one since both apply the same default-account
    // fallback, but there's no reason to pay for the lock twice.
    let account_id = resolve_account_id(&state, params.account.as_deref()).await?;

    // Split-filtered requests need the scoped config before the fetch: an
    // id that matches neither "primary" nor a split in scope (a deleted
    // split, a stale client tab) can bail out here without spending a
    // provider round-trip on mail we'd throw away below.
    let split_config = params.split_id.is_some().then(|| {
        splits::load_splits(
            &state.splits_config_path,
            std::env::var("SUPERVILLAIN_SPLITS").ok().as_deref(),
        )
        .scoped_to(Some(&account_id))
    });

    if let Some(split_id) = params.split_id.as_deref()
        && split_id != "primary"
        && let Some(config) = split_config.as_ref()
        && !config.splits.iter().any(|s| s.id == split_id)
    {
        return Ok((HeaderMap::new(), Json(Vec::<serde_json::Value>::new())));
    }

    let fetch_limit = if params.split_id.is_some() {
        limit * SPLIT_OVERFETCH_MULTIPLIER
    } else {
        limit
    };

    // See `list_is_cacheable`'s doc comment for the full rationale,
    // including why non-default sorts are excluded (roborev 291).
    let is_cacheable = list_is_cacheable(&params, offset, sort);

    // Both live paths below release the session read guard between the id
    // query and each get_emails chunk (provider::get_emails_chunked) so a
    // queued writer — most visibly a send — isn't stuck behind the whole
    // fan-out.
    let (mut emails, stale) = if is_cacheable {
        // `is_cacheable` guarantees `sort == EmailSort::default()` here, so
        // this key's `sort` is always `DateDesc` — the field still joins
        // the key (rather than being dropped) so the cache stays correct
        // by construction if that gating ever loosens. See `InboxKey`'s
        // doc comment.
        let key = crate::prefetch::InboxKey {
            mailbox_id: params.mailbox_id.clone().unwrap(),
            limit,
            sort,
        };
        state
            .prefetch
            .inbox_list_or_fetch(&account_id, key, || async {
                let session_lock = resolve_session(&state, Some(&account_id)).await?;
                let email_ids = {
                    let session = session_lock.read().await;
                    provider::query_emails(
                        &session,
                        params.mailbox_id.as_deref(),
                        fetch_limit,
                        offset,
                        query_ref,
                        sort,
                    )
                    .await?
                };
                provider::get_emails_chunked(
                    &session_lock,
                    &email_ids,
                    false,
                    None,
                    provider::GET_EMAILS_CHUNK,
                )
                .await
            })
            .await?
    } else {
        let session_lock = resolve_session(&state, Some(&account_id)).await?;
        let email_ids = {
            let session = session_lock.read().await;
            provider::query_emails(
                &session,
                params.mailbox_id.as_deref(),
                fetch_limit,
                offset,
                query_ref,
                sort,
            )
            .await?
        };
        let live = provider::get_emails_chunked(
            &session_lock,
            &email_ids,
            false,
            None,
            provider::GET_EMAILS_CHUNK,
        )
        .await?;
        (live, false)
    };

    // Apply split filtering, scoped to this account's splits so "primary"
    // means "not matching any of *this account's* splits". Reuses the
    // config loaded above the fetch — no second load/scope pass.
    if let (Some(split_id), Some(config)) = (params.split_id.as_deref(), split_config.as_ref()) {
        emails = splits::filter_by_split(emails, split_id, config);
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

    // A stale response is a disk-restored snapshot from the previous run,
    // served for instant first paint. The header tells the frontend to keep
    // re-polling (each poll is a cheap cache read) until the warmer has
    // replaced the entry with live data — see loadEmails in app.js.
    let mut headers = HeaderMap::new();
    if stale {
        headers.insert(
            "x-supervillain-stale",
            axum::http::HeaderValue::from_static("1"),
        );
    }
    Ok((headers, Json(response)))
}

async fn get_email(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
    Query(params): Query<GetEmailParams>,
) -> Result<impl IntoResponse, Error> {
    let account_key = match params.account.clone() {
        Some(a) => a,
        None => state.accounts.read().await.default_account.clone(),
    };
    let session_lock = resolve_session(&state, Some(&account_key)).await?;
    let session = session_lock.read().await;

    // The reserved interactive lane is only for fetches the user is actively
    // waiting on (see provider::get_emails). mark_read=false is the prefetch
    // signature — both bundles' adjacent-email warmers set it — and those
    // fire-and-forget warm-ups must not queue full bodies ahead of a
    // genuinely user-blocking open (roborev 315).
    let priority = params.mark_read.unwrap_or(true);
    let email = state
        .prefetch
        .body_or_fetch(&account_key, &email_id, || async {
            // priority: when set, the user is staring at a spinner for exactly
            // this response — it must not queue behind a warm pass's fan-out.
            let emails = provider::get_emails(
                &session,
                std::slice::from_ref(&email_id),
                true,
                None,
                priority,
            )
            .await?;
            emails
                .into_iter()
                .next()
                .ok_or_else(|| Error::NotFound("Email not found".into()))
        })
        .await?;
    let email = &email;

    // Auto mark-read (skippable via ?mark_read=false — see GetEmailParams)
    if params.mark_read.unwrap_or(true) && email.is_unread() {
        let _ = provider::mark_read(&session, &email_id).await;
    }

    // Check for calendar event
    let mut calendar_event = None;
    if email.has_calendar
        && let Ok(Some(ics_data)) = provider::get_calendar_data(&session, &email_id).await
        && let Some(mut event) = calendar::parse_ics(&ics_data)
    {
        // Fetch the stored calendar event once — reused for both the SEQUENCE
        // update decision (REQUEST) and the PARTSTAT merge below. None when the
        // event isn't in the calendar yet or the lookup failed (degrade to the
        // first-time add path / email ICS).
        let stored_event = match provider::get_calendar_event(&session, &event.uid).await {
            Ok(opt) => opt,
            Err(e) => {
                tracing::warn!(
                    "Calendar fetch failed for {}, falling back to email ICS: {e}",
                    event.uid
                );
                None
            }
        };

        // On an Update (rescheduled invite from the verified organizer) we must
        // NOT re-apply the stale stored PARTSTAT — the response is being reset.
        let mut skip_partstat_merge = false;

        if event.method == "REQUEST" {
            // Anti-spoof input: this trusts the From header as delivered (i.e.
            // whatever survived the provider's DMARC/SPF filtering) — it is not
            // cryptographic verification of the organizer.
            let sender_email = email.from.first().map(|a| a.email.as_str());
            let decision = calendar::invite_update_decision(
                stored_event.as_ref().map(|e| e.sequence),
                event.sequence,
                stored_event.as_ref().map(|e| e.organizer_email.as_str()),
                sender_email,
            );
            // Content-idempotence guard (roborev 292, scoped in roborev 295):
            // Outlook's parse_graph_event always reports sequence: 0 (Graph has
            // no SEQUENCE field), so any invite with ICS SEQUENCE >= 1 hits this
            // Update arm on *every* re-open — and remove+re-add wipes the user's
            // stored responseStatus each time. A Gmail event stored before
            // SEQUENCE round-tripping was added has the same issue as a
            // one-time artifact. If the stored event's user-visible fields
            // already match the incoming ICS, nothing actually changed —
            // downgrade to Unchanged so we don't fire the destructive rewrite.
            //
            // Scope this to the sequence-blind cases only: stored.sequence == 0.
            // Once a provider round-trips SEQUENCE faithfully (stored.sequence
            // > 0), the SEQUENCE comparison in invite_update_decision is
            // trustworthy on its own — skip the content-match guard entirely so
            // a real reschedule can't be masked by a content check that doesn't
            // track every field. A real reschedule (content differs) still
            // Updates in the sequence-blind case too.
            let decision = if decision == calendar::InviteAction::Update
                && stored_event.as_ref().is_some_and(|stored| {
                    stored.sequence == 0 && calendar::events_content_match(stored, &event)
                }) {
                calendar::InviteAction::Unchanged
            } else {
                decision
            };
            match decision {
                // First-time add or idempotent re-receipt: today's behavior —
                // add if missing, never overwrite (only_if_new = true).
                calendar::InviteAction::NoStored | calendar::InviteAction::Unchanged => {
                    let state_clone = state.clone();
                    let ics_clone = ics_data.clone();
                    let uid = event.uid.clone();
                    let acct = account_key.clone();
                    tokio::spawn(async move {
                        if let Ok(s_lock) = resolve_session(&state_clone, Some(&acct)).await {
                            let s = s_lock.read().await;
                            if let Err(e) =
                                provider::add_to_calendar(&s, &ics_clone, &uid, true).await
                            {
                                tracing::warn!("Calendar auto-add failed for {uid}: {e}");
                            }
                        }
                    });
                }
                // Rescheduled invite (higher SEQUENCE, organizer verified):
                // overwrite the stored event (only_if_new = false) and reset the
                // user's now-stale RSVP.
                calendar::InviteAction::Update => {
                    event.is_update = true;
                    skip_partstat_merge = true;
                    let state_clone = state.clone();
                    let ics_clone = ics_data.clone();
                    let uid = event.uid.clone();
                    let acct = account_key.clone();
                    tokio::spawn(async move {
                        if let Ok(s_lock) = resolve_session(&state_clone, Some(&acct)).await {
                            let s = s_lock.read().await;
                            if let Err(e) =
                                provider::add_to_calendar(&s, &ics_clone, &uid, false).await
                            {
                                tracing::warn!("Calendar update failed for {uid}: {e}");
                            }
                        }
                    });
                }
                // Higher SEQUENCE but the sender is not the stored organizer.
                // Touch nothing: no calendar write, no status reset. Render the
                // incoming ICS as-is (no banner).
                calendar::InviteAction::RejectSpoof => {
                    tracing::warn!(
                        "Rejected spoofed calendar update for {} (sender {:?} != organizer {:?})",
                        event.uid,
                        sender_email,
                        stored_event.as_ref().map(|e| e.organizer_email.as_str()),
                    );
                }
            }
        } else if event.method == "CANCEL" {
            // Anti-spoof gate (roborev 292): the CANCEL arm has no SEQUENCE to
            // compare, so without this check anyone who learns a UID could
            // delete a stored event by mailing a spoofed METHOD:CANCEL ICS.
            // Only remove when the sender matches the stored event's
            // organizer — the same check used for REQUEST updates above. The
            // cancelled banner still renders regardless (display isn't the
            // attack surface; the calendar write is).
            let sender_email = email.from.first().map(|a| a.email.as_str());
            let stored_organizer_email = stored_event.as_ref().map(|e| e.organizer_email.as_str());
            match calendar::cancel_decision(stored_organizer_email, sender_email) {
                calendar::CancelAction::Remove => {
                    let state_clone = state.clone();
                    let uid = event.uid.clone();
                    let acct = account_key.clone();
                    tokio::spawn(async move {
                        if let Ok(s_lock) = resolve_session(&state_clone, Some(&acct)).await {
                            let s = s_lock.read().await;
                            if let Err(e) = provider::remove_from_calendar(&s, &uid).await {
                                tracing::warn!("Calendar auto-remove failed for {uid}: {e}");
                            }
                        }
                    });
                }
                calendar::CancelAction::NoStored => {
                    tracing::debug!("Event {} not in calendar yet, nothing to cancel", event.uid);
                }
                calendar::CancelAction::RejectSpoof => {
                    tracing::warn!(
                        "Rejected spoofed calendar cancellation for {} (sender {:?} != organizer {:?})",
                        event.uid,
                        sender_email,
                        stored_organizer_email,
                    );
                }
            }
        }

        // Merge current PARTSTAT from the stored calendar event so the UI
        // reflects the user's actual RSVP status, not the stale email ICS.
        // Skipped on Update — a reschedule resets the user's response.
        if !skip_partstat_merge {
            if let Some(cal_event) = &stored_event {
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
            } else {
                tracing::debug!("Event {} not in calendar yet, using email ICS", event.uid);
            }
        }

        // Set user_rsvp_status from the (now-merged) attendee list — but not on
        // an Update, where the response was intentionally reset to None.
        if event.method == "REQUEST" && !event.is_update {
            let attendee_email = determine_attendee_email(email, &event, session.username());
            if let Some(att) = event
                .attendees
                .iter()
                .find(|a| a.email.eq_ignore_ascii_case(&attendee_email))
            {
                event.user_rsvp_status = Some(att.status.clone());
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
        // Threading parent — lets a restored draft rehydrate its reply
        // context so subsequent saves/sends keep in_reply_to (kata wm57).
        "inReplyTo": email.in_reply_to,
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

    let session_lock = resolve_session(&state, params.account.as_deref()).await?;
    let session = session_lock.read().await;

    let (content_type, bytes) = provider::download_blob(&session, &blob_id, &filename).await?;

    let safe_filename = sanitize_filename_for_header(&filename);
    // X-Content-Type-Options: nosniff prevents browsers from sniffing past the
    // declared Content-Type. Combined with Content-Disposition: attachment,
    // this neutralizes the sender-controlled-filename-→-mime-type attack
    // surface (a sender mailing `pwned.html` doesn't get HTML rendered from
    // our origin if the user clicks "open" rather than "save").
    Ok((
        StatusCode::OK,
        [
            ("content-type", content_type),
            (
                "content-disposition",
                format!("attachment; filename=\"{}\"", safe_filename),
            ),
            ("x-content-type-options", "nosniff".to_string()),
        ],
        bytes,
    ))
}

async fn archive_email(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
    Query(params): Query<AccountParam>,
) -> Result<impl IntoResponse, Error> {
    let id = resolve_account_id(&state, params.account.as_deref()).await?;
    let session_lock = resolve_session(&state, Some(&id)).await?;
    let session = session_lock.read().await;
    let success = provider::archive(&session, &email_id).await?;
    drop(session);
    state.prefetch.invalidate(&id).await;
    Ok(Json(serde_json::json!({"success": success})))
}

async fn trash_email(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
    Query(params): Query<AccountParam>,
) -> Result<impl IntoResponse, Error> {
    let id = resolve_account_id(&state, params.account.as_deref()).await?;
    let session_lock = resolve_session(&state, Some(&id)).await?;
    let session = session_lock.read().await;
    let success = provider::trash(&session, &email_id).await?;
    drop(session);
    state.prefetch.invalidate(&id).await;
    Ok(Json(serde_json::json!({"success": success})))
}

async fn mark_read(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
    Query(params): Query<AccountParam>,
) -> Result<impl IntoResponse, Error> {
    let id = resolve_account_id(&state, params.account.as_deref()).await?;
    let session_lock = resolve_session(&state, Some(&id)).await?;
    let session = session_lock.read().await;
    let success = provider::mark_read(&session, &email_id).await?;
    drop(session);
    state.prefetch.invalidate(&id).await;
    Ok(Json(serde_json::json!({"success": success})))
}

async fn mark_unread(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
    Query(params): Query<AccountParam>,
) -> Result<impl IntoResponse, Error> {
    let id = resolve_account_id(&state, params.account.as_deref()).await?;
    let session_lock = resolve_session(&state, Some(&id)).await?;
    let session = session_lock.read().await;
    let success = provider::mark_unread(&session, &email_id).await?;
    drop(session);
    state.prefetch.invalidate(&id).await;
    Ok(Json(serde_json::json!({"success": success})))
}

async fn toggle_flag(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
    Query(params): Query<AccountParam>,
) -> Result<impl IntoResponse, Error> {
    let id = resolve_account_id(&state, params.account.as_deref()).await?;
    let session_lock = resolve_session(&state, Some(&id)).await?;
    let session = session_lock.read().await;
    let success = provider::toggle_flag(&session, &email_id).await?;
    drop(session);
    state.prefetch.invalidate(&id).await;
    Ok(Json(serde_json::json!({"success": success})))
}

async fn move_email(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
    Query(params): Query<AccountParam>,
    Json(body): Json<MoveBody>,
) -> Result<impl IntoResponse, Error> {
    let id = resolve_account_id(&state, params.account.as_deref()).await?;
    let session_lock = resolve_session(&state, Some(&id)).await?;
    let session = session_lock.read().await;
    let success = provider::move_to_mailbox(&session, &email_id, &body.mailbox_id).await?;
    drop(session);
    state.prefetch.invalidate(&id).await;
    Ok(Json(serde_json::json!({"success": success})))
}

// Defense in depth for outbound HTML: scrubs scripts, event handlers,
// dangerous URL schemes (javascript:/vbscript:/non-image data:), and other
// well-known XSS vectors before the message hits the wire. The iframe sandbox
// protects *our* viewer, but reply/forward bodies carry the original sender's
// HTML out to recipients whose clients may render it unsafely. Ammonia's
// defaults are a vetted allowlist sanitizer; this prevents us from being a
// laundering vector for an attacker's payload across the address book.
fn sanitize_outgoing_html(html: &str) -> String {
    ammonia::clean(html)
}

async fn send_email_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<AccountParam>,
    Json(body): Json<SendEmailBody>,
) -> Result<impl IntoResponse, Error> {
    let session_lock = resolve_session(&state, params.account.as_deref()).await?;
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
        html_body: body.html_body.map(|h| sanitize_outgoing_html(&h)),
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

// --- Persistent drafts (kata wm57) -----------------------------------------

/// Build a plain-text `EmailSubmission` from a draft body. v1 persists no
/// html_body, attachments, or calendar — those live only in the live compose
/// session, never in the stored draft.
fn draft_submission(body: DraftBody) -> EmailSubmission {
    EmailSubmission {
        to: body.to,
        cc: body.cc,
        subject: body.subject,
        text_body: body.body,
        bcc: None,
        html_body: None,
        in_reply_to: body.in_reply_to,
        references: None,
        attachments: Vec::new(),
        calendar_ics: None,
    }
}

async fn create_draft_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<AccountParam>,
    Json(body): Json<DraftBody>,
) -> Result<impl IntoResponse, Error> {
    let id = resolve_account_id(&state, params.account.as_deref()).await?;
    let session_lock = resolve_session(&state, Some(&id)).await?;
    let session = session_lock.read().await;
    let from_addr = body
        .from_address
        .clone()
        .unwrap_or_else(|| session.username().to_string());
    let submission = draft_submission(body);
    let draft_id = provider::create_draft(&session, &submission, &from_addr).await?;
    drop(session);
    // A newly created draft belongs in a warmed Drafts-list cache too — without
    // this a prefetched list can hide it for up to 5 minutes (review follow-up).
    state.prefetch.invalidate(&id).await;
    Ok(Json(serde_json::json!({ "id": draft_id })))
}

async fn update_draft_handler(
    State(state): State<Arc<AppState>>,
    Path(draft_id): Path<String>,
    Query(params): Query<AccountParam>,
    Json(body): Json<DraftBody>,
) -> Result<impl IntoResponse, Error> {
    let id = resolve_account_id(&state, params.account.as_deref()).await?;
    let session_lock = resolve_session(&state, Some(&id)).await?;
    let session = session_lock.read().await;
    let from_addr = body
        .from_address
        .clone()
        .unwrap_or_else(|| session.username().to_string());
    let submission = draft_submission(body);
    // Editing a draft is destroy+recreate (JMAP bodies aren't patchable), so
    // this returns a NEW id the client must adopt.
    let new_id = provider::update_draft(&session, &draft_id, &submission, &from_addr).await?;
    drop(session);
    // The destroy+recreate rotates the draft's id — a warmed Drafts list must
    // be invalidated or it keeps serving the now-destroyed old id (review
    // follow-up; same gap as create/delete below).
    state.prefetch.invalidate(&id).await;
    Ok(Json(serde_json::json!({ "id": new_id })))
}

async fn delete_draft_handler(
    State(state): State<Arc<AppState>>,
    Path(draft_id): Path<String>,
    Query(params): Query<AccountParam>,
) -> Result<impl IntoResponse, Error> {
    let id = resolve_account_id(&state, params.account.as_deref()).await?;
    let session_lock = resolve_session(&state, Some(&id)).await?;
    let session = session_lock.read().await;
    let success = provider::destroy_draft(&session, &draft_id).await?;
    drop(session);
    // Without this a warmed Drafts list keeps serving the destroyed id for up
    // to 5 minutes (review follow-up), same as every other mutation route.
    state.prefetch.invalidate(&id).await;
    Ok(Json(serde_json::json!({ "success": success })))
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

    let session_lock = resolve_session(&state, params.account.as_deref()).await?;
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
    let session_lock = resolve_session(&state, params.account.as_deref()).await?;
    let mut session_guard = session_lock.write().await;

    // Get calendar data
    let ics_data = provider::get_calendar_data(&session_guard, &email_id)
        .await?
        .ok_or_else(|| Error::NotFound("No calendar data found".into()))?;

    let event = calendar::parse_ics(&ics_data)
        .ok_or_else(|| Error::Internal("Failed to parse calendar data".into()))?;

    // Determine attendee email (use account username as fallback)
    let attendee_email = {
        let emails = provider::get_emails(
            &session_guard,
            std::slice::from_ref(&email_id),
            false,
            None,
            true, // user-blocking: RSVP click
        )
        .await?;
        let email = emails
            .first()
            .ok_or_else(|| Error::NotFound("Email not found".into()))?;
        determine_attendee_email(email, &event, session_guard.username())
    };

    let reply_tz = timezone::primary_tz(&timezone::load_config(
        &state.timezone_config_path,
        timezone_env_override().as_deref(),
    ));

    // Dispatch full RSVP flow to provider (Fastmail: iTIP email + CalDAV, Outlook: Graph API)
    provider::rsvp(
        &mut session_guard,
        &ics_data,
        &event,
        &attendee_email,
        &body.status,
        reply_tz,
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
    updated_event.user_rsvp_status = Some(body.status.as_ics_str().to_string());
    Ok(Json(serde_json::json!({ "calendarEvent": updated_event })))
}

async fn add_to_calendar(
    State(state): State<Arc<AppState>>,
    Path(email_id): Path<String>,
    Query(params): Query<AccountParam>,
) -> Result<impl IntoResponse, Error> {
    let session_lock = resolve_session(&state, params.account.as_deref()).await?;
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
    let id = resolve_account_id(&state, params.account.as_deref()).await?;
    let session_lock = resolve_session(&state, Some(&id)).await?;
    let session = session_lock.read().await;

    // Get the email to find the sender
    let emails = provider::get_emails(
        &session,
        std::slice::from_ref(&email_id),
        true,
        None,
        true, // user-blocking: unsubscribe click
    )
    .await?;
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
    // Order doesn't matter here — every match gets archived regardless of
    // the sequence they're fetched in — so the default is fine.
    let all_ids =
        provider::query_emails(&session, None, 500, 0, Some(&query), EmailSort::default()).await?;

    // Archive all
    let archived = provider::archive_batch(&session, &all_ids).await?;
    drop(session);
    state.prefetch.invalidate(&id).await;

    Ok(Json(serde_json::json!({
        "success": true,
        "archived": archived,
        "sender": sender_email
    })))
}

// =============================================================================
// Splits CRUD
//
// Definitions live in the single ~/.config/supervillain/splits.json, but
// each split may be tagged with an owning account. Reads (`list_splits`)
// scope to ?account=; writes validate the tag against the registry.
// `/api/split-counts` and `/api/emails?split_id=` scope to the resolved
// account before counting/filtering.
// =============================================================================

/// splits.json is an input to the per-account split-counts cache; any
/// write must leave no window where disk and cache disagree. Splits
/// writes are rare, so invalidating every account's entry is fine.
async fn invalidate_all_split_caches(state: &AppState) {
    let ids: Vec<String> = {
        let reg = state.accounts.read().await;
        reg.account_configs.keys().cloned().collect()
    };
    for id in &ids {
        state.prefetch.invalidate_split_counts(id).await;
    }
}

#[derive(Deserialize)]
struct SplitCountsParams {
    mailbox_id: String,
    account: Option<String>,
    starred: Option<bool>,
}

async fn split_counts(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SplitCountsParams>,
) -> Result<impl IntoResponse, Error> {
    let start = std::time::Instant::now();

    let account_id = resolve_account_id(&state, params.account.as_deref()).await?;
    let config = splits::load_splits(
        &state.splits_config_path,
        std::env::var("SUPERVILLAIN_SPLITS").ok().as_deref(),
    )
    .scoped_to(Some(&account_id));
    if config.splits.is_empty() {
        return Ok(Json(serde_json::json!({})));
    }

    // Default-view counts (no starred filter) go through the prefetch
    // cache; the starred-filter variant is per-session ephemeral and
    // bypasses the cache so it doesn't pollute the steady-state entry.
    let is_cacheable = params.starred != Some(true);

    let counts: HashMap<String, u32> = if is_cacheable {
        let id = account_id.clone();
        let mbox_for_key = params.mailbox_id.clone();
        let mbox_for_fetch = params.mailbox_id.clone();
        let cfg = config.clone();
        let state_for_fetch = state.clone();
        let acct_for_fetch = params.account.clone();
        state
            .prefetch
            .split_counts_or_fetch(&id, &mbox_for_key, || async move {
                compute_split_counts(
                    &state_for_fetch,
                    acct_for_fetch.as_deref(),
                    &mbox_for_fetch,
                    &cfg,
                    None,
                )
                .await
            })
            .await?
    } else {
        let query = crate::types::ParsedQuery {
            is_flagged: Some(true),
            ..Default::default()
        };
        compute_split_counts(
            &state,
            params.account.as_deref(),
            &params.mailbox_id,
            &config,
            Some(query),
        )
        .await?
    };

    tracing::debug!(
        "split-counts: {} splits, {:.0}ms",
        counts.len(),
        start.elapsed().as_millis()
    );

    Ok(Json(serde_json::json!(counts)))
}

/// Shared splits-counting implementation used by the `/api/split-counts`
/// handler *and* the prefetch warmer. Drift between the two would mean
/// the cached value the warmer wrote was computed from a different
/// sample size than the route would have produced on a miss, so the
/// user-facing count would flip every time the cache invalidated. One
/// function, one constant.
pub(crate) async fn compute_split_counts(
    state: &AppState,
    account: Option<&str>,
    mailbox_id: &str,
    config: &SplitsConfig,
    query: Option<crate::types::ParsedQuery>,
) -> Result<HashMap<String, u32>, Error> {
    let session_lock = resolve_session(state, account).await?;

    let fetch_limit = DEFAULT_INBOX_LIMIT * SPLIT_OVERFETCH_MULTIPLIER;
    // Split counts are order-independent (just counting matches), so the
    // default sort is fine here regardless of the user's list sort choice.
    let email_ids = {
        let session = session_lock.read().await;
        provider::query_emails(
            &session,
            Some(mailbox_id),
            fetch_limit,
            0,
            query.as_ref(),
            EmailSort::default(),
        )
        .await?
    };

    // This is the single longest provider fan-out in the app (~1500 gets,
    // minutes on a rate-limited Gmail account), so releasing the session
    // guard between chunks matters most here: a send queued behind one
    // monolithic guard used to stall until the whole sample finished.
    let minimal_props: &[&str] = &["id", "from", "to", "cc", "subject"];
    let all_emails = provider::get_emails_chunked(
        &session_lock,
        &email_ids,
        false,
        Some(minimal_props),
        provider::GET_EMAILS_CHUNK,
    )
    .await?;

    let mut counts = HashMap::new();
    for split in &config.splits {
        let count = all_emails
            .iter()
            .filter(|e| splits::matches_split(e, split))
            .count();
        counts.insert(split.id.clone(), count as u32);
    }
    Ok(counts)
}

#[derive(Deserialize)]
struct ListSplitsParams {
    account: Option<String>,
}

async fn list_splits(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListSplitsParams>,
) -> Result<impl IntoResponse, Error> {
    // No ?account= → full list (management/debugging view). The UI always
    // sends the active account via the api() helper. When an account IS
    // given, validate it — a typo would otherwise silently compute against
    // the untagged-only split list instead of 400ing.
    if let Some(ref acct) = params.account {
        let reg = state.accounts.read().await;
        ensure_known_account(&reg, acct)?;
    }

    let config = splits::load_splits(
        &state.splits_config_path,
        std::env::var("SUPERVILLAIN_SPLITS").ok().as_deref(),
    );
    Ok(Json(serde_json::json!(
        config.scoped_to(params.account.as_deref()).splits
    )))
}

async fn create_split(
    State(state): State<Arc<AppState>>,
    Json(new_split): Json<SplitInbox>,
) -> Result<impl IntoResponse, Error> {
    let mut config = splits::load_splits(
        &state.splits_config_path,
        std::env::var("SUPERVILLAIN_SPLITS").ok().as_deref(),
    );

    // Check for duplicate ID
    if config.splits.iter().any(|s| s.id == new_split.id) {
        return Err(Error::BadRequest(format!(
            "Split with id '{}' already exists",
            new_split.id
        )));
    }

    // Reject typos early: a split tagged to an unknown account would
    // silently never render anywhere.
    if let Some(ref acct) = new_split.account {
        let reg = state.accounts.read().await;
        ensure_known_account(&reg, acct)?;
    }

    config.splits.push(new_split);
    splits::save_splits(&config, &state.splits_config_path)?;
    invalidate_all_split_caches(&state).await;

    Ok(Json(serde_json::json!(config.splits)))
}

async fn update_split(
    State(state): State<Arc<AppState>>,
    Path(split_id): Path<String>,
    Json(updated): Json<SplitInbox>,
) -> Result<impl IntoResponse, Error> {
    let mut config = splits::load_splits(
        &state.splits_config_path,
        std::env::var("SUPERVILLAIN_SPLITS").ok().as_deref(),
    );

    // Reject typos early: a split tagged to an unknown account would
    // silently never render anywhere.
    if let Some(ref acct) = updated.account {
        let reg = state.accounts.read().await;
        ensure_known_account(&reg, acct)?;
    }

    if updated.id != split_id {
        return Err(Error::BadRequest(format!(
            "Split id is immutable ('{split_id}' != '{}')",
            updated.id
        )));
    }

    let existing = config
        .splits
        .iter_mut()
        .find(|s| s.id == split_id)
        .ok_or_else(|| Error::NotFound(format!("Split '{split_id}' not found")))?;

    // PUT replaces the whole split: a body without `account` UNTAGS it
    // (makes it global). Deliberate — the body is the full new state.
    *existing = updated;
    splits::save_splits(&config, &state.splits_config_path)?;
    invalidate_all_split_caches(&state).await;

    Ok(Json(serde_json::json!(config.splits)))
}

async fn delete_split(
    State(state): State<Arc<AppState>>,
    Path(split_id): Path<String>,
) -> Result<impl IntoResponse, Error> {
    let mut config = splits::load_splits(
        &state.splits_config_path,
        std::env::var("SUPERVILLAIN_SPLITS").ok().as_deref(),
    );

    let original_len = config.splits.len();
    config.splits.retain(|s| s.id != split_id);

    if config.splits.len() == original_len {
        return Err(Error::NotFound(format!("Split '{split_id}' not found")));
    }

    splits::save_splits(&config, &state.splits_config_path)?;
    invalidate_all_split_caches(&state).await;

    Ok(Json(serde_json::json!(config.splits)))
}

// =============================================================================
// Timezone settings
// =============================================================================

#[derive(serde::Deserialize)]
struct TimezoneConfigBody {
    #[serde(default = "default_true_bool")]
    use_system: bool,
    #[serde(default)]
    manual_primary: Option<String>,
    #[serde(default)]
    additional: Vec<String>,
}

fn default_true_bool() -> bool {
    true
}

fn timezone_env_override() -> Option<String> {
    std::env::var("SUPERVILLAIN_TIMEZONE").ok()
}

async fn get_timezone(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let cfg = timezone::load_config(
        &state.timezone_config_path,
        timezone_env_override().as_deref(),
    );
    Json(serde_json::to_value(timezone::resolve(&cfg)).unwrap_or_default())
}

async fn put_timezone(
    State(state): State<Arc<AppState>>,
    Json(body): Json<TimezoneConfigBody>,
) -> Result<impl IntoResponse, Error> {
    if let Some(ref primary) = body.manual_primary
        && !primary.is_empty()
        && !timezone::validate_iana(primary)
    {
        return Err(Error::BadRequest(format!(
            "Unknown IANA timezone: {primary}"
        )));
    }
    for tz in &body.additional {
        if !timezone::validate_iana(tz) {
            return Err(Error::BadRequest(format!("Unknown IANA timezone: {tz}")));
        }
    }

    let _guard = state.timezone_write_lock.lock().await;
    let mut existing = timezone::load_config(
        &state.timezone_config_path,
        timezone_env_override().as_deref(),
    );
    existing.use_system = body.use_system;
    existing.manual_primary = body.manual_primary.filter(|s| !s.is_empty());
    existing.additional = body.additional;
    // Seed last_known_system_tz on first save so the change banner has a baseline.
    if existing.last_known_system_tz.is_none() {
        existing.last_known_system_tz = Some(timezone::detect_system_tz());
    }
    timezone::save_config(&existing, &state.timezone_config_path)?;

    Ok(Json(
        serde_json::to_value(timezone::resolve(&existing)).unwrap_or_default(),
    ))
}

async fn accept_system_timezone(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, Error> {
    let _guard = state.timezone_write_lock.lock().await;
    let mut cfg = timezone::load_config(
        &state.timezone_config_path,
        timezone_env_override().as_deref(),
    );
    let system = timezone::detect_system_tz();
    cfg.last_known_system_tz = Some(system.clone());
    cfg.dismissed_change_to = None;
    // If the user wants to track system, that's already wired through use_system.
    // Don't toggle use_system here; "Accept system" means "the new system TZ is fine".
    timezone::save_config(&cfg, &state.timezone_config_path)?;
    Ok(Json(
        serde_json::to_value(timezone::resolve(&cfg)).unwrap_or_default(),
    ))
}

#[derive(serde::Deserialize, Default)]
struct DismissTimezoneBody {
    /// The system TZ value the client was looking at when the user dismissed
    /// the banner. If it no longer matches the currently detected system TZ,
    /// we refuse — the user can't "Keep current" on a change they never saw.
    /// Optional for backwards-compat with clients that don't send it.
    #[serde(default)]
    seen_system: Option<String>,
}

async fn dismiss_timezone_change(
    State(state): State<Arc<AppState>>,
    body: Option<Json<DismissTimezoneBody>>,
) -> Result<impl IntoResponse, Error> {
    let _guard = state.timezone_write_lock.lock().await;
    let current_system = timezone::detect_system_tz();
    if let Some(Json(b)) = body
        && let Some(seen) = b.seen_system
        && !seen.is_empty()
        && seen != current_system
    {
        return Err(Error::Conflict(format!(
            "system timezone changed from {seen} to {current_system} since the banner was shown — please recheck"
        )));
    }
    let mut cfg = timezone::load_config(
        &state.timezone_config_path,
        timezone_env_override().as_deref(),
    );
    cfg.dismissed_change_to = Some(current_system);
    timezone::save_config(&cfg, &state.timezone_config_path)?;
    Ok(Json(
        serde_json::to_value(timezone::resolve(&cfg)).unwrap_or_default(),
    ))
}

async fn list_timezones() -> impl IntoResponse {
    let names: Vec<&'static str> = chrono_tz::TZ_VARIANTS.iter().map(|tz| tz.name()).collect();
    Json(serde_json::json!(names))
}

// =============================================================================
// Calendar invite composition
// =============================================================================

#[derive(serde::Deserialize)]
struct InviteAttendee {
    email: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(serde::Deserialize)]
struct SendInviteBody {
    to: Vec<String>,
    #[serde(default)]
    cc: Vec<String>,
    #[serde(default)]
    bcc: Vec<String>,
    subject: String,
    #[serde(default)]
    body: String,
    summary: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    location: Option<String>,
    start: String, // "2026-06-01T10:00:00"
    end: String,
    #[serde(default)]
    tz: Option<String>,
    #[serde(default)]
    attendees: Vec<InviteAttendee>,
    #[serde(default)]
    from_address: Option<String>,
    /// File attachments resolved at send time the same way `/emails/send`
    /// resolves them. Without this, users who attached files AND enabled
    /// the invite toggle would silently lose the attachments (roborev 186 #6).
    #[serde(default)]
    attachments: Vec<Attachment>,
}

/// Send an email with an embedded iTIP REQUEST.
///
/// Attendee list comes from `body.attendees` only — `to`/`cc`/`bcc` control
/// envelope routing, not ICS ATTENDEE properties. BCC privacy is preserved
/// (BCC recipients are not visible in the ICS) but the caller must include
/// them in `attendees` if it wants them tracked in calendar attendees.
/// The frontend builds `attendees` from `to + cc` deliberately; if BCC
/// support is added to the compose UI, decide policy then (include + warn,
/// or exclude + warn).
async fn send_invite_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<AccountParam>,
    Json(body): Json<SendInviteBody>,
) -> Result<impl IntoResponse, Error> {
    let tz_cfg = timezone::load_config(
        &state.timezone_config_path,
        timezone_env_override().as_deref(),
    );
    let resolved = timezone::resolve(&tz_cfg);
    let tz_name = body
        .tz
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or(resolved.primary.clone());
    if !timezone::validate_iana(&tz_name) {
        return Err(Error::BadRequest(format!(
            "Unknown IANA timezone: {tz_name}"
        )));
    }
    let tz: chrono_tz::Tz = std::str::FromStr::from_str(&tz_name)
        .map_err(|_| Error::BadRequest(format!("Unknown IANA timezone: {tz_name}")))?;

    let start_naive = chrono::NaiveDateTime::parse_from_str(&body.start, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(&body.start, "%Y-%m-%dT%H:%M"))
        .map_err(|e| Error::BadRequest(format!("Invalid start time: {e}")))?;
    let end_naive = chrono::NaiveDateTime::parse_from_str(&body.end, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(&body.end, "%Y-%m-%dT%H:%M"))
        .map_err(|e| Error::BadRequest(format!("Invalid end time: {e}")))?;
    let dtstart = chrono::TimeZone::from_local_datetime(&tz, &start_naive)
        .earliest()
        .ok_or_else(|| Error::BadRequest("start time has no valid mapping in tz".into()))?;
    let dtend = chrono::TimeZone::from_local_datetime(&tz, &end_naive)
        .earliest()
        .ok_or_else(|| Error::BadRequest("end time has no valid mapping in tz".into()))?;
    // Roborev 186 #7: reject negative-duration invites at the boundary rather
    // than relying on the recipient's calendar client.
    if dtend <= dtstart {
        return Err(Error::BadRequest(
            "end time must be after start time".into(),
        ));
    }

    let session_lock = resolve_session(&state, params.account.as_deref()).await?;
    let mut session = session_lock.write().await;
    let from_addr = body
        .from_address
        .clone()
        .unwrap_or_else(|| session.username().to_string());

    let organizer_email = from_addr.clone();
    let attendees: Vec<Attendee> = body
        .attendees
        .iter()
        .map(|a| Attendee {
            email: a.email.clone(),
            name: a.name.clone(),
            status: "NEEDS-ACTION".into(),
        })
        .collect();

    let ics = calendar::generate_invite(
        &organizer_email,
        None,
        &body.summary,
        body.description.as_deref(),
        body.location.as_deref(),
        dtstart,
        dtend,
        &attendees,
        None,
    );

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
        in_reply_to: None,
        references: None,
        attachments: body.attachments,
        calendar_ics: Some(ics),
    };

    let result = provider::send_email(&mut session, &submission, &from_addr, None).await?;
    match result {
        Some(id) => Ok(Json(serde_json::json!({"success": true, "emailId": id}))),
        None => Err(Error::Internal("Failed to send invite".into())),
    }
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

    #[test]
    fn app_js_signature_prefilled_via_clear_compose() {
        // clearCompose is the single choke point startCompose/startReply/
        // startForward all call first, so prefilling there covers new,
        // reply, and forward uniformly — this pins that the prefill lives
        // there rather than being duplicated (or, worse, injected at send
        // time, which the design explicitly forbids: the user must see
        // exactly what sends).
        let start = APP_JS
            .find("function clearCompose()")
            .expect("clearCompose must exist");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}").expect("clearCompose must close");
        let block = &rest[..end];
        assert!(
            block.contains("composeSignaturePrefill()"),
            "clearCompose must prefill the body via composeSignaturePrefill()"
        );
        assert!(
            block.contains("setSelectionRange(0, 0)"),
            "clearCompose must place the cursor at position 0 after prefilling"
        );
        assert!(
            APP_JS.contains("function composeSignaturePrefill"),
            "composeSignaturePrefill helper must exist"
        );
        assert!(
            APP_JS.contains("-- \\n"),
            "signature prefill must use the RFC 3676 delimiter '-- ' \
             (trailing space significant) before a newline"
        );
    }

    // ====================================================================
    // Contact autocomplete on To/Cc (kata e64s, task B6) — client-side only,
    // no server surface. These are string-invariant tests per repo
    // convention (no JS harness): they pin the shape of static/app.js
    // rather than executing it.
    // ====================================================================

    #[test]
    fn contact_index_state_present() {
        assert!(
            APP_JS.contains("contactIndex: new Map()"),
            "state.contactIndex must be a Map, built by harvesting from \
             loaded email list pages and the Sent mailbox (kata e64s)"
        );
    }

    #[test]
    fn contact_index_is_account_scoped() {
        // selectAccount's documented convention: caches are account-scoped
        // (splitListCache/emailCache/scrollPositions all key by account and
        // are never shared across a switch). The contact index must follow
        // it — Account A's contacts leaking into Account B's compose would
        // cross-pollinate address books. Pins: a per-account accessor
        // exists, harvest writes through it, and rankContactMatches only
        // reads the CURRENT account's entries.
        assert!(
            APP_JS.contains("function contactIndexFor("),
            "a per-account contact-map accessor must exist"
        );

        let start = APP_JS
            .find("function harvestContacts(")
            .expect("harvestContacts must exist");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}").expect("harvestContacts must close");
        assert!(
            rest[..end].contains("contactIndexFor(accountId)"),
            "harvestContacts must write into the harvested account's own map"
        );

        let start2 = APP_JS
            .find("function rankContactMatches(")
            .expect("rankContactMatches must exist");
        let rest2 = &APP_JS[start2..];
        let end2 = rest2.find("\n}").expect("rankContactMatches must close");
        assert!(
            rest2[..end2].contains("state.currentAccount"),
            "rankContactMatches must only surface the current account's contacts"
        );
    }

    #[test]
    fn contact_rank_function_sorts_by_count_then_last_seen_and_excludes_self() {
        let start = APP_JS
            .find("function rankContactMatches(")
            .expect("a pure rank/match helper must exist for contact autocomplete");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}").expect("rankContactMatches must close");
        let block = &rest[..end];
        assert!(
            block.contains("b.count - a.count"),
            "contacts must rank by frequency count descending first"
        );
        assert!(
            block.contains("lastSeen") && block.contains("localeCompare"),
            "ties must break by lastSeen descending"
        );
        assert!(
            block.contains("ownIdentityEmails"),
            "rank/match must exclude the account's own identity addresses"
        );
    }

    #[test]
    fn contact_harvest_hooks_list_load_and_refill_paths() {
        assert!(
            APP_JS.contains("function harvestContacts("),
            "harvestContacts must exist to build the contact index from loaded emails"
        );

        // Primary list-load path.
        let start = APP_JS
            .find("async function loadEmails()")
            .expect("loadEmails must exist");
        let rest = &APP_JS[start..];
        let end = rest
            .find("\nasync function maybeRefillEmails")
            .expect("loadEmails must be followed by maybeRefillEmails");
        assert!(
            rest[..end].contains("harvestContacts("),
            "loadEmails must harvest contacts from the fetched page"
        );

        // Infinite-scroll append path.
        let start2 = APP_JS
            .find("async function maybeRefillEmails()")
            .expect("maybeRefillEmails must exist");
        let rest2 = &APP_JS[start2..];
        let end2 = rest2
            .find("\nasync function loadEmailDetail")
            .expect("maybeRefillEmails must be followed by loadEmailDetail");
        assert!(
            rest2[..end2].contains("harvestContacts("),
            "maybeRefillEmails must harvest contacts from newly appended emails"
        );
    }

    #[test]
    fn sent_mailbox_background_harvest_degrades_silently() {
        let start = APP_JS
            .find("function harvestSentContactsOnce")
            .expect("harvestSentContactsOnce must exist");
        let rest = &APP_JS[start..];
        let end = rest
            .find("\n}")
            .expect("harvestSentContactsOnce must close");
        let block = &rest[..end];
        assert!(
            block.contains("role === 'sent'"),
            "must resolve the Sent mailbox from state.mailboxes by role"
        );
        assert!(
            block.contains("console.warn"),
            "a failed Sent-mailbox fetch must degrade silently (console.warn only)"
        );
        assert!(
            !block.contains("showStatus"),
            "a failed Sent-mailbox fetch must not surface an error banner to the user"
        );
    }

    #[test]
    fn contact_dropdown_keydown_scoped_to_focused_input_when_open() {
        // The compose insert-mode key handler must only intercept dropdown
        // navigation keys when the contact dropdown is actually open AND the
        // event target is the To/Cc input it belongs to — otherwise it must
        // fall through untouched to the existing Escape/Ctrl+Enter handling
        // (autosave/send-lock from kata wm57 must not be disturbed).
        let start = APP_JS
            .find("state.view === 'compose' && state.mode === 'insert'")
            .expect("compose insert-mode key handler must exist");
        let rest = &APP_JS[start..];
        let end = rest
            .find("\n    // Compose normal-mode:")
            .expect("compose insert-mode block must be followed by the normal-mode 'a' handler");
        let block = &rest[..end];
        assert!(
            block.contains("state.contactAcField"),
            "compose keydown must gate contact-dropdown key handling on an open dropdown"
        );
        assert!(
            block.contains("e.target === els.composeTo")
                || block.contains("e.target === els.composeCc"),
            "contact-dropdown key handling must be scoped to the focused To/Cc input"
        );
        assert!(
            block.contains("sendEmail()"),
            "existing Ctrl+Enter send handling must still be present"
        );
    }

    #[test]
    fn contact_autocomplete_inserts_bare_email_not_display_name() {
        // To/Cc are parsed downstream as plain comma-separated address
        // strings (see sendEmail / build_draft_email) — inserting a
        // "Name <email>" form here would ship as a literally-invalid
        // recipient address.
        let start = APP_JS
            .find("function acceptContactAutocomplete(")
            .expect("acceptContactAutocomplete must exist");
        let rest = &APP_JS[start..];
        let end = rest
            .find("\n}")
            .expect("acceptContactAutocomplete must close");
        let block = &rest[..end];
        assert!(
            block.contains("contact.email"),
            "acceptContactAutocomplete must insert the bare contact.email"
        );
        assert!(
            !block.contains("contact.name"),
            "acceptContactAutocomplete must not compose a 'Name <email>' string into the field"
        );
    }

    #[test]
    fn contact_segment_bounds_shared_by_match_and_accept() {
        assert!(
            APP_JS.contains("function contactSegmentBounds("),
            "a single segment-bounds helper must exist so the matcher (read) \
             and acceptContactAutocomplete (replace) agree on comma-segment \
             boundaries — otherwise mid-field edits could replace the wrong span"
        );
    }

    #[test]
    fn contact_autocomplete_html_and_css_wired() {
        assert!(INDEX_HTML.contains(r#"id="compose-to-autocomplete""#));
        assert!(INDEX_HTML.contains(r#"id="compose-cc-autocomplete""#));
        assert!(
            STYLE_CSS.contains(".contact-autocomplete"),
            "dropdown container styling must exist"
        );
        assert!(
            STYLE_CSS.contains(".autocomplete-item"),
            "contact dropdown rows should reuse the .autocomplete-item idiom from #search-autocomplete"
        );
    }

    // ====================================================================
    // Threading / conversation grouping in the desktop list view
    // (kata 64z6, task B7) — client-side v1. String-invariant tests per
    // repo convention (no JS harness): they pin the shape of static/app.js
    // rather than executing it.
    // ====================================================================

    #[test]
    fn thread_group_state_present() {
        // The append-time grouping structure and the expand set both live on
        // state. threadGroups: Map threadId -> ordered array of email ids;
        // expandedThreads: Set of threadIds the user has expanded inline.
        assert!(
            APP_JS.contains("threadGroups: new Map()"),
            "state.threadGroups must be a Map (threadId -> ordered email ids), \
             built at append time (kata 64z6)"
        );
        assert!(
            APP_JS.contains("expandedThreads: new Set()"),
            "state.expandedThreads must be a Set of expanded threadIds (kata 64z6)"
        );
    }

    #[test]
    fn thread_groups_built_at_append_sites() {
        // Muratori constraint: the grouping map is built/extended where pages
        // enter state.emails, never rebuilt per-render. Full replace ->
        // rebuildThreadGroups; infinite-scroll append -> extendThreadGroups.
        assert!(
            APP_JS.contains("function extendThreadGroups(")
                && APP_JS.contains("function rebuildThreadGroups("),
            "both an append-extend and a full-rebuild grouping helper must exist"
        );

        // Full list replace path harvests via rebuild.
        let start = APP_JS
            .find("async function loadEmails()")
            .expect("loadEmails must exist");
        let rest = &APP_JS[start..];
        let end = rest
            .find("\nasync function maybeRefillEmails")
            .expect("loadEmails must be followed by maybeRefillEmails");
        assert!(
            rest[..end].contains("rebuildThreadGroups("),
            "loadEmails must rebuild thread groups on a full list replace"
        );

        // Infinite-scroll append path extends the existing groups in place —
        // at the same concat site pages enter state.emails.
        let start2 = APP_JS
            .find("async function maybeRefillEmails()")
            .expect("maybeRefillEmails must exist");
        let rest2 = &APP_JS[start2..];
        let end2 = rest2
            .find("\nasync function loadEmailDetail")
            .expect("maybeRefillEmails must be followed by loadEmailDetail");
        let block = &rest2[..end2];
        assert!(
            block.contains("state.emails.concat(newEmails)")
                && block.contains("extendThreadGroups("),
            "maybeRefillEmails must extend thread groups at the append site"
        );
    }

    #[test]
    fn visible_rows_derivation_exists_and_keyboard_nav_consumes_it() {
        // The single seam: selection / auto-advance / undo index into a
        // derived VISIBLE row model (collapsed thread = 1 row, expanded = row
        // per member), not into state.emails directly. Keyboard nav must go
        // through it so a collapsed thread counts as one step.
        assert!(
            APP_JS.contains("function visibleRows("),
            "a visibleRows() derivation must exist as the flat-row seam (kata 64z6)"
        );

        let start = APP_JS
            .find("function moveSelection(")
            .expect("moveSelection must exist");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}").expect("moveSelection must close");
        assert!(
            rest[..end].contains("visibleRows("),
            "j/k navigation must index into visibleRows(), not state.emails"
        );
    }

    #[test]
    fn thread_row_shows_count_badge_and_aggregate_flags() {
        // A thread with 2+ loaded members collapses to one row carrying a
        // count badge; it reads unread if ANY member is unread and starred if
        // ANY member is starred (aggregate, computed live from state.emails so
        // the badge stays accurate after a member is archived/undone).
        let start = APP_JS
            .find("function visibleRows(")
            .expect("visibleRows must exist");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}\n").expect("visibleRows must close");
        let block = &rest[..end];
        assert!(
            block.contains("anyUnread") && block.contains("anyStarred"),
            "a collapsed thread must aggregate unread/starred across members"
        );
        assert!(
            block.contains("kind: 'thread'"),
            "visibleRows must emit a collapsed-thread row kind"
        );

        assert!(
            APP_JS.contains("email-thread-count"),
            "the collapsed-thread row must render a count-badge element"
        );
        let rstart = APP_JS
            .find("function renderEmailList(")
            .expect("renderEmailList must exist");
        let rrest = &APP_JS[rstart..];
        let rend = rrest.find("\n}\n").expect("renderEmailList must close");
        assert!(
            rrest[..rend].contains("row.count"),
            "the rendered count badge must show the thread's live member count"
        );
    }

    #[test]
    fn undo_reinsert_selects_by_visible_row_not_flat_index() {
        // The flat-row landmine: performUndo used to set selectedIndex to the
        // state.emails insert index. Under grouping, selection indexes the
        // VISIBLE rows, so it must resolve the re-inserted email's visible row.
        let start = APP_JS
            .find("async function performUndo()")
            .expect("performUndo must exist");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}\n").expect("performUndo must close");
        let block = &rest[..end];
        assert!(
            block.contains("visibleRowIndexForEmailId("),
            "undo must select the re-inserted email's VISIBLE row, not a raw \
             state.emails index"
        );
    }

    #[test]
    fn list_selection_reads_visible_row_email_id() {
        // Acting on a collapsed thread row acts on the NEWEST message (v1, no
        // bulk thread actions): the selected id comes from the visible row.
        let start = APP_JS
            .find("function getSelectedEmailId()")
            .expect("getSelectedEmailId must exist");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}").expect("getSelectedEmailId must close");
        assert!(
            rest[..end].contains("visibleRows("),
            "the selected email id (list view) must come from the visible row model"
        );
    }

    #[test]
    fn expanded_member_rows_suppress_date_dividers() {
        // An expanded thread's member sub-rows are older than the collapsed
        // header they sit under, so letting them participate in the date-
        // divider computation injects a stray divider mid-thread AND forces a
        // duplicate divider for the next non-member row. The divider logic
        // must skip member rows entirely: no divider emitted for them and
        // lastGroup must not advance on them.
        let start = APP_JS
            .find("function renderEmailList(")
            .expect("renderEmailList must exist");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}\n").expect("renderEmailList must close");
        let block = &rest[..end];

        let guard = block
            .find("!isMember")
            .expect("the divider computation must be gated off member rows");
        let advance = block
            .find("lastGroup = group")
            .expect("the divider computation must advance lastGroup");
        assert!(
            guard < advance,
            "the member-row guard must wrap the divider/lastGroup advance — \
             member sub-rows must neither emit a divider nor move lastGroup"
        );
    }

    // ====================================================================
    // Settings view (account management) regression sentinels
    // ====================================================================

    #[test]
    fn settings_view_present_in_html() {
        assert!(INDEX_HTML.contains(r#"id="settings-view""#));
        assert!(INDEX_HTML.contains(r#"id="account-pane-list""#));
        assert!(INDEX_HTML.contains(r#"id="account-form""#));
    }

    #[test]
    fn settings_form_uses_data_provider_attribute() {
        // Locality of behaviour: visibility-by-provider rule lives in HTML.
        assert!(
            INDEX_HTML.contains(r#"data-provider="fastmail""#),
            "fastmail-only fields must declare visibility via data-provider"
        );
        assert!(
            INDEX_HTML.contains(r#"data-provider="outlook,gmail""#),
            "OAuth shared fields must declare visibility via data-provider"
        );
    }

    #[test]
    fn settings_styles_present() {
        assert!(STYLE_CSS.contains(".account-pane"));
        assert!(STYLE_CSS.contains(".secret-input"));
        assert!(STYLE_CSS.contains(".confirm-delete"));
        assert!(STYLE_CSS.contains("#mode-indicator.awaiting"));
        assert!(STYLE_CSS.contains(".auth-status-pill"));
    }

    #[test]
    fn settings_authorize_uses_long_poll_not_polling() {
        // Long-poll: one POST to /authorize. No /auth-status polling.
        assert!(
            APP_JS.contains("/authorize"),
            "settings must call POST /authorize"
        );
        assert!(
            !APP_JS.contains("/auth-status"),
            "must not poll a /auth-status endpoint — long-poll is the state machine"
        );
        assert!(APP_JS.contains("AbortController"));
    }

    #[test]
    fn settings_first_run_auto_routes() {
        assert!(
            APP_JS.contains("openSettings({ firstRun: true })"),
            "loadAccounts must auto-route to settings when no accounts exist"
        );
    }

    #[test]
    fn help_overlay_documents_new_shortcuts() {
        assert!(
            INDEX_HTML.contains(">g s<"),
            "help overlay should document the g s chord"
        );
        assert!(
            INDEX_HTML.contains(">Shift+D<"),
            "help overlay should document Shift+D (set default)"
        );
        assert!(
            INDEX_HTML.contains(">Ctrl+Enter<"),
            "help overlay should document Ctrl+Enter (save)"
        );
    }

    #[test]
    fn load_emails_renders_cached_snapshot_before_network_refresh() {
        // The Superhuman-style "instant switch" contract: when loadEmails is
        // called for a mailbox/split/account with a cached entry, the cached
        // list renders immediately (no awaiting the network). The fresh fetch
        // races in the background and replaces the snapshot on arrival. Pins
        // both halves: (1) the cache-hit branch exists, (2) no caller wipes
        // the cache wholesale — that would force a cold reload on every
        // switch and defeat the optimization.
        assert!(
            APP_JS.contains("if (splitListCache[context]) {"),
            "loadEmails must render the cached snapshot before awaiting the network"
        );
        assert!(
            !APP_JS.contains("clearSplitListCache()"),
            "no caller should wipe splitListCache wholesale — keys are already (account, mailbox, split, starred, search)-scoped"
        );
    }

    #[test]
    fn email_caches_are_account_scoped() {
        // Cross-account isolation is enforced by prefixing every cache key
        // with the active account id (`cacheKey(emailId)`), not by wiping
        // caches on account switch. Wiping forced every revisit to refetch
        // from the provider, which was unusably slow (Gmail in particular
        // takes seconds per body). The scoped-key approach is both safer
        // (no leak window between wipe and refill) and preserves state
        // across switches — returning to an email finds its cached body.
        assert!(
            APP_JS.contains("function cacheKey(emailId)"),
            "static/app.js must define cacheKey() so emailCache/scrollPositions are account-scoped"
        );
        assert!(
            APP_JS.contains("state.currentEmail = null"),
            "selectAccount must null out state.currentEmail (no cross-account detail residue)"
        );
        assert!(
            !APP_JS.contains("for (const k in emailCache) delete emailCache[k]"),
            "selectAccount must NOT wipe emailCache — cacheKey() scoping makes the wipe both unnecessary and a performance regression"
        );
        assert!(
            !APP_JS.contains("for (const k in scrollPositions) delete scrollPositions[k]"),
            "selectAccount must NOT wipe scrollPositions — cacheKey() scoping handles isolation"
        );
    }

    #[test]
    fn api_helper_excludes_settings_from_account_param() {
        // The shared api client allowlists which paths receive ?account=.
        // Settings paths (`/accounts/...`) must NOT be auto-tagged.
        assert!(
            API_JS.contains("ACCOUNT_SCOPED_API"),
            "api.js must use an allowlist regex for ?account= injection"
        );
        assert!(
            API_JS.contains(
                "/(emails|mailboxes|identities|splits|upload|split-counts|calendar|drafts)"
            ),
            "allowlist regex must enumerate account-scoped path prefixes"
        );
    }

    #[test]
    fn load_splits_goes_through_account_scoped_api_helper() {
        // A raw fetch skips ?account= and renders every account's tabs.
        assert!(
            APP_JS.contains("const splits = await api('GET', '/splits')"),
            "loadSplits must use the api() helper so ?account= is appended"
        );
        assert!(
            !APP_JS.contains("fetch('/api/splits')"),
            "loadSplits must not bypass api() with a raw fetch"
        );
    }

    #[test]
    fn load_splits_guards_against_stale_account_switch() {
        // On rapid account switches, account A's in-flight response can
        // land after B's and overwrite state.splits while B is active.
        let start = APP_JS
            .find("async function loadSplits()")
            .expect("loadSplits must exist");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}").expect("loadSplits must close");
        let body = &rest[..end];
        assert!(
            body.matches("state.currentAccount?.id !== accountId")
                .count()
                >= 2,
            "loadSplits must discard BOTH a stale success and a stale failure — \
             a failed request from the previous account must not wipe the new \
             account's already-loaded splits in the catch branch"
        );
    }

    #[test]
    fn init_does_not_call_load_splits_directly() {
        // init() runs before account selection, so state.currentAccount is
        // still null — a direct call here fetches the unscoped split list,
        // which can race and overwrite the account-scoped load triggered by
        // selectAccount() (called from loadAccounts()).
        let start = APP_JS.find("function init() {").expect("init must exist");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}").expect("init must close");
        let body = &rest[..end];
        assert!(
            !body.contains("loadSplits()"),
            "init must not call loadSplits() directly; selectAccount() loads it once an account is chosen"
        );
    }

    #[test]
    fn select_account_reloads_splits() {
        // Tab sets differ per account; switching must rebuild the row.
        let start = APP_JS
            .find("function selectAccount")
            .expect("selectAccount must exist");
        let rest = &APP_JS[start..];
        // Slice to the function's own closing brace (first `}` at column
        // 0) so a match can only come from selectAccount's body, not the
        // loadSplits() declaration that follows it in the file.
        let end = rest.find("\n}").expect("selectAccount must close");
        let body = &rest[..end];
        assert!(
            body.contains("loadSplits()"),
            "selectAccount must call loadSplits()"
        );
    }

    #[test]
    fn select_account_resets_stale_split_state() {
        // Regression test: before loadSplits()'s async response lands, the
        // previous account's tabs/counts must not linger — otherwise a fast
        // click into the new account briefly renders the old account's
        // split tabs and counts.
        let start = APP_JS
            .find("function selectAccount")
            .expect("selectAccount must exist");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}").expect("selectAccount must close");
        let body = &rest[..end];
        assert!(
            body.contains("state.splits = []"),
            "selectAccount must clear state.splits before the new account's loadSplits() resolves"
        );
        assert!(
            body.contains("state.splitCounts = {}"),
            "selectAccount must clear state.splitCounts before the new account's loadSplitCounts() resolves"
        );
    }

    // =========================================================================
    // Desktop sort control (kata 09ef) — buildEmailListUrl appends &sort=,
    // splitCacheKey discriminates by it (mirrors starredOnly), and
    // selectAccount resets it to the default like other per-session view state.
    // =========================================================================

    #[test]
    fn build_email_list_url_appends_sort_param() {
        let start = APP_JS
            .find("function buildEmailListUrl")
            .expect("buildEmailListUrl must exist");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}").expect("buildEmailListUrl must close");
        let body = &rest[..end];
        assert!(
            body.contains("sort="),
            "buildEmailListUrl must append &sort= so the sort control reaches the server"
        );
        assert!(
            body.contains("state.sortOrder"),
            "buildEmailListUrl must read the sort value from state.sortOrder"
        );
    }

    #[test]
    fn split_cache_key_includes_sort_order() {
        // Same bug class as the server's prefetch InboxKey: without this,
        // toggling sort would render a stale, wrong-order splitListCache
        // entry before the network refresh corrects it, and the refresh
        // would then overwrite the *other* order's cached entry.
        let start = APP_JS
            .find("function splitCacheKey")
            .expect("splitCacheKey must exist");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}").expect("splitCacheKey must close");
        let body = &rest[..end];
        assert!(
            body.contains("state.sortOrder"),
            "splitCacheKey must fold state.sortOrder into the cache key"
        );
    }

    #[test]
    fn render_sort_toggle_flags_gmail_paged_ascending_order() {
        // Regression test (roborev 291): Gmail's "oldest first" is only
        // oldest-first *within each fetched page* (see gmail.rs's
        // apply_sort_order doc comment) — paginating forward still walks
        // Gmail's underlying pages newest-block-first. Fastmail/Outlook
        // sort globally, so without a per-provider affordance the desktop
        // toggle reads identically for a guarantee that's actually weaker
        // on Gmail. renderSortToggle must call out the Gmail case in both
        // the visible label and a title attribute.
        let start = APP_JS
            .find("function renderSortToggle")
            .expect("renderSortToggle must exist");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}").expect("renderSortToggle must close");
        let body = &rest[..end];
        assert!(
            body.contains("state.currentAccount?.provider === 'gmail'"),
            "renderSortToggle must special-case the Gmail provider"
        );
        assert!(
            body.contains("(per page)"),
            "renderSortToggle must append a per-page hint to the label for Gmail ascending order"
        );
        assert!(
            body.contains("els.sortToggle.title"),
            "renderSortToggle must set a title attribute explaining the per-page limitation"
        );
    }

    #[test]
    fn select_account_resets_sort_order_to_default() {
        let start = APP_JS
            .find("function selectAccount")
            .expect("selectAccount must exist");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}").expect("selectAccount must close");
        let body = &rest[..end];
        assert!(
            body.contains("state.sortOrder"),
            "selectAccount must reset state.sortOrder to the default on account switch"
        );
    }

    #[test]
    fn save_split_tags_current_account() {
        assert!(
            APP_JS.contains("account: state.currentAccount?.id"),
            "saveSplit must scope new splits to the active account"
        );
    }

    // =========================================================================
    // resolve_account_id / list_splits — reject unknown account ids (roborev 271)
    // =========================================================================

    fn test_state(known: &[&str], default_account: &str) -> AppState {
        let mut account_configs = std::collections::BTreeMap::new();
        for id in known {
            account_configs.insert(
                id.to_string(),
                accounts::AccountConfig::Fastmail {
                    username: format!("{id}@example.com"),
                    api_token: "tok".into(),
                    signature: None,
                },
            );
        }
        AppState {
            accounts: tokio::sync::RwLock::new(AccountRegistry {
                sessions: HashMap::new(),
                account_configs,
                default_account: default_account.to_string(),
            }),
            account_errors: tokio::sync::RwLock::new(Vec::new()),
            splits_config_path: std::path::PathBuf::from("/tmp/nonexistent-splits.json"),
            timezone_config_path: std::path::PathBuf::from("/tmp/nonexistent-timezone.json"),
            timezone_write_lock: tokio::sync::Mutex::new(()),
            config_path: std::path::PathBuf::from("/tmp/nonexistent-config"),
            tokens_dir: std::path::PathBuf::from("/tmp/nonexistent-tokens"),
            token_store: std::sync::Arc::new(crate::platform::FsTokenStore::new(
                std::path::PathBuf::from("/tmp/nonexistent-tokens"),
            )),
            authorizing: accounts::AuthorizingSlot::default(),
            config_error_baseline: std::sync::RwLock::new(Vec::new()),
            prefetch: std::sync::Arc::new(crate::prefetch::PrefetchCache::new()),
            prefetch_cache_path: std::env::temp_dir().join("supervillain-test-prefetch-cache.json"),
        }
    }

    #[tokio::test]
    async fn resolve_account_id_rejects_unknown_account() {
        let state = test_state(&["known"], "known");
        let err = resolve_account_id(&state, Some("typo"))
            .await
            .expect_err("an id absent from account_configs must be rejected");
        assert!(
            matches!(err, Error::BadRequest(ref msg) if msg.contains("Unknown account")),
            "expected a BadRequest naming the unknown account, got {err:?}"
        );
    }

    #[tokio::test]
    async fn resolve_account_id_accepts_known_account() {
        let state = test_state(&["known"], "known");
        let id = resolve_account_id(&state, Some("known"))
            .await
            .expect("a configured account must resolve");
        assert_eq!(id, "known");
    }

    #[tokio::test]
    async fn resolve_account_id_default_none_still_works() {
        // The None (default-account) path must keep working — default_account
        // is always kept in sync with account_configs by the mutation
        // handlers, so this can't regress into a false rejection.
        let state = test_state(&["known"], "known");
        let id = resolve_account_id(&state, None)
            .await
            .expect("omitting ?account= must fall back to the default account");
        assert_eq!(id, "known");
    }

    #[tokio::test]
    async fn list_splits_rejects_unknown_account_param() {
        let state = Arc::new(test_state(&["known"], "known"));
        let params = ListSplitsParams {
            account: Some("typo".into()),
        };
        let err = list_splits(State(state), Query(params))
            .await
            .err()
            .expect("an unknown ?account= must 400, not silently scope to untagged splits");
        assert!(
            matches!(err, Error::BadRequest(ref msg) if msg.contains("Unknown account")),
            "expected a BadRequest naming the unknown account, got {err:?}"
        );
    }

    #[tokio::test]
    async fn list_splits_without_account_param_is_unaffected() {
        // No ?account= → full list (management/debugging view); this must
        // stay reachable even though it never hits the new validation gate.
        let state = Arc::new(test_state(&["known"], "known"));
        let params = ListSplitsParams { account: None };
        let resp = list_splits(State(state), Query(params))
            .await
            .expect("omitting ?account= must not be rejected")
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // =========================================================================
    // GetEmailParams deserialization (roborev 285) — mark_read opt-out lets
    // mobile's prefetch warm the body cache without consuming unread state.
    // =========================================================================

    #[test]
    fn get_email_params_mark_read_absent_defaults_to_read_semantics() {
        let uri: axum::http::Uri = "/api/emails/e1".parse().unwrap();
        let Query(params) = Query::<GetEmailParams>::try_from_uri(&uri)
            .expect("no query string at all must still deserialize");
        assert_eq!(params.mark_read, None);
        assert!(
            params.mark_read.unwrap_or(true),
            "absent mark_read must preserve the current (auto-mark-read) behavior"
        );
    }

    #[test]
    fn get_email_params_mark_read_false_parses() {
        let uri: axum::http::Uri = "/api/emails/e1?mark_read=false".parse().unwrap();
        let Query(params) =
            Query::<GetEmailParams>::try_from_uri(&uri).expect("mark_read=false must deserialize");
        assert_eq!(params.mark_read, Some(false));
    }

    #[test]
    fn get_email_params_mark_read_false_with_account_parses() {
        // Mirrors the URL mobile actually sends: mark_read=false alongside
        // ?account=, joined with '&' since the path already has a '?'.
        let uri: axum::http::Uri = "/api/emails/e1?mark_read=false&account=work"
            .parse()
            .unwrap();
        let Query(params) = Query::<GetEmailParams>::try_from_uri(&uri)
            .expect("mark_read=false&account=... must deserialize");
        assert_eq!(params.mark_read, Some(false));
        assert_eq!(params.account.as_deref(), Some("work"));
    }

    #[test]
    fn get_email_keeps_prefetch_off_the_interactive_lane() {
        // provider::get_emails' reserved lane is "only for fetches a user is
        // actively waiting on … never for bulk fan-outs" — but both bundles'
        // prefetchAdjacentEmails hit this same route fire-and-forget (three
        // full-body fetches per email open). mark_read=false is the prefetch
        // signature, so the handler must derive the lane from it instead of
        // hardcoding priority for every caller (roborev 315).
        let src = include_str!("routes.rs");
        let handler_src = src.split("mod tests").next().unwrap_or(src);
        let start = handler_src
            .find("async fn get_email(")
            .expect("get_email must exist");
        let rest = &handler_src[start..];
        let end = rest.find("\n}").expect("get_email must close");
        let block = &rest[..end];
        assert!(
            block.contains("let priority = params.mark_read.unwrap_or(true)"),
            "get_email must derive the interactive-lane flag from mark_read"
        );
        assert!(
            !block.contains("None, true)"),
            "the fetch closure must pass the derived flag, not hardcode priority"
        );
    }

    // =========================================================================
    // ListEmailsParams sort deserialization (kata 09ef) — accept both known
    // values and absence, hard-reject anything else so a typo'd sort=
    // param can never silently fall back to the default order.
    // =========================================================================

    #[test]
    fn list_emails_params_sort_absent_deserializes_to_none() {
        let uri: axum::http::Uri = "/api/emails?mailbox_id=inbox".parse().unwrap();
        let Query(params) = Query::<ListEmailsParams>::try_from_uri(&uri)
            .expect("no sort param must still deserialize");
        assert_eq!(params.sort, None);
        assert_eq!(
            params.sort.unwrap_or_default(),
            EmailSort::DateDesc,
            "absent sort must resolve to today's default (newest-first) behavior"
        );
    }

    #[test]
    fn list_emails_params_sort_date_desc_parses() {
        let uri: axum::http::Uri = "/api/emails?mailbox_id=inbox&sort=date_desc"
            .parse()
            .unwrap();
        let Query(params) =
            Query::<ListEmailsParams>::try_from_uri(&uri).expect("sort=date_desc must deserialize");
        assert_eq!(params.sort, Some(EmailSort::DateDesc));
    }

    #[test]
    fn list_emails_params_sort_date_asc_parses() {
        let uri: axum::http::Uri = "/api/emails?mailbox_id=inbox&sort=date_asc"
            .parse()
            .unwrap();
        let Query(params) =
            Query::<ListEmailsParams>::try_from_uri(&uri).expect("sort=date_asc must deserialize");
        assert_eq!(params.sort, Some(EmailSort::DateAsc));
    }

    #[test]
    fn list_emails_params_sort_garbage_is_rejected() {
        let uri: axum::http::Uri = "/api/emails?mailbox_id=inbox&sort=banana".parse().unwrap();
        let result = Query::<ListEmailsParams>::try_from_uri(&uri);
        assert!(
            result.is_err(),
            "an unrecognized sort value must be a 400, not a silent default"
        );
    }

    // =========================================================================
    // list_is_cacheable sort gating (roborev 291)
    // =========================================================================

    fn cacheable_shape_params(sort: Option<EmailSort>) -> ListEmailsParams {
        ListEmailsParams {
            mailbox_id: Some("inbox".into()),
            limit: None,
            offset: None,
            split_id: None,
            search: None,
            account: None,
            starred: None,
            sort,
        }
    }

    #[test]
    fn list_is_cacheable_true_for_default_shape_and_sort() {
        let params = cacheable_shape_params(None);
        assert!(
            list_is_cacheable(&params, 0, EmailSort::DateDesc),
            "default-inbox shape with default sort must remain cacheable"
        );
    }

    #[test]
    fn list_is_cacheable_false_for_date_asc_sort() {
        // Regression test (roborev 291): a DateAsc request used to populate
        // its own InboxKey slot, keyed apart from the warmer's DateDesc
        // entry (kata 09ef). But the background warmer only ever re-warms
        // the DateDesc slot and there's no TTL, so that DateAsc slot was
        // never refreshed — a user sitting in "Oldest first" would never
        // see new mail until some unrelated local mutation invalidated the
        // whole account's cache. The fix: non-default sorts are no longer
        // cacheable at all, so they always fetch live (no read, no write).
        // DateDesc behavior is untouched — see the sibling
        // `list_is_cacheable_true_for_default_shape_and_sort` test.
        let params = cacheable_shape_params(Some(EmailSort::DateAsc));
        assert!(
            !list_is_cacheable(&params, 0, EmailSort::DateAsc),
            "a DateAsc request must always bypass the prefetch cache"
        );
    }

    #[test]
    fn list_is_cacheable_still_false_for_non_default_shape() {
        // Sanity check that extracting `list_is_cacheable` didn't change
        // the pre-existing (non-sort) gating conditions.
        let mut params = cacheable_shape_params(None);
        params.split_id = Some("primary".into());
        assert!(!list_is_cacheable(&params, 0, EmailSort::DateDesc));

        let mut params = cacheable_shape_params(None);
        params.starred = Some(true);
        assert!(!list_is_cacheable(&params, 0, EmailSort::DateDesc));

        let params = cacheable_shape_params(None);
        assert!(
            !list_is_cacheable(&params, 10, EmailSort::DateDesc),
            "non-zero offset must not be cacheable"
        );
    }

    #[test]
    fn mobile_app_js_prefetch_requests_mark_read_false() {
        let start = MOBILE_APP_JS
            .find("function prefetchAdjacentEmails")
            .expect("prefetchAdjacentEmails must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("prefetchAdjacentEmails must close");
        let block = &rest[..end];
        assert!(
            block.contains("mark_read=false"),
            "prefetching adjacent emails must not silently mark them read \
             (pass ?mark_read=false on the GET)"
        );
    }

    #[test]
    fn mobile_app_js_cache_hit_detail_marks_read_on_server() {
        // Opening a prefetched (mark_read=false) email from cache issues no
        // GET, so nothing tells the server it's now read unless
        // renderScreenDetail does so explicitly — otherwise the local
        // isUnread flip masks a still-unread server record until the next
        // refresh resurfaces it (roborev 286).
        let start = MOBILE_APP_JS
            .find("async function renderScreenDetail")
            .expect("renderScreenDetail must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("renderScreenDetail must close");
        let block = &rest[..end];
        assert!(
            block.contains("/mark-read"),
            "renderScreenDetail's cache-hit path must explicitly mark the \
             email read on the server"
        );
        assert!(
            block.contains("cacheHit"),
            "the mark-read call must be gated to the cache-hit path only — \
             the network-fetch path's GET already auto-marks read server-side"
        );
    }

    #[test]
    fn app_js_prefetch_requests_mark_read_false() {
        // Desktop counterpart to mobile_app_js_prefetch_requests_mark_read_false
        // (roborev 302, fix 2): the bare GET auto-marks read server-side, so
        // background warm-up must never silently consume unread state for an
        // email the user hasn't actually opened.
        let start = APP_JS
            .find("function prefetchAdjacentEmails")
            .expect("prefetchAdjacentEmails must exist");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}").expect("prefetchAdjacentEmails must close");
        let block = &rest[..end];
        assert!(
            block.contains("mark_read=false"),
            "prefetching adjacent emails must not silently mark them read \
             (pass ?mark_read=false on the GET)"
        );
    }

    #[test]
    fn app_js_cache_hit_detail_marks_read_on_server() {
        // Desktop counterpart to mobile_app_js_cache_hit_detail_marks_read_on_server
        // (roborev 302, fix 2): opening a prefetched (mark_read=false) email
        // from cache issues no GET, so nothing tells the server it's now read
        // unless loadEmailDetail's cache-hit branch does so explicitly.
        let start = APP_JS
            .find("async function loadEmailDetail")
            .expect("loadEmailDetail must exist");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}").expect("loadEmailDetail must close");
        let block = &rest[..end];
        assert!(
            block.contains("/mark-read"),
            "loadEmailDetail's cache-hit path must explicitly mark the email \
             read on the server"
        );
    }

    #[test]
    fn app_js_load_email_detail_never_adjusts_split_counts() {
        // roborev 303, fix 1: split-tab counts are presence counts —
        // compute_split_counts counts every matching email regardless of
        // read state, so only archive/trash/removal (membership changes)
        // should ever touch them. Marking an email read here, on either the
        // cache-hit or network path, must not call adjustSplitCounts —
        // mirroring toggleUnread, which never does either. The previous
        // wave's cache-hit branch wrongly called adjustSplitCounts(-1) (and
        // +1 on revert) with no mailbox/account guard, so a late revert
        // could corrupt another mailbox's counts.
        let start = APP_JS
            .find("async function loadEmailDetail")
            .expect("loadEmailDetail must exist");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}").expect("loadEmailDetail must close");
        let block = &rest[..end];
        assert!(
            !block.contains("adjustSplitCounts"),
            "loadEmailDetail must never adjust split-tab counts on mark-read"
        );
    }

    #[test]
    fn app_js_network_open_marks_email_read_locally() {
        // roborev 303, fix 2: get_email (src/routes.rs) fetches the email,
        // then marks it read as a side effect — the JSON it returns still
        // reflects the pre-mark isUnread state. Without a local flip,
        // emailCache and the list row are left stale, so a later cache-hit
        // reopen of loadEmailDetail sees isUnread=true and misfires the
        // mark-read POST it issues from the cache-hit branch above. Mirrors
        // mobile app.js's network path (renderScreenDetail).
        let start = APP_JS
            .find("async function loadEmailDetail")
            .expect("loadEmailDetail must exist");
        let rest = &APP_JS[start..];
        let fetch_pos = rest
            .find("const email = await api('GET', `/emails/${emailId}`);")
            .expect("loadEmailDetail's network path must fetch the email");
        let after_fetch = &rest[fetch_pos..];
        let end = after_fetch
            .find("} catch (err)")
            .expect("loadEmailDetail's network path must have a catch block");
        let block = &after_fetch[..end];
        assert!(
            block.contains("email.isUnread = false"),
            "loadEmailDetail's network path must flip isUnread on the cached email object"
        );
        assert!(
            block.contains("listItem.isUnread = false"),
            "loadEmailDetail's network path must flip isUnread on the matching list row"
        );
        // roborev 304: the flip alone leaves the row's unread styling stale —
        // returning to the list only toggles CSS classes — so the network
        // path must also re-render the list (guarded on the email actually
        // having been unread).
        assert!(
            block.contains("if (wasUnread) renderEmailList()"),
            "loadEmailDetail's network path must re-render the list after the flip"
        );
        // roborev 305: the guard must consider the ROW's pre-flip state too —
        // a stale-unread row whose email was read elsewhere comes back with
        // isUnread: false, and the row still needs its restyle.
        assert!(
            block.contains("email.isUnread || Boolean(listItem?.isUnread)"),
            "the re-render guard must capture both the email's and the row's pre-flip state"
        );
    }

    #[test]
    fn mobile_app_js_signature_prefilled_via_clear_compose_fields() {
        // Mirrors app_js_signature_prefilled_via_clear_compose: mobile's
        // startCompose/startReply/startForward all call clearComposeFields
        // first, so that's the single place to prefill from.
        let start = MOBILE_APP_JS
            .find("function clearComposeFields()")
            .expect("clearComposeFields must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("clearComposeFields must close");
        let block = &rest[..end];
        assert!(
            block.contains("composeSignaturePrefill()"),
            "clearComposeFields must prefill the body via composeSignaturePrefill()"
        );
        assert!(
            block.contains("setSelectionRange(0, 0)"),
            "clearComposeFields must place the cursor at position 0 after prefilling"
        );
        assert!(
            MOBILE_APP_JS.contains("function composeSignaturePrefill"),
            "composeSignaturePrefill helper must exist"
        );
        assert!(
            MOBILE_APP_JS.contains("-- \\n"),
            "signature prefill must use the RFC 3676 delimiter '-- ' \
             (trailing space significant) before a newline"
        );
    }

    #[test]
    fn mobile_app_js_cancel_dirty_check_uses_prefill_baseline() {
        // With a signature configured, clearComposeFields prefills the body
        // with "\n\n-- \n<sig>", so a dirty check based on
        // compose-body.value.trim() is ALWAYS truthy — plain open→Cancel
        // would falsely raise the Discard bar with zero typing. The body
        // dirty test must instead compare against the captured prefill
        // baseline.
        let start = MOBILE_APP_JS
            .find("function clearComposeFields()")
            .expect("clearComposeFields must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("clearComposeFields must close");
        let clear_block = &rest[..end];
        assert!(
            clear_block.contains("state.composeBaseline"),
            "clearComposeFields must capture the prefilled body as state.composeBaseline"
        );

        // The dirty check moved into composeDirty() (kata wm57: shared with
        // autosave) — cancelCompose now delegates to it. The baseline-compare
        // invariant lives there.
        let start = MOBILE_APP_JS
            .find("function composeDirty()")
            .expect("composeDirty must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("composeDirty must close");
        let dirty_block = &rest[..end];
        assert!(
            dirty_block.contains("!== state.composeBaseline"),
            "composeDirty's body-dirty test must compare against state.composeBaseline"
        );
        assert!(
            !dirty_block.contains("compose-body').value.trim()"),
            "composeDirty must not treat the signature prefill as dirty via value.trim()"
        );
        assert!(
            MOBILE_APP_JS.contains("if (composeDirty())"),
            "cancelCompose must delegate its dirty decision to composeDirty()"
        );
    }

    // =========================================================================
    // create_split / update_split validation tests (roborev 271)
    // =========================================================================

    #[tokio::test]
    async fn create_split_rejects_unknown_account() {
        let temp_dir = tempfile::tempdir().unwrap();
        let splits_path = temp_dir.path().join("splits.json");

        let mut state = test_state(&["known"], "known");
        state.splits_config_path = splits_path;

        let new_split = SplitInbox {
            id: "test-split".into(),
            name: "Test".into(),
            icon: None,
            filters: vec![],
            match_mode: Default::default(),
            account: Some("typo".into()),
        };

        let err = create_split(State(Arc::new(state)), Json(new_split))
            .await
            .err()
            .expect("create_split must reject a split tagged to an unknown account");

        assert!(
            matches!(err, Error::BadRequest(ref msg) if msg.contains("Unknown account")),
            "expected a BadRequest containing 'Unknown account', got {err:?}"
        );
    }

    #[tokio::test]
    async fn update_split_rejects_unknown_account() {
        let temp_dir = tempfile::tempdir().unwrap();
        let splits_path = temp_dir.path().join("splits.json");

        let mut state = test_state(&["known"], "known");
        state.splits_config_path = splits_path.clone();

        // Seed a split with id "a" and no account tag
        let existing_split = SplitInbox {
            id: "a".into(),
            name: "Original".into(),
            icon: None,
            filters: vec![],
            match_mode: Default::default(),
            account: None,
        };
        let config = SplitsConfig {
            splits: vec![existing_split],
        };
        splits::save_splits(&config, &splits_path).expect("failed to save seed splits");

        // Try to update it with account="typo"
        let updated = SplitInbox {
            id: "a".into(),
            name: "Updated".into(),
            icon: None,
            filters: vec![],
            match_mode: Default::default(),
            account: Some("typo".into()),
        };

        let err = update_split(State(Arc::new(state)), Path("a".into()), Json(updated))
            .await
            .err()
            .expect("update_split must reject a split tagged to an unknown account");

        assert!(
            matches!(err, Error::BadRequest(ref msg) if msg.contains("Unknown account")),
            "expected a BadRequest containing 'Unknown account', got {err:?}"
        );
    }

    #[tokio::test]
    async fn update_split_rejects_changed_id() {
        let temp_dir = tempfile::tempdir().unwrap();
        let splits_path = temp_dir.path().join("splits.json");

        let mut state = test_state(&["known"], "known");
        state.splits_config_path = splits_path.clone();

        // Seed a split with id "a"
        let existing_split = SplitInbox {
            id: "a".into(),
            name: "Original".into(),
            icon: None,
            filters: vec![],
            match_mode: Default::default(),
            account: None,
        };
        let config = SplitsConfig {
            splits: vec![existing_split],
        };
        splits::save_splits(&config, &splits_path).expect("failed to save seed splits");

        // Try to update it with a different id "b"
        let updated = SplitInbox {
            id: "b".into(),
            name: "Updated".into(),
            icon: None,
            filters: vec![],
            match_mode: Default::default(),
            account: None,
        };

        let err = update_split(State(Arc::new(state)), Path("a".into()), Json(updated))
            .await
            .err()
            .expect("update_split must reject a body id that differs from the path id");

        assert!(
            matches!(err, Error::BadRequest(ref msg) if msg.contains("immutable")),
            "expected a BadRequest containing 'immutable', got {err:?}"
        );
    }

    #[tokio::test]
    async fn update_split_without_account_untags() {
        let temp_dir = tempfile::tempdir().unwrap();
        let splits_path = temp_dir.path().join("splits.json");

        let mut state = test_state(&["known"], "known");
        state.splits_config_path = splits_path.clone();

        // Seed a split with id "a" and account="known"
        let existing_split = SplitInbox {
            id: "a".into(),
            name: "Original".into(),
            icon: None,
            filters: vec![],
            match_mode: Default::default(),
            account: Some("known".into()),
        };
        let config = SplitsConfig {
            splits: vec![existing_split],
        };
        splits::save_splits(&config, &splits_path).expect("failed to save seed splits");

        // Update it with account=None (PUT replaces everything)
        let updated = SplitInbox {
            id: "a".into(),
            name: "Updated".into(),
            icon: None,
            filters: vec![],
            match_mode: Default::default(),
            account: None,
        };

        update_split(State(Arc::new(state)), Path("a".into()), Json(updated))
            .await
            .expect("update_split must succeed when account field is present and valid");

        // Verify the stored split has account=None
        let reloaded = splits::load_splits(&splits_path, None);
        assert_eq!(reloaded.splits.len(), 1);
        assert_eq!(reloaded.splits[0].id, "a");
        assert_eq!(
            reloaded.splits[0].account, None,
            "account field must be None after PUT without account"
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

    #[tokio::test]
    async fn api_js_serves_shared_client() {
        let resp = api_js().await.into_response();
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
            text.contains("class ApiAuthError extends ApiError"),
            "api.js must define the auth-vs-network error taxonomy"
        );
        assert!(
            text.contains("function makeApi("),
            "api.js must define makeApi()"
        );
    }

    #[test]
    fn shared_api_js_loaded_by_both_bundles() {
        assert!(
            INDEX_HTML.contains(r#"<script src="api.js"></script>"#),
            "desktop index.html must load the shared api.js before app.js"
        );
        assert!(
            MOBILE_HTML.contains(r#"<script src="/api.js"></script>"#),
            "mobile index.html must load the shared api.js before the app module"
        );
        assert!(
            APP_JS.contains("makeApi("),
            "desktop app.js must delegate to the shared makeApi client"
        );
        assert!(
            MOBILE_APP_JS.contains("makeApi("),
            "mobile app.js must delegate to the shared makeApi client"
        );
    }

    #[test]
    fn mobile_no_direct_jmap_and_no_stored_token() {
        assert!(
            !MOBILE_APP_JS.contains("api.fastmail.com"),
            "mobile must talk to the server API, never directly to Fastmail"
        );
        assert!(
            !MOBILE_SW.contains("api.fastmail.com"),
            "service worker must not special-case Fastmail — the API is same-origin now"
        );
        assert!(
            !MOBILE_APP_JS.contains("setItem('supervillain_session'"),
            "mobile must never store a bearer token in localStorage"
        );
        assert!(
            MOBILE_APP_JS.contains("removeItem('supervillain_session')"),
            "mobile must scrub the pre-rewire bearer token off installed PWAs"
        );
        assert!(
            !MOBILE_HTML.contains("login-token"),
            "the Fastmail-token login screen must not exist"
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
        let cache_control = resp
            .headers()
            .get("cache-control")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(
            cache_control, "no-cache",
            "sw.js must not be cached itself, or version-busting can never reach the client"
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(
            text.contains("url.pathname.startsWith('/api/')"),
            "SW should exclude server API calls from caching"
        );
        assert!(
            text.contains("resp.ok"),
            "SW should only cache successful responses"
        );
        assert!(
            text.contains("/mobile/index.html"),
            "served SW app shell should include /mobile/index.html"
        );
    }

    #[tokio::test]
    async fn mobile_sw_cache_name_embeds_crate_version() {
        let resp = mobile_sw().await.into_response();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(
            text.contains(env!("CARGO_PKG_VERSION")),
            "served sw.js should embed the crate version in CACHE_NAME"
        );
        assert!(
            text.contains(env!("SUPERVILLAIN_BUILD_ID")),
            "served sw.js should embed the per-build id in CACHE_NAME so consecutive \
             deploys on the same crate version still get a fresh cache"
        );
        assert!(
            !text.contains("__SUPERVILLAIN_VERSION__"),
            "served sw.js must not leak the unreplaced version placeholder"
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

    #[test]
    fn shared_api_js_handles_auth_errors() {
        // 401 and 403 must map to ApiAuthError, not plain ApiError
        assert!(
            API_JS.contains("resp.status === 401 || resp.status === 403"),
            "api.js should classify 401/403 as auth errors"
        );
        assert!(
            API_JS.contains("throw new ApiAuthError"),
            "api.js should throw ApiAuthError on auth failure"
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
            !text.contains("jmap.js"),
            "mobile app.js must not reference the deleted direct-JMAP module"
        );
        assert!(
            text.contains("apiGlobal('GET', '/accounts')"),
            "mobile app.js should load accounts from the server API"
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
    fn mobile_app_js_guards_service_worker_registration() {
        assert!(
            MOBILE_APP_JS.contains("isSecureContext"),
            "SW registration should skip on non-secure contexts (plain http off localhost)"
        );
        let start = MOBILE_APP_JS
            .find("navigator.serviceWorker.register(")
            .expect("mobile app.js should register the service worker");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("SW registration block must close");
        let block = &rest[..end];
        assert!(
            !block.contains(".catch(() => {})"),
            "SW registration must not silently swallow errors with an empty catch — \
             errors should be surfaced via console.warn"
        );
        assert!(
            block.contains("console.warn"),
            "SW registration failure should be logged via console.warn"
        );
    }

    #[test]
    fn mobile_app_js_has_pull_to_refresh() {
        assert!(
            MOBILE_APP_JS.contains("pullToRefreshRecognizer"),
            "app.js should implement pull-to-refresh as a gesture recognizer"
        );
        assert!(
            MOBILE_APP_JS.contains("touchstart"),
            "pull-to-refresh should use touchstart events"
        );
        assert!(
            MOBILE_APP_JS.contains("touchend"),
            "pull-to-refresh should use touchend events"
        );
        assert!(
            MOBILE_APP_JS.contains("Refreshing..."),
            "pull-to-refresh should trigger a refresh once past the threshold"
        );
    }

    #[test]
    fn mobile_app_js_single_touch_controller() {
        // One controller owns every touch gesture; A4's row-swipe recognizer
        // plugs into it rather than registering a second listener set.
        let touchstart_listeners = MOBILE_APP_JS
            .matches("addEventListener('touchstart'")
            .count();
        assert_eq!(
            touchstart_listeners, 1,
            "exactly one gesture controller must own touchstart (found {touchstart_listeners})"
        );
        assert!(
            MOBILE_APP_JS.contains("gestureController"),
            "touch handling should live in a single gesture controller"
        );
    }

    #[test]
    fn mobile_app_js_setscreen_owns_all_display_toggles() {
        assert!(
            MOBILE_APP_JS.contains("function setScreen("),
            "navigation must funnel through a single setScreen()"
        );
        assert!(
            !MOBILE_APP_JS.contains("currentView"),
            "state.currentView must be replaced by state.screen everywhere"
        );
        assert!(
            MOBILE_APP_JS.contains("state.screen"),
            "screen state should live on state.screen"
        );
        // Every show/hide toggle must live inside setScreen so screens can't
        // drift out of sync — bound the function body the way
        // mobile_app_js_guards_service_worker_registration bounds blocks.
        let start = MOBILE_APP_JS
            .find("function setScreen(")
            .expect("setScreen must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("setScreen body must close");
        let region = &rest[..end];
        let total = MOBILE_APP_JS.matches(".style.display").count();
        let inside = region.matches(".style.display").count();
        assert!(inside > 0, "setScreen should own the display toggles");
        assert_eq!(
            total, inside,
            "all .style.display toggles must live inside setScreen ({inside} of {total} do)"
        );
        // The third screen (compose, kata ryzd) must be shown/hidden here too,
        // never via a scattered toggle elsewhere.
        assert!(
            region.contains("compose-screen"),
            "the compose screen's show/hide must live inside setScreen's switch"
        );
        // The mailbox bottom nav (kata 1wdy) is LIST-only chrome — its
        // show/hide must live here too, not a scattered toggle elsewhere.
        assert!(
            region.contains("bottom-nav"),
            "the bottom nav's show/hide must live inside setScreen's switch"
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
    fn mobile_app_js_has_email_actions() {
        // Archive/trash/read/star all go through existing server routes
        // with optimistic updates (mirrors desktop's emailAction/
        // toggleUnread/toggleFlag) — kata 6kx8 (task A4).
        for route in ["/archive", "/trash", "/mark-unread", "/toggle-flag"] {
            assert!(
                MOBILE_APP_JS.contains(route),
                "app.js should call the {route} route"
            );
        }
        assert!(
            MOBILE_APP_JS.contains("undoStack"),
            "archive/trash should push onto an undo stack for a later undo (A5)"
        );
    }

    #[test]
    fn mobile_app_js_has_row_swipe_recognizer() {
        // The row-swipe gesture must plug into the existing single-touch
        // controller (kata 6kx8, task A4) rather than adding a second
        // listener set — re-assert the invariant from
        // mobile_app_js_single_touch_controller alongside the new
        // recognizer so a regression here fails both tests together.
        assert!(
            MOBILE_APP_JS.contains("rowSwipeRecognizer"),
            "app.js should implement row-swipe as a gesture recognizer"
        );
        assert!(
            MOBILE_APP_JS.contains("[pullToRefreshRecognizer, rowSwipeRecognizer]"),
            "the row-swipe recognizer must be registered in gestureController.recognizers"
        );
        let touchstart_listeners = MOBILE_APP_JS
            .matches("addEventListener('touchstart'")
            .count();
        assert_eq!(
            touchstart_listeners, 1,
            "row-swipe must not add its own touchstart listener (found {touchstart_listeners})"
        );
    }

    #[test]
    fn mobile_app_js_has_undo_toast() {
        // A5 (kata ga3w): archive/trash pushes onto A4's undo stack, then
        // surfaces a tappable toast so the user can reverse the action —
        // adapted from desktop's pushUndo/performUndo (static/app.js).
        assert!(
            MOBILE_APP_JS.contains("function performUndo("),
            "app.js should implement performUndo to pop the undo stack and restore the email"
        );
        assert!(
            MOBILE_APP_JS.contains("undo-toast"),
            "app.js should reference the undo-toast element"
        );
    }

    #[test]
    fn mobile_app_js_undo_awaits_action_settled() {
        // A fast Undo tap must not race the original archive/trash request:
        // the entry records the action's settlement and performUndo awaits
        // it before issuing the move-back, otherwise out-of-order completion
        // can leave the email archived despite the undo.
        assert!(
            MOBILE_APP_JS.contains("undoEntry.settled"),
            "emailAction should record the action promise's settlement on the undo entry"
        );
        assert!(
            MOBILE_APP_JS.contains("await entry.settled"),
            "performUndo must await the original action's settlement before the move-back"
        );
    }

    #[test]
    fn mobile_app_js_undo_repush_respects_cap() {
        // Both places that push onto the undo stack (pushUndo and
        // performUndo's failure re-push) must enforce UNDO_STACK_LIMIT
        // through the shared helper — a bare push would bypass the cap.
        let calls = MOBILE_APP_JS.matches("capUndoStack(").count();
        assert!(
            calls >= 3,
            "capUndoStack should be defined and called from every stack push site (found {calls} occurrences)"
        );
    }

    #[test]
    fn mobile_app_js_undo_gates_reinsert_on_current_mailbox() {
        // roborev 288: archive in Inbox, switch to Archive, tap Undo must not
        // splice the email into the Archive list at a stale index — the
        // optimistic local re-insert (and its failure-path revert) is gated
        // on still being on the mailbox the email was archived/trashed from;
        // only the server move-back happens unconditionally.
        let start = MOBILE_APP_JS
            .find("async function performUndo(")
            .expect("performUndo must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("performUndo must close");
        let block = &rest[..end];
        assert!(
            block.contains("state.currentMailbox?.id === entry.mailboxId"),
            "performUndo must gate the optimistic re-insert on the entry's origin mailbox"
        );
        let matches = block.matches("sameMailbox").count();
        assert!(
            matches >= 3,
            "sameMailbox must guard both the optimistic re-insert and its \
             failure-path revert (found {matches} references)"
        );
    }

    #[test]
    fn mobile_app_js_mailbox_and_split_switches_hide_undo_toast() {
        // roborev 288: an undo toast left over from a different mailbox/split
        // invites tapping Undo into a list it no longer describes — every
        // list-switch entry point must hide it, mirroring the abortListLoad()
        // guard those same functions already carry (kata 1wdy).
        for func in [
            "function selectAccount(",
            "function selectMailbox(",
            "function selectSplit(",
        ] {
            let start = MOBILE_APP_JS.find(func).expect("function must exist");
            let rest = &MOBILE_APP_JS[start..];
            let end = rest.find("\n}").expect("function must close");
            assert!(
                rest[..end].contains("hideUndoToast(undoToastEntry)"),
                "{func} must hide any pending undo toast on switch"
            );
        }
    }

    #[test]
    fn mobile_html_has_undo_toast() {
        assert!(
            MOBILE_HTML.contains(r#"id="undo-toast""#),
            "mobile index.html should define the undo-toast element"
        );
        // Both toasts live in one fixed flex stack (undo above error) so a
        // multi-line error toast can never overlap the undo toast.
        let stack = MOBILE_HTML
            .find(r#"id="toast-stack""#)
            .expect("mobile index.html should define the toast-stack container");
        let undo = MOBILE_HTML
            .find(r#"id="undo-toast""#)
            .expect("undo-toast must exist");
        let error = MOBILE_HTML
            .find(r#"id="error-toast""#)
            .expect("error-toast must exist");
        assert!(
            stack < undo && undo < error,
            "toast-stack must contain undo-toast above error-toast (stack={stack}, undo={undo}, error={error})"
        );
    }

    #[test]
    fn mobile_sw_caches_app_shell() {
        assert!(
            MOBILE_SW.contains("/mobile/app.js"),
            "service worker should cache app.js in app shell"
        );
        assert!(
            MOBILE_SW.contains("'/api.js'"),
            "service worker should cache the shared api.js in app shell"
        );
        assert!(
            MOBILE_SW.contains("/mobile/index.html"),
            "service worker should cache the /mobile/index.html alias in app shell"
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

    #[test]
    fn mobile_html_has_detail_action_bar() {
        // Detail-view archive/trash/read/star buttons (kata 6kx8, task A4).
        for id in [
            "detail-archive-btn",
            "detail-trash-btn",
            "detail-read-btn",
            "detail-star-btn",
        ] {
            assert!(
                MOBILE_HTML.contains(id),
                "detail action bar should include a #{id} button"
            );
        }
    }

    // =========================================================================
    // Compose tests (kata ryzd, task A6): new / reply / reply-all / forward
    // =========================================================================

    #[test]
    fn mobile_app_js_has_compose_screen() {
        // Screen.COMPOSE is the third full-screen view — a new enum member
        // plus one setScreen case, no scattered display toggles.
        assert!(
            MOBILE_APP_JS.contains("COMPOSE"),
            "the Screen enum should include a COMPOSE member"
        );
        assert!(
            MOBILE_APP_JS.contains("Screen.COMPOSE"),
            "compose navigation should route through Screen.COMPOSE"
        );
        // The verified send contract: POST /emails/send with reply threading.
        assert!(
            MOBILE_APP_JS.contains("/emails/send"),
            "compose should POST to the /emails/send route"
        );
        assert!(
            MOBILE_APP_JS.contains("in_reply_to"),
            "reply must thread the original via in_reply_to"
        );
        assert!(
            MOBILE_APP_JS.contains("from_address"),
            "compose should send the selected identity as from_address"
        );
    }

    #[test]
    fn mobile_app_js_has_compose_prefill_modes() {
        // All four entry modes mirror desktop's compose semantics
        // (startReply covers reply + reply-all, startForward covers forward,
        // startCompose the blank new message).
        assert!(
            MOBILE_APP_JS.contains("function startReply("),
            "compose should implement startReply (reply + reply-all)"
        );
        assert!(
            MOBILE_APP_JS.contains("function startForward("),
            "compose should implement startForward"
        );
        assert!(
            MOBILE_APP_JS.contains("function startCompose("),
            "compose should implement startCompose for a blank new message"
        );
        assert!(
            MOBILE_APP_JS.contains("autoSelectFromAddress"),
            "compose should auto-select the From identity from the original's recipients"
        );
        assert!(
            MOBILE_APP_JS.contains("/identities"),
            "compose should load identities for the From selector"
        );
    }

    #[test]
    fn mobile_html_has_compose_screen() {
        // Compose markup: the screen container, To/Subject inputs, and the
        // Send button, plus the header compose (new message) entry point.
        for id in [
            "compose-screen",
            "compose-to",
            "compose-subject",
            "compose-send-btn",
            "compose-btn",
        ] {
            assert!(
                MOBILE_HTML.contains(id),
                "mobile compose markup should include #{id}"
            );
        }
    }

    #[test]
    fn mobile_app_js_compose_send_guards_stale_completion() {
        // A send resolving after the user browser-backed out of compose must
        // not fire a second history.back() (popping detail→list or out of the
        // app) or a stray "Sent" toast — the success path is gated on the
        // compose screen still being active (review follow-up on kata ryzd).
        // The screen check alone isn't sufficient: backing out and
        // immediately starting a NEW draft also lands back on
        // Screen.COMPOSE, so a monotonic composeSession token (bumped by
        // startCompose/startReply/startForward) captured before the await
        // must additionally still match before the stale completion touches
        // that new draft (roborev 288). The failure-path showError is the
        // one thing deliberately NOT session-gated: a failed send is a lost
        // email and must surface even after the user moved on (A6
        // re-review: failure-after-leave must surface).
        let start = MOBILE_APP_JS
            .find("async function doSendComposedEmail(")
            .expect("doSendComposedEmail must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("doSendComposedEmail must close");
        let block = &rest[..end];
        assert!(
            block.contains("state.screen === Screen.COMPOSE"),
            "sendComposedEmail's post-await paths must check the compose \
             screen is still active before touching history"
        );
        assert!(
            block.contains("const session = sendingSession"),
            "sendComposedEmail must take its session token from the \
             wrapper-set sendingSession (synchronous with send initiation), \
             not a state.composeSession re-read that can drift across the \
             settle await (roborev 320)"
        );
        assert!(
            block.contains("state.composeSession === session"),
            "the success-path clear/back must require composeSession to \
             still match"
        );
        let catch_start = block.find("} catch (err)").expect("send must have a catch");
        let catch_block = &block[catch_start..];
        assert!(
            catch_block.contains("showError('Send', err)"),
            "the failure path must surface the send error"
        );
        assert!(
            !catch_block.contains("state.composeSession === session"),
            "showError on failure must be UNCONDITIONAL — gating it on the \
             session would silently swallow a lost email after the user \
             starts a new draft"
        );
    }

    #[test]
    fn app_js_send_guards_stale_completion() {
        // Desktop mirror of the mobile contract above (roborev 315): a send
        // resolving after the user Escaped out of compose must not clear or
        // navigate a compose it no longer owns. Escape's leave path runs
        // `flushAutosave(); clearCompose(); showView('list')` with no
        // state.sending gate, so a slow send can still be in flight when the
        // user moves on — clearCompose bumps composeSession, and the
        // completion must check the token it captured before the await.
        let block = js_fn_body(APP_JS, "async function doSendEmail(");
        assert!(
            block.contains("const session = sendingSession"),
            "doSendEmail must take its session token from the wrapper-set \
             sendingSession (synchronous with send initiation), not a \
             state.composeSession re-read that can drift across the settle \
             await (roborev 320)"
        );
        assert!(
            block.matches("state.composeSession === session").count() >= 2,
            "both success paths (invite and regular send) must gate their \
             clear/navigate on composeSession still matching"
        );
        // The failure paths are the one thing deliberately NOT session-gated
        // (same contract as mobile): a failed send is a lost email and must
        // surface even after the user moved on.
        assert_eq!(
            block.matches("} catch (err)").count(),
            2,
            "doSendEmail should have exactly its invite and send catches"
        );
        for (i, _) in block.match_indices("} catch (err)") {
            // 300 chars spans the catch's comment + its showStatus line but
            // stays well short of the next success path's session gate.
            let body = &block[i..(i + 300).min(block.len())];
            assert!(
                body.contains("showStatus("),
                "each send catch must surface the failure"
            );
            assert!(
                !body.contains("composeSession"),
                "failure toasts must be unconditional, never session-gated"
            );
        }
    }

    #[test]
    fn send_deletes_the_draft_it_captured_not_live_state() {
        // Both bundles (roborev 315): at completion time state.draftId may
        // belong to someone else entirely — the user can leave compose
        // mid-send (nulling it, so the just-sent mail's autosaved draft
        // ghosts in Drafts forever) or open a DIFFERENT draft from the
        // Drafts mailbox (a live read would DELETE that draft — real data
        // loss; it passes the server's is-a-draft check). The send must
        // capture the id it owns once — after the in-flight autosave settle
        // adopts the final id — and delete exactly that id on success.
        for (bundle, src, decl) in [
            ("app.js", APP_JS, "async function doSendEmail("),
            (
                "mobile/app.js",
                MOBILE_APP_JS,
                "async function doSendComposedEmail(",
            ),
        ] {
            let block = js_fn_body(src, decl);
            let settle = block
                .find("await saveInFlight")
                .unwrap_or_else(|| panic!("{bundle}: send must settle saveInFlight first"));
            let capture = block
                .find("const draftId = state.draftId")
                .unwrap_or_else(|| panic!("{bundle}: send must capture the draft id it owns"));
            assert!(
                capture > settle,
                "{bundle}: the capture must come after the saveInFlight settle, \
                 when the adopted id is final"
            );
            assert!(
                block.contains("deleteDraftById(draftId)"),
                "{bundle}: send success must delete the CAPTURED id"
            );
            assert!(
                !block.contains("deleteTrackedDraft()"),
                "{bundle}: the send path must not read live draft state at \
                 completion time"
            );
        }
    }

    #[test]
    fn send_skips_delete_when_its_draft_was_reopened() {
        // Counterpart edge to the captured-id delete above (roborev 316):
        // the restore paths (openDraftInCompose / startDraftCompose) adopt
        // the EXISTING draft id rather than POSTing a fresh one. So: send
        // draft X, leave mid-send, reopen X from the Drafts mailbox, send
        // resolves — an unconditional delete of the captured id would yank
        // the draft out from under the active editor AND leave
        // trackedDraftId pointing at a dead id, so every later autosave
        // PUTs a 404 (console.warn only) and the next leave-compose wipes
        // the only copy. The success path must skip the delete when a newer
        // session has recaptured that very id; a live-but-already-sent
        // draft is strictly safer than deleting content under an editor.
        //
        // The guard must read the module-level trackedDraftId/
        // trackedDraftSession pair — which clearCompose/clearComposeFields
        // deliberately never touch — NOT the point-in-time state.draftId: a
        // snapshot guard misses reopen → leave-again before the stale send
        // resolves (state.draftId is nulled by then) and would delete the
        // reopened draft out from under its still-live tracking
        // (roborev 317). Persisting the post-reopen edits themselves is the
        // session-scoped sending gate's job (roborev 318, see
        // autosave_gate_is_scoped_to_the_sending_session below). Desktop
        // has TWO delete sites (invite and regular send); each needs its
        // own guard.
        for (bundle, src, decl, sites) in [
            ("app.js", APP_JS, "async function doSendEmail(", 2usize),
            (
                "mobile/app.js",
                MOBILE_APP_JS,
                "async function doSendComposedEmail(",
                1,
            ),
        ] {
            let block = js_fn_body(src, decl);
            let guard = "trackedDraftSession !== session && trackedDraftId === draftId";
            assert!(
                block.matches(guard).count() >= sites,
                "{bundle}: all {sites} delete site(s) must skip when a newer \
                 compose session has recaptured the captured id, via the \
                 leave-surviving tracked pair"
            );
            assert!(
                !block.contains("state.draftId === draftId"),
                "{bundle}: the recapture guard must not read the \
                 point-in-time state.draftId — it is nulled by leave-compose"
            );
        }
    }

    #[test]
    fn send_snapshots_payload_before_the_settle_await() {
        // roborev 320: the settle await can block >3s and the leave paths
        // have no sending gate, so the user can Escape (or browser-back) and
        // reopen another draft before the send body resumes. Everything the
        // send POSTs — and the session token its completion gates compare
        // against — must be snapshotted synchronously before that await;
        // only draftId waits for the settle (it needs the in-flight save's
        // adopted id). Reading live state afterward sent the NEW compose's
        // fields, passed the completion gates as their owner, and deleted
        // the reopened draft.
        for (bundle, src, decl, pre_reads, forbidden) in [
            (
                "app.js",
                APP_JS,
                "async function doSendEmail(",
                // els.invite covers the invite snapshot too (roborev 321):
                // those fields don't match the els.compose prefix, so
                // without it a live els.invite*.value read drifting back
                // below the settle would pass this test.
                &[
                    "const session = sendingSession",
                    "els.compose",
                    "els.invite",
                    "state.replyContext",
                ][..],
                &[
                    "els.compose",
                    "els.invite",
                    "state.replyContext",
                    "state.pendingAttachments",
                ][..],
            ),
            (
                "mobile/app.js",
                MOBILE_APP_JS,
                "async function doSendComposedEmail(",
                &[
                    "const session = sendingSession",
                    "composeEl(",
                    "state.replyContext",
                ][..],
                &[
                    "composeEl(",
                    "state.replyContext",
                    "state.pendingAttachments",
                ][..],
            ),
        ] {
            let block = js_fn_body(src, decl);
            let settle = block
                .find("await saveInFlight")
                .unwrap_or_else(|| panic!("{bundle}: send must settle saveInFlight"));
            for needle in pre_reads {
                let pos = block
                    .find(needle)
                    .unwrap_or_else(|| panic!("{bundle}: send must read {needle}"));
                assert!(
                    pos < settle,
                    "{bundle}: {needle} must be snapshotted before the settle await"
                );
            }
            let after = &block[settle..];
            for live_read in forbidden {
                assert!(
                    !after.contains(live_read),
                    "{bundle}: no live {live_read} read may follow the settle \
                     await — the compose may belong to someone else by then"
                );
            }
        }
    }

    #[test]
    fn compose_locks_while_its_send_is_in_flight() {
        // roborev 321: the payload is snapshotted at send initiation (see
        // send_snapshots_payload_before_the_settle_await), so anything typed
        // into the SENDING compose afterward would be silently discarded —
        // the session-scoped gate skips its autosaves, the scoped cancel
        // kills its re-armed timer, and success clears the editor under a
        // "Sent!" toast. Lock the compose surface for the duration so
        // mid-send edits are impossible rather than invisible. Unlock on
        // the failure path (a failed send must stay editable for retry) and
        // on every new/restored compose — one can start while an old slow
        // send is still in flight and must never inherit the lock.
        for (bundle, src) in [("app.js", APP_JS), ("mobile/app.js", MOBILE_APP_JS)] {
            assert!(
                src.contains("function setComposeLocked("),
                "{bundle} must implement the compose send-lock"
            );
        }
        let wrapper = js_fn_body(APP_JS, "async function sendEmail(");
        assert!(
            wrapper.contains("setComposeLocked(true)"),
            "desktop's send wrapper must lock the compose at send start"
        );
        // Positional (roborev 322): the unlock must live in the FINALLY —
        // moved to the success path only, a failed send would stay locked
        // forever with no retry possible.
        let finally_pos = wrapper
            .find("finally {")
            .expect("sendEmail must have a finally");
        let unlock_pos = wrapper
            .find("setComposeLocked(false)")
            .expect("desktop's send wrapper must unlock when the send settles");
        assert!(
            unlock_pos > finally_pos,
            "the wrapper's unlock must be inside finally, not the success path"
        );
        // The DOM lock can't constrain the non-form attachment routes
        // (roborev 322): dropping a file on the compose view and pasting an
        // image both still fire mid-send (paste events fire on a readOnly
        // textarea), funneling into addFiles — the attachment would upload,
        // render, then be aborted by clearCompose under the "Sent!" toast,
        // the exact silent-discard class the lock exists to close. The
        // remove buttons are the inverse illusion: "removing" an attachment
        // the snapshotted send still carries. Both must check the lock.
        for entry in ["function addFiles(", "function handleAttachmentListClick("] {
            assert!(
                js_fn_body(APP_JS, entry).contains("composeSendLocked()"),
                "{entry} must refuse while the active compose's send is in flight"
            );
        }
        // Mobile's add paths are fully covered by its DOM lock (button +
        // file input disabled; no drop/paste routes), but the attachment
        // chips' ✕ buttons stay tappable mid-send — the same
        // inverse-illusion as desktop's remove buttons (roborev 323).
        assert!(
            js_fn_body(MOBILE_APP_JS, "function handleComposeAttachmentListClick(")
                .contains("composeSendLocked()"),
            "mobile's chip-remove handler must refuse while the active \
             compose's send is in flight"
        );
        assert!(
            js_fn_body(APP_JS, "function clearCompose(").contains("setComposeLocked(false)"),
            "clearCompose must unlock — a new compose during a slow send \
             must not inherit the lock"
        );
        assert!(
            js_fn_body(MOBILE_APP_JS, "function setComposeSending(")
                .contains("setComposeLocked(sending)"),
            "mobile's sending hook must drive the lock"
        );
        assert!(
            js_fn_body(MOBILE_APP_JS, "function clearComposeFields(")
                .contains("setComposeLocked(false)"),
            "clearComposeFields must unlock — a new compose during a slow \
             send must not inherit the lock"
        );
    }

    #[test]
    fn autosave_gate_is_scoped_to_the_sending_session() {
        // roborev 318: state.sending gates runAutosave so the compose being
        // sent is never re-saved mid-send (it's about to stop being a draft;
        // a late save would ghost a copy in Drafts). But the gate was
        // GLOBAL: a compose the user reopened or started fresh while an
        // unrelated slow send was in flight couldn't persist anything —
        // its debounced saves were skipped and the leave-flush no-op'd on
        // the same gate right before clearCompose wiped the editor: silent
        // data loss. Both bundles must scope the skip to the session that
        // initiated the send; the tracked-pair recapture guard above then
        // keeps the send's completion from deleting whatever id those
        // mid-send saves are tracking.
        for (bundle, src, send_decl) in [
            ("app.js", APP_JS, "async function doSendEmail("),
            (
                "mobile/app.js",
                MOBILE_APP_JS,
                "async function doSendComposedEmail(",
            ),
        ] {
            let block = js_fn_body(src, "async function runAutosave(");
            assert!(
                block.contains("state.sending && state.composeSession === sendingSession"),
                "{bundle}: runAutosave must skip only the sending session's saves"
            );
            assert!(
                src.contains("sendingSession = state.composeSession"),
                "{bundle}: the send wrapper must record which session it is sending"
            );
            assert!(
                src.contains("sendingSession = null"),
                "{bundle}: the send wrapper must clear the sending session when done"
            );
            // The send path's post-settle cancelAutosave must be scoped the
            // same way (roborev 319): a timer alive after the settle await
            // can belong to a compose the user reopened or started fresh
            // DURING that await (leave-compose flushes the old session's
            // timer first, so a surviving timer is the current session's),
            // and killing it silently drops that compose's last edits at
            // the next leave — through the timer instead of the gate.
            let send_block = js_fn_body(src, send_decl);
            assert!(
                send_block
                    .contains("if (state.composeSession === sendingSession) cancelAutosave()"),
                "{bundle}: the post-settle cancel must only kill the sending \
                 session's timer"
            );
        }
    }

    #[test]
    fn mobile_html_has_compose_reply_actions() {
        // Reply / reply-all / forward entry points on the detail action bar.
        for id in [
            "detail-reply-btn",
            "detail-reply-all-btn",
            "detail-forward-btn",
        ] {
            assert!(
                MOBILE_HTML.contains(id),
                "detail view should include a #{id} compose action"
            );
        }
    }

    // =========================================================================
    // Attachment tests (kata 0g9v): detail-view polish + compose sending
    // =========================================================================

    #[test]
    fn mobile_app_js_uploads_attachments_with_explicit_account_param() {
        // Mobile posts raw bytes to /api/upload — state.api() JSON-encodes
        // bodies, so it can't carry a binary File. This bypasses it, and
        // (like attachmentUrl()) must append ?account= explicitly since
        // there's no implicit per-tab session the way desktop's bare-URL xhr
        // assumes (a known, intentionally-not-copied desktop gap).
        assert!(
            MOBILE_APP_JS.contains("/api/upload"),
            "mobile app.js should call the upload endpoint"
        );
        let start = MOBILE_APP_JS
            .find("/api/upload")
            .expect("upload endpoint reference must exist");
        let region = &MOBILE_APP_JS[start..start + 200];
        assert!(
            region.contains("account="),
            "the upload URL must explicitly append ?account= (region: {region})"
        );
        assert!(
            MOBILE_APP_JS.contains("X-Filename"),
            "upload request should set X-Filename like desktop's uploadAttachment"
        );
    }

    #[test]
    fn mobile_app_js_tracks_pending_attachments() {
        assert!(
            MOBILE_APP_JS.contains("pendingAttachments"),
            "mobile app.js should track compose attachments on state.pendingAttachments"
        );
        assert!(
            MOBILE_APP_JS.contains("'uploading'"),
            "pending attachments should carry an uploading status"
        );
        assert!(
            MOBILE_APP_JS.contains("'ready'"),
            "pending attachments should carry a ready status"
        );
        assert!(
            MOBILE_APP_JS.contains("'error'"),
            "pending attachments should carry an error status"
        );
    }

    #[test]
    fn mobile_app_js_blocks_send_during_upload() {
        let start = MOBILE_APP_JS
            .find("async function doSendComposedEmail(")
            .expect("doSendComposedEmail must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("doSendComposedEmail must close");
        let block = &rest[..end];
        assert!(
            block.contains("status === 'uploading'"),
            "send should block while any attachment is still uploading"
        );
    }

    #[test]
    fn mobile_app_js_send_includes_ready_attachments() {
        let start = MOBILE_APP_JS
            .find("async function doSendComposedEmail(")
            .expect("doSendComposedEmail must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("doSendComposedEmail must close");
        let block = &rest[..end];
        assert!(
            block.contains("blob_id") && block.contains("mime_type"),
            "send payload should map ready attachments to {{blob_id, name, mime_type, size}}"
        );
        assert!(
            block.contains("undefined"),
            "attachments should be omitted (undefined) rather than sent as an empty array"
        );
    }

    // =========================================================================
    // Stale-snapshot revalidation + send feedback (cold-open / send-stall fix)
    // =========================================================================

    /// Slice a JS function's body out of a bundle: from its declaration to
    /// the first column-0 closing brace. A column-0 `}` inside a template
    /// literal would silently shrink the slice — true of every copy of this
    /// idiom in these tests; the helper centralizes that assumption so it
    /// can rot in exactly one place (roborev 311).
    fn js_fn_body<'a>(src: &'a str, decl: &str) -> &'a str {
        let start = src
            .find(decl)
            .unwrap_or_else(|| panic!("{decl} must exist"));
        let rest = &src[start..];
        let end = rest
            .find("\n}")
            .unwrap_or_else(|| panic!("{decl} must close"));
        &rest[..end]
    }

    #[test]
    fn load_emails_repolls_disk_stale_lists() {
        // Contract: list_emails tags a disk-restored (stale) cached list
        // with x-supervillain-stale: 1; loadEmails must read that header
        // (via the withMeta api variant) and re-poll until the warmer has
        // replaced the entry, instead of leaving yesterday's mail on screen.
        assert!(
            API_JS.contains("api.withMeta = request"),
            "api.js must expose the withMeta variant that surfaces response headers"
        );
        assert!(
            APP_JS.contains("x-supervillain-stale"),
            "loadEmails must check the stale-snapshot response header"
        );
        assert!(
            APP_JS.contains("function scheduleStaleRevalidate("),
            "app.js must schedule bounded re-polls while the list is stale"
        );
        assert!(
            APP_JS.contains("STALE_REVALIDATE_MAX"),
            "the stale re-poll loop must be bounded"
        );
        // The poll must not fight the user: identical payloads (warmer not
        // done yet) skip the re-render that would reset the selection.
        let block = js_fn_body(APP_JS, "async function loadEmails(");
        assert!(
            block.matches("emailListsEqual").count() >= 2,
            "loadEmails must skip BOTH renders of an unchanged payload during \
             the stale re-poll loop — the eager cache paint AND the post-fetch \
             render; either alone resets the selection to row 0 every poll \
             tick (roborev 307 #1)"
        );
        assert!(
            block.matches("lastRenderedContext !== context").count() >= 2,
            "both render-skips must also require the pane to actually show \
             this context — payload equality alone strands a Loading \
             placeholder when deep-equal payloads span contexts \
             (roborev 308 #1)"
        );
        // The producer side of the same invariant (roborev 309): without
        // the stamp both skips are permanently false and the row-0
        // selection reset returns; without the placeholder nulls the
        // stranded-placeholder bug returns. Both would leave the consumer
        // assertions above passing. Each assertion is scoped to the
        // function that must contain it (roborev 310) — a stamp or null
        // drifting elsewhere no longer means "the pane shows this context".
        assert!(
            block.contains("lastRenderedContext = null"),
            "loadEmails' cold-miss Loading placeholder must null the \
             rendered-context stamp"
        );
        assert!(
            js_fn_body(APP_JS, "function renderEmailList(")
                .contains("lastRenderedContext = splitCacheKey()"),
            "renderEmailList must stamp the context it draws"
        );
        assert!(
            js_fn_body(APP_JS, "function selectAccount(").contains("lastRenderedContext = null"),
            "selectAccount's Loading placeholder must null the \
             rendered-context stamp"
        );
    }

    #[test]
    fn send_gives_immediate_feedback_and_works_from_compose_normal_mode() {
        // The user-visible half of the send-stall fix: feedback appears the
        // moment Ctrl+Enter lands (before any await), and the chord works
        // from compose normal mode too (Escape blurs the field; the send
        // intent is unchanged).
        let start = APP_JS
            .find("async function sendEmail(")
            .expect("sendEmail must exist");
        let rest = &APP_JS[start..];
        let end = rest.find("\n}").expect("sendEmail must close");
        let wrapper = &rest[..end];
        let status_pos = wrapper
            .find("showStatus('Sending…')")
            .expect("sendEmail must show Sending… feedback");
        let await_pos = wrapper
            .find("await doSendEmail()")
            .expect("sendEmail must await the send body");
        assert!(
            status_pos < await_pos,
            "the Sending… status must appear before the first await — a stalled \
             send with no feedback reads as 'nothing happened'"
        );
        assert!(
            APP_JS.contains(
                "state.view === 'compose' && state.mode === 'normal' && \
                 e.key === 'Enter' && (e.ctrlKey || e.metaKey)"
            ),
            "Ctrl+Enter must send from compose normal mode as well as insert mode"
        );
    }

    #[test]
    fn mobile_app_js_clears_attachments_on_compose_reset() {
        // clearComposeFields is the single reset path shared by cancel,
        // discard, send-success, and re-entering compose — attachment
        // cleanup belongs there so every path clears it.
        let start = MOBILE_APP_JS
            .find("function clearComposeFields(")
            .expect("clearComposeFields must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("clearComposeFields must close");
        let block = &rest[..end];
        assert!(
            block.contains("PendingAttachments") || block.contains("pendingAttachments"),
            "clearComposeFields should reset pendingAttachments \
             (region: {block})"
        );
    }

    #[test]
    fn mobile_app_js_cancel_compose_dirty_check_covers_recipients_and_attachments() {
        // roborev 288: the dirty check only looked at subject/body, so a
        // recipients-only draft (To/Cc typed, nothing else) or an
        // attachment-only draft got silently discarded with no confirmation.
        // The check moved into composeDirty() (kata wm57), shared by cancel and
        // autosave — the recipients/attachments coverage lives there now.
        let start = MOBILE_APP_JS
            .find("function composeDirty(")
            .expect("composeDirty must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("composeDirty must close");
        let block = &rest[..end];
        assert!(
            block.contains("compose-to').value.trim()"),
            "cancelCompose's dirty check must include the To field"
        );
        assert!(
            block.contains("compose-cc').value.trim()"),
            "cancelCompose's dirty check must include the Cc field"
        );
        assert!(
            block.contains("pendingAttachments.length"),
            "cancelCompose's dirty check must include pending attachments"
        );
    }

    #[test]
    fn mobile_app_js_has_inline_image_preview() {
        let start = MOBILE_APP_JS
            .find("function renderAttachments(")
            .expect("renderAttachments must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest
            .find("\nfunction ")
            .expect("renderAttachments must close");
        let region = &rest[..end];
        assert!(
            region.contains(r#"loading="lazy""#),
            "image/* attachments should render a lazy-loaded inline preview"
        );
        assert!(
            region.contains("image/"),
            "the preview must be gated on image/* mime types"
        );
    }

    #[test]
    fn mobile_app_js_has_download_all() {
        let start = MOBILE_APP_JS
            .find("function renderAttachments(")
            .expect("renderAttachments must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest
            .find("\nfunction ")
            .expect("renderAttachments must close");
        let region = &rest[..end];
        assert!(
            region.contains("Download All"),
            "2+ attachments should offer a Download All action"
        );
        assert!(
            MOBILE_APP_JS.contains("function downloadAllAttachments("),
            "Download All should mirror desktop's sequential-click helper"
        );
    }

    #[test]
    fn mobile_app_js_has_no_share_sheet_integration() {
        // Deviation from the stale kata body (task A7 brief): no
        // navigator.share — iOS long-press on the attachment link already
        // offers the native share sheet.
        assert!(
            !MOBILE_APP_JS.contains("navigator.share"),
            "mobile app.js should not add navigator.share — the link's \
             long-press share sheet already covers this"
        );
    }

    #[test]
    fn mobile_html_has_attach_button_and_file_input() {
        for id in ["compose-attach-btn", "compose-file-input"] {
            assert!(
                MOBILE_HTML.contains(id),
                "compose markup should include #{id}"
            );
        }
        assert!(
            MOBILE_HTML.contains(r#"type="file""#),
            "the attach control should be backed by a file input"
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

    #[test]
    fn jetbrains_mono_is_self_hosted() {
        // cf614e5 dropped the Google Fonts @import (CSP-blocked; it never
        // loaded) but left 'JetBrains Mono' leading --font-mono, so the
        // typeface silently depended on a local install. Self-host the woff2
        // from this server instead (CSP already allows font-src 'self'):
        // deterministic rendering, no cross-origin fetch, works on the
        // tailnet and offline (roborev 315).
        assert!(
            !STYLE_CSS.contains("@import"),
            "style.css must not depend on cross-origin fetches"
        );
        assert!(
            STYLE_CSS.contains("--font-mono: 'JetBrains Mono'"),
            "the mono stack should still lead with JetBrains Mono"
        );
        let src = include_str!("routes.rs");
        let handler_src = src.split("mod tests").next().unwrap_or(src);
        // Weights matching actual CSS usage: 400 (body), 600, and bold=700.
        // The single italic use synthesizes. include_bytes! makes the files'
        // existence a compile-time guarantee.
        for face in ["Regular", "SemiBold", "Bold"] {
            let url = format!("url('/fonts/JetBrainsMono-{face}.woff2')");
            assert!(
                STYLE_CSS.contains(&url),
                "style.css must declare a self-hosted @font-face src {url}"
            );
            let route = format!("\"/fonts/JetBrainsMono-{face}.woff2\"");
            assert!(
                handler_src.contains(&route),
                "routes must serve {route} from 'self'"
            );
        }
        assert!(
            STYLE_CSS.matches("@font-face").count() >= 3,
            "each shipped weight needs its own @font-face block"
        );
        assert!(
            handler_src.contains("font/woff2"),
            "font responses must carry the woff2 content type"
        );
        // OFL §2: redistribution of the font files must be accompanied by
        // the copyright notice and license text (roborev 316).
        let ofl = include_str!("../static/fonts/OFL.txt");
        assert!(
            ofl.contains("SIL OPEN FONT LICENSE"),
            "static/fonts must ship the OFL license text alongside the woff2s"
        );
        assert!(
            ofl.contains("JetBrains Mono"),
            "the OFL copy must carry the JetBrains Mono copyright line"
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

    // =========================================================================
    // Persistent drafts (kata wm57)
    // =========================================================================

    #[test]
    fn draft_body_deserializes_plain_text() {
        let json = r#"{"to":["a@b.com"],"cc":["c@d.com"],"subject":"WIP","body":"draft text","in_reply_to":"<m@x>","from_address":"me@fastmail.com"}"#;
        let body: DraftBody = serde_json::from_str(json).unwrap();
        assert_eq!(body.to, vec!["a@b.com"]);
        assert_eq!(body.cc, vec!["c@d.com"]);
        assert_eq!(body.subject, "WIP");
        assert_eq!(body.body, "draft text");
        assert_eq!(body.in_reply_to.as_deref(), Some("<m@x>"));
        assert_eq!(body.from_address.as_deref(), Some("me@fastmail.com"));
    }

    #[test]
    fn draft_body_defaults_empty_fields() {
        // An autosave of a near-pristine compose (only a To typed) must still
        // deserialize — every field except the recipients is optional.
        let json = r#"{"to":["a@b.com"]}"#;
        let body: DraftBody = serde_json::from_str(json).unwrap();
        assert!(body.cc.is_empty());
        assert_eq!(body.subject, "");
        assert_eq!(body.body, "");
        assert!(body.in_reply_to.is_none());
    }

    #[test]
    fn draft_submission_is_plain_text_only() {
        // v1 persists no html_body / attachments / calendar — those live only
        // in the live compose session, never in the stored draft.
        let body = DraftBody {
            to: vec!["a@b.com".into()],
            cc: vec![],
            subject: "S".into(),
            body: "B".into(),
            in_reply_to: Some("<m@x>".into()),
            from_address: None,
        };
        let sub = draft_submission(body);
        assert_eq!(sub.text_body, "B");
        assert_eq!(sub.in_reply_to.as_deref(), Some("<m@x>"));
        assert!(sub.html_body.is_none(), "drafts persist no html body");
        assert!(sub.attachments.is_empty(), "drafts persist no attachments");
        assert!(sub.calendar_ics.is_none(), "drafts persist no calendar");
        assert!(sub.bcc.is_none());
    }

    // Review follow-up: every other mutation route (archive/trash/mark_read/
    // mark_unread/toggle_flag/move — see e.g. archive_email above) invalidates
    // state.prefetch after its write, so a warmed list cache never serves a
    // now-stale row. The drafts CRUD handlers mutate the Drafts mailbox just
    // as much (create adds a row, update rotates its id via destroy+recreate,
    // delete removes it) but were missing the same call — a prefetched Drafts
    // list could keep serving destroyed ids for up to the cache's TTL.
    #[test]
    fn draft_mutations_invalidate_prefetch_cache() {
        let src = include_str!("routes.rs");
        let handler_src = src.split("mod tests").next().unwrap_or(src);
        for func in [
            "async fn create_draft_handler(",
            "async fn update_draft_handler(",
            "async fn delete_draft_handler(",
        ] {
            let start = handler_src
                .find(func)
                .unwrap_or_else(|| panic!("{func} must exist"));
            let rest = &handler_src[start..];
            let end = rest.find("\n}").expect("draft handler must close");
            let block = &rest[..end];
            assert!(
                block.contains("resolve_account_id(&state, params.account.as_deref())"),
                "{func} must resolve the account id (matching archive_email et al.) \
                 so the invalidation is scoped to the right account"
            );
            assert!(
                block.contains("state.prefetch.invalidate(&id).await"),
                "{func} must invalidate the prefetch cache after mutating drafts, \
                 or a warmed Drafts list keeps serving destroyed/rotated ids"
            );
        }
    }

    // Both clients must wire the same draft contract: a debounced autosave that
    // POSTs then PUTs, the send-owned draft id (captured at send time, see
    // send_deletes_the_draft_it_captured_not_live_state) deleted on send, and
    // the drafts-mailbox-opens-compose restore — all gated on the fastmail
    // provider.
    fn assert_bundle_has_draft_autosave(js: &str, bundle: &str) {
        for needle in [
            "AUTOSAVE_DEBOUNCE_MS",
            "function scheduleAutosave",
            "function runAutosave",
            "function flushAutosave",
            "function deleteDraftById",
            "state.draftId",
            "'/drafts'",
            "/drafts/",
        ] {
            assert!(
                js.contains(needle),
                "{bundle} must contain draft-autosave wiring: {needle}"
            );
        }
        // Autosave and delete are Fastmail-only in v1.
        assert!(
            js.contains("provider === 'fastmail'"),
            "{bundle} must gate drafts on the fastmail provider"
        );
        // Restore: opening a draft-mailbox row goes to compose, not detail.
        assert!(
            js.contains("role === 'drafts'"),
            "{bundle} must special-case the drafts mailbox for restore"
        );
    }

    #[test]
    fn app_js_has_draft_autosave_wiring() {
        assert_bundle_has_draft_autosave(APP_JS, "desktop app.js");
        assert!(
            APP_JS.contains("function openDraftInCompose"),
            "desktop must route a drafts-mailbox open into compose"
        );
    }

    #[test]
    fn mobile_app_js_has_draft_autosave_wiring() {
        assert_bundle_has_draft_autosave(MOBILE_APP_JS, "mobile app.js");
        assert!(
            MOBILE_APP_JS.contains("function startDraftCompose"),
            "mobile must route a drafts-mailbox open into compose"
        );
    }

    #[test]
    fn draft_autosave_never_fires_on_pristine_compose() {
        // The debounce fire must bail when the compose isn't dirty, so an
        // untouched (or signature-only) compose never creates a draft.
        for (js, bundle) in [(APP_JS, "desktop"), (MOBILE_APP_JS, "mobile")] {
            let start = js
                .find("function runAutosave")
                .unwrap_or_else(|| panic!("{bundle} runAutosave must exist"));
            let rest = &js[start..];
            let end = rest.find("\n}").expect("runAutosave must close");
            let block = &rest[..end];
            assert!(
                block.contains("composeDirty()"),
                "{bundle} runAutosave must guard on composeDirty() so a pristine \
                 compose is never autosaved"
            );
        }
    }

    #[test]
    fn draft_autosave_guarded_during_send() {
        // Review follow-up: an autosave landing while the send is in flight
        // would persist a ghost draft of the very mail being sent — never
        // adopted (compose clears on success) and never deleted. Both
        // runAutosave implementations must bail on the in-flight send lock.
        for (js, bundle) in [(APP_JS, "desktop"), (MOBILE_APP_JS, "mobile")] {
            let start = js
                .find("function runAutosave")
                .unwrap_or_else(|| panic!("{bundle} runAutosave must exist"));
            let rest = &js[start..];
            let end = rest.find("\n}").expect("runAutosave must close");
            let block = &rest[..end];
            assert!(
                block.contains("state.sending"),
                "{bundle} runAutosave must bail while a send is in flight"
            );
        }
        // Both bundles now split send into a wrapper (owns the sending lock)
        // and a body (doSendEmail / doSendComposedEmail). The wrapper must
        // take the lock BEFORE its first await: the old shape (lock set
        // after the autosave settle) let two rapid Ctrl+Enters / taps both
        // slip past a check-only guard during that settle and double-send.
        for (js, bundle, wrapper_fn, body_await, lock_set, lock_clear) in [
            (
                APP_JS,
                "desktop",
                "async function sendEmail(",
                "await doSendEmail()",
                "state.sending = true",
                "state.sending = false",
            ),
            (
                MOBILE_APP_JS,
                "mobile",
                "async function sendComposedEmail(",
                "await doSendComposedEmail()",
                "setComposeSending(true)",
                "setComposeSending(false)",
            ),
        ] {
            let start = js
                .find(wrapper_fn)
                .unwrap_or_else(|| panic!("{bundle} send wrapper must exist"));
            let rest = &js[start..];
            let end = rest.find("\n}").expect("send wrapper must close");
            let wrapper = &rest[..end];
            assert!(
                wrapper.contains("if (state.sending) return"),
                "{bundle} send wrapper must bail when a send is already in flight"
            );
            let lock_pos = wrapper
                .find(lock_set)
                .unwrap_or_else(|| panic!("{bundle} send wrapper must set the sending lock"));
            let await_pos = wrapper
                .find(body_await)
                .unwrap_or_else(|| panic!("{bundle} send wrapper must await the send body"));
            assert!(
                lock_pos < await_pos,
                "{bundle} send wrapper must take the sending lock BEFORE its first \
                 await — after it, a second submit during the autosave settle \
                 double-sends"
            );
            assert!(
                wrapper.contains(lock_clear),
                "{bundle} send wrapper must clear the sending lock when the send settles"
            );
        }

        // The bodies must kill the pending debounce up front (roborev 302,
        // fix 1) — a debounce firing mid-send would chain a fresh doAutosave
        // that lands after deleteTrackedDraft, re-adopting a ghost draft of
        // the sent mail — and AGAIN after awaiting the in-flight save
        // (roborev 303 fix 4 / 304): a keystroke during that await re-arms
        // the debounce. The sending lock (now held from wrapper entry) makes
        // runAutosave skip the re-armed save at fire time, but the second
        // synchronous cancel stays as defense in depth.
        for (js, bundle, body_fn) in [
            (APP_JS, "desktop", "async function doSendEmail("),
            (
                MOBILE_APP_JS,
                "mobile",
                "async function doSendComposedEmail(",
            ),
        ] {
            let start = js
                .find(body_fn)
                .unwrap_or_else(|| panic!("{bundle} send body must exist"));
            let rest = &js[start..];
            let end = rest.find("\n}").expect("send body must close");
            let block = &rest[..end];
            let await_pos = block
                .find("await saveInFlight.catch(() => {});")
                .unwrap_or_else(|| {
                    panic!("{bundle} send body must await the in-flight save before proceeding")
                });
            assert!(
                block[..await_pos].contains("cancelAutosave()"),
                "{bundle} send body must cancel the pending autosave debounce up front"
            );
            assert!(
                block[await_pos..].contains("cancelAutosave()"),
                "{bundle} send body must cancel the autosave debounce AGAIN after \
                 awaiting saveInFlight (re-arm window, roborev 303/304)"
            );
        }
    }

    // roborev 294 fix 4: two autosaves that would otherwise overlap must be
    // serialized onto the same in-flight promise, or both could read the same
    // stale (pre-adoption) draftId and double-POST a create. runAutosave must
    // chain onto `saveInFlight` rather than performing the save itself —
    // `doAutosave` (which reads/writes state.draftId) is only ever entered
    // through that chain.
    #[test]
    fn draft_autosave_serializes_on_shared_in_flight_promise() {
        for (js, bundle) in [(APP_JS, "desktop"), (MOBILE_APP_JS, "mobile")] {
            assert!(
                js.contains("let saveInFlight"),
                "{bundle} must track the in-flight autosave at module scope"
            );
            assert!(
                js.contains("function doAutosave("),
                "{bundle} must split the actual save into doAutosave"
            );
            let start = js
                .find("async function runAutosave")
                .unwrap_or_else(|| panic!("{bundle} runAutosave must exist"));
            let rest = &js[start..];
            let end = rest.find("\n}").expect("runAutosave must close");
            let block = &rest[..end];
            assert!(
                block.contains("saveInFlight ="),
                "{bundle} runAutosave must chain onto saveInFlight rather than \
                 firing its own request directly"
            );
            assert!(
                block.contains(".then(") && block.contains("doAutosave("),
                "{bundle} runAutosave must chain the next save with .then(...) \
                 onto whatever save is already in flight"
            );
        }
    }

    // roborev 294 fix 3: cancelAutosave() only clears the pending debounce
    // TIMER — a save whose HTTP request already started keeps running. Send
    // must await that in-flight save (settled either way) before deciding
    // which draft to delete, or the late-landing save's id is dropped
    // un-adopted and un-deleted: a ghost draft.
    #[test]
    fn draft_send_awaits_in_flight_save_before_deleting_draft() {
        for (js, bundle, func) in [
            (APP_JS, "desktop", "async function doSendEmail("),
            (
                MOBILE_APP_JS,
                "mobile",
                "async function doSendComposedEmail(",
            ),
        ] {
            let start = js
                .find(func)
                .unwrap_or_else(|| panic!("{bundle} {func} must exist"));
            let rest = &js[start..];
            let end = rest.find("\n}").expect("send fn must close");
            let block = &rest[..end];
            let await_pos = block
                .find("await saveInFlight")
                .unwrap_or_else(|| panic!("{bundle} send must await the in-flight autosave"));
            let delete_pos = block.find("deleteDraftById(draftId)").unwrap_or_else(|| {
                panic!("{bundle} send must delete the send-owned draft on success")
            });
            assert!(
                await_pos < delete_pos,
                "{bundle} send must await saveInFlight BEFORE deleting the draft, \
                 so a late-adopted id is the one that gets deleted"
            );
        }
    }

    // roborev 294 fix 2: the server destroys+recreates a draft on every
    // update, so the tracked id rotates on almost every save. If the OLD id
    // is left in the Drafts list row or email cache, it strands a dead id —
    // reopening that row later fetches the now-destroyed old id. adoptDraftId
    // must swap the id into both places (gated on Drafts being the mailbox in
    // view) instead of just overwriting state.draftId directly.
    #[test]
    fn draft_id_rotation_updates_list_row_and_cache_key() {
        for (js, bundle) in [(APP_JS, "desktop"), (MOBILE_APP_JS, "mobile")] {
            let start = js
                .find("function adoptDraftId(")
                .unwrap_or_else(|| panic!("{bundle} must define adoptDraftId"));
            let rest = &js[start..];
            let end = rest.find("\n}").expect("adoptDraftId must close");
            let block = &rest[..end];
            assert!(
                block.contains("role === 'drafts'"),
                "{bundle} adoptDraftId must gate the swap on Drafts being the \
                 current mailbox"
            );
            assert!(
                block.contains("state.emails.find(e => e.id === oldId)"),
                "{bundle} adoptDraftId must look up the list row by the OLD id"
            );
            assert!(
                block.contains("row.id = newId"),
                "{bundle} adoptDraftId must swap the list row's id in place"
            );
            assert!(
                block.contains("delete "),
                "{bundle} adoptDraftId must purge the stale old-id cache entry"
            );
            // doAutosave must route id adoption through adoptDraftId, not
            // assign state.draftId directly — a direct assignment would skip
            // the list/cache swap entirely.
            let doautosave_start = js
                .find("function doAutosave(")
                .unwrap_or_else(|| panic!("{bundle} must define doAutosave"));
            let doautosave_rest = &js[doautosave_start..];
            let doautosave_end = doautosave_rest.find("\n}").expect("doAutosave must close");
            let doautosave_block = &doautosave_rest[..doautosave_end];
            assert!(
                doautosave_block.contains("adoptDraftId(res.id)"),
                "{bundle} doAutosave must adopt a rotated id through adoptDraftId"
            );
        }
    }

    // Review follow-up: every leave-compose path runs flushAutosave() then
    // clearCompose()/clearComposeFields() back to back, synchronously.
    // clearCompose nulls state.draftId (and, on desktop, bumps composeSession)
    // before the flushed save's queued microtask (inside doAutosave) ever gets
    // a turn to run — a live read of state.draftId there would see null and
    // POST a brand-new draft instead of PUTting the one being left,
    // duplicating it. doAutosave must instead decide PUT vs POST off a
    // draft-id/session pair that clearCompose/clearComposeFields never touch.
    #[test]
    fn draft_autosave_targets_tracked_id_immune_to_clear_compose() {
        for (js, bundle) in [(APP_JS, "desktop"), (MOBILE_APP_JS, "mobile")] {
            assert!(
                js.contains("let trackedDraftId"),
                "{bundle} must track the autosave draft id at module scope, \
                 separate from state.draftId"
            );
            assert!(
                js.contains("let trackedDraftSession"),
                "{bundle} must tag the tracked draft id with the compose \
                 session it belongs to, so a later compose can't inherit a \
                 stale id left by a leave-path flush"
            );
            let start = js
                .find("async function doAutosave(")
                .unwrap_or_else(|| panic!("{bundle} doAutosave must exist"));
            let rest = &js[start..];
            let end = rest.find("\n}").expect("doAutosave must close");
            let block = &rest[..end];
            assert!(
                block.contains("trackedDraftSession === session"),
                "{bundle} doAutosave must gate id reuse on the tracked \
                 session matching this save's captured session"
            );
            assert!(
                !block.contains("state.draftId ?"),
                "{bundle} doAutosave must not decide PUT vs POST off a live \
                 read of state.draftId — clearCompose/clearComposeFields can \
                 null it before this microtask runs"
            );
            // roborev 297: sessions only increase, so a stale (older-session)
            // save that completes AFTER a newer restore has already seeded
            // trackedDraftId/trackedDraftSession must not clobber that seed —
            // otherwise the restored draft's next autosave session-mismatches
            // and POSTs a duplicate instead of PUTting the restored id.
            assert!(
                block.contains("session >= trackedDraftSession"),
                "{bundle} doAutosave must guard the trackedDraftId/\
                 trackedDraftSession write on the save's session being at \
                 least as new as the currently tracked one, so an in-flight \
                 stale save can't clobber a newer restore's seed"
            );
        }
    }

    // roborev 299 (reverts roborev 298 fix 3): when the session-recency guard
    // above rejects a stale completion (a later restore already tracked a
    // fresher id/session), and that stale save took the POST path (draftId
    // resolved to null at request time), the server draft it just created is
    // never adopted anywhere. Deleting it is NOT safe: this branch is only
    // reachable when the save carried real user content (the composeDirty
    // gate means autosave never fires on a pristine compose), so the
    // "orphan" is actually a real Drafts message holding the abandoned
    // compose's final edits, stored nowhere else. doAutosave must NOT issue
    // a DELETE in that rejected branch — the stray draft must be left alone,
    // visible and recoverable in the Drafts mailbox — and must document the
    // deliberate trade-off in place of the old cleanup call.
    #[test]
    fn draft_autosave_never_deletes_rejected_stale_post_draft() {
        for (js, bundle) in [(APP_JS, "desktop"), (MOBILE_APP_JS, "mobile")] {
            let start = js
                .find("async function doAutosave(")
                .unwrap_or_else(|| panic!("{bundle} doAutosave must exist"));
            let rest = &js[start..];
            let end = rest.find("\n}").expect("doAutosave must close");
            let block = &rest[..end];
            // The rejected branch (the `else` of the accept-guard, additionally
            // gated on `!draftId` so a PUT never reaches it) must still exist
            // structurally, but must no longer delete anything.
            let branch_start = block
                .find("else if (res?.id && !draftId)")
                .unwrap_or_else(|| {
                    panic!(
                        "{bundle} doAutosave must have a rejected-branch gated \
                         on !draftId documenting the no-delete trade-off"
                    )
                });
            let branch_rest = &block[branch_start..];
            let branch_end = branch_rest
                .find("\n        }")
                .expect("rejected branch must close");
            let branch_block = &branch_rest[..branch_end];
            assert!(
                !branch_block.contains("'DELETE'"),
                "{bundle} doAutosave's rejected-POST branch must NOT delete \
                 the stray server draft — it may be the user's only copy of \
                 abandoned compose edits"
            );
            assert!(
                branch_block.contains("Deliberately NOT deleted"),
                "{bundle} the rejected-POST branch must document the \
                 deliberate trade-off of leaving the stray draft in place \
                 instead of deleting it"
            );
            // The accept branch (PUT-path-inclusive id adoption) must come
            // first and must not itself contain a DELETE either — a PUT's
            // res.id must never be deleted.
            let accept_start = block
                .find("if (res?.id && session >= trackedDraftSession)")
                .unwrap_or_else(|| panic!("{bundle} doAutosave must have the accept guard"));
            assert!(
                accept_start < branch_start,
                "{bundle} the rejected-POST branch must be the `else` of the \
                 accept guard, not a separate earlier branch"
            );
            let accept_rest = &block[accept_start..branch_start];
            assert!(
                !accept_rest.contains("'DELETE'"),
                "{bundle} the accept (session-ok) branch must not issue a \
                 DELETE either — a PUT's res.id must never be deleted"
            );
        }
    }

    // openDraftInCompose/startDraftCompose set state.draftId directly
    // (restoring a draft bypasses doAutosave's normal adoption path) — they
    // must seed trackedDraftId/trackedDraftSession too, or the very next
    // autosave on the restored draft would see a session mismatch and POST a
    // fresh duplicate instead of PUTting the restored draft.
    #[test]
    fn draft_restore_seeds_tracked_autosave_id() {
        for (js, bundle, func) in [
            (APP_JS, "desktop", "async function openDraftInCompose("),
            (MOBILE_APP_JS, "mobile", "async function startDraftCompose("),
        ] {
            let start = js
                .find(func)
                .unwrap_or_else(|| panic!("{bundle} {func} must exist"));
            let rest = &js[start..];
            let end = rest.find("\n}").expect("restore fn must close");
            let block = &rest[..end];
            assert!(
                block.contains("trackedDraftId = emailId"),
                "{bundle} restore must seed trackedDraftId with the restored \
                 draft's id"
            );
            assert!(
                block.contains("trackedDraftSession = state.composeSession"),
                "{bundle} restore must tag the seeded id with the current \
                 compose session"
            );
        }
    }

    #[test]
    fn draft_restore_rehydrates_reply_context() {
        // Review follow-up: the draft persisted in_reply_to; restore must
        // carry it back into replyContext or every subsequent save/send
        // silently drops the threading headers.
        for (js, bundle, func) in [
            (APP_JS, "desktop", "function openDraftInCompose"),
            (MOBILE_APP_JS, "mobile", "function startDraftCompose"),
        ] {
            let start = js
                .find(func)
                .unwrap_or_else(|| panic!("{bundle} {func} must exist"));
            let rest = &js[start..];
            let end = rest.find("\n}").expect("restore fn must close");
            let block = &rest[..end];
            assert!(
                block.contains("inReplyTo: draft.inReplyTo"),
                "{bundle} restore must rehydrate replyContext from the \
                 draft's persisted inReplyTo"
            );
        }
        // Server half of the contract: the detail response must expose the
        // field the clients read. (jmap.rs parse tests cover request+parse;
        // this pins the Email→wire field the handler serializes.)
        let mut email = test_email_with_recipients(vec![], vec![]);
        email.in_reply_to = Some("<parent@example.com>".into());
        let wire = serde_json::to_value(&email).unwrap();
        assert_eq!(
            wire["in_reply_to"], "<parent@example.com>",
            "Email must carry in_reply_to through serialization"
        );
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

    // Email HTML is rendered inside a sandboxed iframe (see
    // `renderHtmlBodyIframe` in static/app.js and static/mobile/app.js)
    // instead of being passed through a JS sanitizer. The iframe's sandbox
    // omits `allow-scripts` and `allow-same-origin`, so any script in the
    // email cannot execute in the app origin — closing the whole class of
    // HTML-sanitizer-bypass vulnerabilities.

    #[test]
    fn app_js_renders_email_body_in_sandboxed_iframe() {
        assert!(
            APP_JS.contains("renderHtmlBodyIframe"),
            "desktop app.js must render HTML email bodies via the sandboxed iframe helper"
        );
        assert!(
            !APP_JS.contains("function sanitizeHtml"),
            "desktop app.js must NOT keep a client-side HTML sanitizer (replaced by iframe sandbox)"
        );
    }

    #[test]
    fn outgoing_html_strips_scripts_and_javascript_urls() {
        // Reply/forward propagates the original sender's HTML out to
        // recipients whose clients may render it unsafely. The iframe
        // sandbox protects only us; outbound sanitization is defense in
        // depth for the recipients.
        let dirty = r#"<p>hi <script>alert(1)</script><a href="javascript:steal()">x</a></p>"#;
        let cleaned = sanitize_outgoing_html(dirty);
        assert!(
            !cleaned.contains("<script"),
            "outbound sanitizer must strip <script> tags"
        );
        assert!(
            !cleaned.to_lowercase().contains("javascript:"),
            "outbound sanitizer must strip javascript: URLs"
        );
        // Should preserve benign content.
        assert!(cleaned.contains("hi"));
    }

    #[test]
    fn outgoing_html_bypass_via_whitespace_prefix_is_blocked() {
        // The very bypass that motivated this whole change (leading TAB
        // before javascript: in href) must be neutralized server-side too.
        let dirty = "<a href=\"\tjavascript:alert(1)\">x</a>";
        let cleaned = sanitize_outgoing_html(dirty);
        assert!(
            !cleaned.to_lowercase().contains("javascript:"),
            "outbound sanitizer must strip whitespace-prefixed javascript: URLs"
        );
    }

    #[test]
    fn app_js_iframe_sandbox_never_allows_scripts() {
        // Strict invariant: `allow-scripts` must NEVER appear in any email-iframe
        // sandbox token list — that is what closes the entire XSS class. Both the
        // read-side iframe and the compose-quote autosize iframe must respect
        // this. (Compose-quote uses `allow-same-origin` for scrollHeight
        // measurement, which is safe specifically because scripts are absent.)
        let pos = APP_JS
            .find("function renderHtmlBodyIframe")
            .expect("function renderHtmlBodyIframe must exist");
        let region = &APP_JS[pos..APP_JS.len().min(pos + 2000)];
        assert!(
            region.contains("'allow-popups allow-popups-to-escape-sandbox'"),
            "read-side iframe sandbox token list must be allow-popups+allow-popups-to-escape-sandbox"
        );
        assert!(
            !region.contains("allow-scripts"),
            "iframe sandbox must NOT include allow-scripts (would re-enable XSS)"
        );
    }

    #[test]
    fn mobile_app_js_renders_email_body_in_sandboxed_iframe() {
        assert!(
            MOBILE_APP_JS.contains("renderHtmlBodyIframe"),
            "mobile app.js must render HTML email bodies via the sandboxed iframe helper"
        );
        assert!(
            !MOBILE_APP_JS.contains("function sanitizeHtml"),
            "mobile app.js must NOT keep a client-side HTML sanitizer"
        );
        let pos = MOBILE_APP_JS
            .find("function renderHtmlBodyIframe")
            .expect("function renderHtmlBodyIframe must exist");
        let region = &MOBILE_APP_JS[pos..MOBILE_APP_JS.len().min(pos + 2000)];
        assert!(
            !region.contains("allow-scripts") && !region.contains("allow-same-origin"),
            "mobile iframe sandbox must omit allow-scripts and allow-same-origin"
        );
    }

    // Returns the source slice covering the whole body of `wrapEmailHtml`,
    // from its `function` declaration up to (but not including) the next
    // top-level `function` declaration. Using the real function boundary
    // instead of a fixed-size window avoids silently truncating (or
    // over-including) the region if the function grows or shrinks.
    fn wrap_email_html_region<'a>(source: &'a str, label: &str) -> &'a str {
        let start = source
            .find("function wrapEmailHtml")
            .unwrap_or_else(|| panic!("{label}: function wrapEmailHtml must exist"));
        let after_decl = start + "function wrapEmailHtml".len();
        let end = source[after_decl..]
            .find("\nfunction ")
            .map(|rel| after_decl + rel)
            .unwrap_or(source.len());
        &source[start..end]
    }

    // Sender CSS like `writing-mode: vertical-rl` (e.g. updates.cash.app)
    // must be neutralized in the iframe's injected stylesheet, or the
    // whole email body renders sideways and unreadable (kata 80v2).
    fn assert_wrap_email_html_neutralizes_vertical_writing_mode(source: &str, label: &str) {
        let region = wrap_email_html_region(source, label);
        assert!(
            region.contains("writing-mode: horizontal-tb !important"),
            "{label}: iframe stylesheet must force horizontal-tb writing-mode on all elements"
        );
        assert!(
            region.contains("text-orientation: mixed !important"),
            "{label}: iframe stylesheet must force mixed text-orientation on all elements"
        );
    }

    #[test]
    fn app_js_wrap_email_html_neutralizes_vertical_writing_mode() {
        assert_wrap_email_html_neutralizes_vertical_writing_mode(APP_JS, "app.js");
    }

    #[test]
    fn mobile_app_js_wrap_email_html_neutralizes_vertical_writing_mode() {
        assert_wrap_email_html_neutralizes_vertical_writing_mode(MOBILE_APP_JS, "mobile app.js");
    }

    #[tokio::test]
    async fn index_html_sets_restrictive_csp() {
        let resp = index_html().await.into_response();
        let csp = resp
            .headers()
            .get("content-security-policy")
            .expect("index.html must set Content-Security-Policy")
            .to_str()
            .unwrap();
        assert!(
            csp.contains("script-src 'self'"),
            "CSP must restrict script-src to 'self' so an innerHTML XSS cannot eval inline script"
        );
        assert!(
            csp.contains("object-src 'none'"),
            "CSP must block <object>/<embed> plugins"
        );
        assert!(
            csp.contains("base-uri 'none'"),
            "CSP must lock down <base> so an attacker cannot rewrite relative URLs"
        );
    }

    #[tokio::test]
    async fn mobile_html_sets_restrictive_csp() {
        let resp = mobile_html().await.into_response();
        let csp = resp
            .headers()
            .get("content-security-policy")
            .expect("mobile index.html must set Content-Security-Policy")
            .to_str()
            .unwrap();
        assert!(csp.contains("script-src 'self'"));
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
            in_reply_to: None,
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
            is_update: false,
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
    fn app_js_user_status_not_block_scoped() {
        // userStatus must be declared before the cancelled if/else, not inside the else block
        let pos = APP_JS
            .find("const userStatus = event.isUpdate ? null : (event.user_rsvp_status")
            .expect("should declare userStatus");
        let after = &APP_JS[pos..];
        let cancelled_pos = after
            .find("if (cancelled)")
            .expect("should have cancelled check");
        // userStatus declaration should come BEFORE the cancelled check
        assert!(
            cancelled_pos > 0,
            "userStatus should be declared before the cancelled if/else block"
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
    fn app_js_has_tentative_keyboard_shortcut() {
        // m for tentative/maybe, alongside y (accept) and n (decline).
        assert!(
            APP_JS.contains("case 'm':") && APP_JS.contains("rsvpToEvent('TENTATIVE')"),
            "app.js should map the 'm' key to a TENTATIVE RSVP"
        );
    }

    #[test]
    fn both_clients_render_update_banner() {
        // Rescheduled invites (calendarEvent.isUpdate) show a non-destructive
        // "updated — please respond again" banner in both bundles.
        for (name, js) in [("app.js", APP_JS), ("mobile/app.js", MOBILE_APP_JS)] {
            assert!(
                js.contains("event.isUpdate"),
                "{name} renderCalendarCard should read event.isUpdate"
            );
            assert!(
                js.contains("cal-updated"),
                "{name} should render the cal-updated banner element"
            );
            assert!(
                js.contains("Updated — please respond again"),
                "{name} should show the update banner copy"
            );
        }
        // Both stylesheets style the banner.
        assert!(
            STYLE_CSS.contains(".cal-updated"),
            "style.css should style .cal-updated"
        );
        assert!(
            MOBILE_HTML.contains(".cal-updated"),
            "mobile index.html should style .cal-updated"
        );
        // Desktop reset gate: on an Update, userStatus must be forced null so
        // the getUserRsvpStatus attendee-scan fallback can't re-highlight a
        // button (or show "You responded X") from the incoming ICS's stale
        // PARTSTAT under the "please respond again" banner.
        assert!(
            APP_JS.contains(
                "const userStatus = event.isUpdate ? null : (event.user_rsvp_status || getUserRsvpStatus(event));"
            ),
            "app.js must gate the RSVP highlight on isUpdate (null on update)"
        );
    }

    #[test]
    fn get_email_reaches_only_if_new_false_on_update() {
        // The SEQUENCE-update path must overwrite the stored event: an Update
        // decision spawns add_to_calendar with only_if_new = false, and sets
        // is_update so the client banners it.
        let src = include_str!("routes.rs");
        assert!(
            src.contains("calendar::invite_update_decision("),
            "get_email must call the pure decision helper"
        );
        assert!(
            src.contains("calendar::InviteAction::Update =>"),
            "get_email must handle the Update arm"
        );
        assert!(
            src.contains("provider::add_to_calendar(&s, &ics_clone, &uid, false)"),
            "the Update arm must add with only_if_new = false (overwrite)"
        );
        assert!(
            src.contains("event.is_update = true;"),
            "the Update arm must flag the event as an update"
        );
        // RejectSpoof must write nothing — no add_to_calendar in that arm.
        assert!(
            src.contains("calendar::InviteAction::RejectSpoof =>"),
            "get_email must handle the RejectSpoof arm"
        );
    }

    #[test]
    fn get_email_content_idempotence_guard_downgrades_noop_update_to_unchanged() {
        // roborev 292 #1: Outlook's parse_graph_event always reports
        // sequence: 0 (Graph has no SEQUENCE field), so any invite with ICS
        // SEQUENCE >= 1 hits the Update arm on *every* re-open even when
        // nothing changed — and the remove+re-add wipes the user's stored
        // responseStatus each time. A legacy Gmail event (stored before
        // SEQUENCE round-tripping was added) hits the same bug once. Verify
        // the content check runs before the match and can downgrade a no-op
        // Update to Unchanged (no write, no reset, no banner), while a real
        // reschedule (content differs) still reaches the Update arm.
        let src = include_str!("routes.rs");
        let handler_src = src.split("mod tests").next().unwrap_or(src);
        assert!(
            handler_src.contains("calendar::events_content_match(stored, &event)"),
            "get_email must call the content-idempotence guard against the stored event"
        );
        assert!(
            handler_src.contains("decision == calendar::InviteAction::Update"),
            "the guard must gate specifically on an Update decision"
        );
        assert!(
            handler_src.contains("calendar::InviteAction::Unchanged"),
            "the guard must downgrade a no-op Update to Unchanged"
        );
    }

    #[test]
    fn get_email_content_idempotence_guard_scoped_to_stored_sequence_zero() {
        // roborev 295 #1: events_content_match only tracks a subset of
        // fields (DTSTART/DTEND/SUMMARY/LOCATION/DESCRIPTION/attendees).
        // Once a provider round-trips SEQUENCE faithfully (stored.sequence >
        // 0), the SEQUENCE comparison in invite_update_decision is already
        // trustworthy — the content-match downgrade must not run in that
        // case, or a genuine reschedule that touches a field the guard
        // doesn't track could be masked as Unchanged. Only the
        // sequence-blind cases (stored.sequence == 0: Outlook always,
        // pre-round-trip legacy Gmail events) should hit the guard.
        let src = include_str!("routes.rs");
        let handler_src = src.split("mod tests").next().unwrap_or(src);
        assert!(
            handler_src
                .contains("stored.sequence == 0 && calendar::events_content_match(stored, &event)"),
            "the content-idempotence guard must be scoped to stored.sequence == 0"
        );
    }

    #[test]
    fn get_email_cancel_arm_gates_removal_on_organizer_match() {
        // roborev 292 #2: the CANCEL arm must not remove the stored event
        // just because someone mailed a METHOD:CANCEL ICS referencing a
        // known UID — only when the sender matches the stored event's
        // organizer (the same anti-spoof check used for REQUEST updates).
        let src = include_str!("routes.rs");
        let handler_src = src.split("mod tests").next().unwrap_or(src);
        assert!(
            handler_src.contains("calendar::cancel_decision("),
            "get_email must call the CANCEL anti-spoof decision helper"
        );
        assert!(
            handler_src.contains("calendar::CancelAction::Remove =>"),
            "get_email must handle the Remove arm"
        );
        assert!(
            handler_src.contains("calendar::CancelAction::RejectSpoof =>"),
            "get_email must handle the RejectSpoof arm (skip removal + warn)"
        );
        // The cancelled banner must still render regardless of the removal
        // decision — display isn't the attack surface, the calendar write is.
        assert!(
            handler_src.contains("calendar_event = Some(event);"),
            "the parsed CANCEL event must still be returned for the banner"
        );
    }

    #[test]
    fn calendar_event_is_update_serializes_camel_case() {
        let mut event = test_calendar_event(vec!["bob@example.com"]);
        event.is_update = true;
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["isUpdate"], true);
        // Default stays false and is exposed as isUpdate (not is_update).
        let plain = test_calendar_event(vec!["bob@example.com"]);
        let plain_json = serde_json::to_value(&plain).unwrap();
        assert_eq!(plain_json["isUpdate"], false);
        assert!(plain_json.get("is_update").is_none());
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

    // =========================================================================
    // RSVP verification tests (THE-192)
    //
    // These verify the contracts between backend and frontend:
    // - rsvp() JSON response shape matches what the frontend expects
    // - CalendarEvent serialization includes user_rsvp_status
    // - Frontend JS handles all verification scenarios correctly
    // =========================================================================

    #[test]
    fn rsvp_response_wraps_in_calendar_event_key() {
        // The frontend expects `result.calendarEvent` from the RSVP endpoint.
        // Verify the JSON shape by constructing what rsvp() returns.
        let mut event = test_calendar_event(vec!["bob@example.com"]);
        event.user_rsvp_status = Some("ACCEPTED".into());
        event.attendees[0].status = "ACCEPTED".into();

        let json = serde_json::json!({ "calendarEvent": event });
        assert!(json.get("calendarEvent").is_some());
        assert_eq!(json["calendarEvent"]["user_rsvp_status"], "ACCEPTED");
    }

    #[test]
    fn rsvp_response_includes_updated_attendee_status() {
        let mut event = test_calendar_event(vec!["bob@example.com", "carol@example.com"]);
        // Simulate what rsvp() does: update the matching attendee
        if let Some(att) = event
            .attendees
            .iter_mut()
            .find(|a| a.email == "bob@example.com")
        {
            att.status = "ACCEPTED".into();
        }
        event.user_rsvp_status = Some("ACCEPTED".into());

        let json = serde_json::json!({ "calendarEvent": event });
        let attendees = json["calendarEvent"]["attendees"].as_array().unwrap();
        let bob = attendees
            .iter()
            .find(|a| a["email"] == "bob@example.com")
            .unwrap();
        assert_eq!(bob["status"], "ACCEPTED");
        // Carol unchanged
        let carol = attendees
            .iter()
            .find(|a| a["email"] == "carol@example.com")
            .unwrap();
        assert_eq!(carol["status"], "NEEDS-ACTION");
    }

    #[test]
    fn rsvp_response_user_rsvp_status_set_for_all_statuses() {
        for (status, expected) in [
            (crate::types::RsvpStatus::Accepted, "ACCEPTED"),
            (crate::types::RsvpStatus::Tentative, "TENTATIVE"),
            (crate::types::RsvpStatus::Declined, "DECLINED"),
        ] {
            let mut event = test_calendar_event(vec!["bob@example.com"]);
            event.user_rsvp_status = Some(status.as_ics_str().to_string());
            let json = serde_json::json!({ "calendarEvent": event });
            assert_eq!(
                json["calendarEvent"]["user_rsvp_status"].as_str().unwrap(),
                expected,
                "user_rsvp_status should be {expected} for {expected}"
            );
        }
    }

    #[test]
    fn calendar_event_serializes_user_rsvp_status_when_some() {
        let mut event = test_calendar_event(vec!["bob@example.com"]);
        event.user_rsvp_status = Some("ACCEPTED".into());
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["user_rsvp_status"], "ACCEPTED");
    }

    #[test]
    fn calendar_event_serializes_user_rsvp_status_as_null_when_none() {
        let event = test_calendar_event(vec!["bob@example.com"]);
        let json = serde_json::to_value(&event).unwrap();
        assert!(json["user_rsvp_status"].is_null());
    }

    #[test]
    fn get_email_response_includes_calendar_event_key() {
        // Verify the JSON shape of get_email() response includes calendarEvent
        let event = test_calendar_event(vec!["bob@example.com"]);
        let json = serde_json::json!({
            "id": "test",
            "calendarEvent": event,
        });
        assert!(json.get("calendarEvent").is_some());
        assert!(json["calendarEvent"]["user_rsvp_status"].is_null());
    }

    // --- Frontend contract: renderCalendarCard handles persisted status ---

    #[test]
    fn app_js_renders_you_responded_for_each_status() {
        // The frontend maps ACCEPTED→"Accepted", TENTATIVE→"Maybe", DECLINED→"Declined"
        assert!(APP_JS.contains("ACCEPTED: 'Accepted'"));
        assert!(APP_JS.contains("TENTATIVE: 'Maybe'"));
        assert!(APP_JS.contains("DECLINED: 'Declined'"));
    }

    #[test]
    fn app_js_hides_status_label_for_needs_action() {
        assert!(
            APP_JS.contains("!== 'NEEDS-ACTION'"),
            "status label should be hidden when status is NEEDS-ACTION"
        );
    }

    #[test]
    fn app_js_rsvp_buttons_toggle_active_class() {
        // Verify all three buttons get .active toggled based on status
        assert!(APP_JS.contains("rsvpAccept.classList.toggle('active'"));
        assert!(APP_JS.contains("rsvpMaybe.classList.toggle('active'"));
        assert!(APP_JS.contains("rsvpDecline.classList.toggle('active'"));
    }

    #[test]
    fn app_js_hides_rsvp_actions_for_cancelled_events() {
        // Cancelled events should hide the RSVP buttons
        let render_fn_pos = APP_JS.find("function renderCalendarCard").unwrap();
        let render_fn = &APP_JS[render_fn_pos..render_fn_pos + 3000];
        assert!(
            render_fn.contains("actions.style.display = 'none'"),
            "cancelled events should hide RSVP actions"
        );
    }

    #[test]
    fn app_js_optimistic_update_reverts_on_error() {
        // rsvpToEvent should deep-clone prevEvent and revert on catch
        let rsvp_fn_pos = APP_JS.find("async function rsvpToEvent").unwrap();
        let rsvp_fn = &APP_JS[rsvp_fn_pos..rsvp_fn_pos + 1500];
        assert!(
            rsvp_fn.contains("JSON.parse(JSON.stringify(event))"),
            "should deep-clone event for revert"
        );
        assert!(
            rsvp_fn.contains("if (prevEvent)"),
            "should check prevEvent in catch block for revert"
        );
    }

    #[test]
    fn app_js_updates_cache_after_rsvp() {
        // After successful RSVP, the email cache should be updated
        let rsvp_fn_pos = APP_JS.find("async function rsvpToEvent").unwrap();
        let rsvp_fn = &APP_JS[rsvp_fn_pos..rsvp_fn_pos + 1500];
        assert!(
            rsvp_fn.contains("emailCache[cacheKey(state.currentEmail.id)] = state.currentEmail"),
            "should update emailCache after RSVP (account-scoped key via cacheKey)"
        );
    }

    #[test]
    fn app_js_keyboard_y_only_in_detail_view_with_calendar() {
        // y shortcut should only fire in detail view with a calendar event
        let y_pos = APP_JS.find("case 'y':").unwrap();
        let y_block = &APP_JS[y_pos..y_pos + 200];
        assert!(y_block.contains("state.view === 'detail'"));
        assert!(y_block.contains("calendarEvent"));
    }

    #[test]
    fn app_js_keyboard_n_only_in_detail_view_with_calendar() {
        // n shortcut should only fire in detail view with a calendar event
        let n_pos = APP_JS.find("case 'n':").unwrap();
        let n_block = &APP_JS[n_pos..n_pos + 200];
        assert!(n_block.contains("state.view === 'detail'"));
        assert!(n_block.contains("calendarEvent"));
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

    // Regression tripwires: these verify that key elements/patterns exist in
    // the embedded static assets. They catch accidental removal but not
    // behavioral correctness — a DOM/integration test suite would do that.

    #[test]
    fn index_html_has_account_error_banner() {
        assert!(
            INDEX_HTML.contains("id=\"account-error-banner\""),
            "index.html should contain the account error banner element"
        );
        assert!(
            INDEX_HTML.contains("id=\"account-error-details\""),
            "index.html should contain the account error details element"
        );
        assert!(
            INDEX_HTML.contains("error-banner-dismiss"),
            "index.html should contain the dismiss button"
        );
    }

    #[test]
    fn app_js_show_account_errors_escapes_html() {
        assert!(
            APP_JS.contains("escapeHtml("),
            "showAccountErrors should use escapeHtml for XSS prevention"
        );
    }

    #[test]
    fn app_js_renders_pending_accounts_in_selector() {
        // Pending (configured-but-unauthorized) accounts are now included in
        // GET /api/accounts; the sidebar must label them instead of rendering
        // "undefined" for a missing email.
        assert!(
            APP_JS.contains("acc.email || acc.id"),
            "account selector should fall back to the account id when email is absent"
        );
        assert!(
            APP_JS.contains("needs auth"),
            "account selector should mark pending accounts as needing authorization"
        );
    }

    #[test]
    fn app_js_routes_pending_account_clicks_to_authorize() {
        // Clicking/selecting a pending account must not fire mailbox fetches
        // that can only fail — it routes into the authorize flow instead.
        assert!(
            APP_JS.contains("account.authStatus === 'pending'"),
            "selectAccount should guard against pending accounts"
        );
    }

    #[test]
    fn app_js_never_fires_oauth_authorize_for_fastmail() {
        // Roborev job 267 finding #1: a Fastmail account whose connect failed
        // is listed as pending; routing it into POST /authorize can only 400.
        // The authorize entry point must branch to the edit form instead.
        // Substring tripwire only — it pins the guard expression in
        // `authorizeAccountFromBanner`, so update it in lockstep with that
        // function (a rename of the `acct` local breaks this; removal of the
        // guard elsewhere would not).
        assert!(
            APP_JS.contains("acct.provider === 'fastmail'"),
            "authorizeAccountFromBanner should route fastmail to the edit form, not OAuth"
        );
    }

    #[test]
    fn app_js_banner_heading_not_connection_specific_for_config_notices() {
        // The stale-config banner (provider "config") must not render under
        // a "failed to connect" heading with empty parentheses.
        // Substring tripwire — update in lockstep with `showAccountErrors`.
        assert!(
            APP_JS.contains("attention:"),
            "banner should use a neutral heading when non-connection notices are present"
        );
        assert!(
            APP_JS.contains("e.provider !== 'config'"),
            "banner heading should branch on the config provider label"
        );
    }

    #[test]
    fn app_js_auto_selects_only_connected_accounts() {
        assert!(
            APP_JS.contains("a.authStatus !== 'pending'"),
            "loadAccounts should auto-select from connected accounts only"
        );
    }

    #[test]
    fn app_js_loads_accounts_from_envelope() {
        assert!(
            APP_JS.contains("data.accounts"),
            "loadAccounts should destructure accounts from envelope"
        );
        assert!(
            APP_JS.contains("data.errors"),
            "loadAccounts should check for errors in envelope"
        );
    }

    #[test]
    fn style_css_has_error_banner_styles() {
        assert!(
            STYLE_CSS.contains("#account-error-banner"),
            "style.css should have error banner styles"
        );
        assert!(
            STYLE_CSS.contains(".error-banner-dismiss"),
            "style.css should have dismiss button styles"
        );
    }

    // =========================================================================
    // Roborev 186 hardening sentinels
    // =========================================================================

    #[test]
    fn send_invite_rejects_end_at_or_before_start() {
        // Roborev 186 #7: handler must reject negative-duration invites.
        let src = include_str!("routes.rs");
        assert!(
            src.contains("end time must be after start time"),
            "send_invite_handler must check dtend > dtstart"
        );
    }

    #[test]
    fn send_invite_passes_attachments_through() {
        // Roborev 186 #6: invite path must accept and forward attachments
        // instead of silently dropping them.
        let src = include_str!("routes.rs");
        assert!(
            src.contains("attachments: body.attachments"),
            "send_invite_handler must thread attachments into EmailSubmission"
        );
        assert!(
            APP_JS.contains("attachments: readyAttachments.length ? readyAttachments : undefined"),
            "the invite POST in app.js must pass readyAttachments through"
        );
    }

    // ====================================================================
    // Timezone + invite endpoints (roborev 188 carryover #8)
    // ====================================================================

    #[test]
    fn timezone_body_deserializes_with_defaults() {
        let body: TimezoneConfigBody = serde_json::from_str("{}").unwrap();
        assert!(body.use_system);
        assert!(body.manual_primary.is_none());
        assert!(body.additional.is_empty());
    }

    #[test]
    fn timezone_body_deserializes_full() {
        let body: TimezoneConfigBody = serde_json::from_str(
            r#"{"use_system":false,"manual_primary":"Europe/London","additional":["America/New_York"]}"#,
        )
        .unwrap();
        assert!(!body.use_system);
        assert_eq!(body.manual_primary.as_deref(), Some("Europe/London"));
        assert_eq!(body.additional, vec!["America/New_York".to_string()]);
    }

    #[test]
    fn dismiss_timezone_body_accepts_missing_seen_system() {
        // Empty body must parse — clients written before the TOCTOU fix
        // don't send `seen_system` and should still work.
        let body: DismissTimezoneBody = serde_json::from_str("{}").unwrap();
        assert!(body.seen_system.is_none());
    }

    #[test]
    fn dismiss_timezone_body_accepts_seen_system() {
        let body: DismissTimezoneBody =
            serde_json::from_str(r#"{"seen_system":"America/Denver"}"#).unwrap();
        assert_eq!(body.seen_system.as_deref(), Some("America/Denver"));
    }

    #[test]
    fn send_invite_body_minimal() {
        let body: SendInviteBody = serde_json::from_str(
            r#"{
                "to":["bob@example.com"],
                "subject":"Sync",
                "summary":"Quick sync",
                "start":"2026-06-01T10:00:00",
                "end":"2026-06-01T11:00:00"
            }"#,
        )
        .unwrap();
        assert_eq!(body.to, vec!["bob@example.com".to_string()]);
        assert!(body.cc.is_empty());
        assert!(body.tz.is_none());
        assert!(body.attendees.is_empty());
    }

    #[test]
    fn timezone_routes_are_registered() {
        // Sentinel: the router string must mention each new endpoint so a
        // refactor that drops the .route(...) line is caught at test time.
        let src = include_str!("routes.rs");
        for path in [
            "/api/timezone",
            "/api/timezone/accept-system",
            "/api/timezone/dismiss-change",
            "/api/timezone/zones",
            "/api/calendar/invite",
        ] {
            assert!(
                src.contains(&format!("\"{path}\"")),
                "router must register {path}"
            );
        }
    }

    #[test]
    fn frontend_sends_seen_system_on_dismiss() {
        // Roborev 188 #1F: client must POST the seen system TZ so the server
        // can refuse to dismiss a change the user never saw.
        assert!(
            APP_JS.contains("seen_system"),
            "dismissTimezoneChange must send seen_system in the body"
        );
    }

    #[test]
    fn frontend_renders_multi_tz_event_card() {
        // The multi-TZ rendering path on the event card must exist.
        assert!(
            APP_JS.contains("formatEventTimeMultiTz"),
            "renderCalendarCard must use the multi-TZ formatter"
        );
    }

    // ============================================================================
    // Mobile: mailbox nav + split tabs (kata 1wdy, task A8)
    // ============================================================================

    #[test]
    fn mobile_app_js_undo_entry_uses_current_mailbox() {
        // A4's undo entries pinned the inbox id; once mailbox switching
        // exists, undoing an archive/trash from any other mailbox must
        // restore there, not to the inbox.
        assert!(
            !MOBILE_APP_JS.contains("state.inboxId"),
            "inbox pinning must be fully replaced by state.currentMailbox"
        );
        let start = MOBILE_APP_JS
            .find("function pushUndo(")
            .expect("pushUndo must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("pushUndo must close");
        assert!(
            rest[..end].contains("state.currentMailbox"),
            "undo entries must record the CURRENT mailbox, not a pinned inbox"
        );
    }

    #[test]
    fn mobile_app_js_email_list_path_has_split_id_wiring() {
        let start = MOBILE_APP_JS
            .find("function emailListPath(")
            .expect("emailListPath must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("emailListPath must close");
        let body = &rest[..end];
        assert!(
            body.contains("split_id"),
            "emailListPath must append split_id to the query"
        );
        assert!(
            body.contains("role === 'inbox'") && body.contains("currentSplit !== 'all'"),
            "split_id must only be appended for the inbox with a non-'all' split selected \
             (mirrors desktop buildEmailListUrl)"
        );
    }

    #[test]
    fn mobile_app_js_has_mailbox_bottom_nav() {
        assert!(
            MOBILE_APP_JS.contains("function renderBottomNav("),
            "mobile app.js must render the bottom nav"
        );
        assert!(
            MOBILE_APP_JS.contains("function selectMailbox("),
            "mobile app.js must have a mailbox-selection entry point"
        );
        assert!(
            MOBILE_APP_JS.contains("bottom-nav"),
            "mobile app.js must reference the bottom-nav element"
        );
    }

    #[test]
    fn mobile_app_js_has_split_tabs() {
        for sym in [
            "function renderSplitTabs(",
            "function selectSplit(",
            "function loadSplits(",
        ] {
            assert!(
                MOBILE_APP_JS.contains(sym),
                "mobile app.js must define {sym}"
            );
        }
        assert!(
            MOBILE_APP_JS.contains("split-count"),
            "split tabs must render a count badge"
        );
    }

    #[test]
    fn mobile_app_js_load_splits_owns_dedicated_abort_controller() {
        // roborev 288: loadSplits used to share state.loadAbort with the
        // list-load abort protocol — any selectMailbox/selectSplit landing
        // before /splits resolved silently aborted it with nothing to
        // retry, permanently losing the split tabs for the session. It must
        // instead own its own controller (mirroring splitCountsController),
        // aborted and recreated only inside loadSplits itself.
        let start = MOBILE_APP_JS
            .find("async function loadSplits(")
            .expect("loadSplits must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("loadSplits must close");
        let block = &rest[..end];
        assert!(
            block.contains("splitsController?.abort()")
                && block.contains("splitsController = new AbortController()"),
            "loadSplits must abort and recreate its own dedicated AbortController"
        );
        assert!(
            !block.contains("state.loadAbort"),
            "loadSplits must not share state.loadAbort — a list-load switch \
             must not silently kill the in-flight splits fetch"
        );
        // The two call sites (selectAccount, restoreFromSnapshot) must call
        // it with just the account id — no shared signal to couple it back
        // to the list-load abort protocol.
        assert!(
            MOBILE_APP_JS.contains("loadSplits(acct)"),
            "loadSplits call sites must not pass state.loadAbort.signal"
        );
    }

    #[test]
    fn mobile_app_js_list_switches_abort_inflight_load() {
        // Review follow-up: loadEmails guards re-entry with a blocking
        // `loading` mutex (unlike desktop's self-aborting loads), so every
        // synchronous re-issue path must go through abortListLoad() — abort
        // the in-flight request AND release the mutex — or the new request
        // is silently dropped (highlighted tab/nav over a stale list).
        for func in [
            "function selectAccount(",
            "function selectMailbox(",
            "function selectSplit(",
        ] {
            let start = MOBILE_APP_JS.find(func).expect("function must exist");
            let rest = &MOBILE_APP_JS[start..];
            let end = rest.find("\n}").expect("function must close");
            assert!(
                rest[..end].contains("abortListLoad()"),
                "{func} must abort the in-flight list load before reloading"
            );
        }
        let start = MOBILE_APP_JS
            .find("function abortListLoad(")
            .expect("abortListLoad must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("abortListLoad must close");
        assert!(
            rest[..end].contains("state.loading = false"),
            "abortListLoad must release loadEmails's loading mutex"
        );
    }

    #[test]
    fn mobile_toast_stack_rides_above_bottom_nav_on_list() {
        // Review follow-up: the fixed toast stack and the bottom nav both
        // anchor to the viewport bottom on LIST — the stack must be lifted
        // clear of the nav band there (and only there).
        assert!(
            MOBILE_HTML.contains("#toast-stack.nav-visible"),
            "mobile html must offset the toast stack above the bottom nav"
        );
        assert!(
            MOBILE_HTML.contains(r#"id="toast-stack" class="nav-visible""#),
            "toast stack must boot lifted — LIST shows without an initial setScreen call"
        );
        let start = MOBILE_APP_JS
            .find("function setScreen(")
            .expect("setScreen must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("setScreen must close");
        assert!(
            rest[..end].contains("nav-visible"),
            "setScreen must toggle the toast stack's nav offset with the screen"
        );
    }

    #[test]
    fn mobile_html_has_bottom_nav_and_split_tabs() {
        assert!(
            MOBILE_HTML.contains(r#"id="bottom-nav""#),
            "mobile html must have a bottom nav element"
        );
        assert!(
            MOBILE_HTML.contains(r#"id="split-tabs""#),
            "mobile html must have a split-tabs row element"
        );
        for role in ["inbox", "archive", "sent", "drafts", "trash"] {
            assert!(
                MOBILE_HTML.contains(&format!(r#"data-role="{role}""#)),
                "bottom nav must include a {role} item"
            );
        }
        assert!(
            !MOBILE_HTML.contains(r#"data-role="spam""#),
            "v1 bottom nav excludes spam and role-less custom folders"
        );
    }

    #[test]
    fn mobile_html_has_search_input() {
        assert!(
            MOBILE_HTML.contains(r#"id="search-btn""#),
            "mobile html must have a header search icon button"
        );
        assert!(
            MOBILE_HTML.contains(r#"id="search-input""#),
            "mobile html must have a search input"
        );
        assert!(
            MOBILE_HTML.contains(r#"type="search""#),
            "search input must use type=search for the mobile keyboard's search affordance"
        );
        assert!(
            MOBILE_HTML.contains(r#"enterkeyhint="search""#),
            "search input must set enterkeyhint=search so the keyboard's action key reads Search"
        );
        assert!(
            MOBILE_HTML.contains(r#"id="search-clear-btn""#),
            "mobile html must have a clear (\u{2715}) affordance in the search bar"
        );
    }

    #[test]
    fn mobile_app_js_has_search_state_and_wiring() {
        assert!(
            MOBILE_APP_JS.contains("searchQuery:"),
            "state must track the active search query"
        );
        for func in [
            "function openSearch(",
            "function closeSearchBar(",
            "function clearSearch(",
            "function submitSearch(",
        ] {
            assert!(
                MOBILE_APP_JS.contains(func),
                "mobile app.js must define {func}"
            );
        }
    }

    #[test]
    fn mobile_app_js_email_list_path_combines_search_with_split() {
        // Desktop's buildEmailListUrl (static/app.js) appends split_id and
        // search independently — both can be present on the same request,
        // they are not mutually exclusive. Mobile must mirror that exactly,
        // not the task brief's initial (incorrect) assumption that search
        // suppresses the split filter.
        let start = MOBILE_APP_JS
            .find("function emailListPath(")
            .expect("emailListPath must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("emailListPath must close");
        let body = &rest[..end];
        assert!(
            body.contains("state.searchQuery"),
            "emailListPath must append the active search query"
        );
        assert!(
            body.contains("&search="),
            "emailListPath must append &search= like desktop's buildEmailListUrl"
        );
        let split_idx = body.find("split_id").expect("split_id branch must exist");
        let search_idx = body.find("&search=").expect("search branch must exist");
        assert!(
            split_idx < search_idx,
            "split_id must be appended before search, mirroring desktop's ordering"
        );
        assert!(
            !body[split_idx..search_idx].contains("else"),
            "split_id and search must not be mutually exclusive branches — desktop combines them"
        );
    }

    #[test]
    fn mobile_app_js_search_clears_on_mailbox_and_account_switch_only() {
        // Desktop clears state.searchTokens in selectMailbox (and
        // selectAccount funnels through a mailbox reset); selectSplit does
        // NOT clear search — search persists across split tabs, only a
        // mailbox/account switch drops it.
        for func in ["function selectAccount(", "function selectMailbox("] {
            let start = MOBILE_APP_JS.find(func).expect("function must exist");
            let rest = &MOBILE_APP_JS[start..];
            let end = rest.find("\n}").expect("function must close");
            assert!(
                rest[..end].contains("state.searchQuery = ''"),
                "{func} must clear the active search query"
            );
        }
        let start = MOBILE_APP_JS
            .find("function selectSplit(")
            .expect("selectSplit must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("selectSplit must close");
        assert!(
            !rest[..end].contains("searchQuery"),
            "selectSplit must not clear search — split and search combine, mirroring desktop"
        );
    }

    #[test]
    fn mobile_app_js_search_actions_use_abort_list_load() {
        // Same abort/reload protocol as selectAccount/selectMailbox/
        // selectSplit (abortListLoad guards every list switch, kata 1wdy) —
        // a committed or cleared search reloads the list the same way.
        for func in ["function submitSearch(", "function clearSearch("] {
            let start = MOBILE_APP_JS.find(func).expect("function must exist");
            let rest = &MOBILE_APP_JS[start..];
            let end = rest.find("\n}").expect("function must close");
            assert!(
                rest[..end].contains("abortListLoad()"),
                "{func} must guard its reload with abortListLoad()"
            );
        }
    }

    // Review follow-up: mirrors mobile_app_js_mailbox_and_split_switches_
    // hide_undo_toast — a search transition splices in a different result
    // set just like a mailbox/split switch does, so an undo toast left over
    // from before the search invites tapping Undo into a list it no longer
    // describes (or splicing into the new results at a stale index).
    // submitSearch/clearSearch already guard their reload with
    // abortListLoad(); they must hide the pending toast too.
    #[test]
    fn mobile_app_js_search_actions_hide_undo_toast() {
        for func in ["function submitSearch(", "function clearSearch("] {
            let start = MOBILE_APP_JS.find(func).expect("function must exist");
            let rest = &MOBILE_APP_JS[start..];
            let end = rest.find("\n}").expect("function must close");
            assert!(
                rest[..end].contains("hideUndoToast(undoToastEntry)"),
                "{func} must hide any pending undo toast on a search transition"
            );
        }
    }

    // Mobile: calendar RSVP card (kata nhxd, task A10)

    #[test]
    fn mobile_app_js_replaces_calendar_indicator_with_card() {
        // The old one-line indicator (pre-A10) is gone — detail now renders
        // the full calendarEvent that GET /emails/:id already returns.
        assert!(
            !MOBILE_APP_JS.contains("calendar-indicator"),
            "mobile should render the full calendar card, not the old indicator"
        );
        assert!(
            MOBILE_APP_JS.contains("function renderCalendarCard("),
            "mobile needs a renderCalendarCard function, mirroring desktop's semantics"
        );
    }

    #[test]
    fn mobile_app_js_calendar_card_shows_desktop_parity_fields() {
        let start = MOBILE_APP_JS
            .find("function renderCalendarCard(")
            .expect("renderCalendarCard must exist");
        let render_fn = &MOBILE_APP_JS[start..start + 2600];
        for field in [
            "event.summary",
            "event.location",
            "organizer",
            "event.attendees",
        ] {
            assert!(
                render_fn.contains(field),
                "renderCalendarCard should render {field}"
            );
        }
    }

    #[test]
    fn mobile_app_js_calendar_card_escapes_attacker_controlled_fields() {
        // summary/location/organizer come straight from the ICS payload
        // (src/types.rs CalendarEvent) — attacker-controlled, must be escaped.
        let start = MOBILE_APP_JS
            .find("function renderCalendarCard(")
            .expect("renderCalendarCard must exist");
        let render_fn = &MOBILE_APP_JS[start..start + 2600];
        assert!(
            render_fn.contains("escapeHtml(event.summary"),
            "summary must be escaped"
        );
        assert!(
            render_fn.contains("escapeHtml(event.location"),
            "location must be escaped"
        );
        assert!(
            render_fn.contains("escapeHtml(organizerLabel"),
            "organizer must be escaped"
        );
    }

    #[test]
    fn mobile_app_js_calendar_card_hides_actions_for_cancelled_and_publish() {
        let start = MOBILE_APP_JS
            .find("function renderCalendarCard(")
            .expect("renderCalendarCard must exist");
        let render_fn = &MOBILE_APP_JS[start..start + 2600];
        assert!(
            render_fn.contains("=== 'CANCEL'"),
            "cancelled events must hide RSVP actions, mirroring desktop"
        );
        assert!(
            render_fn.contains("!== 'PUBLISH'"),
            "PUBLISH events (no attendee to respond as — server never sets \
             user_rsvp_status for them, see routes.rs get_email) must also hide RSVP actions"
        );
    }

    #[test]
    fn mobile_app_js_calendar_card_highlights_current_status() {
        let start = MOBILE_APP_JS
            .find("function renderCalendarCard(")
            .expect("renderCalendarCard must exist");
        let render_fn = &MOBILE_APP_JS[start..start + 2600];
        for status in ["ACCEPTED", "TENTATIVE", "DECLINED"] {
            assert!(
                render_fn.contains(&format!("userStatus === '{status}'")),
                "should highlight the active RSVP button for {status}"
            );
        }
        assert!(
            render_fn.contains("You responded"),
            "should render the 'You responded X' label"
        );
    }

    #[test]
    fn mobile_app_js_has_rsvp_route_call() {
        let start = MOBILE_APP_JS
            .find("async function rsvpToEvent(")
            .expect("rsvpToEvent must exist");
        let rsvp_fn = &MOBILE_APP_JS[start..start + 1000];
        assert!(
            rsvp_fn.contains("state.api('POST',"),
            "rsvpToEvent should post through state.api"
        );
        assert!(
            rsvp_fn.contains("/rsvp'"),
            "rsvpToEvent should hit the /emails/:id/rsvp route"
        );
        assert!(
            rsvp_fn.contains("{ status }"),
            "rsvpToEvent should send {{ status }} as the request body"
        );
    }

    #[test]
    fn mobile_app_js_rsvp_optimistic_flip_reverts_on_failure() {
        let start = MOBILE_APP_JS
            .find("async function rsvpToEvent(")
            .expect("rsvpToEvent must exist");
        let rsvp_fn = &MOBILE_APP_JS[start..start + 1000];
        assert!(
            rsvp_fn.contains("event.user_rsvp_status = status"),
            "should optimistically set user_rsvp_status before the request"
        );
        assert!(rsvp_fn.contains("catch"), "should handle request failure");
        assert!(
            rsvp_fn.contains("event.user_rsvp_status = prevStatus"),
            "should revert user_rsvp_status on failure"
        );
        assert!(
            rsvp_fn.contains("showError("),
            "should report RSVP failures through showError, the only failure sink"
        );
    }

    #[test]
    fn mobile_app_js_rsvp_buttons_delegated_from_detail_calendar() {
        // Delegated like detail-attachments — a fresh innerHTML per render
        // never needs its own rebind.
        assert!(
            MOBILE_APP_JS.contains("getElementById('detail-calendar').addEventListener('click'"),
            "RSVP buttons must be wired via delegation on #detail-calendar"
        );
        assert!(
            MOBILE_APP_JS.contains(".closest('.rsvp-btn')"),
            "click delegation should target .rsvp-btn"
        );
    }

    #[test]
    fn mobile_app_js_has_no_manual_add_to_calendar_button() {
        // Desktop never calls POST /emails/:id/add-to-calendar either —
        // REQUEST events are auto-added server-side (get_email). A manual
        // button would be redundant chrome (kata nhxd, task A10 parity check).
        assert!(
            !MOBILE_APP_JS.contains("add-to-calendar"),
            "mobile should not add a manual add-to-calendar action — server auto-adds on REQUEST"
        );
    }

    #[test]
    fn mobile_html_still_has_detail_calendar_container() {
        assert!(
            MOBILE_HTML.contains(r#"id="detail-calendar""#),
            "detail-calendar container must remain the mount point for the rendered card"
        );
    }

    // Mobile: unsubscribe & archive all (kata 6chy, task A11)

    #[test]
    fn mobile_html_has_detail_more_button_and_unsub_sheet() {
        assert!(
            MOBILE_HTML.contains(r#"id="detail-more-btn""#),
            "detail action bar needs an overflow (\u{22ef}) button as the entry point"
        );
        assert!(
            MOBILE_HTML.contains(r#"id="unsub-sheet""#),
            "needs the unsubscribe confirmation sheet container"
        );
        assert!(
            MOBILE_HTML.contains(r#"id="unsub-sheet-confirm""#),
            "sheet needs a confirm row ('Unsubscribe & archive all from <sender>')"
        );
        assert!(
            MOBILE_HTML.contains(r#"id="unsub-sheet-cancel""#),
            "sheet needs a Cancel row"
        );
    }

    #[test]
    fn mobile_html_unsub_sheet_reuses_account_picker_bottom_sheet_pattern() {
        // Reuse, not a parallel copy: the overlay class the account picker
        // already uses should be shared, not reinvented per sheet.
        let overlay_class_start = MOBILE_HTML
            .find(r#"id="account-picker""#)
            .expect("account picker must exist");
        let snippet = &MOBILE_HTML[overlay_class_start..overlay_class_start + 200];
        let shared_class = if snippet.contains("class=\"sheet-overlay") {
            "sheet-overlay"
        } else {
            panic!("account picker should carry a shared overlay class for other sheets to reuse")
        };
        assert!(
            MOBILE_HTML.match_indices(shared_class).count() >= 2,
            "unsub-sheet should reuse the same overlay class as account-picker, not duplicate its CSS"
        );
    }

    #[test]
    fn mobile_app_js_unsub_entry_point_wired_from_detail_action_bar() {
        assert!(
            MOBILE_APP_JS.contains("getElementById('detail-more-btn')"),
            "the overflow button must be wired up"
        );
        assert!(
            MOBILE_APP_JS.contains("function showUnsubSheet("),
            "needs a function to populate + reveal the sheet with the current email's sender"
        );
    }

    #[test]
    fn mobile_app_js_has_unsub_route_call() {
        let start = MOBILE_APP_JS
            .find("async function unsubscribeAndArchiveAll(")
            .expect("unsubscribeAndArchiveAll must exist");
        let unsub_fn = &MOBILE_APP_JS[start..start + 1700];
        assert!(
            unsub_fn.contains("state.api('POST',"),
            "should post through state.api, same as every other mobile action"
        );
        assert!(
            unsub_fn.contains("/unsubscribe-and-archive-all"),
            "must hit the existing route — no new server endpoint (kata 6chy brief)"
        );
    }

    #[test]
    fn mobile_app_js_unsub_optimistic_removal_reverts_on_failure() {
        let start = MOBILE_APP_JS
            .find("async function unsubscribeAndArchiveAll(")
            .expect("unsubscribeAndArchiveAll must exist");
        let unsub_fn = &MOBILE_APP_JS[start..start + 1700];
        assert!(
            unsub_fn.contains("state.emails.filter("),
            "should optimistically filter all of the sender's rows out of the list"
        );
        assert!(unsub_fn.contains("catch"), "should handle request failure");
        assert!(
            unsub_fn.contains("state.emails.concat(removedEmails)")
                || unsub_fn.contains("state.emails = state.emails.concat("),
            "failure should revert by re-inserting the removed emails, mirroring desktop"
        );
        assert!(
            unsub_fn.contains("showError("),
            "should report failures through showError, the only failure sink"
        );
    }

    #[test]
    fn mobile_app_js_unsub_batch_has_no_undo_stack_entry() {
        // Explicitly out of scope per the brief — the batch bypasses
        // pushUndo/undoStack entirely, unlike single archive/trash.
        let start = MOBILE_APP_JS
            .find("async function unsubscribeAndArchiveAll(")
            .expect("unsubscribeAndArchiveAll must exist");
        let unsub_fn = &MOBILE_APP_JS[start..start + 1700];
        assert!(
            !unsub_fn.contains("pushUndo("),
            "batch unsubscribe/archive must not push an undo-stack entry (out of scope, see brief)"
        );
    }

    // Review follow-up: desktop's failure-path re-insert used to hardcode a
    // descending re-sort (`new Date(b...) - new Date(a...)`) regardless of
    // state.sortOrder — under date_asc that scrambled the list instead of
    // restoring it.
    #[test]
    fn app_js_unsub_revert_resort_respects_sort_order() {
        let start = APP_JS
            .find("async function unsubscribeAndArchiveAll(")
            .expect("unsubscribeAndArchiveAll must exist");
        let rest = &APP_JS[start..];
        let end = rest
            .find("\n}")
            .expect("unsubscribeAndArchiveAll must close");
        let block = &rest[..end];
        assert!(
            block.contains("state.sortOrder"),
            "desktop unsubscribeAndArchiveAll's revert must consult state.sortOrder"
        );
        assert!(
            !block.contains("new Date(b.receivedAt) - new Date(a.receivedAt)"),
            "must not hardcode a descending comparator regardless of sort order"
        );
    }

    // =========================================================================
    // Offline banner (kata 115b, task A12)
    // =========================================================================

    #[test]
    fn mobile_html_has_offline_banner() {
        assert!(
            MOBILE_HTML.contains(r#"id="offline-banner""#),
            "should have a persistent #offline-banner element"
        );
        assert!(
            MOBILE_HTML.contains("Offline") && MOBILE_HTML.contains("data may be stale"),
            "banner copy should tell the user data may be stale while offline"
        );
        // It must default hidden and toggle via a class (never .style.display,
        // see mobile_app_js_offline_banner_uses_class_toggle) — a normal-flow
        // element, not a fixed overlay, so it inherits the app-shell's own
        // safe-top padding for free like bottom-nav does for safe-bottom.
        assert!(
            MOBILE_HTML.contains("#offline-banner {") && MOBILE_HTML.contains("display: none;"),
            "banner should default hidden via CSS"
        );
        assert!(
            MOBILE_HTML.contains("#offline-banner.visible"),
            "banner visibility should be a class toggle, not a screen"
        );
    }

    #[test]
    fn mobile_app_js_offline_banner_uses_class_toggle() {
        // The banner is connectivity-driven chrome, not a screen — its
        // show/hide must NOT live inside setScreen or use .style.display,
        // or it would trip the display-ownership invariant (see
        // mobile_app_js_setscreen_owns_all_display_toggles above).
        assert!(
            MOBILE_APP_JS.contains("getElementById('offline-banner').classList.toggle("),
            "offline banner visibility should be a classList.toggle, not .style.display"
        );
    }

    #[test]
    fn mobile_app_js_has_offline_listeners() {
        assert!(
            MOBILE_APP_JS.contains("addEventListener('online',"),
            "should listen for the online event"
        );
        assert!(
            MOBILE_APP_JS.contains("addEventListener('offline',"),
            "should listen for the offline event"
        );
        assert!(
            MOBILE_APP_JS.contains("navigator.onLine"),
            "should check navigator.onLine at boot, not just wait for an event"
        );
    }

    #[test]
    fn mobile_app_js_offline_reconnect_refreshes_list() {
        // Reconnecting should clear the banner and refresh the list — same
        // abort/reload protocol as selectAccount/selectMailbox/selectSplit
        // (abortListLoad guards every list switch, kata 1wdy).
        let start = MOBILE_APP_JS
            .find("function handleOnline(")
            .expect("handleOnline must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("handleOnline body must close");
        let region = &rest[..end];
        assert!(
            region.contains("abortListLoad()"),
            "reconnect should abort any stale in-flight load first"
        );
        // Silent off the LIST screen (review follow-up): #status-bar is shell
        // chrome, not screen-scoped, so a non-silent reconnect refresh would
        // flash 'Loading...' beneath the detail/compose screen.
        assert!(
            region.contains("loadEmails({ silent: state.screen !== Screen.LIST })"),
            "reconnect should trigger a list refresh, silent off the LIST screen"
        );
    }

    // =========================================================================
    // State persistence across iOS PWA kills (kata mhck, task A13)
    // =========================================================================

    #[test]
    fn mobile_app_js_has_state_snapshot_key() {
        // A single versioned localStorage key holds the resume snapshot.
        assert!(
            MOBILE_APP_JS.contains("supervillain_mobile_state_v1"),
            "app.js should snapshot resume state under a versioned localStorage key"
        );
    }

    #[test]
    fn mobile_app_js_persists_on_lifecycle_events() {
        // iOS only reliably delivers visibilitychange→hidden and pagehide
        // before killing a backgrounded PWA — snapshot on both.
        assert!(
            MOBILE_APP_JS.contains("addEventListener('visibilitychange'"),
            "should snapshot on visibilitychange (iOS backgrounding)"
        );
        assert!(
            MOBILE_APP_JS.contains("addEventListener('pagehide'"),
            "should snapshot on pagehide (unload/bfcache)"
        );
        assert!(
            MOBILE_APP_JS.contains("visibilityState === 'hidden'"),
            "visibilitychange should only snapshot when actually going hidden"
        );
    }

    #[test]
    fn mobile_app_js_persist_excludes_ui_state() {
        // The snapshot is DATA state only. UI state — the open screen, the
        // current email, cached bodies, the undo stack, in-flight compose
        // attachments, the send lock — must never be persisted: restore always
        // lands on the LIST screen, so resurrecting DETAIL/COMPOSE-scoped state
        // would be both wrong and a way to reopen an email the server may have
        // moved on from.
        let start = MOBILE_APP_JS
            .find("function persistState(")
            .expect("persistState must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("persistState body must close");
        let region = &rest[..end];
        for banned in [
            "undoStack",
            "emailCache",
            "currentEmailId",
            "pendingAttachments",
            "sending",
        ] {
            assert!(
                !region.contains(banned),
                "persistState must not persist UI state ({banned})"
            );
        }
        // …but it MUST carry the data fields restore validates and renders.
        assert!(
            region.contains("accountId") && region.contains("savedAt") && region.contains("emails"),
            "the snapshot must carry accountId, savedAt, and the list rows"
        );
        assert!(
            region.contains("MOBILE_STATE_KEY"),
            "persistState should write the versioned state key"
        );
    }

    #[test]
    fn mobile_app_js_persist_is_write_guarded() {
        // Quota/private-mode denials must degrade to a console.warn, never a
        // user-facing toast — a failed background snapshot isn't the user's
        // problem.
        let start = MOBILE_APP_JS
            .find("function persistState(")
            .expect("persistState must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("persistState body must close");
        let region = &rest[..end];
        assert!(
            region.contains("try") && region.contains("catch") && region.contains("console.warn"),
            "the snapshot write must be try/catch-guarded and log, not toast"
        );
        assert!(
            !region.contains("showError") && !region.contains("showToast"),
            "a failed snapshot must never surface a toast"
        );
    }

    #[test]
    fn mobile_app_js_restore_refreshes_silently_and_stays_on_list() {
        // Restore renders the snapshot list instantly, then refreshes it in the
        // background via a SILENT load (the list is already on screen — a
        // non-silent 'Loading...' would flash over it). It NEVER opens detail
        // or compose, so nothing can be yanked out from under the user.
        let start = MOBILE_APP_JS
            .find("function restoreFromSnapshot(")
            .expect("restoreFromSnapshot must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest
            .find("\n}")
            .expect("restoreFromSnapshot body must close");
        let region = &rest[..end];
        assert!(
            region.contains("loadEmails({ silent: true })"),
            "restore must refresh the list with a silent load"
        );
        assert!(
            !region.contains("Screen.DETAIL")
                && !region.contains("Screen.COMPOSE")
                && !region.contains("navigateTo"),
            "restore must never enter DETAIL/COMPOSE"
        );
    }

    #[test]
    fn mobile_app_js_restore_validity_guards() {
        // A snapshot is only applied when its account is still connected and it
        // is fresh (< 24h). Anything else is removed and the app takes its
        // normal default-account boot.
        let start = MOBILE_APP_JS
            .find("function restoreFromSnapshot(")
            .expect("restoreFromSnapshot must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest
            .find("\n}")
            .expect("restoreFromSnapshot body must close");
        let region = &rest[..end];
        assert!(
            region.contains("connectedAccounts()"),
            "restore must check the saved account is still connected"
        );
        assert!(
            region.contains("savedAt") && region.contains("STATE_MAX_AGE_MS"),
            "restore must check snapshot freshness via savedAt against the 24h max age"
        );
        assert!(
            region.contains("removeItem"),
            "an invalid/corrupt snapshot must be removed, not left to rot"
        );
        assert!(
            region.contains("Refreshing"),
            "restore should show a refreshing indicator over the stale list"
        );
    }

    #[test]
    fn mobile_app_js_restore_runs_after_load_accounts() {
        // Restore lives in init() after loadAccounts() succeeds (it needs the
        // connected-account list to validate the snapshot), and the no-snapshot
        // path falls through to the original default-account boot unchanged.
        let start = MOBILE_APP_JS
            .find("async function init(")
            .expect("init must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("init body must close");
        let region = &rest[..end];
        let load_accounts = region
            .find("await loadAccounts()")
            .expect("init loads accounts");
        let restore = region
            .find("restoreFromSnapshot(")
            .expect("init attempts restore");
        assert!(
            restore > load_accounts,
            "restore must run after loadAccounts() so the connected-account list is available"
        );
        // First-run byte-identical: the default-account selection still runs.
        assert!(
            region.contains("selectAccount(defaultAcc)"),
            "the no-snapshot boot must still select the default account unchanged"
        );
    }

    #[test]
    fn mobile_app_js_snapshots_after_successful_load() {
        // Belt-and-suspenders: keep the snapshot warm after every successful
        // list load, in case iOS kills us without firing visibilitychange.
        let start = MOBILE_APP_JS
            .find("async function loadEmails(")
            .expect("loadEmails must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("loadEmails body must close");
        let region = &rest[..end];
        assert!(
            region.contains("persistState()"),
            "loadEmails should refresh the snapshot after a successful load"
        );
    }

    #[test]
    fn mobile_app_js_offline_boot_attempts_degraded_restore() {
        // The SW never caches /api/*, so an offline relaunch fails
        // loadAccounts — init()'s catch must attempt a DEGRADED restore
        // (freshness gate only; connectedness can't be checked without an
        // account list) instead of dead-ending on 'Cannot reach server'.
        let start = MOBILE_APP_JS
            .find("async function init(")
            .expect("init must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("init body must close");
        let region = &rest[..end];
        assert!(
            region.contains("restoreFromSnapshot({ offline: true })"),
            "init's loadAccounts catch must attempt a degraded offline restore"
        );
        assert!(
            region.contains("Cannot reach server"),
            "with no usable snapshot the offline boot must keep today's error path"
        );
        // The degraded path synthesizes the account from the snapshot alone,
        // so the snapshot must carry the account email for the header button.
        let pstart = MOBILE_APP_JS
            .find("function persistState(")
            .expect("persistState must exist");
        let prest = &MOBILE_APP_JS[pstart..];
        let pend = prest.find("\n}").expect("persistState body must close");
        assert!(
            prest[..pend].contains("accountEmail"),
            "the snapshot must carry accountEmail for the degraded offline restore"
        );
    }

    #[test]
    fn mobile_app_js_reconnect_recovers_degraded_boot() {
        // A degraded offline boot has no account list, mailboxes, splits, or
        // identities — reconnecting must re-run the whole boot (init(): fresh
        // loadAccounts, then the same snapshot restores in full online mode
        // with the refresh cascade armed), not just refresh the list against
        // half-initialized state.
        let start = MOBILE_APP_JS
            .find("function handleOnline(")
            .expect("handleOnline must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("handleOnline body must close");
        let region = &rest[..end];
        assert!(
            region.contains("state.accounts.length") && region.contains("init()"),
            "handleOnline must re-run init() when the account list never loaded"
        );
    }

    #[test]
    fn mobile_app_js_restore_awaits_splits_before_filtered_refresh() {
        // emailListPath only appends split_id once state.splits is non-empty —
        // a refresh that runs before the split definitions land would fetch
        // the UNFILTERED inbox and nothing would re-fetch. With a saved split
        // filter, the splits fetch must settle before the loadEmails leg.
        let start = MOBILE_APP_JS
            .find("function restoreFromSnapshot(")
            .expect("restoreFromSnapshot must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest
            .find("\n}")
            .expect("restoreFromSnapshot body must close");
        let region = &rest[..end];
        assert!(
            region.contains("splitsPromise"),
            "restore must hold a handle to the in-flight splits fetch"
        );
        let split_gate = region
            .find("snapshot.splitId !== 'all'")
            .expect("restore must gate on a saved split filter");
        let load = region
            .find("loadEmails({ silent: true })")
            .expect("restore must refresh silently");
        assert!(
            split_gate < load,
            "the saved-split gate must sequence the splits fetch before the refresh"
        );
    }

    #[test]
    fn mobile_app_js_persist_preserves_mailbox_during_restore_window() {
        // persistState can fire while currentMailbox is still null (the
        // restore window before mailboxes land, or a whole degraded offline
        // session) — writing mailboxRole: null would lose the saved mailbox
        // on the NEXT resume. The previous snapshot's mailbox fields must be
        // preserved instead.
        let start = MOBILE_APP_JS
            .find("function persistState(")
            .expect("persistState must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("persistState body must close");
        let region = &rest[..end];
        assert!(
            region.contains("prev.mailboxRole"),
            "a null currentMailbox must fall back to the previous snapshot's mailbox"
        );
    }

    // =========================================================================
    // roborev 289 follow-up: restore split validation, fetch-time freshness,
    // visible restored search
    // =========================================================================

    #[test]
    fn mobile_app_js_restore_resets_deleted_split_before_refresh() {
        // A split can be deleted server-side between snapshot and restore.
        // splitsPromise has settled by the time this runs (see
        // mobile_app_js_restore_awaits_splits_before_filtered_refresh) — an
        // unrecognized non-'all' split must fall back to 'all' and re-render
        // the tabs BEFORE the loadEmails leg, or split_id=<deleted> silently
        // returns an empty list with no highlighted tab.
        let start = MOBILE_APP_JS
            .find("function restoreFromSnapshot(")
            .expect("restoreFromSnapshot must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest
            .find("\n}")
            .expect("restoreFromSnapshot body must close");
        let region = &rest[..end];
        assert!(
            region.contains("state.splits.some(s => s.id === state.currentSplit)"),
            "restore must check the restored split still exists among state.splits"
        );
        assert!(
            region.contains("state.currentSplit = 'all';") && region.contains("renderSplitTabs();"),
            "restore must fall back to 'all' and re-render tabs when the split is missing"
        );
        let validate = region
            .find("state.splits.some(s => s.id === state.currentSplit)")
            .expect("split-validity check must be present");
        let load = region
            .find("loadEmails({ silent: true })")
            .expect("restore must refresh silently");
        assert!(
            validate < load,
            "the split-validity check must run before the loadEmails leg fires"
        );
    }

    #[test]
    fn mobile_app_js_tracks_fetch_time_separately_from_persist_time() {
        // savedAt must reflect when the rows were actually fetched, not when
        // persistState happened to run — otherwise a degraded offline
        // session's background snapshot cycle re-stamps stale rows fresh
        // every cycle and the 24h freshness gate never trips.
        assert!(
            MOBILE_APP_JS.contains("emailsFetchedAt:"),
            "state must track emailsFetchedAt separately from the persisted savedAt"
        );

        let start = MOBILE_APP_JS
            .find("async function loadEmails(")
            .expect("loadEmails must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("loadEmails body must close");
        assert!(
            rest[..end].contains("state.emailsFetchedAt = Date.now();"),
            "loadEmails' success path must stamp emailsFetchedAt when rows are actually fetched"
        );

        let pstart = MOBILE_APP_JS
            .find("function persistState(")
            .expect("persistState must exist");
        let prest = &MOBILE_APP_JS[pstart..];
        let pend = prest.find("\n}").expect("persistState body must close");
        assert!(
            prest[..pend].contains("savedAt: state.emailsFetchedAt ?? Date.now(),"),
            "persistState must stamp savedAt from emailsFetchedAt, not the current write time"
        );

        let rstart = MOBILE_APP_JS
            .find("function restoreFromSnapshot(")
            .expect("restoreFromSnapshot must exist");
        let rrest = &MOBILE_APP_JS[rstart..];
        let rend = rrest
            .find("\n}")
            .expect("restoreFromSnapshot body must close");
        assert!(
            rrest[..rend].contains("state.emailsFetchedAt = snapshot.savedAt;"),
            "restore must carry the snapshot's own fetch time forward into emailsFetchedAt"
        );
    }

    #[test]
    fn mobile_app_js_restore_opens_search_bar_without_focus() {
        // A restored non-empty searchQuery must surface visibly (the bar
        // open) rather than silently filter the list — but restoring
        // shouldn't pop the keyboard on a cold start, so this must add the
        // 'searching' class directly instead of calling openSearch() (which
        // also focuses the input).
        let start = MOBILE_APP_JS
            .find("function restoreFromSnapshot(")
            .expect("restoreFromSnapshot must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest
            .find("\n}")
            .expect("restoreFromSnapshot body must close");
        let region = &rest[..end];
        assert!(
            region.contains("if (state.searchQuery) {"),
            "restore must branch on a restored search query to open the bar"
        );
        assert!(
            region.contains("classList.add('searching')"),
            "restore must open the search bar for a restored query"
        );
        assert!(
            !region.contains("search-input').focus()"),
            "restore must not focus the search input — that would pop the keyboard on a cold start"
        );
        let value_set = region
            .find("search-input').value = state.searchQuery;")
            .expect("restore must fill the search input value");
        let open_bar = region
            .find("classList.add('searching')")
            .expect("restore must open the bar");
        assert!(
            value_set < open_bar,
            "the input value should be set before the bar becomes visible"
        );
    }

    #[test]
    fn mobile_app_js_submit_search_keeps_bar_open_while_input_focused() {
        // WebKit fires the 'search' event with an empty value the instant the
        // native cancel button is tapped. If the input still has focus (the
        // user tapped back in to keep typing), submitSearch must not close
        // the bar out from under them — clearSearch's close: false skips
        // that, keeping the explicit ✕ button's close-always behavior intact.
        let start = MOBILE_APP_JS
            .find("function submitSearch(")
            .expect("submitSearch must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("submitSearch body must close");
        let region = &rest[..end];
        assert!(
            region.contains("clearSearch({ close: document.activeElement !== input })"),
            "submitSearch's empty-value path must gate closing the bar on input focus"
        );

        let cstart = MOBILE_APP_JS
            .find("function clearSearch(")
            .expect("clearSearch must exist");
        let crest = &MOBILE_APP_JS[cstart..];
        let cend = crest.find("\n}").expect("clearSearch body must close");
        assert!(
            crest[..cend].contains("if (close) closeSearchBar();"),
            "clearSearch must make closing the bar conditional on its close param"
        );
    }

    #[test]
    fn mobile_app_js_init_guards_reentrancy() {
        // Bursty online/offline/online events can each call handleOnline,
        // which re-runs init() on a snapshot-less or degraded boot. Two
        // overlapping init() runs would race loadAccounts/restoreFromSnapshot/
        // selectAccount — guard with an in-flight boolean, released in a
        // finally so it can't get stuck true after an error or early return.
        assert!(
            MOBILE_APP_JS.contains("let initInFlight = false;"),
            "app.js must track an in-flight guard for init()"
        );
        let start = MOBILE_APP_JS
            .find("async function init(")
            .expect("init must exist");
        let rest = &MOBILE_APP_JS[start..];
        let end = rest.find("\n}").expect("init body must close");
        let region = &rest[..end];
        assert!(
            region.contains("if (initInFlight) return;"),
            "init must no-op when a run is already in flight"
        );
        assert!(
            region.contains("initInFlight = true;"),
            "init must claim the in-flight guard before doing any work"
        );
        assert!(
            region.contains("} finally {") && region.contains("initInFlight = false;"),
            "init must release the guard in a finally block"
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
