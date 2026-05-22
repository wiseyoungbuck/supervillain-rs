// Gmail provider.
//
// Mirrors src/outlook.rs in shape: session struct, OAuth2 PKCE flow via the
// shared platform abstraction, then a flat set of async functions that
// `src/provider.rs` dispatches into via its enum match arms.
//
// Phase 3 Milestone A scope: OAuth + read-only inbox. Mutations land in
// Milestone B; send/compose in C; calendar in D.
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
use crate::types::{Email, EmailAddress, Identity, Mailbox, ParsedQuery};

// =============================================================================
// Endpoints + constants
// =============================================================================

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GMAIL_BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
const REDIRECT_URI: &str = "http://localhost:8401/callback";
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
        if text.contains("invalid_grant") {
            return Err(Error::Auth(format!(
                "Gmail refresh token expired or revoked. If your OAuth app is in 'Testing' \
                 state in Google Cloud Console, you must be listed as a Test User; otherwise \
                 tokens expire after 7 days. See README §Gmail setup. ({status}): {text}"
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
            .client
            .get(format!("{GMAIL_BASE}/labels"))
            .bearer_auth(&token)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(classify_gmail_error("labels.list", status, &text));
        }
        resp.json().await?
    };

    // Concurrent fan-out for per-label details (counts come from labels.get only).
    let mut join_set = tokio::task::JoinSet::new();
    for stub in stubs.labels {
        let label_type = stub.label_type.as_deref().unwrap_or("user");
        if !should_include_label(&stub.name, label_type) {
            continue;
        }
        let client = session.client.clone();
        let token = token.clone();
        let id = stub.id.clone();
        join_set.spawn(async move {
            let resp = client
                .get(format!("{GMAIL_BASE}/labels/{id}"))
                .bearer_auth(&token)
                .send()
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

/// Query email IDs. Translates cursor pagination to the route handler's
/// offset model. The cache stores the `PageStart` for fetching page N for
/// each (mailbox+query) key; first request seeds it, subsequent forward
/// requests follow it, jump-backs re-walk from 0 (bounded by MAX_REWALK_PAGES).
/// `PageStart::End` entries are respected — we never re-issue a page-0 fetch
/// just because a later page returned no more results.
pub async fn query_emails(
    session: &GmailSession,
    mailbox_id: Option<&str>,
    limit: usize,
    position: usize,
    query: Option<&ParsedQuery>,
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

    Ok(ids)
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
        join_set.spawn(async move {
            let url = format!("{GMAIL_BASE}/messages/{id}?format={format}");
            let resp = client.get(&url).bearer_auth(&token).send().await?;
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

fn base64url_decode(s: &str) -> Result<Vec<u8>, Error> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(s))
        .map_err(|e| Error::Internal(format!("base64url decode failed: {e}")))
}

/// Map a Gmail HTTP error response to the right `Error` variant so frontends
/// can distinguish "your input/state is stale" (4xx — refresh the list) from
/// "Gmail is down" (5xx — retry later). Pure — unit-tested.
pub(crate) fn classify_gmail_error(
    operation: &str,
    status: reqwest::StatusCode,
    body: &str,
) -> Error {
    let msg = format!("Gmail {operation} failed ({status}): {body}");
    if status.is_client_error() {
        Error::BadRequest(msg)
    } else {
        Error::Internal(msg)
    }
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
    let resp = session
        .client
        .post(&url)
        .bearer_auth(&token)
        .json(&modify_body(add, remove))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_gmail_error(
            &format!("messages.modify {msg_id}"),
            status,
            &text,
        ));
    }
    invalidate_label_cache(session).await;
    Ok(true)
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

/// Best-effort MIME guess from filename extension. Gmail's
/// `messages.attachments.get` returns only `{size, data}` with no
/// content-type, and we don't want to spend an extra `messages.get` RTT just
/// to look it up — the user's UI already knows the type from `get_emails`.
/// Falls back to `application/octet-stream` for unknown extensions, which the
/// browser will treat as a download (with the path's filename).
pub(crate) fn mime_type_from_filename(filename: &str) -> &'static str {
    let lower = filename.to_ascii_lowercase();
    let ext = match lower.rsplit_once('.') {
        Some((_, ext)) => ext,
        None => return "application/octet-stream",
    };
    match ext {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "heic" => "image/heic",
        "txt" | "log" | "md" => "text/plain",
        "html" | "htm" => "text/html",
        "csv" => "text/csv",
        "ics" => "text/calendar",
        "json" => "application/json",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

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

    // ---- Milestone B: mime_type_from_filename ----

    #[test]
    fn mime_pdf() {
        assert_eq!(mime_type_from_filename("report.pdf"), "application/pdf");
    }

    #[test]
    fn mime_case_insensitive_extension() {
        assert_eq!(mime_type_from_filename("PHOTO.JPG"), "image/jpeg");
        assert_eq!(mime_type_from_filename("Doc.PDF"), "application/pdf");
    }

    #[test]
    fn mime_jpeg_both_extensions() {
        assert_eq!(mime_type_from_filename("a.jpg"), "image/jpeg");
        assert_eq!(mime_type_from_filename("b.jpeg"), "image/jpeg");
    }

    #[test]
    fn mime_calendar() {
        assert_eq!(mime_type_from_filename("invite.ics"), "text/calendar");
    }

    #[test]
    fn mime_office_docx() {
        assert_eq!(
            mime_type_from_filename("contract.docx"),
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        );
    }

    #[test]
    fn mime_unknown_extension_falls_back_to_octet_stream() {
        assert_eq!(
            mime_type_from_filename("mystery.xyzfoo"),
            "application/octet-stream"
        );
    }

    #[test]
    fn mime_no_extension_falls_back_to_octet_stream() {
        assert_eq!(
            mime_type_from_filename("README"),
            "application/octet-stream"
        );
    }

    #[test]
    fn mime_dot_at_start_treated_as_extension() {
        // Hidden file with no extension — `rsplit_once('.')` returns
        // `("", "bashrc")`, which is "unknown", so octet-stream.
        assert_eq!(
            mime_type_from_filename(".bashrc"),
            "application/octet-stream"
        );
    }

    #[test]
    fn mime_double_extension_uses_last() {
        // tar.gz → ext is "gz" → application/gzip
        assert_eq!(mime_type_from_filename("backup.tar.gz"), "application/gzip");
    }

    // Lock high-traffic entries that bit-rot silently (roborev 174 #7).
    #[test]
    fn mime_common_image_and_av_extensions() {
        assert_eq!(mime_type_from_filename("a.svg"), "image/svg+xml");
        assert_eq!(mime_type_from_filename("a.webp"), "image/webp");
        assert_eq!(mime_type_from_filename("a.heic"), "image/heic");
        assert_eq!(mime_type_from_filename("a.mp4"), "video/mp4");
        assert_eq!(mime_type_from_filename("a.mov"), "video/quicktime");
        assert_eq!(mime_type_from_filename("a.mp3"), "audio/mpeg");
        assert_eq!(mime_type_from_filename("a.wav"), "audio/wav");
    }

    #[test]
    fn mime_common_text_and_data_extensions() {
        assert_eq!(mime_type_from_filename("a.csv"), "text/csv");
        assert_eq!(mime_type_from_filename("a.xml"), "application/xml");
        assert_eq!(mime_type_from_filename("a.zip"), "application/zip");
        assert_eq!(mime_type_from_filename("a.tgz"), "application/gzip");
        assert_eq!(mime_type_from_filename("a.json"), "application/json");
    }

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
}
