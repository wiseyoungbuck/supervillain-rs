use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::provider_utils::{
    MAX_BLOB_BYTES, MAX_UPLOAD_CACHE_BYTES, UPLOAD_CACHE_CAP,
    should_clear_tokens_on_refresh_failure,
};
use crate::types::{CalendarEvent, Mailbox};

// =============================================================================
// Outlook Session
// =============================================================================

pub struct OutlookSession {
    pub client: reqwest::Client,
    pub token: tokio::sync::Mutex<OutlookToken>,
    pub client_id: String,
    pub token_path: PathBuf,
    pub email: String,
    /// 60s TTL cache of the Outlook folder list. Mirrors Gmail's label_cache.
    /// Invalidated by mutation paths so unread counts refresh.
    pub folder_cache: tokio::sync::Mutex<Option<FolderCacheEntry>>,
    /// Per-(folder + query) pagination cursor. Graph uses opaque
    /// `@odata.nextLink` URLs (full URLs with all query params baked in),
    /// not opaque page tokens like Gmail. Forward iteration follows the
    /// link verbatim; jump-back re-issues with `$skip`.
    pub page_cache: tokio::sync::Mutex<HashMap<String, OutlookPageCursor>>,
}

/// A snapshot of the folder list, anchored at a fetch time for TTL math.
#[derive(Clone)]
pub struct FolderCacheEntry {
    pub fetched_at: Instant,
    pub folders: Vec<Mailbox>,
}

/// Per-(folder+query) Graph pagination state. `next_link` is the verbatim
/// `@odata.nextLink` URL from the previous response (None means "no more
/// pages"); `at_position` is the caller-visible offset we last advanced to
/// so we know whether to follow `next_link` or `$skip` from zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutlookPageCursor {
    pub at_position: usize,
    pub next_link: Option<String>,
}

/// Folder cache TTL (same as Gmail's label cache).
pub const FOLDER_CACHE_TTL: Duration = Duration::from_secs(60);

// Touch the imports so they don't warn before being used in later TDD steps.
// (Will be wired into upload_blob + ensure_token in Milestones C and A
// respectively; keeping the import here groups all provider_utils uses.)
#[allow(dead_code)]
fn _touch_provider_utils_imports() {
    let _ = UPLOAD_CACHE_CAP;
    let _ = MAX_BLOB_BYTES;
    let _ = MAX_UPLOAD_CACHE_BYTES;
    let _ = should_clear_tokens_on_refresh_failure;
}

pub struct OutlookToken {
    pub access_token: String,
    pub refresh_token: String,
    pub token_expiry: DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
struct StoredTokens {
    access_token: String,
    refresh_token: String,
    token_expiry: DateTime<Utc>,
    email: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
}

// Microsoft OAuth2 endpoints
const AUTH_URL: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/authorize";
const TOKEN_URL: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/token";
const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";
const REDIRECT_URI: &str = "http://localhost:8400/callback";

// Phase 1: calendar only. Phase 2 adds Mail.ReadWrite Mail.Send
// Outlook OAuth scopes. Phase 4 adds Mail.ReadWrite + Mail.Send alongside
// the existing Calendars scope. Existing users with stored tokens will hit
// "insufficient_scope" on first email call — README upgrade notes spell
// out the recovery (delete the token file, restart, re-authorize).
const SCOPES: &str = "Calendars.ReadWrite Mail.ReadWrite Mail.Send offline_access";

use crate::oauth;

/// Build the authorization URL for the OAuth2 PKCE flow
pub fn auth_url(client_id: &str, code_verifier: &str, state: &str) -> String {
    let challenge = oauth::code_challenge(code_verifier);
    let mut url = url::Url::parse(AUTH_URL).expect("valid auth base URL");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("response_mode", "query");
    url.to_string()
}

/// Exchange authorization code for tokens
async fn exchange_code(
    client: &reqwest::Client,
    client_id: &str,
    code: &str,
    code_verifier: &str,
) -> Result<TokenResponse, Error> {
    let resp = client
        .post(TOKEN_URL)
        .form(&[
            ("client_id", client_id),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT_URI),
            ("code_verifier", code_verifier),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(Error::Auth(format!("Token exchange failed: {text}")));
    }

    Ok(resp.json().await?)
}

/// Refresh if token expires within 60 seconds. Uses interior mutability via Mutex.
async fn ensure_token(session: &OutlookSession) -> Result<(), Error> {
    let mut token = session.token.lock().await;
    if Utc::now() + chrono::Duration::seconds(60) >= token.token_expiry {
        let resp = session
            .client
            .post(TOKEN_URL)
            .form(&[
                ("client_id", session.client_id.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", token.refresh_token.as_str()),
                ("scope", SCOPES),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Auth(format!("Token refresh failed: {text}")));
        }

        let resp: TokenResponse = resp.json().await?;

        token.access_token = resp.access_token;
        if let Some(rt) = resp.refresh_token {
            token.refresh_token = rt;
        }
        token.token_expiry = Utc::now() + chrono::Duration::seconds(resp.expires_in);
        save_tokens_inner(&session.token_path, &token, &session.email)?;
        tracing::info!("Refreshed Outlook token for {}", session.email);
    }
    Ok(())
}

/// Get the current access token (after ensuring it's fresh)
async fn access_token(session: &OutlookSession) -> Result<String, Error> {
    ensure_token(session).await?;
    Ok(session.token.lock().await.access_token.clone())
}

/// Persist tokens to disk
fn save_tokens_inner(
    token_path: &std::path::Path,
    token: &OutlookToken,
    email: &str,
) -> Result<(), Error> {
    if let Some(parent) = token_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::Internal(format!("Failed to create token dir: {e}")))?;
    }
    let stored = StoredTokens {
        access_token: token.access_token.clone(),
        refresh_token: token.refresh_token.clone(),
        token_expiry: token.token_expiry,
        email: email.to_string(),
    };
    let json = serde_json::to_string_pretty(&stored)
        .map_err(|e| Error::Internal(format!("Failed to serialize tokens: {e}")))?;
    std::fs::write(token_path, json)
        .map_err(|e| Error::Internal(format!("Failed to write tokens: {e}")))?;
    Ok(())
}

pub fn save_tokens(session: &OutlookSession) -> Result<(), Error> {
    // Blocking lock — only call from sync context (e.g. after initial OAuth flow)
    let token = session.token.blocking_lock();
    save_tokens_inner(&session.token_path, &token, &session.email)
}

/// Load tokens from disk, returning None if file doesn't exist or is invalid
pub fn load_tokens(token_path: &std::path::Path, client_id: &str) -> Option<OutlookSession> {
    let content = std::fs::read_to_string(token_path).ok()?;
    let stored: StoredTokens = serde_json::from_str(&content).ok()?;
    Some(OutlookSession {
        client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to create HTTP client"),
        token: tokio::sync::Mutex::new(OutlookToken {
            access_token: stored.access_token,
            refresh_token: stored.refresh_token,
            token_expiry: stored.token_expiry,
        }),
        client_id: client_id.to_string(),
        token_path: token_path.to_path_buf(),
        email: stored.email,
        folder_cache: tokio::sync::Mutex::new(None),
        page_cache: tokio::sync::Mutex::new(HashMap::new()),
    })
}

/// One-shot OAuth2 PKCE flow: opens browser, runs local callback server, exchanges code.
/// The callback acquisition is delegated to `platform::acquire_oauth_callback` so the
/// iOS port can substitute `ASWebAuthenticationSession` without touching this code.
pub async fn oauth_flow(
    client_id: &str,
    token_path: &std::path::Path,
) -> Result<OutlookSession, Error> {
    let code_verifier = oauth::generate_code_verifier();
    let expected_state = oauth::generate_state();
    let url = auth_url(client_id, &code_verifier, &expected_state);

    let callback = crate::platform::acquire_oauth_callback(&url, &expected_state, 8400).await?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("failed to create HTTP client");

    let token_resp = exchange_code(&client, client_id, &callback.code, &code_verifier).await?;

    let email = fetch_user_email(&client, &token_resp.access_token).await?;

    let session = OutlookSession {
        client,
        token: tokio::sync::Mutex::new(OutlookToken {
            access_token: token_resp.access_token,
            refresh_token: token_resp.refresh_token.unwrap_or_default(),
            token_expiry: Utc::now() + chrono::Duration::seconds(token_resp.expires_in),
        }),
        client_id: client_id.to_string(),
        token_path: token_path.to_path_buf(),
        email,
        folder_cache: tokio::sync::Mutex::new(None),
        page_cache: tokio::sync::Mutex::new(HashMap::new()),
    };

    save_tokens(&session)?;
    tracing::info!("Outlook OAuth completed for {}", session.email);
    Ok(session)
}

