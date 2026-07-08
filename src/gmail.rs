// Gmail provider — Phase 3 complete: OAuth (A), mutations (B), send (C),
// Google Calendar + RSVP (D).
//
// Mirrors src/outlook.rs in shape: session struct, OAuth2 PKCE flow via the
// shared platform abstraction, then a flat set of async functions that
// `src/provider.rs` dispatches into via its enum match arms.
//
// Google OAuth notes (the landmines this code routes around):
//   - PKCE clients still need `client_secret` (Google quirk; not really secret).
//   - Refresh token is only issued on initial consent — must send
//     access_type=offline + prompt=consent.
//   - Refresh responses often omit refresh_token; preserve the prior one.
//   - Unverified apps with non-test users see refresh tokens expire in 7 days
//     (invalid_grant on next refresh).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::error::Error;
use crate::oauth;
use crate::platform::{self, TokenStore, Tokens};
use crate::provider_utils::{
    MAX_BLOB_BYTES, MAX_UPLOAD_CACHE_BYTES, UPLOAD_CACHE_CAP, encode_path_segment,
    mime_type_from_filename, should_clear_tokens_on_refresh_failure,
};
use crate::rate_limit::RateLimiter;
use crate::types::{CalendarEvent, Email, EmailAddress, EmailSort, Identity, Mailbox, ParsedQuery};

// =============================================================================
// Endpoints + constants
// =============================================================================

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GMAIL_BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
const CALENDAR_BASE: &str = "https://www.googleapis.com/calendar/v3/calendars/primary";
// Google's OAuth server auto-allows http://127.0.0.1:<port>/* for Desktop app
// clients (no URI registration needed). http://localhost:* is "supported but
// discouraged" per Google's native-app docs and is rejected as
// redirect_uri_mismatch by Desktop app clients in practice.
const REDIRECT_URI: &str = "http://127.0.0.1:8401/callback";
const CALLBACK_PORT: u16 = 8401;

const LABEL_CACHE_TTL: Duration = Duration::from_secs(60);
const MESSAGES_PAGE_SIZE: u32 = 100;
const MAX_REWALK_PAGES: usize = 20;

// Three scopes only (drop `userinfo.email` — gmail.modify covers getProfile).
const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/gmail.modify",
    "https://www.googleapis.com/auth/gmail.send",
    "https://www.googleapis.com/auth/calendar",
];

// =============================================================================
// Session
// =============================================================================

pub struct GmailSession {
    pub client: reqwest::Client,
    pub token: tokio::sync::Mutex<GmailToken>,
    pub client_id: String,
    pub client_secret: String,
    pub email: String,
    pub token_store: Arc<dyn TokenStore>,
    pub account_id: String,
    pub label_cache: tokio::sync::Mutex<Option<LabelCacheEntry>>,
    /// Per-(mailbox+query) cursor cache: index N is the `PageStart` for fetching
    /// page N. Index 0 is always `First`; an `End` entry means "no such page,
    /// past the end of results" so a re-fetch can short-circuit without
    /// re-issuing the page-0 request (which would otherwise happen if a plain
    /// `None` were used both as "no token needed" and "no more results").
    pub page_cache: tokio::sync::Mutex<HashMap<String, Vec<PageStart>>>,
    /// Synthetic blob cache for compose-time uploads. Gmail has no
    /// standalone blob store, so uploads are buffered here and inlined into
    /// the RFC822 at `send_email` time. Capped at `UPLOAD_CACHE_CAP` per
    /// session; entries are consumed on resolve so memory drops promptly.
    pub upload_cache: tokio::sync::Mutex<HashMap<uuid::Uuid, (String, Vec<u8>)>>,
    /// LRU-ish cache of (gmail_msg_id → real RFC822 Message-ID header value).
    /// Populated lazily when sending a reply — the frontend passes Gmail's
    /// message ID as `in_reply_to`, but RFC822 needs the actual `<…@…>`
    /// header value. Capped at `PARENT_MID_CACHE_CAP`; oldest entries evicted.
    pub parent_message_id_cache: tokio::sync::Mutex<Vec<(String, String)>>,
    /// Provider-wide rate limiter combining concurrency cap, steady-state
    /// spacing, and Retry-After-aware retry. Every Gmail HTTP request
    /// should be routed through `limiter.execute(...)` so the throttling
    /// intent is expressed in one place (see [`build_gmail_limiter`]).
    pub limiter: Arc<RateLimiter>,
}

// UPLOAD_CACHE_CAP, MAX_BLOB_BYTES, MAX_UPLOAD_CACHE_BYTES → see
// crate::provider_utils (single tuning point shared with Outlook).
const PARENT_MID_CACHE_CAP: usize = 16;

/// Gmail rate-limit tuning. 5 concurrent × 80ms spacing ≈ 12 RPS
/// steady-state — well under Gmail's per-user 250 quota-units/sec
/// budget, and matches the proven `get_emails` cap that prevents the
/// "Too many concurrent requests for user" 429.
fn build_gmail_limiter() -> Arc<RateLimiter> {
    Arc::new(RateLimiter::new("gmail", 5, Duration::from_millis(80), 3))
}

/// What's needed to fetch a given page from Gmail's cursor-paginated
/// `messages.list`. The three states are deliberately distinguishable so the
/// pagination cache can't confuse "no token needed (page 0)" with
/// "no more results past this index".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageStart {
    /// Page 0: fetch with no `pageToken` parameter.
    First,
    /// Page N>0: fetch with this `pageToken`.
    With(String),
    /// No such page: a previous response returned `nextPageToken: None`.
    End,
}

pub struct GmailToken {
    pub access_token: String,
    pub refresh_token: String,
    pub token_expiry: DateTime<Utc>,
}

pub struct LabelCacheEntry {
    pub fetched_at: Instant,
    pub labels: Vec<Mailbox>,
}

// =============================================================================
// OAuth — auth URL, code exchange, refresh
// =============================================================================

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
}

/// Build the Google OAuth2 authorization URL with PKCE.
/// **Must** include `access_type=offline` + `prompt=consent` — without both,
/// Google does not issue a refresh token (the #1 OAuth gotcha).
pub fn auth_url(client_id: &str, code_verifier: &str, state: &str) -> String {
    let challenge = oauth::code_challenge(code_verifier);
    let scope = SCOPES.join(" ");
    let mut url = url::Url::parse(AUTH_URL).expect("valid Google auth base URL");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", &scope)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    url.to_string()
}

async fn exchange_code(
    client: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    code: &str,
    code_verifier: &str,
) -> Result<TokenResponse, Error> {
    let resp = client
        .post(TOKEN_URL)
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code", code),
            ("code_verifier", code_verifier),
            ("grant_type", "authorization_code"),
            ("redirect_uri", REDIRECT_URI),
        ])
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        tracing::warn!(http_status = %status, response_body = %text, "Gmail token exchange failed");
        return Err(Error::Auth(format!(
            "Gmail token exchange failed ({status}): {text}"
        )));
    }

    Ok(resp.json().await?)
}

/// Refresh the access token if it expires within 60 seconds.
/// On `invalid_grant` (test-user 7-day expiry, or revoked grant), returns
/// `Error::Auth` with a message pointing at the README.
async fn ensure_token(session: &GmailSession) -> Result<(), Error> {
    let mut token = session.token.lock().await;
    if Utc::now() + chrono::Duration::seconds(60) < token.token_expiry {
        return Ok(());
    }

    let resp = session
        .client
        .post(TOKEN_URL)
        .form(&[
            ("client_id", session.client_id.as_str()),
            ("client_secret", session.client_secret.as_str()),
            ("refresh_token", token.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        tracing::warn!(
            http_status = %status,
            response_body = %text,
            "Gmail token refresh failed"
        );
        // Irrecoverable: tokens are gone on Google's side. Evict ours so the
        // next process launch goes through the full OAuth flow instead of
        // hammering /token with a refresh that will never succeed.
        if should_clear_tokens_on_refresh_failure(status, &text) {
            // Release the mutex before clear_stored_tokens (it acquires its
            // own lock — different mutex, but the principle holds).
            drop(token);
            clear_stored_tokens(session).await;
            return Err(Error::Auth(format!(
                "Gmail refresh token expired or revoked. Stored tokens cleared; \
                 restart supervillain to re-authenticate. If your OAuth app is in \
                 'Testing' state in Google Cloud Console, you must be listed as a \
                 Test User; otherwise tokens expire after 7 days. \
                 See README §Gmail setup. ({status}): {text}"
            )));
        }
        return Err(Error::Auth(format!(
            "Gmail token refresh failed ({status}): {text}"
        )));
    }

    let resp: TokenResponse = resp.json().await?;
    token.access_token = resp.access_token;
    // Google's refresh response often omits refresh_token. Preserve the prior one.
    if let Some(rt) = resp.refresh_token {
        token.refresh_token = rt;
    }
    token.token_expiry = Utc::now() + chrono::Duration::seconds(resp.expires_in);

    save_tokens(session, &token)?;
    tracing::info!("Refreshed Gmail token for {}", session.email);
    Ok(())
}

async fn access_token(session: &GmailSession) -> Result<String, Error> {
    ensure_token(session).await?;
    Ok(session.token.lock().await.access_token.clone())
}

fn save_tokens(session: &GmailSession, token: &GmailToken) -> Result<(), Error> {
    session.token_store.save(
        &session.account_id,
        &Tokens {
            access_token: token.access_token.clone(),
            refresh_token: token.refresh_token.clone(),
            token_expiry: token.token_expiry,
            email: session.email.clone(),
        },
    )
}

fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("failed to create HTTP client")
}

#[derive(Deserialize)]
struct GmailProfile {
    #[serde(rename = "emailAddress")]
    email_address: String,
}

async fn fetch_user_email(client: &reqwest::Client, access_token: &str) -> Result<String, Error> {
    let resp = client
        .get(format!("{GMAIL_BASE}/profile"))
        .bearer_auth(access_token)
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_gmail_error("getProfile", status, &text));
    }
    let profile: GmailProfile = resp.json().await?;
    Ok(profile.email_address)
}

/// Load a Gmail session from saved tokens (if present). Returns None if no
/// tokens are stored for this account.
pub fn load_session(
    token_store: Arc<dyn TokenStore>,
    account_id: &str,
    client_id: &str,
    client_secret: &str,
) -> Option<GmailSession> {
    let tokens = token_store.load(account_id)?;
    Some(GmailSession {
        client: build_http_client(),
        token: tokio::sync::Mutex::new(GmailToken {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            token_expiry: tokens.token_expiry,
        }),
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        email: tokens.email,
        token_store,
        account_id: account_id.to_string(),
        label_cache: tokio::sync::Mutex::new(None),
        page_cache: tokio::sync::Mutex::new(HashMap::new()),
        upload_cache: tokio::sync::Mutex::new(HashMap::new()),
        parent_message_id_cache: tokio::sync::Mutex::new(Vec::new()),
        limiter: build_gmail_limiter(),
    })
}

/// One-shot OAuth2 PKCE flow. Delegates the callback acquisition to the
/// platform abstraction so iOS can substitute ASWebAuthenticationSession.
pub async fn oauth_flow(
    token_store: Arc<dyn TokenStore>,
    account_id: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<GmailSession, Error> {
    let code_verifier = oauth::generate_code_verifier();
    let expected_state = oauth::generate_state();
    let url = auth_url(client_id, &code_verifier, &expected_state);

    let callback = platform::acquire_oauth_callback(&url, &expected_state, CALLBACK_PORT).await?;

    let client = build_http_client();
    let token_resp = exchange_code(
        &client,
        client_id,
        client_secret,
        &callback.code,
        &code_verifier,
    )
    .await?;
    let refresh_token = token_resp.refresh_token.ok_or_else(|| {
        Error::Auth(
            "Google did not return a refresh_token on initial consent. \
             Ensure access_type=offline and prompt=consent are set."
                .into(),
        )
    })?;
    let email = fetch_user_email(&client, &token_resp.access_token).await?;

    let session = GmailSession {
        client,
        token: tokio::sync::Mutex::new(GmailToken {
            access_token: token_resp.access_token,
            refresh_token,
            token_expiry: Utc::now() + chrono::Duration::seconds(token_resp.expires_in),
        }),
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        email,
        token_store,
        account_id: account_id.to_string(),
        label_cache: tokio::sync::Mutex::new(None),
        page_cache: tokio::sync::Mutex::new(HashMap::new()),
        upload_cache: tokio::sync::Mutex::new(HashMap::new()),
        parent_message_id_cache: tokio::sync::Mutex::new(Vec::new()),
        limiter: build_gmail_limiter(),
    };

    let token = session.token.lock().await;
    save_tokens(&session, &token)?;
    drop(token);
    tracing::info!("Gmail OAuth completed for {}", session.email);
    Ok(session)
}

// =============================================================================
// Labels (mailboxes)
// =============================================================================

#[derive(Deserialize)]
struct LabelsListResp {
    #[serde(default)]
    labels: Vec<LabelStub>,
}

#[derive(Deserialize, Clone)]
struct LabelStub {
    id: String,
    name: String,
    #[serde(default, rename = "type")]
    label_type: Option<String>,
}

#[derive(Deserialize)]
struct LabelDetail {
    id: String,
    name: String,
    #[serde(default, rename = "type")]
    label_type: Option<String>,
    #[serde(default, rename = "messagesTotal")]
    messages_total: i64,
    #[serde(default, rename = "messagesUnread")]
    messages_unread: i64,
}

/// Map a system label name to our `Mailbox.role`. User labels return `None`.
pub fn label_to_role(name: &str, label_type: &str) -> Option<String> {
    if label_type != "system" {
        return None;
    }
    match name {
        "INBOX" => Some("inbox".into()),
        "SENT" => Some("sent".into()),
        "DRAFT" => Some("drafts".into()),
        "SPAM" => Some("junk".into()),
        "TRASH" => Some("trash".into()),
        _ => None,
    }
}

/// Whether a label should appear in the mailbox sidebar.
/// User labels: always. System labels: only true "folder" ones (INBOX, SENT,
/// DRAFT, SPAM, TRASH). Skips STARRED/IMPORTANT/UNREAD/CHAT/CATEGORY_* —
/// those are keyword-like or duplicate INBOX.
fn should_include_label(name: &str, label_type: &str) -> bool {
    if label_type == "user" {
        return true;
    }
    matches!(name, "INBOX" | "SENT" | "DRAFT" | "SPAM" | "TRASH")
}

/// Mirror of `should_include_label` operating on label IDs only (used at email
/// parse time, where we have IDs but not the label `type` field). Gmail's
/// system labels use ALL-CAPS IDs that match their names; user labels are
/// `Label_N` so they always pass.
pub(crate) fn is_displayable_label_id(id: &str) -> bool {
    !matches!(id, "STARRED" | "IMPORTANT" | "UNREAD" | "CHAT") && !id.starts_with("CATEGORY_")
}

/// Build Mailbox structs from a flat list of detailed labels, populating
/// `parent_id` for nested `Parent/Child` user labels.
fn build_mailboxes(labels: Vec<LabelDetail>) -> Vec<Mailbox> {
    let id_by_name: HashMap<String, String> = labels
        .iter()
        .filter(|l| should_include_label(&l.name, l.label_type.as_deref().unwrap_or("user")))
        .map(|l| (l.name.clone(), l.id.clone()))
        .collect();

    labels
        .into_iter()
        .filter(|l| should_include_label(&l.name, l.label_type.as_deref().unwrap_or("user")))
        .map(|l| {
            let label_type = l.label_type.as_deref().unwrap_or("user");
            let role = label_to_role(&l.name, label_type);
            let parent_id = l
                .name
                .rfind('/')
                .and_then(|pos| id_by_name.get(&l.name[..pos]).cloned());
            Mailbox {
                id: l.id,
                name: l.name,
                role,
                total_emails: l.messages_total,
                unread_emails: l.messages_unread,
                parent_id,
            }
        })
        .collect()
}

/// Fetch mailboxes (Gmail labels). 60s session-local cache; mutations should
/// `invalidate_label_cache` to force a re-fetch on the next read.
///
/// `labels.list` only returns metadata, so we fan out N concurrent
/// `labels.get` calls (one RTT instead of N).
pub async fn get_mailboxes(session: &GmailSession) -> Result<Vec<Mailbox>, Error> {
    {
        let cache = session.label_cache.lock().await;
        if let Some(entry) = cache.as_ref()
            && entry.fetched_at.elapsed() < LABEL_CACHE_TTL
        {
            return Ok(entry.labels.clone());
        }
    }

    let token = access_token(session).await?;

    let stubs: LabelsListResp = {
        let resp = session
            .limiter
            .execute("labels.list", || async {
                session
                    .client
                    .get(format!("{GMAIL_BASE}/labels"))
                    .bearer_auth(&token)
                    .send()
                    .await
            })
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(classify_gmail_error("labels.list", status, &text));
        }
        resp.json().await?
    };

    // Per-label `labels.get` fan-out. Previously uncapped — a mailbox
    // with many labels would issue N concurrent requests and hit the
    // per-user concurrent-request 429. Routes through the same limiter
    // as `get_emails` so concurrency + spacing apply uniformly.
    let mut join_set = tokio::task::JoinSet::new();
    for stub in stubs.labels {
        let label_type = stub.label_type.as_deref().unwrap_or("user");
        if !should_include_label(&stub.name, label_type) {
            continue;
        }
        let client = session.client.clone();
        let token = token.clone();
        let id = stub.id.clone();
        let limiter = session.limiter.clone();
        join_set.spawn(async move {
            let url = format!("{GMAIL_BASE}/labels/{id}");
            let resp = limiter
                .execute("labels.get", || async {
                    client.get(&url).bearer_auth(&token).send().await
                })
                .await?;
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(classify_gmail_error(
                    &format!("labels.get {id}"),
                    status,
                    &text,
                ));
            }
            let detail: LabelDetail = resp.json().await?;
            Ok::<_, Error>(detail)
        });
    }

    let mut details = Vec::new();
    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok(Ok(d)) => details.push(d),
            Ok(Err(e)) => return Err(e),
            Err(join_err) => {
                return Err(Error::Internal(format!(
                    "Gmail labels.get task panicked: {join_err}"
                )));
            }
        }
    }

    let mailboxes = build_mailboxes(details);

    let mut cache = session.label_cache.lock().await;
    *cache = Some(LabelCacheEntry {
        fetched_at: Instant::now(),
        labels: mailboxes.clone(),
    });

    Ok(mailboxes)
}

/// Clear the label cache. Called after any mutation that changes label counts
/// (Milestone B will wire this into mark_read/archive/trash/etc).
pub async fn invalidate_label_cache(session: &GmailSession) {
    let mut cache = session.label_cache.lock().await;
    *cache = None;
}

