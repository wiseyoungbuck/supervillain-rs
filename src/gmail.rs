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
    /// Per-(mailbox+query) cursor cache: index N holds the pageToken needed to
    /// fetch page N (index 0 is always None). Updated as pages are walked.
    pub page_cache: tokio::sync::Mutex<HashMap<String, Vec<Option<String>>>>,
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
        return Err(Error::Internal(format!(
            "Gmail getProfile failed ({status}): {text}"
        )));
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
            return Err(Error::Internal(format!(
                "Gmail labels.list failed ({status}): {text}"
            )));
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
                return Err(Error::Internal(format!(
                    "Gmail labels.get {id} failed ({status}): {text}"
                )));
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
        return Err(Error::Internal(format!(
            "Gmail sendAs.list failed ({status}): {text}"
        )));
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
pub fn translate_query_to_q(query: &ParsedQuery, mailbox_id: Option<&str>) -> String {
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
        Some(false) => {}
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
    if let Some(id) = mailbox_id
        && !id.is_empty()
    {
        // Gmail's q= accepts `label:` by name, but the route handler passes a
        // label *id*. We pass labelIds as a separate query param in
        // query_emails(); keep q= clean here.
        let _ = id;
    }

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
        return Err(Error::Internal(format!(
            "Gmail messages.list failed ({status}): {text}"
        )));
    }
    Ok(resp.json().await?)
}

/// Query email IDs. Translates cursor pagination to the route handler's
/// offset model. The cache stores the pageToken needed to fetch page N for
/// each (mailbox+query) key; first request seeds it, subsequent forward
/// requests follow it, jump-backs re-walk from 0 (bounded by MAX_REWALK_PAGES).
pub async fn query_emails(
    session: &GmailSession,
    mailbox_id: Option<&str>,
    limit: usize,
    position: usize,
    query: Option<&ParsedQuery>,
) -> Result<Vec<String>, Error> {
    let q = query
        .map(|q| translate_query_to_q(q, mailbox_id))
        .unwrap_or_default();
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
    let mut tokens_by_page: Vec<Option<String>> = {
        let cache = session.page_cache.lock().await;
        cache.get(&key).cloned().unwrap_or_else(|| vec![None])
    };

    let mut ids: Vec<String> = Vec::with_capacity(limit);
    let mut consumed_in_first_page = 0usize;

    for page_idx in 0..=end_page.saturating_sub(1) {
        // Extend cache forward by walking from the last known page if needed.
        if page_idx >= tokens_by_page.len() {
            // We need pageToken for page_idx but only have up to len-1.
            // Walk forward, fetching pages we don't care about just to get
            // the token. Bounded by MAX_REWALK_PAGES via end_page check above.
            while tokens_by_page.len() <= page_idx {
                let prev_token = tokens_by_page.last().and_then(|t| t.clone());
                if tokens_by_page.len() > 1 && prev_token.is_none() {
                    // Previous page had no nextPageToken — we've reached the end.
                    break;
                }
                let resp =
                    fetch_messages_page(session, &token, mailbox_id, &q, prev_token.as_deref())
                        .await?;
                tokens_by_page.push(resp.next_page_token);
            }
            if page_idx >= tokens_by_page.len() {
                // Ran out of results entirely.
                break;
            }
        }

        // Only fetch pages we actually need to extract IDs from.
        if page_idx < start_page {
            continue;
        }

        let page_token = tokens_by_page[page_idx].clone();
        let resp =
            fetch_messages_page(session, &token, mailbox_id, &q, page_token.as_deref()).await?;

        // Cache the next-page token so future calls can follow.
        let next_idx = page_idx + 1;
        if next_idx >= tokens_by_page.len() {
            tokens_by_page.push(resp.next_page_token.clone());
        } else {
            tokens_by_page[next_idx] = resp.next_page_token.clone();
        }

        for msg in resp.messages {
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
        if tokens_by_page
            .get(next_idx)
            .and_then(|t| t.as_ref())
            .is_none()
        {
            // No next page available; we're done.
            break;
        }
    }

    // Write back cache snapshot.
    let mut cache = session.page_cache.lock().await;
    cache.insert(key, tokens_by_page);

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
                return Err(Error::Internal(format!(
                    "Gmail messages.get {id} failed ({status}): {text}"
                )));
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
    let mailbox_ids: HashMap<String, bool> =
        msg.label_ids.iter().map(|l| (l.clone(), true)).collect();

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
        attachments.push(crate::types::Attachment {
            blob_id: format!("{msg_id}:{att_id}"),
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
        assert_eq!(translate_query_to_q(&q, None), "");
    }

    #[test]
    fn q_translator_single_from() {
        let mut q = ParsedQuery::default();
        q.from.push("alice@example.com".into());
        assert_eq!(translate_query_to_q(&q, None), "from:alice@example.com");
    }

    #[test]
    fn q_translator_multiple_from_ands() {
        let mut q = ParsedQuery::default();
        q.from.push("a@x.com".into());
        q.from.push("b@y.com".into());
        assert_eq!(translate_query_to_q(&q, None), "from:a@x.com from:b@y.com");
    }

    #[test]
    fn q_translator_quotes_whitespace_value() {
        let mut q = ParsedQuery::default();
        q.from.push("Alice Smith".into());
        assert_eq!(translate_query_to_q(&q, None), r#"from:"Alice Smith""#);
    }

    #[test]
    fn q_translator_quotes_subject_with_colon() {
        let mut q = ParsedQuery::default();
        q.subject.push("Re: foo".into());
        assert_eq!(translate_query_to_q(&q, None), r#"subject:"Re: foo""#);
    }

    #[test]
    fn q_translator_escapes_inner_quote() {
        let mut q = ParsedQuery::default();
        q.subject.push(r#"a"b"#.into());
        assert_eq!(translate_query_to_q(&q, None), r#"subject:"a\"b""#);
    }

    #[test]
    fn q_translator_email_with_plus_unquoted() {
        let mut q = ParsedQuery::default();
        q.from.push("bob+test@x.com".into());
        assert_eq!(translate_query_to_q(&q, None), "from:bob+test@x.com");
    }

    #[test]
    fn q_translator_is_unread() {
        let q = ParsedQuery {
            is_unread: Some(true),
            ..Default::default()
        };
        assert_eq!(translate_query_to_q(&q, None), "is:unread");
    }

    #[test]
    fn q_translator_has_attachment() {
        let q = ParsedQuery {
            has_attachment: true,
            ..Default::default()
        };
        assert_eq!(translate_query_to_q(&q, None), "has:attachment");
    }

    #[test]
    fn q_translator_dates_use_slashes_not_dashes() {
        let q = ParsedQuery {
            before: Some(NaiveDate::from_ymd_opt(2026, 1, 15).unwrap()),
            after: Some(NaiveDate::from_ymd_opt(2025, 12, 1).unwrap()),
            ..Default::default()
        };
        let s = translate_query_to_q(&q, None);
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
        assert_eq!(translate_query_to_q(&q, None), "quarterly review");
    }

    #[test]
    fn q_translator_combined_query_space_joined() {
        let mut q = ParsedQuery::default();
        q.from.push("alice@x.com".into());
        q.is_unread = Some(true);
        q.has_attachment = true;
        q.text = "report".into();
        let s = translate_query_to_q(&q, None);
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
}