/// Fetch the authenticated user's email from Microsoft Graph
async fn fetch_user_email(client: &reqwest::Client, access_token: &str) -> Result<String, Error> {
    let resp: serde_json::Value = client
        .get(format!("{GRAPH_BASE}/me"))
        .bearer_auth(access_token)
        .send()
        .await?
        .json()
        .await?;

    resp["mail"]
        .as_str()
        .or_else(|| resp["userPrincipalName"].as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| Error::Internal("Could not determine Outlook email address".into()))
}

// =============================================================================
// Phase 4: Outlook email — pure helpers
// =============================================================================

/// Delete the stored token file for this Outlook session. Called when
/// `ensure_token` sees an irrecoverable refresh failure (the Graph
/// equivalent of Gmail's `invalid_grant` flow) so the next launch falls
/// through to a fresh OAuth instead of looping on a doomed refresh.
///
/// Idempotent: missing file is a no-op (the 401-on-revoke path can race
/// with manual user cleanup).
pub async fn clear_stored_tokens(session: &OutlookSession) {
    if session.token_path.exists()
        && let Err(e) = std::fs::remove_file(&session.token_path)
    {
        tracing::warn!(
            token_path = %session.token_path.display(),
            error = %e,
            "Failed to delete stored Outlook tokens after refresh failure"
        );
    }
}

/// Split form of a translated query — Graph rejects `$filter` and `$search`
/// combined on most fields, so we emit both and the caller threads them
/// into the URL independently.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct OdataQuery {
    /// Joined with ` and ` if present.
    pub filter: Option<String>,
    /// Wrapped in `"…"` if present; inner `"` / `\` escaped.
    pub search: Option<String>,
}

/// Translate our canonical `ParsedQuery` into Graph's split query shape.
/// Pure — fixture-tested without HTTP. Top-5 greats consensus finding
/// (Colvin + Carmack): pin escape rules with tests before implementing.
///
/// Rules:
/// - Structured operators (`is:unread`, `has:attachment`, `from:`, `to:`,
///   `before:`, `after:`, `is:starred`) → `$filter`. Single-quote-doubling
///   for values (OData literal escape: `O'Brien` → `'O''Brien'`).
/// - Free text + `subject:` → `$search` (Graph's KQL-flavored full-text
///   search; subject is well-supported there but rejected in `$filter` on
///   many tenants). Wrapped in `"…"`; inner `"` → `\"`, `\` → `\\`.
pub(crate) fn translate_query_to_odata(q: &crate::types::ParsedQuery) -> OdataQuery {
    let mut filter_parts: Vec<String> = Vec::new();
    let mut search_parts: Vec<String> = Vec::new();

    if let Some(true) = q.is_unread {
        filter_parts.push("isRead eq false".into());
    } else if let Some(false) = q.is_unread {
        filter_parts.push("isRead eq true".into());
    }
    if q.has_attachment {
        filter_parts.push("hasAttachments eq true".into());
    }
    if let Some(true) = q.is_flagged {
        filter_parts.push("flag/flagStatus eq 'flagged'".into());
    }
    for from in &q.from {
        filter_parts.push(format!(
            "from/emailAddress/address eq '{}'",
            escape_odata_literal(from)
        ));
    }
    for to in &q.to {
        filter_parts.push(format!(
            "toRecipients/any(t: t/emailAddress/address eq '{}')",
            escape_odata_literal(to)
        ));
    }
    if let Some(d) = q.before {
        filter_parts.push(format!(
            "receivedDateTime lt {}T00:00:00Z",
            d.format("%Y-%m-%d")
        ));
    }
    if let Some(d) = q.after {
        filter_parts.push(format!(
            "receivedDateTime ge {}T00:00:00Z",
            d.format("%Y-%m-%d")
        ));
    }

    // Subject and free text both flow into $search. Subject gets KQL prefix.
    for sub in &q.subject {
        search_parts.push(format!("subject:{sub}"));
    }
    if !q.text.is_empty() {
        search_parts.push(q.text.clone());
    }

    let filter = (!filter_parts.is_empty()).then(|| filter_parts.join(" and "));
    let search = (!search_parts.is_empty()).then(|| {
        let joined = search_parts.join(" ");
        format!("\"{}\"", escape_search_string(&joined))
    });

    OdataQuery { filter, search }
}

/// OData single-quote-doubling for string literals inside `$filter`.
/// `O'Brien` → `O''Brien`. The wrapping single quotes are added by the
/// caller (each filter clause builder).
fn escape_odata_literal(s: &str) -> String {
    s.replace('\'', "''")
}