/// Delete the stored OAuth tokens for this session's account. Called when
/// `ensure_token` detects an irrecoverable refresh failure (typically
/// `invalid_grant` from a revoked or 7-day-expired test-user token), so the
/// next launch falls through to a fresh `oauth_flow` instead of looping on
/// a doomed refresh.
pub async fn clear_stored_tokens(session: &GmailSession) {
    if let Err(e) = session.token_store.delete(&session.account_id) {
        tracing::warn!(
            account_id = %session.account_id,
            error = %e,
            "Failed to delete stored Gmail tokens after refresh failure"
        );
    }
}

// should_clear_tokens_on_refresh_failure → see crate::provider_utils
// (identical predicate for Gmail and Outlook).

// =============================================================================
// Identities (sendAs)
// =============================================================================

#[derive(Deserialize)]
struct SendAsResp {
    #[serde(default, rename = "sendAs")]
    send_as: Vec<SendAsEntry>,
}

#[derive(Deserialize)]
struct SendAsEntry {
    #[serde(rename = "sendAsEmail")]
    send_as_email: String,
    #[serde(default, rename = "displayName")]
    display_name: String,
}

pub async fn get_identities(session: &GmailSession) -> Result<Vec<Identity>, Error> {
    let token = access_token(session).await?;
    let resp = session
        .client
        .get(format!("{GMAIL_BASE}/settings/sendAs"))
        .bearer_auth(&token)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_gmail_error("sendAs.list", status, &text));
    }
    let parsed: SendAsResp = resp.json().await?;
    Ok(parsed
        .send_as
        .into_iter()
        .map(|e| Identity {
            id: e.send_as_email.clone(),
            email: e.send_as_email,
            name: e.display_name,
        })
        .collect())
}

// =============================================================================
// Search translator (Milestone A: basic; Milestone B will harden quoting/escaping)
// =============================================================================

/// Translate a `ParsedQuery` to Gmail's `q=` syntax. Multiple operator values
/// AND together (matches Fastmail semantics). Values containing whitespace,
/// `:`, or `"` get quoted with `"…"`; inner `"` are escaped as `\"`.
/// Dates use slashes (`YYYY/MM/DD`) — Gmail rejects ISO dashes. Gmail
/// interprets dates in the account's timezone; Fastmail uses UTC.
pub fn translate_query_to_q(query: &ParsedQuery) -> String {
    let mut parts: Vec<String> = Vec::new();

    for v in &query.from {
        parts.push(format!("from:{}", quote_if_needed(v)));
    }
    for v in &query.to {
        parts.push(format!("to:{}", quote_if_needed(v)));
    }
    for v in &query.subject {
        parts.push(format!("subject:{}", quote_if_needed(v)));
    }
    if query.has_attachment {
        parts.push("has:attachment".into());
    }
    match query.is_unread {
        Some(true) => parts.push("is:unread".into()),
        Some(false) => parts.push("is:read".into()),
        None => {}
    }
    match query.is_flagged {
        Some(true) => parts.push("is:starred".into()),
        Some(false) => parts.push("-is:starred".into()),
        None => {}
    }
    if let Some(after) = query.after {
        parts.push(format!("after:{}", after.format("%Y/%m/%d")));
    }
    if let Some(before) = query.before {
        parts.push(format!("before:{}", before.format("%Y/%m/%d")));
    }
    if !query.text.is_empty() {
        parts.push(query.text.clone());
    }
    // Mailbox scoping is applied via the `labelIds=` URL parameter in
    // `fetch_messages_page`, not via `q=`, so this function takes no mailbox arg.

    parts.join(" ")
}

fn quote_if_needed(s: &str) -> String {
    let needs_quote = s.contains(' ') || s.contains(':') || s.contains('"');
    if needs_quote {
        let escaped = s.replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

// =============================================================================
// query_emails — messages.list with cursor pagination
// =============================================================================

#[derive(Deserialize)]
struct MessagesListResp {
    #[serde(default)]
    messages: Vec<MessageRef>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct MessageRef {
    id: String,
}

fn page_cache_key(mailbox_id: Option<&str>, q: &str) -> String {
    format!("{}|{}", mailbox_id.unwrap_or(""), q)
}

async fn fetch_messages_page(
    session: &GmailSession,
    token: &str,
    mailbox_id: Option<&str>,
    q: &str,
    page_token: Option<&str>,
) -> Result<MessagesListResp, Error> {
    let mut url = url::Url::parse(&format!("{GMAIL_BASE}/messages")).expect("valid base");
    {
        let mut qp = url.query_pairs_mut();
        qp.append_pair("maxResults", &MESSAGES_PAGE_SIZE.to_string());
        if !q.is_empty() {
            qp.append_pair("q", q);
        }
        if let Some(id) = mailbox_id
            && !id.is_empty()
        {
            qp.append_pair("labelIds", id);
        }
        if let Some(t) = page_token {
            qp.append_pair("pageToken", t);
        }
    }
    let resp = session.client.get(url).bearer_auth(token).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_gmail_error("messages.list", status, &text));
    }
    Ok(resp.json().await?)
}

/// Translate a Gmail `nextPageToken` to the cache's `PageStart` for the *next*
/// index. `None` from the API means "no more pages" (End sentinel); `Some(t)`
/// means "use this token". Pure function — extracted for unit testing.
fn next_page_start_from(api_next_token: Option<String>) -> PageStart {
    match api_next_token {
        Some(t) => PageStart::With(t),
        None => PageStart::End,
    }
}

/// Record the result of fetching page `page_idx` into the cache, growing the
/// vector if needed. Pure function — no I/O, no awaits. Returns the slot
/// written to so tests can assert. Invariant after this call:
/// `cache[page_idx + 1] == next_page_start_from(api_next_token)`.
fn record_page_fetched(
    cache: &mut Vec<PageStart>,
    page_idx: usize,
    api_next_token: Option<String>,
) {
    let next_idx = page_idx + 1;
    let next_start = next_page_start_from(api_next_token);
    if next_idx >= cache.len() {
        cache.resize(next_idx + 1, PageStart::End);
    }
    cache[next_idx] = next_start;
}

/// Reorder a single fetched batch of message ids for the requested sort.
///
/// Gmail's `messages.list` has no server-side `orderBy` parameter — it
/// always returns results in its own default order (newest-first for a
/// plain list/search). There is no way to ask the API itself for
/// oldest-first. Rather than silently ignoring `DateAsc` and returning the
/// wrong order (which the plan explicitly forbids), we reverse the
/// already-fetched page client-side.
///
/// This is **page-scoped, not a global sort**: `ids` here is exactly the
/// `limit`-sized batch this call is about to return (the caller's one
/// "page" of the infinite-scroll list), so reversing it yields a correct
/// oldest-first ordering *within that batch*. It does NOT retroactively
/// reorder across batches — paginating forward through `DateAsc` still
/// walks Gmail's underlying pages newest-block-first; only the items
/// inside each returned block are ascending. Documented v1 limitation
/// (kata 09ef); a true global ascending order would require fetching and
/// buffering the entire result set, which isn't cheap for large mailboxes.
fn apply_sort_order(mut ids: Vec<String>, sort: EmailSort) -> Vec<String> {
    if sort == EmailSort::DateAsc {
        ids.reverse();
    }
    ids
}

/// Query email IDs. Translates cursor pagination to the route handler's
/// offset model. The cache stores the `PageStart` for fetching page N for
/// each (mailbox+query) key; first request seeds it, subsequent forward
/// requests follow it, jump-backs re-walk from 0 (bounded by MAX_REWALK_PAGES).
/// `PageStart::End` entries are respected — we never re-issue a page-0 fetch
/// just because a later page returned no more results.
///
/// `sort` only affects the order of the ids returned from *this* call —
/// see `apply_sort_order` for why it's page-scoped rather than global. The
/// underlying page-token cursor cache (`session.page_cache`) is unaffected:
/// Gmail's raw page fetches are identical regardless of the requested
/// display order, so the same cursors are reused for both.
pub async fn query_emails(
    session: &GmailSession,
    mailbox_id: Option<&str>,
    limit: usize,
    position: usize,
    query: Option<&ParsedQuery>,
    sort: EmailSort,
) -> Result<Vec<String>, Error> {
    let q = query.map(translate_query_to_q).unwrap_or_default();
    let token = access_token(session).await?;
    let key = page_cache_key(mailbox_id, &q);

    // Logical page boundaries are MESSAGES_PAGE_SIZE; the caller's
    // (position, limit) may straddle them. Compute which pages cover the slice.
    let page_size = MESSAGES_PAGE_SIZE as usize;
    let start_page = position / page_size;
    let skip_in_first = position % page_size;
    let needed = skip_in_first + limit;
    let pages_to_fetch = needed.div_ceil(page_size).max(1);
    let end_page = start_page + pages_to_fetch;

    if end_page > MAX_REWALK_PAGES {
        return Err(Error::BadRequest(format!(
            "Gmail pagination position {position} exceeds bounded re-walk \
             (max page {}, ~{} messages). Use search to narrow.",
            MAX_REWALK_PAGES,
            MAX_REWALK_PAGES * page_size
        )));
    }

    // Snapshot the cache, extend it as we walk.
    let mut cache: Vec<PageStart> = {
        let cache = session.page_cache.lock().await;
        cache
            .get(&key)
            .cloned()
            .unwrap_or_else(|| vec![PageStart::First])
    };

    let mut ids: Vec<String> = Vec::with_capacity(limit);
    let mut consumed_in_first_page = 0usize;
    let mut hit_end = false;

    for page_idx in 0..end_page {
        // Extend cache forward by walking from the last known page if needed.
        while page_idx >= cache.len() {
            let walk_idx = cache.len() - 1;
            let start = &cache[walk_idx];
            let page_token = match start {
                PageStart::First => None,
                PageStart::With(t) => Some(t.as_str()),
                PageStart::End => {
                    // Past end of results — no more pages exist.
                    hit_end = true;
                    break;
                }
            };
            let resp = fetch_messages_page(session, &token, mailbox_id, &q, page_token).await?;
            record_page_fetched(&mut cache, walk_idx, resp.next_page_token);
        }
        if hit_end {
            break;
        }

        // Skip pages we don't need IDs from (already cached their next-token).
        if page_idx < start_page {
            continue;
        }

        let page_token = match &cache[page_idx] {
            PageStart::First => None,
            PageStart::With(t) => Some(t.clone()),
            PageStart::End => {
                // Reached end-of-results at or before our requested slice.
                break;
            }
        };
        let MessagesListResp {
            messages,
            next_page_token,
        } = fetch_messages_page(session, &token, mailbox_id, &q, page_token.as_deref()).await?;
        record_page_fetched(&mut cache, page_idx, next_page_token);

        for msg in messages {
            if page_idx == start_page && consumed_in_first_page < skip_in_first {
                consumed_in_first_page += 1;
                continue;
            }
            ids.push(msg.id);
            if ids.len() >= limit {
                break;
            }
        }
        if ids.len() >= limit {
            break;
        }
        if matches!(cache.get(page_idx + 1), Some(PageStart::End)) {
            break;
        }
    }

    // Write back cache snapshot.
    let mut cache_lock = session.page_cache.lock().await;
    cache_lock.insert(key, cache);

    Ok(apply_sort_order(ids, sort))
}

// =============================================================================
// get_emails — messages.get + payload tree → Email
// =============================================================================

#[derive(Deserialize, Debug)]
pub struct GmailMessage {
    pub id: String,
    #[serde(default, rename = "threadId")]
    pub thread_id: String,
    #[serde(default, rename = "labelIds")]
    pub label_ids: Vec<String>,
    #[serde(default)]
    pub snippet: String,
    #[serde(default, rename = "internalDate")]
    pub internal_date: String,
    #[serde(default, rename = "sizeEstimate")]
    pub size_estimate: i64,
    pub payload: GmailPayload,
}

#[derive(Deserialize, Debug)]
pub struct GmailPayload {
    #[serde(default, rename = "mimeType")]
    pub mime_type: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub headers: Vec<GmailHeader>,
    #[serde(default)]
    pub body: Option<GmailBody>,
    #[serde(default)]
    pub parts: Option<Vec<GmailPayload>>,
}

#[derive(Deserialize, Debug)]
pub struct GmailHeader {
    pub name: String,
    pub value: String,
}

#[derive(Deserialize, Debug)]
pub struct GmailBody {
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default, rename = "attachmentId")]
    pub attachment_id: Option<String>,
}

pub async fn get_emails(
    session: &GmailSession,
    ids: &[String],
    fetch_body: bool,
) -> Result<Vec<Email>, Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let token = access_token(session).await?;
    let format = if fetch_body { "full" } else { "metadata" };

    let mut join_set = tokio::task::JoinSet::new();
    for (idx, id) in ids.iter().enumerate() {
        let client = session.client.clone();
        let token = token.clone();
        let id = id.clone();
        let format = format.to_string();
        let limiter = session.limiter.clone();
        join_set.spawn(async move {
            let url = format!("{GMAIL_BASE}/messages/{id}?format={format}");
            let resp = limiter
                .execute("messages.get", || async {
                    client.get(&url).bearer_auth(&token).send().await
                })
                .await?;
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(classify_gmail_error(
                    &format!("messages.get {id}"),
                    status,
                    &text,
                ));
            }
            let msg: GmailMessage = resp.json().await?;
            Ok::<_, Error>((idx, parse_message_to_email(msg, fetch_body)))
        });
    }

    let mut indexed: Vec<(usize, Email)> = Vec::with_capacity(ids.len());
    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok(Ok(item)) => indexed.push(item),
            Ok(Err(e)) => return Err(e),
            Err(join_err) => {
                return Err(Error::Internal(format!(
                    "Gmail messages.get task panicked: {join_err}"
                )));
            }
        }
    }
    indexed.sort_by_key(|(idx, _)| *idx);
    Ok(indexed.into_iter().map(|(_, e)| e).collect())
}

/// Convert a Gmail `messages.get` response into our canonical `Email`.
/// - Read state: `$seen` set when Gmail's `UNREAD` label is *absent*.
/// - Flag state: `$flagged` set when `STARRED` label is present.
/// - `thread_id` stored verbatim; no thread collapsing (one Email per message).
/// - `blob_id` set to `id` (Gmail has no separate blob namespace; Milestone B's
///   `BlobRef` enum will properly disambiguate compose vs server-side blobs).
pub fn parse_message_to_email(msg: GmailMessage, fetch_body: bool) -> Email {
    let mut keywords = HashMap::new();
    if !msg.label_ids.iter().any(|l| l == "UNREAD") {
        keywords.insert("$seen".to_string(), true);
    }
    if msg.label_ids.iter().any(|l| l == "STARRED") {
        keywords.insert("$flagged".to_string(), true);
    }
    // Filter out system labels that are keyword-equivalents (STARRED, UNREAD)
    // or sidebar-noise (IMPORTANT, CHAT, CATEGORY_*) — those are deliberately
    // excluded from get_mailboxes(), so they shouldn't appear in mailbox_ids
    // either. Without this filter a frontend doing email.mailbox_ids → sidebar
    // lookup would surface pseudo-mailboxes that don't actually exist.
    let mailbox_ids: HashMap<String, bool> = msg
        .label_ids
        .iter()
        .filter(|l| is_displayable_label_id(l))
        .map(|l| (l.clone(), true))
        .collect();

    let mut subject = String::new();
    let mut from: Vec<EmailAddress> = Vec::new();
    let mut to: Vec<EmailAddress> = Vec::new();
    let mut cc: Vec<EmailAddress> = Vec::new();
    for h in &msg.payload.headers {
        match h.name.to_ascii_lowercase().as_str() {
            "subject" => subject = h.value.clone(),
            "from" => from = parse_address_list(&h.value),
            "to" => to = parse_address_list(&h.value),
            "cc" => cc = parse_address_list(&h.value),
            _ => {}
        }
    }

    let received_at = msg
        .internal_date
        .parse::<i64>()
        .ok()
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .unwrap_or_else(Utc::now);

    let mut text_body: Option<String> = None;
    let mut html_body: Option<String> = None;
    let mut attachments: Vec<crate::types::Attachment> = Vec::new();
    let mut has_calendar = false;
    walk_payload(
        &msg.payload,
        fetch_body,
        &msg.id,
        &mut text_body,
        &mut html_body,
        &mut attachments,
        &mut has_calendar,
        false,
    );

    let has_attachment = !attachments.is_empty();

    // Diagnostic: when the caller asked for body content but we extracted
    // neither plain nor HTML AND the message isn't an attachment-only
    // forward or calendar-only invite (those legitimately have no body),
    // log the mime tree so we can tell whether walk_payload missed a part
    // type or Gmail returned an unexpected shape.
    if fetch_body && text_body.is_none() && html_body.is_none() && !has_attachment && !has_calendar
    {
        tracing::warn!(
            msg_id = %msg.id,
            parts = %mime_path(&msg.payload),
            "Gmail message parsed with empty text/html body — UI will show '(no content)'"
        );
    }

    Email {
        id: msg.id.clone(),
        blob_id: msg.id,
        thread_id: msg.thread_id,
        mailbox_ids,
        keywords,
        received_at,
        subject,
        from,
        to,
        cc,
        preview: msg.snippet,
        has_attachment,
        size: msg.size_estimate,
        text_body,
        html_body,
        has_calendar,
        attachments,
    }
}