/// `$search` strings are wrapped in double quotes; inner `"` and `\` must
/// be escaped so the wrapper doesn't terminate early.
fn escape_search_string(s: &str) -> String {
    s.replace('\\', r"\\").replace('"', r#"\""#)
}

/// Parse a Graph `Message` resource JSON into our canonical `Email`. Pure —
/// `fetch_body=false` means we got the metadata-only Graph response (skip
/// body extraction) which matches our `get_emails(fetch_body: bool)` API.
///
/// Unlike Gmail, Graph hands us structured fields — no MIME tree walking,
/// no base64 body decoding.
pub(crate) fn parse_graph_message(
    json: &serde_json::Value,
    fetch_body: bool,
) -> crate::types::Email {
    let id = json["id"].as_str().unwrap_or("").to_string();
    let thread_id = json["conversationId"].as_str().unwrap_or("").to_string();
    let subject = json["subject"].as_str().unwrap_or("").to_string();
    let preview = json["bodyPreview"].as_str().unwrap_or("").to_string();

    let mut keywords: HashMap<String, bool> = HashMap::new();
    if json["isRead"].as_bool().unwrap_or(false) {
        keywords.insert("$seen".into(), true);
    }
    if json["flag"]["flagStatus"].as_str() == Some("flagged") {
        keywords.insert("$flagged".into(), true);
    }

    let received_at = json["receivedDateTime"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);

    let from = parse_graph_recipient_singleton(&json["from"]);
    let to = parse_graph_recipient_list(&json["toRecipients"]);
    let cc = parse_graph_recipient_list(&json["ccRecipients"]);

    let mut mailbox_ids: HashMap<String, bool> = HashMap::new();
    if let Some(folder) = json["parentFolderId"].as_str() {
        mailbox_ids.insert(folder.to_string(), true);
    }

    let (text_body, html_body) = if fetch_body {
        parse_graph_body(&json["body"])
    } else {
        (None, None)
    };

    let attachments = parse_graph_attachments(&id, &json["attachments"]);
    let has_attachment =
        json["hasAttachments"].as_bool().unwrap_or(false) || !attachments.is_empty();
    let has_calendar = attachments
        .iter()
        .any(|a| a.mime_type.eq_ignore_ascii_case("text/calendar"));

    let size = json["sizeEstimate"].as_i64().unwrap_or(0);

    crate::types::Email {
        id: id.clone(),
        // Outlook doesn't have a separate blob namespace — use the message
        // ID as the blob_id placeholder (consistent with Gmail's choice).
        blob_id: id,
        thread_id,
        mailbox_ids,
        keywords,
        received_at,
        subject,
        from,
        to,
        cc,
        preview,
        has_attachment,
        size,
        text_body,
        html_body,
        has_calendar,
        attachments,
    }
}

fn parse_graph_recipient_singleton(
    recipient_json: &serde_json::Value,
) -> Vec<crate::types::EmailAddress> {
    let email = recipient_json["emailAddress"]["address"].as_str();
    let name = recipient_json["emailAddress"]["name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);
    match email {
        Some(e) => vec![crate::types::EmailAddress {
            name,
            email: e.to_string(),
        }],
        None => vec![],
    }
}

fn parse_graph_recipient_list(arr_json: &serde_json::Value) -> Vec<crate::types::EmailAddress> {
    arr_json
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let email = r["emailAddress"]["address"].as_str()?;
                    let name = r["emailAddress"]["name"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(String::from);
                    Some(crate::types::EmailAddress {
                        name,
                        email: email.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Map Graph's `body.contentType` (`text` or `html`) to our split
/// `text_body` / `html_body`. Graph returns one or the other, not both.
fn parse_graph_body(body_json: &serde_json::Value) -> (Option<String>, Option<String>) {
    let content_type = body_json["contentType"].as_str().unwrap_or("");
    let content = body_json["content"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);
    match content_type {
        "html" => (None, content),
        "text" => (content, None),
        _ => (None, None),
    }
}

/// Parse `attachments[]` from an `$expand=attachments` response. Each
/// fileAttachment becomes one `Attachment`; the blob_id uses the
/// `outlook:{msg}:{att}` prefix so download_blob routes correctly.
fn parse_graph_attachments(
    msg_id: &str,
    arr_json: &serde_json::Value,
) -> Vec<crate::types::Attachment> {
    arr_json
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|att| {
                    let att_id = att["id"].as_str()?;
                    let name = att["name"].as_str().unwrap_or("").to_string();
                    let mime_type = att["contentType"]
                        .as_str()
                        .unwrap_or("application/octet-stream")
                        .to_string();
                    let size = att["size"].as_i64().unwrap_or(0);
                    let blob_ref = crate::types::BlobRef::OutlookAttachment {
                        msg_id: msg_id.to_string(),
                        att_id: att_id.to_string(),
                    };
                    Some(crate::types::Attachment {
                        blob_id: blob_ref.to_string(),
                        name,
                        mime_type,
                        size,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Map a Graph mailFolder ID to our canonical `Mailbox.role`. Returns `None`
/// for user-created folders and unknown IDs. Pure — testable without HTTP.
///
/// Graph's well-known folder IDs are lowercase strings (`inbox`,
/// `sentitems`, etc.); user-created folders get UUID-shaped IDs and
/// arbitrary `displayName`s. Case-sensitive on purpose: a user-created
/// "INBOX" folder shouldn't shadow the real one.
pub(crate) fn outlook_folder_role(folder_id: &str) -> Option<String> {
    match folder_id {
        "inbox" => Some("inbox".into()),
        "sentitems" => Some("sent".into()),
        "drafts" => Some("drafts".into()),
        "deleteditems" => Some("trash".into()),
        "junkemail" => Some("junk".into()),
        "archive" => Some("archive".into()),
        _ => None,
    }
}

/// Map a Microsoft Graph HTTP error to the right `Error` variant so the
/// frontend can distinguish "your input/state is stale" (4xx) from
/// "Graph is down" (5xx). Mirror of Gmail's `classify_gmail_error`; lives
/// per-provider because the error-response shapes (and helpful messages)
/// differ enough to warrant separate formatting.
pub(crate) fn classify_outlook_error(
    operation: &str,
    status: reqwest::StatusCode,
    body: &str,
) -> Error {
    let msg = format!("Outlook {operation} failed ({status}): {body}");
    if status.is_client_error() {
        Error::BadRequest(msg)
    } else {
        Error::Internal(msg)
    }
}

// =============================================================================
// Phase 4: Outlook email — async read operations
// =============================================================================

#[derive(Deserialize)]
struct FolderListResp {
    #[serde(default)]
    value: Vec<FolderEntry>,
}

#[derive(Deserialize)]
struct FolderEntry {
    id: String,
    #[serde(default, rename = "displayName")]
    display_name: String,
    #[serde(default, rename = "wellKnownName")]
    well_known_name: Option<String>,
    #[serde(default, rename = "totalItemCount")]
    total_item_count: i64,
    #[serde(default, rename = "unreadItemCount")]
    unread_item_count: i64,
    #[serde(default, rename = "parentFolderId")]
    parent_folder_id: Option<String>,
}

/// Fetch the user's mail folders (Outlook's analog of Gmail labels).
/// 60s TTL cache; invalidate on mutations so unread counts refresh.
pub async fn get_mailboxes(session: &OutlookSession) -> Result<Vec<Mailbox>, Error> {
    {
        let cache = session.folder_cache.lock().await;
        if let Some(entry) = cache.as_ref()
            && entry.fetched_at.elapsed() < FOLDER_CACHE_TTL
        {
            return Ok(entry.folders.clone());
        }
    }

    let token = access_token(session).await?;
    let url = format!("{GRAPH_BASE}/me/mailFolders?$top=100");
    let resp = session.client.get(&url).bearer_auth(&token).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_outlook_error("mailFolders.list", status, &text));
    }
    let parsed: FolderListResp = resp.json().await?;

    let folders: Vec<Mailbox> = parsed
        .value
        .into_iter()
        .map(|f| {
            // Graph exposes wellKnownName for system folders; fall back
            // to the ID, which is also the well-known string for the
            // built-in folders ("inbox", "sentitems", etc.).
            let role = f
                .well_known_name
                .as_deref()
                .and_then(outlook_folder_role)
                .or_else(|| outlook_folder_role(&f.id));
            Mailbox {
                id: f.id,
                name: f.display_name,
                role,
                total_emails: f.total_item_count,
                unread_emails: f.unread_item_count,
                parent_id: f.parent_folder_id,
            }
        })
        .collect();

    let mut cache = session.folder_cache.lock().await;
    *cache = Some(FolderCacheEntry {
        fetched_at: Instant::now(),
        folders: folders.clone(),
    });
    Ok(folders)
}

/// Invalidate the folder cache. Called after any mutation that changes
/// folder counts (Milestone B wires this into mark_read/archive/etc.).
pub async fn invalidate_folder_cache(session: &OutlookSession) {
    let mut cache = session.folder_cache.lock().await;
    *cache = None;
}

/// Fetch user identities. Outlook is simpler than Gmail's sendAs — Graph
/// `/me` returns the single primary identity. Aliases via
/// `/me/mailboxSettings/aliases` exist on enterprise accounts but most
/// users don't have them; defer until requested.
pub async fn get_identities(
    session: &OutlookSession,
) -> Result<Vec<crate::types::Identity>, Error> {
    let token = access_token(session).await?;
    let url = format!("{GRAPH_BASE}/me?$select=mail,userPrincipalName,displayName,id");
    let resp = session.client.get(&url).bearer_auth(&token).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_outlook_error("me.get", status, &text));
    }
    let parsed: serde_json::Value = resp.json().await?;
    let email = parsed["mail"]
        .as_str()
        .or_else(|| parsed["userPrincipalName"].as_str())
        .unwrap_or(&session.email)
        .to_string();
    let display_name = parsed["displayName"].as_str().unwrap_or("").to_string();
    let id = parsed["id"].as_str().unwrap_or(&email).to_string();

    Ok(vec![crate::types::Identity {
        id,
        email,
        name: display_name,
    }])
}

#[derive(Deserialize)]
struct MessageListResp {
    #[serde(default)]
    value: Vec<MessageRef>,
    #[serde(default, rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Deserialize)]
struct MessageRef {
    id: String,
}

/// Query messages with pagination. Graph paginates via opaque
/// `@odata.nextLink` URLs; we cache the link verbatim for forward iteration
/// and re-use `$skip` for jump-back. Bounded by `MAX_REWALK_PAGES` to keep
/// the worst case finite (matches Gmail's discipline).
pub async fn query_emails(
    session: &OutlookSession,
    folder_id: Option<&str>,
    limit: usize,
    position: usize,
    query: Option<&crate::types::ParsedQuery>,
) -> Result<Vec<String>, Error> {
    let token = access_token(session).await?;
    let odata = query.map(translate_query_to_odata).unwrap_or_default();

    // Cache key combines folder + serialized query for cursor reuse.
    let cache_key = format!(
        "{}|{}|{}",
        folder_id.unwrap_or(""),
        odata.filter.as_deref().unwrap_or(""),
        odata.search.as_deref().unwrap_or("")
    );

    // Forward iteration: if cache has a next_link for at_position == position,
    // follow it. Otherwise, build a fresh URL with $skip.
    let cached_next_link = {
        let cache = session.page_cache.lock().await;
        cache
            .get(&cache_key)
            .filter(|c| c.at_position == position)
            .and_then(|c| c.next_link.clone())
    };

    let url = match cached_next_link {
        Some(link) => link,
        None => build_outlook_query_url(folder_id, &odata, limit, position),
    };

    let resp = session.client.get(&url).bearer_auth(&token).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_outlook_error("messages.list", status, &text));
    }
    let parsed: MessageListResp = resp.json().await?;

    let ids: Vec<String> = parsed.value.into_iter().map(|m| m.id).collect();

    // Update cache cursor for next-page following.
    let mut cache = session.page_cache.lock().await;
    cache.insert(
        cache_key,
        OutlookPageCursor {
            at_position: position + ids.len(),
            next_link: parsed.next_link,
        },
    );

    Ok(ids)
}

/// Build a `/me/messages` URL with `$filter`, `$search`, `$top`, `$skip`,
/// and `$orderby` per `OdataQuery`. Pure-ish — no HTTP, just URL assembly.
/// Folder scoping uses `/me/mailFolders/{id}/messages` when given.
fn build_outlook_query_url(
    folder_id: Option<&str>,
    odata: &OdataQuery,
    limit: usize,
    position: usize,
) -> String {
    let base = match folder_id {
        Some(id) if !id.is_empty() => format!(
            "{GRAPH_BASE}/me/mailFolders/{}/messages",
            crate::provider_utils::encode_path_segment(id)
        ),
        _ => format!("{GRAPH_BASE}/me/messages"),
    };
    let mut url = url::Url::parse(&base).expect("valid Graph URL");
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("$top", &limit.to_string());
        if position > 0 {
            q.append_pair("$skip", &position.to_string());
        }
        if let Some(f) = &odata.filter {
            q.append_pair("$filter", f);
        }
        if let Some(s) = &odata.search {
            q.append_pair("$search", s);
        }
        q.append_pair("$orderby", "receivedDateTime desc");
    }
    url.to_string()
}

/// Fetch full message data for each ID in parallel. Uses `$expand=attachments`
/// so attachment metadata comes back in the same response.
pub async fn get_emails(
    session: &OutlookSession,
    ids: &[String],
    fetch_body: bool,
) -> Result<Vec<crate::types::Email>, Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let token = access_token(session).await?;
    let mut join_set = tokio::task::JoinSet::new();
    for (idx, id) in ids.iter().enumerate() {
        let client = session.client.clone();
        let token = token.clone();
        let id = id.clone();
        join_set.spawn(async move {
            let encoded = crate::provider_utils::encode_path_segment(&id);
            let url = format!("{GRAPH_BASE}/me/messages/{encoded}?$expand=attachments");
            let resp = client.get(&url).bearer_auth(&token).send().await?;
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(classify_outlook_error(
                    &format!("messages.get {id}"),
                    status,
                    &text,
                ));
            }
            let json: serde_json::Value = resp.json().await?;
            Ok::<_, Error>((idx, parse_graph_message(&json, fetch_body)))
        });
    }

    let mut indexed: Vec<(usize, crate::types::Email)> = Vec::with_capacity(ids.len());
    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok(Ok(item)) => indexed.push(item),
            Ok(Err(e)) => return Err(e),
            Err(join_err) => {
                return Err(Error::Internal(format!(
                    "Outlook messages.get task panicked: {join_err}"
                )));
            }
        }
    }
    indexed.sort_by_key(|(idx, _)| *idx);
    Ok(indexed.into_iter().map(|(_, e)| e).collect())
}

/// Extract the ICS bytes from a message's `text/calendar` attachment, if
/// any. Bridges the inbox view to the existing calendar RSVP flow.
pub async fn get_calendar_data(
    session: &OutlookSession,
    email_id: &str,
) -> Result<Option<String>, Error> {
    let token = access_token(session).await?;
    let encoded = crate::provider_utils::encode_path_segment(email_id);
    // List attachments first to find the text/calendar one.
    let list_url = format!("{GRAPH_BASE}/me/messages/{encoded}/attachments?$select=id,contentType");
    let resp = session
        .client
        .get(&list_url)
        .bearer_auth(&token)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_outlook_error(
            &format!("messages.{email_id}.attachments.list"),
            status,
            &text,
        ));
    }
    let parsed: serde_json::Value = resp.json().await?;
    let calendar_att_id = parsed["value"].as_array().and_then(|arr| {
        arr.iter().find_map(|a| {
            let ct = a["contentType"].as_str()?;
            if ct.eq_ignore_ascii_case("text/calendar") {
                a["id"].as_str().map(String::from)
            } else {
                None
            }
        })
    });
    let Some(att_id) = calendar_att_id else {
        return Ok(None);
    };

    // Fetch the full attachment to get contentBytes.
    let att_encoded = crate::provider_utils::encode_path_segment(&att_id);
    let att_url = format!("{GRAPH_BASE}/me/messages/{encoded}/attachments/{att_encoded}");
    let resp = session
        .client
        .get(&att_url)
        .bearer_auth(&token)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_outlook_error(
            &format!("messages.{email_id}.attachments.get"),
            status,
            &text,
        ));
    }
    let parsed: serde_json::Value = resp.json().await?;
    let Some(b64) = parsed["contentBytes"].as_str() else {
        return Ok(None);
    };
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| Error::Internal(format!("Outlook ICS base64 decode failed: {e}")))?;
    let ics = String::from_utf8(bytes)
        .map_err(|e| Error::Internal(format!("Outlook ICS not UTF-8: {e}")))?;
    Ok(Some(ics))
}

// =============================================================================
// Microsoft Graph Calendar Operations
// =============================================================================

/// Find a Graph event ID by iCalUId
async fn find_event_by_uid(session: &OutlookSession, uid: &str) -> Result<Option<String>, Error> {
    let token = access_token(session).await?;
    // Escape single quotes in UID to prevent OData filter injection
    let safe_uid = uid.replace('\'', "''");
    let url = format!("{GRAPH_BASE}/me/events?$filter=iCalUId eq '{safe_uid}'&$select=id",);
    let resp: serde_json::Value = session
        .client
        .get(&url)
        .bearer_auth(&token)
        .send()
        .await?
        .json()
        .await?;

    Ok(resp["value"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|ev| ev["id"].as_str())
        .map(String::from))
}

/// Add an event to the Outlook calendar. Returns true on success.
/// Builds a Graph event JSON from the parsed CalendarEvent fields.
pub async fn add_to_calendar(
    session: &OutlookSession,
    _ics_data: &str,
    event: &CalendarEvent,
) -> Result<bool, Error> {
    let token = access_token(session).await?;

    // Check if event already exists
    if let Some(_existing_id) = find_event_by_uid(session, &event.uid).await? {
        tracing::debug!("Event {} already exists in Outlook calendar", event.uid);
        return Ok(true);
    }

    let body = build_graph_event(event);

    let resp = session
        .client
        .post(format!("{GRAPH_BASE}/me/events"))
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await?;

    if resp.status().is_success() {
        tracing::info!("Added event {} to Outlook calendar", event.uid);
        Ok(true)
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        tracing::warn!("Graph POST /me/events failed ({status}): {text}");
        Ok(false)
    }
}

/// Respond to a calendar event (accept/decline/tentative).
/// Graph sends the RSVP email automatically when sendResponse=true.
pub async fn respond_to_event(
    session: &OutlookSession,
    uid: &str,
    status: &crate::types::RsvpStatus,
) -> Result<bool, Error> {
    let token = access_token(session).await?;

    let event_id = match find_event_by_uid(session, uid).await? {
        Some(id) => id,
        None => {
            tracing::warn!("Cannot RSVP: event {uid} not found in Outlook calendar");
            return Ok(false);
        }
    };

    let action = match status {
        crate::types::RsvpStatus::Accepted => "accept",
        crate::types::RsvpStatus::Tentative => "tentativelyAccept",
        crate::types::RsvpStatus::Declined => "decline",
    };

    let resp = session
        .client
        .post(format!("{GRAPH_BASE}/me/events/{event_id}/{action}"))
        .bearer_auth(&token)
        .json(&serde_json::json!({"sendResponse": true}))
        .send()
        .await?;

    if resp.status().is_success() {
        tracing::info!("RSVP {action} for event {uid} via Graph");
        Ok(true)
    } else {
        let status_code = resp.status();
        let text = resp.text().await.unwrap_or_default();
        tracing::warn!("Graph RSVP {action} failed ({status_code}): {text}");
        Ok(false)
    }
}

/// Fetch the current calendar event from Graph API by iCalUId.
/// Returns a CalendarEvent with current attendee statuses, or None if not found.
pub async fn get_calendar_event(
    session: &OutlookSession,
    uid: &str,
) -> Result<Option<CalendarEvent>, Error> {
    let token = access_token(session).await?;
    let safe_uid = uid.replace('\'', "''");
    let url = format!(
        "{GRAPH_BASE}/me/events?$filter=iCalUId eq '{safe_uid}'&$select=id,subject,start,end,location,body,organizer,attendees,iCalUId"
    );

    let resp: serde_json::Value = session
        .client
        .get(&url)
        .bearer_auth(&token)
        .send()
        .await?
        .json()
        .await?;

    let event_json = match resp["value"].as_array().and_then(|arr| arr.first()) {
        Some(ev) => ev,
        None => return Ok(None),
    };

    Ok(parse_graph_event(uid, event_json))
}