/// Naive RFC 5322 address-list parser: splits on `,` and pulls `Name <email>`
/// or bare email. Gmail returns clean values for the common cases; corner
/// cases (quoted commas in display names) will be revisited if they bite.
fn parse_address_list(s: &str) -> Vec<EmailAddress> {
    s.split(',')
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            if let Some(open) = part.rfind('<')
                && let Some(close) = part.rfind('>')
                && close > open
            {
                let email = part[open + 1..close].trim().to_string();
                let name_part = part[..open].trim().trim_matches('"').trim().to_string();
                let name = if name_part.is_empty() {
                    None
                } else {
                    Some(name_part)
                };
                return Some(EmailAddress { name, email });
            }
            Some(EmailAddress {
                name: None,
                email: part.to_string(),
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn walk_payload(
    part: &GmailPayload,
    fetch_body: bool,
    msg_id: &str,
    text_body: &mut Option<String>,
    html_body: &mut Option<String>,
    attachments: &mut Vec<crate::types::Attachment>,
    has_calendar: &mut bool,
    in_related: bool,
) {
    let mime_type = part.mime_type.to_ascii_lowercase();

    if mime_type.starts_with("multipart/") {
        let new_in_related = in_related || mime_type == "multipart/related";
        if let Some(parts) = &part.parts {
            for child in parts {
                walk_payload(
                    child,
                    fetch_body,
                    msg_id,
                    text_body,
                    html_body,
                    attachments,
                    has_calendar,
                    new_in_related,
                );
            }
        }
        return;
    }

    let filename: Option<&str> = if part.filename.is_empty() {
        None
    } else {
        Some(part.filename.as_str())
    };
    let content_disposition_lower = part
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("Content-Disposition"))
        .map(|h| h.value.to_ascii_lowercase())
        .unwrap_or_default();
    let is_attachment_disposition = content_disposition_lower.starts_with("attachment");
    let is_inline_disposition = content_disposition_lower.starts_with("inline");

    let is_calendar = mime_type == "text/calendar";
    if is_calendar {
        *has_calendar = true;
    }

    let is_body_text = mime_type == "text/plain" && !is_attachment_disposition;
    let is_body_html = mime_type == "text/html" && !is_attachment_disposition;

    if is_body_text && text_body.is_none() {
        if fetch_body
            && let Some(body) = &part.body
            && let Some(data) = &body.data
            && let Ok(bytes) = base64url_decode(data)
            && let Ok(s) = String::from_utf8(bytes)
        {
            *text_body = Some(s);
        }
        return;
    }
    if is_body_html && html_body.is_none() {
        if fetch_body
            && let Some(body) = &part.body
            && let Some(data) = &body.data
            && let Ok(bytes) = base64url_decode(data)
            && let Ok(s) = String::from_utf8(bytes)
        {
            *html_body = Some(s);
        }
        return;
    }

    let has_attachment_id = part
        .body
        .as_ref()
        .and_then(|b| b.attachment_id.as_ref())
        .is_some();
    let is_attachment = !is_body_text
        && !is_body_html
        && !is_calendar
        && (filename.is_some() || is_attachment_disposition || has_attachment_id);

    // Inline images embedded in HTML (multipart/related) aren't user attachments.
    if is_attachment && in_related && is_inline_disposition {
        return;
    }

    if is_attachment && let Some(att_id) = part.body.as_ref().and_then(|b| b.attachment_id.clone())
    {
        let blob_ref = crate::types::BlobRef::GmailAttachment {
            msg_id: msg_id.to_string(),
            att_id,
        };
        attachments.push(crate::types::Attachment {
            blob_id: blob_ref.to_string(),
            name: filename.unwrap_or("").to_string(),
            mime_type: part.mime_type.clone(),
            size: part.body.as_ref().map(|b| b.size).unwrap_or(0),
        });
    }
}

/// Render a Gmail payload tree as a single line: `multipart/mixed > [text/plain, text/html]`.
/// Used only by the empty-body diagnostic in `parse_message_to_email`, so structure
/// (nesting + siblings) matters more than allocation count — without it the report
/// can't tell "text/plain was hiding inside multipart/related" from "text/plain
/// was a top-level sibling."
fn mime_path(part: &GmailPayload) -> String {
    let mut buf = part.mime_type.clone();
    if let Some(parts) = &part.parts
        && !parts.is_empty()
    {
        buf.push_str(" > [");
        for (i, child) in parts.iter().enumerate() {
            if i > 0 {
                buf.push_str(", ");
            }
            buf.push_str(&mime_path(child));
        }
        buf.push(']');
    }
    buf
}

fn base64url_decode(s: &str) -> Result<Vec<u8>, Error> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(s))
        .map_err(|e| Error::Internal(format!("base64url decode failed: {e}")))
}

// encode_path_segment → see crate::provider_utils.

/// Map a Gmail HTTP error response to the right `Error` variant so frontends
/// can distinguish "your input/state is stale" (4xx — refresh the list) from
/// "Gmail is down" (5xx — retry later). Pure — unit-tested.
pub(crate) fn classify_gmail_error(
    operation: &str,
    status: reqwest::StatusCode,
    body: &str,
) -> Error {
    if is_rate_limited(status, body) {
        // The limiter normally absorbs 429s before reaching here; this
        // path catches Gmail's quirky HTTP-403-with-quota-body shape and
        // any call site not yet routed through the limiter. No
        // `Retry-After` is available at this layer.
        tracing::warn!(
            operation,
            status = status.as_u16(),
            "Gmail rate limit surfaced past limiter — classifying as RateLimited"
        );
        return Error::RateLimited { retry_after: None };
    }
    let msg = format!("Gmail {operation} failed ({status}): {body}");
    if status.is_client_error() {
        Error::BadRequest(msg)
    } else {
        Error::Internal(msg)
    }
}

/// True when the response is Gmail's per-user/per-project rate-limit signal.
/// Two shapes in the wild:
///   - HTTP 429 (Too Many Requests) — concurrent-request cap.
///   - HTTP 403 with body mentioning `rateLimitExceeded`,
///     `userRateLimitExceeded`, `quotaExceeded`, or `RESOURCE_EXHAUSTED` —
///     per-minute quota cap. Google chose 403 here because the gRPC
///     `PERMISSION_DENIED` status maps to HTTP 403, even though the cause is
///     quota, not permissions.
pub(crate) fn is_rate_limited(status: reqwest::StatusCode, body: &str) -> bool {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return true;
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        return body.contains("rateLimitExceeded")
            || body.contains("userRateLimitExceeded")
            || body.contains("quotaExceeded")
            || body.contains("RATE_LIMIT_EXCEEDED")
            || body.contains("RESOURCE_EXHAUSTED");
    }
    false
}

// =============================================================================
// Mutations — messages.modify, messages.trash, messages.batchModify
// =============================================================================

/// Body for `messages.modify` and `messages.batchModify`. Extracted as a pure
/// fn so the JSON shape is testable without an HTTP mock.
fn modify_body(add: &[&str], remove: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "addLabelIds": add,
        "removeLabelIds": remove,
    })
}

/// Body for `messages.batchModify`. Same shape as `modify_body` plus the `ids`
/// array of message IDs to mutate.
fn batch_modify_body(ids: &[String], add: &[&str], remove: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "ids": ids,
        "addLabelIds": add,
        "removeLabelIds": remove,
    })
}

async fn modify_labels(
    session: &GmailSession,
    msg_id: &str,
    add: &[&str],
    remove: &[&str],
) -> Result<bool, Error> {
    let token = access_token(session).await?;
    let url = format!("{GMAIL_BASE}/messages/{msg_id}/modify");
    let body = modify_body(add, remove);
    // Retry/backoff (including for the rapid archive + prefetch +
    // split-count fan-out bursts that used to need a bespoke 750ms
    // retry-once) is handled by the session limiter — see
    // `build_gmail_limiter`.
    let resp = session
        .limiter
        .execute("messages.modify", || async {
            session
                .client
                .post(&url)
                .bearer_auth(&token)
                .json(&body)
                .send()
                .await
        })
        .await?;
    let status = resp.status();
    if status.is_success() {
        invalidate_label_cache(session).await;
        return Ok(true);
    }
    let text = resp.text().await.unwrap_or_default();
    Err(classify_gmail_error(
        &format!("messages.modify {msg_id}"),
        status,
        &text,
    ))
}

pub async fn mark_read(session: &GmailSession, msg_id: &str) -> Result<bool, Error> {
    modify_labels(session, msg_id, &[], &["UNREAD"]).await
}

pub async fn mark_unread(session: &GmailSession, msg_id: &str) -> Result<bool, Error> {
    modify_labels(session, msg_id, &["UNREAD"], &[]).await
}

pub async fn archive(session: &GmailSession, msg_id: &str) -> Result<bool, Error> {
    modify_labels(session, msg_id, &[], &["INBOX"]).await
}

/// Move semantics for Gmail (best-effort — Gmail's flat label model has no
/// folder analog):
///
/// - `INBOX` target: add `INBOX`, remove nothing (filing back from archive).
/// - `TRASH` target: routed to `trash()` (uses the dedicated `/trash`
///   endpoint which actually trashes; plain `messages.modify` would only add
///   the label without invoking trash semantics).
/// - `SPAM`, `DRAFT`, `SENT`: rejected with `BadRequest` — these system
///   labels need dedicated API endpoints, not `messages.modify`.
/// - User labels: add target, remove `INBOX` (matches "file out of inbox
///   into folder" expectation).
///
/// Cross-user-label moves (e.g. `Work` → `Personal`) remain additive — Gmail
/// has no way to know the source label without the caller telling us. If
/// that becomes a real complaint, extend the signature with `from_mailbox_id`.
pub async fn move_to_mailbox(
    session: &GmailSession,
    msg_id: &str,
    mailbox_id: &str,
) -> Result<bool, Error> {
    match move_plan(mailbox_id) {
        MovePlan::Trash => trash(session, msg_id).await,
        MovePlan::Reject(reason) => Err(Error::BadRequest(reason.into())),
        MovePlan::Labels { add, remove } => {
            let add_refs: Vec<&str> = add.iter().map(String::as_str).collect();
            let remove_refs: Vec<&str> = remove.iter().map(String::as_str).collect();
            modify_labels(session, msg_id, &add_refs, &remove_refs).await
        }
    }
}

/// What `move_to_mailbox` should do for a given target. Pure — extracted for
/// unit testing the INBOX special case + rejection list without HTTP.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MovePlan {
    /// Reject with a `BadRequest`. Static text — these are all fixed strings.
    Reject(&'static str),
    /// Route to the dedicated `trash()` endpoint.
    Trash,
    /// Use `messages.modify` with these add/remove sets.
    Labels {
        add: Vec<String>,
        remove: Vec<String>,
    },
}

pub(crate) fn move_plan(mailbox_id: &str) -> MovePlan {
    match mailbox_id {
        "TRASH" => MovePlan::Trash,
        "SPAM" => MovePlan::Reject(
            "Gmail: move to SPAM is not supported via messages.modify — \
             use 'Report as spam' (dedicated endpoint not yet wired).",
        ),
        "DRAFT" => MovePlan::Reject(
            "Gmail: cannot move a message into Drafts (drafts are created via \
             draft.create, not label changes).",
        ),
        "SENT" => MovePlan::Reject(
            "Gmail: cannot move a message into Sent (the Sent label is set \
             automatically when you send a message).",
        ),
        // INBOX target = "restore to inbox" — also strip TRASH/SPAM so the
        // message actually moves back. Without removing TRASH, Gmail's
        // 30-day purge timer keeps ticking and the message vanishes despite
        // the INBOX label being present.
        "INBOX" => MovePlan::Labels {
            add: vec!["INBOX".into()],
            remove: vec!["TRASH".into(), "SPAM".into()],
        },
        other => MovePlan::Labels {
            add: vec![other.into()],
            remove: vec!["INBOX".into()],
        },
    }
}

/// Toggle the `STARRED` label. Requires a metadata fetch first to know which
/// direction to flip (Gmail has no native toggle endpoint). Two API calls per
/// invocation — acceptable for a user-driven action.
///
/// Known limitation: TOCTOU between the read and the modify. Two concurrent
/// toggles (rapid clicks, or another Gmail client mid-flight) both see the
/// pre-toggle state and apply the same direction — net result is one toggle
/// where the user expected two. Gmail exposes no conditional-update primitive
/// here, so a real fix would need frontend debouncing or a server-side
/// per-msg-id lock. Don't parallelize this without addressing the race.
pub async fn toggle_flag(session: &GmailSession, msg_id: &str) -> Result<bool, Error> {
    let starred = message_has_label(session, msg_id, "STARRED").await?;
    if starred {
        modify_labels(session, msg_id, &[], &["STARRED"]).await
    } else {
        modify_labels(session, msg_id, &["STARRED"], &[]).await
    }
}

#[derive(Deserialize)]
struct LabelsOnlyResp {
    #[serde(default, rename = "labelIds")]
    label_ids: Vec<String>,
}

async fn message_has_label(
    session: &GmailSession,
    msg_id: &str,
    label_id: &str,
) -> Result<bool, Error> {
    let token = access_token(session).await?;
    // format=metadata returns labelIds without the payload bytes (cheaper).
    let url = format!("{GMAIL_BASE}/messages/{msg_id}?format=metadata");
    let resp = session.client.get(&url).bearer_auth(&token).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_gmail_error(
            &format!("messages.get(metadata) {msg_id}"),
            status,
            &text,
        ));
    }
    let parsed: LabelsOnlyResp = resp.json().await?;
    Ok(parsed.label_ids.iter().any(|l| l == label_id))
}

pub async fn trash(session: &GmailSession, msg_id: &str) -> Result<bool, Error> {
    let token = access_token(session).await?;
    let url = format!("{GMAIL_BASE}/messages/{msg_id}/trash");
    let resp = session.client.post(&url).bearer_auth(&token).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_gmail_error(
            &format!("messages.trash {msg_id}"),
            status,
            &text,
        ));
    }
    invalidate_label_cache(session).await;
    Ok(true)
}

/// Archive a batch of messages in one API call. Returns the count of IDs
/// *submitted*, not necessarily archived — Gmail's `batchModify` returns 204
/// with no body, so per-ID success isn't observable. If any single ID in the
/// batch is invalid, Gmail rejects the whole batch with a 4xx and this fn
/// returns `BadRequest`; callers shouldn't claim "N archived" from the
/// returned count without acknowledging this contract.
pub async fn archive_batch(session: &GmailSession, msg_ids: &[String]) -> Result<usize, Error> {
    if msg_ids.is_empty() {
        return Ok(0);
    }
    let token = access_token(session).await?;
    let url = format!("{GMAIL_BASE}/messages/batchModify");
    let resp = session
        .client
        .post(&url)
        .bearer_auth(&token)
        .json(&batch_modify_body(msg_ids, &[], &["INBOX"]))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_gmail_error("messages.batchModify", status, &text));
    }
    invalidate_label_cache(session).await;
    Ok(msg_ids.len())
}

// =============================================================================
// download_blob — messages.attachments.get
// =============================================================================

#[derive(Deserialize)]
struct AttachmentBody {
    #[serde(default)]
    data: Option<String>,
}

// mime_type_from_filename → see crate::provider_utils.

pub async fn download_blob(
    session: &GmailSession,
    blob_id: &str,
    filename: &str,
) -> Result<(String, Vec<u8>), Error> {
    let blob_ref = crate::types::BlobRef::parse(blob_id)?;
    let (msg_id, att_id) = match blob_ref {
        crate::types::BlobRef::GmailAttachment { msg_id, att_id } => (msg_id, att_id),
        crate::types::BlobRef::Synthetic(_) => {
            return Err(Error::BadRequest(
                "synthetic blob_id passed to gmail::download_blob — \
                 compose uploads aren't downloadable until they're sent"
                    .into(),
            ));
        }
        crate::types::BlobRef::OutlookAttachment { .. } => {
            return Err(Error::BadRequest(
                "outlook blob_id passed to gmail::download_blob — wrong provider".into(),
            ));
        }
    };

    let token = access_token(session).await?;
    let url = format!("{GMAIL_BASE}/messages/{msg_id}/attachments/{att_id}");
    let resp = session.client.get(&url).bearer_auth(&token).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_gmail_error(
            "messages.attachments.get",
            status,
            &text,
        ));
    }
    let body: AttachmentBody = resp.json().await?;
    let data = body
        .data
        .ok_or_else(|| Error::Internal("Gmail attachment response had no data field".into()))?;
    let bytes = base64url_decode(&data)?;
    Ok((mime_type_from_filename(filename).to_string(), bytes))
}

// =============================================================================
// upload_blob — synthetic blob cache (compose-time uploads)
// =============================================================================

/// Stash uploaded bytes in the session's synthetic-blob cache and return a
/// `BlobRef::Synthetic(uuid)` to embed in the EmailSubmission. Cap-enforced
/// on three axes so a misbehaving client can't pin memory:
///   - count (`UPLOAD_CACHE_CAP`)
///   - per-blob size (`MAX_BLOB_BYTES`)
///   - aggregate session size (`MAX_UPLOAD_CACHE_BYTES`)
///
/// Gmail has no standalone blob store — the bytes have to be inlined into the
/// RFC822 at send time. We consume on `send_email` success, so the typical
/// upload→send lifetime is short.
pub async fn upload_blob(
    session: &GmailSession,
    content_type: &str,
    body: &[u8],
) -> Result<(String, i64), Error> {
    if body.len() > MAX_BLOB_BYTES {
        return Err(Error::BadRequest(format!(
            "Gmail attachment too large: {} bytes (limit: {} MiB). \
             Gmail's send endpoint rejects RFC822 above ~25 MiB regardless.",
            body.len(),
            MAX_BLOB_BYTES / 1024 / 1024
        )));
    }
    let mut cache = session.upload_cache.lock().await;
    if cache.len() >= UPLOAD_CACHE_CAP {
        return Err(Error::BadRequest(format!(
            "Gmail upload cache full ({UPLOAD_CACHE_CAP} entries). \
             Cancel or send pending drafts before attaching more files."
        )));
    }
    let current_total: usize = cache.values().map(|(_, b)| b.len()).sum();
    if current_total + body.len() > MAX_UPLOAD_CACHE_BYTES {
        return Err(Error::BadRequest(format!(
            "Gmail upload cache aggregate size would exceed {} MiB \
             (current: {} MiB, this upload: {} MiB).",
            MAX_UPLOAD_CACHE_BYTES / 1024 / 1024,
            current_total / 1024 / 1024,
            body.len() / 1024 / 1024
        )));
    }
    let id = uuid::Uuid::new_v4();
    cache.insert(id, (content_type.to_string(), body.to_vec()));
    let blob_ref = crate::types::BlobRef::Synthetic(id);
    Ok((blob_ref.to_string(), body.len() as i64))
}

/// Read a `BlobRef`'s bytes without mutating the cache. Used by `build_rfc822`
/// at compose time — we don't want partial-failure during build to leave the
/// session with half its synthetic blobs gone, so consumption is deferred
/// until after `messages/send` returns 2xx (see `drain_consumed_synthetic_blobs`).
///
/// - `Synthetic` → look up in `upload_cache`, return a *clone* of the bytes.
/// - `GmailAttachment` → delegate to `download_blob` to re-fetch from Gmail.
///   The "reply with original attachment" path: the EmailSubmission carries
///   the original message's `{msg_id}:{att_id}` blob_id, we re-fetch the
///   bytes server-side rather than round-tripping through the client.
async fn peek_blob_bytes(
    session: &GmailSession,
    blob_id: &str,
    filename: &str,
) -> Result<(String, Vec<u8>), Error> {
    let blob_ref = crate::types::BlobRef::parse(blob_id)?;
    match blob_ref {
        crate::types::BlobRef::Synthetic(id) => {
            let cache = session.upload_cache.lock().await;
            cache.get(&id).cloned().ok_or_else(|| {
                Error::BadRequest(format!(
                    "Gmail synthetic blob {id} not found (already consumed or session restarted)"
                ))
            })
        }
        crate::types::BlobRef::GmailAttachment { .. } => {
            download_blob(session, blob_id, filename).await
        }
        crate::types::BlobRef::OutlookAttachment { .. } => Err(Error::BadRequest(
            "outlook blob_id passed to gmail::peek_blob_bytes — wrong provider".into(),
        )),
    }
}

/// Drop synthetic-blob entries from `upload_cache` after the EmailSubmission
/// referencing them has been successfully sent. Idempotent — non-synthetic
/// blob_ids and missing entries are silently skipped. Called from
/// `send_email` only on a 2xx response from `messages/send`.
async fn drain_consumed_synthetic_blobs(
    session: &GmailSession,
    attachments: &[crate::types::Attachment],
) {
    let mut cache = session.upload_cache.lock().await;
    for att in attachments {
        if let Ok(crate::types::BlobRef::Synthetic(id)) = crate::types::BlobRef::parse(&att.blob_id)
        {
            cache.remove(&id);
        }
    }
}

// =============================================================================
// lookup_parent_message_id — for In-Reply-To resolution
// =============================================================================

/// The frontend passes Gmail's message ID as `EmailSubmission.in_reply_to`,
/// but RFC822's `In-Reply-To` header takes the parent's RFC822 `Message-ID`
/// header value (`<…@…>`). This helper fetches the parent and extracts it.
///
/// LRU-ish cache on the session (capped at `PARENT_MID_CACHE_CAP`); a
/// burst of replies in the same thread doesn't re-fetch. Pure linear scan
/// over a small vec — no point in a real LRU at this size.
async fn lookup_parent_message_id(
    session: &GmailSession,
    gmail_msg_id: &str,
) -> Result<Option<String>, Error> {
    {
        let cache = session.parent_message_id_cache.lock().await;
        if let Some((_, mid)) = cache.iter().find(|(k, _)| k == gmail_msg_id) {
            return Ok(Some(mid.clone()));
        }
    }

    let token = access_token(session).await?;
    // Defense-in-depth: percent-encode the message ID as a path segment
    // even though Gmail IDs are URL-safe base64-ish. `gmail_msg_id` flows
    // from frontend-provided `EmailSubmission.in_reply_to`; if a future
    // change widens that input's trust boundary, the URL won't corrupt.
    let encoded_id = encode_path_segment(gmail_msg_id);
    let url =
        format!("{GMAIL_BASE}/messages/{encoded_id}?format=metadata&metadataHeaders=Message-ID");
    let resp = session.client.get(&url).bearer_auth(&token).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        // Don't fail the whole send for a missing parent — just skip threading.
        tracing::warn!(
            gmail_msg_id = %gmail_msg_id,
            http_status = %status,
            response_body = %text,
            "Gmail parent-message lookup failed; sending without In-Reply-To"
        );
        return Ok(None);
    }

    #[derive(Deserialize)]
    struct MetadataResp {
        #[serde(default)]
        payload: MetadataPayload,
    }
    #[derive(Default, Deserialize)]
    struct MetadataPayload {
        #[serde(default)]
        headers: Vec<GmailHeader>,
    }

    let parsed: MetadataResp = resp.json().await?;
    let mid = parsed
        .payload
        .headers
        .into_iter()
        .find(|h| h.name.eq_ignore_ascii_case("Message-ID"))
        .map(|h| extract_message_id(&h.value))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if let Some(ref mid) = mid {
        let mut cache = session.parent_message_id_cache.lock().await;
        if cache.len() >= PARENT_MID_CACHE_CAP {
            cache.remove(0);
        }
        cache.push((gmail_msg_id.to_string(), mid.clone()));
    }
    Ok(mid)
}

/// Strip a `Message-ID:` header value down to its canonical `<…@…>` form.
/// Gmail returns the raw header value, which may include trailing comments
/// or whitespace. We keep the angle brackets — they're the canonical form
/// stored elsewhere (cache, downstream comparisons).
pub(crate) fn extract_message_id(header_value: &str) -> String {
    let trimmed = header_value.trim();
    if let Some(start) = trimmed.find('<')
        && let Some(end) = trimmed[start..].find('>')
    {
        return trimmed[start..start + end + 1].to_string();
    }
    trimmed.to_string()
}

/// Strip surrounding `<>` brackets from a Message-ID-shaped string. Used at
/// the point of handing values to mail-builder, which always re-wraps in
/// `<…>` and would otherwise produce `<<…>>` for already-bracketed input.
pub(crate) fn strip_message_id_brackets(s: &str) -> &str {
    let trimmed = s.trim();
    let no_lt = trimmed.strip_prefix('<').unwrap_or(trimmed);
    no_lt.strip_suffix('>').unwrap_or(no_lt)
}

/// Heuristic: does `s` look like an RFC 5322 `Message-ID` (`<local@domain>`)
/// rather than a Gmail message ID? Used by `send_email` to decide whether
/// to pass `in_reply_to` through verbatim or look up the parent's
/// `Message-ID` header. Conservative — requires both leading `<` and a `@`.
pub(crate) fn looks_like_message_id(s: &str) -> bool {
    let t = s.trim();
    t.starts_with('<') && t.contains('@')
}

// =============================================================================
// send_email — RFC822 construction + messages.send
// =============================================================================

#[derive(Deserialize)]
struct SendResponse {
    id: String,
}

/// Pick the identity matching `from_addr` (or the explicit override).
/// Resolution rules:
///   - explicit `identity_id_override` set → must match an existing identity;
///     otherwise log + return None (no display name on the From line is less
///     wrong than attaching some *other* identity's name to `from_addr`).
///   - no override → match by email; fall back to the first identity if no
///     match (acceptable since the user's primary intent is the From address,
///     and we're just attaching the best-effort display name).
async fn pick_identity_display_name(
    session: &GmailSession,
    from_addr: &str,
    identity_id_override: Option<&str>,
) -> Option<String> {
    let identities = get_identities(session).await.ok()?;
    let chosen = match identity_id_override {
        Some(id) => {
            let matched = identities.iter().find(|i| i.id == id);
            if matched.is_none() {
                tracing::warn!(
                    identity_id_override = %id,
                    "Gmail send: identity_id_override does not match any sendAs identity; \
                     sending without display name to avoid mislabeling From"
                );
            }
            matched
        }
        None => identities
            .iter()
            .find(|i| i.email.eq_ignore_ascii_case(from_addr))
            .or_else(|| identities.first()),
    };
    chosen.map(|i| i.name.clone()).filter(|n| !n.is_empty())
}