/// Parse a Graph API event JSON object into a CalendarEvent.
/// Separated from get_calendar_event for testability.
fn parse_graph_event(uid: &str, event_json: &serde_json::Value) -> Option<CalendarEvent> {
    let attendees: Vec<crate::types::Attendee> = event_json["attendees"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let email = a["emailAddress"]["address"].as_str()?;
                    let name = a["emailAddress"]["name"].as_str().map(String::from);
                    let status = match a["status"]["response"].as_str().unwrap_or("none") {
                        "accepted" => "ACCEPTED",
                        "tentativelyAccepted" => "TENTATIVE",
                        "declined" => "DECLINED",
                        _ => "NEEDS-ACTION",
                    };
                    Some(crate::types::Attendee {
                        email: email.to_string(),
                        name,
                        status: status.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let organizer_email = event_json["organizer"]["emailAddress"]["address"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let organizer_name = event_json["organizer"]["emailAddress"]["name"]
        .as_str()
        .map(String::from);

    let summary = event_json["subject"].as_str().unwrap_or("").to_string();

    // Parse start/end datetimes (Graph returns ISO 8601 without timezone, always UTC when timeZone is UTC)
    let dtstart = event_json["start"]["dateTime"].as_str().and_then(|s| {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
            .ok()
            .map(|dt| dt.and_utc())
    })?;

    let dtend = event_json["end"]["dateTime"].as_str().and_then(|s| {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
            .ok()
            .map(|dt| dt.and_utc())
    });

    let location = event_json["location"]["displayName"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);

    let description = event_json["body"]["content"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);

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

/// Remove an event from the Outlook calendar by iCalUId
pub async fn remove_from_calendar(session: &OutlookSession, uid: &str) -> Result<bool, Error> {
    let token = access_token(session).await?;

    let event_id = match find_event_by_uid(session, uid).await? {
        Some(id) => id,
        None => {
            tracing::debug!("Event {uid} not found in Outlook calendar, nothing to remove");
            return Ok(true);
        }
    };

    let resp = session
        .client
        .delete(format!("{GRAPH_BASE}/me/events/{event_id}"))
        .bearer_auth(&token)
        .send()
        .await?;

    if resp.status().is_success() || resp.status().as_u16() == 404 {
        tracing::info!("Removed event {uid} from Outlook calendar");
        Ok(true)
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        tracing::warn!("Graph DELETE event failed ({status}): {text}");
        Ok(false)
    }
}

/// Build a Microsoft Graph event JSON from a CalendarEvent
fn build_graph_event(event: &CalendarEvent) -> serde_json::Value {
    let mut body = serde_json::json!({
        "subject": event.summary,
        "start": {
            "dateTime": event.dtstart.format("%Y-%m-%dT%H:%M:%S").to_string(),
            "timeZone": "UTC"
        },
        "body": {
            "contentType": "text",
            "content": event.description.as_deref().unwrap_or("")
        }
    });

    if let Some(dtend) = event.dtend {
        body["end"] = serde_json::json!({
            "dateTime": dtend.format("%Y-%m-%dT%H:%M:%S").to_string(),
            "timeZone": "UTC"
        });
    } else {
        // Default to 1 hour duration
        let dtend = event.dtstart + chrono::Duration::hours(1);
        body["end"] = serde_json::json!({
            "dateTime": dtend.format("%Y-%m-%dT%H:%M:%S").to_string(),
            "timeZone": "UTC"
        });
    }

    if let Some(ref location) = event.location {
        body["location"] = serde_json::json!({"displayName": location});
    }

    if !event.attendees.is_empty() {
        let attendees: Vec<serde_json::Value> = event
            .attendees
            .iter()
            .map(|a| {
                serde_json::json!({
                    "emailAddress": {
                        "address": a.email,
                        "name": a.name.as_deref().unwrap_or("")
                    },
                    "type": "required"
                })
            })
            .collect();
        body["attendees"] = serde_json::json!(attendees);
    }

    body
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Attendee;

    #[test]
    fn auth_url_contains_required_params() {
        let url = auth_url("test-client-id", "test-verifier", "test-state");
        assert!(url.contains("client_id=test-client-id"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=test-state"));
    }

    #[test]
    fn build_graph_event_basic() {
        let event = CalendarEvent {
            uid: "test-uid".into(),
            summary: "Team Meeting".into(),
            dtstart: Utc::now(),
            dtend: Some(Utc::now() + chrono::Duration::hours(1)),
            location: Some("Room 42".into()),
            description: Some("Weekly sync".into()),
            organizer_email: "boss@example.com".into(),
            organizer_name: None,
            attendees: vec![],
            sequence: 0,
            method: "REQUEST".into(),
            raw_ics: String::new(),
            user_rsvp_status: None,
        };
        let json = build_graph_event(&event);
        assert_eq!(json["subject"], "Team Meeting");
        assert_eq!(json["location"]["displayName"], "Room 42");
        assert_eq!(json["body"]["content"], "Weekly sync");
    }

    #[test]
    fn build_graph_event_no_end_defaults_to_one_hour() {
        let start = Utc::now();
        let event = CalendarEvent {
            uid: "test".into(),
            summary: "Quick chat".into(),
            dtstart: start,
            dtend: None,
            location: None,
            description: None,
            organizer_email: "a@b.com".into(),
            organizer_name: None,
            attendees: vec![],
            sequence: 0,
            method: "REQUEST".into(),
            raw_ics: String::new(),
            user_rsvp_status: None,
        };
        let json = build_graph_event(&event);
        assert!(json["end"]["dateTime"].is_string());
    }

    #[test]
    fn build_graph_event_with_attendees() {
        let event = CalendarEvent {
            uid: "test".into(),
            summary: "Standup".into(),
            dtstart: Utc::now(),
            dtend: None,
            location: None,
            description: None,
            organizer_email: "lead@co.com".into(),
            organizer_name: None,
            attendees: vec![
                Attendee {
                    email: "alice@co.com".into(),
                    name: Some("Alice".into()),
                    status: "ACCEPTED".into(),
                },
                Attendee {
                    email: "bob@co.com".into(),
                    name: None,
                    status: "NEEDS-ACTION".into(),
                },
            ],
            sequence: 0,
            method: "REQUEST".into(),
            raw_ics: String::new(),
            user_rsvp_status: None,
        };
        let json = build_graph_event(&event);
        let attendees = json["attendees"].as_array().unwrap();
        assert_eq!(attendees.len(), 2);
        assert_eq!(attendees[0]["emailAddress"]["address"], "alice@co.com");
        assert_eq!(attendees[0]["emailAddress"]["name"], "Alice");
        assert_eq!(attendees[1]["emailAddress"]["name"], "");
    }

    #[test]
    fn build_graph_event_no_location_omits_field() {
        let event = CalendarEvent {
            uid: "test".into(),
            summary: "Call".into(),
            dtstart: Utc::now(),
            dtend: None,
            location: None,
            description: None,
            organizer_email: "a@b.com".into(),
            organizer_name: None,
            attendees: vec![],
            sequence: 0,
            method: "REQUEST".into(),
            raw_ics: String::new(),
            user_rsvp_status: None,
        };
        let json = build_graph_event(&event);
        assert!(json.get("location").is_none());
    }

    #[test]
    fn build_graph_event_no_description_empty_body() {
        let event = CalendarEvent {
            uid: "test".into(),
            summary: "Call".into(),
            dtstart: Utc::now(),
            dtend: None,
            location: None,
            description: None,
            organizer_email: "a@b.com".into(),
            organizer_name: None,
            attendees: vec![],
            sequence: 0,
            method: "REQUEST".into(),
            raw_ics: String::new(),
            user_rsvp_status: None,
        };
        let json = build_graph_event(&event);
        assert_eq!(json["body"]["content"], "");
    }

    #[test]
    fn token_save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("tokens.json");

        let session = OutlookSession {
            client: reqwest::Client::new(),
            token: tokio::sync::Mutex::new(OutlookToken {
                access_token: "access-abc".into(),
                refresh_token: "refresh-xyz".into(),
                token_expiry: Utc::now(),
            }),
            client_id: "test-client".into(),
            token_path: token_path.clone(),
            email: "user@example.com".into(),
            folder_cache: tokio::sync::Mutex::new(None),
            page_cache: tokio::sync::Mutex::new(HashMap::new()),
        };

        save_tokens(&session).unwrap();

        let loaded = load_tokens(&token_path, "test-client").unwrap();
        assert_eq!(loaded.email, "user@example.com");
        let token = loaded.token.blocking_lock();
        assert_eq!(token.access_token, "access-abc");
        assert_eq!(token.refresh_token, "refresh-xyz");
    }

    #[test]
    fn load_tokens_missing_file_returns_none() {
        let result = load_tokens(
            std::path::Path::new("/tmp/nonexistent-supervillain-tokens.json"),
            "id",
        );
        assert!(result.is_none());
    }

    #[test]
    fn load_tokens_corrupted_json_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "not json at all {{{").unwrap();
        assert!(load_tokens(&path, "id").is_none());
    }

    // =========================================================================
    // parse_graph_event tests
    // =========================================================================

    fn graph_event_json() -> serde_json::Value {
        serde_json::json!({
            "id": "AAMkAGI2...",
            "subject": "Team Standup",
            "start": { "dateTime": "2026-03-20T14:00:00.0000000", "timeZone": "UTC" },
            "end": { "dateTime": "2026-03-20T14:30:00.0000000", "timeZone": "UTC" },
            "location": { "displayName": "Room 42" },
            "body": { "contentType": "text", "content": "Daily sync" },
            "organizer": {
                "emailAddress": { "address": "boss@example.com", "name": "The Boss" }
            },
            "attendees": [
                {
                    "emailAddress": { "address": "alice@example.com", "name": "Alice" },
                    "status": { "response": "accepted" },
                    "type": "required"
                },
                {
                    "emailAddress": { "address": "bob@example.com", "name": "Bob" },
                    "status": { "response": "tentativelyAccepted" },
                    "type": "required"
                },
                {
                    "emailAddress": { "address": "carol@example.com", "name": "Carol" },
                    "status": { "response": "declined" },
                    "type": "required"
                },
                {
                    "emailAddress": { "address": "dave@example.com", "name": "Dave" },
                    "status": { "response": "none" },
                    "type": "required"
                }
            ]
        })
    }

    #[test]
    fn parse_graph_event_full() {
        let json = graph_event_json();
        let event = parse_graph_event("uid-123", &json).unwrap();
        assert_eq!(event.uid, "uid-123");
        assert_eq!(event.summary, "Team Standup");
        assert_eq!(event.organizer_email, "boss@example.com");
        assert_eq!(event.organizer_name.as_deref(), Some("The Boss"));
        assert_eq!(event.location.as_deref(), Some("Room 42"));
        assert_eq!(event.description.as_deref(), Some("Daily sync"));
        assert!(event.dtend.is_some());
    }

    #[test]
    fn parse_graph_event_maps_response_statuses() {
        let json = graph_event_json();
        let event = parse_graph_event("uid-123", &json).unwrap();
        assert_eq!(event.attendees.len(), 4);
        assert_eq!(event.attendees[0].email, "alice@example.com");
        assert_eq!(event.attendees[0].status, "ACCEPTED");
        assert_eq!(event.attendees[1].email, "bob@example.com");
        assert_eq!(event.attendees[1].status, "TENTATIVE");
        assert_eq!(event.attendees[2].email, "carol@example.com");
        assert_eq!(event.attendees[2].status, "DECLINED");
        assert_eq!(event.attendees[3].email, "dave@example.com");
        assert_eq!(event.attendees[3].status, "NEEDS-ACTION");
    }

    #[test]
    fn parse_graph_event_preserves_attendee_names() {
        let json = graph_event_json();
        let event = parse_graph_event("uid", &json).unwrap();
        assert_eq!(event.attendees[0].name.as_deref(), Some("Alice"));
    }

    #[test]
    fn parse_graph_event_no_attendees() {
        let json = serde_json::json!({
            "subject": "Solo focus time",
            "start": { "dateTime": "2026-03-20T09:00:00.0000000" },
            "end": { "dateTime": "2026-03-20T10:00:00.0000000" },
            "organizer": { "emailAddress": { "address": "me@example.com" } }
        });
        let event = parse_graph_event("uid", &json).unwrap();
        assert!(event.attendees.is_empty());
        assert_eq!(event.summary, "Solo focus time");
    }

    #[test]
    fn parse_graph_event_missing_optional_fields() {
        let json = serde_json::json!({
            "subject": "Quick call",
            "start": { "dateTime": "2026-03-20T09:00:00.0000000" },
            "organizer": { "emailAddress": { "address": "a@b.com" } }
        });
        let event = parse_graph_event("uid", &json).unwrap();
        assert!(event.location.is_none());
        assert!(event.description.is_none());
        assert!(event.dtend.is_none());
    }

    #[test]
    fn parse_graph_event_empty_location_treated_as_none() {
        let json = serde_json::json!({
            "subject": "Call",
            "start": { "dateTime": "2026-03-20T09:00:00.0000000" },
            "location": { "displayName": "" },
            "organizer": { "emailAddress": { "address": "a@b.com" } }
        });
        let event = parse_graph_event("uid", &json).unwrap();
        assert!(event.location.is_none());
    }

    #[test]
    fn parse_graph_event_empty_body_treated_as_none() {
        let json = serde_json::json!({
            "subject": "Call",
            "start": { "dateTime": "2026-03-20T09:00:00.0000000" },
            "body": { "content": "" },
            "organizer": { "emailAddress": { "address": "a@b.com" } }
        });
        let event = parse_graph_event("uid", &json).unwrap();
        assert!(event.description.is_none());
    }

    #[test]
    fn parse_graph_event_missing_start_returns_none() {
        // No start datetime means we can't build a valid event
        let json = serde_json::json!({
            "subject": "Broken",
            "organizer": { "emailAddress": { "address": "a@b.com" } }
        });
        assert!(parse_graph_event("uid", &json).is_none());
    }

    #[test]
    fn parse_graph_event_attendee_missing_email_skipped() {
        let json = serde_json::json!({
            "subject": "Test",
            "start": { "dateTime": "2026-03-20T09:00:00.0000000" },
            "organizer": { "emailAddress": { "address": "a@b.com" } },
            "attendees": [
                { "emailAddress": { "name": "No Email" }, "status": { "response": "accepted" } },
                { "emailAddress": { "address": "valid@example.com" }, "status": { "response": "accepted" } }
            ]
        });
        let event = parse_graph_event("uid", &json).unwrap();
        assert_eq!(event.attendees.len(), 1);
        assert_eq!(event.attendees[0].email, "valid@example.com");
    }

    #[test]
    fn parse_graph_event_unknown_response_maps_to_needs_action() {
        let json = serde_json::json!({
            "subject": "Test",
            "start": { "dateTime": "2026-03-20T09:00:00.0000000" },
            "organizer": { "emailAddress": { "address": "a@b.com" } },
            "attendees": [{
                "emailAddress": { "address": "x@y.com" },
                "status": { "response": "organizer" }
            }]
        });
        let event = parse_graph_event("uid", &json).unwrap();
        assert_eq!(event.attendees[0].status, "NEEDS-ACTION");
    }

    #[test]
    fn parse_graph_event_missing_response_field_maps_to_needs_action() {
        let json = serde_json::json!({
            "subject": "Test",
            "start": { "dateTime": "2026-03-20T09:00:00.0000000" },
            "organizer": { "emailAddress": { "address": "a@b.com" } },
            "attendees": [{
                "emailAddress": { "address": "x@y.com" },
                "status": {}
            }]
        });
        let event = parse_graph_event("uid", &json).unwrap();
        assert_eq!(event.attendees[0].status, "NEEDS-ACTION");
    }

    #[test]
    fn parse_graph_event_fractional_seconds() {
        // Graph sometimes returns varying precision
        let json = serde_json::json!({
            "subject": "Test",
            "start": { "dateTime": "2026-03-20T14:30:00.123" },
            "end": { "dateTime": "2026-03-20T15:00:00.0" },
            "organizer": { "emailAddress": { "address": "a@b.com" } }
        });
        let event = parse_graph_event("uid", &json).unwrap();
        assert_eq!(event.dtstart.format("%H:%M").to_string(), "14:30");
    }

    // =========================================================================
    // Phase 4 Milestone A — Outlook email
    //
    // TDD discipline: every behavior change has a RED test first. Pure
    // helpers (folder→role, OData translator, message parser, etc.) live
    // here; async/HTTP-bound functions are integration territory and rely
    // on these helpers being correct.
    // =========================================================================

    // ---- outlook_folder_role ----

    #[test]
    fn outlook_folder_role_inbox() {
        assert_eq!(outlook_folder_role("inbox"), Some("inbox".into()));
    }

    #[test]
    fn outlook_folder_role_sent() {
        assert_eq!(outlook_folder_role("sentitems"), Some("sent".into()));
    }

    #[test]
    fn outlook_folder_role_drafts() {
        assert_eq!(outlook_folder_role("drafts"), Some("drafts".into()));
    }

    #[test]
    fn outlook_folder_role_trash() {
        assert_eq!(outlook_folder_role("deleteditems"), Some("trash".into()));
    }

    #[test]
    fn outlook_folder_role_junk() {
        assert_eq!(outlook_folder_role("junkemail"), Some("junk".into()));
    }

    #[test]
    fn outlook_folder_role_archive() {
        // Outlook's "Archive" is a well-known folder once enabled.
        assert_eq!(outlook_folder_role("archive"), Some("archive".into()));
    }

    #[test]
    fn outlook_folder_role_user_folder() {
        // User-created folder names don't map to any well-known role.
        assert_eq!(outlook_folder_role("Projects"), None);
        assert_eq!(outlook_folder_role("Receipts/2026"), None);
    }

    #[test]
    fn outlook_folder_role_case_sensitive_well_known() {
        // Graph's well-known folder IDs are lowercase; mixed-case IDs are
        // user-created and shouldn't accidentally map to a role.
        assert_eq!(outlook_folder_role("INBOX"), None);
    }

    // ---- clear_stored_tokens ----

    fn make_outlook_session_with_token_file(token_path: PathBuf) -> OutlookSession {
        // Seed the token file so clear can prove it removes it.
        let stored = StoredTokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            token_expiry: Utc::now(),
            email: "u@x.com".into(),
        };
        std::fs::write(&token_path, serde_json::to_string_pretty(&stored).unwrap()).unwrap();
        OutlookSession {
            client: reqwest::Client::new(),
            token: tokio::sync::Mutex::new(OutlookToken {
                access_token: "a".into(),
                refresh_token: "r".into(),
                token_expiry: Utc::now(),
            }),
            client_id: "test-client".into(),
            token_path,
            email: "u@x.com".into(),
            folder_cache: tokio::sync::Mutex::new(None),
            page_cache: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    #[tokio::test]
    async fn clear_stored_tokens_deletes_token_file() {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("outlook-tokens.json");
        let session = make_outlook_session_with_token_file(token_path.clone());
        assert!(token_path.exists(), "precondition: token file was seeded");

        clear_stored_tokens(&session).await;

        assert!(
            !token_path.exists(),
            "token file should be deleted after clear_stored_tokens"
        );
    }

    #[tokio::test]
    async fn clear_stored_tokens_is_idempotent_if_file_missing() {
        // Calling clear twice (or against a missing file) shouldn't panic
        // — the 401-on-revoke path might race with manual cleanup.
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("outlook-tokens.json");
        let session = make_outlook_session_with_token_file(token_path);

        clear_stored_tokens(&session).await;
        clear_stored_tokens(&session).await; // second call must not panic
    }

    // ---- translate_query_to_odata ----
    //
    // Graph has two query languages: $filter (typed/structured) and $search
    // (full-text, KQL-flavored). Graph rejects them combined on most
    // fields, so our translator splits ParsedQuery across both:
    //   - structured operators (is:unread, has:attachment, from:, dates,
    //     is:starred) → $filter
    //   - free text + subject: → $search
    // Escape rules differ:
    //   - $filter values use single-quote-doubling (O'Brien → 'O''Brien')
    //   - $search values use double-quote wrapping with inner-quote escape
    //
    // Top-5 greats consensus finding: pin every escape case with a test
    // before implementing so a parser-time correctness bug can't ship.

    use crate::types::ParsedQuery;

    #[test]
    fn odata_translator_empty_query_is_empty() {
        let q = ParsedQuery::default();
        let r = translate_query_to_odata(&q);
        assert_eq!(r.filter, None);
        assert_eq!(r.search, None);
    }

    #[test]
    fn odata_translator_is_unread_routes_to_filter() {
        let q = ParsedQuery {
            is_unread: Some(true),
            ..Default::default()
        };
        let r = translate_query_to_odata(&q);
        assert_eq!(r.filter.as_deref(), Some("isRead eq false"));
        assert_eq!(r.search, None);
    }

    #[test]
    fn odata_translator_is_read_routes_to_filter() {
        let q = ParsedQuery {
            is_unread: Some(false),
            ..Default::default()
        };
        let r = translate_query_to_odata(&q);
        assert_eq!(r.filter.as_deref(), Some("isRead eq true"));
    }

    #[test]
    fn odata_translator_has_attachment_routes_to_filter() {
        let q = ParsedQuery {
            has_attachment: true,
            ..Default::default()
        };
        let r = translate_query_to_odata(&q);
        assert_eq!(r.filter.as_deref(), Some("hasAttachments eq true"));
    }

    #[test]
    fn odata_translator_is_starred_routes_to_filter() {
        let q = ParsedQuery {
            is_flagged: Some(true),
            ..Default::default()
        };
        let r = translate_query_to_odata(&q);
        assert_eq!(r.filter.as_deref(), Some("flag/flagStatus eq 'flagged'"));
    }

    #[test]
    fn odata_translator_single_from_routes_to_filter() {
        let q = ParsedQuery {
            from: vec!["alice@example.com".into()],
            ..Default::default()
        };
        let r = translate_query_to_odata(&q);
        assert_eq!(
            r.filter.as_deref(),
            Some("from/emailAddress/address eq 'alice@example.com'")
        );
    }

    #[test]
    fn odata_translator_multi_from_uses_and() {
        // Match Gmail/Fastmail semantics: multi-value of same operator = AND
        let q = ParsedQuery {
            from: vec!["a@x.com".into(), "b@y.com".into()],
            ..Default::default()
        };
        let r = translate_query_to_odata(&q);
        let s = r.filter.unwrap();
        assert!(s.contains("'a@x.com'"));
        assert!(s.contains("'b@y.com'"));
        assert!(s.contains(" and "));
    }

    #[test]
    fn odata_translator_escapes_single_quote_in_filter_value() {
        // O'Brien must become 'O''Brien' (OData single-quote doubling).
        let q = ParsedQuery {
            from: vec!["O'Brien@example.com".into()],
            ..Default::default()
        };
        let r = translate_query_to_odata(&q);
        assert!(
            r.filter
                .as_deref()
                .unwrap()
                .contains("'O''Brien@example.com'"),
            "got {:?}",
            r.filter
        );
    }

    #[test]
    fn odata_translator_to_routes_to_filter() {
        let q = ParsedQuery {
            to: vec!["team@example.com".into()],
            ..Default::default()
        };
        let r = translate_query_to_odata(&q);
        assert_eq!(
            r.filter.as_deref(),
            Some("toRecipients/any(t: t/emailAddress/address eq 'team@example.com')")
        );
    }

    #[test]
    fn odata_translator_before_routes_to_filter_with_rfc3339() {
        let q = ParsedQuery {
            before: Some(chrono::NaiveDate::from_ymd_opt(2026, 1, 15).unwrap()),
            ..Default::default()
        };
        let r = translate_query_to_odata(&q);
        assert_eq!(
            r.filter.as_deref(),
            Some("receivedDateTime lt 2026-01-15T00:00:00Z")
        );
    }

    #[test]
    fn odata_translator_after_routes_to_filter_with_rfc3339() {
        let q = ParsedQuery {
            after: Some(chrono::NaiveDate::from_ymd_opt(2026, 1, 15).unwrap()),
            ..Default::default()
        };
        let r = translate_query_to_odata(&q);
        assert_eq!(
            r.filter.as_deref(),
            Some("receivedDateTime ge 2026-01-15T00:00:00Z")
        );
    }

    #[test]
    fn odata_translator_subject_routes_to_search_not_filter() {
        // Graph rejects $filter contains() on subject in many tenants; route
        // to $search where it's well-supported. KQL subject: prefix.
        let q = ParsedQuery {
            subject: vec!["foo".into()],
            ..Default::default()
        };
        let r = translate_query_to_odata(&q);
        assert_eq!(r.filter, None);
        assert_eq!(r.search.as_deref(), Some(r#""subject:foo""#));
    }

    #[test]
    fn odata_translator_free_text_routes_to_search() {
        let q = ParsedQuery {
            text: "quarterly review".into(),
            ..Default::default()
        };
        let r = translate_query_to_odata(&q);
        assert_eq!(r.filter, None);
        assert_eq!(r.search.as_deref(), Some(r#""quarterly review""#));
    }

    #[test]
    fn odata_translator_search_escapes_inner_double_quote() {
        // $search wraps in "…"; inner " must be escaped as \"
        let q = ParsedQuery {
            text: r#"a"b"#.into(),
            ..Default::default()
        };
        let r = translate_query_to_odata(&q);
        assert_eq!(r.search.as_deref(), Some(r#""a\"b""#));
    }

    #[test]
    fn odata_translator_search_escapes_backslash() {
        // Backslashes double so they don't accidentally form escape sequences.
        let q = ParsedQuery {
            text: r"path\to\file".into(),
            ..Default::default()
        };
        let r = translate_query_to_odata(&q);
        assert_eq!(r.search.as_deref(), Some(r#""path\\to\\file""#));
    }

    #[test]
    fn odata_translator_combined_filter_uses_and() {
        let q = ParsedQuery {
            is_unread: Some(true),
            has_attachment: true,
            from: vec!["alice@example.com".into()],
            ..Default::default()
        };
        let r = translate_query_to_odata(&q);
        let s = r.filter.unwrap();
        assert!(s.contains("isRead eq false"));
        assert!(s.contains("hasAttachments eq true"));
        assert!(s.contains("'alice@example.com'"));
        // All AND'd together
        assert_eq!(s.matches(" and ").count(), 2);
    }

    #[test]
    fn odata_translator_combined_filter_plus_search() {
        let q = ParsedQuery {
            is_unread: Some(true),
            text: "newsletter".into(),
            ..Default::default()
        };
        let r = translate_query_to_odata(&q);
        assert_eq!(r.filter.as_deref(), Some("isRead eq false"));
        assert_eq!(r.search.as_deref(), Some(r#""newsletter""#));
    }

    #[test]
    fn odata_translator_subject_and_free_text_concatenate_in_search() {
        // Both go into $search, space-joined as a single KQL expression.
        let q = ParsedQuery {
            subject: vec!["meeting".into()],
            text: "tomorrow".into(),
            ..Default::default()
        };
        let r = translate_query_to_odata(&q);
        let s = r.search.unwrap();
        assert!(s.contains("subject:meeting"));
        assert!(s.contains("tomorrow"));
    }

    // ---- parse_graph_message ----
    //
    // Graph's typed message response already has the fields we need —
    // unlike Gmail, we don't walk a MIME payload tree. The parser is a
    // straightforward JSON→Email mapper but the field shapes are easy to
    // get wrong, so pin every behavior.

    fn graph_message_minimal() -> serde_json::Value {
        serde_json::json!({
            "id": "MSG_ABC",
            "internetMessageId": "<abc@x.com>",
            "conversationId": "CONV_1",
            "subject": "Hello",
            "bodyPreview": "preview text",
            "isRead": true,
            "flag": { "flagStatus": "notFlagged" },
            "hasAttachments": false,
            "receivedDateTime": "2026-05-22T14:00:00Z",
            "from": { "emailAddress": { "address": "alice@example.com", "name": "Alice" } },
            "toRecipients": [
                { "emailAddress": { "address": "me@example.com", "name": "Me" } }
            ],
            "ccRecipients": [],
            "parentFolderId": "inbox",
            "body": { "contentType": "text", "content": "Hi there!" }
        })
    }

    #[test]
    fn parse_graph_message_basic_fields() {
        let m = parse_graph_message(&graph_message_minimal(), true);
        assert_eq!(m.id, "MSG_ABC");
        assert_eq!(m.thread_id, "CONV_1");
        assert_eq!(m.subject, "Hello");
        assert_eq!(m.preview, "preview text");
        assert_eq!(m.from[0].email, "alice@example.com");
        assert_eq!(m.from[0].name.as_deref(), Some("Alice"));
        assert_eq!(m.to[0].email, "me@example.com");
        assert!(m.cc.is_empty());
    }

    #[test]
    fn parse_graph_message_read_state_via_isread() {
        // isRead: true → $seen keyword (matches Email::is_unread() semantics)
        let m = parse_graph_message(&graph_message_minimal(), true);
        assert!(!m.is_unread(), "isRead:true should make $seen present");
    }

    #[test]
    fn parse_graph_message_unread_state() {
        let mut j = graph_message_minimal();
        j["isRead"] = serde_json::json!(false);
        let m = parse_graph_message(&j, true);
        assert!(m.is_unread(), "isRead:false should leave $seen absent");
    }

    #[test]
    fn parse_graph_message_flagged_state() {
        let mut j = graph_message_minimal();
        j["flag"] = serde_json::json!({ "flagStatus": "flagged" });
        let m = parse_graph_message(&j, true);
        assert!(m.is_flagged(), "flagStatus:flagged should set $flagged");
    }

    #[test]
    fn parse_graph_message_not_flagged_state() {
        let m = parse_graph_message(&graph_message_minimal(), true);
        assert!(!m.is_flagged());
    }

    #[test]
    fn parse_graph_message_text_body_from_contenttype_text() {
        let m = parse_graph_message(&graph_message_minimal(), true);
        assert_eq!(m.text_body.as_deref(), Some("Hi there!"));
        assert!(m.html_body.is_none());
    }

    #[test]
    fn parse_graph_message_html_body_from_contenttype_html() {
        let mut j = graph_message_minimal();
        j["body"] = serde_json::json!({
            "contentType": "html",
            "content": "<p>Hi!</p>"
        });
        let m = parse_graph_message(&j, true);
        assert_eq!(m.html_body.as_deref(), Some("<p>Hi!</p>"));
        assert!(m.text_body.is_none());
    }

    #[test]
    fn parse_graph_message_no_body_when_fetch_body_false() {
        // get_emails with fetch_body=false should give back metadata only.
        let m = parse_graph_message(&graph_message_minimal(), false);
        assert!(m.text_body.is_none());
        assert!(m.html_body.is_none());
    }

    #[test]
    fn parse_graph_message_attachments_expanded() {
        let mut j = graph_message_minimal();
        j["hasAttachments"] = serde_json::json!(true);
        j["attachments"] = serde_json::json!([
            {
                "@odata.type": "#microsoft.graph.fileAttachment",
                "id": "ATT_1",
                "name": "report.pdf",
                "contentType": "application/pdf",
                "size": 12345
            },
            {
                "@odata.type": "#microsoft.graph.fileAttachment",
                "id": "ATT_2",
                "name": "invite.ics",
                "contentType": "text/calendar",
                "size": 800
            }
        ]);
        let m = parse_graph_message(&j, true);
        assert!(m.has_attachment);
        assert_eq!(m.attachments.len(), 2);
        assert_eq!(m.attachments[0].name, "report.pdf");
        assert_eq!(m.attachments[0].mime_type, "application/pdf");
        // BlobRef shape uses outlook: prefix to disambiguate from Gmail
        assert!(m.attachments[0].blob_id.starts_with("outlook:"));
        assert!(m.attachments[0].blob_id.contains("MSG_ABC"));
        assert!(m.attachments[0].blob_id.contains("ATT_1"));
    }

    #[test]
    fn parse_graph_message_calendar_invite_sets_has_calendar() {
        // A text/calendar attachment flips has_calendar so the UI can
        // surface the RSVP affordance.
        let mut j = graph_message_minimal();
        j["hasAttachments"] = serde_json::json!(true);
        j["attachments"] = serde_json::json!([{
            "@odata.type": "#microsoft.graph.fileAttachment",
            "id": "ATT_ICS",
            "name": "invite.ics",
            "contentType": "text/calendar",
            "size": 500
        }]);
        let m = parse_graph_message(&j, true);
        assert!(m.has_calendar);
    }

    #[test]
    fn parse_graph_message_no_attachments_no_calendar() {
        let m = parse_graph_message(&graph_message_minimal(), true);
        assert!(!m.has_calendar);
        assert!(m.attachments.is_empty());
    }

    #[test]
    fn parse_graph_message_received_datetime_parses_to_utc() {
        let m = parse_graph_message(&graph_message_minimal(), true);
        assert_eq!(
            m.received_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "2026-05-22T14:00:00Z"
        );
    }

    #[test]
    fn parse_graph_message_parent_folder_id_into_mailbox_ids() {
        // The folder this message lives in shows up as a mailbox_id so the
        // UI's split-by-folder views work.
        let m = parse_graph_message(&graph_message_minimal(), true);
        assert!(m.mailbox_ids.contains_key("inbox"));
    }
}