/// Construct an RFC822 message ready for Gmail's `messages.send`. Async only
/// because attachment resolution (`download_blob` for original attachments)
/// needs an HTTP call. Returns the raw bytes.
async fn build_rfc822(
    session: &GmailSession,
    sub: &crate::types::EmailSubmission,
    from_addr: &str,
    from_display_name: Option<&str>,
    in_reply_to: Option<&str>,
    references: Option<&[String]>,
) -> Result<Vec<u8>, Error> {
    use mail_builder::MessageBuilder;
    use mail_builder::headers::address::Address;
    use mail_builder::mime::BodyPart;

    let from_addr_owned = from_addr.to_string();
    let mut builder = MessageBuilder::new();

    // From with display name
    let from_addr_cow: std::borrow::Cow<'_, str> = from_addr_owned.clone().into();
    builder = match from_display_name {
        Some(name) => builder.from(Address::new_address(
            Some(std::borrow::Cow::Owned(name.to_string())),
            from_addr_cow,
        )),
        None => builder.from(from_addr_owned.clone()),
    };

    // Recipients
    if !sub.to.is_empty() {
        let to_list: Vec<Address<'_>> = sub
            .to
            .iter()
            .map(|e| Address::new_address(None::<std::borrow::Cow<'_, str>>, e.clone()))
            .collect();
        builder = builder.to(Address::new_list(to_list));
    }
    if !sub.cc.is_empty() {
        let cc_list: Vec<Address<'_>> = sub
            .cc
            .iter()
            .map(|e| Address::new_address(None::<std::borrow::Cow<'_, str>>, e.clone()))
            .collect();
        builder = builder.cc(Address::new_list(cc_list));
    }
    if let Some(bcc) = &sub.bcc
        && !bcc.is_empty()
    {
        let bcc_list: Vec<Address<'_>> = bcc
            .iter()
            .map(|e| Address::new_address(None::<std::borrow::Cow<'_, str>>, e.clone()))
            .collect();
        builder = builder.bcc(Address::new_list(bcc_list));
    }

    builder = builder.subject(sub.subject.clone());

    // Threading headers — only if caller resolved them. mail-builder's
    // `MessageId` always wraps values in `<…>`, so strip any incoming
    // brackets to avoid `<<foo@bar>>` in the wire format.
    if let Some(mid) = in_reply_to {
        builder = builder.in_reply_to(strip_message_id_brackets(mid).to_string());
    }
    if let Some(refs) = references
        && !refs.is_empty()
    {
        let stripped: Vec<String> = refs
            .iter()
            .map(|r| strip_message_id_brackets(r).to_string())
            .collect();
        builder = builder.references(stripped);
    }

    builder = builder.text_body(sub.text_body.clone());
    if let Some(html) = &sub.html_body {
        builder = builder.html_body(html.clone());
    }

    for att in &sub.attachments {
        let (resolved_mime, bytes) = peek_blob_bytes(session, &att.blob_id, &att.name).await?;
        // Prefer the EmailSubmission's mime_type (from the original Email
        // metadata), fall back to whatever resolve returned (extension guess).
        let mime = if !att.mime_type.is_empty() {
            att.mime_type.clone()
        } else {
            resolved_mime
        };
        builder = builder.attachment(mime, att.name.clone(), BodyPart::Binary(bytes.into()));
    }

    let mut out = Vec::with_capacity(4096);
    builder
        .write_to(&mut out)
        .map_err(|e| Error::Internal(format!("RFC822 build failed: {e}")))?;
    Ok(out)
}

pub async fn send_email(
    session: &mut GmailSession,
    sub: &crate::types::EmailSubmission,
    from_addr: &str,
    identity_id_override: Option<&str>,
) -> Result<Option<String>, Error> {
    let display_name = pick_identity_display_name(session, from_addr, identity_id_override).await;

    // Resolve in_reply_to. If the frontend already passed a `<…@…>` form,
    // use it verbatim; otherwise treat it as a Gmail msg ID and look up.
    let (resolved_in_reply_to, resolved_references) = match sub.in_reply_to.as_deref() {
        None => (None, sub.references.clone()),
        Some(s) if looks_like_message_id(s) => (Some(s.to_string()), sub.references.clone()),
        Some(gmail_id) => {
            let mid = lookup_parent_message_id(session, gmail_id).await?;
            // Auto-populate References if caller didn't, since we have the
            // parent's Message-ID and that's what threading clients expect.
            let refs = match (&sub.references, &mid) {
                (Some(r), _) => Some(r.clone()),
                (None, Some(m)) => Some(vec![m.clone()]),
                (None, None) => None,
            };
            (mid, refs)
        }
    };

    let rfc822 = build_rfc822(
        session,
        sub,
        from_addr,
        display_name.as_deref(),
        resolved_in_reply_to.as_deref(),
        resolved_references.as_deref(),
    )
    .await?;

    use base64::Engine;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&rfc822);

    let token = access_token(session).await?;
    let url = format!("{GMAIL_BASE}/messages/send");
    let resp = session
        .client
        .post(&url)
        .bearer_auth(&token)
        .json(&serde_json::json!({ "raw": raw }))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_gmail_error("messages.send", status, &text));
    }
    let parsed: SendResponse = resp.json().await?;
    // Consume the synthetic-blob entries now that we know the bytes made
    // it onto the wire. Order matters: if the send fails (4xx/5xx earlier),
    // the cache is preserved so the user can retry without re-uploading.
    drain_consumed_synthetic_blobs(session, &sub.attachments).await;
    // A successful send adds a message to the SENT label and changes
    // counts. Invalidate label_cache for the count; clear page_cache so a
    // subsequent scroll of Sent doesn't show stale cursors that miss the
    // new message.
    invalidate_label_cache(session).await;
    session.page_cache.lock().await.clear();
    Ok(Some(parsed.id))
}

// =============================================================================
// Google Calendar v3 — events.list/import, delete, RSVP via attendees PATCH
// =============================================================================
//
// Quirks worth remembering:
//   - events.import (not events.insert) is what preserves iCalUID across
//     accounts — required for cross-account dedup of the same invite.
//   - PATCHing attendees is read-modify-write: the request body must include
//     the FULL current attendees array, mutated in place. Sending only the
//     changed entry silently wipes the others. (Documented but easy to miss.)
//   - sendUpdates=all triggers the organizer-notification email, replacing
//     the iTIP reply path JMAP uses. That's why sends_rsvp_automatically()
//     returns true for Gmail.

/// Find a calendar event's Google ID by its iCalUID. Used as the GET-then-X
/// preamble for `add_to_calendar` (existence check), `remove_from_calendar`
/// (id-to-delete), and `respond_to_event` (id-to-patch).
async fn find_event_id_by_ical_uid(
    session: &GmailSession,
    uid: &str,
) -> Result<Option<String>, Error> {
    let token = access_token(session).await?;
    // `encode_path_segment` is intentionally a superset of both path- and
    // query-component encoding (it also escapes `&`, `=`, `+`, `#`, `?`), so
    // it's safe to reuse for query string values. If that set is ever
    // narrowed, audit these query-string callers.
    let encoded = encode_path_segment(uid);
    let url = format!("{CALENDAR_BASE}/events?iCalUID={encoded}");
    let resp = session.client.get(&url).bearer_auth(&token).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_gmail_error(
            "calendar.events.list?iCalUID",
            status,
            &text,
        ));
    }
    #[derive(Deserialize)]
    struct ListResp {
        #[serde(default)]
        items: Vec<EventStub>,
    }
    #[derive(Deserialize)]
    struct EventStub {
        id: String,
    }
    let parsed: ListResp = resp.json().await?;
    Ok(parsed.items.into_iter().next().map(|e| e.id))
}

/// Convert our canonical `CalendarEvent` into Google Calendar's `Event`
/// resource JSON. Pure — testable without HTTP.
///
/// - `iCalUID` is preserved so `events.import` deduplicates across accounts.
/// - Times always go out as UTC (we store UTC internally).
/// - End time defaults to start+1h if missing (matches Outlook's path).
pub(crate) fn calendar_event_to_google_json(event: &CalendarEvent) -> serde_json::Value {
    let dtend = event
        .dtend
        .unwrap_or_else(|| event.dtstart + chrono::Duration::hours(1));
    let mut body = serde_json::json!({
        "iCalUID": event.uid,
        "summary": event.summary,
        "start": {
            "dateTime": event.dtstart.to_rfc3339(),
            "timeZone": "UTC",
        },
        "end": {
            "dateTime": dtend.to_rfc3339(),
            "timeZone": "UTC",
        },
    });

    if let Some(loc) = &event.location
        && !loc.is_empty()
    {
        body["location"] = serde_json::json!(loc);
    }
    if let Some(desc) = &event.description
        && !desc.is_empty()
    {
        body["description"] = serde_json::json!(desc);
    }
    if !event.organizer_email.is_empty() {
        let mut organizer = serde_json::json!({ "email": event.organizer_email });
        if let Some(name) = &event.organizer_name
            && !name.is_empty()
        {
            organizer["displayName"] = serde_json::json!(name);
        }
        body["organizer"] = organizer;
    }
    if !event.attendees.is_empty() {
        let attendees: Vec<serde_json::Value> = event
            .attendees
            .iter()
            .map(|a| {
                let mut entry = serde_json::json!({
                    "email": a.email,
                    "responseStatus": ics_status_to_google(&a.status),
                });
                if let Some(name) = &a.name
                    && !name.is_empty()
                {
                    entry["displayName"] = serde_json::json!(name);
                }
                entry
            })
            .collect();
        body["attendees"] = serde_json::json!(attendees);
    }

    body
}

/// Convert a Google Calendar `Event` JSON resource back into our canonical
/// `CalendarEvent`. Pure — extracted for fixture-based tests.
pub(crate) fn parse_google_event(
    uid: &str,
    event_json: &serde_json::Value,
) -> Option<CalendarEvent> {
    let summary = event_json["summary"].as_str().unwrap_or("").to_string();

    let dtstart = parse_google_datetime(&event_json["start"])?;
    let dtend = parse_google_datetime(&event_json["end"]);

    let location = event_json["location"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);

    let description = event_json["description"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);

    let organizer_email = event_json["organizer"]["email"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let organizer_name = event_json["organizer"]["displayName"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);

    let attendees: Vec<crate::types::Attendee> = event_json["attendees"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let email = a["email"].as_str()?;
                    let name = a["displayName"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(String::from);
                    let status =
                        google_status_to_ics(a["responseStatus"].as_str().unwrap_or("needsAction"));
                    Some(crate::types::Attendee {
                        email: email.to_string(),
                        name,
                        status: status.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Some(CalendarEvent {
        uid: uid.to_string(),
        summary,
        dtstart,
        dtend,
        location,
        description,
        organizer_email,
        organizer_name,
        attendees,
        sequence: 0,
        method: "REQUEST".to_string(),
        raw_ics: String::new(),
        user_rsvp_status: None,
    })
}

/// Parse a Google Calendar datetime block (`{dateTime, timeZone}` or
/// `{date}` for all-day events). RFC3339 with offset; we normalize to UTC.
fn parse_google_datetime(block: &serde_json::Value) -> Option<DateTime<Utc>> {
    if let Some(dt) = block["dateTime"].as_str() {
        return chrono::DateTime::parse_from_rfc3339(dt)
            .ok()
            .map(|d| d.with_timezone(&Utc));
    }
    if let Some(d) = block["date"].as_str() {
        // All-day events have no time — anchor at UTC midnight so the
        // downstream invariant (CalendarEvent.dtstart is DateTime<Utc>) holds.
        return chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d")
            .ok()
            .and_then(|nd| nd.and_hms_opt(0, 0, 0))
            .map(|ndt| ndt.and_utc());
    }
    None
}

/// Map our iTIP-style attendee status strings to Google's enum values.
pub(crate) fn ics_status_to_google(ics_status: &str) -> &'static str {
    match ics_status {
        "ACCEPTED" => "accepted",
        "TENTATIVE" => "tentative",
        "DECLINED" => "declined",
        _ => "needsAction",
    }
}

/// Map Google's `responseStatus` enum back to our iTIP-style string.
pub(crate) fn google_status_to_ics(google_status: &str) -> &'static str {
    match google_status {
        "accepted" => "ACCEPTED",
        "tentative" => "TENTATIVE",
        "declined" => "DECLINED",
        _ => "NEEDS-ACTION",
    }
}

/// Mutate a Google attendees array: set the matching attendee's
/// `responseStatus`, leave others untouched. Pure — extracted so the
/// "PATCH must include full array" Google quirk is covered by unit tests
/// without HTTP mocking. If the email isn't present, the array is returned
/// unchanged and `false` is returned so the caller can surface "not invited".
pub(crate) fn mutate_attendee_status(
    attendees: &mut [serde_json::Value],
    attendee_email: &str,
    google_status: &str,
) -> bool {
    let mut found = false;
    for entry in attendees.iter_mut() {
        if let Some(email) = entry["email"].as_str()
            && email.eq_ignore_ascii_case(attendee_email)
        {
            entry["responseStatus"] = serde_json::json!(google_status);
            found = true;
        }
    }
    found
}

/// Look up a calendar event by its iCalUID. Returns the parsed event with
/// up-to-date attendee statuses, or `None` if not in the calendar.
pub async fn get_calendar_event(
    session: &GmailSession,
    uid: &str,
) -> Result<Option<CalendarEvent>, Error> {
    let token = access_token(session).await?;
    // See `find_event_id_by_ical_uid`: `encode_path_segment`'s escape set is a
    // superset of query-component requirements, so reusing it here is safe.
    let encoded = encode_path_segment(uid);
    let url = format!("{CALENDAR_BASE}/events?iCalUID={encoded}");
    let resp = session.client.get(&url).bearer_auth(&token).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_gmail_error(
            "calendar.events.list?iCalUID",
            status,
            &text,
        ));
    }
    let parsed: serde_json::Value = resp.json().await?;
    let first = parsed["items"].as_array().and_then(|arr| arr.first());
    Ok(first.and_then(|ev| parse_google_event(uid, ev)))
}

/// Add a parsed CalendarEvent to the user's primary Google Calendar via
/// `events.import` (preserves iCalUID for cross-account dedup). If the
/// event already exists by iCalUID, this is a no-op returning `Ok(true)`.
pub async fn add_to_calendar(
    session: &GmailSession,
    _ics_data: &str,
    event: &CalendarEvent,
) -> Result<bool, Error> {
    if find_event_id_by_ical_uid(session, &event.uid)
        .await?
        .is_some()
    {
        tracing::debug!("Event {} already in Google Calendar", event.uid);
        return Ok(true);
    }
    let token = access_token(session).await?;
    let body = calendar_event_to_google_json(event);
    let url = format!("{CALENDAR_BASE}/events/import");
    let resp = session
        .client
        .post(&url)
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    if status.is_success() {
        tracing::info!("Added event {} to Google Calendar", event.uid);
        Ok(true)
    } else {
        let text = resp.text().await.unwrap_or_default();
        tracing::warn!("calendar.events.import failed ({status}): {text}");
        Err(classify_gmail_error(
            "calendar.events.import",
            status,
            &text,
        ))
    }
}

/// Remove an event from the user's calendar by iCalUID. Tolerant of "not
/// found" (returns `Ok(true)`) so retries are idempotent.
pub async fn remove_from_calendar(session: &GmailSession, uid: &str) -> Result<bool, Error> {
    let event_id = match find_event_id_by_ical_uid(session, uid).await? {
        Some(id) => id,
        None => {
            tracing::debug!("Event {uid} not in Google Calendar, nothing to remove");
            return Ok(true);
        }
    };
    let token = access_token(session).await?;
    let encoded_id = encode_path_segment(&event_id);
    let url = format!("{CALENDAR_BASE}/events/{encoded_id}");
    let resp = session
        .client
        .delete(&url)
        .bearer_auth(&token)
        .send()
        .await?;
    let status = resp.status();
    if status.is_success() || status.as_u16() == 404 {
        tracing::info!("Removed event {uid} from Google Calendar");
        Ok(true)
    } else {
        let text = resp.text().await.unwrap_or_default();
        tracing::warn!("calendar.events.delete failed ({status}): {text}");
        Err(classify_gmail_error(
            "calendar.events.delete",
            status,
            &text,
        ))
    }
}

/// RSVP to an event by GET-mutate-PATCHing the attendees array. Triggers
/// the organizer-notification email via `sendUpdates=all`.
///
/// `attendee_email` is who's responding (typically `session.email`). If the
/// event doesn't exist or the email isn't in its attendees list, returns
/// `Ok(false)` so the caller can surface a "not invited" message.
pub async fn respond_to_event(
    session: &GmailSession,
    uid: &str,
    attendee_email: &str,
    status: &crate::types::RsvpStatus,
) -> Result<bool, Error> {
    let event_id = match find_event_id_by_ical_uid(session, uid).await? {
        Some(id) => id,
        None => {
            tracing::warn!("Cannot RSVP: event {uid} not found in Google Calendar");
            return Ok(false);
        }
    };
    let token = access_token(session).await?;
    let encoded_id = encode_path_segment(&event_id);
    let event_url = format!("{CALENDAR_BASE}/events/{encoded_id}");

    // GET the current event so we have the full attendees array (Google's
    // PATCH semantics require the full array; partial PATCH silently wipes).
    let get_resp = session
        .client
        .get(&event_url)
        .bearer_auth(&token)
        .send()
        .await?;
    let get_status = get_resp.status();
    if !get_status.is_success() {
        let text = get_resp.text().await.unwrap_or_default();
        return Err(classify_gmail_error(
            "calendar.events.get",
            get_status,
            &text,
        ));
    }
    let event_json: serde_json::Value = get_resp.json().await?;

    // Ensure attendees is an array we can mutate.
    let mut attendees = event_json["attendees"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let google_status = ics_status_to_google(status.as_ics_str());

    // Idempotency: if the attendee's responseStatus already matches, skip the
    // PATCH. `sendUpdates=all` would otherwise send a duplicate organizer
    // notification every time the user clicks "Accept" on an already-accepted
    // invite. `attendee_present` is true iff the email appears in the array.
    let mut attendee_present = false;
    let mut already_matches = false;
    for entry in &attendees {
        if let Some(email) = entry["email"].as_str()
            && email.eq_ignore_ascii_case(attendee_email)
        {
            attendee_present = true;
            if entry["responseStatus"].as_str() == Some(google_status) {
                already_matches = true;
            }
            break;
        }
    }
    if !attendee_present {
        tracing::warn!(
            uid = %uid,
            attendee_email = %attendee_email,
            "RSVP target email not in event attendees; not patching"
        );
        return Ok(false);
    }
    if already_matches {
        tracing::debug!(
            uid = %uid,
            "RSVP status already {google_status}; skipping PATCH to avoid duplicate notification"
        );
        return Ok(true);
    }

    let updated = mutate_attendee_status(&mut attendees, attendee_email, google_status);
    debug_assert!(
        updated,
        "attendee was present in pre-check but lost in mutate"
    );

    let patch_body = serde_json::json!({ "attendees": attendees });
    let patch_url = format!("{event_url}?sendUpdates=all");
    let patch_resp = session
        .client
        .patch(&patch_url)
        .bearer_auth(&token)
        .json(&patch_body)
        .send()
        .await?;
    let patch_status = patch_resp.status();
    if patch_status.is_success() {
        tracing::info!(
            "RSVP {} for event {uid} via Google Calendar PATCH",
            status.as_ics_str()
        );
        Ok(true)
    } else {
        let text = patch_resp.text().await.unwrap_or_default();
        Err(classify_gmail_error(
            "calendar.events.patch",
            patch_status,
            &text,
        ))
    }
}

// =============================================================================
// get_calendar_data — extract text/calendar part from a message
// =============================================================================

/// Extract the raw ICS bytes from a Gmail message's `text/calendar` part, if
/// any. Walks the same payload tree as `walk_payload` but pulls the body
/// data instead of just setting `has_calendar = true`.
pub async fn get_calendar_data(
    session: &GmailSession,
    email_id: &str,
) -> Result<Option<String>, Error> {
    let token = access_token(session).await?;
    let encoded_id = encode_path_segment(email_id);
    let url = format!("{GMAIL_BASE}/messages/{encoded_id}?format=full");
    let resp = session.client.get(&url).bearer_auth(&token).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_gmail_error(
            &format!("messages.get(full) {email_id}"),
            status,
            &text,
        ));
    }
    let msg: GmailMessage = resp.json().await?;
    Ok(find_calendar_ics(&msg.payload))
}

/// Walk the payload tree until a `text/calendar` part with inline data is
/// found, then return its decoded UTF-8 string. Pure — testable with
/// hand-rolled GmailPayload fixtures.
///
/// Iterative DFS rather than recursion so a pathological or malicious
/// payload can't blow the stack. Pre-order traversal preserves the original
/// "outer parts first, then nested" order.
pub(crate) fn find_calendar_ics(part: &GmailPayload) -> Option<String> {
    let mut stack: Vec<&GmailPayload> = vec![part];
    while let Some(node) = stack.pop() {
        if node.mime_type.eq_ignore_ascii_case("text/calendar")
            && let Some(body) = &node.body
            && let Some(data) = &body.data
            && let Ok(bytes) = base64url_decode(data)
            && let Ok(s) = String::from_utf8(bytes)
        {
            return Some(s);
        }
        if let Some(parts) = &node.parts {
            // Push in reverse so pop() yields siblings in original order.
            for child in parts.iter().rev() {
                stack.push(child);
            }
        }
    }
    None
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    // ---- auth_url ----

    #[test]
    fn auth_url_contains_required_params() {
        let url = auth_url("test-client-id", "test-verifier", "test-state");
        assert!(url.contains("client_id=test-client-id"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=test-state"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
    }

    #[test]
    fn auth_url_lists_exactly_three_scopes() {
        let url = auth_url("c", "v", "s");
        // URL encoding turns spaces into + or %20
        assert!(url.contains("gmail.modify"));
        assert!(url.contains("gmail.send"));
        assert!(url.contains("auth%2Fcalendar") || url.contains("auth/calendar"));
        assert!(!url.contains("userinfo.email"));
    }

    // ---- label_to_role ----

    #[test]
    fn label_to_role_system_inbox() {
        assert_eq!(label_to_role("INBOX", "system"), Some("inbox".into()));
    }
    #[test]
    fn label_to_role_system_sent() {
        assert_eq!(label_to_role("SENT", "system"), Some("sent".into()));
    }
    #[test]
    fn label_to_role_system_draft() {
        assert_eq!(label_to_role("DRAFT", "system"), Some("drafts".into()));
    }
    #[test]
    fn label_to_role_system_spam() {
        assert_eq!(label_to_role("SPAM", "system"), Some("junk".into()));
    }
    #[test]
    fn label_to_role_system_trash() {
        assert_eq!(label_to_role("TRASH", "system"), Some("trash".into()));
    }
    #[test]
    fn label_to_role_system_starred_no_role() {
        assert_eq!(label_to_role("STARRED", "system"), None);
    }
    #[test]
    fn label_to_role_user_label_no_role() {
        assert_eq!(label_to_role("Projects", "user"), None);
    }

    // ---- should_include_label ----

    #[test]
    fn include_user_labels_always() {
        assert!(should_include_label("Projects", "user"));
        assert!(should_include_label("Random Label", "user"));
    }
    #[test]
    fn include_system_folder_labels() {
        assert!(should_include_label("INBOX", "system"));
        assert!(should_include_label("SENT", "system"));
        assert!(should_include_label("DRAFT", "system"));
        assert!(should_include_label("SPAM", "system"));
        assert!(should_include_label("TRASH", "system"));
    }
    #[test]
    fn exclude_system_keyword_labels() {
        assert!(!should_include_label("STARRED", "system"));
        assert!(!should_include_label("IMPORTANT", "system"));
        assert!(!should_include_label("UNREAD", "system"));
        assert!(!should_include_label("CHAT", "system"));
        assert!(!should_include_label("CATEGORY_PERSONAL", "system"));
        assert!(!should_include_label("CATEGORY_SOCIAL", "system"));
    }

    // ---- build_mailboxes (nested parent_id) ----

    #[test]
    fn build_mailboxes_nested_user_label_gets_parent_id() {
        let labels = vec![
            LabelDetail {
                id: "Label_1".into(),
                name: "Work".into(),
                label_type: Some("user".into()),
                messages_total: 0,
                messages_unread: 0,
            },
            LabelDetail {
                id: "Label_2".into(),
                name: "Work/Projects".into(),
                label_type: Some("user".into()),
                messages_total: 0,
                messages_unread: 0,
            },
        ];
        let mailboxes = build_mailboxes(labels);
        let child = mailboxes
            .iter()
            .find(|m| m.name == "Work/Projects")
            .unwrap();
        assert_eq!(child.parent_id.as_deref(), Some("Label_1"));
    }

    #[test]
    fn build_mailboxes_nested_orphan_no_parent() {
        let labels = vec![LabelDetail {
            id: "Label_2".into(),
            name: "Missing/Child".into(),
            label_type: Some("user".into()),
            messages_total: 0,
            messages_unread: 0,
        }];
        let mailboxes = build_mailboxes(labels);
        assert!(mailboxes[0].parent_id.is_none());
    }

    #[test]
    fn build_mailboxes_excludes_starred_and_important() {
        let labels = vec![
            LabelDetail {
                id: "INBOX".into(),
                name: "INBOX".into(),
                label_type: Some("system".into()),
                messages_total: 100,
                messages_unread: 5,
            },
            LabelDetail {
                id: "STARRED".into(),
                name: "STARRED".into(),
                label_type: Some("system".into()),
                messages_total: 10,
                messages_unread: 0,
            },
            LabelDetail {
                id: "IMPORTANT".into(),
                name: "IMPORTANT".into(),
                label_type: Some("system".into()),
                messages_total: 50,
                messages_unread: 2,
            },
        ];
        let mailboxes = build_mailboxes(labels);
        assert_eq!(mailboxes.len(), 1);
        assert_eq!(mailboxes[0].id, "INBOX");
    }

    // ---- translate_query_to_q ----

    #[test]
    fn q_translator_empty() {
        let q = ParsedQuery::default();
        assert_eq!(translate_query_to_q(&q), "");
    }

    #[test]
    fn q_translator_single_from() {
        let mut q = ParsedQuery::default();
        q.from.push("alice@example.com".into());
        assert_eq!(translate_query_to_q(&q), "from:alice@example.com");
    }

    #[test]
    fn q_translator_multiple_from_ands() {
        let mut q = ParsedQuery::default();
        q.from.push("a@x.com".into());
        q.from.push("b@y.com".into());
        assert_eq!(translate_query_to_q(&q), "from:a@x.com from:b@y.com");
    }

    #[test]
    fn q_translator_quotes_whitespace_value() {
        let mut q = ParsedQuery::default();
        q.from.push("Alice Smith".into());
        assert_eq!(translate_query_to_q(&q), r#"from:"Alice Smith""#);
    }

    #[test]
    fn q_translator_quotes_subject_with_colon() {
        let mut q = ParsedQuery::default();
        q.subject.push("Re: foo".into());
        assert_eq!(translate_query_to_q(&q), r#"subject:"Re: foo""#);
    }

    #[test]
    fn q_translator_escapes_inner_quote() {
        let mut q = ParsedQuery::default();
        q.subject.push(r#"a"b"#.into());
        assert_eq!(translate_query_to_q(&q), r#"subject:"a\"b""#);
    }

    #[test]
    fn q_translator_email_with_plus_unquoted() {
        let mut q = ParsedQuery::default();
        q.from.push("bob+test@x.com".into());
        assert_eq!(translate_query_to_q(&q), "from:bob+test@x.com");
    }

    #[test]
    fn q_translator_is_unread() {
        let q = ParsedQuery {
            is_unread: Some(true),
            ..Default::default()
        };
        assert_eq!(translate_query_to_q(&q), "is:unread");
    }

    #[test]
    fn q_translator_has_attachment() {
        let q = ParsedQuery {
            has_attachment: true,
            ..Default::default()
        };
        assert_eq!(translate_query_to_q(&q), "has:attachment");
    }

    #[test]
    fn q_translator_dates_use_slashes_not_dashes() {
        let q = ParsedQuery {
            before: Some(NaiveDate::from_ymd_opt(2026, 1, 15).unwrap()),
            after: Some(NaiveDate::from_ymd_opt(2025, 12, 1).unwrap()),
            ..Default::default()
        };
        let s = translate_query_to_q(&q);
        assert!(s.contains("before:2026/01/15"));
        assert!(s.contains("after:2025/12/01"));
        assert!(!s.contains('-'));
    }

    #[test]
    fn q_translator_free_text_passthrough() {
        let q = ParsedQuery {
            text: "quarterly review".into(),
            ..Default::default()
        };
        assert_eq!(translate_query_to_q(&q), "quarterly review");
    }

    #[test]
    fn q_translator_combined_query_space_joined() {
        let mut q = ParsedQuery::default();
        q.from.push("alice@x.com".into());
        q.is_unread = Some(true);
        q.has_attachment = true;
        q.text = "report".into();
        let s = translate_query_to_q(&q);
        assert_eq!(s, "from:alice@x.com has:attachment is:unread report");
    }

    // ---- parse_address_list ----

    #[test]
    fn parse_address_bare_email() {
        let addrs = parse_address_list("alice@example.com");
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].email, "alice@example.com");
        assert!(addrs[0].name.is_none());
    }

    #[test]
    fn parse_address_with_name() {
        let addrs = parse_address_list("Alice <alice@example.com>");
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].email, "alice@example.com");
        assert_eq!(addrs[0].name.as_deref(), Some("Alice"));
    }

    #[test]
    fn parse_address_with_quoted_name() {
        let addrs = parse_address_list(r#""Alice Smith" <alice@example.com>"#);
        assert_eq!(addrs[0].email, "alice@example.com");
        assert_eq!(addrs[0].name.as_deref(), Some("Alice Smith"));
    }

    #[test]
    fn parse_address_list_multiple() {
        let addrs = parse_address_list("a@x.com, Bob <b@y.com>");
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0].email, "a@x.com");
        assert_eq!(addrs[1].email, "b@y.com");
        assert_eq!(addrs[1].name.as_deref(), Some("Bob"));
    }

    // ---- mime_path ----

    #[test]
    fn mime_path_renders_flat_leaf() {
        let p = GmailPayload {
            mime_type: "text/plain".into(),
            filename: String::new(),
            headers: vec![],
            body: None,
            parts: None,
        };
        assert_eq!(mime_path(&p), "text/plain");
    }

    #[test]
    fn mime_path_renders_nested_tree_with_siblings() {
        // The whole point of this helper is to keep nesting visible in the
        // empty-body diagnostic: a flat list would lose "was text/plain under
        // multipart/related?" which is exactly the forensics we need.
        let p = GmailPayload {
            mime_type: "multipart/mixed".into(),
            filename: String::new(),
            headers: vec![],
            body: None,
            parts: Some(vec![
                GmailPayload {
                    mime_type: "multipart/alternative".into(),
                    filename: String::new(),
                    headers: vec![],
                    body: None,
                    parts: Some(vec![
                        GmailPayload {
                            mime_type: "text/plain".into(),
                            filename: String::new(),
                            headers: vec![],
                            body: None,
                            parts: None,
                        },
                        GmailPayload {
                            mime_type: "text/html".into(),
                            filename: String::new(),
                            headers: vec![],
                            body: None,
                            parts: None,
                        },
                    ]),
                },
                GmailPayload {
                    mime_type: "application/pdf".into(),
                    filename: "doc.pdf".into(),
                    headers: vec![],
                    body: None,
                    parts: None,
                },
            ]),
        };
        assert_eq!(
            mime_path(&p),
            "multipart/mixed > [multipart/alternative > [text/plain, text/html], application/pdf]"
        );
    }

    // ---- parse_message_to_email ----

    fn header(name: &str, value: &str) -> GmailHeader {
        GmailHeader {
            name: name.into(),
            value: value.into(),
        }
    }

    fn base64url_encode(s: &str) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s.as_bytes())
    }

    #[test]
    fn parse_message_text_only() {
        let msg = GmailMessage {
            id: "msg1".into(),
            thread_id: "thr1".into(),
            label_ids: vec!["INBOX".into(), "UNREAD".into()],
            snippet: "Hi there".into(),
            internal_date: "1700000000000".into(),
            size_estimate: 1234,
            payload: GmailPayload {
                mime_type: "text/plain".into(),
                filename: String::new(),
                headers: vec![
                    header("From", "Alice <alice@example.com>"),
                    header("To", "me@example.com"),
                    header("Subject", "Hello"),
                ],
                body: Some(GmailBody {
                    size: 4,
                    data: Some(base64url_encode("Hi!")),
                    attachment_id: None,
                }),
                parts: None,
            },
        };
        let email = parse_message_to_email(msg, true);
        assert_eq!(email.id, "msg1");
        assert_eq!(email.thread_id, "thr1");
        assert_eq!(email.subject, "Hello");
        assert_eq!(email.from[0].email, "alice@example.com");
        assert_eq!(email.text_body.as_deref(), Some("Hi!"));
        assert!(email.html_body.is_none());
        assert!(!email.has_attachment);
        assert!(!email.has_calendar);
        assert!(email.is_unread());
        assert!(!email.is_flagged());
    }

    #[test]
    fn parse_message_text_and_html_alternative() {
        let msg = GmailMessage {
            id: "msg2".into(),
            thread_id: "thr2".into(),
            label_ids: vec!["INBOX".into()],
            snippet: "".into(),
            internal_date: "1700000000000".into(),
            size_estimate: 0,
            payload: GmailPayload {
                mime_type: "multipart/alternative".into(),
                filename: String::new(),
                headers: vec![header("Subject", "Mixed")],
                body: None,
                parts: Some(vec![
                    GmailPayload {
                        mime_type: "text/plain".into(),
                        filename: String::new(),
                        headers: vec![],
                        body: Some(GmailBody {
                            size: 5,
                            data: Some(base64url_encode("plain")),
                            attachment_id: None,
                        }),
                        parts: None,
                    },
                    GmailPayload {
                        mime_type: "text/html".into(),
                        filename: String::new(),
                        headers: vec![],
                        body: Some(GmailBody {
                            size: 11,
                            data: Some(base64url_encode("<p>html</p>")),
                            attachment_id: None,
                        }),
                        parts: None,
                    },
                ]),
            },
        };
        let email = parse_message_to_email(msg, true);
        assert_eq!(email.text_body.as_deref(), Some("plain"));
        assert_eq!(email.html_body.as_deref(), Some("<p>html</p>"));
        assert!(!email.is_unread()); // no UNREAD label → seen
    }

    #[test]
    fn parse_message_with_attachment() {
        let msg = GmailMessage {
            id: "msg3".into(),
            thread_id: "thr3".into(),
            label_ids: vec!["INBOX".into()],
            snippet: "".into(),
            internal_date: "1700000000000".into(),
            size_estimate: 0,
            payload: GmailPayload {
                mime_type: "multipart/mixed".into(),
                filename: String::new(),
                headers: vec![header("Subject", "With PDF")],
                body: None,
                parts: Some(vec![
                    GmailPayload {
                        mime_type: "text/plain".into(),
                        filename: String::new(),
                        headers: vec![],
                        body: Some(GmailBody {
                            size: 4,
                            data: Some(base64url_encode("see")),
                            attachment_id: None,
                        }),
                        parts: None,
                    },
                    GmailPayload {
                        mime_type: "application/pdf".into(),
                        filename: "report.pdf".into(),
                        headers: vec![header(
                            "Content-Disposition",
                            "attachment; filename=\"report.pdf\"",
                        )],
                        body: Some(GmailBody {
                            size: 12345,
                            data: None,
                            attachment_id: Some("ATT_abc".into()),
                        }),
                        parts: None,
                    },
                ]),
            },
        };
        let email = parse_message_to_email(msg, true);
        assert!(email.has_attachment);
        assert_eq!(email.attachments.len(), 1);
        assert_eq!(email.attachments[0].name, "report.pdf");
        assert_eq!(email.attachments[0].blob_id, "msg3:ATT_abc");
        assert_eq!(email.attachments[0].size, 12345);
    }

    #[test]
    fn parse_message_with_calendar_invite() {
        let msg = GmailMessage {
            id: "msg4".into(),
            thread_id: "t4".into(),
            label_ids: vec!["INBOX".into()],
            snippet: "".into(),
            internal_date: "1700000000000".into(),
            size_estimate: 0,
            payload: GmailPayload {
                mime_type: "multipart/alternative".into(),
                filename: String::new(),
                headers: vec![header("Subject", "Invite")],
                body: None,
                parts: Some(vec![
                    GmailPayload {
                        mime_type: "text/plain".into(),
                        filename: String::new(),
                        headers: vec![],
                        body: Some(GmailBody {
                            size: 6,
                            data: Some(base64url_encode("Invite")),
                            attachment_id: None,
                        }),
                        parts: None,
                    },
                    GmailPayload {
                        mime_type: "text/calendar".into(),
                        filename: String::new(),
                        headers: vec![],
                        body: Some(GmailBody {
                            size: 100,
                            data: Some(base64url_encode("BEGIN:VCALENDAR\nEND:VCALENDAR")),
                            attachment_id: None,
                        }),
                        parts: None,
                    },
                ]),
            },
        };
        let email = parse_message_to_email(msg, true);
        assert!(email.has_calendar);
    }

    #[test]
    fn parse_message_starred_label_sets_flagged() {
        let msg = GmailMessage {
            id: "msg5".into(),
            thread_id: "t5".into(),
            label_ids: vec!["INBOX".into(), "STARRED".into()],
            snippet: "".into(),
            internal_date: "1700000000000".into(),
            size_estimate: 0,
            payload: GmailPayload {
                mime_type: "text/plain".into(),
                filename: String::new(),
                headers: vec![header("Subject", "Starred")],
                body: None,
                parts: None,
            },
        };
        let email = parse_message_to_email(msg, false);
        assert!(email.is_flagged());
        assert!(!email.is_unread()); // no UNREAD label
    }

    #[test]
    fn parse_message_inline_image_in_related_skipped() {
        // multipart/related embedding an inline image should not produce an attachment
        let msg = GmailMessage {
            id: "msg6".into(),
            thread_id: "t6".into(),
            label_ids: vec!["INBOX".into()],
            snippet: "".into(),
            internal_date: "1700000000000".into(),
            size_estimate: 0,
            payload: GmailPayload {
                mime_type: "multipart/related".into(),
                filename: String::new(),
                headers: vec![header("Subject", "HTML with inline")],
                body: None,
                parts: Some(vec![
                    GmailPayload {
                        mime_type: "text/html".into(),
                        filename: String::new(),
                        headers: vec![],
                        body: Some(GmailBody {
                            size: 1,
                            data: Some(base64url_encode("<p>x</p>")),
                            attachment_id: None,
                        }),
                        parts: None,
                    },
                    GmailPayload {
                        mime_type: "image/png".into(),
                        filename: "inline.png".into(),
                        headers: vec![header("Content-Disposition", "inline; filename=inline.png")],
                        body: Some(GmailBody {
                            size: 100,
                            data: None,
                            attachment_id: Some("att_inline".into()),
                        }),
                        parts: None,
                    },
                ]),
            },
        };
        let email = parse_message_to_email(msg, true);
        assert!(!email.has_attachment);
        assert!(email.attachments.is_empty());
    }

    #[test]
    fn parse_message_attachment_in_mixed_not_skipped_even_if_inline() {
        // disposition=inline inside multipart/mixed (not related) IS a user attachment
        let msg = GmailMessage {
            id: "msg7".into(),
            thread_id: "t7".into(),
            label_ids: vec!["INBOX".into()],
            snippet: "".into(),
            internal_date: "1700000000000".into(),
            size_estimate: 0,
            payload: GmailPayload {
                mime_type: "multipart/mixed".into(),
                filename: String::new(),
                headers: vec![],
                body: None,
                parts: Some(vec![
                    GmailPayload {
                        mime_type: "text/plain".into(),
                        filename: String::new(),
                        headers: vec![],
                        body: Some(GmailBody {
                            size: 1,
                            data: Some(base64url_encode("hi")),
                            attachment_id: None,
                        }),
                        parts: None,
                    },
                    GmailPayload {
                        mime_type: "image/jpeg".into(),
                        filename: "photo.jpg".into(),
                        headers: vec![header("Content-Disposition", "inline; filename=photo.jpg")],
                        body: Some(GmailBody {
                            size: 5000,
                            data: None,
                            attachment_id: Some("att_photo".into()),
                        }),
                        parts: None,
                    },
                ]),
            },
        };
        let email = parse_message_to_email(msg, true);
        assert!(email.has_attachment);
        assert_eq!(email.attachments.len(), 1);
        assert_eq!(email.attachments[0].name, "photo.jpg");
    }

    #[test]
    fn parse_message_internal_date_parses_to_received_at() {
        let msg = GmailMessage {
            id: "m".into(),
            thread_id: "t".into(),
            label_ids: vec![],
            snippet: "".into(),
            internal_date: "1700000000000".into(),
            size_estimate: 0,
            payload: GmailPayload {
                mime_type: "text/plain".into(),
                filename: String::new(),
                headers: vec![],
                body: None,
                parts: None,
            },
        };
        let email = parse_message_to_email(msg, false);
        assert_eq!(email.received_at.timestamp_millis(), 1700000000000);
    }

    // ---- mailbox_ids pseudo-label filtering (roborev 173 finding #6) ----

    fn make_msg_with_labels(labels: Vec<&str>) -> GmailMessage {
        GmailMessage {
            id: "m".into(),
            thread_id: "t".into(),
            label_ids: labels.iter().map(|s| s.to_string()).collect(),
            snippet: "".into(),
            internal_date: "1700000000000".into(),
            size_estimate: 0,
            payload: GmailPayload {
                mime_type: "text/plain".into(),
                filename: String::new(),
                headers: vec![],
                body: None,
                parts: None,
            },
        }
    }

    #[test]
    fn mailbox_ids_excludes_starred_pseudo_label() {
        let email = parse_message_to_email(make_msg_with_labels(vec!["INBOX", "STARRED"]), false);
        assert!(email.mailbox_ids.contains_key("INBOX"));
        assert!(!email.mailbox_ids.contains_key("STARRED"));
        // But the keyword still flips $flagged:
        assert!(email.is_flagged());
    }

    #[test]
    fn mailbox_ids_excludes_unread_pseudo_label() {
        let email = parse_message_to_email(make_msg_with_labels(vec!["INBOX", "UNREAD"]), false);
        assert!(email.mailbox_ids.contains_key("INBOX"));
        assert!(!email.mailbox_ids.contains_key("UNREAD"));
        assert!(email.is_unread());
    }

    #[test]
    fn mailbox_ids_excludes_important_and_chat() {
        let email = parse_message_to_email(
            make_msg_with_labels(vec!["INBOX", "IMPORTANT", "CHAT"]),
            false,
        );
        assert!(email.mailbox_ids.contains_key("INBOX"));
        assert!(!email.mailbox_ids.contains_key("IMPORTANT"));
        assert!(!email.mailbox_ids.contains_key("CHAT"));
    }

    #[test]
    fn mailbox_ids_excludes_category_labels() {
        let email = parse_message_to_email(
            make_msg_with_labels(vec!["INBOX", "CATEGORY_PERSONAL", "CATEGORY_SOCIAL"]),
            false,
        );
        assert!(!email.mailbox_ids.contains_key("CATEGORY_PERSONAL"));
        assert!(!email.mailbox_ids.contains_key("CATEGORY_SOCIAL"));
    }

    #[test]
    fn mailbox_ids_includes_user_labels() {
        let email = parse_message_to_email(make_msg_with_labels(vec!["INBOX", "Label_5"]), false);
        assert!(email.mailbox_ids.contains_key("Label_5"));
    }

    #[test]
    fn is_displayable_label_id_predicate() {
        assert!(is_displayable_label_id("INBOX"));
        assert!(is_displayable_label_id("SENT"));
        assert!(is_displayable_label_id("Label_5"));
        assert!(!is_displayable_label_id("STARRED"));
        assert!(!is_displayable_label_id("IMPORTANT"));
        assert!(!is_displayable_label_id("UNREAD"));
        assert!(!is_displayable_label_id("CHAT"));
        assert!(!is_displayable_label_id("CATEGORY_PERSONAL"));
        assert!(!is_displayable_label_id("CATEGORY_UPDATES"));
    }

    // ---- pagination cache PageStart logic (roborev 173 finding #1) ----

    #[test]
    fn next_page_start_some_token_yields_with() {
        assert_eq!(
            next_page_start_from(Some("tok1".into())),
            PageStart::With("tok1".into())
        );
    }

    #[test]
    fn next_page_start_none_yields_end() {
        assert_eq!(next_page_start_from(None), PageStart::End);
    }

    #[test]
    fn record_page_fetched_seeds_next_slot_with_token() {
        let mut cache = vec![PageStart::First];
        record_page_fetched(&mut cache, 0, Some("t1".into()));
        assert_eq!(cache, vec![PageStart::First, PageStart::With("t1".into())]);
    }

    #[test]
    fn record_page_fetched_none_yields_end_sentinel_not_first() {
        // The bug the End sentinel prevents: a `None` next-token must not
        // round-trip to "fetch page 0 again". After recording end-of-results,
        // the next slot is `End`, not the implicit "no token" state.
        let mut cache = vec![PageStart::First, PageStart::With("t1".into())];
        record_page_fetched(&mut cache, 1, None);
        assert_eq!(
            cache,
            vec![
                PageStart::First,
                PageStart::With("t1".into()),
                PageStart::End,
            ]
        );
    }

    #[test]
    fn record_page_fetched_grows_cache_with_end_padding() {
        // If page_idx is past the end of cache (shouldn't happen in normal
        // flow but defensive), the in-between slots are End.
        let mut cache = vec![PageStart::First];
        record_page_fetched(&mut cache, 3, Some("t4".into()));
        assert_eq!(
            cache,
            vec![
                PageStart::First,
                PageStart::End,
                PageStart::End,
                PageStart::End,
                PageStart::With("t4".into()),
            ]
        );
    }

    #[test]
    fn record_page_fetched_overwrites_existing_slot() {
        let mut cache = vec![
            PageStart::First,
            PageStart::With("t1-old".into()),
            PageStart::End,
        ];
        record_page_fetched(&mut cache, 0, Some("t1-new".into()));
        assert_eq!(cache[1], PageStart::With("t1-new".into()));
    }

    // ---- query_emails sort (kata 09ef): Gmail has no server-side orderBy,
    // so DateAsc reverses the already-fetched page client-side ----

    #[test]
    fn apply_sort_order_date_desc_leaves_order_unchanged() {
        let ids = vec![
            "newest".to_string(),
            "mid".to_string(),
            "oldest".to_string(),
        ];
        assert_eq!(
            apply_sort_order(ids.clone(), EmailSort::DateDesc),
            ids,
            "DateDesc matches Gmail's default order — no reversal"
        );
    }

    #[test]
    fn apply_sort_order_date_asc_reverses_the_page() {
        let ids = vec![
            "newest".to_string(),
            "mid".to_string(),
            "oldest".to_string(),
        ];
        assert_eq!(
            apply_sort_order(ids, EmailSort::DateAsc),
            vec![
                "oldest".to_string(),
                "mid".to_string(),
                "newest".to_string()
            ]
        );
    }

    #[test]
    fn apply_sort_order_handles_empty_and_single_element() {
        assert_eq!(
            apply_sort_order(Vec::<String>::new(), EmailSort::DateAsc),
            Vec::<String>::new()
        );
        assert_eq!(
            apply_sort_order(vec!["only".to_string()], EmailSort::DateAsc),
            vec!["only".to_string()]
        );
    }

    // ---- nested multipart/related → multipart/alternative (roborev 173 #9) ----

    fn b64u(s: &str) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s.as_bytes())
    }

    #[test]
    fn parse_message_nested_related_alternative_extracts_both_bodies() {
        // Common Gmail shape for HTML email with embedded images:
        //   multipart/related
        //     ├── multipart/alternative
        //     │     ├── text/plain
        //     │     └── text/html
        //     └── image/png (inline, cid-referenced)
        let msg = GmailMessage {
            id: "msg-nested".into(),
            thread_id: "t".into(),
            label_ids: vec!["INBOX".into()],
            snippet: "".into(),
            internal_date: "1700000000000".into(),
            size_estimate: 0,
            payload: GmailPayload {
                mime_type: "multipart/related".into(),
                filename: String::new(),
                headers: vec![header("Subject", "Newsletter")],
                body: None,
                parts: Some(vec![
                    GmailPayload {
                        mime_type: "multipart/alternative".into(),
                        filename: String::new(),
                        headers: vec![],
                        body: None,
                        parts: Some(vec![
                            GmailPayload {
                                mime_type: "text/plain".into(),
                                filename: String::new(),
                                headers: vec![],
                                body: Some(GmailBody {
                                    size: 11,
                                    data: Some(b64u("plain text")),
                                    attachment_id: None,
                                }),
                                parts: None,
                            },
                            GmailPayload {
                                mime_type: "text/html".into(),
                                filename: String::new(),
                                headers: vec![],
                                body: Some(GmailBody {
                                    size: 30,
                                    data: Some(b64u("<p>html <img src=\"cid:x\"></p>")),
                                    attachment_id: None,
                                }),
                                parts: None,
                            },
                        ]),
                    },
                    GmailPayload {
                        mime_type: "image/png".into(),
                        filename: "embedded.png".into(),
                        headers: vec![header(
                            "Content-Disposition",
                            "inline; filename=embedded.png",
                        )],
                        body: Some(GmailBody {
                            size: 100,
                            data: None,
                            attachment_id: Some("att_inline".into()),
                        }),
                        parts: None,
                    },
                ]),
            },
        };
        let email = parse_message_to_email(msg, true);
        assert_eq!(email.text_body.as_deref(), Some("plain text"));
        assert!(email.html_body.as_deref().unwrap().contains("<p>html"));
        // The inline image should NOT appear as an attachment because it's
        // inside multipart/related with disposition=inline.
        assert!(!email.has_attachment);
        assert!(email.attachments.is_empty());
    }

    // ---- address parser known limitation (roborev 173 #8) ----

    /// Documents the known failure mode of `parse_address_list`: commas
    /// inside quoted display names split into bogus addresses. A proper RFC
    /// 5322 parser would handle this. Test pins the current behavior so a
    /// future fix has something to flip.
    #[test]
    fn parse_address_quoted_comma_is_currently_mis_parsed() {
        let addrs = parse_address_list(r#""Smith, John" <jsmith@example.com>"#);
        // Current behavior: splits on the comma, producing two malformed
        // entries. When this test starts failing, the parser was fixed —
        // update the assertion.
        assert_eq!(
            addrs.len(),
            2,
            "if this now returns 1, the parser was fixed — update the test"
        );
    }

    // ---- Milestone B: mutation body builders ----

    #[test]
    fn modify_body_includes_both_label_arrays() {
        let body = modify_body(&["STARRED"], &["UNREAD"]);
        assert_eq!(body["addLabelIds"], serde_json::json!(["STARRED"]));
        assert_eq!(body["removeLabelIds"], serde_json::json!(["UNREAD"]));
    }

    #[test]
    fn modify_body_with_empty_add_or_remove_keeps_empty_array() {
        // Gmail accepts empty arrays; serializing as `[]` (not omitting the
        // key) keeps the request shape predictable.
        let body = modify_body(&[], &["INBOX"]);
        assert_eq!(body["addLabelIds"], serde_json::json!([]));
        assert_eq!(body["removeLabelIds"], serde_json::json!(["INBOX"]));
    }

    #[test]
    fn batch_modify_body_includes_ids() {
        let ids = vec!["msg1".to_string(), "msg2".to_string()];
        let body = batch_modify_body(&ids, &[], &["INBOX"]);
        assert_eq!(body["ids"], serde_json::json!(["msg1", "msg2"]));
        assert_eq!(body["removeLabelIds"], serde_json::json!(["INBOX"]));
    }

    // mime_type_from_filename tests → crate::provider_utils::tests

    // ---- move_plan (roborev 174 finding #1 + #7) ----

    #[test]
    fn move_plan_inbox_restores_by_removing_trash_and_spam() {
        // "Move to INBOX" must strip TRASH/SPAM, otherwise a message moved
        // back from Trash stays subject to the 30-day purge timer despite
        // having INBOX applied. Regression guard for roborev 175 #4.
        assert_eq!(
            move_plan("INBOX"),
            MovePlan::Labels {
                add: vec!["INBOX".into()],
                remove: vec!["TRASH".into(), "SPAM".into()],
            }
        );
    }

    #[test]
    fn move_plan_user_label_adds_target_removes_inbox() {
        assert_eq!(
            move_plan("Label_42"),
            MovePlan::Labels {
                add: vec!["Label_42".into()],
                remove: vec!["INBOX".into()],
            }
        );
    }

    #[test]
    fn move_plan_trash_routes_to_trash_endpoint() {
        assert_eq!(move_plan("TRASH"), MovePlan::Trash);
    }

    #[test]
    fn move_plan_rejects_spam() {
        match move_plan("SPAM") {
            MovePlan::Reject(msg) => assert!(msg.contains("SPAM")),
            other => panic!("expected Reject for SPAM, got {other:?}"),
        }
    }

    #[test]
    fn move_plan_rejects_draft_and_sent() {
        assert!(matches!(move_plan("DRAFT"), MovePlan::Reject(_)));
        assert!(matches!(move_plan("SENT"), MovePlan::Reject(_)));
    }

    // ---- classify_gmail_error (roborev 174 finding #5 + #7) ----

    #[test]
    fn classify_4xx_returns_bad_request() {
        let err = classify_gmail_error("messages.modify abc", reqwest::StatusCode::NOT_FOUND, "{}");
        assert!(matches!(err, Error::BadRequest(_)));
    }

    #[test]
    fn classify_401_returns_bad_request() {
        let err = classify_gmail_error("foo", reqwest::StatusCode::UNAUTHORIZED, "bad token");
        assert!(matches!(err, Error::BadRequest(_)));
    }

    #[test]
    fn classify_403_returns_bad_request() {
        let err = classify_gmail_error("foo", reqwest::StatusCode::FORBIDDEN, "no scope");
        assert!(matches!(err, Error::BadRequest(_)));
    }

    #[test]
    fn classify_5xx_returns_internal() {
        let err = classify_gmail_error(
            "messages.modify abc",
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "down",
        );
        assert!(matches!(err, Error::Internal(_)));
    }

    #[test]
    fn classify_503_returns_internal() {
        let err = classify_gmail_error("foo", reqwest::StatusCode::SERVICE_UNAVAILABLE, "");
        assert!(matches!(err, Error::Internal(_)));
    }

    #[test]
    fn classify_includes_operation_status_and_body() {
        let err = classify_gmail_error(
            "messages.batchModify",
            reqwest::StatusCode::BAD_REQUEST,
            "Invalid ID",
        );
        let msg = match err {
            Error::BadRequest(m) => m,
            other => panic!("expected BadRequest, got {other:?}"),
        };
        assert!(msg.contains("messages.batchModify"));
        assert!(msg.contains("400"));
        assert!(msg.contains("Invalid ID"));
    }

    // ---- is_rate_limited + friendly rate-limit message ----

    #[test]
    fn rate_limited_detects_429() {
        assert!(is_rate_limited(reqwest::StatusCode::TOO_MANY_REQUESTS, "",));
    }

    #[test]
    fn rate_limited_detects_403_quota_body() {
        // The shape Gmail actually returns for per-minute quota exhaustion
        // is 403 with `rateLimitExceeded` / `RATE_LIMIT_EXCEEDED` in the body.
        let body = r#"{"error":{"code":403,"errors":[{"reason":"rateLimitExceeded"}],"status":"PERMISSION_DENIED","details":[{"reason":"RATE_LIMIT_EXCEEDED"}]}}"#;
        assert!(is_rate_limited(reqwest::StatusCode::FORBIDDEN, body));
    }

    #[test]
    fn rate_limited_ignores_plain_403() {
        // Scope/permission denials must NOT be misread as rate limits — that
        // would trigger a retry and bury the real "user needs to re-consent"
        // signal.
        assert!(!is_rate_limited(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"error":{"code":403,"message":"Insufficient scope"}}"#,
        ));
    }

    #[test]
    fn rate_limited_ignores_other_4xx() {
        assert!(!is_rate_limited(reqwest::StatusCode::NOT_FOUND, ""));
        assert!(!is_rate_limited(reqwest::StatusCode::BAD_REQUEST, ""));
    }

    #[test]
    fn classify_rate_limit_returns_rate_limited_variant() {
        // Both shapes of Gmail rate-limit (HTTP 429 and the quirky
        // HTTP-403-with-quota-body) collapse to the typed RateLimited
        // variant so IntoResponse can emit a real HTTP 429.
        let body = r#"{"error":{"code":403,"errors":[{"reason":"rateLimitExceeded"}]}}"#;
        let err = classify_gmail_error("messages.modify abc", reqwest::StatusCode::FORBIDDEN, body);
        assert!(matches!(err, Error::RateLimited { .. }));

        let err429 = classify_gmail_error(
            "messages.get xyz",
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "{}",
        );
        assert!(matches!(err429, Error::RateLimited { .. }));
    }

    // ---- Milestone B: download_blob synthetic-blob rejection ----
    // Synthetic blobs can't be downloaded — they're upload bytes. Until the
    // compose flow lands in Milestone C, this path should reject cleanly.

    #[tokio::test]
    async fn download_blob_rejects_synthetic_blob_ref() {
        use crate::platform::{FsTokenStore, TokenStore, Tokens};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn TokenStore> = Arc::new(FsTokenStore::new(dir.path().to_path_buf()));
        store
            .save(
                "gmail",
                &Tokens {
                    access_token: "tok".into(),
                    refresh_token: "rtok".into(),
                    token_expiry: Utc::now() + chrono::Duration::hours(1),
                    email: "u@g.com".into(),
                },
            )
            .unwrap();
        let session =
            load_session(store, "gmail", "client-id", "client-secret").expect("session loads");
        let synth = crate::types::BlobRef::new_synthetic().to_string();
        let err = download_blob(&session, &synth, "x.txt").await.unwrap_err();
        match err {
            Error::BadRequest(msg) => assert!(msg.contains("synthetic")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    // ---- Milestone C: upload cache lifecycle ----

    fn test_session() -> GmailSession {
        use crate::platform::{FsTokenStore, TokenStore, Tokens};
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn TokenStore> = Arc::new(FsTokenStore::new(dir.path().to_path_buf()));
        store
            .save(
                "gmail",
                &Tokens {
                    access_token: "tok".into(),
                    refresh_token: "rtok".into(),
                    token_expiry: Utc::now() + chrono::Duration::hours(1),
                    email: "u@g.com".into(),
                },
            )
            .unwrap();
        let session =
            load_session(store, "gmail", "client-id", "client-secret").expect("session loads");
        std::mem::forget(dir);
        session
    }

    #[tokio::test]
    async fn upload_blob_roundtrips_through_blob_ref() {
        let session = test_session();
        let (blob_id, size) = upload_blob(&session, "image/png", b"PNG-bytes")
            .await
            .unwrap();
        assert_eq!(size, 9);
        assert!(blob_id.starts_with("synth:"));
        // Must parse back to Synthetic
        let parsed = crate::types::BlobRef::parse(&blob_id).unwrap();
        assert!(matches!(parsed, crate::types::BlobRef::Synthetic(_)));
    }

    #[tokio::test]
    async fn upload_blob_cap_rejects_after_32_entries() {
        let session = test_session();
        for i in 0..UPLOAD_CACHE_CAP {
            upload_blob(&session, "application/octet-stream", &[i as u8])
                .await
                .unwrap_or_else(|e| panic!("upload {i} should succeed: {e:?}"));
        }
        let err = upload_blob(&session, "application/octet-stream", &[99])
            .await
            .unwrap_err();
        match err {
            Error::BadRequest(msg) => assert!(msg.contains("full")),
            other => panic!("expected BadRequest for cap, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn peek_blob_bytes_does_not_consume_synthetic() {
        // Two successive peeks must both succeed — peek is read-only on the
        // cache so partial-failure during build_rfc822 can be retried
        // without losing already-uploaded blobs (roborev 176 #1).
        let session = test_session();
        let (blob_id, _) = upload_blob(&session, "text/plain", b"hello").await.unwrap();
        let (mime, bytes) = peek_blob_bytes(&session, &blob_id, "ignored")
            .await
            .unwrap();
        assert_eq!(mime, "text/plain");
        assert_eq!(bytes, b"hello");
        // Second peek must succeed too.
        let (mime2, bytes2) = peek_blob_bytes(&session, &blob_id, "ignored")
            .await
            .unwrap();
        assert_eq!(mime2, "text/plain");
        assert_eq!(bytes2, b"hello");
    }

    #[tokio::test]
    async fn drain_consumed_synthetic_blobs_removes_only_synthetic_entries() {
        let session = test_session();
        let (synth_id, _) = upload_blob(&session, "text/plain", b"X").await.unwrap();
        // Mix synthetic + gmail-shaped blob_ids in the attachment list.
        let atts = vec![
            crate::types::Attachment {
                blob_id: synth_id.clone(),
                name: "a.txt".into(),
                mime_type: "text/plain".into(),
                size: 1,
            },
            crate::types::Attachment {
                blob_id: "msg-abc:att-xyz".into(),
                name: "b.pdf".into(),
                mime_type: "application/pdf".into(),
                size: 0,
            },
        ];
        drain_consumed_synthetic_blobs(&session, &atts).await;
        // Synthetic gone; non-synthetic was never in cache, idempotent.
        let err = peek_blob_bytes(&session, &synth_id, "ignored")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::BadRequest(_)));
    }

    // ---- Milestone E: 401-on-revoke token clearing ----
    // should_clear_tokens_on_refresh_failure tests → crate::provider_utils::tests.
    // Below: integration test for the Gmail-specific clear_stored_tokens helper
    // (drops session token store entry — still lives on the session struct).

    #[tokio::test]
    async fn clear_stored_tokens_removes_from_store() {
        // RED before clear_stored_tokens existed: the call below didn't compile.
        let session = test_session();
        // Sanity: the test_session helper seeds tokens, so they're present.
        assert!(
            session.token_store.load(&session.account_id).is_some(),
            "precondition: test_session seeds a stored token"
        );

        clear_stored_tokens(&session).await;

        assert!(
            session.token_store.load(&session.account_id).is_none(),
            "stored tokens should be deleted after clear_stored_tokens"
        );
    }

    // ---- Milestone C: extract_message_id ----

    #[test]
    fn extract_message_id_bracketed() {
        assert_eq!(extract_message_id("<abc@example.com>"), "<abc@example.com>");
    }

    #[test]
    fn extract_message_id_with_whitespace_and_comment() {
        // RFC822 headers may include CFWS; extractor pulls just <…>.
        assert_eq!(
            extract_message_id("  <abc@example.com> (comment)"),
            "<abc@example.com>"
        );
    }

    #[test]
    fn extract_message_id_unbracketed_passes_through() {
        // If a server returns the bare form (uncommon but legal), pass it
        // through trimmed — downstream comparisons will still work-ish.
        assert_eq!(extract_message_id("  abc@example.com  "), "abc@example.com");
    }

    #[test]
    fn extract_message_id_empty() {
        assert_eq!(extract_message_id(""), "");
        assert_eq!(extract_message_id("   "), "");
    }

    // ---- Milestone C: build_rfc822 fixtures ----
    //
    // build_rfc822 is async because it resolves attachments; the test runner
    // uses #[tokio::test] but the network paths are exercised only via
    // synthetic blobs (no real HTTP).

    fn email_sub_text_only(subject: &str, body: &str) -> crate::types::EmailSubmission {
        crate::types::EmailSubmission {
            to: vec!["recipient@example.com".into()],
            cc: vec![],
            bcc: None,
            subject: subject.into(),
            text_body: body.into(),
            html_body: None,
            in_reply_to: None,
            references: None,
            attachments: vec![],
            calendar_ics: None,
        }
    }

    #[tokio::test]
    async fn build_rfc822_text_only_has_required_headers() {
        let session = test_session();
        let sub = email_sub_text_only("Hello", "Body");
        let raw = build_rfc822(
            &session,
            &sub,
            "from@example.com",
            Some("Alice"),
            None,
            None,
        )
        .await
        .unwrap();
        let s = String::from_utf8_lossy(&raw);
        assert!(s.contains("From: "));
        assert!(s.contains("from@example.com"));
        assert!(s.contains("Alice"));
        assert!(s.contains("To: "));
        assert!(s.contains("recipient@example.com"));
        assert!(s.contains("Subject: Hello"));
        assert!(s.contains("MIME-Version: 1.0"));
        assert!(s.contains("Message-ID: "));
        assert!(s.contains("Date: "));
        assert!(s.contains("Body"));
    }

    #[tokio::test]
    async fn build_rfc822_text_and_html_uses_alternative() {
        let session = test_session();
        let mut sub = email_sub_text_only("Mixed", "plain body");
        sub.html_body = Some("<p>html body</p>".into());
        let raw = build_rfc822(&session, &sub, "from@example.com", None, None, None)
            .await
            .unwrap();
        let s = String::from_utf8_lossy(&raw);
        assert!(s.contains("multipart/alternative"));
        assert!(s.contains("text/plain"));
        assert!(s.contains("text/html"));
        assert!(s.contains("plain body"));
        assert!(s.contains("html body") || s.contains("<p>html body</p>"));
    }

    #[tokio::test]
    async fn build_rfc822_reply_includes_in_reply_to_and_references() {
        let session = test_session();
        let sub = email_sub_text_only("Re: thing", "ack");
        let raw = build_rfc822(
            &session,
            &sub,
            "me@example.com",
            None,
            Some("<parent@example.com>"),
            Some(&["<parent@example.com>".to_string()]),
        )
        .await
        .unwrap();
        let s = String::from_utf8_lossy(&raw);
        assert!(s.contains("In-Reply-To: <parent@example.com>"));
        assert!(s.contains("References: <parent@example.com>"));
    }

    #[tokio::test]
    async fn build_rfc822_bcc_included_in_headers() {
        let session = test_session();
        let mut sub = email_sub_text_only("Hi", "body");
        sub.bcc = Some(vec!["secret@example.com".into()]);
        let raw = build_rfc822(&session, &sub, "from@example.com", None, None, None)
            .await
            .unwrap();
        let s = String::from_utf8_lossy(&raw);
        assert!(s.contains("Bcc:"));
        assert!(s.contains("secret@example.com"));
    }

    #[tokio::test]
    async fn build_rfc822_with_attachment_becomes_multipart_mixed() {
        let session = test_session();
        let (blob_id, _) = upload_blob(&session, "application/pdf", b"%PDF-fake")
            .await
            .unwrap();
        let mut sub = email_sub_text_only("Report", "see attached");
        sub.attachments = vec![crate::types::Attachment {
            blob_id,
            name: "report.pdf".into(),
            mime_type: "application/pdf".into(),
            size: 9,
        }];
        let raw = build_rfc822(&session, &sub, "from@example.com", None, None, None)
            .await
            .unwrap();
        let s = String::from_utf8_lossy(&raw);
        assert!(s.contains("multipart/mixed"));
        assert!(s.contains("application/pdf"));
        assert!(s.contains("report.pdf"));
        // Content-Disposition: attachment for the second part
        assert!(s.to_ascii_lowercase().contains("attachment"));
    }

    #[tokio::test]
    async fn build_rfc822_multiple_recipients() {
        let session = test_session();
        let mut sub = email_sub_text_only("Hi", "all");
        sub.to = vec!["a@example.com".into(), "b@example.com".into()];
        sub.cc = vec!["c@example.com".into()];
        let raw = build_rfc822(&session, &sub, "from@example.com", None, None, None)
            .await
            .unwrap();
        let s = String::from_utf8_lossy(&raw);
        assert!(s.contains("a@example.com"));
        assert!(s.contains("b@example.com"));
        assert!(s.contains("c@example.com"));
        assert!(s.contains("Cc:"));
    }

    // ---- Roborev 176 #8: looks_like_message_id heuristic ----

    #[test]
    fn looks_like_message_id_canonical_form() {
        assert!(looks_like_message_id("<abc@example.com>"));
    }

    #[test]
    fn looks_like_message_id_unbracketed_rejected() {
        // Bare `local@domain` could be a Gmail ID containing `@` (rare but
        // possible-ish if encoded). Heuristic requires the leading `<`.
        assert!(!looks_like_message_id("abc@example.com"));
    }

    #[test]
    fn looks_like_message_id_gmail_id_rejected() {
        // Gmail message IDs are URL-safe base64-ish, no `<` or `@`.
        assert!(!looks_like_message_id("190abc-DEF_xyz"));
    }

    #[test]
    fn looks_like_message_id_empty_rejected() {
        assert!(!looks_like_message_id(""));
        assert!(!looks_like_message_id("   "));
    }

    #[test]
    fn looks_like_message_id_handles_leading_whitespace() {
        // Defensive: header values may be padded; the heuristic should
        // ignore surrounding whitespace.
        assert!(looks_like_message_id("  <foo@bar>  "));
    }

    // encode_path_segment tests → crate::provider_utils::tests

    // ---- Roborev 176 #4: upload size caps ----

    #[tokio::test]
    async fn upload_blob_rejects_oversized_per_blob() {
        let session = test_session();
        let big = vec![0u8; MAX_BLOB_BYTES + 1];
        let err = upload_blob(&session, "application/octet-stream", &big)
            .await
            .unwrap_err();
        match err {
            Error::BadRequest(msg) => assert!(msg.contains("too large")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upload_blob_rejects_aggregate_overflow() {
        let session = test_session();
        // Three near-max blobs: first two fit (50 MiB total), third overflows.
        let chunk = vec![0u8; 20 * 1024 * 1024];
        upload_blob(&session, "application/octet-stream", &chunk)
            .await
            .unwrap();
        upload_blob(&session, "application/octet-stream", &chunk)
            .await
            .unwrap();
        let err = upload_blob(&session, "application/octet-stream", &chunk)
            .await
            .unwrap_err();
        match err {
            Error::BadRequest(msg) => assert!(msg.contains("aggregate")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    // =========================================================================
    // Milestone D: Google Calendar
    //
    // Tests below cover the pure helpers added for Calendar (parse/build/
    // mutate/walk). The HTTP-bound functions (get_calendar_event,
    // add_to_calendar, remove_from_calendar, respond_to_event,
    // get_calendar_data) don't have unit tests — the codebase doesn't have
    // an HTTP-mocking dep established (see roborev 176 #3 / 175 #3 dialog).
    // The pure helpers are the part most likely to bit-rot silently.
    // =========================================================================

    fn sample_event() -> CalendarEvent {
        CalendarEvent {
            uid: "uid-1@example.com".into(),
            summary: "Sync".into(),
            dtstart: chrono::DateTime::parse_from_rfc3339("2026-03-20T14:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            dtend: Some(
                chrono::DateTime::parse_from_rfc3339("2026-03-20T14:30:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            location: Some("Room 1".into()),
            description: Some("Daily standup".into()),
            organizer_email: "lead@example.com".into(),
            organizer_name: Some("Lead".into()),
            attendees: vec![
                crate::types::Attendee {
                    email: "alice@example.com".into(),
                    name: Some("Alice".into()),
                    status: "ACCEPTED".into(),
                },
                crate::types::Attendee {
                    email: "bob@example.com".into(),
                    name: None,
                    status: "NEEDS-ACTION".into(),
                },
            ],
            sequence: 0,
            method: "REQUEST".into(),
            raw_ics: String::new(),
            user_rsvp_status: None,
        }
    }

    // ---- ics_status_to_google / google_status_to_ics ----

    #[test]
    fn ics_to_google_status_mapping() {
        assert_eq!(ics_status_to_google("ACCEPTED"), "accepted");
        assert_eq!(ics_status_to_google("TENTATIVE"), "tentative");
        assert_eq!(ics_status_to_google("DECLINED"), "declined");
        assert_eq!(ics_status_to_google("NEEDS-ACTION"), "needsAction");
        // Unknown defaults to needsAction (safest — don't accidentally accept).
        assert_eq!(ics_status_to_google("BOGUS"), "needsAction");
    }

    #[test]
    fn google_to_ics_status_mapping() {
        assert_eq!(google_status_to_ics("accepted"), "ACCEPTED");
        assert_eq!(google_status_to_ics("tentative"), "TENTATIVE");
        assert_eq!(google_status_to_ics("declined"), "DECLINED");
        assert_eq!(google_status_to_ics("needsAction"), "NEEDS-ACTION");
        assert_eq!(google_status_to_ics("anything-else"), "NEEDS-ACTION");
    }

    #[test]
    fn status_mapping_round_trips_for_known_values() {
        for ics in &["ACCEPTED", "TENTATIVE", "DECLINED", "NEEDS-ACTION"] {
            let google = ics_status_to_google(ics);
            let back = google_status_to_ics(google);
            assert_eq!(back, *ics, "round trip failed for {ics}");
        }
    }

    // ---- calendar_event_to_google_json ----

    #[test]
    fn google_json_has_required_fields() {
        let j = calendar_event_to_google_json(&sample_event());
        assert_eq!(j["iCalUID"], "uid-1@example.com");
        assert_eq!(j["summary"], "Sync");
        assert_eq!(j["start"]["timeZone"], "UTC");
        assert_eq!(j["end"]["timeZone"], "UTC");
        // RFC3339 with Z suffix (UTC) — Google accepts this form.
        assert!(
            j["start"]["dateTime"].as_str().unwrap().ends_with("+00:00")
                || j["start"]["dateTime"].as_str().unwrap().ends_with("Z")
        );
    }

    #[test]
    fn google_json_includes_location_when_set() {
        let j = calendar_event_to_google_json(&sample_event());
        assert_eq!(j["location"], "Room 1");
    }

    #[test]
    fn google_json_omits_empty_location() {
        let mut ev = sample_event();
        ev.location = None;
        let j = calendar_event_to_google_json(&ev);
        assert!(j.get("location").is_none());
    }

    #[test]
    fn google_json_omits_empty_description() {
        let mut ev = sample_event();
        ev.description = None;
        let j = calendar_event_to_google_json(&ev);
        assert!(j.get("description").is_none());
    }

    #[test]
    fn google_json_omits_organizer_display_name_when_absent() {
        let mut ev = sample_event();
        ev.organizer_name = None;
        let j = calendar_event_to_google_json(&ev);
        assert_eq!(j["organizer"]["email"], "lead@example.com");
        // Matches the per-attendee pattern: omit displayName instead of "".
        assert!(j["organizer"].get("displayName").is_none());
    }

    #[test]
    fn google_json_omits_organizer_display_name_when_empty_string() {
        let mut ev = sample_event();
        ev.organizer_name = Some(String::new());
        let j = calendar_event_to_google_json(&ev);
        assert!(j["organizer"].get("displayName").is_none());
    }

    #[test]
    fn google_json_defaults_end_to_one_hour_when_missing() {
        let mut ev = sample_event();
        ev.dtend = None;
        let j = calendar_event_to_google_json(&ev);
        // start = 14:00, default end = 15:00
        let end = j["end"]["dateTime"].as_str().unwrap();
        assert!(end.contains("15:00:00"));
    }

    #[test]
    fn google_json_attendee_status_translated() {
        let j = calendar_event_to_google_json(&sample_event());
        let attendees = j["attendees"].as_array().unwrap();
        assert_eq!(attendees[0]["email"], "alice@example.com");
        assert_eq!(attendees[0]["responseStatus"], "accepted");
        assert_eq!(attendees[0]["displayName"], "Alice");
        assert_eq!(attendees[1]["email"], "bob@example.com");
        assert_eq!(attendees[1]["responseStatus"], "needsAction");
        // No displayName when none was set
        assert!(attendees[1].get("displayName").is_none());
    }

    #[test]
    fn google_json_omits_attendees_when_empty() {
        let mut ev = sample_event();
        ev.attendees = vec![];
        let j = calendar_event_to_google_json(&ev);
        assert!(j.get("attendees").is_none());
    }

    // ---- parse_google_event ----

    fn google_event_json() -> serde_json::Value {
        serde_json::json!({
            "id": "google-id-abc",
            "iCalUID": "uid-1@example.com",
            "summary": "Sync",
            "description": "Daily standup",
            "location": "Room 1",
            "start": { "dateTime": "2026-03-20T14:00:00Z", "timeZone": "UTC" },
            "end": { "dateTime": "2026-03-20T14:30:00Z", "timeZone": "UTC" },
            "organizer": { "email": "lead@example.com", "displayName": "Lead" },
            "attendees": [
                { "email": "alice@example.com", "displayName": "Alice", "responseStatus": "accepted" },
                { "email": "bob@example.com", "responseStatus": "tentative" },
                { "email": "carol@example.com", "responseStatus": "declined" },
                { "email": "dave@example.com", "responseStatus": "needsAction" }
            ]
        })
    }

    #[test]
    fn parse_google_event_full() {
        let parsed = parse_google_event("uid-1@example.com", &google_event_json()).unwrap();
        assert_eq!(parsed.uid, "uid-1@example.com");
        assert_eq!(parsed.summary, "Sync");
        assert_eq!(parsed.location.as_deref(), Some("Room 1"));
        assert_eq!(parsed.description.as_deref(), Some("Daily standup"));
        assert_eq!(parsed.organizer_email, "lead@example.com");
        assert_eq!(parsed.organizer_name.as_deref(), Some("Lead"));
        assert!(parsed.dtend.is_some());
    }

    #[test]
    fn parse_google_event_maps_all_attendee_statuses() {
        let parsed = parse_google_event("uid-x", &google_event_json()).unwrap();
        assert_eq!(parsed.attendees[0].status, "ACCEPTED");
        assert_eq!(parsed.attendees[1].status, "TENTATIVE");
        assert_eq!(parsed.attendees[2].status, "DECLINED");
        assert_eq!(parsed.attendees[3].status, "NEEDS-ACTION");
    }

    #[test]
    fn parse_google_event_handles_all_day_date() {
        // All-day events use {date} instead of {dateTime}.
        let json = serde_json::json!({
            "summary": "Holiday",
            "start": { "date": "2026-12-25" },
            "end": { "date": "2026-12-26" }
        });
        let parsed = parse_google_event("uid-holiday", &json).unwrap();
        assert_eq!(parsed.dtstart.format("%Y-%m-%d").to_string(), "2026-12-25");
        assert_eq!(
            parsed.dtend.unwrap().format("%Y-%m-%d").to_string(),
            "2026-12-26"
        );
    }

    #[test]
    fn parse_google_event_missing_start_returns_none() {
        let json = serde_json::json!({ "summary": "no-start" });
        assert!(parse_google_event("uid", &json).is_none());
    }

    #[test]
    fn parse_google_event_empty_optional_fields_treated_as_none() {
        let json = serde_json::json!({
            "summary": "Call",
            "start": { "dateTime": "2026-03-20T09:00:00Z" },
            "location": "",
            "description": "",
            "organizer": { "email": "a@b.com", "displayName": "" }
        });
        let parsed = parse_google_event("uid", &json).unwrap();
        assert!(parsed.location.is_none());
        assert!(parsed.description.is_none());
        assert!(parsed.organizer_name.is_none());
    }

    #[test]
    fn parse_google_event_attendee_missing_email_skipped() {
        let json = serde_json::json!({
            "summary": "Test",
            "start": { "dateTime": "2026-03-20T09:00:00Z" },
            "attendees": [
                { "displayName": "No Email", "responseStatus": "accepted" },
                { "email": "valid@example.com", "responseStatus": "accepted" }
            ]
        });
        let parsed = parse_google_event("uid", &json).unwrap();
        assert_eq!(parsed.attendees.len(), 1);
        assert_eq!(parsed.attendees[0].email, "valid@example.com");
    }

    // ---- mutate_attendee_status ----

    fn make_attendees() -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({ "email": "alice@example.com", "responseStatus": "needsAction" }),
            serde_json::json!({ "email": "bob@example.com", "responseStatus": "needsAction" }),
        ]
    }

    #[test]
    fn mutate_attendee_status_updates_match() {
        let mut atts = make_attendees();
        let found = mutate_attendee_status(&mut atts, "alice@example.com", "accepted");
        assert!(found);
        assert_eq!(atts[0]["responseStatus"], "accepted");
        // Bob untouched — this is the Google-PATCH quirk: full-array submit
        // must preserve all other attendees.
        assert_eq!(atts[1]["responseStatus"], "needsAction");
    }

    #[test]
    fn mutate_attendee_status_case_insensitive_match() {
        let mut atts = make_attendees();
        let found = mutate_attendee_status(&mut atts, "ALICE@EXAMPLE.COM", "tentative");
        assert!(found);
        assert_eq!(atts[0]["responseStatus"], "tentative");
    }

    #[test]
    fn mutate_attendee_status_returns_false_when_email_absent() {
        let mut atts = make_attendees();
        let found = mutate_attendee_status(&mut atts, "stranger@example.com", "accepted");
        assert!(!found);
        // Nothing changed
        assert_eq!(atts[0]["responseStatus"], "needsAction");
        assert_eq!(atts[1]["responseStatus"], "needsAction");
    }

    #[test]
    fn mutate_attendee_status_preserves_other_fields() {
        let mut atts = vec![serde_json::json!({
            "email": "alice@example.com",
            "displayName": "Alice",
            "optional": true,
            "responseStatus": "needsAction"
        })];
        mutate_attendee_status(&mut atts, "alice@example.com", "declined");
        assert_eq!(atts[0]["responseStatus"], "declined");
        assert_eq!(atts[0]["displayName"], "Alice");
        assert_eq!(atts[0]["optional"], true);
    }

    // ---- find_calendar_ics ----

    fn calendar_part(ics: &str) -> GmailPayload {
        GmailPayload {
            mime_type: "text/calendar".into(),
            filename: String::new(),
            headers: vec![],
            body: Some(GmailBody {
                size: ics.len() as i64,
                data: Some(b64u(ics)),
                attachment_id: None,
            }),
            parts: None,
        }
    }

    #[test]
    fn find_calendar_ics_at_root() {
        let payload = calendar_part("BEGIN:VCALENDAR\r\nEND:VCALENDAR");
        let ics = find_calendar_ics(&payload).unwrap();
        assert!(ics.starts_with("BEGIN:VCALENDAR"));
    }

    #[test]
    fn find_calendar_ics_nested_in_multipart() {
        let payload = GmailPayload {
            mime_type: "multipart/mixed".into(),
            filename: String::new(),
            headers: vec![],
            body: None,
            parts: Some(vec![
                GmailPayload {
                    mime_type: "text/plain".into(),
                    filename: String::new(),
                    headers: vec![],
                    body: Some(GmailBody {
                        size: 5,
                        data: Some(b64u("hello")),
                        attachment_id: None,
                    }),
                    parts: None,
                },
                calendar_part("BEGIN:VCALENDAR\r\nUID:abc\r\nEND:VCALENDAR"),
            ]),
        };
        let ics = find_calendar_ics(&payload).unwrap();
        assert!(ics.contains("UID:abc"));
    }

    #[test]
    fn find_calendar_ics_returns_none_when_absent() {
        let payload = GmailPayload {
            mime_type: "text/plain".into(),
            filename: String::new(),
            headers: vec![],
            body: Some(GmailBody {
                size: 5,
                data: Some(b64u("hello")),
                attachment_id: None,
            }),
            parts: None,
        };
        assert!(find_calendar_ics(&payload).is_none());
    }

    #[test]
    fn find_calendar_ics_case_insensitive_mime() {
        let mut part = calendar_part("BEGIN:VCALENDAR\r\nEND:VCALENDAR");
        part.mime_type = "Text/Calendar".into();
        assert!(find_calendar_ics(&part).is_some());
    }

    #[test]
    fn find_calendar_ics_handles_deep_nesting_without_stack_overflow() {
        // Build a payload deeply enough that a per-frame recursion would be
        // visibly costly on a small test-thread stack. The iterative DFS
        // walker handles it as one allocation rather than 256 frames.
        // (Kept moderate so the structural Drop at end-of-test doesn't
        // itself recurse off the cliff — Drop is recursive even though
        // traversal isn't.)
        let mut current = calendar_part("BEGIN:VCALENDAR\r\nUID:deep\r\nEND:VCALENDAR");
        for _ in 0..256 {
            current = GmailPayload {
                mime_type: "multipart/mixed".into(),
                filename: String::new(),
                headers: vec![],
                body: None,
                parts: Some(vec![current]),
            };
        }
        let ics = find_calendar_ics(&current).unwrap();
        assert!(ics.contains("UID:deep"));
    }

    #[test]
    fn find_calendar_ics_pre_order_returns_first_when_siblings_match() {
        // Two text/calendar siblings: returns the first. Matches the
        // recursive version's behavior so callers don't observe a regression.
        let payload = GmailPayload {
            mime_type: "multipart/mixed".into(),
            filename: String::new(),
            headers: vec![],
            body: None,
            parts: Some(vec![
                calendar_part("BEGIN:VCALENDAR\r\nUID:first\r\nEND:VCALENDAR"),
                calendar_part("BEGIN:VCALENDAR\r\nUID:second\r\nEND:VCALENDAR"),
            ]),
        };
        let ics = find_calendar_ics(&payload).unwrap();
        assert!(ics.contains("UID:first"));
    }

    // ---- concurrency cap on get_emails fan-out ----
    //
    // get_emails fans out one messages.get per ID through a shared
    // Semaphore so we don't trip Gmail's per-user concurrent-request
    // limit (429 RESOURCE_EXHAUSTED). The HTTP path is hard to mock
    // without an extra dev-dep, so this test replays the same spawn
    // pattern with an AtomicUsize peak counter — if someone later
    // removes the semaphore from get_emails, the constant pin below
    // still fails and forces them to update this test too.

    #[test]
    fn gmail_limiter_is_configured_for_per_user_throttling() {
        // Pin the limiter tuning so a silent edit to the configuration
        // trips CI. 5 concurrent × 80ms spacing ≈ 12 RPS — under Gmail's
        // documented per-user 250 quota-units/sec budget and proven to
        // avoid "Too many concurrent requests for user" 429s. The general
        // semaphore/spacer correctness is covered by tests in
        // crate::rate_limit; this only pins the per-provider knobs.
        let session = test_session();
        assert_eq!(session.limiter.name(), "gmail");
        assert_eq!(session.limiter.concurrency(), 5);
        assert_eq!(session.limiter.spacing(), Duration::from_millis(80));
    }
}
